//! Raw CAN frame channels for the userspace ISO-TP implementation.
//!
//! A [FrameChannel] moves classic CAN 2.0 frames (up to 8 data bytes)
//! between the local ISO-TP state machine and a peer. Backends are selected
//! by the address scheme: a raw SocketCAN socket (`rawcan:`), a TCP client
//! connection (`tcp:`), a port on a local TCP frame hub (`tcplisten:`), or
//! an in-memory pair (tests).

pub(crate) mod mem;
#[cfg(target_os = "linux")]
pub(crate) mod rawcan;

use std::io;

/// A classic CAN 2.0 frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CanFrame {
    /// Raw CAN id including the [crate::EFF_FLAG] bit for 29-bit ids.
    pub can_id: u32,
    /// Number of valid bytes in `data` (0..=8).
    pub len: u8,
    pub data: [u8; 8],
}

impl CanFrame {
    pub(crate) fn new(can_id: u32, payload: &[u8]) -> Self {
        debug_assert!(payload.len() <= 8);
        let mut data = [0u8; 8];
        data[..payload.len()].copy_from_slice(payload);
        CanFrame {
            can_id,
            len: payload.len() as u8,
            data,
        }
    }
}

/// Enum dispatch over the channel backends (no dyn: async trait methods are
/// not dyn-compatible on our pinned toolchain).
pub(crate) enum FrameChannel {
    Mem(mem::MemChannel),
    #[cfg(target_os = "linux")]
    RawCan(rawcan::RawCanChannel),
}

impl FrameChannel {
    pub(crate) async fn send(&mut self, frame: CanFrame) -> io::Result<()> {
        match self {
            FrameChannel::Mem(chan) => chan.send(frame),
            #[cfg(target_os = "linux")]
            FrameChannel::RawCan(chan) => chan.send(frame).await,
        }
    }

    pub(crate) async fn recv(&mut self) -> io::Result<CanFrame> {
        match self {
            FrameChannel::Mem(chan) => chan.recv().await,
            #[cfg(target_os = "linux")]
            FrameChannel::RawCan(chan) => chan.recv().await,
        }
    }
}
