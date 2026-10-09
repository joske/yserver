//! `KmsBackend` — Stage 1b skeleton sibling of `KmsBackend` (v1).
//!
//! Per rendering-model-v2 spec § Stage 1b. Embeds the same
//! `KmsCore` as v1 so protocol bookkeeping (XID maps, window
//! metadata stripped of storage, fonts, SHAPE regions, etc.) lives
//! exactly once. Every paint / scene / RENDER trait method stubs
//! with a once-per-method `warn!` + `Ok(())`. Real components
//! (`PlatformBackend`, `DrawableStore`, `RenderEngine`,
//! `SceneCompositor`) land in Stage 2.
//!
//! The acceptance gate is **synthetic**: the server boots (v2 is
//! now the only render model), opens a connection,
//! services capability queries / atom queries / GetGeometry on
//! root; the first paint op produces exactly one
//! `v2: <method> not yet implemented` warn line per opcode. No
//! real-app gates land at this stage — those wait for Stage 3.

mod for_tests;
mod kms;
mod portable;
mod trait_impl;

use kms::*;
pub(super) use portable::*;

#[cfg(test)]
mod tests;

use std::{
    any::Any,
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet, VecDeque},
    io,
    os::fd::{AsFd, OwnedFd},
    rc::Rc,
};

use ash::vk;
use yserver_core::{
    backend::{
        AnyHandle, Backend, BackendFdKind, ClipState, CrtcConfigApply, CrtcConfigToken,
        CursorHandle, DrawState, Dri3Caps, Dri3ImportModifier, Dri3PixmapExport, FillState,
        FontHandle, GlyphSetHandle, KeymapLoad, OriginContext, PictureHandle, PixmapHandle,
        PresentCaps, PresentScanoutCandidate, PresentSourceWait, WindowHandle, identity_ramp,
        resample_channel,
    },
    core_loop::HostInputEvent,
    host_x11::{
        HostKeyEvent, HostPointerEvent, HostSubwindowConfig, HostSubwindowVisual, HostXidMap,
        PointerEventKind, PointerPosition,
    },
    resources::{ARGB_COLORMAP, ARGB_VISUAL},
    server::ServerState,
};
use yserver_protocol::x11::{
    ClipRectangles, FontMetrics, RENDER_FMT_A1, RENDER_FMT_A8, RENDER_FMT_ARGB32, ResourceId,
    xfixes,
};

use crate::{
    drm,
    internal_probe::{ProbeKmsHandles, RouteProbeRequest},
    kms::{
        backend::OutputKey,
        core::{GradientStop, KmsCore, PictureFilter, PictureRecord},
        cpu_types::{PictTransform, Rectangle16, Repeat},
        render::{
            engine::{RenderEngine, decode_x11_pixel_for_storage},
            glyph_pixels::GlyphSourceFormat,
            platform::{
                ConnectorSnapshot, CrtcKey, PlatformBackend, QualifiedScanoutPlan,
                is_terminal_disposable_probe_error,
            },
            scene::SceneCompositor,
            store::{
                AllocError, DrawableId, DrawableKind, DrawableStore, ImportedDmabufMetadata,
                ImportedDmabufPlane, Storage,
            },
            submit_trace::{
                Flags as SubmitFlags, Op as SubmitOp, SrcClass, SubmitEvent, SubmitKind, TargetKind,
            },
            target::{Dst, PaintTarget, Src},
            telemetry::Telemetry,
        },
        scanout_route::{RenderDeviceId, RenderKmsRelationship, ScanoutRoute},
    },
    platform::drm::ModeIdentity,
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

#[derive(Debug, Clone, Copy)]
pub(crate) struct WindowGeometry {
    pub(crate) x: i16,
    pub(crate) y: i16,
    pub(crate) width: u16,
    pub(crate) height: u16,
    pub(crate) depth: u8,
    pub(crate) mapped: bool,
    /// Mirror of core viewability, kept by `realize_window_storage` / `release_window_storage`.
    pub(crate) viewable: bool,
    pub(crate) parent: Option<u32>,
    pub(crate) stack_rank: u64,
    pub(crate) bg_pixel: Option<u32>,
    pub(crate) bg_pixmap: Option<u32>,
    /// #133 step 2 (P3) — the window's border width, in pixels, as the
    /// core tree has it. Carried on the wire by `create_subwindow` and
    /// by `HostSubwindowConfig::border_width`; recorded here so the
    /// render side has the same value as `Window::border_width` without
    /// reaching back into resources. Nothing consumes it yet: storage
    /// sizing is step 3 and the ring fill is step 4.
    pub(crate) border_width: u16,
    /// #133 step 2 (P3) — the resolved border source, flattened to
    /// primitives exactly as `bg_pixel` / `bg_pixmap` are, so the
    /// `yserver` crate need not know `resources::BorderSource`. Xorg's
    /// `PixUnion border` + `borderIsPixel` (`include/windowstr.h:146`)
    /// is an either/or, so at most one of these is `Some`: a tile
    /// pixmap wins when it has host storage, otherwise the pixel.
    /// Set from `change_subwindow_attributes` (CWBorderPixmap 0x04 /
    /// CWBorderPixel 0x08), which core forwards both at create time and
    /// on every border-attribute change.
    pub(crate) border_pixel: Option<u32>,
    pub(crate) border_pixmap: Option<u32>,
    /// Stage 5 Phase A — per-window X11 cursor attribute. `None`
    /// means inherit from the parent chain; `Some(xid)` pins a
    /// specific cursor on hover-in. Mutated by `define_cursor` only —
    /// `change_subwindow_attributes` does not decode CWCursor (the
    /// cursor arrives through `Backend::define_cursor`, which core
    /// calls for a CWA cursor change as well).
    pub(crate) cursor: Option<u32>,
}

pub(crate) type WindowsMap = HashMap<u32, WindowGeometry>;

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

/// Test-only (#133): one scene participant's placement — its host xid and
/// its output-local rects as `(x, y, w, h)`. Named so
/// `KmsBackend::scene_participant_places_for_tests` has a simple signature.
pub type ScenePlacement = (u32, Vec<(i32, i32, u32, u32)>, Vec<(i32, i32, u32, u32)>);

/// #133 step 3 (P4) — the observable shape of a resolved paint target,
/// for `KmsBackend::paint_target_shape_for_tests`: the content
/// translation into storage coordinates, the content clip as
/// `(x, y, w, h)` in storage coordinates (`None` = the whole storage),
/// and whether the resolved chain carries a border clip at all (the
/// direct-scanout gate's input, 3.5).
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScanoutM0Target {
    Cow,
    CowDescendant,
    Unredirected,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScanoutM0Coverage {
    Root,
    Output(usize),
    None,
}

/// Which retained direct frame a single CRTC is scanning out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DirectFrameSlot {
    Pending,
    Current,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ScanoutM0Shape {
    target: ScanoutM0Target,
    coverage: ScanoutM0Coverage,
    rect: Option<(i32, i32, u32, u32)>,
    source_extent: (u32, u32),
    depth: u8,
    bpp: u8,
    imported: bool,
    fourcc: u32,
    vk_format: vk::Format,
    modifier: u64,
    plane_offset: u64,
    plane_pitch: u32,
    offsets: (i16, i16),
    /// Whether the Present carried a valid / update region, NOT which one.
    ///
    /// Issue #146: these were the region XIDs. A compositor creates a fresh
    /// update region every frame (`update=0x4144dd` → `0x4144eb` →
    /// `0x4144f9` in the reporter's log), so putting the XID in the dedup key
    /// made the key change on every Present by construction — and
    /// `scanout_m0 shape`, which is supposed to log only when the shape
    /// CHANGES, logged once per composited frame instead.
    ///
    /// Only the zero-ness is ever consumed: `regions_ok` tests
    /// `valid_region_xid == 0 && update_region_xid == 0 && update_is_full`.
    /// The identities are printed in the log line straight from the
    /// candidate, so nothing is lost by keeping them out of the key.
    valid_region_present: bool,
    update_region_present: bool,
    update_is_full: bool,
}

struct ScanoutM0Telemetry {
    interval_start: std::time::Instant,
    last_shape_by_dst: HashMap<u32, ScanoutM0Shape>,
    recent_sources_by_dst: HashMap<u32, std::collections::VecDeque<DrawableId>>,
    interval_sources: HashSet<DrawableId>,
    presents: u64,
    authoritative: u64,
    root_coverage: u64,
    output_coverage: u64,
    reject_server_owned: u64,
    reject_target: u64,
    reject_geometry: u64,
    reject_offsets: u64,
    reject_regions: u64,
    m1_gate_candidates: u64,
    m1_gate_open: u64,
    m1_gate_reject_crtc: u64,
    m1_gate_reject_vt: u64,
    m1_gate_reject_outputs: u64,
    m1_gate_reject_cursor: u64,
    m1_gate_reject_overlay: u64,
    m1_gate_reject_source: u64,
    m1_gate_reject_import: u64,
    m1_probe_pass: u64,
    m1_probe_reject: u64,
    m1_probe_error: u64,
}

impl Default for ScanoutM0Telemetry {
    fn default() -> Self {
        Self {
            interval_start: std::time::Instant::now(),
            last_shape_by_dst: HashMap::new(),
            recent_sources_by_dst: HashMap::new(),
            interval_sources: HashSet::new(),
            presents: 0,
            authoritative: 0,
            root_coverage: 0,
            output_coverage: 0,
            reject_server_owned: 0,
            reject_target: 0,
            reject_geometry: 0,
            reject_offsets: 0,
            reject_regions: 0,
            m1_gate_candidates: 0,
            m1_gate_open: 0,
            m1_gate_reject_crtc: 0,
            m1_gate_reject_vt: 0,
            m1_gate_reject_outputs: 0,
            m1_gate_reject_cursor: 0,
            m1_gate_reject_overlay: 0,
            m1_gate_reject_source: 0,
            m1_gate_reject_import: 0,
            m1_probe_pass: 0,
            m1_probe_reject: 0,
            m1_probe_error: 0,
        }
    }
}

struct ScanoutM1ProbeEntry {
    /// Retained solely for its FB/GEM lifetime; `Drop` performs teardown.
    _framebuffer: Option<crate::drm::modeset::DirectScanoutProbeFramebuffer>,
}

impl ScanoutM1ProbeEntry {
    fn rejected() -> Self {
        Self { _framebuffer: None }
    }

    fn accepted(framebuffer: crate::drm::modeset::DirectScanoutProbeFramebuffer) -> Self {
        Self {
            _framebuffer: Some(framebuffer),
        }
    }

    fn framebuffer(&self) -> Option<&crate::drm::modeset::DirectScanoutProbeFramebuffer> {
        self._framebuffer.as_ref()
    }
}

struct ScanoutM1ProbeCache {
    topology_signature: u64,
    entries: HashMap<DrawableId, ScanoutM1ProbeEntry>,
}

impl ScanoutM1ProbeCache {
    fn new() -> Self {
        Self {
            topology_signature: 0,
            entries: HashMap::new(),
        }
    }

    fn remove(&mut self, id: DrawableId) {
        self.entries.remove(&id);
    }

    fn clear(&mut self, reason: &'static str) {
        if !self.entries.is_empty() {
            log::debug!(
                "scanout_m1: dropping {} cached probe framebuffer(s): {reason}",
                self.entries.len()
            );
            self.entries.clear();
        }
    }
}

struct DirectPresentFrame {
    source_pin: u64,
    fallback_target_pin: u64,
    source_id: DrawableId,
    candidate: PresentScanoutCandidate,
    fallback_target: PaintTarget,
    event: yserver_core::backend::CompletedPresentEvent,
    /// Output whose CRTC domain owns CompleteNotify/MSC for this Present.
    completion_output_idx: usize,
    /// Exact pageflip sample from the selected/reference CRTC. Xorg waits for
    /// every grouped CRTC to retire before completing, but stamps the event
    /// from this reference sample rather than the last/max output.
    completion_clock: Option<yserver_core::backend::PresentClockSample>,
    awaiting_outputs: HashSet<usize>,
}

/// Require a short stable run before entering direct ownership. Compositors
/// such as E27 alternate full-root Presents with authoritative region-limited
/// Presents; entering on every eligible member of that stream otherwise
/// causes direct/composed atomic thrash.
const SCANOUT_M2_ELIGIBLE_ROOT_PROBATION: u8 = 8;

struct ScanoutM2State {
    pending: Option<DirectPresentFrame>,
    /// One eligible direct successor, retained while `pending` owns the
    /// hardware transaction. Newer successors replace this slot (latest
    /// wins); there is never more than one not-yet-submitted direct frame.
    queued_successor: Option<DirectPresentFrame>,
    current: Option<DirectPresentFrame>,
    completed: Vec<yserver_core::backend::CompletedPresentEvent>,
    /// Coalesced successors cannot overtake the in-flight predecessor's
    /// CompleteNotify. Release their storage immediately, but publish their
    /// Skip completions only when that predecessor retires.
    deferred_successor_skips: Vec<yserver_core::backend::CompletedPresentEvent>,
    idled: Vec<yserver_core::backend::CompletedPresentEvent>,
    hold_direct: bool,
    cursor_bound_all: bool,
    unflip_requested: bool,
    /// First request in the current direct-to-composed transition. This is
    /// the causal trigger; later composite ticks must not overwrite it.
    unflip_reason: Option<&'static str>,
    /// Most recent request before submission, useful for distinguishing the
    /// initiating event from the gate that finally drove the atomic unflip.
    unflip_last_reason: Option<&'static str>,
    unflip_awaiting_outputs: HashSet<usize>,
    reentry_blocked_until_composed: bool,
    eligible_root_streak: u8,
    unflip_fallback_source: Option<DrawableId>,
    unflip_shadow_ready: bool,
    /// The synchronized composed unflip failed and was degraded to the
    /// per-output scene compose path. Keep direct-frame pins held until those
    /// composed flips replace every direct plane.
    degraded_composed_unflip: bool,
    #[cfg(test)]
    test_force_active: bool,
    #[cfg(test)]
    test_submit_direct_without_drm: bool,
}

impl ScanoutM2State {
    fn new() -> Self {
        Self {
            pending: None,
            queued_successor: None,
            current: None,
            completed: Vec::new(),
            deferred_successor_skips: Vec::new(),
            idled: Vec::new(),
            hold_direct: false,
            cursor_bound_all: false,
            unflip_requested: false,
            unflip_reason: None,
            unflip_last_reason: None,
            unflip_awaiting_outputs: HashSet::new(),
            reentry_blocked_until_composed: false,
            eligible_root_streak: 0,
            unflip_fallback_source: None,
            unflip_shadow_ready: false,
            degraded_composed_unflip: false,
            #[cfg(test)]
            test_force_active: false,
            #[cfg(test)]
            test_submit_direct_without_drm: false,
        }
    }

    fn active(&self) -> bool {
        self.pending.is_some() || self.queued_successor.is_some() || self.current.is_some() || {
            #[cfg(test)]
            {
                self.test_force_active
            }
            #[cfg(not(test))]
            {
                false
            }
        }
    }

    fn admit_eligible_root(&mut self) -> bool {
        if self.active() {
            return true;
        }
        self.eligible_root_streak = self
            .eligible_root_streak
            .saturating_add(1)
            .min(SCANOUT_M2_ELIGIBLE_ROOT_PROBATION);
        self.eligible_root_streak >= SCANOUT_M2_ELIGIBLE_ROOT_PROBATION
    }

    fn reset_eligible_root_probation(&mut self) {
        self.eligible_root_streak = 0;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ScanoutM1OutputGeometry {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    mode_width: u32,
    mode_height: u32,
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

/// Monotonic device-qualified RANDR connector registry.
///
/// Authoritative store for every connector yserver has ever seen:
/// stable ids, connection state, current config, the persistent
/// `client_configured` bit, and the last-known advertised mode list.
/// The rescan/resume layout-preservation rule and `apply_crtc_config`
/// key off this; the core only sees the resulting `Vec<RandrOutput>`.
#[derive(Debug, Default)]
pub(crate) struct RandrIdAllocator {
    next: u32,
    providers: HashMap<RandrProviderEndpoint, u32>,
    connectors: HashMap<OutputKey, ConnectorEntry>,
    modes: HashMap<ModeIdentity, u32>,
}

/// One endpoint that can own a RANDR provider XID.
///
/// Keep the variants tagged even though both verified renderer identities and
/// KMS identities ultimately contain DRM major/minor pairs. A render node and
/// a primary node with the same raw numbers are different endpoint kinds and
/// must never alias by accident. Same-device provider coalescing is an
/// explicit projection decision based on Vulkan's advertised primary node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum RandrProviderEndpoint {
    Kms(crate::platform::drm::DrmDeviceKey),
    Render(crate::kms::render::platform::RenderDeviceId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ConnectorIds {
    pub output_id: u32,
    pub crtc_id: u32,
}

/// Owned connector snapshot used while allocating mode XIDs mutably.
struct NotLiveConnector {
    key: OutputKey,
    ids: ConnectorIds,
    connected: bool,
    modes: Vec<crate::platform::drm::Mode>,
    edid: Vec<u8>,
    mm_width: u32,
    mm_height: u32,
    connector_type: String,
}

type ProjectedRandrOutputState = (bool, u32, u32, i16, i16, u16, u16);

/// Per-connector current configuration in the registry.
// Consumed by the SetCrtcConfig apply path + rescan/resume layout
// preservation (later RANDR output-management tasks); storage only here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnectorConfig {
    /// Connected but not scanning out (mode=None, no CRTC).
    Off,
    /// Scanning out at `(mode_w, mode_h, vrefresh)` placed at `(x, y)`.
    Enabled {
        mode_w: u16,
        mode_h: u16,
        vrefresh: u32,
        x: i32,
        y: i32,
    },
}

/// Everything a relight needs to put a remembered route back exactly where it
/// was, read out of a [`ConnectorConfig::Enabled`].
///
/// P3b adds the assigned CRTC XID to `ConnectorConfig::Enabled` and to this
/// struct; every relight reader goes through
/// [`ConnectorConfig::restorable_route`], so that addition stays local
/// instead of touching each consumer (design, "P3 extends `last_enabled`").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RememberedRoute {
    pub mode: yserver_core::backend::ModeSpec,
    pub x: i32,
    pub y: i32,
}

impl ConnectorConfig {
    /// The route policy this config remembers, or `None` when it is `Off`.
    pub(crate) fn restorable_route(self) -> Option<RememberedRoute> {
        match self {
            Self::Off => None,
            Self::Enabled {
                mode_w,
                mode_h,
                vrefresh,
                x,
                y,
            } => Some(RememberedRoute {
                mode: yserver_core::backend::ModeSpec {
                    width: mode_w,
                    height: mode_h,
                    vrefresh,
                },
                x,
                y,
            }),
        }
    }

    /// The layout rectangle this config occupies, or `None` when it is `Off`.
    pub(crate) fn placed_rect(self) -> Option<crate::kms::render::platform::LayoutRect> {
        match self {
            Self::Off => None,
            Self::Enabled {
                mode_w,
                mode_h,
                x,
                y,
                ..
            } => Some((x, y, mode_w, mode_h)),
        }
    }
}

/// One route [`KmsBackend::take_relight_requests`] decided to restore.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RelightRequest {
    key: OutputKey,
    mode: yserver_core::backend::ModeSpec,
    x: i32,
    y: i32,
}

/// Owned request handed to the asynchronous PRIME route qualifier.
///
/// The duplicate KMS fd keeps the exact DRM open-file description alive until
/// the worker/helper finishes, even if hotplug or backend teardown retires the
/// platform entry before this job starts. The request itself contains only
/// scalar handles and route identities; live output ownership remains in the
/// pending backend entry.
pub(crate) struct CrtcConfigProbeJob {
    pub(crate) kms_fd: OwnedFd,
    pub(crate) request: RouteProbeRequest,
}

/// One completed asynchronous qualification. A successful plan contains no
/// live Vulkan/DRM object and can therefore be rejected safely if its topology
/// or VT snapshot has gone stale.
pub(crate) struct CrtcConfigProbeCompletion {
    pub(crate) token: CrtcConfigToken,
    pub(crate) result: io::Result<QualifiedScanoutPlan>,
}

/// Injectable owner of the worker/helper transport. Production sends the owned
/// job through the process-isolated qualifier; tests use an in-memory
/// implementation without changing the core-facing token lifecycle.
pub(crate) trait CrtcConfigProbeExecutor {
    fn set_core_sender(&mut self, _sender: yserver_core::core_loop::CoreSender) {}
    /// Queue one process-helper request and return without performing the
    /// qualification or waiting for the child on the core thread.
    fn enqueue(&mut self, job: CrtcConfigProbeJob) -> io::Result<()>;
    /// Non-blockingly drain results already delivered by the helper owner.
    fn drain_ready(&mut self) -> Vec<CrtcConfigProbeCompletion>;
    /// Best-effort, non-blocking cancellation/retirement. Implementations must
    /// tolerate this racing a completed child and repeated calls.
    fn cancel(&mut self, _token: CrtcConfigToken) {}
}

struct PendingCrtcConfigProbe {
    output_id: u32,
    output_key: OutputKey,
    connector: String,
    mode: yserver_core::backend::ModeSpec,
    x: i32,
    y: i32,
    route: ScanoutRoute,
    prepared_output: Option<crate::platform::drm::Output>,
    topology_signature: u64,
    topology_epoch: u64,
    vt_state: crate::vt::state::VtState,
    was_active: bool,
}

/// One connector the backend has ever seen.
// Fields consumed by later RANDR output-management tasks (reprobe,
// SetCrtcConfig apply, layout preservation); storage only here.
#[derive(Debug, Clone)]
pub(crate) struct ConnectorEntry {
    pub ids: ConnectorIds,
    pub connected: bool,
    pub config: ConnectorConfig,
    /// Whether `GetOutputInfo.crtc` retains this connector's former CRTC.
    /// Physical loss retains the association; an explicit client disable
    /// clears it. It cannot be inferred from `connected` or `config`: both
    /// physical loss and a deliberate disable leave the route Off.
    pub crtc_associated: bool,
    /// `true` once a client SetCrtcConfig/SetScreenSize touched this
    /// output. The auto-layout (recompact / boot extend-right) only
    /// ever touches `!client_configured` outputs. A lightweight connection
    /// query retains it because that query does not detach the CRTC; the
    /// later heavy physical-topology apply clears it when the route goes.
    pub client_configured: bool,
    /// Route policy of a connector retired by a **physical** disconnect,
    /// kept so the reconnect edge relights it with no client request
    /// (design P1b, invariant 1). `None` whenever nothing is remembered: a
    /// client's explicit `SetCrtcConfig`, an incompatible-mode reconnect and
    /// a successful relight all clear it.
    ///
    /// This is also the reservation record. While it is `Some` and the route
    /// is not live, the remembered rectangle is excluded from auto-layout
    /// packing and unioned into the virtual-screen extent, so a survivor
    /// never moves into the hole a restorable route left (invariant 7).
    /// Releasing a reservation is exactly clearing this field — there is no
    /// second record to drop.
    pub last_enabled: Option<ConnectorConfig>,
    /// Last-known advertised mode list, preferred-first. Retained across
    /// disconnect so a momentarily-gone monitor keeps reporting stable mode
    /// resources until reconnect refreshes them.
    pub modes: Vec<crate::platform::drm::Mode>,
    /// Connector identity from the latest heavy startup/hotplug/resume
    /// snapshot. Lightweight RANDR force queries never refresh these fields;
    /// they invalidate monitor-owned EDID/dimensions when connection or mode
    /// evidence says the previous identity may have departed.
    pub edid: Vec<u8>,
    pub mm_width: u32,
    pub mm_height: u32,
    pub connector_type: String,
}

#[derive(Debug, Default)]
struct ConnectorRegistryDelta {
    /// Outputs whose advertised connection, modes, or monitor identity
    /// changed and therefore need OutputChangeNotify.
    changed_keys: Vec<OutputKey>,
    /// Xorg's config timestamp covers available output/mode configuration,
    /// not EDID, millimeter dimensions, or connector-type metadata alone.
    config_changed: bool,
}

impl ConnectorRegistryDelta {
    fn is_empty(&self) -> bool {
        self.changed_keys.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct GammaLut {
    red: Vec<u16>,
    green: Vec<u16>,
    blue: Vec<u16>,
}

impl GammaLut {
    fn identity(size: u16) -> Self {
        let ramp = identity_ramp(size);
        Self {
            red: ramp.clone(),
            green: ramp.clone(),
            blue: ramp,
        }
    }

    fn len(&self) -> usize {
        self.red.len()
    }

    fn resampled(&self, size: u16) -> Self {
        let dst_len = usize::from(size);
        Self {
            red: resample_channel(&self.red, dst_len),
            green: resample_channel(&self.green, dst_len),
            blue: resample_channel(&self.blue, dst_len),
        }
    }
}

impl RandrIdAllocator {
    fn fresh(&mut self) -> u32 {
        self.next = self.next.saturating_add(1);
        self.next
    }

    pub(crate) fn provider_id_for(&mut self, endpoint: RandrProviderEndpoint) -> u32 {
        if let Some(id) = self.providers.get(&endpoint) {
            return *id;
        }
        let id = self.fresh();
        self.providers.insert(endpoint, id);
        id
    }

    /// Resolve a provider XID back to its tagged endpoint identity.
    ///
    /// The allocator is monotonic, so this may return an endpoint that was
    /// projected earlier in the process lifetime but is not part of the
    /// current provider set. Request paths must additionally validate the
    /// result against [`KmsBackend::randr_provider_endpoints`].
    pub(crate) fn provider_endpoint_for_id(
        &self,
        provider_id: u32,
    ) -> Option<RandrProviderEndpoint> {
        self.providers
            .iter()
            .find_map(|(endpoint, id)| (*id == provider_id).then_some(*endpoint))
    }

    pub(crate) fn ids_for(&mut self, output_key: &OutputKey) -> ConnectorIds {
        if let Some(entry) = self.connectors.get(output_key) {
            return entry.ids;
        }
        let ids = ConnectorIds {
            output_id: self.fresh(),
            crtc_id: self.fresh(),
        };
        self.connectors.insert(
            output_key.clone(),
            ConnectorEntry {
                ids,
                connected: false,
                config: ConnectorConfig::Off,
                crtc_associated: false,
                client_configured: false,
                last_enabled: None,
                modes: Vec::new(),
                edid: Vec::new(),
                mm_width: 0,
                mm_height: 0,
                connector_type: String::new(),
            },
        );
        ids
    }

    pub(crate) fn mode_id(&mut self, mode: &crate::platform::drm::Mode) -> u32 {
        let identity = ModeIdentity::from(mode);
        if let Some(id) = self.modes.get(&identity) {
            return *id;
        }
        let id = self.fresh();
        self.modes.insert(identity, id);
        id
    }

    /// Output keys whose layout a client has explicitly configured
    /// (SetCrtcConfig/SetScreenSize). The auto-layout recompact must
    /// leave these where the client placed them — see Task 5.1.
    pub(crate) fn client_configured_keys(&self) -> HashSet<OutputKey> {
        self.connectors
            .iter()
            .filter(|(_, e)| e.client_configured)
            .map(|(key, _)| key.clone())
            .collect()
    }

    pub(crate) fn connected_keys(&self) -> HashSet<OutputKey> {
        self.connectors
            .iter()
            .filter(|(_, entry)| entry.connected)
            .map(|(key, _)| key.clone())
            .collect()
    }

    pub(crate) fn known_connectors(&self) -> Vec<(OutputKey, ConnectorIds)> {
        self.connectors
            .iter()
            .map(|(key, entry)| (key.clone(), entry.ids))
            .collect()
    }

    /// Get or create the registry entry for a connector, allocating
    /// stable ids on first sight. New entries default to disconnected,
    /// Off, not-client-configured, empty mode list.
    pub(crate) fn entry_mut(&mut self, key: &OutputKey) -> &mut ConnectorEntry {
        if !self.connectors.contains_key(key) {
            // Allocate ids (also inserts the default entry).
            let _ = self.ids_for(key);
        }
        self.connectors.get_mut(key).expect("just inserted")
    }

    #[allow(dead_code)]
    pub(crate) fn entry(&self, key: &OutputKey) -> Option<&ConnectorEntry> {
        self.connectors.get(key)
    }

    pub(crate) fn entries(&self) -> impl Iterator<Item = (&OutputKey, &ConnectorEntry)> {
        self.connectors.iter()
    }
}

/// v2 sibling backend. Shares `KmsCore` with `KmsBackend`;
/// owns `PlatformBackend` (real DRM/Vk/libinput per Stage 2a)
/// plus stub `DrawableStore` / `RenderEngine` / `SceneCompositor`
/// that fill in across Stages 2b–2e. Paint / RENDER / scene ops
/// log gaps until those substages land.
pub struct KmsBackend {
    /// Shared protocol-bookkeeping state. Identical to v1's
    /// `KmsBackend.core` — same struct, same construction path.
    pub(crate) core: KmsCore,

    /// Real DRM/KMS/libinput/Vulkan owner per Stage 2a. Replaced
    /// the flat field set Stage 1b carried.
    pub(crate) platform: PlatformBackend,

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

    /// M0 direct-scanout telemetry. Purely observational: it never owns a
    /// drawable pin, submits DRM work, or affects Present capabilities.
    scanout_m0: ScanoutM0Telemetry,
    scanout_m1: ScanoutM1ProbeCache,
    scanout_m2: ScanoutM2State,

    /// Per-CRTC armed absolute MSC for idle vblank pacing. Keyed by the
    /// stable `crtc::Handle`; presence means "a `DRM_CRTC_SEQUENCE` is
    /// queued on this CRTC and we are waiting for it" (value `0` is the
    /// relative-next-vblank sentinel this spike uses). A per-CRTC map
    /// rather than a single bool so arming CRTC A can't suppress arming
    /// CRTC B (the dual-monitor permanent-stall class).
    ///
    /// **Invariant (load-bearing):** every code path that drops or
    /// completes an armed sequence MUST clear the matching entry —
    /// `on_crtc_sequence_event` (unconditional clear-arm before
    /// validating), `!scanout_allowed()` in `arm_idle_vblanks_with`,
    /// `run_suspend` / DPMS-off (master loss drops queued sequences), and
    /// output removal (`prune_armed_targets_to_live_outputs`). A stuck
    /// entry = a permanent ~0 fps stall on that CRTC.
    pub(crate) armed_vblank_targets: std::collections::HashMap<CrtcKey, u64>,

    /// Per-CRTC absolute (per-target) armed MSCs for deferred Present
    /// execution (spec §msc-due future-target rule). Keyed by raw
    /// `crtc_id` (not `Handle` — `arm_present_absolute_vblank` deals in
    /// the same `u32` ids `queue_crtc_sequence` takes). Own set,
    /// entirely independent of `armed_vblank_targets`: multiple
    /// in-flight `CRTC_QUEUE_SEQUENCE`s per CRTC are legal, and the
    /// absolute arm must neither consume nor suppress the relative-1
    /// idle arm (which a compositor parks every iteration — sharing the
    /// slot would starve one arm kind or the other). Entries retire in
    /// `on_crtc_sequence_event` when a tagged event's `sequence` reaches
    /// the target; `clear_all_armed_vblank_targets` clears this
    /// alongside `armed_vblank_targets` at the same lifecycle edges.
    pub(crate) absolute_vblank_targets:
        std::collections::HashMap<CrtcKey, std::collections::BTreeSet<u64>>,

    /// Latches true the first time `DRM_IOCTL_CRTC_QUEUE_SEQUENCE` returns
    /// EOPNOTSUPP/ENOTTY (pre-4.14 kernels lack the ioctl). Once set we stop
    /// attempting idle arming and degrade to flip-driven MSC only. Shared
    /// between the relative idle arm (`arm_idle_vblanks_ioctl`) and the
    /// absolute per-target arm (`arm_present_absolute_vblank`) — the ioctl
    /// is either supported or not, independent of which caller issues it.
    /// Logged once on transition; never resets within a process lifetime.
    pub(crate) crtc_queue_sequence_unsupported_devices: HashSet<crate::platform::drm::DrmDeviceKey>,

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

    /// DRI3 `FenceFromFD` xshmfence-backed fences keyed by the
    /// client's xid. Mesa's loader_dri3 uses xshmfence (memfd +
    /// futex) for idle/sync fences; the mmap'd mapping lets us
    /// `xshmfence_trigger` directly when the X side wants to
    /// signal idle. Mirrors v1's `dri3_xshmfences` field shape.
    pub(crate) dri3_xshmfences: HashMap<u32, std::sync::Arc<crate::kms::xshmfence::FenceMapping>>,
    /// DRI3 sync-fence resources keyed by the client's xid, from
    /// `FenceFromFD` falling through the xshmfence path (sync_file fd →
    /// `VkSemaphore`). Syncobjs live in `dri3_syncobjs`.
    pub(crate) dri3_sync_resources:
        HashMap<u32, std::sync::Arc<crate::kms::render::owned_semaphore::OwnedSemaphore>>,
    /// DRI3 1.4 syncobj resources keyed by the client's xid, imported by
    /// `ImportSyncobj`. The tuple's first element is the importing client —
    /// Xorg models a DRI3 syncobj as a first-class X resource owned by a
    /// client (`dri3_syncobj_type = CreateNewResourceType(...)`), and every
    /// conformance property in the spec's table falls out of that one
    /// decision: `FreeSyncobj` ownership, the disconnect purge, and the
    /// `PresentPixmapSynced` xid checks.
    pub(crate) dri3_syncobjs: HashMap<
        u32,
        (
            yserver_protocol::x11::ClientId,
            std::sync::Arc<crate::kms::render::imported_syncobj::ImportedSyncobj>,
        ),
    >,

    /// Result of the one-shot `DRM_IOCTL_SYNCOBJ_EVENTFD` probe: `None` until
    /// probed, then the answer for the rest of the session.
    ///
    /// FreeBSD's drm-kmod reports `DRM_CAP_SYNCOBJ_TIMELINE` (which is what
    /// `Dri3Caps::syncobj` derives from) but does not implement the eventfd
    /// ioctl, which is a much later Linux addition. Without this latch every
    /// synced Present logged a warning and retried a call that cannot
    /// succeed — a warning per frame, on a path where per-frame logging is
    /// known to perturb the timing it reports.
    syncobj_eventfd_supported: Option<bool>,

    /// Latches the first "dma-buf sync-file export unsupported" warning.
    ///
    /// `DMA_BUF_IOCTL_EXPORT_SYNC_FILE` is absent on FreeBSD's drm-kmod and
    /// these three sites run per Present: one GhostBSD/MATE session logged 412
    /// of them. Unlike the syncobj eventfd probe this latches only the LOG,
    /// not the behaviour — `ExportedSyncFile::Unsupported` conflates "no such
    /// ioctl" with "this buffer failed", and copying immediately is correct
    /// either way, so suppressing the attempt could disable a working path.
    /// `Cell` permits the source site to record it while still holding a
    /// `&self` borrow of `self.store`; the backend remains on the core thread.
    dmabuf_sync_file_warned: Cell<bool>,

    /// Stage 5 Task 6.1: queue of in-flight deferred PRESENT
    /// completion batches. Drained by `drain_completed_present_events`
    /// when the inner `present_completion_epfd` reports a submitted
    /// batch's exported sync_file as readable, or when the degraded
    /// fallback path pokes `wakeup_eventfd`. Each entry inside each
    /// batch pins an Arc clone of the wake primitive (xshmfence /
    /// syncobj) so the underlying resource survives an intervening
    /// `XFixesDestroyFence` / `FreeSyncobj`.
    pub(crate) pending_present_batches:
        std::collections::VecDeque<crate::kms::render::present_completion::PendingPresentBatch>,

    /// Pinned wakes drained to core but not yet signalled — held here so
    /// core can pace when the real xshmfence / syncobj fires. Keyed by
    /// `CompletedPresentEvent::present_id`.
    retained_present_wakes:
        std::collections::HashMap<u64, crate::kms::render::present_completion::PinnedWake>,

    /// Imported Present sources parked on their producer sync-file. The exact
    /// source `DrawableId` is incref'd while an entry lives here.
    pub(crate) pending_present_source_waits:
        HashMap<u64, crate::kms::render::present_source_wait::PendingPresentSourceWait>,
    pub(crate) next_present_source_wait_id: u64,

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

    // ── VT switching (Direct/VT_PROCESS). Suspend/resume state machine,
    //    driven by VtRelease/VtAcquire (SIGUSR1/2) → `drive_vt_event`.
    /// State machine for the suspend/resume cycle.
    /// Used by `scanout_allowed()` / `run_suspend`.
    vt_state: crate::vt::state::VtState,
    /// Coalesced counter-events for the state machine.
    /// Consumed by `drive_vt_event`.
    vt_pending: crate::vt::state::VtPending,
    /// Controlling console TTY guard. Present in direct mode when the
    /// process is launched on a real VT; `None` when no controlling
    /// console exists (pty / graphical terminal / test harness).
    /// On non-console platforms this is a unit placeholder (unused).
    #[cfg_attr(not(any(target_os = "linux", target_os = "freebsd")), allow(dead_code))]
    console_guard: crate::kms::ConsoleGuardOpt,
    /// Whether VT switching has been armed in direct mode.
    vt_switching_armed: bool,
    /// Direct-mode lock-LED sink: the input thread owns the libinput
    /// devices, so LED changes cross via this relay (mask + eventfd in
    /// the thread's epoll). `None` in test fixtures.
    led_relay: Option<std::sync::Arc<crate::input::LedRelay>>,
    /// Last `input::Led` bits pushed — dedup so only lock-state
    /// TRANSITIONS reach the hardware, not every key event.
    leds_sent: u32,
    /// Independent XKB cooking states for keyboard slaves detached by an
    /// explicit XI2 grab. The XI device ID is stable for the grab lifetime.
    floating_keyboard_states: HashMap<u16, FloatingKeyboardState>,
    /// Xorg's XkbFilterLockState `filter->priv` and `filter->upAction` for
    /// each held LockMods key.
    lock_filter_priv_by_device: HashMap<u16, HashMap<u8, LockFilterPriv>>,
    /// Core-channel sender for backend-originated shutdowns. Handed in via
    /// `set_input_sender` after the channel is created in `lib.rs`.
    input_sender: Option<yserver_core::core_loop::CoreSender>,
    /// Optional asynchronous cross-device scanout qualifier. Absence keeps the
    /// existing synchronous `apply_crtc_config` path unchanged.
    crtc_config_probe_executor: Option<Box<dyn CrtcConfigProbeExecutor>>,
    pending_crtc_config_probes: HashMap<CrtcConfigToken, PendingCrtcConfigProbe>,
    ready_crtc_config_results: HashMap<CrtcConfigToken, io::Result<QualifiedScanoutPlan>>,
    invalidated_crtc_config_probes: HashSet<CrtcConfigToken>,
    ready_crtc_config_announcements: VecDeque<CrtcConfigToken>,
    next_crtc_config_token: u64,
    next_device_config_token: u64,
    /// Invalidates results across any topology quiesce or VT event, including
    /// an ABA transition which happens to restore the same final layout.
    crtc_config_topology_epoch: u64,
    /// Test-only discovery result used to exercise the real `begin` hook with
    /// the fd-less DRM fixture. Production always performs live discovery.
    #[cfg(test)]
    crtc_config_discovery_override: Option<crate::platform::drm::Output>,
    /// Direct-mode input-thread control channel. Used by VT release /
    /// acquire to pause and resume the dedicated input thread.
    input_thread_control: Option<std::sync::Arc<crate::input_thread::InputThreadControl>>,
    randr_id_alloc: RandrIdAllocator,
    /// Session-persistent PRIME Output Source policy, keyed by the KMS sink.
    ///
    /// Values use the provider's tagged endpoint identity rather than a DRM
    /// primary-node key: the selected renderer may be a distinct render node
    /// or the unverified Vulkan fallback, neither of which can be represented
    /// truthfully as a KMS device. Startup seeds every distinct opened KMS
    /// endpoint once; a later explicit detach removes that entry. Provider
    /// XIDs are only a projection of this policy and may be rebuilt without
    /// changing it.
    provider_output_sources: HashMap<crate::platform::drm::DrmDeviceKey, RandrProviderEndpoint>,
    /// Per-output-id identity for RANDR output properties: `(EDID blob,
    /// ConnectorType name)`. Rebuilt by `randr_outputs_and_modes` each
    /// time RANDR state is (re)projected; read by the `EDID`/`EDID_DATA`
    /// /`ConnectorType` output-property handlers via `output_identity`.
    output_identity_by_id: std::collections::HashMap<u32, (Vec<u8>, String)>,
    /// Resolve a RANDR output XID back to its owning DRM device and connector.
    /// Connector names are only unique within one DRM device.
    output_key_by_id: std::collections::HashMap<u32, OutputKey>,
    /// RANDR CRTC XID to the owning device-qualified connector. Gamma
    /// requests address the CRTC XID, so connector names alone are ambiguous
    /// once multiple cards can both expose (for example) `DP-1`.
    crtc_key_by_id: std::collections::HashMap<u32, OutputKey>,
    /// Live Present clock-domain generation per RANDR CRTC XID. The epoch is
    /// retained only while that XID resolves to the same active
    /// device-qualified CRTC. Off/removal or a KMS CRTC reassignment allocates
    /// a fresh epoch on the next live projection, letting core discard raw MSC
    /// targets captured in the old counter domain.
    present_crtc_clock_epochs: HashMap<u32, (CrtcKey, u64)>,
    next_present_crtc_clock_epoch: u64,
    hotplug_rescan_deadline: Option<std::time::Instant>,
    gamma_luts: RefCell<HashMap<OutputKey, GammaLut>>,

    /// GLX-TFP (Tasks 2.3 + 2.4): per-`DrawableId` export tracking for
    /// pixmaps shared with a GL consumer via `GLX_EXT_texture_from_pixmap`.
    /// AT MOST ONE entry per drawable regardless of how many times
    /// `BufferFromPixmap` is issued for it (muffin re-exports per damage).
    /// Canonical owner of the export lifetime ref + the dup'd dma-buf fd;
    /// a parallel sync-only fd dup lives on `store.exported_sync` so the
    /// engine's flush chokepoint can reach it.
    exported_dmabufs: HashMap<crate::kms::render::store::DrawableId, ExportedBacking>,

    /// Cached result of `probe_dmabuf_export_support` run once at
    /// construction.  When `true`, `ServerState::glx_tfp_supported` is
    /// set and the GLX extension string advertises
    /// `GLX_EXT_texture_from_pixmap`.
    dmabuf_export_supported: bool,

    /// Last `export holders` report, for change detection.
    export_holders: crate::kms::render::export_holders::ExportHoldersReporter,
}

/// GLX-TFP export state for one drawable. See `exported_dmabufs`.
struct ExportedBacking {
    /// A dup of the exported dma-buf fd, kept for implicit-sync fence
    /// I/O. `None` until the first `BufferFromPixmap` export (an entry
    /// can exist earlier, created by an `acquire_glx_pixmap_export` from
    /// `glXCreatePixmap`).
    fd: Option<std::os::fd::OwnedFd>,
    /// How many live GLXPixmaps reference this backing. Teardown fires
    /// when this reaches 0.
    glx_refs: u32,
    /// True once the single lifetime ref has been taken (so teardown
    /// releases it exactly once).
    lifetime_ref_held: bool,
    /// The backing's `DrawableId`, captured at ref-take time so teardown
    /// can decref the store entry directly even if the `by_xid` mapping
    /// was detached by an intervening `FreePixmap` (PendingFence).
    backing_id: crate::kms::render::store::DrawableId,
    /// The backing's host-xid handle, captured at ref-take time so
    /// teardown can release the alias_registry counter (keyed by xid)
    /// even after the store entry is gone.
    backing: PixmapHandle,
    /// Which counter holds the lifetime ref: `true` = `alias_registry`
    /// (NameWindowPixmap'd redirect backing); `false` = `DrawableStore`
    /// refcount (plain pixmap). Captured at take time so an alias
    /// torn-down between take and release can't misroute the decref.
    lifetime_via_alias: bool,
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

/// user_data tag for absolute (per-target) sequence arms. The kernel
/// event carries no CRTC; the low 32 bits stay the crtc_id (matching the
/// relative arm's `user_data = u64::from(crtc_id)`), the high bit
/// discriminates the arm kind so `on_crtc_sequence_event` can tell an
/// absolute-arm retirement apart from the single-slot relative idle arm
/// (spec round-4 F2).
const ABSOLUTE_SEQ_TAG: u64 = 1 << 63;

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

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum ScanoutReadSelection {
    OnScreenOnly,
    PermissiveDump,
}

/// Where an on-screen read of one rect actually has to come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScanoutReadRoute {
    /// The compositor's own scanout BO. `local` is the rect rebased into that
    /// BO's coordinate space.
    Pool {
        pool_idx: usize,
        bo_idx: usize,
        local: vk::Rect2D,
    },
    /// A transformed output's intermediate, which holds root pixels. `local`
    /// is `requested_root ∩ footprint_root − crtc_origin` (spec D6).
    Intermediate {
        output_idx: usize,
        local: vk::Rect2D,
    },
    /// A client drawable flipped directly onto this output's CRTC. `source` is
    /// the rect rebased into that drawable's own coordinate space.
    Direct {
        source_id: DrawableId,
        source_xid: u32,
        depth: u8,
        source: vk::Rect2D,
    },
}

/// Which buffer a completed read came out of. Reported so a diagnostic dump
/// can name it and a stale artifact can never be mistaken for the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScanoutReadOrigin {
    /// Degenerate read of an empty rect; no buffer was touched.
    Empty,
    ComposedPool {
        pool_idx: usize,
        bo_idx: usize,
    },
    TransformIntermediate {
        output_idx: usize,
    },
    /// The scene composed afresh for a root read (`fresh_root_readback`).
    RootReadback {
        output_idx: usize,
    },
    DirectSource {
        source_xid: u32,
    },
}

impl ScanoutReadOrigin {
    /// Short filename-safe tag naming the buffer that was read.
    fn label(self) -> String {
        match self {
            Self::Empty => "empty".to_string(),
            Self::ComposedPool { pool_idx, bo_idx } => {
                format!("composed-pool{pool_idx}-bo{bo_idx}")
            }
            Self::TransformIntermediate { output_idx } => {
                format!("transform-intermediate-out{output_idx}")
            }
            Self::RootReadback { output_idx } => format!("root-readback-out{output_idx}"),
            Self::DirectSource { source_xid } => format!("direct-src-0x{source_xid:x}"),
        }
    }
}

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

/// Granularity of [`ensure_scanout_readback`] growth: a full-screen read
/// allocates once, and the per-row reads that follow fit.
const SCANOUT_READBACK_GRANULE: u64 = 1024 * 1024;

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

#[cfg(test)]
#[path = "crtc_transform_tests.rs"]
mod crtc_transform_tests;
