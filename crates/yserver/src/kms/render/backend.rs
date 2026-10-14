//! `KmsBackend`: the `Backend` implementation on DRM/KMS.
//!
//! It owns the protocol bookkeeping (`KmsCore`), the device layer
//! (`PlatformBackend`: DRM/KMS, libinput, Vulkan), drawable storage
//! (`DrawableStore`), the paint engine (`RenderEngine`) and the scene
//! compositor (`SceneCompositor`), plus the cross-cutting state on top of
//! them: tracked windows, direct scanout, Present, cursors, VT/session,
//! RANDR/CRTC config and DRI3 export. This file holds the struct and its
//! supporting types; the methods live in:
//!
//! - `portable/*`: no DRM/GBM imports (windows, redirect, paint targets,
//!   RENDER, text, clip, drawing, input devices, pointer, stats, dumps).
//! - `kms/*`: DRM-bound behaviour (scanout, readback, CRTC config, RANDR,
//!   Present, DMA-BUF export, VT session, hardware cursor).
//! - `trait_impl`: `impl Backend for KmsBackend`, one file until the trait
//!   bodies move to per-subsystem delegators.
//! - `for_tests`: `*_for_tests` entry points for the acceptance tests.
//! - `tests`, `crtc_transform_tests`: unit tests.

#[cfg(kms)]
mod for_tests;
#[cfg(kms)]
mod kms;
#[cfg(kms)]
pub use kms::state::KmsBackend;
#[cfg(kms)]
pub(crate) use kms::state::*;
mod portable;
pub(crate) mod seam;
mod trait_impl;

#[cfg(all(test, kms))]
use crate::kms::render::backend::portable::{
    apply_gc_function, apply_z_plane_mask, assemble_root_scanout, border_ring_rects,
    bordered_storage_extent, clip_trap_bbox_to_extents, composite_needs_source_snapshot,
    compute_render_composite_clip, copy_area_clip_gpu_eligible, default_window_init_color,
    depth_plane_mask, dst_picture_clip_by_children, migrated_content_copy,
    picture_source_domain_clip, read_z_pixmap_pixel, split_root_scanout_reads,
    take_first_occurrence, write_z_pixmap_pixel, z_to_xy_planes,
};
use crate::kms::render::backend::portable::{
    compute_copy_area_dst_rects, intersect_rect_with_clip,
};
#[cfg(all(test, kms))]
use kms::*;
#[cfg(all(test, kms))]
pub(super) use portable::parse_composite_glyph_items;

#[cfg(all(test, kms))]
mod tests;

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
};

#[cfg(all(test, kms))]
use crate::kms::core::PictureRecord;
#[cfg(all(test, kms))]
use crate::kms::cpu_types::Rectangle16;
use ash::vk;
use yserver_protocol::x11::ResourceId;

pub(crate) use super::window_geometry::WindowGeometry;
pub(super) use super::window_geometry::WindowsMap;

use crate::kms::{
    core::KmsCore,
    render::{
        engine::RenderEngine,
        glyph_pixels::GlyphSourceFormat,
        scene::SceneCompositor,
        store::{DrawableId, DrawableStore},
        target::PaintTarget,
        telemetry::Telemetry,
    },
};

/// Per-window geometry tracked by v2's scene assembler. Stage 2 plan
/// Risk 3: a parallel `windows` map on `KmsBackend` (NOT on
/// `KmsCore` — v1 doesn't need it). Stage 4 may collapse into
/// `KmsCore.windows` when `WindowState` splits.
///
/// Stage 3f.6 grows `parent`: subwindows record their parent xid so
/// `build_scene` can recurse top-level → descendants with accumulated
/// offsets. `None` marks top-levels (parent is root, not tracked
/// in `windows`). The `bg_pixel` / `bg_pixmap` slots carry
/// per-window background attributes set via
/// `change_subwindow_attributes`; the bg-pixel is painted into
/// storage at allocate + configure resize so freshly-mapped windows
/// have a defined initial colour.
/// See `input_only_pointer_hosts`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct InputOnlyPointerHost {
    window: ResourceId,
    /// Host of the nearest ancestor with a backend window, where the
    /// cursor walk carries on when no InputOnly window on the way up has
    /// a cursor.
    parent_host: u32,
    /// The first cursor set on the InputOnly windows from `window` up to
    /// that ancestor.
    cursor: Option<u32>,
    /// `window`'s parent-relative origin, which `event_relative_coords`
    /// subtracts as it does a backend window's (the core adds it back).
    origin: (i16, i16),
}

/// [`WindowsMap`] counting its mutable borrows, so a clip computed from
/// it can tell it is still current ([`KmsBackend::subwindow_mode_clip`]).
#[derive(Debug, Default)]
pub(crate) struct TrackedWindows {
    map: WindowsMap,
    generation: u64,
}

impl TrackedWindows {
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }
}

impl std::ops::Deref for TrackedWindows {
    type Target = WindowsMap;
    fn deref(&self) -> &WindowsMap {
        &self.map
    }
}

impl<'a> IntoIterator for &'a TrackedWindows {
    type Item = (&'a u32, &'a WindowGeometry);
    type IntoIter = std::collections::hash_map::Iter<'a, u32, WindowGeometry>;
    fn into_iter(self) -> Self::IntoIter {
        self.map.iter()
    }
}

impl std::ops::DerefMut for TrackedWindows {
    fn deref_mut(&mut self) -> &mut WindowsMap {
        self.generation = self.generation.wrapping_add(1);
        &mut self.map
    }
}

/// The scene's per-call input. A free function over disjoint `KmsBackend`
/// fields, so it can be held across `&mut self.platform` / `&mut self.store`.
pub(super) fn scene_input<'a>(
    core: &'a crate::kms::core::KmsCore,
    windows: &'a WindowsMap,
    outputs: &'a [crate::kms::render::scene::OutputLayout],
    fb_size: (u16, u16),
) -> crate::kms::render::scene::SceneInput<'a> {
    crate::kms::render::scene::SceneInput {
        window_id: core.window_id,
        top_level_order: &core.top_level_order,
        shape_bounding: &core.shape_bounding,
        shape_clip: &core.shape_clip,
        cursor_x: core.cursor_x,
        cursor_y: core.cursor_y,
        windows,
        outputs,
        fb_size,
    }
}

/// Test-only (#133): one scene participant's placement — its host xid and
/// its output-local rects as `(x, y, w, h)`. Named so
/// `KmsBackend::scene_participant_places_for_tests` has a simple signature.
#[cfg(kms)]
pub type ScenePlacement = (u32, Vec<(i32, i32, u32, u32)>, Vec<(i32, i32, u32, u32)>);

/// #133 step 3 (P4) — the observable shape of a resolved paint target,
/// for `KmsBackend::paint_target_shape_for_tests`: the content
/// translation into storage coordinates, the content clip as
/// `(x, y, w, h)` in storage coordinates (`None` = the whole storage),
/// and whether the resolved chain carries a border clip at all (the
/// direct-scanout gate's input, 3.5).
#[cfg(kms)]
pub type PaintTargetShape = ((i32, i32), Option<(i32, i32, u32, u32)>, bool);

/// A moving window's place in an ancestor's shared redirect backing,
/// taken before the configure: see
/// [`KmsBackend::shared_backing_move_source`].
struct SharedBackingMoveSource {
    /// Where the window drew before the move.
    target: PaintTarget,
    /// Higher siblings over the window at its OLD position, in its
    /// local content space: those pixels are theirs, not the window's.
    occluders: Vec<ash::vk::Rect2D>,
}

/// Cooking state owned by one temporarily floating keyboard slave. Its XKB
/// state and duplicate guard must not share the master keyboard's state.
struct FloatingKeyboardState {
    xkb_state: crate::kms::core::XkbState,
    down_keys: HashSet<u8>,
    lock_filter_priv_by_key: HashMap<u8, LockFilterPriv>,
    locked_group: u8,
}

/// The XkbFilterLockState data captured for a held LockMods key. Xorg stores
/// both the pre-press locked bits and the up action; `LockNoUnlock` suppresses
/// clearing those bits when the detached key is released.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LockFilterPriv {
    pre_press_locked_mods: u32,
    no_unlock: bool,
}

/// XkbSA_LockNoUnlock (`LockMods(..., affect=lock)`).
const XKB_SA_LOCK_NO_UNLOCK: u8 = 0x02;

/// #133 step 6 (P8) — what [`KmsBackend::sync_window_leaf_storage`]
/// does with the pixels a window's leaf storage already holds when it
/// has to reallocate.
///
/// The two callers want opposite things and the difference is not an
/// optimisation:
///
/// - A **border-width** change must preserve the client's drawable:
///   nothing about the client's content changed, only where it sits
///   inside the storage. Reallocating and background-filling would
///   erase an otherwise untouched window — [`Self::Migrate`].
/// - A **width/height** resize preserves it too, but only for a window
///   with NO background: X11 discards the contents of a default-gravity
///   window and tiles it with its background, "if no background is
///   defined, the existing screen contents are not altered"
///   (ForgetGravity, ChangeWindowAttributes), which Xorg implements by
///   returning from the paint without touching a pixel
///   (`mi/miexpose.c:438-440`). A window WITH a background is tiled, so
///   it stays on [`Self::Discard`] — see `configure_subwindow` for the
///   #143 measurement and for why a non-Forget `bit_gravity` cannot be
///   honoured here yet (`project_resize_black_window_storage`).
/// - **Unredirect** discards, because the leaf has been stale since the
///   route was installed and `restore_leaves_from_backing` puts the
///   compositor's real pixels back on top of the fresh storage —
///   [`Self::Discard`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LeafContent {
    /// Reallocate and initialise from the background; copy nothing
    /// forward.
    Discard,
    /// Retain the old storage across the reallocation and copy the
    /// intersection of the old and new CONTENT rects into the new one,
    /// from `old_bw` to `new_bw`. Xorg keeps the old pixmap for exactly
    /// this reason — "leaving the old pixmap in cw->pOldPixmap so bits
    /// can be recovered" (`composite/compalloc.c:676-678`) — and
    /// recovers them with a `CopyArea` in `compCopyWindow`
    /// (`composite/compwindow.c:501-540`).
    Migrate,
}

/// One leaf→backing composite emitted by
/// `KmsBackend::plan_backing_inferiors`. Coordinates are
/// backing-local (B's `(0, 0)` == the redirected window's origin);
/// `width`/`height` are already low-side clamped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SeedInferiorDraw {
    leaf_id: crate::kms::render::store::DrawableId,
    src_x: i32,
    src_y: i32,
    dst_x: i32,
    dst_y: i32,
    width: u32,
    height: u32,
}

/// v2 sibling backend. Shares `KmsCore` with `KmsBackend`;
/// owns `PlatformBackend` (real DRM/Vk/libinput per Stage 2a)
/// plus stub `DrawableStore` / `RenderEngine` / `SceneCompositor`
/// that fill in across Stages 2b–2e. Paint / RENDER / scene ops
/// log gaps until those substages land.
///
/// Generic over the platform `P` under it; on DRM/KMS that is [`KmsBackend`].
pub struct XBackend<P: seam::Platform> {
    /// Shared protocol-bookkeeping state. Identical to v1's
    /// `KmsBackend.core` — same struct, same construction path.
    pub(crate) core: KmsCore,

    /// Real DRM/KMS/libinput/Vulkan owner per Stage 2a. Replaced
    /// the flat field set Stage 1b carried.
    pub(crate) platform: P,

    /// Once-per-method dedup set for `v2: <method> not yet
    /// implemented` warnings. `RefCell` to keep the helper callable
    /// from `&self` paths (capability accessors that log gaps).
    logged_gaps: RefCell<HashSet<&'static str>>,

    /// v2's storage layer (Stage 2b). Tracks every drawable's
    /// VkImage + refcount + damage + retirement-fence; allocated
    /// via `PlatformBackend::allocate_drawable_storage`.
    pub(crate) store: DrawableStore,
    /// v2's paint engine (Stage 2c). Drives `fill_rect`,
    /// `put_image`, `get_image` directly into `DrawableStore`
    /// storage; consumed by every `Backend` paint method on this
    /// backend.
    pub(crate) engine: RenderEngine,
    /// v2's scene compositor — real per Stage 2d.
    pub(crate) scene: SceneCompositor,
    /// v2's per-second telemetry counters (Stage 2f). The
    /// per-second emitter logs under `YSERVER_LOOP_TELEMETRY=1`;
    /// lifetime totals are always tracked for the acceptance
    /// harness.
    pub(crate) telemetry: Telemetry,
    /// Stage 5 Task 4 layer 1: last-observed ring lifetime values.
    /// `sync_descriptor_pool_telemetry` computes deltas vs these
    /// snapshots and bumps `telemetry` counters by the delta. Spec
    /// `2026-05-21-descriptor-pool-ring-design.md`.
    pub(crate) last_observed_pool_creates: u64,
    pub(crate) last_observed_pool_resets: u64,
    /// Per-window geometry tracked outside `KmsCore` (v1 doesn't
    /// need it). Keyed by host xid; mutated by
    /// `register_top_level` / `register_subwindow` /
    /// `create_subwindow` / `configure_subwindow` /
    /// `map_subwindow` / `unmap_subwindow` /
    /// `destroy_subwindow`.
    pub(crate) windows: TrackedWindows,
    /// Monotonic allocator for per-parent sibling ordering. V2 scene
    /// assembly still stores windows in a flat map, so child z-order
    /// needs an explicit stable rank instead of relying on HashMap
    /// iteration order.
    next_window_stack_rank: u64,
    /// Stage 4d: `DrawableId` of the Composite Overlay Window
    /// storage, allocated lazily on the first `GetOverlayWindow`
    /// and dropped on the final `ReleaseOverlayWindow`. `None`
    /// when no compositor is holding a COW. Storage handle lives here
    /// (backend / Vk-side state); the claims that keep it alive live on
    /// `ServerState::cow_claims` in core, and the backend deliberately
    /// keeps no count of its own.
    pub(crate) cow_id: Option<crate::kms::render::store::DrawableId>,
    /// Final COW release accepted while direct scanout still owns a frame.
    /// The protocol resource is logically gone, but the backend identity and
    /// storage stay alive until the composed replacement retires.
    deferred_cow_release: bool,

    /// CPU-side clip-mask cache for the current GC clip pixmap
    /// (depth-1 or depth-8). Install keeps identity/origin metadata
    /// current and eagerly refreshes the GPU snapshot, but defers the
    /// CPU `engine.get_image` readback until a run-based clip consumer
    /// actually needs the bytes. `free_pixmap` captures pending bytes
    /// before the source drawable disappears so retain-after-free still
    /// holds for CPU-clipped fills.
    pub(crate) clip_mask_cache: Option<crate::kms::backend::ClipMaskCache>,
    /// CPU cache of depth-1 SHAPE::Mask readbacks (`read_depth1_pixmap`).
    /// Cuts the discrete-NVIDIA drag stall (#32/#96): ~60% of those reads
    /// re-fetch an unchanged mask from VRAM. Keyed by never-recycled
    /// `DrawableId` + validated by `content_version`. See
    /// [`crate::kms::backend::Depth1MaskCache`].
    pub(crate) depth1_mask_cache: crate::kms::backend::Depth1MaskCache,
    /// #137 step 5 — CPU cache of tier-1 uniform glyph-source colours.
    /// A hit skips the ordered `get_image` of
    /// [`Self::uniform_glyph_source_premul`] and, with it, the
    /// `CloseReason::SyncWait` frame close that readback forces — which
    /// measured as ~75% of ALL frame closes on a text-heavy workload.
    /// Keyed by never-recycled `DrawableId` + sampled offset and
    /// validated by `content_version`; populated only AFTER the read.
    /// See [`crate::kms::backend::UniformGlyphSourceCache`].
    pub(crate) uniform_glyph_source_cache: crate::kms::backend::UniformGlyphSourceCache,
    /// GPU snapshot of the current clip-mask pixmap for the masked CopyArea
    /// path (Task 14). Single current-clip carrier, mirroring
    /// `clip_mask_cache`'s ownership: created + eagerly populated on install,
    /// re-refreshed on `content_version` change while the pixmap is live, and
    /// retired on pixmap free or size change.
    clip_mask_snapshot: Option<ClipMaskSnapshot>,
    /// Cached readback of the current GC tile/stipple pixmap. Needed
    /// because X11 GCs retain tile/stipple semantics after the client
    /// frees the source pixmap; once the backing is gone from
    /// `DrawableStore`, patterned fills must still use the last image.
    pub(crate) fill_pattern_cache: Option<FillPatternCache>,

    /// Cached binary KMS power state. `true` when at least one live output is
    /// lit; `false` during DPMS/VT blackout or when the active-output
    /// inventory is empty. Lets `set_dpms_power` no-op when called for
    /// Standby→Suspend / Suspend→Off (same binary state, different protocol
    /// level), and lets Present avoid waiting for a clock that cannot advance.
    pub(crate) kms_outputs_active: bool,

    /// Test-only counter: bumps every time
    /// `clear_window_area_with_background` is entered. Used by the
    /// `cwa_on_redirected_window_does_not_clear_backing` regression
    /// test to verify the Stage 4d CWA-clear-skip behavior without
    /// needing a Vk-backed fixture or scanout-readback. Always
    /// present (not `cfg(test)`) so the increment is a single
    /// branchless line; production paths don't observe it.
    pub(crate) clear_window_area_calls: u32,

    /// Counter incremented every time `copy_area` reaches its
    /// engine.copy_area dispatch loop (i.e. every surviving
    /// sub-rect after GC clip + ClipByChildren). Used by the
    /// `copy_area_clip_by_children_skips_manually_redirected_child`
    /// regression test to verify the manual-redirect exception
    /// without needing a Vk-backed fixture: pre-fix a manual-
    /// redirected child fully covering the dst clips the rect to
    /// empty and the loop never runs (counter = 0); post-fix the
    /// counter increments at least once.
    pub(crate) engine_copy_area_calls: u32,

    /// Diagnostic ring of recent `PRESENT::Pixmap` submissions to
    /// any destination window. Cinnamon's shell menus paint into a
    /// fullscreen Muffin stage pixmap rather than a normal window
    /// backing, so a drawable dump taken while the menu is visible
    /// needs the recent Present sources even when the destination is
    /// not COW. `(src_pixmap_xid, dst_window_xid)` pairs are stored
    /// in submission order, deduplicated only against the immediately
    /// previous pair.
    pub(crate) recent_present_pixmaps: std::collections::VecDeque<(u32, u32)>,

    /// Exact drawable instance retained by each pixmap-backed Picture,
    /// so `render_free_picture` drops that same `DrawableId` rather
    /// than whatever the host xid resolves to at free time. Window
    /// Pictures retain nothing: they resolve the window at each use.
    picture_drawable_ids: HashMap<u32, DrawableId>,

    /// Pictures whose backing drawable was not yet materialized in
    /// the store at `render_create_picture` time. Keyed by the host
    /// picture xid → the host drawable xid it wraps. When the backing
    /// materializes (`store.allocate`), each pending picture takes its
    /// store ref via [`Self::apply_pending_picture_refs`], so a later
    /// `free_pixmap` can't destroy the drawable out from under a live
    /// Picture (game-start transparency bug). Cleared on
    /// `render_free_picture` without a decref if the backing never
    /// materialized.
    pending_picture_drawable_refs: HashMap<u32, u32>,

    /// Paces the root-readback warning, which a client reading the root while no output is lit repeats thousands of times a second.
    root_readback_warn: WarnThrottle,

    /// The last [`KmsBackend::subwindow_mode_clip`]: for which window and
    /// mode, at which generations of the window tree, of the store's
    /// redirect routing and of the shapes, and what it was.
    subwindow_clip_cache: std::cell::RefCell<Option<SubwindowClipCacheEntry>>,
    /// Counts SHAPE changes, for that cache.
    shape_generation: u64,

    /// Set while a RENDER op repeats itself on a destination's inferiors
    /// ([`KmsBackend::include_inferiors_dst_fanout`]), so the repeats do
    /// not fan out again.
    dst_fanout_active: bool,

    /// Stage 5 Task 6.1: queue of in-flight deferred PRESENT
    /// completion batches. Drained by `drain_completed_present_events`
    /// when the inner `present_completion_epfd` reports a submitted
    /// batch's exported sync_file as readable, or when the degraded
    /// fallback path pokes `wakeup_eventfd`. Each entry inside each
    /// batch pins an Arc clone of the wake primitive (xshmfence /
    /// syncobj) so the underlying resource survives an intervening
    /// `XFixesDestroyFence` / `FreeSyncobj`.
    pub(crate) pending_present_batches:
        std::collections::VecDeque<crate::kms::render::present_batch::PendingPresentBatch>,

    /// Pinned wakes drained to core but not yet signalled — held here so
    /// core can pace when the real xshmfence / syncobj fires. Keyed by
    /// `CompletedPresentEvent::present_id`.
    retained_present_wakes:
        std::collections::HashMap<u64, crate::kms::render::present_completion::PinnedWake>,

    /// `pin_present_source` tokens: the xid is resolved to a `DrawableId`
    /// ONCE at pin time and held here, incref'd, so a later `FreePixmap` /
    /// xid reuse on `store.by_xid` cannot re-point an already-pinned
    /// present source out from under a parked entry. Released by
    /// `release_present_source`.
    pub(crate) present_source_pins: HashMap<u64, crate::kms::render::store::DrawableId>,
    pub(crate) next_present_source_pin_id: u64,

    /// Stage 5 Task 6.1: shutdown-time accumulator for PRESENT
    /// completions that need to be drained past `disable_output`
    /// and handed to `lib.rs::run` for client fan-out before the
    /// socket is torn down. Populated only by `disable_output`;
    /// drained by `take_shutdown_present_events`.
    pub(crate) pending_completed_events_on_shutdown:
        Vec<yserver_core::backend::CompletedPresentEvent>,

    /// Stage 5 Phase A — canonical cursor xid → immutable record map.
    /// Inserted by `create_cursor` / `create_glyph_cursor` /
    /// `render_create_cursor` / `create_anim_cursor` (which aliases
    /// frame 0 into this map); read by `define_cursor` /
    /// `update_pointer_window` to swap the effective sprite. `Arc`
    /// so anything that captured a reference (a future Phase D
    /// deferred upload, a pointer grab) keeps stable bytes even
    /// after a later replacement.
    pub(crate) cursor_records:
        HashMap<u32, std::sync::Arc<crate::kms::render::cursor::CursorRecord>>,
    /// Per-cursor uploaded Pixmap drawable. The SW scene path samples
    /// through this. Lifetime is the same as the matching entry in
    /// `cursor_records`; both maps share the same xid keys.
    pub(crate) cursor_pixmaps: HashMap<u32, crate::kms::render::store::DrawableId>,
    /// Monotonically-increasing version counter for new
    /// `CursorRecord` allocations. Compared by VALUE in the Phase B/C
    /// upload-dedup paths.
    pub(crate) next_cursor_version: u64,
    /// Xid of the default-arrow record allocated at backend init.
    /// `define_cursor(_, 0)` (X11 `None`) falls back to this; the
    /// scene's `register_cursor` swaps to the entry recorded here.
    pub(crate) default_cursor_xid: Option<u32>,
    /// InputOnly windows the pointer has reached, by the synthetic host
    /// xid that stands in for them (they have no backend window). Xorg's
    /// sprite enters InputOnly windows like any other (`XYToWindow`) and
    /// shows their cursor; the pointer model here is host-keyed, so each
    /// gets a host xid mapped back to it in `core.xid_map`.
    pub(crate) input_only_pointer_hosts: HashMap<u32, InputOnlyPointerHost>,
    /// Xid of the currently-effective cursor — the one whose sprite
    /// is shown on screen. Driven by `update_effective_cursor`;
    /// `define_cursor` + `update_pointer_window` re-evaluate it.
    pub(crate) effective_cursor_xid: Option<u32>,
    /// Highest-priority sprite override for an active pointer grab
    /// (Xorg `ActivatePointerGrab`). When `Some(h)`, host cursor
    /// handle `h` is displayed regardless of the per-window cursor
    /// chain or the sticky/default fallback, for the grab's duration.
    /// Set/cleared via `set_grab_cursor` from the core grab/ungrab
    /// paths; `None` when no grab (or a `None`-cursor grab) is active.
    pub(crate) grab_cursor_override: Option<u32>,
    /// XFIXES `HideCursor` is in force (some client holds a hide count).
    /// The effective cursor keeps being tracked, so `GetCursorImage` and
    /// `CursorNotify` are unaffected; only the sprite is blanked (Xorg
    /// `CursorDisplayCursor` displays `NullCursor` while hide counts exist).
    pub(crate) cursor_hidden: bool,
    /// Pending XFIXES `CursorNotify` report: set whenever the effective
    /// cursor switches, drained by the core via
    /// `take_displayed_cursor_change`.
    pub(crate) displayed_cursor_pending: Option<yserver_core::backend::DisplayedCursor>,

    /// Animated-cursor frame lists, keyed by the anim cursor's host
    /// handle. Same key-space discipline as `cursor_records` /
    /// `cursor_pixmaps` (see comment at `cursor_records`). A live entry
    /// holds a reference on each frame's cursor (Xorg `animcur.c:360`).
    pub(crate) anim_cursor_records: HashMap<u32, crate::kms::render::cursor::AnimCursorRecord>,
    /// Cursors the core has freed (no XID names them) that are still
    /// referenced here — a window's cursor, the grab override, the sticky
    /// root default, the displayed sprite or a live animated cursor's
    /// frame. Destroyed by `collect_released_cursors` once the last of
    /// those goes, as Xorg's `FreeCursor` does on `refcnt == 0`.
    pub(crate) released_cursors: HashSet<u32>,
    /// The one running animation (the effective cursor is animated),
    /// or `None`.
    pub(crate) active_cursor_anim: Option<crate::kms::render::cursor::ActiveCursorAnim>,

    /// Phase B.1 Task 21: lifetime-opens count seen at the last
    /// `drain_frame_builder_telemetry` call. Delta tracking lets the
    /// drain helper emit one `record_frame_builder_open` per new open
    /// without requiring a separate event queue.
    last_drained_fb_opens: u64,
    /// Independent XKB cooking states for keyboard slaves detached by an
    /// explicit XI2 grab. The XI device ID is stable for the grab lifetime.
    floating_keyboard_states: HashMap<u16, FloatingKeyboardState>,
    /// Xorg's XkbFilterLockState `filter->priv` and `filter->upAction` for
    /// each held LockMods key.
    lock_filter_priv_by_device: HashMap<u16, HashMap<u8, LockFilterPriv>>,

    /// KMS/DRM/VT/DRI3 state that the portable half never touches.
    pub(crate) pstate: P::BackendState,
}

impl<P: seam::Platform> XBackend<P> {
    /// A backend over `platform` with every portable field at its initial
    /// state. Platform bring-up (root storage, cursor sprite, …) follows.
    pub(crate) fn assemble(
        core: KmsCore,
        platform: P,
        engine: RenderEngine,
        scene: SceneCompositor,
        pstate: P::BackendState,
        kms_outputs_active: bool,
    ) -> Self {
        Self {
            core,
            platform,
            logged_gaps: RefCell::new(HashSet::new()),
            store: DrawableStore::new(),
            engine,
            scene,
            windows: TrackedWindows::default(),
            subwindow_clip_cache: std::cell::RefCell::new(None),
            shape_generation: 0,
            next_window_stack_rank: 1,
            telemetry: Telemetry::new(),
            last_observed_pool_creates: 0,
            last_observed_pool_resets: 0,
            cow_id: None,
            deferred_cow_release: false,
            clip_mask_cache: None,
            depth1_mask_cache: crate::kms::backend::Depth1MaskCache::new(256),
            uniform_glyph_source_cache: crate::kms::backend::UniformGlyphSourceCache::new(64),
            clip_mask_snapshot: None,
            fill_pattern_cache: None,
            kms_outputs_active,
            clear_window_area_calls: 0,
            engine_copy_area_calls: 0,
            recent_present_pixmaps: std::collections::VecDeque::with_capacity(32),
            picture_drawable_ids: HashMap::new(),
            pending_picture_drawable_refs: HashMap::new(),
            root_readback_warn: WarnThrottle::default(),
            dst_fanout_active: false,
            pending_present_batches: std::collections::VecDeque::new(),
            retained_present_wakes: std::collections::HashMap::new(),
            present_source_pins: HashMap::new(),
            next_present_source_pin_id: 1,
            pending_completed_events_on_shutdown: Vec::new(),
            cursor_records: HashMap::new(),
            cursor_pixmaps: HashMap::new(),
            next_cursor_version: 1,
            default_cursor_xid: None,
            input_only_pointer_hosts: HashMap::new(),
            effective_cursor_xid: None,
            grab_cursor_override: None,
            cursor_hidden: false,
            displayed_cursor_pending: None,
            anim_cursor_records: HashMap::new(),
            released_cursors: HashSet::new(),
            active_cursor_anim: None,
            last_drained_fb_opens: 0,
            floating_keyboard_states: HashMap::new(),
            lock_filter_priv_by_device: HashMap::new(),
            pstate,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct FillPatternCache {
    pub(crate) pixmap_xid: u32,
    pub(crate) origin: (i16, i16),
    pub(crate) depth: u8,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) bytes: Vec<u8>,
}

/// GPU snapshot handle for the current clip-mask pixmap, used by the
/// masked CopyArea path (Task 14). Single current-clip carrier mirroring
/// `clip_mask_cache`'s ownership: created + eagerly populated on install,
/// re-refreshed on `content_version` change while the pixmap is live, and
/// retired on pixmap free or size change. Retained across `ClipState::None`
/// (retain-after-free), since `install_clip_mask_cache` is not called for None.
struct ClipMaskSnapshot {
    pixmap_xid: u32,
    drawable_id: crate::kms::render::store::DrawableId,
    id: crate::kms::render::engine::SnapshotId,
    width: u32,
    height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KmsButtonDiagnostic {
    PressAlreadyHeld { mask: u16 },
    ReleaseNotHeld { mask: u16 },
}

/// The cut of [`KmsBackend::subwindow_mode_clip`].
#[derive(Clone)]
struct SubwindowModeClip {
    cut: Vec<ash::vk::Rect2D>,
    keep: Option<Vec<ash::vk::Rect2D>>,
}

struct SubwindowClipCacheEntry {
    key: (u32, yserver_core::backend::SubwindowMode, u64, u64, u64),
    clip: Option<SubwindowModeClip>,
}

/// One inferior of a window that draws into storage of its own: see
/// [`KmsBackend::inferior_pieces`].
struct InferiorPiece {
    window: u32,
    target: PaintTarget,
    /// Its content origin, in the content space of the window asked about.
    origin: (i32, i32),
    /// What of it shows there, in that same space.
    rects: Vec<ash::vk::Rect2D>,
}

/// Apply a `RenderChangePicture` value-mask body to the picture
/// record. Mirrors v1's per-bit handler in shape; differences are
/// the v2 record's type and `KmsCore.pictures` as the map.
/// `body` is the full request body shape:
/// `picture(4) + value_mask(4) + values[…]`.
/// Has the once-per-process `CompositeGlyphs` unsupported-drop warning
/// been said yet? See `KmsBackend::record_composite_glyphs_drop` for
/// why this is once per PROCESS and not once per generation.
static COMPOSITE_GLYPHS_DROP_WARNED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// One scanout read for a root-source `CopyArea(IncludeInferiors)` screenshot:
/// which root-absolute region to read from the composited scanout, and where
/// (destination-local) to write it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RootScanoutRead {
    /// Root-absolute rect to hand to `read_scanout_region`. Guaranteed to sit
    /// fully inside one output, so `OnScreenOnly` selection succeeds.
    read: vk::Rect2D,
    /// Destination-LOCAL offset for the read pixels (before `dst_target.offset()`).
    dst_local: vk::Offset2D,
}

/// Where `assemble_root_scanout` wants a piece read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RootReadSource {
    /// Inside one output: what that CRTC scans out.
    Scanout,
    /// Covered by no output: the root window's own storage.
    Background,
}

/// How often an otherwise idle server re-polls the fences of freed
/// drawables parked in `pending_retire`.
const PENDING_RETIRE_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(5);

/// One glyph resolved out of a `CompositeGlyphs` items stream: the
/// glyphset it came from, its id, its size, the picture format it is
/// **stored** in, and its dst-space top-left. Holds no borrow of the
/// glyphset map, so the caller can drop the immutable
/// `core.glyphsets` borrow before the `&mut self.engine` call that
/// consumes pass 2's output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ParsedGlyph {
    pub(super) gs_xid: u32,
    pub(super) glyph_id: u32,
    pub(super) w: u32,
    pub(super) h: u32,
    /// The glyphset's picture format — protocol state, read here
    /// from the glyphset record. The engine turns it into an
    /// effective `GlyphLayout`
    /// (`RenderEngine::effective_glyph_layout`), because that mapping
    /// depends on device state the parse must not consult.
    pub(super) source_format: GlyphSourceFormat,
    pub(super) dst_x: i32,
    pub(super) dst_y: i32,
}

/// What pass 1 of the items parse produced, plus the counters the
/// caller's "nothing parsed" diagnostic reports.
#[derive(Debug, Default)]
pub(super) struct ParsedGlyphItems {
    /// Every drawable glyph the stream named, **in request order**.
    pub(super) glyphs: Vec<ParsedGlyph>,
    pub(super) elements: usize,
    pub(super) found: usize,
    pub(super) missing: usize,
}

/// X11 GetImage `format` wire value for XYPixmap (ZPixmap is 2).
const GET_IMAGE_FORMAT_XY_PIXMAP: u8 = 1;

/// What a pure move inside a shared backing carries, in the window's
/// local content space: its `outer` extent (within its bounding `shape`,
/// which a move does not change) minus the higher siblings
/// over it at either end (`old_occluders`, `new_occluders`), and inside
/// the parent's clip (`parent_bounds`, backing coordinates; `None` = the
/// whole backing) at both the old and the new origin. Xorg's
/// `fbCopyWindow` copies the old `borderClip` into the new one, and a
/// borderClip never leaves the parent's (`mi/mivaltree.c:390`): a GTK
/// bin window taller than its viewport otherwise drags its rows below
/// the viewport over the frame's title bar on every scroll.
fn shared_backing_move_pieces(
    outer: ash::vk::Rect2D,
    shape: Option<&[ash::vk::Rect2D]>,
    old_occluders: &[ash::vk::Rect2D],
    new_occluders: &[ash::vk::Rect2D],
    parent_bounds: Option<ash::vk::Rect2D>,
    old_origin: (i32, i32),
    new_origin: (i32, i32),
) -> Vec<ash::vk::Rect2D> {
    let in_parent_at = |origin: (i32, i32)| -> Vec<ash::vk::Rect2D> {
        parent_bounds.map_or_else(
            || vec![outer],
            |b| {
                intersect_rect_with_clip(
                    outer,
                    &[ash::vk::Rect2D {
                        offset: ash::vk::Offset2D {
                            x: b.offset.x - origin.0,
                            y: b.offset.y - origin.1,
                        },
                        extent: b.extent,
                    }],
                )
            },
        )
    };
    let still_visible = compute_copy_area_dst_rects(outer, new_occluders);
    let (was_inside, is_inside) = (in_parent_at(old_origin), in_parent_at(new_origin));
    let shaped = shape.map_or_else(|| vec![outer], |s| intersect_rect_with_clip(outer, s));
    shaped
        .into_iter()
        .flat_map(|r| compute_copy_area_dst_rects(r, old_occluders))
        .flat_map(|r| intersect_rect_with_clip(r, &still_visible))
        .flat_map(|r| intersect_rect_with_clip(r, &was_inside))
        .flat_map(|r| intersect_rect_with_clip(r, &is_inside))
        .collect()
}

/// Order the disjoint pieces of an in-place move by `delta` so that no
/// piece is read after another piece has written over it. Each copy is
/// individually overlap-safe (`RenderEngine::copy_area` stages a
/// same-image copy through a scratch image), but the pieces are separate
/// copies: piece `j` must go before piece `i` whenever `i`'s destination
/// covers `j`'s source. Xorg's `miCopyRegion` gets the same guarantee by
/// walking its YX-banded boxes against the direction of the move
/// (`mi/micopy.c:54-140`); these pieces are not banded, so the order is
/// derived from the overlaps directly. Should the remaining pieces ever
/// form a cycle, they are appended as they are; the common case — no
/// higher sibling over the window — is a single piece.
fn order_pieces_for_in_place_move(
    pieces: Vec<ash::vk::Rect2D>,
    delta: (i32, i32),
) -> Vec<ash::vk::Rect2D> {
    fn overlaps(a: ash::vk::Rect2D, b: ash::vk::Rect2D) -> bool {
        let right = |r: ash::vk::Rect2D| r.offset.x + i32::try_from(r.extent.width).unwrap_or(0);
        let bottom = |r: ash::vk::Rect2D| r.offset.y + i32::try_from(r.extent.height).unwrap_or(0);
        a.offset.x < right(b)
            && b.offset.x < right(a)
            && a.offset.y < bottom(b)
            && b.offset.y < bottom(a)
    }
    if pieces.len() < 2 {
        return pieces;
    }
    let moved = |r: ash::vk::Rect2D| ash::vk::Rect2D {
        offset: ash::vk::Offset2D {
            x: r.offset.x + delta.0,
            y: r.offset.y + delta.1,
        },
        extent: r.extent,
    };
    let mut left: Vec<ash::vk::Rect2D> = pieces;
    let mut ordered = Vec::with_capacity(left.len());
    while !left.is_empty() {
        // A piece is safe to copy now when its destination covers no
        // other remaining piece's source.
        let ready = (0..left.len()).find(|&i| {
            let dst = moved(left[i]);
            left.iter()
                .enumerate()
                .all(|(j, src)| j == i || !overlaps(dst, *src))
        });
        match ready {
            Some(i) => ordered.push(left.swap_remove(i)),
            None => {
                ordered.append(&mut left);
            }
        }
    }
    ordered
}

/// Logs at most once per [`WarnThrottle::PERIOD`], counting what it held back.
#[derive(Debug, Default)]
struct WarnThrottle {
    last: Option<std::time::Instant>,
    held: u64,
}

impl WarnThrottle {
    const PERIOD: std::time::Duration = std::time::Duration::from_secs(10);

    /// `Some(n)` when a warning should be logged now, `n` being how many were held back since the last one.
    fn check(&mut self, now: std::time::Instant) -> Option<u64> {
        if self
            .last
            .is_some_and(|last| now.duration_since(last) < Self::PERIOD)
        {
            self.held += 1;
            return None;
        }
        self.last = Some(now);
        Some(std::mem::take(&mut self.held))
    }
}

#[cfg(all(test, kms))]
#[path = "crtc_transform_tests.rs"]
mod crtc_transform_tests;
