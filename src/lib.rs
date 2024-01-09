#![deny(clippy::all)]
#![allow(dead_code)]
//! # tokio-socketcan-isotp
//!
//! This library creates asynchronous wrapper around [socketcan-isotp](https://github.com/marcelbuesing/socketcan-isotp)
//!
//! Currently not dependent on the original, but rather on the modified of the socketcan-isotp libary, which adds read_to_vec method, in order to prevent mutable borrowing of the socket, when reading.
//!
//! You may experience busy waiting, or soft locks when two consecutive locks. This is due to the error in Linux Kernel, which is now [solved](https://lore.kernel.org/linux-can/20230818114345.142983-1-lukas.magel@posteo.net/), but the maintainer of your Linux kernel might not have shipped it yet. I tested it on the mainline Kernel 6.6.5 and everything worked correctly. If you experience mentioned errors and cannot upgrade to newer kernel, refer to the src/lib.rs file to the poll method of the IsoTpWriteFuture, where a edit is suggested that might help with your situation.
//!
//! The API is provided by the struct [IsoTpSocket]
//!
//! Example of basic echoing server on vcan0:
//!
//! ```rust
//! use tokio_socketcan_isotp::{IsoTpSocket, StandardId, Error};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Error> {
//!    let mut socket = IsoTpSocket::open(
//!        "vcan0",
//!        StandardId::new(0x123).expect("Invalid src id"),
//!        StandardId::new(0x321).expect("Invalid src id")
//!            )?;
//!            
//!     while let Ok(packet) = socket.read_packet()?.await {
//!         println!("{:?}", packet);
//!         let rx = socket.write_packet(packet)?.await;
//!     }
//! }
//! ```
//!
//! To setup vcan0 run following commands:
//!
//! ```bash
//! sudo modprobe can
//! sudo ip link add dev vcan0 type vcan
//! sudo ip link set up vcan0
//! ```

mod socketcan_isotp;

pub use crate::socketcan_isotp::{
    Error, ExtendedId, FlowControlOptions, Id, IsoTpBehaviour, IsoTpOptions, LinkLayerOptions,
    StandardId, TxFlags,
};
use futures::prelude::*;
use futures::ready;
use std::io;
use std::os::raw::c_int;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::unix::AsyncFd;

/// Future for writing data to IsoTpSocket
pub struct IsoTpWriteFuture<'a> {
    socket: &'a IsoTpSocket,
    packet: &'a [u8],
}

impl Future for IsoTpWriteFuture<'_> {
    type Output = io::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        loop {
            let mut guard = ready!(self.socket.0.poll_write_ready(cx))?;
            match self.socket.0.get_ref().write(self.packet) {
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    guard.clear_ready(); // Comment this line if you are on older Kernel and the communication soft-locks
                    continue;
                }
                Ok(_) => return Poll::Ready(Ok(())),
                Err(err) => return Poll::Ready(Err(err)),
            }
        }
    }
}

/// Future for reading data from IsoTpSocket
pub struct IsoTpReadFuture<'a> {
    socket: &'a IsoTpSocket,
}

impl Future for IsoTpReadFuture<'_> {
    type Output = io::Result<Vec<u8>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        loop {
            let mut ready_guard = ready!(self.socket.0.poll_read_ready(cx))?;
            match ready_guard.try_io(|inner| inner.get_ref().read_to_vec()) {
                Ok(result) => return Poll::Ready(result),
                Err(_would_block) => continue,
            }
        }
    }
}

/// An asynchronous I/O wrapped socketcan_isotp::IsoTpSocket
/// For reading and writting to the socket use [IsoTpSocket::read_packet] and [IsoTpSocket::write_packet] respectively.
pub struct IsoTpSocket(AsyncFd<socketcan_isotp::IsoTpSocket>);
#[allow(dead_code)]
impl IsoTpSocket {
    /// Open a named CAN device such as "vcan0"
    pub fn open(
        ifname: &str,
        src: impl Into<Id>,
        dst: impl Into<Id>,
    ) -> Result<IsoTpSocket, socketcan_isotp::Error> {
        let sock = socketcan_isotp::IsoTpSocket::open(ifname, src, dst)?;
        sock.set_nonblocking(true)?;
        Ok(IsoTpSocket(AsyncFd::new(sock)?))
    }

    pub fn open_with_opts(
        ifname: &str,
        src: impl Into<Id>,
        dst: impl Into<Id>,
        isotp_options: Option<IsoTpOptions>,
        rx_flow_control_options: Option<FlowControlOptions>,
        link_layer_options: Option<LinkLayerOptions>,
    ) -> Result<IsoTpSocket, socketcan_isotp::Error> {
        let sock = socketcan_isotp::IsoTpSocket::open_with_opts(
            ifname,
            src,
            dst,
            isotp_options,
            rx_flow_control_options,
            link_layer_options,
        )?;
        sock.set_nonblocking(true)?;
        Ok(IsoTpSocket(AsyncFd::new(sock)?))
    }

    /// Open by kernel interface number
    pub fn open_if(
        if_index: c_int,
        src: impl Into<Id>,
        dst: impl Into<Id>,
    ) -> Result<IsoTpSocket, socketcan_isotp::Error> {
        let sock = socketcan_isotp::IsoTpSocket::open_if(if_index, src, dst)?;
        sock.set_nonblocking(true)?;
        Ok(IsoTpSocket(AsyncFd::new(sock)?))
    }

    pub fn open_if_with_opts(
        if_index: c_int,
        src: impl Into<Id>,
        dst: impl Into<Id>,
        isotp_options: Option<IsoTpOptions>,
        rx_flow_control_options: Option<FlowControlOptions>,
        link_layer_options: Option<LinkLayerOptions>,
    ) -> Result<IsoTpSocket, socketcan_isotp::Error> {
        let sock = socketcan_isotp::IsoTpSocket::open_if_with_opts(
            if_index,
            src,
            dst,
            isotp_options,
            rx_flow_control_options,
            link_layer_options,
        )?;
        sock.set_nonblocking(true)?;
        Ok(IsoTpSocket(AsyncFd::new(sock)?))
    }

    pub fn write_packet<'a>(&'a self, packet: &'a [u8]) -> Result<IsoTpWriteFuture, Error> {
        Ok(IsoTpWriteFuture {
            socket: self,
            packet,
        })
    }

    pub fn read_packet(&self) -> Result<IsoTpReadFuture, Error> {
        Ok(IsoTpReadFuture { socket: self })
    }
}
