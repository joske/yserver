//! The per-surface frame clock: a display-link (or software) tick count and
//! time. A tick wakes the core loop only when the core armed it: a frame
//! awaiting retirement, or a Present MSC target.

use std::{
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use super::link::ShellLink;

/// No MSC target armed.
const NO_TARGET: u64 = u64::MAX;

/// Refresh rate reported while the tick source has not said.
pub(crate) const FALLBACK_REFRESH_HZ: u32 = 60;

/// One surface's MSC/UST and armed wakes; ticked from any thread.
pub(crate) struct FrameClock {
    /// `(msc, ust_us)` of the latest tick.
    sample: Mutex<(u64, u64)>,
    /// Wake on the next tick (a frame awaits retirement).
    tick_armed: AtomicBool,
    /// Wake on the first tick whose MSC reaches this.
    target_msc: AtomicU64,
    /// Nominal refresh in millihertz; 0 = unknown.
    refresh_mhz: AtomicU32,
    link: Arc<ShellLink>,
    /// The shell's per-tick work, on the tick source's thread.
    tick_hook: OnceLock<Box<dyn Fn() + Send + Sync>>,
}

impl FrameClock {
    pub(crate) fn new(link: Arc<ShellLink>) -> Self {
        Self {
            sample: Mutex::new((0, 0)),
            tick_armed: AtomicBool::new(false),
            target_msc: AtomicU64::new(NO_TARGET),
            refresh_mhz: AtomicU32::new(0),
            link,
            tick_hook: OnceLock::new(),
        }
    }

    /// One refresh at `ust_us`; returns whether it woke the core loop.
    pub(crate) fn tick(&self, ust_us: u64) -> bool {
        let msc = {
            let mut sample = self
                .sample
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            sample.0 = sample.0.wrapping_add(1);
            sample.1 = ust_us;
            sample.0
        };
        let mut wake = self.tick_armed.swap(false, Ordering::AcqRel);
        let target = self.target_msc.load(Ordering::Acquire);
        if target != NO_TARGET
            && msc >= target
            && self
                .target_msc
                .compare_exchange(target, NO_TARGET, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            wake = true;
        }
        if wake {
            self.link.wake();
        }
        if let Some(hook) = self.tick_hook.get() {
            hook();
        }
        wake
    }

    /// Run `hook` on every tick; only the first hook set is kept.
    #[cfg_attr(all(kms, not(test)), allow(dead_code))] // A window shell's.
    pub(crate) fn set_tick_hook(&self, hook: impl Fn() + Send + Sync + 'static) {
        let _ = self.tick_hook.set(Box::new(hook));
    }

    /// `(msc, ust_us)` of the latest tick.
    pub(crate) fn sample(&self) -> (u64, u64) {
        *self
            .sample
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Wake the core loop on the next tick.
    pub(crate) fn arm_tick(&self) {
        self.tick_armed.store(true, Ordering::Release);
    }

    /// Wake the core loop once the MSC reaches `target`; returns whether
    /// this moved the armed target.
    pub(crate) fn arm_msc(&self, target: u64) -> bool {
        self.target_msc.fetch_min(target, Ordering::AcqRel) > target
    }

    /// The tick source's nominal period, in millihertz.
    pub(crate) fn set_refresh_mhz(&self, mhz: u32) {
        self.refresh_mhz.store(mhz, Ordering::Release);
    }

    /// The nominal refresh rate in whole hertz.
    pub(crate) fn refresh_hz(&self) -> u32 {
        match self.refresh_mhz.load(Ordering::Acquire) {
            0 => FALLBACK_REFRESH_HZ,
            mhz => (mhz + 500) / 1000,
        }
    }
}

/// A tick source for surfaces without a display link: a thread ticking
/// `clock` at the fallback rate until dropped.
pub(crate) struct SoftwareTicker {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl SoftwareTicker {
    /// # Errors
    ///
    /// The thread could not be spawned.
    pub(crate) fn start(clock: Arc<FrameClock>) -> std::io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let period = Duration::from_micros(1_000_000 / u64::from(FALLBACK_REFRESH_HZ));
        clock.set_refresh_mhz(FALLBACK_REFRESH_HZ * 1000);
        let origin = Instant::now();
        let thread = {
            let stop = Arc::clone(&stop);
            thread::Builder::new()
                .name("yserver-frame-clock".into())
                .spawn(move || {
                    while !stop.load(Ordering::Acquire) {
                        thread::sleep(period);
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

impl Drop for SoftwareTicker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use yserver_core::core_loop::{self, Message};

    use super::{FALLBACK_REFRESH_HZ, FrameClock};
    use crate::surface::link::ShellLink;

    fn clock() -> (FrameClock, Arc<ShellLink>, core_loop::CoreReceiver) {
        let link = Arc::new(ShellLink::default());
        let (_poll, sender, rx) = core_loop::channel().expect("channel");
        link.connect(sender);
        (FrameClock::new(Arc::clone(&link)), link, rx)
    }

    fn wakes(rx: &core_loop::CoreReceiver) -> usize {
        rx.try_recv_all()
            .filter(|m| matches!(m, Message::PlatformEvent))
            .count()
    }

    #[test]
    fn an_idle_clock_ticks_without_waking_the_core() {
        let (clock, _link, rx) = clock();
        for ust in 1..=10 {
            assert!(!clock.tick(ust * 16_666));
        }
        assert_eq!(wakes(&rx), 0);
        assert_eq!(clock.sample(), (10, 166_660));
    }

    #[test]
    fn msc_is_monotonic_and_carries_the_tick_time() {
        let (clock, _link, _rx) = clock();
        let mut last = clock.sample();
        for ust in [100, 200, 350] {
            clock.tick(ust);
            let now = clock.sample();
            assert!(now.0 > last.0, "{now:?} after {last:?}");
            assert_eq!(now.1, ust);
            last = now;
        }
    }

    #[test]
    fn an_armed_tick_wakes_once() {
        let (clock, link, rx) = clock();
        clock.arm_tick();
        assert!(clock.tick(1));
        link.drain();
        assert!(!clock.tick(2), "the arm is consumed");
        assert_eq!(wakes(&rx), 1);
    }

    #[test]
    fn an_armed_target_wakes_once_when_reached() {
        let (clock, link, rx) = clock();
        assert!(clock.arm_msc(3));
        assert!(!clock.arm_msc(5), "a later target does not move the arm");
        assert!(!clock.tick(1));
        assert!(!clock.tick(2));
        assert!(clock.tick(3));
        link.drain();
        assert!(!clock.tick(4));
        assert_eq!(wakes(&rx), 1);
    }

    #[test]
    fn the_software_ticker_ticks_until_dropped() {
        let (clock, _link, _rx) = clock();
        let clock = Arc::new(clock);
        let ticker = super::SoftwareTicker::start(Arc::clone(&clock)).expect("ticker");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while clock.sample().0 < 3 {
            assert!(std::time::Instant::now() < deadline, "no ticks");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(clock.refresh_hz(), FALLBACK_REFRESH_HZ);
        drop(ticker);
        let stopped = clock.sample().0;
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(clock.sample().0, stopped, "dropping the ticker stops it");
    }

    #[test]
    fn the_tick_hook_runs_on_every_tick() {
        let (clock, _link, rx) = clock();
        let ticks = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let counted = Arc::clone(&ticks);
        clock.set_tick_hook(move || {
            counted.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        });
        clock.tick(1);
        clock.tick(2);
        assert_eq!(ticks.load(std::sync::atomic::Ordering::Relaxed), 2);
        assert_eq!(wakes(&rx), 0, "the hook does not wake the core");
    }

    #[test]
    fn refresh_falls_back_until_the_source_reports() {
        let (clock, _link, _rx) = clock();
        assert_eq!(clock.refresh_hz(), FALLBACK_REFRESH_HZ);
        clock.set_refresh_mhz(119_880);
        assert_eq!(clock.refresh_hz(), 120);
    }
}
