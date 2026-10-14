//! Anonymous shared-memory file descriptors for MIT-SHM `CreateSegment`.
//!
//! Linux and FreeBSD use `memfd_create`. Targets without it get a POSIX
//! shared-memory object that is unlinked right after it is opened, so only
//! the descriptor keeps it alive.

use std::{
    ffi::CStr,
    io,
    os::fd::{FromRawFd, OwnedFd},
};

/// Create a close-on-exec anonymous shared-memory fd of size zero.
/// `name` is a debugging label only.
pub fn create_anon_shm(name: &CStr) -> io::Result<OwnedFd> {
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    {
        let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
    #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
    {
        let _ = name;
        create_unlinked_posix_shm()
    }
}

/// `shm_open(O_EXCL)` under a unique name, `shm_unlink`, then `FD_CLOEXEC`.
#[cfg_attr(
    all(any(target_os = "linux", target_os = "freebsd"), not(test)),
    allow(dead_code)
)]
fn create_unlinked_posix_shm() -> io::Result<OwnedFd> {
    use std::{
        sync::atomic::{AtomicU32, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };
    static COUNTER: AtomicU32 = AtomicU32::new(0);

    for _ in 0..64 {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        // macOS limits POSIX shm names to 31 bytes (PSHMNAMLEN).
        let name = format!(
            "/ys{:x}.{seq:x}.{nanos:x}\0",
            std::process::id() & 0xff_ffff
        );
        let cname = CStr::from_bytes_with_nul(name.as_bytes()).map_err(io::Error::other)?;
        // Apple declares shm_open variadic, so the mode is promoted to c_uint.
        #[cfg(target_vendor = "apple")]
        let mode: libc::c_uint = 0o600;
        #[cfg(not(target_vendor = "apple"))]
        let mode: libc::mode_t = 0o600;
        let fd = unsafe {
            libc::shm_open(
                cname.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
                mode,
            )
        };
        if fd < 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EEXIST) {
                continue;
            }
            return Err(err);
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        unsafe { libc::shm_unlink(cname.as_ptr()) };
        set_cloexec(&fd)?;
        return Ok(fd);
    }
    Err(io::Error::from(io::ErrorKind::AlreadyExists))
}

fn set_cloexec(fd: &OwnedFd) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let raw = fd.as_raw_fd();
    let flags = unsafe { libc::fcntl(raw, libc::F_GETFD) };
    if flags < 0 || unsafe { libc::fcntl(raw, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{create_anon_shm, create_unlinked_posix_shm};
    use std::os::fd::{AsRawFd, OwnedFd};

    fn assert_usable_shm(fd: &OwnedFd) {
        let raw = fd.as_raw_fd();
        let flags = unsafe { libc::fcntl(raw, libc::F_GETFD) };
        assert!(flags & libc::FD_CLOEXEC != 0, "fd is close-on-exec");
        assert_eq!(unsafe { libc::ftruncate(raw, 4096) }, 0, "ftruncate");
        let map = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                4096,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                raw,
                0,
            )
        };
        assert_ne!(map, libc::MAP_FAILED, "mmap");
        unsafe {
            map.cast::<u8>().write(0x5a);
            assert_eq!(map.cast::<u8>().read(), 0x5a);
            libc::munmap(map, 4096);
        }
    }

    #[test]
    fn anon_shm_is_cloexec_and_mappable() {
        let fd = create_anon_shm(c"anon-shm-test").expect("create_anon_shm");
        assert_usable_shm(&fd);
    }

    #[test]
    fn posix_shm_fallback_is_cloexec_mappable_and_unique() {
        let a = create_unlinked_posix_shm().expect("first shm_open fallback");
        let b = create_unlinked_posix_shm().expect("second shm_open fallback");
        assert_ne!(a.as_raw_fd(), b.as_raw_fd());
        assert_usable_shm(&a);
        assert_usable_shm(&b);
    }
}
