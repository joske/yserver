//! Core-thread → libinput-thread keyboard-LED relay (Direct mode).
//!
//! The XKB lock state (Caps/Num/Scroll) lives on the core thread in
//! `KmsCore.xkb_state`; the libinput context that can write LEDs via
//! `libinput_device_led_update` lives on the dedicated input thread in
//! Direct mode. The relay carries the desired LED bitmask across:
//! the core stores the mask and arms an eventfd that sits in the input
//! thread's epoll set alongside the libinput fd; the thread drains the
//! eventfd and applies the mask to every keyboard device.
//!
//! Libseat mode has no thread hop — the backend owns the libinput
//! context on the core thread and calls `Context::update_leds`
//! directly; the relay is unused there.

use std::{
    io,
    os::fd::{AsFd, AsRawFd, RawFd},
    sync::atomic::{AtomicU32, Ordering},
};

use nix::sys::eventfd::{EfdFlags, EventFd};

pub struct LedRelay {
    /// Desired LED state, `input::Led` bits.
    mask: AtomicU32,
    /// Wakes the input thread's epoll when the mask changes.
    efd: EventFd,
}

impl LedRelay {
    pub fn new() -> io::Result<Self> {
        let efd = EventFd::from_value_and_flags(0, EfdFlags::EFD_NONBLOCK | EfdFlags::EFD_CLOEXEC)
            .map_err(|e| io::Error::other(format!("LedRelay eventfd: {e}")))?;
        Ok(Self {
            mask: AtomicU32::new(0),
            efd,
        })
    }

    /// Core side: publish a new LED mask and wake the input thread.
    pub fn set(&self, led_bits: u32) {
        self.mask.store(led_bits, Ordering::Release);
        // Wake the input thread. The LED apply is gated on this eventfd
        // (the thread doesn't re-read the mask on ordinary input
        // events), so a dropped wakeup would leave the LED stale until
        // the NEXT lock transition. Retry on EINTR; log anything else.
        loop {
            match self.efd.write(1) {
                Ok(_) => break,
                Err(nix::errno::Errno::EINTR) => continue,
                Err(e) => {
                    log::warn!("LedRelay: eventfd wakeup write failed: {e}");
                    break;
                }
            }
        }
    }

    /// Input-thread side: fd to register in the epoll set.
    pub fn fd(&self) -> RawFd {
        self.efd.as_fd().as_raw_fd()
    }

    /// Input-thread side: clear the wakeup and read the current mask.
    /// EFD_NONBLOCK makes the read on an un-armed eventfd a harmless
    /// EAGAIN.
    pub fn drain(&self) -> u32 {
        let _ = self.efd.read();
        self.mask.load(Ordering::Acquire)
    }
}
