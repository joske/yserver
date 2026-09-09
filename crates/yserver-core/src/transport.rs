//! Socket transport abstractions for X11 clients.
//!
//! Unix-domain clients are local and can pass descriptors. Opt-in TCP clients
//! use the same byte-stream setup/read/write path with remote-client policy.

use std::{
    io::{self, Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    os::{
        fd::{AsRawFd, RawFd},
        unix::net::{UnixListener, UnixStream},
    },
    time::Duration,
};

#[derive(Debug)]
pub enum Transport {
    Unix(UnixStream),
    Tcp(TcpStream),
}

impl Transport {
    pub fn pair() -> io::Result<(Self, UnixStream)> {
        UnixStream::pair().map(|(stream, peer)| (Self::Unix(stream), peer))
    }

    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        match self {
            Self::Unix(stream) => stream.set_nonblocking(nonblocking),
            Self::Tcp(stream) => stream.set_nonblocking(nonblocking),
        }
    }

    pub fn try_clone(&self) -> io::Result<Self> {
        match self {
            Self::Unix(stream) => stream.try_clone().map(Self::Unix),
            Self::Tcp(stream) => stream.try_clone().map(Self::Tcp),
        }
    }

    pub fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        match self {
            Self::Unix(stream) => stream.shutdown(how),
            Self::Tcp(stream) => stream.shutdown(how),
        }
    }

    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        match self {
            Self::Unix(stream) => stream.set_read_timeout(timeout),
            Self::Tcp(stream) => stream.set_read_timeout(timeout),
        }
    }

    pub fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        match self {
            Self::Unix(stream) => stream.set_write_timeout(timeout),
            Self::Tcp(stream) => stream.set_write_timeout(timeout),
        }
    }
}

impl From<UnixStream> for Transport {
    fn from(stream: UnixStream) -> Self {
        Self::Unix(stream)
    }
}

impl Read for Transport {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Unix(stream) => stream.read(buf),
            Self::Tcp(stream) => stream.read(buf),
        }
    }
}

impl Write for Transport {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Unix(stream) => stream.write(buf),
            Self::Tcp(stream) => stream.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Unix(stream) => stream.flush(),
            Self::Tcp(stream) => stream.flush(),
        }
    }
}

impl AsRawFd for Transport {
    fn as_raw_fd(&self) -> RawFd {
        match self {
            Self::Unix(stream) => stream.as_raw_fd(),
            Self::Tcp(stream) => stream.as_raw_fd(),
        }
    }
}

pub enum Listener {
    Unix(UnixListener),
    Tcp(TcpListener),
}

impl From<UnixListener> for Listener {
    fn from(listener: UnixListener) -> Self {
        Self::Unix(listener)
    }
}

impl Listener {
    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        match self {
            Self::Unix(listener) => listener.set_nonblocking(nonblocking),
            Self::Tcp(listener) => listener.set_nonblocking(nonblocking),
        }
    }
}

impl AsRawFd for Listener {
    fn as_raw_fd(&self) -> RawFd {
        match self {
            Self::Unix(listener) => listener.as_raw_fd(),
            Self::Tcp(listener) => listener.as_raw_fd(),
        }
    }
}
