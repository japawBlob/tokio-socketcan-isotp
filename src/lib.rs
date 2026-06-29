#![deny(clippy::all)]
#![allow(dead_code)]
//! # tokio-socketcan-isotp
//!
//! Asynchronous ISO-TP (ISO 15765-2) sockets for tokio.
//!
//! The transport backend is selected at runtime by the interface address
//! passed to [IsoTpSocket::open] / [IsoTpSocket::open_with_opts]:
//!
//! | Address               | Backend                                       | Requires                      |
//! |-----------------------|-----------------------------------------------|-------------------------------|
//! | `vcan0`, `can0`, ...  | kernel ISO-TP socket (`can_isotp` module)     | Linux                         |
//! | `rawcan:vcan0`        | userspace ISO-TP over a `CAN_RAW` socket      | Linux, feature `isotp-userspace` |
//! | `tcp:host:port`       | userspace ISO-TP over TCP (connect to a hub)  | feature `tcp`                 |
//! | `tcplisten:addr:port` | userspace ISO-TP over TCP (bind a frame hub)  | feature `tcp`                 |
//!
//! The kernel backend is the original transport of this crate and implements
//! the full ISO-TP protocol in the kernel. The userspace backends implement a
//! subset of ISO 15765-2 (single/first/consecutive frames and flow control
//! for classic CAN with normal addressing) and exist so that tests and
//! simulators can run without the `can_isotp` kernel module (`rawcan:`) or
//! without any CAN support at all (`tcp:`, also available on non-Linux
//! platforms).
//!
//! You may experience busy waiting, or soft locks when two consecutive locks.
//! This is due to the error in Linux Kernel, which is now
//! [solved](https://lore.kernel.org/linux-can/20230818114345.142983-1-lukas.magel@posteo.net/),
//! but the maintainer of your Linux kernel might not have shipped it yet.
//! If you experience mentioned errors and cannot upgrade to newer kernel,
//! refer to the src/kernel.rs file to the write_packet method, where an edit
//! is suggested that might help with your situation.
//!
//! Example of basic echoing server on vcan0:
//!
//! ```rust,no_run
//! use tokio_socketcan_isotp::{IsoTpSocket, StandardId, Error};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Error> {
//!     let socket = IsoTpSocket::open(
//!         "vcan0",
//!         StandardId::new(0x123).expect("Invalid src id"),
//!         StandardId::new(0x321).expect("Invalid dst id"),
//!     )?;
//!
//!     while let Ok(packet) = socket.read_packet().await {
//!         println!("{:?}", packet);
//!         socket.write_packet(&packet).await?;
//!     }
//!     Ok(())
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

mod addr;
#[cfg(feature = "isotp-userspace")]
mod channel;
mod error;
#[cfg(target_os = "linux")]
mod kernel;
#[cfg(target_os = "linux")]
mod socketcan_isotp;
mod types;
#[cfg(feature = "isotp-userspace")]
mod userspace;

pub use crate::error::Error;
pub use crate::types::{
    AF_CAN, CAN_ISOTP, CAN_ISOTP_LL_OPTS, CAN_ISOTP_OPTS, CAN_ISOTP_RECV_FC, CAN_ISOTP_RX_STMIN,
    CAN_ISOTP_TX_STMIN, CAN_MAX_DLEN, EFF_FLAG, EFF_MASK, ERR_FLAG, ERR_MASK, ERR_MASK_ALL,
    ERR_MASK_NONE, ExtendedId, FlowControlOptions, Id, IsoTpBehaviour, IsoTpOptions,
    LinkLayerOptions, PF_CAN, RECV_BUFFER_SIZE, RTR_FLAG, SFF_MASK, SOL_CAN_BASE, SOL_CAN_ISOTP,
    StandardId, TxFlags,
};

use std::io;
#[cfg(target_os = "linux")]
use std::os::raw::c_int;

enum Inner {
    #[cfg(target_os = "linux")]
    Kernel(Box<kernel::KernelIsoTp>),
    #[cfg(feature = "isotp-userspace")]
    Userspace(userspace::UserspaceIsoTp),
    /// Present so the enum is never empty on builds without any backend
    /// (non-Linux without the `tcp` feature); cannot be constructed.
    #[cfg(not(target_os = "linux"))]
    Unsupported(std::convert::Infallible),
}

/// An asynchronous ISO-TP socket.
///
/// For reading and writing complete ISO-TP payloads use
/// [IsoTpSocket::read_packet] and [IsoTpSocket::write_packet] respectively.
pub struct IsoTpSocket(Inner);

impl IsoTpSocket {
    /// Open a socket with default options. See the crate documentation for
    /// the supported address forms (`vcan0`, `rawcan:vcan0`, `tcp:...`).
    pub fn open(address: &str, src: impl Into<Id>, dst: impl Into<Id>) -> Result<Self, Error> {
        Self::open_with_opts(
            address,
            src,
            dst,
            Some(IsoTpOptions::default()),
            Some(FlowControlOptions::default()),
            Some(LinkLayerOptions::default()),
        )
    }

    /// Open a socket, passing additional options.
    ///
    /// Backend-specific behavior:
    /// - kernel backend (plain interface name): all options are handed to the
    ///   kernel as socket options, exactly as before.
    /// - userspace backends (`rawcan:`, `tcp:`, `tcplisten:`): see the
    ///   `isotp-userspace` feature documentation for which options are
    ///   honored; extended addressing and listen mode are rejected.
    #[cfg_attr(not(target_os = "linux"), allow(unused_variables))]
    pub fn open_with_opts(
        address: &str,
        src: impl Into<Id>,
        dst: impl Into<Id>,
        isotp_options: Option<IsoTpOptions>,
        rx_flow_control_options: Option<FlowControlOptions>,
        link_layer_options: Option<LinkLayerOptions>,
    ) -> Result<Self, Error> {
        let src: Id = src.into();
        let dst: Id = dst.into();
        match addr::parse(address)? {
            addr::Address::Kernel(ifname) => {
                #[cfg(target_os = "linux")]
                {
                    let sock = socketcan_isotp::IsoTpSocket::open_with_opts(
                        ifname,
                        src,
                        dst,
                        isotp_options,
                        rx_flow_control_options,
                        link_layer_options,
                    )?;
                    Ok(IsoTpSocket(Inner::Kernel(Box::new(
                        kernel::KernelIsoTp::new(sock)?,
                    ))))
                }
                #[cfg(not(target_os = "linux"))]
                {
                    Err(Error::Address(format!(
                        "kernel ISO-TP backend for '{ifname}' is only available on Linux"
                    )))
                }
            }
            addr::Address::RawCan(ifname) => {
                #[cfg(all(target_os = "linux", feature = "isotp-userspace"))]
                {
                    let chan = channel::FrameChannel::RawCan(channel::rawcan::RawCanChannel::open(
                        ifname, src,
                    )?);
                    Ok(IsoTpSocket(Inner::Userspace(
                        userspace::UserspaceIsoTp::new(
                            chan,
                            src,
                            dst,
                            isotp_options,
                            rx_flow_control_options,
                        )?,
                    )))
                }
                #[cfg(not(all(target_os = "linux", feature = "isotp-userspace")))]
                {
                    Err(Error::Address(format!(
                        "'rawcan:{ifname}' requires the `isotp-userspace` feature on Linux"
                    )))
                }
            }
            addr::Address::TcpConnect(endpoint) => {
                #[cfg(feature = "tcp")]
                {
                    let chan = channel::FrameChannel::Tcp(channel::tcp::TcpClientChannel::connect(
                        endpoint,
                    )?);
                    Ok(IsoTpSocket(Inner::Userspace(
                        userspace::UserspaceIsoTp::new(
                            chan,
                            src,
                            dst,
                            isotp_options,
                            rx_flow_control_options,
                        )?,
                    )))
                }
                #[cfg(not(feature = "tcp"))]
                {
                    Err(Error::Address(format!(
                        "'tcp:{endpoint}' requires the `tcp` feature"
                    )))
                }
            }
            addr::Address::TcpListen(endpoint) => {
                #[cfg(feature = "tcp")]
                {
                    let chan = channel::FrameChannel::Hub(channel::tcp::hub_port(endpoint)?);
                    Ok(IsoTpSocket(Inner::Userspace(
                        userspace::UserspaceIsoTp::new(
                            chan,
                            src,
                            dst,
                            isotp_options,
                            rx_flow_control_options,
                        )?,
                    )))
                }
                #[cfg(not(feature = "tcp"))]
                {
                    Err(Error::Address(format!(
                        "'tcplisten:{endpoint}' requires the `tcp` feature"
                    )))
                }
            }
            addr::Address::Socketcand { endpoint, bus } => {
                #[cfg(feature = "socketcand")]
                {
                    let chan = channel::FrameChannel::Socketcand(
                        channel::socketcand::SocketcandChannel::connect(endpoint, bus)?,
                    );
                    Ok(IsoTpSocket(Inner::Userspace(
                        userspace::UserspaceIsoTp::new(
                            chan,
                            src,
                            dst,
                            isotp_options,
                            rx_flow_control_options,
                        )?,
                    )))
                }
                #[cfg(not(feature = "socketcand"))]
                {
                    Err(Error::Address(format!(
                        "'socketcand:{endpoint}:{bus}' requires the `socketcand` feature"
                    )))
                }
            }
        }
    }

    /// Open a kernel ISO-TP socket by kernel interface number.
    #[cfg(target_os = "linux")]
    pub fn open_if(if_index: c_int, src: impl Into<Id>, dst: impl Into<Id>) -> Result<Self, Error> {
        let sock = socketcan_isotp::IsoTpSocket::open_if(if_index, src, dst)?;
        Ok(IsoTpSocket(Inner::Kernel(Box::new(
            kernel::KernelIsoTp::new(sock)?,
        ))))
    }

    /// Open a kernel ISO-TP socket by kernel interface number, passing
    /// additional options.
    #[cfg(target_os = "linux")]
    pub fn open_if_with_opts(
        if_index: c_int,
        src: impl Into<Id>,
        dst: impl Into<Id>,
        isotp_options: Option<IsoTpOptions>,
        rx_flow_control_options: Option<FlowControlOptions>,
        link_layer_options: Option<LinkLayerOptions>,
    ) -> Result<Self, Error> {
        let sock = socketcan_isotp::IsoTpSocket::open_if_with_opts(
            if_index,
            src,
            dst,
            isotp_options,
            rx_flow_control_options,
            link_layer_options,
        )?;
        Ok(IsoTpSocket(Inner::Kernel(Box::new(
            kernel::KernelIsoTp::new(sock)?,
        ))))
    }

    /// Write a complete ISO-TP payload.
    #[cfg_attr(
        not(any(target_os = "linux", feature = "isotp-userspace")),
        allow(unused_variables)
    )]
    pub async fn write_packet(&self, packet: &[u8]) -> io::Result<()> {
        match &self.0 {
            #[cfg(target_os = "linux")]
            Inner::Kernel(sock) => sock.write_packet(packet).await,
            #[cfg(feature = "isotp-userspace")]
            Inner::Userspace(sock) => sock.write_packet(packet).await,
            #[cfg(not(target_os = "linux"))]
            Inner::Unsupported(never) => match *never {},
        }
    }

    /// Read a complete ISO-TP payload.
    pub async fn read_packet(&self) -> io::Result<Vec<u8>> {
        match &self.0 {
            #[cfg(target_os = "linux")]
            Inner::Kernel(sock) => sock.read_packet().await,
            #[cfg(feature = "isotp-userspace")]
            Inner::Userspace(sock) => sock.read_packet().await,
            #[cfg(not(target_os = "linux"))]
            Inner::Unsupported(never) => match *never {},
        }
    }
}
