//! FreeBSD VT: `<sys/consio.h>` and `<sys/kbio.h>`.

use std::io;

pub(super) type IoctlReq = libc::c_ulong;

pub(super) const VT_ACTIVATE: IoctlReq = 537_163_269;
pub(super) const VT_SETMODE: IoctlReq = 2_148_038_146;
pub(super) const VT_RELDISP: IoctlReq = 537_163_268;
pub(super) const KDGKBMODE: IoctlReq = 1_074_023_174;
pub(super) const KDSKBMODE: IoctlReq = 537_152_263;
const KDGETMODE: IoctlReq = 1_074_023_177;
pub(super) const KDSETMODE: IoctlReq = 537_152_266;

const K_RAW: libc::c_int = 0x00;

pub(super) fn vt_device_path(n: u32) -> String {
    format!("/dev/ttyv{n:x}")
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

pub(super) fn vt_frsig(relsig: libc::c_int) -> libc::c_short {
    // FreeBSD vt(4) accepts frsig=0, but legacy syscons validates frsig as a
    // real signal even though the field is documented "not implemented yet".
    // Reusing relsig satisfies both drivers; actual release/acquire handling
    // still uses relsig/acqsig.
    relsig as libc::c_short
}

/// FreeBSD exposes `K_RAW` but not `K_OFF`.
pub(super) fn set_keyboard_raw_mode(raw_fd: libc::c_int) -> io::Result<&'static str> {
    // SAFETY: ioctl with a valid fd, no userspace pointer.
    let rc = unsafe { libc::ioctl(raw_fd, KDSKBMODE, K_RAW) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok("K_RAW")
}

#[cfg(test)]
mod tests {
    use super::vt_device_path;

    #[test]
    fn vt_device_path_uses_freebsd_ttyv_names() {
        assert_eq!(vt_device_path(7), "/dev/ttyv7");
        assert_eq!(vt_device_path(10), "/dev/ttyva");
    }
}
