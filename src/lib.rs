//!
//! # tokio-socketcan-isotp
//!
//! This library sets up to create asynchronous wrapper around [socketcan-isotp](https://github.com/marcelbuesing/socketcan-isotp)
//!
//! This library is currently work in progress. Basic API is working, but your knowledge may vary.
//! Currently not dependent on the original, but rather on the fork of the socketcan-isotp libary.
//!
//! For the correct behaviour, you need to have Linux kernel with applied patch:
//! https://lore.kernel.org/linux-can/20230818114345.142983-1-lukas.magel@posteo.net/#r
//!
//! Without the patch the communication after read will result in soft-lock
//!
//! ```rust
//! use tokio_socketcan_isotp::{IsoTpSocket, StandardId, Error};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Error> {
//!     let mut socket = IsoTpSocket::open(
//!         "vcan0",
//!         StandardId::new(0x123).expect("Invalid src id"),
//!         StandardId::new(0x321).expect("Invalid src id")
//!     )?;
//!
//!     while let Ok(packet) = socket.read_packet()?.await {
//!         println!("{:?}", packet);
//!         let rx = socket.write_packet(packet)?.await;
//!     }
//! }
//! ```

mod socketcan_isotp;

pub use crate::socketcan_isotp::{Id, StandardId, ExtendedId, Error, IsoTpOptions, IsoTpBehaviour, FlowControlOptions, LinkLayerOptions, TxFlags};
use futures::prelude::*;
use futures::ready;
use std::io;
use std::os::raw::c_int;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::unix::AsyncFd;

/// Future for writing data to IsoTPSocket
pub struct IsoTpWriteFuture<'a> {
    socket: &'a IsoTpSocket,
    packet: &'a [u8],
}

impl Future for IsoTpWriteFuture<'_> {
    type Output = io::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        loop {
            let mut guard = ready!(self.socket.0.poll_write_ready(cx))?;
            match self.socket.0.get_ref().write(&self.packet) {
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    println!("would block");
                    guard.clear_ready();
                    continue;
                }
                Ok(_) => return Poll::Ready(Ok(())),
                Err(err) => return Poll::Ready(Err(err)),
            }
            // Following commented code is, how the poll should ideally work.
            // However due to bug in linux it is currently not working as expected,
            // isotp socket returns ready on write, even tho it is not.
            // This behaviour should be resolved shortly.

            // let mut guard = ready!(self.socket.0.poll_write_ready(cx))?;
            // match guard.try_io(|inner| inner.get_ref().write_vec(&self.packet)) {
            //         Err(_) => {
            //             // println!("would block");
            //             // guard.clear_ready(); //Errcomm
            //             continue
            //         },
            //         Ok(_) => {
            //             // println!("writting done");
            //             return Poll::Ready(Ok(()))
            //         },
            // }
        }
    }
}

/// Future for readint data from IsoTPSocket
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
            socket: &self,
            packet,
        })
    }

    pub fn read_packet(&self) -> Result<IsoTpReadFuture, Error> {
        Ok(IsoTpReadFuture { socket: &self })
    }
}
