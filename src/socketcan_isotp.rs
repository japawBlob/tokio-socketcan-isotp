//! Code from [socketcan-isotp](https://github.com/marcelbuesing/socketcan-isotp)
//! and added read_to_vec() method
//!
//! Socketcan ISO-TP support.
//!
//! The Linux kernel supports using CAN-devices through a
//! [network-like API](https://www.kernel.org/doc/Documentation/networking/can.txt).
//! This crate allows easy access to this functionality without having to wrestle
//! libc calls.
//!
//! ISO-TP allows sending data packets that exceed the eight byte of a default CAN frame.
//! [can-isotp](https://github.com/hartkopp/can-isotp) is an ISO-TP kernel module that takes
//! care of handling the ISO-TP protocol.
//!
//! Instructions on how the can-isotp kernel module can be build and loaded can be found
//! at [https://github.com/hartkopp/can-isotp](https://github.com/hartkopp/can-isotp) .

use crate::error::Error;
use crate::types::{
    AF_CAN, CAN_ISOTP, CAN_ISOTP_LL_OPTS, CAN_ISOTP_OPTS, CAN_ISOTP_RECV_FC, FlowControlOptions,
    Id, IsoTpOptions, LinkLayerOptions, RECV_BUFFER_SIZE, SOL_CAN_ISOTP, id_to_raw,
};
use libc::{
    F_GETFL, F_SETFL, O_NONBLOCK, SOCK_DGRAM, bind, c_int, c_short, c_void, close, fcntl, read,
    setsockopt, sockaddr, socket, write,
};
use nix::net::if_::if_nametoindex;
use std::convert::TryInto;
use std::io;
use std::mem::size_of;
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd, RawFd};

const FLOW_CONTROL_OPTIONS_SIZE: usize = size_of::<FlowControlOptions>();

const ISOTP_OPTIONS_SIZE: usize = size_of::<IsoTpOptions>();

const LINK_LAYER_OPTIONS_SIZE: usize = size_of::<LinkLayerOptions>();

#[derive(Debug)]
#[repr(C)]
struct CanAddr {
    _af_can: c_short,
    if_index: c_int, // address familiy,
    /// transport protocol class address information
    rx_id: u32,
    /// transport protocol class address information
    tx_id: u32,
    _pgn: u32,
    _addr: u8,
}

/// An ISO-TP socketcan socket.
///
/// Will be closed upon deallocation. To close manually, use `std::drop::Drop`.
/// Internally this is just a wrapped file-descriptor.
pub struct IsoTpSocket {
    fd: c_int,
    recv_buffer: [u8; RECV_BUFFER_SIZE],
}

impl IsoTpSocket {
    /// Open a named CAN ISO-TP device.
    ///
    /// Usually the more common case, opens a socket can device by name, such
    /// as "vcan0" or "socan0".
    pub fn open(ifname: &str, src: impl Into<Id>, dst: impl Into<Id>) -> Result<Self, Error> {
        Self::open_with_opts(
            ifname,
            src,
            dst,
            Some(IsoTpOptions::default()),
            Some(FlowControlOptions::default()),
            Some(LinkLayerOptions::default()),
        )
    }

    /// Open a named CAN ISO-TP device, passing additional options.
    ///
    /// Usually the more common case, opens a socket can device by name, such
    /// as "vcan0" or "socan0".
    pub fn open_with_opts(
        ifname: &str,
        src: impl Into<Id>,
        dst: impl Into<Id>,
        isotp_options: Option<IsoTpOptions>,
        rx_flow_control_options: Option<FlowControlOptions>,
        link_layer_options: Option<LinkLayerOptions>,
    ) -> Result<Self, Error> {
        let if_index = if_nametoindex(ifname)?;
        Self::open_if_with_opts(
            if_index.try_into().unwrap(),
            src,
            dst,
            isotp_options,
            rx_flow_control_options,
            link_layer_options,
        )
    }

    /// Open CAN ISO-TP device device by interface number.
    ///
    /// Opens a CAN device by kernel interface number.
    pub fn open_if(if_index: c_int, src: impl Into<Id>, dst: impl Into<Id>) -> Result<Self, Error> {
        Self::open_if_with_opts(
            if_index,
            src,
            dst,
            Some(IsoTpOptions::default()),
            Some(FlowControlOptions::default()),
            Some(LinkLayerOptions::default()),
        )
    }

    /// Open CAN ISO-TP device device by interface number, passing additional options.
    ///
    /// Opens a CAN device by kernel interface number.
    pub fn open_if_with_opts(
        if_index: c_int,
        src: impl Into<Id>,
        dst: impl Into<Id>,
        isotp_options: Option<IsoTpOptions>,
        rx_flow_control_options: Option<FlowControlOptions>,
        link_layer_options: Option<LinkLayerOptions>,
    ) -> Result<Self, Error> {
        let rx_id = id_to_raw(src.into());
        let tx_id = id_to_raw(dst.into());
        let addr = CanAddr {
            _af_can: AF_CAN,
            if_index,
            rx_id,
            tx_id,
            _pgn: 0,
            _addr: 0,
        };

        // open socket
        let sock_fd;
        unsafe {
            sock_fd = socket(crate::types::PF_CAN, SOCK_DGRAM, CAN_ISOTP);
        }

        if sock_fd == -1 {
            return Err(Error::from(io::Error::last_os_error()));
        }

        // Set IsoTpOptions
        if let Some(isotp_options) = isotp_options {
            let isotp_options_ptr: *const c_void = &isotp_options as *const _ as *const c_void;
            let err = unsafe {
                setsockopt(
                    sock_fd,
                    SOL_CAN_ISOTP,
                    CAN_ISOTP_OPTS,
                    isotp_options_ptr,
                    ISOTP_OPTIONS_SIZE.try_into().unwrap(),
                )
            };
            if err == -1 {
                return Err(Error::from(io::Error::last_os_error()));
            }
        }

        // Set FlowControlOptions
        if let Some(rx_flow_control_options) = rx_flow_control_options {
            let rx_flow_control_options_ptr: *const c_void =
                &rx_flow_control_options as *const _ as *const c_void;
            let err = unsafe {
                setsockopt(
                    sock_fd,
                    SOL_CAN_ISOTP,
                    CAN_ISOTP_RECV_FC,
                    rx_flow_control_options_ptr,
                    FLOW_CONTROL_OPTIONS_SIZE.try_into().unwrap(),
                )
            };
            if err == -1 {
                return Err(Error::from(io::Error::last_os_error()));
            }
        }

        // Set LinkLayerOptions
        if let Some(link_layer_options) = link_layer_options {
            let link_layer_options_ptr: *const c_void =
                &link_layer_options as *const _ as *const c_void;
            let err = unsafe {
                setsockopt(
                    sock_fd,
                    SOL_CAN_ISOTP,
                    CAN_ISOTP_LL_OPTS,
                    link_layer_options_ptr,
                    LINK_LAYER_OPTIONS_SIZE.try_into().unwrap(),
                )
            };
            if err == -1 {
                return Err(Error::from(io::Error::last_os_error()));
            }
        }

        // bind it
        let bind_rv;
        unsafe {
            let sockaddr_ptr = &addr as *const CanAddr;
            bind_rv = bind(
                sock_fd,
                sockaddr_ptr as *const sockaddr,
                size_of::<CanAddr>().try_into().unwrap(),
            );
        }

        // FIXME: on fail, close socket (do not leak socketfds)
        if bind_rv == -1 {
            let e = io::Error::last_os_error();
            unsafe {
                close(sock_fd);
            }
            return Err(Error::from(e));
        }

        Ok(Self {
            fd: sock_fd,
            recv_buffer: [0x00; RECV_BUFFER_SIZE],
        })
    }

    fn close(&mut self) -> io::Result<()> {
        unsafe {
            let rv = close(self.fd);
            if rv != -1 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    /// Change socket to non-blocking mode
    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        // retrieve current flags
        let oldfl = unsafe { fcntl(self.fd, F_GETFL) };

        if oldfl == -1 {
            return Err(io::Error::last_os_error());
        }

        let newfl = if nonblocking {
            oldfl | O_NONBLOCK
        } else {
            oldfl & !O_NONBLOCK
        };

        let rv = unsafe { fcntl(self.fd, F_SETFL, newfl) };

        if rv != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Blocking read data
    pub fn read(&mut self) -> io::Result<&[u8]> {
        let buffer_ptr = &mut self.recv_buffer as *mut _ as *mut c_void;

        let read_rv = unsafe { read(self.fd, buffer_ptr, RECV_BUFFER_SIZE) };

        if read_rv < 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(&self.recv_buffer[0..read_rv.try_into().unwrap()])
    }

    pub fn write(&self, buffer: &[u8]) -> io::Result<()> {
        let write_rv = unsafe {
            let buffer_ptr = buffer as *const _ as *const c_void;
            write(self.fd, buffer_ptr, buffer.len())
        };
        if write_rv != buffer.len().try_into().unwrap() {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn read_to_vec(&self) -> io::Result<Vec<u8>> {
        let mut buffer: [u8; RECV_BUFFER_SIZE] = [0; RECV_BUFFER_SIZE];
        let buffer_ptr = &mut buffer as *mut _ as *mut c_void;

        let read_rv = unsafe { read(self.fd, buffer_ptr, RECV_BUFFER_SIZE) };

        if read_rv < 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(buffer[0..read_rv.try_into().unwrap()].to_vec())
    }
}

impl AsRawFd for IsoTpSocket {
    fn as_raw_fd(&self) -> RawFd {
        self.fd
    }
}

impl FromRawFd for IsoTpSocket {
    unsafe fn from_raw_fd(fd: RawFd) -> Self {
        Self {
            fd,
            recv_buffer: [0x00; RECV_BUFFER_SIZE],
        }
    }
}

impl IntoRawFd for IsoTpSocket {
    fn into_raw_fd(self) -> RawFd {
        self.fd
    }
}

impl Drop for IsoTpSocket {
    fn drop(&mut self) {
        self.close().ok(); // ignore result
    }
}
