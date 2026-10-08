//! Per-`vkQueueSubmit2` cost and cause counters (#214).
//!
//! `call_stats::queue_submit2` counts calls; this module also attributes
//! each real call to the reason it was made and measures the submitting
//! thread's CPU time inside the call (`CLOCK_THREAD_CPUTIME_ID`, which
//! includes the kernel time of the execbuf ioctl). On anv/i915 every
//! execbuf carries every live `VkDeviceMemory`, so the `vk submit cost`
//! line samples the live allocation count and the pixmap pool's idle
//! entries beside the per-submit cost.
//!
//! Always on: two `clock_gettime` calls and a few relaxed atomics per
//! submit. Only the per-second emit is gated on `YSERVER_LOOP_TELEMETRY`.

use std::sync::atomic::{AtomicU64, Ordering};

/// Why a `vkQueueSubmit2` was made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitCause {
    /// Frame close before a scene compose (`CloseReason::LegacyScCompose`).
    FrameLegacySc,
    /// Frame close before a root read (`CloseReason::LegacyRootRead`).
    FrameRootRead,
    /// Frame close isolating a composite that samples a redirect backing.
    FrameRedirectIsolation,
    /// Frame close at the DamageNotify / Subtract / FetchRegion boundary.
    FrameDamageBoundary,
    /// Frame close carrying a Present completion signal.
    FramePresent,
    /// Frame close before a CPU wait.
    FrameSyncWait,
    /// Frame close before a non-ported paint op.
    FrameNonPorted,
    /// Frame close by the open-frame timeout.
    FrameTimeout,
    /// Any other frame close (shutdown, pin ceiling, scratch grow, glyph atlas full).
    FrameOther,
    /// Submit-group flush not driven by a frame close, by `FlushReason`.
    GroupSync,
    GroupCompose,
    GroupPresent,
    GroupOther,
    /// Scene compose, one per output.
    Compose,
    /// CB-less Present completion signal.
    PresentSignal,
    /// Synchronous one-shot submit.
    OneShot,
    /// Direct-scanout copies and probes.
    Scanout,
    Other,
}

impl SubmitCause {
    pub const ALL: [Self; 18] = [
        Self::FrameLegacySc,
        Self::FrameRootRead,
        Self::FrameRedirectIsolation,
        Self::FrameDamageBoundary,
        Self::FramePresent,
        Self::FrameSyncWait,
        Self::FrameNonPorted,
        Self::FrameTimeout,
        Self::FrameOther,
        Self::GroupSync,
        Self::GroupCompose,
        Self::GroupPresent,
        Self::GroupOther,
        Self::Compose,
        Self::PresentSignal,
        Self::OneShot,
        Self::Scanout,
        Self::Other,
    ];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::FrameLegacySc => "legacy_sc",
            Self::FrameRootRead => "root_read",
            Self::FrameRedirectIsolation => "redirect_iso",
            Self::FrameDamageBoundary => "damage_boundary",
            Self::FramePresent => "present_frame",
            Self::FrameSyncWait => "sync_wait",
            Self::FrameNonPorted => "non_ported",
            Self::FrameTimeout => "timeout",
            Self::FrameOther => "frame_other",
            Self::GroupSync => "group_sync",
            Self::GroupCompose => "group_compose",
            Self::GroupPresent => "group_present",
            Self::GroupOther => "group_other",
            Self::Compose => "compose",
            Self::PresentSignal => "present_signal",
            Self::OneShot => "one_shot",
            Self::Scanout => "scanout",
            Self::Other => "other",
        }
    }

    const fn index(self) -> usize {
        self as usize
    }
}

const N: usize = SubmitCause::ALL.len();

/// Cumulative counters; the emitter swaps them to zero once a second.
pub struct SubmitStats {
    calls: [AtomicU64; N],
    cpu_ns: [AtomicU64; N],
    cpu_ns_max: AtomicU64,
    cbs: AtomicU64,
    cbs_max: AtomicU64,
    cbless: AtomicU64,
    exported: AtomicU64,
    damage_boundary_exported: AtomicU64,
    legacy_sc_composed: AtomicU64,
    legacy_sc_nocompose: AtomicU64,
}

pub static SUBMITS: SubmitStats = SubmitStats {
    calls: [const { AtomicU64::new(0) }; N],
    cpu_ns: [const { AtomicU64::new(0) }; N],
    cpu_ns_max: AtomicU64::new(0),
    cbs: AtomicU64::new(0),
    cbs_max: AtomicU64::new(0),
    cbless: AtomicU64::new(0),
    exported: AtomicU64::new(0),
    damage_boundary_exported: AtomicU64::new(0),
    legacy_sc_composed: AtomicU64::new(0),
    legacy_sc_nocompose: AtomicU64::new(0),
};

/// One second's worth of [`SubmitStats`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SubmitStatsSnapshot {
    pub calls: [u64; N],
    pub cpu_ns: [u64; N],
    pub cpu_ns_max: u64,
    pub cbs: u64,
    pub cbs_max: u64,
    pub cbless: u64,
    pub exported: u64,
    pub damage_boundary_exported: u64,
    pub legacy_sc_composed: u64,
    pub legacy_sc_nocompose: u64,
}

impl SubmitStatsSnapshot {
    #[must_use]
    pub fn total_calls(&self) -> u64 {
        self.calls.iter().sum()
    }

    #[must_use]
    pub fn total_cpu_ns(&self) -> u64 {
        self.cpu_ns.iter().sum()
    }
}

impl SubmitStats {
    /// Count one real `vkQueueSubmit2` of `cbs` command buffers that took
    /// `cpu_ns` of thread CPU. `exported` = it published dma-buf write fences.
    pub fn record(&self, cause: SubmitCause, cbs: usize, cpu_ns: u64, exported: bool) {
        let i = cause.index();
        self.calls[i].fetch_add(1, Ordering::Relaxed);
        self.cpu_ns[i].fetch_add(cpu_ns, Ordering::Relaxed);
        self.cpu_ns_max.fetch_max(cpu_ns, Ordering::Relaxed);
        let cbs = cbs as u64;
        self.cbs.fetch_add(cbs, Ordering::Relaxed);
        self.cbs_max.fetch_max(cbs, Ordering::Relaxed);
        if cbs == 0 {
            self.cbless.fetch_add(1, Ordering::Relaxed);
        }
        if exported {
            self.exported.fetch_add(1, Ordering::Relaxed);
            if cause == SubmitCause::FrameDamageBoundary {
                self.damage_boundary_exported
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// A `LegacyScCompose` close submitted on a `maybe_composite` tick;
    /// `composed` = the tick then composed at least one output.
    pub fn record_legacy_sc_tick(&self, composed: bool) {
        if composed {
            self.legacy_sc_composed.fetch_add(1, Ordering::Relaxed);
        } else {
            self.legacy_sc_nocompose.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[must_use]
    pub fn snapshot_and_reset(&self) -> SubmitStatsSnapshot {
        let swap = |a: &AtomicU64| a.swap(0, Ordering::Relaxed);
        SubmitStatsSnapshot {
            calls: std::array::from_fn(|i| swap(&self.calls[i])),
            cpu_ns: std::array::from_fn(|i| swap(&self.cpu_ns[i])),
            cpu_ns_max: swap(&self.cpu_ns_max),
            cbs: swap(&self.cbs),
            cbs_max: swap(&self.cbs_max),
            cbless: swap(&self.cbless),
            exported: swap(&self.exported),
            damage_boundary_exported: swap(&self.damage_boundary_exported),
            legacy_sc_composed: swap(&self.legacy_sc_composed),
            legacy_sc_nocompose: swap(&self.legacy_sc_nocompose),
        }
    }
}

/// CPU time consumed by the calling thread, in nanoseconds; 0 if the
/// clock is unavailable.
#[must_use]
pub fn thread_cpu_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid out-pointer for the duration of the call.
    if unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &raw mut ts) } != 0 {
        return 0;
    }
    u64::try_from(ts.tv_sec)
        .unwrap_or(0)
        .saturating_mul(1_000_000_000)
        .saturating_add(u64::try_from(ts.tv_nsec).unwrap_or(0))
}

/// Run `submit` (one `vkQueueSubmit2` of `cbs` command buffers) and count
/// it under `cause`, with the thread CPU it took. Counts failed calls too:
/// they cost the same ioctl.
pub fn timed<T>(cause: SubmitCause, cbs: usize, exported: bool, submit: impl FnOnce() -> T) -> T {
    let start = thread_cpu_ns();
    let out = submit();
    let cpu = thread_cpu_ns().saturating_sub(start);
    SUBMITS.record(cause, cbs, cpu, exported);
    out
}

/// The `vk submit cost [1s]` line: real submits, thread CPU spent inside
/// them, command buffers per submit, and the live `VkDeviceMemory` count
/// (`live_allocs`) and idle pixmap-pool entries (`pool_idle`) sampled at
/// the same moment, then the per-cause `count/cpu` split.
#[must_use]
pub fn format_line(s: &SubmitStatsSnapshot, live_allocs: u64, pool_idle: u64) -> String {
    use std::fmt::Write as _;
    let n = s.total_calls();
    let cpu = s.total_cpu_ns();
    let us = |ns: u64| ns as f64 / 1000.0;
    let mut out = format!(
        "vk submit cost [1s]: submits={n} cbless={} cbs_avg={:.2} cbs_max={} \
         cpu_total={:.2}ms cpu_avg={:.1}us cpu_max={:.1}us live_allocs={live_allocs} \
         pool_idle={pool_idle} exported={} |",
        s.cbless,
        s.cbs as f64 / n.max(1) as f64,
        s.cbs_max,
        cpu as f64 / 1e6,
        us(cpu) / n.max(1) as f64,
        us(s.cpu_ns_max),
        s.exported,
    );
    for c in SubmitCause::ALL {
        let i = c.index();
        let _ = write!(
            out,
            " {}={}/{:.2}ms",
            c.label(),
            s.calls[i],
            s.cpu_ns[i] as f64 / 1e6
        );
    }
    let _ = write!(
        out,
        " | legacy_sc_tick composed={} nocompose={} | damage_boundary_exported={}",
        s.legacy_sc_composed, s.legacy_sc_nocompose, s.damage_boundary_exported
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_are_unique_and_indices_match_all_order() {
        let mut seen = std::collections::HashSet::new();
        for (i, c) in SubmitCause::ALL.iter().enumerate() {
            assert_eq!(c.index(), i);
            assert!(seen.insert(c.label()), "duplicate label {}", c.label());
        }
    }

    #[test]
    fn record_accumulates_and_snapshot_resets() {
        let s = SubmitStats {
            calls: [const { AtomicU64::new(0) }; N],
            cpu_ns: [const { AtomicU64::new(0) }; N],
            cpu_ns_max: AtomicU64::new(0),
            cbs: AtomicU64::new(0),
            cbs_max: AtomicU64::new(0),
            cbless: AtomicU64::new(0),
            exported: AtomicU64::new(0),
            damage_boundary_exported: AtomicU64::new(0),
            legacy_sc_composed: AtomicU64::new(0),
            legacy_sc_nocompose: AtomicU64::new(0),
        };
        s.record(SubmitCause::FrameLegacySc, 1, 3_000, false);
        s.record(SubmitCause::FrameLegacySc, 2, 5_000, false);
        s.record(SubmitCause::PresentSignal, 0, 1_000, false);
        s.record(SubmitCause::FrameDamageBoundary, 1, 2_000, true);
        s.record(SubmitCause::FrameRedirectIsolation, 1, 2_000, true);
        s.record_legacy_sc_tick(true);
        s.record_legacy_sc_tick(false);
        s.record_legacy_sc_tick(false);
        let snap = s.snapshot_and_reset();
        assert_eq!(snap.total_calls(), 5);
        assert_eq!(snap.calls[SubmitCause::FrameLegacySc.index()], 2);
        assert_eq!(snap.cpu_ns[SubmitCause::FrameLegacySc.index()], 8_000);
        assert_eq!(snap.total_cpu_ns(), 13_000);
        assert_eq!(snap.cpu_ns_max, 5_000);
        assert_eq!(snap.cbs, 5);
        assert_eq!(snap.cbs_max, 2);
        assert_eq!(snap.cbless, 1);
        assert_eq!(snap.exported, 2);
        assert_eq!(snap.damage_boundary_exported, 1);
        assert_eq!((snap.legacy_sc_composed, snap.legacy_sc_nocompose), (1, 2));
        assert_eq!(s.snapshot_and_reset(), SubmitStatsSnapshot::default());
    }

    #[test]
    fn format_line_reports_averages_and_causes() {
        let mut snap = SubmitStatsSnapshot::default();
        snap.calls[SubmitCause::FrameLegacySc.index()] = 3;
        snap.cpu_ns[SubmitCause::FrameLegacySc.index()] = 3_000_000;
        snap.calls[SubmitCause::Compose.index()] = 1;
        snap.cpu_ns[SubmitCause::Compose.index()] = 1_000_000;
        snap.cpu_ns_max = 1_500_000;
        snap.cbs = 4;
        snap.cbs_max = 1;
        snap.legacy_sc_nocompose = 2;
        let line = format_line(&snap, 512, 339);
        assert!(
            line.starts_with(
                "vk submit cost [1s]: submits=4 cbless=0 cbs_avg=1.00 cbs_max=1 \
                 cpu_total=4.00ms cpu_avg=1000.0us cpu_max=1500.0us live_allocs=512 \
                 pool_idle=339 exported=0 | legacy_sc=3/3.00ms "
            ),
            "{line}"
        );
        assert!(line.contains(" compose=1/1.00ms "), "{line}");
        assert!(
            line.ends_with("| legacy_sc_tick composed=0 nocompose=2 | damage_boundary_exported=0"),
            "{line}"
        );
    }

    #[test]
    fn thread_cpu_clock_advances_under_work() {
        let a = thread_cpu_ns();
        let mut x = 0u64;
        for i in 0..2_000_000u64 {
            x = std::hint::black_box(x.wrapping_add(i));
        }
        std::hint::black_box(x);
        assert!(thread_cpu_ns() > a);
    }
}
