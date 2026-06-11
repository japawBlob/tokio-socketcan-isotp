//! Raw SocketCAN (`CAN_RAW`) frame channel.
//!
//! Each channel owns its own raw socket with a kernel-side filter on the
//! socket's rx id. The kernel duplicates every bus frame to every matching
//! `CAN_RAW` socket, so multiple sockets per interface need no userspace
//! demultiplexer - the same model as multiple kernel ISO-TP sockets.

use super::CanFrame;
use crate::error::Error;
use crate::types::{id_to_raw, Id};
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
}

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

        Ok(RawCanChannel {
            fd: AsyncFd::new(fd)?,
        })
    }

    pub(crate) async fn send(&self, frame: CanFrame) -> io::Result<()> {
        let mut raw: libc::can_frame = unsafe { std::mem::zeroed() };
        raw.can_id = frame.can_id;
        raw.can_dlc = frame.len;
        raw.data[..frame.data.len()].copy_from_slice(&frame.data);
        loop {
            let mut guard = self.fd.writable().await?;
            match guard.try_io(|inner| {
                let n = unsafe {
                    libc::write(
                        inner.get_ref().0,
                        std::ptr::addr_of!(raw).cast(),
                        size_of::<libc::can_frame>(),
                    )
                };
                if n == size_of::<libc::can_frame>() as isize {
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
            let mut raw: libc::can_frame = unsafe { std::mem::zeroed() };
            match guard.try_io(|inner| {
                let n = unsafe {
                    libc::read(
                        inner.get_ref().0,
                        std::ptr::addr_of_mut!(raw).cast(),
                        size_of::<libc::can_frame>(),
                    )
                };
                if n == size_of::<libc::can_frame>() as isize {
                    Ok(())
                } else if n < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "short CAN frame read",
                    ))
                }
            }) {
                Ok(Ok(())) => {
                    let len = raw.can_dlc.min(8);
                    let mut data = [0u8; 8];
                    data.copy_from_slice(&raw.data[..8]);
                    return Ok(CanFrame {
                        can_id: raw.can_id,
                        len,
                        data,
                    });
                }
                Ok(Err(err)) => return Err(err),
                Err(_would_block) => continue,
            }
        }
    }
}
