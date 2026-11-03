//! The surface's frame clock ticked once per desktop composition: a thread
//! waits in `DwmFlush` and ticks after each one, at the time it returned.

use std::{
    io, ptr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use windows_sys::Win32::Graphics::{
    Dwm::DwmFlush,
    Gdi::{DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW},
};

use crate::surface::clock::{FALLBACK_REFRESH_HZ, FrameClock};

/// A running ticker; stopped and joined on drop.
pub(super) struct DwmTicker {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl DwmTicker {
    /// Tick `clock` after every composition of the desktop window manager;
    /// at the primary display's rate from a timer while `DwmFlush` fails.
    ///
    /// # Errors
    ///
    /// The thread could not be spawned.
    pub(super) fn start(clock: Arc<FrameClock>) -> io::Result<Self> {
        let hz = refresh_hz();
        clock.set_refresh_mhz(hz * 1000);
        log::info!("windows: frame clock at {hz} Hz (DwmFlush)");
        let period = Duration::from_micros(1_000_000 / u64::from(hz));
        let stop = Arc::new(AtomicBool::new(false));
        let origin = Instant::now();
        let thread = {
            let stop = Arc::clone(&stop);
            thread::Builder::new()
                .name("yserver-frame-clock".into())
                .spawn(move || {
                    let mut last = Instant::now();
                    let mut warned = false;
                    while !stop.load(Ordering::Acquire) {
                        // SAFETY: no arguments; blocks until the next
                        // composition.
                        let flushed = unsafe { DwmFlush() } >= 0;
                        if !flushed && !warned {
                            warned = true;
                            log::warn!("windows: DwmFlush failed; ticking from a timer");
                        }
                        // No composition to wait for (nothing changed, or
                        // DWM failed): keep to the refresh period.
                        let since = last.elapsed();
                        if since < period / 2 || !flushed {
                            thread::sleep(period.saturating_sub(since));
                        }
                        last = Instant::now();
                        let ust = u64::try_from(origin.elapsed().as_micros()).unwrap_or(u64::MAX);
                        clock.tick(ust);
                    }
                })?
        };
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for DwmTicker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The primary display's refresh rate, or the fallback when it does not
/// say (0 and 1 mean the hardware default).
fn refresh_hz() -> u32 {
    // SAFETY: zeroed is a valid DEVMODEW; `dmSize` is set below.
    let mut mode: DEVMODEW = unsafe { std::mem::zeroed() };
    mode.dmSize = u16::try_from(size_of::<DEVMODEW>()).unwrap_or(0);
    // SAFETY: the primary display (null name) into a sized DEVMODEW.
    let ok = unsafe { EnumDisplaySettingsW(ptr::null(), ENUM_CURRENT_SETTINGS, &raw mut mode) };
    if ok != 0 && mode.dmDisplayFrequency > 1 {
        mode.dmDisplayFrequency
    } else {
        FALLBACK_REFRESH_HZ
    }
}
