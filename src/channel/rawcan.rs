//! Raw SocketCAN (`CAN_RAW`) frame channel, classic CAN and CAN FD.
//!
//! Each channel owns its own raw socket with a kernel-side filter on the
//! socket's rx id. The kernel duplicates every bus frame to every matching
//! `CAN_RAW` socket, so multiple sockets per interface need no userspace
//! demultiplexer - the same model as multiple kernel ISO-TP sockets.

use super::CanFrame;
use crate::error::Error;
use crate::types::{Id, id_to_raw};
use nix::net::if_::if_nametoindex;
use std::io;
use std::mem::size_of;
use std::os::unix::io::{AsRawFd, RawFd};
use tokio::io::unix::AsyncFd;

struct RawCanFd(libc::c_int);

impl AsRawFd for RawCanFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

impl Drop for RawCanFd {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.0);
        }
    }
}

pub(crate) struct RawCanChannel {
    fd: AsyncFd<RawCanFd>,
    /// `CAN_RAW_FD_FRAMES` was enabled successfully.
    fd_enabled: bool,
}

/// `sizeof(struct can_frame)`.
const CAN_MTU: usize = size_of::<libc::can_frame>();
/// `sizeof(struct canfd_frame)`.
const CANFD_MTU: usize = size_of::<libc::canfd_frame>();
/// `canfd_frame.flags`: bit rate switch.
const CANFD_BRS: u8 = 0x01;
/// `canfd_frame.flags`: mark CAN FD for dual use of `struct canfd_frame`.
const CANFD_FDF: u8 = 0x04;

impl RawCanChannel {
    /// Open a `CAN_RAW` socket on `ifname`, filtered to data frames whose
    /// id equals `rx`.
    pub(crate) fn open(ifname: &str, rx: Id) -> Result<Self, Error> {
        let if_index = if_nametoindex(ifname)?;

        let fd = unsafe { libc::socket(libc::PF_CAN, libc::SOCK_RAW, libc::CAN_RAW) };
        if fd == -1 {
            return Err(Error::from(io::Error::last_os_error()));
        }
        let fd = RawCanFd(fd);

        let id_mask = match rx {
            Id::Standard(_) => libc::CAN_SFF_MASK,
            Id::Extended(_) => libc::CAN_EFF_MASK,
        };
        let filter = libc::can_filter {
            can_id: id_to_raw(rx),
            can_mask: libc::CAN_EFF_FLAG | libc::CAN_RTR_FLAG | id_mask,
        };
        let rv = unsafe {
            libc::setsockopt(
                fd.0,
                libc::SOL_CAN_RAW,
                libc::CAN_RAW_FILTER,
                std::ptr::addr_of!(filter).cast(),
                size_of::<libc::can_filter>() as libc::socklen_t,
            )
        };
        if rv == -1 {
            return Err(Error::from(io::Error::last_os_error()));
        }

        let mut addr: libc::sockaddr_can = unsafe { std::mem::zeroed() };
        addr.can_family = libc::AF_CAN as libc::sa_family_t;
        addr.can_ifindex = if_index as libc::c_int;
        let rv = unsafe {
            libc::bind(
                fd.0,
                std::ptr::addr_of!(addr).cast(),
                size_of::<libc::sockaddr_can>() as libc::socklen_t,
            )
        };
        if rv == -1 {
            return Err(Error::from(io::Error::last_os_error()));
        }

        let flags = unsafe { libc::fcntl(fd.0, libc::F_GETFL) };
        if flags == -1
            || unsafe { libc::fcntl(fd.0, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1
        {
            return Err(Error::from(io::Error::last_os_error()));
        }

        // Accept and allow sending CAN FD frames. Fails on kernels/interfaces
        // without CAN FD support; classic operation still works then, and an
        // attempt to send an FD frame surfaces the kernel's error.
        let enable: libc::c_int = 1;
        let fd_enabled = unsafe {
            libc::setsockopt(
                fd.0,
                libc::SOL_CAN_RAW,
                libc::CAN_RAW_FD_FRAMES,
                std::ptr::addr_of!(enable).cast(),
                size_of::<libc::c_int>() as libc::socklen_t,
            )
        } == 0;

        Ok(RawCanChannel {
            fd: AsyncFd::new(fd)?,
            fd_enabled,
        })
    }

    pub(crate) async fn send(&self, frame: CanFrame) -> io::Result<()> {
        if frame.fd && !self.fd_enabled {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "CAN FD frames are not supported on this CAN_RAW socket/interface",
            ));
        }
        let mut raw: libc::canfd_frame = unsafe { std::mem::zeroed() };
        raw.can_id = frame.can_id;
        raw.len = frame.len;
        raw.data[..frame.len as usize].copy_from_slice(frame.payload());
        let size = if frame.fd {
            raw.flags = CANFD_FDF | if frame.brs { CANFD_BRS } else { 0 };
            CANFD_MTU
        } else {
            CAN_MTU
        };
        loop {
            let mut guard = self.fd.writable().await?;
            match guard.try_io(|inner| {
                let n =
                    unsafe { libc::write(inner.get_ref().0, std::ptr::addr_of!(raw).cast(), size) };
                if n == size as isize {
                    Ok(())
                } else {
                    Err(io::Error::last_os_error())
                }
            }) {
                Ok(result) => return result,
                Err(_would_block) => continue,
            }
        }
    }

    pub(crate) async fn recv(&self) -> io::Result<CanFrame> {
        loop {
            let mut guard = self.fd.readable().await?;
            let mut raw: libc::canfd_frame = unsafe { std::mem::zeroed() };
            match guard.try_io(|inner| {
                let n = unsafe {
                    libc::read(
                        inner.get_ref().0,
                        std::ptr::addr_of_mut!(raw).cast(),
                        CANFD_MTU,
                    )
                };
                if n == CANFD_MTU as isize {
                    Ok(true)
                } else if n == CAN_MTU as isize {
                    Ok(false)
                } else if n < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "short CAN frame read",
                    ))
                }
            }) {
                Ok(Ok(is_fd)) => {
                    // For a classic can_frame, `len` aliases `can_dlc`.
                    let max = if is_fd { 64 } else { 8 };
                    let len = (raw.len as usize).min(max);
                    return Ok(CanFrame::with_flags(
                        raw.can_id,
                        &raw.data[..len],
                        is_fd,
                        is_fd && raw.flags & CANFD_BRS != 0,
                    ));
                }
                Ok(Err(err)) => return Err(err),
                Err(_would_block) => continue,
            }
        }
    }
}
