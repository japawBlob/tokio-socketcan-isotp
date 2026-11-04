//! In-memory frame channel used by the unit tests of the userspace ISO-TP
//! implementation.

use super::CanFrame;
use std::io;
use tokio::sync::mpsc;

pub(crate) struct MemChannel {
    tx: mpsc::UnboundedSender<CanFrame>,
    rx: mpsc::UnboundedReceiver<CanFrame>,
}

/// Create a crossed pair of channels: frames sent on one end are received
/// on the other, like two nodes on a private bus.
pub(crate) fn pair() -> (MemChannel, MemChannel) {
    let (a_tx, a_rx) = mpsc::unbounded_channel();
    let (b_tx, b_rx) = mpsc::unbounded_channel();
    (
        MemChannel { tx: a_tx, rx: b_rx },
        MemChannel { tx: b_tx, rx: a_rx },
    )
}

impl MemChannel {
    pub(crate) fn send(&self, frame: CanFrame) -> io::Result<()> {
        self.tx
            .send(frame)
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "peer channel closed"))
    }

    pub(crate) async fn recv(&mut self) -> io::Result<CanFrame> {
        self.rx
            .recv()
            .await
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "peer channel closed"))
    }
}
