//! CAN frames over a [socketcand](https://github.com/linux-can/socketcand)
//! daemon in *rawmode*, an alternative to opening a SocketCAN device
//! directly. Enables the `socketcand:<host>:<port>:<bus>` address scheme on
//! all platforms; the userspace ISO-TP implementation always runs on top.
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

use super::CanFrame;
use crate::error::Error;
use std::io;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

/// `CAN_EFF_FLAG`: bit 31 marks a 29-bit extended id in [CanFrame::can_id].
const EFF_FLAG: u32 = 0x8000_0000;
/// Valid bits in an extended (29-bit) id.
const EFF_MASK: u32 = 0x1fff_ffff;
/// Largest 11-bit standard id.
const SFF_MAX: u32 = 0x7ff;

/// A client connection to a socketcand daemon
/// (`socketcand:<host>:<port>:<bus>`).
pub(crate) struct SocketcandChannel {
    rd: OwnedReadHalf,
    wr: OwnedWriteHalf,
    /// Bytes received but not yet parsed into a complete `< ... >` message.
    rx_buf: Vec<u8>,
}

impl SocketcandChannel {
    /// Connect to the daemon at `endpoint` (`host:port`) and open `bus` in
    /// rawmode. The connect and handshake use blocking I/O (accepted, like the
    /// `tcp:` backend, for a short-lived control path) before the socket is
    /// handed to tokio. Must be called from within a tokio runtime.
    pub(crate) fn connect(endpoint: &str, bus: &str) -> Result<Self, Error> {
        use std::io::Write;

        let mut stream = std::net::TcpStream::connect(endpoint)?;
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;

        // Greeting.
        let greeting = read_message_blocking(&mut stream)?;
        if greeting != "hi" {
            return Err(Error::Address(format!(
                "unexpected socketcand greeting from {endpoint}: '{greeting}'"
            )));
        }

        // Open the bus and confirm.
        stream.write_all(format!("< open {bus} >").as_bytes())?;
        let resp = read_message_blocking(&mut stream)?;
        if resp != "ok" {
            return Err(Error::Address(format!(
                "socketcand at {endpoint} did not accept bus '{bus}': '{resp}'"
            )));
        }

        // Switch to rawmode. Some daemon versions reply `< ok >` here and some
        // do not; rather than block on a reply that may never come, we let the
        // async receive path skip any stray non-frame message.
        stream.write_all(b"< rawmode >")?;

        stream.set_read_timeout(None)?;
        stream.set_nonblocking(true)?;
        let stream = tokio::net::TcpStream::from_std(stream)?;
        let (rd, wr) = stream.into_split();
        Ok(SocketcandChannel {
            rd,
            wr,
            rx_buf: Vec::new(),
        })
    }

    pub(crate) async fn send(&mut self, frame: CanFrame) -> io::Result<()> {
        self.wr.write_all(encode_send(frame).as_bytes()).await
    }

    pub(crate) async fn recv(&mut self) -> io::Result<CanFrame> {
        loop {
            // Drain any already-buffered messages first.
            while let Some(msg) = self.take_message() {
                if let Some(frame) = parse_frame(&msg) {
                    return Ok(frame);
                }
                // Non-frame message (`ok`, `hi`, `error ...`): skip it.
            }
            let mut tmp = [0u8; 1024];
            let n = self.rd.read(&mut tmp).await?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "socketcand connection closed",
                ));
            }
            self.rx_buf.extend_from_slice(&tmp[..n]);
        }
    }

    /// Pop the next complete `< ... >` message from [Self::rx_buf], returning
    /// its trimmed inner content, or `None` if no complete message is buffered.
    fn take_message(&mut self) -> Option<String> {
        let start = self.rx_buf.iter().position(|&b| b == b'<')?;
        let end = self.rx_buf[start..].iter().position(|&b| b == b'>')? + start;
        let inner = String::from_utf8_lossy(&self.rx_buf[start + 1..end])
            .trim()
            .to_owned();
        self.rx_buf.drain(..=end);
        Some(inner)
    }
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
    for byte in &frame.data[..frame.len as usize] {
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
    let mut data = Vec::with_capacity(8);
    let bytes = data_hex.as_bytes();
    let mut i = 0;
    while i + 2 <= bytes.len() {
        let pair = std::str::from_utf8(&bytes[i..i + 2]).ok()?;
        data.push(u8::from_str_radix(pair, 16).ok()?);
        i += 2;
    }
    if data.len() > 8 {
        data.truncate(8);
    }
    Some(CanFrame::new(can_id, &data))
}

/// Read exactly one `< ... >` message from a blocking stream, returning the
/// trimmed inner content. Reads a byte at a time so it never consumes past the
/// closing `>` (which would strand frame bytes the async path needs).
fn read_message_blocking(stream: &mut std::net::TcpStream) -> Result<String, Error> {
    use std::io::Read;

    let mut inner = Vec::new();
    let mut started = false;
    let mut byte = [0u8; 1];
    loop {
        if stream.read(&mut byte)? == 0 {
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
    fn non_frame_messages_are_skipped() {
        assert!(parse_frame("ok").is_none());
        assert!(parse_frame("hi").is_none());
        assert!(parse_frame("error unknown command").is_none());
    }

    /// Drive [SocketcandChannel] against a minimal mock daemon: greeting, bus
    /// open, rawmode, then a pushed frame and a sent frame.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn handshake_and_roundtrip() {
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

            // Push a frame to the client, then read the client's send.
            sock.write_all(b"< frame 708 1.0 62F18601 >").await.unwrap();
            let n = sock.read(&mut buf).await.unwrap();
            assert_eq!(
                String::from_utf8_lossy(&buf[..n]).trim(),
                "< send 700 3 02 10 03 >"
            );
        });

        let endpoint = format!("{}:{}", addr.ip(), addr.port());
        let mut chan =
            tokio::task::spawn_blocking(move || SocketcandChannel::connect(&endpoint, "vcan0"))
                .await
                .unwrap()
                .unwrap();

        let frame = chan.recv().await.unwrap();
        assert_eq!(frame.can_id, 0x708);
        assert_eq!(&frame.data[..frame.len as usize], &[0x62, 0xF1, 0x86, 0x01]);

        chan.send(CanFrame::new(0x700, &[0x02, 0x10, 0x03]))
            .await
            .unwrap();

        server.await.unwrap();
    }
}
