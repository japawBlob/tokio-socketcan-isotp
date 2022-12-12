
use socketcan_isotp::{self, RECV_BUFFER_SIZE};
pub use socketcan_isotp::{Id, StandardId, Error};
use std::io;
use std::os::raw::c_int;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd}; // maybe use RawFd from unix::prelude
// use std::{future::Future};
use std::pin::Pin;
use std::task::{Context, Poll};
use futures::prelude::*;
use futures::ready;
use mio::{event, unix::SourceFd, Interest, Registry, Token};
use tokio::io::unix::AsyncFd;

pub struct IsoTpWriteFuture {
    socket: IsoTpSocket,
    packet: [u8; RECV_BUFFER_SIZE],
}

impl Future for IsoTpWriteFuture {
    type Output = io::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        eprintln!("polling i guess");
        let _ = ready!(self.socket.0.poll_write_ready(cx))?;
        match self.socket.0.get_ref().0.write(&self.packet) {
            Ok(_) => Poll::Ready(Ok(())),
            Err(err) => Poll::Ready(Err(err)),
        }
    }
}

pub struct EventedIsoTpSocket(socketcan_isotp::IsoTpSocket);

impl EventedIsoTpSocket {
    fn get_ref(&self) -> &socketcan_isotp::IsoTpSocket {
        &self.0
    }
}

impl AsRawFd for EventedIsoTpSocket {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

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
pub struct IsoTpSocket(AsyncFd<EventedIsoTpSocket>);

impl IsoTpSocket {
    /// Open a named CAN device such as "vcan0"
    pub fn open(ifname: &str, src: impl Into<Id>, dst: impl Into<Id>) -> Result<IsoTpSocket, socketcan_isotp::Error> {
        let sock = socketcan_isotp::IsoTpSocket::open(ifname, src, dst)?;
        sock.set_nonblocking(true)?;
        Ok(IsoTpSocket(AsyncFd::new(EventedIsoTpSocket(sock))?))
    }

    /// Open by kernel interface number
    pub fn open_if(if_index: c_int, src: impl Into<Id>, dst: impl Into<Id>) -> Result<IsoTpSocket, socketcan_isotp::Error> {
        let sock = socketcan_isotp::IsoTpSocket::open_if(if_index, src, dst)?;
        sock.set_nonblocking(true)?;
        Ok(IsoTpSocket(AsyncFd::new(EventedIsoTpSocket(sock))?))
    }

    pub fn write_packet(&self, packet : [u8; RECV_BUFFER_SIZE]) -> Result<IsoTpWriteFuture, Error>{
        Ok(IsoTpWriteFuture {
            socket: self.try_clone()?,
            packet,
        })
    }

    fn try_clone(&self) -> Result<Self, io::Error> {
        let fd = self.0.get_ref().0.as_raw_fd();
        eprintln!("whelp cloning");
        unsafe {
            // essentially we're cheating and making it cheaper/easier
            // to manage multiple references to the socket by relying
            // on the posix behaviour of `dup()` which essentially lets
            // the kernel worry about keeping track of references;
            // as long as one of the duplicated file descriptors is open
            // the socket as a whole isn't going to be closed.
            let new_fd = libc::dup(fd);
            let new = socketcan_isotp::IsoTpSocket::from_raw_fd(new_fd);
            Ok(IsoTpSocket(AsyncFd::new(EventedIsoTpSocket(new))?))
        }
    }
}

impl Stream for IsoTpSocket {
    type Item = io::Result<[u8; RECV_BUFFER_SIZE]>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        loop {
            let mut ready_guard = ready!(self.0.poll_read_ready(cx))?;
            match ready_guard.try_io(|inner| inner.get_ref().get_ref().read_to_buffer()) {
                Ok(result) => return Poll::Ready(Some(result)),
                Err(_would_block) => continue,
            }
        }
    }
}


impl Sink<[u8; RECV_BUFFER_SIZE]> for IsoTpSocket {
    type Error = io::Error;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let _ = ready!(self.0.poll_write_ready(cx))?;
        Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, item: [u8; RECV_BUFFER_SIZE]) -> Result<(), Self::Error> {
        self.0.get_ref().0.write(&item)?;
        Ok(())
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let mut ready_guard = ready!(self.0.poll_write_ready(cx))?;
        ready_guard.clear_ready();
        Poll::Ready(Ok(()))
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
