//! A pollable, non-blocking wakeup: `eventfd` where the kernel has one, a
//! pipe elsewhere (OpenBSD). Both expose `EventFd`'s `write`/`read`/`as_fd`.

#[cfg(eventfd)]
pub(crate) use nix::sys::eventfd::EventFd as WakeupFd;

#[cfg(eventfd)]
pub(crate) fn wakeup_fd() -> nix::Result<WakeupFd> {
    use nix::sys::eventfd::EfdFlags;
    WakeupFd::from_value_and_flags(0, EfdFlags::EFD_NONBLOCK | EfdFlags::EFD_CLOEXEC)
}

#[cfg(not(eventfd))]
pub(crate) use pipe::WakeupFd;

#[cfg(not(eventfd))]
pub(crate) fn wakeup_fd() -> nix::Result<WakeupFd> {
    WakeupFd::new()
}

#[cfg(not(eventfd))]
mod pipe {
    use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

    use nix::{errno::Errno, fcntl::OFlag};

    #[derive(Debug)]
    pub(crate) struct WakeupFd {
        read: OwnedFd,
        write: OwnedFd,
    }

    impl WakeupFd {
        pub(super) fn new() -> nix::Result<Self> {
            let (read, write) = nix::unistd::pipe2(OFlag::O_NONBLOCK | OFlag::O_CLOEXEC)?;
            Ok(Self { read, write })
        }

        /// Make the read end readable. A full pipe already is, so EAGAIN
        /// counts as delivered.
        pub(crate) fn write(&self, _value: u64) -> nix::Result<usize> {
            match nix::unistd::write(&self.write, &[1]) {
                Err(Errno::EAGAIN) => Ok(0),
                result => result,
            }
        }

        /// Drain every pending wakeup. Like an unsignalled eventfd, an empty
        /// pipe reads as EAGAIN.
        pub(crate) fn read(&self) -> nix::Result<u64> {
            let mut buf = [0_u8; 64];
            let mut total = 0_u64;
            loop {
                match nix::unistd::read(&self.read, &mut buf) {
                    Ok(0) => return Ok(total),
                    Ok(n) => total += n as u64,
                    Err(Errno::EAGAIN) if total > 0 => return Ok(total),
                    Err(err) => return Err(err),
                }
            }
        }
    }

    impl AsFd for WakeupFd {
        fn as_fd(&self) -> BorrowedFd<'_> {
            self.read.as_fd()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsFd;

    use nix::{
        errno::Errno,
        poll::{PollFd, PollFlags, PollTimeout, poll},
    };

    use super::wakeup_fd;

    fn readable(fd: &impl AsFd) -> bool {
        let mut fds = [PollFd::new(fd.as_fd(), PollFlags::POLLIN)];
        poll(&mut fds, PollTimeout::ZERO).expect("poll") == 1
    }

    #[test]
    fn a_write_wakes_until_the_read_drains_it() {
        let wakeup = wakeup_fd().expect("wakeup fd");
        assert!(!readable(&wakeup));
        assert_eq!(wakeup.read(), Err(Errno::EAGAIN));

        wakeup.write(1).expect("first wake");
        wakeup.write(1).expect("second wake");
        assert!(readable(&wakeup));

        assert!(wakeup.read().is_ok());
        assert!(!readable(&wakeup));
        assert_eq!(wakeup.read(), Err(Errno::EAGAIN));
    }
}
