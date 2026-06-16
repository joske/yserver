//! Per-second telemetry counters for v2 (Stage 2f).
//!
//! Per rendering-model-v2 spec §"Required counters / log lines"
//! and Stage 2 plan §"Acceptance discipline". The Stage-3+ perf
//! gates (no `vkQueueWaitIdle` on hot path; queue_submit2 rate
//! ≤ v1 baseline; damage_fraction noticeably <1.0 on window-drag;
//! sustained `full_redraw_fallback == 0`) are only enforceable
//! if v2 emits the named counters that let us judge them.
//!
//! Emission shape: per-second summary line under
//! `YSERVER_LOOP_TELEMETRY=1`, parsable by grep+awk. Each counter
//! is a simple monotonic accumulator; the per-second emitter
//! resets per-second-window state on each emit. Counter sites are
//! the [`Telemetry::record_*`] methods called by the engine,
//! scene, and platform layers.

#![allow(
    dead_code,
    reason = "Counter accessors are consumed by Stage 3+ perf gates + harness"
)]

use std::time::Instant;

use crate::kms::v2::submit_trace::{SubmitEvent, SubmitTrace};

/// Single-second accumulator. Reset on every emission tick.
#[derive(Debug, Default, Clone, Copy)]
pub struct Bucket {
    pub paint_submits: u64,
    pub composite_submits: u64,
    pub one_shot_submits: u64,
    pub queue_submit2: u64,
    pub vk_queue_wait_idle: u64,
    pub cpu_fence_wait_ns: u64,
    pub cpu_fence_wait_count: u64,
    pub damaged_pixels: u64,
    pub output_pixels: u64,
    pub scene_entries_visited: u64,
    pub scene_entries_drawn: u64,
    pub full_redraw_fallback: u64,
    pub storage_allocations: u64,
    pub descriptor_allocations: u64,
    pub image_view_creates: u64,
    pub frame_present_count: u64,
    pub missed_pageflips: u64,
    pub gpu_render_ns: u64,
    pub compose_cb_record_ns: u64,
    pub frames_with_compose: u64,
    // ── Stage 3a glyph/text counters ─────────────────────────
    /// Glyphs successfully interned into the atlas during the
    /// window. One `intern` call that returns `Some(entry)`
    /// without a cache hit increments this.
    pub atlas_intern: u64,
    /// Glyph upload CBs submitted to the graphics queue.
    /// Mirrors `atlas_intern` for the miss path; cache-hit
    /// interns don't touch this. Useful for distinguishing
    /// atlas-cache-hit vs miss rate.
    pub glyph_uploads: u64,
    /// Glyphs the run dropped because the atlas was full. Per
    /// Stage 3 plan § 3a: this is a **lifetime** counter only on
    /// the `Telemetry.lifetime` view; the bucket field exists for
    /// the maybe_emit log line but typically reads zero — the
    /// load-bearing fact is whether the value ever became
    /// non-zero, which the lifetime view captures.
    pub glyphs_dropped_atlas_full: u64,
    // ── Stage 3d RENDER glyph counters (wire sites land in 3d;
    //    Stage 3a only adds the storage). ────────────────────
    pub composite_glyphs_dropped_unsupported: u64,
    pub disjoint_readback_count: u64,
    /// Stage 5 Task 4 layer 1: vkCreateDescriptorPool calls in this
    /// second. Should reach a near-zero floor after warm-up under
    /// the descriptor-pool-ring design (spec 2026-05-21).
    pub descriptor_pool_creates: u64,
    /// Stage 5 Task 4 layer 1: vkResetDescriptorPool calls in this
    /// second. Tracks paint_submits/s / SETS_PER_POOL on a healthy
    /// recycle path.
    pub descriptor_pool_resets: u64,
    /// Stage 5 Task 3 (render-composite generalization): count of
    /// RENDER batches flushed (each maps to one `vkQueueSubmit2`).
    /// Per-second + lifetime.
    pub render_batches_flushed: u64,
    /// Stage 5 Task 3 (render-composite generalization): count of
    /// individual `render_composite` calls folded into batches.
    /// Pre-fix each call would have produced its own `paint_submit`.
    pub render_composites_coalesced: u64,
    // ── Phase B.1 Task 21: frame builder counters ───────────────
    /// Number of frame opens in this window.
    pub(crate) frame_builder_opens: u64,
    /// Number of frame closes in this window.
    pub(crate) frame_builder_closes: u64,
    /// Per-close-reason counters (matches `CloseReason` variants).
    pub(crate) frame_builder_close_reason_scene_compose: u64,
    pub(crate) frame_builder_close_reason_non_ported_paint_op: u64,
    pub(crate) frame_builder_close_reason_legacy_sc_compose: u64,
    pub(crate) frame_builder_close_reason_present_completion_signal: u64,
    pub(crate) frame_builder_close_reason_sync_wait: u64,
    pub(crate) frame_builder_close_reason_timeout: u64,
    pub(crate) frame_builder_close_reason_shutdown: u64,
    pub(crate) frame_builder_close_reason_pin_ceiling: u64,
    /// Phase B.2 Mechanism 3: bumped when a scratch grow forces a
    /// close-reopen so the just-submitted frame's recorded views
    /// target the same scratch instance the close-time emit will
    /// sample. Expected to stay low (most paints don't grow); a
    /// rising rate indicates oversized scratches or workload churn.
    pub(crate) frame_builder_close_reason_scratch_grow: u64,
    /// Sum of `ops_in_frame` across all closes in the window.
    pub(crate) frame_builder_ops_per_frame_total: u64,
    /// Max `ops_in_frame` seen in the current bucket window.
    pub(crate) frame_builder_ops_per_frame_max_in_window: u64,
    /// Ops-per-frame histogram. Buckets: [0..=1, 2..=3, 4..=7, 8..=15,
    /// 16..=31, 32..=63, 64..=127, 128+].
    pub(crate) frame_builder_ops_per_frame_hist: [u64; 8],
    /// High-water mark of `pin_count` seen across all closes.
    pub(crate) frame_builder_active_pins_high_water: u64,
    /// Number of frame closes that followed a Vk error (abort path).
    pub(crate) frame_builder_aborts: u64,
    /// Sum of `glyph_uploads_in_frame` across all closes in the window.
    pub(crate) frame_builder_glyph_uploads_per_frame_total: u64,
    /// Max `glyph_uploads_in_frame` seen in the current bucket window.
    pub(crate) frame_builder_glyph_uploads_per_frame_max_in_window: u64,
    /// Phase B.2 Task 14: sum of `renders_in_frame` (count of
    /// `RecordedOp::RenderComposite` ops) across all closes in the
    /// window. Mirrors the `glyph_uploads_per_frame_total` shape;
    /// divide by `frame_builder_closes` for the per-frame average
    /// rendered in the `v2_telemetry:` log line.
    pub(crate) frame_builder_renders_per_frame_total: u64,
    /// Phase B.2 Task 14: max `renders_in_frame` seen in the current
    /// bucket window. Mirrors `glyph_uploads_per_frame_max_in_window`.
    pub(crate) frame_builder_renders_per_frame_max_in_window: u64,
    // ── Phase A: SubmitGroup size + flush-reason histogram ───────
    /// Number of SubmitGroup flushes that actually submitted at
    /// least one entry (non-abort). Per-second + lifetime.
    pub(crate) submit_group_flushes: u64,
    /// Number of SubmitGroup flushes that aborted (Vk error path).
    pub(crate) submit_group_aborts: u64,
    /// Maximum flushed-entry count observed in the current bucket
    /// window. Reset each second.
    pub(crate) submit_group_size_max_in_window: u64,
    /// Sum of flushed-entry counts across all flushes in the window.
    /// Divide by `submit_group_flushes` for average group size.
    pub(crate) submit_group_size_total: u64,
    /// Histogram of flushed group sizes. Buckets: [1, 2, 4, 8, 12, 16+].
    pub(crate) submit_group_hist: [u64; 6],
    // Per-reason flush counters.
    pub(crate) submit_group_flush_reason_sync_boundary: u64,
    pub(crate) submit_group_flush_reason_present_completion_signal: u64,
    pub(crate) submit_group_flush_reason_scene_compose: u64,
    pub(crate) submit_group_flush_reason_pageflip_retire: u64,
    pub(crate) submit_group_flush_reason_max_size: u64,
    pub(crate) submit_group_flush_reason_shutdown: u64,
    pub(crate) submit_group_flush_reason_frame_builder: u64,
    // ── Phase A: retention high-water gauges ─────────────────────
    /// High-water mark of descriptor pool ring pool count sampled
    /// each tick.
    pub(crate) active_descriptor_pool_count_high_water: u64,
    /// High-water mark of active staging buffer bytes across
    /// submitted + parked ops, sampled each tick.
    pub(crate) active_staging_bytes_high_water: u64,
    /// High-water mark of active scratch image bytes across
    /// submitted + parked ops, sampled each tick.
    pub(crate) active_scratch_bytes_high_water: u64,
    /// Number of per-CRTC `cursor_plane.move_to` atomic commits
    /// that returned `EBUSY` (kernel rejected the cursor move
    /// because a primary-plane commit is pending on the same
    /// CRTC). Bumped once per per-CRTC EBUSY, so a single
    /// motion event on a dual-output system can contribute 2.
    /// Lifetime tells the cumulative count; the per-second
    /// bucket value (printed in the summary line) tells the
    /// current contention rate. Hypothesised to be the cause of
    /// HW-cursor jerkiness on AMD under heavy compositing.
    pub cursor_move_ebusy: u64,
}

/// v2 telemetry state. One per `KmsBackendV2`. Counter sites
/// call `record_*` directly; the emitter ticks once per second
/// from the core loop (driven through `maybe_emit`).
pub struct Telemetry {
    enabled: bool,
    last_emit: Instant,
    bucket: Bucket,
    /// Lifetime-aggregate counts (not reset per-emit). Useful
    /// for the acceptance harness which compares totals after
    /// driving a test sequence.
    pub lifetime: Bucket,
    /// Stage 5 Task 3 diagnostic: per-submit event log,
    /// enabled by `YSERVER_SUBMIT_TRACE=<path>`. `None` in the
    /// default off case (zero hot-path cost).
    submit_trace: Option<SubmitTrace>,
    /// Bumped at the top of `maybe_composite` (one tick = one
    /// frame_id). Every `record_submit_event` writes the
    /// current value, so paint events recorded between ticks
    /// share the surrounding tick's id.
    frame_id: u64,
}

impl Telemetry {
    /// Construct. Reads `YSERVER_LOOP_TELEMETRY` once at boot to
    /// decide whether the per-second emitter actually logs.
    /// Counter sites always update — the cost of an `+= 1` is
    /// trivial, and tests check `lifetime.*` regardless of the
    /// env var.
    #[must_use]
    pub(crate) fn new() -> Self {
        let enabled = matches!(
            std::env::var_os("YSERVER_LOOP_TELEMETRY")
                .as_deref()
                .and_then(|s| s.to_str()),
            Some("1" | "true" | "yes" | "on")
        );
        Self {
            enabled,
            last_emit: Instant::now(),
            bucket: Bucket::default(),
            lifetime: Bucket::default(),
            submit_trace: SubmitTrace::from_env(),
            frame_id: 0,
        }
    }

    /// Stage 5 Task 3 diagnostic: log one submit event to the
    /// trace file if `YSERVER_SUBMIT_TRACE` is set. No-op
    /// otherwise (and the wrapping `Option::None` lets the
    /// optimizer fold the call away on the default-off path).
    #[inline]
    pub(crate) fn record_submit_event(&mut self, mut event: SubmitEvent) {
        if let Some(trace) = self.submit_trace.as_mut() {
            event.frame_id = self.frame_id;
            trace.record(&event);
        }
    }

    /// Bumped once per main-loop tick (top of `maybe_composite`).
    /// All submit events recorded between calls share the
    /// current frame_id; the `scene_compose` event for tick N
    /// also carries id N.
    pub(crate) fn advance_frame(&mut self) {
        self.frame_id = self.frame_id.wrapping_add(1);
    }

    /// Current frame_id. Mainly exposed for tests + diagnostic
    /// log lines.
    #[must_use]
    pub(crate) fn frame_id(&self) -> u64 {
        self.frame_id
    }

    /// Tick the emitter; if ≥ 1s has elapsed, print the
    /// per-second summary and reset the bucket. Safe to call
    /// every core-loop iteration — no-op when below threshold.
    ///
    /// Independent of the `enabled` gate: when a [`SubmitTrace`] is
    /// active, the same 1Hz cadence also flushes its `BufWriter`.
    /// That bounds the data loss from hard kill (zap, kernel hang,
    /// hard reboot) to ≤ 1s and lets `tail -f` work during a live
    /// capture. The cost is one `write(2)` syscall per second when
    /// tracing — negligible.
    /// `submitted_depth` is the engine's `submitted`-queue depth at
    /// call time (an instantaneous gauge, not a per-second rate),
    /// passed in by every caller so it can't go stale or be forgotten:
    /// it's the reclamation-starvation leak gauge, which climbs without
    /// bound on a build that drains `submitted` only on page-flip once
    /// the display goes dark while clients keep drawing, and stays flat
    /// with the `before_block` reclaim drive
    /// (project_reclamation_starvation_leak).
    pub(crate) fn maybe_emit(&mut self, submitted_depth: usize) {
        let now = Instant::now();
        let dt = now.duration_since(self.last_emit);
        if dt < std::time::Duration::from_secs(1) {
            return;
        }
        // 1Hz periodic flush of the submit trace — runs whether or
        // not the per-second log line is enabled.
        if let Some(trace) = self.submit_trace.as_mut() {
            trace.flush();
        }
        if !self.enabled {
            // Advance the timer so the next flush also runs at 1Hz.
            // The per-second `bucket` keeps accumulating when
            // disabled — only the log emission + reset are gated.
            self.last_emit = now;
            return;
        }
        let b = self.bucket;
        let denom = b.frames_with_compose.max(1);
        let avg_compose_cb_ns = b.compose_cb_record_ns / denom;
        let avg_gpu_render_ns = b.gpu_render_ns / denom;
        let damage_fraction = if b.output_pixels > 0 {
            #[allow(clippy::cast_precision_loss)]
            (b.damaged_pixels as f64 / b.output_pixels as f64)
        } else {
            0.0
        };
        let group_avg = if b.submit_group_flushes > 0 {
            #[allow(clippy::cast_precision_loss)]
            (b.submit_group_size_total as f64 / b.submit_group_flushes as f64)
        } else {
            0.0
        };
        log::info!(
            "v2_telemetry: paint_submits/s={} composite_submits/s={} \
             one_shot_submits/s={} queue_submit2/s={} \
             vk_queue_wait_idle/s={} cpu_fence_wait_ns/s={} \
             cpu_fence_wait_count/s={} damage_fraction={damage_fraction:.3} \
             scene_entries_visited={} scene_entries_drawn={} \
             full_redraw_fallback/s={} storage_allocations/s={} \
             descriptor_allocations/s={} image_view_creates/s={} \
             frame_present_count/s={} missed_pageflips/s={} \
             atlas_intern/s={} glyph_uploads/s={} \
             glyphs_dropped_atlas_full(lifetime)={} \
             composite_glyphs_dropped_unsupported(lifetime)={} \
             disjoint_readback_count/s={} \
             descriptor_pool_creates/s={} descriptor_pool_resets/s={} \
             render_batches_flushed/s={} render_composites_coalesced/s={} \
             avg_gpu_render_ns={avg_gpu_render_ns} \
             avg_compose_cb_record_ns={avg_compose_cb_ns} \
             submit_group_flushes/s={} submit_group_aborts/s={} \
             submit_group_size_avg={group_avg:.2} \
             submit_group_size_max_in_window={} \
             submit_group_hist={:?} \
             submit_group_flush_reason_sync_boundary/s={} \
             submit_group_flush_reason_present_completion_signal/s={} \
             submit_group_flush_reason_scene_compose/s={} \
             submit_group_flush_reason_pageflip_retire/s={} \
             submit_group_flush_reason_max_size/s={} \
             submit_group_flush_reason_shutdown/s={} \
             submit_group_flush_reason_frame_builder/s={} \
             active_descriptor_pool_count_high_water={} \
             active_staging_bytes_high_water={} \
             active_scratch_bytes_high_water={} \
             cursor_move_ebusy/s={} cursor_move_ebusy(lifetime)={} \
             submitted_queue_depth={}",
            b.paint_submits,
            b.composite_submits,
            b.one_shot_submits,
            b.queue_submit2,
            b.vk_queue_wait_idle,
            b.cpu_fence_wait_ns,
            b.cpu_fence_wait_count,
            b.scene_entries_visited,
            b.scene_entries_drawn,
            b.full_redraw_fallback,
            b.storage_allocations,
            b.descriptor_allocations,
            b.image_view_creates,
            b.frame_present_count,
            b.missed_pageflips,
            b.atlas_intern,
            b.glyph_uploads,
            self.lifetime.glyphs_dropped_atlas_full,
            self.lifetime.composite_glyphs_dropped_unsupported,
            b.disjoint_readback_count,
            b.descriptor_pool_creates,
            b.descriptor_pool_resets,
            b.render_batches_flushed,
            b.render_composites_coalesced,
            b.submit_group_flushes,
            b.submit_group_aborts,
            b.submit_group_size_max_in_window,
            b.submit_group_hist,
            b.submit_group_flush_reason_sync_boundary,
            b.submit_group_flush_reason_present_completion_signal,
            b.submit_group_flush_reason_scene_compose,
            b.submit_group_flush_reason_pageflip_retire,
            b.submit_group_flush_reason_max_size,
            b.submit_group_flush_reason_shutdown,
            b.submit_group_flush_reason_frame_builder,
            self.lifetime.active_descriptor_pool_count_high_water,
            self.lifetime.active_staging_bytes_high_water,
            self.lifetime.active_scratch_bytes_high_water,
            b.cursor_move_ebusy,
            self.lifetime.cursor_move_ebusy,
            submitted_depth,
        );
        #[allow(clippy::cast_precision_loss)]
        let fb_ops_avg =
            b.frame_builder_ops_per_frame_total as f64 / b.frame_builder_closes.max(1) as f64;
        #[allow(clippy::cast_precision_loss)]
        let fb_glyph_avg = b.frame_builder_glyph_uploads_per_frame_total as f64
            / b.frame_builder_closes.max(1) as f64;
        #[allow(clippy::cast_precision_loss)]
        let fb_renders_avg =
            b.frame_builder_renders_per_frame_total as f64 / b.frame_builder_closes.max(1) as f64;
        log::info!(
            "v2_telemetry: frame_builder_opens={} closes={} aborts={} \
             ops/frame_avg={fb_ops_avg:.1} max={} hist={:?} \
             glyph_uploads/frame_avg={fb_glyph_avg:.1} max={} \
             renders/frame_avg={fb_renders_avg:.1} max={} active_pins_hw={} \
             close_reasons[scene_compose={} non_ported={} legacy_sc={} \
             present_completion={} sync_wait={} timeout={} shutdown={} pin_ceiling={} \
             scratch_grow={}]",
            b.frame_builder_opens,
            b.frame_builder_closes,
            b.frame_builder_aborts,
            b.frame_builder_ops_per_frame_max_in_window,
            b.frame_builder_ops_per_frame_hist,
            b.frame_builder_glyph_uploads_per_frame_max_in_window,
            b.frame_builder_renders_per_frame_max_in_window,
            b.frame_builder_active_pins_high_water,
            b.frame_builder_close_reason_scene_compose,
            b.frame_builder_close_reason_non_ported_paint_op,
            b.frame_builder_close_reason_legacy_sc_compose,
            b.frame_builder_close_reason_present_completion_signal,
            b.frame_builder_close_reason_sync_wait,
            b.frame_builder_close_reason_timeout,
            b.frame_builder_close_reason_shutdown,
            b.frame_builder_close_reason_pin_ceiling,
            b.frame_builder_close_reason_scratch_grow,
        );
        self.bucket = Bucket::default();
        self.last_emit = now;
    }

    /// Whether emission is enabled. Tests use this to decide
    /// whether to assert lifetime counts.
    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }

    /// Explicit flush of the active [`SubmitTrace`]. Called from
    /// `KmsBackendV2::disable_output` before any potentially-
    /// blocking teardown (VkDevice destroy on the platform side
    /// can hang on some drivers) so the trace's `BufWriter`
    /// commits its buffered tail to disk while we still control
    /// the timing. No-op when tracing is off.
    pub(crate) fn flush_submit_trace(&mut self) {
        if let Some(trace) = self.submit_trace.as_mut() {
            trace.flush();
        }
    }

    // ── Counter sites ───────────────────────────────────────────

    pub(crate) fn record_paint_submit(&mut self) {
        self.bucket.paint_submits += 1;
        self.bucket.queue_submit2 += 1;
        self.lifetime.paint_submits += 1;
        self.lifetime.queue_submit2 += 1;
    }

    pub(crate) fn record_composite_submit(&mut self) {
        self.bucket.composite_submits += 1;
        self.bucket.queue_submit2 += 1;
        self.bucket.frames_with_compose += 1;
        self.lifetime.composite_submits += 1;
        self.lifetime.queue_submit2 += 1;
        self.lifetime.frames_with_compose += 1;
    }

    pub(crate) fn record_one_shot_submit(&mut self) {
        self.bucket.one_shot_submits += 1;
        self.bucket.queue_submit2 += 1;
        self.lifetime.one_shot_submits += 1;
        self.lifetime.queue_submit2 += 1;
    }

    pub(crate) fn record_vk_queue_wait_idle(&mut self) {
        self.bucket.vk_queue_wait_idle += 1;
        self.lifetime.vk_queue_wait_idle += 1;
    }

    pub(crate) fn record_fence_wait(&mut self, ns: u64) {
        self.bucket.cpu_fence_wait_ns = self.bucket.cpu_fence_wait_ns.saturating_add(ns);
        self.bucket.cpu_fence_wait_count += 1;
        self.lifetime.cpu_fence_wait_ns = self.lifetime.cpu_fence_wait_ns.saturating_add(ns);
        self.lifetime.cpu_fence_wait_count += 1;
    }

    pub(crate) fn record_damage_pixels(&mut self, damaged: u64, output: u64) {
        self.bucket.damaged_pixels = self.bucket.damaged_pixels.saturating_add(damaged);
        self.bucket.output_pixels = self.bucket.output_pixels.saturating_add(output);
        self.lifetime.damaged_pixels = self.lifetime.damaged_pixels.saturating_add(damaged);
        self.lifetime.output_pixels = self.lifetime.output_pixels.saturating_add(output);
    }

    pub(crate) fn record_scene_entries(&mut self, visited: u64, drawn: u64) {
        self.bucket.scene_entries_visited =
            self.bucket.scene_entries_visited.saturating_add(visited);
        self.bucket.scene_entries_drawn = self.bucket.scene_entries_drawn.saturating_add(drawn);
        self.lifetime.scene_entries_visited =
            self.lifetime.scene_entries_visited.saturating_add(visited);
        self.lifetime.scene_entries_drawn = self.lifetime.scene_entries_drawn.saturating_add(drawn);
    }

    pub(crate) fn record_full_redraw_fallback(&mut self) {
        self.bucket.full_redraw_fallback += 1;
        self.lifetime.full_redraw_fallback += 1;
    }

    pub(crate) fn record_storage_allocation(&mut self) {
        self.bucket.storage_allocations += 1;
        self.lifetime.storage_allocations += 1;
    }

    pub(crate) fn record_descriptor_allocations(&mut self, n: u64) {
        self.bucket.descriptor_allocations = self.bucket.descriptor_allocations.saturating_add(n);
        self.lifetime.descriptor_allocations =
            self.lifetime.descriptor_allocations.saturating_add(n);
    }

    pub(crate) fn record_image_view_create(&mut self) {
        self.bucket.image_view_creates += 1;
        self.lifetime.image_view_creates += 1;
    }

    pub(crate) fn record_frame_present(&mut self) {
        self.bucket.frame_present_count += 1;
        self.lifetime.frame_present_count += 1;
    }

    pub(crate) fn record_missed_pageflip(&mut self) {
        self.bucket.missed_pageflips += 1;
        self.lifetime.missed_pageflips += 1;
    }

    pub(crate) fn record_compose_cb_record_ns(&mut self, ns: u64) {
        self.bucket.compose_cb_record_ns = self.bucket.compose_cb_record_ns.saturating_add(ns);
        self.lifetime.compose_cb_record_ns = self.lifetime.compose_cb_record_ns.saturating_add(ns);
    }

    // ── Stage 3a counter sites ──────────────────────────────────

    /// Bumped on every successful `intern` that pushed a new
    /// entry (cache miss). Cache hits do NOT increment this.
    pub(crate) fn record_atlas_intern(&mut self) {
        self.bucket.atlas_intern += 1;
        self.lifetime.atlas_intern += 1;
    }

    /// Bumped on every glyph upload CB submitted to the queue.
    /// Today matches `atlas_intern` 1:1 (per-glyph upload); kept
    /// distinct so Stage 5's batched-upload work can keep the
    /// intern rate honest while collapsing `glyph_uploads`.
    pub(crate) fn record_glyph_upload(&mut self) {
        self.bucket.glyph_uploads += 1;
        self.bucket.queue_submit2 += 1;
        self.lifetime.glyph_uploads += 1;
        self.lifetime.queue_submit2 += 1;
    }

    /// Bumped when a text run skips a glyph because the atlas is
    /// full (post-eviction would refuse the pack). Lifetime
    /// counter only — Stage 3 plan §"Non-goals" §6: "drop the
    /// glyph + log once + counter."
    pub(crate) fn record_glyph_dropped_atlas_full(&mut self) {
        self.bucket.glyphs_dropped_atlas_full += 1;
        self.lifetime.glyphs_dropped_atlas_full += 1;
    }

    // ── Stage 3d counter sites (sites wired in 3d) ──────────────

    pub(crate) fn record_composite_glyphs_dropped_unsupported(&mut self) {
        self.bucket.composite_glyphs_dropped_unsupported += 1;
        self.lifetime.composite_glyphs_dropped_unsupported += 1;
    }

    pub(crate) fn record_disjoint_readback(&mut self) {
        self.bucket.disjoint_readback_count += 1;
        self.lifetime.disjoint_readback_count += 1;
    }

    /// Stage 5 Task 4 layer 1: one `vkCreateDescriptorPool` site
    /// inside `DescriptorPoolRing::acquire_set` (no-Free-slot growth
    /// branch).
    pub(crate) fn record_descriptor_pool_create(&mut self) {
        self.bucket.descriptor_pool_creates += 1;
        self.lifetime.descriptor_pool_creates += 1;
    }

    /// Stage 5 Task 4 layer 1: bumped once per `vkResetDescriptorPool`
    /// `Ok` arm inside `DescriptorPoolRing::release_up_to`. `n` is
    /// the number of pools the call reset in a single sweep (the
    /// return value of `release_up_to`).
    pub(crate) fn record_descriptor_pool_reset(&mut self, n: u64) {
        self.bucket.descriptor_pool_resets = self.bucket.descriptor_pool_resets.saturating_add(n);
        self.lifetime.descriptor_pool_resets =
            self.lifetime.descriptor_pool_resets.saturating_add(n);
    }

    // ── Phase B.1 Task 21: frame builder recording sites ─────────

    /// Bumped once per `FrameBuilder::open_for_paint` (delta-tracked
    /// via `KmsBackendV2::drain_frame_builder_telemetry`).
    pub(crate) fn record_frame_builder_open(&mut self) {
        self.bucket.frame_builder_opens += 1;
        self.lifetime.frame_builder_opens += 1;
    }

    /// Record one frame close with the given reason and per-frame stats.
    /// Updates per-close-reason counters, ops histogram, glyph-upload
    /// totals, and max-in-window gauges in both the current bucket and
    /// the lifetime aggregate.
    pub(crate) fn record_frame_builder_close(
        &mut self,
        reason: super::frame_builder::CloseReason,
        ops_in_frame: usize,
        glyph_uploads_in_frame: u32,
        renders_in_frame: u32,
    ) {
        use super::frame_builder::CloseReason as R;
        self.bucket.frame_builder_closes += 1;
        self.lifetime.frame_builder_closes += 1;
        let ops = u64::try_from(ops_in_frame).unwrap_or(u64::MAX);
        self.bucket.frame_builder_ops_per_frame_total += ops;
        self.lifetime.frame_builder_ops_per_frame_total += ops;
        if ops > self.bucket.frame_builder_ops_per_frame_max_in_window {
            self.bucket.frame_builder_ops_per_frame_max_in_window = ops;
        }
        if ops > self.lifetime.frame_builder_ops_per_frame_max_in_window {
            self.lifetime.frame_builder_ops_per_frame_max_in_window = ops;
        }
        let hist_idx = match ops_in_frame {
            0 | 1 => 0,
            2..=3 => 1,
            4..=7 => 2,
            8..=15 => 3,
            16..=31 => 4,
            32..=63 => 5,
            64..=127 => 6,
            _ => 7,
        };
        self.bucket.frame_builder_ops_per_frame_hist[hist_idx] += 1;
        self.lifetime.frame_builder_ops_per_frame_hist[hist_idx] += 1;
        let uploads = u64::from(glyph_uploads_in_frame);
        self.bucket.frame_builder_glyph_uploads_per_frame_total += uploads;
        self.lifetime.frame_builder_glyph_uploads_per_frame_total += uploads;
        if uploads
            > self
                .bucket
                .frame_builder_glyph_uploads_per_frame_max_in_window
        {
            self.bucket
                .frame_builder_glyph_uploads_per_frame_max_in_window = uploads;
        }
        if uploads
            > self
                .lifetime
                .frame_builder_glyph_uploads_per_frame_max_in_window
        {
            self.lifetime
                .frame_builder_glyph_uploads_per_frame_max_in_window = uploads;
        }
        // Phase B.2 Task 14: accumulate per-frame render counts the
        // same way glyph_uploads_per_frame does — bucket + lifetime
        // totals, max-in-window gauges in both.
        let renders = u64::from(renders_in_frame);
        self.bucket.frame_builder_renders_per_frame_total = self
            .bucket
            .frame_builder_renders_per_frame_total
            .saturating_add(renders);
        self.lifetime.frame_builder_renders_per_frame_total = self
            .lifetime
            .frame_builder_renders_per_frame_total
            .saturating_add(renders);
        if renders > self.bucket.frame_builder_renders_per_frame_max_in_window {
            self.bucket.frame_builder_renders_per_frame_max_in_window = renders;
        }
        if renders > self.lifetime.frame_builder_renders_per_frame_max_in_window {
            self.lifetime.frame_builder_renders_per_frame_max_in_window = renders;
        }
        let (b, l) = match reason {
            R::SceneCompose => (
                &mut self.bucket.frame_builder_close_reason_scene_compose,
                &mut self.lifetime.frame_builder_close_reason_scene_compose,
            ),
            R::NonPortedPaintOp => (
                &mut self.bucket.frame_builder_close_reason_non_ported_paint_op,
                &mut self.lifetime.frame_builder_close_reason_non_ported_paint_op,
            ),
            R::LegacyScCompose => (
                &mut self.bucket.frame_builder_close_reason_legacy_sc_compose,
                &mut self.lifetime.frame_builder_close_reason_legacy_sc_compose,
            ),
            R::PresentCompletionSignal => (
                &mut self
                    .bucket
                    .frame_builder_close_reason_present_completion_signal,
                &mut self
                    .lifetime
                    .frame_builder_close_reason_present_completion_signal,
            ),
            R::SyncWait => (
                &mut self.bucket.frame_builder_close_reason_sync_wait,
                &mut self.lifetime.frame_builder_close_reason_sync_wait,
            ),
            R::Timeout => (
                &mut self.bucket.frame_builder_close_reason_timeout,
                &mut self.lifetime.frame_builder_close_reason_timeout,
            ),
            R::Shutdown => (
                &mut self.bucket.frame_builder_close_reason_shutdown,
                &mut self.lifetime.frame_builder_close_reason_shutdown,
            ),
            R::PinCeiling => (
                &mut self.bucket.frame_builder_close_reason_pin_ceiling,
                &mut self.lifetime.frame_builder_close_reason_pin_ceiling,
            ),
            R::ScratchGrow => (
                &mut self.bucket.frame_builder_close_reason_scratch_grow,
                &mut self.lifetime.frame_builder_close_reason_scratch_grow,
            ),
        };
        *b += 1;
        *l += 1;
    }

    /// Bumped when a frame close followed a Vk error (abort path). In
    /// B.1 the abort signal is inferred from the drain caller; see
    /// `KmsBackendV2::drain_frame_builder_telemetry`.
    pub(crate) fn record_frame_builder_abort(&mut self) {
        self.bucket.frame_builder_aborts += 1;
        self.lifetime.frame_builder_aborts += 1;
    }

    /// Update the pin-count high-water mark. Called once per drained
    /// `FrameCloseEvent` with `event.pin_count` cast to u64.
    pub(crate) fn record_frame_builder_active_pins_high_water(&mut self, n: u64) {
        if n > self.bucket.frame_builder_active_pins_high_water {
            self.bucket.frame_builder_active_pins_high_water = n;
        }
        if n > self.lifetime.frame_builder_active_pins_high_water {
            self.lifetime.frame_builder_active_pins_high_water = n;
        }
    }

    // ── Phase A: SubmitGroup + retention recording sites ─────────

    /// Record one non-aborted SubmitGroup flush with the given
    /// group size (number of entries submitted) and flush reason.
    pub(crate) fn record_submit_group_flush(
        &mut self,
        size: usize,
        reason: super::submit_group::FlushReason,
    ) {
        let s = u64::try_from(size).unwrap_or(u64::MAX);
        self.bucket.submit_group_flushes += 1;
        self.lifetime.submit_group_flushes += 1;
        self.bucket.submit_group_size_total += s;
        self.lifetime.submit_group_size_total += s;
        if s > self.bucket.submit_group_size_max_in_window {
            self.bucket.submit_group_size_max_in_window = s;
        }
        if s > self.lifetime.submit_group_size_max_in_window {
            self.lifetime.submit_group_size_max_in_window = s;
        }
        let bucket_idx = match size {
            0 | 1 => 0,
            2 => 1,
            3..=4 => 2,
            5..=8 => 3,
            9..=12 => 4,
            _ => 5,
        };
        self.bucket.submit_group_hist[bucket_idx] += 1;
        self.lifetime.submit_group_hist[bucket_idx] += 1;
        use super::submit_group::FlushReason as R;
        let (b, l) = match reason {
            R::SyncBoundary => (
                &mut self.bucket.submit_group_flush_reason_sync_boundary,
                &mut self.lifetime.submit_group_flush_reason_sync_boundary,
            ),
            R::PresentCompletionSignal => (
                &mut self
                    .bucket
                    .submit_group_flush_reason_present_completion_signal,
                &mut self
                    .lifetime
                    .submit_group_flush_reason_present_completion_signal,
            ),
            R::SceneCompose => (
                &mut self.bucket.submit_group_flush_reason_scene_compose,
                &mut self.lifetime.submit_group_flush_reason_scene_compose,
            ),
            R::PageflipRetire => (
                &mut self.bucket.submit_group_flush_reason_pageflip_retire,
                &mut self.lifetime.submit_group_flush_reason_pageflip_retire,
            ),
            R::MaxSize => (
                &mut self.bucket.submit_group_flush_reason_max_size,
                &mut self.lifetime.submit_group_flush_reason_max_size,
            ),
            R::Shutdown => (
                &mut self.bucket.submit_group_flush_reason_shutdown,
                &mut self.lifetime.submit_group_flush_reason_shutdown,
            ),
            R::FrameBuilder => (
                &mut self.bucket.submit_group_flush_reason_frame_builder,
                &mut self.lifetime.submit_group_flush_reason_frame_builder,
            ),
        };
        *b += 1;
        *l += 1;
    }

    /// Record one aborted SubmitGroup flush (Vk error path).
    pub(crate) fn record_submit_group_abort(&mut self) {
        self.bucket.submit_group_aborts += 1;
        self.lifetime.submit_group_aborts += 1;
    }

    /// Sample the current descriptor pool ring count; update the
    /// high-water mark if `n` exceeds the current maximum.
    pub(crate) fn record_active_descriptor_pool_high_water(&mut self, n: u64) {
        if n > self.bucket.active_descriptor_pool_count_high_water {
            self.bucket.active_descriptor_pool_count_high_water = n;
        }
        if n > self.lifetime.active_descriptor_pool_count_high_water {
            self.lifetime.active_descriptor_pool_count_high_water = n;
        }
    }

    /// Sample current active staging bytes; update the high-water
    /// mark for both the current bucket window and lifetime.
    pub(crate) fn record_active_staging_high_water(&mut self, bytes: u64) {
        if bytes > self.bucket.active_staging_bytes_high_water {
            self.bucket.active_staging_bytes_high_water = bytes;
        }
        if bytes > self.lifetime.active_staging_bytes_high_water {
            self.lifetime.active_staging_bytes_high_water = bytes;
        }
    }

    /// Sample current active scratch bytes; update the high-water
    /// mark for both the current bucket window and lifetime.
    pub(crate) fn record_active_scratch_high_water(&mut self, bytes: u64) {
        if bytes > self.bucket.active_scratch_bytes_high_water {
            self.bucket.active_scratch_bytes_high_water = bytes;
        }
        if bytes > self.lifetime.active_scratch_bytes_high_water {
            self.lifetime.active_scratch_bytes_high_water = bytes;
        }
    }

    /// Per-CRTC cursor-plane atomic move that returned `EBUSY`
    /// (kernel rejected the cursor commit because a primary-plane
    /// commit is pending on the same CRTC). `n` is the number of
    /// CRTCs that EBUSY'd in this one `cursor_plane_move` call —
    /// 0 means everything committed cleanly, >0 means the cursor
    /// position was lost on that many CRTCs.
    pub(crate) fn record_cursor_move_ebusy(&mut self, n: u64) {
        self.bucket.cursor_move_ebusy = self.bucket.cursor_move_ebusy.saturating_add(n);
        self.lifetime.cursor_move_ebusy = self.lifetime.cursor_move_ebusy.saturating_add(n);
    }

    /// Stage 5 Task 3 (render-composite generalization): one
    /// render batch flushed. Bumps `paint_submits` + `queue_submit2`
    /// since each flush is one real `vkQueueSubmit2`.
    pub(crate) fn record_render_batch_flushed(&mut self, coalesced: u32) {
        self.bucket.render_batches_flushed += 1;
        self.bucket.render_composites_coalesced = self
            .bucket
            .render_composites_coalesced
            .saturating_add(u64::from(coalesced));
        self.bucket.paint_submits += 1;
        self.bucket.queue_submit2 += 1;
        self.lifetime.render_batches_flushed += 1;
        self.lifetime.render_composites_coalesced = self
            .lifetime
            .render_composites_coalesced
            .saturating_add(u64::from(coalesced));
        self.lifetime.paint_submits += 1;
        self.lifetime.queue_submit2 += 1;
    }
}

impl Default for Telemetry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_accumulate_in_bucket_and_lifetime() {
        let mut t = Telemetry::new();
        t.record_paint_submit();
        t.record_paint_submit();
        t.record_composite_submit();
        t.record_one_shot_submit();
        assert_eq!(t.lifetime.paint_submits, 2);
        assert_eq!(t.lifetime.composite_submits, 1);
        assert_eq!(t.lifetime.one_shot_submits, 1);
        // All three increment queue_submit2 too.
        assert_eq!(t.lifetime.queue_submit2, 4);
        assert_eq!(t.bucket.queue_submit2, 4);
    }

    #[test]
    fn fence_wait_aggregates_ns_and_count() {
        let mut t = Telemetry::new();
        t.record_fence_wait(1_000);
        t.record_fence_wait(2_500);
        assert_eq!(t.lifetime.cpu_fence_wait_ns, 3_500);
        assert_eq!(t.lifetime.cpu_fence_wait_count, 2);
    }

    #[test]
    fn maybe_emit_resets_bucket_after_log() {
        let mut t = Telemetry::new();
        t.record_paint_submit();
        t.bucket.compose_cb_record_ns = 100;
        // Simulate >1s elapsed by adjusting last_emit.
        t.last_emit = Instant::now() - std::time::Duration::from_secs(2);
        // Force enabled so emit body actually runs the reset
        // (logging is suppressed when env var unset, but bucket
        // reset still happens).
        t.enabled = true;
        t.maybe_emit(0);
        assert_eq!(t.bucket.paint_submits, 0);
        assert_eq!(t.bucket.compose_cb_record_ns, 0);
        // Lifetime preserved.
        assert_eq!(t.lifetime.paint_submits, 1);
    }

    #[test]
    fn vk_queue_wait_idle_counted_separately() {
        let mut t = Telemetry::new();
        t.record_vk_queue_wait_idle();
        t.record_vk_queue_wait_idle();
        assert_eq!(t.lifetime.vk_queue_wait_idle, 2);
        // Target zero in steady state — the gate is "lifetime
        // count stays at 0 except inside get_image".
    }

    #[test]
    fn atlas_counters_disjoint_from_paint_submits() {
        let mut t = Telemetry::new();
        t.record_atlas_intern();
        t.record_atlas_intern();
        t.record_glyph_upload();
        t.record_glyph_upload();
        t.record_glyph_dropped_atlas_full();
        // intern is logical (cache miss) — does NOT bump queue_submit2.
        assert_eq!(t.lifetime.atlas_intern, 2);
        // glyph upload bumps queue_submit2 (one CB per upload).
        assert_eq!(t.lifetime.glyph_uploads, 2);
        assert_eq!(t.lifetime.queue_submit2, 2);
        assert_eq!(t.lifetime.glyphs_dropped_atlas_full, 1);
        // No paint_submits side effect from atlas activity.
        assert_eq!(t.lifetime.paint_submits, 0);
    }

    #[test]
    fn stage3d_counters_lifetime_track() {
        let mut t = Telemetry::new();
        t.record_composite_glyphs_dropped_unsupported();
        t.record_disjoint_readback();
        t.record_disjoint_readback();
        assert_eq!(t.lifetime.composite_glyphs_dropped_unsupported, 1);
        assert_eq!(t.lifetime.disjoint_readback_count, 2);
    }

    #[test]
    fn descriptor_pool_counters_accumulate() {
        let mut t = Telemetry::new();
        t.record_descriptor_pool_create();
        t.record_descriptor_pool_create();
        t.record_descriptor_pool_reset(3);
        assert_eq!(t.lifetime.descriptor_pool_creates, 2);
        assert_eq!(t.lifetime.descriptor_pool_resets, 3);
    }

    /// Phase B.2 Task 14 step 9: smoke test that the `ScratchGrow`
    /// close-reason bucket actually increments when
    /// `record_frame_builder_close` is called with that variant. The
    /// bucket field was added by Task 1; this test makes sure the
    /// `record_frame_builder_close` match arm + bucket field stay in
    /// lockstep with `CloseReason::ScratchGrow` going forward, so the
    /// telemetry log line never silently drops a close reason.
    #[test]
    fn v2_telemetry_record_frame_builder_close_counts_scratch_grow() {
        let mut t = Telemetry::new();
        t.record_frame_builder_close(
            crate::kms::v2::frame_builder::CloseReason::ScratchGrow,
            /* ops_in_frame = */ 0,
            /* glyph_uploads_in_frame = */ 0,
            /* renders_in_frame = */ 0,
        );
        assert_eq!(t.lifetime.frame_builder_close_reason_scratch_grow, 1);
        assert_eq!(t.bucket.frame_builder_close_reason_scratch_grow, 1);
    }

    /// Phase B.2 Task 14: `record_frame_builder_close` accumulates the
    /// renders-per-frame total (sum across closes) and tracks the
    /// max-in-window gauge in both the bucket and lifetime aggregates.
    /// Pure telemetry test — no Vk needed.
    #[test]
    fn v2_telemetry_record_frame_builder_close_accumulates_renders_per_frame() {
        let mut t = Telemetry::new();
        use crate::kms::v2::frame_builder::CloseReason as R;
        t.record_frame_builder_close(R::SceneCompose, 0, 0, 3);
        t.record_frame_builder_close(R::SceneCompose, 0, 0, 7);
        t.record_frame_builder_close(R::SceneCompose, 0, 0, 1);
        assert_eq!(t.lifetime.frame_builder_renders_per_frame_total, 11);
        assert_eq!(t.bucket.frame_builder_renders_per_frame_total, 11);
        assert_eq!(t.lifetime.frame_builder_renders_per_frame_max_in_window, 7);
        assert_eq!(t.bucket.frame_builder_renders_per_frame_max_in_window, 7);
    }

    #[test]
    fn cursor_move_ebusy_accumulates_in_bucket_and_lifetime() {
        let mut t = Telemetry::new();
        // Single-CRTC EBUSY.
        t.record_cursor_move_ebusy(1);
        // Multi-CRTC EBUSY in one motion event (e.g., dual-output).
        t.record_cursor_move_ebusy(2);
        assert_eq!(t.bucket.cursor_move_ebusy, 3);
        assert_eq!(t.lifetime.cursor_move_ebusy, 3);
        // Zero call (caller invariant: only called with n > 0, but
        // saturating add still has to do the right thing).
        t.record_cursor_move_ebusy(0);
        assert_eq!(t.bucket.cursor_move_ebusy, 3);
        assert_eq!(t.lifetime.cursor_move_ebusy, 3);
    }

    #[test]
    fn telemetry_submit_group_hist_buckets_correctly() {
        let mut t = Telemetry::new();
        use crate::kms::v2::submit_group::FlushReason;
        for size in [1, 2, 4, 8, 12, 16, 32] {
            t.record_submit_group_flush(size, FlushReason::SceneCompose);
        }
        assert_eq!(t.lifetime.submit_group_hist[0], 1); // 1
        assert_eq!(t.lifetime.submit_group_hist[1], 1); // 2
        assert_eq!(t.lifetime.submit_group_hist[2], 1); // 4
        assert_eq!(t.lifetime.submit_group_hist[3], 1); // 8
        assert_eq!(t.lifetime.submit_group_hist[4], 1); // 12
        assert_eq!(t.lifetime.submit_group_hist[5], 2); // 16, 32
    }
}
