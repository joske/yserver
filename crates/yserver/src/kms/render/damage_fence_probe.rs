//! Diagnostic (#100): how many DamageNotify events reach a compositor's
//! socket while the damaged exported backing still has GPU writes whose
//! WRITE fence is not yet on the dma-buf.
//!
//! Implicit sync only orders a reader after fences already attached when
//! the reader submits. Per exported drawable this tracks the first write of
//! the batch still waiting for a submit-group flush, plus the last flushed
//! batch, so a probe stamped at socket-write time `t` is classified exactly:
//! before the fence iff a batch with a write at or before `t` was published
//! after `t` (or is still unpublished).

use std::{
    collections::HashMap,
    hash::Hash,
    time::{Duration, Instant},
};

/// Gap (socket write → fence publish) histogram bounds, in µs.
const GAP_BUCKETS_US: [u64; 4] = [100, 1_000, 4_000, 16_000];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DamageNotifyClass {
    /// The damaged drawable has no live dma-buf export.
    NotExported,
    /// Exported, but the event bytes stayed in the outbound queue.
    Buffered,
    /// Exported; every write before the socket write was already published.
    AfterFence,
    /// Exported; a write recorded before the socket write was not yet
    /// published.
    BeforeFence,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProbeWindow {
    pub(crate) total: u64,
    pub(crate) exported: u64,
    pub(crate) buffered: u64,
    pub(crate) before_fence: u64,
    pub(crate) gap_count: u64,
    pub(crate) gap_sum_us: u64,
    pub(crate) gap_max_us: u64,
    /// `<100µs`, `<1ms`, `<4ms`, `<16ms`, `>=16ms`.
    pub(crate) gap_hist: [u64; 5],
}

impl ProbeWindow {
    fn record_gap(&mut self, gap: Duration) {
        let us = u64::try_from(gap.as_micros()).unwrap_or(u64::MAX);
        self.gap_count += 1;
        self.gap_sum_us = self.gap_sum_us.saturating_add(us);
        self.gap_max_us = self.gap_max_us.max(us);
        let bucket = GAP_BUCKETS_US
            .iter()
            .position(|&bound| us < bound)
            .unwrap_or(GAP_BUCKETS_US.len());
        self.gap_hist[bucket] += 1;
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct ExportBatches {
    /// First write of the batch not yet flushed.
    open_first_write: Option<Instant>,
    /// (first write, publish time) of the last flushed batch.
    last_published: Option<(Instant, Instant)>,
}

#[derive(Debug)]
pub(crate) struct DamageFenceProbe<K> {
    batches: HashMap<K, ExportBatches>,
    /// Before-fence probes whose batch is still unpublished.
    waiters: Vec<(K, Instant)>,
    window: ProbeWindow,
    window_start: Instant,
}

impl<K: Copy + Eq + Hash> DamageFenceProbe<K> {
    pub(crate) fn new() -> Self {
        Self {
            batches: HashMap::new(),
            waiters: Vec::new(),
            window: ProbeWindow::default(),
            window_start: Instant::now(),
        }
    }

    /// A GPU write to exported `id` was recorded.
    pub(crate) fn note_export_write(&mut self, id: K) {
        self.note_export_write_with(id, Instant::now);
    }

    fn note_export_write_with(&mut self, id: K, now: impl FnOnce() -> Instant) {
        let batch = self.batches.entry(id).or_default();
        if batch.open_first_write.is_none() {
            batch.open_first_write = Some(now());
        }
    }

    /// The submit-group flush carrying `id`'s writes publishes its fence.
    pub(crate) fn note_published(&mut self, id: K, at: Instant) {
        if let Some(batch) = self.batches.get_mut(&id)
            && let Some(first) = batch.open_first_write.take()
        {
            batch.last_published = Some((first, at));
        }
        let window = &mut self.window;
        self.waiters.retain(|&(waiter, sent)| {
            if waiter != id {
                return true;
            }
            window.record_gap(at.saturating_duration_since(sent));
            false
        });
    }

    /// `id`'s export was torn down.
    pub(crate) fn forget(&mut self, id: K) {
        self.batches.remove(&id);
        self.waiters.retain(|&(waiter, _)| waiter != id);
    }

    /// Classify one DamageNotify. `exported` is the damaged drawable's
    /// backing when it is live-exported; `on_wire_at` is when the event's
    /// bytes reached the socket (`None`: queued in the outbound buffer).
    pub(crate) fn classify(
        &mut self,
        exported: Option<K>,
        on_wire_at: Option<Instant>,
    ) -> DamageNotifyClass {
        self.window.total += 1;
        let Some(id) = exported else {
            return DamageNotifyClass::NotExported;
        };
        self.window.exported += 1;
        let Some(sent) = on_wire_at else {
            self.window.buffered += 1;
            return DamageNotifyClass::Buffered;
        };
        let batch = self.batches.get(&id).copied().unwrap_or_default();
        if batch.open_first_write.is_some_and(|first| first <= sent) {
            self.window.before_fence += 1;
            self.waiters.push((id, sent));
            return DamageNotifyClass::BeforeFence;
        }
        if let Some((first, published)) = batch.last_published
            && first <= sent
            && sent < published
        {
            self.window.before_fence += 1;
            self.window.record_gap(published.duration_since(sent));
            return DamageNotifyClass::BeforeFence;
        }
        DamageNotifyClass::AfterFence
    }

    /// The counters of a window at least a second old, resetting them.
    pub(crate) fn take_window_if_due(&mut self, now: Instant) -> Option<ProbeWindow> {
        if now.saturating_duration_since(self.window_start) < Duration::from_secs(1) {
            return None;
        }
        self.window_start = now;
        Some(std::mem::take(&mut self.window))
    }

    /// Emit the `damage_fence_probe:` line once a second while DamageNotify
    /// traffic flows.
    pub(crate) fn maybe_log(&mut self, now: Instant) {
        let Some(w) = self.take_window_if_due(now) else {
            return;
        };
        if w.total == 0 && w.gap_count == 0 {
            return;
        }
        let gap_avg_us = w.gap_sum_us / w.gap_count.max(1);
        log::info!(
            "damage_fence_probe: damage_notify_total/s={} damage_notify_exported/s={} \
             damage_notify_before_fence/s={} damage_notify_buffered/s={} \
             gap_us[n={} avg={gap_avg_us} max={}] \
             gap_hist_us[<100={} <1000={} <4000={} <16000={} >=16000={}]",
            w.total,
            w.exported,
            w.before_fence,
            w.buffered,
            w.gap_count,
            w.gap_max_us,
            w.gap_hist[0],
            w.gap_hist[1],
            w.gap_hist[2],
            w.gap_hist[3],
            w.gap_hist[4],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe() -> DamageFenceProbe<u64> {
        DamageFenceProbe::new()
    }

    fn at(base: Instant, us: u64) -> Instant {
        base + Duration::from_micros(us)
    }

    #[test]
    fn unexported_drawable_is_counted_but_not_classified_exported() {
        let mut p = probe();
        assert_eq!(
            p.classify(None, Some(Instant::now())),
            DamageNotifyClass::NotExported
        );
        assert_eq!(p.window.total, 1);
        assert_eq!(p.window.exported, 0);
    }

    #[test]
    fn exported_write_pending_at_socket_write_is_before_fence() {
        let mut p = probe();
        let t0 = Instant::now();
        p.note_export_write_with(7, || t0);
        let sent = at(t0, 10);
        assert_eq!(
            p.classify(Some(7), Some(sent)),
            DamageNotifyClass::BeforeFence
        );
        let published = at(sent, 250);
        p.note_published(7, published);
        assert_eq!(p.window.before_fence, 1);
        assert_eq!(p.window.gap_count, 1);
        assert_eq!(p.window.gap_max_us, 250);
        assert_eq!(p.window.gap_hist, [0, 1, 0, 0, 0]);
        assert!(p.waiters.is_empty());
    }

    #[test]
    fn exported_write_published_before_socket_write_is_after_fence() {
        let mut p = probe();
        let t0 = Instant::now();
        p.note_export_write_with(7, || t0);
        p.note_published(7, at(t0, 5));
        let sent = at(t0, 6);
        assert_eq!(
            p.classify(Some(7), Some(sent)),
            DamageNotifyClass::AfterFence
        );
        assert_eq!(p.window.exported, 1);
        assert_eq!(p.window.before_fence, 0);
    }

    #[test]
    fn publish_between_socket_write_and_classification_is_before_fence() {
        let mut p = probe();
        let t0 = Instant::now();
        p.note_export_write_with(7, || t0);
        let sent = at(t0, 10);
        p.note_published(7, at(sent, 5_000));
        assert_eq!(
            p.classify(Some(7), Some(sent)),
            DamageNotifyClass::BeforeFence
        );
        assert_eq!(p.window.gap_hist, [0, 0, 0, 1, 0]);
    }

    #[test]
    fn write_recorded_after_socket_write_is_not_before_fence() {
        let mut p = probe();
        let sent = Instant::now();
        p.note_export_write_with(7, || at(sent, 1));
        assert_eq!(
            p.classify(Some(7), Some(sent)),
            DamageNotifyClass::AfterFence
        );
    }

    #[test]
    fn event_left_in_outbound_buffer_is_buffered() {
        let mut p = probe();
        p.note_export_write(7);
        assert_eq!(p.classify(Some(7), None), DamageNotifyClass::Buffered);
        assert_eq!(p.window.buffered, 1);
        assert_eq!(p.window.before_fence, 0);
    }

    #[test]
    fn forgetting_an_export_drops_its_waiters() {
        let mut p = probe();
        p.note_export_write(7);
        p.classify(Some(7), Some(Instant::now()));
        p.forget(7);
        p.note_published(7, Instant::now());
        assert_eq!(p.window.gap_count, 0);
    }
}
