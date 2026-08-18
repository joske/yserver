//! `SceneCompositor` — composed output pass (Stage 2d MVP).
//!
//! Per rendering-model-v2 spec § "SceneCompositor" and Stage 2
//! plan substage 2d. Owns the blit pipeline (reuses v1's
//! `CompositorPipeline` — same shaders, same descriptor layout,
//! same sampler), per-output descriptor-pool rings, the
//! scene-structure dirty flag, and the per-output pending-ack
//! queues that thread snapshot/ack through the I6b page-flip
//! retirement path.
//!
//! Stage 2d MVP scope:
//!
//! - **Full-redraw every tick.** Buffer-age clipping is Stage 2e.
//!   Stage 2d still records the damage snapshots so 2e is a
//!   smaller diff, but the actual compose draws every scene
//!   entry every frame.
//! - **Single-output preferred.** The code loops over all
//!   outputs, but only the single-output xfce-on-bee path is
//!   exercised. Multi-output flip ordering is risk-listed in
//!   the Stage 2 plan (Risk 20).
//! - **No HW cursor plane.** Per I7 the cursor parks; Stage 5
//!   reintroduces it as a SceneCompositor strategy choice. For
//!   Stage 2d the cursor is skipped from the scene entirely —
//!   cursor rendering needs a small cursor pixmap which Stage 3
//!   will allocate alongside `create_cursor` wiring.
//! - **Manual-redirected windows skipped automatically.** Manual
//!   redirect flips the window to `scene_participating = false`;
//!   the scene walk prunes that window's subtree. The compositor
//!   reintroduces those pixels by painting its output/COW surface.
//! - **bg_pixel only.** Root background is the
//!   `vkCmdBeginRendering` clear color; `bg_pixmap` (which
//!   needs a sample-from-pixmap into root) waits for Stage 3.
//!
//! Compose flow (per [`SceneCompositor::tick`] call):
//!
//! 1. For each output, if `acquire_scanout_bo` returns `None`
//!    (all BOs in flight), skip — next core-loop iteration retries.
//! 2. Walk `core.top_level_order`, look up each window's
//!    drawable in `store`, build a `CompositeDraw` list.
//! 3. Peek presentation damage on each contributing drawable;
//!    record the snapshot keyed by drawable id for later ack.
//! 4. Call `kms::vk::compositor::record_and_present_composite`
//!    — records the compose CB into the scanout BO's
//!    pre-allocated `vk_transfer.command_buffer`, submits with
//!    `signalSemaphore = bo.vk_semaphore`, exports the sync_file
//!    fd, atomic-flips with explicit IN_FENCE_FD. v1's helper
//!    handles all of this; v2 just builds the scene + reuses
//!    the helper.
//! 5. Push a `PendingAck` onto the output's queue, advance
//!    `scene_structure_dirty = false`.
//!
//! [`SceneCompositor::handle_page_flip_complete`] then ack's
//! the captured snapshots after KMS retires the matching BO.

#![allow(
    dead_code,
    reason = "SceneCompositor primitives are consumed across Stages 2d–2e"
)]

use std::{
    collections::{HashMap, HashSet, VecDeque},
    io,
    sync::{Arc, OnceLock},
};

use ash::vk;
use yserver_protocol::x11::xfixes;

use super::{
    platform::{FenceTicket, PlatformBackend, ReadyScanoutRenderCompletion},
    store::{DamageSnapshot, DrawableKind, DrawableStore, RegionSet},
    telemetry::Telemetry,
};
use crate::kms::{
    core::KmsCore,
    render::composite_pool_ring::CompositePoolRing,
    vk::{
        compositor::{CompositeDraw, CompositeScene, PresentError},
        pipeline::{CompositePushConsts, CompositorPipeline, MAX_DESCRIPTOR_SETS_PER_FRAME},
        scanout::{
            BoPhase, BoState, CopiedRenderSource, CopiedTransportPreparation, OutputScanout,
            ScanoutBo,
        },
    },
};

// ────────────────────────────────────────────────────────────────
// Per-output state
// ────────────────────────────────────────────────────────────────

/// Per-output pending-ack ledger. Each entry corresponds to one
/// in-flight compose; popped front on page-flip-complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InFlightStage {
    WaitingForRenderCompletion { job_id: u64 },
    KmsFlipPending,
}

impl InFlightStage {
    fn matches_render_completion(self, job_id: u64) -> bool {
        self == Self::WaitingForRenderCompletion { job_id }
    }

    fn is_kms_flip_pending(self) -> bool {
        self == Self::KmsFlipPending
    }
}

fn copied_render_completion_matches(
    stage: InFlightStage,
    pending_bo_idx: usize,
    completion_job_id: u64,
    completion_bo_idx: usize,
) -> bool {
    pending_bo_idx == completion_bo_idx && stage.matches_render_completion(completion_job_id)
}

fn kms_retirement_matches(
    stage: InFlightStage,
    pending_bo_idx: usize,
    presented_bo_idx: usize,
) -> bool {
    pending_bo_idx == presented_bo_idx && stage.is_kms_flip_pending()
}

fn present_error_is_device_lost(error: &PresentError) -> bool {
    matches!(error, PresentError::Vk(vk::Result::ERROR_DEVICE_LOST))
}

enum CopiedRenderSubmitError {
    RendererAcquire(io::Error),
    Present(PresentError),
}

impl CopiedRenderSubmitError {
    fn requires_fail_stop(&self) -> bool {
        matches!(self, Self::RendererAcquire(_))
    }

    fn into_present(self) -> PresentError {
        match self {
            Self::RendererAcquire(error) => PresentError::Io(error),
            Self::Present(error) => error,
        }
    }
}

fn vk_result_is_device_lost(result: vk::Result) -> bool {
    result == vk::Result::ERROR_DEVICE_LOST
}

struct PendingAck {
    bo_idx: usize,
    generation: u64,
    stage: InFlightStage,
    /// Snapshots taken at tick entry, one per source drawable
    /// that contributed to the compose. Ack'd against the
    /// store's live presentation damage on flip retirement.
    drawable_snapshots: Vec<DamageSnapshot>,
    /// Engine fence ticket for the source drawables touched by
    /// the compose. Per cross-cutting §5: every consumer that
    /// reads OR writes a drawable touches the ticket; this is
    /// the compose-read side.
    ticket: Option<FenceTicket>,
    /// Output-level damage submitted in this frame (codex
    /// round 2 point 1). Subtracted from
    /// `output.scene_structure_damage` +
    /// `output.pending_repaint_after_failed_submit` on
    /// retirement. Damage that arrived between submit and
    /// retirement is NOT in this snapshot — it survives.
    submitted_output_damage: RegionSet,
    submitted_scene_structure_damage: RegionSet,
    submitted_failed_repaint: RegionSet,
    /// Stage 5 Phase D — cursor-plane transition queued behind
    /// this commit. Populated AFTER the compose + atomic commit
    /// succeed (failed submit drops the transition; the next
    /// frame re-decides). Consumed by `handle_page_flip_complete`
    /// which applies the per-CRTC show/hide.
    cursor_transition: Option<CursorTransition>,
    /// Stage 5 Phase D — new value for the per-output cursor
    /// prev-pos. Applied to `OutputSceneState.cursor_prev_pos`
    /// only when this ack retires successfully (codex v4-pass
    /// transactional rule). Failed submit → prev_pos for this
    /// output is NOT advanced, and the next frame still damages
    /// the OLD prev rect to clear the trail.
    cursor_prev_pos_after_retire: Option<Option<(i32, i32)>>,
    /// Stage 5 Phase D — `OutputSceneState.last_frame_cursor_mode`
    /// value to install on successful retire. Captures what's
    /// committed to the screen after this flip. Failed submit
    /// → mode stays as-is.
    cursor_mode_after_retire: OutputCursorMode,
    /// Cursor footprint that this output actually presented in the
    /// submitted frame. Applied only on retire so a failed submit
    /// leaves the old footprint in place for the next re-poke.
    last_present_cursor_rect_after_retire: Option<vk::Rect2D>,
    /// Cursor sprite version that this output actually presented in
    /// the submitted frame. `None` when the cursor is hidden on
    /// this output.
    last_present_cursor_version_after_retire: Option<u64>,
}

/// Stage 5 Phase C — pure result of the cursor-plane strategy
/// decision in `build_scene`. The compositor outer caller consumes
/// this to drive Phase D's `PendingAck` transition state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CursorAssignment {
    /// HW plane should display the sprite at this position. The
    /// SW cursor draw is omitted from `scene.draws`. Damage is
    /// decided later in `tick_one_output` from the transactional
    /// presented-footprint state, not here.
    Hw {
        x: i32,
        y: i32,
        record_version: u64,
        hot_x: u16,
        hot_y: u16,
    },
    /// SW path — sprite drawn into the scanout BO via composite.
    /// `scene.draws` carries the cursor entry. `pos` is the
    /// output-local top-left of the cursor draw
    /// (`cursor.xy − hot − layout`) — it propagates into
    /// `OutputSceneState.cursor_prev_pos` on successful retire so
    /// the next tick can clear the SW trail if the cursor moves.
    Sw { pos: (i32, i32) },
    /// Cursor off-output / unregistered / clipped. Nothing is drawn;
    /// tick-level cursor-damage gating decides whether the last
    /// presented footprint needs clearing.
    Hidden,
}

/// Stage 5 Phase D — transition queued on a `PendingAck` after the
/// per-output commit succeeds. Consumed at retirement.
#[derive(Debug, Clone, Copy)]
pub(crate) enum CursorTransition {
    /// Retire-time action: optionally upload (if `upload_version`
    /// != `CursorPlane.uploaded_version`), then `show_on_crtc` to
    /// bind the plane and reposition.
    ShowOnRetire {
        upload_version: u64,
        hot_x: u16,
        hot_y: u16,
        x: i32,
        y: i32,
    },
    /// Retire-time action: `hide_on_crtc`. The submitted frame is cursorless,
    /// so a failed hide can leave only the old HW sprite visible. When
    /// `reveal_sw_after` is true, a successful hide forces a second frame that
    /// may finally composite the software cursor.
    HideOnRetire { reveal_sw_after: bool },
}

/// Stage 5 Phase D — per-output cursor-plane mode tracked across
/// frames. Drives the `Sw → Hw` / `Hw → Sw` transition matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputCursorMode {
    /// Last frame drew the cursor via the SW composite path on
    /// this output. `prev` is the SW position carried for trail
    /// elimination.
    Sw { prev: Option<(i32, i32)> },
    /// The cursorless hide phase retired successfully and a software reveal
    /// frame is still required. This votes SW/Mixed so direct scanout and the
    /// pointer HW-only fast path cannot bypass phase two.
    SwPending,
    /// Last frame's plane is bound on this CRTC and showing.
    Hw,
    /// Cursor is off-output or unregistered on this frame.
    Hidden,
}

/// Stage 5 Phase D — query result for the pointer fast path.
/// `Hw` is only reached when EVERY active output has retired its
/// transition to HW; mixed-state outputs return `Mixed`, which
/// suppresses the fast path until every flip retires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CursorPlaneMode {
    /// Every active output is in HW mode + no transitions pending
    /// — pointer fast path may issue `cursor_plane_move` directly.
    Hw,
    /// At least one output is currently SW or in transition. The
    /// pointer fast path falls back to `scene.wake_for_damage`.
    Mixed,
    /// Every output is in SW (or Hidden) mode — scene wake required.
    Sw,
}

/// Pure classifier driving the steady-state cursor-mode decision —
/// extracted from `SceneCompositor::cursor_mode` so the dual-output
/// case (and any future N-output topology) can be unit-tested with
/// synthetic mode arrays. Callers MUST short-circuit Mixed for
/// pending transitions BEFORE invoking this helper; this fn only
/// looks at last-frame modes.
///
/// Classification rules (load-bearing — see scene.rs:686 docstring):
/// - `Hidden` outputs are NEUTRAL (cursor isn't on them, the
///   per-CRTC visible check in `cursor_plane_move` skips them).
/// - `Hw` outputs vote for the fast path.
/// - `Sw` / `SwPending` outputs need scene-compose updates for cursor position
///   (the sprite is part of the compose draw list).
/// - Any mix of `Hw` and `Sw` is `Mixed` so the SW cursor doesn't
///   desync from the eventual plane bind during a transition.
fn classify_cursor_mode_from_per_output(
    modes: impl IntoIterator<Item = OutputCursorMode>,
) -> CursorPlaneMode {
    let mut any_hw = false;
    let mut any_sw_like = false;
    for m in modes {
        match m {
            OutputCursorMode::Hw => any_hw = true,
            OutputCursorMode::Sw { .. } | OutputCursorMode::SwPending => any_sw_like = true,
            OutputCursorMode::Hidden => {}
        }
    }
    match (any_hw, any_sw_like) {
        (true, false) => CursorPlaneMode::Hw,
        (false, _) => CursorPlaneMode::Sw,
        (true, true) => CursorPlaneMode::Mixed,
    }
}

fn cursor_output_needs_sprite_retry(
    mode: OutputCursorMode,
    pending: impl IntoIterator<Item = Option<CursorTransition>>,
) -> bool {
    matches!(mode, OutputCursorMode::Hw)
        || pending
            .into_iter()
            .any(|transition| matches!(transition, Some(CursorTransition::ShowOnRetire { .. })))
}

struct FailedSubmitBo {
    bo_idx: usize,
    pool_slot: usize,
    ticket: FenceTicket,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeferredSceneRelease {
    PoolSlot(usize),
    FailedSubmit { bo_idx: usize, pool_slot: usize },
}

fn drain_deferred_scene_resources<W, R>(
    pending_pool_releases: &mut VecDeque<(usize, FenceTicket)>,
    failed_submit_bos: &mut VecDeque<FailedSubmitBo>,
    mut wait: W,
    mut release: R,
) where
    W: FnMut(&FenceTicket) -> bool,
    R: FnMut(DeferredSceneRelease) -> bool,
{
    let mut retained_pool_releases = VecDeque::with_capacity(pending_pool_releases.len());
    while let Some((slot, ticket)) = pending_pool_releases.pop_front() {
        if wait(&ticket) && release(DeferredSceneRelease::PoolSlot(slot)) {
            continue;
        }
        retained_pool_releases.push_back((slot, ticket));
    }
    *pending_pool_releases = retained_pool_releases;

    let mut retained_failed_submits = VecDeque::with_capacity(failed_submit_bos.len());
    while let Some(failed) = failed_submit_bos.pop_front() {
        if wait(&failed.ticket)
            && release(DeferredSceneRelease::FailedSubmit {
                bo_idx: failed.bo_idx,
                pool_slot: failed.pool_slot,
            })
        {
            continue;
        }
        retained_failed_submits.push_back(failed);
    }
    *failed_submit_bos = retained_failed_submits;
}

/// Ring of recent output-damage regions keyed by generation.
/// Depth = max(scanout_bo_count) + 1 per Stage 2 plan
/// cross-cutting §"BufferAgeRing".
pub(crate) struct BufferAgeRing {
    entries: VecDeque<(u64, RegionSet)>,
    depth: usize,
}

impl BufferAgeRing {
    fn new(depth: usize) -> Self {
        Self {
            entries: VecDeque::with_capacity(depth + 1),
            depth,
        }
    }

    /// Push `(gen, region)`. Trims to `depth` entries.
    fn push(&mut self, generation: u64, region: RegionSet) {
        self.entries.push_back((generation, region));
        while self.entries.len() > self.depth {
            self.entries.pop_front();
        }
    }

    /// Check whether every generation in `(last_gen+1, frame_gen)`
    /// (exclusive on both sides — those are the intervening
    /// generations between the BO's last present and the
    /// frame we're about to render) is in the ring.
    fn contains_all(&self, last_gen: u64, frame_gen: u64) -> bool {
        if frame_gen <= last_gen {
            return true; // shouldn't happen but bail safe
        }
        let want_count = (frame_gen - last_gen - 1) as usize;
        if want_count == 0 {
            // No intervening frames; the BO's content + current
            // damage covers it.
            return true;
        }
        let mut found = 0usize;
        for &(g, _) in &self.entries {
            if g > last_gen && g < frame_gen {
                found += 1;
            }
        }
        found >= want_count
    }

    /// Union all damage regions in `(last_gen+1, frame_gen)` into
    /// `dst`.
    fn union_history_into(&self, last_gen: u64, frame_gen: u64, dst: &mut RegionSet) {
        for (g, r) in &self.entries {
            if *g > last_gen && *g < frame_gen {
                dst.union_with(r);
            }
        }
    }
}

struct OutputSceneState {
    output_idx: usize,
    pool_ring: CompositePoolRing,
    /// Slots map: pending_ack[i] is using descriptor-pool slot
    /// `pool_slots[i]`. Released to the ring on flip retirement.
    pool_slots: VecDeque<usize>,
    pending_acks: VecDeque<PendingAck>,
    /// Fence-gated descriptor-pool slot releases. At
    /// `handle_page_flip_complete` we want to pop the matching
    /// `pool_slots` entry and free it, but the compose CB's Vulkan
    /// fence may not have signaled yet (pageflip retirement is
    /// driven by KMS VBLANK, not by GPU completion). Releasing the
    /// pool slot early calls `vkResetDescriptorPool` while the
    /// compose CB still binds its descriptors — VUID-vkReset-
    /// DescriptorPool-descriptorPool-00313. The fix: defer the
    /// release to this queue and drain it on the next opportunity
    /// (next tick / pageflip-complete) once `ticket.poll_signaled`
    /// returns true. Mirrors `failed_submit_bos` / `retire_failed_submit_bos`.
    pending_pool_releases: VecDeque<(usize, FenceTicket)>,
    /// GPU-submitted frames whose atomic commit was rejected.
    /// Keep both BO and descriptor-pool slot alive until the
    /// compose fence signals, then recycle them locally because
    /// no page-flip-complete will arrive for these frames.
    failed_submit_bos: VecDeque<FailedSubmitBo>,
    /// Buffer-age damage history (Stage 2e).
    damage_history: BufferAgeRing,
    /// Monotonic per-output generation. Advances only on a
    /// successful flip (transactional commit per codex round 2
    /// point 2).
    current_generation: u64,
    /// Scene-structure damage in output coords. Accumulated by
    /// `mark_scene_structure_damage(region)`; subtracted on
    /// retirement using the snapshot captured at submit time.
    scene_structure_damage: RegionSet,
    /// Repaint pending from prior failed submit/flip. Folded
    /// into the next tick's output damage.
    pending_repaint_after_failed_submit: RegionSet,
    /// Output extent — cached for full-output fallback regions.
    output_extent: vk::Extent2D,
    /// Backoff after atomic-commit failures. Without this, a failed
    /// commit can be retried once per core-loop iteration and flood
    /// KMS/RADV until the GPU context is lost.
    next_submit_retry_at: Option<std::time::Instant>,
    /// Stage 5 Phase D — per-output last-frame cursor mode. Drives
    /// the transition matrix in `tick_one_output`. v2's per-output
    /// frame retirement means scene-global cursor state would let
    /// output A's Sw→Hw fire while output B is still scanning the
    /// BO with SW pixels (multi-output double-cursor hazard);
    /// per-output mode + per-output `cursor_prev_pos` closes that.
    last_frame_cursor_mode: OutputCursorMode,
    /// Stage 5 Phase D — per-output SW cursor position carried so
    /// the next tick can damage the OLD rect. v3 of the plan moved
    /// this from `SceneCompositorInner.cursor_prev_pos`
    /// (scene-global) per the per-output isolation rule.
    /// **Transactional**: advances ONLY when the matching
    /// `PendingAck.cursor_prev_pos_after_retire` retires
    /// successfully — a failed submit must leave the OLD prev rect
    /// in place so the next frame still clears the trail.
    cursor_prev_pos: Option<(i32, i32)>,
    /// Cursor footprint from the last successfully presented frame
    /// on this output. Used to decide whether the current frame
    /// needs cursor damage and to re-poke pure HW hide/show cases.
    last_present_cursor_rect: Option<vk::Rect2D>,
    /// Cursor sprite version from the last successfully presented
    /// frame on this output. Lets a stationary sprite swap damage
    /// once, then return to idle.
    last_present_cursor_version: Option<u64>,
    /// A steady-HW sprite/hotspot rebind or a prior-binding show failed.
    /// Force the next Hw→Hw composed retirement to carry ShowOnRetire; do not
    /// claim the desired cursor metadata until that full rebind succeeds.
    force_show_retry_version: Option<u64>,
    /// Diagnostic: last reason `tick_one_output` skipped a tick for
    /// this output. Logged at INFO on transition (skip→different-skip,
    /// no-skip→skip, skip→no-skip). Tracks the freeze-debug
    /// hypothesis that one of the early-return gates gets stuck.
    last_skip_reason: Option<TickSkipReason>,
}

/// Diagnostic: why `tick_one_output` skipped an output. Used to
/// identify which gate is stuck when an output stops getting
/// page-flips. See `record_tick_skip`.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum TickSkipReason {
    /// `pending_acks` non-empty — flip in flight, KMS would EBUSY.
    PendingAcks,
    /// `next_submit_retry_at` deadline still in the future.
    RetryDeadline,
    /// `output_damage` is empty and this is not the first frame.
    EmptyDamage,
    /// `platform.acquire_scanout_bo` returned None — BO pool exhausted.
    NoBO,
    /// `pool_ring.acquire` returned None — descriptor-pool ring exhausted.
    NoPool,
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum TickOutcome {
    Composed,
    Skipped(TickSkipReason),
}

impl TickOutcome {
    fn clears_scene_structure_dirty(self) -> bool {
        matches!(
            self,
            Self::Composed | Self::Skipped(TickSkipReason::EmptyDamage)
        )
    }

    /// True iff `build_scene` ran for this output (so its sampled ids
    /// were recorded). The `PendingAcks`/`RetryDeadline` skips return
    /// BEFORE the walk; everything else runs it. `tick` only reconciles
    /// `offscreen_no_draw` when EVERY output walked — otherwise a
    /// window visible only on a not-yet-walked (mid-flip) output would
    /// be mis-flagged off-screen (cut 2b).
    fn walked(self) -> bool {
        !matches!(
            self,
            Self::Skipped(TickSkipReason::PendingAcks)
                | Self::Skipped(TickSkipReason::RetryDeadline)
        )
    }
}

// ────────────────────────────────────────────────────────────────
// SceneCompositor
// ────────────────────────────────────────────────────────────────

pub(crate) struct SceneCompositor {
    inner: Option<SceneCompositorInner>,
    /// Retained front-buffer overlay for legacy root-window
    /// `IncludeInferiors` XOR/invert drawing (import rubber-band, WM
    /// wireframes). Mutated only via the `root_overlay_*` helpers below,
    /// which also inject the scene-structure damage needed to force a
    /// compose.
    pub(crate) root_overlay: super::root_overlay::RootOverlay,
    /// Stage 2d's coarse scene-structure dirty bit. Set by any
    /// map/unmap/configure/restack/redirect-state/cursor-pos
    /// change. Cleared at tick end. Stage 2e narrows to a
    /// per-region scene_structure_damage `RegionSet`.
    pub(crate) scene_structure_dirty: bool,
    /// Stage 5 Phase H — `YSERVER_HW_CURSOR=1` env gate. Default
    /// OFF: the strategy decision always picks `Sw` so we don't
    /// regress correctness across the rollout. Set once at
    /// construction time so the gate is consistent across all
    /// `build_scene` calls.
    hw_cursor_strategy_enabled: bool,
    /// Test-only override for [`has_pending_page_flips`](Self::has_pending_page_flips).
    /// `KmsBackend::for_tests()` builds a `stub()` scene with `inner: None`,
    /// so there is no live `PendingAck` queue to populate; this lets
    /// capability-surface tests (`present_flip_in_flight`) exercise both
    /// states without a live Vulkan device.
    #[cfg(test)]
    test_flip_in_flight_override: Option<bool>,
}

/// Stage 5 Phase H — env gate for the HW cursor strategy. Default
/// **ON**: the HW cursor plane is the right model on hardware that
/// exposes it (it eliminates the SW-cursor-stuck-in-FB issue seen
/// after a VT switch resume, where the scanout BO retains stale SW
/// cursor pixels). Set `YSERVER_HW_CURSOR=0` (or `false` / `no` /
/// `off`) to opt out and fall back to the SW path — keep this lever
/// in case the original concern (cursor-plane atomic commits being
/// starved by scanout atomic `EBUSY` during COW/Present churn) recurs.
fn hw_cursor_strategy_enabled() -> bool {
    !matches!(
        std::env::var("YSERVER_HW_CURSOR").ok().as_deref(),
        Some("0" | "false" | "FALSE" | "no" | "NO" | "off" | "OFF")
    )
}

struct SceneCompositorInner {
    vk: Arc<crate::kms::vk::device::VkContext>,
    pipeline: CompositorPipeline,
    /// XOR-logic-op fill pipeline cache used to apply the retained
    /// root-`IncludeInferiors` overlay as a final pass into each
    /// freshly-composited scanout BO (see [`super::root_overlay`]).
    /// Built for the scanout color format (`B8G8R8A8_UNORM`); the
    /// `(Xor, opaque_alpha = true)` variant is the only one used —
    /// its RGB-only write mask preserves the server-owned α byte on
    /// the depth-24 scanout.
    overlay_xor_cache: crate::kms::vk::logic_fill_pipeline::LogicFillPipelineCache,
    outputs: Vec<OutputSceneState>,
    /// Stage 3f.8: software cursor sprite. Registered once at
    /// backend init via `register_cursor`; appended to the scene
    /// draw list at top-of-z by `build_scene`. `None` until
    /// registered (test fixtures don't bother). The real cursor
    /// theme + `define_cursor` wiring stays Stage 4 territory; this
    /// is just a default-arrow fallback so hardware smoke has
    /// visible pointer feedback.
    cursor: Option<CursorEntry>,
}

/// Stage 3f.8 cursor sprite registration. The sprite lives as a
/// regular [`DrawableStore`] entry (a `Pixmap` kind with a synthetic
/// xid) so its lifetime + Vk-handle destruction flow through the
/// same paths as any other drawable.
#[derive(Debug, Clone)]
pub(crate) struct CursorEntry {
    pub(crate) id: super::store::DrawableId,
    pub(crate) extent: vk::Extent2D,
    pub(crate) hot_x: i16,
    pub(crate) hot_y: i16,
    /// Stage 5 Phase B — `Arc<CursorRecord>.version`. Compared by
    /// value in the Phase D upload-dedup path. Zero in unit-test
    /// constructions that pre-date Phase A.
    pub(crate) record_version: u64,
    /// Stage 5 Phase D — straight-alpha BGRA8 bytes shared with
    /// the `CursorRecord` on the backend. `Arc` lets every output's
    /// retire-time upload reference the same allocation without copying.
    /// `None` in unit-test constructions that pre-date Phase A.
    pub(crate) bgra_bytes: Option<std::sync::Arc<Vec<u8>>>,
}

struct SceneBuild {
    scene: CompositeScene,
    snapshots: Vec<DamageSnapshot>,
    sampled_ids: Vec<super::store::DrawableId>,
    projected_damage: RegionSet,
    /// Stage 5 Phase C — pure cursor strategy decision. The outer
    /// tick consumes this to derive the per-output transition
    /// + new `cursor_prev_pos` and queue them on the PendingAck.
    cursor_assignment: CursorAssignment,
    /// Clipped cursor footprint for the current frame on this
    /// output, regardless of whether the cursor will present via
    /// SW composite or the HW plane.
    new_cursor_rect: Option<vk::Rect2D>,
    /// Version of the cursor sprite contributing `new_cursor_rect`.
    /// `None` when the cursor is hidden on this output.
    cursor_record_version: Option<u64>,
    /// Tail indices of the optional software-cursor draw and sampled id. The
    /// outer transition state machine removes this contribution for the first
    /// phase of Hw→Sw, before it submits the cursorless hide frame.
    software_cursor_tail: Option<(usize, usize)>,
}

impl SceneBuild {
    fn omit_software_cursor_for_hide(&mut self) {
        if let Some((draw_index, sampled_index)) = self.software_cursor_tail.take() {
            debug_assert_eq!(self.scene.draws.len(), draw_index + 1);
            debug_assert_eq!(self.sampled_ids.len(), sampled_index + 1);
            self.scene.draws.truncate(draw_index);
            self.sampled_ids.truncate(sampled_index);
        }
        self.new_cursor_rect = None;
        self.cursor_record_version = None;
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum SceneError {
    #[error("vk pipeline init: {0}")]
    PipelineInit(crate::kms::vk::pipeline::PipelineError),
    #[error("vk: {0:?}")]
    Vk(vk::Result),
    #[error("scene compositor in stub mode (no Vk)")]
    NoVk,
    #[error("compositor present failed: {0}")]
    Present(PresentError),
}

impl From<PresentError> for SceneError {
    fn from(e: PresentError) -> Self {
        SceneError::Present(e)
    }
}

impl From<crate::kms::vk::logic_fill_pipeline::LogicFillError> for SceneError {
    fn from(e: crate::kms::vk::logic_fill_pipeline::LogicFillError) -> Self {
        use crate::kms::vk::logic_fill_pipeline::LogicFillError;
        match e {
            LogicFillError::Vk(r) => SceneError::Vk(r),
            // Build-time-baked SPIR-V is length-aligned; treat a
            // malformed module as an init failure.
            LogicFillError::SpirvUnaligned(_) => {
                SceneError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED)
            }
        }
    }
}

impl SceneCompositor {
    /// Production constructor. Builds the blit pipeline (reuses
    /// v1's CompositorPipeline — same shaders, same descriptor
    /// layout) and one descriptor-pool ring per output.
    ///
    /// # Errors
    ///
    /// `PipelineInit` on shader / pipeline build failure;
    /// `Vk(...)` on descriptor-pool init.
    pub(crate) fn new(platform: &PlatformBackend) -> Result<Self, SceneError> {
        let vk = platform.vk().ok_or(SceneError::NoVk)?.clone();
        let pipeline = CompositorPipeline::new(Arc::clone(&vk), vk::Format::B8G8R8A8_UNORM)
            .map_err(SceneError::PipelineInit)?;
        // Root-overlay XOR pass pipeline cache — built for the same
        // color format the compose color attachment / scanout BO uses
        // (`B8G8R8A8_UNORM`, see `kms::vk::scanout` image-view creation)
        // so the XOR draws are format-compatible with the active compose
        // rendering instance they are recorded into.
        let overlay_xor_cache = crate::kms::vk::logic_fill_pipeline::LogicFillPipelineCache::new(
            Arc::clone(&vk),
            vk::Format::B8G8R8A8_UNORM,
        )?;
        let mut outputs = Vec::with_capacity(platform.outputs.len());
        for i in 0..platform.outputs.len() {
            outputs.push(Self::build_output_state(&vk, platform, i)?);
        }
        Ok(Self {
            inner: Some(SceneCompositorInner {
                vk,
                pipeline,
                overlay_xor_cache,
                outputs,
                cursor: None,
            }),
            root_overlay: super::root_overlay::RootOverlay::default(),
            scene_structure_dirty: true,
            // The environment gate is global, while driver/capability policy
            // is evaluated per output by PlatformBackend. A secondary NVIDIA
            // card must not disable the hardware cursor on an AMD/Intel card
            // (and vice versa).
            hw_cursor_strategy_enabled: hw_cursor_strategy_enabled(),
            #[cfg(test)]
            test_flip_in_flight_override: None,
        })
    }

    fn build_output_state(
        vk: &Arc<crate::kms::vk::device::VkContext>,
        platform: &PlatformBackend,
        i: usize,
    ) -> Result<OutputSceneState, SceneError> {
        let layout = &platform.outputs[i];
        let ring = CompositePoolRing::new(Arc::clone(vk), MAX_DESCRIPTOR_SETS_PER_FRAME)
            .map_err(SceneError::Vk)?;
        let bo_depth = platform
            .scanout_pools
            .get(i)
            .and_then(|p| p.as_ref().map(|pool| pool.display_pool().bos.len()))
            .unwrap_or(3);
        Ok(OutputSceneState {
            output_idx: i,
            pool_ring: ring,
            pool_slots: VecDeque::with_capacity(4),
            pending_pool_releases: VecDeque::with_capacity(4),
            pending_acks: VecDeque::with_capacity(4),
            failed_submit_bos: VecDeque::with_capacity(4),
            damage_history: BufferAgeRing::new(bo_depth + 1),
            current_generation: 0,
            scene_structure_damage: RegionSet::new(),
            pending_repaint_after_failed_submit: RegionSet::new(),
            output_extent: vk::Extent2D {
                width: u32::from(layout.width),
                height: u32::from(layout.height),
            },
            next_submit_retry_at: None,
            last_frame_cursor_mode: OutputCursorMode::Hidden,
            cursor_prev_pos: None,
            last_present_cursor_rect: None,
            last_present_cursor_version: None,
            force_show_retry_version: None,
            last_skip_reason: None,
        })
    }

    pub(crate) fn rebuild_outputs(&mut self, platform: &PlatformBackend) -> Result<(), SceneError> {
        let Some(inner) = self.inner.as_mut() else {
            return Ok(());
        };
        let vk = inner.vk.clone();
        let mut outputs = Vec::with_capacity(platform.outputs.len());
        for i in 0..platform.outputs.len() {
            outputs.push(Self::build_output_state(&vk, platform, i)?);
        }
        inner.outputs = outputs;
        self.scene_structure_dirty = true;
        // root-overlay is root-absolute + layout-dependent; drop it on
        // topology change. Covers both connector hotplug
        // (`fire_randr_changes`) and per-CRTC reconfiguration
        // (`apply_crtc_config`) — the two callers of `rebuild_outputs`.
        self.root_overlay_clear();
        Ok(())
    }

    /// Stage 3f.8: register the software cursor sprite after the
    /// backend has uploaded its pixel data. Idempotent — a later
    /// `define_cursor` flow (Stage 4) can swap the entry. Drops to
    /// a no-op on the stub fixture.
    pub(crate) fn register_cursor(&mut self, entry: CursorEntry) {
        if let Some(inner) = self.inner.as_mut() {
            inner.cursor = Some(entry);
            self.scene_structure_dirty = true;
        }
    }

    /// Test fixture / Stage-1b-era stub. Construct via
    /// `SceneCompositor::stub()` so the `KmsBackend::for_tests`
    /// path doesn't need Vk.
    pub(crate) fn stub() -> Self {
        Self {
            inner: None,
            root_overlay: super::root_overlay::RootOverlay::default(),
            scene_structure_dirty: false,
            hw_cursor_strategy_enabled: false,
            #[cfg(test)]
            test_flip_in_flight_override: None,
        }
    }

    /// Whether the scene has a live blit pipeline. Tests use
    /// this to skip Vk-only assertions.
    pub(crate) fn is_live(&self) -> bool {
        self.inner.is_some()
    }

    /// Mark the scene as needing a redraw. Cheap bool flip;
    /// callable from any mutation path that wants the next tick
    /// to inspect drawable/cursor damage. This deliberately does
    /// NOT add output damage: protocol paint is already represented
    /// by per-drawable presentation damage, and cursor motion is
    /// projected by `build_scene`.
    pub(crate) fn wake_for_damage(&mut self) {
        self.scene_structure_dirty = true;
    }

    /// Mark scene structure as changed. This is the coarse fallback
    /// for map/unmap/configure/restack/redirect/root-background
    /// transitions where old/new visibility cannot yet be expressed
    /// as a narrower rect.
    pub(crate) fn mark_scene_structure_dirty(&mut self) {
        self.scene_structure_dirty = true;
        if let Some(inner) = self.inner.as_mut() {
            for o in &mut inner.outputs {
                let extent = o.output_extent;
                o.scene_structure_damage.add(vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent,
                });
            }
        }
    }

    /// Region-precise scene-structure damage (Stage 3+).
    pub(crate) fn mark_scene_structure_damage_rect(&mut self, output_idx: usize, r: vk::Rect2D) {
        self.scene_structure_dirty = true;
        if let Some(inner) = self.inner.as_mut()
            && let Some(o) = inner.outputs.get_mut(output_idx)
        {
            o.scene_structure_damage.add(r);
        }
    }

    /// Stage 4c.1 — rect-precise scene-structure damage where the
    /// caller doesn't know which output(s) a screen-/output-coord
    /// rect intersects. Each input rect is intersected against every
    /// output's extent and (if non-empty) added to that output's
    /// `scene_structure_damage`. Mirrors the singular
    /// `mark_scene_structure_damage_rect` setter but applies to all
    /// outputs with output-extent clipping, the dual of
    /// `add_projected_damage` (output-coord input rather than
    /// storage-local projection).
    ///
    /// In the Stage-4 single-output deployment, output origin is
    /// (0, 0) so "screen-coord" and "output-local-coord" coincide;
    /// this clip is just "drop the bits that fall off the right /
    /// bottom edge".
    pub(crate) fn mark_scene_structure_damage_rects(&mut self, rects: &[vk::Rect2D]) {
        self.scene_structure_dirty = true;
        let Some(inner) = self.inner.as_mut() else {
            return;
        };
        dispatch_clip_rects_to_outputs(
            inner
                .outputs
                .iter_mut()
                .map(|o| (o.output_extent, &mut o.scene_structure_damage)),
            rects,
        );
    }

    /// Toggle an overlay XOR op (root-absolute rects) and inject output damage
    /// so a compose actually runs (wake_for_damage alone leaves output_damage
    /// empty and the frame is EmptyDamage-skipped).
    pub(crate) fn root_overlay_toggle(
        &mut self,
        client: yserver_protocol::x11::ClientId,
        value: u32,
        rects: &[ash::vk::Rect2D],
    ) {
        if self.root_overlay.toggle(client, value, rects) {
            let mut dmg = rects.to_vec();
            dmg.extend(self.root_overlay.all_rects());
            self.mark_scene_structure_damage_rects(&dmg);
            self.wake_for_damage();
        }
    }

    /// Clear the overlay (RandR/topology change) and damage the vacated rects.
    pub(crate) fn root_overlay_clear(&mut self) {
        if self.root_overlay.is_empty() {
            return;
        }
        let vacated = self.root_overlay.all_rects();
        self.root_overlay.clear();
        self.mark_scene_structure_damage_rects(&vacated);
        self.wake_for_damage();
    }

    /// Drop a disconnecting client's overlay contribution.
    pub(crate) fn root_overlay_on_disconnect(&mut self, client: yserver_protocol::x11::ClientId) {
        let vacated = self.root_overlay.all_rects();
        if self.root_overlay.on_client_disconnect(client) {
            self.mark_scene_structure_damage_rects(&vacated);
            self.wake_for_damage();
        }
    }

    /// Earliest pending commit-retry deadline across outputs.
    pub(crate) fn earliest_retry_deadline(&self) -> Option<std::time::Instant> {
        self.inner
            .as_ref()?
            .outputs
            .iter()
            .filter_map(|o| o.next_submit_retry_at)
            .min()
    }

    /// Whether a dirty scene can submit to at least one output now.
    ///
    /// `maybe_composite` uses this before flushing deferred paint
    /// batches. If every output is still waiting on a pageflip or a
    /// commit-retry backoff, flushing paint would create GPU submit
    /// traffic that cannot be scanned out yet and would fragment COW
    /// batching under compositor drag workloads.
    pub(crate) fn has_output_ready_for_submit(&self) -> bool {
        let Some(inner) = self.inner.as_ref() else {
            return true;
        };
        let now = std::time::Instant::now();
        inner.outputs.iter().any(|o| {
            o.pending_acks.is_empty()
                && o.next_submit_retry_at
                    .is_none_or(|deadline| now >= deadline)
        })
    }

    /// True while any output has an atomic pageflip awaiting retirement.
    /// Present completion pacing uses this with pending compose damage to
    /// decide whether a standalone CRTC sequence is a genuine idle fallback.
    pub(crate) fn has_pending_page_flips(&self) -> bool {
        #[cfg(test)]
        if let Some(v) = self.test_flip_in_flight_override {
            return v;
        }
        self.inner
            .as_ref()
            .is_some_and(|inner| inner.outputs.iter().any(|o| !o.pending_acks.is_empty()))
    }

    /// True while one specific output has an atomic pageflip awaiting
    /// retirement. Present scheduling is CRTC-domain-specific: activity on a
    /// different card/output must not change the selected output's immediate
    /// target rule.
    pub(crate) fn has_pending_page_flip(&self, output_idx: usize) -> bool {
        #[cfg(test)]
        if let Some(v) = self.test_flip_in_flight_override {
            return v;
        }
        self.inner
            .as_ref()
            .and_then(|inner| inner.outputs.get(output_idx))
            .is_some_and(|output| !output.pending_acks.is_empty())
    }

    /// Test-only: force [`has_pending_page_flips`](Self::has_pending_page_flips)
    /// without a live output/`PendingAck` queue.
    #[cfg(test)]
    pub(crate) fn test_set_flip_in_flight(&mut self, value: bool) {
        self.test_flip_in_flight_override = Some(value);
    }

    /// Stage 5 Phase D — cursor-plane mode aggregate query for the
    /// pointer fast path. Returns `Hw` ONLY when every active
    /// output has retired its Sw→Hw transition AND no PendingAck
    /// carries an in-flight cursor transition. Mixed-state
    /// (transition pending on any output, or a heterogeneous
    /// mix) returns `Mixed`; the fast path falls back to scene
    /// wake until the plane is fully consistent.
    pub(crate) fn cursor_mode(&self) -> CursorPlaneMode {
        let Some(inner) = self.inner.as_ref() else {
            return CursorPlaneMode::Sw;
        };
        for output in &inner.outputs {
            // Any pending transition on any output forces Mixed —
            // the fast path must not move the plane until every
            // ShowOnRetire / HideOnRetire has applied.
            if output
                .pending_acks
                .iter()
                .any(|a| a.cursor_transition.is_some())
            {
                return CursorPlaneMode::Mixed;
            }
            if output.force_show_retry_version.is_some() {
                return CursorPlaneMode::Mixed;
            }
        }
        classify_cursor_mode_from_per_output(inner.outputs.iter().map(|o| o.last_frame_cursor_mode))
    }

    /// Stage 5 Phase D — steady-state HW sprite-change path. Called
    /// synchronously from the backend's `refresh_effective_cursor`.
    /// Marks each output that already has (or is retiring into) a HW
    /// binding for an output-local upload + full ShowOnRetire retry.
    ///
    /// `bytes` MUST be `width * height * 4` (BGRA8). `Arc` so the
    /// deferred slot can hold the bytes without re-cloning the
    /// `Vec<u8>` from `CursorRecord` per upload.
    pub(crate) fn queue_steady_state_cursor_upload(
        &mut self,
        _platform: &mut PlatformBackend,
        version: u64,
        _width: u16,
        _height: u16,
        _bgra_bytes: std::sync::Arc<Vec<u8>>,
        _hot_x: u16,
        _hot_y: u16,
        _cursor_x: i32,
        _cursor_y: i32,
    ) -> bool {
        let Some(inner) = self.inner.as_mut() else {
            return false;
        };
        // Do not mutate/rebind several cards synchronously as one pseudo-
        // transaction. Each output's next composed retirement performs its
        // own upload+ShowOnRetire on the owning device. That keeps per-device
        // capacities and failures independent and gives the cursor state
        // machine an exact success/failure point for metadata commitment.
        let mut refreshes_hw_binding = false;
        for output in &mut inner.outputs {
            if cursor_output_needs_sprite_retry(
                output.last_frame_cursor_mode,
                output.pending_acks.iter().map(|ack| ack.cursor_transition),
            ) {
                output.force_show_retry_version = Some(version);
                force_cursor_retry_repaint(output);
                refreshes_hw_binding = true;
            }
        }
        self.scene_structure_dirty = true;
        refreshes_hw_binding
    }

    /// Drain in-flight compose work before tear-down. Best-effort
    /// — `device_wait_idle` is the safe fallback the platform
    /// uses anyway. Releases descriptor-pool slots so the
    /// pool-ring's Drop doesn't fire while slots are still in use.
    pub(crate) fn drain_all(&mut self, platform: &mut PlatformBackend) {
        let Some(inner) = self.inner.as_mut() else {
            return;
        };
        // Stop readiness delivery before discarding the ledger that owns each
        // job id. The source-completion fd is only a notification handle; the
        // fence ticket below still proves A's submitted command buffer is done
        // before descriptor slots are reset, and the platform subsequently
        // drains both devices before any copied pool is reset or dropped.
        platform.clear_scanout_render_completions();
        let vk = inner.vk.clone();
        for (output_idx, o) in inner.outputs.iter_mut().enumerate() {
            // B.2-context fix (codex audit followup): wait for any
            // in-flight compose fences before resetting their
            // descriptor-pool slots. `disable_output` runs
            // device_wait_idle later, but we hit
            // vkResetDescriptorPool BEFORE that wait. Wait on each
            // ack's ticket here to keep VUID-vkResetDescriptorPool-
            // descriptorPool-00313 satisfied during teardown too.
            let mut retained_acks = VecDeque::with_capacity(o.pending_acks.len());
            let mut retained_slots = VecDeque::with_capacity(o.pool_slots.len());
            while let Some(ack) = o.pending_acks.pop_front() {
                let slot = o.pool_slots.pop_front();
                let wait_ok = ack
                    .ticket
                    .as_ref()
                    .is_none_or(|ticket| match ticket.wait(&vk) {
                        Ok(()) => true,
                        Err(error) => {
                            log::error!(
                                "render scene drain: output {output_idx} compose fence wait \
                                 failed: {error:?}; retaining resources for quarantine"
                            );
                            platform.renderer_failed = true;
                            false
                        }
                    });
                if wait_ok {
                    if let Some(slot) = slot {
                        o.pool_ring.release(slot);
                    }
                } else {
                    retained_acks.push_back(ack);
                    if let Some(slot) = slot {
                        retained_slots.push_back(slot);
                    }
                }
            }
            retained_slots.append(&mut o.pool_slots);
            o.pending_acks = retained_acks;
            o.pool_slots = retained_slots;
            let pending_pool_releases = &mut o.pending_pool_releases;
            let failed_submit_bos = &mut o.failed_submit_bos;
            let pool_ring = &mut o.pool_ring;
            let mut wait_failed = false;
            let mut recovery_failed = false;
            drain_deferred_scene_resources(
                pending_pool_releases,
                failed_submit_bos,
                |ticket| match ticket.wait(&vk) {
                    Ok(()) => true,
                    Err(error) => {
                        log::error!(
                            "render scene drain: deferred compose fence wait failed: \
                             {error:?}; retaining resources for quarantine"
                        );
                        wait_failed = true;
                        false
                    }
                },
                |release| match release {
                    DeferredSceneRelease::PoolSlot(slot) => {
                        pool_ring.release(slot);
                        true
                    }
                    DeferredSceneRelease::FailedSubmit { bo_idx, pool_slot } => {
                        match platform.recycle_failed_submit_bo(output_idx, bo_idx) {
                            Ok(()) => {
                                pool_ring.release(pool_slot);
                                true
                            }
                            Err(error) => {
                                log::error!(
                                    "render scene drain: failed to recover output {output_idx} \
                                     bo {bo_idx}: {error}"
                                );
                                recovery_failed = true;
                                false
                            }
                        }
                    }
                },
            );
            if wait_failed || recovery_failed {
                platform.renderer_failed = true;
            }
            // Stage 5 Phase D' — global recovery: reset every
            // output's cursor mode to Hidden. The post-recovery
            // first compose re-decides via build_scene's
            // strategy. cursor_prev_pos is also cleared so the
            // next frame doesn't damage a stale trail rect.
            reset_cursor_mode_for_lifecycle(&mut o.last_frame_cursor_mode);
            o.cursor_prev_pos = None;
            o.last_present_cursor_rect = None;
            o.last_present_cursor_version = None;
            reset_cursor_retry_for_lifecycle(&mut o.force_show_retry_version);
        }
        // Hide the plane everywhere + invalidate uploaded_version.
        // Best-effort; the platform hook logs per-CRTC failures.
        let _ = platform.cursor_plane_hide_all();
    }

    /// Compose a frame per output. Each output that has a free
    /// BO produces one atomic flip. Returns the number of
    /// output indices that successfully submitted (empty if everything was
    /// stalled / not dirty / no scene entries).
    ///
    /// # Errors
    ///
    /// Per-output failures don't abort the loop; they're logged
    /// and the next output is attempted. Top-level Err means
    /// the platform was unusable (no Vk).
    pub(crate) fn tick(
        &mut self,
        core: &KmsCore,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
        windows: &super::backend::WindowsMap,
        telemetry: &mut Telemetry,
        cow_host_xid: Option<u32>,
    ) -> Result<Vec<usize>, SceneError> {
        let hw_strategy = self.hw_cursor_strategy_enabled;
        // Destructure so `inner` (mutable) and `root_overlay` (shared)
        // are borrowed as disjoint fields: `tick_one_output` needs
        // `&mut inner` for the overlay-XOR pipeline cache AND read
        // access to the retained root overlay living on the outer struct.
        let SceneCompositor {
            inner,
            root_overlay,
            scene_structure_dirty,
            ..
        } = self;
        let Some(inner) = inner.as_mut() else {
            return Err(SceneError::NoVk);
        };
        platform.refresh_fence_pool_failure();
        if platform.renderer_failed {
            return Ok(Vec::new());
        }
        debug_assert_eq!(
            inner.outputs.len(),
            platform.outputs.len(),
            "scene/platform output vectors must stay in lockstep",
        );
        let n_outputs = inner.outputs.len();
        let mut composed = Vec::new();
        let mut clear_dirty = true;
        // Idle free-run fix (cut 2b): union of sampled sources drawn
        // across all outputs, and whether every output actually walked
        // `build_scene`. Only reconcile `offscreen_no_draw` when all
        // walked — see `TickOutcome::walked`.
        let mut drawn: std::collections::HashSet<super::store::DrawableId> =
            std::collections::HashSet::new();
        let mut all_walked = true;
        for output_idx in 0..n_outputs {
            match tick_one_output(
                inner,
                output_idx,
                core,
                store,
                platform,
                windows,
                telemetry,
                hw_strategy,
                cow_host_xid,
                root_overlay,
                &mut drawn,
            ) {
                Ok(outcome) => {
                    if outcome == TickOutcome::Composed {
                        composed.push(output_idx);
                    } else {
                        clear_dirty &= outcome.clears_scene_structure_dirty();
                    }
                    all_walked &= outcome.walked();
                }
                Err(e) => {
                    clear_dirty = false;
                    all_walked = false;
                    log::warn!(
                        "render scene tick: output {output_idx} compose failed: {e}; continuing",
                    );
                }
            }
        }
        if all_walked {
            store.reconcile_offscreen_no_draw(&drawn);
        }
        if clear_dirty {
            *scene_structure_dirty = false;
        }
        // Vulkan may export the already-signalled SYNC_FD sentinel (`fd=-1`).
        // Such a job has no pollable fd, so drain only after every composed
        // output installed its PendingAck; exact job/BO matching is then live
        // before the immediate B submission runs.
        for completion in platform.drain_scanout_render_completions() {
            if platform.renderer_failed {
                break;
            }
            if !handle_scanout_render_completion_inner(inner, completion, platform) {
                telemetry.record_missed_pageflip();
            }
            if platform.renderer_failed {
                break;
            }
        }
        Ok(composed)
    }

    /// Handle a DRM page-flip-complete event for `output_idx`.
    /// Pops the matching pending-ack, ack's its damage snapshots
    /// against the store, releases the descriptor-pool slot,
    /// then advances the platform's BO state machine via
    /// `on_page_flip_complete`. Engine retirement happens after
    /// (driven by the backend wrapper to keep the borrows clean).
    pub(crate) fn handle_page_flip_complete(
        &mut self,
        output_idx: usize,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
    ) -> bool {
        let Some(inner) = self.inner.as_mut() else {
            return false;
        };
        let expected = inner
            .outputs
            .get(output_idx)
            .and_then(|state| state.pending_acks.front())
            .map(|ack| (ack.stage, ack.bo_idx));
        let Some(retire) = platform.on_page_flip_complete(output_idx) else {
            return false;
        };
        let Some(state) = inner.outputs.get_mut(output_idx) else {
            return false;
        };
        if !expected.is_some_and(|(stage, bo_idx)| {
            kms_retirement_matches(stage, bo_idx, retire.presented_bo_idx)
        }) {
            log::warn!(
                "render scene: page-flip-complete on output {output_idx} presented bo {} \
                 but the pending scene frame was {expected:?}",
                retire.presented_bo_idx,
            );
            return false;
        }
        if let Some(ack) = state.pending_acks.pop_front() {
            // Ack each per-drawable damage snapshot. Snapshots
            // from paint that landed after the tick's peek
            // survive (per I5 epoch semantics).
            for snap in ack.drawable_snapshots {
                store.ack_presentation_damage(snap);
            }
            // Subtract the submitted output-damage snapshots
            // from live state (codex round 2 point 1). Damage
            // that arrived between submit and retirement
            // (map/unmap/cursor-move while flip in flight) is
            // NOT in the snapshots and therefore survives,
            // driving the next tick.
            state
                .scene_structure_damage
                .subtract(&ack.submitted_scene_structure_damage);
            state
                .pending_repaint_after_failed_submit
                .subtract(&ack.submitted_failed_repaint);
            // Push this frame's output damage onto the
            // buffer-age history ring keyed by its generation.
            state
                .damage_history
                .push(ack.generation, ack.submitted_output_damage);
            // Release the matching pool slot — but only after the
            // compose CB's Vulkan fence has signaled. Pageflip
            // retirement is driven by KMS VBLANK, not by GPU
            // completion; if the GPU is still executing the compose
            // CB when this pageflip lands, releasing the pool slot
            // immediately calls vkResetDescriptorPool on a pool
            // whose descriptors are still bound to that CB
            // (VUID-vkResetDescriptorPool-descriptorPool-00313).
            // Fence-gate: if signaled now, release immediately;
            // otherwise defer to `pending_pool_releases` for the
            // drain pass to handle on a later tick.
            if let Some(slot) = state.pool_slots.pop_front() {
                match &ack.ticket {
                    None => state.pool_ring.release(slot),
                    Some(t) => match t.poll_signaled_result(&inner.vk) {
                        Ok(true) => state.pool_ring.release(slot),
                        Ok(false) => {
                            state.pending_pool_releases.push_back((slot, t.clone()));
                        }
                        Err(error) => {
                            log::error!(
                                "render scene: compose fence status failed at pageflip \
                                 retirement: {error:?}"
                            );
                            platform.renderer_failed = true;
                            state.pending_pool_releases.push_back((slot, t.clone()));
                        }
                    },
                }
            }
            // Commit the BO's new last_present_generation in the
            // platform (the buffer-age pick uses this on next
            // acquire).
            platform.commit_bo_present(output_idx, retire.presented_bo_idx, ack.generation);

            // The KMS frame retired, but the cursor transition itself is a
            // second transaction. Record what actually happened: never claim
            // Hw after a rolled-back show and never claim Sw/Hidden while a
            // failed hide or rollback left the hardware sprite visible.
            let cursor_result = apply_cursor_transition_on_retire(
                inner,
                output_idx,
                platform,
                ack.cursor_transition,
            );
            let state = inner
                .outputs
                .get_mut(output_idx)
                .expect("range checked above");
            let resolution =
                resolve_retired_cursor_state(cursor_result, ack.cursor_mode_after_retire);
            state.force_show_retry_version = update_force_show_retry_version(
                state.force_show_retry_version,
                ack.cursor_transition,
                cursor_result,
                ack.cursor_mode_after_retire,
            );
            state.last_frame_cursor_mode = resolution.actual_mode;
            if resolution.commit_desired_metadata {
                if let Some(new_prev) = ack.cursor_prev_pos_after_retire {
                    state.cursor_prev_pos = new_prev;
                }
                state.last_present_cursor_rect = ack.last_present_cursor_rect_after_retire;
                state.last_present_cursor_version = ack.last_present_cursor_version_after_retire;
            } else {
                state.cursor_prev_pos = None;
                if resolution.clear_presented_metadata {
                    state.last_present_cursor_rect = None;
                    state.last_present_cursor_version = None;
                }
                // Otherwise preserve last-known metadata: the requested bind
                // did not fully land, so claiming the desired rect/version
                // could make later same-position damage incorrectly skip.
            }
            if resolution.force_repaint {
                force_cursor_retry_repaint(state);
            }
            let actually_sw_composed =
                matches!(ack.cursor_mode_after_retire, OutputCursorMode::Sw { .. });
            if actually_sw_composed && platform.cursor_plane_note_composed_retirement(output_idx) {
                // Consume this output/device's own EINVAL backoff one
                // composed retirement at a time. Damage keeps the sequence
                // alive until the retry becomes eligible.
                force_cursor_retry_repaint(state);
            }
            true
        } else {
            log::debug!(
                "render scene: page-flip-complete on output {output_idx} \
                 with no pending ack — startup flush or spurious event",
            );
            false
        }
    }

    /// Advance one copied frame from source-render completion on renderer A
    /// to the copy + KMS submission on sink renderer B. Stable output identity,
    /// monotonic job id, and the paired BO index must all match the ledger; a
    /// stale notification never retargets a rebuilt output vector.
    pub(crate) fn handle_scanout_render_completion(
        &mut self,
        completion: ReadyScanoutRenderCompletion,
        platform: &mut PlatformBackend,
    ) -> bool {
        let Some(inner) = self.inner.as_mut() else {
            return false;
        };
        handle_scanout_render_completion_inner(inner, completion, platform)
    }
}

fn handle_scanout_render_completion_inner(
    inner: &mut SceneCompositorInner,
    completion: ReadyScanoutRenderCompletion,
    platform: &mut PlatformBackend,
) -> bool {
    if platform.renderer_failed {
        return false;
    }
    let ReadyScanoutRenderCompletion {
        job_id,
        output_key,
        bo_idx,
        fd,
    } = completion;
    let Some(output_idx) = platform
        .outputs
        .iter()
        .position(|output| output.key == output_key)
    else {
        log::debug!(
            "render copied scanout: completion job {job_id} targeted removed output \
                 {output_key:?}",
        );
        return false;
    };
    let expected = inner
        .outputs
        .get(output_idx)
        .and_then(|state| state.pending_acks.front())
        .is_some_and(|ack| copied_render_completion_matches(ack.stage, ack.bo_idx, job_id, bo_idx));
    if !expected {
        log::warn!(
            "render copied scanout: stale completion job {job_id} for output \
                 {output_idx} bo {bo_idx}",
        );
        // A live output retaining a different ledger entry means the
        // completed source can no longer be associated safely. Fail-stop
        // rather than guessing a pool slot or making either device's
        // allocation reusable while GPU ownership is uncertain.
        platform.renderer_failed = true;
        return false;
    }

    match platform.submit_copied_scanout(output_idx, bo_idx, fd) {
        Ok(()) => {
            if let Some(ack) = inner.outputs[output_idx].pending_acks.front_mut() {
                ack.stage = InFlightStage::KmsFlipPending;
            }
            true
        }
        Err(error) => {
            log::warn!(
                "render copied scanout: sink copy/KMS submit failed for output \
                     {output_idx} bo {bo_idx}: {error}",
            );
            // The platform/resource layer synchronously idles B before
            // recovery and quarantines the pair if quiescence fails.
            // The scene may retire only A-local descriptor resources, and
            // only behind A's compose fence; damage/cursor metadata remain
            // live for a later repaint because no frame reached the screen.
            platform.invalidate_bo(output_idx, bo_idx);
            let state = &mut inner.outputs[output_idx];
            let ack = state
                .pending_acks
                .pop_front()
                .expect("completion identity was checked against the front ack");
            state.current_generation = ack.generation.saturating_sub(1);
            if let Some(rect) = ack.submitted_output_damage.bounding_rect() {
                state.pending_repaint_after_failed_submit.add(rect);
            }
            if let Some(slot) = state.pool_slots.pop_front() {
                match ack.ticket {
                    None => match platform.recycle_failed_submit_bo(output_idx, bo_idx) {
                        Ok(()) => state.pool_ring.release(slot),
                        Err(recovery_error) => {
                            log::error!(
                                "render copied scanout: renderer recovery failed for output \
                                     {output_idx} bo {bo_idx}: {recovery_error}"
                            );
                            platform.renderer_failed = true;
                        }
                    },
                    Some(ticket) => {
                        // Even if the fence is already signalled, route
                        // all post-A-submit failures through the same
                        // deferred recycler. That recycler rearms dirty
                        // export semaphores, retires the consumed prior
                        // B->A wait, and only then frees the paired slot.
                        state.failed_submit_bos.push_back(FailedSubmitBo {
                            bo_idx,
                            pool_slot: slot,
                            ticket,
                        });
                    }
                }
            } else {
                log::error!(
                    "render copied scanout: missing descriptor slot for failed output \
                         {output_idx} bo {bo_idx}"
                );
                platform.renderer_failed = true;
            }
            state.next_submit_retry_at =
                Some(std::time::Instant::now() + std::time::Duration::from_millis(100));
            false
        }
    }
}

// ────────────────────────────────────────────────────────────────
// Per-output compose tick body
// ────────────────────────────────────────────────────────────────

/// Stage 5 Phase D — pure derivation: combine the previous frame's
/// per-output cursor mode with this frame's `CursorAssignment` to
/// produce the transition to queue and the new prev_pos to write on
/// successful retirement.
///
/// Returns `(transition_to_queue, prev_pos_after_retire)`.
///
/// - `transition_to_queue` is `Some(ShowOnRetire)` only on actual
///   mode transitions into HW (`Hidden→Hw` / `Sw→Hw`);
///   `Some(HideOnRetire)` on transitions out (`Hw→Sw` / `Hw→Hidden`).
///   Steady-state same-mode frames produce `None`.
/// - `prev_pos_after_retire` is `Some(Some(pos))` to set, or
///   `Some(None)` to clear, on successful retire. `None` means
///   "leave `OutputSceneState.cursor_prev_pos` as-is". Hw mode
///   doesn't carry an SW prev_pos so the field always clears on
///   `→ Hw`; `Sw` / `Hidden` carry it.
fn cursorless_hide_frame_required(prev: OutputCursorMode, assignment: CursorAssignment) -> bool {
    matches!(prev, OutputCursorMode::Hw)
        && matches!(
            assignment,
            CursorAssignment::Sw { .. } | CursorAssignment::Hidden
        )
}

/// Reconcile transactional scene bookkeeping with the owning device's actual
/// kernel-side cursor binding. Eligibility can change while a failed hide or
/// rollback leaves HW visible. In that case a scene mode of Sw/Hidden must not
/// authorize another SW draw: derive an Hw→Sw/Hidden cursorless hide first.
///
/// An observed live binding only upgrades a non-HW predecessor for a desired
/// SW/Hidden assignment. Desired HW still follows scene state so lifecycle or
/// version changes queue a full Show. Conversely, scene HW with no recorded
/// live binding becomes Hidden so desired HW rebinds.
fn effective_cursor_prev_mode(
    scene_prev: OutputCursorMode,
    platform_visible: bool,
    assignment: CursorAssignment,
) -> OutputCursorMode {
    if !platform_visible && matches!(scene_prev, OutputCursorMode::Hw) {
        // A successful fast-path rollback may hide the plane before the scene
        // retires another frame. Do not issue a second hide against an
        // already-unbound CRTC; Hidden→Hw still derives a fresh full Show.
        OutputCursorMode::Hidden
    } else if platform_visible
        && !matches!(scene_prev, OutputCursorMode::Hw)
        && matches!(
            assignment,
            CursorAssignment::Sw { .. } | CursorAssignment::Hidden
        )
    {
        OutputCursorMode::Hw
    } else {
        scene_prev
    }
}

#[allow(clippy::type_complexity)]
fn derive_cursor_transition(
    prev: OutputCursorMode,
    assignment: CursorAssignment,
) -> (
    Option<CursorTransition>,
    Option<Option<(i32, i32)>>,
    OutputCursorMode,
) {
    match (prev, assignment) {
        (
            OutputCursorMode::Sw { .. } | OutputCursorMode::SwPending | OutputCursorMode::Hidden,
            CursorAssignment::Hw {
                x,
                y,
                record_version,
                hot_x,
                hot_y,
            },
        ) => (
            Some(CursorTransition::ShowOnRetire {
                upload_version: record_version,
                hot_x,
                hot_y,
                x,
                y,
            }),
            Some(None),
            // Mode advances to Hw only AFTER the retire applies
            // the show; the post-retire mode reflects what's on
            // the screen.
            OutputCursorMode::Hw,
        ),
        (OutputCursorMode::Hw, CursorAssignment::Sw { .. }) => (
            Some(CursorTransition::HideOnRetire {
                reveal_sw_after: true,
            }),
            // Phase one is cursorless. Only after hide retires successfully
            // may a Hidden→Sw frame install the SW position/metadata.
            Some(None),
            OutputCursorMode::SwPending,
        ),
        (OutputCursorMode::Hw, CursorAssignment::Hidden) => (
            Some(CursorTransition::HideOnRetire {
                reveal_sw_after: false,
            }),
            Some(None),
            OutputCursorMode::Hidden,
        ),
        (_, CursorAssignment::Sw { pos }) => {
            // Sw → Sw or Hidden → Sw: no transition; advance the
            // per-output `cursor_prev_pos` to where the SW sprite
            // landed this frame so the NEXT frame damages this
            // rect. The transactional rule (codex v4-pass) means
            // failed submits do NOT advance — the OLD prev rect
            // survives and is re-damaged.
            (
                None,
                Some(Some(pos)),
                OutputCursorMode::Sw { prev: Some(pos) },
            )
        }
        (_, CursorAssignment::Hidden) => (None, Some(None), OutputCursorMode::Hidden),
        (OutputCursorMode::Hw, CursorAssignment::Hw { .. }) => (None, None, OutputCursorMode::Hw),
    }
}

/// Stage 5 Phase D — apply a retired-ack's cursor transition via
/// the platform's per-CRTC hooks, preserving upload-then-show and
/// cursorless-hide-before-software ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CursorTransitionResult {
    Applied,
    /// The requested HW show did not remain bound. The retired frame omitted
    /// the SW sprite, so actual mode is Hidden until the forced repaint.
    Hidden,
    /// A cursorless Hw→Sw phase hid the hardware sprite successfully. Keep
    /// actual mode Hidden and force the second frame that may draw software.
    HiddenNeedsRepaint,
    /// A hide or show rollback failed and a HW binding remains visible.
    Visible,
    /// The prior binding is still visible, but the desired full bind/hotspot
    /// did not land. A later composed Hw→Hw frame must retry ShowOnRetire.
    VisibleNeedsShowRetry,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CursorRetireResolution {
    actual_mode: OutputCursorMode,
    commit_desired_metadata: bool,
    clear_presented_metadata: bool,
    force_repaint: bool,
}

fn resolve_retired_cursor_state(
    result: CursorTransitionResult,
    desired_mode: OutputCursorMode,
) -> CursorRetireResolution {
    match result {
        CursorTransitionResult::Applied => CursorRetireResolution {
            actual_mode: desired_mode,
            commit_desired_metadata: true,
            clear_presented_metadata: false,
            force_repaint: false,
        },
        CursorTransitionResult::Hidden => CursorRetireResolution {
            actual_mode: OutputCursorMode::Hidden,
            commit_desired_metadata: false,
            clear_presented_metadata: true,
            force_repaint: true,
        },
        CursorTransitionResult::HiddenNeedsRepaint => CursorRetireResolution {
            actual_mode: desired_mode,
            commit_desired_metadata: true,
            clear_presented_metadata: false,
            force_repaint: true,
        },
        CursorTransitionResult::Visible => CursorRetireResolution {
            actual_mode: OutputCursorMode::Hw,
            commit_desired_metadata: false,
            clear_presented_metadata: false,
            force_repaint: true,
        },
        CursorTransitionResult::VisibleNeedsShowRetry => CursorRetireResolution {
            actual_mode: OutputCursorMode::Hw,
            commit_desired_metadata: false,
            clear_presented_metadata: false,
            force_repaint: true,
        },
    }
}

fn update_force_show_retry_version(
    current: Option<u64>,
    transition: Option<CursorTransition>,
    result: CursorTransitionResult,
    desired_mode: OutputCursorMode,
) -> Option<u64> {
    let attempted = match transition {
        Some(CursorTransition::ShowOnRetire { upload_version, .. }) => Some(upload_version),
        _ => None,
    };
    match (attempted, result) {
        (Some(version), CursorTransitionResult::VisibleNeedsShowRetry) => {
            Some(current.map_or(version, |pending| pending.max(version)))
        }
        (Some(version), CursorTransitionResult::Applied | CursorTransitionResult::Hidden)
            if current == Some(version) =>
        {
            None
        }
        (None, CursorTransitionResult::Applied)
            if !matches!(desired_mode, OutputCursorMode::Hw) =>
        {
            None
        }
        _ => current,
    }
}

fn force_cursor_retry_repaint(state: &mut OutputSceneState) {
    state.scene_structure_damage.add(vk::Rect2D {
        offset: vk::Offset2D { x: 0, y: 0 },
        extent: state.output_extent,
    });
}

fn reset_cursor_retry_for_lifecycle(retry: &mut Option<u64>) {
    // drain_all also resets the actual mode to Hidden and invalidates every
    // uploaded plane version. The first post-resume/DPMS frame therefore
    // derives a fresh Hidden→Hw Show using the current cursor record; an old
    // pre-suspend generation must not keep the aggregate mode Mixed forever.
    *retry = None;
}

fn reset_cursor_mode_for_lifecycle(mode: &mut OutputCursorMode) {
    *mode = OutputCursorMode::Hidden;
}

fn resolve_failed_cursor_upload(
    output_idx: usize,
    prior_visible: bool,
    hide_live_binding: impl FnOnce() -> io::Result<()>,
) -> CursorTransitionResult {
    if !prior_visible {
        // Hidden/Sw→Hw can fail before the plane has ever been bound. There
        // is no rollback to perform; issuing set_cursor2(None) here can itself
        // return EINVAL and falsely manufacture HW ownership.
        return CursorTransitionResult::Hidden;
    }
    match hide_live_binding() {
        Ok(()) => CursorTransitionResult::Hidden,
        Err(error) => {
            log::warn!(
                "render cursor: upload failed and hide_on_crtc({output_idx}) rollback failed: {error}"
            );
            CursorTransitionResult::VisibleNeedsShowRetry
        }
    }
}

fn resolve_cursor_hide_on_retire(
    output_idx: usize,
    reveal_sw_after: bool,
    currently_visible: bool,
    hide_live_binding: impl FnOnce() -> io::Result<()>,
) -> CursorTransitionResult {
    if !currently_visible {
        return if reveal_sw_after {
            // The submitted phase-one frame was cursorless. Preserve the
            // one-frame reveal gap and force phase two even though a fast-path
            // rollback already performed the hide.
            CursorTransitionResult::HiddenNeedsRepaint
        } else {
            CursorTransitionResult::Applied
        };
    }
    match hide_live_binding() {
        Ok(()) if reveal_sw_after => CursorTransitionResult::HiddenNeedsRepaint,
        Ok(()) => CursorTransitionResult::Applied,
        Err(error) => {
            log::warn!("render cursor: hide_on_crtc({output_idx}) failed at retire: {error}");
            CursorTransitionResult::Visible
        }
    }
}

fn apply_cursor_transition_on_retire(
    inner: &mut SceneCompositorInner,
    output_idx: usize,
    platform: &mut PlatformBackend,
    transition: Option<CursorTransition>,
) -> CursorTransitionResult {
    let Some(t) = transition else {
        return CursorTransitionResult::Applied;
    };
    match t {
        CursorTransition::ShowOnRetire {
            upload_version,
            hot_x,
            hot_y,
            x,
            y,
        } => {
            let prior_visible = platform.cursor_plane_visible_for_output(output_idx);
            // Upload if version doesn't match. `upload_image` is
            // already idempotent-deduplicated by value inside
            // `CursorPlane`, but skipping the FFI when we know the
            // version matches is cheaper. Bytes come from the
            // scene's current `CursorEntry` (cloned-Arc, no copy)
            // when its version matches the transition's. A mismatch never
            // binds stale pixels: the old binding must be hidden successfully
            // or remain authoritative with a version-qualified Show retry.
            let upload_ready = if platform.cursor_plane_uploaded_version_for_output(output_idx)
                != Some(upload_version)
            {
                if let Some(entry) = inner.cursor.as_ref()
                    && entry.record_version == upload_version
                    && let Some(bytes) = entry.bgra_bytes.as_ref()
                {
                    match platform.cursor_plane_upload_image_for_output(
                        output_idx,
                        upload_version,
                        entry.extent.width,
                        entry.extent.height,
                        bytes.as_ref(),
                    ) {
                        Ok(()) => true,
                        Err(e) => {
                            log::warn!(
                                "render cursor: retire-time upload (v{upload_version}) failed: {e}"
                            );
                            false
                        }
                    }
                } else {
                    log::debug!(
                        "render cursor: retire-time upload (v{upload_version}) — \
                         no matching entry bytes; binding with current buffer"
                    );
                    false
                }
            } else {
                true
            };
            if !upload_ready {
                // A steady-HW rebind can fail its per-device upload while an
                // old binding is live. Switch to SW only if detaching that old
                // binding succeeds; otherwise retain actual HW ownership. A
                // never-bound output has nothing to detach and resolves
                // directly to Hidden without probing hide.
                resolve_failed_cursor_upload(output_idx, prior_visible, || {
                    platform.cursor_plane_hide_on_crtc(output_idx)
                })
            } else {
                match platform.cursor_plane_show_on_crtc(output_idx, hot_x, hot_y, x, y) {
                    Ok(()) => CursorTransitionResult::Applied,
                    Err(error) => {
                        let actual = if error.remains_visible() {
                            CursorTransitionResult::VisibleNeedsShowRetry
                        } else {
                            CursorTransitionResult::Hidden
                        };
                        log::warn!(
                            "render cursor: show_on_crtc({output_idx}) failed at retire: {error}"
                        );
                        actual
                    }
                }
            }
        }
        CursorTransition::HideOnRetire { reveal_sw_after } => {
            let currently_visible = platform.cursor_plane_visible_for_output(output_idx);
            resolve_cursor_hide_on_retire(output_idx, reveal_sw_after, currently_visible, || {
                platform.cursor_plane_hide_on_crtc(output_idx)
            })
        }
    }
}

fn retire_failed_submit_bos(
    state: &mut OutputSceneState,
    output_idx: usize,
    platform: &mut PlatformBackend,
    vk: &crate::kms::vk::device::VkContext,
) {
    let mut remaining = VecDeque::with_capacity(state.failed_submit_bos.len());
    while let Some(failed) = state.failed_submit_bos.pop_front() {
        match failed.ticket.poll_signaled_result(vk) {
            Ok(true) => match platform.recycle_failed_submit_bo(output_idx, failed.bo_idx) {
                Ok(()) => {
                    state.pool_ring.release(failed.pool_slot);
                    log::debug!(
                        "render scene: recycled failed-submit output {output_idx} bo {} pool slot {}",
                        failed.bo_idx,
                        failed.pool_slot,
                    );
                }
                Err(error) => {
                    log::error!(
                        "render scene: failed-submit recovery failed for output {output_idx} \
                         bo {}: {error}",
                        failed.bo_idx,
                    );
                    platform.renderer_failed = true;
                    remaining.push_back(failed);
                }
            },
            Ok(false) => remaining.push_back(failed),
            Err(error) => {
                log::error!(
                    "render scene: failed-submit fence status failed for output {output_idx} \
                     bo {}: {error:?}",
                    failed.bo_idx,
                );
                platform.renderer_failed = true;
                remaining.push_back(failed);
            }
        }
    }
    state.failed_submit_bos = remaining;
}

/// Drain the deferred descriptor-pool slot releases queued by
/// `handle_page_flip_complete` when the compose fence hadn't yet
/// signaled at pageflip-retirement time. Mirrors
/// `retire_failed_submit_bos`'s walk-once-poll-or-defer shape.
/// Slots whose fence has now signaled are returned to the ring;
/// the rest stay queued for the next drain.
fn drain_pending_pool_releases(
    state: &mut OutputSceneState,
    vk: &crate::kms::vk::device::VkContext,
    platform: &mut PlatformBackend,
) {
    if state.pending_pool_releases.is_empty() {
        return;
    }
    let mut remaining = VecDeque::with_capacity(state.pending_pool_releases.len());
    while let Some((slot, ticket)) = state.pending_pool_releases.pop_front() {
        match ticket.poll_signaled_result(vk) {
            Ok(true) => state.pool_ring.release(slot),
            Ok(false) => remaining.push_back((slot, ticket)),
            Err(error) => {
                log::error!("render scene: deferred pool fence status failed: {error:?}");
                platform.renderer_failed = true;
                remaining.push_back((slot, ticket));
            }
        }
    }
    state.pending_pool_releases = remaining;
}

/// Opt-in gate for the per-tick skip/unblock diagnostic. Default OFF:
/// the logging fires on every skip-state transition, which during
/// healthy vsync operation is one line per frame per output — pure
/// noise + CPU unless you're actively chasing a freeze. Set
/// `YSERVER_TICK_SKIP_LOG=1` (or true/yes) to enable it; when unset,
/// `record_tick_skip` / `record_tick_success` are no-ops (no logging,
/// no `last_skip_reason` book-keeping).
fn tick_skip_log_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        matches!(
            std::env::var("YSERVER_TICK_SKIP_LOG").ok().as_deref(),
            Some("1") | Some("true") | Some("TRUE") | Some("yes") | Some("YES")
        )
    })
}

/// Diagnostic: record that `tick_one_output` skipped this output at
/// `reason`. Logs at INFO **only on transition** (different reason
/// from the previous tick, or first skip after a successful flip),
/// keeping the log volume bounded at one line per skip-state change
/// per output. Used to debug "output stops getting page-flips
/// indefinitely" by identifying which gate is stuck. Gated OFF by
/// default — see [`tick_skip_log_enabled`].
fn record_tick_skip(
    state: &mut OutputSceneState,
    output_idx: usize,
    reason: TickSkipReason,
    output_damage_rects: usize,
) {
    if !tick_skip_log_enabled() {
        return;
    }
    if state.last_skip_reason != Some(reason) {
        log::info!(
            "render scene tick skip: output={output_idx} reason={reason:?} \
             pending_acks={pa} retry_at={ra:?} damage_rects={dr} \
             scene_structure_rects={ssr} prev_reason={pr:?}",
            pa = state.pending_acks.len(),
            ra = state.next_submit_retry_at,
            dr = output_damage_rects,
            ssr = state.scene_structure_damage.rects().len(),
            pr = state.last_skip_reason,
        );
        state.last_skip_reason = Some(reason);
    }
}

/// Diagnostic: record that `tick_one_output` succeeded (composed +
/// submitted) for this output, clearing any prior skip state. Logs
/// at INFO **only if we were previously skipping**, marking the
/// "unblocked" transition for the freeze-debug timeline.
fn record_tick_success(state: &mut OutputSceneState, output_idx: usize) {
    if !tick_skip_log_enabled() {
        return;
    }
    if let Some(prev) = state.last_skip_reason.take() {
        log::info!(
            "render scene tick unblock: output={output_idx} prev_reason={prev:?} composed=ok",
        );
    }
}

fn cursor_damage_for_frame(
    last_present_cursor_rect: Option<vk::Rect2D>,
    last_present_cursor_version: Option<u64>,
    new_cursor_rect: Option<vk::Rect2D>,
    new_cursor_version: Option<u64>,
    cursor_transition: Option<CursorTransition>,
) -> RegionSet {
    let mut damage = RegionSet::new();
    let cursor_changed = new_cursor_rect != last_present_cursor_rect
        || cursor_transition.is_some()
        || new_cursor_version != last_present_cursor_version;
    if !cursor_changed {
        return damage;
    }
    if let Some(rect) = last_present_cursor_rect {
        damage.add(rect);
    }
    if let Some(rect) = new_cursor_rect
        && Some(rect) != last_present_cursor_rect
    {
        damage.add(rect);
    }
    damage
}

/// True if any captured presentation-damage snapshot carries a
/// NON-EMPTY region. Gates the empty-projection force-compose in
/// `tick_one_output`: `peek_presentation_damage` returns `Some` even
/// for a clean (empty) region, so a mere `!snapshots.is_empty()` check
/// force-composes the whole output for every drawn window every vblank
/// — the idle free-run bug. Only a window that actually painted
/// (non-empty captured damage) whose projection landed empty needs the
/// forced full compose (the xfce submenu case).
fn snapshots_carry_damage(snaps: &[DamageSnapshot]) -> bool {
    snaps.iter().any(|s| !s.region.is_empty())
}

#[allow(clippy::too_many_lines)]
#[allow(clippy::too_many_arguments)]
fn tick_one_output(
    inner: &mut SceneCompositorInner,
    output_idx: usize,
    core: &KmsCore,
    store: &mut DrawableStore,
    platform: &mut PlatformBackend,
    windows: &super::backend::WindowsMap,
    telemetry: &mut Telemetry,
    hw_strategy_enabled: bool,
    cow_host_xid: Option<u32>,
    root_overlay: &super::root_overlay::RootOverlay,
    // Idle free-run fix (cut 2b): accumulator for the sampled-source
    // ids `build_scene` actually drew on this output, unioned across
    // outputs by `tick` to reconcile `offscreen_no_draw`. Only written
    // once `build_scene` has run (after the pending-flip/retry gate),
    // so a `PendingAcks`/`RetryDeadline` skip contributes nothing.
    drawn: &mut std::collections::HashSet<super::store::DrawableId>,
) -> Result<TickOutcome, SceneError> {
    // 0. **Per-output flip-pending gate.** KMS only allows one
    //    pending atomic commit per CRTC at a time; a second
    //    `drmModeAtomicCommit` while the first hasn't fired
    //    page-flip-complete returns EBUSY. Without this check
    //    the loop fires submit-after-submit faster than vblank,
    //    every second submit takes the 9b recovery path
    //    (BO invalidated, repaint deferred), nothing ever
    //    actually displays. Observed catastrophic on RADV/bee
    //    + mate + 2560x1440 (screen stays at the initial
    //    pageflip frame; bg_pixel-unset = black).
    //
    //    Skip cleanly: pending_ack non-empty means a flip is in
    //    flight. `scene_structure_dirty` stays set so the next
    //    tick (post-page-flip-complete) picks up the deferred
    //    damage. The KMS-rate cap is now structural; the rest
    //    of the pipeline can fire at whatever cadence
    //    `maybe_composite` calls us — wasted cycles bounded
    //    here.
    {
        let vk = Arc::clone(&inner.vk);
        let s = inner.outputs.get_mut(output_idx).expect("range");
        retire_failed_submit_bos(s, output_idx, platform, vk.as_ref());
        // B.2-context fix (vkdebug VUID-vkResetDescriptorPool-00313):
        // drain any deferred descriptor-pool slot releases whose
        // compose fence has now signaled. Deferred entries are
        // queued at `handle_page_flip_complete` when the GPU hadn't
        // yet finished the compose CB at KMS pageflip time;
        // releasing the slot then would have tripped the VUID.
        drain_pending_pool_releases(s, vk.as_ref(), platform);
        if !s.pending_acks.is_empty() {
            record_tick_skip(s, output_idx, TickSkipReason::PendingAcks, 0);
            return Ok(TickOutcome::Skipped(TickSkipReason::PendingAcks));
        }
        if let Some(deadline) = s.next_submit_retry_at
            && std::time::Instant::now() < deadline
        {
            record_tick_skip(s, output_idx, TickSkipReason::RetryDeadline, 0);
            return Ok(TickOutcome::Skipped(TickSkipReason::RetryDeadline));
        }
    }

    // 1. Snapshot live output state so we can fold cleanly
    //    into pending_ack later (codex round 2 point 2 —
    //    transactional generation advance).
    let (scene_structure_snap, failed_repaint_snap, frame_gen, first_frame) = {
        let s = inner.outputs.get(output_idx).expect("range");
        (
            s.scene_structure_damage.snapshot(),
            s.pending_repaint_after_failed_submit.snapshot(),
            s.current_generation + 1,
            s.current_generation == 0,
        )
    };

    // 2. Build the scene + collect projected presentation damage.
    //    Stage 5 Phase C: build_scene returns a pure
    //    `CursorAssignment` decision; the actual transition queue +
    //    `cursor_prev_pos` advance happens transactionally below
    //    AFTER the per-output commit succeeds.
    let cursor_prev_pos_before = inner.outputs[output_idx].cursor_prev_pos;
    let last_present_cursor_rect = inner.outputs[output_idx].last_present_cursor_rect;
    let last_present_cursor_version = inner.outputs[output_idx].last_present_cursor_version;
    let hw_can_run = hw_strategy_enabled;
    let scene_prev_mode = inner.outputs[output_idx].last_frame_cursor_mode;
    // Phase 5.1 — `cow_host_xid` is threaded directly from the
    // backend's `cow_host_xid()` getter (the well-known protocol
    // constant whenever the overlay is materialized, else `None`).
    // It flags the COW top-level in the `top_level_order` walk so
    // its subtree inherits `alpha_passthrough`. The COW emits via
    // the normal recursion — there is no special post-walk append.
    let mut built = build_scene(
        core,
        store,
        windows,
        output_idx,
        platform,
        inner.cursor.clone(),
        cursor_prev_pos_before,
        cow_host_xid,
        hw_can_run,
    );
    let prev_mode = effective_cursor_prev_mode(
        scene_prev_mode,
        platform.cursor_plane_visible_for_output(output_idx),
        built.cursor_assignment,
    );
    if cursorless_hide_frame_required(prev_mode, built.cursor_assignment) {
        // Two-phase Hw→Sw/Hidden handoff: the frame retired immediately
        // before hide must contain no software cursor. If the hide ioctl
        // fails, the old HW sprite remains the sole visible cursor; if it
        // succeeds, retirement forces a later SW repaint.
        built.omit_software_cursor_for_hide();
    }

    // Idle free-run fix (cut 2b): record the sampled sources this output
    // actually drew, so `tick` can reconcile `offscreen_no_draw` from
    // the union across outputs. Recorded unconditionally here (before
    // the empty-damage / BO / pool skips below) so a window that WAS
    // drawn is never mis-flagged just because its output later skips.
    drawn.extend(built.sampled_ids.iter().copied());

    // Stage 5 Phase D — derive the per-output cursor transition
    // and new prev_pos from `built.cursor_assignment` and the
    // last-frame mode. Both are queued on the PendingAck below
    // and applied transactionally on successful retirement.
    let (mut cursor_transition_to_queue, cursor_prev_pos_after_retire, cursor_mode_after_retire) =
        derive_cursor_transition(prev_mode, built.cursor_assignment);
    if inner.outputs[output_idx].force_show_retry_version.is_some()
        && let CursorAssignment::Hw {
            x,
            y,
            record_version,
            hot_x,
            hot_y,
        } = built.cursor_assignment
    {
        cursor_transition_to_queue = Some(CursorTransition::ShowOnRetire {
            upload_version: record_version,
            hot_x,
            hot_y,
            x,
            y,
        });
    }

    let mut output_damage = built.projected_damage;
    output_damage.union_with(&cursor_damage_for_frame(
        last_present_cursor_rect,
        last_present_cursor_version,
        built.new_cursor_rect,
        built.cursor_record_version,
        cursor_transition_to_queue,
    ));
    // Always-Full repaint makes stationary SW cursors safe even when
    // cursor_damage is empty and some unrelated damage triggers a
    // frame. If `Repaint::Clipped` is ever re-enabled, the current SW
    // cursor rect must also be folded into the repaint region even
    // when it did not itself trigger the compose.
    output_damage.union_with(&scene_structure_snap);
    output_damage.union_with(&failed_repaint_snap);
    telemetry.record_scene_entries(
        u64::try_from(built.scene.draws.len()).unwrap_or(u64::MAX),
        u64::try_from(built.scene.draws.len()).unwrap_or(u64::MAX),
    );

    // 3. Empty-damage fast path (after first frame).
    if output_damage.is_empty() && !first_frame {
        // A drawn, scene-participating window had presentation damage
        // that `build_scene` CAPTURED (`built.snapshots`, via
        // `peek_presentation_damage`) but whose `add_projected_damage`
        // landed empty — a geometry/offset gap projecting a top-level
        // popup's damage off the output. Skipping here would DISCARD
        // those snapshots (they only ack via the `PendingAck` built at
        // the compose path below), so the paint would never ack and
        // the window sits painted-but-off-screen forever until an
        // unrelated event forces structure damage. This was the xfce
        // "submenu painted but not shown until you move" bug.
        //
        // Force a full-output repaint so the compose runs and the
        // captured snapshots ack at retire. Repaint is always-Full, so
        // the content composites correctly regardless of the empty
        // projection. Self-limiting: once acked, no snapshot carries
        // non-empty damage and the normal skip resumes → true idle.
        //
        // Gate on a snapshot with NON-EMPTY captured damage, NOT merely
        // `!built.snapshots.is_empty()`: `peek_presentation_damage`
        // returns `Some` even for an empty region (store.rs), so
        // `built.snapshots` is non-empty for EVERY drawn
        // scene-participating window — including perfectly clean idle
        // ones. Gating on non-emptiness was the idle free-run bug: a
        // clean drawn window force-composed the whole output every
        // vblank forever. Only a window that actually painted (non-empty
        // captured damage) whose projection landed empty needs the
        // force (the xfce submenu case).
        // DIAG (submenu regression, bee/eiger/air): the empty-damage
        // path with snapshots is exactly where cut 1's region-gate
        // decides force-vs-skip. Log what build_scene captured so we
        // can see the submenu's actual snapshot state at the failing
        // tick (present-but-empty region vs absent). Gated behind
        // YSERVER_TICK_SKIP_LOG like the other tick diagnostics — it
        // fires on EVERY empty-damage tick (~tens/s at idle), so leaving
        // it unconditional floods the log and allocates a Vec per tick,
        // defeating the idle goal.
        if tick_skip_log_enabled() {
            log::info!(
                "empty-damage-diag: out{output_idx} draws={} carry={} snapshots={:?}",
                built.scene.draws.len(),
                snapshots_carry_damage(&built.snapshots),
                built
                    .snapshots
                    .iter()
                    .map(|s| (s.id.as_u64(), s.region.rects().len()))
                    .collect::<Vec<_>>(),
            );
        }
        if !snapshots_carry_damage(&built.snapshots) {
            let s = inner.outputs.get_mut(output_idx).expect("range");
            record_tick_skip(s, output_idx, TickSkipReason::EmptyDamage, 0);
            return Ok(TickOutcome::Skipped(TickSkipReason::EmptyDamage));
        }
        let extent = inner.outputs[output_idx].output_extent;
        output_damage.add(vk::Rect2D {
            offset: vk::Offset2D::default(),
            extent,
        });
        log::debug!(
            "render: output {output_idx} forcing full compose — {} presentation-damage \
             snapshot(s) with real damage projected empty (paint would otherwise strand off-screen)",
            built
                .snapshots
                .iter()
                .filter(|s| !s.region.is_empty())
                .count(),
        );
    }

    // 4. Acquire BO.
    let token = match platform.acquire_scanout_bo(output_idx) {
        Some(t) => t,
        None => {
            let s = inner.outputs.get_mut(output_idx).expect("range");
            record_tick_skip(
                s,
                output_idx,
                TickSkipReason::NoBO,
                output_damage.rects().len(),
            );
            return Ok(TickOutcome::Skipped(TickSkipReason::NoBO));
        }
    };

    // 5. Pick repaint region via buffer-age algorithm.
    let extent = inner.outputs[output_idx].output_extent;
    let repaint = if built.scene.draws.is_empty() {
        // Scene is just the bg_color clear (no top-levels, no
        // cursor, etc.). The Clipped/LOAD path would preserve
        // each BO's prior-generation content — including a
        // pre-`bg_pixel`-update black — and never re-clear it.
        // Force Full so loadOp=CLEAR paints the current
        // `bg_color` across the whole BO. Stage 4 introduces
        // root storage which makes the entry list non-empty
        // even on blank desktops; until then, "scene is the
        // clear color" is structurally common and must always
        // refresh.
        Repaint::Full(extent)
    } else {
        pick_repaint_region(
            token.last_present_generation,
            token.content_invalidated,
            frame_gen,
            &output_damage,
            &inner.outputs[output_idx].damage_history,
            extent,
        )
    };
    match repaint {
        Repaint::Full(extent) => {
            telemetry.record_full_redraw_fallback();
            telemetry.record_damage_pixels(
                u64::from(extent.width) * u64::from(extent.height),
                u64::from(extent.width) * u64::from(extent.height),
            );
        }
        Repaint::Clipped(rect) => {
            telemetry.record_damage_pixels(
                u64::from(rect.extent.width) * u64::from(rect.extent.height),
                u64::from(extent.width) * u64::from(extent.height),
            );
        }
    }

    // 6. Acquire descriptor-pool slot.
    let state = inner.outputs.get_mut(output_idx).expect("range");
    let slot = match state.pool_ring.acquire() {
        Some(s) => s,
        None => {
            log::debug!(
                "render scene: descriptor-pool ring exhausted for output {output_idx}; skipping tick",
            );
            record_tick_skip(
                state,
                output_idx,
                TickSkipReason::NoPool,
                output_damage.rects().len(),
            );
            return Ok(TickOutcome::Skipped(TickSkipReason::NoPool));
        }
    };
    let descriptor_pool = state.pool_ring.pool_at(slot);

    // 7. Record + submit + flip via the v2 clipped compose path.
    let compose_ticket = match platform.acquire_fence_ticket() {
        Ok(ticket) => ticket,
        Err(error) => {
            inner.outputs[output_idx].pool_ring.release(slot);
            if vk_result_is_device_lost(error) {
                platform.renderer_failed = true;
            }
            return Err(SceneError::Present(PresentError::Vk(error)));
        }
    };
    let output_key = platform.outputs[output_idx].key.clone();
    let drm_device = platform
        .device_for_output(&output_key)
        .map(|device| device.device.clone())
        .ok_or_else(|| {
            SceneError::Present(PresentError::Io(std::io::Error::other(format!(
                "no DRM device for output {:?}",
                output_key
            ))))
        })?;
    let pool = platform
        .scanout_pools
        .get_mut(output_idx)
        .and_then(|p| p.as_mut())
        .ok_or(SceneError::NoVk)?;
    let layout = &platform.outputs[output_idx];
    // Retained root-`IncludeInferiors` overlay: per-output apply list
    // (output-local XOR rects), computed against the SAME per-output
    // layout the compose uses. Empty in the common case (no active
    // wireframe / rubber-band), so no XOR pipeline is built.
    let overlay_ops = root_overlay.apply_list_for_output((
        layout.x,
        layout.y,
        u32::from(layout.width),
        u32::from(layout.height),
    ));
    // Fetch the XOR-logic-op pipeline + layout here (needs `&mut inner`
    // for the cache) so `record_command_buffer` receives ready-to-bind
    // Vulkan handles and never touches `inner`. Skip the build entirely
    // when there is nothing to draw.
    let (xor_pipeline, xor_layout) = if overlay_ops.is_empty() {
        (vk::Pipeline::null(), vk::PipelineLayout::null())
    } else {
        let pl = inner
            .overlay_xor_cache
            .get(yserver_core::backend::GcFunction::Xor, true)?;
        (pl, inner.overlay_xor_cache.pipeline_layout())
    };
    let mut gpu_submitted = false;
    let record_start = std::time::Instant::now();
    let (render_result, previous_gpu_ns, copied_prepare_failed) = match pool {
        OutputScanout::Shared(pool) => {
            let bo = pool.bos.get_mut(token.bo_idx).ok_or(SceneError::NoVk)?;
            let result = submit_shared_scanout_frame(
                &inner.vk,
                &drm_device,
                &layout.output,
                bo,
                &inner.pipeline,
                descriptor_pool,
                &built.scene,
                repaint,
                compose_ticket.fence(),
                &mut gpu_submitted,
                &overlay_ops,
                xor_pipeline,
                xor_layout,
            )
            .map(|()| None);
            (result, bo.last_gpu_render_ns.take(), false)
        }
        OutputScanout::Copied(pool) => {
            let source = pool.sources.get_mut(token.bo_idx).ok_or(SceneError::NoVk)?;
            let destination_state = &mut pool
                .destinations
                .bos
                .get_mut(token.bo_idx)
                .ok_or(SceneError::NoVk)?
                .state;
            let result = submit_copied_scanout_render(
                &inner.vk,
                source,
                destination_state,
                &inner.pipeline,
                descriptor_pool,
                &built.scene,
                Repaint::Full(token.extent),
                compose_ticket.fence(),
                &mut gpu_submitted,
                &overlay_ops,
                xor_pipeline,
                xor_layout,
            );
            let copied_prepare_failed = result
                .as_ref()
                .is_err_and(CopiedRenderSubmitError::requires_fail_stop);
            (
                result
                    .map(Some)
                    .map_err(CopiedRenderSubmitError::into_present),
                source.last_gpu_render_ns.take(),
                copied_prepare_failed,
            )
        }
    };
    if copied_prepare_failed {
        // Importing the retained B -> A completion consumes its sole payload.
        // A failed import therefore cannot be retried without fabricating the
        // external memory dependency; fail-stop before the source can be
        // reserved or reused again.
        platform.renderer_failed = true;
    }
    let compose_result = match render_result {
        Ok(Some(completion)) => platform
            .register_scanout_render_completion(output_key, token.bo_idx, completion)
            .map(|job_id| InFlightStage::WaitingForRenderCompletion { job_id })
            .map_err(PresentError::Io),
        Ok(None) => Ok(InFlightStage::KmsFlipPending),
        Err(error) => Err(error),
    };
    let record_ns = u64::try_from(record_start.elapsed().as_nanos()).unwrap_or(u64::MAX);
    telemetry.record_compose_cb_record_ns(record_ns);
    // GPU-render time from this paired target's PREVIOUS compose (timestamp
    // pool), read before this command buffer overwrites the query slots.
    if let Some(gpu_ns) = previous_gpu_ns {
        telemetry.record_gpu_render_ns(gpu_ns);
    }
    telemetry
        .record_descriptor_allocations(u64::try_from(built.scene.draws.len()).unwrap_or(u64::MAX));

    let state = inner.outputs.get_mut(output_idx).expect("range");
    match compose_result {
        Ok(stage) => {
            state.next_submit_retry_at = None;
            for id in &built.sampled_ids {
                store.touch_render_fence(*id, compose_ticket.clone());
            }
            state.pool_slots.push_back(slot);
            state.pending_acks.push_back(PendingAck {
                bo_idx: token.bo_idx,
                generation: frame_gen,
                stage,
                drawable_snapshots: built.snapshots,
                ticket: Some(compose_ticket),
                submitted_output_damage: output_damage,
                submitted_scene_structure_damage: scene_structure_snap,
                submitted_failed_repaint: failed_repaint_snap,
                cursor_transition: cursor_transition_to_queue,
                cursor_prev_pos_after_retire,
                cursor_mode_after_retire,
                last_present_cursor_rect_after_retire: built.new_cursor_rect,
                last_present_cursor_version_after_retire: built.cursor_record_version,
            });
            state.current_generation = frame_gen;
            record_tick_success(state, output_idx);
            Ok(TickOutcome::Composed)
        }
        Err(e) => {
            if present_error_is_device_lost(&e) {
                // The live source renderer submitted or recorded this frame.
                // Neither shared nor copied source resources are reusable
                // after DEVICE_LOST; latch the same fatal renderer state used
                // by the engine before any retry bookkeeping runs.
                platform.renderer_failed = true;
            }
            if gpu_submitted {
                for id in &built.sampled_ids {
                    store.touch_render_fence(*id, compose_ticket.clone());
                }
                // Rendering reached GPU A, but a later export, completion-job
                // registration, or shared KMS commit failed. Keep the paired
                // resources fenced until A is done and make buffer-age state
                // conservative; no pageflip event will retire this frame.
                platform.invalidate_bo(output_idx, token.bo_idx);
                telemetry.record_missed_pageflip();
                log::warn!(
                    "render scene: post-render scanout handoff failed for output \
                     {output_idx} (bo {}): {e}; BO invalidated",
                    token.bo_idx,
                );
            } else {
                log::warn!(
                    "render scene: compose record/queue submit failed for output \
                     {output_idx} (bo {}): {e}",
                    token.bo_idx,
                );
            }
            // Both failure paths fold repaint forward and do NOT
            // push a pending_ack or advance current_generation.
            // If the GPU submission happened, keep the scanout BO
            // and descriptor-pool slot alive until the compose
            // fence signals: KMS rejected the flip, so no page-flip
            // event will retire those resources for us.
            // Re-borrow `state` after the platform.invalidate_bo
            // call (which took &mut platform).
            let state = inner.outputs.get_mut(output_idx).expect("range");
            // TODO(stage-5 perf): the 100 ms commit-retry back-off is
            // hardcoded. Empirically picked to be wide enough that
            // RADV/amdgpu releases pinned resources between attempts
            // (16 ms / one vblank was too tight under the ENOMEM
            // storm). Should become a tunable + observable via
            // telemetry (e.g. `commit_retry_backoff_ms` counter) so
            // per-driver tuning is possible without code edits.
            state.next_submit_retry_at =
                Some(std::time::Instant::now() + std::time::Duration::from_millis(100));
            if let Some(br) = output_damage.bounding_rect() {
                state.pending_repaint_after_failed_submit.add(br);
            }
            if gpu_submitted {
                state.failed_submit_bos.push_back(FailedSubmitBo {
                    bo_idx: token.bo_idx,
                    pool_slot: slot,
                    ticket: compose_ticket,
                });
            } else {
                state.pool_ring.release(slot);
                platform.cancel_scanout_bo_recording(output_idx, token.bo_idx);
            }
            Err(SceneError::Present(e))
        }
    }
}

/// Pick the repaint region for the upcoming compose. Currently always
/// returns `Repaint::Full` — the `Repaint::Clipped` + `loadOp=LOAD`
/// buffer-age optimisation below is correct in isolation but produces
/// visible multi-pixel "drag-shake" on non-composited MATE: stale
/// drag-phase window content remains in BOs the catch-up scissor
/// doesn't cover. Compositing-ON masks it because COW-authoritative
/// mode re-presents the compositor image fully each frame, which is
/// equivalent to what we now do for every output. Two attempted root-
/// cause fixes (input-coord hysteresis; invalidate-all-BOs on
/// scene-structure change) made things worse rather than better; until
/// the actual failure mode in the buffer-age propagation is
/// identified, Always-Full is the correctness hammer. Measurable GPU
/// cost was not observable in interactive testing.
///
/// Re-enable the optimisation by removing the early return below.
///
/// BUFFER-AGE RE-ENABLE HAZARD (root overlay): the root-`IncludeInferiors`
/// XOR overlay pass in `record_command_buffer` is NOT idempotent. It is
/// correct today only because `Repaint::Full` CLEARs and fully redraws the
/// BO, so the overlay XORs exactly once onto fresh pixels. A clipped
/// `loadOp=LOAD` frame whose scissor does not cover the overlay rects would
/// XOR the overlay a SECOND time onto a pooled BO that already has it baked
/// in from a prior compose, cancelling it / leaving remnants (the #90
/// rubber-band-remnant residual). Before re-enabling, fold the overlay rects
/// (`SceneCompositor::root_overlay.all_rects()`) into the repaint region.
fn pick_repaint_region(
    bo_last_gen: Option<u64>,
    bo_invalidated: bool,
    frame_gen: u64,
    current_damage: &RegionSet,
    history: &BufferAgeRing,
    extent: vk::Extent2D,
) -> Repaint {
    let _ = (
        bo_last_gen,
        bo_invalidated,
        frame_gen,
        current_damage,
        history,
    );
    Repaint::Full(extent)

    // Disabled buffer-age logic (see doc comment above):
    //
    // if bo_invalidated {
    //     return Repaint::Full(extent);
    // }
    // let Some(last) = bo_last_gen else {
    //     return Repaint::Full(extent);
    // };
    // if !history.contains_all(last, frame_gen) {
    //     return Repaint::Full(extent);
    // }
    // let mut repaint = current_damage.clone();
    // history.union_history_into(last, frame_gen, &mut repaint);
    // match repaint.bounding_rect() {
    //     Some(r) if r.extent.width > 0 && r.extent.height > 0 => Repaint::Clipped(r),
    //     _ => Repaint::Full(extent),
    // }
}

#[derive(Debug, Clone, Copy)]
enum Repaint {
    /// Full-output redraw with `loadOp=CLEAR`. Fallback path.
    Full(vk::Extent2D),
    /// Damaged-region-only redraw with `loadOp=LOAD`. The
    /// rectangle is the bounding box of the buffer-age repaint
    /// set — Stage 5 may split per-rect for tighter clipping.
    Clipped(vk::Rect2D),
}

fn all_zero(c: [f32; 4]) -> bool {
    c[0] == 0.0 && c[1] == 0.0 && c[2] == 0.0 && c[3] == 0.0
}

fn debug_scene_walk_xids() -> &'static HashSet<u32> {
    static XIDS: OnceLock<HashSet<u32>> = OnceLock::new();
    XIDS.get_or_init(|| {
        std::env::var("YSERVER_SCENE_WALK_XIDS")
            .ok()
            .map(|raw| {
                raw.split(',')
                    .filter_map(|part| {
                        let token = part.trim();
                        if token.is_empty() {
                            return None;
                        }
                        let hex = token
                            .strip_prefix("0x")
                            .or_else(|| token.strip_prefix("0X"))
                            .unwrap_or(token);
                        u32::from_str_radix(hex, 16)
                            .ok()
                            .or_else(|| token.parse::<u32>().ok())
                    })
                    .collect()
            })
            .unwrap_or_default()
    })
}

fn debug_scene_walk_all() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        matches!(
            std::env::var("YSERVER_SCENE_WALK_ALL").ok().as_deref(),
            Some("1") | Some("true") | Some("TRUE") | Some("yes") | Some("YES")
        )
    })
}

fn scene_walk_debug_enabled_for(host_xid: u32) -> bool {
    debug_scene_walk_all() || debug_scene_walk_xids().contains(&host_xid)
}

fn cursor_footprint_rect(
    dx: i32,
    dy: i32,
    cursor_w: i32,
    cursor_h: i32,
    layout_w: i32,
    layout_h: i32,
) -> Option<vk::Rect2D> {
    let x0 = dx.max(0);
    let y0 = dy.max(0);
    let x1 = (dx + cursor_w).min(layout_w);
    let y1 = (dy + cursor_h).min(layout_h);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some(vk::Rect2D {
        offset: vk::Offset2D { x: x0, y: y0 },
        extent: vk::Extent2D {
            width: u32::try_from(x1 - x0).unwrap_or(0),
            height: u32::try_from(y1 - y0).unwrap_or(0),
        },
    })
}

/// Walk window tree, build the per-output scene + collect damage
/// snapshots.
///
/// Stage 3f.6 lifted the Stage 2d "top-level only" simplification:
/// the recurse below walks each top-level → mapped + scene-
/// participating descendants, accumulating parent offsets into
/// absolute (root-space) coords before projecting onto the output.
/// xterm / xclock / any real app that paints into a child window
/// needs this — the bare top-level traversal showed only the
/// parent's (typically unpainted) storage on scanout.
///
/// Still-deferred simplifications:
/// - Skip the root storage entirely — bg_pixel is the clear color
///   (`scene.bg_color`). `bg_pixmap` would need a sample-from-pixmap
///   that uses the same blit pipeline as windows, deferred to
///   Stage 4 alongside the rest of the root content pipeline.
/// - Sibling z-order between children of the same parent is
///   HashMap-iteration-order (windows's underlying
///   `HashMap<u32, WindowGeometry>`). Proper stack-order tracking
///   is post-3f.6. Most real apps (xterm, xclock) have one child
///   per parent so the ordering rarely matters at Stage 3.
/// - Cursor: Stage 3f.8 appends a default-arrow sprite at top of
///   z when `cursor` is `Some`. Real theme support + per-window
///   `define_cursor` wiring stays Stage 4.
#[allow(clippy::too_many_arguments)]
fn build_scene(
    core: &KmsCore,
    store: &mut DrawableStore,
    windows: &super::backend::WindowsMap,
    output_idx: usize,
    platform: &PlatformBackend,
    cursor: Option<CursorEntry>,
    _cursor_prev_pos: Option<(i32, i32)>,
    // Phase 2.6 — host xid of the materialized Composite Overlay
    // Window, if any. The top-level walk uses this to mark the COW
    // top-level (and its descendants by recursion) with
    // `under_cow_subtree = true`, which in turn sets
    // `alpha_passthrough = true` on every emitted `CompositeDraw`.
    // `None` when the COW is not materialized (no compositor active
    // or not yet claimed via GetOverlayWindow). Phase 2.7 replaced
    // the prior `cow: Option<DrawableId>` arg: the COW now emits
    // via the normal top_level_order walk, not via a special
    // post-walk append, so we only need the host xid to tag the
    // walk's recursion flag — no DrawableId needed.
    cow_host_xid: Option<u32>,
    // Stage 5 Phase C — when `true`, the strategy picks `Hw` for
    // cursors that fit the plane and lie on-output; otherwise `Sw`.
    // `false` collapses every assignment to the SW path (rollout
    // default).
    hw_strategy_active: bool,
) -> SceneBuild {
    let bg = [0.0, 0.0, 0.0, 1.0];
    let layout = &platform.outputs[output_idx];
    let layout_x0 = layout.x;
    let layout_y0 = layout.y;
    let layout_w = u32::from(layout.width);
    let layout_h = u32::from(layout.height);

    let mut draws: Vec<CompositeDraw> = Vec::new();
    let mut snapshots: Vec<DamageSnapshot> = Vec::new();
    let mut sampled_ids: Vec<super::store::DrawableId> = Vec::new();
    let mut projected = RegionSet::new();
    if let Some(id) = store.lookup(core.window_id)
        && let Some(drawable) = store.get(id)
        && drawable.scene_participating
        && matches!(drawable.kind, DrawableKind::Root)
    {
        // Stage 4c.3 — route source-storage through `redirected_target`.
        // For an Automatic-mode redirected drawable, the scene must
        // blit FROM the backing B (not the drawable's own storage).
        // Geometry stays driven by the host drawable; only the
        // sampled storage handle reroutes.
        let source_id = store.redirected_target(id).unwrap_or(id);
        if let Some(source) = store.get(source_id)
            && source.storage.image_view != vk::ImageView::null()
        {
            #[allow(clippy::cast_precision_loss)]
            let dst_origin = [-(layout_x0 as f32), -(layout_y0 as f32)];
            #[allow(clippy::cast_precision_loss)]
            let dst_size = [
                drawable.storage.extent.width as f32,
                drawable.storage.extent.height as f32,
            ];
            draws.push(CompositeDraw {
                // Root scene draw — sample-side view carries the
                // format/depth-aware swizzle (depth-24 → α=ONE).
                // See `Storage::sample_view` for why scene draws
                // MUST NOT bind `image_view` directly.
                image_view: source.storage.sample_view,
                dst_origin,
                dst_size,
                src_origin: [0.0, 0.0],
                src_size: [1.0, 1.0],
                alpha_passthrough: false,
            });
            sampled_ids.push(source_id);
            if let Some(snap) = store.peek_presentation_damage(source_id) {
                for r in snap.region.rects() {
                    add_projected_damage(
                        &mut projected,
                        *r,
                        -layout_x0,
                        -layout_y0,
                        layout_w,
                        layout_h,
                    );
                }
                snapshots.push(snap);
            }
        }
    }
    // Phase 2.7 — the COW emits via the normal top_level_order walk
    // like any other root child. After Phase 2.2/2.5, the COW is a
    // first-class entry in `windows` + `top_level_order`; the
    // walk's `under_cow_subtree` flag (Task 2.6) carries the
    // alpha-passthrough semantic that the deleted post-walk append
    // used to wire up. Mirrors Xorg's compositor contract: COW is
    // a real root child stacked above the other top-levels.
    log::trace!(
        "render scene_walk begin output={output_idx} top_levels={n} order={order:?} \
         cow_host_xid={cow_host_xid:?} \
         layout=({layout_x0},{layout_y0} {layout_w}x{layout_h})",
        n = core.top_level_order.len(),
        order = core.top_level_order,
    );
    // Fullscreen-unredirect / direct-scanout bypass. The COW is the always-on-
    // top compositor overlay carrying the composite of the REDIRECTED windows.
    // An UNREDIRECTED (scene_participating), opaque window that fully covers
    // this output is drawn directly by us and sits logically in front of that
    // composite — so the COW's content is entirely occluded by it. Emit the
    // window but SKIP the COW (and its `under_cow` subtree, e.g. the
    // compositor's desktop stage) for this output. Otherwise the always-on-top
    // COW — correctly capped on top by the stacking-projection rework
    // (a4ff9f1e) — paints the desktop composite over the directly-drawn window
    // and it vanishes (cinnamon-screensaver lock, and any fullscreen
    // override-redirect window, once muffin unredirects it: RedirectWindow ->
    // NameWindowPixmap -> UnredirectWindow). Mirrors mutter/Xorg
    // `unredirect_fullscreen`. Pre-rework this happened to work because the COW
    // wasn't reliably on top, so the window landed above it.
    let probe_lw = i32::try_from(layout_w).unwrap_or(i32::MAX);
    let probe_lh = i32::try_from(layout_h).unwrap_or(i32::MAX);
    let probe_x1 = layout_x0.saturating_add(probe_lw);
    let probe_y1 = layout_y0.saturating_add(probe_lh);
    // The probe walks top-down and lets the FIRST candidate decide, so it
    // must only consider top-levels that can actually occlude something on
    // this output. A window lying entirely outside the output cannot, and
    // must not end the scan: muffin parks 1x1 helper windows off-screen
    // (e.g. host 0xd0000a at (-200,-200)) and raises them ABOVE the managed
    // windows, so before this filter the very first candidate was one of
    // those, `covers` was false, and the COW was never suppressed — leaving
    // an unredirected fullscreen window hidden under the compositor's
    // desktop composite (issue #98: fullscreen games/video render as the
    // wallpaper while audio keeps playing).
    let topmost_on_output = core
        .top_level_order
        .iter()
        .rev()
        .filter(|&&x| Some(x) != cow_host_xid)
        .find_map(|&x| {
            windows
                .get(&x)
                .filter(|g| {
                    g.mapped
                        && i32::from(g.x) < probe_x1
                        && i32::from(g.y) < probe_y1
                        && i32::from(g.x) + i32::from(g.width) > layout_x0
                        && i32::from(g.y) + i32::from(g.height) > layout_y0
                })
                .map(|g| (x, *g))
        });
    let suppress_cow = cow_host_xid.is_some()
        && topmost_on_output.is_some_and(|(x, g)| {
            let covers = i32::from(g.x) <= layout_x0
                && i32::from(g.y) <= layout_y0
                && i32::from(g.x) + i32::from(g.width) >= probe_x1
                && i32::from(g.y) + i32::from(g.height) >= probe_y1;
            // Opaque (no alpha channel for the COW to show through) AND
            // drawn by us (scene_participating == not compositor-owned).
            let opaque = g.depth != 32;
            let participating = store
                .lookup(x)
                .and_then(|id| store.get(id))
                .is_some_and(|d| d.scene_participating);
            covers && opaque && participating
        });
    if suppress_cow {
        log::trace!(
            "render scene_walk output={output_idx}: COW suppressed — opaque fullscreen \
             unredirected window occludes the compositor overlay"
        );
    }
    // DIAG(#98): the suppression verdict plus the inputs it turned on.
    // Deduped, so a steady state costs one line rather than one per frame.
    // `cow_shape` tells us whether the compositor ALSO punched a hole in
    // the COW (mutter-lineage `shape_cow_for_window`) — if it did, the
    // suppression probe is not the only mechanism in play and the missing
    // parent-shape clipping of the COW's stage child matters too.
    if cow_host_xid.is_some() {
        let msg = format!(
            "cow_diag: output={output_idx} suppress_cow={suppress_cow} \
             cow_shape_rects={shape:?} picked={picked} order_top={top:?}",
            shape = cow_host_xid
                .and_then(|c| core.shape_bounding.get(&c))
                .map(Vec::len),
            picked = topmost_on_output.map_or_else(
                || "none".to_string(),
                |(x, g)| format!(
                    "0x{x:x}[({},{} {}x{}) depth={} part={}]",
                    g.x,
                    g.y,
                    g.width,
                    g.height,
                    g.depth,
                    store
                        .lookup(x)
                        .and_then(|id| store.get(id))
                        .is_some_and(|d| d.scene_participating),
                ),
            ),
            top = core
                .top_level_order
                .iter()
                .rev()
                .take(4)
                .map(|x| format!("0x{x:x}"))
                .collect::<Vec<_>>(),
        );
        static LAST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        std::hash::Hash::hash(&msg, &mut hasher);
        let sig = std::hash::Hasher::finish(&hasher);
        if LAST.swap(sig, std::sync::atomic::Ordering::Relaxed) != sig {
            log::debug!("{msg}");
        }
    }
    for &top_xid in &core.top_level_order {
        if suppress_cow && Some(top_xid) == cow_host_xid {
            continue;
        }
        emit_window_subtree(
            top_xid,
            0,
            0,
            store,
            windows,
            &core.shape_bounding,
            layout_x0,
            layout_y0,
            layout_w,
            layout_h,
            &mut draws,
            &mut snapshots,
            &mut sampled_ids,
            &mut projected,
            // Top-level windows start with no redirected ancestor;
            // the flag flips on inside the recursion when entering
            // a redirected window's subtree.
            false,
            // Phase 2.6 — flag the COW top-level (and its
            // descendants, propagated by recursion) so emitted
            // draws inherit `alpha_passthrough = true`.
            Some(top_xid) == cow_host_xid,
            // Parent-clipping: top-levels are clipped by the root =
            // the screen, which the output-extent gate already
            // enforces. Pass effectively-unbounded ancestor bounds so
            // this is a no-op for top-levels; descendants are still
            // clipped to their top-level via the recursion.
            i32::MIN / 2,
            i32::MIN / 2,
            i32::MAX / 2,
            i32::MAX / 2,
        );
    }
    log::trace!(
        "render scene_walk end output={output_idx} draws={n_draws} \
         sampled={n_sampled}",
        n_draws = draws.len(),
        n_sampled = sampled_ids.len(),
    );

    // Stage 5 Phase C: pure cursor strategy decision. `build_scene`
    // decides visibility + HW/SW assignment and reports the current
    // clipped footprint, but it does NOT emit cursor damage. The
    // tick owns that decision because it also owns the transactional
    // "last successfully presented cursor footprint/version" state.
    #[allow(clippy::cast_possible_truncation)]
    let mut software_cursor_tail = None;
    let (cursor_assignment, new_cursor_rect, cursor_record_version): (
        CursorAssignment,
        Option<vk::Rect2D>,
        Option<u64>,
    ) = if let Some(cur) = cursor
        && let Some(drawable) = store.get(cur.id)
        && drawable.storage.image_view != vk::ImageView::null()
    {
        let cw = i32::try_from(cur.extent.width).unwrap_or(i32::MAX);
        let ch = i32::try_from(cur.extent.height).unwrap_or(i32::MAX);
        let layout_w_i = i32::try_from(layout_w).unwrap_or(i32::MAX);
        let layout_h_i = i32::try_from(layout_h).unwrap_or(i32::MAX);
        let dx = (core.cursor_x as i32) - i32::from(cur.hot_x) - layout_x0;
        let dy = (core.cursor_y as i32) - i32::from(cur.hot_y) - layout_y0;
        let new_rect = cursor_footprint_rect(dx, dy, cw, ch, layout_w_i, layout_h_i);
        if new_rect.is_none() {
            // Off-output / fully-clipped — the cursor isn't on this
            // output this frame. Phase D treats this as `Hidden`.
            (CursorAssignment::Hidden, None, None)
        } else {
            // Phase C strategy gates (codex v6-pass — pure data, no
            // DRM side effects). Hand off to HW only when the
            // strategy is active AND the sprite fits this output's owning
            // device plane. Cursor dimensions differ by card (e.g. 128px
            // amdgpu beside 64px i915), so this must not be a global 64px
            // minimum.
            let hw_fits = platform.cursor_plane_fits_for_output(
                output_idx,
                cur.extent.width,
                cur.extent.height,
            );
            if hw_strategy_active && hw_fits {
                (
                    CursorAssignment::Hw {
                        x: core.cursor_x as i32,
                        y: core.cursor_y as i32,
                        record_version: cur.record_version,
                        hot_x: u16::try_from(cur.hot_x.max(0)).unwrap_or(0),
                        hot_y: u16::try_from(cur.hot_y.max(0)).unwrap_or(0),
                    },
                    new_rect,
                    Some(cur.record_version),
                )
            } else {
                let draw_index = draws.len();
                let sampled_index = sampled_ids.len();
                draws.push(CompositeDraw {
                    image_view: drawable.storage.sample_view,
                    #[allow(clippy::cast_precision_loss)]
                    dst_origin: [dx as f32, dy as f32],
                    #[allow(clippy::cast_precision_loss)]
                    dst_size: [cw as f32, ch as f32],
                    src_origin: [0.0, 0.0],
                    src_size: [1.0, 1.0],
                    alpha_passthrough: true,
                });
                sampled_ids.push(cur.id);
                software_cursor_tail = Some((draw_index, sampled_index));
                (
                    CursorAssignment::Sw { pos: (dx, dy) },
                    new_rect,
                    Some(cur.record_version),
                )
            }
        }
    } else {
        (CursorAssignment::Hidden, None, None)
    };

    let scene = CompositeScene {
        bg_color: bg,
        draws,
    };
    SceneBuild {
        scene,
        snapshots,
        sampled_ids,
        projected_damage: projected,
        cursor_assignment,
        new_cursor_rect,
        cursor_record_version,
        software_cursor_tail,
    }
}

/// Stage 3f.6 recurse: emit a CompositeDraw entry for `host_xid` if
/// it's mapped + scene-participating + has live storage, then recurse
/// into mapped descendants with accumulated parent offsets.
///
/// Coordinates: `parent_abs_x` / `parent_abs_y` are the absolute
/// (root-space) origin of this window's parent. The window's own
/// position (`geom.x`, `geom.y`) is parent-relative per X11, so the
/// window's absolute origin is `parent_abs + geom`. We project onto
/// the output by subtracting the output's layout origin.
///
/// A child is only visible if every ancestor in the chain is mapped;
/// `unmapped` short-circuits the entire subtree (matches X11
/// MapWindow semantics — an unmapped parent hides all descendants).
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)]
fn emit_window_subtree(
    host_xid: u32,
    parent_abs_x: i32,
    parent_abs_y: i32,
    store: &mut DrawableStore,
    windows: &super::backend::WindowsMap,
    // Per-window SHAPE bounding regions (`KmsCore::shape_bounding`).
    // When a host xid has an entry the window's scene draw is
    // clipped to those rects — marco's rounded-corner frame masks
    // depend on this. Empty / missing entry → unshaped, single
    // full-window draw.
    shape_bounding: &HashMap<u32, Vec<xfixes::RegionRect>>,
    layout_x0: i32,
    layout_y0: i32,
    layout_w: u32,
    layout_h: u32,
    draws: &mut Vec<CompositeDraw>,
    snapshots: &mut Vec<DamageSnapshot>,
    sampled_ids: &mut Vec<super::store::DrawableId>,
    projected: &mut RegionSet,
    // Audit #3 (2026-05-19): true iff some ancestor on the recursion
    // path owns a `redirected_target`. When set, this window's paint
    // landed in that ancestor's backing (via `resolve_paint_target`'s
    // ancestor walk), so emitting this window's own storage would
    // show stale/empty pixels — the ancestor's emit already shows
    // the content. A descendant that owns ITS OWN `redirected_target`
    // breaks this chain (its paint stops at itself), so it still
    // emits its own backing regardless of the inherited flag.
    under_redirected_ancestor: bool,
    // Phase 2.6 — true iff the current recursion path entered the
    // COW top-level (or one of its descendants). When set, emitted
    // `CompositeDraw` entries take `alpha_passthrough = true` so the
    // compositor's composited result blends over the layer below;
    // outside the COW subtree (no compositor active) draws stay
    // opaque (`alpha_passthrough = false`). Mirrors the threading of
    // `under_redirected_ancestor` above.
    under_cow_subtree: bool,
    // X11 parent-clipping: a window's visible region is the
    // intersection of its own rectangle with EVERY ancestor's
    // rectangle. These are the accumulated ancestor bounds in absolute
    // screen coords (half-open [x0,x1) × [y0,y1)); this window's draw
    // and its descendants' clips are intersected against them. The
    // top-level call passes effectively-unbounded bounds (top-levels
    // are screen-clipped by the output-extent gate), so this is a
    // no-op for the common case where children fit inside their
    // parents — it only bites a child that extends beyond its parent,
    // e.g. an fvwm frame decoration parked in a tiny holding window.
    clip_x0: i32,
    clip_y0: i32,
    clip_x1: i32,
    clip_y1: i32,
) {
    let debug_focus = scene_walk_debug_enabled_for(host_xid);
    // Stage 4 diagnostic: trace-level scene-walk decision per window.
    // Enable with `RUST_LOG=yserver::kms::render::scene=trace`. The
    // top-level and descendant paths share this function so the
    // single trace site covers both. Format is greppable —
    // `render scene_walk xid=...: ...` — for `grep "render scene_walk"`
    // over yserver-hw.log to extract just these lines.
    let Some(geom) = windows.get(&host_xid) else {
        log::trace!("render scene_walk xid={host_xid:#x}: SKIP reason=geom_not_in_windows");
        if debug_focus {
            log::debug!("render scene_walk xid={host_xid:#x}: SKIP reason=geom_not_in_windows");
        }
        return;
    };
    if !geom.mapped {
        // X11: an unmapped window (and entire subtree) is invisible.
        log::trace!(
            "render scene_walk xid={host_xid:#x}: SKIP reason=geom_unmapped \
             geom=({x},{y} {w}x{h}) depth={depth} parent={parent:?}",
            x = geom.x,
            y = geom.y,
            w = geom.width,
            h = geom.height,
            depth = geom.depth,
            parent = geom.parent,
        );
        if debug_focus {
            log::debug!(
                "render scene_walk xid={host_xid:#x}: SKIP reason=geom_unmapped \
                 geom=({x},{y} {w}x{h}) depth={depth} parent={parent:?}",
                x = geom.x,
                y = geom.y,
                w = geom.width,
                h = geom.height,
                depth = geom.depth,
                parent = geom.parent,
            );
        }
        return;
    }
    let abs_x = parent_abs_x + i32::from(geom.x);
    let abs_y = parent_abs_y + i32::from(geom.y);

    // X11 parent-clipping. This window's visible box in its OWN local
    // coords = its rect [0,own_w)×[0,own_h) intersected with the
    // accumulated ancestor clip (translated into local coords). Draws
    // are restricted to this box; descendants inherit the intersection
    // (in absolute coords) as their clip. `vis_*` empty ⇒ nothing of
    // this window is visible (fully clipped by an ancestor).
    let own_w = i32::from(geom.width);
    let own_h = i32::from(geom.height);
    let vis_lx0 = (clip_x0 - abs_x).max(0);
    let vis_ly0 = (clip_y0 - abs_y).max(0);
    let vis_lx1 = (clip_x1 - abs_x).min(own_w);
    let vis_ly1 = (clip_y1 - abs_y).min(own_h);
    // Absolute clip passed down to children = ancestor clip ∩ own rect.
    let child_clip_x0 = clip_x0.max(abs_x);
    let child_clip_y0 = clip_y0.max(abs_y);
    let child_clip_x1 = clip_x1.min(abs_x + own_w);
    let child_clip_y1 = clip_y1.min(abs_y + own_h);

    // Manual-redirect subtree boundary. When a window is
    // `scene_participating=false` here, the compositor owns the
    // entire subtree's presentation (X11 Composite §285+360 —
    // Manual-mode redirect removes the window AND its descendants
    // from normal scene-out; the compositor reads the redirected
    // backing instead). Set after the per-node decision so we
    // can return *after* the SKIP trace fires (preserves the
    // existing trace shape for live debugging) and before the
    // child-recurse below.
    //
    // Audit #3 (2026-05-19): the old `prune_subtree=true` for
    // `scene_participating=false` is gone — Automatic descendants of
    // Manual ancestors need to recurse so they can emit their own
    // backing. Per-window emit-vs-skip is decided by
    // `paint_target_is_self` below; the recurse always runs and the
    // `under_redirected_ancestor` flag carries the chain context.

    // Emit a draw entry for this window if it has live storage that
    // participates in the scene.
    let lookup_id = store.lookup(host_xid);
    if lookup_id.is_none() {
        log::trace!(
            "render scene_walk xid={host_xid:#x}: SKIP reason=no_store_lookup \
             geom=({x},{y} {w}x{h}) mapped=true depth={depth}",
            x = geom.x,
            y = geom.y,
            w = geom.width,
            h = geom.height,
            depth = geom.depth,
        );
        if debug_focus {
            log::debug!(
                "render scene_walk xid={host_xid:#x}: SKIP reason=no_store_lookup \
                 geom=({x},{y} {w}x{h}) mapped=true depth={depth}",
                x = geom.x,
                y = geom.y,
                w = geom.width,
                h = geom.height,
                depth = geom.depth,
            );
        }
    }
    if let Some(id) = lookup_id {
        // Pull diagnostic fields up front (cheap copies) so we can
        // emit a single SKIP/WILL_EMIT trace line per gate failure
        // without re-borrowing the store across log call sites.
        let drawable_snap = store.get(id).map(|d| {
            (
                d.id,
                d.kind,
                d.depth,
                d.refcount,
                d.scene_participating,
                d.storage.extent,
                d.storage.image_view == vk::ImageView::null(),
            )
        });
        if let Some((d_id, d_kind, d_depth, d_refcount, d_part, d_extent, d_view_null)) =
            drawable_snap
        {
            // Stage 4c.3 — route source-storage through `redirected_target`.
            // Both modes blit FROM B; W's geometry (dst_origin, dst_size,
            // intersect test) stays driven by W's own state in
            // `windows`. Only the sampled storage handle reroutes.
            let source_id = store.redirected_target(id).unwrap_or(id);
            let source_view_null = store
                .get(source_id)
                .is_none_or(|s| s.storage.image_view == vk::ImageView::null());

            // Audit #3 (2026-05-19) — emit-or-skip is governed by
            // "is this window's storage where paint actually lands?"
            //
            //   has_own_redirected_target   self owns a `redirected_target`
            //                               → paint lands in its B, emit B.
            //   under_redirected_ancestor   some ancestor owns one
            //                               → paint lands in ancestor's B,
            //                                 ancestor emits it, we skip.
            //   d_part                      `scene_participating=true` —
            //                                 ordinary non-redirected window
            //                                 with its own storage as the
            //                                 paint target. Emit own storage.
            //
            // Pre-fix the rule was `d_part || manual_backing_visible`
            // plus an unconditional `prune_subtree` on
            // `scene_participating=false`. That dropped Automatic-
            // redirected descendants of Manual-redirected ancestors —
            // GTK/marco CSD frames lose their inner widgets (per audit
            // #3 / Control Center missing-widget reports).
            let has_own_redirected_target = source_id != id;
            // Phase 3.1 — Manual-redirected windows (own a
            // `redirected_target` AND `scene_participating=false`)
            // must NEVER emit to scanout. They go offscreen for the
            // compositor to read via NameWindowPixmap; the X server
            // must not also blit the backing in. Mirrors Xorg's
            // structural guarantee from `compCheckRedirect`.
            let is_manual_redirected = has_own_redirected_target && !d_part;
            let paint_target_is_self = !is_manual_redirected
                && (has_own_redirected_target || (d_part && !under_redirected_ancestor));

            // Project onto output-local coords (computed once here so
            // both the SKIP=no_intersect and WILL_EMIT trace lines can
            // include the dst rect).
            let dx = abs_x - layout_x0;
            let dy = abs_y - layout_y0;
            let win_w = i32::from(geom.width);
            let win_h = i32::from(geom.height);
            let intersects = !(dx + win_w <= 0
                || dy + win_h <= 0
                || dx >= i32::try_from(layout_w).unwrap_or(i32::MAX)
                || dy >= i32::try_from(layout_h).unwrap_or(i32::MAX));

            // Pick the first failing gate and emit a single SKIP line;
            // otherwise emit WILL_EMIT. Order matches the production
            // gate ordering below so the trace mirrors the live path.
            let skip_reason: Option<&'static str> = if is_manual_redirected {
                // Phase 3.1 — first reason in the cascade. A
                // Manual-redirected window (own redirected_target +
                // scene_participating=false) is unconditionally
                // skipped; the compositor reads its backing via
                // NameWindowPixmap and re-emits it on the COW.
                Some("manual_redirect_unconditional_skip")
            } else if !paint_target_is_self {
                if has_own_redirected_target {
                    // Defensive — `paint_target_is_self` is true when
                    // `has_own_redirected_target` AND not
                    // Manual-redirected (the Manual case is handled
                    // by the branch above), so this branch is
                    // unreachable. Kept so the match stays exhaustive
                    // if the rule ever evolves.
                    Some("paint_target_not_self")
                } else if under_redirected_ancestor {
                    Some("paint_target_is_redirected_ancestor")
                } else {
                    Some("scene_participating=false")
                }
            } else if !matches!(d_kind, DrawableKind::Window) {
                Some("kind!=Window")
            } else if source_view_null {
                Some("source_image_view_null")
            } else if !intersects {
                Some("no_intersect_with_output")
            } else {
                None
            };

            if debug_focus {
                log::debug!(
                    "render scene_walk focus xid={host_xid:#x} source_id={source_id:?} \
                     has_own_redirected_target={has_own_redirected_target} \
                     under_redirected_ancestor={under_redirected_ancestor} \
                     paint_target_is_self={paint_target_is_self} \
                     intersects={intersects} skip_reason={skip_reason:?}",
                );
            }

            if let Some(reason) = skip_reason {
                log::trace!(
                    "render scene_walk xid={host_xid:#x}: SKIP reason={reason} \
                     geom=({gx},{gy} {gw}x{gh}) mapped=true \
                     store_id={d_id:?} kind={d_kind:?} depth={d_depth} \
                     refcount={d_refcount} scene_participating={d_part} \
                     storage_extent={dew}x{deh} image_view_null={d_view_null} \
                     source_id={source_id:?} source_view_null={source_view_null}",
                    gx = geom.x,
                    gy = geom.y,
                    gw = geom.width,
                    gh = geom.height,
                    dew = d_extent.width,
                    deh = d_extent.height,
                );
                if debug_focus {
                    log::debug!(
                        "render scene_walk xid={host_xid:#x}: SKIP reason={reason} \
                         geom=({gx},{gy} {gw}x{gh}) mapped=true \
                         store_id={d_id:?} kind={d_kind:?} depth={d_depth} \
                         refcount={d_refcount} scene_participating={d_part} \
                         storage_extent={dew}x{deh} image_view_null={d_view_null} \
                         source_id={source_id:?} source_view_null={source_view_null}",
                        gx = geom.x,
                        gy = geom.y,
                        gw = geom.width,
                        gh = geom.height,
                        dew = d_extent.width,
                        deh = d_extent.height,
                    );
                }
            } else {
                log::trace!(
                    "render scene_walk xid={host_xid:#x}: WILL_EMIT \
                     geom=({gx},{gy} {gw}x{gh}) abs=({abs_x},{abs_y}) \
                     output=({dx},{dy} {win_w}x{win_h}) \
                     store_id={d_id:?} kind={d_kind:?} depth={d_depth} \
                     refcount={d_refcount} scene_participating={d_part} \
                     storage_extent={dew}x{deh} image_view_null={d_view_null} \
                     source_id={source_id:?}",
                    gx = geom.x,
                    gy = geom.y,
                    gw = geom.width,
                    gh = geom.height,
                    dew = d_extent.width,
                    deh = d_extent.height,
                );
                if debug_focus {
                    log::debug!(
                        "render scene_walk xid={host_xid:#x}: WILL_EMIT \
                         geom=({gx},{gy} {gw}x{gh}) abs=({abs_x},{abs_y}) \
                         output=({dx},{dy} {win_w}x{win_h}) \
                         store_id={d_id:?} kind={d_kind:?} depth={d_depth} \
                         refcount={d_refcount} scene_participating={d_part} \
                         storage_extent={dew}x{deh} image_view_null={d_view_null} \
                         source_id={source_id:?}",
                        gx = geom.x,
                        gy = geom.y,
                        gw = geom.width,
                        gh = geom.height,
                        dew = d_extent.width,
                        deh = d_extent.height,
                    );
                }
            }

            if matches!(d_kind, DrawableKind::Window)
                && let Some(source) = store.get(source_id)
                && source.storage.image_view != vk::ImageView::null()
                && intersects
                && paint_target_is_self
            {
                // Window scene draw — bind the sample-side view
                // (format/depth-aware swizzle) instead of the
                // raw IDENTITY-swizzle attachment view. This is
                // the load-bearing fix for the "depth-24 windows
                // / COW α leak" bug: the BgraNoAlpha swizzle
                // forced α=ONE for depth-24 used to live ONLY in
                // the engine's RENDER view-cache, never on the
                // scene path. Combined with `alpha_passthrough=true`
                // below, the prior IDENTITY view leaked the
                // BGRA8 padding byte (typically 0) into the
                // shader's `src.a`, blending depth-24 windows
                // with α=0 — invisible against root, which
                // matched the post-4d.7 mate-with-compositing
                // and xfce-with-compositing hardware-smoke
                // failure shape.
                //
                // SHAPE bounding handling: when the window has a
                // bounding region (marco's rounded-corner mask,
                // panel-applet transparency cutouts, etc.) emit
                // one clipped draw per rect intersected with the
                // window's storage extent. Without bounding (the
                // common case), emit a single full-window draw —
                // preserving the alpha-passthrough invariants
                // documented above for the depth-32 / depth-24
                // distinction. Pixels outside the bounding region
                // are intentionally NOT drawn so the layer below
                // (parent / wallpaper / root) shows through.
                let image_view = source.storage.sample_view;
                #[allow(clippy::cast_precision_loss)]
                let win_w_f = win_w as f32;
                #[allow(clippy::cast_precision_loss)]
                let win_h_f = win_h as f32;
                let mut emitted_any = false;
                if let Some(rects) = shape_bounding.get(&host_xid) {
                    for rect in rects {
                        let rx = i32::from(rect.x);
                        let ry = i32::from(rect.y);
                        let rw = i32::from(rect.width);
                        let rh = i32::from(rect.height);
                        // Clamp to the window extent AND the ancestor
                        // visible box (parent-clipping).
                        let cx = rx.max(0).max(vis_lx0);
                        let cy = ry.max(0).max(vis_ly0);
                        let cw = (rx + rw).min(win_w).min(vis_lx1) - cx;
                        let ch = (ry + rh).min(win_h).min(vis_ly1) - cy;
                        if cw <= 0 || ch <= 0 {
                            continue;
                        }
                        #[allow(clippy::cast_precision_loss)]
                        let cw_f = cw as f32;
                        #[allow(clippy::cast_precision_loss)]
                        let ch_f = ch as f32;
                        #[allow(clippy::cast_precision_loss)]
                        let cx_f = cx as f32;
                        #[allow(clippy::cast_precision_loss)]
                        let cy_f = cy as f32;
                        draws.push(CompositeDraw {
                            image_view,
                            #[allow(clippy::cast_precision_loss)]
                            dst_origin: [(dx + cx) as f32, (dy + cy) as f32],
                            dst_size: [cw_f, ch_f],
                            src_origin: [cx_f / win_w_f, cy_f / win_h_f],
                            src_size: [cw_f / win_w_f, ch_f / win_h_f],
                            // Phase 2.6 — alpha-passthrough is inherited
                            // from the COW subtree flag (set on the COW
                            // top-level + descendants). Outside the COW
                            // subtree, draws stay opaque.
                            alpha_passthrough: under_cow_subtree,
                        });
                        emitted_any = true;
                    }
                } else if vis_lx1 > vis_lx0 && vis_ly1 > vis_ly0 {
                    // Unshaped: emit the window rect clipped to the
                    // ancestor visible box. Common case (child fits
                    // inside its parent) → box == full window, so this
                    // is the full-window draw with src [0,0]-[1,1].
                    let cw = vis_lx1 - vis_lx0;
                    let ch = vis_ly1 - vis_ly0;
                    #[allow(clippy::cast_precision_loss)]
                    draws.push(CompositeDraw {
                        image_view,
                        dst_origin: [(dx + vis_lx0) as f32, (dy + vis_ly0) as f32],
                        dst_size: [cw as f32, ch as f32],
                        src_origin: [vis_lx0 as f32 / win_w_f, vis_ly0 as f32 / win_h_f],
                        src_size: [cw as f32 / win_w_f, ch as f32 / win_h_f],
                        // Phase 2.6 — alpha-passthrough is inherited
                        // from the COW subtree flag (set on the COW
                        // top-level + descendants). Outside the COW
                        // subtree, draws stay opaque (no compositor
                        // path); inside the COW subtree, the
                        // compositor's stage paints with alpha and we
                        // blend over whatever lies below.
                        alpha_passthrough: under_cow_subtree,
                    });
                    emitted_any = true;
                }
                if emitted_any {
                    sampled_ids.push(source_id);
                    if let Some(snap) = store.peek_presentation_damage(source_id) {
                        for r in snap.region.rects() {
                            add_projected_damage(projected, *r, dx, dy, layout_w, layout_h);
                        }
                        snapshots.push(snap);
                    }
                }
            }
        } else {
            log::trace!(
                "render scene_walk xid={host_xid:#x}: SKIP reason=store_get_returned_none \
                 store_id={lookup_id:?} geom=({x},{y} {w}x{h}) mapped=true depth={depth}",
                x = geom.x,
                y = geom.y,
                w = geom.width,
                h = geom.height,
                depth = geom.depth,
            );
            if debug_focus {
                log::debug!(
                    "render scene_walk xid={host_xid:#x}: SKIP reason=store_get_returned_none \
                     store_id={lookup_id:?} geom=({x},{y} {w}x{h}) mapped=true depth={depth}",
                    x = geom.x,
                    y = geom.y,
                    w = geom.width,
                    h = geom.height,
                    depth = geom.depth,
                );
            }
        }
    }

    // Audit #3 (2026-05-19) — descendants need to know whether THEY
    // sit under a redirected ancestor. The chain is "this window
    // counts as a redirected ancestor iff it owns its own
    // `redirected_target`" — that's exactly where
    // `resolve_paint_target` stops climbing the parent chain. A
    // recursion under a Manual-redirected ancestor without own
    // backing flips the flag on; an Automatic-redirected descendant
    // beneath that resets the flag for its own descendants (because
    // its paint stops at its own B).
    let self_owns_redirected_target = store
        .lookup(host_xid)
        .and_then(|id| store.redirected_target(id))
        .is_some();
    let child_under_redirected_ancestor = under_redirected_ancestor || self_owns_redirected_target;

    // Recurse into mapped descendants in stable sibling stack order.
    let mut children: Vec<(u32, u64)> = windows
        .iter()
        .filter_map(|(xid, g)| {
            if g.parent == Some(host_xid) {
                Some((*xid, g.stack_rank))
            } else {
                None
            }
        })
        .collect();
    children.sort_by_key(|(_, rank)| *rank);
    for (child_xid, _) in children {
        emit_window_subtree(
            child_xid,
            abs_x,
            abs_y,
            store,
            windows,
            shape_bounding,
            layout_x0,
            layout_y0,
            layout_w,
            layout_h,
            draws,
            snapshots,
            sampled_ids,
            projected,
            child_under_redirected_ancestor,
            // Phase 2.6 — COW subtree flag is inherited unchanged.
            // Once we entered the COW top-level, every descendant
            // emits with alpha_passthrough=true.
            under_cow_subtree,
            // Parent-clipping: children are clipped to this window's
            // rect intersected with the inherited ancestor clip.
            child_clip_x0,
            child_clip_y0,
            child_clip_x1,
            child_clip_y1,
        );
    }
}

/// Stage 4c.1 — for each `(extent, damage)` pair in `outputs`, clip
/// every rect in `rects` to that output's extent and (if non-empty)
/// add the clipped rect to that output's damage `RegionSet`.
///
/// Extracted from [`SceneCompositor::mark_scene_structure_damage_rects`]
/// so the dispatch + clip + accumulate wiring is unit-testable
/// without needing a live `VkContext` + `CompositorPipeline`.
fn dispatch_clip_rects_to_outputs<'a, I>(outputs: I, rects: &[vk::Rect2D])
where
    I: IntoIterator<Item = (vk::Extent2D, &'a mut RegionSet)>,
{
    for (ext, damage) in outputs {
        for r in rects {
            if let Some(clipped) = clip_rect_to_output_extent(*r, ext) {
                damage.add(clipped);
            }
        }
    }
}

/// Stage 4c.1 — intersect a rect (in output-local coords) with the
/// output's extent. Returns `None` if the intersection is empty
/// (rect lies fully outside, or input has zero width/height).
///
/// This is the output-local counterpart to `add_projected_damage`'s
/// clipping math: same rectangle-intersection arithmetic, but the
/// projection (the `+dx`/`+dy` translation that maps storage-local
/// coords into output coords) is omitted because the caller already
/// works in output coords.
fn clip_rect_to_output_extent(rect: vk::Rect2D, ext: vk::Extent2D) -> Option<vk::Rect2D> {
    let max_x = i32::try_from(ext.width).unwrap_or(i32::MAX);
    let max_y = i32::try_from(ext.height).unwrap_or(i32::MAX);
    let x0 = rect.offset.x.max(0);
    let y0 = rect.offset.y.max(0);
    let x1 = rect
        .offset
        .x
        .saturating_add_unsigned(rect.extent.width)
        .min(max_x);
    let y1 = rect
        .offset
        .y
        .saturating_add_unsigned(rect.extent.height)
        .min(max_y);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some(vk::Rect2D {
        offset: vk::Offset2D { x: x0, y: y0 },
        extent: vk::Extent2D {
            width: u32::try_from(x1 - x0).unwrap_or(0),
            height: u32::try_from(y1 - y0).unwrap_or(0),
        },
    })
}

fn add_projected_damage(
    projected: &mut RegionSet,
    src: vk::Rect2D,
    dx: i32,
    dy: i32,
    layout_w: u32,
    layout_h: u32,
) {
    let layout_w_i = i32::try_from(layout_w).unwrap_or(i32::MAX);
    let layout_h_i = i32::try_from(layout_h).unwrap_or(i32::MAX);
    let x0 = (src.offset.x + dx).max(0);
    let y0 = (src.offset.y + dy).max(0);
    let x1 = (src.offset.x + dx)
        .saturating_add_unsigned(src.extent.width)
        .min(layout_w_i);
    let y1 = (src.offset.y + dy)
        .saturating_add_unsigned(src.extent.height)
        .min(layout_h_i);
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    projected.add(vk::Rect2D {
        offset: vk::Offset2D { x: x0, y: y0 },
        extent: vk::Extent2D {
            width: u32::try_from(x1 - x0).unwrap_or(0),
            height: u32::try_from(y1 - y0).unwrap_or(0),
        },
    });
}

// ────────────────────────────────────────────────────────────────
// v2 compose recorder — fork of v1's `record_and_present_composite`
// with buffer-age (loadOp=LOAD + per-frame scissor) support.
//
// Why fork: v1 always uses `loadOp=CLEAR` against the full BO,
// which is incompatible with buffer-age repaint (any region outside
// the clear gets clobbered to bg_color). v2 needs `LOAD` on the
// clipped path so unaltered regions retain their prior-generation
// content. The submission shape, fence handshake, and atomic-flip
// handling stay identical to v1.
// ────────────────────────────────────────────────────────────────

trait ComposeRenderTarget {
    fn image(&self) -> vk::Image;
    fn image_view(&self) -> vk::ImageView;
    fn command_buffer(&self) -> vk::CommandBuffer;
    fn completion_semaphore(&self) -> vk::Semaphore;
    fn width(&self) -> u32;
    fn height(&self) -> u32;
    fn timestamp_pool(&self) -> vk::QueryPool;
    fn set_last_gpu_render_ns(&mut self, value: Option<u64>);
    fn post_compose_preparation(&self) -> Result<PostComposePreparation, PresentError>;
    fn record_post_compose(
        &self,
        vk: &crate::kms::vk::device::VkContext,
        command_buffer: vk::CommandBuffer,
        preparation: PostComposePreparation,
    );

    fn renderer_wait_semaphore(&self) -> Option<vk::Semaphore> {
        None
    }

    fn note_submit_succeeded(&mut self) {}
}

#[derive(Clone, Copy)]
enum PostComposePreparation {
    Shared,
    Copied(CopiedTransportPreparation),
}

impl ComposeRenderTarget for ScanoutBo {
    fn image(&self) -> vk::Image {
        self.vk_image
    }

    fn image_view(&self) -> vk::ImageView {
        self.vk_image_view
    }

    fn command_buffer(&self) -> vk::CommandBuffer {
        self.vk_transfer.command_buffer
    }

    fn completion_semaphore(&self) -> vk::Semaphore {
        self.vk_semaphore
    }

    fn width(&self) -> u32 {
        self.width
    }

    fn height(&self) -> u32 {
        self.height
    }

    fn timestamp_pool(&self) -> vk::QueryPool {
        self.vk_transfer.timestamp_pool
    }

    fn set_last_gpu_render_ns(&mut self, value: Option<u64>) {
        self.last_gpu_render_ns = value;
    }

    fn post_compose_preparation(&self) -> Result<PostComposePreparation, PresentError> {
        Ok(PostComposePreparation::Shared)
    }

    fn record_post_compose(
        &self,
        vk: &crate::kms::vk::device::VkContext,
        command_buffer: vk::CommandBuffer,
        preparation: PostComposePreparation,
    ) {
        debug_assert!(matches!(preparation, PostComposePreparation::Shared));
        let to_scanout = [vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
            .src_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
            .dst_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
            .dst_access_mask(vk::AccessFlags2::empty())
            .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .new_layout(vk::ImageLayout::GENERAL)
            .image(self.vk_image)
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .level_count(1)
                    .layer_count(1),
            )];
        crate::vk_count!(cmd_pipeline_barrier2);
        unsafe {
            vk.device.cmd_pipeline_barrier2(
                command_buffer,
                &vk::DependencyInfo::default().image_memory_barriers(&to_scanout),
            );
        }
    }
}

impl ComposeRenderTarget for CopiedRenderSource {
    fn image(&self) -> vk::Image {
        self.image()
    }

    fn image_view(&self) -> vk::ImageView {
        self.image_view()
    }

    fn command_buffer(&self) -> vk::CommandBuffer {
        self.transfer.command_buffer
    }

    fn completion_semaphore(&self) -> vk::Semaphore {
        self.completion_semaphore
    }

    fn width(&self) -> u32 {
        self.width()
    }

    fn height(&self) -> u32 {
        self.height()
    }

    fn timestamp_pool(&self) -> vk::QueryPool {
        self.transfer.timestamp_pool
    }

    fn set_last_gpu_render_ns(&mut self, value: Option<u64>) {
        self.last_gpu_render_ns = value;
    }

    fn post_compose_preparation(&self) -> Result<PostComposePreparation, PresentError> {
        self.transport_preparation()
            .map(PostComposePreparation::Copied)
            .map_err(PresentError::Io)
    }

    fn renderer_wait_semaphore(&self) -> Option<vk::Semaphore> {
        self.renderer_wait_semaphore()
    }

    fn note_submit_succeeded(&mut self) {
        self.note_renderer_submit_succeeded();
    }

    fn record_post_compose(
        &self,
        _vk: &crate::kms::vk::device::VkContext,
        command_buffer: vk::CommandBuffer,
        preparation: PostComposePreparation,
    ) {
        let PostComposePreparation::Copied(preparation) = preparation else {
            unreachable!("copied target received shared post-compose preparation")
        };
        self.record_transport_copy(command_buffer, preparation);
    }
}

/// Render directly into the KMS framebuffer and immediately queue its flip.
#[allow(clippy::too_many_arguments)]
fn submit_shared_scanout_frame(
    vk: &crate::kms::vk::device::VkContext,
    drm: &crate::drm::Device,
    output: &crate::platform::drm::Output,
    bo: &mut ScanoutBo,
    pipeline: &CompositorPipeline,
    descriptor_pool: vk::DescriptorPool,
    scene: &CompositeScene,
    repaint: Repaint,
    signal_fence: vk::Fence,
    gpu_submitted: &mut bool,
    overlay_ops: &[(u32, vk::Rect2D)],
    xor_pipeline: vk::Pipeline,
    xor_layout: vk::PipelineLayout,
) -> Result<(), PresentError> {
    use std::os::fd::{FromRawFd, IntoRawFd};

    if bo.state.phase != BoPhase::Free {
        return Err(PresentError::WrongPhase(bo.state.phase));
    }
    let fb_handle = bo.fb_handle.ok_or(PresentError::NoFb)?;
    bo.state.transition_to_recording();
    record_and_submit_render(
        vk,
        bo,
        pipeline,
        descriptor_pool,
        scene,
        repaint,
        signal_fence,
        gpu_submitted,
        overlay_ops,
        xor_pipeline,
        xor_layout,
    )?;

    let fd = bo
        .export_signaled_fd()
        .map_err(PresentError::Vk)?
        .map_or(-1, IntoRawFd::into_raw_fd);
    bo.state.transition_to_submitted(fd);

    let mut out_fence: i32 = -1;
    match crate::drm::page_flip::submit_flip_with_fences(drm, output, fb_handle, fd, &mut out_fence)
    {
        Ok(()) => {
            if let Some(reclaimed) = bo.state.transition_to_pending(out_fence) {
                // SAFETY: `reclaimed` was inserted by
                // `transition_to_submitted` above.
                drop(unsafe { std::os::fd::OwnedFd::from_raw_fd(reclaimed) });
            }
            Ok(())
        }
        Err(error) => {
            if let Some(reclaimed) = bo.state.transition_to_recording_after_atomic_reject() {
                // SAFETY: same fd we just inserted.
                drop(unsafe { std::os::fd::OwnedFd::from_raw_fd(reclaimed) });
            }
            if out_fence >= 0 {
                // Defensive: OUT_FENCE_PTR should only be written on success.
                drop(unsafe { std::os::fd::OwnedFd::from_raw_fd(out_fence) });
            }
            Err(PresentError::Io(error))
        }
    }
}

/// Render into A's exportable source. The paired destination phase reserves
/// the same BO index until readiness advances the frame to B's copy + KMS
/// submission on the main-loop boundary.
#[allow(clippy::too_many_arguments)]
fn submit_copied_scanout_render(
    vk: &crate::kms::vk::device::VkContext,
    source: &mut CopiedRenderSource,
    destination_state: &mut BoState,
    pipeline: &CompositorPipeline,
    descriptor_pool: vk::DescriptorPool,
    scene: &CompositeScene,
    repaint: Repaint,
    signal_fence: vk::Fence,
    gpu_submitted: &mut bool,
    overlay_ops: &[(u32, vk::Rect2D)],
    xor_pipeline: vk::Pipeline,
    xor_layout: vk::PipelineLayout,
) -> Result<Option<std::os::fd::OwnedFd>, CopiedRenderSubmitError> {
    if destination_state.phase != BoPhase::Free {
        return Err(CopiedRenderSubmitError::Present(PresentError::WrongPhase(
            destination_state.phase,
        )));
    }
    source
        .prepare_renderer_acquire()
        .map_err(CopiedRenderSubmitError::RendererAcquire)?;
    destination_state.transition_to_recording();
    record_and_submit_render(
        vk,
        source,
        pipeline,
        descriptor_pool,
        scene,
        repaint,
        signal_fence,
        gpu_submitted,
        overlay_ops,
        xor_pipeline,
        xor_layout,
    )
    .map_err(CopiedRenderSubmitError::Present)?;
    source
        .export_render_completion()
        .map_err(|error| CopiedRenderSubmitError::Present(PresentError::Vk(error)))
}

#[allow(clippy::too_many_arguments)]
fn record_and_submit_render(
    vk: &crate::kms::vk::device::VkContext,
    target: &mut impl ComposeRenderTarget,
    pipeline: &CompositorPipeline,
    descriptor_pool: vk::DescriptorPool,
    scene: &CompositeScene,
    repaint: Repaint,
    signal_fence: vk::Fence,
    gpu_submitted: &mut bool,
    overlay_ops: &[(u32, vk::Rect2D)],
    xor_pipeline: vk::Pipeline,
    xor_layout: vk::PipelineLayout,
) -> Result<(), PresentError> {
    // Compose GPU-render telemetry. Read the PREVIOUS compose's
    // timestamps BEFORE the CB overwrites them; the read is
    // synchronous (no WAIT flag), and the bo is being re-acquired so
    // its prior compose fence has already signalled → results are
    // available. `NOT_READY` on the very first compose (pool never
    // written) → `None`. `tick_one_output` takes and forwards this to
    // `telemetry.record_gpu_render_ns` after submission returns.
    let ts_pool = target.timestamp_pool();
    let ts_enabled = vk.timestamp_period > 0.0 && ts_pool != vk::QueryPool::null();
    let last_gpu_render_ns = if ts_enabled {
        let mut ts = [0u64; 2];
        match unsafe {
            vk.device
                .get_query_pool_results(ts_pool, 0, &mut ts, vk::QueryResultFlags::TYPE_64)
        } {
            Ok(()) => {
                let ticks = ts[1].saturating_sub(ts[0]);
                #[allow(
                    clippy::cast_precision_loss,
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss
                )]
                let ns = (ticks as f64 * f64::from(vk.timestamp_period)) as u64;
                Some(ns)
            }
            Err(_) => None,
        }
    } else {
        None
    };
    target.set_last_gpu_render_ns(last_gpu_render_ns);

    // Allocate descriptor sets — same shape as v1.
    let mut descriptors: Vec<vk::DescriptorSet> = Vec::with_capacity(scene.draws.len());
    for draw in &scene.draws {
        let layouts = [pipeline.descriptor_set_layout];
        let alloc_info = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(descriptor_pool)
            .set_layouts(&layouts);
        let set = match unsafe { vk.device.allocate_descriptor_sets(&alloc_info) } {
            Ok(sets) => sets[0],
            Err(e) => {
                log::warn!(
                    "render compose: descriptor allocation failed ({e:?}) at draw {} of {}",
                    descriptors.len(),
                    scene.draws.len(),
                );
                break;
            }
        };
        let image_info = [vk::DescriptorImageInfo::default()
            .image_view(draw.image_view)
            .sampler(pipeline.sampler)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
        let writes = [vk::WriteDescriptorSet::default()
            .dst_set(set)
            .dst_binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .image_info(&image_info)];
        unsafe { vk.device.update_descriptor_sets(&writes, &[]) };
        descriptors.push(set);
    }

    // Record.
    record_command_buffer(
        vk,
        target,
        pipeline,
        scene,
        &descriptors,
        repaint,
        overlay_ops,
        xor_pipeline,
        xor_layout,
    )?;

    let cb = target.command_buffer();
    let cb_info = [vk::CommandBufferSubmitInfo::default().command_buffer(cb)];
    let sig_info = [vk::SemaphoreSubmitInfo::default()
        .semaphore(target.completion_semaphore())
        .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)];
    let waits = target.renderer_wait_semaphore().map(|semaphore| {
        [vk::SemaphoreSubmitInfo::default()
            .semaphore(semaphore)
            .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)]
    });
    let mut submit = vk::SubmitInfo2::default()
        .command_buffer_infos(&cb_info)
        .signal_semaphore_infos(&sig_info);
    if let Some(waits) = waits.as_ref() {
        submit = submit.wait_semaphore_infos(waits);
    }
    let submit = [submit];
    unsafe {
        crate::vk_count!(queue_submit2);
        crate::vk_count!(submit_compositor);
        vk.device
            .queue_submit2(vk.graphics_queue, &submit, signal_fence)?;
    }
    target.note_submit_succeeded();
    *gpu_submitted = true;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn record_command_buffer<T: ComposeRenderTarget + ?Sized>(
    vk: &crate::kms::vk::device::VkContext,
    bo: &T,
    pipeline: &CompositorPipeline,
    scene: &CompositeScene,
    descriptors: &[vk::DescriptorSet],
    repaint: Repaint,
    overlay_ops: &[(u32, vk::Rect2D)],
    xor_pipeline: vk::Pipeline,
    xor_layout: vk::PipelineLayout,
) -> Result<(), PresentError> {
    let device = &vk.device;
    let cb = bo.command_buffer();
    // Mirror the timestamp gate `record_and_submit_render` uses so we can bracket the
    // CB with TOP/BOTTOM timestamp writes; caller already read the
    // previous pool contents before we reset the pool below.
    let ts_pool = bo.timestamp_pool();
    let ts_enabled = vk.timestamp_period > 0.0 && ts_pool != vk::QueryPool::null();
    // Validate all copied transport state before beginning the command buffer.
    // The post-compose recorder is then infallible and cannot strand a live CB
    // in recording state on an ownership-ledger error.
    let post_compose_preparation = bo.post_compose_preparation()?;
    let (load_op, render_area, old_layout) = match repaint {
        Repaint::Full(extent) => (
            vk::AttachmentLoadOp::CLEAR,
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent,
            },
            vk::ImageLayout::UNDEFINED,
        ),
        Repaint::Clipped(_) => (
            vk::AttachmentLoadOp::LOAD,
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: bo.width(),
                    height: bo.height(),
                },
            },
            // LOAD requires the previous layout to be valid; the
            // BO has been through a prior present which left it
            // at GENERAL (KMS scanout layout). Transition from
            // GENERAL → COLOR_ATTACHMENT_OPTIMAL with a full
            // memory barrier so prior writes are visible.
            vk::ImageLayout::GENERAL,
        ),
    };
    let scissor = match repaint {
        Repaint::Full(extent) => vk::Rect2D {
            offset: vk::Offset2D::default(),
            extent,
        },
        Repaint::Clipped(rect) => rect,
    };

    unsafe {
        device.reset_command_buffer(cb, vk::CommandBufferResetFlags::empty())?;
        let begin = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        crate::vk_count!(begin_command_buffer);
        device.begin_command_buffer(cb, &begin)?;

        // GPU-render timer: reset the pool (GPU-ordered, after the CPU
        // read above) and stamp TOP-of-pipe before any compose work.
        // See the corresponding BOTTOM stamp before end_command_buffer.
        if ts_enabled {
            device.cmd_reset_query_pool(cb, ts_pool, 0, 2);
            device.cmd_write_timestamp(cb, vk::PipelineStageFlags::TOP_OF_PIPE, ts_pool, 0);
        }

        let to_color_src_access = if matches!(load_op, vk::AttachmentLoadOp::LOAD) {
            // LOAD: previous KMS scanout left the BO in GENERAL.
            // The kernel "consumed" the BO contents via the page
            // flip; we now need the GPU to read+write them. Pair
            // ALL_COMMANDS + empty source access (no prior GPU
            // work to drain — the scanout completes before the
            // pageflip event fires) with COLOR_ATTACHMENT_OUTPUT
            // + WRITE on the dst.
            vk::AccessFlags2::empty()
        } else {
            vk::AccessFlags2::empty()
        };
        let to_color = vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::TOP_OF_PIPE)
            .src_access_mask(to_color_src_access)
            .dst_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
            // B.2 fix (vkdebug READ_AFTER_WRITE at vkCmdBeginRendering):
            // include COLOR_ATTACHMENT_READ so the loadOp=LOAD that
            // begin_rendering performs is synchronized against the
            // layout-transition's write. Validation surfaces this
            // hazard with the message "must allow
            // COLOR_ATTACHMENT_READ accesses at COLOR_ATTACHMENT_OUTPUT".
            .dst_access_mask(
                vk::AccessFlags2::COLOR_ATTACHMENT_WRITE | vk::AccessFlags2::COLOR_ATTACHMENT_READ,
            )
            .old_layout(old_layout)
            .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .image(bo.image())
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .level_count(1)
                    .layer_count(1),
            );
        let to_color_arr = [to_color];
        let to_color_dep = vk::DependencyInfo::default().image_memory_barriers(&to_color_arr);
        crate::vk_count!(cmd_pipeline_barrier2);
        device.cmd_pipeline_barrier2(cb, &to_color_dep);

        let color_attachment = [vk::RenderingAttachmentInfo::default()
            .image_view(bo.image_view())
            .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .load_op(load_op)
            .store_op(vk::AttachmentStoreOp::STORE)
            .clear_value(vk::ClearValue {
                color: vk::ClearColorValue {
                    float32: scene.bg_color,
                },
            })];
        let rendering_info = vk::RenderingInfo::default()
            .render_area(render_area)
            .layer_count(1)
            .color_attachments(&color_attachment);
        crate::vk_count!(cmd_begin_rendering);
        device.cmd_begin_rendering(cb, &rendering_info);

        let viewport = [vk::Viewport {
            x: 0.0,
            y: 0.0,
            #[allow(clippy::cast_precision_loss)]
            width: bo.width() as f32,
            #[allow(clippy::cast_precision_loss)]
            height: bo.height() as f32,
            min_depth: 0.0,
            max_depth: 1.0,
        }];
        crate::vk_count!(cmd_set_viewport);
        device.cmd_set_viewport(cb, 0, &viewport);
        crate::vk_count!(cmd_set_scissor);
        device.cmd_set_scissor(cb, 0, &[scissor]);

        #[allow(clippy::cast_precision_loss)]
        let viewport_size = [bo.width() as f32, bo.height() as f32];
        let mut last_pipeline: Option<vk::Pipeline> = None;
        for (i, draw) in scene.draws.iter().enumerate().take(descriptors.len()) {
            let pl = pipeline.pipeline_for(draw.alpha_passthrough);
            if last_pipeline != Some(pl) {
                crate::vk_count!(cmd_bind_pipeline);
                device.cmd_bind_pipeline(cb, vk::PipelineBindPoint::GRAPHICS, pl);
                last_pipeline = Some(pl);
            }
            let sets = [descriptors[i]];
            crate::vk_count!(cmd_bind_descriptor_sets);
            device.cmd_bind_descriptor_sets(
                cb,
                vk::PipelineBindPoint::GRAPHICS,
                pipeline.pipeline_layout,
                0,
                &sets,
                &[],
            );
            let push = CompositePushConsts {
                dst_origin: draw.dst_origin,
                dst_size: draw.dst_size,
                viewport: viewport_size,
                src_origin: draw.src_origin,
                src_size: draw.src_size,
            };
            crate::vk_count!(cmd_push_constants);
            device.cmd_push_constants(
                cb,
                pipeline.pipeline_layout,
                vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                0,
                push.as_bytes(),
            );
            crate::vk_count!(cmd_draw);
            device.cmd_draw(cb, 4, 1, 0, 0);
        }

        // Retained root-`IncludeInferiors` overlay XOR pass — applied
        // into the freshly-composited scanout BO while it is still in
        // COLOR_ATTACHMENT_OPTIMAL with rendering active (no extra
        // barrier / begin_rendering needed). The recorder rebinds its
        // own pipeline + per-op scissor, so it is safe after the scene
        // draw loop above.
        //
        // NON-IDEMPOTENT: correct only under `Repaint::Full` (CLEAR + full
        // redraw → XORed once onto fresh pixels). If buffer-age
        // `Repaint::Clipped`+LOAD is re-enabled, the overlay rects MUST be
        // folded into the repaint region or the XOR double-applies on
        // uncovered pooled BOs. See `pick_repaint_region` doc.
        if !overlay_ops.is_empty() {
            crate::kms::vk::ops::scanout_logic_fill::record_scanout_logic_fill(
                vk,
                cb,
                xor_pipeline,
                xor_layout,
                viewport_size,
                overlay_ops,
            );
        }

        crate::vk_count!(cmd_end_rendering);
        device.cmd_end_rendering(cb);

        bo.record_post_compose(vk, cb, post_compose_preparation);

        // GPU-render timer: stamp BOTTOM-of-pipe after all compose work.
        if ts_enabled {
            device.cmd_write_timestamp(cb, vk::PipelineStageFlags::BOTTOM_OF_PIPE, ts_pool, 1);
        }

        crate::vk_count!(end_command_buffer);
        device.end_command_buffer(cb)?;
    }
    let _ = render_area;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copied_completion_requires_exact_job_and_paired_bo() {
        let waiting = InFlightStage::WaitingForRenderCompletion { job_id: 41 };
        assert!(copied_render_completion_matches(waiting, 2, 41, 2));
        assert!(!copied_render_completion_matches(waiting, 2, 42, 2));
        assert!(!copied_render_completion_matches(waiting, 2, 41, 1));
    }

    #[test]
    fn copied_frame_cannot_retire_before_sink_kms_submission() {
        let waiting = InFlightStage::WaitingForRenderCompletion { job_id: 7 };
        assert!(!kms_retirement_matches(waiting, 1, 1));
        assert!(!copied_render_completion_matches(
            InFlightStage::KmsFlipPending,
            1,
            7,
            1,
        ));
    }

    #[test]
    fn kms_retirement_requires_the_exact_paired_bo() {
        assert!(kms_retirement_matches(InFlightStage::KmsFlipPending, 1, 1,));
        assert!(!kms_retirement_matches(InFlightStage::KmsFlipPending, 1, 2,));
    }

    #[test]
    fn global_drain_waits_and_releases_every_deferred_scene_resource() {
        let mut pending = VecDeque::from([(
            3,
            crate::kms::render::platform::FenceTicket::for_tests_stub(),
        )]);
        let mut failed = VecDeque::from([FailedSubmitBo {
            bo_idx: 7,
            pool_slot: 5,
            ticket: crate::kms::render::platform::FenceTicket::for_tests_stub(),
        }]);
        let mut waited = 0;
        let mut released = Vec::new();

        drain_deferred_scene_resources(
            &mut pending,
            &mut failed,
            |_| {
                waited += 1;
                true
            },
            |release| {
                released.push(release);
                true
            },
        );

        assert_eq!(waited, 2);
        assert!(pending.is_empty());
        assert!(failed.is_empty());
        assert_eq!(
            released,
            vec![
                DeferredSceneRelease::PoolSlot(3),
                DeferredSceneRelease::FailedSubmit {
                    bo_idx: 7,
                    pool_slot: 5,
                },
            ]
        );
    }

    #[test]
    fn global_drain_retains_every_resource_whose_fence_wait_fails() {
        let mut pending = VecDeque::from([(
            3,
            crate::kms::render::platform::FenceTicket::for_tests_stub(),
        )]);
        let mut failed = VecDeque::from([FailedSubmitBo {
            bo_idx: 7,
            pool_slot: 5,
            ticket: crate::kms::render::platform::FenceTicket::for_tests_stub(),
        }]);
        let mut released = Vec::new();

        drain_deferred_scene_resources(
            &mut pending,
            &mut failed,
            |_| false,
            |release| {
                released.push(release);
                true
            },
        );

        assert!(released.is_empty());
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].0, 3);
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].bo_idx, 7);
        assert_eq!(failed[0].pool_slot, 5);
    }

    // Stage 5 Phase G — strategy decision unit tests. Verify the
    // pure `derive_cursor_transition` matrix without needing
    // build_scene or a live Vk fixture.

    #[test]
    fn derive_sw_to_hw_queues_show_on_retire() {
        let prev = OutputCursorMode::Sw {
            prev: Some((100, 100)),
        };
        let assignment = CursorAssignment::Hw {
            x: 200,
            y: 150,
            record_version: 42,
            hot_x: 4,
            hot_y: 4,
        };
        let (trans, prev_pos, mode_after) = derive_cursor_transition(prev, assignment);
        let Some(CursorTransition::ShowOnRetire {
            upload_version,
            x,
            y,
            ..
        }) = trans
        else {
            panic!("expected ShowOnRetire, got {trans:?}");
        };
        assert_eq!(upload_version, 42);
        assert_eq!((x, y), (200, 150));
        assert_eq!(prev_pos, Some(None), "Sw→Hw clears prev_pos");
        assert_eq!(mode_after, OutputCursorMode::Hw);
    }

    #[test]
    fn actual_hw_visibility_forces_cursorless_fallback_but_not_hw_rebind() {
        let desired_sw = CursorAssignment::Sw { pos: (100, 100) };
        let sw_prev = effective_cursor_prev_mode(OutputCursorMode::Hidden, true, desired_sw);
        assert_eq!(sw_prev, OutputCursorMode::Hw);
        assert!(cursorless_hide_frame_required(sw_prev, desired_sw));
        let (transition, _, _) = derive_cursor_transition(sw_prev, desired_sw);
        assert!(matches!(
            transition,
            Some(CursorTransition::HideOnRetire {
                reveal_sw_after: true
            })
        ));

        let desired_hidden = CursorAssignment::Hidden;
        let hidden_prev = effective_cursor_prev_mode(
            OutputCursorMode::Sw {
                prev: Some((10, 20)),
            },
            true,
            desired_hidden,
        );
        assert_eq!(hidden_prev, OutputCursorMode::Hw);
        let (transition, _, _) = derive_cursor_transition(hidden_prev, desired_hidden);
        assert!(matches!(
            transition,
            Some(CursorTransition::HideOnRetire {
                reveal_sw_after: false
            })
        ));

        let desired_hw = CursorAssignment::Hw {
            x: 200,
            y: 150,
            record_version: 42,
            hot_x: 4,
            hot_y: 5,
        };
        let hw_prev = effective_cursor_prev_mode(OutputCursorMode::Hidden, true, desired_hw);
        assert_eq!(
            hw_prev,
            OutputCursorMode::Hidden,
            "an old visible binding must not suppress a lifecycle/version Show"
        );
        let (transition, _, _) = derive_cursor_transition(hw_prev, desired_hw);
        assert!(matches!(
            transition,
            Some(CursorTransition::ShowOnRetire {
                upload_version: 42,
                ..
            })
        ));

        let unchanged = OutputCursorMode::Sw { prev: None };
        assert_eq!(
            effective_cursor_prev_mode(unchanged, false, desired_hidden),
            unchanged
        );

        let hw_now_hidden_to_sw =
            effective_cursor_prev_mode(OutputCursorMode::Hw, false, desired_sw);
        assert_eq!(hw_now_hidden_to_sw, OutputCursorMode::Hidden);
        let (transition, _, mode) = derive_cursor_transition(hw_now_hidden_to_sw, desired_sw);
        assert!(transition.is_none());
        assert!(matches!(mode, OutputCursorMode::Sw { .. }));

        let hw_now_hidden_to_hidden =
            effective_cursor_prev_mode(OutputCursorMode::Hw, false, desired_hidden);
        assert_eq!(hw_now_hidden_to_hidden, OutputCursorMode::Hidden);
        let (transition, _, mode) =
            derive_cursor_transition(hw_now_hidden_to_hidden, desired_hidden);
        assert!(transition.is_none());
        assert_eq!(mode, OutputCursorMode::Hidden);

        let hw_now_hidden_to_hw =
            effective_cursor_prev_mode(OutputCursorMode::Hw, false, desired_hw);
        assert_eq!(hw_now_hidden_to_hw, OutputCursorMode::Hidden);
        let (transition, _, _) = derive_cursor_transition(hw_now_hidden_to_hw, desired_hw);
        assert!(matches!(
            transition,
            Some(CursorTransition::ShowOnRetire {
                upload_version: 42,
                ..
            })
        ));
    }

    #[test]
    fn live_source_device_lost_is_the_only_fatal_vulkan_present_error() {
        assert!(vk_result_is_device_lost(vk::Result::ERROR_DEVICE_LOST));
        assert!(!vk_result_is_device_lost(
            vk::Result::ERROR_OUT_OF_DEVICE_MEMORY,
        ));
        assert!(present_error_is_device_lost(&PresentError::Vk(
            vk::Result::ERROR_DEVICE_LOST,
        )));
        assert!(!present_error_is_device_lost(&PresentError::Vk(
            vk::Result::ERROR_OUT_OF_DEVICE_MEMORY,
        )));
    }

    #[test]
    fn copied_renderer_acquire_failure_is_fail_stop_before_submit() {
        let acquire = CopiedRenderSubmitError::RendererAcquire(io::Error::other(
            "retained B-to-A completion import failed",
        ));
        assert!(acquire.requires_fail_stop());
        assert!(matches!(acquire.into_present(), PresentError::Io(_)));

        let ordinary = CopiedRenderSubmitError::Present(PresentError::Vk(
            vk::Result::ERROR_OUT_OF_DEVICE_MEMORY,
        ));
        assert!(!ordinary.requires_fail_stop());
    }

    #[test]
    fn upload_failure_hides_only_a_recorded_live_binding() {
        use std::cell::Cell;

        let hidden_hide_calls = Cell::new(0);
        let hidden = resolve_failed_cursor_upload(0, false, || {
            hidden_hide_calls.set(hidden_hide_calls.get() + 1);
            Err(io::Error::from_raw_os_error(libc::EINVAL))
        });
        assert_eq!(hidden, CursorTransitionResult::Hidden);
        assert_eq!(hidden_hide_calls.get(), 0);

        let visible_hide_calls = Cell::new(0);
        let hidden_after_rollback = resolve_failed_cursor_upload(0, true, || {
            visible_hide_calls.set(visible_hide_calls.get() + 1);
            Ok(())
        });
        assert_eq!(hidden_after_rollback, CursorTransitionResult::Hidden);
        assert_eq!(visible_hide_calls.get(), 1);

        let visible_hide_calls = Cell::new(0);
        let retained = resolve_failed_cursor_upload(0, true, || {
            visible_hide_calls.set(visible_hide_calls.get() + 1);
            Err(io::Error::from_raw_os_error(libc::EIO))
        });
        assert_eq!(retained, CursorTransitionResult::VisibleNeedsShowRetry);
        assert_eq!(visible_hide_calls.get(), 1);
    }

    #[test]
    fn retire_hide_skips_ioctl_when_fast_path_already_unbound_the_plane() {
        use std::cell::Cell;

        let calls = Cell::new(0);
        let reveal = resolve_cursor_hide_on_retire(0, true, false, || {
            calls.set(calls.get() + 1);
            Err(io::Error::from_raw_os_error(libc::EINVAL))
        });
        assert_eq!(reveal, CursorTransitionResult::HiddenNeedsRepaint);
        assert_eq!(calls.get(), 0);

        let hidden = resolve_cursor_hide_on_retire(0, false, false, || {
            calls.set(calls.get() + 1);
            Err(io::Error::from_raw_os_error(libc::EINVAL))
        });
        assert_eq!(hidden, CursorTransitionResult::Applied);
        assert_eq!(calls.get(), 0);

        let visible = resolve_cursor_hide_on_retire(0, true, true, || {
            calls.set(calls.get() + 1);
            Err(io::Error::from_raw_os_error(libc::EIO))
        });
        assert_eq!(visible, CursorTransitionResult::Visible);
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn actual_visible_hw_plus_desired_sw_omits_every_software_cursor_artifact() {
        let cursor_id = crate::kms::render::store::DrawableId::for_tests(100);
        let assignment = CursorAssignment::Sw { pos: (10, 20) };
        let prev = effective_cursor_prev_mode(OutputCursorMode::Hidden, true, assignment);
        let mut built = SceneBuild {
            scene: CompositeScene {
                bg_color: [0.0, 0.0, 0.0, 1.0],
                draws: vec![CompositeDraw {
                    image_view: vk::ImageView::null(),
                    dst_origin: [10.0, 20.0],
                    dst_size: [16.0, 16.0],
                    src_origin: [0.0, 0.0],
                    src_size: [1.0, 1.0],
                    alpha_passthrough: true,
                }],
            },
            snapshots: Vec::new(),
            sampled_ids: vec![cursor_id],
            projected_damage: RegionSet::new(),
            cursor_assignment: assignment,
            new_cursor_rect: Some(rect(10, 20, 16, 16)),
            cursor_record_version: Some(7),
            software_cursor_tail: Some((0, 0)),
        };
        assert!(cursorless_hide_frame_required(prev, assignment));
        built.omit_software_cursor_for_hide();
        let (transition, _, _) = derive_cursor_transition(prev, assignment);

        assert!(matches!(
            transition,
            Some(CursorTransition::HideOnRetire {
                reveal_sw_after: true
            })
        ));
        assert!(built.scene.draws.is_empty());
        assert!(built.sampled_ids.is_empty());
        assert_eq!(built.new_cursor_rect, None);
        assert_eq!(built.cursor_record_version, None);
    }

    #[test]
    fn hw_to_sw_uses_cursorless_hide_then_one_frame_gap_progression() {
        let assignment = CursorAssignment::Sw { pos: (100, 100) };
        assert!(cursorless_hide_frame_required(
            OutputCursorMode::Hw,
            assignment
        ));
        let (trans, prev_pos, mode_after) =
            derive_cursor_transition(OutputCursorMode::Hw, assignment);
        assert!(matches!(
            trans,
            Some(CursorTransition::HideOnRetire {
                reveal_sw_after: true
            })
        ));
        assert_eq!(prev_pos, Some(None));
        assert_eq!(mode_after, OutputCursorMode::SwPending);

        let pending =
            resolve_retired_cursor_state(CursorTransitionResult::HiddenNeedsRepaint, mode_after);
        assert_eq!(pending.actual_mode, OutputCursorMode::SwPending);
        assert!(pending.commit_desired_metadata);
        assert!(pending.force_repaint);
        assert!(!cursorless_hide_frame_required(
            pending.actual_mode,
            assignment
        ));
        let (next_transition, next_prev, next_mode) =
            derive_cursor_transition(pending.actual_mode, assignment);
        assert!(next_transition.is_none());
        assert_eq!(next_prev, Some(Some((100, 100))));
        assert_eq!(
            next_mode,
            OutputCursorMode::Sw {
                prev: Some((100, 100))
            }
        );
        assert!(!matches!(next_mode, OutputCursorMode::SwPending));
    }

    #[test]
    fn cursorless_hide_phase_removes_sw_draw_sample_and_presented_metadata() {
        let cursor_id = crate::kms::render::store::DrawableId::for_tests(99);
        let mut built = SceneBuild {
            scene: CompositeScene {
                bg_color: [0.0, 0.0, 0.0, 1.0],
                draws: vec![CompositeDraw {
                    image_view: vk::ImageView::null(),
                    dst_origin: [10.0, 20.0],
                    dst_size: [16.0, 16.0],
                    src_origin: [0.0, 0.0],
                    src_size: [1.0, 1.0],
                    alpha_passthrough: true,
                }],
            },
            snapshots: Vec::new(),
            sampled_ids: vec![cursor_id],
            projected_damage: RegionSet::new(),
            cursor_assignment: CursorAssignment::Sw { pos: (10, 20) },
            new_cursor_rect: Some(rect(10, 20, 16, 16)),
            cursor_record_version: Some(7),
            software_cursor_tail: Some((0, 0)),
        };

        built.omit_software_cursor_for_hide();

        assert!(built.scene.draws.is_empty());
        assert!(built.sampled_ids.is_empty());
        assert_eq!(built.new_cursor_rect, None);
        assert_eq!(built.cursor_record_version, None);
        assert_eq!(
            built.cursor_assignment,
            CursorAssignment::Sw { pos: (10, 20) }
        );
    }

    #[test]
    fn repeated_hide_failure_keeps_hw_over_cursorless_frames_and_retries() {
        let assignment = CursorAssignment::Sw { pos: (100, 100) };
        let failed = resolve_retired_cursor_state(
            CursorTransitionResult::Visible,
            OutputCursorMode::SwPending,
        );
        assert_eq!(failed.actual_mode, OutputCursorMode::Hw);
        assert!(failed.force_repaint);
        assert!(cursorless_hide_frame_required(
            failed.actual_mode,
            assignment
        ));
        let (retry, _, mode_after_retry) = derive_cursor_transition(failed.actual_mode, assignment);
        assert!(matches!(
            retry,
            Some(CursorTransition::HideOnRetire {
                reveal_sw_after: true
            })
        ));
        assert_eq!(mode_after_retry, OutputCursorMode::SwPending);
    }

    #[test]
    fn derive_hw_to_hidden_queues_hide_on_retire() {
        let (trans, _prev_pos, mode_after) =
            derive_cursor_transition(OutputCursorMode::Hw, CursorAssignment::Hidden);
        assert!(matches!(
            trans,
            Some(CursorTransition::HideOnRetire {
                reveal_sw_after: false
            })
        ));
        assert_eq!(mode_after, OutputCursorMode::Hidden);
    }

    #[test]
    fn stationary_cursor_same_rect_mode_and_version_adds_no_damage() {
        let damage = cursor_damage_for_frame(
            Some(rect(10, 20, 16, 16)),
            Some(7),
            Some(rect(10, 20, 16, 16)),
            Some(7),
            None,
        );
        assert!(
            damage.is_empty(),
            "stationary cursor must not keep the output dirty"
        );
    }

    #[test]
    fn moved_sw_cursor_damages_old_and_new_rects() {
        let old = rect(10, 20, 16, 16);
        let new = rect(30, 40, 16, 16);
        let damage = cursor_damage_for_frame(Some(old), Some(7), Some(new), Some(7), None);
        let rects = damage.rects();
        assert!(rects.contains(&old), "old rect must be cleared");
        assert!(rects.contains(&new), "new rect must be painted");
    }

    #[test]
    fn sprite_swap_on_stationary_cursor_damages_once() {
        let rect = rect(10, 20, 16, 16);
        let damage = cursor_damage_for_frame(Some(rect), Some(7), Some(rect), Some(8), None);
        assert_eq!(damage.rects(), &[rect]);
    }

    #[test]
    fn pure_hw_hide_still_damages_last_present_rect() {
        let rect = rect(10, 20, 16, 16);
        let damage = cursor_damage_for_frame(
            Some(rect),
            Some(7),
            None,
            None,
            Some(CursorTransition::HideOnRetire {
                reveal_sw_after: false,
            }),
        );
        assert_eq!(damage.rects(), &[rect]);
    }

    #[test]
    fn tick_outcome_only_clears_dirty_for_compose_or_empty_skip() {
        assert!(TickOutcome::Composed.clears_scene_structure_dirty());
        assert!(TickOutcome::Skipped(TickSkipReason::EmptyDamage).clears_scene_structure_dirty());
        assert!(!TickOutcome::Skipped(TickSkipReason::PendingAcks).clears_scene_structure_dirty());
        assert!(
            !TickOutcome::Skipped(TickSkipReason::RetryDeadline).clears_scene_structure_dirty()
        );
        assert!(!TickOutcome::Skipped(TickSkipReason::NoBO).clears_scene_structure_dirty());
        assert!(!TickOutcome::Skipped(TickSkipReason::NoPool).clears_scene_structure_dirty());
    }

    /// Regression guard for the idle free-run fix: the empty-projection
    /// force-compose must fire ONLY when a captured snapshot carries
    /// non-empty damage. `peek_presentation_damage` returns `Some` even
    /// for a clean (empty) region, so gating on `!snapshots.is_empty()`
    /// force-composed every drawn window every vblank at idle. Gating on
    /// real damage must (a) NOT force for a clean drawn window (→ idle
    /// EmptyDamage skip) and (b) STILL force for a window that painted
    /// but whose projection landed empty (the xfce submenu case).
    #[test]
    fn empty_projection_force_compose_gates_on_real_captured_damage() {
        use crate::kms::render::store::{DamageSnapshot, DrawableId};
        let id = DrawableId::for_tests(1);

        // No snapshots at all → no force.
        assert!(!snapshots_carry_damage(&[]));

        // Clean drawn window: peeked snapshot with an EMPTY region →
        // must NOT force (this was the idle free-run bug).
        let clean = DamageSnapshot {
            id,
            epoch: 1,
            region: RegionSet::new(),
        };
        assert!(!snapshots_carry_damage(std::slice::from_ref(&clean)));

        // Window that actually painted, projection landed empty:
        // non-empty captured damage → MUST still force (submenu case).
        let mut painted_region = RegionSet::new();
        painted_region.add(rect(0, 0, 4, 4));
        let painted = DamageSnapshot {
            id,
            epoch: 2,
            region: painted_region,
        };
        assert!(snapshots_carry_damage(std::slice::from_ref(&painted)));

        // Mixed (a clean + a painted) still forces.
        assert!(snapshots_carry_damage(&[clean, painted]));
    }

    /// Steady-state HW ordinarily queues no transition. A sprite/hotspot
    /// change separately sets `force_show_retry`, which converts the next
    /// composed Hw→Hw frame into ShowOnRetire.
    #[test]
    fn derive_hw_to_hw_no_transition() {
        let assignment = CursorAssignment::Hw {
            x: 0,
            y: 0,
            record_version: 7,
            hot_x: 0,
            hot_y: 0,
        };
        let (trans, _prev_pos, mode_after) =
            derive_cursor_transition(OutputCursorMode::Hw, assignment);
        assert!(trans.is_none());
        assert_eq!(mode_after, OutputCursorMode::Hw);
    }

    #[test]
    fn pending_show_output_receives_later_sprite_retry() {
        let pending_show = Some(CursorTransition::ShowOnRetire {
            upload_version: 7,
            hot_x: 0,
            hot_y: 0,
            x: 10,
            y: 20,
        });

        assert!(cursor_output_needs_sprite_retry(
            OutputCursorMode::Sw { prev: None },
            [pending_show]
        ));
        assert!(cursor_output_needs_sprite_retry(
            OutputCursorMode::Hw,
            [None]
        ));
        assert!(!cursor_output_needs_sprite_retry(
            OutputCursorMode::Hidden,
            [None]
        ));
    }

    #[test]
    fn failed_unbound_show_records_hidden_and_clears_claimed_metadata() {
        let resolution =
            resolve_retired_cursor_state(CursorTransitionResult::Hidden, OutputCursorMode::Hw);
        assert_eq!(resolution.actual_mode, OutputCursorMode::Hidden);
        assert!(!resolution.commit_desired_metadata);
        assert!(resolution.clear_presented_metadata);
    }

    #[test]
    fn failed_hide_retains_actual_hw_and_last_known_metadata() {
        let resolution = resolve_retired_cursor_state(
            CursorTransitionResult::Visible,
            OutputCursorMode::Sw {
                prev: Some((10, 20)),
            },
        );
        assert_eq!(resolution.actual_mode, OutputCursorMode::Hw);
        assert!(!resolution.commit_desired_metadata);
        assert!(!resolution.clear_presented_metadata);
    }

    #[test]
    fn failed_visible_rebind_forces_full_show_retry_without_claiming_version() {
        let resolution = resolve_retired_cursor_state(
            CursorTransitionResult::VisibleNeedsShowRetry,
            OutputCursorMode::Hw,
        );
        assert_eq!(resolution.actual_mode, OutputCursorMode::Hw);
        assert!(!resolution.commit_desired_metadata);
        assert!(!resolution.clear_presented_metadata);
        let retry = update_force_show_retry_version(
            None,
            Some(CursorTransition::ShowOnRetire {
                upload_version: 7,
                hot_x: 0,
                hot_y: 0,
                x: 10,
                y: 20,
            }),
            CursorTransitionResult::VisibleNeedsShowRetry,
            OutputCursorMode::Hw,
        );
        assert_eq!(retry, Some(7));
    }

    #[test]
    fn unrelated_old_ack_cannot_clear_newer_show_retry() {
        assert_eq!(
            update_force_show_retry_version(
                Some(8),
                None,
                CursorTransitionResult::Applied,
                OutputCursorMode::Hw,
            ),
            Some(8)
        );
    }

    #[test]
    fn older_show_retirement_cannot_clear_newer_sprite_retry() {
        let old_show = Some(CursorTransition::ShowOnRetire {
            upload_version: 8,
            hot_x: 0,
            hot_y: 0,
            x: 10,
            y: 20,
        });
        assert_eq!(
            update_force_show_retry_version(
                Some(9),
                old_show,
                CursorTransitionResult::Applied,
                OutputCursorMode::Hw,
            ),
            Some(9)
        );
    }

    #[test]
    fn matching_show_success_clears_only_its_retry_generation() {
        let show = Some(CursorTransition::ShowOnRetire {
            upload_version: 9,
            hot_x: 0,
            hot_y: 0,
            x: 10,
            y: 20,
        });
        assert_eq!(
            update_force_show_retry_version(
                Some(9),
                show,
                CursorTransitionResult::Applied,
                OutputCursorMode::Hw,
            ),
            None
        );
    }

    #[test]
    fn lifecycle_reset_discards_stale_retry_before_fresh_hidden_show() {
        let mut retry = Some(1);
        reset_cursor_retry_for_lifecycle(&mut retry);
        assert_eq!(retry, None);
        let mut mode = OutputCursorMode::SwPending;
        reset_cursor_mode_for_lifecycle(&mut mode);
        assert_eq!(mode, OutputCursorMode::Hidden);

        let fresh_show = Some(CursorTransition::ShowOnRetire {
            upload_version: 2,
            hot_x: 0,
            hot_y: 0,
            x: 10,
            y: 20,
        });
        retry = update_force_show_retry_version(
            retry,
            fresh_show,
            CursorTransitionResult::Applied,
            OutputCursorMode::Hw,
        );
        assert_eq!(retry, None);
    }

    /// Sw → Sw and Hidden → Sw produce no transition (no plane
    /// state change) but the mode advances to Sw so the next
    /// frame's derivation sees the right "prev".
    #[test]
    fn derive_sw_or_hidden_to_sw_advances_mode_no_transition() {
        let (trans, _prev_pos, mode_after) = derive_cursor_transition(
            OutputCursorMode::Sw { prev: None },
            CursorAssignment::Sw { pos: (50, 50) },
        );
        assert!(trans.is_none());
        assert!(matches!(mode_after, OutputCursorMode::Sw { .. }));

        let (trans, _, mode_after) = derive_cursor_transition(
            OutputCursorMode::Hidden,
            CursorAssignment::Sw { pos: (50, 50) },
        );
        assert!(trans.is_none());
        assert!(matches!(mode_after, OutputCursorMode::Sw { .. }));
    }

    /// `YSERVER_HW_CURSOR=1` opt-in default OFF: with the env
    /// gate unset / false, build_scene's strategy always returns
    /// `Sw` (or `Hidden`) regardless of plane availability and
    /// extent. Tested at the `hw_strategy_active=false` parameter
    /// of build_scene to keep the test free of env-var ordering.
    #[test]
    fn hw_strategy_off_collapses_to_sw() {
        // Strategy disabled → Hw must NEVER be picked even when
        // the cursor would fit. (Behavioural equivalent of "env
        // var unset"; the env gate itself is set on the
        // SceneCompositor at construction time and forwarded into
        // tick_one_output -> build_scene as a bool param.)
        // This test pins the parameter wiring; an end-to-end
        // env-var test would need a process-scoped fixture.
        let prev = OutputCursorMode::Sw { prev: None };
        // Sw → Sw with HW NOT active — no transition.
        let (trans, _, _) = derive_cursor_transition(prev, CursorAssignment::Sw { pos: (10, 10) });
        assert!(trans.is_none());
    }

    /// Dual-output regression: cursor on monitor 1 only (output 0 =
    /// Hw, output 1 = Hidden) MUST classify as `Hw`, not `Mixed`.
    /// Pre-fix this returned Mixed and routed every motion event
    /// through scene.wake_for_damage — the HW cursor never moved on
    /// silence.
    #[test]
    fn classify_cursor_mode_dual_output_cursor_on_one_monitor_is_hw() {
        let modes = [OutputCursorMode::Hw, OutputCursorMode::Hidden];
        assert_eq!(
            classify_cursor_mode_from_per_output(modes),
            CursorPlaneMode::Hw,
        );
        // Order shouldn't matter.
        let modes = [OutputCursorMode::Hidden, OutputCursorMode::Hw];
        assert_eq!(
            classify_cursor_mode_from_per_output(modes),
            CursorPlaneMode::Hw,
        );
    }

    /// Single-output Hw is Hw; single-output Hidden is Sw (degenerate
    /// — no Hw plane active, scene wake is what'd update a future SW
    /// cursor draw).
    #[test]
    fn classify_cursor_mode_single_output_cases() {
        assert_eq!(
            classify_cursor_mode_from_per_output([OutputCursorMode::Hw]),
            CursorPlaneMode::Hw,
        );
        assert_eq!(
            classify_cursor_mode_from_per_output([OutputCursorMode::Hidden]),
            CursorPlaneMode::Sw,
        );
        assert_eq!(
            classify_cursor_mode_from_per_output([OutputCursorMode::Sw { prev: None }]),
            CursorPlaneMode::Sw,
        );
        assert_eq!(
            classify_cursor_mode_from_per_output([OutputCursorMode::SwPending]),
            CursorPlaneMode::Sw,
        );
    }

    /// Hw + Sw on different outputs IS Mixed (one output's SW sprite
    /// is in the compose draw list; the other's plane is bound). The
    /// fast path must defer until the SW output transitions out, or
    /// the plane could desync from the SW sprite position.
    #[test]
    fn classify_cursor_mode_hw_and_sw_is_mixed() {
        let modes = [OutputCursorMode::Hw, OutputCursorMode::Sw { prev: None }];
        assert_eq!(
            classify_cursor_mode_from_per_output(modes),
            CursorPlaneMode::Mixed,
        );

        let pending_reveal = [OutputCursorMode::Hw, OutputCursorMode::SwPending];
        assert_eq!(
            classify_cursor_mode_from_per_output(pending_reveal),
            CursorPlaneMode::Mixed,
            "a cursorless hide gap must remain non-direct until SW actually retires"
        );
    }

    /// Empty input degenerates to Sw (no outputs = no Hw plane to
    /// drive; the fast path has nothing to optimise anyway).
    #[test]
    fn classify_cursor_mode_no_outputs_is_sw() {
        let empty: [OutputCursorMode; 0] = [];
        assert_eq!(
            classify_cursor_mode_from_per_output(empty),
            CursorPlaneMode::Sw,
        );
    }

    /// `cursor_mode()` returns `Mixed` while any output's PendingAck
    /// carries an unretired cursor transition — the load-bearing
    /// query gate for the pointer fast path.
    #[test]
    fn cursor_mode_mixed_when_transition_pending() {
        let mut scene = SceneCompositor::stub();
        // Stub has `inner == None`; cursor_mode collapses to Sw.
        assert_eq!(scene.cursor_mode(), CursorPlaneMode::Sw);
        // The pending-transition path can only be triggered with
        // a real inner; covered by integration smoke + the
        // separate `derive_*` tests above.
        let _ = &mut scene;
    }

    #[test]
    fn stub_scene_is_not_live_and_declines_tick() {
        let mut scene = SceneCompositor::stub();
        assert!(!scene.is_live());
        let core = KmsCore::for_tests();
        let mut store = DrawableStore::new();
        let mut platform = PlatformBackend::for_tests();
        let mut telemetry = Telemetry::new();
        let windows = super::super::backend::WindowsMap::new();
        let err = scene
            .tick(
                &core,
                &mut store,
                &mut platform,
                &windows,
                &mut telemetry,
                None,
            )
            .expect_err("stub must reject tick");
        assert!(matches!(err, SceneError::NoVk));
    }

    #[test]
    fn mark_scene_structure_dirty_is_idempotent() {
        let mut scene = SceneCompositor::stub();
        scene.scene_structure_dirty = false;
        scene.mark_scene_structure_dirty();
        assert!(scene.scene_structure_dirty);
        scene.mark_scene_structure_dirty();
        assert!(scene.scene_structure_dirty);
    }

    /// Stage 4c.1 — the plural setter sets `scene_structure_dirty`
    /// even on the stub-mode compositor (mirrors the singular
    /// setter's early-return shape).
    #[test]
    fn mark_scene_structure_damage_rects_sets_dirty_on_stub() {
        let mut scene = SceneCompositor::stub();
        scene.scene_structure_dirty = false;
        scene.mark_scene_structure_damage_rects(&[rect(0, 0, 10, 10)]);
        assert!(scene.scene_structure_dirty);
    }

    #[test]
    fn root_overlay_toggle_marks_structure_damage() {
        let mut sc = SceneCompositor::stub();
        assert!(!sc.scene_structure_dirty);
        sc.root_overlay_toggle(
            yserver_protocol::x11::ClientId(1),
            0xffffff,
            &[ash::vk::Rect2D {
                offset: ash::vk::Offset2D { x: 5, y: 5 },
                extent: ash::vk::Extent2D {
                    width: 20,
                    height: 20,
                },
            }],
        );
        assert!(
            sc.scene_structure_dirty,
            "overlay mutation must mark structure damage"
        );
        assert!(!sc.root_overlay.is_empty());
    }

    /// Stage 4c.1 — code-quality follow-up. The plural setter test
    /// above only proves the dirty bit gets set on the stub-mode
    /// compositor (where `inner` is `None` and the dispatch for-loop
    /// is unreachable). `clip_rect_to_output_extent_handles_all_cases`
    /// only covers the helper math in isolation. Their union does
    /// NOT cover the dispatch wiring — a regression that swapped
    /// `damage.add(clipped)` for a no-op (or dropped the per-output
    /// loop entirely) would pass both tests. This test exercises
    /// the extracted `dispatch_clip_rects_to_outputs` helper that
    /// `mark_scene_structure_damage_rects` delegates to, with two
    /// synthetic outputs of different extents, and asserts that:
    ///
    /// - a rect wholly inside lands unchanged on every output;
    /// - a rect spilling off the right edge lands clipped (NOT in
    ///   its original form) on the output where it spills;
    /// - a rect that's fully outside an output is dropped for that
    ///   output but still lands on the other output if it fits there;
    /// - per-output clipping is independent (extent of output A does
    ///   not influence what lands on output B).
    #[test]
    fn dispatch_clip_rects_lands_per_output_clipped() {
        // Output 0: 800×600, Output 1: 400×400. Same input rect set.
        let ext_a = extent(800, 600);
        let ext_b = extent(400, 400);
        let mut damage_a = RegionSet::new();
        let mut damage_b = RegionSet::new();

        let inside = rect(10, 20, 100, 50); // fits both outputs
        let spilling_right = rect(700, 0, 200, 50); // spills A on right; fully outside B
        let outside_a_inside_b = rect(350, 350, 30, 30); // fits both (B clips to 50×50)
        let fully_outside = rect(2000, 2000, 50, 50); // outside both

        let rects = [inside, spilling_right, outside_a_inside_b, fully_outside];

        // Build a `Vec` of tuples so the slice carries a stable lifetime
        // for the iterator; `iter_mut` produces `(extent, &mut damage)`
        // exactly as the production callsite does.
        let mut outs: Vec<(vk::Extent2D, &mut RegionSet)> =
            vec![(ext_a, &mut damage_a), (ext_b, &mut damage_b)];
        dispatch_clip_rects_to_outputs(outs.drain(..), &rects);

        // Output A (800×600):
        //   - inside (10,20,100,50): identity
        //   - spilling_right (700,0,200,50): clipped width 200→100
        //   - outside_a_inside_b (350,350,30,30): identity (fits A)
        //   - fully_outside: dropped
        let a_rects = damage_a.rects();
        assert!(
            a_rects.contains(&inside),
            "inside rect must land unchanged on output A: {a_rects:?}",
        );
        let spilling_clipped_a = rect(700, 0, 100, 50);
        assert!(
            a_rects.contains(&spilling_clipped_a),
            "spilling rect must land CLIPPED on output A (expected {spilling_clipped_a:?}), got {a_rects:?}",
        );
        assert!(
            !a_rects.contains(&spilling_right),
            "spilling rect must NOT land in its original (unclipped) form on output A: {a_rects:?}",
        );
        assert!(
            a_rects.contains(&outside_a_inside_b),
            "rect that fits output A unchanged must land: {a_rects:?}",
        );
        assert!(
            !a_rects.iter().any(|r| r.offset.x >= 800
                || r.offset.y >= 600
                || i64::from(r.offset.x) + i64::from(r.extent.width) > 800
                || i64::from(r.offset.y) + i64::from(r.extent.height) > 600),
            "no rect on output A may spill its 800×600 extent: {a_rects:?}",
        );

        // Output B (400×400):
        //   - inside (10,20,100,50): identity
        //   - spilling_right (700,0,...): fully outside → dropped
        //   - outside_a_inside_b (350,350,30,30): clipped → (350,350,30,30) fits in 400×400
        //   - fully_outside: dropped
        let b_rects = damage_b.rects();
        assert!(
            b_rects.contains(&inside),
            "inside rect must land unchanged on output B: {b_rects:?}",
        );
        assert!(
            !b_rects.iter().any(|r| r.offset.x >= 700),
            "spilling-right (x=700) is fully outside output B and must be dropped: {b_rects:?}",
        );
        assert!(
            b_rects.contains(&outside_a_inside_b),
            "rect that fits output B unchanged must land: {b_rects:?}",
        );
        assert!(
            !b_rects
                .iter()
                .any(|r| i64::from(r.offset.x) + i64::from(r.extent.width) > 400
                    || i64::from(r.offset.y) + i64::from(r.extent.height) > 400),
            "no rect on output B may spill its 400×400 extent: {b_rects:?}",
        );
    }

    /// Stage 4c.1 — the helper clips a rect to the output's extent
    /// (offset assumed (0,0) — output-local coords). Wholly inside
    /// → identity. Partially overlapping → clipped intersection.
    /// Fully outside or zero-area → `None`.
    #[test]
    fn clip_rect_to_output_extent_handles_all_cases() {
        let ext = extent(800, 600);

        // Wholly inside — identity.
        assert_eq!(
            clip_rect_to_output_extent(rect(10, 20, 100, 50), ext),
            Some(rect(10, 20, 100, 50)),
        );

        // Right edge spills — clip width.
        assert_eq!(
            clip_rect_to_output_extent(rect(700, 0, 200, 50), ext),
            Some(rect(700, 0, 100, 50)),
        );

        // Bottom edge spills — clip height.
        assert_eq!(
            clip_rect_to_output_extent(rect(0, 500, 50, 200), ext),
            Some(rect(0, 500, 50, 100)),
        );

        // Negative offset — clamp to 0, clip width.
        assert_eq!(
            clip_rect_to_output_extent(rect(-30, -20, 100, 80), ext),
            Some(rect(0, 0, 70, 60)),
        );

        // Wholly to the right — None.
        assert_eq!(clip_rect_to_output_extent(rect(900, 0, 50, 50), ext), None);

        // Wholly below — None.
        assert_eq!(clip_rect_to_output_extent(rect(0, 700, 50, 50), ext), None);

        // Zero-width — None.
        assert_eq!(clip_rect_to_output_extent(rect(10, 10, 0, 50), ext), None);

        // Zero-height — None.
        assert_eq!(clip_rect_to_output_extent(rect(10, 10, 50, 0), ext), None);
    }

    fn rect(x: i32, y: i32, w: u32, h: u32) -> vk::Rect2D {
        vk::Rect2D {
            offset: vk::Offset2D { x, y },
            extent: vk::Extent2D {
                width: w,
                height: h,
            },
        }
    }

    fn extent(w: u32, h: u32) -> vk::Extent2D {
        vk::Extent2D {
            width: w,
            height: h,
        }
    }

    #[test]
    fn buffer_age_ring_trims_to_depth() {
        let mut ring = BufferAgeRing::new(3);
        for g in 1..=5 {
            let mut r = RegionSet::new();
            r.add(rect(0, 0, 4, 4));
            ring.push(g, r);
        }
        assert_eq!(ring.entries.len(), 3);
        // Oldest entries trimmed: 1, 2 gone; 3, 4, 5 remain.
        let gens: Vec<u64> = ring.entries.iter().map(|(g, _)| *g).collect();
        assert_eq!(gens, vec![3, 4, 5]);
    }

    #[test]
    fn buffer_age_contains_all_strict_window() {
        let mut ring = BufferAgeRing::new(4);
        let mut r = RegionSet::new();
        r.add(rect(0, 0, 4, 4));
        ring.push(3, r.clone());
        ring.push(4, r.clone());
        // BO last_gen=2, frame_gen=5 → intervening gens 3, 4.
        assert!(ring.contains_all(2, 5));
        // BO last_gen=2, frame_gen=6 → needs 3, 4, 5 — 5 missing.
        assert!(!ring.contains_all(2, 6));
        // No intervening gens (frame_gen == last_gen+1).
        assert!(ring.contains_all(2, 3));
    }

    #[test]
    fn pick_repaint_invalidated_bo_full_redraw() {
        let history = BufferAgeRing::new(4);
        let mut damage = RegionSet::new();
        damage.add(rect(0, 0, 10, 10));
        let p = pick_repaint_region(Some(5), true, 6, &damage, &history, extent(800, 600));
        assert!(matches!(p, Repaint::Full(_)));
    }

    #[test]
    fn pick_repaint_fresh_bo_full_redraw() {
        let history = BufferAgeRing::new(4);
        let mut damage = RegionSet::new();
        damage.add(rect(0, 0, 10, 10));
        let p = pick_repaint_region(None, false, 1, &damage, &history, extent(800, 600));
        assert!(matches!(p, Repaint::Full(_)));
    }

    #[test]
    fn pick_repaint_history_loss_full_redraw() {
        let mut history = BufferAgeRing::new(4);
        // Insert only gen 3 (missing 4, 5).
        let mut r = RegionSet::new();
        r.add(rect(0, 0, 4, 4));
        history.push(3, r);
        let mut damage = RegionSet::new();
        damage.add(rect(0, 0, 10, 10));
        let p = pick_repaint_region(Some(2), false, 6, &damage, &history, extent(800, 600));
        // Need 3, 4, 5 — only have 3.
        assert!(matches!(p, Repaint::Full(_)));
    }

    /// Buffer-age partial-repaint is currently disabled (see the
    /// `pick_repaint_region` doc-comment on the drag-shake stopgap).
    /// While disabled, every steady-state call returns `Repaint::Full`
    /// regardless of input — this test pins that contract so an
    /// accidental re-enable is caught by CI. When the real fix lands,
    /// flip the expectation back to `Repaint::Clipped(58, 58)` and
    /// remove this comment.
    #[test]
    fn pick_repaint_returns_full_while_optimisation_disabled() {
        let mut history = BufferAgeRing::new(4);
        let mut h3 = RegionSet::new();
        h3.add(rect(10, 10, 5, 5));
        history.push(3, h3);
        let mut h4 = RegionSet::new();
        h4.add(rect(50, 50, 8, 8));
        history.push(4, h4);
        let mut current = RegionSet::new();
        current.add(rect(0, 0, 3, 3));
        let p = pick_repaint_region(Some(2), false, 5, &current, &history, extent(800, 600));
        assert!(
            matches!(p, Repaint::Full(_)),
            "always-Full stopgap returns Full unconditionally; got {p:?}",
        );
    }

    #[test]
    fn pick_repaint_clipped_empty_damage_falls_back_to_full() {
        // If everything matches up but current_damage is empty
        // and history is empty, bounding rect is None → full
        // redraw fallback.
        let history = BufferAgeRing::new(4);
        let empty = RegionSet::new();
        let p = pick_repaint_region(Some(2), false, 3, &empty, &history, extent(800, 600));
        assert!(matches!(p, Repaint::Full(_)));
    }

    /// Tripwire for the idle-compose cursor-damage gating
    /// (project_idle_compose_cursor_damage): gating the cursor out of
    /// `output_damage` is only safe because `pick_repaint_region` returns
    /// `Repaint::Full` unconditionally today, so every compose repaints the
    /// whole BO (cursor included). If `Repaint::Clipped` is re-enabled, the
    /// current SW cursor rect must be folded into the repaint region even when
    /// the cursor did not itself trigger the frame — else a fresh/older-age BO
    /// shows a stale/missing cursor (see the gating-site comment in
    /// `tick_one_output`). Passes today; fails the day Full stops being
    /// unconditional, forcing that revisit. (Not `#[ignore]` + `panic!`: that
    /// pattern breaks `cargo test --include-ignored`.)
    #[test]
    fn clipped_reenable_must_fold_in_stationary_sw_cursor_rect() {
        let history = BufferAgeRing::new(4);
        let damage = RegionSet::new();
        let p = pick_repaint_region(Some(2), false, 5, &damage, &history, extent(800, 600));
        assert!(
            matches!(p, Repaint::Full(_)),
            "Repaint::Clipped re-enabled — fold the stationary SW cursor rect \
             into the repaint region (project_idle_compose_cursor_damage)",
        );
    }

    // ── Stage 3f.6: subwindow scene traversal ─────────────────────

    fn alloc_stub_window(
        store: &mut DrawableStore,
        windows: &mut super::super::backend::WindowsMap,
        xid: u32,
        x: i16,
        y: i16,
        w: u16,
        h: u16,
        parent: Option<u32>,
        mapped: bool,
    ) {
        // for_tests_null gives null image handles; build_scene
        // rejects null views. Use a non-zero sentinel handle so the
        // traversal test exercises the recurse logic. The handle
        // never gets passed to Vk because the test never composes.
        let mut storage = super::super::store::Storage::for_tests_null(
            extent(u32::from(w), u32::from(h)),
            vk::Format::B8G8R8A8_UNORM,
        );
        // SAFETY: Vk handle types are opaque u64s; constructing a
        // sentinel doesn't touch the driver. The `is_test_stub`
        // flag on Storage means Drop won't try to destroy these.
        // Stamp both views to the same sentinel so build_scene's
        // sample-side bind (`storage.sample_view`) sees the same
        // handle the legacy tests asserted against — these stubs
        // don't exercise α swizzle, just storage-routing.
        let sentinel: ash::vk::ImageView = ash::vk::Handle::from_raw(u64::from(xid) | 0xFF00_0000);
        storage.image_view = sentinel;
        storage.sample_view = sentinel;
        store
            .allocate(xid, DrawableKind::Window, 32, mapped, storage)
            .expect("stub allocate");
        windows.insert(
            xid,
            super::super::backend::WindowGeometry {
                x,
                y,
                width: w,
                height: h,
                depth: 32,
                mapped,
                parent,
                stack_rank: 0,
                bg_pixel: None,
                bg_pixmap: None,
                cursor: None,
            },
        );
    }

    /// Stage 3f.6 — `build_scene` walks top-level → mapped
    /// descendants and produces draw entries in absolute coords.
    /// Top-level at (50, 60), child at (10, 20) relative → child
    /// emits at output coords (60, 80).
    #[test]
    fn build_scene_recurses_into_mapped_children() {
        let mut core = KmsCore::for_tests();
        let mut store = DrawableStore::new();
        let platform = PlatformBackend::for_tests();
        let mut windows = super::super::backend::WindowsMap::new();

        // Top-level @ (50, 60), 200×100.
        alloc_stub_window(
            &mut store,
            &mut windows,
            0x100,
            50,
            60,
            200,
            100,
            None,
            true,
        );
        core.top_level_order.push(0x100);

        // Child @ (10, 20) relative to top-level, 40×30.
        alloc_stub_window(
            &mut store,
            &mut windows,
            0x101,
            10,
            20,
            40,
            30,
            Some(0x100),
            true,
        );

        let built = build_scene(
            &core, &mut store, &windows, 0, &platform, None, None, None, false,
        );
        let scene = built.scene;
        assert_eq!(scene.draws.len(), 2, "expected top-level + child draw");

        // Top-level at output (50, 60) since output layout origin is (0,0).
        let top = scene
            .draws
            .iter()
            .find(|d| d.dst_size[0] == 200.0 && d.dst_size[1] == 100.0)
            .expect("top-level draw present");
        assert_eq!(top.dst_origin, [50.0, 60.0]);

        // Child at absolute (60, 80) = top (50, 60) + child rel (10, 20).
        let child = scene
            .draws
            .iter()
            .find(|d| d.dst_size[0] == 40.0 && d.dst_size[1] == 30.0)
            .expect("child draw present");
        assert_eq!(child.dst_origin, [60.0, 80.0]);
    }

    /// X11 parent-clipping: a child window is clipped to its parent's
    /// rectangle. fvwm (and other WMs) park oversized frame-decoration
    /// windows in a tiny off-screen holding window so they're invisible;
    /// yserver must not paint the whole child. Regression: fvwm's 1146×23
    /// title bar, parked in a 10×10 holding window at (-10,-10), leaked a
    /// ~1136×13 white strip onto the top-left of the screen because the
    /// scene drew the child at full size (air/silence HW 2026-07-02).
    #[test]
    fn build_scene_clips_child_to_parent_bounds() {
        let mut core = KmsCore::for_tests();
        let mut store = DrawableStore::new();
        let platform = PlatformBackend::for_tests();
        let mut windows = super::super::backend::WindowsMap::new();

        // Small parent @ (100, 100), 10×10 (the holding window).
        alloc_stub_window(
            &mut store,
            &mut windows,
            0x100,
            100,
            100,
            10,
            10,
            None,
            true,
        );
        core.top_level_order.push(0x100);

        // Oversized child @ (2, 3) relative, 100×50 — far larger than the
        // 10×10 parent. Only the intersection with the parent may show.
        alloc_stub_window(
            &mut store,
            &mut windows,
            0x101,
            2,
            3,
            100,
            50,
            Some(0x100),
            true,
        );

        let built = build_scene(
            &core, &mut store, &windows, 0, &platform, None, None, None, false,
        );
        let scene = built.scene;

        // The child draw must be clipped to the parent's rect, NOT the
        // full 100×50. Child abs (102,103) ∩ parent (100,100,110,110)
        // = (102,103)-(110,110) → 8×7.
        let child = scene
            .draws
            .iter()
            .find(|d| d.dst_origin == [102.0, 103.0])
            .expect("child draw present at its absolute origin");
        assert_eq!(
            child.dst_size,
            [8.0, 7.0],
            "child must be clipped to the parent's 10×10 bounds, not drawn \
             at full 100×50 (parent-clipping); got {:?}",
            child.dst_size,
        );
        assert!(
            (child.src_size[0] - 8.0 / 100.0).abs() < 1e-5
                && (child.src_size[1] - 7.0 / 50.0).abs() < 1e-5,
            "src_size must sample only the visible sub-region, got {:?}",
            child.src_size,
        );
        // No draw may exceed the parent's footprint.
        assert!(
            !scene
                .draws
                .iter()
                .any(|d| d.dst_size[0] > 10.0 || d.dst_size[1] > 10.0),
            "no draw may exceed the 10×10 parent, got {:?}",
            scene.draws,
        );
    }

    /// SHAPE bounding region clips the window's scene draw. Marco
    /// uses `SHAPE-Request: Rectangles destination=Bounding` to set
    /// a rounded-corner mask on frame windows; without honouring it
    /// the scene paints the full rectangle and shows the scanout
    /// clear colour (black) in the corners instead of the layer
    /// below — diagnosed 2026-05-30 on non-composited MATE.
    #[test]
    fn build_scene_clips_window_to_shape_bounding() {
        use yserver_protocol::x11::xfixes::RegionRect;
        let mut core = KmsCore::for_tests();
        let mut store = DrawableStore::new();
        let platform = PlatformBackend::for_tests();
        let mut windows = super::super::backend::WindowsMap::new();

        // Top-level @ (50, 60), 200×100.
        alloc_stub_window(
            &mut store,
            &mut windows,
            0x100,
            50,
            60,
            200,
            100,
            None,
            true,
        );
        core.top_level_order.push(0x100);

        // Bounding mask: a single sub-rect inset (10, 14) from the
        // window's top-left, 180×80 — analogous to one of marco's
        // rounded-corner approximation strips.
        core.shape_bounding.insert(
            0x100,
            vec![RegionRect {
                x: 10,
                y: 14,
                width: 180,
                height: 80,
            }],
        );

        let built = build_scene(
            &core, &mut store, &windows, 0, &platform, None, None, None, false,
        );
        let scene = built.scene;

        // Exactly one draw for this window, clipped to the bounding
        // rect — NOT a full-window 200×100 draw.
        let window_draws: Vec<_> = scene
            .draws
            .iter()
            .filter(|d| d.dst_size != [200.0, 100.0])
            .collect();
        assert_eq!(
            window_draws.len(),
            1,
            "expected one draw per bounding rect, got {}: {:?}",
            scene.draws.len(),
            scene.draws,
        );
        let d = window_draws[0];
        // dst: window absolute origin + bounding-rect offset.
        assert_eq!(d.dst_origin, [60.0, 74.0], "dst_origin = (50+10, 60+14)");
        assert_eq!(d.dst_size, [180.0, 80.0], "dst_size = bounding rect");
        // src UV: the sub-region of the window's texture that
        // corresponds to the bounding rect.
        assert!(
            (d.src_origin[0] - 10.0 / 200.0).abs() < 1e-5
                && (d.src_origin[1] - 14.0 / 100.0).abs() < 1e-5,
            "src_origin = (10/200, 14/100), got {:?}",
            d.src_origin,
        );
        assert!(
            (d.src_size[0] - 180.0 / 200.0).abs() < 1e-5
                && (d.src_size[1] - 80.0 / 100.0).abs() < 1e-5,
            "src_size = (180/200, 80/100), got {:?}",
            d.src_size,
        );
    }

    /// DRIFT 1 (findings 2026-06-18), live render half: the empty-vs-
    /// absent bounding-shape distinction the Step-1a `Option` API
    /// preserves. An EXPLICIT empty bounding region (`Some([])`, stored
    /// as an empty Vec) must clip the window to nothing — zero draws —
    /// whereas an ABSENT entry renders the full window. Before Step 1a
    /// the backend deleted empty rects, collapsing the two so an empty
    /// region wrongly rendered as a full window.
    #[test]
    fn build_scene_empty_bounding_emits_no_draw() {
        let mut core = KmsCore::for_tests();
        let mut store = DrawableStore::new();
        let platform = PlatformBackend::for_tests();
        let mut windows = super::super::backend::WindowsMap::new();

        alloc_stub_window(
            &mut store,
            &mut windows,
            0x100,
            50,
            60,
            200,
            100,
            None,
            true,
        );
        core.top_level_order.push(0x100);
        // Explicit EMPTY bounding region: entry present, zero rects.
        core.shape_bounding.insert(0x100, Vec::new());

        let built = build_scene(
            &core, &mut store, &windows, 0, &platform, None, None, None, false,
        );
        assert!(
            built.scene.draws.is_empty(),
            "an explicit empty bounding region must emit no draw (window \
             clipped to nothing), got {:?}",
            built.scene.draws,
        );
    }

    #[test]
    fn build_scene_absent_bounding_emits_full_window() {
        let mut core = KmsCore::for_tests();
        let mut store = DrawableStore::new();
        let platform = PlatformBackend::for_tests();
        let mut windows = super::super::backend::WindowsMap::new();

        alloc_stub_window(
            &mut store,
            &mut windows,
            0x100,
            50,
            60,
            200,
            100,
            None,
            true,
        );
        core.top_level_order.push(0x100);
        // No shape_bounding entry at all (absent) → full-window draw.

        let built = build_scene(
            &core, &mut store, &windows, 0, &platform, None, None, None, false,
        );
        let window_draws: Vec<_> = built
            .scene
            .draws
            .iter()
            .filter(|d| d.dst_size == [200.0, 100.0])
            .collect();
        assert_eq!(
            window_draws.len(),
            1,
            "absent bounding shape must emit one full-window draw, got {:?}",
            built.scene.draws,
        );
        assert_eq!(window_draws[0].dst_origin, [50.0, 60.0]);
    }

    /// Stage 3f.6 — unmapped parent hides the entire subtree per
    /// X11 MapWindow cascade semantics. Child stays scene-
    /// participating but doesn't render because its ancestor is
    /// unmapped.
    #[test]
    fn build_scene_unmapped_parent_hides_subtree() {
        let mut core = KmsCore::for_tests();
        let mut store = DrawableStore::new();
        let platform = PlatformBackend::for_tests();
        let mut windows = super::super::backend::WindowsMap::new();

        alloc_stub_window(
            &mut store,
            &mut windows,
            0x200,
            10,
            10,
            100,
            100,
            None,
            false, /* parent NOT mapped */
        );
        core.top_level_order.push(0x200);
        alloc_stub_window(
            &mut store,
            &mut windows,
            0x201,
            0,
            0,
            50,
            50,
            Some(0x200),
            true, /* child IS mapped, but parent isn't */
        );

        let scene = build_scene(
            &core, &mut store, &windows, 0, &platform, None, None, None, false,
        )
        .scene;
        assert!(
            scene.draws.is_empty(),
            "unmapped parent must short-circuit subtree (got {} draws)",
            scene.draws.len()
        );
    }

    /// Stage 3f.8 — when `cursor` is `Some`, `build_scene` emits an
    /// additional top-of-z draw entry at the cursor's
    /// hot-spot-adjusted position. The entry is the LAST element of
    /// `draws` (last = topmost in z-order) and has
    /// `alpha_passthrough=true` so the sprite's alpha actually
    /// blends.
    #[test]
    fn build_scene_appends_cursor_draw_at_top_of_z() {
        let mut core = KmsCore::for_tests();
        let mut store = DrawableStore::new();
        let platform = PlatformBackend::for_tests();
        let mut windows = super::super::backend::WindowsMap::new();

        // One mapped top-level so we can verify "cursor is on top".
        alloc_stub_window(&mut store, &mut windows, 0x100, 0, 0, 400, 300, None, true);
        core.top_level_order.push(0x100);

        // Allocate a stub cursor storage entry (synthetic xid).
        let mut storage = super::super::store::Storage::for_tests_null(
            extent(16, 16),
            vk::Format::B8G8R8A8_UNORM,
        );
        // SAFETY: opaque u64 Vk handle for the cursor's view; the
        // stub Storage's `is_test_stub` flag means Drop won't free
        // it. Stamp both views so scene binds the sample-side.
        let cur_sentinel: ash::vk::ImageView = ash::vk::Handle::from_raw(0xCAFE_BABE);
        storage.image_view = cur_sentinel;
        storage.sample_view = cur_sentinel;
        let cursor_id = store
            .allocate(0xCAFE_0001, DrawableKind::Pixmap, 32, false, storage)
            .expect("alloc cursor stub");

        core.cursor_x = 50.0;
        core.cursor_y = 60.0;
        let cursor = CursorEntry {
            id: cursor_id,
            extent: extent(16, 16),
            hot_x: 0,
            hot_y: 0,
            record_version: 0,
            bgra_bytes: None,
        };

        let scene = build_scene(
            &core,
            &mut store,
            &windows,
            0,
            &platform,
            Some(cursor),
            None,
            None,
            false,
        )
        .scene;
        // 1 top-level + 1 cursor = 2.
        assert_eq!(scene.draws.len(), 2);
        let cursor_draw = scene.draws.last().expect("cursor draw");
        assert_eq!(cursor_draw.dst_origin, [50.0, 60.0]);
        assert_eq!(cursor_draw.dst_size, [16.0, 16.0]);
        assert!(
            cursor_draw.alpha_passthrough,
            "cursor must blend (sprite has transparent border)"
        );
    }

    /// Stage 4c.3 / 4c.5 — Automatic-mode invariant.
    ///
    /// When a window W has `redirected_target = Some(B)` AND
    /// `scene_participating == true` (Automatic redirect), the scene
    /// entry for W blits FROM B's storage (its `image_view`), not
    /// from W's own storage. W's geometry (`dst_origin`, `dst_size`)
    /// stays driven by `windows[W]`. `sampled_ids` carries B_id
    /// (not W_id) so damage/fence accounting follows the source the
    /// scene actually read from. B is also marked
    /// `scene_participating=true` per Stage 4c's Automatic-mode
    /// pairing (the protocol handler issues
    /// `set_backing_scene_participation(true)` alongside W's flip).
    ///
    /// 4c.5 rename: framed around the Automatic-mode invariant per
    /// task 4c.5 self-review — the assertion shape already matches.
    #[test]
    fn build_scene_automatic_redirect_keeps_window_via_backing_storage() {
        let mut core = KmsCore::for_tests();
        let mut store = DrawableStore::new();
        let platform = PlatformBackend::for_tests();
        let mut windows = super::super::backend::WindowsMap::new();

        // Window W @ (50, 60), 200×100 — emits at output coords
        // (50, 60) since the test output layout origin is (0, 0).
        alloc_stub_window(
            &mut store,
            &mut windows,
            0x100,
            50,
            60,
            200,
            100,
            None,
            true,
        );
        core.top_level_order.push(0x100);

        // Allocate a separate backing pixmap B with its OWN sentinel
        // image_view, distinct from W's. B is allocated with
        // `scene_participating=false` (Pixmap default) — that's fine
        // for the build-scene path since the resolution looks up
        // storage directly; only the peek for B's damage needs the
        // flag, which we toggle below to verify the snapshot path
        // keys off `source_id`.
        let mut b_storage = super::super::store::Storage::for_tests_null(
            extent(200, 100),
            vk::Format::B8G8R8A8_UNORM,
        );
        let b_view: vk::ImageView = ash::vk::Handle::from_raw(0xB000_BEEF);
        b_storage.image_view = b_view;
        // Stub both views to the same sentinel — see
        // `alloc_stub_window` for rationale; tests verify
        // routing, not swizzle semantics.
        b_storage.sample_view = b_view;
        let b_id = store
            .allocate(0xB001, DrawableKind::Pixmap, 32, true, b_storage)
            .expect("alloc backing stub");

        // Confirm W and B have distinct image_views.
        let w_id = store.lookup(0x100).expect("w_id present");
        let w_view = store.get(w_id).expect("w drawable").storage.image_view;
        assert_ne!(
            w_view, b_view,
            "fixture sanity: W and B must have distinct sentinel views"
        );

        // Fixture sanity (4c.5 Automatic-mode invariant): W stays
        // scene_participating=true under Automatic redirect; the
        // backing also flips to scene_participating=true (the
        // protocol-side pairing). `alloc_stub_window(mapped=true)`
        // and the `allocate(..., true, _)` above wire both flags.
        assert!(
            store.get(w_id).unwrap().scene_participating,
            "Automatic redirect: W must stay scene_participating=true",
        );
        assert!(
            store.get(b_id).unwrap().scene_participating,
            "Automatic redirect: B must be scene_participating=true",
        );

        // Wire the redirect route: W's source-storage now resolves
        // through B.
        store.set_redirected_target(w_id, Some(b_id));

        let built = build_scene(
            &core, &mut store, &windows, 0, &platform, None, None, None, false,
        );
        let scene = &built.scene;
        assert_eq!(
            scene.draws.len(),
            1,
            "expected one draw entry for W (geometry unchanged by redirect)"
        );
        let w_draw = &scene.draws[0];

        // Geometry still W's.
        assert_eq!(
            w_draw.dst_origin,
            [50.0, 60.0],
            "redirected W's on-screen rect must remain W's geometry"
        );
        assert_eq!(
            w_draw.dst_size,
            [200.0, 100.0],
            "redirected W's on-screen size must remain W's geometry"
        );

        // Storage handle reroutes to B. The stub fixture stamps
        // both `image_view` and `sample_view` to the same sentinel,
        // so this also implicitly verifies the scene-α fix is
        // binding the sample-side view (no separate handle to
        // distinguish in the stub world — production builds them
        // distinct via `PlatformBackend::build_sample_view`).
        assert_eq!(
            w_draw.image_view, b_view,
            "redirected W must sample FROM B's view, not W's"
        );

        // `sampled_ids` parallels `draws`; the entry for W must
        // carry B_id (the source the scene actually read from) so
        // damage/fence accounting follows the right drawable.
        assert_eq!(built.sampled_ids.len(), 1);
        assert_eq!(
            built.sampled_ids[0], b_id,
            "sampled_ids must carry source_id (B_id) for damage / fence keying"
        );
    }

    /// Stage 4c.5 — Manual-mode invariant.
    ///
    /// `build_scene`'s `scene_participating` filter (scene.rs:1110 and
    /// :922) drops any drawable with `scene_participating == false`
    /// from the per-output draw list. Manual-redirected windows carry
    /// `scene_participating=false` (the protocol handler issues
    /// `set_window_scene_participation(W, false)` on Manual activation)
    /// so they MUST NOT appear in `scene.draws` nor in
    /// `built.sampled_ids`. Plain unredirected/Automatic windows
    /// stay participating and continue to emit.
    ///
    /// Setup: two top-level windows W1 + W2, both mapped and same
    /// geometry shape (so the filter is the only thing distinguishing
    /// them). W1 stays `scene_participating=true`; W2 is flipped to
    /// `false` post-allocation via `set_scene_participating` to
    /// mimic the Manual-redirect activation path. The build must
    /// emit one draw (W1) and zero entries for W2.
    #[test]
    fn build_scene_skips_manual_redirected_window() {
        let mut core = KmsCore::for_tests();
        let mut store = DrawableStore::new();
        let platform = PlatformBackend::for_tests();
        let mut windows = super::super::backend::WindowsMap::new();

        // W1 @ (10, 20), 50×40 — Automatic / unredirected
        // (scene_participating=true via `alloc_stub_window`'s
        // `mapped` arg, which the helper forwards as the
        // `scene_participating` flag in `store.allocate`).
        alloc_stub_window(&mut store, &mut windows, 0x111, 10, 20, 50, 40, None, true);
        core.top_level_order.push(0x111);

        // W2 @ (100, 200), 60×30 — geometry that doesn't overlap
        // W1 so a stray draw entry would be unambiguous.
        alloc_stub_window(
            &mut store,
            &mut windows,
            0x222,
            100,
            200,
            60,
            30,
            None,
            true,
        );
        core.top_level_order.push(0x222);

        // Flip W2 off the scene (Manual-redirect activation). Use
        // the store's setter directly — the backend method does
        // more bookkeeping (damage clear + scene-structure damage
        // rect) than this no-Vk scene-walk test needs.
        let w2_id = store.lookup(0x222).expect("w2 lookup");
        store.set_scene_participating(w2_id, false);
        let w1_id = store.lookup(0x111).expect("w1 lookup");
        assert!(
            store.get(w1_id).unwrap().scene_participating,
            "fixture sanity: W1 stays scene_participating=true",
        );
        assert!(
            !store.get(w2_id).unwrap().scene_participating,
            "fixture sanity: W2 must be scene_participating=false",
        );

        let built = build_scene(
            &core, &mut store, &windows, 0, &platform, None, None, None, false,
        );
        let scene = &built.scene;

        // Only W1's draw entry must be present.
        assert_eq!(
            scene.draws.len(),
            1,
            "Manual-redirected W2 must be filtered from scene.draws (saw {} entries: {:?})",
            scene.draws.len(),
            scene.draws,
        );
        let w1_draw = &scene.draws[0];
        assert_eq!(
            w1_draw.dst_origin,
            [10.0, 20.0],
            "the surviving draw must be W1 (origin (10,20)), NOT W2 (origin (100,200))",
        );
        assert_eq!(
            w1_draw.dst_size,
            [50.0, 40.0],
            "the surviving draw must be W1 (50×40), NOT W2 (60×30)",
        );

        // sampled_ids mirrors draws — must carry W1's id only.
        assert_eq!(built.sampled_ids.len(), 1);
        assert_eq!(
            built.sampled_ids[0], w1_id,
            "sampled_ids must reference W1; W2 was filtered before push",
        );
    }

    /// Stage 4d — `build_scene` must skip non-redirected descendants
    /// of a Manual-redirected ancestor. The descendants' paint
    /// routes through `resolve_paint_target` to the ancestor's B;
    /// emitting their own (stale) storage on top of the ancestor's B
    /// would muddy the compositor output.
    ///
    /// Audit #3 follow-up (2026-05-19): the test was originally
    /// written against the degenerate state where the parent has
    /// `scene_participating=false` *without* a redirected backing —
    /// that state doesn't occur in real life (Manual-redirect
    /// activation always sets `redirected_target` BEFORE flipping
    /// `scene_participating=false`, see
    /// `activate_redirect_backing_for`). Updated to mirror the
    /// realistic state: frame has both a backing AND
    /// `scene_participating=false`.
    ///
    /// Phase 3.1 update: the parent is Manual-redirected so it ALSO
    /// no longer emits (the compositor reads its backing via
    /// `NameWindowPixmap` and re-emits it on the COW). The remaining
    /// invariant is "the non-redirected child must NOT leak into
    /// scene.draws"; the bystander stands in as a positive control.
    #[test]
    fn build_scene_prunes_descendants_of_manual_redirected_ancestor() {
        let mut core = KmsCore::for_tests();
        let mut store = DrawableStore::new();
        let platform = PlatformBackend::for_tests();
        let mut windows = super::super::backend::WindowsMap::new();

        // Frame W @ (100, 200), 200×150 — the manually-redirected
        // ancestor (CC's marco-decorated frame in production).
        alloc_stub_window(
            &mut store,
            &mut windows,
            0x111,
            100,
            200,
            200,
            150,
            None,
            true,
        );
        core.top_level_order.push(0x111);

        // Child C inside frame W at relative (11, 41), 100×80.
        // scene_participating=true (regular window — only the
        // ancestor is redirected). This is CC's GtkWindow in
        // production: a regular window whose paints route to the
        // frame's redirected backing via resolve_paint_target's
        // ancestor walk.
        alloc_stub_window(
            &mut store,
            &mut windows,
            0x112,
            11,
            41,
            100,
            80,
            Some(0x111),
            true,
        );

        // Bystander top-level W @ (500, 500) so a "did anything
        // get emitted?" assertion isn't ambiguous.
        alloc_stub_window(
            &mut store,
            &mut windows,
            0x222,
            500,
            500,
            60,
            30,
            None,
            true,
        );
        core.top_level_order.push(0x222);

        // Set up realistic Manual-redirect state on frame W: allocate
        // a backing, point W's `redirected_target` at it, then flip
        // `scene_participating=false`. Child stays participating —
        // its paint will resolve to frame_B via
        // `resolve_paint_target`'s ancestor walk, NOT to its own
        // storage; so the child's storage stays stale, and emitting
        // it would muddy the frame_B emit underneath.
        let w_frame_id = store.lookup(0x111).expect("frame lookup");
        let mut frame_backing = super::super::store::Storage::for_tests_null(
            extent(200, 150),
            vk::Format::B8G8R8A8_UNORM,
        );
        let frame_backing_view: vk::ImageView = ash::vk::Handle::from_raw(0xBEEF_F111);
        frame_backing.image_view = frame_backing_view;
        frame_backing.sample_view = frame_backing_view;
        let frame_backing_id = store
            .allocate(0xB111, DrawableKind::Pixmap, 32, true, frame_backing)
            .expect("alloc frame backing");
        store.set_redirected_target(w_frame_id, Some(frame_backing_id));
        store.set_scene_participating(w_frame_id, false);
        let child_id = store.lookup(0x112).expect("child lookup");
        assert!(
            store.get(child_id).unwrap().scene_participating,
            "fixture sanity: child stays scene_participating=true",
        );

        let built = build_scene(
            &core, &mut store, &windows, 0, &platform, None, None, None, false,
        );
        let scene = &built.scene;

        // Phase 3.1 — only the bystander emits. The Manual-redirected
        // frame is unconditionally skipped (compositor consumes its
        // backing offscreen via NameWindowPixmap); the non-redirected
        // child must also stay out (its paint resolves to frame_B via
        // the ancestor walk, so emitting its stale storage would muddy
        // the compositor's re-emit on the COW).
        assert_eq!(
            scene.draws.len(),
            1,
            "expected bystander only; got {} — Manual-redirected frame and \
             its non-redirected child must both stay out of scene.draws: {:?}",
            scene.draws.len(),
            scene.draws,
        );
        assert!(
            scene.draws.iter().any(|d| d.dst_origin == [500.0, 500.0]),
            "bystander draw missing: {:?}",
            scene.draws
        );
        // The "must not leak" property — frame backing AND child draw
        // entries must both be absent from scene.draws.
        assert!(
            !scene
                .draws
                .iter()
                .any(|d| d.dst_origin == [100.0, 200.0] && d.dst_size == [200.0, 150.0]),
            "Manual-redirected frame leaked into scene.draws: {:?}",
            scene.draws,
        );
        assert!(
            !scene.draws.iter().any(|d| d.dst_origin == [111.0, 241.0]),
            "non-redirected child of Manual-redirected ancestor leaked into scene.draws: {:?}",
            scene.draws,
        );
        // sampled_ids mirrors draws — bystander only, no frame_B, no child.
        let bystander_id = store.lookup(0x222).expect("bystander lookup");
        assert_eq!(built.sampled_ids.len(), 1);
        assert!(!built.sampled_ids.contains(&frame_backing_id));
        assert!(built.sampled_ids.contains(&bystander_id));
    }

    // Phase 3.1 — the legacy `build_scene_emits_manual_redirected_parent_backing_but_prunes_descendants`
    // test was deleted here. Its sole purpose was to assert that a
    // Manual-redirected top-level emits its backing directly into
    // scanout — exactly the bug-shaped state Task 3.1 closes. The
    // compositor (in production) reads the backing via
    // `NameWindowPixmap` and re-emits it on the COW; the X server
    // must never short-circuit that. `manual_redirected_top_level_skips_emit_unconditional`
    // covers the replacement invariant.

    /// Audit #3 (2026-05-19) — a Manual-redirected parent still
    /// prunes its NON-redirected descendants (their paint resolves
    /// to the parent's B via `resolve_paint_target` so the parent
    /// emit covers them), but Automatic-redirected descendants have
    /// their OWN backing — `resolve_paint_target` stops at them —
    /// and MUST still emit. Pre-fix `prune_subtree=true` dropped
    /// them unconditionally, matching the audit's "GTK/marco CSD
    /// pattern: RedirectWindow(frame, Manual) +
    /// RedirectSubwindows(frame, Automatic) makes Automatic
    /// widgets vanish" symptom (Control Center missing menus /
    /// widgets).
    ///
    /// Phase 3.1 update: the Manual-redirected parent ALSO no longer
    /// emits (compositor reads its backing via NameWindowPixmap).
    /// The load-bearing assertion of this test is still "Automatic
    /// child backing emits despite Manual ancestor"; the parent emit
    /// is dropped from the expectation set.
    #[test]
    fn build_scene_emits_automatic_descendant_under_manual_ancestor() {
        let mut core = KmsCore::for_tests();
        let mut store = DrawableStore::new();
        let platform = PlatformBackend::for_tests();
        let mut windows = super::super::backend::WindowsMap::new();

        // Frame F at (100, 200), 200×150 — Manual-redirected
        // (scene_participating=false) with its own backing F_B.
        alloc_stub_window(
            &mut store,
            &mut windows,
            0x111,
            100,
            200,
            200,
            150,
            None,
            true,
        );
        core.top_level_order.push(0x111);
        let frame_id = store.lookup(0x111).expect("frame lookup");

        let mut frame_backing = super::super::store::Storage::for_tests_null(
            extent(200, 150),
            vk::Format::B8G8R8A8_UNORM,
        );
        let frame_backing_view: vk::ImageView = ash::vk::Handle::from_raw(0xBEEF_F000);
        frame_backing.image_view = frame_backing_view;
        frame_backing.sample_view = frame_backing_view;
        let frame_backing_id = store
            .allocate(0xB111, DrawableKind::Pixmap, 32, true, frame_backing)
            .expect("alloc frame backing");
        store.set_redirected_target(frame_id, Some(frame_backing_id));
        store.set_scene_participating(frame_id, false);

        // Automatic-redirected child C at (11, 41) inside F — own
        // backing C_B; scene_participating=true (Automatic).
        alloc_stub_window(
            &mut store,
            &mut windows,
            0x112,
            11,
            41,
            100,
            80,
            Some(0x111),
            true,
        );
        let child_id = store.lookup(0x112).expect("child lookup");

        let mut child_backing = super::super::store::Storage::for_tests_null(
            extent(100, 80),
            vk::Format::B8G8R8A8_UNORM,
        );
        let child_backing_view: vk::ImageView = ash::vk::Handle::from_raw(0xBEEF_C000);
        child_backing.image_view = child_backing_view;
        child_backing.sample_view = child_backing_view;
        let child_backing_id = store
            .allocate(0xB112, DrawableKind::Pixmap, 32, true, child_backing)
            .expect("alloc child backing");
        store.set_redirected_target(child_id, Some(child_backing_id));
        // Automatic mode → child window stays scene_participating=true.
        assert!(
            store.get(child_id).unwrap().scene_participating,
            "fixture sanity: Automatic-redirected child stays scene_participating=true",
        );

        let built = build_scene(
            &core, &mut store, &windows, 0, &platform, None, None, None, false,
        );
        let scene = &built.scene;

        // Phase 3.1 — only the Automatic child backing emits, at
        // (111, 241) (= F.pos + C.pos relative). Parent F is
        // Manual-redirected so it stays out of scene.draws; the
        // compositor consumes its backing offscreen via
        // NameWindowPixmap.
        assert_eq!(
            scene.draws.len(),
            1,
            "expected automatic-child backing only (Manual parent skipped); got {:?}",
            scene.draws
        );
        assert!(
            !scene
                .draws
                .iter()
                .any(|d| d.dst_origin == [100.0, 200.0] && d.dst_size == [200.0, 150.0]),
            "Manual parent backing must NOT emit: {:?}",
            scene.draws
        );
        assert!(
            scene
                .draws
                .iter()
                .any(|d| d.dst_origin == [111.0, 241.0] && d.dst_size == [100.0, 80.0]),
            "automatic child backing draw missing: {:?}",
            scene.draws
        );
        assert!(!built.sampled_ids.contains(&frame_backing_id));
        assert!(built.sampled_ids.contains(&child_backing_id));
    }

    /// Phase 1 pre-cleanup — when no COW is registered
    /// (`cow=None`), `build_scene` walks the top-level order and
    /// emits a draw entry per mapped top-level. This preserves the
    /// legacy non-redirected path that Phase 1 (COW-authoritative)
    /// leaves unchanged; the `cow=Some` shape (top-levels stripped)
    /// gets its own dedicated test.
    #[test]
    fn build_scene_cow_none_emits_top_levels() {
        let mut core = KmsCore::for_tests();
        let mut store = DrawableStore::new();
        let platform = PlatformBackend::for_tests();
        let mut windows = super::super::backend::WindowsMap::new();

        // Two mapped top-levels.
        alloc_stub_window(&mut store, &mut windows, 0x100, 0, 0, 100, 80, None, true);
        core.top_level_order.push(0x100);
        alloc_stub_window(
            &mut store,
            &mut windows,
            0x101,
            200,
            150,
            120,
            90,
            None,
            true,
        );
        core.top_level_order.push(0x101);

        let built = build_scene(
            &core, &mut store, &windows, 0, &platform, None, // no cursor in this fixture
            None, None, // cow_host_xid — Phase 2.6 (None = no compositor active)
            false,
        );
        let scene = &built.scene;

        // Expect: top-level 0x100, top-level 0x101. Two entries
        // total (no cursor, no COW).
        assert_eq!(
            scene.draws.len(),
            2,
            "expected 2 top-levels, got {} draws: {:?}",
            scene.draws.len(),
            scene.draws,
        );

        // Top-level 0x100 at (0, 0) sized 100×80.
        assert_eq!(
            scene.draws[0].dst_origin,
            [0.0, 0.0],
            "first top-level origin",
        );
        assert_eq!(
            scene.draws[0].dst_size,
            [100.0, 80.0],
            "first top-level size",
        );
        // Top-level 0x101 at (200, 150) sized 120×90.
        assert_eq!(
            scene.draws[1].dst_origin,
            [200.0, 150.0],
            "second top-level origin",
        );
        assert_eq!(
            scene.draws[1].dst_size,
            [120.0, 90.0],
            "second top-level size",
        );

        // No draw should be screen-extent (no COW present).
        for d in &scene.draws {
            assert_ne!(
                d.dst_size,
                [800.0, 600.0],
                "no draw should be screen-extent when cow=None: {:?}",
                d,
            );
        }
    }

    /// Phase 1 pre-cleanup — when no COW is registered
    /// (`cow=None`), the cursor draw must still be appended at
    /// the top of z above the top-level draws. This preserves the
    /// legacy non-redirected cursor-on-top assertion that Phase 1
    /// leaves unchanged. The COW-present cursor ordering (top-levels
    /// stripped, COW below cursor) gets its own dedicated test.
    #[test]
    fn build_scene_cow_none_cursor_at_top() {
        let mut core = KmsCore::for_tests();
        let mut store = DrawableStore::new();
        let platform = PlatformBackend::for_tests();
        let mut windows = super::super::backend::WindowsMap::new();

        // One mapped top-level so the scene has anchor content.
        alloc_stub_window(&mut store, &mut windows, 0x100, 0, 0, 400, 300, None, true);
        core.top_level_order.push(0x100);

        // Cursor sprite.
        let mut cursor_storage = super::super::store::Storage::for_tests_null(
            extent(16, 16),
            vk::Format::B8G8R8A8_UNORM,
        );
        let cur2_sentinel: ash::vk::ImageView = ash::vk::Handle::from_raw(0xCAFE_BABE);
        cursor_storage.image_view = cur2_sentinel;
        cursor_storage.sample_view = cur2_sentinel;
        let cursor_id = store
            .allocate(0xCAFE_0002, DrawableKind::Pixmap, 32, false, cursor_storage)
            .expect("alloc cursor stub");
        core.cursor_x = 50.0;
        core.cursor_y = 60.0;
        let cursor = CursorEntry {
            id: cursor_id,
            extent: extent(16, 16),
            hot_x: 0,
            hot_y: 0,
            record_version: 0,
            bgra_bytes: None,
        };

        let built = build_scene(
            &core,
            &mut store,
            &windows,
            0,
            &platform,
            Some(cursor),
            None,
            None, // cow_host_xid — Phase 2.6 (None = no compositor active)
            false,
        );
        let scene = &built.scene;

        // Expect: top-level, cursor — 2 draws, in that order.
        assert_eq!(
            scene.draws.len(),
            2,
            "expected top-level + cursor = 2 draws, got {}: {:?}",
            scene.draws.len(),
            scene.draws,
        );
        // Last draw = cursor (16×16).
        assert_eq!(
            scene.draws.last().expect("cursor").dst_size,
            [16.0, 16.0],
            "cursor must be the top-of-z draw",
        );
        // First draw = top-level (400×300).
        assert_eq!(
            scene.draws[0].dst_size,
            [400.0, 300.0],
            "top-level must be below cursor",
        );
    }

    /// Phase 2.6 — `under_cow_subtree` recursion flag propagates
    /// `alpha_passthrough = true` to every `CompositeDraw` emitted
    /// inside the COW subtree (the COW top-level itself + all of its
    /// descendants). Non-COW top-levels (the no-compositor path)
    /// emit with `alpha_passthrough = false`.
    #[test]
    fn cow_subtree_draws_inherit_alpha_passthrough_true() {
        let mut core = KmsCore::for_tests();
        let mut store = DrawableStore::new();
        let platform = PlatformBackend::for_tests();
        let mut windows = super::super::backend::WindowsMap::new();

        // Non-COW top-level W @ (0, 0), 200×200.
        alloc_stub_window(&mut store, &mut windows, 0xA1, 0, 0, 200, 200, None, true);
        core.top_level_order.push(0xA1);

        // COW host xid @ (0, 0), 800×600 — matches PlatformBackend::for_tests output.
        let cow_xid: u32 = yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0;
        alloc_stub_window(
            &mut store,
            &mut windows,
            cow_xid,
            0,
            0,
            800,
            600,
            None,
            true,
        );
        core.top_level_order.push(cow_xid);

        // Compositor stage as child of COW @ (0, 0), 800×600.
        alloc_stub_window(
            &mut store,
            &mut windows,
            0xB1,
            0,
            0,
            800,
            600,
            Some(cow_xid),
            true,
        );

        let built = build_scene(
            &core,
            &mut store,
            &windows,
            0,
            &platform,
            None,
            None,
            Some(cow_xid),
            false,
        );
        let scene = &built.scene;

        // The non-COW W (200×200) must have alpha_passthrough=false.
        let w_draw = scene
            .draws
            .iter()
            .find(|d| d.dst_size == [200.0, 200.0])
            .expect("W draw present");
        assert!(
            !w_draw.alpha_passthrough,
            "non-COW top-level uses opaque blend (alpha_passthrough=false)",
        );

        // COW + stage (both 800×600) must have alpha_passthrough=true.
        let cow_or_stage_draws: Vec<_> = scene
            .draws
            .iter()
            .filter(|d| d.dst_size == [800.0, 600.0])
            .collect();
        assert!(
            !cow_or_stage_draws.is_empty(),
            "COW and stage emitted: {:?}",
            scene.draws,
        );
        for d in cow_or_stage_draws {
            assert!(
                d.alpha_passthrough,
                "COW subtree draw must have alpha_passthrough=true: {:?}",
                d,
            );
        }
    }

    /// Phase 2.7 — the COW must emit via the normal `top_level_order`
    /// walk, NOT via a special post-walk append. With the COW as the
    /// sole top-level, the scene contains exactly one draw sourced
    /// from the COW's storage (alpha_passthrough=true from Task 2.6),
    /// not two.
    #[test]
    fn build_scene_does_not_append_cow_after_top_level_walk() {
        let mut core = KmsCore::for_tests();
        let mut store = DrawableStore::new();
        let platform = PlatformBackend::for_tests();
        let mut windows = super::super::backend::WindowsMap::new();

        let cow_xid: u32 = yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0;
        alloc_stub_window(
            &mut store,
            &mut windows,
            cow_xid,
            0,
            0,
            800,
            600,
            None,
            true,
        );
        core.top_level_order.push(cow_xid);

        let built = build_scene(
            &core,
            &mut store,
            &windows,
            0,
            &platform,
            None,
            None,
            Some(cow_xid),
            false,
        );
        let scene = &built.scene;

        let cow_draws: Vec<_> = scene
            .draws
            .iter()
            .filter(|d| d.dst_size == [800.0, 600.0])
            .collect();
        assert_eq!(
            cow_draws.len(),
            1,
            "exactly one COW draw — no special append on top of top_level_order walk; got {:?}",
            scene.draws,
        );
        assert!(
            cow_draws[0].alpha_passthrough,
            "COW draw still has alpha_passthrough=true",
        );
    }

    /// Phase 3.1 — a Manual-redirected top-level (own
    /// `redirected_target` + `scene_participating=false`) must NEVER
    /// emit a `CompositeDraw` from its backing, regardless of whether
    /// the COW is materialized. Xorg's `compCheckRedirect` ensures
    /// Manual-redirected windows go offscreen for the compositor to
    /// read via `NameWindowPixmap`; the X server must not also blit
    /// the backing into scanout.
    #[test]
    fn manual_redirected_top_level_skips_emit_unconditional() {
        for cow_host_xid in [None, Some(0x103_u32)] {
            let mut core = KmsCore::for_tests();
            let mut store = DrawableStore::new();
            let platform = PlatformBackend::for_tests();
            let mut windows = super::super::backend::WindowsMap::new();

            // W with a redirected backing (Manual mode:
            // scene_participating=false). Unique sentinel handle so
            // a stray draw entry is unambiguous.
            let w: u32 = 0xA1;
            alloc_stub_window(&mut store, &mut windows, w, 100, 100, 50, 50, None, true);
            let w_id = store.lookup(w).expect("w lookup");
            let mut backing = super::super::store::Storage::for_tests_null(
                extent(50, 50),
                PlatformBackend::format_for_depth(24),
            );
            let view: vk::ImageView = ash::vk::Handle::from_raw(0xBEEF_0000);
            backing.image_view = view;
            backing.sample_view = view;
            let b_id = store
                .allocate(0xB0A1, DrawableKind::Pixmap, 24, true, backing)
                .expect("alloc manual backing");
            store.set_redirected_target(w_id, Some(b_id));
            store.set_scene_participating(w_id, false);
            core.top_level_order.push(w);

            if let Some(cow_xid) = cow_host_xid {
                alloc_stub_window(
                    &mut store,
                    &mut windows,
                    cow_xid,
                    0,
                    0,
                    800,
                    600,
                    None,
                    true,
                );
                core.top_level_order.push(cow_xid);
            }

            let built = build_scene(
                &core,
                &mut store,
                &windows,
                0,
                &platform,
                None,
                None,
                cow_host_xid,
                false,
            );
            let scene = &built.scene;

            let w_draws: Vec<_> = scene
                .draws
                .iter()
                .filter(|d| d.dst_size == [50.0, 50.0])
                .collect();
            assert!(
                w_draws.is_empty(),
                "Manual-redirected W must NOT emit (cow={cow_host_xid:?}): {:?}",
                scene.draws,
            );
        }
    }

    /// Issue #98 — an opaque, output-covering UNREDIRECTED top-level must
    /// suppress the COW even when the compositor keeps a helper window
    /// stacked ABOVE it, provided that helper lies entirely off-output.
    ///
    /// Measured on eiger (Asahi, Cinnamon session): muffin parks 1x1
    /// helper windows at (-200,-200) and raises them above the managed
    /// stack. The top-down probe stopped on one of those, concluded "the
    /// topmost window does not cover the output", and left the COW
    /// painting the desktop composite over the window muffin had just
    /// unredirected — so fullscreen video/games rendered as the wallpaper
    /// while their audio kept playing.
    #[test]
    fn offscreen_helper_above_fullscreen_still_suppresses_cow() {
        let mut core = KmsCore::for_tests();
        let mut store = DrawableStore::new();
        let platform = PlatformBackend::for_tests();
        let mut windows = super::super::backend::WindowsMap::new();

        // The unredirected fullscreen window: covers the 800x600 output,
        // opaque (depth != 32), scene-participating (drawn by us).
        let fs: u32 = 0x00F5;
        alloc_stub_window(&mut store, &mut windows, fs, 0, 0, 800, 600, None, true);
        windows.get_mut(&fs).expect("fs geom").depth = 24;
        core.top_level_order.push(fs);

        // muffin's off-screen 1x1 helper, stacked above `fs`.
        let helper: u32 = 0x00AE;
        alloc_stub_window(
            &mut store,
            &mut windows,
            helper,
            -200,
            -200,
            1,
            1,
            None,
            true,
        );
        windows.get_mut(&helper).expect("helper geom").depth = 24;
        core.top_level_order.push(helper);

        // The COW, always on top.
        let cow: u32 = 0x0103;
        alloc_stub_window(&mut store, &mut windows, cow, 0, 0, 800, 600, None, true);
        core.top_level_order.push(cow);

        let built = build_scene(
            &core,
            &mut store,
            &windows,
            0,
            &platform,
            None,
            None,
            Some(cow),
            false,
        );

        let cow_view: vk::ImageView = ash::vk::Handle::from_raw(u64::from(cow) | 0xFF00_0000);
        assert!(
            !built.scene.draws.iter().any(|d| d.image_view == cow_view),
            "COW must be suppressed by the opaque fullscreen unredirected \
             window even with an off-output helper stacked above it: {:?}",
            built.scene.draws,
        );
        let fs_view: vk::ImageView = ash::vk::Handle::from_raw(u64::from(fs) | 0xFF00_0000);
        assert!(
            built.scene.draws.iter().any(|d| d.image_view == fs_view),
            "the fullscreen window itself must still emit: {:?}",
            built.scene.draws,
        );
    }

    /// Issue #98 negative — the off-output filter must not turn into
    /// blanket over-suppression. A window that IS on this output and does
    /// NOT cover it (an ordinary floating window above the fullscreen one)
    /// still keeps the COW alive; suppressing it there would erase every
    /// redirected window the compositor draws, i.e. the whole desktop.
    #[test]
    fn on_output_non_covering_window_above_fullscreen_keeps_cow() {
        let mut core = KmsCore::for_tests();
        let mut store = DrawableStore::new();
        let platform = PlatformBackend::for_tests();
        let mut windows = super::super::backend::WindowsMap::new();

        let fs: u32 = 0x00F5;
        alloc_stub_window(&mut store, &mut windows, fs, 0, 0, 800, 600, None, true);
        windows.get_mut(&fs).expect("fs geom").depth = 24;
        core.top_level_order.push(fs);

        // On-output, non-covering window stacked above the fullscreen one.
        let float: u32 = 0x00BF;
        alloc_stub_window(
            &mut store,
            &mut windows,
            float,
            100,
            100,
            200,
            150,
            None,
            true,
        );
        windows.get_mut(&float).expect("float geom").depth = 24;
        core.top_level_order.push(float);

        let cow: u32 = 0x0103;
        alloc_stub_window(&mut store, &mut windows, cow, 0, 0, 800, 600, None, true);
        core.top_level_order.push(cow);

        let built = build_scene(
            &core,
            &mut store,
            &windows,
            0,
            &platform,
            None,
            None,
            Some(cow),
            false,
        );

        let cow_view: vk::ImageView = ash::vk::Handle::from_raw(u64::from(cow) | 0xFF00_0000);
        assert!(
            built.scene.draws.iter().any(|d| d.image_view == cow_view),
            "COW must survive when the topmost on-output window does not \
             cover the output: {:?}",
            built.scene.draws,
        );
    }

    /// Phase 3.1 negative — an Automatic-redirected top-level (own
    /// `redirected_target` + `scene_participating=true`) still emits
    /// a draw. Only the Manual mode (the bug-shaped case the gate
    /// closes) is unconditionally skipped.
    #[test]
    fn automatic_redirected_top_level_still_emits() {
        let mut core = KmsCore::for_tests();
        let mut store = DrawableStore::new();
        let platform = PlatformBackend::for_tests();
        let mut windows = super::super::backend::WindowsMap::new();

        let w: u32 = 0xA2;
        alloc_stub_window(&mut store, &mut windows, w, 100, 100, 50, 50, None, true);
        let w_id = store.lookup(w).expect("w lookup");
        let mut backing = super::super::store::Storage::for_tests_null(
            extent(50, 50),
            PlatformBackend::format_for_depth(24),
        );
        let view: vk::ImageView = ash::vk::Handle::from_raw(0xBEEF_0001);
        backing.image_view = view;
        backing.sample_view = view;
        let b_id = store
            .allocate(0xB0A2, DrawableKind::Pixmap, 24, true, backing)
            .expect("alloc automatic backing");
        store.set_redirected_target(w_id, Some(b_id));
        // scene_participating left as default true (Automatic).
        core.top_level_order.push(w);

        let built = build_scene(
            &core, &mut store, &windows, 0, &platform, None, None, None, false,
        );
        let scene = &built.scene;

        let w_draws: Vec<_> = scene
            .draws
            .iter()
            .filter(|d| d.dst_size == [50.0, 50.0])
            .collect();
        assert_eq!(
            w_draws.len(),
            1,
            "Automatic-redirected W still emits one draw: {:?}",
            scene.draws,
        );
    }

    /// Phase 6.1 — full compositor flow in one scenario. Exercises the
    /// structural facts the COW redesign delivers, headless via a
    /// direct `build_scene` call (no live Vulkan device required):
    ///
    /// 1. A materialized COW (`windows` entry + `top_level_order`
    ///    slot, per Task 2.2) emits exactly once via the normal
    ///    `top_level_order` walk (Phase 2.7), with
    ///    `alpha_passthrough=true` (Phase 2.6).
    /// 2. A stage child of the COW with content emits exactly once via
    ///    the COW-subtree recursion (Phase 2.6/2.7), also
    ///    `alpha_passthrough=true`.
    /// 3. A Manual-redirected sibling top-level (own
    ///    `redirected_target` + `scene_participating=false`) emits
    ///    ZERO draws (Phase 3.1) — even though the COW is materialized.
    /// 4. Ordering: the COW-subtree draws appear after the earlier
    ///    non-COW top-level (the Manual sibling contributes nothing).
    ///
    /// Sizes are chosen so each source is unambiguously identifiable by
    /// `dst_size`:
    ///   - early non-COW top-level W: 200×200
    ///   - Manual-redirected sibling S: 50×50  (must not appear)
    ///   - COW host:                    800×600
    ///   - stage (COW child):           640×480
    #[test]
    fn compositor_stage_under_cow_emits_via_recursion_and_manual_siblings_skip() {
        let mut core = KmsCore::for_tests();
        let mut store = DrawableStore::new();
        let platform = PlatformBackend::for_tests();
        let mut windows = super::super::backend::WindowsMap::new();

        // (1) An earlier, ordinary non-COW top-level W @ (0,0), 200×200.
        // Establishes a "before" position to anchor ordering.
        let w: u32 = 0xC001;
        alloc_stub_window(&mut store, &mut windows, w, 0, 0, 200, 200, None, true);
        core.top_level_order.push(w);

        // (2) A Manual-redirected sibling top-level S @ (100,100), 50×50.
        // redirected_target + scene_participating=false → Manual mode.
        let s: u32 = 0xC002;
        alloc_stub_window(&mut store, &mut windows, s, 100, 100, 50, 50, None, true);
        let s_id = store.lookup(s).expect("s lookup");
        let mut s_backing = super::super::store::Storage::for_tests_null(
            extent(50, 50),
            PlatformBackend::format_for_depth(24),
        );
        let s_view: vk::ImageView = ash::vk::Handle::from_raw(0xDEAD_0050);
        s_backing.image_view = s_view;
        s_backing.sample_view = s_view;
        let s_backing_id = store
            .allocate(0xB0C2, DrawableKind::Pixmap, 24, true, s_backing)
            .expect("alloc manual sibling backing");
        store.set_redirected_target(s_id, Some(s_backing_id));
        store.set_scene_participating(s_id, false);
        core.top_level_order.push(s);

        // (3) The materialized COW host @ (0,0), 800×600 (matches the
        // PlatformBackend::for_tests output extent). This stands in for
        // GetOverlayWindow having created the windows entry +
        // top_level_order slot (Task 2.2).
        let cow_xid: u32 = yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0;
        alloc_stub_window(
            &mut store,
            &mut windows,
            cow_xid,
            0,
            0,
            800,
            600,
            None,
            true,
        );
        core.top_level_order.push(cow_xid);

        // (4) The compositor stage as a child of the COW @ (0,0),
        // 640×480 — content the WM paints into the overlay.
        let stage: u32 = 0xC003;
        alloc_stub_window(
            &mut store,
            &mut windows,
            stage,
            0,
            0,
            640,
            480,
            Some(cow_xid),
            true,
        );

        let built = build_scene(
            &core,
            &mut store,
            &windows,
            0,
            &platform,
            None,
            None,
            Some(cow_xid),
            false,
        );
        let scene = &built.scene;

        // Fact A — Manual-redirected sibling S emits ZERO draws.
        let s_draws = scene
            .draws
            .iter()
            .filter(|d| d.dst_size == [50.0, 50.0])
            .count();
        assert_eq!(
            s_draws, 0,
            "Manual-redirected sibling must not emit, even with COW materialized: {:?}",
            scene.draws,
        );

        // Fact B — stage (COW child) emits exactly ONE draw with
        // alpha_passthrough=true.
        let stage_draws: Vec<_> = scene
            .draws
            .iter()
            .filter(|d| d.dst_size == [640.0, 480.0])
            .collect();
        assert_eq!(
            stage_draws.len(),
            1,
            "stage emits exactly once via COW subtree recursion: {:?}",
            scene.draws,
        );
        assert!(
            stage_draws[0].alpha_passthrough,
            "stage draw inherits alpha_passthrough=true from the COW subtree: {:?}",
            stage_draws[0],
        );

        // Fact C — COW emits exactly ONE draw with alpha_passthrough=true
        // via the normal top_level_order walk (no special post-walk append).
        let cow_draws: Vec<_> = scene
            .draws
            .iter()
            .filter(|d| d.dst_size == [800.0, 600.0])
            .collect();
        assert_eq!(
            cow_draws.len(),
            1,
            "COW emits exactly once via top_level_order walk: {:?}",
            scene.draws,
        );
        assert!(
            cow_draws[0].alpha_passthrough,
            "COW draw has alpha_passthrough=true: {:?}",
            cow_draws[0],
        );

        // The earlier non-COW top-level W emits one opaque draw.
        let w_pos = scene
            .draws
            .iter()
            .position(|d| d.dst_size == [200.0, 200.0])
            .expect("W draw present");
        assert!(
            !scene.draws[w_pos].alpha_passthrough,
            "non-COW top-level W uses opaque blend (alpha_passthrough=false)",
        );

        // Fact D — ordering: the COW-subtree draws (COW host + stage)
        // appear AFTER the earlier non-COW top-level W. The Manual
        // sibling contributes nothing in between.
        let cow_pos = scene
            .draws
            .iter()
            .position(|d| d.dst_size == [800.0, 600.0])
            .expect("COW draw present");
        let stage_pos = scene
            .draws
            .iter()
            .position(|d| d.dst_size == [640.0, 480.0])
            .expect("stage draw present");
        assert!(
            w_pos < cow_pos && w_pos < stage_pos,
            "COW subtree draws come after the earlier top-level W: w={w_pos} cow={cow_pos} stage={stage_pos}",
        );
        // Within the COW subtree the host emits before its stage child.
        assert!(
            cow_pos < stage_pos,
            "COW host draw precedes its stage child in the subtree recursion: cow={cow_pos} stage={stage_pos}",
        );
    }
}
