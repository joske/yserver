//! Linux VT: `<linux/vt.h>` and `<linux/kd.h>`.

use std::io;

#[cfg(target_env = "musl")]
pub(super) type IoctlReq = libc::c_int;
#[cfg(not(target_env = "musl"))]
pub(super) type IoctlReq = libc::c_ulong;

pub(super) const VT_ACTIVATE: IoctlReq = 0x5606;
pub(super) const VT_SETMODE: IoctlReq = 0x5602;
pub(super) const VT_RELDISP: IoctlReq = 0x5605;
pub(super) const KDGKBMODE: IoctlReq = 0x4B44;
pub(super) const KDSKBMODE: IoctlReq = 0x4B45;
const KDGETMODE: IoctlReq = 0x4B3B;
pub(super) const KDSETMODE: IoctlReq = 0x4B3A;

const K_RAW: libc::c_int = 0x00;
const K_OFF: libc::c_int = 0x04;

pub(super) fn vt_device_path(n: u32) -> String {
    format!("/dev/tty{n}")
}

pub(super) fn screen_mode(raw_fd: libc::c_int) -> io::Result<libc::c_int> {
    let mut mode: libc::c_int = 0;
    // SAFETY: ioctl with a valid fd writing into a stack-local int.
    let rc = unsafe { libc::ioctl(raw_fd, KDGETMODE, &mut mode) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(mode)
}

pub(super) fn vt_frsig(_: libc::c_int) -> libc::c_short {
    0
}

pub(super) fn set_keyboard_raw_mode(raw_fd: libc::c_int) -> io::Result<&'static str> {
    // SAFETY: ioctl with a valid fd, no userspace pointer.
    let rc = unsafe { libc::ioctl(raw_fd, KDSKBMODE, K_OFF) };
    if rc < 0 {
        // SAFETY: ioctl with a valid fd, no userspace pointer.
        let rc2 = unsafe { libc::ioctl(raw_fd, KDSKBMODE, K_RAW) };
        if rc2 < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok("K_RAW")
    } else {
        Ok("K_OFF")
    }
}

#[cfg(test)]
mod tests {
    use super::vt_device_path;

    #[test]
    fn vt_device_path_uses_linux_tty_names() {
        assert_eq!(vt_device_path(7), "/dev/tty7");
    }
}
