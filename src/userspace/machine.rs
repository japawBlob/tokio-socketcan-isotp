//! Userspace implementation of the ISO 15765-2 transport protocol
//! (classic CAN 2.0 and CAN FD, normal addressing).
//!
//! Implemented: single frames, first/consecutive frames, flow control
//! (CTS / WAIT / OVERFLOW) with block size and STmin pacing, TX padding,
//! N_Bs / N_Cr timeouts, 11-bit and 29-bit ids, and the ISO 15765-2:2016
//! extensions used with CAN FD: configurable TX link layer data length
//! (`tx_dl` 8..=64), escaped single frames (`SF_DL` > 7), escaped first
//! frames (32-bit `FF_DL`, payloads > 4095 bytes), and padding to valid CAN
//! FD frame lengths. The RX path accepts all link layer data lengths,
//! independent of the TX configuration (like the Linux kernel).
//!
//! Not implemented (rejected or ignored at socket creation): extended
//! addressing, listen mode, RX padding checks, wait-frame emission (received
//! wait frames are honored).

use crate::channel::{CAN_MAX_DLEN, CANFD_MAX_DLEN, CanFrame, FrameChannel, fd_frame_len};
use crate::types::{ERR_FLAG, RTR_FLAG};
use std::io;
use std::time::Duration;
use tokio::time::timeout;

const PCI_MASK: u8 = 0xF0;
const PCI_SF: u8 = 0x00;
const PCI_FF: u8 = 0x10;
const PCI_CF: u8 = 0x20;
const PCI_FC: u8 = 0x30;

const FC_CTS: u8 = 0;
const FC_WAIT: u8 = 1;
const FC_OVERFLOW: u8 = 2;

/// Max payload of a single frame with the 4-bit `SF_DL` encoding.
const SF_MAX_4BIT: usize = 7;
/// Max ISO-TP payload with the 12-bit `FF_DL` encoding; larger payloads use
/// the 32-bit escape encoding.
const MAX_12BIT_PAYLOAD: usize = 4095;
/// Max ISO-TP payload accepted on the RX path (guards memory against bogus
/// 32-bit first frame lengths); larger transfers are answered with FC
/// OVERFLOW.
const MAX_RX_PAYLOAD: usize = 1 << 20;
/// Padding byte used for mandatory CAN FD padding when TX padding is off
/// (`CAN_ISOTP_DEFAULT_PAD_CONTENT`).
const DEFAULT_PAD_CONTENT: u8 = 0xCC;

/// Timeout waiting for a flow control frame after FF / a block (N_Bs).
const N_BS_TIMEOUT: Duration = Duration::from_millis(1000);
/// Timeout waiting for the next consecutive frame (N_Cr).
const N_CR_TIMEOUT: Duration = Duration::from_millis(1000);
/// Max number of FC WAIT frames accepted before giving up.
const MAX_WAIT_FRAMES: u32 = 16;

/// Static per-socket configuration of the state machine.
pub(crate) struct IsoTpConfig {
    pub tx_id: u32,
    pub rx_id: u32,
    /// `Some(pad byte)`: pad all transmitted frames to at least 8 bytes (and
    /// to the next valid CAN FD length).
    pub tx_padding: Option<u8>,
    /// Minimum gap between transmitted consecutive frames (N_As).
    pub frame_txtime: Duration,
    /// Block size advertised in flow control frames we emit (0 = no limit).
    pub fc_bs: u8,
    /// STmin advertised in flow control frames we emit (raw encoding).
    pub fc_stmin: u8,
    /// TX link layer data length: max data bytes per transmitted frame (8
    /// for classic CAN; 8, 12, 16, 20, 24, 32, 48 or 64 for CAN FD).
    pub tx_dl: usize,
    /// Transmit CAN FD frames (link layer MTU `CANFD_MTU`).
    pub fd: bool,
    /// Set the bit rate switch flag on transmitted CAN FD frames.
    pub brs: bool,
}

fn decode_stmin(stmin: u8) -> Duration {
    match stmin {
        // 0x00-0x7F: 0-127 ms
        0x00..=0x7F => Duration::from_millis(u64::from(stmin)),
        // 0xF1-0xF9: 100-900 us
        0xF1..=0xF9 => Duration::from_micros(100 * u64::from(stmin - 0xF0)),
        // reserved: max value per ISO 15765-2
        _ => Duration::from_millis(0x7F),
    }
}

fn invalid_data(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_owned())
}

fn timed_out(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, msg.to_owned())
}

impl IsoTpConfig {
    /// Build a frame carrying `payload` (PCI + data, at most `tx_dl` bytes),
    /// applying TX padding and the mandatory padding to a valid CAN FD frame
    /// length.
    fn frame(&self, payload: &[u8]) -> CanFrame {
        debug_assert!(payload.len() <= self.tx_dl);
        let len = payload.len();
        let (frame_len, pad) = match self.tx_padding {
            // Like the kernel: pad to at least 8 bytes, and up to the next
            // valid CAN FD frame length beyond that.
            Some(pad) => (fd_frame_len(len.max(CAN_MAX_DLEN)), pad),
            // Mandatory padding for CAN FD frames longer than 8 bytes.
            None => (fd_frame_len(len), DEFAULT_PAD_CONTENT),
        };
        let mut data = [pad; CANFD_MAX_DLEN];
        data[..len].copy_from_slice(payload);
        CanFrame::with_flags(self.tx_id, &data[..frame_len], self.fd, self.brs)
    }

    /// Max payload of an escaped single frame (`0x00`, `SF_DL`, data).
    fn sf_max(&self) -> usize {
        if self.tx_dl > CAN_MAX_DLEN {
            self.tx_dl - 2
        } else {
            SF_MAX_4BIT
        }
    }
}

/// Receive the next frame addressed to us, skipping foreign ids and
/// RTR/error frames.
async fn recv_matching(chan: &mut FrameChannel, rx_id: u32) -> io::Result<CanFrame> {
    loop {
        let frame = chan.recv().await?;
        if frame.can_id != rx_id || frame.can_id & (RTR_FLAG | ERR_FLAG) != 0 {
            continue;
        }
        if frame.len == 0 {
            continue;
        }
        return Ok(frame);
    }
}

/// Wait for a flow control frame; returns `(block size, decoded STmin)`.
/// Non-FC frames received while waiting are dropped, matching the kernel.
async fn wait_for_fc(chan: &mut FrameChannel, rx_id: u32) -> io::Result<(u8, Duration)> {
    let mut waits = 0u32;
    loop {
        let frame = timeout(N_BS_TIMEOUT, recv_matching(chan, rx_id))
            .await
            .map_err(|_| timed_out("timed out waiting for ISO-TP flow control (N_Bs)"))??;
        if frame.data[0] & PCI_MASK != PCI_FC {
            continue;
        }
        if frame.len < 3 {
            return Err(invalid_data("short ISO-TP flow control frame"));
        }
        match frame.data[0] & 0x0F {
            FC_CTS => return Ok((frame.data[1], decode_stmin(frame.data[2]))),
            FC_WAIT => {
                waits += 1;
                if waits > MAX_WAIT_FRAMES {
                    return Err(timed_out("too many ISO-TP flow control wait frames"));
                }
            }
            FC_OVERFLOW => {
                return Err(invalid_data("ISO-TP receiver reported buffer overflow"));
            }
            _ => return Err(invalid_data("invalid ISO-TP flow control status")),
        }
    }
}

pub(crate) async fn send(
    chan: &mut FrameChannel,
    cfg: &IsoTpConfig,
    data: &[u8],
) -> io::Result<()> {
    if data.is_empty() {
        // The kernel rejects zero-length writes as well.
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "empty ISO-TP payload",
        ));
    }
    let mut buf = [0u8; CANFD_MAX_DLEN];

    // Single frame: 4-bit SF_DL if it fits a classic frame, otherwise the
    // CAN FD escape encoding (0x00, SF_DL) when the TX_DL allows it.
    if data.len() <= SF_MAX_4BIT {
        buf[0] = PCI_SF | data.len() as u8;
        buf[1..=data.len()].copy_from_slice(data);
        return chan.send(cfg.frame(&buf[..=data.len()])).await;
    }
    if data.len() <= cfg.sf_max() {
        buf[0] = PCI_SF;
        buf[1] = data.len() as u8;
        buf[2..2 + data.len()].copy_from_slice(data);
        return chan.send(cfg.frame(&buf[..2 + data.len()])).await;
    }

    // First frame: 12-bit FF_DL, or the 32-bit escape for > 4095 bytes. It
    // always fills the whole TX_DL.
    let header = if data.len() <= MAX_12BIT_PAYLOAD {
        buf[0] = PCI_FF | ((data.len() >> 8) as u8 & 0x0F);
        buf[1] = (data.len() & 0xFF) as u8;
        2
    } else {
        let total = u32::try_from(data.len()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("ISO-TP payload too large: {} bytes", data.len()),
            )
        })?;
        buf[0] = PCI_FF;
        buf[1] = 0;
        buf[2..6].copy_from_slice(&total.to_be_bytes());
        6
    };
    let first_take = cfg.tx_dl - header;
    buf[header..cfg.tx_dl].copy_from_slice(&data[..first_take]);
    chan.send(cfg.frame(&buf[..cfg.tx_dl])).await?;

    let cf_max = cfg.tx_dl - 1;
    let (mut bs, mut stmin) = wait_for_fc(chan, cfg.rx_id).await?;
    let mut sent = first_take;
    let mut sequence = 1u8;
    let mut in_block = 0u32;
    while sent < data.len() {
        if bs != 0 && in_block == u32::from(bs) {
            (bs, stmin) = wait_for_fc(chan, cfg.rx_id).await?;
            in_block = 0;
        }
        let take = cf_max.min(data.len() - sent);
        buf[0] = PCI_CF | sequence;
        buf[1..=take].copy_from_slice(&data[sent..sent + take]);
        chan.send(cfg.frame(&buf[..=take])).await?;
        sent += take;
        sequence = (sequence + 1) & 0x0F;
        in_block += 1;
        if sent < data.len() {
            let gap = stmin.max(cfg.frame_txtime);
            if !gap.is_zero() {
                tokio::time::sleep(gap).await;
            }
        }
    }
    Ok(())
}

/// Decode a single frame; `None` for malformed frames (dropped, like the
/// kernel).
fn decode_single_frame(frame: &CanFrame) -> Option<&[u8]> {
    let frame_len = frame.len as usize;
    let nibble = (frame.data[0] & 0x0F) as usize;
    if nibble != 0 {
        // 4-bit SF_DL: only valid in frames of up to 8 bytes.
        if frame_len > CAN_MAX_DLEN || nibble >= frame_len {
            return None;
        }
        return Some(&frame.data[1..=nibble]);
    }
    // Escape encoding (0x00, SF_DL): only valid in frames longer than 8
    // bytes.
    if frame_len <= CAN_MAX_DLEN {
        return None;
    }
    let len = frame.data[1] as usize;
    if len == 0 || 2 + len > frame_len {
        return None;
    }
    Some(&frame.data[2..2 + len])
}

pub(crate) async fn recv(chan: &mut FrameChannel, cfg: &IsoTpConfig) -> io::Result<Vec<u8>> {
    loop {
        let frame = recv_matching(chan, cfg.rx_id).await?;
        match frame.data[0] & PCI_MASK {
            PCI_SF => match decode_single_frame(&frame) {
                Some(payload) => return Ok(payload.to_vec()),
                None => continue,
            },
            PCI_FF => return recv_segmented(chan, cfg, &frame).await,
            // Stray CF (aborted transfer) or stale FC: drop.
            _ => continue,
        }
    }
}

async fn recv_segmented(
    chan: &mut FrameChannel,
    cfg: &IsoTpConfig,
    first: &CanFrame,
) -> io::Result<Vec<u8>> {
    // The sender's link layer data length is the length of its first frame.
    let rx_dl = first.len as usize;
    if rx_dl < CAN_MAX_DLEN || fd_frame_len(rx_dl) != rx_dl {
        return Err(invalid_data("invalid ISO-TP first frame length"));
    }
    let total12 = (((first.data[0] & 0x0F) as usize) << 8) | first.data[1] as usize;
    let (total, header) = if total12 != 0 {
        (total12, 2)
    } else {
        let total =
            u32::from_be_bytes([first.data[2], first.data[3], first.data[4], first.data[5]]);
        (total as usize, 6)
    };
    // A first frame must not carry data that would have fit a single frame
    // (same check as the kernel).
    if total < CAN_MAX_DLEN || total + header < rx_dl {
        return Err(invalid_data("invalid ISO-TP first frame length"));
    }
    if total > MAX_RX_PAYLOAD {
        let overflow = [PCI_FC | FC_OVERFLOW, 0, 0];
        chan.send(cfg.frame(&overflow)).await?;
        return Err(invalid_data("ISO-TP payload exceeds the receive buffer"));
    }

    let mut buf = Vec::with_capacity(total);
    buf.extend_from_slice(&first.data[header..rx_dl]);

    let fc = [PCI_FC | FC_CTS, cfg.fc_bs, cfg.fc_stmin];
    chan.send(cfg.frame(&fc)).await?;

    let cf_max = rx_dl - 1;
    let mut sequence = 1u8;
    let mut in_block = 0u32;
    while buf.len() < total {
        let frame = timeout(N_CR_TIMEOUT, recv_matching(chan, cfg.rx_id))
            .await
            .map_err(|_| timed_out("timed out waiting for ISO-TP consecutive frame (N_Cr)"))??;
        if frame.data[0] & PCI_MASK != PCI_CF {
            return Err(invalid_data("expected ISO-TP consecutive frame"));
        }
        if frame.data[0] & 0x0F != sequence {
            return Err(invalid_data("ISO-TP consecutive frame out of sequence"));
        }
        let take = cf_max.min(total - buf.len());
        if (frame.len as usize) < 1 + take {
            return Err(invalid_data("short ISO-TP consecutive frame"));
        }
        buf.extend_from_slice(&frame.data[1..=take]);
        sequence = (sequence + 1) & 0x0F;
        in_block += 1;
        if cfg.fc_bs != 0 && in_block == u32::from(cfg.fc_bs) && buf.len() < total {
            chan.send(cfg.frame(&fc)).await?;
            in_block = 0;
        }
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::mem;

    fn config(tx_id: u32, rx_id: u32) -> IsoTpConfig {
        IsoTpConfig {
            tx_id,
            rx_id,
            tx_padding: None,
            frame_txtime: Duration::ZERO,
            fc_bs: 0,
            fc_stmin: 0,
            tx_dl: 8,
            fd: false,
            brs: false,
        }
    }

    fn fd_config(tx_id: u32, rx_id: u32, tx_dl: usize) -> IsoTpConfig {
        IsoTpConfig {
            tx_dl,
            fd: true,
            brs: true,
            ..config(tx_id, rx_id)
        }
    }

    fn chan_pair() -> (FrameChannel, FrameChannel) {
        let (a, b) = mem::pair();
        (FrameChannel::Mem(a), FrameChannel::Mem(b))
    }

    #[test]
    fn stmin_decoding() {
        assert_eq!(decode_stmin(0x00), Duration::ZERO);
        assert_eq!(decode_stmin(0x7F), Duration::from_millis(127));
        assert_eq!(decode_stmin(0xF1), Duration::from_micros(100));
        assert_eq!(decode_stmin(0xF9), Duration::from_micros(900));
        assert_eq!(decode_stmin(0x80), Duration::from_millis(127));
        assert_eq!(decode_stmin(0xFF), Duration::from_millis(127));
    }

    #[tokio::test]
    async fn single_frame_is_padded() {
        let (mut tx_chan, mut peer) = chan_pair();
        let mut cfg = config(0x7E0, 0x7E8);
        cfg.tx_padding = Some(0xAA);
        send(&mut tx_chan, &cfg, &[0x3E, 0x00]).await.unwrap();
        let frame = peer.recv().await.unwrap();
        assert_eq!(frame.can_id, 0x7E0);
        assert_eq!(frame.len, 8);
        assert_eq!(
            frame.payload(),
            [0x02, 0x3E, 0x00, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA]
        );
    }

    #[tokio::test]
    async fn multi_frame_respects_block_size() {
        let (mut tx_chan, mut peer) = chan_pair();
        let cfg = config(0x7E0, 0x7E8);
        let payload: Vec<u8> = (0..20).collect();
        let sender = tokio::spawn(async move {
            send(&mut tx_chan, &cfg, &payload).await.unwrap();
            tx_chan
        });

        // Expect FF, answer with FC CTS bs=1, then one CF per FC.
        let ff = peer.recv().await.unwrap();
        assert_eq!(ff.data[0], PCI_FF | 0x00);
        assert_eq!(ff.data[1], 20);
        let mut received = ff.data[2..8].to_vec();
        let mut fc_count = 0;
        while received.len() < 20 {
            peer.send(CanFrame::new(0x7E8, &[PCI_FC | FC_CTS, 1, 0]))
                .await
                .unwrap();
            fc_count += 1;
            let cf = peer.recv().await.unwrap();
            assert_eq!(cf.data[0] & PCI_MASK, PCI_CF);
            let take = 7.min(20 - received.len());
            received.extend_from_slice(&cf.data[1..=take]);
        }
        assert_eq!(fc_count, 2);
        assert_eq!(received, (0..20).collect::<Vec<u8>>());
        sender.await.unwrap();
    }

    #[tokio::test]
    async fn fc_timeout_errors() {
        let (mut tx_chan, _peer) = chan_pair();
        let cfg = config(0x7E0, 0x7E8);
        let payload = [0u8; 20];
        let err = send(&mut tx_chan, &cfg, &payload).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    }

    #[tokio::test]
    async fn fc_overflow_errors() {
        let (mut tx_chan, mut peer) = chan_pair();
        let cfg = config(0x7E0, 0x7E8);
        let relay = tokio::spawn(async move {
            let _ff = peer.recv().await.unwrap();
            peer.send(CanFrame::new(0x7E8, &[PCI_FC | FC_OVERFLOW, 0, 0]))
                .await
                .unwrap();
            peer
        });
        let err = send(&mut tx_chan, &cfg, &[0u8; 20]).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        relay.await.unwrap();
    }

    #[tokio::test]
    async fn fc_wait_then_cts() {
        let (mut tx_chan, mut peer) = chan_pair();
        let cfg = config(0x7E0, 0x7E8);
        let relay = tokio::spawn(async move {
            let _ff = peer.recv().await.unwrap();
            peer.send(CanFrame::new(0x7E8, &[PCI_FC | FC_WAIT, 0, 0]))
                .await
                .unwrap();
            peer.send(CanFrame::new(0x7E8, &[PCI_FC | FC_CTS, 0, 0]))
                .await
                .unwrap();
            let mut frames = Vec::new();
            for _ in 0..2 {
                frames.push(peer.recv().await.unwrap());
            }
            frames
        });
        send(&mut tx_chan, &cfg, &[7u8; 20]).await.unwrap();
        let frames = relay.await.unwrap();
        assert_eq!(frames.len(), 2);
    }

    #[tokio::test]
    async fn wrong_sequence_number_errors() {
        let (mut rx_chan, mut peer) = chan_pair();
        let cfg = config(0x7E8, 0x7E0);
        let relay = tokio::spawn(async move {
            let mut ff = [0u8; 8];
            ff[0] = PCI_FF;
            ff[1] = 20;
            peer.send(CanFrame::new(0x7E0, &ff)).await.unwrap();
            let _fc = peer.recv().await.unwrap();
            // Wrong sequence number: 2 instead of 1.
            peer.send(CanFrame::new(0x7E0, &[PCI_CF | 2, 0, 0, 0, 0, 0, 0, 0]))
                .await
                .unwrap();
        });
        let err = recv(&mut rx_chan, &cfg).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        relay.await.unwrap();
    }

    async fn round_trip(len: usize) {
        let (mut a, mut b) = chan_pair();
        let cfg_a = IsoTpConfig {
            tx_padding: Some(0x00),
            ..config(0x7E0, 0x7E8)
        };
        let cfg_b = IsoTpConfig {
            fc_bs: 4,
            ..config(0x7E8, 0x7E0)
        };
        let payload: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        let expected = payload.clone();
        let receiver = tokio::spawn(async move { recv(&mut b, &cfg_b).await.unwrap() });
        send(&mut a, &cfg_a, &payload).await.unwrap();
        assert_eq!(receiver.await.unwrap(), expected);
    }

    #[tokio::test]
    async fn round_trips() {
        for len in [1usize, 6, 7, 8, 13, 62, 4095] {
            round_trip(len).await;
        }
    }

    #[tokio::test]
    async fn max_payload_in_one_unlimited_block() {
        // bs = 0 means a single block of 585 consecutive frames; the block
        // counter must not overflow (regression: was a u8).
        let (mut a, mut b) = chan_pair();
        let cfg_a = config(0x7E0, 0x7E8);
        let cfg_b = config(0x7E8, 0x7E0);
        let payload = vec![0x55u8; 4095];
        let expected = payload.clone();
        let receiver = tokio::spawn(async move { recv(&mut b, &cfg_b).await.unwrap() });
        send(&mut a, &cfg_a, &payload).await.unwrap();
        assert_eq!(receiver.await.unwrap(), expected);
    }

    #[tokio::test]
    async fn zero_length_payload_is_sent_as_empty_sf() {
        // A 0-byte SF is degenerate; the kernel rejects writes of len 0.
        // We mirror that by sending nothing and erroring.
        let (mut tx_chan, _peer) = chan_pair();
        let cfg = config(0x7E0, 0x7E8);
        let err = send(&mut tx_chan, &cfg, &[]).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[tokio::test]
    async fn foreign_ids_are_ignored() {
        let (mut rx_chan, mut peer) = chan_pair();
        let cfg = config(0x7E8, 0x7E0);
        let relay = tokio::spawn(async move {
            // Noise on other ids must be skipped.
            peer.send(CanFrame::new(0x123, &[0x02, 0xAA, 0xBB]))
                .await
                .unwrap();
            peer.send(CanFrame::new(0x7E0, &[0x02, 0x10, 0x03]))
                .await
                .unwrap();
        });
        let data = recv(&mut rx_chan, &cfg).await.unwrap();
        assert_eq!(data, vec![0x10, 0x03]);
        relay.await.unwrap();
    }

    #[tokio::test]
    async fn classic_payload_above_4095_uses_escaped_first_frame() {
        let (mut a, mut b) = chan_pair();
        let cfg_a = config(0x7E0, 0x7E8);
        let cfg_b = config(0x7E8, 0x7E0);
        let payload: Vec<u8> = (0..5000).map(|i| (i % 253) as u8).collect();
        let expected = payload.clone();
        let receiver = tokio::spawn(async move { recv(&mut b, &cfg_b).await.unwrap() });
        send(&mut a, &cfg_a, &payload).await.unwrap();
        assert_eq!(receiver.await.unwrap(), expected);
    }

    #[tokio::test]
    async fn fd_short_single_frame_uses_4bit_length() {
        let (mut tx_chan, mut peer) = chan_pair();
        let mut cfg = fd_config(0x7E0, 0x7E8, 64);
        cfg.tx_padding = Some(0x55);
        send(&mut tx_chan, &cfg, &[0x22, 0xF1, 0x90]).await.unwrap();
        let frame = peer.recv().await.unwrap();
        assert!(frame.fd);
        assert!(frame.brs);
        assert_eq!(
            frame.payload(),
            [0x03, 0x22, 0xF1, 0x90, 0x55, 0x55, 0x55, 0x55]
        );
    }

    #[tokio::test]
    async fn fd_escaped_single_frame_is_padded_to_valid_length() {
        let (mut tx_chan, mut peer) = chan_pair();
        let cfg = fd_config(0x7E0, 0x7E8, 64);
        let payload: Vec<u8> = (0..20).collect();
        send(&mut tx_chan, &cfg, &payload).await.unwrap();
        let frame = peer.recv().await.unwrap();
        // 2 PCI bytes + 20 data bytes -> next valid CAN FD length 24, with
        // mandatory 0xCC padding.
        assert_eq!(frame.len, 24);
        assert!(frame.fd);
        assert_eq!(&frame.payload()[..2], &[0x00, 20]);
        assert_eq!(&frame.payload()[2..22], payload.as_slice());
        assert_eq!(&frame.payload()[22..], &[0xCC, 0xCC]);
    }

    #[tokio::test]
    async fn fd_single_frame_limited_by_tx_dl() {
        // With tx_dl = 16 the escaped SF carries at most 14 bytes; 15 bytes
        // need a first frame.
        let (mut tx_chan, mut peer) = chan_pair();
        let cfg = fd_config(0x7E0, 0x7E8, 16);
        let sender =
            tokio::spawn(async move { send(&mut tx_chan, &cfg, &[0xAB; 15]).await.unwrap_err() });
        let ff = peer.recv().await.unwrap();
        assert_eq!(ff.len, 16);
        assert_eq!(&ff.payload()[..2], &[PCI_FF, 15]);
        // No flow control -> N_Bs timeout.
        assert_eq!(sender.await.unwrap().kind(), io::ErrorKind::TimedOut);
    }

    #[tokio::test]
    async fn receives_escaped_single_frame_from_ecu() {
        // Real response captured on a CAN FD bench (PES297M, 22 F1 90).
        let (mut rx_chan, mut peer) = chan_pair();
        let cfg = config(0x7E8, 0x7E0);
        let mut data = vec![0x00, 0x14, 0x62, 0xF1, 0x90];
        data.extend_from_slice(&[0x3F; 17]);
        data.extend_from_slice(&[0x00, 0x00]);
        peer.send(CanFrame::with_flags(0x7E0, &data, true, true))
            .await
            .unwrap();
        let payload = recv(&mut rx_chan, &cfg).await.unwrap();
        assert_eq!(payload.len(), 20);
        assert_eq!(&payload[..3], &[0x62, 0xF1, 0x90]);
    }

    #[tokio::test]
    async fn malformed_single_frames_are_dropped() {
        let (mut rx_chan, mut peer) = chan_pair();
        let cfg = config(0x7E8, 0x7E0);
        let relay = tokio::spawn(async move {
            // Escape encoding in a classic frame: invalid.
            peer.send(CanFrame::new(0x7E0, &[0x00, 0x03, 1, 2, 3, 0, 0, 0]))
                .await
                .unwrap();
            // 4-bit SF_DL in a frame longer than 8 bytes: invalid.
            peer.send(CanFrame::with_flags(
                0x7E0,
                &[0x02, 1, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0],
                true,
                false,
            ))
            .await
            .unwrap();
            // Escaped SF_DL exceeding the frame: invalid.
            let mut long = [0u8; 12];
            long[1] = 11;
            peer.send(CanFrame::with_flags(0x7E0, &long, true, false))
                .await
                .unwrap();
            peer.send(CanFrame::new(0x7E0, &[0x01, 0x42]))
                .await
                .unwrap();
        });
        assert_eq!(recv(&mut rx_chan, &cfg).await.unwrap(), vec![0x42]);
        relay.await.unwrap();
    }

    async fn fd_round_trip(len: usize, tx_dl_a: usize, tx_dl_b: usize, padding: Option<u8>) {
        let (mut a, mut b) = chan_pair();
        let cfg_a = IsoTpConfig {
            tx_padding: padding,
            ..fd_config(0x7E0, 0x7E8, tx_dl_a)
        };
        let cfg_b = IsoTpConfig {
            fc_bs: 3,
            tx_padding: padding,
            ..fd_config(0x7E8, 0x7E0, tx_dl_b)
        };
        let payload: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        let expected = payload.clone();
        let receiver = tokio::spawn(async move { recv(&mut b, &cfg_b).await.unwrap() });
        send(&mut a, &cfg_a, &payload).await.unwrap();
        assert_eq!(
            receiver.await.unwrap(),
            expected,
            "len {len} tx_dl {tx_dl_a}"
        );
    }

    #[tokio::test]
    async fn fd_round_trips() {
        for tx_dl in [8, 12, 16, 20, 24, 32, 48, 64] {
            for len in [
                1, 7, 8, 10, 11, 61, 62, 63, 64, 100, 500, 4095, 4096, 10_000,
            ] {
                fd_round_trip(len, tx_dl, 64, None).await;
                fd_round_trip(len, tx_dl, 8, Some(0x55)).await;
            }
        }
    }

    #[tokio::test]
    async fn fd_frames_have_valid_lengths_and_cf_size() {
        let (mut a, mut b) = chan_pair();
        let cfg_a = fd_config(0x7E0, 0x7E8, 64);
        let payload = vec![0x11u8; 200];
        let sender = tokio::spawn(async move {
            send(&mut a, &cfg_a, &payload).await.unwrap();
        });
        let ff = b.recv().await.unwrap();
        assert_eq!(ff.len, 64);
        assert_eq!(&ff.payload()[..2], &[PCI_FF, 200]);
        b.send(CanFrame::new(0x7E8, &[PCI_FC | FC_CTS, 0, 0]))
            .await
            .unwrap();
        // 200 - 62 = 138 bytes in CFs of 63: 63 + 63 + 12.
        let lens: Vec<u8> = [b.recv().await, b.recv().await, b.recv().await]
            .into_iter()
            .map(|f| f.unwrap().len)
            .collect();
        // The last CF carries 13 bytes (PCI + 12) -> padded to 16.
        assert_eq!(lens, vec![64, 64, 16]);
        sender.await.unwrap();
    }

    #[tokio::test]
    async fn oversized_first_frame_is_rejected_with_overflow() {
        let (mut rx_chan, mut peer) = chan_pair();
        let cfg = config(0x7E8, 0x7E0);
        let relay = tokio::spawn(async move {
            let mut ff = [0u8; 8];
            ff[0] = PCI_FF;
            ff[2..6].copy_from_slice(&u32::MAX.to_be_bytes());
            peer.send(CanFrame::new(0x7E0, &ff)).await.unwrap();
            peer.recv().await.unwrap()
        });
        let err = recv(&mut rx_chan, &cfg).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        let fc = relay.await.unwrap();
        assert_eq!(fc.data[0], PCI_FC | FC_OVERFLOW);
    }
}
