//! OpenBSD VT: wsdisplay's USL compatibility, `<dev/wscons/wsdisplay_usl_io.h>`.
//! Follows Xorg's `xf86OpenPcvt`/`xf86OpenConsole` for OpenBSD.

use std::io;

pub(super) type IoctlReq = libc::c_ulong;

pub(super) const VT_ACTIVATE: IoctlReq = 536_901_125;
pub(super) const VT_SETMODE: IoctlReq = 2_148_038_146;
pub(super) const VT_RELDISP: IoctlReq = 536_901_124;
pub(super) const KDGKBMODE: IoctlReq = 1_074_023_174;
pub(super) const KDSKBMODE: IoctlReq = 536_890_119;
pub(super) const KDSETMODE: IoctlReq = 536_890_122;

const KD_TEXT: libc::c_int = 0x00;

/// VT n is wsdisplay screen n-1.
pub(super) fn vt_device_path(n: u32) -> String {
    format!("/dev/ttyC{:x}", n.saturating_sub(1))
}

/// There is no `KDGETMODE`; Xorg restores `KD_TEXT` on exit.
#[allow(clippy::unnecessary_wraps)]
pub(super) fn screen_mode(_raw_fd: libc::c_int) -> io::Result<libc::c_int> {
    Ok(KD_TEXT)
}

/// Xorg sets frsig to the release signal too.
pub(super) fn vt_frsig(relsig: libc::c_int) -> libc::c_short {
    relsig as libc::c_short
}

/// Xorg's BSD console code never sets the keyboard mode on OpenBSD either.
#[allow(clippy::unnecessary_wraps)]
pub(super) fn set_keyboard_raw_mode(_raw_fd: libc::c_int) -> io::Result<&'static str> {
    Ok("unchanged")
}

#[cfg(test)]
mod tests {
    use super::vt_device_path;

    #[test]
    fn vt_device_path_uses_openbsd_ttyc_names() {
        assert_eq!(vt_device_path(1), "/dev/ttyC0");
        assert_eq!(vt_device_path(12), "/dev/ttyCb");
    }
}
