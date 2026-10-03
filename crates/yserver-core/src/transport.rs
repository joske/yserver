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

#[cfg(test)]
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

#[derive(Debug)]
pub enum Transport {
    Unix(UnixStream),
    Tcp(TcpStream),
    #[cfg(test)]
    Capture(Arc<Mutex<VecDeque<u8>>>),
}

#[cfg(test)]
pub(crate) struct CapturedPeer {
    bytes: Arc<Mutex<VecDeque<u8>>>,
    nonblocking: bool,
}

#[cfg(test)]
impl CapturedPeer {
    pub(crate) fn set_nonblocking(&mut self, nonblocking: bool) -> io::Result<()> {
        self.nonblocking = nonblocking;
        Ok(())
    }
}

#[cfg(test)]
impl Read for CapturedPeer {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut bytes = self.bytes.lock().unwrap();
        if bytes.is_empty() {
            return if self.nonblocking {
                Err(io::ErrorKind::WouldBlock.into())
            } else {
                Ok(0)
            };
        }
        let count = buf.len().min(bytes.len());
        for slot in &mut buf[..count] {
            *slot = bytes.pop_front().expect("count is within capture length");
        }
        Ok(count)
    }
}

impl Transport {
    #[cfg(test)]
    pub(crate) fn capture_pair() -> (Self, CapturedPeer) {
        let bytes = Arc::new(Mutex::new(VecDeque::new()));
        (
            Self::Capture(bytes.clone()),
            CapturedPeer {
                bytes,
                nonblocking: false,
            },
        )
    }

    pub fn pair() -> io::Result<(Self, UnixStream)> {
        UnixStream::pair().map(|(stream, peer)| (Self::Unix(stream), peer))
    }

    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        match self {
            Self::Unix(stream) => stream.set_nonblocking(nonblocking),
            Self::Tcp(stream) => stream.set_nonblocking(nonblocking),
            #[cfg(test)]
            Self::Capture(_) => Ok(()),
        }
    }

    pub fn try_clone(&self) -> io::Result<Self> {
        match self {
            Self::Unix(stream) => stream.try_clone().map(Self::Unix),
            Self::Tcp(stream) => stream.try_clone().map(Self::Tcp),
            #[cfg(test)]
            Self::Capture(bytes) => Ok(Self::Capture(bytes.clone())),
        }
    }

    pub fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        match self {
            Self::Unix(stream) => stream.shutdown(how),
            Self::Tcp(stream) => stream.shutdown(how),
            #[cfg(test)]
            Self::Capture(_) => Ok(()),
        }
    }

    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        match self {
            Self::Unix(stream) => stream.set_read_timeout(timeout),
            Self::Tcp(stream) => stream.set_read_timeout(timeout),
            #[cfg(test)]
            Self::Capture(_) => Ok(()),
        }
    }

    pub fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        match self {
            Self::Unix(stream) => stream.set_write_timeout(timeout),
            Self::Tcp(stream) => stream.set_write_timeout(timeout),
            #[cfg(test)]
            Self::Capture(_) => Ok(()),
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
            #[cfg(test)]
            Self::Capture(_) => Ok(0),
        }
    }
}

impl Write for Transport {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Unix(stream) => stream.write(buf),
            Self::Tcp(stream) => stream.write(buf),
            #[cfg(test)]
            Self::Capture(bytes) => {
                bytes.lock().unwrap().extend(buf.iter().copied());
                Ok(buf.len())
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Unix(stream) => stream.flush(),
            Self::Tcp(stream) => stream.flush(),
            #[cfg(test)]
            Self::Capture(_) => Ok(()),
        }
    }
}

impl AsRawFd for Transport {
    fn as_raw_fd(&self) -> RawFd {
        match self {
            Self::Unix(stream) => stream.as_raw_fd(),
            Self::Tcp(stream) => stream.as_raw_fd(),
            #[cfg(test)]
            Self::Capture(_) => -1,
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
