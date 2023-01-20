use socketcan_isotp::{self, RECV_BUFFER_SIZE};
pub use socketcan_isotp::{Id, StandardId, Error};
use std::io;
use std::os::raw::c_int;
use std::os::unix::io::{AsRawFd, RawFd};
use std::pin::Pin;
use std::task::{Context, Poll};
use futures::prelude::*;
use futures::ready;
use mio::{event, unix::SourceFd, Interest, Registry, Token};
use tokio::io::unix::AsyncFd;

/// Future for writing data to IsoTPSocket
pub struct IsoTpWriteFuture<'a> {
    socket: &'a IsoTpSocket,
    packet: Vec<u8>,
}

impl Future for IsoTpWriteFuture<'_> {
    type Output = io::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        loop {
            let _ = ready!(self.socket.0.poll_write_ready(cx))?;
            match self.socket.0.get_ref().write_vec(&self.packet) {
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => continue,
                Ok(_) => return Poll::Ready(Ok(())),
                Err(err) => return Poll::Ready(Err(err))
            }
        }
    }
}

/// Future for readint data from IsoTPSocket
pub struct IsoTpReadFuture<'a> {
    socket: &'a IsoTpSocket,
}

impl Future for IsoTpReadFuture<'_> {
    type Output = io::Result<Vec<u8>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        loop {
            let mut ready_guard = ready!(self.socket.0.poll_read_ready(cx))?;
            match ready_guard.try_io(|inner| inner.get_ref().read_to_vec()) {
                Ok(result) => return Poll::Ready(result),
                Err(_would_block) => continue,
            }
        }
    }

}

pub struct EventedIsoTpSocket(socketcan_isotp::IsoTpSocket);

impl EventedIsoTpSocket {
    fn get_ref(&self) -> &socketcan_isotp::IsoTpSocket {
        &self.0
    }
    fn get_mut(&mut self) -> &mut socketcan_isotp::IsoTpSocket {
        &mut self.0
    }
}

impl AsRawFd for EventedIsoTpSocket {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

/// trait used for regstering the EventedIsoTpSocket to mio events
impl event::Source for EventedIsoTpSocket {
    fn register(
        &mut self,
        registry: &Registry,
        token: Token,
        interests: Interest,
    ) -> io::Result<()> {
        SourceFd(&self.0.as_raw_fd()).register(registry, token, interests)
    }

    fn reregister(
        &mut self,
        registry: &Registry,
        token: Token,
        interests: Interest,
    ) -> io::Result<()> {
        SourceFd(&self.0.as_raw_fd()).reregister(registry, token, interests)
    }

    fn deregister(&mut self, registry: &Registry) -> io::Result<()> {
        SourceFd(&self.0.as_raw_fd()).deregister(registry)
    }
}

/// An asynchronous I/O wrapped socketcan_isotp::IsoTpSocket
pub struct IsoTpSocket(
    AsyncFd<socketcan_isotp::IsoTpSocket>
);

impl IsoTpSocket {
    /// Open a named CAN device such as "vcan0"
    pub fn open(ifname: &str, src: impl Into<Id>, dst: impl Into<Id>) -> Result<IsoTpSocket, socketcan_isotp::Error> {
        let sock = socketcan_isotp::IsoTpSocket::open(ifname, src, dst)?;
        sock.set_nonblocking(true)?;
        Ok(IsoTpSocket(AsyncFd::new(sock)?))
    }

    /// Open by kernel interface number
    pub fn open_if(if_index: c_int, src: impl Into<Id>, dst: impl Into<Id>) -> Result<IsoTpSocket, socketcan_isotp::Error> {
        let sock = socketcan_isotp::IsoTpSocket::open_if(if_index, src, dst)?;
        sock.set_nonblocking(true)?;
        Ok(IsoTpSocket(AsyncFd::new(sock)?))
    }

    pub fn write_packet(&self, packet : Vec<u8>) -> Result<IsoTpWriteFuture, Error>{
        Ok(IsoTpWriteFuture {
            socket: &self,
            packet,
        })
    }

    pub fn read_packet(&self) -> Result<IsoTpReadFuture, Error>{
        Ok(IsoTpReadFuture {
            socket: &self,
        })
    }
}

// pub fn add(left: usize, right: usize) -> usize {
//     left + right
// }

#[cfg(test)]
mod tests {
    use super::*;

    // #[test]
    // fn it_works() {
    //     let result = add(2, 2);
    //     assert_eq!(result, 4);
    // }
}
