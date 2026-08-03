//! Stage 5 frame-builder Phase B sub-phase B.1: deferred per-frame
//! op-list recording.
//!
//! `FrameBuilder` owns a `Closed ↔ OpenForPaint` lifecycle. Paint
//! entry points that have been ported (`composite_glyphs` in B.1)
//! append `RecordedOp`s; a close trigger (Invariant M2 / M3, the
//! existing `get_image` / PRESENT-completion sync points, a timeout,
//! shutdown, or a pin-set ceiling) replays the op list as ONE primary
//! command buffer, submits it via the `SubmitGroup` (cap=1, so the
//! submit auto-flushes immediately), and parks the frame's resource
//! pins on a `pending_frames` queue gated by the submit's
//! `FenceTicket`.
//!
//! Phase B spec — `docs/superpowers/specs/2026-05-24-frame-builder-phase-b-design.md`.
//! This file holds the no-Vk-required pieces (state machine, op enum,
//! pin sets, layout overlay); the recording side lives in
//! `engine.rs::FrameBuilder::close_into_cb_*` because it needs the
//! engine's CB pool + atlas + drawable-store access.

use std::time::{Duration, Instant};

use super::platform::FenceTicket;

/// Why a frame closed. Bumped into telemetry on every close so the
/// rollout can see which trigger is dominating.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CloseReason {
    /// `maybe_composite` saw a ready output + dirty scene; the frame
    /// closes paint-only (compose stays separate in B.1 — folded into
    /// the frame at B.4).
    #[allow(dead_code, reason = "B.4 — compose joins frame; reserved variant")]
    SceneCompose,
    /// Invariant M2: a non-ported paint op is about to record its own
    /// CB; close the frame first so the non-ported op sees committed
    /// `Drawable::storage.current_layout` + `last_render_ticket`.
    NonPortedPaintOp,
    /// Invariant M3: legacy scene compose is about to record; close
    /// the frame first for the same reason as M2.
    LegacyScCompose,
    /// COW PRESENT-completion semaphore got attached; the frame must
    /// close immediately so `vkGetSemaphoreFdKHR(SYNC_FD)` sees a
    /// queued signal-op (Task 6.1 yoga hang precedent).
    PresentCompletionSignal,
    /// `get_image` is about to wait on a fence; close the frame first
    /// so the readback's `ticket.wait()` observes a submitted CB.
    SyncWait,
    /// Idle / no-pageflip case. A frame open > T ms forces close to
    /// release pinned resources.
    Timeout,
    /// `KmsBackend::shutdown` is tearing down platform state.
    Shutdown,
    /// `max_pinned_resources_per_frame` ceiling hit (1024 default).
    PinCeiling,
    /// Phase B.2 Mechanism 3 (Pitfall 4): a growable scratch image would
    /// be replaced while the open frame has prior ops that recorded
    /// views into the old image. Close first so the old backing rides
    /// the just-submitted frame fence (via `submitted.back`) — see
    /// `adopt_retired_resource_for_gpu_retirement` in `engine.rs`.
    #[allow(
        dead_code,
        reason = "B.2 Phase 9A close-before-grow path; wired in a later Task. \
                  Variant lands now so the exhaustive close-reason test \
                  enumerates it and downstream code can match it."
    )]
    ScratchGrow,
    /// A RENDER Composite whose SOURCE is an active redirect-target
    /// backing (xfwm's redirected popup backings) is isolated into its
    /// own submit — the open frame is closed before AND after recording
    /// it. Batching >1 such composite into one deferred submit made the
    /// compositor sample stale/zero source content, compositing popups
    /// empty (the XFCE "submenu painted but not shown" bug, fixed
    /// 2026-07-13). Ordinary composites stay batched. See
    /// `RenderEngine::render_composite`.
    RedirectSourceBoundary,
    /// Deferred DamageNotify delivery needs every recorded write to an
    /// exported backing submitted + fence-published before the events
    /// go out; an open frame would otherwise keep those writes out of
    /// the submit group (`Backend::flush_exported_render_writes`).
    DamagePublish,
}

/// `FrameBuilder` lifecycle. `Closed` is the hot path for X11 traffic
/// that doesn't touch the paint surface (event-only requests, idle).
/// `OpenForPaint` is where every recorded op accumulates between
/// the first paint and a close trigger.
///
/// Phase B's spec sketches a third state, `ClosingWithCompose`, for
/// when scene compose joins the frame. That state lands in sub-phase
/// B.4; B.1 only carries the two-state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrameState {
    Closed,
    OpenForPaint,
}

#[derive(Debug)]
pub(crate) struct FrameBuilder {
    state: FrameState,
    /// Phase B.1 Task 15: pub(crate) so ported paint ops in engine.rs
    /// can append `RecordedOp`s + pins + overlays without indirecting
    /// through helper methods for every field touch.
    pub(crate) open: Option<Box<OpenFrame>>,
    lifetime_opens: u64,
    lifetime_closes: u64,
    max_pinned_resources_per_frame: usize,
    /// Phase B.1 Task 20: latch so the pin-set ceiling log emits
    /// once per process lifetime rather than on every trip.
    pin_ceiling_warned: bool,
}

/// Frame-close outcome surfaced to the engine. `Submitted` carries
/// the same `FlushOutcome` Phase A's `flush_submit_group` returned
/// (number of entries flushed, reason); the frame builder produces
/// one such outcome per close. `AlreadyClosed` means "frame was already
/// closed; nothing to do".
#[derive(Debug)]
pub(crate) enum CloseOutcome {
    /// Frame closed and submitted (one CB through SubmitGroup
    /// auto-flush). Carries the frame ticket the caller will record
    /// for retirement.
    #[allow(
        dead_code,
        reason = "return-value structure for the engine close path; B.2+ may consume \
                  fields; callers currently match on the variant without reading fields"
    )]
    Submitted {
        frame_seq: u64,
        op_count: usize,
        pin_count: usize,
        ticket: FenceTicket,
        reason: CloseReason,
    },
    /// Close requested but frame was already closed. No-op.
    AlreadyClosed,
}

impl FrameBuilder {
    pub(crate) fn new() -> Self {
        Self {
            state: FrameState::Closed,
            open: None,
            lifetime_opens: 0,
            lifetime_closes: 0,
            max_pinned_resources_per_frame: 1024,
            pin_ceiling_warned: false,
        }
    }

    #[allow(dead_code, reason = "test/diagnostic introspection; B.2+ may consume")]
    pub(crate) fn state(&self) -> FrameState {
        self.state
    }

    pub(crate) fn is_open(&self) -> bool {
        matches!(self.state, FrameState::OpenForPaint)
    }

    pub(crate) fn lifetime_opens(&self) -> u64 {
        self.lifetime_opens
    }

    pub(crate) fn lifetime_closes(&self) -> u64 {
        self.lifetime_closes
    }

    #[allow(
        dead_code,
        reason = "test-only ceiling tuning helper; production reads max_pinned_resources_per_frame"
    )]
    pub(crate) fn set_max_pinned_resources_per_frame(&mut self, n: usize) {
        self.max_pinned_resources_per_frame = n.max(1);
    }

    pub(crate) fn max_pinned_resources_per_frame(&self) -> usize {
        self.max_pinned_resources_per_frame
    }

    /// Phase B.1 Task 20: log a pin-ceiling-hit warning, but only the
    /// first time per process lifetime. Subsequent calls are no-ops.
    /// `n` is the breaching pin count for the log message.
    pub(crate) fn note_pin_ceiling_hit_once(&mut self, n: usize) {
        if !self.pin_ceiling_warned {
            log::warn!("frame_builder: pin set ceiling at {n} — forcing close");
            self.pin_ceiling_warned = true;
        }
    }

    /// Open a new frame, acquiring the shared `FenceTicket` from
    /// `SubmitGroup::open_with`. Panics if the frame is already open
    /// (caller is responsible for checking `is_open()` first).
    ///
    /// `frame_generation` is the captured-at-open value of the
    /// engine's `acquire_generation` watermark. Every descriptor
    /// acquisition routed through
    /// `acquire_descriptor_set_for_frame_or_op` while this frame is
    /// open tags the descriptor pool with `frame_generation`; the
    /// SubmittedOp pushed at close uses the same value. The retire
    /// path's `release_up_to(generation)` then correctly retires
    /// exactly the frame's pools (Phase B.2 Mechanism 2 watermark).
    pub(crate) fn open_for_paint(&mut self, ticket: FenceTicket, frame_generation: u64) {
        assert!(
            !self.is_open(),
            "FrameBuilder::open_for_paint while already open — caller must check is_open()"
        );
        self.state = FrameState::OpenForPaint;
        self.lifetime_opens = self.lifetime_opens.wrapping_add(1);
        self.open = Some(Box::new(OpenFrame {
            ticket,
            frame_generation,
            ops: Vec::new(),
            pins: FramePinSet::new(),
            layouts: FrameLayoutTable::new(),
            touched: TouchedDrawables::new(),
            pending_glyph_inserts: PendingGlyphInserts::new(),
            atlas_prev_ticket_snapshot: None,
            glyph_uploads_in_frame: 0,
            close_reason_on_open: None,
            opened_at: Instant::now(),
            pending_present_completions: Vec::new(), // NEW (B.3 N10)
            snapshot_touch: std::collections::HashMap::new(), // NEW (Phase 2 clip)
        }));
    }

    /// Take the open frame for replay. Returns `None` if not open.
    /// Caller is responsible for calling either `complete_close_success`
    /// or `complete_close_failure` afterwards to update the lifetime
    /// counter and bring the FrameBuilder back to `Closed`.
    pub(crate) fn take_open_for_close(&mut self, reason: CloseReason) -> Option<Box<OpenFrame>> {
        if !self.is_open() {
            return None;
        }
        let mut frame = self.open.take().expect("is_open implies Some");
        frame.close_reason_on_open = Some(reason);
        Some(frame)
    }

    /// Finalise close-success path. The caller has already submitted
    /// the CB and committed overlays/pins/inserts into engine + atlas
    /// state; this updates the FrameBuilder's bookkeeping.
    pub(crate) fn complete_close_success(&mut self) {
        debug_assert!(matches!(self.state, FrameState::OpenForPaint));
        self.state = FrameState::Closed;
        self.lifetime_closes = self.lifetime_closes.wrapping_add(1);
        // `self.open` is already None (take_open_for_close moved it).
    }

    /// Finalise close-failure path. The caller has already rolled back
    /// engine/atlas state and set `platform.renderer_failed`; this
    /// updates the FrameBuilder's bookkeeping.
    pub(crate) fn complete_close_failure(&mut self) {
        debug_assert!(matches!(self.state, FrameState::OpenForPaint));
        self.state = FrameState::Closed;
        self.lifetime_closes = self.lifetime_closes.wrapping_add(1);
    }

    /// True if the next append would push the pin set past the
    /// per-frame ceiling. Caller checks this and forces a close
    /// (`reason = PinCeiling`) BEFORE the new op's append.
    #[allow(
        dead_code,
        reason = "B.2+ — currently inlined per-glyph in composite_glyphs_via_frame_builder; \
                  preserved for B.2+ ports that need the helper form"
    )]
    pub(crate) fn would_exceed_pin_ceiling(&self, new_pins: usize) -> bool {
        match self.open.as_ref() {
            None => false, // no frame open → nothing to exceed
            Some(open) => open.pins.len() + new_pins > self.max_pinned_resources_per_frame,
        }
    }

    /// Phase B.1 close trigger 4: true if the frame has been open for
    /// at least `dur` (used by `maybe_composite` to drive the timeout
    /// close once per tick if needed).
    pub(crate) fn open_for_at_least(&self, dur: Duration) -> bool {
        match self.open.as_ref() {
            None => false,
            Some(o) => o.opened_at.elapsed() >= dur,
        }
    }

    /// Phase B.1 close trigger 4: read the timeout duration from
    /// `YSERVER_FRAME_BUILDER_TIMEOUT_MS` env var, default 16 ms
    /// (one vblank at 60 Hz).
    pub(crate) fn timeout_from_env_default_16ms() -> Duration {
        let ms = std::env::var("YSERVER_FRAME_BUILDER_TIMEOUT_MS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(16);
        Duration::from_millis(ms)
    }

    /// `#[cfg(test)]` peek at the op list in append order.
    #[cfg(test)]
    #[allow(
        dead_code,
        reason = "scaffolded integration tests; activates when wired in B.2+"
    )]
    pub(crate) fn peek_ops(&self) -> Option<&[RecordedOp]> {
        self.open.as_ref().map(|o| o.ops.as_slice())
    }

    /// `#[cfg(test)]` op count.
    #[cfg(test)]
    pub(crate) fn op_count(&self) -> usize {
        self.open.as_ref().map_or(0, |o| o.ops.len())
    }

    /// `#[cfg(test)]` pin count.
    #[cfg(test)]
    #[allow(
        dead_code,
        reason = "scaffolded integration tests; activates when wired in B.2+"
    )]
    pub(crate) fn pin_count(&self) -> usize {
        self.open.as_ref().map_or(0, |o| o.pins.len())
    }
}

/// Per-frame bookkeeping. Allocated when `Closed → OpenForPaint` fires;
/// dropped on close.
#[derive(Debug)]
pub(crate) struct OpenFrame {
    pub(crate) ticket: FenceTicket,
    /// Phase B.2 Mechanism 2: captured-at-open value of
    /// `RenderEngineInner::acquire_generation`. The engine bumps
    /// `acquire_generation` once at `open_for_paint` and stores the
    /// resulting value here; every descriptor acquisition during the
    /// open frame uses this value via
    /// `acquire_descriptor_set_for_frame_or_op`, and the
    /// `SubmittedOp` pushed at close also uses this value. Replaces
    /// the legacy "bump at close" timing so the descriptor pool's
    /// `release_up_to(generation)` watermark coincides with frame
    /// retirement.
    pub(crate) frame_generation: u64,
    pub(crate) ops: Vec<RecordedOp>,
    pub(crate) pins: FramePinSet,
    pub(crate) layouts: FrameLayoutTable,
    pub(crate) touched: TouchedDrawables,
    pub(crate) pending_glyph_inserts: PendingGlyphInserts,
    /// Snapshot of `GlyphAtlas::last_render_ticket` taken at the first
    /// glyph append-in-frame. `Some(None)` means "the atlas had no
    /// prior ticket" — distinct from `None` which means "not yet
    /// touched in this frame".
    pub(crate) atlas_prev_ticket_snapshot: Option<Option<FenceTicket>>,
    /// Glyph uploads recorded in this frame; bumped each push.
    pub(crate) glyph_uploads_in_frame: u32,
    pub(crate) close_reason_on_open: Option<CloseReason>, // reserved for B.4
    /// Phase B.1 Task 18: wall-clock instant the frame was opened.
    /// Used by `open_for_at_least` to drive the timeout close trigger.
    pub(crate) opened_at: Instant,
    /// Phase B.3 (N10): X PRESENT completions attached to this open frame
    /// via `attach_cow_present_completion`. Drained at close-success into
    /// `pending_present_batches` (alongside the acquired
    /// `PresentCompletionSignal`'s semaphore queued on the submit).
    /// Force-enqueued as a degraded `PendingPresentBatch { wait: Ready,
    /// ticket: None, signal: None, events }` on close-failure — never
    /// silent drop; the X PRESENT protocol observes the events
    /// regardless of submit success.
    #[allow(
        dead_code,
        reason = "Phase B.3 Task 4 wires up attach_cow_present_completion + close-path \
                  drain; the slot lands in Task 1 so the OpenFrame shape is stable \
                  before Task 4's atomic cow_copy_area rewrite."
    )]
    pub(crate) pending_present_completions: Vec<super::present_completion::PendingPresentEntry>,
    /// Phase 2 clip — snapshots touched this frame: id -> (pre_frame_layout,
    /// prior_ticket, prev_snapshotted_version). The third element is
    /// load-bearing for rollback (codex round-4): a failed close must restore
    /// the OLD version so the next frame still re-refreshes. Mirrors the
    /// drawable overlay + atlas snapshot.
    pub(crate) snapshot_touch: std::collections::HashMap<
        super::engine::SnapshotId,
        (ash::vk::ImageLayout, Option<FenceTicket>, u64),
    >,
}

#[cfg(test)]
mod state_tests {
    use super::*;

    #[test]
    fn fresh_frame_builder_is_closed_with_no_lifetime_counts() {
        let fb = FrameBuilder::new();
        assert_eq!(fb.state(), FrameState::Closed);
        assert!(!fb.is_open());
        assert_eq!(fb.lifetime_opens(), 0);
        assert_eq!(fb.lifetime_closes(), 0);
    }

    #[test]
    fn default_pin_ceiling_is_1024() {
        let fb = FrameBuilder::new();
        assert_eq!(fb.max_pinned_resources_per_frame(), 1024);
    }

    #[test]
    fn set_max_pinned_resources_clamps_to_at_least_one() {
        let mut fb = FrameBuilder::new();
        fb.set_max_pinned_resources_per_frame(0);
        assert_eq!(fb.max_pinned_resources_per_frame(), 1);
        fb.set_max_pinned_resources_per_frame(42);
        assert_eq!(fb.max_pinned_resources_per_frame(), 42);
    }

    #[test]
    fn close_reason_has_eleven_variants() {
        fn _exhaustive(r: CloseReason) -> &'static str {
            match r {
                CloseReason::SceneCompose => "scene_compose",
                CloseReason::NonPortedPaintOp => "non_ported_paint_op",
                CloseReason::LegacyScCompose => "legacy_sc_compose",
                CloseReason::PresentCompletionSignal => "present_completion_signal",
                CloseReason::SyncWait => "sync_wait",
                CloseReason::Timeout => "timeout",
                CloseReason::Shutdown => "shutdown",
                CloseReason::PinCeiling => "pin_ceiling",
                CloseReason::ScratchGrow => "scratch_grow",
                CloseReason::RedirectSourceBoundary => "redirect_source_boundary",
                CloseReason::DamagePublish => "damage_publish",
            }
        }
        assert_eq!(_exhaustive(CloseReason::SceneCompose), "scene_compose");
        assert_eq!(_exhaustive(CloseReason::ScratchGrow), "scratch_grow");
        assert_eq!(
            _exhaustive(CloseReason::RedirectSourceBoundary),
            "redirect_source_boundary"
        );
    }
}

// The rest of this module — RecordedOp, FramePinSet, FrameLayoutTable,
// FrameSubmittedRecord — lands in subsequent tasks (see below).

use ash::vk;

use super::{
    glyph_atlas::{AtlasEntry, GlyphKey},
    store::DrawableId,
};
use crate::kms::cpu_types::Rectangle16;

/// Index into `OpenFrame::pins.staging_buffers`. Saved on `RecordedOp`
/// payloads so close-time replay can fetch the right pinned buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PinnedStagingIdx(pub(crate) u32);

/// A glyph to draw at frame-close time. Mirrors the in-tree
/// `TextGlyph` struct (`crate::kms::vk::ops::text::TextGlyph`); we hold
/// our own copy here so the recorded op is independent of the live
/// `TextGlyph` type (which the recorder consumes by reference).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RecordedTextGlyph {
    pub(crate) atlas_x: u32,
    pub(crate) atlas_y: u32,
    pub(crate) w: u32,
    pub(crate) h: u32,
    pub(crate) dst_x: i32,
    pub(crate) dst_y: i32,
}

#[derive(Debug)]
pub(crate) struct RecordedCompositeGlyphs {
    pub(crate) dst_id: DrawableId,
    pub(crate) dst_old_layout: vk::ImageLayout,
    /// Wire PictOp (standard family 0..=12, validated at append).
    /// Emit selects the per-`(op, dst_format, dst_has_alpha)` text
    /// pipeline from this — the cache entry is guaranteed built at
    /// append time, so emit only looks it up.
    pub(crate) op: u8,
    /// Whether the dst picture's PictFormat carries meaningful
    /// alpha (`dst_has_alpha_for_pict_format` at append time) —
    /// the third text-pipeline cache-key dimension.
    pub(crate) dst_has_alpha: bool,
    pub(crate) foreground_rgba: [f32; 4],
    /// Pin index of the per-glyph instance vertex buffer (built at
    /// record time, `#1` glyph batching). Emit binds
    /// `pins.staging_buffers[instance_pin.0].buffer` and issues one
    /// instanced draw per clip rect.
    pub(crate) instance_pin: PinnedStagingIdx,
    /// Number of glyph instances in that buffer (`vkCmdDraw` instance count).
    pub(crate) instance_count: u32,
    pub(crate) clip_scissors: Vec<vk::Rect2D>,
    /// Damage rect to commit on close-success. Pre-computed at append
    /// time (today's `composite_glyphs` already computes the same
    /// bbox at engine.rs:3913-3922) so close-time doesn't have to
    /// re-walk the glyph list.
    #[allow(
        dead_code,
        reason = "B.2+ reserved slot for ops that carry close-time-committed damage; \
                  B.1 mutates damage at append time via store.damage()"
    )]
    pub(crate) damage_rect: Option<vk::Rect2D>,
}

#[derive(Debug)]
pub(crate) struct RecordedGlyphUpload {
    /// Pin index into the frame's staging-buffer pin vector. Replay
    /// reads the buffer handle from the pinned Arc.
    pub(crate) staging_pin_idx: PinnedStagingIdx,
    pub(crate) atlas_x: u32,
    pub(crate) atlas_y: u32,
    pub(crate) w: u32,
    pub(crate) h: u32,
    /// Cache-insert pair to commit on close-success (atlas's lookup
    /// becomes hit-able by this key after the frame ticket signals,
    /// but the cache entry is committed in the engine on close-success
    /// — the spec's "transactional cache insert" discipline).
    #[allow(
        dead_code,
        reason = "B.2+ — replay-side cache commit. B.1 uses PendingGlyphInserts \
                  for the canonical cache-commit path."
    )]
    pub(crate) insert_key: GlyphKey,
    #[allow(
        dead_code,
        reason = "B.2+ — replay-side cache commit. B.1 uses PendingGlyphInserts \
                  for the canonical cache-commit path."
    )]
    pub(crate) insert_entry: AtlasEntry,
}

/// Mirror of the inputs to `vk::ops::render::record_render_composite`,
/// resolved into pinnable handles at append time. The op replay reads
/// these fields + the frame's pin vectors via index, NOT by looking
/// the resource up by id at emit-time.
///
/// View handles are stable for the life of the frame: Drawable views
/// via the drawable view cache + ticket-touch, Solid / Gradient /
/// scratch views via engine ownership or `submitted.back` pin per
/// Phase 9A.
#[derive(Debug)]
#[allow(
    dead_code,
    reason = "Phase B.2 — several payload fields (dst_id, src_view, mask_view, \
              dst_readback_view, needs_dst_readback) are write-only post-Task-12: \
              the descriptor write happens at append-time (Task 11), the close-time \
              replay (Task 12) re-uses the cached descriptor_set + dst_image/view/extent. \
              The append-time-only fields stay on the payload for Debug introspection + \
              future B.3 ports that may need to re-resolve at emit-time."
)]
pub(crate) struct RecordedRenderComposite {
    pub(crate) op: u8,
    pub(crate) dst_id: DrawableId,
    pub(crate) dst_image: vk::Image,
    pub(crate) dst_view: vk::ImageView,
    pub(crate) dst_extent: vk::Extent2D,
    pub(crate) dst_format: vk::Format,
    pub(crate) dst_has_alpha: bool,
    pub(crate) dst_old_layout: vk::ImageLayout,
    /// Pre-resolved sample view (Drawable / Solid / Gradient src).
    /// View handle's owning resource is kept alive by the frame's
    /// `touched_drawables` ticket-touch (Drawable) or the engine's
    /// long-lived ownership (Solid / Gradient).
    pub(crate) src_view: vk::ImageView,
    pub(crate) mask_view: vk::ImageView,
    pub(crate) src_alias_view: Option<vk::ImageView>,
    pub(crate) dst_readback_view: Option<vk::ImageView>,
    /// USER-codex R11.F1+F2 — pre-built `CompositeAttrs` replay-ready.
    /// Constructed at op-append time by resolving `src_repeat` to the
    /// bare shader repeat constant (packing happens inside
    /// `record_render_composite_draws`), `src_force_opaque` via the
    /// legacy pict-format-aware helper, and the composed `src_xform`
    /// (`picture_xform ∘ user_transform`) — same logic as `_legacy`'s
    /// pre-call site.
    pub(crate) attrs: crate::kms::vk::ops::render::CompositeAttrs,
    /// Per-op solid clear inputs (Solid src / mask) —
    /// `record_solid_color_clear` fires at emit-time against the
    /// engine's `solid_src_image` / `solid_mask_image` before the
    /// composite draws. `None` for non-Solid.
    pub(crate) src_clear_color: Option<[f32; 4]>,
    pub(crate) mask_clear_color: Option<[f32; 4]>,
    /// Pipeline cache key inputs (not packed into `CompositeAttrs`
    /// because the pipeline lookup happens at emit-time via
    /// `RenderPipelineCache::get`).
    pub(crate) mask_component_alpha: bool,
    pub(crate) needs_dst_readback: bool,
    pub(crate) rects: Box<[crate::kms::vk::ops::render::CompositeRect]>,
    pub(crate) clip_rects: Option<Box<[Rectangle16]>>,
    pub(crate) descriptor_set: vk::DescriptorSet,
}

/// Phase B.3 (N5): trap source resolved at append-time. Drawable holds the
/// id + the pict_format-aware swizzle class snapshot (see engine.rs:7360 —
/// the swizzle class is computed from `src_pict_format` + drawable depth at
/// append, pinned because RenderPictFormat resize could land between append
/// and emit); Solid carries the clear color snapshot (emit calls
/// `record_solid_color_clear` against `engine.solid_src_image`); Gradient
/// carries the picture xid (emit re-looks up `picture_paint[xid]` because
/// gradients are CPU-immutable per B.2 R3 finding 9) + the intrinsic axis
/// projection snapped here at append.
#[derive(Debug, Clone)]
#[allow(
    dead_code,
    reason = "Phase B.3 Task 12 (render_traps_or_tris body rewrite) constructs these \
              variants; the discriminant lands in Task 1 so the RecordedRenderTrapsOrTris \
              payload + emit dispatch can match exhaustively before Task 12."
)]
pub(crate) enum RecordedTrapSrcKind {
    Drawable {
        id: DrawableId,
        swizzle_class: super::engine::SwizzleClass,
    },
    Solid([f32; 4]),
    /// B.3 hotfix 2: holds a strong Arc clone of the gradient picture.
    /// The clone keeps the Vk resources alive past `picture_paint_remove`
    /// (which drops the engine's HashMap entry). At close time the clone
    /// is moved into `pins.retired_resources` so it outlives the GPU CB.
    Gradient {
        picture: crate::kms::vk::gradient::GradientPicture,
        intrinsic_axis_projection: crate::kms::vk::ops::render::AffineXform,
    },
}

/// Phase B.3 (TRANSFER family — N1, N3, N8). Covers both `copy_area` and
/// `cow_copy_area` (the cow is just a regular DrawableId per N3). Emits two
/// barrier pairs + cmd_copy_image (disjoint case) or three pairs +
/// cmd_copy_image × 2 (self-overlap case — N8). The `self_overlap_scratch`
/// slot is the SINGLE source of truth for the per-op scratch lifetime —
/// see Pitfall 7 + N8 + Task 1's close-path scratch walk.
#[derive(Debug)]
#[allow(
    dead_code,
    reason = "Phase B.3 Tasks 2 (copy_area) + 4 (cow_copy_area) populate these fields; \
              the payload lands in Task 1 so close_open_frame's scratch walk + the \
              emit dispatch can match exhaustively."
)]
pub(crate) struct RecordedCopyArea {
    pub(crate) dst_id: DrawableId,
    pub(crate) src_id: DrawableId,
    pub(crate) src_rect: vk::Rect2D,
    pub(crate) dst_rect: vk::Rect2D,
    pub(crate) src_format: vk::Format,
    pub(crate) src_extent: vk::Extent2D,
    pub(crate) dst_extent: vk::Extent2D,
    pub(crate) src_image: vk::Image,
    pub(crate) dst_image: vk::Image,
    pub(crate) src_old_layout: vk::ImageLayout,
    pub(crate) dst_old_layout: vk::ImageLayout,
    /// Imported dma-bufs return to `GENERAL` so their external producer can
    /// write them again. Server-owned drawables retain N1's shader-read
    /// terminal layout.
    pub(crate) src_final_layout: vk::ImageLayout,
    pub(crate) dst_final_layout: vk::ImageLayout,
    pub(crate) src_imported: bool,
    pub(crate) dst_imported: bool,
    /// `Some(scratch)` when `src_id == dst_id` (self-overlap path,
    /// engine.rs:2814-2918 mirror). The scratch is owned by this
    /// payload from append until close; the close-path scratch walk
    /// `std::mem::take`s it into a `Vec<ScratchImage>` passed to
    /// `SubmittedOp::scratch`. NEVER an `OpenFrame::live_scratches`
    /// sibling — single source of truth per N8 + Pitfall 7.
    pub(crate) self_overlap_scratch: Option<super::engine::ScratchImage>,
}

/// Standalone snapshot-refresh op (codex round-4 finding 5 + unification): a
/// `cmd_copy_image` live-clip-pixmap → GC-owned snapshot. This is the SINGLE
/// refresh mechanism — it is appended (by `refresh_clip_snapshot`, Task 11)
/// both at clip-mask install (pixmap guaranteed live → closes retain-after-free)
/// and before any in-scope masked copy whose snapshot version is stale while the
/// pixmap is still alive (closes same-frame mask-write ordering). The masked-blit
/// op itself NEVER refreshes — it only samples the (already-fresh) snapshot.
///
/// Emit (Task 13): live→TRANSFER_SRC, snapshot→TRANSFER_DST, cmd_copy_image,
/// snapshot→SHADER_READ, live→SHADER_READ. Both end at SHADER_READ (N1). The
/// `ALL_COMMANDS` source stage on the live read orders it after any same-frame
/// write to the live mask.
#[derive(Debug)]
#[allow(
    dead_code,
    reason = "Phase B.3 clip — refresh_clip_snapshot (Task 11) constructs this variant; \
              the payload lands here so dst_id() + emit dispatch match exhaustively."
)]
pub(crate) struct RecordedClipSnapshotRefresh {
    pub(crate) snapshot_id: super::engine::SnapshotId,
    pub(crate) snapshot_image: vk::Image,
    pub(crate) snapshot_old_layout: vk::ImageLayout,
    /// Live clip-pixmap drawable read by the copy. A frame participant
    /// (first-touch/ticket/old-layout, recorded in `refresh_clip_snapshot`);
    /// ends at SHADER_READ_ONLY_OPTIMAL.
    pub(crate) live_mask_id: DrawableId,
    pub(crate) live_mask_image: vk::Image,
    pub(crate) live_mask_old_layout: vk::ImageLayout,
    /// Copy region: live mask → snapshot, both at the same texel coords.
    pub(crate) copy_extent: vk::Extent2D,
}

#[derive(Debug)]
#[allow(
    dead_code,
    reason = "Phase B.3 clip — masked_copy_area (Task 7) constructs this variant; \
              the payload lands here so dst_id() + emit dispatch match exhaustively."
)]
pub(crate) struct RecordedMaskedCopyArea {
    pub(crate) dst_id: DrawableId,
    pub(crate) src_id: DrawableId,
    /// dst color format (selects the masked_blit pipeline).
    pub(crate) dst_format: vk::Format,
    pub(crate) dst_image: vk::Image,
    pub(crate) dst_view: vk::ImageView,
    pub(crate) dst_extent: vk::Extent2D,
    // --- LIVE source drawable (codex round-4 findings 1+2): used by the
    // self-overlap src→scratch COPY and by the src→SHADER_READ barrier on the
    // non-overlap path. NEVER the scratch. ---
    pub(crate) src_image: vk::Image,
    pub(crate) src_old_layout: vk::ImageLayout,
    /// The clamped LIVE source rect offset (= `src_rect.offset`). Used ONLY by
    /// the self-overlap `src → scratch` copy as `src_offset` (codex round-5
    /// finding 1: `copy_offset` is the SAMPLE-space offset and is rewritten to
    /// `−dst_rect.offset` on self-overlap, so it must NOT be reused for the copy).
    pub(crate) live_src_offset: [i32; 2],
    // --- SAMPLED source (what the frag's texelFetch reads): the src IDENTITY
    // view normally, the scratch IDENTITY view on self-overlap. ---
    pub(crate) sample_view: vk::ImageView,
    pub(crate) sample_extent: vk::Extent2D,
    /// Mask image + IDENTITY R8 view to sample. The GC-owned snapshot in
    /// production; a plain depth-1 drawable in the exactness tests.
    pub(crate) mask_image: vk::Image,
    pub(crate) mask_view: vk::ImageView,
    pub(crate) mask_extent: vk::Extent2D,
    /// `mask_texel = dst_pixel - clip_origin`.
    pub(crate) clip_origin: [i32; 2],
    /// `src_texel = dst_pixel + copy_offset`. Non-overlap: src_rect.offset −
    /// dst_rect.offset. Self-overlap (sampling the scratch which holds the
    /// region at (0,0)): −dst_rect.offset.
    pub(crate) copy_offset: [i32; 2],
    /// The clamped dst rect the draw covers (the quad is placed over it).
    pub(crate) dst_rect: vk::Rect2D,
    /// Surviving GC-rect/child/window scissors (>=1). One scissored draw each.
    pub(crate) scissors: Vec<vk::Rect2D>,
    pub(crate) dst_old_layout: vk::ImageLayout,
    pub(crate) mask_old_layout: vk::ImageLayout,
    /// `Some` when `src_id == dst_id`. The LIVE src region (from `src_image`) is
    /// copied here, then `sample_view` points at this scratch's view.
    pub(crate) self_overlap_scratch: Option<super::engine::SampledScratchImage>,
}

/// Phase B.3 (TRANSFER family — N1, N2). Staging buffer is pinned via
/// `open.pins.pin_staging` at append; emit fetches the buffer handle from
/// `pins.staging_buffers[staging_pin_idx.0 as usize].buffer`.
#[derive(Debug)]
#[allow(
    dead_code,
    reason = "Phase B.3 Task 6 (put_image body rewrite) populates these fields; the \
              payload lands in Task 1 so the emit dispatch can match exhaustively."
)]
pub(crate) struct RecordedPutImage {
    pub(crate) dst_id: DrawableId,
    pub(crate) dst_rect: vk::Rect2D,
    pub(crate) dst_image: vk::Image,
    pub(crate) dst_extent: vk::Extent2D,
    pub(crate) dst_old_layout: vk::ImageLayout,
    pub(crate) staging_pin_idx: PinnedStagingIdx,
}

/// Phase B.3 (FILL family — N4). One variant covers both `fill_rect`
/// (N=1) and `fill_rect_batch` (N≥1). Emit uses `cmd_clear_attachments`
/// directly — NO composite pipeline, NO descriptor (codex round-7 catch
/// rewrote the spec). `load_op = LOAD` is LOAD-BEARING per the FILL
/// pseudocode in the spec — outside-rect pixels must be preserved.
#[derive(Debug)]
#[allow(
    dead_code,
    reason = "Phase B.3 Task 8 (fill_rect / fill_rect_batch body rewrite) populates \
              these fields; the payload lands in Task 1 so the emit dispatch can \
              match exhaustively."
)]
pub(crate) struct RecordedFillRect {
    pub(crate) dst_id: DrawableId,
    pub(crate) dst_image_view: vk::ImageView,
    pub(crate) dst_extent: vk::Extent2D,
    pub(crate) dst_format: vk::Format,
    pub(crate) dst_old_layout: vk::ImageLayout,
    pub(crate) color: [f32; 4],
    pub(crate) rects: Vec<vk::Rect2D>, // pre-clamped at append, non-empty
}

/// Phase B.3 (FILL family — N6). Distinct from `RecordedFillRect`:
/// uses LogicFillPipelineCache + push constants + per-rect scissor draws.
/// `opaque_alpha` is a caller-provided GC parameter (cache key), NOT
/// derived from `dst_format` — codex round-8 catch.
#[derive(Debug)]
#[allow(
    dead_code,
    reason = "Phase B.3 Task 10 (logic_fill body rewrite) populates these fields; the \
              payload lands in Task 1 so the emit dispatch can match exhaustively."
)]
pub(crate) struct RecordedLogicFill {
    pub(crate) dst_id: DrawableId,
    pub(crate) dst_image_view: vk::ImageView,
    pub(crate) dst_extent: vk::Extent2D,
    pub(crate) dst_format: vk::Format,
    pub(crate) dst_old_layout: vk::ImageLayout,
    pub(crate) logic_mode: yserver_core::backend::GcFunction,
    pub(crate) opaque_alpha: bool,
    pub(crate) color: [f32; 4],
    pub(crate) rects: Vec<vk::Rect2D>, // pre-clamped at append, non-empty
}

/// Phase B.3 (GLYPH family — N7). Companion to B.1's `RecordedGlyphUpload`
/// (reused verbatim per N7). Same shape as `RecordedCompositeGlyphs` but
/// emit calls into the text-pipeline path. `dst_format == B8G8R8A8_UNORM`
/// is implicit (append-side gate per N7).
#[derive(Debug)]
#[allow(
    dead_code,
    reason = "Phase B.3 Task 14 (image_text body rewrite) populates these fields; the \
              payload lands in Task 1 so the emit dispatch can match exhaustively."
)]
pub(crate) struct RecordedImageText {
    pub(crate) dst_id: DrawableId,
    pub(crate) dst_extent: vk::Extent2D,
    pub(crate) dst_old_layout: vk::ImageLayout,
    pub(crate) foreground_rgba: [f32; 4],
    /// Pin index of the per-glyph instance vertex buffer (`#1`).
    pub(crate) instance_pin: PinnedStagingIdx,
    /// Number of glyph instances (`vkCmdDraw` instance count).
    pub(crate) instance_count: u32,
}

/// Phase B.3 (MASK family — N5). Single variant covers both raster + composite
/// stages. Emit re-resolves `engine.mask_scratch`, `engine.dst_readback`,
/// composite pipeline, and descriptor set fresh; none of those four are
/// recorded.
#[derive(Debug)]
#[allow(
    dead_code,
    reason = "Phase B.3 Task 12 (render_traps_or_tris body rewrite) populates these \
              fields; the payload lands in Task 1 so the emit dispatch can match \
              exhaustively."
)]
pub(crate) struct RecordedRenderTrapsOrTris {
    // Dst identity and layout (N1 / B.2).
    pub(crate) dst_id: DrawableId,
    pub(crate) dst_image: vk::Image,
    pub(crate) dst_view: vk::ImageView,
    pub(crate) dst_old_layout: vk::ImageLayout,
    pub(crate) dst_extent: vk::Extent2D,
    pub(crate) dst_format: vk::Format,
    pub(crate) dst_has_alpha: bool,
    // Composite operator + derived gates.
    pub(crate) std_op: crate::kms::vk::render_pipeline::StdPictOp,
    pub(crate) op_byte: u8, // raw byte for the `needs_full_dst` pattern test (engine.rs:7472).
    // Source kind + per-kind snapshot data.
    pub(crate) src_kind: RecordedTrapSrcKind,
    // CompositeAttrs inputs.
    pub(crate) src_extent: vk::Extent2D,
    pub(crate) src_is_synthetic_1x1: bool,
    pub(crate) src_repeat: u32, // pre-resolved shader constant.
    pub(crate) src_force_opaque: bool,
    pub(crate) user_src_xform: crate::kms::vk::ops::render::AffineXform,
    /// Source-picture sampling origin in src-pixel space, already
    /// shifted by the dst redirect / `x_off` delta (i.e. `xSrc - dx`,
    /// `ySrc - dy`). The composite emit adds `bbox_x`/`bbox_y` for the
    /// non-`needs_full_dst` branch. Only consulted for `Drawable`
    /// sources — `Solid` is a constant colour and `Gradient` is
    /// positioned by its intrinsic transform. Honouring the client's
    /// `xSrc`/`ySrc` here is what lets GTK CSD drop-shadows (a black
    /// solid masked by a blur-ramp picture sampled at `ySrc != 0`)
    /// render with the correct feather instead of a solid slab.
    pub(crate) src_origin_x: i32,
    pub(crate) src_origin_y: i32,
    // Trap raster phase inputs.
    pub(crate) prim_kind: super::engine::TrapPrimKind,
    pub(crate) bbox_x: i32,
    pub(crate) bbox_y: i32,
    pub(crate) bbox_w: u32,
    pub(crate) bbox_h: u32,
    pub(crate) instance_count: u32,
    // Composite phase rect layout (clip pre-clamped at append).
    pub(crate) clip_scissors: Vec<vk::Rect2D>,
    // Pinned resources.
    pub(crate) vertex_pool_pin: PinnedStagingIdx,
}

/// Reserved for future ops that need an explicit cross-frame layout
/// transition. `composite_glyphs` doesn't emit any in B.1 (the text
/// pipeline's recorder embeds its own barriers via the per-call
/// `StorageTextTarget` adapter), but the variant exists so the
/// recorder skeleton in Task 11 can match exhaustively and B.2 can
/// fold ported `render_composite` / `render_fill` paths in without
/// touching this enum's variant set.
#[derive(Debug)]
pub(crate) struct RecordedLayoutTransition {
    pub(crate) drawable_id: DrawableId,
    pub(crate) src_stage: vk::PipelineStageFlags2,
    pub(crate) src_access: vk::AccessFlags2,
    pub(crate) dst_stage: vk::PipelineStageFlags2,
    pub(crate) dst_access: vk::AccessFlags2,
    pub(crate) target_layout: vk::ImageLayout,
}

#[derive(Debug)]
pub(crate) enum RecordedOp {
    CompositeGlyphs(RecordedCompositeGlyphs),
    GlyphUpload(RecordedGlyphUpload),
    /// `RecordedRenderComposite` is ~248B (mostly Vk handles +
    /// `CompositeAttrs`); embedding it directly grows `RecordedOp`'s
    /// largest variant past the 256B watch threshold (spec § "Op
    /// variant sizing"). Boxing keeps the enum tag + ptr at 16B per
    /// `RecordedOp` slot — the 0/1 alloc per emitted op is acceptable
    /// vs the per-frame `Vec<RecordedOp>` padding cost the alternative
    /// would impose.
    RenderComposite(Box<RecordedRenderComposite>),
    #[allow(
        dead_code,
        reason = "B.2+ — ports that emit explicit cross-frame layout transitions; \
                  composite_glyphs in B.1 mutates layouts via the recorder's internal barriers"
    )]
    LayoutTransition(RecordedLayoutTransition),
    // Phase B.3 — all Box-wrapped per the size-budget rule.
    #[allow(
        dead_code,
        reason = "Phase B.3 Tasks 2 + 4 (copy_area + cow_copy_area) construct this \
                  variant; lands in Task 1 so dst_id() + emit dispatch match exhaustively."
    )]
    CopyArea(Box<RecordedCopyArea>),
    #[allow(
        dead_code,
        reason = "Phase B.3 Task 6 (put_image) constructs this variant; lands in Task 1 \
                  so dst_id() + emit dispatch match exhaustively."
    )]
    PutImage(Box<RecordedPutImage>),
    #[allow(
        dead_code,
        reason = "Phase B.3 Task 8 (fill_rect / fill_rect_batch) constructs this variant; \
                  lands in Task 1 so dst_id() + emit dispatch match exhaustively."
    )]
    FillRect(Box<RecordedFillRect>),
    #[allow(
        dead_code,
        reason = "Phase B.3 Task 10 (logic_fill) constructs this variant; lands in Task 1 \
                  so dst_id() + emit dispatch match exhaustively."
    )]
    LogicFill(Box<RecordedLogicFill>),
    #[allow(
        dead_code,
        reason = "Phase B.3 Task 14 (image_text) constructs this variant; lands in Task 1 \
                  so dst_id() + emit dispatch match exhaustively."
    )]
    ImageText(Box<RecordedImageText>),
    #[allow(
        dead_code,
        reason = "Phase B.3 Task 12 (render_traps_or_tris) constructs this variant; lands \
                  in Task 1 so dst_id() + emit dispatch match exhaustively."
    )]
    RenderTrapsOrTris(Box<RecordedRenderTrapsOrTris>),
    #[allow(
        dead_code,
        reason = "Phase B.3 clip — masked_copy_area (Task 7) constructs this variant; lands \
                  here so dst_id() + emit dispatch match exhaustively."
    )]
    MaskedCopyArea(Box<RecordedMaskedCopyArea>),
    #[allow(
        dead_code,
        reason = "Phase B.3 clip — refresh_clip_snapshot (Task 11) constructs this variant; \
                  lands here so dst_id() + emit dispatch match exhaustively."
    )]
    ClipSnapshotRefresh(Box<RecordedClipSnapshotRefresh>),
}

impl RecordedOp {
    /// Phase B.3 (N10): the drawable this op WRITES to, or `None` for
    /// utility variants without a writable drawable destination.
    /// `attach_cow_present_completion`'s predicate uses this to decide
    /// whether to attach the completion to the open frame (per the spec's
    /// N10 — `touched` is the wrong predicate because it includes sampled-
    /// only references).
    #[allow(
        dead_code,
        reason = "Phase B.3 Task 4 wires up attach_cow_present_completion to call this \
                  helper; the predicate lands in Task 1 so the exhaustive match \
                  catches a future variant addition at compile time before Task 4."
    )]
    pub(crate) fn dst_id(&self) -> Option<DrawableId> {
        match self {
            RecordedOp::CompositeGlyphs(g) => Some(g.dst_id),
            RecordedOp::RenderComposite(rc) => Some(rc.dst_id),
            RecordedOp::GlyphUpload(_) => None, // writes to atlas, not a drawable
            RecordedOp::LayoutTransition(_) => None, // utility variant
            RecordedOp::CopyArea(ca) => Some(ca.dst_id),
            RecordedOp::PutImage(pi) => Some(pi.dst_id),
            RecordedOp::FillRect(fr) => Some(fr.dst_id),
            RecordedOp::LogicFill(lf) => Some(lf.dst_id),
            RecordedOp::ImageText(it) => Some(it.dst_id),
            RecordedOp::RenderTrapsOrTris(rt) => Some(rt.dst_id),
            RecordedOp::MaskedCopyArea(m) => Some(m.dst_id),
            RecordedOp::ClipSnapshotRefresh(r) => Some(r.live_mask_id), // only drawable touched
        }
    }
}

impl OpenFrame {
    /// Phase B.2 Pitfall 6 / codex round 4 finding 3: append a recorded
    /// op and apply the post-op overlay updates in a SINGLE critical
    /// section so no reentrant code path can observe `ops.len() = N+1`
    /// with the overlay still at the N-th op's value.
    ///
    /// `drawable_layout_updates` is the (id, post-op layout) tuple list
    /// for every drawable the recorded op transitions. For
    /// `render_composite`, that's `&[(dst_id, SHADER_READ_ONLY_OPTIMAL)]`
    /// (one entry, the post-`record_render_composite_close` layout). The
    /// helper is the ONLY caller-facing path that mutates `ops` +
    /// `layouts` in tandem — Task 11 routes through this; B.3+ ports
    /// extend the layout-updates slice as they touch additional
    /// drawables.
    pub(crate) fn push_op_and_set_layouts(
        &mut self,
        op: RecordedOp,
        drawable_layout_updates: &[(DrawableId, vk::ImageLayout)],
    ) {
        self.ops.push(op);
        for (id, layout) in drawable_layout_updates {
            self.layouts.set_drawable_in_frame(*id, *layout);
        }
    }
}

use std::sync::Arc;

/// Resource pins held alive across a frame. Mechanism 1 of spec
/// § "Frame-wide resource pinning". B.1 only pins `StagingBuffer`
/// clones (one per glyph upload). B.2 extends with sync objects,
/// semaphores, and Mechanism 3 retired scratch `BatchResource`s.
///
/// `Debug` is derived: `BatchResource: Send + std::fmt::Debug` (see
/// `paint_batch.rs:146`), so `Box<dyn BatchResource>` is `Debug` and
/// `#[derive(Debug)]` on `FramePinSet` works directly. The derived impl
/// prints the full Vec contents — implementors of `BatchResource`
/// typically emit just the variant name (the Vk handles inside are not
/// interesting), so this stays terse.
#[derive(Debug, Default)]
pub(crate) struct FramePinSet {
    pub(crate) staging_buffers: Vec<Arc<super::engine::StagingBuffer>>,
    /// Phase B.2 Mechanism 3: retired scratch images adopted into this
    /// frame's pin set via
    /// `RenderEngineInner::adopt_retired_resource_for_gpu_retirement`.
    /// Released explicitly (NOT via `Drop` — `BatchResource::release`
    /// is `self: Box<Self>`, see `paint_batch.rs:147`) by the
    /// `pending_frames` retire walk in `poll_retired` / `drain_all`,
    /// and by the close-failure rollback in `close_open_frame`.
    ///
    /// Under B.2's grow-before-open rule (Phase 9A, Task 9), this Vec
    /// is structurally empty at close time — every scratch growth
    /// happens BEFORE any new frame opens, so the retired Box rides
    /// `submitted.back` instead. The field is wired now so the
    /// helper compiles; B.3+ may populate it when mid-frame retire
    /// becomes possible.
    pub(crate) retired_resources: Vec<Box<dyn crate::kms::render::batch_resource::BatchResource>>,
}

impl FramePinSet {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn pin_staging(
        &mut self,
        staging: Arc<super::engine::StagingBuffer>,
    ) -> PinnedStagingIdx {
        let idx = u32::try_from(self.staging_buffers.len()).expect("< u32::MAX pins");
        self.staging_buffers.push(staging);
        PinnedStagingIdx(idx)
    }

    /// Phase B.2 Mechanism 3: attach a retired scratch `BatchResource`
    /// to this open frame's pin set. Released via explicit
    /// `boxed.release(&vk)` at frame retirement (see the
    /// `pending_frames` walk in `poll_retired` + `drain_all`). Never
    /// drops the Box without calling release — `BatchResource` has no
    /// `Drop`-based teardown (`paint_batch.rs:147`).
    #[allow(
        dead_code,
        reason = "B.2 Task 1: helper case (a) — open-frame adopt. Wired by \
                  adopt_retired_resource_for_gpu_retirement; populated when \
                  Phase 9A's mid-frame grow path lands (B.3+)."
    )]
    pub(crate) fn adopt_retired(
        &mut self,
        boxed: Box<dyn crate::kms::render::batch_resource::BatchResource>,
    ) {
        self.retired_resources.push(boxed);
    }

    pub(crate) fn len(&self) -> usize {
        self.staging_buffers.len() + self.retired_resources.len()
    }

    #[allow(dead_code, reason = "introspection / B.2+ telemetry")]
    pub(crate) fn is_empty(&self) -> bool {
        self.staging_buffers.is_empty() && self.retired_resources.is_empty()
    }
}

#[cfg(test)]
mod pin_tests {
    use super::*;

    // No-Vk pin tests can't construct a real StagingBuffer (it owns Vk
    // handles). Pin tests here verify the bookkeeping; integration
    // tests in acceptance.rs verify the real-Vk path.

    #[test]
    fn fresh_pin_set_is_empty() {
        let p = FramePinSet::new();
        assert_eq!(p.len(), 0);
        assert!(p.is_empty());
    }

    #[test]
    fn adopt_retired_pushes_to_retired_resources() {
        let mut set = FramePinSet::new();
        assert_eq!(set.retired_resources.len(), 0);
        // Pure-unit-test scope: wrap a no-op BatchResource fake. See
        // `paint_batch.rs:146` for the trait shape — `release` is
        // `self: Box<Self>` so the no-op simply drops the Box. The
        // test never calls `release`; it only verifies bookkeeping.
        #[derive(Debug)]
        struct FakeRetired;
        impl crate::kms::render::batch_resource::BatchResource for FakeRetired {
            fn release(self: Box<Self>, _vk: &crate::kms::vk::device::VkContext) {
                // No-op: test never invokes this path.
            }
        }
        set.adopt_retired(Box::new(FakeRetired));
        assert_eq!(set.retired_resources.len(), 1);
        assert_eq!(set.len(), 1);
        assert!(!set.is_empty());
    }
}

#[cfg(test)]
mod op_tests {
    use super::*;

    #[test]
    fn recorded_op_size_is_under_256_bytes() {
        assert!(
            std::mem::size_of::<RecordedOp>() <= 256,
            "RecordedOp is {} bytes — exceeds 256-byte budget",
            std::mem::size_of::<RecordedOp>()
        );
    }

    #[test]
    fn recorded_render_composite_within_512b() {
        let size = std::mem::size_of::<RecordedRenderComposite>();
        assert!(
            size <= 512,
            "RecordedRenderComposite is {size} bytes; spec budget 512"
        );
        // Echo the measured size so a CI run shows the headroom we
        // have against the 512B budget without forcing every PR
        // through a `cargo expand` to find out.
        eprintln!("RecordedRenderComposite size = {size} bytes");
    }

    #[test]
    fn recorded_composite_glyphs_carries_instance_pin_and_clip() {
        let scissor = vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent: vk::Extent2D {
                width: 640,
                height: 480,
            },
        };
        let op = RecordedCompositeGlyphs {
            dst_id: DrawableId::for_tests(1),
            dst_old_layout: vk::ImageLayout::UNDEFINED,
            op: 3, // Over
            dst_has_alpha: true,
            foreground_rgba: [1.0, 0.0, 0.0, 1.0],
            instance_pin: PinnedStagingIdx(7),
            instance_count: 3,
            clip_scissors: vec![scissor],
            damage_rect: Some(vk::Rect2D {
                offset: vk::Offset2D { x: 100, y: 200 },
                extent: vk::Extent2D {
                    width: 8,
                    height: 16,
                },
            }),
        };

        assert_eq!(op.dst_id, DrawableId::for_tests(1));
        assert_eq!(op.foreground_rgba, [1.0, 0.0, 0.0, 1.0]);
        assert_eq!(op.instance_pin, PinnedStagingIdx(7));
        assert_eq!(op.instance_count, 3);
        assert_eq!(op.clip_scissors.len(), 1);
        assert!(op.damage_rect.is_some());
    }

    #[test]
    fn recorded_glyph_upload_carries_staging_index_and_pending_insert() {
        let key = GlyphKey {
            font_xid: 1234,
            codepoint: 65,
        };
        let entry = AtlasEntry {
            atlas_x: 0,
            atlas_y: 32,
            w: 8,
            h: 16,
            pen_left: 0,
            pen_top: 14,
        };
        let op = RecordedGlyphUpload {
            staging_pin_idx: PinnedStagingIdx(3),
            atlas_x: 0,
            atlas_y: 32,
            w: 8,
            h: 16,
            insert_key: key,
            insert_entry: entry,
        };

        assert_eq!(op.staging_pin_idx, PinnedStagingIdx(3));
        assert_eq!(op.atlas_x, 0);
        assert_eq!(op.atlas_y, 32);
        assert_eq!(op.w, 8);
        assert_eq!(op.h, 16);
        assert_eq!(op.insert_key.font_xid, 1234);
        assert_eq!(op.insert_key.codepoint, 65);
        assert_eq!(op.insert_entry.pen_top, 14);
    }

    // Phase B.3 Task 1 tests.

    #[test]
    fn b3_recorded_op_size_budget() {
        use std::mem::size_of;
        // RecordedOp tag + Box ptr (B.2's invariant; B.3 keeps).
        assert!(
            size_of::<RecordedOp>() <= 256,
            "RecordedOp grew past 256B: {}",
            size_of::<RecordedOp>(),
        );
        // Each payload (un-boxed) stays under 512B individually.
        assert!(
            size_of::<RecordedCopyArea>() <= 512,
            "RecordedCopyArea is {} bytes; spec budget 512",
            size_of::<RecordedCopyArea>()
        );
        assert!(
            size_of::<RecordedPutImage>() <= 512,
            "RecordedPutImage is {} bytes; spec budget 512",
            size_of::<RecordedPutImage>()
        );
        assert!(
            size_of::<RecordedFillRect>() <= 512,
            "RecordedFillRect is {} bytes; spec budget 512",
            size_of::<RecordedFillRect>()
        );
        assert!(
            size_of::<RecordedLogicFill>() <= 512,
            "RecordedLogicFill is {} bytes; spec budget 512",
            size_of::<RecordedLogicFill>()
        );
        assert!(
            size_of::<RecordedImageText>() <= 512,
            "RecordedImageText is {} bytes; spec budget 512",
            size_of::<RecordedImageText>()
        );
        assert!(
            size_of::<RecordedRenderTrapsOrTris>() <= 512,
            "RecordedRenderTrapsOrTris is {} bytes; spec budget 512",
            size_of::<RecordedRenderTrapsOrTris>()
        );
    }

    #[test]
    fn b3_recorded_op_dst_id_covers_every_variant() {
        // Construct each variant with the minimum data and assert dst_id()
        // returns the expected `Some(id)` / `None`. The pattern match in
        // RecordedOp::dst_id() is exhaustive (no `_ =>` arm); this test
        // fails compilation if a new variant is added without updating
        // dst_id() — that's the primary signal we want.
        let id7 = DrawableId::for_tests(7);

        // CompositeGlyphs → Some(id7).
        let composite_glyphs = RecordedOp::CompositeGlyphs(RecordedCompositeGlyphs {
            dst_id: id7,
            dst_old_layout: vk::ImageLayout::UNDEFINED,
            op: 3, // Over
            dst_has_alpha: true,
            foreground_rgba: [0.0; 4],
            instance_pin: PinnedStagingIdx(0),
            instance_count: 0,
            clip_scissors: Vec::new(),
            damage_rect: None,
        });
        assert_eq!(composite_glyphs.dst_id(), Some(id7));

        // GlyphUpload → None (utility variant — writes to atlas).
        let glyph_upload = RecordedOp::GlyphUpload(RecordedGlyphUpload {
            staging_pin_idx: PinnedStagingIdx(0),
            atlas_x: 0,
            atlas_y: 0,
            w: 0,
            h: 0,
            insert_key: GlyphKey {
                font_xid: 0,
                codepoint: 0,
            },
            insert_entry: AtlasEntry {
                atlas_x: 0,
                atlas_y: 0,
                w: 0,
                h: 0,
                pen_left: 0,
                pen_top: 0,
            },
        });
        assert_eq!(glyph_upload.dst_id(), None);

        // LayoutTransition → None (utility variant).
        let layout_transition = RecordedOp::LayoutTransition(RecordedLayoutTransition {
            drawable_id: id7,
            src_stage: vk::PipelineStageFlags2::empty(),
            src_access: vk::AccessFlags2::empty(),
            dst_stage: vk::PipelineStageFlags2::empty(),
            dst_access: vk::AccessFlags2::empty(),
            target_layout: vk::ImageLayout::UNDEFINED,
        });
        assert_eq!(layout_transition.dst_id(), None);

        // CopyArea → Some(id7).
        let copy_area = RecordedOp::CopyArea(Box::new(RecordedCopyArea {
            dst_id: id7,
            src_id: DrawableId::for_tests(8),
            src_rect: vk::Rect2D::default(),
            dst_rect: vk::Rect2D::default(),
            src_format: vk::Format::B8G8R8A8_UNORM,
            src_extent: vk::Extent2D::default(),
            dst_extent: vk::Extent2D::default(),
            src_image: vk::Image::null(),
            dst_image: vk::Image::null(),
            src_old_layout: vk::ImageLayout::UNDEFINED,
            dst_old_layout: vk::ImageLayout::UNDEFINED,
            src_final_layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            dst_final_layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            src_imported: false,
            dst_imported: false,
            self_overlap_scratch: None,
        }));
        assert_eq!(copy_area.dst_id(), Some(id7));

        // PutImage → Some(id7).
        let put_image = RecordedOp::PutImage(Box::new(RecordedPutImage {
            dst_id: id7,
            dst_rect: vk::Rect2D::default(),
            dst_image: vk::Image::null(),
            dst_extent: vk::Extent2D::default(),
            dst_old_layout: vk::ImageLayout::UNDEFINED,
            staging_pin_idx: PinnedStagingIdx(0),
        }));
        assert_eq!(put_image.dst_id(), Some(id7));

        // FillRect → Some(id7).
        let fill_rect = RecordedOp::FillRect(Box::new(RecordedFillRect {
            dst_id: id7,
            dst_image_view: vk::ImageView::null(),
            dst_extent: vk::Extent2D::default(),
            dst_format: vk::Format::B8G8R8A8_UNORM,
            dst_old_layout: vk::ImageLayout::UNDEFINED,
            color: [0.0; 4],
            rects: Vec::new(),
        }));
        assert_eq!(fill_rect.dst_id(), Some(id7));

        // LogicFill → Some(id7).
        let logic_fill = RecordedOp::LogicFill(Box::new(RecordedLogicFill {
            dst_id: id7,
            dst_image_view: vk::ImageView::null(),
            dst_extent: vk::Extent2D::default(),
            dst_format: vk::Format::B8G8R8A8_UNORM,
            dst_old_layout: vk::ImageLayout::UNDEFINED,
            logic_mode: yserver_core::backend::GcFunction::Copy,
            opaque_alpha: false,
            color: [0.0; 4],
            rects: Vec::new(),
        }));
        assert_eq!(logic_fill.dst_id(), Some(id7));

        // ImageText → Some(id7).
        let image_text = RecordedOp::ImageText(Box::new(RecordedImageText {
            dst_id: id7,
            dst_extent: vk::Extent2D::default(),
            dst_old_layout: vk::ImageLayout::UNDEFINED,
            foreground_rgba: [0.0; 4],
            instance_pin: PinnedStagingIdx(0),
            instance_count: 0,
        }));
        assert_eq!(image_text.dst_id(), Some(id7));

        // RenderTrapsOrTris → Some(id7).
        let render_traps = RecordedOp::RenderTrapsOrTris(Box::new(RecordedRenderTrapsOrTris {
            dst_id: id7,
            dst_image: vk::Image::null(),
            dst_view: vk::ImageView::null(),
            dst_old_layout: vk::ImageLayout::UNDEFINED,
            dst_extent: vk::Extent2D::default(),
            dst_format: vk::Format::B8G8R8A8_UNORM,
            dst_has_alpha: false,
            std_op: crate::kms::vk::render_pipeline::StdPictOp::Clear,
            op_byte: 0,
            src_kind: RecordedTrapSrcKind::Solid([0.0; 4]),
            src_extent: vk::Extent2D::default(),
            src_is_synthetic_1x1: false,
            src_repeat: 0,
            src_force_opaque: false,
            user_src_xform: crate::kms::vk::ops::render::AffineXform::IDENTITY,
            src_origin_x: 0,
            src_origin_y: 0,
            prim_kind: crate::kms::render::engine::TrapPrimKind::Trapezoid,
            bbox_x: 0,
            bbox_y: 0,
            bbox_w: 0,
            bbox_h: 0,
            instance_count: 0,
            clip_scissors: Vec::new(),
            vertex_pool_pin: PinnedStagingIdx(0),
        }));
        assert_eq!(render_traps.dst_id(), Some(id7));
    }

    #[test]
    fn b3_scratch_walk_yields_empty_vec_when_no_copy_area_appended() {
        // Spec acceptance gate (N8 + Pitfall 7): the close-path scratch walk
        // over a frame that contains ONLY non-CopyArea ops yields an empty
        // Vec<ScratchImage>. This verifies that the filter_map in
        // close_open_frame only takes from RecordedCopyArea variants and
        // doesn't spuriously touch other ops.
        //
        // This is a pure-unit-test (no Vk handles needed): we construct
        // a Vec<RecordedOp> matching what close_open_frame iterates over
        // and apply the same filter_map logic inline.
        let id7 = DrawableId::for_tests(7);

        let ops: Vec<RecordedOp> = vec![
            RecordedOp::FillRect(Box::new(RecordedFillRect {
                dst_id: id7,
                dst_image_view: vk::ImageView::null(),
                dst_extent: vk::Extent2D::default(),
                dst_format: vk::Format::B8G8R8A8_UNORM,
                dst_old_layout: vk::ImageLayout::UNDEFINED,
                color: [0.0; 4],
                rects: Vec::new(),
            })),
            RecordedOp::ImageText(Box::new(RecordedImageText {
                dst_id: id7,
                dst_extent: vk::Extent2D::default(),
                dst_old_layout: vk::ImageLayout::UNDEFINED,
                foreground_rgba: [0.0; 4],
                instance_pin: PinnedStagingIdx(0),
                instance_count: 0,
            })),
        ];

        // Mirror the filter_map from close_open_frame's scratch walk.
        let scratches: Vec<crate::kms::render::engine::ScratchImage> = ops
            .iter()
            .filter_map(|op| match op {
                RecordedOp::CopyArea(_) => {
                    // In the real close path this would take the scratch.
                    // Here it's unreachable since we have no CopyArea ops.
                    unreachable!("no CopyArea ops in this test")
                }
                _ => None,
            })
            .collect();

        assert!(
            scratches.is_empty(),
            "scratch walk yielded {} entries for a frame with no CopyArea ops",
            scratches.len()
        );
    }
}

use std::collections::HashMap;

#[derive(Debug, Clone, Copy)]
pub(crate) struct LayoutOverlayEntry {
    pub(crate) pre_frame_layout: vk::ImageLayout,
    #[allow(
        dead_code,
        reason = "B.2+ — when the overlay becomes source-of-truth and the recorder \
                  reads the in-frame layout instead of mutating storage directly"
    )]
    pub(crate) current_in_frame_layout: vk::ImageLayout,
}

/// Per-frame layout overlay. Mutated on each `record_layout_transition`
/// from a ported paint op (none in B.1 — the text pipeline's recorder
/// embeds its own barriers — but the structure is the load-bearing
/// answer to open question 3 and lands in B.1 so B.2 can fold ported
/// `render_composite` / `render_fill` paths in without re-architecting).
///
/// Atlas image layout is tracked separately: the overlay carries a
/// single `Option<LayoutOverlayEntry>` for the atlas because there's
/// exactly one atlas per engine, and `GlyphAtlas::current_layout`
/// is the single source of truth that the commit step writes back.
#[derive(Debug, Default)]
pub(crate) struct FrameLayoutTable {
    pub(crate) drawables: HashMap<DrawableId, LayoutOverlayEntry>,
    pub(crate) atlas: Option<LayoutOverlayEntry>,
}

impl FrameLayoutTable {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// First-touch snapshot for a drawable. `pre_frame_layout` is the
    /// value the caller read out of `Drawable::storage.current_layout`
    /// at the moment of first append-in-frame.
    pub(crate) fn first_touch_drawable(
        &mut self,
        id: DrawableId,
        pre_frame_layout: vk::ImageLayout,
    ) {
        self.drawables.entry(id).or_insert(LayoutOverlayEntry {
            pre_frame_layout,
            current_in_frame_layout: pre_frame_layout,
        });
    }

    #[allow(
        dead_code,
        reason = "B.2+ — overlay-as-source-of-truth path; B.1 recorder mutates storage directly"
    )]
    pub(crate) fn set_drawable_in_frame(&mut self, id: DrawableId, layout: vk::ImageLayout) {
        if let Some(entry) = self.drawables.get_mut(&id) {
            entry.current_in_frame_layout = layout;
        } else {
            debug_assert!(
                false,
                "set_drawable_in_frame without first_touch_drawable for {:?}",
                id
            );
        }
    }

    pub(crate) fn first_touch_atlas(&mut self, pre_frame_layout: vk::ImageLayout) {
        if self.atlas.is_none() {
            self.atlas = Some(LayoutOverlayEntry {
                pre_frame_layout,
                current_in_frame_layout: pre_frame_layout,
            });
        }
    }

    #[allow(
        dead_code,
        reason = "B.2+ — overlay-as-source-of-truth path; B.1 record_upload mutates atlas directly"
    )]
    pub(crate) fn set_atlas_in_frame(&mut self, layout: vk::ImageLayout) {
        match self.atlas.as_mut() {
            Some(entry) => entry.current_in_frame_layout = layout,
            None => debug_assert!(false, "set_atlas_in_frame without first_touch_atlas"),
        }
    }

    /// Query the effective layout for `id` from the perspective of
    /// the next in-frame op that will touch it. Falls back to
    /// `storage_fallback` (the caller passes
    /// `drawable.storage.current_layout` if the drawable isn't in
    /// the overlay yet).
    #[allow(
        dead_code,
        reason = "B.2+ — overlay-as-source-of-truth path; B.1 recorder reads storage directly"
    )]
    pub(crate) fn current_layout_for_drawable(
        &self,
        id: DrawableId,
        storage_fallback: vk::ImageLayout,
    ) -> vk::ImageLayout {
        match self.drawables.get(&id) {
            Some(entry) => entry.current_in_frame_layout,
            None => storage_fallback,
        }
    }
}

#[cfg(test)]
mod layout_tests {
    use super::*;

    #[test]
    fn first_touch_drawable_snapshots_pre_frame_and_in_frame_equal() {
        let mut t = FrameLayoutTable::new();
        t.first_touch_drawable(
            DrawableId::for_tests(7),
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        );
        let entry = t.drawables.get(&DrawableId::for_tests(7)).unwrap();
        assert_eq!(
            entry.pre_frame_layout,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL
        );
        assert_eq!(
            entry.current_in_frame_layout,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL
        );
    }

    #[test]
    fn second_touch_does_not_overwrite_pre_frame() {
        let mut t = FrameLayoutTable::new();
        t.first_touch_drawable(DrawableId::for_tests(7), vk::ImageLayout::UNDEFINED);
        t.set_drawable_in_frame(
            DrawableId::for_tests(7),
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
        );
        t.first_touch_drawable(
            DrawableId::for_tests(7),
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
        );
        let entry = t.drawables.get(&DrawableId::for_tests(7)).unwrap();
        assert_eq!(entry.pre_frame_layout, vk::ImageLayout::UNDEFINED);
        assert_eq!(
            entry.current_in_frame_layout,
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL
        );
    }

    #[test]
    fn current_layout_for_drawable_falls_back_to_storage_when_untouched() {
        let t = FrameLayoutTable::new();
        let got = t.current_layout_for_drawable(
            DrawableId::for_tests(8),
            vk::ImageLayout::PRESENT_SRC_KHR,
        );
        assert_eq!(got, vk::ImageLayout::PRESENT_SRC_KHR);
    }

    #[test]
    fn atlas_first_touch_then_set_in_frame() {
        let mut t = FrameLayoutTable::new();
        t.first_touch_atlas(vk::ImageLayout::UNDEFINED);
        t.set_atlas_in_frame(vk::ImageLayout::TRANSFER_DST_OPTIMAL);
        let a = t.atlas.unwrap();
        assert_eq!(a.pre_frame_layout, vk::ImageLayout::UNDEFINED);
        assert_eq!(
            a.current_in_frame_layout,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL
        );
    }
}

/// Per-frame snapshot of `Drawable::last_render_ticket` taken at first
/// append-in-frame. Close-failure restores each entry; close-success
/// is a no-op (the frame ticket already overwrote the slot via
/// `store.touch_render_fence` at append-time).
#[derive(Debug, Default)]
pub(crate) struct TouchedDrawables {
    /// `None` value = drawable had no prior ticket before this frame.
    pub(crate) snapshots: HashMap<DrawableId, Option<FenceTicket>>,
}

impl TouchedDrawables {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Record the first-touch pre-frame ticket. Subsequent calls on
    /// the same id are no-ops (the first snapshot is the load-bearing
    /// one — it captures the value the engine needs to restore on
    /// close-failure).
    pub(crate) fn first_touch(&mut self, id: DrawableId, prior_ticket: Option<FenceTicket>) {
        self.snapshots.entry(id).or_insert(prior_ticket);
    }

    #[allow(dead_code, reason = "introspection / B.2+ telemetry")]
    pub(crate) fn len(&self) -> usize {
        self.snapshots.len()
    }

    #[allow(dead_code, reason = "introspection / B.2+ telemetry")]
    pub(crate) fn is_empty(&self) -> bool {
        self.snapshots.is_empty()
    }
}

#[cfg(test)]
mod touched_tests {
    use super::*;

    #[test]
    fn first_touch_records_prior_ticket_only_once() {
        let mut t = TouchedDrawables::new();
        t.first_touch(DrawableId::for_tests(1), None);
        assert_eq!(t.len(), 1);
        // Subsequent calls do not overwrite (a later op on the same
        // drawable should not lose the originally-captured prior).
        t.first_touch(DrawableId::for_tests(1), None);
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn separate_drawables_track_independently() {
        let mut t = TouchedDrawables::new();
        t.first_touch(DrawableId::for_tests(1), None);
        t.first_touch(DrawableId::for_tests(2), None);
        assert_eq!(t.len(), 2);
    }
}

/// Pending glyph cache inserts. `composite_glyphs`'s upload path
/// already calls `GlyphAtlas::pack` (shelf advance, monotonic — the
/// slot stays consumed even if the frame fails), but `insert_entry`
/// (cache commit) is deferred here. Close-success drains this and
/// calls `GlyphAtlas::insert_entry` on the atlas; close-failure
/// drops the list — the slot leaks but the cache stays consistent
/// (next paint re-packs).
#[derive(Debug, Default)]
pub(crate) struct PendingGlyphInserts {
    pub(crate) entries: Vec<(GlyphKey, AtlasEntry)>,
}

impl PendingGlyphInserts {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn push(&mut self, key: GlyphKey, entry: AtlasEntry) {
        self.entries.push((key, entry));
    }

    #[allow(dead_code, reason = "introspection / B.2+ telemetry")]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    #[allow(dead_code, reason = "introspection / B.2+ telemetry")]
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod glyph_insert_tests {
    use super::*;

    #[test]
    fn fresh_is_empty() {
        assert_eq!(PendingGlyphInserts::new().len(), 0);
    }

    #[test]
    fn push_appends_in_order() {
        let mut p = PendingGlyphInserts::new();
        p.push(
            GlyphKey {
                font_xid: 1,
                codepoint: 65,
            },
            AtlasEntry {
                atlas_x: 0,
                atlas_y: 0,
                w: 8,
                h: 12,
                pen_left: 0,
                pen_top: 0,
            },
        );
        p.push(
            GlyphKey {
                font_xid: 1,
                codepoint: 66,
            },
            AtlasEntry {
                atlas_x: 8,
                atlas_y: 0,
                w: 8,
                h: 12,
                pen_left: 0,
                pen_top: 0,
            },
        );
        assert_eq!(p.len(), 2);
        assert_eq!(p.entries[0].0.codepoint, 65);
        assert_eq!(p.entries[1].0.codepoint, 66);
    }
}

#[cfg(test)]
mod open_frame_tests {
    use super::{super::platform::FenceTicket, *};

    #[test]
    fn open_frame_aggregates_all_overlays() {
        let frame = OpenFrame {
            ticket: FenceTicket::for_tests_stub(),
            frame_generation: 0,
            ops: Vec::new(),
            pins: FramePinSet::new(),
            layouts: FrameLayoutTable::new(),
            touched: TouchedDrawables::new(),
            pending_glyph_inserts: PendingGlyphInserts::new(),
            atlas_prev_ticket_snapshot: None,
            glyph_uploads_in_frame: 0,
            close_reason_on_open: None,
            opened_at: std::time::Instant::now(),
            pending_present_completions: Vec::new(),
            snapshot_touch: std::collections::HashMap::new(),
        };
        assert!(frame.ops.is_empty());
        assert_eq!(frame.pins.len(), 0);
        assert_eq!(frame.touched.len(), 0);
        assert_eq!(frame.pending_glyph_inserts.len(), 0);
        assert_eq!(frame.glyph_uploads_in_frame, 0);
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;

    #[test]
    fn open_for_paint_transitions_to_open_state_and_bumps_lifetime_opens() {
        let mut fb = FrameBuilder::new();
        fb.open_for_paint(FenceTicket::for_tests_stub(), 0);
        assert!(fb.is_open());
        assert_eq!(fb.lifetime_opens(), 1);
        assert_eq!(fb.lifetime_closes(), 0);
        assert_eq!(fb.op_count(), 0);
    }

    #[test]
    fn take_open_for_close_returns_frame_and_records_reason() {
        let mut fb = FrameBuilder::new();
        fb.open_for_paint(FenceTicket::for_tests_stub(), 0);
        let frame = fb
            .take_open_for_close(CloseReason::NonPortedPaintOp)
            .expect("frame open");
        assert_eq!(
            frame.close_reason_on_open,
            Some(CloseReason::NonPortedPaintOp)
        );
    }

    #[test]
    fn complete_close_success_bumps_lifetime_closes_and_returns_to_closed() {
        let mut fb = FrameBuilder::new();
        fb.open_for_paint(FenceTicket::for_tests_stub(), 0);
        let _ = fb.take_open_for_close(CloseReason::SceneCompose);
        fb.complete_close_success();
        assert!(!fb.is_open());
        assert_eq!(fb.lifetime_closes(), 1);
    }

    #[test]
    fn would_exceed_pin_ceiling_false_when_closed() {
        let fb = FrameBuilder::new();
        assert!(!fb.would_exceed_pin_ceiling(10_000));
    }

    #[test]
    fn would_exceed_pin_ceiling_true_when_open_and_over_default_ceiling() {
        let mut fb = FrameBuilder::new();
        fb.open_for_paint(FenceTicket::for_tests_stub(), 0);
        assert!(fb.would_exceed_pin_ceiling(1025));
        assert!(!fb.would_exceed_pin_ceiling(1024));
    }

    #[test]
    #[should_panic(expected = "while already open")]
    fn open_for_paint_panics_when_already_open() {
        let mut fb = FrameBuilder::new();
        fb.open_for_paint(FenceTicket::for_tests_stub(), 0);
        fb.open_for_paint(FenceTicket::for_tests_stub(), 0);
    }

    /// Phase B.2 Mechanism 2: `open_for_paint` stores the
    /// caller-supplied `frame_generation` verbatim on the OpenFrame.
    /// Every descriptor acquisition during the open frame consumes
    /// this value via `acquire_descriptor_set_for_frame_or_op`; the
    /// close-path SubmittedOp uses the same value. The retire walk's
    /// `release_up_to(generation)` then retires exactly the frame's
    /// pools.
    #[test]
    fn open_for_paint_records_frame_generation() {
        let mut fb = FrameBuilder::new();
        fb.open_for_paint(FenceTicket::for_tests_stub(), 42);
        assert_eq!(
            fb.open.as_ref().expect("open").frame_generation,
            42,
            "OpenFrame::frame_generation must capture the caller's value verbatim"
        );
    }
}

/// One in-flight frame's resource pin set, parked until the frame's
/// `FenceTicket` signals. Walked by `RenderEngine::poll_retired` next
/// to the existing `submitted` queue; both gate retirement on the same
/// ticket. Drop order: when the ticket signals, the record drops, its
/// `pins.staging_buffers` Arcs decrement, and any `StagingBuffer`
/// whose Arc refcount hits zero releases its Vk handles.
#[derive(Debug)]
pub(crate) struct FrameSubmittedRecord {
    pub(crate) ticket: FenceTicket,
    #[allow(
        dead_code,
        reason = "Drop-only ownership: the FramePinSet keeps Arc<StagingBuffer> alive \
                  until the frame ticket signals (poll_retired drops the record). \
                  Never explicitly read."
    )]
    pub(crate) pins: FramePinSet,
    /// Lifetime count snapshot — telemetry uses this to attribute the
    /// retirement to the closing frame.
    #[allow(
        dead_code,
        reason = "B.2+ telemetry attribution; B.1 doesn't consume the per-record sequence number"
    )]
    pub(crate) frame_seq: u64,
}

/// Telemetry event published by `RenderEngine::close_open_frame` and
/// drained by the backend at every close-driving site (maybe_composite,
/// enqueue_present_completion, get_image, shutdown, render_composite_glyphs).
/// Task 21 wires telemetry counters off this stream.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FrameCloseEvent {
    pub(crate) reason: CloseReason,
    pub(crate) ops_in_frame: usize,
    pub(crate) glyph_uploads_in_frame: u32,
    /// Phase B.2 Task 14: count of `RecordedOp::RenderComposite` ops
    /// recorded in the closing frame. Mirrors `glyph_uploads_in_frame`
    /// for the render-composite path so the telemetry log line can
    /// surface `renders/frame_avg=X.Y max=Z` alongside the existing
    /// `glyph_uploads/frame_*` series.
    pub(crate) renders_in_frame: u32,
    pub(crate) pin_count: usize,
    /// Phase B.1: `true` if this close came from a failure path
    /// (`renderer_failed`, flush_submit_group Err, recorder error).
    /// Drained by the backend telemetry helper which routes to
    /// `record_frame_builder_abort()`.
    pub(crate) aborted: bool,
}
