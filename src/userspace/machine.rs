//! Userspace implementation of the ISO 15765-2 transport protocol
//! (classic CAN 2.0, normal addressing).
//!
//! Implemented: single frames, first/consecutive frames, flow control
//! (CTS / WAIT / OVERFLOW) with block size and STmin pacing, TX padding,
//! N_Bs / N_Cr timeouts, payloads up to 4095 bytes, 11-bit and 29-bit ids.
//!
//! Not implemented (rejected or ignored at socket creation): extended
//! addressing, listen mode, CAN FD frames, RX padding checks, wait-frame
//! emission (received wait frames are honored).

use crate::channel::{CanFrame, FrameChannel};
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

/// Max payload of a single frame (classic CAN, normal addressing).
const SF_MAX: usize = 7;
/// Max payload bytes per consecutive frame.
const CF_MAX: usize = 7;
/// Max ISO-TP payload with the 12-bit FF_DL encoding.
const MAX_PAYLOAD: usize = 4095;

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
    /// `Some(pad byte)`: pad all transmitted frames to 8 bytes.
    pub tx_padding: Option<u8>,
    /// Minimum gap between transmitted consecutive frames (N_As).
    pub frame_txtime: Duration,
    /// Block size advertised in flow control frames we emit (0 = no limit).
    pub fc_bs: u8,
    /// STmin advertised in flow control frames we emit (raw encoding).
    pub fc_stmin: u8,
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
    fn frame(&self, payload: &[u8]) -> CanFrame {
        let mut frame = CanFrame::new(self.tx_id, payload);
        if let Some(pad) = self.tx_padding {
            for byte in &mut frame.data[payload.len()..] {
                *byte = pad;
            }
            frame.len = 8;
        }
        frame
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
    if data.len() <= SF_MAX {
        let mut payload = [0u8; 8];
        payload[0] = PCI_SF | data.len() as u8;
        payload[1..=data.len()].copy_from_slice(data);
        return chan.send(cfg.frame(&payload[..=data.len()])).await;
    }
    if data.len() > MAX_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("ISO-TP payload too large: {} > {MAX_PAYLOAD}", data.len()),
        ));
    }

    let mut first = [0u8; 8];
    first[0] = PCI_FF | ((data.len() >> 8) as u8 & 0x0F);
    first[1] = (data.len() & 0xFF) as u8;
    first[2..8].copy_from_slice(&data[..6]);
    chan.send(cfg.frame(&first)).await?;

    let (mut bs, mut stmin) = wait_for_fc(chan, cfg.rx_id).await?;
    let mut sent = 6usize;
    let mut sequence = 1u8;
    let mut in_block = 0u8;
    while sent < data.len() {
        if bs != 0 && in_block == bs {
            (bs, stmin) = wait_for_fc(chan, cfg.rx_id).await?;
            in_block = 0;
        }
        let take = CF_MAX.min(data.len() - sent);
        let mut payload = [0u8; 8];
        payload[0] = PCI_CF | sequence;
        payload[1..=take].copy_from_slice(&data[sent..sent + take]);
        chan.send(cfg.frame(&payload[..=take])).await?;
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

pub(crate) async fn recv(chan: &mut FrameChannel, cfg: &IsoTpConfig) -> io::Result<Vec<u8>> {
    loop {
        let frame = recv_matching(chan, cfg.rx_id).await?;
        match frame.data[0] & PCI_MASK {
            PCI_SF => {
                let len = (frame.data[0] & 0x0F) as usize;
                if len == 0 || len > SF_MAX || len >= frame.len as usize {
                    // len == 0 is the CAN FD escape encoding (unsupported);
                    // oversized lengths are malformed. Drop, like the kernel.
                    continue;
                }
                return Ok(frame.data[1..=len].to_vec());
            }
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
    let total = (((first.data[0] & 0x0F) as usize) << 8) | first.data[1] as usize;
    if total < 8 {
        // total == 0 is the CAN FD escape encoding (unsupported), 1..=7
        // must be sent as single frames.
        return Err(invalid_data("invalid ISO-TP first frame length"));
    }
    if first.len < 8 {
        return Err(invalid_data("short ISO-TP first frame"));
    }

    let mut buf = Vec::with_capacity(total);
    buf.extend_from_slice(&first.data[2..8]);

    let fc = [PCI_FC | FC_CTS, cfg.fc_bs, cfg.fc_stmin];
    chan.send(cfg.frame(&fc)).await?;

    let mut sequence = 1u8;
    let mut in_block = 0u8;
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
        let take = CF_MAX.min(total - buf.len());
        if (frame.len as usize) < 1 + take {
            return Err(invalid_data("short ISO-TP consecutive frame"));
        }
        buf.extend_from_slice(&frame.data[1..=take]);
        sequence = (sequence + 1) & 0x0F;
        in_block += 1;
        if cfg.fc_bs != 0 && in_block == cfg.fc_bs && buf.len() < total {
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
        assert_eq!(frame.data, [0x02, 0x3E, 0x00, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA]);
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
    async fn zero_length_payload_is_sent_as_empty_sf() {
        // A 0-byte SF is degenerate; the kernel rejects writes of len 0.
        // We mirror that by sending nothing and erroring.
        let (mut tx_chan, _peer) = chan_pair();
        let cfg = config(0x7E0, 0x7E8);
        let err = send(&mut tx_chan, &cfg, &[]).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[tokio::test]
    async fn oversized_payload_errors() {
        let (mut tx_chan, _peer) = chan_pair();
        let cfg = config(0x7E0, 0x7E8);
        let err = send(&mut tx_chan, &cfg, &[0u8; 4096]).await.unwrap_err();
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
}
