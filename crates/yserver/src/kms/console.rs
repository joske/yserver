//! Virtual-console takeover for bare-metal yserver.
//!
//! When yserver is launched from a real console TTY, the kernel keyboard
//! layer continues to translate keystrokes into characters on the active
//! VT in parallel with the evdev path. That means physical Ctrl-C generates
//! a `\x03` on the controlling TTY and SIGINT to its foreground process
//! group — even though the user is typing into an xterm window served by
//! yserver. The user's session dies as a side effect of trying to stop a
//! command inside an X client.
//!
//! Mirrors the behaviour of `xf86OpenConsole` in
//! `xserver/hw/xfree86/os-support/linux/lnx_init.c`: switch the active VT's
//! keyboard mode away from cooked TTY translation (`K_OFF` on Linux,
//! `K_RAW` on FreeBSD) and the VT to graphics mode for the lifetime of the
//! server. State is saved on acquire and restored on drop (graceful exit,
//! panic, or signalfd-driven shutdown).
//!
//! VT switching is handled separately: the direct-mode path can arm
//! `VT_PROCESS` on the controlling VT so Ctrl-Alt-F<n> switch signals
//! are delivered through the core loop.

use std::{
    fs::{File, OpenOptions},
    io,
    os::fd::AsRawFd,
};

use nix::sys::termios::{
    ControlFlags, InputFlags, LocalFlags, OutputFlags, SetArg, SpecialCharacterIndices, Termios,
    tcgetattr, tcsetattr,
};

#[cfg(target_os = "freebsd")]
#[path = "console/freebsd.rs"]
mod os;
#[cfg(target_os = "linux")]
#[path = "console/linux.rs"]
mod os;
#[cfg(target_os = "openbsd")]
#[path = "console/openbsd.rs"]
mod os;

use os::{
    KDGKBMODE, KDSETMODE, KDSKBMODE, VT_ACTIVATE, VT_RELDISP, VT_SETMODE, screen_mode,
    set_keyboard_raw_mode, vt_device_path, vt_frsig,
};

const VT_AUTO: libc::c_char = 0;
const VT_PROCESS: libc::c_char = 1;
pub(crate) const VT_ACKACQ: libc::c_long = 2;

const KD_GRAPHICS: libc::c_int = 0x01;

/// Kernel `vt_mode` layout for `VT_SETMODE`.
///
/// `#[repr(C)]` is required: the kernel reads a fixed C layout.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct VtMode {
    mode: libc::c_char,
    waitv: libc::c_char,
    relsig: libc::c_short,
    acqsig: libc::c_short,
    frsig: libc::c_short,
}

/// RAII guard for the console TTY. Restores keyboard mode, console mode,
/// and termios on drop. `None` means we're not on a console TTY (e.g. a
/// pty under SSH or a graphical terminal emulator) and there's nothing to
/// do — the bug doesn't exist there.
pub struct ConsoleGuard {
    fd: File,
    saved_keyboard_mode: libc::c_int,
    saved_screen_mode: libc::c_int,
    saved_termios: Termios,
}

impl ConsoleGuard {
    /// Try to take over the controlling TTY. Returns `Ok(None)` (with a
    /// log line) if we're not running on a supported real virtual console, since
    /// that's a normal/expected case for development.
    ///
    /// # Errors
    ///
    /// Returns an error if the controlling TTY is a real VT but `KDGETMODE`,
    /// `KDSKBMODE`, or `tcgetattr` fails
    /// — i.e. we identified ourselves as on a console but couldn't actually
    /// take it over. Non-VC TTYs (ptys, redirected stdin) are reported via
    /// `Ok(None)` and a log line, not an error.
    pub fn acquire(vt: Option<u32>) -> io::Result<Option<Self>> {
        // Prefer the explicit VT device (`/dev/ttyN` on Linux,
        // `/dev/ttyvN` on FreeBSD, `/dev/ttyC{N-1}` on OpenBSD, from the
        // `vtN` launch arg) over
        // `/dev/tty`. A display-manager-launched server has NO
        // controlling terminal, so `/dev/tty` fails with ENXIO and we
        // never take over the console or arm VT switching. The explicit
        // VT device opens regardless of controlling-terminal status —
        // matching Xorg's `xf86OpenConsole`, which opens the VT by
        // number. Falls back to `/dev/tty` when no VT was given
        // (shell-launched, where the controlling tty IS the VT).
        let path = vt.map_or_else(|| "/dev/tty".to_string(), vt_device_path);
        let fd = match OpenOptions::new().read(true).write(true).open(&path) {
            Ok(f) => f,
            Err(err) => {
                log::info!(
                    "yserver: console takeover skipped (open {path}: {err}); \
                     kernel keystroke→TTY translation not suppressed"
                );
                return Ok(None);
            }
        };
        let raw_fd = fd.as_raw_fd();

        // KDGKBMODE doubles as our "is this actually a real VT" probe: it
        // returns ENOTTY on ptys.
        let mut saved_keyboard_mode: libc::c_int = 0;
        // SAFETY: ioctl with a valid fd writing into a stack-local int.
        let rc = unsafe { libc::ioctl(raw_fd, KDGKBMODE, &mut saved_keyboard_mode) };
        if rc < 0 {
            let err = io::Error::last_os_error();
            log::info!(
                "yserver: console takeover skipped (KDGKBMODE: {err}); \
                 controlling TTY is not a supported VT"
            );
            return Ok(None);
        }

        let saved_screen_mode = screen_mode(raw_fd)?;

        let saved_termios = tcgetattr(&fd).map_err(io::Error::from)?;

        // Stop the kernel from feeding characters to the TTY. Linux prefers
        // K_OFF (no TTY-side keyboard processing at all) and falls back to
        // K_RAW. FreeBSD exposes K_RAW but not K_OFF. OpenBSD leaves the
        // mode alone (see its `set_keyboard_raw_mode`).
        let used_mode = set_keyboard_raw_mode(raw_fd)?;

        // KDSETMODE is best-effort: if the user lacks CAP_SYS_TTY_CONFIG
        // we still benefit from K_OFF alone.
        // SAFETY: ioctl with a valid fd, no userspace pointer.
        let rc = unsafe { libc::ioctl(raw_fd, KDSETMODE, KD_GRAPHICS) };
        if rc < 0 {
            log::warn!(
                "yserver: KDSETMODE KD_GRAPHICS failed: {}",
                io::Error::last_os_error()
            );
        }

        // Belt-and-suspenders: raw-ish termios so any stray bytes that do
        // reach the TTY don't get cooked. Mirrors xf86OpenConsole.
        let mut new_t = saved_termios.clone();
        new_t.input_flags =
            (InputFlags::IGNPAR | InputFlags::IGNBRK) & !InputFlags::PARMRK & !InputFlags::ISTRIP;
        new_t.output_flags = OutputFlags::empty();
        new_t.control_flags = ControlFlags::CREAD | ControlFlags::CS8;
        new_t.local_flags = LocalFlags::empty();
        new_t.control_chars[SpecialCharacterIndices::VTIME as usize] = 0;
        new_t.control_chars[SpecialCharacterIndices::VMIN as usize] = 1;
        if let Err(err) = tcsetattr(&fd, SetArg::TCSANOW, &new_t) {
            log::warn!("yserver: tcsetattr failed: {err}");
        }

        log::info!("yserver: console takeover via KDSKBMODE={used_mode} + KD_GRAPHICS");

        Ok(Some(Self {
            fd,
            saved_keyboard_mode,
            saved_screen_mode,
            saved_termios,
        }))
    }

    /// Arm `VT_PROCESS` on the controlling VT so release/acquire signals
    /// are delivered to this process.
    pub fn arm_vt_process(&self, relsig: libc::c_int, acqsig: libc::c_int) -> io::Result<()> {
        self.set_vt_mode(VtMode {
            mode: VT_PROCESS,
            waitv: 0,
            relsig: relsig as libc::c_short,
            acqsig: acqsig as libc::c_short,
            frsig: vt_frsig(relsig),
        })
    }

    /// Restore `VT_AUTO` on the controlling VT.
    pub fn disarm_vt_process(&self) -> io::Result<()> {
        self.set_vt_mode(VtMode {
            mode: VT_AUTO,
            waitv: 0,
            relsig: 0,
            acqsig: 0,
            frsig: 0,
        })
    }

    /// Request a switch to VT `n` via `VT_ACTIVATE`. Non-blocking: the
    /// kernel marks the switch pending and (since we armed `VT_PROCESS`)
    /// sends us the release signal; we must NOT `VT_WAITACTIVE` here or we
    /// would block the core loop that has to run the release handshake.
    /// Mirrors Xorg's `xf86_vt_switch` → `ioctl(VT_ACTIVATE)`.
    pub fn vt_activate(&self, n: u32) -> io::Result<()> {
        let raw_fd = self.fd.as_raw_fd();
        // SAFETY: ioctl with a valid fd, no userspace pointer.
        let rc = unsafe { libc::ioctl(raw_fd, VT_ACTIVATE, vt_ioctl_index_arg(n)?) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Acknowledge a VT release/acquire event via `VT_RELDISP`.
    pub fn vt_reldisp(&self, arg: libc::c_long) -> io::Result<()> {
        let raw_fd = self.fd.as_raw_fd();
        // SAFETY: ioctl with a valid fd, no userspace pointer.
        let rc = unsafe {
            libc::ioctl(
                raw_fd,
                VT_RELDISP,
                libc::c_int::try_from(arg).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("VT_RELDISP argument out of range: {arg}"),
                    )
                })?,
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn set_vt_mode(&self, mode: VtMode) -> io::Result<()> {
        let raw_fd = self.fd.as_raw_fd();
        // SAFETY: ioctl with a valid fd and a kernel-defined C struct.
        let rc = unsafe { libc::ioctl(raw_fd, VT_SETMODE, &mode) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for ConsoleGuard {
    fn drop(&mut self) {
        let raw_fd = self.fd.as_raw_fd();

        if let Err(err) = self.disarm_vt_process() {
            log::warn!("yserver: VT_AUTO restore failed: {err}");
        }

        // Restore in reverse order. Failures here are logged but not
        // surfaced — there's nothing the caller can do at this point.
        // SAFETY: ioctl with a valid fd, no userspace pointer.
        let rc = unsafe { libc::ioctl(raw_fd, KDSETMODE, self.saved_screen_mode) };
        if rc < 0 {
            log::warn!(
                "yserver: KDSETMODE restore failed: {}",
                io::Error::last_os_error()
            );
        }
        // SAFETY: ioctl with a valid fd, no userspace pointer.
        let rc = unsafe { libc::ioctl(raw_fd, KDSKBMODE, self.saved_keyboard_mode) };
        if rc < 0 {
            // If this happens the user may need `kbd_mode -a` or a VT
            // switch to recover keystrokes on the console.
            log::error!(
                "yserver: KDSKBMODE restore failed: {} — run `kbd_mode -a` if console keyboard is dead",
                io::Error::last_os_error()
            );
        }
        if let Err(err) = tcsetattr(&self.fd, SetArg::TCSANOW, &self.saved_termios) {
            log::warn!("yserver: tcsetattr restore failed: {err}");
        }

        log::info!("yserver: console state restored");
    }
}

fn vt_ioctl_index_arg(n: u32) -> io::Result<libc::c_int> {
    libc::c_int::try_from(n).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("VT index out of range: {n}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::VtMode;
    use std::mem::{offset_of, size_of};

    #[test]
    fn vt_mode_matches_kernel_layout() {
        assert_eq!(size_of::<VtMode>(), 8);
        assert_eq!(offset_of!(VtMode, mode), 0);
        assert_eq!(offset_of!(VtMode, waitv), 1);
        assert_eq!(offset_of!(VtMode, relsig), 2);
        assert_eq!(offset_of!(VtMode, acqsig), 4);
        assert_eq!(offset_of!(VtMode, frsig), 6);
    }
}
