//! CAN frames over TCP: a virtual bus for tests and platforms without
//! SocketCAN.
//!
//! # Wire format
//!
//! Every message on a TCP connection is exactly 16 bytes, mirroring the
//! Linux `struct can_frame` with fixed little-endian encoding:
//!
//! ```text
//! bytes 0..4   u32 LE can_id; bit 31 = extended (29-bit) id flag
//! byte  4      u8 number of valid data bytes (0..=8)
//! bytes 5..8   reserved, zero
//! bytes 8..16  frame data (padded with zeros unless the ISO-TP layer
//!              applied its own padding)
//! ```
//!
//! # Topology
//!
//! One process binds a hub (`tcplisten:<addr>:<port>`); any number of
//! local sockets and remote TCP clients (`tcp:<host>:<port>`) attach to
//! it. The hub broadcasts every frame to all attached ports except the
//! one it came from, like a CAN bus that does not echo to the sender.
//! Hubs live for the rest of the process so that short-lived sockets
//! (one per transaction) do not tear down the listener under connected
//! clients' feet.

use super::CanFrame;
use crate::error::Error;
use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::mpsc;

const FRAME_LEN: usize = 16;

fn encode_frame(frame: CanFrame) -> [u8; FRAME_LEN] {
    let mut buf = [0u8; FRAME_LEN];
    buf[0..4].copy_from_slice(&frame.can_id.to_le_bytes());
    buf[4] = frame.len;
    buf[8..16].copy_from_slice(&frame.data);
    buf
}

fn decode_frame(buf: &[u8; FRAME_LEN]) -> CanFrame {
    let mut data = [0u8; 8];
    data.copy_from_slice(&buf[8..16]);
    CanFrame {
        can_id: u32::from_le_bytes(buf[0..4].try_into().unwrap()),
        len: buf[4].min(8),
        data,
    }
}

/// A client connection to a remote frame hub (`tcp:<host>:<port>`).
pub(crate) struct TcpClientChannel {
    rd: OwnedReadHalf,
    wr: OwnedWriteHalf,
}

impl TcpClientChannel {
    /// Connect to `endpoint` (`host:port`). The connect itself is blocking;
    /// this is accepted for a localhost test transport.
    pub(crate) fn connect(endpoint: &str) -> Result<Self, Error> {
        let stream = std::net::TcpStream::connect(endpoint)?;
        stream.set_nodelay(true)?;
        stream.set_nonblocking(true)?;
        let stream = tokio::net::TcpStream::from_std(stream)?;
        let (rd, wr) = stream.into_split();
        Ok(TcpClientChannel { rd, wr })
    }

    pub(crate) async fn send(&mut self, frame: CanFrame) -> io::Result<()> {
        self.wr.write_all(&encode_frame(frame)).await
    }

    pub(crate) async fn recv(&mut self) -> io::Result<CanFrame> {
        let mut buf = [0u8; FRAME_LEN];
        self.rd.read_exact(&mut buf).await?;
        Ok(decode_frame(&buf))
    }
}

/// Frame router shared by all ports of one listen address.
struct Hub {
    ports: Mutex<HashMap<u64, mpsc::UnboundedSender<CanFrame>>>,
    next_id: AtomicU64,
}

impl Hub {
    fn new() -> Self {
        Hub {
            ports: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(0),
        }
    }

    fn register(&self) -> (u64, mpsc::UnboundedReceiver<CanFrame>) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::unbounded_channel();
        self.ports.lock().unwrap().insert(id, tx);
        (id, rx)
    }

    fn deregister(&self, id: u64) {
        self.ports.lock().unwrap().remove(&id);
    }

    fn broadcast(&self, frame: CanFrame, from: u64) {
        let ports = self.ports.lock().unwrap();
        for (id, tx) in ports.iter() {
            if *id != from {
                // A closed receiver is cleaned up by its own task.
                let _ = tx.send(frame);
            }
        }
    }
}

/// Hubs are keyed by the literal listen address string and live for the
/// rest of the process.
static HUBS: OnceLock<Mutex<HashMap<String, Arc<Hub>>>> = OnceLock::new();

/// A local port on an in-process frame hub (`tcplisten:<addr>:<port>`).
pub(crate) struct HubPort {
    hub: Arc<Hub>,
    id: u64,
    rx: mpsc::UnboundedReceiver<CanFrame>,
}

impl HubPort {
    pub(crate) async fn send(&mut self, frame: CanFrame) -> io::Result<()> {
        self.hub.broadcast(frame, self.id);
        Ok(())
    }

    pub(crate) async fn recv(&mut self) -> io::Result<CanFrame> {
        self.rx
            .recv()
            .await
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "frame hub shut down"))
    }
}

impl Drop for HubPort {
    fn drop(&mut self) {
        self.hub.deregister(self.id);
    }
}

/// Attach a local port to the hub at `listen_addr`, binding the TCP
/// listener and spawning its accept loop on first use. Must be called
/// from within a tokio runtime.
pub(crate) fn hub_port(listen_addr: &str) -> Result<HubPort, Error> {
    let hubs = HUBS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = hubs.lock().unwrap();
    let hub = match map.get(listen_addr) {
        Some(hub) => Arc::clone(hub),
        None => {
            let listener = std::net::TcpListener::bind(listen_addr)?;
            listener.set_nonblocking(true)?;
            let listener = tokio::net::TcpListener::from_std(listener)?;
            let hub = Arc::new(Hub::new());
            tokio::spawn(accept_loop(listener, Arc::clone(&hub)));
            map.insert(listen_addr.to_owned(), Arc::clone(&hub));
            hub
        }
    };
    let (id, rx) = hub.register();
    Ok(HubPort { hub, id, rx })
}

async fn accept_loop(listener: tokio::net::TcpListener, hub: Arc<Hub>) {
    loop {
        match listener.accept().await {
            Ok((stream, _peer)) => {
                let _ = stream.set_nodelay(true);
                let (rd, wr) = stream.into_split();
                let (port_id, rx) = hub.register();
                tokio::spawn(peer_writer(wr, rx));
                tokio::spawn(peer_reader(rd, Arc::clone(&hub), port_id));
            }
            Err(_) => {
                // Transient accept failure (e.g. EMFILE); back off briefly.
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
}

async fn peer_reader(mut rd: OwnedReadHalf, hub: Arc<Hub>, port_id: u64) {
    let mut buf = [0u8; FRAME_LEN];
    while rd.read_exact(&mut buf).await.is_ok() {
        hub.broadcast(decode_frame(&buf), port_id);
    }
    hub.deregister(port_id);
}

async fn peer_writer(mut wr: OwnedWriteHalf, mut rx: mpsc::UnboundedReceiver<CanFrame>) {
    // Ends when the port is deregistered (sender dropped) or the peer
    // connection breaks.
    while let Some(frame) = rx.recv().await {
        if wr.write_all(&encode_frame(frame)).await.is_err() {
            break;
        }
    }
}
