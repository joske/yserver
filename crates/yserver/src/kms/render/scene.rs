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
//! - **Manual-redirected windows are skipped, their subtrees are
//!   not.** Manual redirect flips the window to
//!   `scene_participating = false` and the walk skips that node; the
//!   compositor reintroduces its pixels by painting its output/COW
//!   surface. The walk still recurses into the descendants, and a
//!   descendant owning its own `redirected_target` (an Automatic
//!   redirect under a Manual ancestor — GTK/marco CSD frames) emits
//!   its own backing. Audit #3 (2026-05-19) removed the old
//!   whole-subtree prune because it dropped those inner widgets.
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
    panic::Location,
    sync::{Arc, OnceLock},
};

use ash::vk;
use yserver_protocol::x11::xfixes;

use super::{
    platform::{FenceTicket, PlatformBackend, ReadyScanoutRenderCompletion},
    region::Region,
    scanout_damage::ScanoutDamage,
    scene_diff::{
        ParticipantId, PresenceSignature, ScenePresence, SceneRole, presence_from_place,
        structural_damage,
    },
    store::{DamageSnapshot, DrawableKind, DrawableStore, RegionSet},
    telemetry::Telemetry,
};
use crate::kms::{
    core::KmsCore,
    render::composite_pool_ring::CompositePoolRing,
    vk::{
        compositor::{CompositeDraw, CompositeScene, PresentError},
        damage_audit_compare::{DamageAuditComparePipeline, DamageAuditTileSummary},
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
    /// Step 2 — the participants this frame emitted. Becomes
    /// `prev_presented` if and only if the frame retires successfully.
    submitted_participants: Vec<ScenePresence>,
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
    damage_audit: Option<OutputDamageAudit>,
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
    /// Layout origin of this output in root/screen coordinates, cached for the
    /// same reason as the extent: the damage-marking entry points are on
    /// `SceneCompositor` and have no `PlatformBackend` in scope, so without this
    /// they cannot translate a screen-absolute rect into output-local space.
    output_origin: (i32, i32),
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
    /// Step 3 — per-scanout-BO damage: what each BO is missing relative to the
    /// current scene. Fed and staged below while `pick_repaint_region` still
    /// returns `Repaint::Full`, so nothing on screen depends on it yet; step 4
    /// makes it drive the repaint region. See `scanout_damage.rs` for the
    /// invariant and the transaction rules.
    damage: ScanoutDamage,
    /// Step 2 — the participants of the last **successfully presented** frame
    /// on this output. Diffed against the frame being built to derive structural
    /// damage. Advanced only at retirement, like everything else in this design:
    /// a failed submit must leave it alone or the structural damage is lost.
    ///
    /// Needs no lifecycle invalidation, unlike `damage`. It only ever advances
    /// to a frame that actually reached the screen, so it can be *behind* the
    /// live scene but never ahead of it — and behind means the next diff
    /// over-damages, which is safe. A fresh state starts empty, which damages
    /// every participant present, i.e. the whole output.
    prev_presented: Vec<ScenePresence>,
    /// Sampled sources that emitted at least one piece on this output in its
    /// most recent walk (`SceneBuild::pieces_ids`). Read by the pre-walk
    /// predicate ([`walk_needed`] / [`pending_presentation_for_output`]) to
    /// decide whether an armed drawable's damage can possibly land here.
    ///
    /// Exact between structural changes: visibility on an output changes only
    /// through a structural change, and every one sets `scene_structure_dirty`,
    /// which forces the walk before this set is consulted again. A fresh state
    /// (startup, `rebuild_outputs`) starts empty, and a drawable in NO output's
    /// set is treated as unknown ⇒ every output walks — conservative.
    last_pieces: std::collections::HashSet<super::store::DrawableId>,
}

struct OutputDamageAudit {
    candidate: DamageAuditTarget,
    reference: DamageAuditTarget,
    compare: DamageAuditComparePipeline,
    initialized: bool,
    frame: u64,
    consumed_event_id: u64,
    active_episodes: HashMap<u32, DamageAuditEpisodeStart>,
    episodes_opened: u64,
    episodes_healed: u64,
    reset_count: u64,
    /// Total comparisons actually executed. A soak that reports no
    /// mismatches is only evidence if this is non-zero and growing —
    /// see `emit_damage_audit_heartbeat`.
    comparisons: u64,
    /// Scene draw count at seed time and at the current comparison.
    /// A mismatch where `seed_draws == 0` and `draws > 0` is a draw
    /// APPEARING with no damage covering it — a scene-structure change,
    /// not a paint. Distinguishes that from a stale-pixel damage hole.
    seed_draws: usize,
    frame_draws: usize,
    /// Source drawables sampled by the seed compose, and by the current
    /// comparison. When the draw list is unchanged but the images differ,
    /// the culprit is one of these drawables' *contents* changing without
    /// reporting damage — this says which.
    seed_sampled: Vec<(u64, u32)>,
    frame_sampled: Vec<(u64, u32)>,
    /// Comparison classification. A clean run only means something in
    /// proportion to `idle` + `partial`: on a `full` frame the candidate
    /// was wholly recomposed, so the comparison is a tautology, not a
    /// test. Window management damages the whole output by construction
    /// (`mark_scene_structure_dirty`), so drag/resize/menu runs are
    /// almost entirely `full` and prove nothing about damage completeness.
    /// Summed GPU compose time for the clipped candidate and the full
    /// reference, over comparisons where both were composed this frame.
    /// Their ratio is the measured ceiling on what clipped repaint can
    /// save: the clipped pass still records every draw call and descriptor
    /// bind, so only fragment work shrinks.
    clipped_gpu_ns: u128,
    full_gpu_ns: u128,
    gpu_samples: u64,
    comparisons_idle: u64,
    comparisons_partial: u64,
    comparisons_full: u64,
    /// Summed repaint-bbox area over non-idle comparisons, against
    /// `output area x those comparisons`, for a mean damage fraction.
    damage_pixels: u128,
    damage_frames: u64,
    /// Wall-clock of the last executed comparison, for the idle
    /// re-compare. `None` until the first one runs.
    last_compare_at: Option<std::time::Instant>,
    last_heartbeat_at: Option<std::time::Instant>,
}

#[derive(Clone, Copy, Debug)]
struct DamageAuditEpisodeStart {
    frame: u64,
    first_event_id: u64,
    next_event_id: u64,
}

struct DamageAuditTarget {
    vk: Arc<crate::kms::vk::device::VkContext>,
    image: vk::Image,
    view: vk::ImageView,
    memory: vk::DeviceMemory,
    extent: vk::Extent2D,
    command_pool: vk::CommandPool,
    command_buffer: vk::CommandBuffer,
    /// 2-slot TIMESTAMP pool bracketing this target's compose CB.
    /// `record_and_submit_render` fills it whenever it is non-null, which
    /// turns the audit's existing candidate-vs-reference pair into a direct
    /// A/B of clipped versus full compose cost on an identical scene.
    timestamp_pool: vk::QueryPool,
    last_gpu_render_ns: Option<u64>,
}

impl DamageAuditTarget {
    fn new(
        vk: Arc<crate::kms::vk::device::VkContext>,
        extent: vk::Extent2D,
    ) -> Result<Self, vk::Result> {
        let info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk::Format::B8G8R8A8_UNORM)
            .extent(vk::Extent3D {
                width: extent.width,
                height: extent.height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(
                vk::ImageUsageFlags::COLOR_ATTACHMENT
                    | vk::ImageUsageFlags::TRANSFER_SRC
                    | vk::ImageUsageFlags::STORAGE,
            )
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let image = unsafe { vk.device.create_image(&info, None)? };
        let requirements = unsafe { vk.device.get_image_memory_requirements(image) };
        let properties = unsafe {
            vk.instance
                .get_physical_device_memory_properties(vk.physical_device)
        };
        let memory_type_index = (0..properties.memory_type_count).find(|&index| {
            requirements.memory_type_bits & (1 << index) != 0
                && properties.memory_types[index as usize]
                    .property_flags
                    .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
        });
        let Some(memory_type_index) = memory_type_index else {
            unsafe { vk.device.destroy_image(image, None) };
            return Err(vk::Result::ERROR_FEATURE_NOT_PRESENT);
        };
        let allocation = vk::MemoryAllocateInfo::default()
            .allocation_size(requirements.size)
            .memory_type_index(memory_type_index);
        let memory = match unsafe { vk.device.allocate_memory(&allocation, None) } {
            Ok(memory) => memory,
            Err(error) => {
                unsafe { vk.device.destroy_image(image, None) };
                return Err(error);
            }
        };
        if let Err(error) = unsafe { vk.device.bind_image_memory(image, memory, 0) } {
            unsafe {
                vk.device.destroy_image(image, None);
                vk.device.free_memory(memory, None);
            }
            return Err(error);
        }
        let view_info = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(vk::Format::B8G8R8A8_UNORM)
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .level_count(1)
                    .layer_count(1),
            );
        let view = match unsafe { vk.device.create_image_view(&view_info, None) } {
            Ok(view) => view,
            Err(error) => {
                unsafe {
                    vk.device.destroy_image(image, None);
                    vk.device.free_memory(memory, None);
                }
                return Err(error);
            }
        };
        let pool_info = vk::CommandPoolCreateInfo::default()
            .queue_family_index(vk.graphics_queue_family)
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
        let command_pool = match unsafe { vk.device.create_command_pool(&pool_info, None) } {
            Ok(pool) => pool,
            Err(error) => {
                unsafe {
                    vk.device.destroy_image_view(view, None);
                    vk.device.destroy_image(image, None);
                    vk.device.free_memory(memory, None);
                }
                return Err(error);
            }
        };
        let cb_info = vk::CommandBufferAllocateInfo::default()
            .command_pool(command_pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        let command_buffer = match unsafe { vk.device.allocate_command_buffers(&cb_info) } {
            Ok(buffers) => buffers[0],
            Err(error) => {
                unsafe {
                    vk.device.destroy_command_pool(command_pool, None);
                    vk.device.destroy_image_view(view, None);
                    vk.device.destroy_image(image, None);
                    vk.device.free_memory(memory, None);
                }
                return Err(error);
            }
        };
        let timestamp_pool = if vk.timestamp_period > 0.0 {
            let info = vk::QueryPoolCreateInfo::default()
                .query_type(vk::QueryType::TIMESTAMP)
                .query_count(2);
            unsafe { vk.device.create_query_pool(&info, None) }.unwrap_or(vk::QueryPool::null())
        } else {
            vk::QueryPool::null()
        };
        Ok(Self {
            vk,
            image,
            view,
            memory,
            extent,
            command_pool,
            command_buffer,
            timestamp_pool,
            last_gpu_render_ns: None,
        })
    }
}

impl Drop for DamageAuditTarget {
    fn drop(&mut self) {
        if self.vk.requires_drop_device_idle() {
            let wait = unsafe { self.vk.device.device_wait_idle() };
            if !matches!(wait, Ok(()) | Err(vk::Result::ERROR_DEVICE_LOST)) {
                log::warn!(
                    "damage audit target: vkDeviceWaitIdle failed during teardown: {wait:?}; \
                     leaking uncertain target resources"
                );
                std::mem::forget(Arc::clone(&self.vk));
                return;
            }
        }
        unsafe {
            if self.timestamp_pool != vk::QueryPool::null() {
                self.vk.device.destroy_query_pool(self.timestamp_pool, None);
            }
            self.vk.device.destroy_command_pool(self.command_pool, None);
            self.vk.device.destroy_image_view(self.view, None);
            self.vk.device.destroy_image(self.image, None);
            self.vk.device.free_memory(self.memory, None);
        }
    }
}

struct DamageAuditLedgerEntry {
    id: u64,
    site: &'static Location<'static>,
    expected_area: Vec<vk::Rect2D>,
    contributed_outputs: Vec<usize>,
}

fn build_output_damage_audit(
    vk: &Arc<crate::kms::vk::device::VkContext>,
    extent: vk::Extent2D,
) -> Result<Option<OutputDamageAudit>, SceneError> {
    if !damage_audit_enabled() {
        return Ok(None);
    }
    if !DamageAuditComparePipeline::is_supported(vk, extent.width, extent.height) {
        log::warn!(
            "damage-audit: unavailable for {}x{} on this Vulkan context",
            extent.width,
            extent.height
        );
        return Ok(None);
    }
    let candidate = DamageAuditTarget::new(Arc::clone(vk), extent).map_err(SceneError::Vk)?;
    let reference = DamageAuditTarget::new(Arc::clone(vk), extent).map_err(SceneError::Vk)?;
    let compare = DamageAuditComparePipeline::new(Arc::clone(vk), extent.width, extent.height)
        .map_err(SceneError::Vk)?;
    log::info!(
        "damage-audit: enabled output extent={}x{} grid={}x{} interval={}",
        extent.width,
        extent.height,
        compare.grid_width(),
        compare.grid_height(),
        damage_audit_interval()
    );
    Ok(Some(OutputDamageAudit {
        candidate,
        reference,
        compare,
        initialized: false,
        frame: 0,
        consumed_event_id: 0,
        active_episodes: HashMap::new(),
        episodes_opened: 0,
        episodes_healed: 0,
        reset_count: 0,
        comparisons: 0,
        seed_draws: 0,
        frame_draws: 0,
        seed_sampled: Vec::new(),
        frame_sampled: Vec::new(),
        clipped_gpu_ns: 0,
        full_gpu_ns: 0,
        gpu_samples: 0,
        comparisons_idle: 0,
        comparisons_partial: 0,
        comparisons_full: 0,
        damage_pixels: 0,
        damage_frames: 0,
        last_compare_at: None,
        last_heartbeat_at: None,
    }))
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
    /// Nothing that could produce damage has changed since the last walk:
    /// no structural change, no armed presentation damage, no owed repaint,
    /// no audit. The tick returns BEFORE `build_scene` — see [`walk_needed`].
    NothingPending,
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

    /// True iff `build_scene` ran for this output (so its presented and
    /// pieces ids were recorded and its `last_pieces` refreshed). The
    /// `PendingAcks`/`RetryDeadline`/`NothingPending` skips return BEFORE
    /// the walk; everything else runs it. `tick` reconciles dormancy on any
    /// tick where some output walked; the outputs that did not walk speak
    /// through their retained `last_pieces` — see `dormancy_inputs`.
    fn walked(self) -> bool {
        !matches!(
            self,
            Self::Skipped(TickSkipReason::PendingAcks)
                | Self::Skipped(TickSkipReason::RetryDeadline)
                | Self::Skipped(TickSkipReason::NothingPending)
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
    /// Test-only override for [`has_pending_page_flips`](Self::has_pending_page_flips).
    /// `KmsBackend::for_tests()` builds a `stub()` scene with `inner: None`,
    /// so there is no live `PendingAck` queue to populate; this lets
    /// capability-surface tests (`present_flip_in_flight`) exercise both
    /// states without a live Vulkan device.
    #[cfg(test)]
    test_flip_in_flight_override: Option<bool>,
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
    damage_audit_ledger: VecDeque<DamageAuditLedgerEntry>,
    damage_audit_next_event_id: u64,
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

/// Whether the scene walk clips each node to what nothing above it covers.
///
/// Step 1 of the damage-repaint plan: Xorg never paints a pixel twice for
/// non-composited windows — `miComputeClips` gives every window a clip list and
/// the clip lists partition the screen. `On` reproduces that: a fully covered
/// window emits nothing, a partly covered one emits only its visible pieces,
/// and the root becomes the output minus every opaque top-level. `Off` is the
/// pre-step-1 emitter, byte for byte; the damage audit renders its reference
/// from it (so a visibility bug that hides pixels shows up as a mismatch rather
/// than passing clean on both sides), and the tests use it as the oracle.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Visibility {
    Off,
    On,
}

/// What the walk did, for telemetry. `nodes_visited` counts every mapped node
/// with geometry that reached the decision (plus the root); `draws_emitted` is
/// the post-visibility, pre-scissor draw count; `collapses` counts every time a
/// region the walk holds hit the 32-box cap and became its bounding box (a
/// superset — safe, but a scene that collapses every frame is one where the
/// pass buys nothing); `hidden_participants` counts nodes that passed every
/// gate and emitted zero draws because something above covers them entirely.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct WalkStats {
    nodes_visited: u64,
    draws_emitted: u64,
    /// Collapses split by site, so telemetry shows where the cap bites: the
    /// `mine` union of a non-leaf node, an opaque node's claim subtraction, a
    /// non-opaque node's `taken` subtraction from the universe, and a `taken`
    /// remainder that itself collapsed and was therefore not claimed at all.
    collapses_mine: u64,
    collapses_claim: u64,
    collapses_taken: u64,
    collapses_taken_skipped: u64,
    hidden_participants: u64,
    /// Stage C — snapshots with non-empty captured damage, classified by what
    /// their projection onto this output did (see [`ContentDamage`]).
    content_visible: u64,
    content_hidden: u64,
    content_off_output: u64,
    content_other_output: u64,
}

/// Where a node's captured content damage landed on this output.
///
/// Decided in the walk, where the visible pieces are known, and threaded to
/// the tick through [`WalkStats`] so the tick never recomputes geometry.
///
/// **Only `OffOutput` may force a compose.** The empty-damage path used to
/// force a Full compose whenever any snapshot carried damage while the output
/// damage was empty — that was only ever the `OffOutput` case (a popup whose
/// projection missed the output entirely: the xfce submenu, which must still
/// ack so its paint is not stranded). Once content damage is clipped to
/// visibility, a paint into the covered part of a window produces the very same
/// "captured but projected empty" state, and forcing a Full compose per hidden
/// paint would undo step 1.
///
/// **Hidden damage is deliberately NOT acked.** The plan proposed acking it
/// after every output had walked, under a multi-output rule; none of that is
/// needed. An un-acked snapshot is simply re-peeked on the next walk. While the
/// window stays covered its damage accumulates in the store's `RegionSet`
/// (capped, a superset — safe) and costs nothing on the GPU. When the cover
/// moves away, the mover's structural damage (old ∪ new) repaints the uncovered
/// area, the accumulated damage projects visibly on that or the next tick, is
/// composed, and is acked at retire like any other. On a two-output layout a
/// window hidden on A and visible on B is composed and acked by B —
/// `ack_presentation_damage` clears the drawable globally, so *not* acking from
/// the hidden side is exactly what keeps B correct. Hidden snapshots therefore
/// do NOT ride `built.snapshots` either (2026-09-04): a compose this output
/// makes for another reason must not carry — and at retire ack — damage it did
/// not present. The same holds for `OtherOutput`.
///
/// **Hidden damage must also stop arming the scheduler.** A drawable whose
/// damage classified `Hidden` on every walked output is left out of the
/// `presented_ids` the tick feeds to `reconcile_offscreen_no_draw`, so it goes
/// dormant and `has_pending_presentation_damage` ignores it. Without that the
/// tick woke on its own pending damage, walked, found nothing to compose and
/// woke again: ~1850 walks/s at 2 composes/s with mpv under a terminal
/// (silence/MATE, 2026-09-04). The dormancy has two reasons with different
/// re-arm rules (`store::DormantReason`): a node that emitted NO pieces stays
/// dormant until a structural change wakes the tick, while a partially covered
/// node whose damage happened to be hidden is re-armed by its next paint,
/// which may land in its visible part — one walk per paint, not one per
/// wake. See [`WalkSink::presented_ids`] and [`WalkSink::pieces_ids`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ContentDamage {
    /// Some of the projection intersects the node's visible pieces.
    Visible,
    /// The projection lands on the output but entirely under a cover.
    Hidden,
    /// The projection misses the output entirely AND the drawable has no
    /// visible pieces on any other output either: a stranded paint (the xfce
    /// submenu) that this output must force-compose and ack so it drains.
    OffOutput,
    /// The projection misses this output, but the drawable is visible on
    /// another output, which presents and acks it. Neither forced nor carried
    /// here — carrying it would let THIS output's retire ack damage the owning
    /// output has not composed yet (the multi-output ack race, silence/MATE
    /// 2026-09-04: a hover highlight on a two-monitor caja desktop lost on the
    /// monitor it was painted on).
    OtherOutput,
}

impl WalkStats {
    /// True if some snapshot's captured damage projected entirely off this
    /// output — the only classification that may force a compose on the
    /// empty-damage path. See [`ContentDamage`].
    fn off_output_damage_forces_compose(&self) -> bool {
        self.content_off_output > 0
    }

    fn collapses(&self) -> u64 {
        self.collapses_mine
            + self.collapses_claim
            + self.collapses_taken
            + self.collapses_taken_skipped
    }
}

/// Everything the walk produces, threaded through the recursion.
///
/// Pushed in **computation** order (children top → bottom, then self) and
/// reversed once at the end of the walk into painter's order (self, then
/// children bottom → top). One presence, one sampled id and at most one
/// snapshot per node, pushed at the node's own step, so the four lists reverse
/// consistently.
struct WalkSink<'a> {
    /// Which output this walk is for — only for the gated diagnostics.
    output_idx: usize,
    /// Sampled sources that emitted pieces on some OTHER output at its last
    /// walk (the union of the other outputs' retained `last_pieces`). Decides
    /// `ContentDamage::OtherOutput` vs `OffOutput`. Empty when unknown (single
    /// output, first frame, tests), which degrades to the old force-and-ack.
    elsewhere: &'a std::collections::HashSet<super::store::DrawableId>,
    draws: Vec<CompositeDraw>,
    snapshots: Vec<DamageSnapshot>,
    sampled_ids: Vec<super::store::DrawableId>,
    projected: RegionSet,
    participants: Vec<ScenePresence>,
    stats: WalkStats,
    /// Stage C — the visible pieces of the node being emitted, for clipping
    /// its content damage. A scratch buffer on the sink, cleared per node, so
    /// the hot path allocates once per walk rather than once per node.
    pieces: Vec<vk::Rect2D>,
    /// The sampled sources whose pending content damage this output PRESENTED
    /// (projected `Visible`, or off-output — which forces a compose that acks
    /// it — or carrying no damage at all). This, not `sampled_ids`, is what
    /// `reconcile_offscreen_no_draw` must be fed: a node whose damage
    /// classified `Hidden` was sampled but nothing of its paint reached the
    /// screen, and counting it as drawn kept `has_pending_presentation_damage`
    /// true forever — the tick woke, walked, found nothing to compose, and woke
    /// again, ~1850 walks/s at 2 composes/s with mpv under a terminal on
    /// silence/MATE (codex, post-merge review of `02bafec3`, finding 1). Left
    /// out of this set on every output, the drawable goes dormant
    /// (`store::DormantReason`) and stops arming the scheduler; its damage is
    /// preserved, and either its next paint (`HiddenDamage`) or the mover's
    /// structural change (`NoPieces`) brings it back.
    presented_ids: Vec<super::store::DrawableId>,
    /// The sampled sources that emitted at least one piece on this output.
    /// With `presented_ids` this decides the dormancy REASON: not presented
    /// and no pieces anywhere ⇒ `DormantReason::NoPieces`; not presented but
    /// pieces ⇒ `HiddenDamage`, which the next paint re-arms.
    pieces_ids: Vec<super::store::DrawableId>,
}

impl<'a> WalkSink<'a> {
    fn new(
        output_idx: usize,
        elsewhere: &'a std::collections::HashSet<super::store::DrawableId>,
    ) -> Self {
        Self {
            output_idx,
            elsewhere,
            draws: Vec::new(),
            snapshots: Vec::new(),
            sampled_ids: Vec::new(),
            projected: RegionSet::new(),
            participants: Vec::new(),
            stats: WalkStats::default(),
            pieces: Vec::new(),
            presented_ids: Vec::new(),
            pieces_ids: Vec::new(),
        }
    }

    /// Computation order → painter's order. See the type doc.
    fn reverse(&mut self) {
        self.draws.reverse();
        self.snapshots.reverse();
        self.sampled_ids.reverse();
        self.participants.reverse();
        self.presented_ids.reverse();
        self.pieces_ids.reverse();
    }
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
    /// Step 2 — one entry per scene participant that passed every gate, with
    /// its region derived from its **placement** (not from what it emitted —
    /// step 1 clips emission to visibility, and the diff must not read that;
    /// see `scene_diff`). Diffed against the last presented frame to yield
    /// structural damage. The cursor is deliberately absent.
    participants: Vec<ScenePresence>,
    /// Step 1 — what the walk did, for telemetry.
    stats: WalkStats,
    /// See [`WalkSink::presented_ids`]. Feeds the tick's `drawn` set.
    presented_ids: Vec<super::store::DrawableId>,
    /// See [`WalkSink::pieces_ids`]. Feeds the tick's `had_pieces` set.
    pieces_ids: Vec<super::store::DrawableId>,
}

impl SceneBuild {
    fn omit_software_cursor_for_hide(&mut self) {
        if let Some((draw_index, sampled_index)) = self.software_cursor_tail.take() {
            debug_assert_eq!(self.scene.draws.len(), draw_index + 1);
            debug_assert_eq!(self.sampled_ids.len(), sampled_index + 1);
            let cursor_id = self.sampled_ids[sampled_index];
            self.presented_ids.retain(|id| *id != cursor_id);
            self.pieces_ids.retain(|id| *id != cursor_id);
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
                damage_audit_ledger: VecDeque::new(),
                damage_audit_next_event_id: 0,
                cursor: None,
            }),
            root_overlay: super::root_overlay::RootOverlay::default(),
            scene_structure_dirty: true,
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
            damage_audit: build_output_damage_audit(
                vk,
                vk::Extent2D {
                    width: u32::from(layout.width),
                    height: u32::from(layout.height),
                },
            )?,
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
            output_origin: (layout.x, layout.y),
            next_submit_retry_at: None,
            last_frame_cursor_mode: OutputCursorMode::Hidden,
            cursor_prev_pos: None,
            last_present_cursor_rect: None,
            last_present_cursor_version: None,
            force_show_retry_version: None,
            last_skip_reason: None,
            // Sized from the *current* pool, exactly as `bo_depth` above is.
            // `rebuild_outputs` replaces every `OutputSceneState`, so this is
            // also how a pool that changed length or identity gets a correctly
            // shaped `missing` vector — see the plan's 3.4.
            prev_presented: Vec::new(),
            last_pieces: std::collections::HashSet::new(),
            damage: ScanoutDamage::new(
                bo_depth,
                vk::Extent2D {
                    width: u32::from(layout.width),
                    height: u32::from(layout.height),
                },
            ),
        })
    }

    /// Step 3 — mark every output's scanout BOs wholly stale.
    ///
    /// The safe fallback for lifecycle transitions the per-BO damage model
    /// cannot reason about: it costs one full repaint per output and can never
    /// show a stale pixel. Used by the two backend-side sites that change what
    /// is on screen without going through a compose — `set_logical_screen_size`
    /// (which reallocates root/COW storage but deliberately avoids
    /// `drain_all` + `rebuild_outputs`) and the return from direct scanout
    /// (during which the composed BOs are not painted at all).
    pub(crate) fn invalidate_all_scanout_damage(&mut self) {
        if let Some(inner) = self.inner.as_mut() {
            for o in &mut inner.outputs {
                o.damage.invalidate();
            }
        }
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
            #[cfg(test)]
            test_flip_in_flight_override: None,
        }
    }

    /// Whether the scene has a live blit pipeline. Tests use
    /// this to skip Vk-only assertions.
    pub(crate) fn is_live(&self) -> bool {
        self.inner.is_some()
    }

    fn full_output_audit_area(&self) -> Vec<vk::Rect2D> {
        self.inner
            .as_ref()
            .map(|inner| {
                inner
                    .outputs
                    .iter()
                    .map(|output| vk::Rect2D {
                        offset: vk::Offset2D::default(),
                        extent: output.output_extent,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn record_damage_audit_event(
        &mut self,
        site: &'static Location<'static>,
        expected_area: Vec<vk::Rect2D>,
    ) -> Option<u64> {
        if !self.damage_audit_active() {
            return None;
        }
        let inner = self.inner.as_mut()?;
        let id = inner.damage_audit_next_event_id;
        inner.damage_audit_next_event_id = inner.damage_audit_next_event_id.saturating_add(1);
        inner.damage_audit_ledger.push_back(DamageAuditLedgerEntry {
            id,
            site,
            expected_area,
            contributed_outputs: Vec::new(),
        });
        bound_damage_audit_ledger(inner);
        Some(id)
    }

    fn damage_audit_active(&self) -> bool {
        damage_audit_enabled()
            && self.inner.as_ref().is_some_and(|inner| {
                inner
                    .outputs
                    .iter()
                    .any(|output| output.damage_audit.is_some())
            })
    }

    /// Mark the scene as needing a redraw. Cheap bool flip;
    /// callable from any mutation path that wants the next tick
    /// to inspect drawable/cursor damage. This deliberately does
    /// NOT add output damage: protocol paint is already represented
    /// by per-drawable presentation damage, and cursor motion is
    /// projected by `build_scene`.
    #[track_caller]
    pub(crate) fn wake_for_damage(&mut self) {
        if self.damage_audit_active() {
            self.record_damage_audit_event(Location::caller(), self.full_output_audit_area());
        }
        self.scene_structure_dirty = true;
    }

    /// Mark scene structure as changed. This is the coarse fallback
    /// for map/unmap/configure/restack/redirect/root-background
    /// transitions where old/new visibility cannot yet be expressed
    /// as a narrower rect.
    #[track_caller]
    pub(crate) fn mark_scene_structure_dirty(&mut self) {
        let event_id = if self.damage_audit_active() {
            self.record_damage_audit_event(Location::caller(), self.full_output_audit_area())
        } else {
            None
        };
        self.scene_structure_dirty = true;
        if let Some(inner) = self.inner.as_mut() {
            let mut contributed = Vec::with_capacity(inner.outputs.len());
            for o in &mut inner.outputs {
                let extent = o.output_extent;
                o.scene_structure_damage.add(vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent,
                });
                contributed.push(o.output_idx);
            }
            if let Some(id) = event_id {
                note_damage_audit_contributions(inner, id, &contributed);
            }
        }
    }

    /// Region-precise scene-structure damage (Stage 3+).
    #[track_caller]
    pub(crate) fn mark_scene_structure_damage_rect(&mut self, output_idx: usize, r: vk::Rect2D) {
        let event_id = if self.damage_audit_active() {
            self.record_damage_audit_event(Location::caller(), vec![r])
        } else {
            None
        };
        self.scene_structure_dirty = true;
        if let Some(inner) = self.inner.as_mut()
            && let Some(o) = inner.outputs.get_mut(output_idx)
        {
            o.scene_structure_damage.add(r);
            if let Some(id) = event_id {
                note_damage_audit_contributions(inner, id, &[output_idx]);
            }
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
    #[track_caller]
    pub(crate) fn mark_scene_structure_damage_rects(&mut self, rects: &[vk::Rect2D]) {
        let event_id = if self.damage_audit_active() {
            self.record_damage_audit_event(Location::caller(), rects.to_vec())
        } else {
            None
        };
        self.scene_structure_dirty = true;
        let Some(inner) = self.inner.as_mut() else {
            return;
        };
        let mut contributed = Vec::new();
        for output_idx in 0..inner.outputs.len() {
            let output = &mut inner.outputs[output_idx];
            let before = output.scene_structure_damage.rects().len();
            dispatch_clip_rects_to_outputs(
                std::iter::once((
                    output.output_origin,
                    output.output_extent,
                    &mut output.scene_structure_damage,
                )),
                rects,
            );
            if output.scene_structure_damage.rects().len() != before {
                contributed.push(output_idx);
            }
        }
        if let Some(id) = event_id {
            note_damage_audit_contributions(inner, id, &contributed);
        }
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
        let outcome = self.root_overlay.toggle(client, value, rects);
        if outcome.changed {
            let mut dmg = rects.to_vec();
            dmg.extend(self.root_overlay.all_rects());
            // An erase that does not exactly match what was drawn inserts a
            // second copy instead of removing the first; the two XOR to
            // identity, so every fresh compose looks right while pixels
            // inverted earlier stay stale — the same symptom as damage that
            // never composed. `removed`/`inserted` separate the two.
            if tick_skip_log_enabled() {
                log::info!(
                    "overlay-diag: value={value:#x} batch={} removed={} inserted={} total={} damaged={}",
                    rects.len(),
                    outcome.removed,
                    outcome.inserted,
                    outcome.total,
                    dmg.len(),
                );
            }
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

    /// True if any output's per-BO damage model owes a repaint that no producer
    /// is reporting — after an `invalidate`, or a submitted frame that never
    /// reached the screen. The backend's compose-wanted predicate must include
    /// this, or an invalidated output waits for unrelated damage to be repaired.
    pub(crate) fn owes_repaint(&self) -> bool {
        self.inner
            .as_ref()
            .is_some_and(|inner| inner.outputs.iter().any(|o| o.damage.owes_repaint()))
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
            // Step 3 — this pops every ack, and retains any whose fence wait
            // failed, so a staged frame can be discarded or left half-retired.
            // Invalidating is consistent with both, and it is also how suspend,
            // DPMS off/on and the topology quiesce get covered: all three run
            // `drain_all` first.
            o.damage.invalidate();
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
        let mut composed = Vec::new();
        let mut clear_dirty = true;
        // Idle free-run fix (cut 2b): union of sampled sources drawn
        // across all outputs, and whether every output actually walked
        // `build_scene`. Only reconcile `offscreen_no_draw` when all
        // walked — see `TickOutcome::walked`.
        let mut drawn: std::collections::HashSet<super::store::DrawableId> =
            std::collections::HashSet::new();
        let mut had_pieces: std::collections::HashSet<super::store::DrawableId> =
            std::collections::HashSet::new();
        // Which outputs ran `build_scene` this tick. Dormancy is reconciled on
        // any tick where at least one did; outputs that did not walk
        // contribute their retained `last_pieces` instead (see
        // `dormancy_inputs`). Requiring EVERY output to walk (the old rule)
        // meant that with two outputs and one of them skipping as
        // `NothingPending`, reconciliation never ran at all and nothing ever
        // went dormant — the root's covered damage was re-peeked and
        // re-classified Hidden ~1000×/s (silence/MATE, 2026-09-04).
        let mut walked_outputs: Vec<bool> = vec![false; inner.outputs.len()];
        if damage_audit_enabled() {
            emit_damage_audit_heartbeat(inner);
        }
        // Inputs of the pre-walk predicate that are global to the tick, read
        // once: the structure flag, and whether any armed drawable has
        // presentation damage waiting (an O(drawables) scan with an early exit
        // — far cheaper than one walk, let alone one per output).
        let structure_dirty = *scene_structure_dirty;
        // Per output: can an armed, damaged drawable land here? Decided from
        // each output's retained `last_pieces` — see
        // `pending_presentation_for_output`. Computed for every output up
        // front because the loop below borrows `inner` mutably.
        let pending_presentation_per_output: Vec<bool> = {
            let armed = store.armed_damaged_ids();
            let all: Vec<&std::collections::HashSet<super::store::DrawableId>> =
                inner.outputs.iter().map(|o| &o.last_pieces).collect();
            inner
                .outputs
                .iter()
                .map(|o| pending_presentation_for_output(&armed, &o.last_pieces, &all))
                .collect()
        };
        for (output_idx, &pending_presentation) in
            pending_presentation_per_output.iter().enumerate()
        {
            // The union of the OTHER outputs' retained pieces, read now rather
            // than before the loop so an output walked earlier in this same
            // iteration contributes its fresh set.
            let elsewhere: std::collections::HashSet<super::store::DrawableId> = inner
                .outputs
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != output_idx)
                .flat_map(|(_, o)| o.last_pieces.iter().copied())
                .collect();
            match tick_one_output(
                inner,
                output_idx,
                core,
                store,
                platform,
                windows,
                telemetry,
                true,
                cow_host_xid,
                root_overlay,
                &elsewhere,
                &mut drawn,
                &mut had_pieces,
                structure_dirty,
                pending_presentation,
            ) {
                Ok(outcome) => {
                    if outcome == TickOutcome::Composed {
                        composed.push(output_idx);
                    } else {
                        clear_dirty &= outcome.clears_scene_structure_dirty();
                    }
                    walked_outputs[output_idx] = outcome.walked();
                }
                Err(e) => {
                    clear_dirty = false;
                    log::warn!(
                        "render scene tick: output {output_idx} compose failed: {e}; continuing",
                    );
                }
            }
        }
        if walked_outputs.iter().any(|w| *w) {
            let none = std::collections::HashSet::new();
            let reports: Vec<OutputWalkReport<'_>> = inner
                .outputs
                .iter()
                .zip(&walked_outputs)
                .map(|(o, &walked)| OutputWalkReport {
                    walked,
                    // `drawn` is the union over the walked outputs; for the
                    // union `dormancy_inputs` takes, attributing it to each
                    // walked output is equivalent.
                    presented: if walked { &drawn } else { &none },
                    last_pieces: &o.last_pieces,
                })
                .collect();
            let (keep_armed, pieces_anywhere) = dormancy_inputs(&reports);
            debug_assert!(
                had_pieces.iter().all(|id| pieces_anywhere.contains(id)),
                "a walked output's pieces are its retained last_pieces"
            );
            let changed = store.reconcile_offscreen_no_draw(&keep_armed, &pieces_anywhere);
            // A window whose damage is pending while it is wrongly dormant is
            // stranded: `NoPieces` never re-arms on paint, so its content only
            // heals where something else composes. Log every transition behind
            // the tick-skip gate so a hardware log can name the drawable
            // instead of leaving the verdict to inference.
            if tick_skip_log_enabled() {
                for (id, reason) in changed {
                    log::info!("dormant-diag: drawable={id:?} reason={reason:?}");
                }
            }
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
            // `platform.on_page_flip_complete` above already advanced the BO
            // phase machine (previous OnScreen -> Free, Pending -> OnScreen), and
            // we are about to return without popping the ack. Platform and scene
            // have diverged, so neither `missing` nor any staged frame can be
            // trusted: fall back to "the whole output is stale", which costs one
            // full repaint and cannot show a stale pixel.
            //
            // The retained ack itself is a pre-existing wedge (the tick's
            // flip-pending gate will skip this output until another flip event
            // arrives); this is deliberately not the change that addresses it.
            state.damage.invalidate();
            return false;
        }
        if let Some(ack) = state.pending_acks.pop_front() {
            // Ack each per-drawable damage snapshot. Snapshots
            // from paint that landed after the tick's peek
            // survive (per I5 epoch semantics).
            for snap in ack.drawable_snapshots {
                if tick_skip_log_enabled() {
                    log::info!(
                        "ack-diag: out{output_idx} acked drawable={:?} epoch={} rects={}",
                        snap.id,
                        snap.epoch,
                        snap.region.rects().len(),
                    );
                }
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
            // Step 3 — the staged frame reached the screen: its damage is now
            // stale in every OTHER BO, and what it painted is no longer missing
            // from the one it painted into. A no-op when nothing was staged,
            // which is what makes it safe to call on a copied output too.
            state.damage.retire_success();
            // Step 2 — the frame reached the screen, so it becomes the baseline
            // the next diff runs against.
            state.prev_presented = ack.submitted_participants;
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

fn damage_audit_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        matches!(
            std::env::var("YSERVER_DAMAGE_AUDIT").ok().as_deref(),
            Some("1") | Some("true") | Some("TRUE") | Some("yes") | Some("YES")
        )
    })
}

fn damage_audit_interval() -> u64 {
    static INTERVAL: OnceLock<u64> = OnceLock::new();
    *INTERVAL.get_or_init(|| {
        std::env::var("YSERVER_DAMAGE_AUDIT_INTERVAL")
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|&n| n > 0)
            .unwrap_or(1)
    })
}

/// Seconds between idle re-comparisons. At true idle no transition is
/// recorded, so the event-gated empty-damage hook never fires and the
/// candidate is never checked. Without this a stale divergence simply
/// stops being reported the moment the desktop goes quiet, and a static
/// soak — the primary gate — cannot produce evidence either way.
fn damage_audit_idle_recompare_secs() -> u64 {
    static SECS: OnceLock<u64> = OnceLock::new();
    *SECS.get_or_init(|| {
        std::env::var("YSERVER_DAMAGE_AUDIT_IDLE_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|&n| n > 0)
            .unwrap_or(1)
    })
}

/// Frame at which the candidate is first seeded. Seeding at frame 1 means
/// the candidate samples drawable storage during startup churn, so a paint
/// whose GPU work lands after the seed — but whose damage was already
/// consumed that same frame — latches a stale read the candidate can never
/// correct. Delaying the seed separates that from a genuine damage hole:
/// if a divergence still appears at the first compared frame after a late
/// seed, the damage really is incomplete.
fn damage_audit_seed_frame() -> u64 {
    static SEED: OnceLock<u64> = OnceLock::new();
    *SEED.get_or_init(|| {
        std::env::var("YSERVER_DAMAGE_AUDIT_SEED_FRAME")
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|&n| n > 0)
            .unwrap_or(1)
    })
}

/// Whether this output's candidate is due an idle re-comparison.
fn audit_idle_recompare_due(inner: &SceneCompositorInner, output_idx: usize) -> bool {
    let Some(audit) = inner
        .outputs
        .get(output_idx)
        .and_then(|o| o.damage_audit.as_ref())
    else {
        return false;
    };
    if !audit.initialized {
        return false;
    }
    let due = std::time::Duration::from_secs(damage_audit_idle_recompare_secs());
    audit.last_compare_at.is_none_or(|at| at.elapsed() >= due)
}

/// Mean repaint-bbox area as a fraction of the output, over non-idle
/// comparisons. Near 1.0 means the run was almost all whole-output
/// repaints and says nothing about damage completeness.
fn mean_damage_fraction(audit: &OutputDamageAudit) -> f64 {
    if audit.damage_frames == 0 {
        return 0.0;
    }
    let area = u128::from(audit.candidate.extent.width) * u128::from(audit.candidate.extent.height);
    let denom = area.saturating_mul(u128::from(audit.damage_frames));
    if denom == 0 {
        return 0.0;
    }
    #[allow(clippy::cast_precision_loss)]
    let fraction = (audit.damage_pixels as f64) / (denom as f64);
    fraction
}

#[allow(clippy::cast_precision_loss)]
fn mean_us(total_ns: u128, samples: u64) -> f64 {
    if samples == 0 {
        return 0.0;
    }
    (total_ns as f64) / (samples as f64) / 1000.0
}

/// Periodic proof-of-life. A clean run is only meaningful if the audit
/// can be shown to have actually looked; a silent log is otherwise
/// indistinguishable between "running and clean" and "not running".
fn emit_damage_audit_heartbeat(inner: &mut SceneCompositorInner) {
    const HEARTBEAT: std::time::Duration = std::time::Duration::from_secs(5);
    for output_idx in 0..inner.outputs.len() {
        let Some(audit) = inner.outputs[output_idx].damage_audit.as_mut() else {
            continue;
        };
        if audit
            .last_heartbeat_at
            .is_some_and(|at| at.elapsed() < HEARTBEAT)
        {
            continue;
        }
        audit.last_heartbeat_at = Some(std::time::Instant::now());
        log::info!(
            "damage-audit\theartbeat\toutput={output_idx}\tframe={}\tcomparisons={}\
             \tidle={}\tpartial={}\tfull={}\tmean_damage={:.3}\
             \tclipped_us={:.1}\tfull_us={:.1}\tgpu_n={}\
             \tepisodes_open={}\tepisodes_opened={}\tepisodes_healed={}\tresets={}",
            audit.frame,
            audit.comparisons,
            audit.comparisons_idle,
            audit.comparisons_partial,
            audit.comparisons_full,
            mean_damage_fraction(audit),
            mean_us(audit.clipped_gpu_ns, audit.gpu_samples),
            mean_us(audit.full_gpu_ns, audit.gpu_samples),
            audit.gpu_samples,
            audit.active_episodes.len(),
            audit.episodes_opened,
            audit.episodes_healed,
            audit.reset_count,
        );
    }
}

fn note_damage_audit_contributions(
    inner: &mut SceneCompositorInner,
    event_id: u64,
    output_indices: &[usize],
) {
    if !damage_audit_enabled() {
        return;
    }
    if let Some(entry) = inner
        .damage_audit_ledger
        .iter_mut()
        .find(|entry| entry.id == event_id)
    {
        for &output_idx in output_indices {
            if !entry.contributed_outputs.contains(&output_idx) {
                entry.contributed_outputs.push(output_idx);
            }
        }
    }
}

fn bound_damage_audit_ledger(inner: &mut SceneCompositorInner) {
    const MAX_LEDGER_ENTRIES: usize = 16_384;
    while inner.damage_audit_ledger.len() > MAX_LEDGER_ENTRIES {
        if let Some(entry) = inner.damage_audit_ledger.pop_front() {
            log::warn!(
                "damage-audit: ledger bound hit; dropped oldest event id={} site={}:{}; run suspect",
                entry.id,
                entry.site.file(),
                entry.site.line()
            );
        }
    }
}

fn retire_damage_audit_ledger(inner: &mut SceneCompositorInner) {
    let min_consumed = inner
        .outputs
        .iter()
        .filter_map(|output| {
            output
                .damage_audit
                .as_ref()
                .map(|audit| audit.consumed_event_id)
        })
        .min();
    let Some(min_consumed) = min_consumed else {
        return;
    };
    while inner
        .damage_audit_ledger
        .front()
        .is_some_and(|entry| entry.id < min_consumed)
    {
        inner.damage_audit_ledger.pop_front();
    }
}

fn audit_has_unretired_event(inner: &SceneCompositorInner, output_idx: usize) -> bool {
    let consumed = inner.outputs[output_idx]
        .damage_audit
        .as_ref()
        .map(|audit| audit.consumed_event_id)
        .unwrap_or(0);
    inner
        .damage_audit_ledger
        .back()
        .is_some_and(|entry| entry.id >= consumed)
}

fn audit_overlay_pipeline(
    inner: &mut SceneCompositorInner,
    needed: bool,
) -> Result<(vk::Pipeline, vk::PipelineLayout), SceneError> {
    if !needed {
        return Ok((vk::Pipeline::null(), vk::PipelineLayout::null()));
    }
    let pipeline = inner
        .overlay_xor_cache
        .get(yserver_core::backend::GcFunction::Xor, true)?;
    Ok((pipeline, inner.overlay_xor_cache.pipeline_layout()))
}

/// Diagnostic: pair each sampled drawable with its xid so a damage-audit
/// mismatch can name the drawable whose contents changed.
fn audit_sampled_pairs(
    store: &DrawableStore,
    sampled_ids: &[super::store::DrawableId],
) -> Vec<(u64, u32)> {
    if !damage_audit_enabled() {
        return Vec::new();
    }
    sampled_ids
        .iter()
        .map(|id| {
            let xid = store
                .xid_entries()
                .find_map(|(xid, entry)| (entry == *id).then_some(xid))
                .unwrap_or(0);
            (id.as_u64(), xid)
        })
        .collect()
}

/// Step 1 — the damage audit's reference scene: the same frame built with
/// `Visibility::Off`, so the reference paints every node's full placement while
/// the candidate paints what the visibility walk left. Only built when the audit
/// is armed (one extra walk per audited frame). Mirrors the production build's
/// software-cursor decision so the two scenes differ in visibility alone.
#[allow(clippy::too_many_arguments)]
fn audit_reference_scene(
    production_has_sw_cursor: bool,
    core: &KmsCore,
    store: &mut DrawableStore,
    windows: &super::backend::WindowsMap,
    output_idx: usize,
    platform: &PlatformBackend,
    cursor: Option<CursorEntry>,
    cursor_prev_pos: Option<(i32, i32)>,
    cow_host_xid: Option<u32>,
    hw_strategy_active: bool,
) -> Option<SceneBuild> {
    if !damage_audit_enabled() {
        return None;
    }
    let mut reference = build_scene(
        core,
        store,
        windows,
        output_idx,
        platform,
        cursor,
        cursor_prev_pos,
        cow_host_xid,
        hw_strategy_active,
        Visibility::Off,
    );
    if !production_has_sw_cursor {
        // Either the production frame had no software cursor or the tick
        // stripped it for a hide frame; either way the reference must not
        // carry one.
        reference.omit_software_cursor_for_hide();
    }
    Some(reference)
}

#[allow(clippy::too_many_arguments)]
fn run_damage_audit(
    inner: &mut SceneCompositorInner,
    output_idx: usize,
    platform: &mut PlatformBackend,
    scene: &CompositeScene,
    // Step 1 — the scene the REFERENCE composes: built with `Visibility::Off`,
    // i.e. every node's full placement. Candidate and reference used to render
    // the same list, so a visibility bug that hides pixels would have passed
    // clean on both sides and the audit would be vacuous for step 1. With the
    // unclipped reference, a pixel the visibility walk wrongly culls shows up
    // as a mismatch. Identical to `scene` when the audit is not armed.
    reference_scene: &CompositeScene,
    sampled: &[(u64, u32)],
    output_damage: &RegionSet,
    reset_reason: Option<&str>,
    compare_after_empty_damage: bool,
    overlay_ops: &[(u32, vk::Rect2D)],
    xor_pipeline: vk::Pipeline,
    xor_layout: vk::PipelineLayout,
) -> Result<(), SceneError> {
    if !damage_audit_enabled() {
        return Ok(());
    }
    if !matches!(
        platform
            .scanout_pools
            .get(output_idx)
            .and_then(Option::as_ref),
        Some(OutputScanout::Shared(_))
    ) {
        log::debug!("damage-audit: output {output_idx} skipped; copied scanout is out of scope");
        return Ok(());
    }

    let latest_event_id = inner.damage_audit_next_event_id;
    let Some(audit) = inner.outputs[output_idx].damage_audit.as_mut() else {
        return Ok(());
    };
    if let Some(reason) = reset_reason {
        audit.initialized = false;
        audit.active_episodes.clear();
        audit.reset_count = audit.reset_count.saturating_add(1);
        log::info!(
            "damage-audit\treset\toutput={output_idx}\tframe={}\treason={reason}\tresets={}",
            audit.frame,
            audit.reset_count
        );
    }

    audit.frame = audit.frame.saturating_add(1);
    let frame = audit.frame;
    let interval = damage_audit_interval();
    let compare_this_frame = interval == 1 || frame.is_multiple_of(interval);

    let mut just_seeded = false;

    // Hold off seeding until the configured frame so the candidate is not
    // captured mid-startup. Events are still consumed so the ledger does
    // not accumulate across the delay.
    if !audit.initialized && frame < damage_audit_seed_frame() {
        audit.consumed_event_id = latest_event_id;
        retire_damage_audit_ledger(inner);
        return Ok(());
    }

    if !audit.initialized {
        let candidate_extent = audit.candidate.extent;
        let complete = submit_audit_compose(
            &inner.vk,
            platform,
            &inner.pipeline,
            &mut audit.candidate,
            scene,
            Repaint::Full(candidate_extent),
            overlay_ops,
            xor_pipeline,
            xor_layout,
        )?;
        if !complete {
            audit.initialized = false;
            audit.consumed_event_id = latest_event_id;
            log::warn!(
                "damage-audit\treset\toutput={output_idx}\tframe={frame}\
                 \treason=partial-seed-compose\trun_suspect=true"
            );
            retire_damage_audit_ledger(inner);
            return Ok(());
        }
        audit.initialized = true;
        audit.seed_draws = scene.draws.len();
        audit.seed_sampled = sampled.to_vec();
        log::info!(
            "damage-audit\tseed\toutput={output_idx}\tframe={frame}\tdraws={}\tsampled={:?}",
            scene.draws.len(),
            sampled,
        );
        // Fall through to compose the reference and compare on this very
        // frame. Both images are then full composes of the same scene at
        // the same instant, so a mismatch HERE cannot be a damage hole —
        // it means the two composes disagree, i.e. the compose sampled
        // drawable storage whose paint had not landed. That is the only
        // clean way to separate a startup sampling artefact from a real
        // hole straddled by the seed.
        just_seeded = true;
    }

    if !compare_after_empty_damage && !just_seeded {
        let Some(candidate_repaint) = output_damage
            .bounding_rect()
            .map(Repaint::AuditClearClipped)
        else {
            log::warn!(
                "damage-audit\tskip\toutput={output_idx}\tframe={frame}\
                 \treason=empty-candidate-damage-on-compose-path\trun_suspect=true"
            );
            return Ok(());
        };
        let complete = submit_audit_compose(
            &inner.vk,
            platform,
            &inner.pipeline,
            &mut audit.candidate,
            scene,
            candidate_repaint,
            overlay_ops,
            xor_pipeline,
            xor_layout,
        )?;
        if !complete {
            audit.initialized = false;
            audit.active_episodes.clear();
            audit.consumed_event_id = latest_event_id;
            log::warn!(
                "damage-audit\treset\toutput={output_idx}\tframe={frame}\
                 \treason=partial-candidate-compose\trun_suspect=true"
            );
            retire_damage_audit_ledger(inner);
            return Ok(());
        }
    }

    if !compare_this_frame && !just_seeded {
        audit.consumed_event_id = latest_event_id;
        retire_damage_audit_ledger(inner);
        return Ok(());
    }

    let reference_extent = audit.reference.extent;
    let complete = submit_audit_compose(
        &inner.vk,
        platform,
        &inner.pipeline,
        &mut audit.reference,
        reference_scene,
        Repaint::Full(reference_extent),
        overlay_ops,
        xor_pipeline,
        xor_layout,
    )?;
    if !complete {
        audit.initialized = false;
        audit.active_episodes.clear();
        audit.consumed_event_id = latest_event_id;
        log::warn!(
            "damage-audit\treset\toutput={output_idx}\tframe={frame}\
             \treason=partial-reference-compose\trun_suspect=true"
        );
        retire_damage_audit_ledger(inner);
        return Ok(());
    }

    // Classify this comparison before running it — see the field docs on
    // `comparisons_full`. `compare_after_empty_damage` is the idle path,
    // where the candidate is deliberately left untouched and a match is a
    // genuine retention test.
    let output_area =
        u128::from(audit.candidate.extent.width) * u128::from(audit.candidate.extent.height);
    if compare_after_empty_damage {
        audit.comparisons_idle = audit.comparisons_idle.saturating_add(1);
    } else {
        let bbox = output_damage.bounding_rect().map_or(0u128, |r| {
            u128::from(r.extent.width) * u128::from(r.extent.height)
        });
        if bbox >= output_area {
            audit.comparisons_full = audit.comparisons_full.saturating_add(1);
        } else {
            audit.comparisons_partial = audit.comparisons_partial.saturating_add(1);
        }
        audit.damage_pixels = audit.damage_pixels.saturating_add(bbox);
        audit.damage_frames = audit.damage_frames.saturating_add(1);
    }

    if !compare_after_empty_damage
        && !just_seeded
        && let (Some(clipped), Some(full)) = (
            audit.candidate.last_gpu_render_ns,
            audit.reference.last_gpu_render_ns,
        )
    {
        audit.clipped_gpu_ns = audit.clipped_gpu_ns.saturating_add(u128::from(clipped));
        audit.full_gpu_ns = audit.full_gpu_ns.saturating_add(u128::from(full));
        audit.gpu_samples = audit.gpu_samples.saturating_add(1);
    }

    submit_damage_audit_compare(&inner.vk, platform, audit)?;
    audit.frame_draws = scene.draws.len();
    audit.frame_sampled = sampled.to_vec();
    audit.comparisons = audit.comparisons.saturating_add(1);
    audit.last_compare_at = Some(std::time::Instant::now());
    let summaries = audit.compare.read_summary().map_err(SceneError::Vk)?;
    process_damage_audit_summary(
        output_idx,
        audit,
        &inner.damage_audit_ledger,
        &summaries,
        audit.consumed_event_id,
        latest_event_id,
        interval,
        just_seeded,
    );
    audit.consumed_event_id = latest_event_id;
    retire_damage_audit_ledger(inner);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn submit_audit_compose(
    vk: &crate::kms::vk::device::VkContext,
    platform: &PlatformBackend,
    pipeline: &CompositorPipeline,
    target: &mut DamageAuditTarget,
    scene: &CompositeScene,
    repaint: Repaint,
    overlay_ops: &[(u32, vk::Rect2D)],
    xor_pipeline: vk::Pipeline,
    xor_layout: vk::PipelineLayout,
) -> Result<bool, SceneError> {
    let descriptor_pool = create_audit_descriptor_pool(vk, scene.draws.len())?;
    let ticket = platform.acquire_fence_ticket().map_err(SceneError::Vk)?;
    let mut gpu_submitted = false;
    let result = record_and_submit_render(
        vk,
        target,
        pipeline,
        descriptor_pool,
        scene,
        repaint,
        &[],
        ticket.fence(),
        &mut gpu_submitted,
        overlay_ops,
        xor_pipeline,
        xor_layout,
    );
    let wait = if result.is_ok() {
        ticket.wait(vk).map_err(SceneError::Vk)
    } else {
        Ok(())
    };
    unsafe {
        vk.device.destroy_descriptor_pool(descriptor_pool, None);
    }
    let submitted = result?;
    wait?;
    Ok(compose_submit_was_complete(submitted, scene.draws.len()))
}

fn compose_submit_was_complete(submitted: ComposeSubmit, draw_count: usize) -> bool {
    submitted.descriptor_count == draw_count
}

/// Step 3's staging, gated on the submit having recorded every draw.
///
/// A truncated submit (descriptor pool exhausted, `record_command_buffer` drew
/// only the allocated prefix) painted less than `painted` claims. Recording it
/// as painted would clear `missing` for pixels never touched and bake the hole
/// into that BO permanently; `invalidate` costs one full repaint instead.
fn stage_submitted_frame(
    damage: &mut ScanoutDamage,
    complete: bool,
    bo_idx: usize,
    repaint: &Region,
    painted: &Region,
) {
    if complete {
        damage.commit_submitted(bo_idx, repaint, painted);
    } else {
        damage.invalidate();
    }
}

fn create_audit_descriptor_pool(
    vk: &crate::kms::vk::device::VkContext,
    draw_count: usize,
) -> Result<vk::DescriptorPool, SceneError> {
    let count = u32::try_from(draw_count.max(1)).unwrap_or(u32::MAX);
    let pool_sizes = [vk::DescriptorPoolSize {
        ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
        descriptor_count: count,
    }];
    let pool_info = vk::DescriptorPoolCreateInfo::default()
        .max_sets(count)
        .pool_sizes(&pool_sizes);
    unsafe { vk.device.create_descriptor_pool(&pool_info, None) }.map_err(SceneError::Vk)
}

fn submit_damage_audit_compare(
    vk: &crate::kms::vk::device::VkContext,
    platform: &PlatformBackend,
    audit: &mut OutputDamageAudit,
) -> Result<(), SceneError> {
    let cb = audit.candidate.command_buffer;
    let ticket = platform.acquire_fence_ticket().map_err(SceneError::Vk)?;
    unsafe {
        vk.device
            .reset_command_buffer(cb, vk::CommandBufferResetFlags::empty())
            .map_err(SceneError::Vk)?;
        let begin = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        vk.device
            .begin_command_buffer(cb, &begin)
            .map_err(SceneError::Vk)?;

        let to_transfer = [
            image_general_to_transfer_src_barrier(audit.candidate.image),
            image_general_to_transfer_src_barrier(audit.reference.image),
        ];
        vk.device.cmd_pipeline_barrier2(
            cb,
            &vk::DependencyInfo::default().image_memory_barriers(&to_transfer),
        );

        let extent = vk::Extent3D {
            width: audit.candidate.extent.width,
            height: audit.candidate.extent.height,
            depth: 1,
        };
        let copy = [vk::BufferImageCopy2::default()
            .image_subresource(
                vk::ImageSubresourceLayers::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .layer_count(1),
            )
            .image_extent(extent)];
        vk.device.cmd_copy_image_to_buffer2(
            cb,
            &vk::CopyImageToBufferInfo2::default()
                .src_image(audit.candidate.image)
                .src_image_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                .dst_buffer(audit.compare.candidate_buffer())
                .regions(&copy),
        );
        vk.device.cmd_copy_image_to_buffer2(
            cb,
            &vk::CopyImageToBufferInfo2::default()
                .src_image(audit.reference.image)
                .src_image_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                .dst_buffer(audit.compare.reference_buffer())
                .regions(&copy),
        );

        audit.compare.record_after_transfers(cb);

        let to_general = [
            image_transfer_src_to_general_barrier(audit.candidate.image),
            image_transfer_src_to_general_barrier(audit.reference.image),
        ];
        vk.device.cmd_pipeline_barrier2(
            cb,
            &vk::DependencyInfo::default().image_memory_barriers(&to_general),
        );

        vk.device.end_command_buffer(cb).map_err(SceneError::Vk)?;
        let cb_info = [vk::CommandBufferSubmitInfo::default().command_buffer(cb)];
        let submit = [vk::SubmitInfo2::default().command_buffer_infos(&cb_info)];
        vk.device
            .queue_submit2(vk.graphics_queue, &submit, ticket.fence())
            .map_err(SceneError::Vk)?;
    }
    ticket.wait(vk).map_err(SceneError::Vk)
}

fn image_general_to_transfer_src_barrier(image: vk::Image) -> vk::ImageMemoryBarrier2<'static> {
    vk::ImageMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
        .src_access_mask(vk::AccessFlags2::MEMORY_WRITE)
        .dst_stage_mask(vk::PipelineStageFlags2::COPY)
        .dst_access_mask(vk::AccessFlags2::TRANSFER_READ)
        .old_layout(vk::ImageLayout::GENERAL)
        .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
        .image(image)
        .subresource_range(
            vk::ImageSubresourceRange::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .level_count(1)
                .layer_count(1),
        )
}

fn image_transfer_src_to_general_barrier(image: vk::Image) -> vk::ImageMemoryBarrier2<'static> {
    vk::ImageMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::COPY)
        .src_access_mask(vk::AccessFlags2::TRANSFER_READ)
        .dst_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
        .dst_access_mask(vk::AccessFlags2::MEMORY_READ | vk::AccessFlags2::MEMORY_WRITE)
        .old_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
        .new_layout(vk::ImageLayout::GENERAL)
        .image(image)
        .subresource_range(
            vk::ImageSubresourceRange::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .level_count(1)
                .layer_count(1),
        )
}

fn process_damage_audit_summary(
    output_idx: usize,
    audit: &mut OutputDamageAudit,
    ledger: &VecDeque<DamageAuditLedgerEntry>,
    summaries: &[DamageAuditTileSummary],
    first_event_id: u64,
    latest_event_id: u64,
    interval: u64,
    at_seed: bool,
) {
    let mut mismatched_tiles = HashSet::new();
    let grid_width = audit.compare.grid_width();
    let grid_height = audit.compare.grid_height();
    for summary in summaries {
        if summary.mismatch_count == 0 {
            continue;
        }
        mismatched_tiles.insert(summary.tile_id);
        if audit.active_episodes.contains_key(&summary.tile_id) {
            continue;
        }
        audit.episodes_opened = audit.episodes_opened.saturating_add(1);
        let start = DamageAuditEpisodeStart {
            frame: audit.frame,
            first_event_id,
            next_event_id: latest_event_id,
        };
        audit.active_episodes.insert(summary.tile_id, start);
        let tile_rect = tile_rect_for_id(
            audit.candidate.extent,
            grid_width,
            grid_height,
            summary.tile_id,
        );
        let candidates = ledger_candidates_for_tile(
            ledger,
            output_idx,
            tile_rect,
            first_event_id,
            latest_event_id,
        );
        let first_x = summary.first_pixel_index % audit.candidate.extent.width;
        let first_y = summary.first_pixel_index / audit.candidate.extent.width;
        log::warn!(
            "damage-audit\tmismatch\toutput={output_idx}\tframe={}\ttile={}\tpixel={},{}\
             \tcount={}\tcandidate=0x{:08x}\treference=0x{:08x}\tseed_draws={}\tdraws={}\
             \tseed_sampled={:?}\tsampled={:?}\
             \tledger={}\tinterval={}\tqualifies={}\tat_seed={}",
            audit.frame,
            summary.tile_id,
            first_x,
            first_y,
            summary.mismatch_count,
            summary.candidate,
            summary.reference,
            audit.seed_draws,
            audit.frame_draws,
            audit.seed_sampled,
            audit.frame_sampled,
            candidates,
            interval,
            interval == 1,
            at_seed,
        );
    }

    let healed: Vec<u32> = audit
        .active_episodes
        .keys()
        .copied()
        .filter(|tile| !mismatched_tiles.contains(tile))
        .collect();
    for tile in healed {
        if let Some(start) = audit.active_episodes.remove(&tile) {
            audit.episodes_healed = audit.episodes_healed.saturating_add(1);
            log::info!(
                "damage-audit\thealed\toutput={output_idx}\tframe={}\ttile={tile}\
                 \tstart_frame={}\tstart_event={}\thealed={}",
                audit.frame,
                start.frame,
                start.first_event_id,
                audit.episodes_healed
            );
        }
    }
}

fn ledger_candidates_for_tile(
    ledger: &VecDeque<DamageAuditLedgerEntry>,
    output_idx: usize,
    tile: vk::Rect2D,
    first_event_id: u64,
    next_event_id: u64,
) -> String {
    let mut out = String::new();
    for entry in ledger {
        if entry.id < first_event_id || entry.id >= next_event_id {
            continue;
        }
        if !entry
            .expected_area
            .iter()
            .any(|expected| rects_intersect(*expected, tile))
        {
            continue;
        }
        if !out.is_empty() {
            out.push(',');
        }
        let contributed = entry.contributed_outputs.contains(&output_idx);
        let _ = std::fmt::Write::write_fmt(
            &mut out,
            format_args!(
                "{}@{}:{}:{}",
                entry.id,
                entry.site.file(),
                entry.site.line(),
                if contributed { "contrib" } else { "missing" }
            ),
        );
    }
    if out.is_empty() {
        "none".to_string()
    } else {
        out
    }
}

fn rects_intersect(a: vk::Rect2D, b: vk::Rect2D) -> bool {
    let ax1 = a.offset.x.saturating_add_unsigned(a.extent.width);
    let ay1 = a.offset.y.saturating_add_unsigned(a.extent.height);
    let bx1 = b.offset.x.saturating_add_unsigned(b.extent.width);
    let by1 = b.offset.y.saturating_add_unsigned(b.extent.height);
    a.offset.x < bx1 && b.offset.x < ax1 && a.offset.y < by1 && b.offset.y < ay1
}

fn tile_rect_for_id(
    extent: vk::Extent2D,
    grid_width: u32,
    grid_height: u32,
    tile_id: u32,
) -> vk::Rect2D {
    let tile_x = tile_id % grid_width;
    let tile_y = tile_id / grid_width;
    let (x0, x1) = partition_bounds(extent.width, grid_width, tile_x);
    let (y0, y1) = partition_bounds(extent.height, grid_height, tile_y);
    vk::Rect2D {
        offset: vk::Offset2D {
            x: i32::try_from(x0).unwrap_or(i32::MAX),
            y: i32::try_from(y0).unwrap_or(i32::MAX),
        },
        extent: vk::Extent2D {
            width: x1.saturating_sub(x0),
            height: y1.saturating_sub(y0),
        },
    }
}

fn partition_bounds(extent: u32, grid: u32, block: u32) -> (u32, u32) {
    let base = extent / grid;
    let extra = extent % grid;
    let start = block * base + block.min(extra);
    let end = start + base + u32::from(block < extra);
    (start, end)
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
/// Whether a tick with no output damage may skip composing.
///
/// `owed` is what the per-BO damage model still has to paint regardless of
/// producers — set by `ScanoutDamage::invalidate` (a truncated submit, a failed
/// flip, a return from direct scanout, a drain) and by a frame whose submit
/// succeeded but never reached the screen. Before this was consulted here, an
/// invalidated output with no fresh damage stayed on its stale frame until
/// unrelated damage arrived, because this skip ran before the plan ever looked
/// at the model (codex, post-merge review of `02bafec3`, finding 4).
fn skip_for_empty_damage(damage_empty: bool, first_frame: bool, owed: bool) -> bool {
    damage_empty && !first_frame && !owed
}

/// Whether this tick has to walk the scene at all.
///
/// The walk is how the tick learns whether anything is damaged, so until now
/// every wake walked every output and then, most of the time, took the
/// `EmptyDamage` skip. On the e16 phased workload that was 769 walks/s at 57
/// composes/s (13.8 per compose, 436 µs each — a third of a core), because
/// e16's pager copies ~1000 strips/s and every paint wakes the loop, and the
/// present-completion poll wakes it every millisecond besides.
///
/// Everything `build_scene` can discover is announced beforehand by one of
/// these inputs, so a tick where none is set cannot produce damage and may skip
/// without walking:
///
/// - `structure_dirty` — `scene_structure_dirty`: every mutation of a scene
///   input the walk reads (geometry, map state, stacking, shape, redirect
///   routing, storage reallocation, cursor image or software-cursor position,
///   root overlay, output topology, direct-scanout transitions) calls
///   `wake_for_damage` or `mark_scene_structure_*`. A hardware-cursor move
///   goes straight to the plane and correctly does not set it.
/// - `pending_presentation` — an ARMED drawable painted, was not acked, and
///   can land on THIS output: see [`pending_presentation_for_output`]. Dormant
///   drawables (`DormantReason`) are excluded by design; a `HiddenDamage` one
///   re-arms on its next paint. The global form of this input had no effect
///   on e16 (2 outputs): the pager paints ~1000/s on output 0, so some armed
///   drawable is always damaged, and the wake walked output 1 — where nothing
///   is — ~700 times a second to find `EmptyDamage`.
/// - `first_frame` — nothing has ever been presented on this output.
/// - `owed` — the per-BO damage model still owes a repaint (`invalidate`,
///   a frame that never reached the screen).
/// - `pending_structure` — this output already holds rect-precise structure
///   damage or a failed-submit repaint. Both setters also raise
///   `structure_dirty`; listed separately so the predicate does not depend on
///   that coupling.
/// - `audit_armed` — the damage audit uses the empty-damage path for its idle
///   re-compare and must keep walking.
///
/// A wake that a mutation site forgot shows up as a stale window; the audit
/// (`YSERVER_DAMAGE_AUDIT=1`) is what catches it, which is why it forces walks.
fn walk_needed(
    structure_dirty: bool,
    pending_presentation: bool,
    first_frame: bool,
    owed: bool,
    pending_structure: bool,
    audit_armed: bool,
) -> bool {
    structure_dirty
        || pending_presentation
        || first_frame
        || owed
        || pending_structure
        || audit_armed
}

/// One output's contribution to this tick's dormancy decision.
struct OutputWalkReport<'a> {
    /// `build_scene` ran for this output this tick.
    walked: bool,
    /// This walk's presented ids (empty when it did not walk).
    presented: &'a std::collections::HashSet<super::store::DrawableId>,
    /// The output's retained `last_pieces` — refreshed by this walk if it
    /// walked, otherwise as of its most recent walk.
    last_pieces: &'a std::collections::HashSet<super::store::DrawableId>,
}

/// The two sets `DrawableStore::reconcile_offscreen_no_draw` needs, computed so
/// that reconciliation can run on any tick where at least one output walked.
///
/// A drawable with pending damage goes dormant iff, for EVERY output, either
/// that output walked this tick and did not present it, or that output did not
/// walk and the drawable is not in its retained `last_pieces`. The second clause
/// is what makes skipping safe: an output that did not walk (flip pending,
/// retry deadline, nothing pending) has unchanged visibility since its last
/// walk — every structural change forces a walk everywhere — so if the drawable
/// has pieces there, that output may present it once it walks, and its pre-walk
/// predicate WILL walk it, because the drawable is armed and in its
/// `last_pieces`. Until then it must stay armed. Returns `(keep_armed,
/// pieces_anywhere)`: ids that stay armed, and ids with pieces on some output
/// (which decides `HiddenDamage` vs `NoPieces` for the rest).
fn dormancy_inputs(
    reports: &[OutputWalkReport<'_>],
) -> (
    std::collections::HashSet<super::store::DrawableId>,
    std::collections::HashSet<super::store::DrawableId>,
) {
    let mut keep_armed = std::collections::HashSet::new();
    let mut pieces_anywhere = std::collections::HashSet::new();
    for r in reports {
        if r.walked {
            keep_armed.extend(r.presented.iter().copied());
        } else {
            // Unchanged visibility since its last walk: whatever had pieces
            // there may still be presented there, once it walks.
            keep_armed.extend(r.last_pieces.iter().copied());
        }
        pieces_anywhere.extend(r.last_pieces.iter().copied());
    }
    (keep_armed, pieces_anywhere)
}

/// Whether any armed, damaged drawable can land on one output — the per-output
/// form of the `pending_presentation` input of [`walk_needed`].
///
/// `mine` is this output's retained `last_pieces`; `all` is every output's.
/// A drawable is pending for this output if it emitted pieces here in the
/// last walk, or if it emitted pieces on NO output — never walked, newly
/// created, or otherwise unknown — in which case every output walks. That is
/// the conservative direction: the cost of a wrong "yes" is one walk, the cost
/// of a wrong "no" is a stale window.
///
/// Why the sets are trustworthy without geometry: where a drawable is visible
/// changes only through a structural change, and every structural change sets
/// `scene_structure_dirty`, which forces the walk on its own. So whenever this
/// function is the deciding input, the sets are from a walk of the current
/// structure.
///
/// Multi-output: a drawable spanning both outputs is in both sets ⇒ both walk.
/// One on output 0 only ⇒ output 1 skips; its damage is captured by output 0's
/// compose and acked at that retire, as today. An off-output window emits no
/// pieces anywhere ⇒ in no set ⇒ every output walks, and today's
/// off-output force-compose path is preserved.
fn pending_presentation_for_output(
    armed: &[super::store::DrawableId],
    mine: &std::collections::HashSet<super::store::DrawableId>,
    all: &[&std::collections::HashSet<super::store::DrawableId>],
) -> bool {
    armed
        .iter()
        .any(|id| mine.contains(id) || !all.iter().any(|set| set.contains(id)))
}

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
    // What the OTHER outputs showed at their last walk, for classifying an
    // off-output paint as theirs (`ContentDamage::OtherOutput`) rather than
    // stranded. See `WalkSink::elsewhere`.
    elsewhere: &std::collections::HashSet<super::store::DrawableId>,
    drawn: &mut std::collections::HashSet<super::store::DrawableId>,
    // Sampled sources that emitted at least one piece on this output — with
    // `drawn` this picks the dormancy reason (see `DormantReason`).
    had_pieces: &mut std::collections::HashSet<super::store::DrawableId>,
    // Pre-walk predicate inputs read once per tick by `tick` — see
    // `walk_needed`.
    structure_dirty: bool,
    pending_presentation: bool,
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
        // 0b. Pre-walk predicate. Everything `build_scene` could find is
        //     announced by one of these inputs; if none is set, walking would
        //     only end in the `EmptyDamage` skip below at the cost of the walk.
        if !walk_needed(
            structure_dirty,
            pending_presentation,
            s.current_generation == 0,
            s.damage.owes_repaint(),
            !s.scene_structure_damage.is_empty()
                || !s.pending_repaint_after_failed_submit.is_empty(),
            damage_audit_enabled(),
        ) {
            record_tick_skip(s, output_idx, TickSkipReason::NothingPending, 0);
            telemetry.record_tick_skip_nothing_pending();
            return Ok(TickOutcome::Skipped(TickSkipReason::NothingPending));
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
    // Step 1 — the walk was unmeasured: `compose_cb_record_ns` starts after
    // `build_scene` returns, and this is the pass whose cost grows with the
    // window count. Timed on every tick that gets this far, composed or not.
    let build_scene_start = std::time::Instant::now();
    let mut built = build_scene_with(
        core,
        store,
        windows,
        output_idx,
        platform,
        inner.cursor.clone(),
        cursor_prev_pos_before,
        cow_host_xid,
        hw_can_run,
        Visibility::On,
        elsewhere,
    );
    telemetry.record_build_scene_ns(
        u64::try_from(build_scene_start.elapsed().as_nanos()).unwrap_or(u64::MAX),
    );
    // Retain what emitted pieces on this output for the pre-walk predicate.
    // Recorded before any skip below so the set always reflects the most
    // recent walk, composed or not.
    {
        let s = &mut inner.outputs[output_idx];
        s.last_pieces.clear();
        s.last_pieces.extend(built.pieces_ids.iter().copied());
    }
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

    // Idle free-run fix (cut 2b): record the sampled sources whose pending
    // damage this output PRESENTED, so `tick` can reconcile
    // `offscreen_no_draw` from the union across outputs. Recorded
    // unconditionally here (before the empty-damage / BO / pool skips below)
    // so a window that WAS presented is never mis-flagged just because its
    // output later skips. `presented_ids`, not `sampled_ids`: a node sampled
    // but with all of its damage under a cover must NOT count, or the
    // scheduler never goes dormant (see `WalkSink::presented_ids`).
    drawn.extend(built.presented_ids.iter().copied());
    had_pieces.extend(built.pieces_ids.iter().copied());

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

    // Step 2 — structural damage from diffing this frame's participants against
    // the last presented ones. Folded in HERE, before the empty-damage check
    // below: once 2b demotes the `mark_scene_structure_dirty` sites to a bare
    // wake, this is the ONLY thing that will report a map, unmap, restack or
    // drag. If it landed after that check, such a frame would find
    // `output_damage` empty, take the EmptyDamage skip, and the window would
    // never appear — a functional break, not a performance one.
    //
    // 2a keeps the whole-output hammer in place, so this can only ever add
    // damage the hammer already covers; it is exercised without being relied on.
    let structural = structural_damage(
        &inner.outputs[output_idx].prev_presented,
        &built.participants,
    );
    // Overdraw: summed draw area over output area, on the emitted draw list
    // before the scissor cull — how much the scene overpaints, not how much
    // survives a scissor. Summing clipped rect areas rather than unioning them
    // is deliberate: the union is the output area anyway, because the root
    // covers it.
    //
    // Computed here but RECORDED only on a frame that composes (beside
    // `record_damage_pixels`, which supplies the denominator). The walk runs on
    // every wake — ~11 per compose on silence/MATE — so recording it per walk
    // inflated `overdraw` by the walks-per-compose ratio: the "25×" measured on
    // 2026-09-02 was ~2× overdraw times ~11 walks per compose. Caught on the
    // z400 on 2026-09-03, where a startup bucket read 2284 with one compose.
    let scene_draw_pixels: u64 = built
        .scene
        .draws
        .iter()
        .filter_map(draw_dst_rect_inward)
        .filter_map(|r| clip_rect_to_output_extent(r, inner.outputs[output_idx].output_extent))
        .map(|r| u64::from(r.extent.width) * u64::from(r.extent.height))
        .sum();
    telemetry.record_structural_damage_pixels(structural.area());
    for rect in structural.rects() {
        output_damage.add(rect);
    }
    // Step 1 — nodes the walk visited vs draws it emitted after visibility
    // clipping (pre-scissor). These used to both be `draws.len()`.
    telemetry.record_scene_entries(built.stats.nodes_visited, built.stats.draws_emitted);
    telemetry.record_visibility(
        [
            built.stats.collapses_mine,
            built.stats.collapses_claim,
            built.stats.collapses_taken,
            built.stats.collapses_taken_skipped,
        ],
        built.stats.hidden_participants,
    );
    telemetry.record_content_damage(
        built.stats.content_hidden,
        built.stats.content_off_output,
        built.stats.content_other_output,
    );

    // 3. Empty-damage fast path (after first frame).
    if skip_for_empty_damage(
        output_damage.is_empty(),
        first_frame,
        inner.outputs[output_idx].damage.owes_repaint(),
    ) {
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
                "empty-damage-diag: out{output_idx} draws={} carry={} hidden={} off_output={} \
                 snapshots={:?}",
                built.scene.draws.len(),
                snapshots_carry_damage(&built.snapshots),
                built.stats.content_hidden,
                built.stats.content_off_output,
                built
                    .snapshots
                    .iter()
                    .map(|s| (s.id.as_u64(), s.region.rects().len()))
                    .collect::<Vec<_>>(),
            );
        }
        // Stage C — only damage that projected entirely OFF the output forces
        // a compose here. Damage that landed on the output but under a cover
        // (`ContentDamage::Hidden`) skips like clean idle: nothing on screen
        // changed, and the snapshot is re-peeked next walk. See
        // `ContentDamage` for why it is not acked either.
        if !built.stats.off_output_damage_forces_compose() {
            // Audit on the empty-damage path when a transition woke the
            // scene and reported nothing (the archetype bug), OR when the
            // idle re-compare is due. Without the second condition a
            // quiet desktop is never checked at all, so an unhealed
            // divergence would silently stop being reported — a clean
            // static soak would then mean nothing.
            if audit_has_unretired_event(inner, output_idx)
                || audit_idle_recompare_due(inner, output_idx)
            {
                let layout = &platform.outputs[output_idx];
                let overlay_ops = root_overlay.apply_list_for_output((
                    layout.x,
                    layout.y,
                    u32::from(layout.width),
                    u32::from(layout.height),
                ));
                let (xor_pipeline, xor_layout) =
                    audit_overlay_pipeline(inner, !overlay_ops.is_empty())?;
                let reference = audit_reference_scene(
                    built.software_cursor_tail.is_some(),
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
                run_damage_audit(
                    inner,
                    output_idx,
                    platform,
                    &built.scene,
                    reference.as_ref().map_or(&built.scene, |r| &r.scene),
                    &audit_sampled_pairs(store, &built.sampled_ids),
                    &output_damage,
                    None,
                    true,
                    &overlay_ops,
                    xor_pipeline,
                    xor_layout,
                )?;
            }
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

    // 3b. Step 3 — attribute this frame's damage.
    //
    // Placed HERE and not where `output_damage` is first assembled, because the
    // empty-damage block above can still inject a full-output rect when a
    // drawable carries real damage whose projection landed empty (the xfce
    // submenu case). Feeding the model before that would drop the injection.
    //
    // Shared outputs only: a copied (reverse-PRIME) output renders
    // `Repaint::Full` unconditionally and never consults this state, so it must
    // not accumulate any either.
    let shared_output = matches!(
        platform
            .scanout_pools
            .get(output_idx)
            .and_then(Option::as_ref),
        Some(OutputScanout::Shared(_))
    );
    if shared_output {
        let mut damage_region = Region::from_rects(output_damage.rects().iter().copied());
        // Clip to the output. Damage outside it cannot be presented, and letting
        // it through would trip `commit_submitted`'s "painted covers repaint"
        // assertion — which compares against the full-output rect — turning a
        // stray rect from some producer into a debug-build panic on hardware.
        damage_region.intersect_rect(vk::Rect2D {
            offset: vk::Offset2D::default(),
            extent: inner.outputs[output_idx].output_extent,
        });
        inner.outputs[output_idx].damage.add_damage(&damage_region);
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

    // 4b. Step 3 — what this BO is missing. Pure: acquiring mutates nothing, so
    // any later skip or failure leaves the model exactly as it was and the next
    // tick recomputes the same answer.
    //
    // `loadable` is the same condition `Repaint::Clipped` needs for a valid
    // `loadOp = LOAD`: the BO must have been through a present and not been
    // invalidated since. When false everything is missing, and step 4 must also
    // render Full — loading from a never-presented BO is invalid, not just stale.
    let bo_loadable = !token.content_invalidated && token.last_present_generation.is_some();
    let bo_repaint = if shared_output {
        inner.outputs[output_idx]
            .damage
            .repaint_for(token.bo_idx, bo_loadable)
    } else {
        Region::new()
    };

    // 5. Step 4 — decide how to repaint, and what that will paint.
    let extent = inner.outputs[output_idx].output_extent;

    // Two producers must be folded into the region before the decision: neither
    // is expressed as damage, and both are wrong under a scissor that misses
    // them.
    let mut requested = bo_repaint.clone();

    // The root `IncludeInferiors` XOR overlay is NOT idempotent. It is correct
    // today only because `Repaint::Full` CLEARs and fully redraws the BO, so the
    // overlay XORs exactly once onto fresh pixels. A clipped `loadOp = LOAD`
    // frame whose scissor misses the overlay rects would XOR them a SECOND time
    // onto a pooled BO that already has them baked in from a prior compose,
    // cancelling them or leaving remnants — the #90 rubber-band residual.
    for (_, rect) in root_overlay.apply_list_for_output((
        platform.outputs[output_idx].x,
        platform.outputs[output_idx].y,
        u32::from(platform.outputs[output_idx].width),
        u32::from(platform.outputs[output_idx].height),
    )) {
        requested.add_rect(rect);
    }

    // A stationary software cursor lives only in the BO that last drew it, so it
    // must be repainted even on a frame triggered by unrelated damage. Keyed off
    // the draw list rather than off the cursor assignment, which avoids two
    // mistakes: `new_cursor_rect` is `Some` for a HW-plane cursor too (that rect
    // is plane content, not BO content, and folding it would repaint a region on
    // every cursor move on the very path that exists to avoid that), and
    // `omit_software_cursor_for_hide` strips the SW draw for the Hw->Sw handoff
    // frame after the assignment was computed.
    if built.software_cursor_tail.is_some()
        && let Some(cursor_rect) = built.new_cursor_rect
    {
        requested.add_rect(cursor_rect);
    }
    requested.intersect_rect(vk::Rect2D {
        offset: vk::Offset2D::default(),
        extent,
    });

    let plan = plan_repaint(
        &requested,
        &built.scene.draws,
        extent,
        bo_loadable,
        shared_output,
    );
    let repaint = plan.repaint;

    // The culled draw list is a SEPARATE product; `built.scene` stays whole for
    // the snapshots and the audit oracle. See `cull_scene_to_rect`.
    let culled = match repaint {
        Repaint::Clipped(_) => Some(cull_scene_to_region(&built.scene, &plan.painted)),
        Repaint::Full(_) | Repaint::AuditClearClipped(_) => None,
    };
    let render_scene: &CompositeScene = culled.as_ref().unwrap_or(&built.scene);
    if let (Repaint::Clipped(_), Some(c)) = (repaint, culled.as_ref()) {
        // Per scissor rect, not once against the bbox: with 4.5's per-rect
        // rendering different rects are legitimately covered by different
        // draws, and once step 1 fragments the root no single draw covers
        // anything that straddles a window edge.
        debug_assert!(
            plan.scissors
                .iter()
                .all(|r| opaque_cover_exists(&c.draws, *r)),
            "culling removed an opaque draw the clipped path depends on"
        );
    }

    match plan.full_reason {
        Some(reason) => {
            telemetry.record_full_redraw_fallback();
            telemetry.record_full_reason(match reason {
                FullReason::EmptyDrawList => "empty_draws",
                FullReason::UnloadableBo => "unloadable_bo",
                FullReason::NoOpaqueCover => "no_opaque_cover",
                FullReason::Threshold => "threshold",
                FullReason::CopiedRoute => "copied_route",
            });
        }
        None => telemetry.record_clipped_repaint(),
    }
    // `damage_fraction` now reports what was actually rasterised, which is the
    // number that tracks GPU cost. The requested-region area is reported
    // separately, and the gap between them is bbox waste — the input to the
    // multi-rect decision in 4.5.
    telemetry.record_damage_pixels(
        plan.painted.area(),
        u64::from(extent.width) * u64::from(extent.height),
    );
    telemetry.record_damage_region_pixels(requested.area());
    // Same denominator as `damage_fraction`: one output area per compose.
    telemetry.record_scene_draw_pixels(scene_draw_pixels);

    let layout = &platform.outputs[output_idx];
    let overlay_ops = root_overlay.apply_list_for_output((
        layout.x,
        layout.y,
        u32::from(layout.width),
        u32::from(layout.height),
    ));
    let (xor_pipeline, xor_layout) = audit_overlay_pipeline(inner, !overlay_ops.is_empty())?;
    let reference = audit_reference_scene(
        built.software_cursor_tail.is_some(),
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
    run_damage_audit(
        inner,
        output_idx,
        platform,
        &built.scene,
        reference.as_ref().map_or(&built.scene, |r| &r.scene),
        &audit_sampled_pairs(store, &built.sampled_ids),
        &output_damage,
        None,
        false,
        &overlay_ops,
        xor_pipeline,
        xor_layout,
    )?;

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
    // Retained root-`IncludeInferiors` overlay: per-output apply list
    // (output-local XOR rects), computed against the SAME per-output
    // layout the compose uses. Empty in the common case (no active
    // wireframe / rubber-band), so no XOR pipeline is built.
    let layout = &platform.outputs[output_idx];
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
    // Step 1 — whether every draw of `render_scene` was actually recorded.
    // Descriptor allocation `break`s on pool exhaustion and the recorder draws
    // only the allocated prefix; on the clipped path a frame that painted less
    // than it claims must not be staged as `painted`. The copied route renders
    // Full and re-clears every frame, so only the shared path reports it.
    let mut compose_complete = true;
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
                render_scene,
                repaint,
                &plan.scissors,
                compose_ticket.fence(),
                &mut gpu_submitted,
                &overlay_ops,
                xor_pipeline,
                xor_layout,
            )
            .map(|submitted| {
                compose_complete = compose_submit_was_complete(submitted, render_scene.draws.len());
                None
            });
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
                render_scene,
                Repaint::Full(token.extent),
                &[],
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
        .record_descriptor_allocations(u64::try_from(render_scene.draws.len()).unwrap_or(u64::MAX));

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
                submitted_participants: built.participants,
                submitted_scene_structure_damage: scene_structure_snap,
                submitted_failed_repaint: failed_repaint_snap,
                cursor_transition: cursor_transition_to_queue,
                cursor_prev_pos_after_retire,
                cursor_mode_after_retire,
                last_present_cursor_rect_after_retire: built.new_cursor_rect,
                last_present_cursor_version_after_retire: built.cursor_record_version,
            });
            state.current_generation = frame_gen;
            // Step 3 — stage the frame that just succeeded. Deliberately here
            // and not at submit *attempt*: an attempt that failed never staged,
            // so `pending` was never taken and the next tick recomputes an
            // identical repaint with nothing to roll back.
            //
            // `painted` is the whole output because `pick_repaint_region` still
            // returns `Repaint::Full`; step 4 replaces it with what the recorder
            // actually covered. It must always be a superset of `bo_repaint` —
            // `commit_submitted` asserts exactly that.
            if shared_output {
                stage_submitted_frame(
                    &mut state.damage,
                    compose_complete,
                    token.bo_idx,
                    &requested,
                    &plan.painted,
                );
                if !compose_complete {
                    // Once per output rather than per frame: a scene that
                    // overflows the pool does so every Full frame.
                    static WARNED: std::sync::atomic::AtomicU32 =
                        std::sync::atomic::AtomicU32::new(0);
                    let bit = 1u32 << (output_idx % 32);
                    if WARNED.fetch_or(bit, std::sync::atomic::Ordering::Relaxed) & bit == 0 {
                        log::warn!(
                            "render scene: output {output_idx} composed {} of {} draws \
                             (descriptor pool exhausted); BO state invalidated, next \
                             frame repaints in full",
                            render_scene
                                .draws
                                .len()
                                .min(MAX_DESCRIPTOR_SETS_PER_FRAME as usize),
                            render_scene.draws.len(),
                        );
                    }
                }
            }
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

/// Why a frame fell back to a full-output repaint. Counted per reason, because
/// "clipped repaint is not helping" and "clipped repaint is being rejected" look
/// identical in a `full_redraw_fallback` count and want completely different
/// fixes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FullReason {
    /// Scene is just the background clear.
    EmptyDrawList,
    /// BO never presented, or its contents were invalidated: `loadOp = LOAD`
    /// would be invalid, not merely stale.
    UnloadableBo,
    /// No opaque draw covers the region, so `loadOp = LOAD` would leave whatever
    /// the previous compose of this BO left behind showing through.
    NoOpaqueCover,
    /// Clipping costs more than it saves above this fraction of the output.
    Threshold,
    /// Copied (reverse-PRIME) route: always Full, never tracked.
    CopiedRoute,
}

/// The step-4 decision: how to render, and what that will actually paint.
struct RepaintPlan {
    repaint: Repaint,
    /// Scissor rects to render under. One (the bounding box) in the common
    /// case; the damage region's own rects when the bbox wastes enough to be
    /// worth the extra draw calls — see [`MULTI_RECT_MIN_GAIN`].
    ///
    /// Disjoint by construction, because they come from a canonical `Region`.
    /// That is load-bearing for the root XOR overlay, which is not idempotent:
    /// each overlay pixel must fall in exactly one scissor.
    scissors: Vec<vk::Rect2D>,
    /// **What the recorder will cover** — not what was asked for. The bounding
    /// box under clipped rendering, the whole output under Full. Staged on the
    /// frame's damage transaction, and always a superset of the requested
    /// region: recording a frame as having painted more than it drew clears
    /// `missing` for pixels that were never touched.
    painted: Region,
    full_reason: Option<FullReason>,
}

/// Render per damage rect rather than under one bounding box once the box wastes
/// this much.
///
/// Measured on silence with the phased workload: bounding-box waste is 0% while
/// idle, 19% resizing and **36% dragging** — a moved window's damage is exactly
/// two disjoint rects, old and new, and their box spans both plus the empty gap
/// between them. 1.5 sits below the 1.57 the drag phase produces and well above
/// the 1.0 of a single contiguous rect.
///
/// An earlier reading of whole-session medians put the waste at 0.5-1.8% and
/// concluded this was not worth building. That average was dominated by idle
/// frames; a median over a whole session cannot answer a question about one kind
/// of frame.
const MULTI_RECT_MIN_GAIN: f64 = 1.5;

/// Cap on scissor rects per frame.
///
/// Set to the region's own rect cap, which means it never binds: a `Region`
/// collapses to its extents above [`Region::MAX_RECTS`], so the list handed here
/// is already bounded.
///
/// It was 8, on the reasoning that each scissor re-issues every draw that
/// intersects it and the draw-call count would explode. **That reasoning was
/// wrong, and measurably so.** The count that matters is the *post-cull* draw
/// count, and on MATE that is 4.0 per compose against 53.6 pre-cull — the
/// scissor cull removes 92% of draws because damage is a small fraction of the
/// screen and most windows do not intersect it. So the cost is scissors × ~4,
/// not scissors × ~53.
///
/// The 8-rect cap cost real work: on MATE the panels and desktop fragment a drag
/// region past 8, so it fell back to the bounding box and 34% of the painted
/// area was the empty gap between a window's old and new position — the exact
/// waste 4.5 exists to remove, reappearing on the more realistic desktop while
/// the tiling-WM measurement looked fine.
const MAX_SCISSOR_RECTS: usize = Region::MAX_RECTS;

/// Above this fraction of the output, clipping costs more than it saves.
///
/// Measured on bee: at a damage fraction of 0.857 a clipped compose cost
/// 208.7 µs against 199.3 µs for a full one — a sub-rect pass still pays scissor
/// setup and every draw call, so only fragment work shrinks. Without the
/// threshold, clipped repaint is a net loss on exactly the frames that are
/// whole-output today.
///
/// Applied to the area that will be **painted** (the bounding box), never to the
/// damage region: sparse damage spread across the screen has a small region and
/// a near-full bbox, and thresholding on the region would pick the clipped path
/// and then rasterise almost everything anyway, with the LOAD and scissor
/// overhead on top.
///
/// The bee number is a fast GPU; re-measure the crossover on the z400 and adjust
/// once. A constant, deliberately not an environment variable.
const CLIPPED_REPAINT_MAX_FRACTION: f64 = 0.6;

/// A draw's destination rect, rounded **inward**.
///
/// `dst_origin`/`dst_size` are `f32`. Rounding the origin up and the far edge
/// down means a fractional edge never counts as covered, so the opaque-cover
/// guard can only ever be conservative.
fn draw_dst_rect_inward(draw: &CompositeDraw) -> Option<vk::Rect2D> {
    let x0 = draw.dst_origin[0].ceil();
    let y0 = draw.dst_origin[1].ceil();
    let x1 = (draw.dst_origin[0] + draw.dst_size[0]).floor();
    let y1 = (draw.dst_origin[1] + draw.dst_size[1]).floor();
    if !(x0.is_finite() && y0.is_finite() && x1.is_finite() && y1.is_finite())
        || x1 <= x0
        || y1 <= y0
    {
        return None;
    }
    #[allow(clippy::cast_possible_truncation)]
    Some(vk::Rect2D {
        offset: vk::Offset2D {
            x: x0 as i32,
            y: y0 as i32,
        },
        extent: vk::Extent2D {
            width: (x1 - x0) as u32,
            height: (y1 - y0) as u32,
        },
    })
}

/// True if the **opaque** draws together cover every pixel of `rect`.
///
/// This is what makes `loadOp = LOAD` + scissor equal a full compose inside the
/// region, and it is why step 4 needs no background algebra: yserver already
/// draws an opaque bottom layer. `alpha_passthrough == false` selects the
/// force-opaque pipeline variant, whose fragment shader sets `src.a = 1` against
/// `ONE / ONE_MINUS_SRC_ALPHA` blending, so such a draw fully overwrites the
/// destination and whatever the previous compose left is irrelevant.
///
/// A **union** of draws, not a single one, because step 1 clips the root to
/// `output − opaque windows`: a repaint rect that straddles a window edge is
/// then covered by the root fragment on one side and the window on the other,
/// and no single draw contains it. Computed by subtracting each opaque draw from
/// a remainder rather than by unioning the draws and testing containment: the
/// 32-box cap collapses a `Region` to its bounding box, and a collapsed
/// *remainder* is a superset (⇒ "not covered" ⇒ Full, safe) while a collapsed
/// *union* claims coverage it does not have — the defect that broke the naive
/// occlusion cull (`findings/2026-09-03-naive-occlusion-cull-postmortem.md`).
///
/// Note what is never an opaque bottom layer: every COW-subtree draw is
/// `alpha_passthrough = true` by construction, and so is the software cursor. A
/// compositing desktop therefore usually fails this gate and renders Full —
/// which is correct and costs nothing, because a compositor presents a
/// full-screen surface every frame regardless.
fn opaque_cover_exists(draws: &[CompositeDraw], rect: vk::Rect2D) -> bool {
    let mut remainder = Region::from_rect(rect);
    for d in draws {
        if remainder.is_empty() {
            break;
        }
        if d.alpha_passthrough {
            continue;
        }
        if let Some(dst) = draw_dst_rect_inward(d)
            && rects_intersect(dst, rect)
        {
            remainder.subtract(&Region::from_rect(dst));
        }
    }
    remainder.is_empty()
}

/// Decide how to repaint, and report what that will paint.
///
/// Every gate here is a documented way to corrupt the screen under
/// `loadOp = LOAD`; each one falls back to Full rather than trying to be clever.
fn plan_repaint(
    requested: &Region,
    draws: &[CompositeDraw],
    extent: vk::Extent2D,
    loadable: bool,
    shared_route: bool,
) -> RepaintPlan {
    let full = |reason: FullReason| {
        let whole = vk::Rect2D {
            offset: vk::Offset2D::default(),
            extent,
        };
        RepaintPlan {
            repaint: Repaint::Full(extent),
            scissors: vec![whole],
            painted: Region::from_rect(whole),
            full_reason: Some(reason),
        }
    };

    if !shared_route {
        return full(FullReason::CopiedRoute);
    }
    if draws.is_empty() {
        // The Clipped/LOAD path would preserve each BO's prior-generation
        // content — including a pre-`bg_pixel`-update black — and never
        // re-clear it. Full so `loadOp = CLEAR` paints the current `bg_color`
        // across the whole BO.
        return full(FullReason::EmptyDrawList);
    }
    if !loadable {
        return full(FullReason::UnloadableBo);
    }
    let Some(bbox) = requested.bounding_rect() else {
        // Nothing to paint at all. Conservative rather than clever: a degenerate
        // case should not be the one path with bespoke handling.
        return full(FullReason::EmptyDrawList);
    };

    // Per-rect or bounding box? Decided before the threshold, because per-rect
    // paints less and so keeps frames on the clipped path that a box would push
    // over the line.
    let rects: Vec<vk::Rect2D> = requested.rects().collect();
    let bbox_area = u64::from(bbox.extent.width) * u64::from(bbox.extent.height);
    #[allow(clippy::cast_precision_loss)]
    let wasteful =
        requested.area() > 0 && bbox_area as f64 > MULTI_RECT_MIN_GAIN * requested.area() as f64;
    let (scissors, painted) = if rects.len() > 1 && rects.len() <= MAX_SCISSOR_RECTS && wasteful {
        (rects, requested.clone())
    } else {
        (vec![bbox], Region::from_rect(bbox))
    };

    let output_area = u64::from(extent.width) * u64::from(extent.height);
    #[allow(clippy::cast_precision_loss)]
    let fraction = if output_area == 0 {
        1.0
    } else {
        painted.area() as f64 / output_area as f64
    };
    if fraction >= CLIPPED_REPAINT_MAX_FRACTION {
        return full(FullReason::Threshold);
    }

    // Every pixel that will be painted needs some opaque draw over it, so the
    // check is per scissor: different rects may legitimately be covered by
    // different draws.
    if !scissors.iter().all(|r| opaque_cover_exists(draws, *r)) {
        return full(FullReason::NoOpaqueCover);
    }

    RepaintPlan {
        repaint: Repaint::Clipped(bbox),
        scissors,
        painted,
        full_reason: None,
    }
}

/// The draws that intersect `rect`, in unchanged order.
///
/// Culling before descriptor allocation is where the per-compose CPU floor comes
/// down: the audit's fit put 40-110 µs per compose in draw calls, descriptor
/// binds and pipeline switches that clipping fragment work alone never removes.
///
/// **The result is a separate product and must never replace `built.scene`.**
/// The full list is what the drawable snapshots, the audit's reference oracle
/// and (from step 2) the previous-frame scene diff all read. Recording the culled
/// list as the frame's scene would make every culled draw read as "disappeared"
/// next frame, manufacturing structural damage over all of them — the screen
/// would look perfectly correct while the entire saving evaporated.
fn cull_scene_to_region(scene: &CompositeScene, keep: &Region) -> CompositeScene {
    CompositeScene {
        bg_color: scene.bg_color,
        draws: scene
            .draws
            .iter()
            .filter(|d| draw_dst_rect_inward(d).is_none_or(|dst| keep.intersects_rect(dst)))
            .copied()
            .collect(),
    }
}

#[derive(Debug, Clone, Copy)]
enum Repaint {
    /// Full-output redraw with `loadOp=CLEAR`. Fallback path.
    Full(vk::Extent2D),
    /// Damaged-region-only redraw with `loadOp=LOAD`. The
    /// rectangle is the bounding box of the buffer-age repaint
    /// set — Stage 5 may split per-rect for tighter clipping.
    Clipped(vk::Rect2D),
    /// Diagnostic-only damaged-region redraw with `loadOp=CLEAR` and
    /// `renderArea` clipped to the same rect. This models the future
    /// canonical image update rule without changing production scanout.
    AuditClearClipped(vk::Rect2D),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ComposeSubmit {
    descriptor_count: usize,
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
    cursor_prev_pos: Option<(i32, i32)>,
    cow_host_xid: Option<u32>,
    hw_strategy_active: bool,
    mode: Visibility,
) -> SceneBuild {
    // No knowledge of other outputs: every off-output paint is treated as
    // stranded (forced and acked here), which is the single-output rule.
    let nowhere = std::collections::HashSet::new();
    build_scene_with(
        core,
        store,
        windows,
        output_idx,
        platform,
        cursor,
        cursor_prev_pos,
        cow_host_xid,
        hw_strategy_active,
        mode,
        &nowhere,
    )
}

/// [`build_scene`] with knowledge of what the OTHER outputs showed at their
/// last walk, so an off-output paint that another output presents is classified
/// `ContentDamage::OtherOutput` and left to that output. See [`WalkSink::elsewhere`].
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)]
fn build_scene_with(
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
    // Step 1 — clip each node to what nothing above it covers (`On`), or emit
    // every node's full placement as before step 1 (`Off`). Production passes
    // `On`; the damage audit's reference and the tests use `Off`.
    mode: Visibility,
    elsewhere: &std::collections::HashSet<super::store::DrawableId>,
) -> SceneBuild {
    let bg = [0.0, 0.0, 0.0, 1.0];
    let layout = &platform.outputs[output_idx];
    let layout_x0 = layout.x;
    let layout_y0 = layout.y;
    let layout_w = u32::from(layout.width);
    let layout_h = u32::from(layout.height);

    let mut sink = WalkSink::new(output_idx, elsewhere);
    // Stage 4c.3 — the root samples through `redirected_target` like any other
    // node; geometry stays the host drawable's. Decided up front, emitted last
    // (see below).
    let root = root_node(core, store, layout_x0, layout_y0, layout_w, layout_h, mode);
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
         layout=({layout_x0},{layout_y0} {layout_w}x{layout_h}) mode={mode:?}",
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
    //
    // Step 1 does NOT replace this. The fullscreen window is *below* the COW in
    // stacking and occludes it only because the compositor unredirected it —
    // not an occlusion the tree can express. The probe and its filter matrix
    // stay exactly as they are.
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
    let children = children_index(windows);
    // Step 1 — the universe for the root's children is the output: X11 clips
    // top-levels to the screen. Under `Off` the region is never read.
    let mut universe = match mode {
        Visibility::On => Region::from_rect(vk::Rect2D {
            offset: vk::Offset2D::default(),
            extent: vk::Extent2D {
                width: layout_w,
                height: layout_h,
            },
        }),
        Visibility::Off => Region::new(),
    };
    // Computation order is top → bottom (`miComputeClips` visits the topmost
    // sibling first); the sink is reversed into painter's order below.
    for &top_xid in core.top_level_order.iter().rev() {
        if suppress_cow && Some(top_xid) == cow_host_xid {
            continue;
        }
        visit_window_subtree(
            top_xid,
            0,
            0,
            store,
            windows,
            &children,
            &core.shape_bounding,
            layout_x0,
            layout_y0,
            layout_w,
            layout_h,
            mode,
            &mut universe,
            &mut sink,
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
    // The root is the last node in computation order — the bottom of the
    // stack — so it lands first after the reversal, exactly where the old
    // emitter pushed it. Under `On` it gets what the top-levels left.
    if let Some(root) = root {
        sink.stats.nodes_visited += 1;
        let out = emit_node(
            &mut sink,
            mode,
            &universe,
            &root.place,
            root.dx,
            root.dy,
            root.denom_w,
            root.denom_h,
            root.view,
            false,
            root.source_id,
            store,
            layout_w,
            layout_h,
        );
        log::trace!(
            "render scene_walk root output={output_idx}: place={} pieces={}",
            root.place.len(),
            out.emitted,
        );
        push_presence(
            &mut sink,
            root.place,
            out,
            ParticipantId {
                role: SceneRole::Root,
                xid: core.window_id,
                generation: root.id.as_u64(),
            },
        );
    }
    sink.reverse();
    let WalkSink {
        output_idx: _,
        elsewhere: _,
        mut draws,
        snapshots,
        mut sampled_ids,
        projected,
        participants,
        stats,
        pieces: _,
        mut presented_ids,
        mut pieces_ids,
    } = sink;
    log::trace!(
        "render scene_walk end output={output_idx} draws={n_draws} \
         sampled={n_sampled} nodes={nodes} hidden={hidden} collapses={collapses}",
        n_draws = draws.len(),
        n_sampled = sampled_ids.len(),
        nodes = stats.nodes_visited,
        hidden = stats.hidden_participants,
        collapses = stats.collapses(),
    );

    // Stage 5 Phase C: pure cursor strategy decision. `build_scene`
    // decides visibility + HW/SW assignment and reports the current
    // clipped footprint, but it does NOT emit cursor damage. The
    // tick owns that decision because it also owns the transactional
    // "last successfully presented cursor footprint/version" state.
    //
    // Appended AFTER the reversal so `software_cursor_tail` keeps pointing at
    // the last draw / sampled id.
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
                presented_ids.push(cur.id);
                pieces_ids.push(cur.id);
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
        participants,
        stats,
        presented_ids,
        pieces_ids,
    }
}

/// The root as a node of the walk: its place is its storage rect at the
/// output's origin, its children are the top-levels, its visible region is what
/// they leave. Emitted last in computation order (= first after the reversal),
/// which is where the old emitter pushed it.
struct RootNode {
    id: super::store::DrawableId,
    source_id: super::store::DrawableId,
    view: vk::ImageView,
    /// Output-local rect(s) the root occupies — under `On` clipped to the
    /// output, under `Off` the storage rect as the old emitter drew it.
    place: Vec<vk::Rect2D>,
    /// Output-local origin of the root's storage.
    dx: i32,
    dy: i32,
    /// `src` denominators: the sampled source's extent under `On`, the host
    /// storage extent under `Off` (what the old emitter divided by; equal
    /// unless the root is redirected to a backing of a different size).
    denom_w: i32,
    denom_h: i32,
}

fn root_node(
    core: &KmsCore,
    store: &DrawableStore,
    layout_x0: i32,
    layout_y0: i32,
    layout_w: u32,
    layout_h: u32,
    mode: Visibility,
) -> Option<RootNode> {
    let id = store.lookup(core.window_id)?;
    let drawable = store.get(id)?;
    if !drawable.scene_participating || !matches!(drawable.kind, DrawableKind::Root) {
        return None;
    }
    // Stage 4c.3 — route source-storage through `redirected_target`.
    // For an Automatic-mode redirected drawable, the scene must
    // blit FROM the backing B (not the drawable's own storage).
    // Geometry stays driven by the host drawable; only the
    // sampled storage handle reroutes.
    let source_id = store.redirected_target(id).unwrap_or(id);
    let source = store.get(source_id)?;
    if source.storage.image_view == vk::ImageView::null() {
        return None;
    }
    let dx = -layout_x0;
    let dy = -layout_y0;
    let host = drawable.storage.extent;
    let full = vk::Rect2D {
        offset: vk::Offset2D { x: dx, y: dy },
        extent: host,
    };
    let (place, denom) = match mode {
        Visibility::Off => (vec![full], host),
        Visibility::On => (
            clip_rect_to_output_extent(
                full,
                vk::Extent2D {
                    width: layout_w,
                    height: layout_h,
                },
            )
            .into_iter()
            .collect(),
            source.storage.extent,
        ),
    };
    Some(RootNode {
        id,
        source_id,
        // Root scene draw — sample-side view carries the
        // format/depth-aware swizzle (depth-24 → α=ONE).
        // See `Storage::sample_view` for why scene draws
        // MUST NOT bind `image_view` directly.
        view: source.storage.sample_view,
        place,
        dx,
        dy,
        denom_w: i32::try_from(denom.width).unwrap_or(i32::MAX),
        denom_h: i32::try_from(denom.height).unwrap_or(i32::MAX),
    })
}

/// One quad for one output-local piece of a node.
///
/// `src` is derived by translating the piece back into the node's own pixels
/// (`piece − (dx, dy)`) and dividing by the sampled source's extent — so a
/// piece of a straddling window, or of a window on an output with a non-zero
/// layout origin, samples exactly the texels the unclipped draw would have.
/// Same arithmetic, same casts, as the pre-step-1 emitter, so `Visibility::Off`
/// reproduces it bit for bit.
fn piece_draw(
    piece: vk::Rect2D,
    dx: i32,
    dy: i32,
    denom_w: i32,
    denom_h: i32,
    view: vk::ImageView,
    alpha_passthrough: bool,
) -> CompositeDraw {
    let cx = piece.offset.x - dx;
    let cy = piece.offset.y - dy;
    let cw = i32::try_from(piece.extent.width).unwrap_or(i32::MAX);
    let ch = i32::try_from(piece.extent.height).unwrap_or(i32::MAX);
    #[allow(clippy::cast_precision_loss)]
    let (cw_f, ch_f, cx_f, cy_f, dw_f, dh_f) = (
        cw as f32,
        ch as f32,
        cx as f32,
        cy as f32,
        denom_w as f32,
        denom_h as f32,
    );
    CompositeDraw {
        image_view: view,
        #[allow(clippy::cast_precision_loss)]
        dst_origin: [(dx + cx) as f32, (dy + cy) as f32],
        dst_size: [cw_f, ch_f],
        src_origin: [cx_f / dw_f, cy_f / dh_f],
        src_size: [cw_f / dw_f, ch_f / dh_f],
        alpha_passthrough,
    }
}

fn union_bbox(a: vk::Rect2D, b: vk::Rect2D) -> vk::Rect2D {
    let x0 = a.offset.x.min(b.offset.x);
    let y0 = a.offset.y.min(b.offset.y);
    let x1 = (a.offset.x.saturating_add_unsigned(a.extent.width))
        .max(b.offset.x.saturating_add_unsigned(b.extent.width));
    let y1 = (a.offset.y.saturating_add_unsigned(a.extent.height))
        .max(b.offset.y.saturating_add_unsigned(b.extent.height));
    vk::Rect2D {
        offset: vk::Offset2D { x: x0, y: y0 },
        extent: vk::Extent2D {
            width: u32::try_from(x1 - x0).unwrap_or(0),
            height: u32::try_from(y1 - y0).unwrap_or(0),
        },
    }
}

/// Emit one node that passed every gate: its draws (clipped to `mine` under
/// `On`), its sampled id, its presence and its damage snapshot. Returns the
/// number of draws pushed — zero for a node something above covers entirely,
/// which is **still a participant** (see `ScenePresence`).
///
/// Draws for the node's place rects are pushed in reverse so that the sink's
/// final reversal restores place order, which is what makes `Off` byte-identical
/// to the old emitter.
#[allow(clippy::too_many_arguments)]
fn emit_node(
    sink: &mut WalkSink<'_>,
    mode: Visibility,
    mine: &Region,
    place: &[vk::Rect2D],
    dx: i32,
    dy: i32,
    denom_w: i32,
    denom_h: i32,
    view: vk::ImageView,
    alpha_passthrough: bool,
    source_id: super::store::DrawableId,
    store: &DrawableStore,
    layout_w: u32,
    layout_h: u32,
) -> Emitted {
    let mut emitted = 0u64;
    let mut visible_bbox: Option<vk::Rect2D> = None;
    // The exact visible pieces this node emits, for clipping its content
    // damage below. Disjoint (each is `mine ∩ r` for a distinct place rect, and
    // the place rects are disjoint), so per-piece intersection sums exactly.
    sink.pieces.clear();
    for r in place.iter().rev() {
        match mode {
            Visibility::Off => {
                sink.draws.push(piece_draw(
                    *r,
                    dx,
                    dy,
                    denom_w,
                    denom_h,
                    view,
                    alpha_passthrough,
                ));
                emitted += 1;
            }
            Visibility::On => {
                // Per place rect, so a piece never leaves its own shape rect
                // even when `mine` is a capped superset.
                let vis = mine.clip_to_rect(*r);
                for piece in vis.rects() {
                    sink.draws.push(piece_draw(
                        piece,
                        dx,
                        dy,
                        denom_w,
                        denom_h,
                        view,
                        alpha_passthrough,
                    ));
                    emitted += 1;
                    sink.pieces.push(piece);
                    // `visible` on the presence is a damage-side summary where
                    // a superset is safe — the bounding box of the pieces, not
                    // their union, which would cost a `combine` per piece on
                    // the hot path. Content damage is clipped to the exact
                    // `pieces` instead.
                    visible_bbox = Some(match visible_bbox {
                        None => piece,
                        Some(acc) => union_bbox(acc, piece),
                    });
                }
            }
        }
    }
    let visible = match mode {
        Visibility::Off => Region::from_rects(place.iter().copied()),
        Visibility::On => visible_bbox.map_or_else(Region::new, Region::from_rect),
    };
    sink.stats.draws_emitted += emitted;
    if emitted == 0 {
        sink.stats.hidden_participants += 1;
    } else {
        sink.pieces_ids.push(source_id);
    }
    sink.sampled_ids.push(source_id);
    // Signature from the first PLACE rect — what the unclipped draw would
    // carry — never from an emitted piece, whose src moves whenever the cover
    // above moves and would read as a resample every frame. The presence itself
    // is pushed by the caller once the claim step is done with `place`
    // (`push_presence`), so the decision's rect list moves into it uncopied.
    let signature = place.first().map(|first| {
        let unclipped = piece_draw(*first, dx, dy, denom_w, denom_h, view, alpha_passthrough);
        PresenceSignature::new(
            unclipped.image_view,
            unclipped.src_origin,
            unclipped.src_size,
            unclipped.alpha_passthrough,
        )
    });
    if let Some(snap) = store.peek_presentation_damage(source_id) {
        // Stage C — project the captured damage onto the output and, under
        // `On`, keep only what lands on this node's visible pieces: a paint
        // into the covered part of a window cannot have changed a pixel on
        // screen. `Off` keeps the unclipped projection so the audit's reference
        // damages what it always did.
        let mut on_output = false;
        let mut added = false;
        for r in snap.region.rects() {
            let Some(proj) = project_onto_output(*r, dx, dy, layout_w, layout_h) else {
                continue;
            };
            on_output = true;
            match mode {
                Visibility::Off => {
                    sink.projected.add(proj);
                    added = true;
                }
                Visibility::On => {
                    for piece in &sink.pieces {
                        if let Some(hit) = intersect_rects(proj, *piece) {
                            sink.projected.add(hit);
                            added = true;
                        }
                    }
                }
            }
        }
        // Presented, and carried into this output's PendingAck, ONLY for
        // `Visible` — damage that reached the screen here — plus a node with no
        // damage at all, which has nothing to present. `Hidden`, `OtherOutput`
        // and `OffOutput` are none of those: the snapshot stays in the store for
        // the walk that does present it. Acking a snapshot this output did not
        // present is the multi-output ack race, and it must not depend on
        // cross-output knowledge to avoid: `OffOutput` used to be carried on the
        // theory that it is stranded everywhere, but "everywhere" was decided
        // from `elsewhere`, which is one walk stale. Measured 2026-09-04 on
        // silence/MATE: the caja desktop spans both outputs, output 1 classified
        // its rubberband-erase damage `OffOutput` 104× while output 0's set was
        // cold, acked it, and left the selection on output 0's scanout —
        // 260 audit mismatches, 247 unhealed. `OffOutput` still FORCES a compose
        // here (the xfce submenu case: a paint whose projection is empty must
        // not sit undrained), but the drain now comes from dormancy — the
        // snapshot is not presented, so reconciliation makes it dormant and it
        // stops re-forcing until its next paint. See [`ContentDamage`] and
        // `WalkSink::presented_ids`.
        let mut presented = true;
        let mut carry = true;
        if !snap.region.is_empty() {
            let class = if !on_output {
                if sink.elsewhere.contains(&source_id) {
                    ContentDamage::OtherOutput
                } else {
                    ContentDamage::OffOutput
                }
            } else if added {
                ContentDamage::Visible
            } else {
                ContentDamage::Hidden
            };
            match class {
                ContentDamage::Visible => sink.stats.content_visible += 1,
                ContentDamage::Hidden => {
                    sink.stats.content_hidden += 1;
                    presented = false;
                    carry = false;
                }
                ContentDamage::OffOutput => {
                    sink.stats.content_off_output += 1;
                    presented = false;
                    carry = false;
                }
                ContentDamage::OtherOutput => {
                    sink.stats.content_other_output += 1;
                    presented = false;
                    carry = false;
                }
            }
            if class != ContentDamage::Visible && tick_skip_log_enabled() {
                log::info!(
                    "content-diag: out{} drawable={:?} epoch={} class={class:?}",
                    sink.output_idx,
                    source_id,
                    snap.epoch,
                );
            }
        }
        if presented {
            sink.presented_ids.push(source_id);
        }
        if carry {
            sink.snapshots.push(snap);
        }
    } else {
        // Nothing pending to present; parity with the old "sampled ⇒ drawn".
        sink.presented_ids.push(source_id);
    }
    Emitted {
        emitted,
        visible,
        signature,
    }
}

/// What [`emit_node`] hands back for the caller to finish the node with.
struct Emitted {
    /// Pieces pushed.
    emitted: u64,
    /// Damage-side summary of the pieces (bbox under `On`, the place under
    /// `Off`), for the presence.
    visible: Region,
    /// `None` only for a node with no place rects, which has no presence.
    signature: Option<PresenceSignature>,
}

/// Push the node's presence, consuming the decision's `place`. Called after the
/// claim step, which is the last reader of `place`; still within the node's own
/// step, so the participants list keeps computation order.
fn push_presence(
    sink: &mut WalkSink<'_>,
    place: Vec<vk::Rect2D>,
    out: Emitted,
    participant: ParticipantId,
) {
    if let Some(signature) = out.signature
        && let Some(p) = presence_from_place(place, out.visible, participant, signature)
    {
        sink.participants.push(p);
    }
}

/// Intersection of two rects, `None` if they do not overlap.
fn intersect_rects(a: vk::Rect2D, b: vk::Rect2D) -> Option<vk::Rect2D> {
    let x0 = a.offset.x.max(b.offset.x);
    let y0 = a.offset.y.max(b.offset.y);
    let x1 = a
        .offset
        .x
        .saturating_add_unsigned(a.extent.width)
        .min(b.offset.x.saturating_add_unsigned(b.extent.width));
    let y1 = a
        .offset
        .y
        .saturating_add_unsigned(a.extent.height)
        .min(b.offset.y.saturating_add_unsigned(b.extent.height));
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

/// Bottom-to-top child order per parent, built once per `build_scene`.
///
/// `emit_window_subtree` used to scan the whole `WindowsMap` and sort a fresh
/// `Vec` for every node it visited — O(N²) on a busy desktop (e16 emits ~2265
/// draws per compose). Step 1's visibility walk needs the same index, so it is
/// built here first, changing nothing about what is emitted.
type ChildrenIndex = HashMap<u32, Vec<u32>>;

fn children_index(windows: &super::backend::WindowsMap) -> ChildrenIndex {
    let mut by_parent: HashMap<u32, Vec<(u32, u64)>> = HashMap::new();
    for (xid, g) in windows {
        if let Some(parent) = g.parent {
            by_parent
                .entry(parent)
                .or_default()
                .push((*xid, g.stack_rank));
        }
    }
    by_parent
        .into_iter()
        .map(|(parent, mut children)| {
            children.sort_by_key(|(_, rank)| *rank);
            (parent, children.into_iter().map(|(xid, _)| xid).collect())
        })
        .collect()
}

/// What the store says about a node, captured once so the trace lines and the
/// emission decision read the same snapshot.
#[derive(Clone, Copy, Debug)]
struct NodeStoreInfo {
    d_id: super::store::DrawableId,
    d_kind: DrawableKind,
    d_depth: u8,
    d_refcount: u32,
    d_part: bool,
    d_extent: vk::Extent2D,
    d_view_null: bool,
    source_id: super::store::DrawableId,
    source_view_null: bool,
    /// The sample-side view of the source (null when `source_view_null`).
    source_view: vk::ImageView,
    /// Storage extent of the SAMPLED source — the `src` denominator under
    /// `Visibility::On`. Differs from the host geometry only for a redirected
    /// window whose backing outgrew it.
    source_extent: vk::Extent2D,
    has_own_redirected_target: bool,
    paint_target_is_self: bool,
    /// First failing gate in the production cascade, `None` when the node
    /// emits. Order matters: it is what the trace prints.
    skip_reason: Option<&'static str>,
}

/// The per-node decision of the scene walk, separated from emission.
///
/// Step 1 (plan: "Factor the per-node decision") — the gate cascade exists once,
/// here, so the visibility pass and the emitter cannot drift. `place` is
/// **geometry, not an emission result**: it is computed for every mapped node
/// with geometry, whether or not the node emits, because a non-emitting parent
/// (manual-redirected, no storage) still clips and claims through its
/// descendants. Coordinates are output-local, using exactly the clamps the
/// emitter applied before this refactor: shape rects clamped to the window
/// extent and the ancestor visible box; unshaped = the visible box. Under
/// `Visibility::On` the rects are clipped to the output as well.
#[derive(Clone, Debug)]
struct NodeDecision {
    /// Absolute (root-space) origin of the window.
    abs_x: i32,
    abs_y: i32,
    /// Output-local origin of the window rect and its size.
    dx: i32,
    dy: i32,
    win_w: i32,
    win_h: i32,
    /// This window's visible box in its OWN local coords: own rect ∩ ancestor
    /// clip, translated. Empty (x1 <= x0 or y1 <= y0) means fully clipped.
    vis_lx0: i32,
    vis_ly0: i32,
    vis_lx1: i32,
    vis_ly1: i32,
    /// Absolute clip passed to children = ancestor clip ∩ own rect.
    child_clip_x0: i32,
    child_clip_y0: i32,
    child_clip_x1: i32,
    child_clip_y1: i32,
    /// True when the window rect touches this output at all.
    intersects: bool,
    /// `store.lookup(host_xid)`; `None` reads as `no_store_lookup`.
    lookup_id: Option<super::store::DrawableId>,
    /// `store.get(lookup_id)` snapshot; `None` with `lookup_id == Some` reads as
    /// `store_get_returned_none`.
    store: Option<NodeStoreInfo>,
    /// Output-local destination rects this node occupies — today's clamps.
    place: Vec<vk::Rect2D>,
    /// The node passes every gate and will push `place.len()` draws.
    emits: bool,
    /// `emits` and outside the COW subtree: the draw overwrites its dst.
    opaque: bool,
    /// Whether this node's children are under a redirected ancestor.
    child_under_redirected_ancestor: bool,
}

#[allow(clippy::too_many_arguments)]
fn decide_node(
    host_xid: u32,
    geom: &super::backend::WindowGeometry,
    parent_abs_x: i32,
    parent_abs_y: i32,
    store: &DrawableStore,
    shape_bounding: &HashMap<u32, Vec<xfixes::RegionRect>>,
    layout_x0: i32,
    layout_y0: i32,
    layout_w: u32,
    layout_h: u32,
    mode: Visibility,
    under_redirected_ancestor: bool,
    under_cow_subtree: bool,
    clip_x0: i32,
    clip_y0: i32,
    clip_x1: i32,
    clip_y1: i32,
) -> NodeDecision {
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

    // Project onto output-local coords.
    let dx = abs_x - layout_x0;
    let dy = abs_y - layout_y0;
    let win_w = own_w;
    let win_h = own_h;
    let intersects = !(dx + win_w <= 0
        || dy + win_h <= 0
        || dx >= i32::try_from(layout_w).unwrap_or(i32::MAX)
        || dy >= i32::try_from(layout_h).unwrap_or(i32::MAX));

    // Place: where this window's pixels land on the output, using the same
    // clamps the emitter always applied. Independent of the store, so a
    // non-emitting node still has a place for its descendants.
    let mut place: Vec<vk::Rect2D> = Vec::new();
    if let Some(rects) = shape_bounding.get(&host_xid) {
        for rect in rects {
            let rx = i32::from(rect.x);
            let ry = i32::from(rect.y);
            let rw = i32::from(rect.width);
            let rh = i32::from(rect.height);
            // Clamp to the window extent AND the ancestor visible box
            // (parent-clipping).
            let cx = rx.max(0).max(vis_lx0);
            let cy = ry.max(0).max(vis_ly0);
            let cw = (rx + rw).min(win_w).min(vis_lx1) - cx;
            let ch = (ry + rh).min(win_h).min(vis_ly1) - cy;
            if cw <= 0 || ch <= 0 {
                continue;
            }
            place.push(vk::Rect2D {
                offset: vk::Offset2D {
                    x: dx + cx,
                    y: dy + cy,
                },
                extent: vk::Extent2D {
                    width: u32::try_from(cw).unwrap_or(0),
                    height: u32::try_from(ch).unwrap_or(0),
                },
            });
        }
    } else if vis_lx1 > vis_lx0 && vis_ly1 > vis_ly0 {
        // Unshaped: the window rect clipped to the ancestor visible box.
        // Common case (child fits inside its parent) → box == full window.
        place.push(vk::Rect2D {
            offset: vk::Offset2D {
                x: dx + vis_lx0,
                y: dy + vis_ly0,
            },
            extent: vk::Extent2D {
                width: u32::try_from(vis_lx1 - vis_lx0).unwrap_or(0),
                height: u32::try_from(vis_ly1 - vis_ly0).unwrap_or(0),
            },
        });
    }

    // Step 1 — under `On`, place is output-clipped as well: the universe is the
    // output, and a piece must be translated back to window pixels for its
    // `src`, which only works if it lies on the output. Under `Off` the
    // pre-step-1 clamps stand and the rasteriser clips a straddling window.
    if mode == Visibility::On {
        let output = vk::Extent2D {
            width: layout_w,
            height: layout_h,
        };
        place = place
            .into_iter()
            .filter_map(|r| clip_rect_to_output_extent(r, output))
            .collect();
    }

    let lookup_id = store.lookup(host_xid);
    let store_info = lookup_id.and_then(|id| {
        let d = store.get(id)?;
        // Stage 4c.3 — route source-storage through `redirected_target`.
        // Both modes blit FROM B; W's geometry (dst_origin, dst_size,
        // intersect test) stays driven by W's own state in `windows`.
        // Only the sampled storage handle reroutes.
        let source_id = store.redirected_target(id).unwrap_or(id);
        let source = store.get(source_id);
        let source_view_null = source.is_none_or(|s| s.storage.image_view == vk::ImageView::null());
        let source_view = source.map_or(vk::ImageView::null(), |s| s.storage.sample_view);
        let source_extent = source.map_or(vk::Extent2D::default(), |s| s.storage.extent);

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
        let has_own_redirected_target = source_id != id;
        // Phase 3.1 — Manual-redirected windows (own a
        // `redirected_target` AND `scene_participating=false`)
        // must NEVER emit to scanout. They go offscreen for the
        // compositor to read via NameWindowPixmap; the X server
        // must not also blit the backing in. Mirrors Xorg's
        // structural guarantee from `compCheckRedirect`.
        let is_manual_redirected = has_own_redirected_target && !d.scene_participating;
        let paint_target_is_self = !is_manual_redirected
            && (has_own_redirected_target || (d.scene_participating && !under_redirected_ancestor));

        // First failing gate, in production order.
        let skip_reason: Option<&'static str> = if is_manual_redirected {
            Some("manual_redirect_unconditional_skip")
        } else if !paint_target_is_self {
            if has_own_redirected_target {
                // Unreachable by construction (see `paint_target_is_self`);
                // kept so the cascade stays exhaustive if the rule evolves.
                Some("paint_target_not_self")
            } else if under_redirected_ancestor {
                Some("paint_target_is_redirected_ancestor")
            } else {
                Some("scene_participating=false")
            }
        } else if !matches!(d.kind, DrawableKind::Window) {
            Some("kind!=Window")
        } else if source_view_null {
            Some("source_image_view_null")
        } else if !intersects {
            Some("no_intersect_with_output")
        } else {
            None
        };

        Some(NodeStoreInfo {
            d_id: d.id,
            d_kind: d.kind,
            d_depth: d.depth,
            d_refcount: d.refcount,
            d_part: d.scene_participating,
            d_extent: d.storage.extent,
            d_view_null: d.storage.image_view == vk::ImageView::null(),
            source_id,
            source_view_null,
            source_view,
            source_extent,
            has_own_redirected_target,
            paint_target_is_self,
            skip_reason,
        })
    });

    let emits = store_info.is_some_and(|s| s.skip_reason.is_none()) && !place.is_empty();
    // Audit #3 — descendants need to know whether THEY sit under a
    // redirected ancestor. The chain is "this window counts as a redirected
    // ancestor iff it owns its own `redirected_target`" — exactly where
    // `resolve_paint_target` stops climbing the parent chain.
    let self_owns_redirected_target = lookup_id
        .and_then(|id| store.redirected_target(id))
        .is_some();

    NodeDecision {
        abs_x,
        abs_y,
        dx,
        dy,
        win_w,
        win_h,
        vis_lx0,
        vis_ly0,
        vis_lx1,
        vis_ly1,
        child_clip_x0,
        child_clip_y0,
        child_clip_x1,
        child_clip_y1,
        intersects,
        lookup_id,
        store: store_info,
        place,
        emits,
        opaque: emits && !under_cow_subtree,
        child_under_redirected_ancestor: under_redirected_ancestor || self_owns_redirected_target,
    }
}

/// One node of the scene walk, mirroring Xorg's `miComputeClips`
/// (`mi/mivaltree.c:197`): decide, clip the children to this node's place,
/// visit them top → bottom, emit what they left of this node, then claim this
/// node's pixels from the caller's universe.
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
///
/// The per-node decision lives in [`decide_node`]; this function logs it,
/// runs the visibility algebra, and recurses. Under `Visibility::Off` the
/// universe is never read and every place rect is emitted, which — with the
/// sink's final reversal — reproduces the pre-step-1 emitter byte for byte.
///
/// # The cap, and the one safe direction
///
/// `universe` and `mine` are only ever intersected and subtracted, so when the
/// 32-box cap collapses one to its bounding box the result is a **superset** of
/// the truly unclaimed area: nodes below emit more than needed (harmless under
/// painter's order), never less. `mine` is the one union in the walk (a union
/// of `universe ∩ r` over the place rects); when it collapses, children are
/// clipped to the parent's bounding box — what the emitter did before step 1.
/// `place` itself is never a `Region`: a collapsed place would claim shape
/// holes, and that is a hole nothing fills.
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)]
fn visit_window_subtree(
    host_xid: u32,
    parent_abs_x: i32,
    parent_abs_y: i32,
    store: &mut DrawableStore,
    windows: &super::backend::WindowsMap,
    children: &ChildrenIndex,
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
    mode: Visibility,
    // What nothing above this node has claimed yet, output-local. This node's
    // subtree subtracts what it opaquely covers on the way out.
    universe: &mut Region,
    sink: &mut WalkSink<'_>,
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
    // Under `On` the universe clips children to the parent's place as well,
    // which is what also clips them to the parent's SHAPE.
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

    let node = decide_node(
        host_xid,
        geom,
        parent_abs_x,
        parent_abs_y,
        store,
        shape_bounding,
        layout_x0,
        layout_y0,
        layout_w,
        layout_h,
        mode,
        under_redirected_ancestor,
        under_cow_subtree,
        clip_x0,
        clip_y0,
        clip_x1,
        clip_y1,
    );
    sink.stats.nodes_visited += 1;
    let abs_x = node.abs_x;
    let abs_y = node.abs_y;

    // Manual-redirect subtree boundary. When a window is
    // `scene_participating=false` here, the compositor owns the
    // entire subtree's presentation (X11 Composite §285+360 —
    // Manual-mode redirect removes the window AND its descendants
    // from normal scene-out; the compositor reads the redirected
    // backing instead).
    //
    // Audit #3 (2026-05-19): the old `prune_subtree=true` for
    // `scene_participating=false` is gone — Automatic descendants of
    // Manual ancestors need to recurse so they can emit their own
    // backing. Per-window emit-vs-skip is decided by
    // `paint_target_is_self` in `decide_node`; the recurse always
    // runs and the `under_redirected_ancestor` flag carries the
    // chain context.
    if node.lookup_id.is_none() {
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
    if node.lookup_id.is_some() {
        if let Some(s) = node.store {
            let NodeStoreInfo {
                d_id,
                d_kind,
                d_depth,
                d_refcount,
                d_part,
                d_extent,
                d_view_null,
                source_id,
                source_view_null,
                has_own_redirected_target,
                paint_target_is_self,
                skip_reason,
                ..
            } = s;
            let dx = node.dx;
            let dy = node.dy;
            let win_w = node.win_w;
            let win_h = node.win_h;
            let intersects = node.intersects;

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
        } else {
            log::trace!(
                "render scene_walk xid={host_xid:#x}: SKIP reason=store_get_returned_none \
                 store_id={lookup_id:?} geom=({x},{y} {w}x{h}) mapped=true depth={depth}",
                lookup_id = node.lookup_id,
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
                    lookup_id = node.lookup_id,
                    x = geom.x,
                    y = geom.y,
                    w = geom.width,
                    h = geom.height,
                    depth = geom.depth,
                );
            }
        }
    }

    // Step 2 of the walk: `mine = ⋃ᵣ (universe ∩ r)` over the exact place
    // rects — what this node and its descendants may still paint. Clipping the
    // children to the parent's PLACE (rect ∩ ancestors ∩ shape), not its
    // bounding box, is the parent-bounding-shape fix: Xorg's child universe is
    // `∩ borderSize`, and `borderSize` is shape-clipped. This is the one union
    // in the walk; a collapse degrades to "children clipped to the parent's
    // bbox", which is exactly the pre-step-1 behaviour.
    //
    // Leaf fast path: a node with no children needs no `mine` at all. Its
    // pieces are `universe ∩ r` per place rect (which is exactly `mine ∩ r`,
    // since `mine ∩ rⱼ = ⋃ᵢ (universe ∩ rᵢ ∩ rⱼ) = universe ∩ rⱼ`), and a
    // non-opaque leaf has no descendants to claim for. e16's shaped
    // decorations are almost all leaves with many rects, so this removes both
    // the union and the collapses it caused — a leaf's pieces are exact where
    // a collapsed `mine` would have made them a superset.
    let kids = children.get(&host_xid).filter(|k| !k.is_empty());
    let is_leaf = kids.is_none();
    let mut mine = Region::new();
    if mode == Visibility::On && !is_leaf {
        match node.place.as_slice() {
            [] => {}
            [only] => mine = universe.clip_to_rect(*only),
            many => {
                for r in many {
                    let piece = universe.clip_to_rect(*r);
                    if mine.union_with_reporting(&piece) {
                        sink.stats.collapses_mine += 1;
                    }
                }
            }
        }
    }

    // Step 3: children TOP → BOTTOM, each claiming from `mine` on the way out.
    // (Computation order; the sink is reversed into painter's order at the
    // end of the walk.)
    if let Some(kids) = kids {
        for &child_xid in kids.iter().rev() {
            visit_window_subtree(
                child_xid,
                abs_x,
                abs_y,
                store,
                windows,
                children,
                shape_bounding,
                layout_x0,
                layout_y0,
                layout_w,
                layout_h,
                mode,
                &mut mine,
                sink,
                node.child_under_redirected_ancestor,
                // Phase 2.6 — COW subtree flag is inherited unchanged.
                // Once we entered the COW top-level, every descendant
                // emits with alpha_passthrough=true.
                under_cow_subtree,
                // Parent-clipping: children are clipped to this window's
                // rect intersected with the inherited ancestor clip.
                node.child_clip_x0,
                node.child_clip_y0,
                node.child_clip_x1,
                node.child_clip_y1,
            );
        }
    }

    // Step 4: emit what the children left of this node. The presence is pushed
    // in step 6, after the claim step has finished reading `place`.
    let mut emitted_presence: Option<(Emitted, ParticipantId)> = None;
    if node.emits
        && let Some(s) = node.store
    {
        // Window scene draw — bind the sample-side view
        // (format/depth-aware swizzle) instead of the raw
        // IDENTITY-swizzle attachment view. This is the
        // load-bearing fix for the "depth-24 windows / COW α
        // leak" bug: the BgraNoAlpha swizzle forced α=ONE for
        // depth-24 used to live ONLY in the engine's RENDER
        // view-cache, never on the scene path. Combined with
        // `alpha_passthrough=true` in the COW subtree, the
        // prior IDENTITY view leaked the BGRA8 padding byte
        // (typically 0) into the shader's `src.a`, blending
        // depth-24 windows with α=0 — invisible against root.
        //
        // One draw per visible piece of each `place` rect: a SHAPE
        // bounding region (marco's rounded-corner mask, panel-applet
        // cutouts) yields one clipped draw per rect; the unshaped,
        // uncovered common case yields the single full-window draw
        // with src [0,0]-[1,1]. Pixels outside the bounding region
        // are intentionally NOT drawn so the layer below (parent /
        // wallpaper / root) shows through.
        //
        // Phase 2.6 — alpha-passthrough is inherited from the
        // COW subtree flag (set on the COW top-level +
        // descendants). Outside the COW subtree, draws stay
        // opaque (no compositor path); inside it, the
        // compositor's stage paints with alpha and we blend
        // over whatever lies below.
        //
        // `src` denominators: under `Off` the host window size, which is
        // what the pre-step-1 emitter divided by; under `On` the SAMPLED
        // source's extent. They differ only for a redirected window whose
        // backing is larger than its host geometry (a shrink keeps the old
        // backing: `redirected_backing_can_fit` accepts `extent >= size`),
        // where the window's content sits at the backing's origin
        // (`resolve_paint_target` routes with offset (0,0)) and dividing by
        // the host size stretches it — the pre-step-1 behaviour, kept under
        // `Off` only so the audit's reference stays comparable frame for
        // frame.
        let (denom_w, denom_h) = match mode {
            Visibility::Off => (node.win_w, node.win_h),
            Visibility::On => (
                i32::try_from(s.source_extent.width).unwrap_or(i32::MAX),
                i32::try_from(s.source_extent.height).unwrap_or(i32::MAX),
            ),
        };
        let out = emit_node(
            sink,
            mode,
            if is_leaf { universe } else { &mine },
            &node.place,
            node.dx,
            node.dy,
            denom_w,
            denom_h,
            s.source_view,
            under_cow_subtree,
            s.source_id,
            store,
            layout_w,
            layout_h,
        );
        // Identity is the host drawable, so a redirect swap is a resample
        // rather than a replacement.
        emitted_presence = Some((
            out,
            ParticipantId {
                role: SceneRole::Window,
                xid: host_xid,
                generation: s.d_id.as_u64(),
            },
        ));
    }

    // Step 5: claim from the caller's universe. An opaque node takes its whole
    // place (its visible pieces plus everything its descendants took, which
    // lie inside it — X11 clips them to the parent). A non-opaque node (COW
    // subtree, manual-redirected, no storage, off-output) takes only what its
    // descendants took: per place rect, `r − mine_after_children`, never
    // through a union of the place rects. Subtraction of exact rects from a
    // capped region can only leave a superset — the safe direction. The one
    // way to over-claim here is `taken` itself collapsing to its bounding box,
    // so a collapsed `taken` is not claimed at all.
    if mode == Visibility::On {
        // The full invariant check (`universe_after ⊆ universe_before`) clones
        // and subtracts a region per node — real work under the
        // `-C debug-assertions=yes` builds every HW recipe and the reporter
        // use — so it is test-only; the pixel-oracle tests exercise every
        // branch of this step. Only the allocation-free check stays a
        // `debug_assert!`.
        #[cfg(test)]
        let before = universe.clone();
        let mut collapsed_here = false;
        if node.opaque {
            for r in &node.place {
                // Nothing to subtract if nothing above left this rect in the
                // universe — the common case for a covered window.
                if !universe.intersects_rect(*r) {
                    continue;
                }
                if universe.subtract_reporting(&Region::from_rect(*r)) {
                    sink.stats.collapses_claim += 1;
                    collapsed_here = true;
                }
            }
        } else if !is_leaf {
            // A non-opaque leaf has no descendants, so it claims nothing; a
            // non-opaque parent claims what its descendants took.
            for r in &node.place {
                if !universe.intersects_rect(*r) {
                    continue;
                }
                let mut taken = Region::from_rect(*r);
                if taken.subtract_reporting(&mine) {
                    // Superset: claiming it could hide something visible.
                    sink.stats.collapses_taken_skipped += 1;
                    collapsed_here = true;
                    continue;
                }
                if universe.subtract_reporting(&taken) {
                    sink.stats.collapses_taken += 1;
                    collapsed_here = true;
                }
            }
        }
        // The invariants hold exactly unless the cap collapsed the universe to
        // its bounding box — which is a superset by design and may well
        // exceed `before` (it fills the holes higher siblings left). That is
        // the documented safe direction, not a bug, so it is not asserted.
        #[cfg(test)]
        if !collapsed_here {
            assert!(
                before.contains(universe),
                "visibility walk: universe grew at xid={host_xid:#x}"
            );
        }
        if !collapsed_here && node.opaque {
            for r in &node.place {
                debug_assert!(
                    !universe.intersects_rect(*r),
                    "visibility walk: opaque place still in universe at xid={host_xid:#x}"
                );
            }
        }
    }

    // Step 6: the presence, consuming the decision's `place` — nothing reads it
    // after the claim step, so it moves into the presence uncopied.
    if let Some((out, participant)) = emitted_presence {
        push_presence(sink, node.place, out, participant);
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
    I: IntoIterator<Item = ((i32, i32), vk::Extent2D, &'a mut RegionSet)>,
{
    for (origin, ext, damage) in outputs {
        for r in rects {
            // Callers pass ROOT-ABSOLUTE rects — `window_absolute_rect` for the
            // scene-participation path, and the root overlay's own root-absolute
            // rects. `clip_rect_to_output_extent` works in output-local space,
            // so the layout origin has to come off first. Omitting it was a
            // latent bug: on a single output at (0,0) the two spaces coincide,
            // but on a multi-output layout with a non-zero origin the damage
            // landed on the wrong output or was clipped away entirely. Note that
            // the overlay's *rendering* path already translates
            // (`apply_list_for_output`), so this was the two halves disagreeing.
            let local = vk::Rect2D {
                offset: vk::Offset2D {
                    x: r.offset.x - origin.0,
                    y: r.offset.y - origin.1,
                },
                extent: r.extent,
            };
            if let Some(clipped) = clip_rect_to_output_extent(local, ext) {
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
    if let Some(r) = project_onto_output(src, dx, dy, layout_w, layout_h) {
        projected.add(r);
    }
}

/// A storage-local rect translated by the node's output-local origin and
/// clipped to the output; `None` if nothing of it lands on the output.
fn project_onto_output(
    src: vk::Rect2D,
    dx: i32,
    dy: i32,
    layout_w: u32,
    layout_h: u32,
) -> Option<vk::Rect2D> {
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

impl ComposeRenderTarget for DamageAuditTarget {
    fn image(&self) -> vk::Image {
        self.image
    }

    fn image_view(&self) -> vk::ImageView {
        self.view
    }

    fn command_buffer(&self) -> vk::CommandBuffer {
        self.command_buffer
    }

    fn completion_semaphore(&self) -> vk::Semaphore {
        vk::Semaphore::null()
    }

    fn width(&self) -> u32 {
        self.extent.width
    }

    fn height(&self) -> u32 {
        self.extent.height
    }

    fn timestamp_pool(&self) -> vk::QueryPool {
        self.timestamp_pool
    }

    fn set_last_gpu_render_ns(&mut self, value: Option<u64>) {
        if value.is_some() {
            self.last_gpu_render_ns = value;
        }
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
        let to_general = [vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
            .src_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
            .dst_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
            .dst_access_mask(vk::AccessFlags2::MEMORY_READ | vk::AccessFlags2::MEMORY_WRITE)
            .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .new_layout(vk::ImageLayout::GENERAL)
            .image(self.image)
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
                &vk::DependencyInfo::default().image_memory_barriers(&to_general),
            );
        }
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
    scissors: &[vk::Rect2D],
    signal_fence: vk::Fence,
    gpu_submitted: &mut bool,
    overlay_ops: &[(u32, vk::Rect2D)],
    xor_pipeline: vk::Pipeline,
    xor_layout: vk::PipelineLayout,
) -> Result<ComposeSubmit, PresentError> {
    use std::os::fd::{FromRawFd, IntoRawFd};

    if bo.state.phase != BoPhase::Free {
        return Err(PresentError::WrongPhase(bo.state.phase));
    }
    let fb_handle = bo.fb_handle.ok_or(PresentError::NoFb)?;
    bo.state.transition_to_recording();
    let submitted = record_and_submit_render(
        vk,
        bo,
        pipeline,
        descriptor_pool,
        scene,
        repaint,
        scissors,
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
            Ok(submitted)
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
    scissors: &[vk::Rect2D],
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
        scissors,
        signal_fence,
        gpu_submitted,
        overlay_ops,
        xor_pipeline,
        xor_layout,
    )
    .map(|_| ())
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
    scissors: &[vk::Rect2D],
    signal_fence: vk::Fence,
    gpu_submitted: &mut bool,
    overlay_ops: &[(u32, vk::Rect2D)],
    xor_pipeline: vk::Pipeline,
    xor_layout: vk::PipelineLayout,
) -> Result<ComposeSubmit, PresentError> {
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
        scissors,
        overlay_ops,
        xor_pipeline,
        xor_layout,
    )?;

    let cb = target.command_buffer();
    let cb_info = [vk::CommandBufferSubmitInfo::default().command_buffer(cb)];
    let signal_semaphore = target.completion_semaphore();
    let sig_info = [vk::SemaphoreSubmitInfo::default()
        .semaphore(signal_semaphore)
        .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)];
    let waits = target.renderer_wait_semaphore().map(|semaphore| {
        [vk::SemaphoreSubmitInfo::default()
            .semaphore(semaphore)
            .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)]
    });
    let mut submit = vk::SubmitInfo2::default().command_buffer_infos(&cb_info);
    if signal_semaphore != vk::Semaphore::null() {
        submit = submit.signal_semaphore_infos(&sig_info);
    }
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
    Ok(ComposeSubmit {
        descriptor_count: descriptors.len(),
    })
}

#[allow(clippy::too_many_arguments)]
fn record_command_buffer<T: ComposeRenderTarget + ?Sized>(
    vk: &crate::kms::vk::device::VkContext,
    bo: &T,
    pipeline: &CompositorPipeline,
    scene: &CompositeScene,
    descriptors: &[vk::DescriptorSet],
    repaint: Repaint,
    scissors: &[vk::Rect2D],
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
        Repaint::Clipped(rect) => (
            vk::AttachmentLoadOp::LOAD,
            // Step 4: `render_area` is the clipped rect, not the whole
            // attachment. Asking the driver to LOAD an attachment we then refuse
            // to touch is pure cost. The layout barrier stays full-subresource,
            // which is consistent — dynamic rendering is free to use a smaller
            // render area than the image.
            rect,
            // LOAD requires the previous layout to be valid; the
            // BO has been through a prior present which left it
            // at GENERAL (KMS scanout layout). Transition from
            // GENERAL → COLOR_ATTACHMENT_OPTIMAL with a full
            // memory barrier so prior writes are visible.
            vk::ImageLayout::GENERAL,
        ),
        Repaint::AuditClearClipped(rect) => {
            (vk::AttachmentLoadOp::CLEAR, rect, vk::ImageLayout::GENERAL)
        }
    };
    // Scissors to render under. `plan_repaint` supplies the damage region's own
    // rects when the bounding box wastes enough to be worth the extra draw
    // calls; otherwise a single rect, which is the behaviour this had before.
    // They are disjoint (a canonical `Region`), so every fragment is written
    // exactly once — which is what keeps the non-idempotent overlay XOR correct.
    let default_scissor = match repaint {
        Repaint::Full(extent) => vk::Rect2D {
            offset: vk::Offset2D::default(),
            extent,
        },
        Repaint::Clipped(rect) | Repaint::AuditClearClipped(rect) => rect,
    };
    let scissors: &[vk::Rect2D] = if scissors.is_empty() {
        std::slice::from_ref(&default_scissor)
    } else {
        scissors
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

        #[allow(clippy::cast_precision_loss)]
        let viewport_size = [bo.width() as f32, bo.height() as f32];
        let mut last_pipeline: Option<vk::Pipeline> = None;
        // Scissor-major: for each rect, replay the draws that touch it. A draw
        // spanning two rects is issued twice, which is correct because the rects
        // are disjoint, and is why `MAX_SCISSOR_RECTS` bounds the list.
        for scissor in scissors {
            crate::vk_count!(cmd_set_scissor);
            device.cmd_set_scissor(cb, 0, std::slice::from_ref(scissor));
            for (i, draw) in scene.draws.iter().enumerate().take(descriptors.len()) {
                if scissors.len() > 1
                    && draw_dst_rect_inward(draw).is_some_and(|dst| !rects_intersect(dst, *scissor))
                {
                    continue;
                }
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

    fn audit_rect(x: i32, y: i32, width: u32, height: u32) -> vk::Rect2D {
        vk::Rect2D {
            offset: vk::Offset2D { x, y },
            extent: vk::Extent2D { width, height },
        }
    }

    #[test]
    fn damage_audit_attribution_filters_to_episode_event_range() {
        let site = Location::caller();
        let ledger = VecDeque::from([
            DamageAuditLedgerEntry {
                id: 3,
                site,
                expected_area: vec![audit_rect(0, 0, 100, 100)],
                contributed_outputs: Vec::new(),
            },
            DamageAuditLedgerEntry {
                id: 4,
                site,
                expected_area: vec![audit_rect(0, 0, 100, 100)],
                contributed_outputs: vec![0],
            },
            DamageAuditLedgerEntry {
                id: 5,
                site,
                expected_area: vec![audit_rect(0, 0, 100, 100)],
                contributed_outputs: Vec::new(),
            },
            DamageAuditLedgerEntry {
                id: 6,
                site,
                expected_area: vec![audit_rect(0, 0, 100, 100)],
                contributed_outputs: Vec::new(),
            },
        ]);

        let found = ledger_candidates_for_tile(&ledger, 0, audit_rect(10, 10, 1, 1), 4, 6);

        assert!(!found.contains("3@"), "stale pre-episode event included");
        assert!(found.contains("4@"));
        assert!(found.contains(":contrib"));
        assert!(found.contains("5@"));
        assert!(found.contains(":missing"));
        assert!(!found.contains("6@"), "post-episode event included");
    }

    #[test]
    fn damage_audit_partial_compose_is_detectable() {
        assert!(compose_submit_was_complete(
            ComposeSubmit {
                descriptor_count: 4
            },
            4
        ));
        assert!(!compose_submit_was_complete(
            ComposeSubmit {
                descriptor_count: 3
            },
            4
        ));
    }

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
            presented_ids: vec![cursor_id],
            pieces_ids: vec![cursor_id],
            stats: WalkStats::default(),
            projected_damage: RegionSet::new(),
            cursor_assignment: assignment,
            new_cursor_rect: Some(rect(10, 20, 16, 16)),
            cursor_record_version: Some(7),
            software_cursor_tail: Some((0, 0)),
            participants: Vec::new(),
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
            presented_ids: vec![cursor_id],
            pieces_ids: vec![cursor_id],
            stats: WalkStats::default(),
            projected_damage: RegionSet::new(),
            cursor_assignment: CursorAssignment::Sw { pos: (10, 20) },
            new_cursor_rect: Some(rect(10, 20, 16, 16)),
            cursor_record_version: Some(7),
            software_cursor_tail: Some((0, 0)),
            participants: Vec::new(),
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
        assert!(
            !TickOutcome::Skipped(TickSkipReason::NothingPending).clears_scene_structure_dirty()
        );
    }

    /// A `NothingPending` skip returns before `build_scene`, so it must not
    /// count as walked — dormancy reconciliation would otherwise run on ids
    /// this output never recorded and flag every armed window dormant.
    #[test]
    fn nothing_pending_skip_did_not_walk() {
        assert!(!TickOutcome::Skipped(TickSkipReason::NothingPending).walked());
        assert!(!TickOutcome::Skipped(TickSkipReason::PendingAcks).walked());
        assert!(TickOutcome::Skipped(TickSkipReason::EmptyDamage).walked());
        assert!(TickOutcome::Composed.walked());
    }

    /// Each input of the pre-walk predicate alone forces a walk; with none set
    /// the tick may skip before walking. The dormant-only case is the one the
    /// predicate exists for: `has_pending_presentation_damage` already
    /// excludes dormant drawables, so it arrives here as `false`.
    #[test]
    fn walk_needed_for_each_input_alone_and_not_otherwise() {
        assert!(!walk_needed(false, false, false, false, false, false));
        assert!(
            walk_needed(true, false, false, false, false, false),
            "structure dirty"
        );
        assert!(
            walk_needed(false, true, false, false, false, false),
            "armed damage"
        );
        assert!(
            walk_needed(false, false, true, false, false, false),
            "first frame"
        );
        assert!(
            walk_needed(false, false, false, true, false, false),
            "owed repaint"
        );
        assert!(
            walk_needed(false, false, false, false, true, false),
            "structure rects"
        );
        assert!(
            walk_needed(false, false, false, false, false, true),
            "audit armed"
        );
    }

    /// The per-output form of the presentation input: a damaged drawable that
    /// emitted pieces on output 0 only makes output 0 walk; one in no output's
    /// set makes every output walk; nothing armed makes none walk.
    #[test]
    fn pending_presentation_is_decided_per_output_from_retained_pieces() {
        use super::super::store::DrawableId;
        use std::collections::HashSet;
        let a = DrawableId::for_tests(1);
        let b = DrawableId::for_tests(2);
        let out0: HashSet<DrawableId> = [a].into_iter().collect();
        let out1: HashSet<DrawableId> = HashSet::new();
        let all = [&out0, &out1];
        // `a` damaged, on output 0 only.
        assert!(pending_presentation_for_output(&[a], &out0, &all));
        assert!(!pending_presentation_for_output(&[a], &out1, &all));
        // `b` damaged but in no output's set ⇒ unknown ⇒ both walk.
        assert!(pending_presentation_for_output(&[b], &out0, &all));
        assert!(pending_presentation_for_output(&[b], &out1, &all));
        // Nothing armed ⇒ neither.
        assert!(!pending_presentation_for_output(&[], &out0, &all));
        assert!(!pending_presentation_for_output(&[], &out1, &all));
        // Spanning both outputs ⇒ both.
        let both0: HashSet<DrawableId> = [a].into_iter().collect();
        let both1: HashSet<DrawableId> = [a].into_iter().collect();
        let all2 = [&both0, &both1];
        assert!(pending_presentation_for_output(&[a], &both0, &all2));
        assert!(pending_presentation_for_output(&[a], &both1, &all2));
        // Structure dirty forces the walk regardless of the per-output answer.
        assert!(walk_needed(true, false, false, false, false, false));
    }

    /// The scheduler's view of a dormant drawable is exactly what the
    /// predicate consumes: a `HiddenDamage`/`NoPieces` drawable with damage
    /// reads as no armed id, so no output walks for it; the paint that re-arms
    /// it flips both back.
    #[test]
    fn dormant_only_damage_does_not_walk_until_a_paint_rearms_it() {
        use super::super::store::{DormantReason, DrawableKind, DrawableStore, Storage};
        let mut store = DrawableStore::new();
        let storage = Storage::for_tests_null(
            vk::Extent2D {
                width: 8,
                height: 8,
            },
            vk::Format::B8G8R8A8_UNORM,
        );
        let id = store
            .allocate(0x700, DrawableKind::Window, 24, true, storage)
            .expect("allocate");
        store.damage(id, audit_rect(0, 0, 4, 4));
        assert!(store.has_pending_presentation_damage());
        // Reconciled as hidden-under-a-cover: pieces but no presented damage.
        let none = std::collections::HashSet::new();
        let pieces: std::collections::HashSet<_> = [id].into_iter().collect();
        store.reconcile_offscreen_no_draw(&none, &pieces);
        assert_eq!(
            store.get(id).map(|d| d.dormant),
            Some(Some(DormantReason::HiddenDamage))
        );
        assert!(!store.has_pending_presentation_damage());
        assert!(!walk_needed(
            false,
            store.has_pending_presentation_damage(),
            false,
            false,
            false,
            false
        ));
        // A new paint re-arms it and the predicate walks again.
        store.damage(id, audit_rect(2, 2, 2, 2));
        assert!(store.has_pending_presentation_damage());
        assert!(walk_needed(
            false,
            store.has_pending_presentation_damage(),
            false,
            false,
            false,
            false
        ));
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

        // Build a `Vec` of tuples so the slice carries a stable lifetime for
        // the iterator; the production callsite produces
        // `(origin, extent, &mut damage)`. Both outputs sit at the origin here,
        // which is the case where root-absolute and output-local coincide — see
        // the test below for the case where they do not.
        let mut outs: Vec<((i32, i32), vk::Extent2D, &mut RegionSet)> = vec![
            ((0, 0), ext_a, &mut damage_a),
            ((0, 0), ext_b, &mut damage_b),
        ];
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

    // ── Step 4: the gates that make clipping safe ─────────────────

    fn draw_at(x: f32, y: f32, w: f32, h: f32, alpha_passthrough: bool) -> CompositeDraw {
        CompositeDraw {
            image_view: vk::ImageView::null(),
            dst_origin: [x, y],
            dst_size: [w, h],
            src_origin: [0.0, 0.0],
            src_size: [1.0, 1.0],
            alpha_passthrough,
        }
    }

    /// An opaque full-output bottom layer, i.e. what the root draw is.
    fn opaque_root(w: f32, h: f32) -> CompositeDraw {
        draw_at(0.0, 0.0, w, h, false)
    }

    fn region_of(rects: &[vk::Rect2D]) -> Region {
        Region::from_rects(rects.iter().copied())
    }

    fn small_damage() -> Region {
        region_of(&[audit_rect(10, 10, 40, 40)])
    }

    #[test]
    fn clipped_path_is_taken_for_small_damage_under_an_opaque_root() {
        let draws = [opaque_root(800.0, 600.0)];
        let plan = plan_repaint(&small_damage(), &draws, extent(800, 600), true, true);
        assert!(plan.full_reason.is_none());
        let Repaint::Clipped(rect) = plan.repaint else {
            panic!("expected Clipped, got {:?}", plan.repaint);
        };
        assert_eq!(rect, audit_rect(10, 10, 40, 40));
        // `painted` is what the recorder will cover: the bbox.
        assert_eq!(plan.painted.bounding_rect(), Some(rect));
    }

    #[test]
    fn painted_always_covers_what_was_requested() {
        // The invariant `commit_submitted` asserts. Checked here for every gate
        // outcome, because a Full fallback must also claim the whole output.
        let cases: [(&[CompositeDraw], bool, bool); 4] = [
            (&[opaque_root(800.0, 600.0)], true, true),
            (&[], true, true),
            (&[opaque_root(800.0, 600.0)], false, true),
            (&[draw_at(0.0, 0.0, 800.0, 600.0, true)], true, true),
        ];
        for (draws, loadable, shared) in cases {
            let requested = small_damage();
            let plan = plan_repaint(&requested, draws, extent(800, 600), loadable, shared);
            assert!(
                plan.painted.contains(&requested),
                "painted must cover requested for {:?}",
                plan.full_reason
            );
        }
    }

    #[test]
    fn empty_draw_list_forces_full() {
        let plan = plan_repaint(&small_damage(), &[], extent(800, 600), true, true);
        assert_eq!(plan.full_reason, Some(FullReason::EmptyDrawList));
        assert!(matches!(plan.repaint, Repaint::Full(_)));
    }

    #[test]
    fn unloadable_bo_forces_full() {
        let draws = [opaque_root(800.0, 600.0)];
        let plan = plan_repaint(&small_damage(), &draws, extent(800, 600), false, true);
        assert_eq!(plan.full_reason, Some(FullReason::UnloadableBo));
    }

    #[test]
    fn copied_route_forces_full() {
        let draws = [opaque_root(800.0, 600.0)];
        let plan = plan_repaint(&small_damage(), &draws, extent(800, 600), true, false);
        assert_eq!(plan.full_reason, Some(FullReason::CopiedRoute));
    }

    #[test]
    fn a_blended_bottom_layer_forces_full() {
        // Every COW-subtree draw is alpha_passthrough by construction, so a
        // compositing desktop lands here — correctly, and at no cost, since a
        // compositor presents a full-screen surface every frame anyway.
        let draws = [draw_at(0.0, 0.0, 800.0, 600.0, true)];
        let plan = plan_repaint(&small_damage(), &draws, extent(800, 600), true, true);
        assert_eq!(plan.full_reason, Some(FullReason::NoOpaqueCover));
    }

    #[test]
    fn an_opaque_draw_that_does_not_reach_the_damage_forces_full() {
        // Opaque, but only over part of the output: the uncovered part of the
        // region would show whatever the previous compose of this BO left.
        let draws = [draw_at(0.0, 0.0, 20.0, 20.0, false)];
        let plan = plan_repaint(&small_damage(), &draws, extent(800, 600), true, true);
        assert_eq!(plan.full_reason, Some(FullReason::NoOpaqueCover));
    }

    #[test]
    fn a_fractional_edge_does_not_count_as_covering() {
        // dst is f32; rounding inward means a half-pixel short of the damage is
        // not cover. The guard can only ever be conservative.
        let requested = region_of(&[audit_rect(0, 0, 800, 600)]);
        let draws = [draw_at(0.5, 0.0, 800.0, 600.0, false)];
        assert!(!opaque_cover_exists(
            &draws,
            requested.bounding_rect().expect("non-empty")
        ));
    }

    #[test]
    fn damage_above_the_threshold_renders_full() {
        // Below the threshold clips, above it does not; the constant is the only
        // thing that moves between these two.
        let draws = [opaque_root(800.0, 600.0)];
        let below = region_of(&[audit_rect(0, 0, 800, 300)]); // 0.5
        assert!(
            plan_repaint(&below, &draws, extent(800, 600), true, true)
                .full_reason
                .is_none()
        );
        let above = region_of(&[audit_rect(0, 0, 800, 420)]); // 0.7
        assert_eq!(
            plan_repaint(&above, &draws, extent(800, 600), true, true).full_reason,
            Some(FullReason::Threshold)
        );
    }

    #[test]
    fn the_threshold_is_measured_on_what_will_be_painted() {
        // Superseded the earlier "measured on the bounding box" rule when 4.5
        // landed: the box is only what gets painted when the frame renders under
        // a single scissor. Two small rects at opposite corners have a near-full
        // box and a tiny area — under 4.5 they render per rect and must stay
        // clipped, because the box is never rasterised.
        let draws = [opaque_root(800.0, 600.0)];
        let sparse = region_of(&[audit_rect(0, 0, 8, 8), audit_rect(790, 590, 8, 8)]);
        assert!(sparse.area() < 200, "region really is tiny");
        let plan = plan_repaint(&sparse, &draws, extent(800, 600), true, true);
        assert!(plan.full_reason.is_none(), "per-rect keeps this clipped");
        assert_eq!(plan.scissors.len(), 2);

        // The converse — one big scissor being measured on its own area — is
        // pinned by `damage_above_the_threshold_renders_full`.
    }

    // ── 4.5: per-rect rendering ──────────────────────────────────

    #[test]
    fn a_single_contiguous_damage_rect_renders_under_one_scissor() {
        let draws = [opaque_root(800.0, 600.0)];
        let plan = plan_repaint(&small_damage(), &draws, extent(800, 600), true, true);
        assert_eq!(plan.scissors.len(), 1);
        assert_eq!(plan.scissors[0], audit_rect(10, 10, 40, 40));
    }

    #[test]
    fn two_separated_rects_render_per_rect_and_painted_excludes_the_gap() {
        // The drag shape: a window's old and new positions. The bounding box
        // spans both plus the empty gap, which measured 36% waste on hardware.
        let draws = [opaque_root(800.0, 600.0)];
        let dragged = region_of(&[audit_rect(0, 0, 100, 100), audit_rect(300, 0, 100, 100)]);
        let plan = plan_repaint(&dragged, &draws, extent(800, 600), true, true);
        assert!(plan.full_reason.is_none());
        assert_eq!(plan.scissors.len(), 2, "should render per rect");
        assert_eq!(plan.painted.area(), 2 * 100 * 100, "the gap is not painted");
        // `repaint` stays the bbox: it is the render area, not the scissor.
        assert!(matches!(plan.repaint, Repaint::Clipped(_)));
    }

    #[test]
    fn adjacent_rects_stay_under_one_scissor() {
        // Touching rects coalesce in the Region, so there is no gap to save and
        // no reason to pay for a second pass.
        let draws = [opaque_root(800.0, 600.0)];
        let touching = region_of(&[audit_rect(0, 0, 50, 50), audit_rect(50, 0, 50, 50)]);
        let plan = plan_repaint(&touching, &draws, extent(800, 600), true, true);
        assert_eq!(plan.scissors.len(), 1);
    }

    #[test]
    fn per_rect_rendering_keeps_frames_off_the_full_path() {
        // Two rects whose bounding box is over the threshold but whose actual
        // area is well under it. Thresholding on the box would render Full and
        // paint the whole screen; thresholding on what will be painted clips.
        let draws = [opaque_root(800.0, 600.0)];
        let spread = region_of(&[audit_rect(0, 0, 100, 100), audit_rect(700, 500, 100, 100)]);
        let bbox_fraction = 800.0 * 600.0 / (800.0 * 600.0);
        assert!(
            bbox_fraction >= CLIPPED_REPAINT_MAX_FRACTION,
            "bbox is the screen"
        );
        let plan = plan_repaint(&spread, &draws, extent(800, 600), true, true);
        assert!(
            plan.full_reason.is_none(),
            "per-rect should have kept this clipped, got {:?}",
            plan.full_reason
        );
        assert_eq!(plan.painted.area(), 2 * 100 * 100);
    }

    #[test]
    fn a_fragmented_region_still_renders_per_rect() {
        // Superseded "too many rects falls back to the box", which pinned the
        // 8-rect cap. That cap made 4.5 stop engaging on MATE, where panels and
        // the desktop fragment a drag region past 8 — reintroducing the 34% box
        // waste the step exists to remove. The bound that matters is the
        // region's own rect cap, and the draw-call cost is scissors × the
        // POST-cull draw count, which is ~4.
        let draws = [opaque_root(800.0, 600.0)];
        let mut scattered = Region::new();
        for i in 0..12 {
            scattered.add_rect(audit_rect(i * 60, i * 40, 10, 10));
        }
        assert_eq!(scattered.rect_count(), 12);
        let plan = plan_repaint(&scattered, &draws, extent(800, 600), true, true);
        assert_eq!(
            plan.scissors.len(),
            12,
            "each fragment gets its own scissor"
        );
        assert_eq!(
            plan.painted.area(),
            12 * 100,
            "and the gaps are not painted"
        );
    }

    #[test]
    fn a_region_past_its_own_cap_arrives_already_collapsed() {
        // `Region` collapses to its extents above MAX_RECTS, so `plan_repaint`
        // can never see an unbounded list — which is why the scissor cap no
        // longer needs to bind.
        let draws = [opaque_root(800.0, 600.0)];
        let mut many = Region::new();
        for i in 0..(Region::MAX_RECTS + 10) {
            many.add_rect(audit_rect((i as i32 % 40) * 20, (i as i32 / 40) * 20, 5, 5));
        }
        assert!(many.rect_count() <= Region::MAX_RECTS);
        let plan = plan_repaint(&many, &draws, extent(800, 600), true, true);
        assert!(plan.scissors.len() <= Region::MAX_RECTS);
    }

    #[test]
    fn scissors_are_disjoint_which_the_overlay_xor_depends_on() {
        // The root IncludeInferiors overlay is not idempotent, so each of its
        // pixels must fall in exactly ONE scissor. Region rects are disjoint by
        // construction; this pins it, including across a band boundary, which is
        // where a hand-rolled rect list would overlap.
        let draws = [opaque_root(800.0, 600.0)];
        let straddling = region_of(&[
            audit_rect(0, 0, 100, 30),
            audit_rect(0, 30, 40, 30),
            audit_rect(300, 0, 100, 100),
        ]);
        let plan = plan_repaint(&straddling, &draws, extent(800, 600), true, true);
        let s = &plan.scissors;
        for (i, a) in s.iter().enumerate() {
            for b in &s[i + 1..] {
                assert!(!rects_intersect(*a, *b), "scissors overlap: {a:?} {b:?}");
            }
        }
        // And they cover exactly the damage, no more.
        let mut covered = Region::new();
        for r in s {
            covered.add_rect(*r);
        }
        if s.len() > 1 {
            assert_eq!(covered, straddling);
        }
    }

    #[test]
    fn a_full_fallback_still_claims_the_whole_output() {
        let plan = plan_repaint(&small_damage(), &[], extent(800, 600), true, true);
        assert_eq!(plan.scissors, vec![audit_rect(0, 0, 800, 600)]);
        assert_eq!(plan.painted.area(), 800 * 600);
    }

    #[test]
    fn culling_drops_draws_outside_the_rect_and_keeps_order() {
        let scene = CompositeScene {
            bg_color: [0.0, 0.0, 0.0, 1.0],
            draws: vec![
                opaque_root(800.0, 600.0),
                draw_at(400.0, 400.0, 50.0, 50.0, true),
                draw_at(10.0, 10.0, 20.0, 20.0, true),
            ],
        };
        let culled = cull_scene_to_region(&scene, &Region::from_rect(audit_rect(0, 0, 100, 100)));
        assert_eq!(culled.draws.len(), 2, "the far draw is culled");
        assert_eq!(culled.draws[0].dst_size, [800.0, 600.0], "root stays first");
        assert_eq!(culled.draws[1].dst_origin, [10.0, 10.0]);
        assert_eq!(culled.bg_color, scene.bg_color);
        // The guard the clipped path depends on must survive its own cull.
        assert!(opaque_cover_exists(
            &culled.draws,
            audit_rect(0, 0, 100, 100)
        ));
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
            &core,
            &mut store,
            &windows,
            0,
            &platform,
            None,
            None,
            None,
            false,
            Visibility::Off,
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
            &core,
            &mut store,
            &windows,
            0,
            &platform,
            None,
            None,
            None,
            false,
            Visibility::Off,
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
            &core,
            &mut store,
            &windows,
            0,
            &platform,
            None,
            None,
            None,
            false,
            Visibility::Off,
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
            &core,
            &mut store,
            &windows,
            0,
            &platform,
            None,
            None,
            None,
            false,
            Visibility::Off,
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
            &core,
            &mut store,
            &windows,
            0,
            &platform,
            None,
            None,
            None,
            false,
            Visibility::Off,
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
            &core,
            &mut store,
            &windows,
            0,
            &platform,
            None,
            None,
            None,
            false,
            Visibility::Off,
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
            Visibility::Off,
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
            &core,
            &mut store,
            &windows,
            0,
            &platform,
            None,
            None,
            None,
            false,
            Visibility::Off,
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
            &core,
            &mut store,
            &windows,
            0,
            &platform,
            None,
            None,
            None,
            false,
            Visibility::Off,
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
            &core,
            &mut store,
            &windows,
            0,
            &platform,
            None,
            None,
            None,
            false,
            Visibility::Off,
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
            &core,
            &mut store,
            &windows,
            0,
            &platform,
            None,
            None,
            None,
            false,
            Visibility::Off,
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
            &core,
            &mut store,
            &windows,
            0,
            &platform,
            None, // no cursor in this fixture
            None,
            None, // cow_host_xid — Phase 2.6 (None = no compositor active)
            false,
            Visibility::Off,
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
            Visibility::Off,
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
            Visibility::Off,
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
            Visibility::Off,
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
                Visibility::Off,
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
            Visibility::Off,
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
            Visibility::Off,
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
            &core,
            &mut store,
            &windows,
            0,
            &platform,
            None,
            None,
            None,
            false,
            Visibility::Off,
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
            Visibility::Off,
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

    // ── Step 1 stage A: the refactored emitter is a no-op ─────────────
    //
    // `legacy_emit_window_subtree` is the emitter as it stood before the
    // per-node decision was factored out (`decide_node`) and the children
    // index replaced the per-node `WindowsMap` scan. It is kept verbatim so the
    // refactor can be checked against it on trees the WM-shaped tests do not
    // build: deep nesting, overlapping siblings, shaped nodes, children that
    // extend beyond their parent, manual/automatic redirect chains, a COW
    // subtree, a non-zero layout origin, a window straddling the output edge.
    // Delete it together with this test once stage B changes what is emitted.
    /// Verbatim pre-refactor emitter (2026-09-03), test twin only.
    fn legacy_emit_window_subtree(
        host_xid: u32,
        parent_abs_x: i32,
        parent_abs_y: i32,
        store: &mut DrawableStore,
        windows: &super::super::backend::WindowsMap,
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
        sampled_ids: &mut Vec<super::super::store::DrawableId>,
        projected: &mut RegionSet,
        // Step 2 — one presence per participant that emits, region derived from the
        // draws it pushed. Threaded rather than returned so the recursion can append
        // in emission order.
        participants: &mut Vec<ScenePresence>,
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
                    let draw_start = draws.len();
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
                        // Region unioned across every draw this window pushed, so a
                        // shaped window emitting one quad per shape rect is ONE
                        // participant. Identity is the host drawable, so a redirect
                        // swap is a resample rather than a replacement.
                        if let Some(p) = legacy_presence_from_draws(
                            draws,
                            draw_start,
                            ParticipantId {
                                role: SceneRole::Window,
                                xid: host_xid,
                                generation: d_id.as_u64(),
                            },
                        ) {
                            participants.push(p);
                        }
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
        let child_under_redirected_ancestor =
            under_redirected_ancestor || self_owns_redirected_target;

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
            legacy_emit_window_subtree(
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
                participants,
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

    #[derive(Debug, PartialEq, Eq)]
    struct DrawKey {
        view: u64,
        dst_origin: [u32; 2],
        dst_size: [u32; 2],
        src_origin: [u32; 2],
        src_size: [u32; 2],
        alpha_passthrough: bool,
    }

    fn draw_key(d: &CompositeDraw) -> DrawKey {
        DrawKey {
            view: ash::vk::Handle::as_raw(d.image_view),
            dst_origin: d.dst_origin.map(f32::to_bits),
            dst_size: d.dst_size.map(f32::to_bits),
            src_origin: d.src_origin.map(f32::to_bits),
            src_size: d.src_size.map(f32::to_bits),
            alpha_passthrough: d.alpha_passthrough,
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    struct WalkOut {
        draws: Vec<DrawKey>,
        participants: Vec<ScenePresence>,
        sampled: Vec<super::super::store::DrawableId>,
        snapshots: Vec<(super::super::store::DrawableId, u64)>,
        projected: Vec<vk::Rect2D>,
    }

    /// The pre-step-1 presence constructor, kept verbatim for the legacy
    /// emitter: region = union of the emitted dst rects (outward-rounded),
    /// signature from the first draw. `visible` did not exist; it equals the
    /// region, which is what `Visibility::Off` produces too.
    fn legacy_presence_from_draws(
        draws: &[CompositeDraw],
        from: usize,
        id: ParticipantId,
    ) -> Option<ScenePresence> {
        let emitted = draws.get(from..)?;
        let first = emitted.first()?;
        let mut region = Region::new();
        let mut place = Vec::new();
        for d in emitted {
            let x0 = d.dst_origin[0].floor();
            let y0 = d.dst_origin[1].floor();
            let x1 = (d.dst_origin[0] + d.dst_size[0]).ceil();
            let y1 = (d.dst_origin[1] + d.dst_size[1]).ceil();
            if x1 > x0 && y1 > y0 {
                #[allow(clippy::cast_possible_truncation)]
                let r = vk::Rect2D {
                    offset: vk::Offset2D {
                        x: x0 as i32,
                        y: y0 as i32,
                    },
                    extent: vk::Extent2D {
                        width: (x1 - x0) as u32,
                        height: (y1 - y0) as u32,
                    },
                };
                region.add_rect(r);
                place.push(r);
            }
        }
        if region.is_empty() {
            return None;
        }
        Some(ScenePresence {
            id,
            visible: region.clone(),
            region,
            place,
            signature: PresenceSignature::new(
                first.image_view,
                first.src_origin,
                first.src_size,
                first.alpha_passthrough,
            ),
        })
    }

    fn platform_with_layout(layout: (i32, i32, u32, u32)) -> PlatformBackend {
        let (lx, ly, lw, lh) = layout;
        let mut platform = PlatformBackend::for_tests();
        let out = &mut platform.outputs[0];
        out.x = lx;
        out.y = ly;
        out.width = u16::try_from(lw).expect("test layout width");
        out.height = u16::try_from(lh).expect("test layout height");
        platform
    }

    /// `build_scene` on a single output at `layout`, no cursor.
    fn build_with(
        mode: Visibility,
        core: &KmsCore,
        store: &mut DrawableStore,
        windows: &super::super::backend::WindowsMap,
        layout: (i32, i32, u32, u32),
        cow_host_xid: Option<u32>,
    ) -> SceneBuild {
        let platform = platform_with_layout(layout);
        build_scene(
            core,
            store,
            windows,
            0,
            &platform,
            None,
            None,
            cow_host_xid,
            false,
            mode,
        )
    }

    /// `build_with` for one output of a multi-output layout: `elsewhere` is
    /// what the OTHER output(s) showed at their last walk (their `pieces_ids`).
    fn build_with_elsewhere(
        mode: Visibility,
        core: &KmsCore,
        store: &mut DrawableStore,
        windows: &super::super::backend::WindowsMap,
        layout: (i32, i32, u32, u32),
        cow_host_xid: Option<u32>,
        elsewhere: &std::collections::HashSet<super::super::store::DrawableId>,
    ) -> SceneBuild {
        let platform = platform_with_layout(layout);
        build_scene_with(
            core,
            store,
            windows,
            0,
            &platform,
            None,
            None,
            cow_host_xid,
            false,
            mode,
            elsewhere,
        )
    }

    fn sorted_rects(mut rects: Vec<vk::Rect2D>) -> Vec<vk::Rect2D> {
        rects.sort_by_key(|r| (r.offset.y, r.offset.x, r.extent.height, r.extent.width));
        rects
    }

    fn walk_out_of(built: &SceneBuild) -> WalkOut {
        WalkOut {
            draws: built.scene.draws.iter().map(draw_key).collect(),
            participants: built.participants.clone(),
            sampled: built.sampled_ids.clone(),
            snapshots: built.snapshots.iter().map(|s| (s.id, s.epoch)).collect(),
            projected: sorted_rects(built.projected_damage.rects().to_vec()),
        }
    }

    /// Run the top-level walk with the LEGACY emitter (`legacy == true`) or the
    /// real `build_scene` under `Visibility::Off`, and normalise the output.
    /// The fixture has no root drawable, so the two lists line up one to one.
    fn walk_with(
        legacy: bool,
        core: &KmsCore,
        store: &mut DrawableStore,
        windows: &super::super::backend::WindowsMap,
        layout: (i32, i32, u32, u32),
        cow_host_xid: Option<u32>,
    ) -> WalkOut {
        if !legacy {
            let built = build_with(Visibility::Off, core, store, windows, layout, cow_host_xid);
            return walk_out_of(&built);
        }
        let (lx, ly, lw, lh) = layout;
        let mut draws = Vec::new();
        let mut snapshots = Vec::new();
        let mut sampled = Vec::new();
        let mut projected = RegionSet::new();
        let mut participants = Vec::new();
        for &top in &core.top_level_order {
            let under_cow = Some(top) == cow_host_xid;
            legacy_emit_window_subtree(
                top,
                0,
                0,
                store,
                windows,
                &core.shape_bounding,
                lx,
                ly,
                lw,
                lh,
                &mut draws,
                &mut snapshots,
                &mut sampled,
                &mut projected,
                &mut participants,
                false,
                under_cow,
                i32::MIN / 2,
                i32::MIN / 2,
                i32::MAX / 2,
                i32::MAX / 2,
            );
        }
        WalkOut {
            draws: draws.iter().map(draw_key).collect(),
            participants,
            sampled,
            snapshots: snapshots.iter().map(|s| (s.id, s.epoch)).collect(),
            projected: sorted_rects(projected.rects().to_vec()),
        }
    }

    fn set_rank(windows: &mut super::super::backend::WindowsMap, xid: u32, rank: u64) {
        windows.get_mut(&xid).expect("window present").stack_rank = rank;
    }

    fn alloc_backing(
        store: &mut DrawableStore,
        xid: u32,
        w: u32,
        h: u32,
    ) -> super::super::store::DrawableId {
        let mut storage =
            super::super::store::Storage::for_tests_null(extent(w, h), vk::Format::B8G8R8A8_UNORM);
        let view: vk::ImageView = ash::vk::Handle::from_raw(u64::from(xid) | 0xB000_0000);
        storage.image_view = view;
        storage.sample_view = view;
        store
            .allocate(xid, DrawableKind::Pixmap, 32, true, storage)
            .expect("alloc backing stub")
    }

    /// The tree every differential case runs on. Ranks are all distinct so
    /// sibling order does not depend on `HashMap` iteration.
    fn differential_fixture() -> (KmsCore, DrawableStore, super::super::backend::WindowsMap) {
        let mut core = KmsCore::for_tests();
        let mut store = DrawableStore::new();
        let mut windows = super::super::backend::WindowsMap::new();
        let mut rank = 1u64;
        let mut add = |store: &mut DrawableStore,
                       windows: &mut super::super::backend::WindowsMap,
                       xid: u32,
                       x: i16,
                       y: i16,
                       w: u16,
                       h: u16,
                       parent: Option<u32>,
                       mapped: bool| {
            alloc_stub_window(store, windows, xid, x, y, w, h, parent, mapped);
            set_rank(windows, xid, rank);
            rank += 1;
        };

        // Nesting three deep.
        add(
            &mut store,
            &mut windows,
            0x100,
            10,
            10,
            300,
            200,
            None,
            true,
        );
        add(
            &mut store,
            &mut windows,
            0x101,
            20,
            20,
            200,
            100,
            Some(0x100),
            true,
        );
        add(
            &mut store,
            &mut windows,
            0x102,
            30,
            30,
            50,
            40,
            Some(0x101),
            true,
        );
        // Unmapped child with a mapped grandchild: whole subtree hidden.
        add(
            &mut store,
            &mut windows,
            0x103,
            5,
            5,
            50,
            50,
            Some(0x100),
            false,
        );
        add(
            &mut store,
            &mut windows,
            0x104,
            1,
            1,
            10,
            10,
            Some(0x103),
            true,
        );
        // Overlapping siblings.
        add(
            &mut store,
            &mut windows,
            0x200,
            100,
            100,
            150,
            150,
            None,
            true,
        );
        add(
            &mut store,
            &mut windows,
            0x201,
            200,
            150,
            150,
            150,
            None,
            true,
        );
        // Shaped node with five rects, one of them outside the window.
        add(
            &mut store,
            &mut windows,
            0x300,
            400,
            40,
            120,
            90,
            None,
            true,
        );
        core.shape_bounding.insert(
            0x300,
            vec![
                xfixes::RegionRect {
                    x: 0,
                    y: 0,
                    width: 120,
                    height: 10,
                },
                xfixes::RegionRect {
                    x: 0,
                    y: 10,
                    width: 10,
                    height: 70,
                },
                xfixes::RegionRect {
                    x: 110,
                    y: 10,
                    width: 10,
                    height: 70,
                },
                xfixes::RegionRect {
                    x: 0,
                    y: 80,
                    width: 120,
                    height: 10,
                },
                xfixes::RegionRect {
                    x: 100,
                    y: 85,
                    width: 60,
                    height: 30,
                },
            ],
        );
        // Child extending beyond a tiny parent (the fvwm holding-window case),
        // plus a grandchild that is clipped away entirely.
        add(
            &mut store,
            &mut windows,
            0x400,
            600,
            300,
            10,
            10,
            None,
            true,
        );
        add(
            &mut store,
            &mut windows,
            0x401,
            -5,
            -5,
            100,
            100,
            Some(0x400),
            true,
        );
        add(
            &mut store,
            &mut windows,
            0x402,
            50,
            50,
            20,
            20,
            Some(0x401),
            true,
        );
        // Straddling the output's top-left corner, and fully off-output.
        add(
            &mut store,
            &mut windows,
            0x500,
            -50,
            -50,
            100,
            100,
            None,
            true,
        );
        add(
            &mut store,
            &mut windows,
            0x501,
            5000,
            5000,
            10,
            10,
            None,
            true,
        );
        // Manual-redirected top-level with (a) an automatic-redirected child
        // owning its own backing and (b) a plain child whose paint lands in
        // the manual ancestor's backing.
        add(
            &mut store,
            &mut windows,
            0x600,
            50,
            400,
            200,
            100,
            None,
            true,
        );
        add(
            &mut store,
            &mut windows,
            0x601,
            10,
            10,
            60,
            40,
            Some(0x600),
            true,
        );
        add(
            &mut store,
            &mut windows,
            0x602,
            100,
            10,
            60,
            40,
            Some(0x600),
            true,
        );
        let m_id = store.lookup(0x600).expect("manual present");
        let m_backing = alloc_backing(&mut store, 0xB600, 200, 100);
        store.set_redirected_target(m_id, Some(m_backing));
        store.set_scene_participating(m_id, false);
        let a_id = store.lookup(0x601).expect("automatic present");
        let a_backing = alloc_backing(&mut store, 0xB601, 60, 40);
        store.set_redirected_target(a_id, Some(a_backing));
        // Automatic-redirected top-level (sampled through its backing).
        add(
            &mut store,
            &mut windows,
            0x700,
            300,
            400,
            80,
            60,
            None,
            true,
        );
        let r_id = store.lookup(0x700).expect("automatic top present");
        let r_backing = alloc_backing(&mut store, 0xB700, 80, 60);
        store.set_redirected_target(r_id, Some(r_backing));
        // COW top-level with a stage child — alpha_passthrough subtree.
        add(&mut store, &mut windows, 0x800, 0, 0, 800, 600, None, true);
        add(
            &mut store,
            &mut windows,
            0x801,
            0,
            0,
            800,
            600,
            Some(0x800),
            true,
        );
        // A window with geometry but no storage at all.
        windows.insert(
            0x900,
            super::super::backend::WindowGeometry {
                x: 700,
                y: 500,
                width: 40,
                height: 40,
                depth: 24,
                mapped: true,
                parent: None,
                stack_rank: rank,
                bg_pixel: None,
                bg_pixmap: None,
                cursor: None,
            },
        );

        core.top_level_order = vec![
            0x100, 0x200, 0x201, 0x300, 0x400, 0x500, 0x501, 0x600, 0x700, 0x900, 0x800,
        ];
        (core, store, windows)
    }

    #[test]
    fn refactored_emitter_matches_the_legacy_emitter_exactly() {
        for layout in [
            (0, 0, 800u32, 600u32),
            (100, 50, 2560, 1440),
            (-300, -200, 800, 600),
            (123, 45, 640, 480),
        ] {
            for cow in [None, Some(0x800u32)] {
                let (core, mut store, windows) = differential_fixture();
                let legacy = walk_with(true, &core, &mut store, &windows, layout, cow);
                let new = walk_with(false, &core, &mut store, &windows, layout, cow);
                assert!(
                    !legacy.draws.is_empty(),
                    "fixture sanity: the tree must emit something at layout {layout:?}"
                );
                assert_eq!(new, legacy, "layout {layout:?} cow {cow:?}");
            }
        }
    }

    /// The fixture exercises the gates it claims to: count what each case
    /// contributes so a silent no-op fixture cannot pass the test above.
    #[test]
    fn differential_fixture_exercises_every_gate() {
        let (core, mut store, windows) = differential_fixture();
        let out = walk_with(
            false,
            &core,
            &mut store,
            &windows,
            (0, 0, 800, 600),
            Some(0x800),
        );
        let views: Vec<u64> = out.draws.iter().map(|d| d.view).collect();
        let has = |xid: u32| views.contains(&(u64::from(xid) | 0xFF00_0000));
        let has_backing = |xid: u32| views.contains(&(u64::from(xid) | 0xB000_0000));
        assert!(
            has(0x100) && has(0x101) && has(0x102),
            "nesting emits all three"
        );
        assert!(!has(0x103) && !has(0x104), "unmapped subtree emits nothing");
        assert!(has(0x200) && has(0x201), "overlapping siblings both emit");
        assert_eq!(
            out.draws
                .iter()
                .filter(|d| d.view == u64::from(0x300u32) | 0xFF00_0000)
                .count(),
            5,
            "shaped node emits one draw per rect (the fifth clamped, not dropped)"
        );
        assert!(has(0x401), "oversized child emits, clipped to its parent");
        assert!(
            !has(0x402),
            "grandchild clipped away entirely emits nothing"
        );
        assert!(has(0x500), "straddling window emits");
        assert!(!has(0x501), "off-output window does not emit");
        assert!(
            !has(0x600) && !has_backing(0xB600),
            "manual-redirected node never emits"
        );
        assert!(
            has_backing(0xB601),
            "automatic child under a manual ancestor emits its backing"
        );
        assert!(
            !has(0x602),
            "plain child under a manual ancestor paints into the ancestor"
        );
        assert!(
            has_backing(0xB700) && !has(0x700),
            "automatic top-level samples its backing"
        );
        assert!(
            out.draws.iter().filter(|d| d.alpha_passthrough).count() == 2,
            "exactly the COW and its stage are alpha_passthrough"
        );
        assert!(!has(0x900), "geometry without storage emits nothing");
    }

    // ── Step 1 stage A: opaque cover is a union, tested by subtraction ────

    #[test]
    fn opaque_cover_accepts_a_union_of_draws_across_a_window_edge() {
        // Root fragmented around a window at (200,150)-(400,350), plus the window.
        let draws = [
            draw_at(0.0, 0.0, 800.0, 150.0, false),     // above
            draw_at(0.0, 350.0, 800.0, 250.0, false),   // below
            draw_at(0.0, 150.0, 200.0, 200.0, false),   // left
            draw_at(400.0, 150.0, 400.0, 200.0, false), // right
            draw_at(200.0, 150.0, 200.0, 200.0, false), // the window
        ];
        assert!(opaque_cover_exists(&draws, audit_rect(150, 100, 100, 100)));
        assert!(opaque_cover_exists(&draws, audit_rect(0, 0, 800, 600)));
    }

    #[test]
    fn opaque_cover_rejects_a_one_pixel_gap_in_the_union() {
        let draws = [
            draw_at(0.0, 0.0, 800.0, 150.0, false),
            draw_at(0.0, 351.0, 800.0, 249.0, false), // leaves row 350 uncovered
            draw_at(0.0, 150.0, 200.0, 200.0, false),
            draw_at(400.0, 150.0, 400.0, 200.0, false),
            draw_at(200.0, 150.0, 200.0, 200.0, false),
        ];
        assert!(!opaque_cover_exists(&draws, audit_rect(150, 300, 100, 100)));
        // A translucent draw over the gap does not close it.
        let mut with_alpha = draws.to_vec();
        with_alpha.push(draw_at(0.0, 340.0, 800.0, 20.0, true));
        assert!(!opaque_cover_exists(
            &with_alpha,
            audit_rect(150, 300, 100, 100)
        ));
    }

    #[test]
    fn clipped_path_with_two_scissors_covered_by_different_draws() {
        // No root: two opaque windows, each covering one damage rect.
        let draws = [
            draw_at(0.0, 0.0, 400.0, 600.0, false),
            draw_at(400.0, 0.0, 400.0, 600.0, false),
        ];
        // Two small rects far apart so the bbox is wasteful and 4.5 splits.
        let damage = region_of(&[audit_rect(10, 10, 20, 20), audit_rect(700, 500, 20, 20)]);
        let plan = plan_repaint(&damage, &draws, extent(800, 600), true, true);
        assert!(plan.full_reason.is_none(), "{:?}", plan.full_reason);
        assert_eq!(plan.scissors.len(), 2);
        assert!(
            plan.scissors
                .iter()
                .all(|r| opaque_cover_exists(&draws, *r))
        );
    }

    // ── Step 1 stage A: an incomplete submit never stages `painted` ──────

    /// After an `invalidate` the model owes a full repaint; a tick that finds no
    /// producer damage must compose anyway, not take the EmptyDamage skip.
    #[test]
    fn an_owed_repaint_is_not_an_empty_damage_skip() {
        assert!(skip_for_empty_damage(true, false, false), "idle skips");
        assert!(
            !skip_for_empty_damage(false, false, false),
            "damage composes"
        );
        assert!(
            !skip_for_empty_damage(true, true, false),
            "first frame composes"
        );
        assert!(
            !skip_for_empty_damage(true, false, true),
            "owed repaint (invalidated BO) must compose"
        );
    }

    #[test]
    fn incomplete_submit_invalidates_instead_of_staging() {
        let ext = extent(800, 600);
        let mut damage = ScanoutDamage::new(2, ext);
        let repaint = region_of(&[audit_rect(10, 10, 40, 40)]);
        // Complete: staged as usual.
        stage_submitted_frame(&mut damage, true, 0, &repaint, &repaint);
        assert!(damage.has_staged_frame());
        damage.retire_success();
        assert!(!damage.has_staged_frame());
        // Incomplete: nothing staged, and every BO owes the whole output again.
        stage_submitted_frame(&mut damage, false, 1, &repaint, &repaint);
        assert!(!damage.has_staged_frame());
        for bo in 0..2 {
            assert_eq!(
                damage.missing_area(bo),
                u64::from(ext.width) * u64::from(ext.height),
                "bo {bo} must owe the full output after an incomplete submit"
            );
        }
    }

    // ── Step 1 stage B: the visibility walk ──────────────────────────────

    /// One rasterised pixel: what the compose would show there, as the stack
    /// of (view, source u, source v) samples an alpha draw leaves and an
    /// opaque draw resets. Comparing stacks pixel for pixel between the
    /// `On` and `Off` scenes is the invariant step 1 must keep: clipping
    /// changes what is *drawn*, never what is *shown*.
    type PixelStack = Vec<(u64, f64, f64)>;

    fn rasterise(draws: &[CompositeDraw], w: u32, h: u32) -> Vec<PixelStack> {
        let (wi, hi) = (w as usize, h as usize);
        let mut grid: Vec<PixelStack> = vec![Vec::new(); wi * hi];
        for d in draws {
            let x0 = d.dst_origin[0].floor().max(0.0) as usize;
            let y0 = d.dst_origin[1].floor().max(0.0) as usize;
            let x1 = ((d.dst_origin[0] + d.dst_size[0]).ceil().max(0.0) as usize).min(wi);
            let y1 = ((d.dst_origin[1] + d.dst_size[1]).ceil().max(0.0) as usize).min(hi);
            let view = ash::vk::Handle::as_raw(d.image_view);
            for py in y0..y1 {
                for px in x0..x1 {
                    let fx =
                        (px as f64 + 0.5 - f64::from(d.dst_origin[0])) / f64::from(d.dst_size[0]);
                    let fy =
                        (py as f64 + 0.5 - f64::from(d.dst_origin[1])) / f64::from(d.dst_size[1]);
                    let u = f64::from(d.src_origin[0]) + fx * f64::from(d.src_size[0]);
                    let v = f64::from(d.src_origin[1]) + fy * f64::from(d.src_size[1]);
                    let cell = &mut grid[py * wi + px];
                    if d.alpha_passthrough {
                        cell.push((view, u, v));
                    } else {
                        cell.clear();
                        cell.push((view, u, v));
                    }
                }
            }
        }
        grid
    }

    fn stacks_equal(a: &PixelStack, b: &PixelStack) -> bool {
        a.len() == b.len()
            && a.iter()
                .zip(b)
                .all(|(x, y)| x.0 == y.0 && (x.1 - y.1).abs() < 1e-4 && (x.2 - y.2).abs() < 1e-4)
    }

    /// Assert the `On` scene shows the same pixels as the `Off` scene of the
    /// same fixture, on every pixel of the output. Returns the `On` build for
    /// further assertions.
    fn assert_oracle(
        core: &KmsCore,
        store: &mut DrawableStore,
        windows: &super::super::backend::WindowsMap,
        layout: (i32, i32, u32, u32),
        cow: Option<u32>,
        label: &str,
    ) -> SceneBuild {
        let off = build_with(Visibility::Off, core, store, windows, layout, cow);
        let on = build_with(Visibility::On, core, store, windows, layout, cow);
        let (w, h) = (layout.2, layout.3);
        let a = rasterise(&off.scene.draws, w, h);
        let b = rasterise(&on.scene.draws, w, h);
        for (i, (sa, sb)) in a.iter().zip(&b).enumerate() {
            assert!(
                stacks_equal(sa, sb),
                "{label}: pixel ({},{}) differs: off={sa:?} on={sb:?} (layout {layout:?}, cow {cow:?})",
                i % w as usize,
                i / w as usize,
            );
        }
        assert_eq!(
            on.stats.draws_emitted,
            u64::try_from(on.scene.draws.len()).unwrap(),
            "{label}: the stats count what was emitted"
        );
        on
    }

    /// Root drawable at the logical screen size, sampled through a sentinel view.
    fn alloc_root(core: &KmsCore, store: &mut DrawableStore, w: u32, h: u32) {
        let mut storage =
            super::super::store::Storage::for_tests_null(extent(w, h), vk::Format::B8G8R8A8_UNORM);
        let view: ash::vk::ImageView = ash::vk::Handle::from_raw(0x00A0_7000);
        storage.image_view = view;
        storage.sample_view = view;
        store
            .allocate(core.window_id, DrawableKind::Root, 24, true, storage)
            .expect("alloc root stub");
    }

    fn area_of(r: vk::Rect2D) -> u64 {
        u64::from(r.extent.width) * u64::from(r.extent.height)
    }

    fn draws_of(built: &SceneBuild, view_raw: u64) -> Vec<vk::Rect2D> {
        built
            .scene
            .draws
            .iter()
            .filter(|d| ash::vk::Handle::as_raw(d.image_view) == view_raw)
            .filter_map(draw_dst_rect_inward)
            .collect()
    }

    fn win_view(xid: u32) -> u64 {
        u64::from(xid) | 0xFF00_0000
    }

    /// The reversal proof with a root present: under `Visibility::Off`,
    /// `build_scene` produces exactly what the pre-step-1 code produced — the
    /// root draw first, then the legacy emitter's list — bit for bit.
    #[test]
    fn off_mode_reproduces_the_legacy_root_and_emitter_exactly() {
        for layout in [
            (0, 0, 800u32, 600u32),
            (2560, 0, 2560, 1440),
            (-300, -200, 800, 600),
        ] {
            for cow in [None, Some(0x800u32)] {
                let (core, mut store, windows) = differential_fixture();
                alloc_root(&core, &mut store, 5120, 1440);
                let legacy = walk_with(true, &core, &mut store, &windows, layout, cow);
                let off = build_with(Visibility::Off, &core, &mut store, &windows, layout, cow);
                // The legacy root draw, as the old `build_scene` pushed it.
                let root_key = DrawKey {
                    view: 0x00A0_7000,
                    dst_origin: [(-layout.0) as f32, (-layout.1) as f32].map(f32::to_bits),
                    dst_size: [5120.0f32, 1440.0f32].map(f32::to_bits),
                    src_origin: [0.0f32, 0.0f32].map(f32::to_bits),
                    src_size: [1.0f32, 1.0f32].map(f32::to_bits),
                    alpha_passthrough: false,
                };
                let mut expect_draws = vec![root_key];
                expect_draws.extend(legacy.draws);
                let got = walk_out_of(&off);
                assert_eq!(
                    got.draws, expect_draws,
                    "draws, layout {layout:?} cow {cow:?}"
                );
                assert_eq!(
                    got.participants.len(),
                    legacy.participants.len() + 1,
                    "one root presence plus the legacy ones"
                );
                assert_eq!(got.participants[0].id.role, SceneRole::Root);
                assert_eq!(&got.participants[1..], &legacy.participants[..]);
                assert_eq!(got.sampled.len(), legacy.sampled.len() + 1);
                assert_eq!(&got.sampled[1..], &legacy.sampled[..]);
                assert_eq!(got.stats_free_snapshots(), legacy.snapshots.len() + 1);
            }
        }
    }

    impl WalkOut {
        fn stats_free_snapshots(&self) -> usize {
            self.snapshots.len()
        }
    }

    /// The pixel oracle over the whole differential fixture, with a root, on
    /// several layouts, with and without the COW subtree.
    #[test]
    fn visibility_shows_the_same_pixels_as_the_unclipped_scene() {
        for layout in [
            (0, 0, 800u32, 600u32),
            (100, 50, 700, 500),
            (-300, -200, 800, 600),
            (2560, 0, 640, 480),
        ] {
            for cow in [None, Some(0x800u32)] {
                let (core, mut store, windows) = differential_fixture();
                alloc_root(&core, &mut store, 5120, 1440);
                let on = assert_oracle(&core, &mut store, &windows, layout, cow, "fixture");
                assert!(on.stats.nodes_visited > 0);
                assert!(
                    on.stats.draws_emitted == u64::try_from(on.scene.draws.len()).unwrap(),
                    "stats count what was emitted"
                );
            }
        }
    }

    fn two_windows(
        lower: (i16, i16, u16, u16),
        upper: (i16, i16, u16, u16),
    ) -> (KmsCore, DrawableStore, super::super::backend::WindowsMap) {
        let mut core = KmsCore::for_tests();
        let mut store = DrawableStore::new();
        let mut windows = super::super::backend::WindowsMap::new();
        alloc_root(&core, &mut store, 800, 600);
        alloc_stub_window(
            &mut store,
            &mut windows,
            0x100,
            lower.0,
            lower.1,
            lower.2,
            lower.3,
            None,
            true,
        );
        alloc_stub_window(
            &mut store,
            &mut windows,
            0x200,
            upper.0,
            upper.1,
            upper.2,
            upper.3,
            None,
            true,
        );
        set_rank(&mut windows, 0x100, 1);
        set_rank(&mut windows, 0x200, 2);
        core.top_level_order = vec![0x100, 0x200];
        (core, store, windows)
    }

    /// An opaque top-level fully covering a lower one: the lower emits nothing
    /// but is still a participant; the root emits the output minus the cover.
    #[test]
    fn a_fully_covered_window_emits_nothing_and_stays_a_participant() {
        let (core, mut store, windows) = two_windows((100, 100, 50, 50), (80, 80, 100, 100));
        let on = assert_oracle(
            &core,
            &mut store,
            &windows,
            (0, 0, 800, 600),
            None,
            "full cover",
        );
        assert!(
            draws_of(&on, win_view(0x100)).is_empty(),
            "covered window emits nothing"
        );
        assert_eq!(draws_of(&on, win_view(0x200)).len(), 1);
        let root: u64 = draws_of(&on, 0x00A0_7000).iter().map(|r| area_of(*r)).sum();
        assert_eq!(
            root,
            800 * 600 - 100 * 100,
            "root = output − the opaque cover"
        );
        assert_eq!(on.stats.hidden_participants, 1);
        let hidden = on
            .participants
            .iter()
            .find(|p| p.id.xid == 0x100)
            .expect("hidden window is still a participant");
        assert!(hidden.visible.is_empty());
        assert_eq!(
            hidden.region.bounding_rect(),
            Some(audit_rect(100, 100, 50, 50))
        );
        // Placement, not visibility: the presence region is the full window.
        assert_eq!(hidden.region.area(), 50 * 50);
    }

    /// Partial cover: the lower window emits only its visible pieces.
    #[test]
    fn a_partly_covered_window_emits_only_its_visible_pieces() {
        let (core, mut store, windows) = two_windows((100, 100, 200, 200), (250, 150, 200, 200));
        let on = assert_oracle(
            &core,
            &mut store,
            &windows,
            (0, 0, 800, 600),
            None,
            "partial",
        );
        let lower: u64 = draws_of(&on, win_view(0x100))
            .iter()
            .map(|r| area_of(*r))
            .sum();
        // 200×200 minus the 50×150 overlap.
        assert_eq!(lower, 200 * 200 - 50 * 150);
        assert!(
            draws_of(&on, win_view(0x100)).len() > 1,
            "emitted as pieces"
        );
        assert_eq!(on.stats.hidden_participants, 0);
    }

    /// The parent-bounding-shape fix, written before the walk: an EMPTY parent
    /// shape suppresses its children; a partial shape clips them. Under the
    /// pre-step-1 emitter the child clip came from the parent's rect alone.
    #[test]
    fn an_empty_parent_shape_suppresses_its_children() {
        let mut core = KmsCore::for_tests();
        let mut store = DrawableStore::new();
        let mut windows = super::super::backend::WindowsMap::new();
        alloc_root(&core, &mut store, 800, 600);
        alloc_stub_window(
            &mut store,
            &mut windows,
            0x100,
            100,
            100,
            200,
            200,
            None,
            true,
        );
        alloc_stub_window(
            &mut store,
            &mut windows,
            0x101,
            10,
            10,
            50,
            50,
            Some(0x100),
            true,
        );
        core.top_level_order = vec![0x100];
        core.shape_bounding.insert(0x100, Vec::new());
        let on = build_with(
            Visibility::On,
            &core,
            &mut store,
            &windows,
            (0, 0, 800, 600),
            None,
        );
        assert!(
            draws_of(&on, win_view(0x100)).is_empty(),
            "empty shape: parent draws nothing"
        );
        assert!(
            draws_of(&on, win_view(0x101)).is_empty(),
            "…and neither do its children"
        );
        let root: u64 = draws_of(&on, 0x00A0_7000).iter().map(|r| area_of(*r)).sum();
        assert_eq!(root, 800 * 600, "the root shows through the whole hole");
    }

    #[test]
    fn a_partial_parent_shape_clips_its_children() {
        let mut core = KmsCore::for_tests();
        let mut store = DrawableStore::new();
        let mut windows = super::super::backend::WindowsMap::new();
        alloc_root(&core, &mut store, 800, 600);
        alloc_stub_window(
            &mut store,
            &mut windows,
            0x100,
            100,
            100,
            200,
            200,
            None,
            true,
        );
        // Child spans the whole parent; the parent's shape is its left half.
        alloc_stub_window(
            &mut store,
            &mut windows,
            0x101,
            0,
            0,
            200,
            200,
            Some(0x100),
            true,
        );
        core.top_level_order = vec![0x100];
        core.shape_bounding.insert(
            0x100,
            vec![xfixes::RegionRect {
                x: 0,
                y: 0,
                width: 100,
                height: 200,
            }],
        );
        let on = build_with(
            Visibility::On,
            &core,
            &mut store,
            &windows,
            (0, 0, 800, 600),
            None,
        );
        let child = draws_of(&on, win_view(0x101));
        let child_area: u64 = child.iter().map(|r| area_of(*r)).sum();
        assert_eq!(child_area, 100 * 200, "child clipped to the parent's shape");
        assert!(
            child
                .iter()
                .all(|r| r.offset.x + i32::try_from(r.extent.width).unwrap() <= 200),
            "no child pixel outside the shaped half: {child:?}"
        );
        // The parent is entirely under its child within the shape: nothing left.
        assert!(draws_of(&on, win_view(0x100)).is_empty());
        let root: u64 = draws_of(&on, 0x00A0_7000).iter().map(|r| area_of(*r)).sum();
        assert_eq!(root, 800 * 600 - 100 * 200);
    }

    /// COW subtree above opaque windows: the COW claims nothing, so the windows
    /// below still emit in full, and the COW blends over them.
    #[test]
    fn a_cow_subtree_claims_nothing() {
        let mut core = KmsCore::for_tests();
        let mut store = DrawableStore::new();
        let mut windows = super::super::backend::WindowsMap::new();
        alloc_root(&core, &mut store, 800, 600);
        alloc_stub_window(
            &mut store,
            &mut windows,
            0x100,
            100,
            100,
            200,
            200,
            None,
            true,
        );
        alloc_stub_window(&mut store, &mut windows, 0x800, 0, 0, 800, 600, None, true);
        alloc_stub_window(
            &mut store,
            &mut windows,
            0x801,
            0,
            0,
            800,
            600,
            Some(0x800),
            true,
        );
        set_rank(&mut windows, 0x100, 1);
        set_rank(&mut windows, 0x800, 2);
        core.top_level_order = vec![0x100, 0x800];
        let on = assert_oracle(
            &core,
            &mut store,
            &windows,
            (0, 0, 800, 600),
            Some(0x800),
            "cow",
        );
        assert_eq!(
            draws_of(&on, win_view(0x100)).len(),
            1,
            "window under the COW emits whole"
        );
        let root: u64 = draws_of(&on, 0x00A0_7000).iter().map(|r| area_of(*r)).sum();
        assert_eq!(
            root,
            800 * 600 - 200 * 200,
            "root loses only the opaque window"
        );
        assert_eq!(on.stats.hidden_participants, 0);
    }

    /// A manual-redirected top-level emits nothing itself but its opaque
    /// automatic child claims through it: the root loses the child's area.
    #[test]
    fn an_opaque_automatic_child_claims_through_a_manual_parent() {
        let mut core = KmsCore::for_tests();
        let mut store = DrawableStore::new();
        let mut windows = super::super::backend::WindowsMap::new();
        alloc_root(&core, &mut store, 800, 600);
        alloc_stub_window(
            &mut store,
            &mut windows,
            0x600,
            50,
            400,
            200,
            100,
            None,
            true,
        );
        alloc_stub_window(
            &mut store,
            &mut windows,
            0x601,
            10,
            10,
            60,
            40,
            Some(0x600),
            true,
        );
        let m_id = store.lookup(0x600).unwrap();
        let m_backing = alloc_backing(&mut store, 0xB600, 200, 100);
        store.set_redirected_target(m_id, Some(m_backing));
        store.set_scene_participating(m_id, false);
        let a_id = store.lookup(0x601).unwrap();
        let a_backing = alloc_backing(&mut store, 0xB601, 60, 40);
        store.set_redirected_target(a_id, Some(a_backing));
        core.top_level_order = vec![0x600];
        let on = assert_oracle(
            &core,
            &mut store,
            &windows,
            (0, 0, 800, 600),
            None,
            "manual",
        );
        let backing_view = u64::from(0xB601u32) | 0xB000_0000;
        assert_eq!(draws_of(&on, backing_view).len(), 1);
        let root = draws_of(&on, 0x00A0_7000);
        let root_area: u64 = root.iter().map(|r| area_of(*r)).sum();
        assert_eq!(
            root_area,
            800 * 600 - 60 * 40,
            "root loses exactly the child's area"
        );
        assert!(
            root.iter()
                .all(|r| !rects_intersect(*r, audit_rect(60, 410, 60, 40))),
            "no root piece under the automatic child"
        );
    }

    /// Straddling window and non-zero layout origin: pieces sample the same
    /// texels as the unclipped draw (checked by the oracle) and lie on the
    /// output.
    #[test]
    fn straddling_windows_and_layout_origins_sample_the_right_texels() {
        let (core, mut store, windows) = two_windows((-50, -50, 100, 100), (700, 500, 200, 200));
        let on = assert_oracle(
            &core,
            &mut store,
            &windows,
            (0, 0, 800, 600),
            None,
            "straddle",
        );
        for d in &on.scene.draws {
            let r = draw_dst_rect_inward(d).unwrap();
            assert!(r.offset.x >= 0 && r.offset.y >= 0, "output-clipped: {r:?}");
            assert!(
                r.offset.x + i32::try_from(r.extent.width).unwrap() <= 800
                    && r.offset.y + i32::try_from(r.extent.height).unwrap() <= 600,
                "output-clipped: {r:?}"
            );
        }
        // The top-left straddler shows its bottom-right quarter: src starts at 0.5.
        let piece = on
            .scene
            .draws
            .iter()
            .find(|d| ash::vk::Handle::as_raw(d.image_view) == win_view(0x100))
            .expect("straddler emits");
        assert_eq!(piece.dst_origin, [0.0, 0.0]);
        assert_eq!(piece.src_origin, [0.5, 0.5]);
        assert_eq!(piece.src_size, [0.5, 0.5]);
        // Same tree on the second output of a side-by-side layout.
        let (core, mut store, windows) = two_windows((2500, 100, 120, 100), (2700, 300, 50, 50));
        let on = assert_oracle(
            &core,
            &mut store,
            &windows,
            (2560, 0, 640, 480),
            None,
            "x0=2560",
        );
        let piece = on
            .scene
            .draws
            .iter()
            .find(|d| ash::vk::Handle::as_raw(d.image_view) == win_view(0x100))
            .expect("emits");
        // Window at logical x=2500 is 60px off the left edge of this output.
        assert_eq!(piece.dst_origin, [0.0, 100.0]);
        assert_eq!(piece.src_origin, [0.5, 0.0]);
        assert_eq!(piece.src_size, [0.5, 1.0]);
    }

    /// A redirected window whose backing outgrew it (a shrink keeps the old
    /// backing): `src` divides by the SAMPLED source's extent, so the piece
    /// samples the window's texels at the backing's origin rather than
    /// stretching the whole backing. The pre-step-1 emitter divided by the
    /// host size — kept under `Off` only — so this case has no oracle and is
    /// asserted directly.
    #[test]
    fn a_redirected_backing_larger_than_its_host_is_sampled_unstretched() {
        let mut core = KmsCore::for_tests();
        let mut store = DrawableStore::new();
        let mut windows = super::super::backend::WindowsMap::new();
        alloc_root(&core, &mut store, 800, 600);
        alloc_stub_window(
            &mut store,
            &mut windows,
            0x700,
            100,
            100,
            80,
            60,
            None,
            true,
        );
        let r_id = store.lookup(0x700).unwrap();
        let backing = alloc_backing(&mut store, 0xB700, 160, 120); // 2× the host
        store.set_redirected_target(r_id, Some(backing));
        core.top_level_order = vec![0x700];
        let on = build_with(
            Visibility::On,
            &core,
            &mut store,
            &windows,
            (0, 0, 800, 600),
            None,
        );
        let d = on
            .scene
            .draws
            .iter()
            .find(|d| ash::vk::Handle::as_raw(d.image_view) == (u64::from(0xB700u32) | 0xB000_0000))
            .expect("redirected window emits its backing");
        assert_eq!(d.dst_origin, [100.0, 100.0]);
        assert_eq!(d.dst_size, [80.0, 60.0]);
        assert_eq!(d.src_origin, [0.0, 0.0]);
        assert_eq!(
            d.src_size,
            [0.5, 0.5],
            "80/160 × 60/120: the host's texels only"
        );
        // And the presence signature agrees with the unclipped draw.
        let p = on.participants.iter().find(|p| p.id.xid == 0x700).unwrap();
        assert_eq!(
            p.signature,
            PresenceSignature::new(d.image_view, d.src_origin, d.src_size, false)
        );
    }

    /// More than 32 opaque fragments over the root: the root's universe
    /// collapses to its bounding box (a superset), so the root over-emits —
    /// and the oracle still holds, because painter's order repaints the extra.
    #[test]
    fn a_collapsed_universe_over_emits_but_shows_the_same_pixels() {
        let mut core = KmsCore::for_tests();
        let mut store = DrawableStore::new();
        let mut windows = super::super::backend::WindowsMap::new();
        alloc_root(&core, &mut store, 800, 600);
        let mut rank = 1;
        for i in 0..7i16 {
            for j in 0..7i16 {
                let xid = 0x1000 + u32::try_from(i * 7 + j).unwrap();
                alloc_stub_window(
                    &mut store,
                    &mut windows,
                    xid,
                    20 + i * 100,
                    20 + j * 70,
                    40,
                    30,
                    None,
                    true,
                );
                set_rank(&mut windows, xid, rank);
                rank += 1;
                core.top_level_order.push(xid);
            }
        }
        let on = assert_oracle(
            &core,
            &mut store,
            &windows,
            (0, 0, 800, 600),
            None,
            "collapse",
        );
        assert!(
            on.stats.collapses() > 0,
            "49 disjoint claims must overflow the 32-box cap"
        );
        let root: u64 = draws_of(&on, 0x00A0_7000).iter().map(|r| area_of(*r)).sum();
        assert!(
            root > 800 * 600 - 49 * 40 * 30,
            "root over-emits after the collapse (superset), never under"
        );
        assert!(root <= 800 * 600);
    }

    /// Step 2 under step 1: a window hidden by an unrelated move above it
    /// contributes no structural damage of its own — the mover's old ∪ new
    /// covers it — and its rank is unchanged, so nothing reads as restacked.
    #[test]
    fn hiding_a_window_by_moving_another_over_it_damages_only_the_mover() {
        // Frame 1: B beside A. Frame 2: B moved onto A, covering it entirely.
        let (core, mut store, windows) = two_windows((100, 100, 50, 50), (400, 400, 100, 100));
        let before = build_with(
            Visibility::On,
            &core,
            &mut store,
            &windows,
            (0, 0, 800, 600),
            None,
        );
        let (core2, mut store2, windows2) = two_windows((100, 100, 50, 50), (80, 80, 100, 100));
        let after = build_with(
            Visibility::On,
            &core2,
            &mut store2,
            &windows2,
            (0, 0, 800, 600),
            None,
        );
        // Participant identity uses the store generation; both fixtures allocate
        // in the same order so the ids line up.
        assert_eq!(
            before.participants.iter().map(|p| p.id).collect::<Vec<_>>(),
            after.participants.iter().map(|p| p.id).collect::<Vec<_>>(),
            "same participants in the same (painter's) order — nothing restacked, \
             and the hidden window is still listed"
        );
        let damage = structural_damage(&before.participants, &after.participants);
        let mut expect = Region::from_rect(audit_rect(400, 400, 100, 100));
        expect.union_with(&Region::from_rect(audit_rect(80, 80, 100, 100)));
        assert_eq!(damage, expect, "exactly the mover's old ∪ new");
        assert!(
            after
                .participants
                .iter()
                .any(|p| p.id.xid == 0x100 && p.visible.is_empty()),
            "the covered window is present with an empty visible region"
        );
    }

    /// A pure restack of two overlapping top-levels owes only their overlap: the
    /// step-2 rule damages pairwise intersections of participants whose relative
    /// order flipped, not the whole region of everything whose rank moved.
    #[test]
    fn swapping_two_overlapping_top_levels_damages_only_their_overlap() {
        let lower = (100i16, 100i16, 300u16, 300u16);
        let upper = (250i16, 250i16, 300u16, 300u16);
        let (core, mut store, windows) = two_windows(lower, upper);
        let before = build_with(
            Visibility::On,
            &core,
            &mut store,
            &windows,
            (0, 0, 800, 600),
            None,
        );
        let (mut core2, mut store2, mut windows2) = two_windows(lower, upper);
        // Swap the stacking: 0x200 now below 0x100.
        set_rank(&mut windows2, 0x100, 2);
        set_rank(&mut windows2, 0x200, 1);
        core2.top_level_order = vec![0x200, 0x100];
        let after = build_with(
            Visibility::On,
            &core2,
            &mut store2,
            &windows2,
            (0, 0, 800, 600),
            None,
        );
        let damage = structural_damage(&before.participants, &after.participants);
        let overlap = Region::from_rect(audit_rect(250, 250, 150, 150));
        assert_eq!(damage, overlap, "exactly lower ∩ upper");
    }

    /// The `Off` scene keeps `visible == region` for every participant, so the
    /// audit's reference carries the same presences as before step 1.
    #[test]
    fn off_mode_presences_are_fully_visible() {
        let (core, mut store, windows) = differential_fixture();
        let off = build_with(
            Visibility::Off,
            &core,
            &mut store,
            &windows,
            (0, 0, 800, 600),
            None,
        );
        for p in &off.participants {
            assert_eq!(p.visible, p.region, "{:?}", p.id);
        }
        assert_eq!(off.stats.hidden_participants, 0);
        assert_eq!(off.stats.collapses(), 0);
    }

    // ── Step 1 stage C: content damage clipped to visibility ─────────────

    fn drawable_of(store: &DrawableStore, xid: u32) -> super::super::store::DrawableId {
        store.lookup(xid).expect("fixture window has a drawable")
    }

    fn projected_sorted(built: &SceneBuild) -> Vec<vk::Rect2D> {
        sorted_rects(built.projected_damage.rects().to_vec())
    }

    /// A paint into the covered part of a window changes no pixel on screen:
    /// it projects nothing, classifies `Hidden`, and must not force a compose.
    /// The snapshot is still carried (it acks if something else composes).
    #[test]
    fn hidden_paint_projects_nothing_and_does_not_force() {
        // Lower 0x100 fully under upper 0x200.
        let (core, mut store, windows) = two_windows((100, 100, 50, 50), (80, 80, 100, 100));
        let lower = drawable_of(&store, 0x100);
        store.damage(lower, rect(5, 5, 20, 20));
        let built = build_with(
            Visibility::On,
            &core,
            &mut store,
            &windows,
            (0, 0, 800, 600),
            None,
        );
        assert!(
            built.projected_damage.is_empty(),
            "hidden paint projected {:?}",
            built.projected_damage.rects()
        );
        assert_eq!(built.stats.content_hidden, 1);
        assert_eq!(built.stats.content_off_output, 0);
        assert_eq!(built.stats.content_visible, 0);
        assert!(
            !built.stats.off_output_damage_forces_compose(),
            "hidden damage must take the EmptyDamage skip, not force a Full compose"
        );
        assert!(
            !built.snapshots.iter().any(|s| s.id == lower),
            "a hidden snapshot must NOT ride this output's build: it was not presented \
             here, and carrying it would let this output's retire ack it globally \
             (the multi-output ack race, 2026-09-04)"
        );
        // Un-acked: the store still holds it for the next walk.
        assert!(
            !store
                .peek_presentation_damage(lower)
                .unwrap()
                .region
                .is_empty()
        );
    }

    /// Codex, post-merge review of `02bafec3` (finding 1): a drawable whose
    /// damage classified `Hidden` was still counted as drawn, so
    /// `reconcile_offscreen_no_draw` never flagged it and
    /// `has_pending_presentation_damage` kept waking the tick — ~1850 walks/s
    /// at 2 composes/s with mpv under a terminal. The presented set must leave
    /// it out; a drawable with visible damage stays in.
    #[test]
    fn hidden_damage_is_not_presented_so_the_scheduler_can_go_dormant() {
        // Lower 0x100 fully under upper 0x200; both painted.
        let (core, mut store, windows) = two_windows((100, 100, 50, 50), (80, 80, 100, 100));
        let lower = drawable_of(&store, 0x100);
        let upper = drawable_of(&store, 0x200);
        store.damage(lower, rect(5, 5, 20, 20));
        store.damage(upper, rect(1, 1, 5, 5));
        let built = build_with(
            Visibility::On,
            &core,
            &mut store,
            &windows,
            (0, 0, 800, 600),
            None,
        );
        assert_eq!(built.stats.content_hidden, 1);
        assert!(
            built.sampled_ids.contains(&lower),
            "sampling bookkeeping is unchanged: the hidden node is still a participant"
        );
        assert!(
            !built.presented_ids.contains(&lower),
            "hidden damage was not presented: {:?}",
            built.presented_ids
        );
        assert!(built.presented_ids.contains(&upper));
        assert!(
            !built.pieces_ids.contains(&lower),
            "fully covered: no pieces either ⇒ NoPieces, stays dormant across paints"
        );
        let drawn: std::collections::HashSet<_> = built.presented_ids.iter().copied().collect();
        let pieces: std::collections::HashSet<_> = built.pieces_ids.iter().copied().collect();
        store.reconcile_offscreen_no_draw(&drawn, &pieces);
        assert_eq!(
            store.get(lower).unwrap().dormant,
            Some(super::super::store::DormantReason::NoPieces),
            "flagged out of the scheduler"
        );
        assert!(store.get(upper).unwrap().dormant.is_none());
        assert!(
            !store
                .peek_presentation_damage(lower)
                .unwrap()
                .region
                .is_empty(),
            "damage is preserved, only the flag changes"
        );

        // A PARTIALLY covered node whose damage lies entirely under the cover:
        // it emits pieces (it is drawn) but presented nothing of its paint.
        let (core, mut store, windows) = two_windows((100, 100, 200, 200), (200, 100, 200, 200));
        let lower = drawable_of(&store, 0x100);
        // Storage-local x 150..190 → output x 250..290, under the cover (x ≥ 200).
        store.damage(lower, rect(150, 50, 40, 20));
        let built = build_with(
            Visibility::On,
            &core,
            &mut store,
            &windows,
            (0, 0, 800, 600),
            None,
        );
        assert_eq!(built.stats.content_hidden, 1);
        assert!(built.stats.draws_emitted > 0);
        assert!(built.sampled_ids.contains(&lower));
        assert!(!built.presented_ids.contains(&lower));
        assert!(
            built.pieces_ids.contains(&lower),
            "partially covered: it emitted pieces ⇒ HiddenDamage, re-armed by the next paint"
        );
        let drawn: std::collections::HashSet<_> = built.presented_ids.iter().copied().collect();
        let pieces: std::collections::HashSet<_> = built.pieces_ids.iter().copied().collect();
        store.reconcile_offscreen_no_draw(&drawn, &pieces);
        assert_eq!(
            store.get(lower).unwrap().dormant,
            Some(super::super::store::DormantReason::HiddenDamage)
        );
        assert!(
            !store.has_pending_presentation_damage(),
            "nothing presentable is pending: the scheduler must go dormant"
        );
        // The next paint lands in the VISIBLE part (storage-local x 10..40 →
        // output 110..140, left of the cover): it must re-arm and present.
        store.damage(lower, rect(10, 10, 30, 30));
        assert!(
            store.has_pending_presentation_damage(),
            "a paint into a HiddenDamage-dormant window re-arms the scheduler"
        );
        let built = build_with(
            Visibility::On,
            &core,
            &mut store,
            &windows,
            (0, 0, 800, 600),
            None,
        );
        assert_eq!(built.stats.content_visible, 1);
        assert!(built.presented_ids.contains(&lower));
    }

    /// Two outputs: hidden on output 0, visible on output 1. The union of the
    /// two outputs' presented sets contains it, so the drawable stays armed and
    /// output 1 composes it.
    #[test]
    fn damage_visible_on_one_output_keeps_the_drawable_armed() {
        let (core, mut store, windows) = two_windows((700, 100, 200, 100), (650, 0, 150, 600));
        let w = drawable_of(&store, 0x100);
        store.damage(w, rect(0, 0, 200, 100));
        let out0 = build_with(
            Visibility::On,
            &core,
            &mut store,
            &windows,
            (0, 0, 800, 600),
            None,
        );
        let out1 = build_with(
            Visibility::On,
            &core,
            &mut store,
            &windows,
            (800, 0, 800, 600),
            None,
        );
        assert_eq!(out0.stats.content_hidden, 1);
        assert!(!out0.presented_ids.contains(&w));
        assert_eq!(out1.stats.content_visible, 1);
        assert!(out1.presented_ids.contains(&w));
        let mut drawn: std::collections::HashSet<_> = out0.presented_ids.iter().copied().collect();
        drawn.extend(out1.presented_ids.iter().copied());
        let mut pieces: std::collections::HashSet<_> = out0.pieces_ids.iter().copied().collect();
        pieces.extend(out1.pieces_ids.iter().copied());
        store.reconcile_offscreen_no_draw(&drawn, &pieces);
        assert!(store.get(w).unwrap().dormant.is_none());
        assert!(store.has_pending_presentation_damage());
    }

    /// Paint straddling a cover's edge projects only the visible side.
    #[test]
    fn paint_across_a_cover_edge_projects_only_the_visible_side() {
        // Lower 0x100 at x 100..300; upper 0x200 covers x 200..400.
        let (core, mut store, windows) = two_windows((100, 100, 200, 200), (200, 100, 200, 200));
        let lower = drawable_of(&store, 0x100);
        // Storage-local (50,50)+(100x20) → output (150,150)+(100x20); visible x 150..200.
        store.damage(lower, rect(50, 50, 100, 20));
        let built = build_with(
            Visibility::On,
            &core,
            &mut store,
            &windows,
            (0, 0, 800, 600),
            None,
        );
        assert_eq!(projected_sorted(&built), vec![rect(150, 150, 50, 20)]);
        assert_eq!(built.stats.content_visible, 1);
        assert_eq!(built.stats.content_hidden, 0);
        // Paint entirely in the visible half projects whole.
        let (core, mut store, windows) = two_windows((100, 100, 200, 200), (200, 100, 200, 200));
        let lower = drawable_of(&store, 0x100);
        store.damage(lower, rect(10, 10, 30, 30));
        let built = build_with(
            Visibility::On,
            &core,
            &mut store,
            &windows,
            (0, 0, 800, 600),
            None,
        );
        assert_eq!(projected_sorted(&built), vec![rect(110, 110, 30, 30)]);
    }

    /// The xfce-submenu case is pinned: a paint whose projection misses the
    /// output entirely still forces a compose, so the snapshot can ack.
    #[test]
    fn off_output_paint_still_forces_a_compose() {
        // 0x100 straddles the right edge: x 750..850 on an 800-wide output.
        let (core, mut store, windows) = two_windows((750, 100, 100, 50), (10, 10, 20, 20));
        let w = drawable_of(&store, 0x100);
        // Storage-local x 60..90 → output x 810..840: off the output.
        store.damage(w, rect(60, 5, 30, 10));
        let built = build_with(
            Visibility::On,
            &core,
            &mut store,
            &windows,
            (0, 0, 800, 600),
            None,
        );
        assert!(built.projected_damage.is_empty());
        assert_eq!(built.stats.content_off_output, 1);
        assert_eq!(built.stats.content_hidden, 0);
        assert!(built.stats.off_output_damage_forces_compose());
        // And a paint on the on-output part of the same window is Visible.
        let (core, mut store, windows) = two_windows((750, 100, 100, 50), (10, 10, 20, 20));
        let w = drawable_of(&store, 0x100);
        store.damage(w, rect(10, 5, 30, 10));
        let built = build_with(
            Visibility::On,
            &core,
            &mut store,
            &windows,
            (0, 0, 800, 600),
            None,
        );
        assert_eq!(projected_sorted(&built), vec![rect(760, 105, 30, 10)]);
        assert_eq!(built.stats.content_visible, 1);
        assert!(!built.stats.off_output_damage_forces_compose());
    }

    /// Hidden damage is not acked; it accumulates and shows when uncovered.
    /// Frame 1: W paints under A (Hidden). Frame 2: A moves away — A's
    /// structural old ∪ new covers W, and W's accumulated damage projects.
    #[test]
    fn uncovering_a_window_surfaces_its_accumulated_hidden_paint() {
        let (core, mut store, mut windows) = two_windows((100, 100, 50, 50), (80, 80, 100, 100));
        let lower = drawable_of(&store, 0x100);
        store.damage(lower, rect(5, 5, 20, 20));
        let frame1 = build_with(
            Visibility::On,
            &core,
            &mut store,
            &windows,
            (0, 0, 800, 600),
            None,
        );
        assert!(frame1.projected_damage.is_empty());
        assert_eq!(frame1.stats.content_hidden, 1);
        // Not acked (no compose happened): a second hidden paint accumulates.
        store.damage(lower, rect(30, 30, 10, 10));
        // Frame 2: A moves off W.
        let a = windows.get_mut(&0x200).expect("A");
        a.x = 400;
        a.y = 400;
        let frame2 = build_with(
            Visibility::On,
            &core,
            &mut store,
            &windows,
            (0, 0, 800, 600),
            None,
        );
        assert_eq!(
            projected_sorted(&frame2),
            vec![rect(105, 105, 20, 20), rect(130, 130, 10, 10)],
            "both accumulated paints project once W is visible"
        );
        assert_eq!(frame2.stats.content_visible, 1);
        let structural = structural_damage(&frame1.participants, &frame2.participants);
        assert!(
            structural.contains_rect(rect(100, 100, 50, 50)),
            "the mover's old ∪ new covers the uncovered window: {structural:?}"
        );
    }

    /// Two outputs, one store: W hidden on output 0 is visible on output 1.
    /// Output 0 classifies Hidden and does not force; output 1 projects it. No
    /// ack happens on the hidden side, which is what keeps output 1 correct.
    #[test]
    fn hidden_on_one_output_visible_on_the_other() {
        // W 0x100 at x 700..900 spans both outputs; A 0x200 covers x 650..800.
        let (core, mut store, windows) = two_windows((700, 100, 200, 100), (650, 0, 150, 600));
        let w = drawable_of(&store, 0x100);
        store.damage(w, rect(0, 0, 200, 100));
        let out0 = build_with(
            Visibility::On,
            &core,
            &mut store,
            &windows,
            (0, 0, 800, 600),
            None,
        );
        assert!(out0.projected_damage.is_empty());
        assert_eq!(out0.stats.content_hidden, 1);
        assert!(!out0.stats.off_output_damage_forces_compose());
        // Still in the store for output 1's walk.
        assert!(!store.peek_presentation_damage(w).unwrap().region.is_empty());
        let out1 = build_with(
            Visibility::On,
            &core,
            &mut store,
            &windows,
            (800, 0, 800, 600),
            None,
        );
        assert_eq!(projected_sorted(&out1), vec![rect(0, 100, 100, 100)]);
        assert_eq!(out1.stats.content_visible, 1);
        // Only the presenting output carries the snapshot into its PendingAck.
        assert!(
            !out0.snapshots.iter().any(|s| s.id == w),
            "output 0 (hidden) must not carry W's snapshot"
        );
        assert!(
            out1.snapshots
                .iter()
                .any(|s| s.id == w && !s.region.is_empty()),
            "output 1 (visible) carries it and acks it at its retire"
        );
    }

    // ── dormancy across outputs that did not walk ────────────────────────

    fn set(ids: &[u64]) -> std::collections::HashSet<super::super::store::DrawableId> {
        ids.iter()
            .map(|i| super::super::store::DrawableId::for_tests(*i))
            .collect()
    }

    /// Hidden on output 0 (walked), but output 1 skipped this tick and its
    /// retained pieces include the drawable: it may present it once it walks,
    /// so it stays armed.
    #[test]
    fn dormancy_keeps_a_drawable_armed_when_a_skipped_output_may_present_it() {
        let none = set(&[]);
        let out0_pieces = set(&[7, 9]);
        let out0_presented = set(&[9]);
        let out1_last = set(&[7]);
        let reports = [
            OutputWalkReport {
                walked: true,
                presented: &out0_presented,
                last_pieces: &out0_pieces,
            },
            OutputWalkReport {
                walked: false,
                presented: &none,
                last_pieces: &out1_last,
            },
        ];
        let (keep_armed, pieces) = dormancy_inputs(&reports);
        assert!(keep_armed.contains(&set(&[7]).into_iter().next().unwrap()));
        assert!(keep_armed.contains(&set(&[9]).into_iter().next().unwrap()));
        assert!(pieces.contains(&set(&[7]).into_iter().next().unwrap()));
    }

    /// Same, but output 1 walked and did not present it either: dormant with
    /// reason `HiddenDamage`, since it has pieces.
    #[test]
    fn dormancy_flags_hidden_damage_when_every_walked_output_declined_it() {
        let out0_pieces = set(&[7, 9]);
        let out0_presented = set(&[9]);
        let out1_pieces = set(&[7]);
        let out1_presented = set(&[]);
        let reports = [
            OutputWalkReport {
                walked: true,
                presented: &out0_presented,
                last_pieces: &out0_pieces,
            },
            OutputWalkReport {
                walked: true,
                presented: &out1_presented,
                last_pieces: &out1_pieces,
            },
        ];
        let (keep_armed, pieces) = dormancy_inputs(&reports);
        let seven = set(&[7]).into_iter().next().unwrap();
        assert!(!keep_armed.contains(&seven));
        assert!(pieces.contains(&seven), "⇒ HiddenDamage");
    }

    /// In no output's pieces, output 1 skipped: `NoPieces`.
    #[test]
    fn dormancy_flags_no_pieces_when_no_output_shows_it() {
        let out0_pieces = set(&[9]);
        let out0_presented = set(&[9]);
        let none = set(&[]);
        let out1_last = set(&[11]);
        let reports = [
            OutputWalkReport {
                walked: true,
                presented: &out0_presented,
                last_pieces: &out0_pieces,
            },
            OutputWalkReport {
                walked: false,
                presented: &none,
                last_pieces: &out1_last,
            },
        ];
        let (keep_armed, pieces) = dormancy_inputs(&reports);
        let seven = set(&[7]).into_iter().next().unwrap();
        assert!(!keep_armed.contains(&seven));
        assert!(!pieces.contains(&seven), "⇒ NoPieces");
        // And 11, shown only on the skipped output, stays armed.
        assert!(keep_armed.contains(&set(&[11]).into_iter().next().unwrap()));
    }

    /// The hardware case (silence/MATE 2026-09-04): the root's damage lies
    /// under covers on both outputs; output 1 keeps skipping as NothingPending.
    /// After output 0's walk alone the root must go dormant (`HiddenDamage`:
    /// it has pieces on both), or it is re-peeked and re-classified Hidden
    /// ~1000×/s forever.
    #[test]
    fn dormancy_runs_without_every_output_walking() {
        let root = set(&[1]);
        let out0_pieces = set(&[1, 5]);
        let out0_presented = set(&[5]);
        let none = set(&[]);
        let out1_last = set(&[1, 6]);
        let reports = [
            OutputWalkReport {
                walked: true,
                presented: &out0_presented,
                last_pieces: &out0_pieces,
            },
            OutputWalkReport {
                walked: false,
                presented: &none,
                last_pieces: &out1_last,
            },
        ];
        let (keep_armed, pieces) = dormancy_inputs(&reports);
        let one = root.into_iter().next().unwrap();
        // Output 1 has pieces for the root and did not walk ⇒ it MAY present
        // it ⇒ armed. That is the sound rule; what makes it terminate on
        // hardware is that output 1's predicate then walks (root armed and in
        // its last_pieces), declines it (Hidden), and the NEXT reconciliation
        // sees both outputs decline ⇒ dormant.
        assert!(keep_armed.contains(&one));
        let out1_pieces = set(&[1, 6]);
        let out1_presented = set(&[6]);
        let reports = [
            OutputWalkReport {
                walked: true,
                presented: &out0_presented,
                last_pieces: &out0_pieces,
            },
            OutputWalkReport {
                walked: true,
                presented: &out1_presented,
                last_pieces: &out1_pieces,
            },
        ];
        let (keep_armed, pieces2) = dormancy_inputs(&reports);
        assert!(!keep_armed.contains(&one));
        assert!(pieces2.contains(&one), "⇒ HiddenDamage");
        let _ = pieces;
    }

    /// The multi-output ack race, first half (silence/MATE, 2026-09-04): a
    /// paint into a window spanning both outputs, landing inside output 0 only.
    /// Output 1 knows (from output 0's retained pieces) that the window is shown
    /// there, so it classifies `OtherOutput`: no force, no snapshot, not
    /// presented — output 0 owns that damage. Before this rule output 1 read it
    /// as `OffOutput`, forced a Full compose, and its retire acked the damage
    /// before output 0 had composed it.
    #[test]
    fn other_output_damage_is_neither_forced_nor_carried_here() {
        // W 0x100 at x 700..900 spans the boundary at 800; 0x200 is far away.
        let (core, mut store, windows) = two_windows((700, 100, 200, 100), (0, 0, 10, 10));
        let w = drawable_of(&store, 0x100);
        // Storage-local x 0..50 → output-0 x 700..750 only.
        store.damage(w, rect(0, 0, 50, 100));
        let out0 = build_with(
            Visibility::On,
            &core,
            &mut store,
            &windows,
            (0, 0, 800, 600),
            None,
        );
        assert_eq!(out0.stats.content_visible, 1);
        assert!(
            out0.snapshots
                .iter()
                .any(|s| s.id == w && !s.region.is_empty())
        );
        assert!(out0.presented_ids.contains(&w));
        let elsewhere: std::collections::HashSet<_> = out0.pieces_ids.iter().copied().collect();
        let out1 = build_with_elsewhere(
            Visibility::On,
            &core,
            &mut store,
            &windows,
            (800, 0, 800, 600),
            None,
            &elsewhere,
        );
        assert_eq!(out1.stats.content_other_output, 1, "{:?}", out1.stats);
        assert_eq!(out1.stats.content_off_output, 0);
        assert!(
            !out1.stats.off_output_damage_forces_compose(),
            "damage another output presents must not force a Full compose here"
        );
        assert!(
            !out1.snapshots.iter().any(|s| s.id == w),
            "output 1 must not carry (and later ack) damage it did not present"
        );
        assert!(!out1.presented_ids.contains(&w));
        assert!(
            out1.pieces_ids.contains(&w),
            "W's right half is visible on output 1, so it has pieces there"
        );
        assert!(out1.projected_damage.is_empty());
    }

    /// The xfce-submenu rule, pinned: an off-output paint still FORCES a
    /// compose here, so a paint whose projection is empty is not left
    /// undrained. It is deliberately **not carried and not presented** — the
    /// forced compose displays none of those pixels, and carrying the snapshot
    /// is what let a cold `elsewhere` turn this branch into the multi-output
    /// ack race (2026-09-04: caja's spanning desktop, 247 unhealed audit
    /// mismatches). The drain comes from dormancy instead: not presented ⇒
    /// dormant ⇒ no re-forcing until the next paint. A window entirely off the
    /// output never reaches the classifier at all (the intersects gate), so the
    /// fixture is a spanning window whose damage projects off output 0.
    #[test]
    fn damage_off_output_forces_a_compose_but_is_never_carried() {
        let (core, mut store, windows) = two_windows((700, 100, 200, 100), (0, 0, 10, 10));
        let w = drawable_of(&store, 0x100);
        // Storage-local x 150..200 → x 850..900: off output 0, on output 1.
        store.damage(w, rect(150, 0, 50, 100));
        let nowhere = std::collections::HashSet::new();
        let out0 = build_with_elsewhere(
            Visibility::On,
            &core,
            &mut store,
            &windows,
            (0, 0, 800, 600),
            None,
            &nowhere,
        );
        assert_eq!(out0.stats.content_off_output, 1, "{:?}", out0.stats);
        assert_eq!(out0.stats.content_other_output, 0);
        assert!(
            out0.stats.off_output_damage_forces_compose(),
            "an empty projection must still force a compose (xfce submenu)"
        );
        assert!(
            !out0.snapshots.iter().any(|s| s.id == w),
            "the forced compose shows none of those pixels, so it must not ack them"
        );
        assert!(
            !out0.presented_ids.contains(&w),
            "not presented ⇒ dormancy stops the forcing until the next paint"
        );
        // Once output 1's pieces are known, the same paint is output 1's.
        let out1 = build_with_elsewhere(
            Visibility::On,
            &core,
            &mut store,
            &windows,
            (800, 0, 800, 600),
            None,
            &nowhere,
        );
        assert_eq!(out1.stats.content_visible, 1);
        let elsewhere: std::collections::HashSet<_> = out1.pieces_ids.iter().copied().collect();
        let out0 = build_with_elsewhere(
            Visibility::On,
            &core,
            &mut store,
            &windows,
            (0, 0, 800, 600),
            None,
            &elsewhere,
        );
        assert_eq!(out0.stats.content_other_output, 1);
        assert!(!out0.stats.off_output_damage_forces_compose());
        assert!(!out0.snapshots.iter().any(|s| s.id == w));
    }

    /// The multi-output ack race, second half: output 0 is flip-pending when
    /// the paint lands, so only output 1 walks; output 1 composes for its own
    /// reasons, retires, and acks what it carried. W's damage must survive that
    /// ack, and output 0's next walk must project it.
    #[test]
    fn an_output_never_acks_damage_it_did_not_present() {
        let (core, mut store, windows) = two_windows((700, 100, 200, 100), (0, 0, 10, 10));
        let w = drawable_of(&store, 0x100);
        // Output 0's most recent walk saw W (its retained pieces); no paint yet.
        let warm = build_with(
            Visibility::On,
            &core,
            &mut store,
            &windows,
            (0, 0, 800, 600),
            None,
        );
        let elsewhere: std::collections::HashSet<_> = warm.pieces_ids.iter().copied().collect();
        assert!(elsewhere.contains(&w));
        // The paint lands while output 0 is flip-pending: only output 1 walks.
        store.damage(w, rect(0, 0, 50, 100));
        let out1 = build_with_elsewhere(
            Visibility::On,
            &core,
            &mut store,
            &windows,
            (800, 0, 800, 600),
            None,
            &elsewhere,
        );
        // Output 1 composes (say, for its own cursor) and retires: it acks
        // exactly what it carried.
        for snap in out1.snapshots {
            store.ack_presentation_damage(snap);
        }
        assert!(
            !store.peek_presentation_damage(w).unwrap().region.is_empty(),
            "output 1 never presented W's damage, so its retire must not have acked it"
        );
        // Output 0 retires and walks: the highlight is still there to compose.
        let out0 = build_with(
            Visibility::On,
            &core,
            &mut store,
            &windows,
            (0, 0, 800, 600),
            None,
        );
        assert_eq!(projected_sorted(&out0), vec![rect(700, 100, 50, 100)]);
        assert!(
            out0.snapshots
                .iter()
                .any(|s| s.id == w && !s.region.is_empty())
        );
    }

    /// `Off` keeps the unclipped projection: what the legacy emitter damaged.
    #[test]
    fn off_mode_projection_is_unchanged_by_stage_c() {
        let (core, mut store, windows) = differential_fixture();
        // Paint into several windows, including ones the fixture covers.
        let xids: Vec<u32> = windows.keys().copied().collect();
        for (i, xid) in xids.iter().enumerate() {
            if let Some(id) = store.lookup(*xid) {
                #[allow(clippy::cast_possible_truncation)]
                let k = (i % 7) as i32;
                store.damage(id, rect(k, k, 8 + k as u32, 6));
            }
        }
        for layout in [(0, 0, 800, 600), (100, 50, 800, 600)] {
            let legacy = walk_with(true, &core, &mut store, &windows, layout, None);
            let off = walk_with(false, &core, &mut store, &windows, layout, None);
            assert!(
                !legacy.projected.is_empty(),
                "fixture damage must project at layout {layout:?}"
            );
            assert_eq!(off.projected, legacy.projected, "layout {layout:?}");
        }
        // And `On` projects a subset of `Off` (never more).
        let off = build_with(
            Visibility::Off,
            &core,
            &mut store,
            &windows,
            (0, 0, 800, 600),
            None,
        );
        let on = build_with(
            Visibility::On,
            &core,
            &mut store,
            &windows,
            (0, 0, 800, 600),
            None,
        );
        let off_region = Region::from_rects(off.projected_damage.rects().iter().copied());
        for r in on.projected_damage.rects() {
            assert!(
                off_region.contains_rect(*r),
                "On projected {r:?} outside Off"
            );
        }
    }

    #[test]
    fn only_off_output_damage_forces() {
        let mut s = WalkStats::default();
        assert!(!s.off_output_damage_forces_compose());
        s.content_hidden = 5;
        s.content_visible = 2;
        assert!(!s.off_output_damage_forces_compose());
        s.content_off_output = 1;
        assert!(s.off_output_damage_forces_compose());
    }

    #[test]
    fn intersect_rects_clips_and_rejects_disjoint() {
        assert_eq!(
            intersect_rects(rect(0, 0, 10, 10), rect(5, 5, 10, 10)),
            Some(rect(5, 5, 5, 5))
        );
        assert_eq!(intersect_rects(rect(0, 0, 10, 10), rect(10, 0, 5, 5)), None);
        assert_eq!(
            intersect_rects(rect(0, 0, 10, 10), rect(-5, -5, 30, 30)),
            Some(rect(0, 0, 10, 10))
        );
    }

    // ── Step 1 stage B: walk cost bench ──────────────────────────────────

    /// An e16-like tree on one 2560×1440 output: 10 unshaped top-levels, each
    /// with 6 shaped leaf children of 8 rects, plus one large opaque window
    /// covering half the screen, over a root.
    fn e16_like_fixture() -> (KmsCore, DrawableStore, super::super::backend::WindowsMap) {
        let mut core = KmsCore::for_tests();
        let mut store = DrawableStore::new();
        let mut windows = super::super::backend::WindowsMap::new();
        alloc_root(&core, &mut store, 2560, 1440);
        let mut rank = 1u64;
        for t in 0..10i16 {
            let top = 0x1000 + u32::try_from(t).unwrap() * 0x10;
            let (tx, ty) = (40 + (t % 5) * 480, 60 + (t / 5) * 640);
            alloc_stub_window(&mut store, &mut windows, top, tx, ty, 440, 560, None, true);
            set_rank(&mut windows, top, rank);
            rank += 1;
            core.top_level_order.push(top);
            for c in 0..6i16 {
                let child = top + 1 + u32::try_from(c).unwrap();
                let (cx, cy) = (10 + (c % 3) * 140, 20 + (c / 3) * 260);
                alloc_stub_window(
                    &mut store,
                    &mut windows,
                    child,
                    cx,
                    cy,
                    128,
                    240,
                    Some(top),
                    true,
                );
                set_rank(&mut windows, child, rank);
                rank += 1;
                // A frame-like shape: 4 edges + 4 corner nubs, all disjoint.
                core.shape_bounding.insert(
                    child,
                    vec![
                        xfixes::RegionRect {
                            x: 0,
                            y: 0,
                            width: 128,
                            height: 8,
                        },
                        xfixes::RegionRect {
                            x: 0,
                            y: 232,
                            width: 128,
                            height: 8,
                        },
                        xfixes::RegionRect {
                            x: 0,
                            y: 8,
                            width: 8,
                            height: 224,
                        },
                        xfixes::RegionRect {
                            x: 120,
                            y: 8,
                            width: 8,
                            height: 224,
                        },
                        xfixes::RegionRect {
                            x: 8,
                            y: 8,
                            width: 16,
                            height: 16,
                        },
                        xfixes::RegionRect {
                            x: 104,
                            y: 8,
                            width: 16,
                            height: 16,
                        },
                        xfixes::RegionRect {
                            x: 8,
                            y: 216,
                            width: 16,
                            height: 16,
                        },
                        xfixes::RegionRect {
                            x: 104,
                            y: 216,
                            width: 16,
                            height: 16,
                        },
                    ],
                );
            }
        }
        // The big opaque window on top, covering the right half.
        alloc_stub_window(
            &mut store,
            &mut windows,
            0x9000,
            1280,
            0,
            1280,
            1440,
            None,
            true,
        );
        set_rank(&mut windows, 0x9000, rank);
        core.top_level_order.push(0x9000);
        (core, store, windows)
    }

    /// `cargo test --release -p yserver -- --ignored walk_bench --nocapture`
    #[test]
    #[ignore = "bench: prints µs per walk, run in release with --nocapture"]
    fn walk_bench() {
        let (core, mut store, windows) = e16_like_fixture();
        let layout = (0, 0, 2560u32, 1440u32);
        let platform = platform_with_layout(layout);
        let run = |mode: Visibility, store: &mut DrawableStore| {
            build_scene(
                &core, store, &windows, 0, &platform, None, None, None, false, mode,
            )
        };
        let warm = run(Visibility::On, &mut store);
        eprintln!("walk_bench: collapse split {:?}", warm.stats);
        eprintln!(
            "walk_bench: nodes={} draws={} hidden={} collapses={} (off draws={})",
            warm.stats.nodes_visited,
            warm.scene.draws.len(),
            warm.stats.hidden_participants,
            warm.stats.collapses(),
            run(Visibility::Off, &mut store).scene.draws.len(),
        );
        for mode in [Visibility::Off, Visibility::On] {
            let mut best = f64::MAX;
            for _ in 0..5 {
                let start = std::time::Instant::now();
                for _ in 0..1000 {
                    let b = run(mode, &mut store);
                    std::hint::black_box(&b);
                }
                let per = start.elapsed().as_secs_f64() * 1e6 / 1000.0;
                best = best.min(per);
            }
            eprintln!("walk_bench: {mode:?}: best of 5 runs = {best:.1} µs/walk");
        }
    }
}
