//! Async wrapper around the kernel ISO-TP socket (`can_isotp` module).
//!
//! This is the original transport of this crate: the kernel implements the
//! complete ISO 15765-2 protocol, we only register the non-blocking fd with
//! tokio and surface complete ISO-TP payloads.

use crate::socketcan_isotp;
use std::future::poll_fn;
use std::io;
use std::task::{ready, Poll};
use tokio::io::unix::AsyncFd;

pub(crate) struct KernelIsoTp(AsyncFd<socketcan_isotp::IsoTpSocket>);

impl KernelIsoTp {
    pub(crate) fn new(sock: socketcan_isotp::IsoTpSocket) -> Result<Self, crate::Error> {
        sock.set_nonblocking(true)?;
        Ok(KernelIsoTp(AsyncFd::new(sock)?))
    }

    pub(crate) async fn write_packet(&self, packet: &[u8]) -> io::Result<()> {
        poll_fn(|cx| loop {
            let mut guard = ready!(self.0.poll_write_ready(cx))?;
            match self.0.get_ref().write(packet) {
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    guard.clear_ready(); // Comment this line if you are on older Kernel and the communication soft-locks
                    continue;
                }
                Ok(()) => return Poll::Ready(Ok(())),
                Err(err) => return Poll::Ready(Err(err)),
            }
        })
        .await
    }

    pub(crate) async fn read_packet(&self) -> io::Result<Vec<u8>> {
        poll_fn(|cx| loop {
            let mut ready_guard = ready!(self.0.poll_read_ready(cx))?;
            match ready_guard.try_io(|inner| inner.get_ref().read_to_vec()) {
                Ok(result) => return Poll::Ready(result),
                Err(_would_block) => continue,
            }
        })
        .await
    }
}
