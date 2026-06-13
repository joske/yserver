//! Linux virtual-console takeover for bare-metal yserver.
//!
//! When yserver is launched from a console TTY, the kernel keyboard layer
//! continues to translate keystrokes into characters on the active VT in
//! parallel with the evdev path. That means physical Ctrl-C generates a
//! `\x03` on the controlling TTY and SIGINT to its foreground process group
//! — even though the user is typing into an xterm window served by yserver.
//! The user's session dies as a side effect of trying to stop a command
//! inside an X client.
//!
//! Mirrors the behaviour of `xf86OpenConsole` in
//! `xserver/hw/xfree86/os-support/linux/lnx_init.c`: switch the active VT's
//! keyboard mode to `K_OFF` (fall back to `K_RAW`) and the VT to graphics
//! mode for the lifetime of the server. State is saved on acquire and
//! restored on drop (graceful exit, panic, or signalfd-driven shutdown).
//!
//! VT switching (`VT_PROCESS`/`VT_SETMODE`) is intentionally out of scope
//! for now; the immediate goal is to stop kernel keystroke translation.

use std::{
    fs::{File, OpenOptions},
    io,
    os::fd::AsRawFd,
};

use nix::sys::termios::{
    ControlFlags, InputFlags, LocalFlags, OutputFlags, SetArg, SpecialCharacterIndices, Termios,
    tcgetattr, tcsetattr,
};

// linux/kd.h
//
// `libc::Ioctl` is the request type for `libc::ioctl`: `c_ulong` on glibc,
// `c_int` on musl/Android. Use it directly so the crate builds on musl
// (issue #15). These constants are all small and fit either width.
const KDGKBMODE: libc::Ioctl = 0x4B44;
const KDSKBMODE: libc::Ioctl = 0x4B45;
const KDGETMODE: libc::Ioctl = 0x4B3B;
const KDSETMODE: libc::Ioctl = 0x4B3A;

const K_RAW: libc::c_long = 0x00;
const K_OFF: libc::c_long = 0x04;

const KD_GRAPHICS: libc::c_long = 0x01;

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
    /// log line) if we're not running on a Linux virtual console, since
    /// that's a normal/expected case for development.
    ///
    /// # Errors
    ///
    /// Returns an error if the controlling TTY is a Linux VC but `KDGETMODE`,
    /// `KDSKBMODE` (both `K_OFF` and `K_RAW` fallback), or `tcgetattr` fails
    /// — i.e. we identified ourselves as on a console but couldn't actually
    /// take it over. Non-VC TTYs (ptys, redirected stdin) are reported via
    /// `Ok(None)` and a log line, not an error.
    pub fn acquire() -> io::Result<Option<Self>> {
        let fd = match OpenOptions::new().read(true).write(true).open("/dev/tty") {
            Ok(f) => f,
            Err(err) => {
                log::info!(
                    "yserver: console takeover skipped (open /dev/tty: {err}); \
                     kernel keystroke→TTY translation not suppressed"
                );
                return Ok(None);
            }
        };
        let raw_fd = fd.as_raw_fd();

        // KDGKBMODE doubles as our "is this actually a Linux VC" probe: it
        // returns ENOTTY on ptys.
        let mut saved_keyboard_mode: libc::c_int = 0;
        // SAFETY: ioctl with a valid fd writing into a stack-local int.
        let rc = unsafe { libc::ioctl(raw_fd, KDGKBMODE, &mut saved_keyboard_mode) };
        if rc < 0 {
            let err = io::Error::last_os_error();
            log::info!(
                "yserver: console takeover skipped (KDGKBMODE: {err}); \
                 controlling TTY is not a Linux VC"
            );
            return Ok(None);
        }

        let mut saved_screen_mode: libc::c_int = 0;
        // SAFETY: ioctl with a valid fd writing into a stack-local int.
        let rc = unsafe { libc::ioctl(raw_fd, KDGETMODE, &mut saved_screen_mode) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }

        let saved_termios = tcgetattr(&fd).map_err(io::Error::from)?;

        // Stop the kernel from feeding characters to the TTY. Prefer K_OFF
        // (no events at all on the VT); fall back to K_RAW for older
        // kernels that don't accept K_OFF.
        let used_mode = unsafe {
            let rc = libc::ioctl(raw_fd, KDSKBMODE, K_OFF);
            if rc < 0 {
                let rc2 = libc::ioctl(raw_fd, KDSKBMODE, K_RAW);
                if rc2 < 0 {
                    return Err(io::Error::last_os_error());
                }
                "K_RAW"
            } else {
                "K_OFF"
            }
        };

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
}

impl Drop for ConsoleGuard {
    fn drop(&mut self) {
        let raw_fd = self.fd.as_raw_fd();

        // Restore in reverse order. Failures here are logged but not
        // surfaced — there's nothing the caller can do at this point.
        // SAFETY: ioctl with a valid fd, no userspace pointer.
        let rc = unsafe {
            libc::ioctl(
                raw_fd,
                KDSETMODE,
                libc::c_long::from(self.saved_screen_mode),
            )
        };
        if rc < 0 {
            log::warn!(
                "yserver: KDSETMODE restore failed: {}",
                io::Error::last_os_error()
            );
        }
        // SAFETY: ioctl with a valid fd, no userspace pointer.
        let rc = unsafe {
            libc::ioctl(
                raw_fd,
                KDSKBMODE,
                libc::c_long::from(self.saved_keyboard_mode),
            )
        };
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
