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

mod build;
mod compose;
mod cursor;
mod damage;
mod damage_audit;
mod flip;
mod for_tests;
mod lifecycle;
mod repaint;
mod root_readback;
mod tick;
mod transform;
mod walk;

pub(crate) use build::*;
use compose::*;
use cursor::*;
use damage::*;
use damage_audit::*;
use flip::*;
#[cfg(test)]
use lifecycle::drain_deferred_scene_resources;
use repaint::*;
use tick::*;
use transform::*;
use walk::*;

#[cfg(test)]
mod tests;

use std::{
    collections::{HashMap, HashSet, VecDeque},
    io,
    panic::Location,
    sync::{Arc, OnceLock},
};

use ash::vk;
use yserver_protocol::x11::xfixes;

use super::{
    cursor_save::{CursorSaveTarget, CursorSaves},
    platform::{FenceTicket, PlatformBackend, ReadyScanoutRenderCompletion},
    region::Region,
    scanout_damage::ScanoutDamage,
    scene_diff::{
        ParticipantId, PresenceSignature, ScenePresence, SceneRole, presence_from_place,
        structural_damage,
    },
    store::{DamageSnapshot, DrawableKind, DrawableStore, RegionSet},
    telemetry::Telemetry,
    transform_intermediate::TransformIntermediate,
};
use crate::kms::{
    core::KmsCore,
    render::composite_pool_ring::CompositePoolRing,
    vk::{
        compositor::{CompositeDraw, CompositeScene, PresentError},
        damage_audit_compare::{DamageAuditComparePipeline, DamageAuditTileSummary},
        pipeline::{CompositePushConsts, CompositorPipeline, MAX_DESCRIPTOR_SETS_PER_FRAME},
        scale_pipeline::{ScalePassPipeline, ScalePushConsts},
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

enum CopiedRenderSubmitError {
    RendererAcquire(io::Error),
    Present(PresentError),
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

/// Ring of recent output-damage regions keyed by generation.
/// Depth = max(scanout_bo_count) + 1 per Stage 2 plan
/// cross-cutting §"BufferAgeRing".
pub(crate) struct BufferAgeRing {
    entries: VecDeque<(u64, RegionSet)>,
    depth: usize,
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
    last_pieces: std::collections::HashSet<crate::kms::render::store::DrawableId>,
    /// The presentation-damage epoch of every drawable this output's most
    /// recent submitted compose carried. Another output's compose of a
    /// drawable at a newer epoch hands this one the damage
    /// ([`fan_out_carried_damage`]), because that output's retire acks it in
    /// the store for everyone.
    presented_epochs: std::collections::HashMap<crate::kms::render::store::DrawableId, u64>,
    /// The footprint-sized image a RANDR-transformed output composites into
    /// before the scale pass; `None` at identity (spec D4).
    intermediate: Option<TransformIntermediate>,
    /// The transform the scanout BOs were laid out for: a same-footprint
    /// rotation or reflection change keeps every extent but not the pixels.
    transform: Option<yserver_core::randr::CrtcTransform>,
    /// The pixels under the software cursor in each compose image, for root
    /// reads. Empty while the cursor is on the HW plane.
    cursor_saves: CursorSaves,
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
    /// See `TransferResources::timestamps_written`.
    timestamps_written: bool,
    last_gpu_render_ns: Option<u64>,
}

struct DamageAuditLedgerEntry {
    id: u64,
    site: &'static Location<'static>,
    expected_area: Vec<vk::Rect2D>,
    contributed_outputs: Vec<usize>,
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
    pub(crate) root_overlay: crate::kms::render::root_overlay::RootOverlay,
    /// Stage 2d's coarse scene-structure dirty bit. Set by any
    /// map/unmap/configure/restack/redirect-state/cursor-pos
    /// change. Cleared at tick end. Stage 2e narrows to a
    /// per-region scene_structure_damage `RegionSet`.
    pub(crate) scene_structure_dirty: bool,
    /// Counts the changes that set `scene_structure_dirty`: with the
    /// store's [`DrawableStore::scene_damage_generation`], what a root
    /// readback was composed at ([`Self::root_readback`]).
    structure_generation: u64,
    /// Test-only override for [`has_pending_page_flips`](Self::has_pending_page_flips).
    /// `KmsBackend::for_tests()` builds a `stub()` scene with `inner: None`,
    /// so there is no live `PendingAck` queue to populate; this lets
    /// capability-surface tests (`present_flip_in_flight`) exercise both
    /// states without a live Vulkan device.
    #[cfg(test)]
    test_flip_in_flight_override: Option<bool>,
    /// Test-only: the most draws a priming compose reports recorded, standing
    /// in for an exhausted descriptor pool.
    #[cfg(test)]
    pub(crate) test_prime_descriptor_sets: Option<usize>,
}

struct SceneCompositorInner {
    vk: Arc<crate::kms::vk::device::VkContext>,
    pipeline: CompositorPipeline,
    /// The RANDR transform scale pass, built when an output first needs it.
    scale_pipeline: Option<ScalePassPipeline>,
    /// XOR-logic-op fill pipeline cache used to apply the retained
    /// root-`IncludeInferiors` overlay as a final pass into each
    /// freshly-composited scanout BO (see [`crate::kms::render::root_overlay`]).
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
    /// Per output, what a root read sees: [`SceneCompositor::root_readback`].
    root_readbacks: Vec<Option<RootReadback>>,
}

/// The screen of one output as it stands, composed for a root read.
struct RootReadback {
    target: DamageAuditTarget,
    /// `(structure_generation, scene_damage_generation)` it was composed at.
    generation: (u64, u64),
    /// What of it is current at `generation`, target-local.
    valid: Vec<vk::Rect2D>,
    /// It has been composed into whole once, so a clipped compose may load it.
    whole: bool,
    /// Reads at `generation`, and at the one before: a poller reading many
    /// rects between two changes gets one whole compose per change.
    reads: u32,
    prev_reads: u32,
}

/// Stage 3f.8 cursor sprite registration. The sprite lives as a
/// regular [`DrawableStore`] entry (a `Pixmap` kind with a synthetic
/// xid) so its lifetime + Vk-handle destruction flow through the
/// same paths as any other drawable.
#[derive(Debug, Clone)]
pub(crate) struct CursorEntry {
    pub(crate) id: crate::kms::render::store::DrawableId,
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

/// Content damage a compose carried, in root coordinates.
///
/// Its output's retire acks the drawable's damage in the store, which is
/// global, so an output that has not composed that epoch yet (flip-pending
/// while the paint landed) would never see it. The compose therefore hands it
/// to the other outputs as structure damage: [`fan_out_carried_damage`].
#[derive(Clone, Debug)]
struct CarriedDamage {
    id: crate::kms::render::store::DrawableId,
    epoch: u64,
    root: Vec<vk::Rect2D>,
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
    /// Root position of this output's layout: output-local + origin = root.
    origin: (i32, i32),
    /// Sampled sources that emitted pieces on some OTHER output at its last
    /// walk (the union of the other outputs' retained `last_pieces`). Decides
    /// `ContentDamage::OtherOutput` vs `OffOutput`. Empty when unknown (single
    /// output, first frame, tests), which degrades to the old force-and-ack.
    elsewhere: &'a std::collections::HashSet<crate::kms::render::store::DrawableId>,
    draws: Vec<CompositeDraw>,
    snapshots: Vec<DamageSnapshot>,
    /// The non-empty carried snapshots again, in root coordinates. Unordered.
    carried: Vec<CarriedDamage>,
    sampled_ids: Vec<crate::kms::render::store::DrawableId>,
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
    presented_ids: Vec<crate::kms::render::store::DrawableId>,
    /// The sampled sources that emitted at least one piece on this output.
    /// With `presented_ids` this decides the dormancy REASON: not presented
    /// and no pieces anywhere ⇒ `DormantReason::NoPieces`; not presented but
    /// pieces ⇒ `HiddenDamage`, which the next paint re-arms.
    pieces_ids: Vec<crate::kms::render::store::DrawableId>,
}

struct SceneBuild {
    scene: CompositeScene,
    snapshots: Vec<DamageSnapshot>,
    /// See [`WalkSink::carried`].
    carried: Vec<CarriedDamage>,
    sampled_ids: Vec<crate::kms::render::store::DrawableId>,
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
    presented_ids: Vec<crate::kms::render::store::DrawableId>,
    /// See [`WalkSink::pieces_ids`]. Feeds the tick's `had_pieces` set.
    pieces_ids: Vec<crate::kms::render::store::DrawableId>,
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
    #[error("output {0} is transformed but has no intermediate")]
    NoIntermediate(usize),
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

/// One output's contribution to this tick's dormancy decision.
struct OutputWalkReport<'a> {
    /// `build_scene` ran for this output this tick.
    walked: bool,
    /// This walk's presented ids (empty when it did not walk).
    presented: &'a std::collections::HashSet<crate::kms::render::store::DrawableId>,
    /// The output's retained `last_pieces` — refreshed by this walk if it
    /// walked, otherwise as of its most recent walk.
    last_pieces: &'a std::collections::HashSet<crate::kms::render::store::DrawableId>,
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
    /// RANDR-transformed output: the scale pass repaints fully (spec D4).
    Transformed,
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

/// The root as a node of the walk: its place is its storage rect at the
/// output's origin, its children are the top-levels, its visible region is what
/// they leave. Emitted last in computation order (= first after the reversal),
/// which is where the old emitter pushed it.
struct RootNode {
    id: crate::kms::render::store::DrawableId,
    source_id: crate::kms::render::store::DrawableId,
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

/// Bottom-to-top child order per parent, built once per `build_scene`.
///
/// `emit_window_subtree` used to scan the whole `WindowsMap` and sort a fresh
/// `Vec` for every node it visited — O(N²) on a busy desktop (e16 emits ~2265
/// draws per compose). Step 1's visibility walk needs the same index, so it is
/// built here first, changing nothing about what is emitted.
type ChildrenIndex = HashMap<u32, Vec<u32>>;

/// What the store says about a node, captured once so the trace lines and the
/// emission decision read the same snapshot.
#[derive(Clone, Copy, Debug)]
struct NodeStoreInfo {
    d_id: crate::kms::render::store::DrawableId,
    d_kind: DrawableKind,
    d_depth: u8,
    d_refcount: u32,
    d_part: bool,
    d_extent: vk::Extent2D,
    d_view_null: bool,
    source_id: crate::kms::render::store::DrawableId,
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
    /// #133 step 5 (P6) — the OUTER absolute (root-space) origin: the
    /// window's border-inclusive origin, Xorg's `borderSize` origin
    /// (`dix/window.c:1760`, `pWin->drawable.x - bw`). This is also where the
    /// node's STORAGE begins, because a bordered window's storage is the
    /// bordered extent placed at the outer origin (`compAllocPixmap`,
    /// `composite/compalloc.c:610`) — so it is both what the node samples
    /// from and how it occludes siblings. Equal to `content_abs_*` at
    /// `bw == 0`.
    abs_x: i32,
    abs_y: i32,
    /// #133 step 5 (P6) — the CONTENT absolute origin (`outer + bw`), Xorg's
    /// `winSize` origin / `pWin->drawable.x`. This is the origin children are
    /// positioned against and the origin the child clip descends from;
    /// nothing samples from it.
    content_abs_x: i32,
    content_abs_y: i32,
    /// Output-local origin of the window's STORAGE (i.e. its OUTER origin)
    /// and the OUTER extent `(w + 2bw) x (h + 2bw)` — the rect this node
    /// samples.
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
    lookup_id: Option<crate::kms::render::store::DrawableId>,
    /// `store.get(lookup_id)` snapshot; `None` with `lookup_id == Some` reads as
    /// `store_get_returned_none`.
    store: Option<NodeStoreInfo>,
    /// Output-local destination rects this node occupies — today's clamps —
    /// in the OUTER space: Xorg's `borderSize` (`SetBorderSize`,
    /// `dix/window.c:1747`), the expanded box intersected with the bounding
    /// shape. What the node draws, and what it claims when opaque.
    place: Vec<vk::Rect2D>,
    /// #133 step 5 (5.2 / 5.3) — the INNER region, Xorg's `winSize`
    /// (`SetWinSize`, `dix/window.c:1713`): content ∩ bounding ∩ clip shape.
    /// Descendants are clipped to THIS, never to `place`, so that no child
    /// overlaps its parent's border (`mi/mivaltree.c:386`).
    ///
    /// `None` means "identical to `place`" — the case for every window with
    /// no border and no clip shape, i.e. everything before #133. The
    /// `bw == 0` path therefore computes nothing extra and allocates nothing.
    child_place: Option<Vec<vk::Rect2D>>,
    /// The node passes every gate and will push `place.len()` draws.
    emits: bool,
    /// `emits` and outside the COW subtree: the draw overwrites its dst.
    opaque: bool,
    /// Whether this node's children are under a redirected ancestor.
    child_under_redirected_ancestor: bool,
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

/// A one-shot compose of a transformed output's intermediate alone, for a
/// root read that comes before the output's first frame.
struct IntermediatePrimeTarget {
    vk: Arc<crate::kms::vk::device::VkContext>,
    image: vk::Image,
    view: vk::ImageView,
    extent: vk::Extent2D,
    command_pool: vk::CommandPool,
    command_buffer: vk::CommandBuffer,
}

/// The transformed-output half of a compose (spec D4): the scene renders into
/// the intermediate, then one full-screen draw scales it into the target.
#[derive(Clone, Copy)]
struct ScalePass {
    image: vk::Image,
    view: vk::ImageView,
    extent: vk::Extent2D,
    descriptor_set: vk::DescriptorSet,
    pipeline: vk::Pipeline,
    layout: vk::PipelineLayout,
    push: ScalePushConsts,
    /// Footprint ∩ root, intermediate-local; the rest stays transparent black.
    composite_rect: Option<vk::Rect2D>,
    /// Scale into the target; `false` composes the intermediate alone.
    into_target: bool,
}

trait ComposeRenderTarget {
    fn image(&self) -> vk::Image;
    fn image_view(&self) -> vk::ImageView;
    fn command_buffer(&self) -> vk::CommandBuffer;
    fn completion_semaphore(&self) -> vk::Semaphore;
    fn width(&self) -> u32;
    fn height(&self) -> u32;
    fn timestamp_pool(&self) -> vk::QueryPool;
    /// Whether a submitted compose has reset and written this target's
    /// timestamp queries, i.e. whether reading them is legal yet.
    fn timestamps_written(&self) -> bool;
    fn mark_timestamps_written(&mut self);
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
