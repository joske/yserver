//! Cross-platform readiness set for deferred PRESENT-completion FDs.
//!
//! Wraps the OS-native pollable object — `epoll` on Linux, `kqueue` on
//! FreeBSD — behind one uniform API so the rest of v2 (`platform`,
//! `backend`) never imports `nix::sys::epoll`/`event` and carries no
//! `#[cfg(target_os = …)]` blocks for this concern. All the per-OS
//! divergence (including the asymmetric `AsFd` impls — nix's `Kqueue`
//! implements `AsFd`, its `Epoll` does not) lives here.
//!
//! The object is a pure readiness *aggregator*: callers add/remove FDs
//! and hand its raw fd to the core loop's poll set (`poll_fds`). It is
//! never waited on directly here — the outer mio `Poll` does the
//! waiting, and completion is detected by polling the per-batch
//! sync_file FDs. The `token` recorded at registration mirrors the
//! native epoll-data / kqueue-udata for parity but is not consumed by
//! any wait in this server.

use std::{
    io,
    os::fd::{AsRawFd, BorrowedFd, RawFd},
};

#[cfg(target_os = "linux")]
use nix::sys::epoll::{Epoll, EpollCreateFlags, EpollEvent, EpollFlags};

#[cfg(target_os = "freebsd")]
use nix::sys::event::{EvFlags, EventFilter, FilterFlag, KEvent, Kqueue};

/// Zero-timeout poll for the FreeBSD `kevent` changelist calls (apply
/// changes, never block waiting for events).
#[cfg(target_os = "freebsd")]
const ZERO_TIMEOUT: libc::timespec = libc::timespec {
    tv_sec: 0,
    tv_nsec: 0,
};

/// OS-native readiness set (epoll on Linux, kqueue on FreeBSD) for
/// deferred PRESENT-completion FDs. See module docs.
pub(crate) struct CompletionPoller {
    #[cfg(target_os = "linux")]
    inner: Epoll,
    #[cfg(target_os = "freebsd")]
    inner: Kqueue,
}

impl CompletionPoller {
    /// Create an empty readiness set.
    ///
    /// # Errors
    /// Propagates `epoll_create1` / `kqueue` failures.
    pub(crate) fn new() -> io::Result<Self> {
        #[cfg(target_os = "linux")]
        {
            let inner = Epoll::new(EpollCreateFlags::EPOLL_CLOEXEC)
                .map_err(|e| io::Error::other(format!("epoll_create1: {e}")))?;
            Ok(Self { inner })
        }
        #[cfg(target_os = "freebsd")]
        {
            let inner = Kqueue::new().map_err(|e| io::Error::other(format!("kqueue: {e}")))?;
            Ok(Self { inner })
        }
    }

    /// Register `fd` for level-triggered read-readiness, associated with
    /// `token` (epoll event-data / kqueue udata).
    ///
    /// # Errors
    /// Propagates `epoll_ctl ADD` / `kevent EV_ADD` failures.
    pub(crate) fn register(&self, fd: BorrowedFd<'_>, token: u64) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        {
            self.inner
                .add(fd, EpollEvent::new(EpollFlags::EPOLLIN, token))
                .map_err(|e| io::Error::other(format!("epoll_ctl ADD: {e}")))
        }
        #[cfg(target_os = "freebsd")]
        {
            let changes = [KEvent::new(
                fd.as_raw_fd() as usize,
                EventFilter::EVFILT_READ,
                EvFlags::EV_ADD,
                FilterFlag::empty(),
                0,
                token as isize,
            )];
            let mut out: Vec<KEvent> = Vec::new();
            self.inner
                .kevent(&changes, &mut out, Some(ZERO_TIMEOUT))
                .map(|_| ())
                .map_err(|e| io::Error::other(format!("kevent EV_ADD: {e}")))
        }
    }

    /// Remove `fd` from the set.
    ///
    /// # Errors
    /// Propagates `epoll_ctl DEL` / `kevent EV_DELETE` failures.
    pub(crate) fn unregister(&self, fd: BorrowedFd<'_>) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        {
            self.inner
                .delete(fd)
                .map_err(|e| io::Error::other(format!("epoll_ctl DEL: {e}")))
        }
        #[cfg(target_os = "freebsd")]
        {
            let changes = [KEvent::new(
                fd.as_raw_fd() as usize,
                EventFilter::EVFILT_READ,
                EvFlags::EV_DELETE,
                FilterFlag::empty(),
                0,
                0,
            )];
            let mut out: Vec<KEvent> = Vec::new();
            self.inner
                .kevent(&changes, &mut out, Some(ZERO_TIMEOUT))
                .map(|_| ())
                .map_err(|e| io::Error::other(format!("kevent EV_DELETE: {e}")))
        }
    }
}

impl AsRawFd for CompletionPoller {
    fn as_raw_fd(&self) -> RawFd {
        // nix asymmetry: `Epoll` exposes its inner `OwnedFd` as `.0` but
        // does not implement `AsFd`/`AsRawFd`; `Kqueue` implements `AsFd`.
        #[cfg(target_os = "linux")]
        {
            self.inner.0.as_raw_fd()
        }
        #[cfg(target_os = "freebsd")]
        {
            use std::os::fd::AsFd;
            self.inner.as_fd().as_raw_fd()
        }
    }
}
