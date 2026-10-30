//! CAN frames over a [socketcand](https://github.com/linux-can/socketcand)
//! daemon in *rawmode*, an alternative to opening a SocketCAN device
//! directly. Enables the `socketcand:<host>:<port>:<bus>` address scheme on
//! all platforms; the userspace ISO-TP implementation always runs on top.
//!
//! # Topology
//!
//! A userspace ISO-TP socket is opened per transaction, but socketcand is a
//! TCP connection with a handshake - reconnecting per transaction (and per
//! functional keep-alive) churns short-lived connections, which under load
//! ties up the runtime and trips intermittent DNS failures. Instead this
//! backend keeps **one persistent, shared connection per
//! `host:port:bus` endpoint** (process-global, like the `tcp:` frame hub) and
//! fans every received frame out to all attached ports; each port's userspace
//! ISO-TP machine filters by its own rx id. Opening/closing an ISO-TP socket
//! only registers/deregisters a port - no I/O, no handshake.
//!
//! # Protocol
//!
//! socketcand speaks a line-less ASCII protocol where every message is framed
//! by `< ` and ` >`. The rawmode handshake after the TCP connect is:
//!
//! ```text
//! daemon -> < hi >
//! client -> < open <bus> >
//! daemon -> < ok >
//! client -> < rawmode >
//! ```
//!
//! Afterwards received frames arrive as
//! `< frame <can_id> <sec.usec> <datahex> >` (contiguous hex, no spaces) and
//! frames are transmitted as `< send <can_id> <dlc> <b0> <b1> ... >`
//! (space-separated lowercase hex bytes). 11-bit ids are written as up to
//! three hex digits, 29-bit ids zero-padded to eight - matching how socketcand
//! distinguishes standard from extended frames.
//!
//! # CAN FD
//!
//! Upstream socketcand is classic CAN only (at most 8 data bytes per frame).
//! FD-aware servers such as
//! [cda-can-bridge](https://github.com/floroks/cda-can-bridge) run the bus in
//! CAN FD mode and use the same messages with up to 64 data bytes: received
//! frames with more than 8 bytes are treated as CAN FD frames, and frames
//! sent with more than 8 bytes are put on the bus as CAN FD frames (the
//! server decides FD/BRS for shorter frames from its bus configuration).
//! Frames longer than 8 bytes are therefore only sent when the ISO-TP socket
//! is configured for CAN FD via [crate::LinkLayerOptions].

use super::{CAN_MAX_DLEN, CANFD_MAX_DLEN, CanFrame};
use crate::error::Error;
use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

/// `CAN_EFF_FLAG`: bit 31 marks a 29-bit extended id in [CanFrame::can_id].
const EFF_FLAG: u32 = 0x8000_0000;
/// Valid bits in an extended (29-bit) id.
const EFF_MASK: u32 = 0x1fff_ffff;
/// Largest 11-bit standard id.
const SFF_MAX: u32 = 0x7ff;
/// Longest backoff between reconnect attempts.
const RECONNECT_MAX: Duration = Duration::from_secs(5);

/// Shared, persistent connection to one socketcand endpoint. Received frames
/// are broadcast to every attached port; outbound frames from all ports are
/// multiplexed onto the single connection.
struct SocketcandConn {
    ports: Mutex<HashMap<u64, mpsc::UnboundedSender<CanFrame>>>,
    next_id: AtomicU64,
    /// Outbound frames from all ports, drained by the connection task.
    out_tx: mpsc::UnboundedSender<CanFrame>,
}

impl SocketcandConn {
    fn register(&self) -> (u64, mpsc::UnboundedReceiver<CanFrame>) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::unbounded_channel();
        self.ports.lock().unwrap().insert(id, tx);
        (id, rx)
    }

    fn deregister(&self, id: u64) {
        self.ports.lock().unwrap().remove(&id);
    }

    fn broadcast(&self, frame: CanFrame) {
        let ports = self.ports.lock().unwrap();
        for tx in ports.values() {
            // A closed receiver is cleaned up when its port is dropped.
            let _ = tx.send(frame);
        }
    }
}

/// Connections are keyed by `endpoint|bus` and live for the rest of the
/// process, so short-lived per-transaction sockets don't reconnect.
static CONNS: OnceLock<Mutex<HashMap<String, Arc<SocketcandConn>>>> = OnceLock::new();

/// A port on the shared socketcand connection (`socketcand:<host>:<port>:<bus>`).
pub(crate) struct SocketcandChannel {
    conn: Arc<SocketcandConn>,
    id: u64,
    rx: mpsc::UnboundedReceiver<CanFrame>,
}

impl SocketcandChannel {
    pub(crate) async fn send(&mut self, frame: CanFrame) -> io::Result<()> {
        self.conn
            .out_tx
            .send(frame)
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "socketcand connection closed"))
    }

    pub(crate) async fn recv(&mut self) -> io::Result<CanFrame> {
        self.rx.recv().await.ok_or_else(|| {
            io::Error::new(io::ErrorKind::BrokenPipe, "socketcand connection closed")
        })
    }
}

impl Drop for SocketcandChannel {
    fn drop(&mut self) {
        self.conn.deregister(self.id);
    }
}

/// Attach a port to the shared connection for `endpoint` (`host:port`) / `bus`,
/// spawning the connection task on first use. Must be called from within a
/// tokio runtime.
pub(crate) fn socketcand_port(endpoint: &str, bus: &str) -> Result<SocketcandChannel, Error> {
    let key = format!("{endpoint}|{bus}");
    let conns = CONNS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = conns.lock().unwrap();
    let conn = match map.get(&key) {
        Some(conn) => Arc::clone(conn),
        None => {
            let (out_tx, out_rx) = mpsc::unbounded_channel();
            let conn = Arc::new(SocketcandConn {
                ports: Mutex::new(HashMap::new()),
                next_id: AtomicU64::new(0),
                out_tx,
            });
            tokio::spawn(run_connection(
                endpoint.to_owned(),
                bus.to_owned(),
                out_rx,
                Arc::clone(&conn),
            ));
            map.insert(key, Arc::clone(&conn));
            conn
        }
    };
    let (id, rx) = conn.register();
    Ok(SocketcandChannel { conn, id, rx })
}

/// Owns the socketcand TCP connection: (re)connects with backoff, broadcasts
/// received frames to the ports, and writes outbound frames. Runs for the life
/// of the process.
async fn run_connection(
    endpoint: String,
    bus: String,
    mut out_rx: mpsc::UnboundedReceiver<CanFrame>,
    conn: Arc<SocketcandConn>,
) {
    let mut attempt: u32 = 0;
    loop {
        let stream = match connect(&endpoint, &bus).await {
            Ok(stream) => {
                attempt = 0;
                stream
            }
            Err(e) => {
                attempt = attempt.saturating_add(1);
                let delay = RECONNECT_MAX.min(Duration::from_millis(500) * (1 << attempt.min(4)));
                tokio::time::sleep(delay).await;
                let _ = &e;
                continue;
            }
        };

        let (mut rd, mut wr) = stream.into_split();
        let mut rx_buf: Vec<u8> = Vec::new();
        let mut read_buf = [0u8; 1024];
        loop {
            tokio::select! {
                read = rd.read(&mut read_buf) => match read {
                    Ok(0) | Err(_) => break, // connection closed; reconnect
                    Ok(n) => {
                        rx_buf.extend_from_slice(&read_buf[..n]);
                        while let Some(msg) = take_message(&mut rx_buf) {
                            if let Some(frame) = parse_frame(&msg) {
                                conn.broadcast(frame);
                            }
                        }
                    }
                },
                frame = out_rx.recv() => match frame {
                    None => return, // all ports and the registry are gone
                    Some(frame) => {
                        if wr.write_all(encode_send(frame).as_bytes()).await.is_err() {
                            break; // write failed; reconnect
                        }
                    }
                },
            }
        }
    }
}

/// Connect to `endpoint` and complete the rawmode handshake for `bus`. Uses
/// async I/O throughout, so a slow or failing DNS lookup never blocks the
/// runtime.
async fn connect(endpoint: &str, bus: &str) -> Result<TcpStream, Error> {
    let mut stream = TcpStream::connect(endpoint).await?;
    stream.set_nodelay(true)?;

    let greeting = read_message(&mut stream).await?;
    if greeting != "hi" {
        return Err(Error::Address(format!(
            "unexpected socketcand greeting from {endpoint}: '{greeting}'"
        )));
    }
    stream
        .write_all(format!("< open {bus} >").as_bytes())
        .await?;
    let resp = read_message(&mut stream).await?;
    if resp != "ok" {
        return Err(Error::Address(format!(
            "socketcand at {endpoint} did not accept bus '{bus}': '{resp}'"
        )));
    }
    // Switch to rawmode. Some daemons reply `< ok >`, some do not; the frame
    // reader skips any stray non-frame message, so we don't wait for it.
    stream.write_all(b"< rawmode >").await?;
    Ok(stream)
}

/// Read one complete `< ... >` message, returning its trimmed inner content.
/// Reads a byte at a time so it never consumes past the closing `>`.
async fn read_message(stream: &mut TcpStream) -> Result<String, Error> {
    let mut inner = Vec::new();
    let mut started = false;
    let mut byte = [0u8; 1];
    loop {
        if stream.read(&mut byte).await? == 0 {
            return Err(Error::Io {
                source: io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "socketcand closed during handshake",
                ),
            });
        }
        match byte[0] {
            b'<' => {
                started = true;
                inner.clear();
            }
            b'>' if started => break,
            b if started => inner.push(b),
            _ => {}
        }
    }
    Ok(String::from_utf8_lossy(&inner).trim().to_owned())
}

/// Pop the next complete `< ... >` message from `buf`, returning its trimmed
/// inner content, or `None` if no complete message is buffered.
fn take_message(buf: &mut Vec<u8>) -> Option<String> {
    let start = buf.iter().position(|&b| b == b'<')?;
    let end = buf[start..].iter().position(|&b| b == b'>')? + start;
    let inner = String::from_utf8_lossy(&buf[start + 1..end])
        .trim()
        .to_owned();
    buf.drain(..=end);
    Some(inner)
}

/// Render a `< send ... >` message for `frame`.
fn encode_send(frame: CanFrame) -> String {
    let extended = frame.can_id & EFF_FLAG != 0;
    let arb = frame.can_id & EFF_MASK;
    let id = if extended {
        format!("{arb:08X}")
    } else {
        format!("{arb:X}")
    };
    let mut msg = format!("< send {id} {}", frame.len);
    for byte in frame.payload() {
        msg.push_str(&format!(" {byte:02x}"));
    }
    msg.push_str(" >");
    msg
}

/// Parse the inner content of a message into a [CanFrame], or `None` if it is
/// not a `frame` message we can decode.
fn parse_frame(msg: &str) -> Option<CanFrame> {
    let mut tokens = msg.split_whitespace();
    if tokens.next()? != "frame" {
        return None;
    }
    let id_tok = tokens.next()?;
    let _timestamp = tokens.next()?; // <sec.usec>, ignored
    let id_val = u32::from_str_radix(id_tok, 16).ok()?;
    // socketcand prints extended ids as eight hex digits; treat anything
    // longer than three digits or larger than the standard range as extended.
    let extended = id_tok.len() > 3 || id_val > SFF_MAX;
    let can_id = if extended {
        (id_val & EFF_MASK) | EFF_FLAG
    } else {
        id_val
    };

    // Remaining tokens hold the data as contiguous hex (one block) or, for
    // tolerance, space-separated bytes; concatenate then split into byte pairs.
    let data_hex: String = tokens.collect();
    let mut data = Vec::with_capacity(CANFD_MAX_DLEN);
    let bytes = data_hex.as_bytes();
    let mut i = 0;
    while i + 2 <= bytes.len() {
        let pair = std::str::from_utf8(&bytes[i..i + 2]).ok()?;
        data.push(u8::from_str_radix(pair, 16).ok()?);
        i += 2;
    }
    // More than 8 bytes can only be a CAN FD frame (relayed by an FD-aware
    // server such as cda-can-bridge); anything beyond 64 is malformed.
    if data.len() > CANFD_MAX_DLEN {
        return None;
    }
    let fd = data.len() > CAN_MAX_DLEN;
    Some(CanFrame::with_flags(can_id, &data, fd, false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_standard_frame() {
        let frame = CanFrame::new(0x700, &[0x02, 0x10, 0x03]);
        assert_eq!(encode_send(frame), "< send 700 3 02 10 03 >");
    }

    #[test]
    fn encode_extended_frame() {
        let frame = CanFrame::new(0x18DA10F1 | EFF_FLAG, &[0x01]);
        assert_eq!(encode_send(frame), "< send 18DA10F1 1 01 >");
    }

    #[test]
    fn parse_standard_frame() {
        let frame = parse_frame("frame 708 23.424242 62F18601").unwrap();
        assert_eq!(frame.can_id, 0x708);
        assert_eq!(frame.len, 4);
        assert_eq!(&frame.data[..4], &[0x62, 0xF1, 0x86, 0x01]);
    }

    #[test]
    fn parse_extended_frame() {
        let frame = parse_frame("frame 18DAF110 1.2 50").unwrap();
        assert_eq!(frame.can_id, 0x18DAF110 | EFF_FLAG);
        assert_eq!(frame.len, 1);
        assert_eq!(frame.data[0], 0x50);
    }

    #[test]
    fn parse_zero_length_frame() {
        let frame = parse_frame("frame 708 1.0").unwrap();
        assert_eq!(frame.can_id, 0x708);
        assert_eq!(frame.len, 0);
    }

    #[test]
    fn parse_fd_frame() {
        let frame =
            parse_frame("frame 18E02C80 1.0 001462f1903f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f0000")
                .unwrap();
        assert_eq!(frame.can_id, 0x18E02C80 | EFF_FLAG);
        assert_eq!(frame.len, 24);
        assert!(frame.fd);
        assert_eq!(&frame.payload()[..5], &[0x00, 0x14, 0x62, 0xF1, 0x90]);
    }

    #[test]
    fn parse_oversized_frame_is_rejected() {
        let msg = format!("frame 708 1.0 {}", "00".repeat(65));
        assert!(parse_frame(&msg).is_none());
    }

    #[test]
    fn encode_fd_frame() {
        let frame = CanFrame::with_flags(0x7E0, &[0xAB; 12], true, true);
        let msg = encode_send(frame);
        assert!(msg.starts_with("< send 7E0 12 ab ab"));
        assert_eq!(msg.matches(" ab").count(), 12);
    }

    #[test]
    fn non_frame_messages_are_skipped() {
        assert!(parse_frame("ok").is_none());
        assert!(parse_frame("hi").is_none());
        assert!(parse_frame("error unknown command").is_none());
    }

    /// Drive [socketcand_port] against a minimal mock daemon: greeting, bus
    /// open, rawmode, then a pushed frame delivered to the port and a frame
    /// sent from the port reaching the daemon. Two ports share one connection.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shared_connection_roundtrip() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 64];

            sock.write_all(b"< hi >").await.unwrap();
            let n = sock.read(&mut buf).await.unwrap();
            assert_eq!(String::from_utf8_lossy(&buf[..n]).trim(), "< open vcan0 >");
            sock.write_all(b"< ok >").await.unwrap();
            let n = sock.read(&mut buf).await.unwrap();
            assert_eq!(String::from_utf8_lossy(&buf[..n]).trim(), "< rawmode >");

            // Push a frame (broadcast to all ports), then read a port's send.
            sock.write_all(b"< frame 708 1.0 62F18601 >").await.unwrap();
            let n = sock.read(&mut buf).await.unwrap();
            assert_eq!(
                String::from_utf8_lossy(&buf[..n]).trim(),
                "< send 700 3 02 10 03 >"
            );
        });

        let endpoint = format!("{}:{}", addr.ip(), addr.port());
        let mut port_a = socketcand_port(&endpoint, "vcan0").unwrap();
        let mut port_b = socketcand_port(&endpoint, "vcan0").unwrap();

        // Both ports receive the broadcast frame.
        let a = port_a.recv().await.unwrap();
        let b = port_b.recv().await.unwrap();
        assert_eq!(a.can_id, 0x708);
        assert_eq!(b.can_id, 0x708);
        assert_eq!(&a.data[..a.len as usize], &[0x62, 0xF1, 0x86, 0x01]);

        // A send from one port reaches the daemon over the shared connection.
        port_a
            .send(CanFrame::new(0x700, &[0x02, 0x10, 0x03]))
            .await
            .unwrap();

        server.await.unwrap();
    }
}
