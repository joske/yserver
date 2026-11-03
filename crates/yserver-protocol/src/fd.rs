//! The platform's file descriptor types.
//!
//! Windows has no descriptors to pass. Its `OwnedFd` and `BorrowedFd` are
//! uninhabited, so every path that would carry one (DRI3, MIT-SHM
//! `AttachFd`, sync files) is unreachable there. [`RawSocket`] is what the
//! socket readiness APIs take: a descriptor, or a Windows `SOCKET`.

#[cfg(unix)]
pub use std::os::fd::{AsFd, AsRawFd, BorrowedFd, IntoRawFd, OwnedFd, RawFd};
#[cfg(unix)]
pub type RawSocket = RawFd;
#[cfg(windows)]
pub use std::os::windows::io::RawSocket;

#[cfg(windows)]
pub use self::stand_in::{AsFd, AsRawFd, BorrowedFd, IntoRawFd, OwnedFd, RawFd};

#[cfg(windows)]
mod stand_in {
    pub type RawFd = i32;

    #[derive(Debug)]
    pub enum OwnedFd {}

    impl OwnedFd {
        pub fn try_clone(&self) -> std::io::Result<Self> {
            match *self {}
        }
    }

    #[derive(Debug, Clone, Copy)]
    pub struct BorrowedFd<'fd>(&'fd OwnedFd);

    pub trait AsFd {
        fn as_fd(&self) -> BorrowedFd<'_>;
    }

    pub trait AsRawFd {
        fn as_raw_fd(&self) -> RawFd;
    }

    pub trait IntoRawFd {
        fn into_raw_fd(self) -> RawFd;
    }

    impl Drop for OwnedFd {
        fn drop(&mut self) {
            match *self {}
        }
    }

    impl AsFd for OwnedFd {
        fn as_fd(&self) -> BorrowedFd<'_> {
            match *self {}
        }
    }

    impl IntoRawFd for OwnedFd {
        fn into_raw_fd(self) -> RawFd {
            match self {}
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
