//! The platform's file descriptor types.
//!
//! Windows has no descriptors to pass. Its `OwnedFd` and `BorrowedFd` are
//! uninhabited, so every path that would carry one (DRI3, MIT-SHM
//! `AttachFd`) is unreachable there, and its `RawFd` is a raw socket, the
//! one handle kind its readiness APIs take.

#[cfg(unix)]
pub use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd, RawFd};

#[cfg(windows)]
pub use self::stand_in::{AsRawFd, BorrowedFd, OwnedFd, RawFd};

#[cfg(windows)]
mod stand_in {
    pub type RawFd = std::os::windows::io::RawSocket;

    #[derive(Debug)]
    pub enum OwnedFd {}

    #[derive(Debug, Clone, Copy)]
    pub struct BorrowedFd<'fd>(&'fd OwnedFd);

    pub trait AsRawFd {
        fn as_raw_fd(&self) -> RawFd;
    }

    impl Drop for OwnedFd {
        fn drop(&mut self) {
            match *self {}
        }
    }

    impl AsRawFd for OwnedFd {
        fn as_raw_fd(&self) -> RawFd {
            match *self {}
        }
    }

    impl AsRawFd for BorrowedFd<'_> {
        fn as_raw_fd(&self) -> RawFd {
            match *self.0 {}
        }
    }
}
