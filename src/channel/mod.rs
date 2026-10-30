//! Raw CAN frame channels for the userspace ISO-TP implementation.
//!
//! A [FrameChannel] moves classic CAN 2.0 frames (up to 8 data bytes) and
//! CAN FD frames (up to 64 data bytes) between the local ISO-TP state
//! machine and a peer. Backends are selected by the address scheme: a raw
//! SocketCAN socket (`rawcan:`), a socketcand daemon (`socketcand:`), or an
//! in-memory pair (tests).

pub(crate) mod mem;
#[cfg(target_os = "linux")]
pub(crate) mod rawcan;
#[cfg(feature = "socketcand")]
pub(crate) mod socketcand;

use std::io;

/// Max data bytes of a classic CAN 2.0 frame.
pub(crate) const CAN_MAX_DLEN: usize = 8;
/// Max data bytes of a CAN FD frame.
pub(crate) const CANFD_MAX_DLEN: usize = 64;

/// A classic CAN 2.0 or CAN FD frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CanFrame {
    /// Raw CAN id including the [crate::EFF_FLAG] bit for 29-bit ids.
    pub can_id: u32,
    /// Number of valid bytes in `data` (0..=8 classic, 0..=64 CAN FD).
    pub len: u8,
    pub data: [u8; CANFD_MAX_DLEN],
    /// CAN FD frame (EDL). Frames with more than 8 data bytes are always FD.
    pub fd: bool,
    /// CAN FD bit rate switch. Only meaningful if `fd` is set.
    pub brs: bool,
}

impl CanFrame {
    /// A classic CAN frame (`payload.len() <= 8`).
    pub(crate) fn new(can_id: u32, payload: &[u8]) -> Self {
        debug_assert!(payload.len() <= CAN_MAX_DLEN);
        Self::with_flags(can_id, payload, false, false)
    }

    /// A frame with explicit CAN FD flags (`payload.len() <= 64`).
    pub(crate) fn with_flags(can_id: u32, payload: &[u8], fd: bool, brs: bool) -> Self {
        debug_assert!(payload.len() <= CANFD_MAX_DLEN);
        let mut data = [0u8; CANFD_MAX_DLEN];
        data[..payload.len()].copy_from_slice(payload);
        CanFrame {
            can_id,
            len: payload.len() as u8,
            data,
            fd: fd || payload.len() > CAN_MAX_DLEN,
            brs: brs && (fd || payload.len() > CAN_MAX_DLEN),
        }
    }

    /// The valid data bytes.
    pub(crate) fn payload(&self) -> &[u8] {
        &self.data[..self.len as usize]
    }
}

/// Smallest valid CAN FD data length (8, 12, 16, 20, 24, 32, 48, 64) that
/// can hold `len` bytes; lengths up to 8 are returned unchanged.
pub(crate) fn fd_frame_len(len: usize) -> usize {
    match len {
        0..=8 => len,
        9..=12 => 12,
        13..=16 => 16,
        17..=20 => 20,
        21..=24 => 24,
        25..=32 => 32,
        33..=48 => 48,
        _ => 64,
    }
}

/// Whether `len` is a valid CAN FD frame data length.
pub(crate) fn is_valid_fd_len(len: usize) -> bool {
    len <= CANFD_MAX_DLEN && fd_frame_len(len) == len
}

/// Enum dispatch over the channel backends (no dyn: async trait methods are
/// not dyn-compatible on our pinned toolchain).
pub(crate) enum FrameChannel {
    Mem(mem::MemChannel),
    #[cfg(target_os = "linux")]
    RawCan(rawcan::RawCanChannel),
    #[cfg(feature = "socketcand")]
    Socketcand(socketcand::SocketcandChannel),
}

impl FrameChannel {
    pub(crate) async fn send(&mut self, frame: CanFrame) -> io::Result<()> {
        match self {
            FrameChannel::Mem(chan) => chan.send(frame),
            #[cfg(target_os = "linux")]
            FrameChannel::RawCan(chan) => chan.send(frame).await,
            #[cfg(feature = "socketcand")]
            FrameChannel::Socketcand(chan) => chan.send(frame).await,
        }
    }

    pub(crate) async fn recv(&mut self) -> io::Result<CanFrame> {
        match self {
            FrameChannel::Mem(chan) => chan.recv().await,
            #[cfg(target_os = "linux")]
            FrameChannel::RawCan(chan) => chan.recv().await,
            #[cfg(feature = "socketcand")]
            FrameChannel::Socketcand(chan) => chan.recv().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fd_lengths() {
        assert_eq!(fd_frame_len(0), 0);
        assert_eq!(fd_frame_len(8), 8);
        assert_eq!(fd_frame_len(9), 12);
        assert_eq!(fd_frame_len(33), 48);
        assert_eq!(fd_frame_len(64), 64);
        for len in [0, 1, 7, 8, 12, 16, 20, 24, 32, 48, 64] {
            assert!(is_valid_fd_len(len), "{len}");
        }
        for len in [9, 13, 25, 63, 65] {
            assert!(!is_valid_fd_len(len), "{len}");
        }
    }

    #[test]
    fn long_frames_are_fd() {
        let frame = CanFrame::with_flags(0x7E0, &[0; 12], false, true);
        assert!(frame.fd);
        assert!(frame.brs);
        let frame = CanFrame::with_flags(0x7E0, &[0; 8], false, true);
        assert!(!frame.fd);
        assert!(!frame.brs);
    }
}
