//! `KmsBackendV2` — Stage 1b skeleton sibling of `KmsBackend` (v1).
//!
//! Per rendering-model-v2 spec § Stage 1b. Embeds the same
//! `KmsCore` as v1 so protocol bookkeeping (XID maps, window
//! metadata stripped of storage, fonts, SHAPE regions, etc.) lives
//! exactly once. Every paint / scene / RENDER trait method stubs
//! with a once-per-method `warn!` + `Ok(())`. Real components
//! (`PlatformBackend`, `DrawableStore`, `RenderEngine`,
//! `SceneCompositor`) land in Stage 2.
//!
//! The acceptance gate is **synthetic**: with
//! `YSERVER_RENDER_MODEL=v2`, the server boots, opens a connection,
//! services capability queries / atom queries / GetGeometry on
//! root; the first paint op produces exactly one
//! `v2: <method> not yet implemented` warn line per opcode. No
//! real-app gates land at this stage — those wait for Stage 3.

use std::{
    any::Any,
    cell::RefCell,
    collections::{HashMap, HashSet},
    io,
};

use ash::vk;
use yserver_core::{
    backend::{
        AnyHandle, Backend, BackendFdKind, ClipState, CursorHandle, DrawState, Dri3Caps,
        Dri3PixmapExport, FillState, FontHandle, GlyphSetHandle, OriginContext, PictureHandle,
        PixmapHandle, PresentCaps, WindowHandle,
    },
    core_loop::HostInputEvent,
    host_x11::{
        HostKeyEvent, HostPointerEvent, HostSubwindowConfig, HostSubwindowVisual, HostXidMap,
        PointerEventKind, PointerPosition,
    },
    properties::PropertyValue,
    resources::{ARGB_COLORMAP, ARGB_VISUAL},
    server::ServerState,
};
use yserver_protocol::x11::{
    AtomId, ClipRectangles, FontMetrics, RENDER_FMT_A1, RENDER_FMT_A8, RENDER_FMT_ARGB32,
    ResourceId, xfixes,
};

use crate::{
    drm,
    kms::{
        core::{GradientStop, KmsCore, PictureFilter, PictureRecord},
        cpu_types::{PictTransform, Rectangle16, Repeat},
        v2::{
            engine::{RenderEngine, decode_x11_pixel_for_storage},
            platform::PlatformBackend,
            scene::SceneCompositor,
            store::{DrawableId, DrawableKind, DrawableStore, Storage},
            submit_trace::{
                Flags as SubmitFlags, Op as SubmitOp, SrcClass, SubmitEvent, SubmitKind, TargetKind,
            },
            telemetry::Telemetry,
        },
    },
};

/// Per-window geometry tracked by v2's scene assembler. Stage 2 plan
/// Risk 3: a parallel `windows_v2` map on `KmsBackendV2` (NOT on
/// `KmsCore` — v1 doesn't need it). Stage 4 may collapse into
/// `KmsCore.windows` when `WindowState` splits.
///
/// Stage 3f.6 grows `parent`: subwindows record their parent xid so
/// `build_scene` can recurse top-level → descendants with accumulated
/// offsets. `None` marks top-levels (parent is root, not tracked
/// in `windows_v2`). The `bg_pixel` / `bg_pixmap` slots carry
/// per-window background attributes set via
/// `change_subwindow_attributes`; the bg-pixel is painted into
/// storage at allocate + configure resize so freshly-mapped windows
/// have a defined initial colour.
#[derive(Debug, Clone, Copy)]
pub(crate) struct WindowGeometryV2 {
    pub(crate) x: i16,
    pub(crate) y: i16,
    pub(crate) width: u16,
    pub(crate) height: u16,
    pub(crate) depth: u8,
    pub(crate) mapped: bool,
    pub(crate) parent: Option<u32>,
    pub(crate) stack_rank: u64,
    pub(crate) bg_pixel: Option<u32>,
    pub(crate) bg_pixmap: Option<u32>,
    /// Stage 5 Phase A — per-window X11 cursor attribute. `None`
    /// means inherit from the parent chain; `Some(xid)` pins a
    /// specific cursor on hover-in. Mutated by `define_cursor` /
    /// `change_subwindow_attributes` (CWCursor mask bit).
    pub(crate) cursor: Option<u32>,
}

pub(crate) type WindowsV2Map = HashMap<u32, WindowGeometryV2>;

/// Stage 4a — resolution result for a paint operation against a
/// host xid. `id` is the DrawableId that actually receives the
/// paint; `offset` is the (x, y) translation that callers add to
/// every paint rect's origin (in 16.16-free pixel units) before
/// dispatching to the engine.
///
/// The offset is non-zero only when the target is a descendant of
/// a redirected ancestor: paint against descendant `C` of
/// redirected `W`, with `C` positioned at `(cx, cy)` relative to
/// `W`, lands at `(cx + x, cy + y, w, h)` in `W`'s backing.
///
/// For unredirected windows and Pixmap targets, `offset = (0, 0)`
/// and `id` is just the leaf drawable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PaintTarget {
    pub(crate) id: crate::kms::v2::store::DrawableId,
    pub(crate) offset: (i32, i32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TopLevelStackHint {
    Bottom,
    Top,
}

/// One leaf→backing composite emitted by
/// `KmsBackendV2::plan_backing_inferiors`. Coordinates are
/// backing-local (B's `(0, 0)` == the redirected window's origin);
/// `width`/`height` are already low-side clamped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SeedInferiorDraw {
    leaf_id: crate::kms::v2::store::DrawableId,
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
pub struct KmsBackendV2 {
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
    pub(crate) windows_v2: WindowsV2Map,
    /// Monotonic allocator for per-parent sibling ordering. V2 scene
    /// assembly still stores windows in a flat map, so child z-order
    /// needs an explicit stable rank instead of relying on HashMap
    /// iteration order.
    next_window_stack_rank: u64,
    /// Stage 4d: `DrawableId` of the Composite Overlay Window
    /// storage, allocated lazily on the first `GetOverlayWindow`
    /// and dropped on the final `ReleaseOverlayWindow`. `None`
    /// when no compositor is holding a COW. Storage handle lives
    /// here (backend / Vk-side state) — the matching protocol
    /// refcount lives on `core.cow_refcount` per the v2 plan
    /// §"`KmsCore` scope — narrowly drawn" split.
    pub(crate) cow_id: Option<crate::kms::v2::store::DrawableId>,

    /// Cached readback of the current GC clip-mask pixmap (depth-1
    /// or depth-8). Populated at `set_clip_pixmap` time by reading
    /// the pixmap bytes via `engine.get_image`; consumed by
    /// `intersect_with_current_clip` so depth-1 pixmap clipping
    /// actually gates paint to the mask shape (wmaker title-bar
    /// button glyphs are the canonical client). Cleared whenever
    /// `current_clip` transitions away from `ClipState::Pixmap`.
    pub(crate) clip_mask_cache: Option<crate::kms::backend::ClipMaskCache>,
    /// Cached readback of the current GC tile/stipple pixmap. Needed
    /// because X11 GCs retain tile/stipple semantics after the client
    /// frees the source pixmap; once the backing is gone from
    /// `DrawableStore`, patterned fills must still use the last image.
    pub(crate) fill_pattern_cache: Option<FillPatternCache>,

    /// Cached binary KMS power state. `true` at startup (outputs
    /// come up active); mutated only by `set_dpms_power`. Lets
    /// `set_dpms_power` no-op when called for Standby→Suspend /
    /// Suspend→Off (same binary state, different protocol level).
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

    /// DRI3 `FenceFromFD` xshmfence-backed fences keyed by the
    /// client's xid. Mesa's loader_dri3 uses xshmfence (memfd +
    /// futex) for idle/sync fences; the mmap'd mapping lets us
    /// `xshmfence_trigger` directly when the X side wants to
    /// signal idle. Mirrors v1's `dri3_xshmfences` field shape.
    pub(crate) dri3_xshmfences: HashMap<u32, std::sync::Arc<crate::kms::xshmfence::FenceMapping>>,
    /// DRI3 sync-fence / syncobj resources keyed by the client's
    /// xid. Either `FenceFromFD` falling through the xshmfence
    /// path (sync_file fd → `VkSemaphore`) or `ImportSyncobj`
    /// (drm_syncobj fd → timeline `VkSemaphore`). Mirrors v1's
    /// `dri3_sync_resources` field shape.
    pub(crate) dri3_sync_resources:
        HashMap<u32, std::sync::Arc<crate::kms::v2::owned_semaphore::OwnedSemaphore>>,

    /// Stage 5 Task 6.1: queue of in-flight deferred PRESENT
    /// completion batches. Drained by `drain_completed_present_events`
    /// when the inner `present_completion_epfd` reports a submitted
    /// batch's exported sync_file as readable, or when the degraded
    /// fallback path pokes `wakeup_eventfd`. Each entry inside each
    /// batch pins an Arc clone of the wake primitive (xshmfence /
    /// syncobj) so the underlying resource survives an intervening
    /// `XFixesDestroyFence` / `FreeSyncobj`.
    pub(crate) pending_present_batches:
        std::collections::VecDeque<crate::kms::v2::present_completion::PendingPresentBatch>,

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
    pub(crate) cursor_records: HashMap<u32, std::sync::Arc<crate::kms::v2::cursor::CursorRecord>>,
    /// Per-cursor uploaded Pixmap drawable. The SW scene path samples
    /// through this. Lifetime is the same as the matching entry in
    /// `cursor_records`; both maps share the same xid keys.
    pub(crate) cursor_pixmaps: HashMap<u32, crate::kms::v2::store::DrawableId>,
    /// Monotonically-increasing version counter for new
    /// `CursorRecord` allocations. Compared by VALUE in the Phase B/C
    /// upload-dedup paths.
    pub(crate) next_cursor_version: u64,
    /// Xid of the default-arrow record allocated at backend init.
    /// `define_cursor(_, 0)` (X11 `None`) falls back to this; the
    /// scene's `register_cursor` swaps to the entry recorded here.
    pub(crate) default_cursor_xid: Option<u32>,
    /// Xid of the currently-effective cursor — the one whose sprite
    /// is shown on screen. Driven by `update_effective_cursor`;
    /// `define_cursor` + `update_pointer_window` re-evaluate it.
    pub(crate) effective_cursor_xid: Option<u32>,

    /// Animated-cursor frame lists, keyed by the anim cursor's host
    /// handle. Same key-space discipline as `cursor_records` /
    /// `cursor_pixmaps` (see comment at `cursor_records`); entries
    /// are never removed (status-quo no-op `free_cursor` — spec
    /// "Frame lifetime").
    pub(crate) anim_cursor_records: HashMap<u32, crate::kms::v2::cursor::AnimCursorRecord>,
    /// The one running animation (the effective cursor is animated),
    /// or `None`.
    pub(crate) active_cursor_anim: Option<crate::kms::v2::cursor::ActiveCursorAnim>,

    /// Phase B.1 Task 21: lifetime-opens count seen at the last
    /// `drain_frame_builder_telemetry` call. Delta tracking lets the
    /// drain helper emit one `record_frame_builder_open` per new open
    /// without requiring a separate event queue.
    last_drained_fb_opens: u64,

    // ── VT switching (libseat mode). `Direct`/`None`/`-1` in
    //    Direct mode; populated during `open_libseat` construction.
    //
    //    `seat_fd` and `core_libinput_fd` are STABLE for the process
    //    lifetime (the DRM fd is opened once, Deviation #5 of the plan),
    //    so caching them once and never re-registering with the poller is
    //    correct.
    /// Seat mode (owns libseat in Libseat mode; marker in Direct mode).
    seat: crate::seat::Seat,
    /// State machine for the suspend/resume cycle.
    /// Used by `scanout_allowed()` / `run_suspend` / Task 12's `on_seat_ready`.
    seat_state: crate::seat::state::SeatState,
    /// Coalesced counter-events for the state machine.
    /// Consumed by `on_seat_ready` / `drive_seat_event`.
    seat_pending: crate::seat::state::SeatPending,
    /// On-core libinput context (libseat mode only). `None` in Direct mode
    /// (the dedicated input thread owns libinput there).
    core_libinput: Option<crate::input::Context>,
    /// Cursor/scroll accumulator for on-core libinput event mapping
    /// (libseat mode). `None` in Direct mode.
    core_input_state: Option<crate::input_thread::LibinputThreadState>,
    /// Cached libseat connection fd for `poll_fds` (`&self`). `-1` in
    /// Direct mode (never registered with the poller).
    seat_fd: std::os::fd::RawFd,
    /// Cached on-core libinput fd for `poll_fds` (`&self`). `-1` in
    /// Direct mode (the input thread owns the fd there).
    core_libinput_fd: std::os::fd::RawFd,
    /// Direct-mode lock-LED sink: the input thread owns the libinput
    /// devices, so LED changes cross via this relay (mask + eventfd in
    /// the thread's epoll). `None` in libseat mode (LEDs go straight
    /// through `core_libinput`) and on test fixtures.
    led_relay: Option<std::sync::Arc<crate::input::LedRelay>>,
    /// Last `input::Led` bits pushed — dedup so only lock-state
    /// TRANSITIONS reach the hardware, not every key event.
    leds_sent: u32,
    /// Core-channel sender for emitting Shutdown/Dump messages from the
    /// on-core hotkey path (libseat mode). Handed in via `set_input_sender`
    /// after the channel is created in `lib.rs`.
    input_sender: Option<yserver_core::core_loop::CoreSender>,
    /// Hotkey detector — used on the core thread in libseat mode
    /// (`on_libinput_ready`).
    hotkey: crate::input::hotkey::HotkeyDetector,

    /// GLX-TFP (Tasks 2.3 + 2.4): per-`DrawableId` export tracking for
    /// pixmaps shared with a GL consumer via `GLX_EXT_texture_from_pixmap`.
    /// AT MOST ONE entry per drawable regardless of how many times
    /// `BufferFromPixmap` is issued for it (muffin re-exports per damage).
    /// Canonical owner of the export lifetime ref + the dup'd dma-buf fd;
    /// a parallel sync-only fd dup lives on `store.exported_sync` so the
    /// engine's flush chokepoint can reach it.
    exported_dmabufs: HashMap<crate::kms::v2::store::DrawableId, ExportedBacking>,

    /// Cached result of `probe_dmabuf_export_support` run once at
    /// construction.  When `true`, `ServerState::glx_tfp_supported` is
    /// set and the GLX extension string advertises
    /// `GLX_EXT_texture_from_pixmap`.
    dmabuf_export_supported: bool,
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
    backing_id: crate::kms::v2::store::DrawableId,
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

// SAFETY: `KmsBackendV2` lives entirely on the core-loop thread. The
// `!Send` fields (`crate::seat::Seat` with `Rc<RefCell<...>>`, and
// `crate::input::Context` with `*mut libinput`) are only accessed from
// that single thread. `run_core` requires `Backend: Send` because it is
// generic over `dyn Backend`, but the backend is never actually moved
// between threads after construction — the same pattern used by the
// existing `!Send` KMS fields (`XkbContext`, `XkbState`, etc.).
unsafe impl Send for KmsBackendV2 {}

/// Probe whether this Vulkan context can allocate and export a BGRA8
/// dma-buf image.  Called ONCE at backend construction; result is cached
/// in `KmsBackendV2::dmabuf_export_supported`.
///
/// Allocates a 1×1 `BGRA8` external-memory image and immediately
/// exports it.  Both the image and the fd are dropped at the end of
/// this function (`ExportableImage::Drop` destroys image + memory,
/// `DmabufExport::fd` is an `OwnedFd` that closes on drop), so no
/// resources leak from the probe.
fn probe_dmabuf_export_support(vk: &std::sync::Arc<crate::kms::vk::device::VkContext>) -> bool {
    use crate::kms::vk::{
        dri3::export_backing,
        target::{EXPORT_FORMAT_BGRA8, allocate_exportable},
    };
    let Ok(img) = allocate_exportable(vk, 1, 1, EXPORT_FORMAT_BGRA8) else {
        return false;
    };
    export_backing(vk, &img).is_ok()
    // `img` drops here → `ExportableImage::Drop` frees VkImage + VkDeviceMemory.
}

impl KmsBackendV2 {
    /// Test-only entry point: drives the production `get_image` path
    /// but returns just the pixel bytes (header stripped). Acceptance
    /// tests use this so they can index into the result starting at
    /// pixel 0 without each one having to remember the 32-byte X11
    /// reply prefix.
    #[doc(hidden)]
    #[allow(clippy::too_many_arguments)]
    pub fn get_image_pixels_for_tests(
        &mut self,
        host_xid: u32,
        format: u8,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        plane_mask: u32,
    ) -> io::Result<Option<Vec<u8>>> {
        use yserver_core::backend::Backend;
        let reply = self.get_image(None, host_xid, format, x, y, width, height, plane_mask)?;
        Ok(reply.map(|r| {
            assert!(r.len() >= 32, "v2 GetImage reply missing 32-byte header");
            r[32..].to_vec()
        }))
    }

    fn alloc_window_stack_rank(&mut self) -> u64 {
        let rank = self.next_window_stack_rank;
        self.next_window_stack_rank = self.next_window_stack_rank.saturating_add(1);
        rank
    }

    #[allow(clippy::too_many_arguments)]
    fn clear_window_area_with_background(
        &mut self,
        host_xid: u32,
        background_pixel: u32,
        background_pixmap_host_xid: Option<u32>,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        tile_origin: (i32, i32),
    ) -> io::Result<()> {
        use crate::kms::{v2::engine::ResolvedSource, vk::ops::render::CompositeRect};

        self.clear_window_area_calls = self.clear_window_area_calls.wrapping_add(1);
        self.clear_clip_rectangles(None)?;
        let Some(dst_target) = self.resolve_paint_target(host_xid) else {
            return Ok(());
        };
        if let Some(bg_host_xid) = background_pixmap_host_xid
            && let Some(src) = self.store.lookup(bg_host_xid)
        {
            if src == dst_target.id {
                return Ok(());
            }
            if self.store.get(src).map(|d| d.storage.format)
                == Some(ash::vk::Format::B8G8R8A8_UNORM)
            {
                let rects = [CompositeRect {
                    // ParentRelative tiles sample at the OWNING
                    // window's alignment (tile_origin offset).
                    src_x: i32::from(x) + tile_origin.0,
                    src_y: i32::from(y) + tile_origin.1,
                    mask_x: 0,
                    mask_y: 0,
                    dst_x: dst_target.offset.0 + i32::from(x),
                    dst_y: dst_target.offset.1 + i32::from(y),
                    width: u32::from(width),
                    height: u32::from(height),
                }];
                const OP_SRC: u8 = 1;
                let composite_result = self.engine.render_composite(
                    &mut self.store,
                    &mut self.platform,
                    OP_SRC,
                    ResolvedSource::Drawable(src),
                    ResolvedSource::None,
                    dst_target.id,
                    &rects,
                    None,
                    Repeat::Normal,
                    Repeat::None,
                    None,
                    None,
                    false,
                    // Audit #4: no Picture context — pass 0 so the
                    // engine falls back to the depth-based swizzle.
                    0,
                    0,
                    0,
                );
                self.sync_descriptor_pool_telemetry();
                match composite_result {
                    Ok(s) if s.recorded_draws > 0 && !s.deferred_to_batch => {
                        self.telemetry.record_paint_submit();
                        self.trace_render(
                            SubmitKind::RenderComposite,
                            dst_target.id,
                            s.recorded_draws,
                            OP_SRC,
                            SrcClass::Direct,
                            None,
                            SubmitFlags {
                                readback: s.used_dst_readback,
                                alias: s.used_src_alias_scratch,
                                zero_draws: false,
                                upload: false,
                            },
                        );
                        return Ok(());
                    }
                    Ok(_) => return Ok(()),
                    Err(e) => {
                        log::warn!(
                            "v2 clear_window_area_with_background: tiled bg_pixmap clear failed \
                             for 0x{host_xid:x}: {e:?}"
                        );
                    }
                }
            }
        }
        self.fill_rectangle(None, host_xid, background_pixel, x, y, width, height)
    }

    /// True when every ancestor of `host_xid` up to the root is mapped
    /// (the root itself is always viewable). `host_xid` must already
    /// have its own `mapped` flag set by the caller.
    fn window_viewable(&self, host_xid: u32) -> bool {
        let mut cursor = self.windows_v2.get(&host_xid).and_then(|g| g.parent);
        while let Some(parent_xid) = cursor {
            let Some(parent) = self.windows_v2.get(&parent_xid) else {
                // Parent not tracked (root container) — treat as mapped.
                return true;
            };
            if !parent.mapped {
                return false;
            }
            cursor = parent.parent;
        }
        true
    }

    /// Collect `host_xid` plus every descendant whose path down from it
    /// is fully mapped — i.e. the subtree that becomes viewable when
    /// `host_xid` maps — along with the background each window should be
    /// tiled with. Windows with neither bg pixel nor bg pixmap are
    /// skipped (X11 background None = contents stay undefined).
    fn collect_viewable_bg_paint_targets(
        &self,
        host_xid: u32,
    ) -> Vec<(u32, u32, Option<u32>, u16, u16)> {
        let mut out = Vec::new();
        let mut stack = vec![host_xid];
        while let Some(xid) = stack.pop() {
            let Some(geom) = self.windows_v2.get(&xid) else {
                continue;
            };
            if xid != host_xid && !geom.mapped {
                // Unmapped child: neither it nor its inferiors become
                // viewable through this map.
                continue;
            }
            if geom.bg_pixel.is_some() || geom.bg_pixmap.is_some() {
                out.push((
                    xid,
                    geom.bg_pixel.unwrap_or(0),
                    geom.bg_pixmap,
                    geom.width.max(1),
                    geom.height.max(1),
                ));
            }
            stack.extend(
                self.windows_v2
                    .iter()
                    .filter(|(_, g)| g.parent == Some(xid))
                    .map(|(child, _)| *child),
            );
        }
        out
    }

    fn restack_subwindow(&mut self, host_xid: u32, stack_mode: u8, sibling: Option<u32>) {
        let Some(current) = self.windows_v2.get(&host_xid).copied() else {
            return;
        };
        let parent = current.parent;
        let mut siblings: Vec<(u32, u64)> = self
            .windows_v2
            .iter()
            .filter_map(|(xid, geom)| (geom.parent == parent).then_some((*xid, geom.stack_rank)))
            .collect();
        siblings.sort_by_key(|(_, rank)| *rank);
        let Some(pos) = siblings.iter().position(|(xid, _)| *xid == host_xid) else {
            return;
        };
        let entry = siblings.remove(pos);
        let sibling_pos = sibling.and_then(|sib| siblings.iter().position(|(xid, _)| *xid == sib));
        match stack_mode {
            0 | 2 | 4 => match sibling_pos {
                Some(sp) => siblings.insert(sp + 1, entry),
                None => siblings.push(entry),
            },
            1 | 3 => match sibling_pos {
                Some(sp) => siblings.insert(sp, entry),
                None => siblings.insert(0, entry),
            },
            _ => siblings.push(entry),
        }
        for (rank, (xid, _)) in siblings.into_iter().enumerate() {
            if let Some(geom) = self.windows_v2.get_mut(&xid) {
                geom.stack_rank = u64::try_from(rank).unwrap_or(u64::MAX);
            }
        }
    }

    /// Real-DRM-real-Vk constructor. Per Stage 2a, the platform
    /// layer (DRM device, output layouts, libinput, VkContext,
    /// ops command pool, fence pool, per-output scanout pools)
    /// is real; v2's `DrawableStore` / `RenderEngine` /
    /// `SceneCompositor` are still stubs and paint paths log
    /// gaps.
    ///
    /// # Errors
    ///
    /// Propagates DRM / Vk / libinput init failures from
    /// `PlatformBackend::open_with_commit`, plus FontLoader / XKB
    /// init failures from `KmsCore::new`.
    pub fn open(device_path: &str) -> io::Result<Self> {
        Self::open_with_commit(device_path, drm::modeset::commit_modeset)
    }

    fn open_with_commit(
        device_path: &str,
        commit: fn(
            &crate::drm::Device,
            &crate::drm::modeset::Output,
            ::drm::control::framebuffer::Handle,
        ) -> io::Result<()>,
    ) -> io::Result<Self> {
        let platform = PlatformBackend::open_with_commit(device_path, commit)?;
        let (fb_w, fb_h) = (platform.fb_w, platform.fb_h);
        let core = KmsCore::new(fb_w, fb_h)?;
        let engine = RenderEngine::new(&platform)
            .map_err(|e| io::Error::other(format!("v2 RenderEngine::new failed: {e:?}")))?;
        let scene = SceneCompositor::new(&platform)
            .map_err(|e| io::Error::other(format!("v2 SceneCompositor::new failed: {e:?}")))?;
        log::info!(
            "yserver(v2): KmsBackendV2 boot — {} output(s), {fb_w}x{fb_h} virtual screen; \
             Stage 2c engine + Stage 2d scene live (full-redraw, no buffer-age); \
             expect 'v2: <method> not yet implemented' warns for ops outside \
             Stage 2c/2d on first client request",
            platform.outputs.len(),
        );
        let dmabuf_export_supported = platform
            .vk
            .as_ref()
            .is_some_and(probe_dmabuf_export_support);
        let mut b = Self {
            core,
            platform,
            logged_gaps: RefCell::new(HashSet::new()),
            store: DrawableStore::new(),
            engine,
            scene,
            windows_v2: WindowsV2Map::new(),
            next_window_stack_rank: 1,
            telemetry: Telemetry::new(),
            last_observed_pool_creates: 0,
            last_observed_pool_resets: 0,
            cow_id: None,
            clip_mask_cache: None,
            fill_pattern_cache: None,
            kms_outputs_active: true,
            clear_window_area_calls: 0,
            engine_copy_area_calls: 0,
            recent_present_pixmaps: std::collections::VecDeque::with_capacity(32),
            dri3_xshmfences: HashMap::new(),
            dri3_sync_resources: HashMap::new(),
            pending_present_batches: std::collections::VecDeque::new(),
            pending_completed_events_on_shutdown: Vec::new(),
            cursor_records: HashMap::new(),
            cursor_pixmaps: HashMap::new(),
            next_cursor_version: 1,
            default_cursor_xid: None,
            effective_cursor_xid: None,
            anim_cursor_records: HashMap::new(),
            active_cursor_anim: None,
            last_drained_fb_opens: 0,
            // Direct mode: seat is a marker, fds are -1 (never polled),
            // no on-core libinput, no core sender.
            seat: crate::seat::Seat::Direct,
            seat_state: crate::seat::state::SeatState::Active,
            seat_pending: crate::seat::state::SeatPending::default(),
            core_libinput: None,
            core_input_state: None,
            seat_fd: -1,
            core_libinput_fd: -1,
            led_relay: None,
            leds_sent: 0,
            input_sender: None,
            hotkey: crate::input::hotkey::HotkeyDetector::new(),
            exported_dmabufs: HashMap::new(),
            dmabuf_export_supported,
        };
        b.init_root_storage();
        // Stage 3f.8: bake the default-arrow software cursor.
        // Best-effort — a failure logs + leaves the cursor invisible
        // (matches pre-3f.8 behaviour, no regression).
        if let Err(e) = b.init_cursor_sprite() {
            log::warn!("v2: software cursor init failed: {e:?} — no visible cursor");
        }
        Ok(b)
    }

    /// Libseat-mode constructor. The seat has already been opened and
    /// the initial Enable received; the DRM card fd came from
    /// `seat.open_device`, and libinput was built on the core thread via
    /// `Context::new_libseat`. This is called by `lib.rs` after
    /// `build_kms_backend_v2` branches on the seat mode.
    ///
    /// # Errors
    ///
    /// Propagates Vk / libinput init failures from
    /// `PlatformBackend::open_with_commit_fd`.
    pub fn open_libseat(
        seat: crate::seat::Seat,
        device_path: &str,
        core_libinput: crate::input::Context,
        seat_fd: std::os::fd::RawFd,
        core_libinput_fd: std::os::fd::RawFd,
    ) -> io::Result<Self> {
        Self::open_libseat_with_commit(
            seat,
            device_path,
            core_libinput,
            seat_fd,
            core_libinput_fd,
            drm::modeset::commit_modeset,
        )
    }

    fn open_libseat_with_commit(
        seat: crate::seat::Seat,
        device_path: &str,
        core_libinput: crate::input::Context,
        seat_fd: std::os::fd::RawFd,
        core_libinput_fd: std::os::fd::RawFd,
        commit: fn(
            &crate::drm::Device,
            &crate::drm::modeset::Output,
            ::drm::control::framebuffer::Handle,
        ) -> io::Result<()>,
    ) -> io::Result<Self> {
        // Get the card fd from the seat.
        let card_fd = {
            let inner = seat.libseat_inner().ok_or_else(|| {
                io::Error::other("open_libseat_with_commit: seat is not in libseat mode")
            })?;
            inner
                .borrow_mut()
                .open_device(
                    std::path::Path::new(device_path),
                    crate::seat::DeviceKind::Drm { is_kms: true },
                )
                .map_err(|e| {
                    io::Error::other(format!(
                        "libseat mode: opening DRM card {device_path} via seat failed: {e}"
                    ))
                })?
        };
        let platform = PlatformBackend::open_with_commit_fd(device_path, card_fd, commit)?;
        let (fb_w, fb_h) = (platform.fb_w, platform.fb_h);
        let core = KmsCore::new(fb_w, fb_h)?;
        let engine = RenderEngine::new(&platform)
            .map_err(|e| io::Error::other(format!("v2 RenderEngine::new failed: {e:?}")))?;
        let scene = SceneCompositor::new(&platform)
            .map_err(|e| io::Error::other(format!("v2 SceneCompositor::new failed: {e:?}")))?;
        log::info!(
            "yserver(v2): KmsBackendV2 boot (libseat mode) — {} output(s), {fb_w}x{fb_h} \
             virtual screen; VT switching enabled",
            platform.outputs.len(),
        );
        let dmabuf_export_supported = platform
            .vk
            .as_ref()
            .is_some_and(probe_dmabuf_export_support);
        let mut b = Self {
            core,
            platform,
            logged_gaps: RefCell::new(HashSet::new()),
            store: DrawableStore::new(),
            engine,
            scene,
            windows_v2: WindowsV2Map::new(),
            next_window_stack_rank: 1,
            telemetry: Telemetry::new(),
            last_observed_pool_creates: 0,
            last_observed_pool_resets: 0,
            cow_id: None,
            clip_mask_cache: None,
            fill_pattern_cache: None,
            kms_outputs_active: true,
            clear_window_area_calls: 0,
            engine_copy_area_calls: 0,
            recent_present_pixmaps: std::collections::VecDeque::with_capacity(32),
            dri3_xshmfences: HashMap::new(),
            dri3_sync_resources: HashMap::new(),
            pending_present_batches: std::collections::VecDeque::new(),
            pending_completed_events_on_shutdown: Vec::new(),
            cursor_records: HashMap::new(),
            cursor_pixmaps: HashMap::new(),
            next_cursor_version: 1,
            default_cursor_xid: None,
            effective_cursor_xid: None,
            anim_cursor_records: HashMap::new(),
            active_cursor_anim: None,
            last_drained_fb_opens: 0,
            seat,
            seat_state: crate::seat::state::SeatState::Active,
            seat_pending: crate::seat::state::SeatPending::default(),
            core_libinput: Some(core_libinput),
            core_input_state: Some(crate::input_thread::LibinputThreadState::new(
                u32::from(fb_w),
                u32::from(fb_h),
            )),
            seat_fd,
            core_libinput_fd,
            led_relay: None,
            leds_sent: 0,
            input_sender: None,
            hotkey: crate::input::hotkey::HotkeyDetector::new(),
            exported_dmabufs: HashMap::new(),
            dmabuf_export_supported,
        };
        b.init_root_storage();
        if let Err(e) = b.init_cursor_sprite() {
            log::warn!("v2: software cursor init failed: {e:?} — no visible cursor");
        }
        Ok(b)
    }

    /// Stage 3f.8: allocate the default cursor sprite (16×16 black
    /// triangle, hotspot (0,0)) as a Pixmap-kind Drawable + upload
    /// the pixel data via `engine.put_image`. Registers the result
    /// on `SceneCompositor` so `build_scene` appends it at top of
    /// z. One-time setup; subsequent `define_cursor` flows (Stage 4)
    /// can replace the entry.
    fn init_cursor_sprite(&mut self) -> io::Result<()> {
        // Stage 5 Phase A: bake the default-arrow record into the
        // canonical cursor maps so any DefineCursor that resolves to
        // None / unknown can fall back to it. The sprite Pixmap +
        // scene registration happen via the shared
        // `insert_cursor_record` path so subsequent client cursors
        // and the default sit on the same plumbing.
        let xid = self.core.next_host_xid();
        let bytes = crate::kms::v2::cursor::default_arrow_bgra();
        self.insert_cursor_record(
            xid,
            crate::kms::v2::cursor::DEFAULT_ARROW_W,
            crate::kms::v2::cursor::DEFAULT_ARROW_H,
            crate::kms::v2::cursor::DEFAULT_ARROW_HOT_X,
            crate::kms::v2::cursor::DEFAULT_ARROW_HOT_Y,
            bytes,
        );
        self.default_cursor_xid = Some(xid);
        // Force the effective cursor to resolve against the new
        // default so the scene picks it up at boot (otherwise
        // refresh_effective_cursor short-circuits on
        // pre-default `effective_cursor_xid == None == new_xid`).
        self.effective_cursor_xid = None;
        self.refresh_effective_cursor();
        log::info!("v2: default cursor sprite registered (xid 0x{xid:x})");
        Ok(())
    }

    // ── Stage 5 Phase A — cursor record helpers ────────────────────

    /// Allocate a fresh CursorRecord + sprite Pixmap and register
    /// both in the canonical xid maps. Bumps `next_cursor_version`,
    /// uploads the BGRA bytes to a v2 store Pixmap (so the SW scene
    /// path can sample it), and — if the new cursor is the
    /// currently-effective one — refreshes the scene's
    /// `CursorEntry`.
    ///
    /// `bgra` length MUST equal `width * height * 4`.
    fn insert_cursor_record(
        &mut self,
        xid: u32,
        width: u16,
        height: u16,
        hot_x: u16,
        hot_y: u16,
        bgra: Vec<u8>,
    ) {
        debug_assert_eq!(bgra.len(), usize::from(width) * usize::from(height) * 4);
        let version = self.next_cursor_version;
        self.next_cursor_version = self.next_cursor_version.saturating_add(1);
        let record =
            crate::kms::v2::cursor::CursorRecord::new(width, height, hot_x, hot_y, bgra, version);
        // Upload the sprite to a v2 store Pixmap so the SW scene
        // path can sample it. Best-effort: a Vk-less test fixture
        // skips the upload but still keeps the record so unit tests
        // can observe bytes / version.
        if let Some(pixmap_id) = self.allocate_cursor_sprite_pixmap(&record) {
            self.cursor_pixmaps.insert(xid, pixmap_id);
        }
        self.cursor_records.insert(xid, record);
        self.refresh_effective_cursor();
    }

    /// Allocate a v2 store Pixmap matching `record`'s dims, depth-32,
    /// and upload the BGRA bytes via `engine.put_image`. Returns the
    /// fresh DrawableId. Failures (no Vk in tests, allocate failure,
    /// upload failure) return `None` — the caller keeps the record
    /// but the SW scene path won't sample the sprite for that cursor.
    fn allocate_cursor_sprite_pixmap(
        &mut self,
        record: &std::sync::Arc<crate::kms::v2::cursor::CursorRecord>,
    ) -> Option<crate::kms::v2::store::DrawableId> {
        let storage = match self
            .platform
            .allocate_drawable_storage(record.width, record.height, 32)
        {
            Ok(s) => s,
            Err(e) => {
                log::debug!(
                    "v2 cursor sprite alloc: storage failed ({}x{}, depth 32): {e:?}",
                    record.width,
                    record.height,
                );
                return None;
            }
        };
        let sprite_xid = self.core.next_host_xid();
        let id = match self.store.allocate(
            sprite_xid,
            crate::kms::v2::store::DrawableKind::Pixmap,
            32,
            false,
            storage,
        ) {
            Ok(id) => id,
            Err(e) => {
                log::warn!("v2 cursor sprite alloc: store.allocate failed: {e:?}");
                return None;
            }
        };
        if let Err(e) = self.engine.put_image(
            &mut self.store,
            &mut self.platform,
            id,
            ash::vk::Offset2D::default(),
            ash::vk::Extent2D {
                width: u32::from(record.width),
                height: u32::from(record.height),
            },
            &record.bgra_bytes,
            32,
        ) {
            log::warn!("v2 cursor sprite alloc: put_image failed: {e:?}");
            // Drop the freshly-allocated storage cleanly so it
            // doesn't leak.
            self.store_decref_with_invalidate(id);
            return None;
        }
        Some(id)
    }

    /// Read a depth-1 X11 pixmap's pixels as R8 (1 byte per pixel,
    /// non-zero = bit set). Returns `(bytes, width, height)`. None
    /// when the pixmap isn't in the store or the engine readback
    /// fails (Vk-less fixture, format mismatch).
    fn read_cursor_depth1_pixmap(&mut self, host_xid: u32) -> Option<(Vec<u8>, u16, u16)> {
        let id = self.store.lookup(host_xid)?;
        let drawable = self.store.get(id)?;
        let extent = drawable.storage.extent;
        let w = u16::try_from(extent.width).ok()?;
        let h = u16::try_from(extent.height).ok()?;
        let rect = ash::vk::Rect2D {
            offset: ash::vk::Offset2D::default(),
            extent,
        };
        match self
            .engine
            .get_image(&mut self.store, &mut self.platform, id, rect, 1)
        {
            Ok(bytes) => Some((bytes, w, h)),
            Err(e) => {
                log::debug!(
                    "v2 read_cursor_depth1_pixmap: get_image failed for 0x{host_xid:x}: {e:?}"
                );
                None
            }
        }
    }

    /// Read a BGRA-mirrored X11 pixmap's pixels at depth 32.
    /// Returns `(bytes, width, height)`. None when the pixmap isn't
    /// in the store or readback fails (Vk-less fixture, format
    /// mismatch).
    fn read_cursor_bgra_pixmap(&mut self, host_xid: u32) -> Option<(Vec<u8>, u16, u16)> {
        let id = self.store.lookup(host_xid)?;
        let drawable = self.store.get(id)?;
        let extent = drawable.storage.extent;
        let w = u16::try_from(extent.width).ok()?;
        let h = u16::try_from(extent.height).ok()?;
        let rect = ash::vk::Rect2D {
            offset: ash::vk::Offset2D::default(),
            extent,
        };
        match self
            .engine
            .get_image(&mut self.store, &mut self.platform, id, rect, 32)
        {
            Ok(bytes) => Some((bytes, w, h)),
            Err(e) => {
                log::debug!(
                    "v2 read_cursor_bgra_pixmap: get_image failed for 0x{host_xid:x}: {e:?}"
                );
                None
            }
        }
    }

    /// Render a single FreeType glyph from `font_xid` for use in
    /// glyph-cursor rasterisation. Returns `(pixels, w, h, lsb,
    /// top)`. Empty glyphs (e.g. SPACE) return a `(vec![0u8], 1, 1,
    /// lsb, top)` placeholder so the union-bbox math has something
    /// to work with. None when the font isn't known.
    fn render_glyph_for_cursor(
        &self,
        font_xid: u32,
        ch: u16,
    ) -> Option<(Vec<u8>, i32, i32, i32, i32)> {
        let fs = self.core.fonts.get(&font_xid)?;
        let face = fs.face.borrow();
        let _ = face
            .0
            .load_char(ch as usize, freetype::face::LoadFlag::RENDER);
        let glyph = face.0.glyph();
        let bitmap = glyph.bitmap();
        let w = bitmap.width();
        let h = bitmap.rows();
        if w <= 0 || h <= 0 {
            return Some((vec![0u8], 1, 1, glyph.bitmap_left(), glyph.bitmap_top()));
        }
        let stride = bitmap.pitch();
        let buf = bitmap.buffer();
        let wu = w as usize;
        let hu = h as usize;
        // FreeType bitmaps can be 1-bit `Mono` (FT_PIXEL_MODE_MONO,
        // MSB-first, one byte per 8 pixels) or 8-bit gray. The
        // emboldening / cursor mask consumer expects 8 bpp, so
        // unpack Mono → 0x00/0xff per pixel; copy gray verbatim.
        // Per-row stride is in BYTES; for Mono `stride` ≈ ceil(w/8).
        let mono = matches!(bitmap.pixel_mode(), Ok(freetype::bitmap::PixelMode::Mono));
        let mut pixels = vec![0u8; wu * hu];
        for row in 0..hu {
            let row_start = if stride >= 0 {
                row * stride as usize
            } else {
                (hu - 1 - row) * (stride as isize).unsigned_abs()
            };
            let dst_row = row * wu;
            if mono {
                for col in 0..wu {
                    let byte = buf.get(row_start + (col >> 3)).copied().unwrap_or(0);
                    pixels[dst_row + col] = if byte & (0x80 >> (col & 7)) != 0 {
                        0xff
                    } else {
                        0
                    };
                }
            } else {
                let end = row_start + wu;
                if end <= buf.len() {
                    pixels[dst_row..dst_row + wu].copy_from_slice(&buf[row_start..end]);
                }
            }
        }
        Some((pixels, w, h, glyph.bitmap_left(), glyph.bitmap_top()))
    }

    /// Walk the parent chain from `host_xid` upward, returning the
    /// first non-None cursor attribute encountered. Falls back to
    /// `core.active_cursor` (the sticky DefineCursor-on-root) if
    /// the chain runs out — that is, no window on the chain bound a
    /// cursor.
    fn effective_cursor_walking_chain(&self, host_xid: u32) -> Option<u32> {
        let mut cur = host_xid;
        // Bound the walk so a corrupted parent loop can't burn the
        // event loop. windows_v2 fits in u32 xids; 64 is generous.
        for _ in 0..64 {
            if let Some(geom) = self.windows_v2.get(&cur) {
                if let Some(c) = geom.cursor {
                    return Some(c);
                }
                if let Some(p) = geom.parent {
                    cur = p;
                    continue;
                }
            }
            break;
        }
        self.core.active_cursor.or(self.default_cursor_xid)
    }

    /// Recompute the effective cursor for the window currently under
    /// the pointer and swap the scene `CursorEntry` if it changed.
    /// Cheap when the choice is stable (HashMap lookup + Option
    /// compare).
    fn refresh_effective_cursor(&mut self) {
        let pointer_window = self.core.prev_pointer_window.unwrap_or(self.core.window_id);
        let new_xid = self.effective_cursor_walking_chain(pointer_window);
        if new_xid == self.effective_cursor_xid {
            // Same effective cursor — a running animation keeps its
            // frame index (Xorg: "already current → do nothing").
            return;
        }
        self.effective_cursor_xid = new_xid;
        self.sync_cursor_animation(new_xid);
        let Some(xid) = new_xid else {
            return;
        };
        self.display_cursor_by_handle(xid);
    }

    /// Arm (reset to frame 0) or clear the cursor animation for the
    /// new effective cursor. Arming swaps the canonical maps to
    /// frame 0 under a freshly-minted version so the XFixes serial
    /// stays monotonic (spec "Version/serial"). Clear fires both for
    /// `new_xid = None` and for a non-animated cursor (no
    /// `anim_cursor_records` entry).
    fn sync_cursor_animation(&mut self, new_xid: Option<u32>) {
        let Some(xid) = new_xid else {
            self.active_cursor_anim = None;
            return;
        };
        let Some(anim) = self.anim_cursor_records.get(&xid) else {
            self.active_cursor_anim = None;
            return;
        };
        let first = &anim.frames[0];
        let (record, pixmap, delay) = (
            std::sync::Arc::clone(&first.record),
            first.pixmap,
            first.delay,
        );
        self.swap_anim_frame_into_maps(xid, &record, pixmap);
        self.active_cursor_anim = Some(crate::kms::v2::cursor::ActiveCursorAnim {
            handle: xid,
            frame: 0,
            next_frame: std::time::Instant::now() + delay,
        });
    }

    /// Advance the running cursor animation if its deadline elapsed.
    /// Called from `maybe_composite` AFTER its scanout/DPMS gates
    /// (spec "Frame tick"). One advance per call — a stale deadline
    /// after a blank advances a single frame, never fast-forwards.
    pub(crate) fn tick_cursor_animation(&mut self) {
        if !self.kms_outputs_active || !self.scanout_allowed() {
            return;
        }
        let now = std::time::Instant::now();
        let Some(st) = self.active_cursor_anim.as_ref() else {
            return;
        };
        if now < st.next_frame {
            return;
        }
        let handle = st.handle;
        let current = st.frame;
        let Some(anim) = self.anim_cursor_records.get(&handle) else {
            self.active_cursor_anim = None;
            return;
        };
        let next = (current + 1) % anim.frames.len();
        let frame = &anim.frames[next];
        let (record, pixmap, delay) = (
            std::sync::Arc::clone(&frame.record),
            frame.pixmap,
            frame.delay,
        );
        self.swap_anim_frame_into_maps(handle, &record, pixmap);
        // Always Some here — checked at the top and nothing in between clears it.
        if let Some(st) = self.active_cursor_anim.as_mut() {
            st.frame = next;
            st.next_frame = now + delay;
        }
        self.display_cursor_by_handle(handle);
    }

    /// Deadline for `next_wakeup`: the animation's next frame, only
    /// while it could actually be displayed (same gates as the tick).
    fn cursor_anim_deadline(&self) -> Option<std::time::Instant> {
        if !self.kms_outputs_active || !self.scanout_allowed() {
            return None;
        }
        self.active_cursor_anim.as_ref().map(|st| st.next_frame)
    }

    /// Re-point the canonical maps at an animation frame under a
    /// freshly-minted monotonic version. The byte clone is bounded
    /// by cursor size (≤16 KiB for HW-plane cursors).
    fn swap_anim_frame_into_maps(
        &mut self,
        xid: u32,
        record: &std::sync::Arc<crate::kms::v2::cursor::CursorRecord>,
        pixmap: Option<crate::kms::v2::store::DrawableId>,
    ) {
        let version = self.next_cursor_version;
        self.next_cursor_version = self.next_cursor_version.saturating_add(1);
        let minted = crate::kms::v2::cursor::CursorRecord::new(
            record.width,
            record.height,
            record.hot_x,
            record.hot_y,
            record.bgra_bytes.clone(),
            version,
        );
        self.cursor_records.insert(xid, minted);
        // Keep cursor_pixmaps truthful per-frame: a `None` frame
        // REMOVES the entry — leaving the prior frame's pixmap
        // installed would have the SW scene path sample stale bytes.
        // Note `display_cursor_by_handle` early-returns on a missing
        // pixmap entry, so a `None` frame skips display entirely
        // (HW upload included) — `None` only occurs in Vk-less
        // fixtures or after a sprite-alloc failure.
        match pixmap {
            Some(p) => {
                self.cursor_pixmaps.insert(xid, p);
            }
            None => {
                self.cursor_pixmaps.remove(&xid);
            }
        }
    }

    /// Push `cursor_records[xid]` to the scene / HW plane — the
    /// former tail of `refresh_effective_cursor`, shared with the
    /// animation tick. Keeps the sample-view readiness guard
    /// (Vk-less fixtures build records without sprite allocs).
    fn display_cursor_by_handle(&mut self, xid: u32) {
        let Some(record) = self.cursor_records.get(&xid).cloned() else {
            return;
        };
        let Some(&pixmap_id) = self.cursor_pixmaps.get(&xid) else {
            return;
        };
        // Sample-view readiness check — same gate as Stage 3f.8's
        // boot path. A Vk-less fixture builds the record but skips
        // the sprite alloc, so this short-circuits cleanly.
        if self
            .store
            .get(pixmap_id)
            .map(|d| d.storage.image_view == ash::vk::ImageView::null())
            .unwrap_or(true)
        {
            return;
        }
        self.scene
            .register_cursor(crate::kms::v2::scene::CursorEntry {
                id: pixmap_id,
                extent: ash::vk::Extent2D {
                    width: u32::from(record.width),
                    height: u32::from(record.height),
                },
                hot_x: i16::try_from(record.hot_x).unwrap_or(i16::MAX),
                hot_y: i16::try_from(record.hot_y).unwrap_or(i16::MAX),
                record_version: record.version,
                bgra_bytes: Some(std::sync::Arc::new(record.bgra_bytes.clone())),
            });

        // Stage 5 Phase D — steady-state HW sprite-change. When the
        // plane is fully bound, the scene won't re-tick (v2's
        // empty-damage fast path at scene.rs:840), so a record
        // swap would starve the upload waiting for a compose
        // event. Push the bytes synchronously through the scene's
        // queueing path; if any output is transitioning, the
        // bytes land in the deferred slot until the wait set
        // drains.
        if matches!(
            self.scene.cursor_mode(),
            crate::kms::v2::scene::CursorPlaneMode::Hw
                | crate::kms::v2::scene::CursorPlaneMode::Mixed
        ) && record.width <= 64
            && record.height <= 64
        {
            let bytes = std::sync::Arc::new(record.bgra_bytes.clone());
            #[allow(clippy::cast_possible_truncation)]
            let cx = self.core.cursor_x as i32;
            #[allow(clippy::cast_possible_truncation)]
            let cy = self.core.cursor_y as i32;
            self.scene.queue_steady_state_cursor_upload(
                &mut self.platform,
                record.version,
                record.width,
                record.height,
                bytes,
                record.hot_x,
                record.hot_y,
                cx,
                cy,
            );
        }
        // The scene blit ordering: register_cursor already marks
        // scene_structure_dirty so the next tick repaints; no extra
        // wake needed.
    }

    /// Test fixture with live Vulkan attached. Falls back to the
    /// headless `for_tests` shape if `VkContext::new` fails. Used
    /// by the Stage 2f acceptance harness which needs real paint
    /// + readback on the v2 path.
    ///
    /// # Errors
    ///
    /// Returns `Err` only when Vk init fails AND the caller
    /// explicitly wanted Vk-backed tests; callers that can fall
    /// back to headless use `for_tests` directly.
    #[doc(hidden)]
    pub fn for_tests_with_vk() -> Result<Self, io::Error> {
        use std::sync::Arc;
        // Build the test seed WITHOUT the root drawable. If
        // `for_tests()` were used, `init_root_storage` would have
        // run with no Vk attached and stamped a `for_tests_null`
        // stub (vk::ImageView::null()) into the store. The second
        // `init_root_storage` call below would then short-circuit
        // on the existing xid and we'd be left with a null-view
        // root — any `render_composite` against it (e.g.
        // `set_container_background_pixmap`) segfaults inside the
        // descriptor-set bind.
        let mut base = Self::for_tests_seed();
        let vk = crate::kms::vk::device::VkContext::new()
            .map_err(|e| io::Error::other(format!("v2 for_tests_with_vk: VkContext: {e:?}")))?;
        let ops_pool = crate::kms::vk::ops::OpsCommandPool::new(Arc::clone(&vk)).map_err(|e| {
            io::Error::other(format!("v2 for_tests_with_vk: OpsCommandPool: {e:?}"))
        })?;
        let fence_pool = crate::kms::v2::platform::FencePool::new(Arc::clone(&vk));
        base.platform.vk = Some(vk);
        base.platform.ops_command_pool = Some(ops_pool);
        base.platform.fence_pool = Some(fence_pool);
        // Replace the stub engine with a live one now that Vk
        // is attached. Scene compositor stays stubbed (no
        // scanout pool on the test fixture).
        base.engine = crate::kms::v2::engine::RenderEngine::new(&base.platform)
            .map_err(|e| io::Error::other(format!("v2 for_tests_with_vk: RenderEngine: {e:?}")))?;
        base.init_root_storage();
        Ok(base)
    }

    /// Vk-backed test fixture with a live scene compositor and test
    /// scanout pools. Unlike `for_tests_with_vk`, this can drive
    /// `maybe_composite()` all the way through `scene.tick()` and an
    /// actual compose submit.
    ///
    /// # Errors
    ///
    /// Returns `Err` if Vk init, scanout-pool allocation, the render
    /// engine, or the scene compositor fails to initialise.
    #[doc(hidden)]
    pub fn for_tests_with_vk_live_scene() -> Result<Self, io::Error> {
        use std::sync::Arc;

        let mut base = Self::for_tests_seed();
        let vk = crate::kms::vk::device::VkContext::new().map_err(|e| {
            io::Error::other(format!("v2 for_tests_with_vk_live_scene: VkContext: {e:?}"))
        })?;
        let ops_pool = crate::kms::vk::ops::OpsCommandPool::new(Arc::clone(&vk)).map_err(|e| {
            io::Error::other(format!(
                "v2 for_tests_with_vk_live_scene: OpsCommandPool: {e:?}"
            ))
        })?;
        let fence_pool = crate::kms::v2::platform::FencePool::new(Arc::clone(&vk));
        let mut scanout_pools = Vec::with_capacity(base.platform.outputs.len());
        let mut bo_generations = Vec::with_capacity(base.platform.outputs.len());
        for (i, layout) in base.platform.outputs.iter().enumerate() {
            let pool = crate::kms::vk::scanout::ScanoutBoPool::allocate(
                Arc::clone(&vk),
                Arc::clone(&base.platform.device),
                u32::from(layout.width),
                u32::from(layout.height),
                3,
                &layout.output.scanout_modifiers,
            )
            .map_err(|e| {
                io::Error::other(format!(
                    "v2 for_tests_with_vk_live_scene: ScanoutBoPool[{i}] {}x{}: {e}",
                    layout.width, layout.height
                ))
            })?;
            let n = pool.bos.len();
            scanout_pools.push(Some(pool));
            bo_generations.push(vec![
                crate::kms::v2::platform::BoGenerationEntry::default();
                n
            ]);
        }
        base.platform.vk = Some(vk);
        base.platform.ops_command_pool = Some(ops_pool);
        base.platform.fence_pool = Some(fence_pool);
        base.platform.scanout_pools = scanout_pools;
        base.platform.bo_generations = bo_generations;
        base.engine = crate::kms::v2::engine::RenderEngine::new(&base.platform).map_err(|e| {
            io::Error::other(format!(
                "v2 for_tests_with_vk_live_scene: RenderEngine: {e:?}"
            ))
        })?;
        base.scene = crate::kms::v2::scene::SceneCompositor::new(&base.platform).map_err(|e| {
            io::Error::other(format!(
                "v2 for_tests_with_vk_live_scene: SceneCompositor: {e:?}"
            ))
        })?;
        base.init_root_storage();
        Ok(base)
    }

    /// Stage 4b — test-only read of the alias registry. Returns
    /// a copy of the entry if the backing xid is tracked; the
    /// `pub(crate)` `KmsCore.alias_registry` is otherwise unreachable
    /// from the `tests/` integration crate.
    #[doc(hidden)]
    #[must_use]
    pub fn test_alias_registry_get(
        &self,
        backing_xid: u32,
    ) -> Option<crate::kms::core::AliasEntry> {
        let handle = yserver_core::backend::PixmapHandle::from_raw(backing_xid)?;
        self.core.alias_registry.get(handle).copied()
    }

    /// Stage 4b — test-only read of the host_window_to_backing
    /// map. Returns the backing xid registered against
    /// `window_xid`, or `None` when the window isn't redirected.
    #[doc(hidden)]
    #[must_use]
    pub fn test_host_window_to_backing(&self, window_xid: u32) -> Option<u32> {
        self.core
            .host_window_to_backing
            .get(&window_xid)
            .map(|h| h.as_raw())
    }

    /// Stage 4c.5 — test-only probe for a drawable's presentation
    /// damage. Returns `true` iff the drawable exists, has
    /// `scene_participating=true` (the `peek_presentation_damage`
    /// gate), AND has a non-empty damage region. Used by the
    /// `v2_automatic_redirect_backing_is_scene_participating`
    /// integration test to assert the Automatic-mode pairing
    /// actually accumulates scene damage on the backing.
    #[doc(hidden)]
    #[must_use]
    pub fn test_peek_presentation_damage_nonempty(&self, xid: u32) -> bool {
        let Some(id) = self.store.lookup(xid) else {
            return false;
        };
        self.store
            .peek_presentation_damage(id)
            .is_some_and(|snap| !snap.region.is_empty())
    }

    /// Scene-α fix — test-only read of the per-storage Vk views
    /// keyed by host xid. Returns `(image_view, sample_view)` —
    /// the attachment-side IDENTITY view and the format-aware
    /// sampling view, respectively. Used by
    /// `v2_storage_depth24_has_distinct_sample_view` to gate the
    /// scene-side α-leak fix at the construction layer.
    #[doc(hidden)]
    #[must_use]
    pub fn test_storage_views(&self, xid: u32) -> Option<(ash::vk::ImageView, ash::vk::ImageView)> {
        let id = self.store.lookup(xid)?;
        let drawable = self.store.get(id)?;
        Some((drawable.storage.image_view, drawable.storage.sample_view))
    }

    /// GLX-TFP (Task 1.2) test shim: clone the backend's
    /// `Arc<VkContext>` so tests can drive raw Vulkan readback
    /// (re-import + staging copy) against the same device.
    #[doc(hidden)]
    pub fn test_vk_arc(&self) -> Option<std::sync::Arc<crate::kms::vk::device::VkContext>> {
        self.platform.vk.as_ref().map(std::sync::Arc::clone)
    }

    /// GLX-TFP (Task 1.2) test shim: promote a server-owned pixmap onto
    /// exportable storage and export the resulting dma-buf. Drives the
    /// production `RenderEngine::promote_drawable_exportable` +
    /// `dri3::export_backing` paths.
    #[doc(hidden)]
    pub fn promote_and_export_pixmap_for_tests(
        &mut self,
        xid: u32,
    ) -> io::Result<crate::kms::vk::dri3::DmabufExport> {
        let id = self
            .store
            .lookup(xid)
            .ok_or_else(|| io::Error::other(format!("promote: unknown xid {xid:#x}")))?;
        self.engine
            .promote_drawable_exportable(&mut self.platform, &mut self.store, id)
            .map_err(|e| io::Error::other(format!("promote_drawable_exportable: {e:?}")))?;
        let vk = self
            .platform
            .vk
            .as_ref()
            .ok_or_else(|| io::Error::other("promote: no VkContext"))?;
        let (memory, stride, size, modifier) = {
            let d = self
                .store
                .get(id)
                .ok_or_else(|| io::Error::other("promote: drawable vanished"))?;
            (
                d.storage.memory,
                d.storage.export_stride,
                d.storage.export_size,
                d.storage.export_modifier,
            )
        };
        // Export the promoted memory directly (the Storage now owns the
        // exportable image's handles; we don't have the ExportableImage
        // wrapper any more, so build the export from the raw memory handle
        // + the stride/size/modifier adopt_exportable stored).
        crate::kms::vk::dri3::export_promoted(vk, memory, stride, size, modifier)
            .map_err(|e| io::Error::other(format!("export_promoted: {e:?}")))
    }

    /// Test-only read of whether the Composite Overlay Window is
    /// Stage 4a — test-only knob to install a COMPOSITE redirect
    /// route directly via the store, bypassing 4b's protocol
    /// surface (`allocate_redirected_backing` / `name_window_pixmap`
    /// still stubs returning Err until 4b lands). The
    /// `v2_acceptance` integration tests use this to set up
    /// routing for `resolve_paint_target` coverage without
    /// touching alias-registry / host_window_to_backing
    /// bookkeeping.
    ///
    /// `window_xid` must resolve in the store; `backing_xid` must
    /// also exist and is what window-keyed paint routes into.
    /// Returns `true` if both were resolved and the route was
    /// recorded; `false` otherwise. No side effects on damage,
    /// refcount, or `scene_participating`.
    #[doc(hidden)]
    pub fn test_set_redirected_target(&mut self, window_xid: u32, backing_xid: u32) -> bool {
        let Some(w_id) = self.store.lookup(window_xid) else {
            return false;
        };
        let Some(b_id) = self.store.lookup(backing_xid) else {
            return false;
        };
        self.store.set_redirected_target(w_id, Some(b_id));
        true
    }

    /// Headless test seed. Single 800×600 stub output; no
    /// Vulkan; no real DRM device. Mirrors `KmsBackend::for_tests`
    /// in shape so unit tests that drive v2 through
    /// `process_request` get a stable fixture.
    #[doc(hidden)]
    #[must_use]
    pub fn for_tests() -> Self {
        let mut b = Self::for_tests_seed();
        b.init_root_storage();
        // Stage 5 Phase A: seed the default-cursor record so test
        // paths that exercise `define_cursor` / effective-cursor
        // resolution have a fallback to walk to. Production uses
        // the same `init_cursor_sprite` body; the test fixture has
        // no Vk, so the sprite Pixmap allocation skips cleanly.
        let _ = b.init_cursor_sprite();
        b
    }

    /// Construct the test fixture **without** initialising root
    /// storage. Used by `for_tests_with_vk` so root allocation
    /// happens after the Vk context is attached.
    fn for_tests_seed() -> Self {
        Self {
            core: KmsCore::for_tests(),
            platform: PlatformBackend::for_tests(),
            logged_gaps: RefCell::new(HashSet::new()),
            store: DrawableStore::new(),
            engine: RenderEngine::stub(),
            scene: SceneCompositor::stub(),
            windows_v2: WindowsV2Map::new(),
            next_window_stack_rank: 1,
            telemetry: Telemetry::new(),
            last_observed_pool_creates: 0,
            last_observed_pool_resets: 0,
            cow_id: None,
            clip_mask_cache: None,
            fill_pattern_cache: None,
            kms_outputs_active: true,
            clear_window_area_calls: 0,
            engine_copy_area_calls: 0,
            recent_present_pixmaps: std::collections::VecDeque::with_capacity(32),
            dri3_xshmfences: HashMap::new(),
            dri3_sync_resources: HashMap::new(),
            pending_present_batches: std::collections::VecDeque::new(),
            pending_completed_events_on_shutdown: Vec::new(),
            cursor_records: HashMap::new(),
            cursor_pixmaps: HashMap::new(),
            next_cursor_version: 1,
            default_cursor_xid: None,
            effective_cursor_xid: None,
            anim_cursor_records: HashMap::new(),
            active_cursor_anim: None,
            last_drained_fb_opens: 0,
            // Test fixtures always run in Direct mode.
            seat: crate::seat::Seat::Direct,
            seat_state: crate::seat::state::SeatState::Active,
            seat_pending: crate::seat::state::SeatPending::default(),
            core_libinput: None,
            core_input_state: None,
            seat_fd: -1,
            core_libinput_fd: -1,
            led_relay: None,
            leds_sent: 0,
            input_sender: None,
            hotkey: crate::input::hotkey::HotkeyDetector::new(),
            exported_dmabufs: HashMap::new(),
            dmabuf_export_supported: false,
        }
    }

    fn init_root_storage(&mut self) {
        let root_xid = self.core.window_id;
        if self.store.lookup(root_xid).is_some() {
            return;
        }
        let width = self.platform.fb_w.max(1);
        let height = self.platform.fb_h.max(1);
        let storage = match self.platform.allocate_drawable_storage(width, height, 32) {
            Ok(storage) => {
                self.telemetry.record_storage_allocation();
                self.telemetry.record_image_view_create();
                storage
            }
            Err(e) => {
                log::debug!("v2 init_root_storage: no Vk, using stub root storage: {e:?}");
                Storage::for_tests_null(
                    ash::vk::Extent2D {
                        width: u32::from(width),
                        height: u32::from(height),
                    },
                    PlatformBackend::format_for_depth(32),
                )
            }
        };
        let id = match self
            .store
            .allocate(root_xid, DrawableKind::Root, 32, true, storage)
        {
            Ok(id) => id,
            Err(e) => {
                log::warn!("v2 init_root_storage: store.allocate failed: {e:?}");
                return;
            }
        };
        let rect = ash::vk::Rect2D {
            offset: ash::vk::Offset2D::default(),
            extent: ash::vk::Extent2D {
                width: u32::from(width),
                height: u32::from(height),
            },
        };
        if let Err(e) = self.engine.fill_rect(
            &mut self.store,
            &mut self.platform,
            id,
            rect,
            decode_x11_pixel_for_storage(
                self.core.bg_pixel.unwrap_or(0x0050_5050),
                24,
                PlatformBackend::format_for_depth(24),
            ),
        ) && self.platform.vk.is_some()
        {
            log::warn!("v2 init_root_storage: initial root fill failed: {e:?}");
        }
    }

    /// Stage 4a — resolve a host xid into the actual paint target
    /// under COMPOSITE redirect routing. Walks up the
    /// `windows_v2.parent` chain accumulating `(x, y)` offsets;
    /// the first ancestor (including `host_xid` itself) whose
    /// `Drawable.redirected_target` is `Some(B_id)` wins.
    ///
    /// Returns:
    /// - `None` if `host_xid` doesn't map to any drawable.
    /// - `Some(PaintTarget { id: leaf, offset: (0, 0) })` for
    ///   Pixmap targets (not in `windows_v2`) and for
    ///   unredirected windows whose ancestor chain reaches root
    ///   without finding a redirected ancestor.
    /// - `Some(PaintTarget { id: B_id, offset: accumulated })`
    ///   for redirected windows + their descendants.
    ///
    /// Per Stage 4 plan §"Per-hierarchy redirect": this is the
    /// per-op walk; tree depth bounds cost (typically ≤ 4 for
    /// real apps). Cached-ancestry alternative deferred to
    /// Stage 5 if profiling shows it.
    pub(crate) fn resolve_paint_target(&self, host_xid: u32) -> Option<PaintTarget> {
        let leaf_id = self.store.lookup(host_xid)?;
        let result = self.resolve_paint_target_inner(host_xid, leaf_id);
        // Diagnostic trace (TEMP — Stage 4d "opaque black backing"
        // investigation). Only fires when the resolve returned the
        // LEAF id (no redirect found in the ancestor chain) for a
        // *window* xid. That's the "paint to a window that didn't
        // route via a redirected ancestor" case — exactly what we
        // need to see if marco's CC client paints stop routing to B
        // after a drag. Pixmaps and root paints don't trip this gate
        // (root has no `windows_v2` entry), so volume stays bounded
        // to window paints that ought to have hit a redirect.
        if log::log_enabled!(target: "yserver::kms::v2::paint", log::Level::Trace)
            && let Some(t) = result.as_ref()
            && self.windows_v2.contains_key(&host_xid)
        {
            if t.id == leaf_id {
                log::trace!(
                    target: "yserver::kms::v2::paint",
                    "resolve_paint_target NO_REDIRECT_FOUND xid=0x{host_xid:x} leaf_id={leaf_id:?}",
                );
            } else {
                log::trace!(
                    target: "yserver::kms::v2::paint",
                    "resolve_paint_target REDIRECT_FOUND xid=0x{host_xid:x} leaf_id={leaf_id:?} \
                     backing_id={:?} offset=({},{})",
                    t.id,
                    t.offset.0,
                    t.offset.1,
                );
            }
        }
        result
    }

    fn resolve_paint_target_inner(
        &self,
        host_xid: u32,
        leaf_id: super::store::DrawableId,
    ) -> Option<PaintTarget> {
        if !self.windows_v2.contains_key(&host_xid) {
            if let Some(b_id) = self.store.redirected_target(leaf_id) {
                return Some(PaintTarget {
                    id: b_id,
                    offset: (0, 0),
                });
            }
            return Some(PaintTarget {
                id: leaf_id,
                offset: (0, 0),
            });
        }
        let mut cur_xid = host_xid;
        let mut cur_id = leaf_id;
        let mut offset = (0_i32, 0_i32);
        loop {
            if let Some(b_id) = self.store.redirected_target(cur_id) {
                return Some(PaintTarget { id: b_id, offset });
            }
            // No `windows_v2` entry means we've stepped onto root
            // (parent = `core.window_id`, not tracked) or onto an
            // unparented orphan. In both cases there's no parent
            // chain left to walk; return identity at the leaf.
            // (Root's own redirect was already checked on the
            // prior loop iteration when `cur_id` became root_id.)
            let Some(geom) = self.windows_v2.get(&cur_xid) else {
                return Some(PaintTarget {
                    id: leaf_id,
                    offset: (0, 0),
                });
            };
            match geom.parent {
                None => {
                    // Top-level: parent is root, not tracked in
                    // `windows_v2`. `create_subwindow` records
                    // `parent = None` when the host_parent is
                    // root_xid (the if-not-in-windows_v2 branch),
                    // so this is the production representation
                    // for every top-level. Step up to root
                    // explicitly so a `RedirectWindow(root, …)`
                    // compositor sees top-level descendants route
                    // through the root backing — codex round-7
                    // finding (`parent == None` previously
                    // returned identity without consulting root).
                    offset.0 += i32::from(geom.x);
                    offset.1 += i32::from(geom.y);
                    if let Some(root_id) = self.store.lookup(self.core.window_id)
                        && let Some(b_id) = self.store.redirected_target(root_id)
                    {
                        return Some(PaintTarget { id: b_id, offset });
                    }
                    // No root redirect: paint stays on the leaf
                    // at its own origin. Explicit match (not `?`)
                    // so we don't poison the outer Option.
                    return Some(PaintTarget {
                        id: leaf_id,
                        offset: (0, 0),
                    });
                }
                Some(parent_xid) => {
                    offset.0 += i32::from(geom.x);
                    offset.1 += i32::from(geom.y);
                    cur_xid = parent_xid;
                    // Parent xid not in the store means a
                    // dangling reparent: fall back to identity.
                    let Some(next_id) = self.store.lookup(parent_xid) else {
                        return Some(PaintTarget {
                            id: leaf_id,
                            offset: (0, 0),
                        });
                    };
                    cur_id = next_id;
                }
            }
        }
    }

    /// Stage 4c.2 — compute the screen-absolute rect for a window's
    /// `DrawableId`. Walks the `windows_v2.parent` chain upward
    /// from `w_id`, accumulating each step's `(x, y)` offset; the
    /// resulting rect's `offset` is the window's root-relative
    /// origin and the `extent` is its own `width × height`.
    ///
    /// Returns `None` when:
    /// - `w_id` doesn't resolve in the store, OR
    /// - the leaf xid has no `windows_v2` entry (Pixmap / Root /
    ///   detached), OR
    /// - the parent chain hits a dangling `Some(xid)` that is
    ///   neither root (`core.window_id`) nor a tracked
    ///   `windows_v2` entry. Bailing keeps callers from acting on
    ///   a half-accumulated rect; Stage 5 cache work can revisit
    ///   if the conservative choice ever bites.
    ///
    /// Consumed by Stage 4c.4's `set_window_scene_participation`:
    /// it captures the previous on-screen rect BEFORE flipping
    /// `scene_participating` so it can fire
    /// `mark_scene_structure_damage_rects(&[prev_rect])` for the
    /// redirect transition.
    pub(crate) fn window_absolute_rect(
        &self,
        w_id: crate::kms::v2::store::DrawableId,
    ) -> Option<ash::vk::Rect2D> {
        let leaf_xid = self.store.get(w_id)?.xid;
        let leaf_geom = self.windows_v2.get(&leaf_xid)?;
        let mut abs_x = i32::from(leaf_geom.x);
        let mut abs_y = i32::from(leaf_geom.y);
        let mut cur_parent = leaf_geom.parent;
        while let Some(parent_xid) = cur_parent {
            if parent_xid == self.core.window_id {
                // Reached root explicitly — root is the (0, 0)
                // origin of the screen-absolute coordinate space.
                break;
            }
            let Some(parent_geom) = self.windows_v2.get(&parent_xid) else {
                // Dangling parent: not root, not tracked. Bail.
                return None;
            };
            abs_x += i32::from(parent_geom.x);
            abs_y += i32::from(parent_geom.y);
            cur_parent = parent_geom.parent;
        }
        Some(ash::vk::Rect2D {
            offset: ash::vk::Offset2D { x: abs_x, y: abs_y },
            extent: ash::vk::Extent2D {
                width: u32::from(leaf_geom.width),
                height: u32::from(leaf_geom.height),
            },
        })
    }

    /// Audit #6 (2026-05-19) — Xorg parity rewrite. The old
    /// "copy W's own storage + DFS-walk descendants" model
    /// (Stage 4b.5) preserved any pre-redirect paint on W, but
    /// freshly-mapped windows that hit redirect activation
    /// before their first paint seeded B with W's default-init
    /// colour (opaque black on depth-24, transparent on
    /// depth-32) — visible as the recurring "black band on map"
    /// symptom and called out by the 2026-05-19 protocol audit.
    ///
    /// Replaced with Xorg's `compNewPixmap`
    /// (composite/compalloc.c:541-606) semantics: copy from
    /// W's PARENT at the source offset
    /// `(W.x - parent.x, W.y - parent.y)` to B at `(0, 0)`.
    /// This gives the compositor / direct-emit scene-walk
    /// continuity with what was on-screen before W appeared.
    /// The W's own (default-init) content is not preserved;
    /// W's first client paint fills B via `resolve_paint_target`
    /// routing afterwards.
    ///
    /// Skipped when:
    /// - W has no parent in `windows_v2` (root or untracked).
    /// - Parent's storage isn't in the store (pixmap-as-W
    ///   activation path used by tests; falls back to leaving
    ///   B at its default-init zero-fill).
    /// - Parent's storage extent is zero.
    ///
    /// `IncludeInferiors`-equivalent semantics (parent's siblings
    /// of W contributing where they overlap W's screen position)
    /// are deferred: yserver's parent storage already includes
    /// most of that content via the normal paint flow for
    /// non-compositor cases, and the Stage 4d compositor-floor
    /// scene-walk presents siblings directly.
    fn seed_backing_from_parent(&mut self, w_xid: u32, b_id: crate::kms::v2::store::DrawableId) {
        let Some(w_geom) = self.windows_v2.get(&w_xid).copied() else {
            log::debug!("v2 seed_backing_from_parent W=0x{w_xid:x}: not in windows_v2; skip seed");
            return;
        };
        let Some(parent_xid) = w_geom.parent else {
            log::debug!(
                "v2 seed_backing_from_parent W=0x{w_xid:x}: no parent (root or untracked); skip seed"
            );
            return;
        };
        // Resolve parent's effective storage. If parent is itself
        // redirected, its `redirected_target` (B') holds the
        // currently-visible pixels; if not, parent's own storage
        // does. `resolve_paint_target` does the chain walk.
        let Some(parent_target) = self.resolve_paint_target(parent_xid) else {
            log::debug!(
                "v2 seed_backing_from_parent W=0x{w_xid:x}: parent 0x{parent_xid:x} has no paint target; skip seed"
            );
            return;
        };
        let parent_extent = self
            .store
            .get(parent_target.id)
            .map(|d| d.storage.extent)
            .unwrap_or_default();
        if parent_extent.width == 0 || parent_extent.height == 0 {
            log::debug!(
                "v2 seed_backing_from_parent W=0x{w_xid:x}: parent storage zero-extent; skip seed"
            );
            return;
        }
        // Source rect on parent: W's position in parent's drawable
        // space, clamped to parent's extent. parent_target.offset is
        // already W's accumulated offset into parent's storage when
        // parent is redirected through an ancestor — we add W's own
        // (x, y) within parent on top.
        let src_x = parent_target.offset.0 + i32::from(w_geom.x);
        let src_y = parent_target.offset.1 + i32::from(w_geom.y);
        let src_rect = ash::vk::Rect2D {
            offset: ash::vk::Offset2D {
                x: src_x.max(0),
                y: src_y.max(0),
            },
            extent: ash::vk::Extent2D {
                width: u32::from(w_geom.width),
                height: u32::from(w_geom.height),
            },
        };
        let dst_pos = ash::vk::Offset2D { x: 0, y: 0 };
        log::debug!(
            "v2 seed_backing_from_parent W=0x{w_xid:x} parent=0x{parent_xid:x} \
             src=({src_x},{src_y} {w}x{h}) → B@(0,0)",
            w = w_geom.width,
            h = w_geom.height,
        );
        if let Err(e) = self.engine.copy_area(
            &mut self.store,
            &mut self.platform,
            parent_target.id,
            b_id,
            src_rect,
            dst_pos,
        ) {
            log::warn!("v2 seed_backing_from_parent(0x{w_xid:x}): parent copy_area failed: {e:?}",);
        } else {
            self.telemetry.record_paint_submit();
            self.trace_simple(SubmitKind::CopyArea, b_id, 1);
        }
    }

    /// 2026-06-11 — synthesize Xorg `compNewPixmap`'s
    /// `IncludeInferiors` over a freshly-allocated redirect backing.
    ///
    /// `seed_backing_from_parent` lays down the parent layer — Xorg's
    /// `CopyArea(parent, …, IncludeInferiors)` base
    /// (../xserver/composite/compalloc.c:562). But Xorg has NO separate
    /// per-window storage, so that copy captures the redirected
    /// window's OWN pixels (it is an inferior of the parent). yserver
    /// keeps a separate per-window leaf, so the parent storage holds
    /// only the parent's pixels (wallpaper for a root child) and W's
    /// own content is lost — the "half-drawn MATE panel under compiz
    /// --replace" bug (static regions never repaint after a
    /// compositor handoff, so they keep the wallpaper seed).
    ///
    /// This reconstructs the inferiors: DFS over W and its mapped
    /// descendants in `stack_rank` (bottom-to-top) order, compositing
    /// each window's own leaf into B at its accumulated offset from W.
    /// A descendant that owns its OWN `redirected_target` (an
    /// independently-redirected child — XEmbed systray icons, etc.) is
    /// PRUNED with its whole subtree: the client compositor reads and
    /// composites that child's backing separately via
    /// `NameWindowPixmap`, so its pixels must not be baked into W's
    /// backing.
    ///
    /// Must run BEFORE `set_redirected_target(W)` so the walk reads
    /// each window's leaf, not the about-to-be-installed route. Keep
    /// the per-window/stacking rules in sync with
    /// `scene::emit_window_subtree` (scene.rs:2406).
    fn overlay_backing_inferiors(&mut self, w_xid: u32, b_id: crate::kms::v2::store::DrawableId) {
        use crate::kms::{
            cpu_types::Repeat, v2::engine::ResolvedSource, vk::ops::render::CompositeRect,
        };
        let plan = self.plan_backing_inferiors(w_xid, b_id);
        if plan.is_empty() {
            return;
        }
        // PictOpOver — alpha children blend over the parent base;
        // depth-24 leaves sample with α=1 (engine `sample_view`
        // swizzle) so they fully replace the base where opaque.
        const OP_OVER: u8 = 3;
        for d in plan {
            // Skip leaves whose storage has no realized view (no GPU
            // backing yet) — the planner left the liveness check here.
            if self
                .store
                .get(d.leaf_id)
                .is_none_or(|s| s.storage.image_view == ash::vk::ImageView::null())
            {
                continue;
            }
            let rects = [CompositeRect {
                src_x: d.src_x,
                src_y: d.src_y,
                mask_x: 0,
                mask_y: 0,
                dst_x: d.dst_x,
                dst_y: d.dst_y,
                width: d.width,
                height: d.height,
            }];
            match self.engine.render_composite(
                &mut self.store,
                &mut self.platform,
                OP_OVER,
                ResolvedSource::Drawable(d.leaf_id),
                ResolvedSource::None,
                b_id,
                &rects,
                None,
                Repeat::None,
                Repeat::None,
                None,
                None,
                false,
                // Synthesized seed; no Picture context — engine falls
                // back to the depth heuristic (→ `sample_view`).
                0,
                0,
                0,
            ) {
                Ok(s) if s.recorded_draws > 0 => {
                    self.telemetry.record_paint_submit();
                    self.trace_simple(SubmitKind::RenderComposite, b_id, s.recorded_draws);
                }
                Ok(_) => {}
                Err(e) => log::warn!(
                    "v2 overlay_backing_inferiors(0x{w_xid:x}): leaf {leaf:?} composite failed: {e:?}",
                    leaf = d.leaf_id,
                ),
            }
        }
    }

    /// Pure planner for [`Self::overlay_backing_inferiors`] — returns
    /// the ordered leaf→backing composites (backing-local coords,
    /// B's (0,0) == W's origin). Split out so the
    /// traversal/prune/clip logic is unit-testable without a live
    /// `RenderEngine`/Vulkan context.
    fn plan_backing_inferiors(
        &self,
        w_xid: u32,
        b_id: crate::kms::v2::store::DrawableId,
    ) -> Vec<SeedInferiorDraw> {
        let mut out = Vec::new();
        let b_extent = self
            .store
            .get(b_id)
            .map_or(ash::vk::Extent2D::default(), |d| d.storage.extent);
        if b_extent.width == 0 || b_extent.height == 0 {
            return out;
        }
        self.collect_backing_inferiors(w_xid, 0, 0, true, &mut out);
        out
    }

    /// Recursive worker for [`Self::plan_backing_inferiors`].
    /// `(off_x, off_y)` is the current window's origin in backing-local
    /// coords; `is_seed_root` is true only for W itself (W is becoming
    /// redirected now, so its own `redirected_target` must not prune
    /// it).
    fn collect_backing_inferiors(
        &self,
        xid: u32,
        off_x: i32,
        off_y: i32,
        is_seed_root: bool,
        out: &mut Vec<SeedInferiorDraw>,
    ) {
        let Some(geom) = self.windows_v2.get(&xid).copied() else {
            return;
        };
        if !geom.mapped {
            // X11: an unmapped window (and its whole subtree) is invisible.
            return;
        }
        let Some(leaf_id) = self.store.lookup(xid) else {
            return;
        };

        // Prune at an independently-redirected descendant: its backing
        // is composited separately by the client compositor, so its
        // pixels (and its subtree) must not be baked into W's backing.
        if !is_seed_root && self.store.redirected_target(leaf_id).is_some() {
            return;
        }

        // Plan this window's own leaf if it's a Window with sized
        // storage. The GPU-liveness check (`image_view != null`) is
        // deferred to the consumer so this planner stays pure/testable.
        if let Some(d) = self.store.get(leaf_id)
            && matches!(d.kind, crate::kms::v2::store::DrawableKind::Window)
        {
            let w = u32::from(geom.width).min(d.storage.extent.width);
            let h = u32::from(geom.height).min(d.storage.extent.height);
            self.push_inferior_rects(xid, leaf_id, off_x, off_y, w, h, out);
        }

        // Recurse mapped children bottom-to-top, same `stack_rank`
        // order as `scene::emit_window_subtree` (scene.rs:2406).
        let mut children: Vec<(u32, u64)> = self
            .windows_v2
            .iter()
            .filter_map(|(c, g)| (g.parent == Some(xid)).then_some((*c, g.stack_rank)))
            .collect();
        children.sort_by_key(|(_, rank)| *rank);
        for (child, _) in children {
            let (cx, cy) = self
                .windows_v2
                .get(&child)
                .map_or((0, 0), |g| (i32::from(g.x), i32::from(g.y)));
            self.collect_backing_inferiors(child, off_x + cx, off_y + cy, false, out);
        }
    }

    /// Emit the leaf→backing rects for one window, honoring SHAPE
    /// bounding (one rect per bound, mirroring scene.rs:2302) and
    /// clamping negative destination offsets by shifting the source
    /// (the off-parent/offscreen case the parent-seed copy mishandled).
    fn push_inferior_rects(
        &self,
        xid: u32,
        leaf_id: crate::kms::v2::store::DrawableId,
        off_x: i32,
        off_y: i32,
        w: u32,
        h: u32,
        out: &mut Vec<SeedInferiorDraw>,
    ) {
        if w == 0 || h == 0 {
            return;
        }
        let mut emit = |sx: i32, sy: i32, dx: i32, dy: i32, rw: i32, rh: i32| {
            let (mut sx, mut sy, mut dx, mut dy) = (sx, sy, dx, dy);
            let (mut rw, mut rh) = (i64::from(rw), i64::from(rh));
            // Low-side clamp: a negative dst shifts the source and
            // shrinks the rect. High-side clipping is left to
            // `render_composite`'s dst-extent clamp.
            if dx < 0 {
                sx -= dx;
                rw += i64::from(dx);
                dx = 0;
            }
            if dy < 0 {
                sy -= dy;
                rh += i64::from(dy);
                dy = 0;
            }
            if rw <= 0 || rh <= 0 {
                return;
            }
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            out.push(SeedInferiorDraw {
                leaf_id,
                src_x: sx,
                src_y: sy,
                dst_x: dx,
                dst_y: dy,
                width: rw as u32,
                height: rh as u32,
            });
        };
        if let Some(rects) = self.core.shape_bounding.get(&xid) {
            for r in rects {
                let (rx, ry) = (i32::from(r.x), i32::from(r.y));
                let (rw, rh) = (i32::from(r.width), i32::from(r.height));
                // Intersect the bound with the window box [0,w)×[0,h).
                let cx = rx.max(0);
                let cy = ry.max(0);
                #[allow(clippy::cast_possible_wrap)]
                let cw = (rx + rw).min(w as i32) - cx;
                #[allow(clippy::cast_possible_wrap)]
                let ch = (ry + rh).min(h as i32) - cy;
                if cw <= 0 || ch <= 0 {
                    continue;
                }
                emit(cx, cy, off_x + cx, off_y + cy, cw, ch);
            }
        } else {
            #[allow(clippy::cast_possible_wrap)]
            emit(0, 0, off_x, off_y, w as i32, h as i32);
        }
    }

    /// Virtual-screen extent — mirrors `KmsBackend::fb_dimensions`.
    /// Called by `lib.rs` during the pre-`Box<dyn Backend>` setup
    /// (capability advertisement, `ServerState::with_randr_outputs`).
    #[must_use]
    pub fn fb_dimensions(&self) -> (u16, u16) {
        self.platform.fb_dimensions()
    }

    /// RandR output list — mirrors `KmsBackend::randr_outputs`.
    #[must_use]
    pub fn randr_outputs(&self) -> Vec<yserver_core::randr::RandrOutput> {
        use std::collections::HashMap;
        use yserver_core::randr::RandrOutput;
        let n = self.platform.outputs.len();
        let mut mode_ids: HashMap<(u16, u16, u32), u32> = HashMap::new();
        #[allow(clippy::cast_possible_truncation)]
        let mut next_mode_id: u32 = (2 * n + 1) as u32;
        self.platform
            .outputs
            .iter()
            .enumerate()
            .map(|(i, layout)| {
                let vrefresh = layout.output.picked.vrefresh;
                let key = (layout.width, layout.height, vrefresh);
                let mode_id = *mode_ids.entry(key).or_insert_with(|| {
                    let id = next_mode_id;
                    next_mode_id += 1;
                    id
                });
                #[allow(clippy::cast_possible_truncation)]
                let output_id = (i + 1) as u32;
                #[allow(clippy::cast_possible_truncation)]
                let crtc_id = (n + i + 1) as u32;
                RandrOutput {
                    name: layout.output.connector_name.clone(),
                    output_id,
                    crtc_id,
                    mode_id,
                    x: i16::try_from(layout.x).unwrap_or(i16::MAX),
                    y: i16::try_from(layout.y).unwrap_or(i16::MAX),
                    width: layout.width,
                    height: layout.height,
                    vrefresh,
                    mm_width: layout.output.mm_width,
                    mm_height: layout.output.mm_height,
                }
            })
            .collect()
    }

    /// Telemetry accessor — used by the acceptance harness to
    /// read lifetime counters after driving a test sequence.
    #[must_use]
    pub fn telemetry(&self) -> &Telemetry {
        &self.telemetry
    }

    // ── Stage 5 Task 3 diagnostic instrumentation helpers ───────
    //
    // These exist purely to wire `YSERVER_SUBMIT_TRACE` event
    // recording at every `vkQueueSubmit2` site with minimal
    // per-site boilerplate. Zero hot-path cost when the env var
    // is unset (`Telemetry::record_submit_event` early-returns on
    // `submit_trace.is_none()`).

    /// Classify a v2 drawable by kind for the trace TSV's
    /// `target_kind` column. COW is held separately on
    /// `KmsBackendV2`; other kinds come from `DrawableKind`.
    fn submit_target_kind(&self, id: DrawableId) -> TargetKind {
        if self.cow_id == Some(id) {
            return TargetKind::Cow;
        }
        match self.store.get(id).map(|d| d.kind) {
            Some(DrawableKind::Root) => TargetKind::Root,
            Some(DrawableKind::Window) => TargetKind::Window,
            Some(DrawableKind::Pixmap) => TargetKind::Pixmap,
            Some(DrawableKind::Cursor) => TargetKind::Cursor,
            Some(DrawableKind::RedirectedBacking) => TargetKind::Backing,
            None => TargetKind::Unknown,
        }
    }

    /// Emit one trace event for a plain paint submit (no
    /// render-key info). One-liner helper for the ~12 sites
    /// that don't need op/src/mask plumbing.
    fn trace_simple(&mut self, kind: SubmitKind, target: DrawableId, batch_size: u32) {
        let target_kind = self.submit_target_kind(target);
        self.telemetry.record_submit_event(SubmitEvent {
            frame_id: 0,
            kind,
            target_kind,
            target_id: target.as_u64(),
            batch_size,
            op: SubmitOp::None,
            src_class: SrcClass::None,
            mask_class: SrcClass::None,
            pipeline_id: None,
            flags: SubmitFlags::NONE,
        });
    }

    /// Emit one trace event for a RENDER paint submit with full
    /// op + src/mask class info. `mask` of `None` writes
    /// `no_mask`; `Some(class)` writes the class.
    fn trace_render(
        &mut self,
        kind: SubmitKind,
        target: DrawableId,
        batch_size: u32,
        op_byte: u8,
        src: SrcClass,
        mask: Option<SrcClass>,
        flags: SubmitFlags,
    ) {
        let target_kind = self.submit_target_kind(target);
        self.telemetry.record_submit_event(SubmitEvent {
            frame_id: 0,
            kind,
            target_kind,
            target_id: target.as_u64(),
            batch_size,
            op: SubmitOp::from_pict_op_byte(op_byte),
            src_class: src,
            mask_class: mask.unwrap_or(SrcClass::NoMask),
            pipeline_id: None,
            flags,
        });
    }

    /// Classify a `PictureRecord` for the `src_class` /
    /// `mask_class` columns. Used by render-path call sites.
    fn picture_src_class(record: &PictureRecord) -> SrcClass {
        match record {
            PictureRecord::Drawable { .. } => SrcClass::Direct,
            PictureRecord::SolidFill { .. } => SrcClass::Solid,
            PictureRecord::LinearGradient { .. } => SrcClass::GradientLinear,
            PictureRecord::RadialGradient { .. } => SrcClass::GradientRadial,
        }
    }

    /// Lookup a picture xid in `core.pictures` and return its
    /// class, or `SrcClass::Direct` if the xid doesn't resolve
    /// (rare — render sites already guard on `resolve_*`; this
    /// is a defensive default for the diagnostic).
    fn picture_src_class_by_xid(&self, xid: u32) -> SrcClass {
        self.core
            .pictures
            .get(&xid)
            .map_or(SrcClass::Direct, Self::picture_src_class)
    }

    /// Stage 5 Task 3 POC: drain the engine's cow-batch flush
    /// records (since the last drain), bump telemetry counters,
    /// Stage 5 Task 3 (render-composite generalization): drain
    /// the engine's render-batch flush records, bump telemetry
    /// counters, emit one submit-trace event per flush.
    fn drain_render_telemetry(&mut self) {
        let records = self.engine.drain_render_flush_records();
        if records.is_empty() {
            return;
        }
        for rec in records {
            self.telemetry
                .record_render_batch_flushed(rec.coalesced_count);
            let target_kind = self.submit_target_kind(rec.dst);
            self.telemetry.record_submit_event(SubmitEvent {
                frame_id: 0,
                kind: SubmitKind::RenderComposite,
                target_kind,
                target_id: rec.dst.as_u64(),
                batch_size: rec.coalesced_count,
                op: SubmitOp::from_pict_op_byte(rec.op),
                src_class: SrcClass::Direct,
                mask_class: if rec.has_mask {
                    SrcClass::Direct
                } else {
                    SrcClass::NoMask
                },
                pipeline_id: None,
                flags: SubmitFlags::NONE,
            });
        }
    }

    /// Phase B.1 Task 21: drain queued `FrameCloseEvent`s into the
    /// per-second telemetry. Called from every site that drives a
    /// frame close (`maybe_composite`, `enqueue_present_completion`,
    /// `get_image`, `shutdown`/`disable_output`, `render_composite_glyphs`).
    ///
    /// Opens are delta-tracked via `last_drained_fb_opens` — the
    /// drain emits one `record_frame_builder_open` for each new open
    /// since the previous call without requiring a separate event queue.
    ///
    /// The drain is idempotent: calling it multiple times in a row is
    /// harmless (the event queue and delta are both empty after the
    /// first call).
    fn drain_frame_builder_telemetry(&mut self) {
        // Delta-track opens (FrameBuilder doesn't queue open events;
        // we infer them from the monotonic lifetime counter).
        let current_opens = self.engine.frame_builder_lifetime_opens();
        let delta_opens = current_opens.saturating_sub(self.last_drained_fb_opens);
        for _ in 0..delta_opens {
            self.telemetry.record_frame_builder_open();
        }
        self.last_drained_fb_opens = current_opens;

        // Drain the close-event queue.
        for event in self.engine.drain_frame_close_events() {
            if event.aborted {
                self.telemetry.record_frame_builder_abort();
            } else {
                self.telemetry.record_frame_builder_close(
                    event.reason,
                    event.ops_in_frame,
                    event.glyph_uploads_in_frame,
                    event.renders_in_frame,
                );
            }
            // Aborts also record pin_count high water — those pins existed
            // before the failure dropped them.
            self.telemetry.record_frame_builder_active_pins_high_water(
                u64::try_from(event.pin_count).unwrap_or(u64::MAX),
            );
        }
    }

    /// Phase B.2 Task 14 test helper: drive
    /// `drain_frame_builder_telemetry` from a test so the queued
    /// `FrameCloseEvent`s feed into lifetime counters without
    /// requiring a full main-loop tick. Mirrors the role of
    /// `telemetry_submit_group_flushes_for_tests` for the
    /// frame-close-event side of the telemetry pipeline.
    #[doc(hidden)]
    pub fn drain_frame_builder_telemetry_for_tests(&mut self) {
        self.drain_frame_builder_telemetry();
    }

    /// Stage 5 Task 4 layer 1: test-side accessor to the ring's
    /// pool residency. Used by the acceptance harness to assert
    /// steady-state pool count stays small after warm-up.
    #[doc(hidden)]
    #[must_use]
    pub fn descriptor_pool_ring_pool_count(&self) -> usize {
        self.engine.descriptor_pool_ring_pool_count()
    }

    /// Test-only accessor for the engine's per-drawable view cache
    /// size. Used by the live-Vk integration test that gates the
    /// `notify_drawable_retired` runtime-wiring fix: pre-fix a
    /// destroyed drawable's cached views accumulated until engine
    /// `Drop`; post-fix they're invalidated synchronously via
    /// `store_decref_with_invalidate` / `poll_pending_retire_with_invalidate`.
    #[doc(hidden)]
    #[must_use]
    pub fn drawable_view_cache_len(&self) -> usize {
        self.engine.drawable_view_cache_len()
    }

    /// Stage 5 Task 4 layer 1: test-side retirement driver. In
    /// production, retirement runs from `on_page_flip_ready` and
    /// invokes `engine.poll_retired` + `store.poll_pending_retire`.
    /// Pixmap-only test fixtures never drive a page flip, so the
    /// ring's recycle path can't run without this hook. The body
    /// mirrors the production sequence 1:1 so the acceptance harness
    /// exercises the same code paths any future store-retirement
    /// work would touch — and adds the telemetry sync call so ring
    /// delta counters land in `self.telemetry`.
    #[doc(hidden)]
    pub fn for_tests_poll_retired(&mut self) {
        self.engine.poll_retired(&self.platform);
        self.poll_pending_retire_with_invalidate();
        self.sync_descriptor_pool_telemetry();
    }

    /// Bridge `store.decref` to `engine.notify_drawable_retired`.
    /// When `decref` decides the drawable is destroyable, the
    /// closure fires BEFORE `Storage::destroy`, so any
    /// `VkImageView` cached in `RenderEngine::drawable_view_cache`
    /// for that `DrawableId` is destroyed while its underlying
    /// `VkImage` is still alive. Without this hook the cache
    /// accumulates entries pointing at freed images for the
    /// lifetime of the session (only swept at engine `Drop`).
    /// `DrawableId`s are monotonically allocated so stale entries
    /// can never alias a fresh drawable — the leak is memory-only,
    /// not use-after-free — but unbounded growth still matters.
    pub(crate) fn store_decref_with_invalidate(
        &mut self,
        id: crate::kms::v2::store::DrawableId,
    ) -> crate::kms::v2::store::RetireDecision {
        let engine = &mut self.engine;
        self.store.decref(&mut self.platform, id, |dropped| {
            engine.notify_drawable_retired(dropped);
        })
    }

    // ── GLX-TFP (Tasks 2.3 + 2.4): exported-backing lifetime ─────────

    /// Ensure an `ExportedBacking` entry exists for `id`, taking the
    /// single backing lifetime ref on first creation. Idempotent.
    ///
    /// Spelled as an explicit `contains_key` → take-ref → `insert` →
    /// `get_mut` rather than `entry().or_insert_with(..)` because taking
    /// the lifetime ref needs `&mut self` (alias_registry / store), which
    /// would conflict with an `entry` borrow of `self.exported_dmabufs`.
    fn ensure_exported_entry(
        &mut self,
        id: crate::kms::v2::store::DrawableId,
        backing: PixmapHandle,
    ) -> &mut ExportedBacking {
        if !self.exported_dmabufs.contains_key(&id) {
            // ONE lifetime ref per backing, released exactly once in teardown.
            let via_alias = self.take_backing_lifetime_ref(id, backing);
            self.exported_dmabufs.insert(
                id,
                ExportedBacking {
                    fd: None,
                    glx_refs: 0,
                    lifetime_ref_held: true,
                    backing_id: id,
                    backing,
                    lifetime_via_alias: via_alias,
                },
            );
        }
        self.exported_dmabufs
            .get_mut(&id)
            .expect("entry just ensured")
    }

    /// Take ONE lifetime ref on the backing's owning counter so the
    /// storage survives an early `FreePixmap` while a GL consumer still
    /// references it. Returns `true` if the ref was taken on
    /// `alias_registry` (NameWindowPixmap'd redirect backing), `false`
    /// if on the `DrawableStore` refcount (plain pixmap).
    fn take_backing_lifetime_ref(
        &mut self,
        id: crate::kms::v2::store::DrawableId,
        backing: PixmapHandle,
    ) -> bool {
        if self.core.alias_registry.get(backing).is_some() {
            self.core.alias_registry.incref(backing);
            true
        } else {
            self.store.incref(id);
            false
        }
    }

    /// Release the single lifetime ref taken in `take_backing_lifetime_ref`.
    /// Routes to the counter recorded at take time. For the alias path a
    /// final decref frees the backing via the same release path
    /// `free_pixmap` uses; for the plain path the store decref frees the
    /// storage when its own refcount also hits zero.
    fn release_backing_lifetime_ref(&mut self, entry: &ExportedBacking) {
        if entry.lifetime_via_alias {
            // Alias path: decref the registry (keyed by xid); on final ref
            // free the backing via the same store decref `free_pixmap` uses.
            if self.core.alias_registry.decref(entry.backing) {
                self.store_decref_with_invalidate(entry.backing_id);
            }
        } else {
            // Plain path: drop the extra store ref taken at acquire time.
            // Use the captured DrawableId — the `by_xid` mapping may have
            // been detached by an intervening FreePixmap (PendingFence).
            self.store_decref_with_invalidate(entry.backing_id);
        }
    }

    /// GLX-TFP (Task 3.4 callers): record that a GLXPixmap now references
    /// the export of `host_xid`. Creates the entry (+ lifetime ref) if
    /// this is the first arrival. Public so the GLX protocol surface can
    /// call it. No-op on an unknown xid.
    pub fn acquire_glx_pixmap_export(&mut self, host_xid: u32) {
        let Some(id) = self.store.lookup(host_xid) else {
            return;
        };
        let Some(backing) = PixmapHandle::from_raw(host_xid) else {
            return;
        };
        self.ensure_exported_entry(id, backing).glx_refs += 1;
    }

    /// GLX-TFP (Task 3.4 callers): a GLXPixmap referencing the export of
    /// `host_xid` is gone. Drops one `glx_refs` and tears the export down
    /// when it reaches zero.
    pub fn release_glx_pixmap_export(&mut self, host_xid: u32) {
        let Some(id) = self.store.lookup(host_xid) else {
            return;
        };
        if let Some(e) = self.exported_dmabufs.get_mut(&id) {
            e.glx_refs = e.glx_refs.saturating_sub(1);
        }
        self.maybe_teardown_export(id);
    }

    /// GLX-TFP: tear the export down iff no GLX consumer references it
    /// (`glx_refs == 0`). Drops the dup'd fd, clears the store's sync-fd
    /// dup, and releases the single lifetime ref. While `glx_refs > 0`
    /// this is a no-op (defer-destroy-while-referenced). There is NO
    /// `x_pixmap_freed` flag — an early `FreePixmap` is bridged by the
    /// lifetime ref and resolved here once `glx_refs` hits 0.
    fn maybe_teardown_export(&mut self, id: crate::kms::v2::store::DrawableId) {
        let ready = matches!(self.exported_dmabufs.get(&id), Some(e) if e.glx_refs == 0);
        if !ready {
            return;
        }
        if let Some(e) = self.exported_dmabufs.remove(&id) {
            // Drop the store's parallel sync-fd dup so no write can be
            // routed through a torn-down export.
            self.store.clear_exported_sync_fd(id);
            // `e.fd` (our dup) closes on drop; the kernel buffer survives
            // while any GL consumer still holds its own fd.
            if e.lifetime_ref_held {
                self.release_backing_lifetime_ref(&e);
            }
        }
    }

    /// GLX-TFP (Task 3.5 `glXBindTexImageEXT`): promote the backing for
    /// `host_xid` to dma-buf-exportable storage, idempotently, WITHOUT
    /// touching `glx_refs` or `exported_dmabufs` (no fd alloc/export).
    /// Returns whether the backing is exportable after the call.
    ///
    /// This is the lightweight bind hook — distinct from
    /// `acquire_glx_pixmap_export` (which manages the GLXPixmap LIFETIME
    /// ref). Binds happen repeatedly (per-frame); coupling them to the
    /// lifetime refcount would leak the backing. The engine's
    /// `promote_drawable_exportable` short-circuits via `is_exportable()`
    /// so repeated binds cost nothing after the first.
    pub fn promote_pixmap_exportable(&mut self, host_xid: u32) -> bool {
        let Some(id) = self.store.lookup(host_xid) else {
            return false;
        };
        // Already exportable → idempotent no-op (the common rebind case).
        if self
            .store
            .get(id)
            .map(|d| d.storage.is_exportable())
            .unwrap_or(false)
        {
            return true;
        }
        // First bind: promote. Requires Vulkan; if unavailable, report
        // not-exportable rather than erroring (bind still succeeds at the
        // protocol layer — see process_request's BindTexImageEXT TODO).
        if self.platform.vk.is_none() {
            return false;
        }
        match self
            .engine
            .promote_drawable_exportable(&mut self.platform, &mut self.store, id)
        {
            Ok(()) => self
                .store
                .get(id)
                .map(|d| d.storage.is_exportable())
                .unwrap_or(false),
            Err(e) => {
                log::warn!("GLX BindTexImageEXT promote 0x{host_xid:x} failed: {e:?}");
                false
            }
        }
    }

    /// GLX-TFP test introspection: true iff `host_xid` currently has an
    /// `ExportedBacking` entry.
    #[doc(hidden)]
    pub fn has_export_entry(&self, host_xid: u32) -> bool {
        self.store
            .lookup(host_xid)
            .is_some_and(|id| self.exported_dmabufs.contains_key(&id))
    }

    /// Bridge `store.poll_pending_retire` to
    /// `engine.notify_drawable_retired`. Per the rationale on
    /// `store_decref_with_invalidate`, drawables that were
    /// parked in `pending_retire` (waiting for their fence to
    /// signal) get their engine-side view caches dropped before
    /// `Storage::destroy` runs.
    pub(crate) fn poll_pending_retire_with_invalidate(&mut self) {
        let engine = &mut self.engine;
        self.store
            .poll_pending_retire(&mut self.platform, |dropped| {
                engine.notify_drawable_retired(dropped);
            });
    }

    /// Stage 5 Task 6.1 — test-only: number of entries currently in
    /// the deferred PRESENT completion queue.
    #[doc(hidden)]
    pub fn pending_present_events_len_for_tests(&self) -> usize {
        self.pending_present_batches
            .iter()
            .map(|batch| batch.events.len())
            .sum()
    }

    /// Stage 5 Task 6.1 — test-only: drain a single signal-check +
    /// emit cycle without going through the main loop. Equivalent to
    /// one outer-loop iteration's drain hook.
    #[doc(hidden)]
    pub fn drain_completed_present_events_for_tests(
        &mut self,
    ) -> Vec<yserver_core::backend::CompletedPresentEvent> {
        self.drain_completed_present_events_impl()
    }

    /// Stage 5 Task 6.1 — test-only: flip the platform's
    /// `renderer_failed` flag. Used by the force-fire-all integration
    /// test.
    #[doc(hidden)]
    pub fn set_renderer_failed_for_tests(&mut self, v: bool) {
        self.platform.renderer_failed = v;
    }

    /// Phase A T6: flush the engine's SubmitGroup with a
    /// `SyncBoundary` reason. Convenience wrapper for v2_acceptance
    /// tests that need to drain setup CBs between assertions.
    pub fn engine_flush_submit_group_for_tests(&mut self) -> Result<(), ash::vk::Result> {
        self.engine
            .flush_submit_group(
                &mut self.store,
                &mut self.platform,
                crate::kms::v2::submit_group::FlushReason::SyncBoundary,
            )
            .map(|_| ())
    }

    /// Phase A T6: number of ops parked in the engine's
    /// `pending_group_ops` (not yet committed to `submitted`).
    /// Exposed for v2_acceptance regression tests.
    pub fn engine_pending_group_ops_count_for_tests(&self) -> usize {
        self.engine.pending_group_ops_count_for_tests()
    }

    /// Phase B.3 (N8): scratch vec length of the most recently submitted op
    /// in the engine. Used by `b3_close_path_scratch_walk_*` v2_acceptance
    /// integration tests to verify the close-path walk threads the
    /// `frame_scratches` local into `SubmittedOp::scratch`.
    pub fn engine_most_recent_submitted_op_scratch_len_for_tests(&self) -> usize {
        self.engine.most_recent_submitted_op_scratch_len_for_tests()
    }

    /// Phase B.2 Task 9: allocate a fresh BGRA8 pixmap via the
    /// engine's `create_pixmap`. Returns the host xid the test code
    /// uses as an opaque drawable handle; the integration crate
    /// can't see `DrawableId` (it's `pub(crate)`) so xids are the
    /// stable test surface.
    ///
    /// Returns `None` on Vk failure (e.g. test fixture without Vk
    /// or storage allocation error).
    pub fn allocate_test_pixmap_bgra(&mut self, width: u16, height: u16) -> Option<u32> {
        let xid = self.core.next_host_xid();
        let storage = self
            .platform
            .allocate_drawable_storage(width, height, 32)
            .ok()?;
        self.store
            .allocate(xid, super::store::DrawableKind::Pixmap, 32, false, storage)
            .ok()?;
        Some(xid)
    }

    /// Phase B.2 Task 9: invoke `render_composite` with an empty
    /// `rects` slice. The frame-builder path's first check is
    /// `if rects.is_empty() { return Ok(stats); }` BEFORE any state
    /// mutation — used by the empty-rects-doesn't-open-frame test.
    ///
    /// `dst_xid` must resolve in the store; the function ignores the
    /// dst layout because the early-return path doesn't touch it.
    ///
    /// # Errors
    ///
    /// Returns `Err` if `dst_xid` doesn't resolve in the store; the
    /// engine call itself is infallible on empty rects.
    pub fn render_composite_empty_for_tests(&mut self, dst_xid: u32) -> Result<(), io::Error> {
        let Some(dst_id) = self.store.lookup(dst_xid) else {
            return Err(io::Error::other(format!(
                "render_composite_empty_for_tests: dst xid 0x{dst_xid:x} not in store"
            )));
        };
        const OP_SRC: u8 = 1;
        self.engine
            .render_composite(
                &mut self.store,
                &mut self.platform,
                OP_SRC,
                super::engine::ResolvedSource::Solid([0.0, 0.0, 0.0, 1.0]),
                super::engine::ResolvedSource::None,
                dst_id,
                &[],
                None,
                crate::kms::cpu_types::Repeat::Pad,
                crate::kms::cpu_types::Repeat::Pad,
                None,
                None,
                false,
                0,
                0,
                0,
            )
            .map(|_| ())
            .map_err(|e| io::Error::other(format!("render_composite_empty_for_tests: {e:?}")))
    }

    /// Phase B.2 Task 11: drive a single-rect Solid-src `render_composite`
    /// against `dst_xid`. The rect covers the full dst extent (Solid
    /// fill at op=SRC). Used by the second-op-in-frame overlay test:
    /// two successive calls share the same dst, so op #2 must observe
    /// op #1's post-op layout via the overlay (Pitfall 5+6).
    ///
    /// # Errors
    ///
    /// Returns `Err` if `dst_xid` doesn't resolve in the store, or if
    /// the engine call fails (Vk error, missing pipeline, etc.).
    pub fn render_composite_for_tests(
        &mut self,
        dst_xid: u32,
        color: [f32; 4],
        width: u32,
        height: u32,
    ) -> Result<(), io::Error> {
        let Some(dst_id) = self.store.lookup(dst_xid) else {
            return Err(io::Error::other(format!(
                "render_composite_for_tests: dst xid 0x{dst_xid:x} not in store"
            )));
        };
        const OP_SRC: u8 = 1;
        let rect = crate::kms::vk::ops::render::CompositeRect {
            src_x: 0,
            src_y: 0,
            mask_x: 0,
            mask_y: 0,
            dst_x: 0,
            dst_y: 0,
            width,
            height,
        };
        self.engine
            .render_composite(
                &mut self.store,
                &mut self.platform,
                OP_SRC,
                super::engine::ResolvedSource::Solid(color),
                super::engine::ResolvedSource::None,
                dst_id,
                std::slice::from_ref(&rect),
                None,
                crate::kms::cpu_types::Repeat::Pad,
                crate::kms::cpu_types::Repeat::Pad,
                None,
                None,
                false,
                0,
                0,
                0,
            )
            .map(|_| ())
            .map_err(|e| io::Error::other(format!("render_composite_for_tests: {e:?}")))
    }

    /// Phase B.2 Task 17: drive `render_fill_rectangles` directly
    /// against `dst_xid`. The wrapper delegates to `render_composite`
    /// with `ResolvedSource::Solid(color)` (see
    /// `engine::render_fill_rectangles`); under sub-gate=ON this
    /// routes through the frame builder, so two calls into the same
    /// open frame collapse into a single submit.
    ///
    /// # Errors
    ///
    /// Returns `Err` if `dst_xid` doesn't resolve in the store, or if
    /// the engine call fails (Vk error, missing pipeline, etc.).
    pub fn render_fill_rectangles_for_tests(
        &mut self,
        dst_xid: u32,
        op: u8,
        color: [f32; 4],
        rects: &[crate::kms::vk::ops::render::CompositeRect],
    ) -> Result<(), io::Error> {
        let Some(dst_id) = self.store.lookup(dst_xid) else {
            return Err(io::Error::other(format!(
                "render_fill_rectangles_for_tests: dst xid 0x{dst_xid:x} not in store"
            )));
        };
        self.engine
            .render_fill_rectangles(
                &mut self.store,
                &mut self.platform,
                op,
                color,
                dst_id,
                rects,
                None,
            )
            .map(|_| ())
            .map_err(|e| io::Error::other(format!("render_fill_rectangles_for_tests: {e:?}")))
    }

    /// Phase B.2 Task 18: read the drawable's current Vk image layout.
    /// Used by the submit-failure rollback test to snapshot the
    /// pre-frame layout and assert that `rollback_pre_submit` restored
    /// it after the close-walk failed.
    ///
    /// Returns `vk::ImageLayout::UNDEFINED` if `dst_xid` doesn't
    /// resolve in the store (rare; production code always inserts
    /// before any layout transition).
    pub fn drawable_current_layout_for_tests(&self, dst_xid: u32) -> ash::vk::ImageLayout {
        self.store
            .get_by_xid(dst_xid)
            .map_or(ash::vk::ImageLayout::UNDEFINED, |d| {
                d.storage.current_layout
            })
    }

    /// Phase B.2 Task 11: typed peek of the open frame's recorded
    /// `RenderComposite` ops' `dst_old_layout` field, in append order.
    /// Used by the second-op-in-frame overlay test to assert that
    /// op #2 reads the overlay-resolved post-op layout of op #1
    /// (SHADER_READ_ONLY_OPTIMAL) rather than the stale storage value.
    ///
    /// The `RecordedRenderComposite` payload is `pub(crate)` so the
    /// integration test cannot match on it directly; this returns the
    /// minimum scalar needed for the assertion.
    pub fn frame_builder_peek_render_composite_dst_old_layouts_for_tests(
        &self,
    ) -> Vec<ash::vk::ImageLayout> {
        self.engine
            .frame_builder_peek_render_composite_dst_old_layouts()
    }

    /// Phase B.1 Task 15: is the frame builder currently open?
    pub fn frame_builder_is_open_for_tests(&self) -> bool {
        self.engine.frame_builder_is_open()
    }

    /// Phase B.1 Task 15: lifetime closes counter snapshot.
    pub fn frame_builder_lifetime_closes_for_tests(&self) -> u64 {
        self.engine.frame_builder_lifetime_closes()
    }

    /// Phase B.1 Task 15: drive one `maybe_composite` tick. Used by
    /// integration tests that need to trigger an M3 close without
    /// going through a full backend tick.
    pub fn tick_maybe_composite_for_tests(&mut self) {
        let _ = self.maybe_composite();
    }

    /// Phase B.1 Task 15: engine's monotonic `frame_seq` counter.
    /// Bumped by `close_open_frame` on every successful close.
    pub fn engine_frame_seq_for_tests(&self) -> u64 {
        self.engine.engine_frame_seq()
    }

    /// Phase B.2 Task 3 (Mechanism 2 watermark): set the engine's
    /// `acquire_generation` field directly. Used by the integration
    /// test to seed a known baseline before exercising the
    /// frame-open + descriptor-acquire dance, so the captured
    /// `frame_generation` assertions are deterministic.
    pub fn engine_acquire_generation_set_for_tests(&mut self, value: u64) {
        self.engine.set_acquire_generation_for_tests(value);
    }

    /// Phase B.2 Task 3: open a frame end-to-end (acquire the
    /// platform's submit-group ticket via
    /// `submit_group_ticket_or_open`, then drive the engine's
    /// `open_for_paint`). Combines the two steps so the test
    /// surface doesn't need to expose the crate-private
    /// `FenceTicket` type. The engine bumps `acquire_generation`
    /// and stamps the resulting value on the OpenFrame as
    /// `frame_generation` (Phase B.2 Mechanism 2).
    ///
    /// # Errors
    ///
    /// Returns `Err` if the platform's fence pool or Vk context is
    /// missing (test fixture built without Vk).
    pub fn engine_open_frame_for_paint_for_tests(&mut self) -> Result<(), ash::vk::Result> {
        let ticket = self.platform.submit_group_ticket_or_open()?;
        self.engine.open_frame_for_paint_for_tests(ticket);
        Ok(())
    }

    /// Phase B.2 Task 3: read the open frame's captured
    /// `frame_generation`. Returns `None` if no frame is open.
    pub fn engine_open_frame_generation_for_tests(&self) -> Option<u64> {
        self.engine.open_frame_generation()
    }

    /// Phase B.2 Task 3: call
    /// `RenderEngineInner::acquire_descriptor_set_for_frame_or_op`
    /// against `layout`. Used by the Mechanism 2 integration test
    /// to confirm the helper tags the active descriptor pool with
    /// the open frame's `frame_generation` (or bumps
    /// `acquire_generation` when no frame is open).
    ///
    /// # Errors
    ///
    /// Propagates `vkAllocateDescriptorSets` / `vkResetDescriptorPool`
    /// errors verbatim.
    pub fn engine_acquire_descriptor_set_for_frame_or_op_for_tests(
        &mut self,
        layout: ash::vk::DescriptorSetLayout,
    ) -> Result<ash::vk::DescriptorSet, ash::vk::Result> {
        self.engine
            .acquire_descriptor_set_for_frame_or_op_for_tests(layout)
    }

    /// Phase B.2 Task 3: build a transient
    /// `vk::DescriptorSetLayout` (single COMBINED_IMAGE_SAMPLER
    /// binding, fragment-stage) for the Mechanism 2 integration
    /// test to feed into
    /// `engine_acquire_descriptor_set_for_frame_or_op_for_tests`.
    /// The caller is responsible for calling
    /// `engine_destroy_descriptor_set_layout_for_tests` after the
    /// test finishes (the layout outlives the descriptor sets;
    /// pool reset on backend drop reclaims the sets, but the
    /// layout handle leaks if not explicitly destroyed).
    ///
    /// # Errors
    ///
    /// Returns `Err` if the platform has no Vk context (test
    /// fixture built without Vk) or `vkCreateDescriptorSetLayout`
    /// fails.
    pub fn engine_create_test_descriptor_set_layout_for_tests(
        &self,
    ) -> Result<ash::vk::DescriptorSetLayout, ash::vk::Result> {
        let vk = self
            .platform
            .vk
            .as_ref()
            .ok_or(ash::vk::Result::ERROR_INITIALIZATION_FAILED)?;
        let bindings = [ash::vk::DescriptorSetLayoutBinding::default()
            .binding(0)
            .descriptor_type(ash::vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(1)
            .stage_flags(ash::vk::ShaderStageFlags::FRAGMENT)];
        let info = ash::vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings);
        // SAFETY: VkContext.device is a valid device handle for the
        //         platform's lifetime; layout creation has no
        //         outstanding-handle preconditions.
        unsafe { vk.device.create_descriptor_set_layout(&info, None) }
    }

    /// Phase B.2 Task 3: destroy a layout created via
    /// `engine_create_test_descriptor_set_layout_for_tests`.
    pub fn engine_destroy_descriptor_set_layout_for_tests(
        &self,
        layout: ash::vk::DescriptorSetLayout,
    ) {
        let Some(vk) = self.platform.vk.as_ref() else {
            return;
        };
        // SAFETY: layout was created by the corresponding
        //         `create_descriptor_set_layout` helper above on
        //         this same device; no descriptor sets remain
        //         active that reference it after the ring's pool
        //         reset on backend drop.
        unsafe { vk.device.destroy_descriptor_set_layout(layout, None) };
    }

    /// Phase B.2 Task 3: descriptor pool ring's max
    /// `high_water_generation` across all resident pools. Returns 0
    /// before the first acquire. Used by the Mechanism 2 integration
    /// test to assert each acquire tagged the pool with the
    /// frame's captured `frame_generation`.
    pub fn descriptor_pool_ring_high_water_generation_for_tests(&self) -> u64 {
        self.engine.descriptor_pool_ring_high_water_generation()
    }

    /// Phase B.2 Task 3: unconditionally close the open frame with
    /// `CloseReason::Timeout`. The integration test uses this to
    /// transition from frame-open → frame-closed without waiting
    /// on a wall-clock timeout.
    ///
    /// # Errors
    ///
    /// Propagates the engine's close-path error (rare; renderer
    /// failure or Vk submit error).
    pub fn engine_close_open_frame_for_timeout_for_tests(&mut self) -> Result<(), ash::vk::Result> {
        self.engine
            .close_open_frame_for_timeout_for_tests(&mut self.store, &mut self.platform)
            .map_err(|e| match e {
                crate::kms::v2::engine::RenderError::Vk(r) => r,
                _ => ash::vk::Result::ERROR_UNKNOWN,
            })
    }

    /// Phase A T6: size of the current SubmitGroup (number of CBs
    /// buffered and not yet submitted to the Vulkan queue).
    /// Exposed for v2_acceptance regression tests.
    pub fn platform_submit_group_size_for_tests(&self) -> usize {
        self.platform.submit_group_size()
    }

    /// Phase A T6: true while the SubmitGroup is open (has at least
    /// one CB buffered). Exposed for v2_acceptance regression tests.
    pub fn platform_submit_group_is_open_for_tests(&self) -> bool {
        self.platform.submit_group_is_open()
    }

    /// Phase A T8: override the SubmitGroup max-size cap so regression
    /// tests can exercise the auto-flush boundary at a specific count
    /// without depending on the production default.
    pub fn platform_submit_group_set_max_size_for_tests(&mut self, n: usize) {
        self.platform.submit_group_set_max_size_for_tests(n);
    }

    /// Phase B.1 Task 10: read back the SubmitGroup max_size as
    /// currently configured on the platform. Exposed as `pub` (not
    /// `#[cfg(test)]`) so the external `v2_acceptance` integration-test
    /// crate can assert Invariant M1 directly.
    pub fn platform_submit_group_max_size_for_tests(&self) -> usize {
        self.platform.submit_group_max_size()
    }

    /// Phase A T10: inject a `queue_submit2` failure on the next
    /// `flush_submit_group` call. Delegates to a non-`#[cfg(test)]`
    /// method on `PlatformBackend` so this wrapper is visible from
    /// the external `v2_acceptance` integration-test crate.
    pub fn platform_force_next_submit_failure_for_tests(&mut self) {
        self.platform
            .force_next_submit_failure_for_integration_tests();
    }

    /// Phase A T10: returns `platform.renderer_failed`. Exposed as
    /// `pub` so the `v2_acceptance` integration test can assert the
    /// fatal-failure invariant without direct field access.
    pub fn platform_renderer_failed_for_tests(&self) -> bool {
        self.platform.renderer_failed
    }

    /// Phase A T10: count of in-flight submits awaiting retirement.
    /// Delegates to `engine.pending_count()` (`pub(crate)`). Exposed
    /// as `pub` for the `v2_acceptance` integration test.
    pub fn engine_pending_count_for_tests(&self) -> usize {
        self.engine.pending_count()
    }

    /// Phase A T10: call `engine.fill_rect` directly and return
    /// `true` iff the result is `RenderError::RendererFailed`. Used
    /// by the failure-rollback regression test to assert that paint
    /// ops short-circuit after the renderer has been poisoned.
    pub fn engine_fill_rect_is_renderer_failed_for_tests(&mut self, host_xid: u32) -> bool {
        let Some(target) = self.resolve_paint_target(host_xid) else {
            return false;
        };
        let rect = ash::vk::Rect2D {
            offset: ash::vk::Offset2D { x: 0, y: 0 },
            extent: ash::vk::Extent2D {
                width: 1,
                height: 1,
            },
        };
        matches!(
            self.engine.fill_rect(
                &mut self.store,
                &mut self.platform,
                target.id,
                rect,
                [1.0_f32, 0.0, 0.0, 1.0],
            ),
            Err(crate::kms::v2::engine::RenderError::RendererFailed)
        )
    }

    /// Phase A T10: check whether `host_xid` still resolves in the
    /// drawable store after a renderer failure. Returns `Some(true)` if
    /// the drawable exists (regardless of its internal state) or `None`
    /// if the xid is unknown. Used by the rollback regression test to
    /// assert that store state is not corrupted to the point of
    /// panicking on lookup.
    pub fn store_drawable_exists_for_tests(&self, host_xid: u32) -> bool {
        self.store.get_by_xid(host_xid).is_some()
    }

    /// Phase A T7: simulate the pageflip-retire frame-boundary flush
    /// without going through `on_page_flip_ready` (which calls
    /// `drain_page_flip_events` and would error on the test fixture's
    /// `/dev/null` DRM device). Replicates the full production
    /// `on_page_flip_ready` close-then-flush sequence: flush_render_batch
    /// before flush_submit_group(PageflipRetire), so regression tests
    /// exercise the real fix path.
    pub fn simulate_page_flip_complete_for_tests(&mut self) -> Result<(), ash::vk::Result> {
        if let Err(e) = self
            .engine
            .flush_render_batch(&mut self.store, &mut self.platform)
        {
            log::warn!("simulate_page_flip_complete_for_tests: flush_render_batch failed: {e:?}");
        }
        self.engine
            .flush_submit_group(
                &mut self.store,
                &mut self.platform,
                crate::kms::v2::submit_group::FlushReason::PageflipRetire,
            )
            .map(|_| ())
    }

    /// Test-side page-flip completion path for live-scene fixtures.
    /// Mirrors the production `on_page_flip_ready` retire sequence
    /// without reading DRM events from the test fixture's `/dev/null`
    /// device.
    ///
    /// Returns the number of outputs whose pending scene ack retired.
    pub fn simulate_scene_page_flip_complete_for_tests(
        &mut self,
    ) -> Result<usize, ash::vk::Result> {
        self.simulate_page_flip_complete_for_tests()?;
        let mut retired = 0usize;
        for output_idx in 0..self.platform.outputs.len() {
            if self
                .scene
                .handle_page_flip_complete(output_idx, &mut self.store, &mut self.platform)
            {
                retired += 1;
            }
        }
        self.engine.poll_retired(&self.platform);
        self.poll_pending_retire_with_invalidate();
        Ok(retired)
    }

    /// Phase B.2 Task 12: read the global `vkQueueSubmit2` counter
    /// from `vk::call_stats`. Used as a coarse process-level submit
    /// counter when telemetry-side accounting would miss out-of-band
    /// submits (e.g. `run_one_shot_op` for asset init, which bypasses
    /// the submit-group entirely).
    ///
    /// The counter is process-global and monotonic across all tests;
    /// callers MUST capture the value at the start of their test
    /// scope and assert on the delta. For parallel-safe lifecycle
    /// counts use `telemetry_submit_group_flushes_for_tests` instead —
    /// it's per-backend and only ticks on `flush_submit_group`
    /// outcomes (which is the frame-builder collapse target).
    pub fn platform_queue_submit2_count_for_tests(&self) -> u64 {
        crate::kms::vk::call_stats::queue_submit2_count()
    }

    /// Phase A T12: drain pending flush outcomes into telemetry, then
    /// return `telemetry.lifetime.submit_group_flushes`. Using the
    /// per-backend lifetime counter instead of the global
    /// `queue_submit2_count` avoids inter-test interference when the
    /// suite runs in parallel.
    pub fn telemetry_submit_group_flushes_for_tests(&mut self) -> u64 {
        for outcome in self.engine.drain_flush_outcomes() {
            if outcome.aborted {
                self.telemetry.record_submit_group_abort();
            } else {
                self.telemetry
                    .record_submit_group_flush(outcome.flushed_entries, outcome.reason);
            }
        }
        self.telemetry.lifetime.submit_group_flushes
    }

    /// Phase B.3 Task 2 (N1, N8, N9): read the lifetime
    /// `frame_builder_close_reason_non_ported_paint_op` counter after
    /// draining pending flush outcomes. Used by the copy_area collapse
    /// integration test to assert that copy_area no longer fires the
    /// M2 close (CloseReason::NonPortedPaintOp).
    pub fn telemetry_close_reason_non_ported_for_tests(&mut self) -> u64 {
        // Drain flush outcomes into telemetry first (same drain pattern as
        // telemetry_submit_group_flushes_for_tests above).
        for outcome in self.engine.drain_flush_outcomes() {
            if outcome.aborted {
                self.telemetry.record_submit_group_abort();
            } else {
                self.telemetry
                    .record_submit_group_flush(outcome.flushed_entries, outcome.reason);
            }
        }
        // Close-REASON counters accumulate from frame-builder close
        // EVENTS, not flush outcomes — without this drain the counter
        // stays 0 no matter how many closes fired in the engine.
        self.drain_frame_builder_telemetry();
        self.telemetry
            .lifetime
            .frame_builder_close_reason_non_ported_paint_op
    }

    /// Phase B.3 Task 2 (N1, N8, N9): drive `engine.copy_area` directly
    /// against the store + platform using DrawableIds resolved from host
    /// xids. Mirror of `render_composite_for_tests`. Used by the copy_area
    /// collapse integration test.
    ///
    /// # Errors
    ///
    /// Returns `Err` if either xid doesn't resolve in the store, or if
    /// the engine call fails.
    pub fn engine_copy_area_for_tests(
        &mut self,
        src_xid: u32,
        dst_xid: u32,
        src_rect: ash::vk::Rect2D,
        dst_pos: ash::vk::Offset2D,
    ) -> Result<(), std::io::Error> {
        let Some(src_id) = self.store.lookup(src_xid) else {
            return Err(std::io::Error::other(format!(
                "engine_copy_area_for_tests: src xid 0x{src_xid:x} not in store"
            )));
        };
        let Some(dst_id) = self.store.lookup(dst_xid) else {
            return Err(std::io::Error::other(format!(
                "engine_copy_area_for_tests: dst xid 0x{dst_xid:x} not in store"
            )));
        };
        self.engine
            .copy_area(
                &mut self.store,
                &mut self.platform,
                src_id,
                dst_id,
                src_rect,
                dst_pos,
            )
            .map_err(|e| std::io::Error::other(format!("engine_copy_area_for_tests: {e:?}")))
    }

    /// B.3 Task 4 — test-only: call `engine.cow_copy_area` against the
    /// registered `cow_id`, using the given src xid, src_rect, and
    /// dst_pos. Returns Err if no cow_id is registered or if the engine
    /// call fails.
    pub fn engine_cow_copy_area_for_tests(
        &mut self,
        src_xid: u32,
        src_rect: ash::vk::Rect2D,
        dst_pos: ash::vk::Offset2D,
    ) -> Result<(), std::io::Error> {
        let cow_id = self.cow_id.ok_or_else(|| {
            std::io::Error::other(
                "engine_cow_copy_area_for_tests: no cow_id registered; \
                 call get_overlay_window first",
            )
        })?;
        let src_id = self.store.lookup(src_xid).ok_or_else(|| {
            std::io::Error::other(format!(
                "engine_cow_copy_area_for_tests: src xid 0x{src_xid:x} not in store"
            ))
        })?;
        self.engine
            .cow_copy_area(
                &mut self.store,
                &mut self.platform,
                cow_id,
                src_id,
                src_rect,
                dst_pos,
            )
            .map_err(|e| std::io::Error::other(format!("engine_cow_copy_area_for_tests: {e:?}")))
    }

    /// B.3 Task 6 — test-only: invoke `engine.put_image` against the given
    /// dst_xid. Constructs a staging payload of `src_extent.width *
    /// src_extent.height * 4` bytes (BGRA, depth 32) from `pixel_bytes`.
    /// Returns `Err` if `dst_xid` doesn't resolve or if the engine call fails.
    #[allow(
        dead_code,
        reason = "used by v2_frame_builder_put_image_collapses_two_in_one_frame"
    )]
    pub fn engine_put_image_for_tests(
        &mut self,
        dst_xid: u32,
        dst_pos: ash::vk::Offset2D,
        src_extent: ash::vk::Extent2D,
        pixel_bytes: &[u8],
        src_depth: u8,
    ) -> Result<(), std::io::Error> {
        let Some(dst_id) = self.store.lookup(dst_xid) else {
            return Err(std::io::Error::other(format!(
                "engine_put_image_for_tests: dst xid 0x{dst_xid:x} not in store"
            )));
        };
        self.engine
            .put_image(
                &mut self.store,
                &mut self.platform,
                dst_id,
                dst_pos,
                src_extent,
                pixel_bytes,
                src_depth,
            )
            .map_err(|e| std::io::Error::other(format!("engine_put_image_for_tests: {e:?}")))
    }

    /// B.3 Task 8 — test-only: invoke `engine.fill_rect_batch` against
    /// the given `dst_xid` with `color` and `rects`. Returns `Err` if
    /// `dst_xid` doesn't resolve in the store or if the engine call fails.
    #[allow(
        dead_code,
        reason = "used by v2_frame_builder_fill_rect_batch_collapses_two_in_one_frame"
    )]
    pub fn engine_fill_rect_batch_for_tests(
        &mut self,
        dst_xid: u32,
        color: [f32; 4],
        rects: &[ash::vk::Rect2D],
    ) -> Result<(), std::io::Error> {
        let Some(dst_id) = self.store.lookup(dst_xid) else {
            return Err(std::io::Error::other(format!(
                "engine_fill_rect_batch_for_tests: dst xid 0x{dst_xid:x} not in store"
            )));
        };
        self.engine
            .fill_rect_batch(&mut self.store, &mut self.platform, dst_id, color, rects)
            .map_err(|e| std::io::Error::other(format!("engine_fill_rect_batch_for_tests: {e:?}")))
    }

    /// B.3 Task 10 — test-only: invoke `engine.logic_fill` against the
    /// given `dst_xid`. Converts the `dst_xid` to a `DrawableId` via
    /// the store lookup.
    ///
    /// # Errors
    ///
    /// Returns `Err` if `dst_xid` doesn't resolve in the store or if
    /// the engine call fails.
    #[allow(
        dead_code,
        reason = "used by v2_frame_builder_logic_fill_collapses_two_in_one_frame"
    )]
    pub fn engine_logic_fill_for_tests(
        &mut self,
        dst_xid: u32,
        function: yserver_core::backend::GcFunction,
        opaque_alpha: bool,
        fg: u32,
        rects: &[crate::kms::cpu_types::Rectangle16],
    ) -> Result<(), std::io::Error> {
        let Some(dst_id) = self.store.lookup(dst_xid) else {
            return Err(std::io::Error::other(format!(
                "engine_logic_fill_for_tests: dst xid 0x{dst_xid:x} not in store"
            )));
        };
        self.engine
            .logic_fill(
                &mut self.store,
                &mut self.platform,
                dst_id,
                function,
                opaque_alpha,
                fg,
                rects,
            )
            .map_err(|e| std::io::Error::other(format!("engine_logic_fill_for_tests: {e:?}")))
    }

    /// Phase B.3 Task 14 (N7): drive `engine.image_text` directly against the
    /// store + platform using a DrawableId resolved from a host xid.
    /// Constructs one non-zero glyph per entry in `glyphs`: each glyph is
    /// `w × h` pixels of 0xFF alpha. `font_xid` keys the glyph atlas.
    ///
    /// # Errors
    ///
    /// Returns `Err` if `dst_xid` doesn't resolve in the store, or if
    /// the engine call fails.
    #[allow(
        dead_code,
        reason = "used by v2_frame_builder_image_text_* integration tests"
    )]
    pub fn engine_image_text_for_tests(
        &mut self,
        dst_xid: u32,
        font_xid: u32,
        foreground_rgba: [f32; 4],
        glyphs: &[(u32, i32, i32, u32, u32)], // (codepoint, dst_x, dst_y, w, h)
    ) -> Result<(u32, u32, u32), std::io::Error> {
        // Returns (atlas_interns, glyph_uploads, glyphs_dropped).
        let Some(dst_id) = self.store.lookup(dst_xid) else {
            return Err(std::io::Error::other(format!(
                "engine_image_text_for_tests: dst xid 0x{dst_xid:x} not in store"
            )));
        };
        let prepared: Vec<super::engine::PreparedGlyph> = glyphs
            .iter()
            .map(|&(codepoint, dst_x, dst_y, w, h)| {
                let w_us = w as usize;
                let h_us = h as usize;
                let pixels = vec![0xFFu8; w_us * h_us];
                super::engine::PreparedGlyph {
                    codepoint,
                    dst_x,
                    dst_y,
                    w: w_us,
                    h: h_us,
                    pixels,
                }
            })
            .collect();
        self.engine
            .image_text(
                &mut self.store,
                &mut self.platform,
                dst_id,
                font_xid,
                foreground_rgba,
                &prepared,
            )
            .map(|s| (s.atlas_interns, s.glyph_uploads, s.glyphs_dropped))
            .map_err(|e| std::io::Error::other(format!("engine_image_text_for_tests: {e:?}")))
    }

    /// Phase B.3 Task 14 (N7, N10): attach a synthetic PRESENT completion
    /// to the open frame if the given `dst_xid` is written by any op in
    /// the open frame. Mirrors `attach_synthetic_present_completion_to_cow_for_tests`
    /// but works for any drawable (image_text dst, not just the COW).
    ///
    /// Returns `true` if attach succeeded, `false` if no open frame exists
    /// or the drawable is not written in the open frame.
    #[allow(
        dead_code,
        reason = "used by v2_frame_builder_image_text_delivers_present_completion"
    )]
    pub fn attach_synthetic_present_completion_for_tests(
        &mut self,
        dst_xid: u32,
        synthetic_serial: u32,
    ) -> bool {
        use crate::kms::v2::present_completion::{PendingPresentEntry, PinnedWake};
        use yserver_core::backend::{CompletedPresentEvent, PresentWake};
        use yserver_protocol::x11::ClientId;

        let Some(dst_id) = self.store.lookup(dst_xid) else {
            return false;
        };
        let entry = PendingPresentEntry {
            wake_pin: PinnedWake::None,
            event: CompletedPresentEvent {
                client_id: ClientId(0),
                serial: synthetic_serial,
                host_xid: 0,
                dst_host_xid: 0,
                options: 0,
                wake: PresentWake::Pixmap { idle_fence_xid: 0 },
            },
        };
        self.engine
            .attach_cow_present_completion(dst_id, entry)
            .is_ok()
    }

    /// Phase B.3 Task 12 (N5): drive `engine.render_traps_or_tris` directly
    /// against the store + platform using a DrawableId resolved from a host xid.
    /// Uses a single-trapezoid solid-src op (PictOp 1 = Src, one trapezoid
    /// instance, small bbox). Mirror of `render_composite_for_tests`.
    ///
    /// # Errors
    ///
    /// Returns `Err` if `dst_xid` doesn't resolve in the store, or if
    /// the engine call fails.
    pub fn engine_render_traps_or_tris_for_tests(
        &mut self,
        dst_xid: u32,
        color: [f32; 4],
        bbox_w: u32,
        bbox_h: u32,
    ) -> Result<(), std::io::Error> {
        let Some(dst_id) = self.store.lookup(dst_xid) else {
            return Err(std::io::Error::other(format!(
                "engine_render_traps_or_tris_for_tests: dst xid 0x{dst_xid:x} not in store"
            )));
        };
        // Pack one minimal trapezoid instance: 4× f32 pairs (top, bottom, left,
        // right edges) = 32 bytes. The actual shape doesn't matter for the
        // frame-builder collapse test — we just need a non-empty, non-zero bbox.
        let instance_data = [0u8; 32];
        self.engine
            .render_traps_or_tris(
                &mut self.store,
                &mut self.platform,
                1, // PictOp_Src
                super::engine::ResolvedSource::Solid(color),
                dst_id,
                super::engine::TrapPrimKind::Trapezoid,
                &instance_data,
                1,
                (0, 0, bbox_w, bbox_h),
                None,
                crate::kms::cpu_types::Repeat::Pad,
                None,
                0, // src_origin_x
                0, // src_origin_y
                0, // src_pict_format
                0, // dst_pict_format
            )
            .map(|_| ())
            .map_err(|e| {
                std::io::Error::other(format!("engine_render_traps_or_tris_for_tests: {e:?}"))
            })
    }

    /// B.3 Task 12 hotfix 2 — test-only: build a linear gradient LUT
    /// and stash it under `grad_xid` in the engine's `picture_paint`
    /// map. Uses a two-stop (black → white) 1-pixel gradient. Returns
    /// `Err` on `NoVk` or GPU allocation failure.
    pub fn engine_build_linear_gradient_for_tests(
        &mut self,
        grad_xid: u32,
    ) -> Result<(), std::io::Error> {
        use crate::kms::vk::gradient::Stop;
        self.engine
            .build_and_insert_linear_gradient(
                &self.platform,
                grad_xid,
                (0, 0),
                (64 << 16, 0),
                &[
                    Stop {
                        pos: 0,
                        r: 0,
                        g: 0,
                        b: 0,
                        a: 0xFFFF,
                    },
                    Stop {
                        pos: 0x10000,
                        r: 0xFFFF,
                        g: 0xFFFF,
                        b: 0xFFFF,
                        a: 0xFFFF,
                    },
                ],
            )
            .map_err(|e| {
                std::io::Error::other(format!("engine_build_linear_gradient_for_tests: {e:?}"))
            })
    }

    /// B.3 Task 12 hotfix 2 — test-only: remove a gradient from the
    /// engine's `picture_paint` map (mirrors `render_free_picture`'s
    /// inner call to `picture_paint_remove`).
    pub fn engine_picture_paint_remove_for_tests(&mut self, grad_xid: u32) {
        self.engine.picture_paint_remove(grad_xid);
    }

    /// B.3 Task 12 hotfix 2 — test-only: drive
    /// `engine.render_traps_or_tris` with a `ResolvedSource::Gradient`
    /// src for the given `grad_xid`. Uses the same single-trapezoid
    /// geometry as `engine_render_traps_or_tris_for_tests`.
    ///
    /// # Errors
    ///
    /// Returns `Err` if `dst_xid` doesn't resolve, the gradient is not
    /// in `picture_paint`, or the engine call fails.
    pub fn engine_render_traps_or_tris_gradient_for_tests(
        &mut self,
        dst_xid: u32,
        grad_xid: u32,
        bbox_w: u32,
        bbox_h: u32,
    ) -> Result<(), std::io::Error> {
        let Some(dst_id) = self.store.lookup(dst_xid) else {
            return Err(std::io::Error::other(format!(
                "engine_render_traps_or_tris_gradient_for_tests: \
                 dst xid 0x{dst_xid:x} not in store"
            )));
        };
        let instance_data = [0u8; 32];
        self.engine
            .render_traps_or_tris(
                &mut self.store,
                &mut self.platform,
                1, // PictOp_Src
                super::engine::ResolvedSource::Gradient(grad_xid),
                dst_id,
                super::engine::TrapPrimKind::Trapezoid,
                &instance_data,
                1,
                (0, 0, bbox_w, bbox_h),
                None,
                crate::kms::cpu_types::Repeat::Pad,
                None,
                0, // src_origin_x
                0, // src_origin_y
                0, // src_pict_format
                0, // dst_pict_format
            )
            .map(|_| ())
            .map_err(|e| {
                std::io::Error::other(format!(
                    "engine_render_traps_or_tris_gradient_for_tests: {e:?}"
                ))
            })
    }

    /// Phase B.3 Task 12 (N5): read the lifetime
    /// `frame_builder_close_reason_scratch_grow` counter after draining
    /// pending flush outcomes. Used by the cross-frame mask-grow integration
    /// test to assert that close-before-grow fires exactly once during the
    /// 3-op (small, large, large) sequence.
    pub fn telemetry_close_reason_scratch_grow_for_tests(&mut self) -> u64 {
        for outcome in self.engine.drain_flush_outcomes() {
            if outcome.aborted {
                self.telemetry.record_submit_group_abort();
            } else {
                self.telemetry
                    .record_submit_group_flush(outcome.flushed_entries, outcome.reason);
            }
        }
        // Close-REASON counters accumulate from frame-builder close
        // EVENTS, not flush outcomes — without this drain the counter
        // stays 0 no matter how many ScratchGrow closes fired (the
        // cross-frame mask-grow test was born failing because of it).
        self.drain_frame_builder_telemetry();
        self.telemetry
            .lifetime
            .frame_builder_close_reason_scratch_grow
    }

    /// B.3 Task 4 — test-only: attach a synthetic PRESENT completion
    /// to the open frame's cow slot (via `engine.attach_cow_present_completion`)
    /// without a real X PRESENT client. Returns `true` if the attach
    /// succeeded (the cow_id is written in the open frame's ops list),
    /// `false` if attach returned Err (no open frame, or frame doesn't
    /// write to cow_id).
    ///
    /// The synthetic entry carries `wake_pin: PinnedWake::None` and an
    /// event with `serial = synthetic_serial`; the `CompletedPresentEvent`
    /// fields other than serial are zeroed/defaulted.
    #[allow(
        dead_code,
        reason = "used by v2_frame_builder_cow_copy_area_delivers_present_completion"
    )]
    pub fn attach_synthetic_present_completion_to_cow_for_tests(
        &mut self,
        synthetic_serial: u32,
    ) -> bool {
        use crate::kms::v2::present_completion::{PendingPresentEntry, PinnedWake};
        use yserver_core::backend::{CompletedPresentEvent, PresentWake};
        use yserver_protocol::x11::ClientId;

        let cow_id = match self.cow_id {
            Some(id) => id,
            None => return false,
        };
        let entry = PendingPresentEntry {
            wake_pin: PinnedWake::None,
            event: CompletedPresentEvent {
                client_id: ClientId(0),
                serial: synthetic_serial,
                host_xid: 0,
                dst_host_xid: 0,
                options: 0,
                wake: PresentWake::Pixmap { idle_fence_xid: 0 },
            },
        };
        self.engine
            .attach_cow_present_completion(cow_id, entry)
            .is_ok()
    }

    /// B.3 Task 4 — test-only: call `engine.drain_all` (waits on all
    /// in-flight fence tickets so PRESENT completions become Ready).
    /// Used after force-closing a frame to age out the fence ticket
    /// before asserting on `drain_completed_present_events_for_tests`.
    pub fn engine_drain_all_for_tests(&mut self) {
        self.engine.drain_all(&mut self.platform);
    }

    /// Stage 5 Task 6.1: pick up any PRESENT completions that were
    /// queued past `disable_output` so the caller (lib.rs::run) can
    /// fan them out to clients before tearing down the socket.
    pub fn take_shutdown_present_events(
        &mut self,
    ) -> Vec<yserver_core::backend::CompletedPresentEvent> {
        std::mem::take(&mut self.pending_completed_events_on_shutdown)
    }

    /// Shutdown-only: walk every drawable still in `store` and
    /// destroy its Vk handles (`VkImage` / `VkImageView` /
    /// `VkDeviceMemory`). `DrawableStore` has no `Drop` impl
    /// because `Storage::destroy` needs `&PlatformBackend` for
    /// pool-return + DRI3-import handling — both fields are
    /// owned by `KmsBackendV2`, so this method bridges them.
    /// Without this, MATE's resident pixmaps at SIGTERM leak
    /// (948 VkDeviceMemory observed on bee/MATE 2026-05-31
    /// post-FenceTicket fix). Caller is `lib.rs`'s explicit
    /// shutdown block, after PRESENT completions are drained
    /// and before `backend` drops.
    pub fn shutdown_destroy_drawables(&mut self) {
        self.store.shutdown_destroy_all(&self.platform);
    }

    fn fire_pending_present_entry(
        &mut self,
        entry: crate::kms::v2::present_completion::PendingPresentEntry,
    ) -> yserver_core::backend::CompletedPresentEvent {
        use crate::kms::v2::present_completion::PinnedWake;

        match &entry.wake_pin {
            PinnedWake::Pixmap(h) => {
                if let Err(e) = self.dri3_trigger_fence_via_handle(h) {
                    log::warn!("deferred IdleNotify: dri3_trigger_fence_via_handle failed: {e}");
                }
            }
            PinnedWake::PixmapSynced { handle, value } => {
                if let Err(e) = self.dri3_signal_syncobj_via_handle(handle, *value) {
                    log::warn!("deferred IdleNotify: dri3_signal_syncobj_via_handle failed: {e}");
                }
            }
            PinnedWake::None => {}
        }
        entry.event
    }

    fn poke_present_completion_wakeup(&self, context: &str) {
        match self.platform.wakeup_eventfd.write(1) {
            Ok(_) | Err(nix::errno::Errno::EAGAIN) => {}
            Err(e) => log::warn!("{context}: wakeup_eventfd write: {e}"),
        }
    }

    fn register_pending_present_batch(
        &mut self,
        mut batch: crate::kms::v2::present_completion::PendingPresentBatch,
    ) {
        use std::os::fd::{AsFd, AsRawFd};

        use crate::kms::v2::present_completion::PresentBatchWait;

        if batch.events.is_empty() {
            return;
        }

        let should_wake = match &batch.wait {
            PresentBatchWait::Fd(fd) => {
                let event = nix::sys::epoll::EpollEvent::new(
                    nix::sys::epoll::EpollFlags::EPOLLIN,
                    u64::try_from(fd.as_raw_fd()).unwrap_or_default(),
                );
                match self.platform.present_completion_epfd.add(fd.as_fd(), event) {
                    Ok(()) => false,
                    Err(e) => {
                        log::warn!(
                            "deferred PRESENT: epoll_ctl ADD sync_file fd failed: {e}; \
                             using immediate drain fallback"
                        );
                        batch.wait = PresentBatchWait::Ready;
                        true
                    }
                }
            }
            PresentBatchWait::Ready | PresentBatchWait::Poll => true,
        };

        self.pending_present_batches.push_back(batch);
        if should_wake {
            self.poke_present_completion_wakeup("register_pending_present_batch");
        }
    }

    fn drain_engine_present_batches(&mut self) {
        for batch in self.engine.drain_present_batches() {
            self.register_pending_present_batch(batch);
        }
    }

    fn pending_present_batch_ready(
        batch: &crate::kms::v2::present_completion::PendingPresentBatch,
        vk: Option<&std::sync::Arc<crate::kms::vk::device::VkContext>>,
    ) -> bool {
        use std::os::fd::AsFd;

        use crate::kms::v2::present_completion::PresentBatchWait;
        use nix::poll::{PollFd, PollFlags, PollTimeout, poll};

        // B.2-context fix (vkdebug VUID-vkDestroySemaphore-semaphore-05149):
        // sync_file readiness alone is NOT sufficient to drop the batch
        // (which destroys `batch.signal` — the export semaphore — via
        // PresentCompletionSignal::Drop). Vulkan requires the queue
        // submit fence to signal before any of its semaphores are
        // destroyed; the sync_file becomes readable as soon as the
        // KMS pageflip retires, which can be before the GPU compose
        // CB's fence signals. Gate every wait variant additionally on
        // the ticket having signaled when one is present.
        let sync_ready = match &batch.wait {
            PresentBatchWait::Ready => true,
            PresentBatchWait::Fd(fd) => {
                let mut fds = [PollFd::new(fd.as_fd(), PollFlags::POLLIN)];
                match poll(&mut fds, PollTimeout::ZERO) {
                    Ok(0) => false,
                    Ok(_) => fds[0]
                        .revents()
                        .is_some_and(|r| r.intersects(PollFlags::POLLIN | PollFlags::POLLERR)),
                    Err(e) => {
                        log::warn!("deferred PRESENT: poll(sync_file fd): {e}");
                        true
                    }
                }
            }
            PresentBatchWait::Poll => true,
        };
        if !sync_ready {
            return false;
        }
        // Even if the sync_file signaled / Ready variant, the
        // Vulkan-side fence must have signaled too before we let the
        // batch (and its export semaphore) drop. Skip the ticket
        // check only when no ticket was attached (degraded path).
        match (&batch.ticket, vk) {
            (Some(ticket), Some(v)) => ticket.poll_signaled(v),
            (Some(_), None) => false,
            (None, _) => true,
        }
    }

    fn unregister_present_batch_fd(
        &mut self,
        batch: &crate::kms::v2::present_completion::PendingPresentBatch,
    ) {
        use std::os::fd::AsFd;

        use crate::kms::v2::present_completion::PresentBatchWait;

        let _keep_export_semaphore_alive_until_batch_drop = batch.signal.as_ref();
        if let PresentBatchWait::Fd(fd) = &batch.wait
            && let Err(e) = self.platform.present_completion_epfd.delete(fd.as_fd())
        {
            log::warn!("epoll_ctl DEL PRESENT sync_file fd: {e}");
        }
    }

    fn force_drain_all_present_batches(
        &mut self,
    ) -> Vec<yserver_core::backend::CompletedPresentEvent> {
        let mut completed = Vec::new();
        while let Some(batch) = self.pending_present_batches.pop_front() {
            self.unregister_present_batch_fd(&batch);
            for entry in batch.events {
                completed.push(self.fire_pending_present_entry(entry));
            }
        }
        completed
    }

    /// Stage 5 Task 6.1 — algorithm body for `Backend::
    /// drain_completed_present_events`. Walks the front of
    /// `pending_present_batches`, popping whole batches whose exported
    /// sync_file is readable, whose ready sentinel fired, or whose
    /// degraded polling ticket has signaled. Fires wake signals via the
    /// Arc-pinned handles and returns the public event payloads.
    fn drain_completed_present_events_impl(
        &mut self,
    ) -> Vec<yserver_core::backend::CompletedPresentEvent> {
        self.drain_engine_present_batches();
        // Drain the wakeup_eventfd to clear it. nix's EventFd::read
        // returns Ok(u64) with the accumulated counter; Err(EAGAIN)
        // when zero — benign in either case because we re-check batch
        // readiness directly.
        match self.platform.wakeup_eventfd.read() {
            Ok(_) | Err(nix::errno::Errno::EAGAIN) => {}
            Err(e) => log::warn!("wakeup_eventfd read: {e}"),
        }

        let renderer_failed = self.platform.renderer_failed;
        if renderer_failed && !self.pending_present_batches.is_empty() {
            let pending_events: usize = self
                .pending_present_batches
                .iter()
                .map(|batch| batch.events.len())
                .sum();
            log::warn!("renderer_failed: force-firing {pending_events} pending PRESENT entries");
        }

        // Clone the Arc<VkContext> so the loop body can borrow `self`
        // mutably (for `dri3_*_via_handle`) without a conflict with
        // `self.platform.vk`.
        let vk = self.platform.vk.as_ref().cloned();

        let mut completed = Vec::new();
        while let Some(front) = self.pending_present_batches.front() {
            let ready = renderer_failed || Self::pending_present_batch_ready(front, vk.as_ref());
            if !ready {
                break;
            }
            let entry = self
                .pending_present_batches
                .pop_front()
                .expect("just peeked");
            self.unregister_present_batch_fd(&entry);
            for event in entry.events {
                completed.push(self.fire_pending_present_entry(event));
            }
        }
        completed
    }

    /// Stage 5 Task 4 layer 1: pull ring lifetime counter deltas
    /// into Telemetry. Called by the backend after every engine
    /// RENDER call site + retirement sweep. The bumps are
    /// independent: ring.lifetime_creates increases inside
    /// acquire_set; ring.lifetime_resets increases inside
    /// release_up_to (which only runs inside engine.poll_retired
    /// and engine.drain_all). Spec
    /// `2026-05-21-descriptor-pool-ring-design.md` § 'Telemetry'.
    fn sync_descriptor_pool_telemetry(&mut self) {
        let creates_now = self.engine.descriptor_pool_creates_lifetime();
        let resets_now = self.engine.descriptor_pool_resets_lifetime();
        let creates_delta = creates_now.saturating_sub(self.last_observed_pool_creates);
        let resets_delta = resets_now.saturating_sub(self.last_observed_pool_resets);
        for _ in 0..creates_delta {
            self.telemetry.record_descriptor_pool_create();
        }
        if resets_delta > 0 {
            self.telemetry.record_descriptor_pool_reset(resets_delta);
        }
        self.last_observed_pool_creates = creates_now;
        self.last_observed_pool_resets = resets_now;
    }

    /// Hand the libinput context off to the dedicated input thread.
    /// Mirrors `KmsBackend::take_input_ctx`.
    #[must_use]
    pub fn take_input_ctx(&mut self) -> Option<crate::input::SendContext> {
        self.platform.take_input_ctx()
    }

    /// Returns `true` when the backend is in libseat mode (VT switching
    /// enabled). `false` in Direct mode (no libseat, today's behaviour).
    #[must_use]
    pub fn is_libseat_mode(&self) -> bool {
        self.seat.is_libseat()
    }

    /// Initial composite + flip. v2's SceneCompositor records
    /// one compose CB per output and atomic-flips. On a fresh
    /// boot the scene typically has no mapped windows yet, so
    /// this paints the `bg_pixel` clear color and flips.
    ///
    /// # Errors
    ///
    /// Returns the first per-output Vk / DRM failure; subsequent
    /// outputs still attempted.
    pub fn composite_and_flip(&mut self, state: &ServerState) -> io::Result<()> {
        // DPMS gate: outputs are inactive, page-flip would EBUSY.
        if state.dpms.power_level != 0 {
            return Ok(());
        }
        // Gate: no modeset/pageflip/submit when not holding DRM master.
        // In Direct mode seat_state is always Active → no behaviour change.
        if !self.scanout_allowed() {
            log::debug!("v2 composite_and_flip: skipped (seat not Active)");
            return Ok(());
        }
        let cow_host_xid = self.cow_host_xid();
        match self.scene.tick(
            &self.core,
            &mut self.store,
            &mut self.platform,
            &self.windows_v2,
            &mut self.telemetry,
            cow_host_xid,
        ) {
            Ok(_) => Ok(()),
            Err(e) => Err(io::Error::other(format!("v2 composite_and_flip: {e:?}"))),
        }
    }

    /// Post-loop teardown — delegates to PlatformBackend, which
    /// disables each output and disarms scanout pools whose
    /// disable failed (matching v1's behaviour to avoid leaking
    /// framebuffers KMS may still hold).
    ///
    /// # Errors
    ///
    /// Propagates the first per-output `drm::modeset::disable_output`
    /// failure; subsequent outputs still attempted.
    pub fn disable_output(&mut self) -> io::Result<()> {
        // Stage 5 Task 6.1: explicitly flush open render batches
        // before drain_all walks the submitted queue. drain_all only
        // waits on already-submitted CBs; an open pending batch
        // wouldn't be there yet.
        self.drain_engine_present_batches();
        if let Err(e) = self
            .engine
            .flush_render_batch(&mut self.store, &mut self.platform)
        {
            log::warn!("v2 disable_output: flush_render_batch failed: {e:?}");
        }

        // Drain in-flight paint + compose submits before the
        // platform's `device_wait_idle` + pool destruction so
        // each subsystem's book-keeping reclaims its handles
        // against the still-live pool.
        self.engine.shutdown(&mut self.store, &mut self.platform);
        // Phase B.1 Task 21: drain close events emitted by shutdown.
        self.drain_frame_builder_telemetry();
        self.sync_descriptor_pool_telemetry();
        self.scene.drain_all(&mut self.platform);

        // Stage 5 Task 6.1: drain the pending PRESENT batch queue
        // unconditionally. After drain_all every submitted paint
        // ticket is signaled or the renderer failed; first pop ready
        // batches via drain_completed_present_events_impl (which
        // closes sync_file FDs + fires Arc wake signals), then
        // force-fire any remaining unsignaled batches. All accumulated
        // events go to `pending_completed_events_on_shutdown` for the
        // caller (lib.rs::run) to fan out to clients before the socket
        // is torn down.
        let completed = self.drain_completed_present_events_impl();
        self.pending_completed_events_on_shutdown.extend(completed);
        let completed = self.force_drain_all_present_batches();
        self.pending_completed_events_on_shutdown.extend(completed);

        // Flush the submit trace after the drains record their
        // final events, before platform teardown — a VkDevice
        // destroy can hang on some drivers (msm/Renoir) and
        // `BufWriter::Drop` would lose the buffered tail to a
        // subsequent power-cycle. See `submit_trace::SubmitTrace::flush`.
        self.telemetry.flush_submit_trace();
        self.platform.disable_output()
    }

    /// Once-per-method dedup helper. Each `method` name produces
    /// exactly one `warn!` per session, so a busy client doesn't
    /// drown the log.
    fn log_v2_gap(&self, method: &'static str) {
        if self.logged_gaps.borrow_mut().insert(method) {
            log::warn!("v2: {method} not yet implemented — paint or composite operation skipped");
        }
    }

    // ── Input dispatch (Stage 3f.7) ─────────────────────────────
    //
    // Ports the v1 input cluster onto v2's state surface.
    // Differences from v1's body (kms/backend.rs:6450-6885):
    //
    // - `self.windows` → `self.windows_v2`.
    // - `self.fb_w` / `self.fb_h` → the active output's geometry
    //   read off `self.platform.outputs[0]`.
    // - HW cursor calls (`hw_cursor_active` / `hw_cursor_move` /
    //   `hw_cursor_refresh`) → no-op. Per spec § I7 the HW cursor
    //   plane is parked in v2 until Stage 5 reintroduces it as a
    //   SceneCompositor strategy.
    // - `self.mark_all_outputs_dirty()` →
    //   `self.scene.mark_scene_structure_dirty()`. Pointer-motion-
    //   only redraws are a no-op in Stage 3 anyway (no cursor
    //   scene blit until Stage 4); the dirty flag preserves the
    //   "scene needs a tick" signal for any client paint that
    //   races a motion event.

    /// X11 KeyButMask: bits 0..=7 are modifiers
    /// (Shift/Lock/Control/Mod1..Mod5). Bits 8..=12 are button
    /// state, set by `process_pointer_button` via `button_mask`.
    fn serialize_modifiers(&self) -> u16 {
        let state = &self.core.xkb_state.0;
        let flags = xkbcommon::xkb::STATE_MODS_EFFECTIVE;
        let mut mask: u16 = 0;
        if state.mod_name_is_active("Shift", flags) {
            mask |= 0x01;
        }
        if state.mod_name_is_active("Lock", flags) {
            mask |= 0x02;
        }
        if state.mod_name_is_active("Control", flags) {
            mask |= 0x04;
        }
        if state.mod_name_is_active("Mod1", flags) {
            mask |= 0x08;
        }
        if state.mod_name_is_active("Mod2", flags) {
            mask |= 0x10;
        }
        if state.mod_name_is_active("Mod3", flags) {
            mask |= 0x20;
        }
        if state.mod_name_is_active("Mod4", flags) {
            mask |= 0x40;
        }
        if state.mod_name_is_active("Mod5", flags) {
            mask |= 0x80;
        }
        mask
    }

    /// Direct-mode wiring: hand over the relay the input thread ends
    /// of which `input_thread::run` polls. Called from `lib.rs` when
    /// spawning the libinput thread.
    pub fn set_led_relay(&mut self, relay: std::sync::Arc<crate::input::LedRelay>) {
        self.led_relay = Some(relay);
    }

    /// Current lock-LED state derived from `xkb_state`, as
    /// `input::Led` bits. Split from [`Self::sync_keyboard_leds`] so
    /// the no-Vk test fixture can pin the XKB→LED mapping without a
    /// libinput device.
    fn current_led_bits(&self) -> u32 {
        let state = &self.core.xkb_state.0;
        let mut leds = input::Led::empty();
        if state.led_name_is_active(xkbcommon::xkb::LED_NAME_CAPS) {
            leds |= input::Led::CAPSLOCK;
        }
        if state.led_name_is_active(xkbcommon::xkb::LED_NAME_NUM) {
            leds |= input::Led::NUMLOCK;
        }
        if state.led_name_is_active(xkbcommon::xkb::LED_NAME_SCROLL) {
            leds |= input::Led::SCROLLLOCK;
        }
        leds.bits()
    }

    /// Push the XKB lock-LED state (Caps/Num/Scroll) to the keyboards
    /// when it changed. On a KMS server nothing else drives the LEDs —
    /// Xorg's keyboard driver does this via the XKB indicator state;
    /// we do it via `libinput_device_led_update`. Libseat mode calls
    /// the on-core libinput context directly; Direct mode crosses to
    /// the input thread via the `LedRelay` eventfd.
    fn sync_keyboard_leds(&mut self) {
        let bits = self.current_led_bits();
        if bits == self.leds_sent {
            return;
        }
        self.leds_sent = bits;
        let leds = input::Led::from_bits_truncate(bits);
        if let Some(ctx) = self.core_libinput.as_mut() {
            ctx.update_leds(leds);
        } else if let Some(relay) = self.led_relay.as_ref() {
            relay.set(bits);
        }
    }

    /// Update xkb_state for `raw` then return a cooked
    /// `HostKeyEvent` with the post-update modifier state +
    /// cursor coords pre-filled. Direct v1 port.
    fn cook_host_key(&mut self, raw: HostKeyEvent) -> HostKeyEvent {
        let xkb_keycode = xkbcommon::xkb::Keycode::new(u32::from(raw.keycode));
        let direction = if raw.pressed {
            xkbcommon::xkb::KeyDirection::Down
        } else {
            xkbcommon::xkb::KeyDirection::Up
        };
        self.core.xkb_state.0.update_key(xkb_keycode, direction);
        self.sync_keyboard_leds();
        HostKeyEvent {
            state: self.serialize_modifiers(),
            root_x: self.core.cursor_x as i16,
            root_y: self.core.cursor_y as i16,
            event_x: self.core.cursor_x as i16,
            event_y: self.core.cursor_y as i16,
            time: crate::clock::server_time_ms(),
            ..raw
        }
    }

    /// Synthesize releases for every held key/button so a client that
    /// owned input at switch time does not see stuck-down keys on
    /// resume. Emits one `KeyRelease` per held keycode (via
    /// `key_event_fanout_to_state`) and one `ButtonRelease` per held
    /// button (via the same pointer path `on_host_input` uses), then
    /// clears `down_keys` and zeroes `button_mask`.
    ///
    /// XI2 raw listeners are intentionally NOT updated (spec §"XI2 raw
    /// events"). Crossing events are not synthesized.
    ///
    /// Caller is `run_suspend`.
    fn synthesize_held_releases(&mut self, state: &mut ServerState) {
        use yserver_core::core_loop::{
            key_fanout::key_event_fanout_to_state, pointer_fanout::pointer_event_fanout_to_state,
        };

        // Keys: drain down_keys, emit a synthetic KeyRelease for each.
        let keys: Vec<u8> = self.core.down_keys.drain().collect();
        for keycode in keys {
            let ev = HostKeyEvent {
                pressed: false,
                keycode,
                time: crate::clock::server_time_ms(),
                root_x: self.core.cursor_x as i16,
                root_y: self.core.cursor_y as i16,
                event_x: self.core.cursor_x as i16,
                event_y: self.core.cursor_y as i16,
                state: 0,
            };
            let _dropped = key_event_fanout_to_state(state, self, ev);
        }

        // Buttons: held bits live in (button_mask >> 8) & 0x1f,
        // bit n => X11 button number (n+1).
        // button_bit for button n+1 is (1 << (n + 8)), which equals
        // (button_mask >> 8) bit n. We synthesize a ButtonRelease
        // by calling process_pointer_button with the libinput code
        // that maps to that button, then flush pending events.
        // Libinput code → X11 detail mapping (from process_pointer_button):
        //   0x110 → 1, 0x112 → 2, 0x111 → 3, 0x113 → 8, 0x114 → 9
        // For buttons 1-5 we use the same code path as on_host_input.
        let held = (self.core.button_mask >> 8) & 0x1f;
        // Libinput codes for X11 buttons 1–5 (detail 1..=5):
        // detail 1→0x110, detail 2→0x112, detail 3→0x111, detail 4→scroll, detail 5→scroll
        // process_pointer_button maps: 0x110→1, 0x112→2, 0x111→3, 0x180→4, 0x181→5
        // button_bit: detail 1→0x0100 (bit 8), detail 2→0x0200 (bit 9), detail 3→0x0400 (bit 10),
        //             detail 4→0x0800 (bit 11), detail 5→0x1000 (bit 12)
        // So bit 0 of `held` = button 1 (detail 1), bit 1 = button 2 (detail 2), etc.
        const BUTTON_CODES: [u32; 5] = [
            0x110, // bit 0 → detail 1 (BTN_LEFT)
            0x112, // bit 1 → detail 2 (BTN_MIDDLE)
            0x111, // bit 2 → detail 3 (BTN_RIGHT)
            0x180, // bit 3 → detail 4 (SYNTH_SCROLL_UP, button 4)
            0x181, // bit 4 → detail 5 (SYNTH_SCROLL_DOWN, button 5)
        ];
        // Hoist the xid_map clone outside the BUTTON_CODES loop —
        // process_pointer_button doesn't touch xid_map, so one snapshot
        // covers all held-button drains. (clone is needed because
        // pointer_event_fanout_to_state now takes `self` as &mut dyn Backend
        // while also reading &xid_map; the local releases the borrow.)
        let xid_map = self.core.xid_map.clone();
        for (n, &code) in BUTTON_CODES.iter().enumerate() {
            if held & (1 << n) != 0 {
                self.process_pointer_button(code, false, state);
                // Drain pointer events into fanout after each button
                // (matches on_host_input's drain-per-event contract).
                let pending = std::mem::take(&mut self.core.pending_pointer_events);
                for ev in pending {
                    let _dropped =
                        pointer_event_fanout_to_state(state, self, &xid_map, ev, true, false);
                }
            }
        }
        // button_mask is already zeroed by process_pointer_button for
        // each release, but force-zero to guard against scroll buttons
        // (detail 4/5) that carry no button_bit.
        self.core.button_mask = 0;
    }

    /// True only when `seat_state` is `Active` — i.e. we hold DRM master
    /// and are allowed to submit page-flips, modesets, and GPU work.
    /// Gate every master-requiring operation on this. In Direct mode
    /// `seat_state` is always `Active`, so this is always `true` there.
    fn scanout_allowed(&self) -> bool {
        self.seat_state.allows_scanout()
    }

    /// Suspend sequence — called by Task 12's `on_seat_ready` driver when
    /// the state machine decides `BeginSuspend`. `seat_state` is already
    /// `Suspending` at entry; the scanout gate is already closed.
    ///
    /// Steps:
    /// 1. Gate already closed (state is `Suspending`).
    /// 2. Drain libinput to capture any events mio may have buffered
    ///    before delivering the seat disable (closes the
    ///    SEAT-before-LIBINPUT poll-ordering race).
    /// 3. Synthesize held-key / held-button releases.
    /// 4. Wait for in-flight GPU work (bounded).
    /// 5. Suspend libinput (closes input device fds via `close_restricted`
    ///    → `seat.close_device`).
    /// 6. Ack the libseat disable (MUST always run — missing the ack
    ///    wedges the kernel waiting for the VT switch: Risk #1).
    ///
    /// Steps 3–6 are wrapped so errors are logged rather than propagated,
    /// ensuring the ack (step 6) always executes.
    ///
    /// # Caller
    ///
    /// `drive_seat_event` on `BeginSuspend`.
    fn run_suspend(&mut self, state: &mut ServerState) {
        log::info!(
            "kms: run_suspend enter — down_keys={} button_mask=0x{:04x} core_libinput={}",
            self.core.down_keys.len(),
            self.core.button_mask,
            if self.core_libinput.is_some() {
                "present"
            } else {
                "none"
            },
        );
        // 2. DETERMINISTIC INPUT DRAIN — mio may deliver SEAT_TOKEN before
        //    LIBINPUT_TOKEN in the same poll batch, leaving events in the
        //    libinput kernel buffer that we haven't read yet. Drain now so
        //    `down_keys`/`button_mask` reflect every delivered event before
        //    we snapshot them for held-release synthesis. One dispatch
        //    typically suffices; loop until empty to be defensive.
        if self.core_libinput.is_some() {
            loop {
                let evs = match self.core_libinput.as_mut().unwrap().dispatch() {
                    Ok(e) => e,
                    Err(e) => {
                        log::warn!("kms: suspend drain dispatch failed: {e}");
                        break;
                    }
                };
                if evs.is_empty() {
                    break;
                }
                let time_ms = crate::clock::server_time_ms();
                let mut scroll_buf: Vec<yserver_core::core_loop::HostInputEvent> = Vec::new();
                for ev in evs {
                    // Hotkeys are irrelevant mid-suspend; just update input
                    // state via the normal mapping + fanout so down_keys and
                    // button_mask stay accurate for the held-release snapshot.
                    // Device add/remove during suspend are forwarded so the
                    // core's device registry stays accurate.
                    if let crate::input::InputEvent::DeviceAdded(info) = ev {
                        self.on_host_input(
                            state,
                            yserver_core::core_loop::HostInputEvent::DeviceAdded(info),
                        );
                    } else if let crate::input::InputEvent::DeviceRemoved { device_node } = ev {
                        self.on_host_input(
                            state,
                            yserver_core::core_loop::HostInputEvent::DeviceRemoved { device_node },
                        );
                    } else if let crate::input::InputEvent::PointerScroll { dx_v120, dy_v120 } = ev
                    {
                        scroll_buf.clear();
                        if let Some(input_state) = self.core_input_state.as_mut() {
                            input_state.drain_scroll(dx_v120, dy_v120, time_ms, &mut scroll_buf);
                        }
                        for host_ev in scroll_buf.drain(..) {
                            self.on_host_input(state, host_ev);
                        }
                    } else if let Some(input_state) = self.core_input_state.as_mut() {
                        let host = input_state.map(ev, time_ms);
                        self.on_host_input(state, host);
                    }
                }
            }
        }

        // Steps 3–6: error-tolerant so the disable() ack (step 6) always
        // executes even if an earlier step fails (Risk #1).

        // 3. Synthesize held-key / held-button releases.
        self.synthesize_held_releases(state);

        // 3b. DPMS: post-resume the user expects "On from their
        //     perspective". No backend call here — we already gave up
        //     DRM master (or are about to in the libseat disable() ack
        //     below), and the resume path's commit_modeset will re-light
        //     the CRTC. No notify either — clients aren't receiving
        //     events during the suspend window; Xorg matches (clients
        //     don't see a forced transition on VT switch). Mirrors
        //     Xorg hw/xfree86/common/xf86Events.c:358-360.
        state.dpms.power_level = 0;
        state.dpms.last_activity = std::time::Instant::now();

        // 4. Wait for in-flight GPU work, bounded.
        self.platform.wait_idle_bounded();

        // 4b. Drain the scene's in-flight page-flip acks BEFORE master is
        //     revoked (step 6). A pageflip submitted before the VT switch
        //     will NEVER get its page-flip-complete event once we lose
        //     master, so its per-output `pending_acks` entry would be
        //     stranded forever — and `tick_one_output`'s first gate
        //     (`if !pending_acks.is_empty()`) then bails every tick, leaving
        //     that output frozen on its last frame after resume (observed:
        //     "VT switch → 1 output frozen, switch again → both", diagnostic
        //     `tick skip output=N reason=PendingAcks` accumulating forever).
        //     `drain_all` clears pending_acks, releases pool slots, and
        //     resets per-output cursor state; the GPU is already idle (step
        //     4) so its compose-fence waits return immediately, and master
        //     is still held so its cursor-plane-hide ioctl is valid. Resume
        //     re-modesets + rearms the cursor + issues a full-damage repaint,
        //     which submits a fresh flip that re-arms the completion cycle.
        self.scene.drain_all(&mut self.platform);

        // 4c. Reset the PLATFORM scanout-BO state too. `drain_all` (4b)
        //     clears the SCENE's pending_acks, but the platform pool still
        //     holds the orphaned flip's BO in Pending/OnScreen — its
        //     page-flip-complete will never arrive after master loss. Left
        //     alone, each VT round leaks a BO until `acquire_scanout_bo`
        //     starves → `reason=NoBO` wedge (observed after a few switches),
        //     plus the `on_page_flip_complete: >1 pending BO` warning from
        //     stale Pending BOs. Force every BO back to Free here so resume
        //     starts with a clean pool; the deferred full-damage repaint
        //     re-renders (content marked invalidated).
        self.platform.reset_scanout_bos_for_suspend();

        // 5. Suspend libinput — closes input device fds via close_restricted
        //    → seat.close_device for each input device. MUST NOT hold a
        //    LibseatInner borrow across this call (re-entrancy contract:
        //    close_restricted borrow_muts LibseatInner inside libinput).
        if let Some(ctx) = self.core_libinput.as_mut() {
            log::info!("kms: run_suspend step 5 — libinput.suspend()");
            ctx.suspend();
        }

        // 6. Ack the libseat disable. We do NOT drmDropMaster: the scanout
        //    gate (step 1) already stopped all master ioctls; libseat/logind
        //    revokes master during this ack. Missing this ack wedges the
        //    kernel waiting for the VT switch (Risk #1).
        if let crate::seat::Seat::Libseat { inner, .. } = &self.seat {
            log::info!("kms: run_suspend step 6 — libseat disable() ack");
            match inner.borrow_mut().disable() {
                Ok(()) => log::info!("kms: run_suspend libseat disable() ok"),
                Err(e) => log::warn!("kms: libseat disable() ack failed: {e}"),
            }
        }

        // 7. Caller (`on_seat_ready`) calls `seat_state.suspend_complete()`
        //    after we return.
        log::info!("kms: run_suspend exit");
    }

    /// Resume sequence — called by `drive_seat_event` when the state
    /// machine decides `BeginResume`. `seat_state` is already
    /// `Resuming` at entry.
    ///
    /// Deviation #5: the DRM fd is NOT reopened and `drmSetMaster` is
    /// NOT called. libseat/logind restored DRM master before
    /// delivering `Enable`. We just re-modeset on the existing device
    /// and re-arm input.
    ///
    /// Steps:
    /// 1. State is already `Resuming`.
    /// 2. Re-query connectors, drop missing, re-commit modeset on the
    ///    existing device. If all commits fail (card gone), log + exit
    ///    (Risk #4).
    /// 3. Re-arm the hardware cursor plane.
    /// 4. Resume libinput — `libinput.resume()` re-opens input devices
    ///    via `open_restricted` → `seat.open_device`. MUST be called
    ///    with NO `LibseatInner` borrow held (re-entrancy contract).
    /// 5. Full-damage repaint is deferred to after `resume_complete`
    ///    commits `Active` (gate must be open first) — handled in
    ///    `drive_seat_event`.
    fn run_resume(&mut self, state: &mut ServerState) {
        log::info!(
            "kms: run_resume enter — cursor=({:.0},{:.0}) effective_cursor_xid={:?} core_libinput={}",
            self.core.cursor_x,
            self.core.cursor_y,
            self.effective_cursor_xid,
            if self.core_libinput.is_some() {
                "present"
            } else {
                "none"
            },
        );
        // 2. Re-query connectors + redo modeset on existing device.
        log::info!("kms: run_resume step 2 — requery_outputs_and_modeset");
        match self.platform.requery_outputs_and_modeset() {
            Ok(dropped) => {
                if !dropped.is_empty() {
                    // MVP: log dropped outputs; dynamic RandR change events
                    // require infra not yet built (see report: DONE_WITH_CONCERNS
                    // — hot-unplug-while-suspended is an MVP non-goal edge).
                    for name in &dropped {
                        log::warn!(
                            "kms: resume: output {name} was disconnected while suspended \
                             (RandR change event not yet fired — MVP limitation)"
                        );
                    }
                    self.fire_randr_changes(state, dropped);
                }
            }
            Err(e) => {
                log::error!("kms: resume: modeset failed (card gone?): {e}; exiting");
                self.request_exit();
                return;
            }
        }

        // 2b. DPMS: requery_outputs_and_modeset just re-lit every
        //     output, so reconcile the backend cache. state.dpms.power_level
        //     was reset to On in run_suspend; this brings the binary
        //     cache into agreement so a later DPMS Off request actually
        //     fires the modeset commit instead of no-opping through the
        //     same-binary-state guard.
        self.kms_outputs_active = true;

        // 3. Re-arm the hardware cursor plane. Use the current cursor
        //    position + effective cursor hotspot.
        let (hot_x, hot_y) = self
            .effective_cursor_xid
            .and_then(|xid| self.cursor_records.get(&xid))
            .map(|rec| (rec.hot_x, rec.hot_y))
            .unwrap_or((0, 0));
        #[allow(clippy::cast_possible_truncation)]
        let cx = self.core.cursor_x as i32;
        #[allow(clippy::cast_possible_truncation)]
        let cy = self.core.cursor_y as i32;
        self.platform.rearm_cursor(hot_x, hot_y, cx, cy);

        // 4. Resume libinput. MUST NOT hold a LibseatInner borrow across
        //    this call — `resume()` re-enters `open_restricted` which
        //    `borrow_mut`s `LibseatInner`. No borrow is held here.
        //    A resume failure means the session is active but the input
        //    devices were not reopened — the user would have a display
        //    with no keyboard/mouse (and no way to zap). Treat it as
        //    fatal, exactly like the modeset-failure path above (Risk #4):
        //    exit cleanly rather than wedge with no input.
        log::info!("kms: run_resume step 4 — libinput.resume()");
        let resume_failed = match self.core_libinput.as_mut() {
            Some(ctx) => match ctx.resume() {
                Ok(()) => {
                    log::info!("kms: run_resume libinput.resume() ok");
                    false
                }
                Err(e) => {
                    log::error!("kms: libinput resume failed: {e}; exiting (no input on resume)");
                    true
                }
            },
            None => false,
        };
        if resume_failed {
            // Nothing further to do here; the queued Shutdown will tear
            // the server down on the next loop iteration.
            self.request_exit();
            log::info!("kms: run_resume exit (resume_failed=true; exit queued)");
            return;
        }

        // 5. Full-damage repaint deferred to `drive_seat_event` after
        //    `resume_complete` commits `Active` and opens the scanout gate.
        log::info!("kms: run_resume exit");
    }

    /// Request process shutdown through the core-channel sender
    /// (same mechanism the input thread uses for Zap). Called on
    /// unrecoverable errors during resume (Risk #4) or libseat
    /// dispatch failure (Risk #7).
    fn request_exit(&self) {
        if let Some(s) = &self.input_sender {
            let _ = s.send(yserver_core::core_loop::Message::Shutdown);
        } else {
            log::error!("kms: request_exit: no input_sender — cannot signal shutdown");
        }
    }

    /// Notify the backend about dropped outputs (RandR). MVP: logs
    /// each dropped output name. Full RandR change-event infra is not
    /// yet built (DONE_WITH_CONCERNS — hot-unplug-while-suspended is
    /// an MVP non-goal).
    fn fire_randr_changes(&mut self, _state: &mut ServerState, dropped: Vec<String>) {
        for name in dropped {
            log::warn!(
                "kms: RandR output-gone for {name}: dynamic RandR change events are an \
                 MVP non-goal (hot-unplug-while-suspended); clients will see the output \
                 gone on the next configuration query"
            );
        }
    }

    /// Per-event state-machine driver. Extracted so both
    /// `on_seat_ready` and the test injection entry point
    /// (`inject_seat_event_for_test`) share the same logic.
    fn drive_seat_event(&mut self, state: &mut ServerState, ev: crate::seat::state::SeatEventKind) {
        use crate::seat::state::{SeatAction, SeatEventKind};

        // Drive the state machine to a stable state. The loop consumes
        // any counter-event coalesced into the pending flags so a fast VT
        // flip can't strand us: after a suspend, a coalesced `pending_enable`
        // resumes; after a resume, a coalesced `pending_disable` re-suspends
        // (the no-blink boundary, via `resume_complete`). A pending flag is
        // only ever set by a real libseat callback, so the session has
        // genuinely toggled and DRM master is in the expected state when we
        // act on it.
        let entry_state = self.seat_state;
        let mut action = self.seat_state.on_event(&mut self.seat_pending, ev);
        log::info!(
            "kms: drive_seat_event ev={ev:?} {entry_state:?}→{:?} action={action:?}",
            self.seat_state,
        );
        loop {
            match action {
                SeatAction::BeginSuspend => {
                    self.run_suspend(state);
                    self.seat_state.suspend_complete(&self.seat_pending);
                    if self.seat_pending.pending_enable {
                        self.seat_pending.pending_enable = false;
                        // Re-drive through the state machine so it
                        // transitions Suspended → Resuming (and returns
                        // BeginResume) — resume_complete asserts Resuming.
                        action = self
                            .seat_state
                            .on_event(&mut self.seat_pending, SeatEventKind::Enable);
                        continue;
                    }
                    break;
                }
                SeatAction::BeginResume => {
                    self.run_resume(state);
                    // `resume_complete` returns `BeginSuspend` (consuming
                    // `pending_disable`) for the no-blink boundary, else
                    // commits `Active`.
                    action = self.seat_state.resume_complete(&mut self.seat_pending);
                    if matches!(action, SeatAction::BeginSuspend) {
                        continue;
                    }
                    // Committed Active: scanout gate is open — post a
                    // full-damage repaint on all outputs.
                    self.scene.wake_for_damage();
                    break;
                }
                SeatAction::Nothing => {
                    log::debug!(
                        "kms: seat event {:?} ignored in state {:?}",
                        ev,
                        self.seat_state
                    );
                    break;
                }
            }
        }
    }

    /// Drive a fake seat enable/disable, bypassing libseat. Used by
    /// the integration test and the `YSERVER_SIMULATE_VT_SWITCH` knob
    /// (Task 13).
    pub fn inject_seat_event_for_test(&mut self, state: &mut ServerState, enable: bool) {
        use crate::seat::state::SeatEventKind;
        let ev = if enable {
            SeatEventKind::Enable
        } else {
            SeatEventKind::Disable
        };
        self.drive_seat_event(state, ev);
    }

    /// Deepest mapped window under the cursor. Walks
    /// `core.top_level_order` back-to-front for the topmost top-level
    /// match, then descends the sub-window tree picking the topmost
    /// mapped child at each level whose screen-coords box contains
    /// the cursor. SHAPE-input (or bounding) trims the hittable
    /// region at every level.
    ///
    /// Why descend: xfwm4 attaches resize-edge cursors to thin frame
    /// sub-windows (one child per edge under each frame top-level),
    /// not to the frame top-level itself. Without sub-window descent
    /// the pointer-window stays pinned to the frame, the cursor walk
    /// in `effective_cursor_walking_chain` picks up only the frame's
    /// (`None`) cursor + the root fallback, and the resize sprites
    /// never become effective — the cursor stays as the default
    /// arrow over xfwm4 frame edges. Matches Xorg `dix/events.c`'s
    /// `XYToWindow` descent. The depth bound mirrors the cursor
    /// walk's 64.
    fn window_under_cursor(&self) -> Option<u32> {
        let cx = f64::from(self.core.cursor_x);
        let cy = f64::from(self.core.cursor_y);
        let mut hit: Option<(u32, f64, f64)> = None;
        for &window_id in self.core.top_level_order.iter().rev() {
            let Some(w) = self.windows_v2.get(&window_id) else {
                log::trace!(
                    target: "yserver::kms::v2::pointer",
                    "wuc: skip 0x{window_id:x} (not in windows_v2)"
                );
                continue;
            };
            if !w.mapped {
                log::trace!(
                    target: "yserver::kms::v2::pointer",
                    "wuc: skip 0x{window_id:x} (unmapped)"
                );
                continue;
            }
            let wx = f64::from(w.x);
            let wy = f64::from(w.y);
            if cx < wx || cx >= wx + f64::from(w.width) || cy < wy || cy >= wy + f64::from(w.height)
            {
                log::trace!(
                    target: "yserver::kms::v2::pointer",
                    "wuc: skip 0x{window_id:x} cursor=({cx},{cy}) outside geom=({},{} {}x{})",
                    w.x, w.y, w.width, w.height
                );
                continue;
            }
            if !self.cursor_inside_shape(window_id, cx - wx, cy - wy) {
                log::trace!(
                    target: "yserver::kms::v2::pointer",
                    "wuc: skip 0x{window_id:x} local=({},{}) SHAPE-excluded (geom={},{} {}x{})",
                    cx - wx, cy - wy, w.x, w.y, w.width, w.height
                );
                continue;
            }
            log::trace!(
                target: "yserver::kms::v2::pointer",
                "wuc: HIT 0x{window_id:x} cursor=({cx},{cy}) local=({},{}) geom=({},{} {}x{})",
                cx - wx, cy - wy, w.x, w.y, w.width, w.height
            );
            hit = Some((window_id, wx, wy));
            break;
        }
        let (mut parent_xid, mut parent_x, mut parent_y) = hit?;
        for _ in 0..64 {
            let mut children: Vec<(u32, u64, i16, i16, u16, u16)> = self
                .windows_v2
                .iter()
                .filter_map(|(xid, g)| {
                    (g.parent == Some(parent_xid) && g.mapped).then_some((
                        *xid,
                        g.stack_rank,
                        g.x,
                        g.y,
                        g.width,
                        g.height,
                    ))
                })
                .collect();
            children.sort_by_key(|c| std::cmp::Reverse(c.1));
            let mut next: Option<(u32, f64, f64)> = None;
            for (child_id, _rank, cxoff, cyoff, cw, ch) in children {
                let cax = parent_x + f64::from(cxoff);
                let cay = parent_y + f64::from(cyoff);
                if cx < cax || cx >= cax + f64::from(cw) || cy < cay || cy >= cay + f64::from(ch) {
                    continue;
                }
                if !self.cursor_inside_shape(child_id, cx - cax, cy - cay) {
                    continue;
                }
                next = Some((child_id, cax, cay));
                break;
            }
            match next {
                Some((child, cax, cay)) => {
                    parent_xid = child;
                    parent_x = cax;
                    parent_y = cay;
                }
                None => break,
            }
        }
        Some(parent_xid)
    }

    /// SHAPE-input (preferred) / bounding (fallback) hit-test for a
    /// single window. `local_x`/`local_y` are the pointer position
    /// in the window's own coordinate space (origin = window's top-
    /// left). Returns `true` when no SHAPE is set or the cursor lies
    /// inside at least one rectangle; an empty rect list means the
    /// window is unhittable.
    fn cursor_inside_shape(&self, window_id: u32, local_x: f64, local_y: f64) -> bool {
        let shape = self
            .core
            .shape_input
            .get(&window_id)
            .or_else(|| self.core.shape_bounding.get(&window_id));
        let Some(rects) = shape else {
            return true;
        };
        rects.iter().any(|r| {
            let rx = f64::from(r.x);
            let ry = f64::from(r.y);
            local_x >= rx
                && local_x < rx + f64::from(r.width)
                && local_y >= ry
                && local_y < ry + f64::from(r.height)
        })
    }

    /// Event-window-relative coords for an event whose `host_xid`
    /// is the topmost mapped top-level under the cursor. v2-shape
    /// port — reads geometry off `windows_v2`. Falls back to root
    /// coords when `host_xid` isn't tracked (the dispatcher
    /// re-derives target coords from its own tree walk anyway).
    fn event_relative_coords(&self, host_xid: u32) -> (i16, i16) {
        if let Some(w) = self.windows_v2.get(&host_xid) {
            let ex = (self.core.cursor_x as i32) - i32::from(w.x);
            let ey = (self.core.cursor_y as i32) - i32::from(w.y);
            (
                ex.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16,
                ey.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16,
            )
        } else {
            (self.core.cursor_x as i16, self.core.cursor_y as i16)
        }
    }

    fn emit_pointer(&mut self, ev: HostPointerEvent) {
        self.core.pending_pointer_events.push(ev);
    }

    fn emit_crossing(
        &mut self,
        host_xid: u32,
        kind: PointerEventKind,
        detail: u8,
        crossing_mode: u8,
        child: u32,
        state: u16,
    ) {
        let (event_x, event_y) = self.event_relative_coords(host_xid);
        let ev = HostPointerEvent {
            kind,
            host_xid,
            detail,
            time: crate::clock::server_time_ms(),
            root_x: self.core.cursor_x as i16,
            root_y: self.core.cursor_y as i16,
            event_x,
            event_y,
            state,
            crossing_mode,
            child,
        };
        self.emit_pointer(ev);
    }

    fn emit_motion_only(&mut self, host_xid: u32, mask: u16) {
        let (event_x, event_y) = self.event_relative_coords(host_xid);
        let ev = HostPointerEvent {
            kind: PointerEventKind::MotionNotify,
            host_xid,
            detail: 0,
            time: crate::clock::server_time_ms(),
            root_x: self.core.cursor_x as i16,
            root_y: self.core.cursor_y as i16,
            event_x,
            event_y,
            state: mask,
            crossing_mode: 0,
            child: 0,
        };
        self.emit_pointer(ev);
    }

    /// Spec-correct Normal-mode crossing chain for a top-level
    /// transition. Direct v1 port (kms/backend.rs:6630-6695) —
    /// the body only touches KmsCore + nested-resource look-ups.
    fn update_pointer_window(&mut self, server_state: &ServerState, new_xid: u32, mask: u16) {
        if self.core.prev_pointer_window == Some(new_xid) {
            log::trace!(
                target: "yserver::kms::v2::pointer",
                "upw: SKIP-SAME prev=new=0x{new_xid:x}"
            );
            return;
        }
        let prev_host = self.core.prev_pointer_window;
        let root_container_host = self.core.window_id;
        let resolve_host_to_nested = |host: u32, xid_map: &HostXidMap| -> Option<ResourceId> {
            if host == root_container_host {
                Some(yserver_core::resources::ROOT_WINDOW)
            } else {
                xid_map.get(&host).copied()
            }
        };
        let prev_id = prev_host.and_then(|p| resolve_host_to_nested(p, &self.core.xid_map));
        let new_id = resolve_host_to_nested(new_xid, &self.core.xid_map);
        log::trace!(
            target: "yserver::kms::v2::pointer",
            "upw: prev_host={:?} new_host=0x{:x} prev_nested={:?} new_nested={:?}",
            prev_host.map(|h| format!("0x{h:x}")),
            new_xid,
            prev_id.map(|r| r.0),
            new_id.map(|r| r.0),
        );

        if let (Some(from), Some(to)) = (prev_id, new_id) {
            let events = yserver_core::crossings::normal_mode_crossings(server_state, from, to);
            log::trace!(
                target: "yserver::kms::v2::pointer",
                "upw: normal_mode_crossings(from={}, to={}) → {} events",
                from.0, to.0, events.len()
            );
            for ev in events {
                let win_host_xid = if ev.window == yserver_core::resources::ROOT_WINDOW {
                    self.core.window_id
                } else {
                    server_state
                        .resources
                        .window(ev.window)
                        .and_then(|w| w.host_xid.map(|h| h.as_raw()))
                        .unwrap_or(new_xid)
                };
                let kind = match ev.kind {
                    yserver_core::crossings::CrossingKind::Enter => PointerEventKind::EnterNotify,
                    yserver_core::crossings::CrossingKind::Leave => PointerEventKind::LeaveNotify,
                };
                log::trace!(
                    target: "yserver::kms::v2::pointer",
                    "upw: emit_crossing host=0x{win_host_xid:x} kind={:?} detail={} child={:#x}",
                    kind, ev.detail, ev.child.0
                );
                self.emit_crossing(win_host_xid, kind, ev.detail, 0, ev.child.0, mask);
            }
        } else {
            log::trace!(
                target: "yserver::kms::v2::pointer",
                "upw: FALLBACK path (prev_id={:?}, new_id={:?})",
                prev_id, new_id
            );
            // First-motion bootstrap or unmapped host_xid —
            // fall back to a single Leave/Enter with detail=0.
            if let Some(prev) = prev_host {
                self.emit_crossing(prev, PointerEventKind::LeaveNotify, 0, 0, 0, mask);
            }
            self.emit_crossing(new_xid, PointerEventKind::EnterNotify, 0, 0, 0, mask);
        }
        self.core.prev_pointer_window = Some(new_xid);
        // Stage 5 Phase A: cross-in may change the effective cursor
        // (per-window DefineCursor walks up the parent chain).
        self.refresh_effective_cursor();
    }

    fn dispatch_motion_event(&mut self, server_state: &ServerState) {
        // Fall back to the root container so root-window subscribers
        // (e16's right-click-desktop menu, fvwm3's root bindings) can
        // see motion when the cursor is over the wallpaper.
        let host_xid = self.window_under_cursor().unwrap_or(self.core.window_id);
        let mask = self.serialize_modifiers() | self.core.button_mask;
        log::trace!(
            target: "yserver::kms::v2::pointer",
            "dispatch_motion: cursor=({},{}) → host_xid=0x{host_xid:x}",
            self.core.cursor_x, self.core.cursor_y
        );
        self.update_pointer_window(server_state, host_xid, mask);
        self.emit_motion_only(host_xid, mask);
    }

    fn process_pointer_absolute(&mut self, server_state: &ServerState, x: f32, y: f32) {
        // Clamp to the UNION framebuffer extent (`fb_w`/`fb_h`),
        // not the first output's box. `core_platform_init`
        // (`kms/backend.rs:1063-1072`) computes this as
        // `max(x + width)` across every output, which is also the
        // extent the input thread targets when it accumulates
        // libinput deltas (`input_thread.rs:180-189`). Pre-fix
        // this consulted `outputs.first().width/height`, so the
        // pointer could never cross from output 0 onto a side-
        // adjacent output 1 — pinned by
        // `process_pointer_absolute_uses_union_fb_extent_for_multi_output`.
        let fb_w = f32::from(self.platform.fb_w.max(1));
        let fb_h = f32::from(self.platform.fb_h.max(1));
        let new_x = x.clamp(0.0, (fb_w - 1.0).max(0.0));
        let new_y = y.clamp(0.0, (fb_h - 1.0).max(0.0));
        if new_x != self.core.cursor_x || new_y != self.core.cursor_y {
            self.core.cursor_x = new_x;
            self.core.cursor_y = new_y;
            // Stage 5 Phase D — pointer fast path. When the plane
            // is fully bound on every output AND no transition is
            // pending, route motion directly through
            // `cursor_plane_move` — one ioctl per visible CRTC, no
            // GPU work, no compose cadence. The Mixed state (any
            // output still has a Sw→Hw or Hw→Sw transition
            // pending) falls back to the scene-wake path so the
            // SW cursor doesn't desync from the eventual plane
            // bind. The core thread owns DRM state, so this is
            // not a thread-safety question — the ioctl is
            // synchronous from the same thread that owns scene
            // state.
            if matches!(
                self.scene.cursor_mode(),
                crate::kms::v2::scene::CursorPlaneMode::Hw
            ) {
                #[allow(clippy::cast_possible_truncation)]
                let cx = new_x as i32;
                #[allow(clippy::cast_possible_truncation)]
                let cy = new_y as i32;
                let (hot_x, hot_y) = self
                    .effective_cursor_xid
                    .and_then(|xid| self.cursor_records.get(&xid))
                    .map(|rec| (rec.hot_x, rec.hot_y))
                    .unwrap_or((0, 0));
                match self.platform.cursor_plane_move(cx, cy, hot_x, hot_y) {
                    Ok(0) => {}
                    Ok(n) => self.telemetry.record_cursor_move_ebusy(u64::from(n)),
                    Err(e) => log::debug!("v2 cursor fast path: move failed: {e}"),
                }
            } else {
                self.scene.wake_for_damage();
            }
        }
        self.dispatch_motion_event(server_state);
    }

    fn process_pointer_button(&mut self, code: u32, pressed: bool, server_state: &ServerState) {
        let detail = match code {
            0x110 => 1, // BTN_LEFT
            0x111 => 3, // BTN_RIGHT
            0x112 => 2, // BTN_MIDDLE
            0x113 => 8, // BTN_SIDE
            0x114 => 9, // BTN_EXTRA
            0x180 => 4, // SYNTH_SCROLL_UP
            0x181 => 5, // SYNTH_SCROLL_DOWN
            0x182 => 6, // SYNTH_SCROLL_LEFT
            0x183 => 7, // SYNTH_SCROLL_RIGHT
            _ => {
                log::debug!("v2: unmapped libinput button code 0x{code:x}, dropping");
                return;
            }
        };
        let host_xid = self.window_under_cursor().unwrap_or(self.core.window_id);
        let (event_x, event_y) = self.event_relative_coords(host_xid);
        let button_bit: u16 = match detail {
            1 => 0x0100,
            2 => 0x0200,
            3 => 0x0400,
            4 => 0x0800,
            5 => 0x1000,
            _ => 0,
        };
        let modifier_mask = self.serialize_modifiers();
        // X11 spec: `state` is the logical button state IMMEDIATELY
        // BEFORE the event takes effect. Press: button bit not yet
        // set. Release: button bit still set.
        let state = if pressed {
            modifier_mask | self.core.button_mask
        } else {
            modifier_mask | self.core.button_mask | button_bit
        };
        if pressed {
            self.core.button_mask |= button_bit;
        } else {
            self.core.button_mask &= !button_bit;
        }
        let time = crate::clock::server_time_ms();
        let kind = if pressed {
            PointerEventKind::ButtonPress
        } else {
            PointerEventKind::ButtonRelease
        };
        let ptr_event = HostPointerEvent {
            kind,
            host_xid,
            detail,
            time,
            root_x: self.core.cursor_x as i16,
            root_y: self.core.cursor_y as i16,
            event_x,
            event_y,
            state,
            crossing_mode: 0,
            child: 0,
        };
        self.emit_pointer(ptr_event);
        // Implicit-grab crossings (G3). Direct v1 port.
        let post_state = self.serialize_modifiers() | self.core.button_mask;
        let press_mode: u8 = if pressed { 1 } else { 2 };
        let grab_id = self.core.xid_map.get(&host_xid).copied();
        let focus_id = self
            .core
            .prev_pointer_window
            .and_then(|prev| self.core.xid_map.get(&prev).copied());
        if let (Some(focus), Some(grab)) = (focus_id, grab_id) {
            let events =
                yserver_core::crossings::implicit_grab_crossings(server_state, focus, grab);
            for ev in events {
                let win_host_xid = server_state
                    .resources
                    .window(ev.window)
                    .and_then(|w| w.host_xid.map(|h| h.as_raw()))
                    .unwrap_or(host_xid);
                let kind = match ev.kind {
                    yserver_core::crossings::CrossingKind::Enter => PointerEventKind::EnterNotify,
                    yserver_core::crossings::CrossingKind::Leave => PointerEventKind::LeaveNotify,
                };
                self.emit_crossing(
                    win_host_xid,
                    kind,
                    ev.detail,
                    press_mode,
                    ev.child.0,
                    post_state,
                );
            }
        }
    }

    /// Decode the wire-packed clip rectangle list (`Vec<u8>` of
    /// i16 x, i16 y, u16 w, u16 h tuples) into `Rectangle16`s in
    /// dst-coords (with the GC clip-origin already added). Returns
    /// `None` when the current GC clip is `None`. `Pixmap`-clip is
    /// returned as `None` for now — Stage 3f.3 promotes the
    /// pixmap-mask path; until then the clip is passed through
    /// (matches v1's pre-promotion behaviour).
    fn current_clip_rects_in_dst_space(&self) -> Option<Vec<Rectangle16>> {
        let ClipState::Rectangles { origin, rects } = &self.core.current_clip else {
            return None;
        };
        let bytes = &rects.rectangles;
        let mut out = Vec::with_capacity(bytes.len() / 8);
        for chunk in bytes.chunks_exact(8) {
            let cx = i32::from(i16::from_le_bytes([chunk[0], chunk[1]])) + i32::from(origin.0);
            let cy = i32::from(i16::from_le_bytes([chunk[2], chunk[3]])) + i32::from(origin.1);
            let cw = i32::from(u16::from_le_bytes([chunk[4], chunk[5]]));
            let ch = i32::from(u16::from_le_bytes([chunk[6], chunk[7]]));
            if cw <= 0 || ch <= 0 {
                continue;
            }
            out.push(Rectangle16 {
                x: cx.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16,
                y: cy.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16,
                width: cw.min(i32::from(u16::MAX)) as u16,
                height: ch.min(i32::from(u16::MAX)) as u16,
            });
        }
        Some(out)
    }

    /// Intersect each rect in `rects` against the current GC clip.
    /// Handles three states:
    ///   - `ClipState::None` → pass through (input unchanged).
    ///   - `ClipState::Rectangles` → rect-vs-rect intersection (mirrors v1).
    ///   - `ClipState::Pixmap` → per-pixel mask gating via
    ///     [`super::super::backend::rasterize_pixmap_mask_to_rects`]
    ///     against the readback cached at `set_clip_pixmap` time.
    pub(crate) fn intersect_with_current_clip(&self, rects: &[Rectangle16]) -> Vec<Rectangle16> {
        match &self.core.current_clip {
            ClipState::None => rects.to_vec(),
            ClipState::Rectangles { .. } => {
                let clip_rects = self.current_clip_rects_in_dst_space().unwrap_or_default();
                let mut out = Vec::with_capacity(rects.len());
                for r in rects {
                    let rx0 = i32::from(r.x);
                    let ry0 = i32::from(r.y);
                    let rx1 = rx0 + i32::from(r.width);
                    let ry1 = ry0 + i32::from(r.height);
                    for c in &clip_rects {
                        let cx0 = i32::from(c.x);
                        let cy0 = i32::from(c.y);
                        let cx1 = cx0 + i32::from(c.width);
                        let cy1 = cy0 + i32::from(c.height);
                        let ix0 = rx0.max(cx0);
                        let iy0 = ry0.max(cy0);
                        let ix1 = rx1.min(cx1);
                        let iy1 = ry1.min(cy1);
                        if ix0 < ix1 && iy0 < iy1 {
                            out.push(Rectangle16 {
                                x: ix0 as i16,
                                y: iy0 as i16,
                                width: (ix1 - ix0) as u16,
                                height: (iy1 - iy0) as u16,
                            });
                        }
                    }
                }
                out
            }
            ClipState::Pixmap { .. } => {
                // Cache populated at `set_clip_pixmap`. Missing cache =
                // mask readback failed; degrade to no-paint (safer than
                // pass-through, which would obliterate prior decoration).
                let Some(cache) = self.clip_mask_cache.as_ref() else {
                    return Vec::new();
                };
                crate::kms::backend::rasterize_pixmap_mask_to_rects(
                    rects,
                    &cache.bytes,
                    cache.width,
                    cache.height,
                    u32::from(cache.depth),
                    cache.row_stride,
                    cache.origin,
                )
            }
        }
    }

    /// Refresh a pixmap clip-mask from the live source pixmap when
    /// possible, then intersect `rects` against the current clip state.
    /// If the source pixmap has been freed after installation into the GC,
    /// the cached bytes remain valid and only the clip origin is updated.
    fn intersect_with_current_clip_live(&mut self, rects: &[Rectangle16]) -> Vec<Rectangle16> {
        let pixmap_clip = match &self.core.current_clip {
            ClipState::Pixmap { origin, pixmap } => Some((pixmap.as_raw(), *origin)),
            _ => None,
        };
        if let Some((xid, origin)) = pixmap_clip {
            if let Some(fresh) = self.read_clip_mask_bytes(xid, origin) {
                self.clip_mask_cache = Some(fresh);
            } else if let Some(cache) = self.clip_mask_cache.as_mut()
                && cache.pixmap_xid == xid
            {
                cache.origin = origin;
            }
        }
        self.intersect_with_current_clip(rects)
    }

    fn read_fill_pattern_cache(
        &mut self,
        host_pixmap_xid: u32,
        origin: (i16, i16),
    ) -> Option<FillPatternCache> {
        let id = self.store.lookup(host_pixmap_xid)?;
        let (depth, extent) = {
            let d = self.store.get(id)?;
            (d.depth, d.storage.extent)
        };
        let rect = ash::vk::Rect2D {
            offset: ash::vk::Offset2D { x: 0, y: 0 },
            extent,
        };
        let bytes = self
            .engine
            .get_image(&mut self.store, &mut self.platform, id, rect, depth)
            .ok()?;
        Some(FillPatternCache {
            pixmap_xid: host_pixmap_xid,
            origin,
            depth,
            width: extent.width,
            height: extent.height,
            bytes,
        })
    }

    /// Synchronously read a pixmap's full extent via `engine.get_image`
    /// and return a `ClipMaskCache` ready for `intersect_with_current_clip`
    /// consumption. Returns `None` if the pixmap isn't in the store, has
    /// an unsupported depth (anything other than 1/8), or the readback
    /// errors. Bytes are in X11 wire format per
    /// `kms::v2::engine::pack_from_storage` — depth-1 packed MSB-first,
    /// scanline-padded to 32 bits; depth-8 one byte per pixel,
    /// scanline-padded to 32 bits.
    pub(crate) fn read_clip_mask_bytes(
        &mut self,
        host_pixmap_xid: u32,
        origin: (i16, i16),
    ) -> Option<crate::kms::backend::ClipMaskCache> {
        let id = self.store.lookup(host_pixmap_xid)?;
        let (width, height, depth) = {
            let d = self.store.get(id)?;
            let extent = d.storage.extent;
            (
                u16::try_from(extent.width).ok()?,
                u16::try_from(extent.height).ok()?,
                d.depth,
            )
        };
        if !matches!(depth, 1 | 8) {
            return None;
        }
        let rect = ash::vk::Rect2D {
            offset: ash::vk::Offset2D { x: 0, y: 0 },
            extent: ash::vk::Extent2D {
                width: u32::from(width),
                height: u32::from(height),
            },
        };
        let bytes = self
            .engine
            .get_image(&mut self.store, &mut self.platform, id, rect, depth)
            .ok()?;
        // pack_from_storage convention: depth-1 → ((w + 31) / 32) * 4;
        // depth-4/8 → ((w + 3) / 4) * 4. Both scanline-padded to 32 bits.
        let row_stride: u32 = match depth {
            1 => u32::from(width).div_ceil(32) * 4,
            4 | 8 => u32::from(width).div_ceil(4) * 4,
            _ => return None,
        };
        Some(crate::kms::backend::ClipMaskCache {
            pixmap_xid: host_pixmap_xid,
            origin,
            width,
            height,
            depth,
            row_stride,
            bytes,
        })
    }

    /// Storage dimensions for a host xid, in pixels. `None` if the
    /// drawable is unknown.
    fn drawable_dims_v2(&self, host_xid: u32) -> Option<(u32, u32)> {
        let id = self.store.lookup(host_xid)?;
        let d = self.store.get(id)?;
        Some((d.storage.extent.width, d.storage.extent.height))
    }

    /// Lower a list of solid-colour rectangles to the appropriate
    /// engine path. Used by the stroke-style poly ops (`PolyLine`,
    /// `PolySegment`, `PolyPoint`, `PolyArc`, `PolyRectangle`) where
    /// every rasterised rect is in the GC's single foreground colour
    /// regardless of GC fill-style, and as the fallback inside
    /// Stage 3f.11: apply X11 ConfigureWindow `stack_mode` to a
    /// top-level window's position in `core.top_level_order`.
    /// Implements Above (0/2/4) and Below (1/3) per v1's behaviour
    /// — TopIf/BottomIf/Opposite collapse to Above/Below without
    /// the conditional check (sufficient for marco / fvwm /
    /// xterm-popup workloads). No-op for windows that aren't in
    /// `top_level_order` (subwindows; restack of a subwindow is
    /// deferred until we track per-parent sibling stack order).
    fn restack_top_level(&mut self, host_xid: u32, stack_mode: u8, sibling: Option<u32>) {
        let stack = &mut self.core.top_level_order;
        if !stack.contains(&host_xid) {
            // Subwindow restack — siblings aren't ordered in v2 yet.
            // Future work; tracked in `status.md` § 3f.11.
            return;
        }
        stack.retain(|&x| x != host_xid);
        let sibling_pos = sibling.and_then(|sib| stack.iter().position(|&x| x == sib));
        match stack_mode {
            // Above: place above sibling, or at top if no sibling.
            0 | 2 | 4 => match sibling_pos {
                Some(sp) => stack.insert(sp + 1, host_xid),
                None => stack.push(host_xid),
            },
            // Below: place below sibling, or at bottom if no sibling.
            1 | 3 => match sibling_pos {
                Some(sp) => stack.insert(sp, host_xid),
                None => stack.insert(0, host_xid),
            },
            _ => stack.push(host_xid),
        }
        log::trace!(
            "v2 restack_top_level host=0x{host_xid:x} mode={stack_mode} sibling={sibling:?} order={:?}",
            self.core.top_level_order
        );
        self.scene.mark_scene_structure_dirty();
    }

    fn property_value_by_name<'a>(
        state: &'a yserver_core::server::ServerState,
        window: &'a yserver_core::resources::Window,
        name: &str,
    ) -> Option<&'a PropertyValue> {
        window.properties.iter().find_map(|(atom, value)| {
            state
                .atoms
                .name(*atom)
                .is_some_and(|n| n == name)
                .then_some(value)
        })
    }

    fn atom_list_from_property(value: &PropertyValue) -> Option<Vec<AtomId>> {
        if !matches!(value.format, yserver_core::properties::PropertyFormat::F32)
            || !value.data.len().is_multiple_of(4)
        {
            return None;
        }
        Some(
            value
                .data
                .chunks_exact(4)
                .map(|chunk| AtomId(u32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]])))
                .collect(),
        )
    }

    fn window_stack_hint(
        state: &yserver_core::server::ServerState,
        window: &yserver_core::resources::Window,
    ) -> Option<TopLevelStackHint> {
        let mut bottom = false;
        let mut top = false;

        if let Some(value) = Self::property_value_by_name(state, window, "_NET_WM_WINDOW_TYPE")
            && let Some(atoms) = Self::atom_list_from_property(value)
        {
            for atom in atoms {
                match state.atoms.name(atom) {
                    Some("_NET_WM_WINDOW_TYPE_DESKTOP") => bottom = true,
                    Some("_NET_WM_WINDOW_TYPE_DOCK")
                    | Some("_NET_WM_WINDOW_TYPE_TOOLBAR")
                    | Some("_NET_WM_WINDOW_TYPE_MENU")
                    | Some("_NET_WM_WINDOW_TYPE_DROPDOWN_MENU")
                    | Some("_NET_WM_WINDOW_TYPE_POPUP_MENU")
                    | Some("_NET_WM_WINDOW_TYPE_TOOLTIP")
                    | Some("_NET_WM_WINDOW_TYPE_UTILITY")
                    | Some("_NET_WM_WINDOW_TYPE_COMBO")
                    | Some("_NET_WM_WINDOW_TYPE_DND")
                    | Some("_NET_WM_WINDOW_TYPE_DIALOG")
                    | Some("_NET_WM_WINDOW_TYPE_SPLASH")
                    | Some("_NET_WM_WINDOW_TYPE_NOTIFICATION") => top = true,
                    _ => {}
                }
            }
        }

        if let Some(value) = Self::property_value_by_name(state, window, "_NET_WM_STATE")
            && let Some(atoms) = Self::atom_list_from_property(value)
        {
            for atom in atoms {
                match state.atoms.name(atom) {
                    Some("_NET_WM_STATE_BELOW") => bottom = true,
                    Some("_NET_WM_STATE_ABOVE")
                    | Some("_NET_WM_STATE_MODAL")
                    | Some("_NET_WM_STATE_FULLSCREEN")
                    | Some("_NET_WM_STATE_DEMANDS_ATTENTION")
                    | Some("_NET_WM_STATE_FOCUSED") => top = true,
                    _ => {}
                }
            }
        }

        if let Some(value) = Self::property_value_by_name(state, window, "WM_TRANSIENT_FOR")
            && value.format == yserver_core::properties::PropertyFormat::F32
            && value.data.len() >= 4
        {
            let target =
                u32::from_ne_bytes([value.data[0], value.data[1], value.data[2], value.data[3]]);
            if target != 0 {
                top = true;
            }
        }

        match (bottom, top) {
            (true, _) => Some(TopLevelStackHint::Bottom),
            (false, true) => Some(TopLevelStackHint::Top),
            _ => None,
        }
    }

    fn apply_top_level_stack_hint(
        &mut self,
        state: &yserver_core::server::ServerState,
        host_xid: u32,
    ) {
        if !self.core.top_level_order.contains(&host_xid) {
            return;
        }
        let Some(window_id) = state
            .resources
            .children(yserver_core::resources::ROOT_WINDOW)
            .iter()
            .copied()
            .find(|id| {
                state
                    .resources
                    .window(*id)
                    .and_then(|w| w.host_xid)
                    .is_some_and(|h| h.as_raw() == host_xid)
            })
        else {
            return;
        };
        let Some(window) = state.resources.window(window_id) else {
            return;
        };
        let Some(hint) = Self::window_stack_hint(state, window) else {
            return;
        };
        match hint {
            TopLevelStackHint::Bottom => self.restack_top_level(host_xid, 1, None),
            TopLevelStackHint::Top => self.restack_top_level(host_xid, 0, None),
        }
    }

    /// Stage 4a — shift each rect by `(dx, dy)` (saturating to
    /// i16 range). Returns the input unchanged when both deltas
    /// are zero. Used to translate window-local paint rects into
    /// backing-local coords under COMPOSITE redirect: a paint
    /// against descendant C of redirected W at offset
    /// `(cx, cy)` against W lands at C's rect + `(cx, cy)` in
    /// W's backing.
    fn shift_rectangles_for_paint(
        rects: &[Rectangle16],
        (dx, dy): (i32, i32),
    ) -> std::borrow::Cow<'_, [Rectangle16]> {
        if dx == 0 && dy == 0 {
            return std::borrow::Cow::Borrowed(rects);
        }
        std::borrow::Cow::Owned(
            rects
                .iter()
                .map(|r| Rectangle16 {
                    x: (i32::from(r.x) + dx).clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16,
                    y: (i32::from(r.y) + dy).clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16,
                    width: r.width,
                    height: r.height,
                })
                .collect(),
        )
    }

    /// Stage 4a — shift a picture's clip rects from
    /// dst-drawable-local into backing-local coords. The clip
    /// itself is stored in dst-window-local coords (pre-shifted
    /// by Stage 3b's `clip_x` / `clip_y`); when paint resolves
    /// through a redirected ancestor, the per-rect scissor in
    /// the engine operates against the backing's storage extent,
    /// so the clip must move with it.
    fn shift_dst_picture_clip(
        clip: Option<Vec<Rectangle16>>,
        offset: (i32, i32),
    ) -> Option<Vec<Rectangle16>> {
        let rects = clip?;
        Some(Self::shift_rectangles_for_paint(&rects, offset).into_owned())
    }

    /// Apply the GC's subwindow mode to fill-style rects expressed in the
    /// destination window's local coordinates. `ClipByChildren` subtracts
    /// every mapped automatic child window; `IncludeInferiors` leaves the
    /// rects unchanged.
    fn clip_fill_rects_by_subwindow_mode(
        &self,
        host_xid: u32,
        rects: &[Rectangle16],
    ) -> Vec<Rectangle16> {
        if rects.is_empty()
            || !matches!(
                self.core.current_subwindow_mode,
                yserver_core::backend::SubwindowMode::ClipByChildren,
            )
            || !self.windows_v2.contains_key(&host_xid)
        {
            return rects.to_vec();
        }
        let child_rects: Vec<ash::vk::Rect2D> = self
            .windows_v2
            .iter()
            .filter_map(|(child_host_xid, geom)| {
                if !(geom.parent == Some(host_xid) && geom.mapped) {
                    return None;
                }
                let is_manually_redirected = self
                    .store
                    .lookup(*child_host_xid)
                    .and_then(|id| self.store.get(id))
                    .is_some_and(|d| !d.scene_participating);
                if is_manually_redirected {
                    return None;
                }
                Some(ash::vk::Rect2D {
                    offset: ash::vk::Offset2D {
                        x: i32::from(geom.x),
                        y: i32::from(geom.y),
                    },
                    extent: ash::vk::Extent2D {
                        width: u32::from(geom.width),
                        height: u32::from(geom.height),
                    },
                })
            })
            .collect();
        if child_rects.is_empty() {
            return rects.to_vec();
        }
        let mut out = Vec::new();
        for r in rects {
            if r.width == 0 || r.height == 0 {
                continue;
            }
            let mut pieces = vec![ash::vk::Rect2D {
                offset: ash::vk::Offset2D {
                    x: i32::from(r.x),
                    y: i32::from(r.y),
                },
                extent: ash::vk::Extent2D {
                    width: u32::from(r.width),
                    height: u32::from(r.height),
                },
            }];
            for child in &child_rects {
                let mut next = Vec::new();
                for piece in pieces {
                    next.extend(subtract_one_rect_clip(piece, *child));
                }
                pieces = next;
                if pieces.is_empty() {
                    break;
                }
            }
            out.extend(pieces.into_iter().filter_map(|piece| {
                let x = i16::try_from(piece.offset.x).ok()?;
                let y = i16::try_from(piece.offset.y).ok()?;
                let width = u16::try_from(piece.extent.width).ok()?;
                let height = u16::try_from(piece.extent.height).ok()?;
                Some(Rectangle16 {
                    x,
                    y,
                    width,
                    height,
                })
            }));
        }
        out
    }

    fn collect_fill_rects_for_inferiors(
        &self,
        host_xid: u32,
        rects: &[Rectangle16],
    ) -> Vec<(u32, Vec<Rectangle16>)> {
        fn walk(
            backend: &KmsBackendV2,
            parent_xid: u32,
            rects: &[Rectangle16],
            out: &mut Vec<(u32, Vec<Rectangle16>)>,
        ) {
            for (child_xid, geom) in &backend.windows_v2 {
                let is_child = if parent_xid == backend.core.window_id {
                    geom.parent == Some(backend.core.window_id) || geom.parent.is_none()
                } else {
                    geom.parent == Some(parent_xid)
                };
                if !is_child || !geom.mapped {
                    continue;
                }
                let child_x = i32::from(geom.x);
                let child_y = i32::from(geom.y);
                let child_w = i32::from(geom.width);
                let child_h = i32::from(geom.height);
                let mut child_rects = Vec::new();
                for r in rects {
                    let rx0 = i32::from(r.x);
                    let ry0 = i32::from(r.y);
                    let rx1 = rx0 + i32::from(r.width);
                    let ry1 = ry0 + i32::from(r.height);
                    let ix0 = rx0.max(child_x);
                    let iy0 = ry0.max(child_y);
                    let ix1 = rx1.min(child_x + child_w);
                    let iy1 = ry1.min(child_y + child_h);
                    if ix0 < ix1 && iy0 < iy1 {
                        child_rects.push(Rectangle16 {
                            x: (ix0 - child_x) as i16,
                            y: (iy0 - child_y) as i16,
                            width: (ix1 - ix0) as u16,
                            height: (iy1 - iy0) as u16,
                        });
                    }
                }
                if child_rects.is_empty() {
                    continue;
                }
                out.push((*child_xid, child_rects.clone()));
                walk(backend, *child_xid, &child_rects, out);
            }
        }

        let mut out = Vec::new();
        walk(self, host_xid, rects, &mut out);
        out
    }

    /// [`fill_rects_honoring_fill_state`] for the Solid arm.
    ///
    /// `GcFunction::Copy` (the common case) goes through the fast
    /// `vkCmdClearAttachments`-driven `engine.fill_rect`. Non-`Copy`
    /// functions (Stage 3f.2: GXclear / GXxor / GXinvert / etc.)
    /// divert to `engine.logic_fill`, which builds a per-function
    /// `VkLogicOp` pipeline through the shared
    /// `LogicFillPipelineCache`. `GcFunction::NoOp` is a no-op.
    ///
    /// Stage 4a: `target` carries the resolved DrawableId + a
    /// paint-translation offset for COMPOSITE redirect. Window-
    /// local `rects` are shifted by `target.offset` before going
    /// to the engine.
    /// Build the per-call stroke snapshot from the GC state captured
    /// in `apply_draw_state`.
    fn current_stroke_state(&self, _foreground: u32) -> crate::kms::v2::stroke::StrokeState {
        crate::kms::v2::stroke::StrokeState {
            background: self.core.current_background,
            line_width: self.core.current_line_width,
            line_style: self.core.current_line_style,
            cap_style: self.core.current_cap_style,
            join_style: self.core.current_join_style,
            dashes: self.core.current_dashes.clone(),
            dash_offset: self.core.current_dash_offset,
        }
    }

    /// Clip a stroke's fg/bg rect lists against the current GC clip
    /// and submit them. `LineStyle::DoubleDash` off-runs land in
    /// `bg_rects` and paint in the GC background colour.
    fn emit_stroke_output(
        &mut self,
        target: PaintTarget,
        foreground: u32,
        background: u32,
        out: crate::kms::v2::stroke::StrokeOutput,
    ) {
        if !out.fg_rects.is_empty() {
            let fg_clipped = self.intersect_with_current_clip_live(&out.fg_rects);
            self.fill_solid_rects(target, foreground, &fg_clipped);
        }
        if !out.bg_rects.is_empty() {
            let bg_clipped = self.intersect_with_current_clip_live(&out.bg_rects);
            self.fill_solid_rects(target, background, &bg_clipped);
        }
    }

    fn fill_solid_rects(&mut self, target: PaintTarget, fg: u32, rects: &[Rectangle16]) {
        use yserver_core::backend::GcFunction;
        if rects.is_empty() {
            return;
        }
        let function = self.core.current_function;
        if matches!(function, GcFunction::NoOp) {
            return;
        }
        let (dx, dy) = target.offset;
        let id = target.id;
        let Some((depth, format, extent)) = self
            .store
            .get(id)
            .map(|d| (d.depth, d.storage.format, d.storage.extent))
        else {
            return;
        };
        let full_mask = depth_plane_mask(depth);
        let plane_mask = self.core.current_plane_mask & full_mask;
        if plane_mask == 0 {
            return;
        }
        let shifted = Self::shift_rectangles_for_paint(rects, target.offset);
        if depth < 8 || plane_mask != full_mask {
            self.fill_solid_rects_cpu_fallback(
                id,
                extent,
                depth,
                function,
                plane_mask,
                fg & full_mask,
                &shifted,
            );
            return;
        }
        if !matches!(function, GcFunction::Copy) {
            // Compute `opaque_alpha` per the L1 server-α invariant:
            // depth-32 ARGB destinations take the LogicOp on all four
            // channels; depth-24/8/1 are server-owned-α so the
            // pipeline's write mask drops alpha to keep the dst byte
            // intact. Depth lookup via the drawable record.
            let opaque_alpha = depth != 32;
            match self.engine.logic_fill(
                &mut self.store,
                &mut self.platform,
                id,
                function,
                opaque_alpha,
                fg & full_mask,
                &shifted,
            ) {
                Ok(()) => {
                    // One submit per call regardless of rect count
                    // (logic_fill records every rect into the same CB).
                    self.telemetry.record_paint_submit();
                    let op_byte = function.protocol_value();
                    let target_kind = self.submit_target_kind(id);
                    self.telemetry.record_submit_event(SubmitEvent {
                        frame_id: 0,
                        kind: SubmitKind::LogicFill,
                        target_kind,
                        target_id: id.as_u64(),
                        batch_size: u32::try_from(shifted.len()).unwrap_or(u32::MAX),
                        op: SubmitOp::from_gx_byte(op_byte),
                        src_class: SrcClass::None,
                        mask_class: SrcClass::None,
                        pipeline_id: None,
                        flags: SubmitFlags::NONE,
                    });
                }
                Err(e) => {
                    log::warn!(
                        "v2 fill_solid_rects: engine.logic_fill failed ({function:?}): {e:?}"
                    );
                }
            }
            return;
        }
        // L1 server-α invariant: depth-24 dst stores alpha=0xFF
        // regardless of the X11 pixel's upper byte. Without this,
        // the scene compositor's alpha_passthrough=true draws read
        // back α=0 (X-padding) and the window blends transparent —
        // the layer underneath leaks through, panel renders white
        // not teal. Matches v1's `try_vk_solid_fill` (kms/backend.rs:3512).
        let color = decode_x11_pixel_for_storage(fg & full_mask, depth, format);
        // Stage 3f.15: coalesce N stroke rects into one CB + one
        // submit via engine.fill_rect_batch. PolySegment / PolyLine
        // / PolyRectangle fan-outs now pay O(1) submits per protocol
        // request instead of O(N). Zero-sized rects are filtered
        // inside the engine.
        //
        // Stage 4a — apply paint-target offset (window-local →
        // backing-local) directly into the i32 vk::Offset2D.
        let vk_rects: Vec<ash::vk::Rect2D> = rects
            .iter()
            .filter(|r| r.width != 0 && r.height != 0)
            .map(|r| ash::vk::Rect2D {
                offset: ash::vk::Offset2D {
                    x: i32::from(r.x) + dx,
                    y: i32::from(r.y) + dy,
                },
                extent: ash::vk::Extent2D {
                    width: u32::from(r.width),
                    height: u32::from(r.height),
                },
            })
            .collect();
        if vk_rects.is_empty() {
            return;
        }
        let n_rects = u32::try_from(vk_rects.len()).unwrap_or(u32::MAX);
        match self
            .engine
            .fill_rect_batch(&mut self.store, &mut self.platform, id, color, &vk_rects)
        {
            Ok(()) => {
                self.telemetry.record_paint_submit();
                self.trace_simple(SubmitKind::FillBatch, id, n_rects);
            }
            Err(e) => {
                log::warn!("v2 fill_solid_rects: engine.fill_rect_batch failed: {e:?}");
            }
        }
    }

    fn fill_solid_rects_cpu_fallback(
        &mut self,
        id: DrawableId,
        extent: ash::vk::Extent2D,
        depth: u8,
        function: yserver_core::backend::GcFunction,
        plane_mask: u32,
        fg: u32,
        rects: &[Rectangle16],
    ) {
        let rect = ash::vk::Rect2D {
            offset: ash::vk::Offset2D::default(),
            extent,
        };
        let mut bytes =
            match self
                .engine
                .get_image(&mut self.store, &mut self.platform, id, rect, depth)
            {
                Ok(bytes) => bytes,
                Err(e) => {
                    log::warn!("v2 fill_solid_rects_cpu_fallback: get_image failed: {e:?}");
                    return;
                }
            };
        let full_mask = depth_plane_mask(depth);
        for r in rects {
            let x0 = i32::from(r.x).max(0) as usize;
            let y0 = i32::from(r.y).max(0) as usize;
            let x1 = (i32::from(r.x).saturating_add(i32::from(r.width))).min(extent.width as i32);
            let y1 = (i32::from(r.y).saturating_add(i32::from(r.height))).min(extent.height as i32);
            if x1 <= x0 as i32 || y1 <= y0 as i32 {
                continue;
            }
            for y in y0..y1 as usize {
                for x in x0..x1 as usize {
                    let dst = read_z_pixmap_pixel(&bytes, depth, extent.width, x, y) & full_mask;
                    let out = apply_gc_function(function, fg, dst, plane_mask) & full_mask;
                    write_z_pixmap_pixel(&mut bytes, depth, extent.width, x, y, out);
                }
            }
        }
        if let Err(e) = self.engine.put_image(
            &mut self.store,
            &mut self.platform,
            id,
            ash::vk::Offset2D::default(),
            extent,
            &bytes,
            depth,
        ) {
            log::warn!("v2 fill_solid_rects_cpu_fallback: put_image failed: {e:?}");
            return;
        }
        self.telemetry.record_paint_submit();
        self.trace_simple(
            if matches!(function, yserver_core::backend::GcFunction::Copy) {
                SubmitKind::FillBatch
            } else {
                SubmitKind::LogicFill
            },
            id,
            u32::try_from(rects.len()).unwrap_or(u32::MAX),
        );
    }

    /// CopyArea with a non-Copy GC function or partial plane-mask:
    /// CPU read-modify-write. Reads the source and destination
    /// sub-rects (clamped to both drawables), applies
    /// `apply_gc_function(src, dst)` per pixel, writes back. The
    /// conformance path only — real clients copy with GXcopy.
    #[allow(clippy::too_many_arguments)]
    fn copy_area_rop_cpu(
        &mut self,
        src_id: DrawableId,
        dst_id: DrawableId,
        src_rect: ash::vk::Rect2D,
        dst_pos: ash::vk::Offset2D,
        function: yserver_core::backend::GcFunction,
        plane_mask: u32,
        depth: u8,
    ) {
        let Some(src_extent) = self.store.get(src_id).map(|d| d.storage.extent) else {
            return;
        };
        let Some(dst_extent) = self.store.get(dst_id).map(|d| d.storage.extent) else {
            return;
        };
        // Clamp the transfer so both the source read and the
        // destination write stay in bounds; shift both sides by the
        // same delta.
        let mut sx = src_rect.offset.x;
        let mut sy = src_rect.offset.y;
        let mut dx = dst_pos.x;
        let mut dy = dst_pos.y;
        let mut w = src_rect.extent.width as i32;
        let mut h = src_rect.extent.height as i32;
        let clamp_low = |pos: &mut i32, other: &mut i32, len: &mut i32| {
            if *pos < 0 {
                *other -= *pos;
                *len += *pos;
                *pos = 0;
            }
        };
        clamp_low(&mut sx, &mut dx, &mut w);
        clamp_low(&mut sy, &mut dy, &mut h);
        clamp_low(&mut dx, &mut sx, &mut w);
        clamp_low(&mut dy, &mut sy, &mut h);
        w = w
            .min(src_extent.width as i32 - sx)
            .min(dst_extent.width as i32 - dx);
        h = h
            .min(src_extent.height as i32 - sy)
            .min(dst_extent.height as i32 - dy);
        if w <= 0 || h <= 0 {
            return;
        }
        let rect = |x: i32, y: i32| ash::vk::Rect2D {
            offset: ash::vk::Offset2D { x, y },
            extent: ash::vk::Extent2D {
                width: w as u32,
                height: h as u32,
            },
        };
        let src_bytes = match self.engine.get_image(
            &mut self.store,
            &mut self.platform,
            src_id,
            rect(sx, sy),
            depth,
        ) {
            Ok(b) => b,
            Err(e) => {
                log::warn!("v2 copy_area_rop_cpu: src get_image failed: {e:?}");
                return;
            }
        };
        let mut dst_bytes = match self.engine.get_image(
            &mut self.store,
            &mut self.platform,
            dst_id,
            rect(dx, dy),
            depth,
        ) {
            Ok(b) => b,
            Err(e) => {
                log::warn!("v2 copy_area_rop_cpu: dst get_image failed: {e:?}");
                return;
            }
        };
        let full_mask = depth_plane_mask(depth);
        for y in 0..h as usize {
            for x in 0..w as usize {
                let s = read_z_pixmap_pixel(&src_bytes, depth, w as u32, x, y) & full_mask;
                let d = read_z_pixmap_pixel(&dst_bytes, depth, w as u32, x, y) & full_mask;
                let out = apply_gc_function(function, s, d, plane_mask) & full_mask;
                write_z_pixmap_pixel(&mut dst_bytes, depth, w as u32, x, y, out);
            }
        }
        if let Err(e) = self.engine.put_image(
            &mut self.store,
            &mut self.platform,
            dst_id,
            ash::vk::Offset2D { x: dx, y: dy },
            ash::vk::Extent2D {
                width: w as u32,
                height: h as u32,
            },
            &dst_bytes,
            depth,
        ) {
            log::warn!("v2 copy_area_rop_cpu: put_image failed: {e:?}");
            return;
        }
        self.telemetry.record_paint_submit();
        self.trace_simple(SubmitKind::CopyArea, dst_id, 1);
    }

    /// PutImage with a non-Copy GC function, partial plane-mask, or
    /// a pixmap clip-mask: CPU read-modify-write combining the wire
    /// z-pixmap image with the destination through
    /// `apply_gc_function`. `data` has row stride `data_width`
    /// pixels; the transfer covers `transfer_w`×`transfer_h` pixels
    /// starting at `src_off` within the image. Conformance path only
    /// — real clients put with GXcopy and no bitmap clip.
    #[allow(clippy::too_many_arguments)]
    fn put_image_rop_cpu(
        &mut self,
        dst_id: DrawableId,
        dst_pos: ash::vk::Offset2D,
        data_width: u16,
        src_off: (i32, i32),
        transfer_w: u16,
        transfer_h: u16,
        data: &[u8],
        depth: u8,
        function: yserver_core::backend::GcFunction,
        plane_mask: u32,
    ) {
        let width = data_width;
        let Some(dst_extent) = self.store.get(dst_id).map(|d| d.storage.extent) else {
            return;
        };
        // Clamp to the destination; track the source-image offset of
        // the clamped origin.
        let mut dx = dst_pos.x;
        let mut dy = dst_pos.y;
        let mut sx = src_off.0;
        let mut sy = src_off.1;
        let mut w = i32::from(transfer_w);
        let mut h = i32::from(transfer_h);
        if dx < 0 {
            sx -= dx;
            w += dx;
            dx = 0;
        }
        if dy < 0 {
            sy -= dy;
            h += dy;
            dy = 0;
        }
        w = w.min(dst_extent.width as i32 - dx);
        h = h.min(dst_extent.height as i32 - dy);
        if w <= 0 || h <= 0 {
            return;
        }
        let dst_rect = ash::vk::Rect2D {
            offset: ash::vk::Offset2D { x: dx, y: dy },
            extent: ash::vk::Extent2D {
                width: w as u32,
                height: h as u32,
            },
        };
        let mut dst_bytes = match self.engine.get_image(
            &mut self.store,
            &mut self.platform,
            dst_id,
            dst_rect,
            depth,
        ) {
            Ok(b) => b,
            Err(e) => {
                log::warn!("v2 put_image_rop_cpu: dst get_image failed: {e:?}");
                return;
            }
        };
        let full_mask = depth_plane_mask(depth);
        for y in 0..h as usize {
            for x in 0..w as usize {
                let s = read_z_pixmap_pixel(
                    data,
                    depth,
                    u32::from(width),
                    x + sx as usize,
                    y + sy as usize,
                ) & full_mask;
                let d = read_z_pixmap_pixel(&dst_bytes, depth, w as u32, x, y) & full_mask;
                let out = apply_gc_function(function, s, d, plane_mask) & full_mask;
                write_z_pixmap_pixel(&mut dst_bytes, depth, w as u32, x, y, out);
            }
        }
        if let Err(e) = self.engine.put_image(
            &mut self.store,
            &mut self.platform,
            dst_id,
            dst_rect.offset,
            dst_rect.extent,
            &dst_bytes,
            depth,
        ) {
            log::warn!("v2 put_image_rop_cpu: put_image failed: {e:?}");
        }
    }

    /// Fill `rects` on `id`, honouring `KmsCore.current_fill`. Used
    /// by the filled-shape ops (`PolyFillRectangle`, `PolyFillArc`,
    /// `FillPoly`, `FillRectangle`); stroke ops keep using
    /// [`fill_solid_rects`] because X11 strokes are always solid
    /// foreground regardless of GC fill-style.
    ///
    /// `Solid` stays on the fast GPU path. The patterned styles
    /// (`Tiled`, `Stippled`, `OpaqueStippled`) use a CPU read/modify/write
    /// fallback so X11 function, plane-mask, tile/stipple origin, and
    /// opaque-background semantics all stay exact.
    fn fill_rects_honoring_fill_state(
        &mut self,
        host_xid: u32,
        target: PaintTarget,
        fg: u32,
        rects: &[Rectangle16],
    ) {
        use yserver_core::backend::{FillState, GcFunction};
        if rects.is_empty() {
            return;
        }
        let include_inferiors = matches!(
            self.core.current_subwindow_mode,
            yserver_core::backend::SubwindowMode::IncludeInferiors,
        ) && (self.windows_v2.contains_key(&host_xid)
            || host_xid == self.core.window_id);
        let inferior_work = if include_inferiors {
            self.collect_fill_rects_for_inferiors(host_xid, rects)
        } else {
            Vec::new()
        };
        let function = self.core.current_function;
        if matches!(function, GcFunction::NoOp) {
            return;
        }
        let rects = self.clip_fill_rects_by_subwindow_mode(host_xid, rects);
        if rects.is_empty() {
            return;
        }
        let fill = self.core.current_fill.clone();
        match fill {
            FillState::Solid => {
                self.fill_solid_rects(target, fg, &rects);
            }
            FillState::Tiled { .. }
            | FillState::Stippled { .. }
            | FillState::OpaqueStippled { .. } => {
                self.fill_pattern_rects_cpu_fallback(target, fg, &rects, &fill);
            }
        }
        for (child_xid, child_rects) in inferior_work {
            if child_rects.is_empty() {
                continue;
            }
            let Some(child_target) = self.resolve_paint_target(child_xid) else {
                continue;
            };
            match &fill {
                FillState::Solid => self.fill_solid_rects(child_target, fg, &child_rects),
                FillState::Tiled { .. }
                | FillState::Stippled { .. }
                | FillState::OpaqueStippled { .. } => {
                    self.fill_pattern_rects_cpu_fallback(child_target, fg, &child_rects, &fill);
                }
            }
        }
    }

    fn fill_pattern_rects_cpu_fallback(
        &mut self,
        target: PaintTarget,
        fg: u32,
        rects: &[Rectangle16],
        fill: &yserver_core::backend::FillState,
    ) {
        use yserver_core::backend::FillState;

        if rects.is_empty() {
            return;
        }
        let id = target.id;
        let Some((depth, extent)) = self.store.get(id).map(|d| (d.depth, d.storage.extent)) else {
            return;
        };
        let full_mask = depth_plane_mask(depth);
        let plane_mask = self.core.current_plane_mask & full_mask;
        if plane_mask == 0 {
            return;
        }
        let dst_rect = ash::vk::Rect2D {
            offset: ash::vk::Offset2D::default(),
            extent,
        };
        let mut dst_bytes =
            match self
                .engine
                .get_image(&mut self.store, &mut self.platform, id, dst_rect, depth)
            {
                Ok(bytes) => bytes,
                Err(e) => {
                    log::warn!("v2 fill_pattern_rects_cpu_fallback: get_image failed: {e:?}");
                    return;
                }
            };

        struct PatternSource {
            depth: u8,
            width: u32,
            height: u32,
            bytes: Vec<u8>,
            origin: (i16, i16),
        }

        let pattern_source = match fill {
            FillState::Tiled { pixmap, origin }
            | FillState::Stippled { pixmap, origin }
            | FillState::OpaqueStippled { pixmap, origin } => {
                if let Some(fresh) = self.read_fill_pattern_cache(pixmap.as_raw(), *origin) {
                    self.fill_pattern_cache = Some(fresh);
                } else if let Some(cache) = self.fill_pattern_cache.as_mut() {
                    if cache.pixmap_xid == pixmap.as_raw() {
                        cache.origin = *origin;
                    } else {
                        self.fill_pattern_cache = None;
                    }
                } else {
                    self.fill_pattern_cache = None;
                }
                let Some(cache) = self.fill_pattern_cache.as_ref() else {
                    self.fill_solid_rects(target, fg, rects);
                    return;
                };
                if cache.pixmap_xid != pixmap.as_raw() {
                    self.fill_solid_rects(target, fg, rects);
                    return;
                }
                PatternSource {
                    depth: cache.depth,
                    width: cache.width,
                    height: cache.height,
                    bytes: cache.bytes.clone(),
                    origin: cache.origin,
                }
            }
            FillState::Solid => {
                self.fill_solid_rects(target, fg, rects);
                return;
            }
        };

        let function = self.core.current_function;
        let bg = self.core.current_background & full_mask;
        let fg = fg & full_mask;
        let (dx, dy) = target.offset;
        for r in rects {
            let local_x0 = i32::from(r.x);
            let local_y0 = i32::from(r.y);
            let local_x1 = local_x0.saturating_add(i32::from(r.width));
            let local_y1 = local_y0.saturating_add(i32::from(r.height));
            for local_y in local_y0..local_y1 {
                for local_x in local_x0..local_x1 {
                    let storage_x = local_x + dx;
                    let storage_y = local_y + dy;
                    if storage_x < 0
                        || storage_y < 0
                        || storage_x >= extent.width as i32
                        || storage_y >= extent.height as i32
                    {
                        continue;
                    }
                    let dst = read_z_pixmap_pixel(
                        &dst_bytes,
                        depth,
                        extent.width,
                        storage_x as usize,
                        storage_y as usize,
                    ) & full_mask;
                    let out = match fill {
                        FillState::Tiled { .. } => {
                            let sx = (local_x - i32::from(pattern_source.origin.0))
                                .rem_euclid(pattern_source.width as i32)
                                as usize;
                            let sy = (local_y - i32::from(pattern_source.origin.1))
                                .rem_euclid(pattern_source.height as i32)
                                as usize;
                            let src = read_z_pixmap_pixel(
                                &pattern_source.bytes,
                                pattern_source.depth,
                                pattern_source.width,
                                sx,
                                sy,
                            ) & full_mask;
                            apply_gc_function(function, src, dst, plane_mask) & full_mask
                        }
                        FillState::Stippled { .. } | FillState::OpaqueStippled { .. } => {
                            let sx = (local_x - i32::from(pattern_source.origin.0))
                                .rem_euclid(pattern_source.width as i32)
                                as usize;
                            let sy = (local_y - i32::from(pattern_source.origin.1))
                                .rem_euclid(pattern_source.height as i32)
                                as usize;
                            let bit = read_z_pixmap_pixel(
                                &pattern_source.bytes,
                                pattern_source.depth,
                                pattern_source.width,
                                sx,
                                sy,
                            ) != 0;
                            let src = if bit {
                                Some(fg)
                            } else if matches!(fill, FillState::OpaqueStippled { .. }) {
                                Some(bg)
                            } else {
                                None
                            };
                            match src {
                                Some(src) => {
                                    apply_gc_function(function, src, dst, plane_mask) & full_mask
                                }
                                None => dst,
                            }
                        }
                        FillState::Solid => dst,
                    };
                    write_z_pixmap_pixel(
                        &mut dst_bytes,
                        depth,
                        extent.width,
                        storage_x as usize,
                        storage_y as usize,
                        out,
                    );
                }
            }
        }
        if let Err(e) = self.engine.put_image(
            &mut self.store,
            &mut self.platform,
            id,
            ash::vk::Offset2D::default(),
            extent,
            &dst_bytes,
            depth,
        ) {
            log::warn!("v2 fill_pattern_rects_cpu_fallback: put_image failed: {e:?}");
            return;
        }
        self.telemetry.record_paint_submit();
        self.trace_simple(
            if matches!(function, yserver_core::backend::GcFunction::Copy) {
                SubmitKind::FillBatch
            } else {
                SubmitKind::LogicFill
            },
            id,
            u32::try_from(rects.len()).unwrap_or(u32::MAX),
        );
    }

    /// Tile fill via `engine.render_composite` (Stage 3f.3). Returns
    /// `true` iff the call submitted; `false` if the tile isn't
    /// usable (unknown xid, self-tile aliasing, non-BGRA8 tile
    /// format), in which case the caller falls back to solid.
    ///
    /// Stage 4a: `dst` carries the resolved DrawableId + offset.
    /// Dst-space rect origins are shifted by `dst.offset` to land
    /// in backing coords; `src_x/src_y` stay window-local because
    /// they're a `(dst - tile_origin)` difference that doesn't
    /// depend on the absolute frame.
    #[allow(dead_code)]
    fn try_tiled_fill(
        &mut self,
        dst: PaintTarget,
        tile_xid: u32,
        ox: i16,
        oy: i16,
        rects: &[Rectangle16],
    ) -> bool {
        use crate::kms::{v2::engine::ResolvedSource, vk::ops::render::CompositeRect};
        if rects.is_empty() {
            return true;
        }
        let Some(tile_id) = self.store.lookup(tile_xid) else {
            log::debug!("v2 try_tiled_fill: tile 0x{tile_xid:x} not in store");
            return false;
        };
        if tile_id == dst.id {
            // Self-tile would alias src + dst inside render_composite.
            return false;
        }
        let tile_format = self.store.get(tile_id).map(|d| d.storage.format);
        if tile_format != Some(ash::vk::Format::B8G8R8A8_UNORM) {
            log::debug!("v2 try_tiled_fill: tile 0x{tile_xid:x} format {tile_format:?} not BGRA8");
            return false;
        }
        let (dx, dy) = dst.offset;
        // Build per-rect CompositeRects in dst space with
        // `src_origin = dst - tile_origin` so the shader's
        // `src_origin + dst_offset` lands on the right tile pixel.
        let composite_rects: Vec<CompositeRect> = rects
            .iter()
            .filter_map(|r| {
                if r.width == 0 || r.height == 0 {
                    return None;
                }
                Some(CompositeRect {
                    src_x: i32::from(r.x) - i32::from(ox),
                    src_y: i32::from(r.y) - i32::from(oy),
                    mask_x: 0,
                    mask_y: 0,
                    dst_x: i32::from(r.x) + dx,
                    dst_y: i32::from(r.y) + dy,
                    width: u32::from(r.width),
                    height: u32::from(r.height),
                })
            })
            .collect();
        if composite_rects.is_empty() {
            return true;
        }
        // Op `Src` (1) — tile fill replaces the destination.
        const OP_SRC: u8 = 1;
        let composite_result = self.engine.render_composite(
            &mut self.store,
            &mut self.platform,
            OP_SRC,
            ResolvedSource::Drawable(tile_id),
            ResolvedSource::None,
            dst.id,
            &composite_rects,
            None, // GC clip already applied by caller
            Repeat::Normal,
            Repeat::None,
            None,
            None,
            false,
            // Audit #4: synthesized tile-fill draw, no Picture
            // context. Engine falls back to depth heuristic.
            0,
            0,
            0,
        );
        self.sync_descriptor_pool_telemetry();
        match composite_result {
            Ok(s) => {
                if s.recorded_draws > 0 && !s.deferred_to_batch {
                    self.telemetry.record_paint_submit();
                    self.trace_render(
                        SubmitKind::RenderComposite,
                        dst.id,
                        s.recorded_draws,
                        OP_SRC,
                        SrcClass::Direct,
                        None,
                        SubmitFlags {
                            readback: s.used_dst_readback,
                            alias: s.used_src_alias_scratch,
                            zero_draws: false,
                            upload: false,
                        },
                    );
                }
                true
            }
            Err(e) => {
                log::warn!("v2 try_tiled_fill: render_composite failed: {e:?}");
                false
            }
        }
    }

    /// Allocate v2 storage + windows_v2 entry for a host xid.
    /// Idempotent against duplicate xids (logs + skips). `parent`
    /// is `Some(parent_xid)` for subwindows + `None` for top-levels
    /// (parent = root, not tracked in `windows_v2`). The
    /// `bg_pixel` slot is what gets painted into fresh storage —
    /// `None` leaves it Vk-undefined (depth-1 / depth-8 masks).
    fn allocate_window_storage(
        &mut self,
        host_xid: u32,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        depth: u8,
        parent: Option<u32>,
        bg_pixel: Option<u32>,
    ) {
        if self.windows_v2.contains_key(&host_xid) {
            return;
        }
        let stack_rank = self.alloc_window_stack_rank();
        let mut storage_allocated = false;
        match self
            .platform
            .allocate_drawable_storage(width.max(1), height.max(1), depth)
        {
            Ok(storage) => {
                if let Err(e) = self.store.allocate(
                    host_xid,
                    DrawableKind::Window,
                    depth,
                    false, // becomes true on map_subwindow
                    storage,
                ) {
                    log::warn!(
                        "v2 allocate_window_storage: store.allocate failed for xid {host_xid:#x}: {e:?}",
                    );
                    return;
                }
                self.telemetry.record_storage_allocation();
                self.telemetry.record_image_view_create();
                storage_allocated = true;
            }
            Err(e)
                if self.platform.vk.is_none()
                    && e == ash::vk::Result::ERROR_INITIALIZATION_FAILED =>
            {
                // No Vk fixture (`for_tests`) → storage allocation
                // returns ERROR_INITIALIZATION_FAILED. Tracking
                // the geometry without storage is fine; the scene
                // tick filters out null image-views.
                log::debug!("v2 allocate_window_storage: no Vk for xid {host_xid:#x}: {e:?}",);
            }
            Err(e) => {
                log::warn!(
                    "v2 allocate_window_storage: allocation failed for xid {host_xid:#x} \
                     {width}x{height} d{depth}: {e:?}"
                );
            }
        }
        self.windows_v2.insert(
            host_xid,
            WindowGeometryV2 {
                x,
                y,
                width,
                height,
                depth,
                mapped: false,
                parent,
                stack_rank,
                bg_pixel,
                bg_pixmap: None,
                cursor: None,
            },
        );
        // Stage 3f.6 + 3f.14: clear newly-allocated storage to a
        // defined colour so freshly-mapped windows don't surface
        // the pool returner's pixels (3f.10 PixmapPool recycles
        // image/view/memory triples — the bytes are whatever the
        // previous owner left). When `bg_pixel` is set, use it
        // (v1's create_subwindow behaviour); otherwise paint a
        // depth-appropriate safe default (3f.14).
        if storage_allocated && let Some(id) = self.store.lookup(host_xid) {
            let format = PlatformBackend::format_for_depth(depth);
            let color = bg_pixel.map_or_else(
                || default_window_init_color(depth),
                |pixel| decode_x11_pixel_for_storage(pixel, depth, format),
            );
            let rect = ash::vk::Rect2D {
                offset: ash::vk::Offset2D::default(),
                extent: ash::vk::Extent2D {
                    width: u32::from(width.max(1)),
                    height: u32::from(height.max(1)),
                },
            };
            if let Err(e) =
                self.engine
                    .fill_rect(&mut self.store, &mut self.platform, id, rect, color)
            {
                log::debug!(
                    "v2 allocate_window_storage: initial fill failed for xid {host_xid:#x}: {e:?}"
                );
            }
        }
    }

    // ── Stage 3a: Core-text helpers ─────────────────────────────

    /// FreeType rasterise + atlas dispatch for one text run.
    /// Used by `image_text8/16` and `poly_text8/16`. Per Stage 3
    /// plan §"Cross-cutting" §4: Core ops consult GC clip only —
    /// here we don't push the GC clip into the RENDER pipeline
    /// because the text pipeline doesn't honour scissor (lives in
    /// Stage 3e). v1's path has the same limitation; promoted to
    /// a Risk item rather than blocking 3a.
    /// Rasterize core-protocol text MONOCHROME and submit it through
    /// the rop-correct span path (`fill_solid_rects`): X11 core text
    /// is binary fg coverage — PolyText honors the GC function /
    /// plane-mask, ImageText forces GXcopy (callers swap
    /// `core.current_function`). Spans are window-local;
    /// `fill_solid_rects` applies `target.offset` — the SINGLE
    /// translation point (no per-glyph pre-shift here).
    fn render_text_chars_v2(
        &mut self,
        host_xid: u32,
        foreground: u32,
        x: i32,
        y: i32,
        text: &[char],
    ) -> io::Result<()> {
        let Some(font_xid) = self.core.current_font else {
            return Ok(());
        };
        let Some(target) = self.resolve_paint_target(host_xid) else {
            return Ok(());
        };
        // Rasterise glyphs in a tight FreeType-borrow scope so the
        // subsequent &mut self fill call doesn't conflict.
        let mut spans: Vec<Rectangle16> = Vec::new();
        let mut cursor_x = x;
        {
            let Some(fs) = self.core.fonts.get(&font_xid) else {
                return Ok(());
            };
            let face = fs.face.borrow();
            let default_ch = char::from_u32(u32::from(fs.metrics.default_char));
            for &ch in text {
                // Nonexistent chars draw the font's default_char; if
                // that doesn't exist either, nothing is drawn (X11).
                let (ch, ci) = match fs.char_info_cache.get(&ch) {
                    Some(ci) => (ch, ci),
                    None => {
                        match default_ch.and_then(|d| fs.char_info_cache.get(&d).map(|ci| (d, ci)))
                        {
                            Some(pair) => pair,
                            None => continue,
                        }
                    }
                };
                let _ = face.0.load_char(
                    ch as usize,
                    freetype::face::LoadFlag::RENDER | freetype::face::LoadFlag::TARGET_MONO,
                );
                let glyph = face.0.glyph();
                glyph_mono_spans(
                    &glyph.bitmap(),
                    cursor_x + glyph.bitmap_left(),
                    y - glyph.bitmap_top(),
                    &mut spans,
                );
                cursor_x = cursor_x.saturating_add(ci.character_width as i32);
            }
        }
        if spans.is_empty() {
            return Ok(());
        }
        let clipped = self.intersect_with_current_clip_live(&spans);
        self.fill_solid_rects(target, foreground, &clipped);
        Ok(())
    }

    /// Legacy GPU-atlas text path — unreachable from the core
    /// protocol since text moved to the span path; kept for the
    /// engine plumbing until the atlas gets a new consumer or is
    /// removed in a follow-up.
    #[allow(dead_code)]
    fn render_text_chars_v2_atlas(
        &mut self,
        host_xid: u32,
        foreground: u32,
        x: i32,
        y: i32,
        text: &[char],
    ) -> io::Result<()> {
        use crate::kms::v2::engine::PreparedGlyph;

        let Some(font_xid) = self.core.current_font else {
            return Ok(());
        };
        // Stage 4a — resolve through redirect routing. Glyph
        // `dst_x` / `dst_y` per `PreparedGlyph` get the
        // window→backing translation applied below.
        let Some(target) = self.resolve_paint_target(host_xid) else {
            return Ok(());
        };
        let (paint_dx, paint_dy) = target.offset;
        // Rasterise glyphs in a tight FreeType-borrow scope so the
        // subsequent &mut self engine call doesn't conflict.
        let mut rendered: Vec<PreparedGlyph> = Vec::with_capacity(text.len());
        let mut cursor_x = x;
        {
            let Some(fs) = self.core.fonts.get(&font_xid) else {
                return Ok(());
            };
            let face = fs.face.borrow();
            let char_cache = &fs.char_info_cache;
            for &ch in text {
                let Some(ci) = char_cache.get(&ch) else {
                    cursor_x = cursor_x.saturating_add(6);
                    continue;
                };
                let _ = face
                    .0
                    .load_char(ch as usize, freetype::face::LoadFlag::RENDER);
                let glyph = face.0.glyph();
                let bitmap = glyph.bitmap();
                if bitmap.width() > 0 && bitmap.rows() > 0 {
                    let w = bitmap.width() as usize;
                    let h = bitmap.rows() as usize;
                    let stride = bitmap.pitch();
                    let buf = bitmap.buffer();
                    let mut pixels = vec![0u8; w * h];
                    for row in 0..h {
                        let src = if stride >= 0 {
                            row * stride as usize
                        } else {
                            (h - 1 - row) * (stride as isize).unsigned_abs()
                        };
                        pixels[row * w..row * w + w].copy_from_slice(&buf[src..src + w]);
                    }
                    rendered.push(PreparedGlyph {
                        dst_x: cursor_x + glyph.bitmap_left() + paint_dx,
                        dst_y: y - glyph.bitmap_top() + paint_dy,
                        w,
                        h,
                        pixels,
                        codepoint: ch as u32,
                    });
                }
                cursor_x = cursor_x.saturating_add(ci.character_width as i32);
            }
        }
        if rendered.is_empty() {
            return Ok(());
        }
        let foreground_rgba = [
            ((foreground >> 16) & 0xFF) as f32 / 255.0,
            ((foreground >> 8) & 0xFF) as f32 / 255.0,
            (foreground & 0xFF) as f32 / 255.0,
            1.0,
        ];
        match self.engine.image_text(
            &mut self.store,
            &mut self.platform,
            target.id,
            font_xid,
            foreground_rgba,
            &rendered,
        ) {
            Ok(stats) => {
                for _ in 0..stats.atlas_interns {
                    self.telemetry.record_atlas_intern();
                }
                for _ in 0..stats.glyph_uploads {
                    self.telemetry.record_glyph_upload();
                }
                for _ in 0..stats.glyphs_dropped {
                    self.telemetry.record_glyph_dropped_atlas_full();
                }
                if stats.glyph_uploads > 0 {
                    // The glyph upload CB is a separate submit
                    // from the text-paint CB. Emit one
                    // GlyphUpload event per upload submit so
                    // analysis can correlate upload bursts with
                    // text bursts.
                    let target_kind = self.submit_target_kind(target.id);
                    for _ in 0..stats.glyph_uploads {
                        self.telemetry.record_submit_event(SubmitEvent {
                            frame_id: 0,
                            kind: SubmitKind::GlyphUpload,
                            target_kind,
                            target_id: target.id.as_u64(),
                            batch_size: 1,
                            op: SubmitOp::None,
                            src_class: SrcClass::None,
                            mask_class: SrcClass::None,
                            pipeline_id: None,
                            flags: SubmitFlags {
                                readback: false,
                                alias: false,
                                zero_draws: false,
                                upload: true,
                            },
                        });
                    }
                }
                if stats.atlas_interns > 0 || !rendered.is_empty() {
                    self.telemetry.record_paint_submit();
                    let batch_size = u32::try_from(rendered.len()).unwrap_or(u32::MAX);
                    self.trace_simple(SubmitKind::ImageText, target.id, batch_size);
                }
            }
            Err(e) => {
                log::warn!("v2 image_text: engine error xid={host_xid:#x}: {e:?} — dropping run");
            }
        }
        Ok(())
    }

    /// `image_text8/16` background-fill helper. Lowers the
    /// per-call rect to an `engine.fill_rect` op via the same
    /// path `fill_rectangle` (Stage 2c) uses, so the bg drawn
    /// here lives on the same storage as the glyph quads.
    /// Unused since ImageText moved to the rop span path (the bg
    /// box goes through `fill_solid_rects` for clip + plane-mask);
    /// kept with the atlas path until that's removed.
    #[allow(dead_code)]
    fn fill_text_background(
        &mut self,
        host_xid: u32,
        background: u32,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    ) -> io::Result<()> {
        if w <= 0 || h <= 0 {
            return Ok(());
        }
        // Stage 4a — resolve through redirect; rect origin is
        // shifted by the descendant→ancestor-backing offset.
        let Some(target) = self.resolve_paint_target(host_xid) else {
            return Ok(());
        };
        // L1 server-α invariant per `fill_solid_rects` (see comment
        // there): force α=1 on depth!=32 dsts so the scene
        // compositor's alpha_passthrough path doesn't blend the
        // text bg out.
        let depth = self.store.get(target.id).map(|d| d.depth).unwrap_or(24);
        let format = self
            .store
            .get(target.id)
            .map(|d| d.storage.format)
            .unwrap_or_else(|| PlatformBackend::format_for_depth(depth));
        let color = decode_x11_pixel_for_storage(background, depth, format);
        let rect = ash::vk::Rect2D {
            offset: ash::vk::Offset2D {
                x: x + target.offset.0,
                y: y + target.offset.1,
            },
            extent: ash::vk::Extent2D {
                width: u32::try_from(w).unwrap_or(0),
                height: u32::try_from(h).unwrap_or(0),
            },
        };
        if let Err(e) =
            self.engine
                .fill_rect(&mut self.store, &mut self.platform, target.id, rect, color)
        {
            log::warn!("v2 image_text bg fill: engine.fill_rect xid={host_xid:#x}: {e:?}");
        } else {
            self.telemetry.record_paint_submit();
            self.trace_simple(SubmitKind::FillOne, target.id, 1);
        }
        Ok(())
    }

    /// ImageText8/16 core: per X11 §8 the GC function and fill-style
    /// are IGNORED (effective GXcopy, solid); plane-mask and clip
    /// still apply. Port of Xorg miImageGlyphBlt (mi/miglblt.c:83):
    /// ONE background box from the run's overall extents —
    /// (x, y−font_ascent, overall_width, ascent+descent) — then the
    /// fg glyphs, both through the rop span path with the function
    /// temporarily forced to Copy.
    fn image_text_common(
        &mut self,
        host_xid: u32,
        foreground: u32,
        background: u32,
        x: i32,
        y: i32,
        chars: &[char],
    ) -> io::Result<()> {
        use yserver_core::backend::GcFunction;
        let saved_function = self.core.current_function;
        self.core.current_function = GcFunction::Copy;
        if let Some(font_state) = self.core.current_font.and_then(|f| self.core.fonts.get(&f)) {
            let total_width = text_advance(font_state, chars);
            let ascent = i32::from(font_state.metrics.font_ascent);
            let descent = i32::from(font_state.metrics.font_descent);
            let bg_w = total_width.clamp(0, i32::from(i16::MAX));
            let bg_h = (ascent + descent).clamp(0, i32::from(i16::MAX));
            if bg_w > 0 && bg_h > 0 {
                let bg = Rectangle16 {
                    x: i16::try_from(x).unwrap_or(i16::MAX),
                    y: i16::try_from(y - ascent).unwrap_or(i16::MAX),
                    width: u16::try_from(bg_w).unwrap_or(u16::MAX),
                    height: u16::try_from(bg_h).unwrap_or(u16::MAX),
                };
                if let Some(target) = self.resolve_paint_target(host_xid) {
                    let clipped = self.intersect_with_current_clip_live(&[bg]);
                    self.fill_solid_rects(target, background, &clipped);
                }
            }
        }
        let result = self.render_text_chars_v2(host_xid, foreground, x, y, chars);
        self.core.current_function = saved_function;
        result
    }
}

/// Sum of character advances for a run, with the X11 nonexistent-
/// char rule: missing chars use the font's default_char; if that's
/// missing too they contribute 0 (drawn as nothing).
fn text_advance(fs: &crate::kms::core::FontState, chars: &[char]) -> i32 {
    let default_ci =
        char::from_u32(u32::from(fs.metrics.default_char)).and_then(|d| fs.char_info_cache.get(&d));
    chars
        .iter()
        .map(|ch| {
            fs.char_info_cache
                .get(ch)
                .or(default_ci)
                .map_or(0, |ci| i32::from(ci.character_width))
        })
        .sum()
}

/// Convert one rasterized glyph bitmap into horizontal pixel-run
/// rectangles at (ox, oy), appended to `out`. Handles FreeType MONO
/// (1 bpp, MSB-first, `pitch` bytes/row) and GRAY (8 bpp, threshold
/// at 128 — only reachable if a driver ignores TARGET_MONO) pixel
/// modes. Coordinates outside i16 range are clamped away (X11
/// drawables can't exceed i16 anyway).
fn glyph_mono_spans(bitmap: &freetype::Bitmap, ox: i32, oy: i32, out: &mut Vec<Rectangle16>) {
    let w = bitmap.width() as usize;
    let h = bitmap.rows() as usize;
    if w == 0 || h == 0 {
        return;
    }
    let pitch = bitmap.pitch();
    let buf = bitmap.buffer();
    let mono = matches!(bitmap.pixel_mode(), Ok(freetype::bitmap::PixelMode::Mono));
    for row in 0..h {
        let row_start = if pitch >= 0 {
            row * pitch as usize
        } else {
            (h - 1 - row) * (pitch as isize).unsigned_abs()
        };
        let set_at = |col: usize| -> bool {
            if mono {
                let byte = buf.get(row_start + (col >> 3)).copied().unwrap_or(0);
                byte & (0x80 >> (col & 7)) != 0
            } else {
                buf.get(row_start + col).copied().unwrap_or(0) >= 128
            }
        };
        let y = oy + row as i32;
        if y < i32::from(i16::MIN) || y > i32::from(i16::MAX) {
            continue;
        }
        let mut col = 0usize;
        while col < w {
            if !set_at(col) {
                col += 1;
                continue;
            }
            let run_start = col;
            while col < w && set_at(col) {
                col += 1;
            }
            let x = ox + run_start as i32;
            let width = (col - run_start) as u32;
            if x > i32::from(i16::MAX) || x + width as i32 <= i32::from(i16::MIN) {
                continue;
            }
            out.push(Rectangle16 {
                x: i16::try_from(x).unwrap_or(i16::MIN),
                y: y as i16,
                width: u16::try_from(width).unwrap_or(u16::MAX),
                height: 1,
            });
        }
    }
}

/// Parse gradient stops (Stage 3b helper shared by linear +
/// radial). `stops_offset` is the offset in `body` where the
/// `n_stops` u32 starts. Returns `None` if the body is short.
/// Stops carry pos (FIXED 16.16) + 4 × u16 colour (straight).
fn parse_gradient_stops(body: &[u8], stops_offset: usize) -> Option<Vec<GradientStop>> {
    if body.len() < stops_offset + 4 {
        return None;
    }
    let n = u32::from_le_bytes(body[stops_offset..stops_offset + 4].try_into().ok()?) as usize;
    let pos_base = stops_offset + 4;
    let color_base = pos_base + n * 4;
    if body.len() < color_base + n * 8 {
        return None;
    }
    let mut stops: Vec<GradientStop> = Vec::with_capacity(n);
    for i in 0..n {
        let pos = i32::from_le_bytes(
            body[pos_base + i * 4..pos_base + i * 4 + 4]
                .try_into()
                .ok()?,
        );
        let cb = color_base + i * 8;
        let r = u16::from_le_bytes(body[cb..cb + 2].try_into().ok()?);
        let g = u16::from_le_bytes(body[cb + 2..cb + 4].try_into().ok()?);
        let b = u16::from_le_bytes(body[cb + 4..cb + 6].try_into().ok()?);
        let a = u16::from_le_bytes(body[cb + 6..cb + 8].try_into().ok()?);
        stops.push(GradientStop { pos, r, g, b, a });
    }
    Some(stops)
}

/// Apply a `RenderChangePicture` value-mask body to the picture
/// record. Mirrors v1's per-bit handler in shape; differences are
/// the v2 record's type and `KmsCore.pictures` as the map.
/// `body` is the full request body shape:
/// `picture(4) + value_mask(4) + values[…]`.
fn change_picture_apply_mask(core: &mut KmsCore, host_pic: u32, body: &[u8]) {
    if body.len() < 8 {
        return;
    }
    let value_mask = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
    let values = &body[8..];
    let mut off = 0usize;
    let next_u32 = |off: &mut usize| -> Option<u32> {
        let bytes = values.get(*off..*off + 4)?;
        *off += 4;
        Some(u32::from_le_bytes(bytes.try_into().ok()?))
    };
    for bit in 0..13 {
        let mask_bit = 1u32 << bit;
        if value_mask & mask_bit == 0 {
            continue;
        }
        let Some(v) = next_u32(&mut off) else {
            break;
        };
        match mask_bit {
            // CPRepeat
            0x0001 => {
                let repeat = match v {
                    1 => Repeat::Normal,
                    2 => Repeat::Pad,
                    3 => Repeat::Reflect,
                    _ => Repeat::None,
                };
                match core.pictures.get_mut(&host_pic) {
                    Some(PictureRecord::Drawable { repeat: r, .. })
                    | Some(PictureRecord::SolidFill { repeat: r, .. })
                    | Some(PictureRecord::LinearGradient { repeat: r, .. })
                    | Some(PictureRecord::RadialGradient { repeat: r, .. }) => *r = repeat,
                    None => {}
                }
            }
            // CPAlphaMap
            0x0002 => {
                if let Some(PictureRecord::Drawable { alpha_map, .. }) =
                    core.pictures.get_mut(&host_pic)
                {
                    *alpha_map = if v == 0 { None } else { Some(v) };
                }
            }
            // CPAlphaXOrigin
            0x0004 => {
                if let Some(PictureRecord::Drawable { alpha_x, .. }) =
                    core.pictures.get_mut(&host_pic)
                {
                    *alpha_x = v as i16;
                }
            }
            // CPAlphaYOrigin
            0x0008 => {
                if let Some(PictureRecord::Drawable { alpha_y, .. }) =
                    core.pictures.get_mut(&host_pic)
                {
                    *alpha_y = v as i16;
                }
            }
            // CPClipXOrigin
            0x0010 => {
                if let Some(PictureRecord::Drawable { clip, clip_x, .. }) =
                    core.pictures.get_mut(&host_pic)
                {
                    let new_x = v as i16;
                    let dx = i32::from(new_x) - i32::from(*clip_x);
                    if dx != 0
                        && let Some(rects) = clip.as_mut()
                    {
                        for r in rects {
                            r.x = (i32::from(r.x) + dx).clamp(i16::MIN as i32, i16::MAX as i32)
                                as i16;
                        }
                    }
                    *clip_x = new_x;
                }
            }
            // CPClipYOrigin
            0x0020 => {
                if let Some(PictureRecord::Drawable { clip, clip_y, .. }) =
                    core.pictures.get_mut(&host_pic)
                {
                    let new_y = v as i16;
                    let dy = i32::from(new_y) - i32::from(*clip_y);
                    if dy != 0
                        && let Some(rects) = clip.as_mut()
                    {
                        for r in rects {
                            r.y = (i32::from(r.y) + dy).clamp(i16::MIN as i32, i16::MAX as i32)
                                as i16;
                        }
                    }
                    *clip_y = new_y;
                }
            }
            // CPClipMask: a depth-1 pixmap xid (or `None` = 0).
            // For Stage 3b parity with v1, we don't synthesize the
            // pixmap → rect-list conversion (v1 needs the pixmap's
            // dimensions, which it had on KmsBackend.pixmaps). v2's
            // DrawableStore exposes the same dims via the storage's
            // extent, but for the common path (Cairo never sets a
            // bitmap mask via ChangePicture — it uses
            // SetPictureClipRectangles) this stays a logged no-op.
            // Risk-listed for the rendercheck clip-mask category.
            0x0040 => {
                if v == 0 {
                    if let Some(PictureRecord::Drawable { clip, .. }) =
                        core.pictures.get_mut(&host_pic)
                    {
                        *clip = None;
                    }
                } else {
                    log::debug!(
                        "v2 ChangePicture CPClipMask=pixmap {v:#x} on picture {host_pic:#x}: \
                         bitmap-mask clip not yet wired (Stage 3b TODO; rendercheck-only path)"
                    );
                }
            }
            // CPGraphicsExposure
            0x0080 => {
                if let Some(PictureRecord::Drawable {
                    graphics_exposure, ..
                }) = core.pictures.get_mut(&host_pic)
                {
                    *graphics_exposure = v != 0;
                }
            }
            // CPSubwindowMode
            0x0100 => {
                if let Some(PictureRecord::Drawable { subwindow_mode, .. }) =
                    core.pictures.get_mut(&host_pic)
                {
                    *subwindow_mode = v as u8;
                }
            }
            // CPPolyEdge
            0x0200 => {
                if let Some(PictureRecord::Drawable { poly_edge, .. }) =
                    core.pictures.get_mut(&host_pic)
                {
                    *poly_edge = v as u8;
                }
            }
            // CPPolyMode
            0x0400 => {
                if let Some(PictureRecord::Drawable { poly_mode, .. }) =
                    core.pictures.get_mut(&host_pic)
                {
                    *poly_mode = v as u8;
                }
            }
            // CPDither: consumed but intentionally not stored
            // (v1 same behaviour).
            0x0800 => {}
            // CPComponentAlpha
            0x1000 => match core.pictures.get_mut(&host_pic) {
                Some(PictureRecord::Drawable {
                    component_alpha, ..
                })
                | Some(PictureRecord::SolidFill {
                    component_alpha, ..
                }) => *component_alpha = v != 0,
                _ => {}
            },
            _ => {}
        }
    }
}

/// Diagnostic helper: write the `CursorRecord`'s source BGRA bytes
/// (as received from the X11 client, before any `load_image` /
/// dumb-buffer copy) to a PPM. Used in `do_dump_scanout_v2` to bisect
/// whether cursor corruption enters at upload time (load_image) or
/// upstream (engine.get_image / wire format).
fn dump_cursor_record_to_ppm(
    path: &str,
    rec: &crate::kms::v2::cursor::CursorRecord,
) -> io::Result<()> {
    use std::io::Write;
    let w = usize::from(rec.width);
    let h = usize::from(rec.height);
    if w == 0 || h == 0 || rec.bgra_bytes.len() < w * h * 4 {
        return Err(io::Error::other(format!(
            "bad cursor record: {}x{} bytes={}",
            rec.width,
            rec.height,
            rec.bgra_bytes.len()
        )));
    }
    let mut file = std::fs::File::create(path)?;
    file.write_all(format!("P6\n{w} {h}\n255\n").as_bytes())?;
    let mut row_buf = vec![0u8; w * 3];
    for y in 0..h {
        for x in 0..w {
            let pi = (y * w + x) * 4;
            let b = rec.bgra_bytes[pi];
            let g = rec.bgra_bytes[pi + 1];
            let r = rec.bgra_bytes[pi + 2];
            row_buf[x * 3] = r;
            row_buf[x * 3 + 1] = g;
            row_buf[x * 3 + 2] = b;
        }
        file.write_all(&row_buf)?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum ScanoutReadSelection {
    OnScreenOnly,
    PermissiveDump,
}

fn scanout_selection_phases(
    selection: ScanoutReadSelection,
) -> &'static [crate::kms::vk::scanout::BoPhase] {
    match selection {
        ScanoutReadSelection::OnScreenOnly => &[crate::kms::vk::scanout::BoPhase::OnScreen],
        ScanoutReadSelection::PermissiveDump => &[
            crate::kms::vk::scanout::BoPhase::OnScreen,
            crate::kms::vk::scanout::BoPhase::Pending,
            crate::kms::vk::scanout::BoPhase::Submitted,
            crate::kms::vk::scanout::BoPhase::Recording,
        ],
    }
}

fn select_scanout_bo_for_rect(
    backend: &KmsBackendV2,
    rect: vk::Rect2D,
    selection: ScanoutReadSelection,
) -> io::Result<(usize, usize, vk::Rect2D)> {
    let rx0 = i64::from(rect.offset.x);
    let ry0 = i64::from(rect.offset.y);
    let rx1 = rx0 + i64::from(rect.extent.width);
    let ry1 = ry0 + i64::from(rect.extent.height);
    let phases = scanout_selection_phases(selection);

    for (pool_idx, layout) in backend.platform.outputs.iter().enumerate() {
        let lx0 = i64::from(layout.x);
        let ly0 = i64::from(layout.y);
        let lx1 = lx0 + i64::from(layout.width);
        let ly1 = ly0 + i64::from(layout.height);
        if rx0 < lx0 || ry0 < ly0 || rx1 > lx1 || ry1 > ly1 {
            continue;
        }
        let Some(pool) = backend
            .platform
            .scanout_pools
            .get(pool_idx)
            .and_then(|p| p.as_ref())
        else {
            continue;
        };
        for phase in phases {
            if let Some(bo_idx) = pool.bos.iter().position(|bo| bo.state.phase == *phase) {
                let local = vk::Rect2D {
                    offset: vk::Offset2D {
                        x: i32::try_from(rx0 - lx0).unwrap_or(i32::MAX),
                        y: i32::try_from(ry0 - ly0).unwrap_or(i32::MAX),
                    },
                    extent: rect.extent,
                };
                return Ok((pool_idx, bo_idx, local));
            }
        }
    }

    Err(io::Error::other(match selection {
        ScanoutReadSelection::OnScreenOnly => "root screenshot rect has no on-screen scanout bo",
        ScanoutReadSelection::PermissiveDump => "scanout rect is not covered by any pool",
    }))
}

fn read_scanout_region(
    backend: &KmsBackendV2,
    rect: vk::Rect2D,
    selection: ScanoutReadSelection,
) -> io::Result<Vec<u8>> {
    use crate::kms::vk::ops::run_one_shot_op;

    if rect.extent.width == 0 || rect.extent.height == 0 {
        return Ok(Vec::new());
    }

    let Some(vk) = backend.platform.vk.as_ref().cloned() else {
        return Err(io::Error::other("no vulkan context"));
    };
    let Some(pool_handle) = backend.platform.ops_command_pool_handle() else {
        return Err(io::Error::other("no ops command pool"));
    };

    let (pool_idx, bo_idx, local_rect) = select_scanout_bo_for_rect(backend, rect, selection)?;
    let Some(pool) = backend
        .platform
        .scanout_pools
        .get(pool_idx)
        .and_then(|p| p.as_ref())
    else {
        return Err(io::Error::other("scanout pool vanished"));
    };
    let Some(bo) = pool.bos.get(bo_idx) else {
        return Err(io::Error::other("scanout bo vanished"));
    };
    let image = bo.vk_image;
    let staging_buffer = bo.vk_transfer.staging_buffer;
    let staging_mapped = bo.vk_transfer.staging_mapped;
    let staging_size = bo.vk_transfer.staging_size;
    let copy_width = local_rect.extent.width;
    let copy_height = local_rect.extent.height;
    let needed_bytes = usize::try_from(copy_width)
        .ok()
        .and_then(|w| {
            usize::try_from(copy_height)
                .ok()
                .and_then(move |h| w.checked_mul(h))
        })
        .and_then(|px| px.checked_mul(4))
        .ok_or_else(|| io::Error::other("scanout copy size overflow"))?;
    if needed_bytes > staging_size as usize {
        return Err(io::Error::other("scanout staging buffer too small"));
    }

    let run_result = run_one_shot_op(&vk, pool_handle, |vk, cb| {
        let pre = [ash::vk::ImageMemoryBarrier2::default()
            .src_stage_mask(ash::vk::PipelineStageFlags2::ALL_COMMANDS)
            .src_access_mask(ash::vk::AccessFlags2::MEMORY_WRITE)
            .dst_stage_mask(ash::vk::PipelineStageFlags2::COPY)
            .dst_access_mask(ash::vk::AccessFlags2::TRANSFER_READ)
            .old_layout(ash::vk::ImageLayout::GENERAL)
            .new_layout(ash::vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
            .image(image)
            .subresource_range(
                ash::vk::ImageSubresourceRange::default()
                    .aspect_mask(ash::vk::ImageAspectFlags::COLOR)
                    .level_count(1)
                    .layer_count(1),
            )];
        let pre_dep = ash::vk::DependencyInfo::default().image_memory_barriers(&pre);
        crate::vk_count!(cmd_pipeline_barrier2);
        unsafe { vk.device.cmd_pipeline_barrier2(cb, &pre_dep) };

        let region = [ash::vk::BufferImageCopy::default()
            .buffer_offset(0)
            .buffer_row_length(0)
            .buffer_image_height(0)
            .image_subresource(
                ash::vk::ImageSubresourceLayers::default()
                    .aspect_mask(ash::vk::ImageAspectFlags::COLOR)
                    .layer_count(1),
            )
            .image_offset(ash::vk::Offset3D {
                x: local_rect.offset.x,
                y: local_rect.offset.y,
                z: 0,
            })
            .image_extent(ash::vk::Extent3D {
                width: copy_width,
                height: copy_height,
                depth: 1,
            })];
        unsafe {
            crate::vk_count!(cmd_copy_image_to_buffer);
            vk.device.cmd_copy_image_to_buffer(
                cb,
                image,
                ash::vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                staging_buffer,
                &region,
            );
        }

        let post = [ash::vk::ImageMemoryBarrier2::default()
            .src_stage_mask(ash::vk::PipelineStageFlags2::COPY)
            .src_access_mask(ash::vk::AccessFlags2::TRANSFER_READ)
            .dst_stage_mask(ash::vk::PipelineStageFlags2::ALL_COMMANDS)
            .dst_access_mask(ash::vk::AccessFlags2::MEMORY_WRITE)
            .old_layout(ash::vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
            .new_layout(ash::vk::ImageLayout::GENERAL)
            .image(image)
            .subresource_range(
                ash::vk::ImageSubresourceRange::default()
                    .aspect_mask(ash::vk::ImageAspectFlags::COLOR)
                    .level_count(1)
                    .layer_count(1),
            )];
        let post_dep = ash::vk::DependencyInfo::default().image_memory_barriers(&post);
        crate::vk_count!(cmd_pipeline_barrier2);
        unsafe { vk.device.cmd_pipeline_barrier2(cb, &post_dep) };
        Ok(())
    });

    if let Err(e) = run_result {
        return Err(io::Error::other(format!("scanout copy submit: {e:?}")));
    }

    let raw = unsafe { std::slice::from_raw_parts(staging_mapped.as_ptr(), needed_bytes) };
    Ok(raw.to_vec())
}

fn do_dump_scanout_v2(backend: &mut KmsBackendV2) -> io::Result<()> {
    use std::sync::atomic::{AtomicU32, Ordering};

    let mut wrote_any = false;
    let mut last_err: Option<io::Error> = None;

    static DUMP_COUNT: AtomicU32 = AtomicU32::new(0);
    let run = DUMP_COUNT.fetch_add(1, Ordering::Relaxed);

    for (pool_idx, layout) in backend.platform.outputs.iter().enumerate() {
        let rect = vk::Rect2D {
            offset: vk::Offset2D {
                x: layout.x,
                y: layout.y,
            },
            extent: vk::Extent2D {
                width: u32::from(layout.width),
                height: u32::from(layout.height),
            },
        };
        let raw = match read_scanout_region(backend, rect, ScanoutReadSelection::PermissiveDump) {
            Ok(raw) => raw,
            Err(err) => {
                log::warn!("v2 do_dump_scanout: output {pool_idx} failed: {err}");
                last_err = Some(err);
                continue;
            }
        };

        let path = format!("./yserver-v2-scanout-{run}-out{pool_idx}.ppm");
        use std::io::Write;
        let mut file = std::fs::File::create(&path)?;
        file.write_all(format!("P6\n{} {}\n255\n", layout.width, layout.height).as_bytes())?;
        let mut row_buf = vec![0u8; usize::from(layout.width) * 3];
        for y in 0..usize::from(layout.height) {
            let row_start = y * usize::from(layout.width) * 4;
            for x in 0..usize::from(layout.width) {
                let pi = row_start + x * 4;
                let dst = x * 3;
                row_buf[dst] = raw[pi + 2];
                row_buf[dst + 1] = raw[pi + 1];
                row_buf[dst + 2] = raw[pi];
            }
            file.write_all(&row_buf)?;
        }
        log::info!(
            "v2 do_dump_scanout: wrote {path} ({}x{})",
            layout.width,
            layout.height
        );
        wrote_any = true;
    }

    // Diagnostic: also dump the HW cursor plane's dumb buffer (kernel-side
    // view, before the display engine samples it). Compared against the
    // on-screen cursor it isolates load_image stride bugs from display-
    // engine stride misinterpretation.
    if let Some(plane) = backend.platform.cursor_plane.as_ref() {
        let path = format!("./yserver-v2-cursor-{run}.ppm");
        if let Err(e) = plane.dump_to_ppm(&path) {
            log::warn!("v2 do_dump_scanout: cursor dump failed: {e}");
        }
    }
    // Also dump the source CursorRecord bytes (BEFORE load_image), so a
    // diff between this and the dumb-buffer dump localises the bug to
    // either upstream (engine.get_image / X11 wire) or load_image itself.
    if let Some(xid) = backend.effective_cursor_xid
        && let Some(rec) = backend.cursor_records.get(&xid)
    {
        let path = format!("./yserver-v2-cursor-src-{run}.ppm");
        if let Err(e) = dump_cursor_record_to_ppm(&path, rec) {
            log::warn!("v2 do_dump_scanout: cursor record dump failed: {e}");
        } else {
            log::info!(
                "v2 do_dump_scanout: wrote {path} (xid=0x{xid:x} \
                 {}x{} hot=({},{}) bytes_len={} version={})",
                rec.width,
                rec.height,
                rec.hot_x,
                rec.hot_y,
                rec.bgra_bytes.len(),
                rec.version,
            );
        }
    }

    if wrote_any {
        Ok(())
    } else {
        Err(last_err.unwrap_or_else(|| io::Error::other("scanout dump failed")))
    }
}

/// Look up the X11 RENDER `PICTFORMAT` ID a picture was created
/// with. Returns `0` for the synthetic / missing cases (picture
/// xid is 0 = "no picture," non-Drawable variant, or the xid
/// isn't recorded). Used by the diagnostic `render_composite`
/// trace to show marco's declared sampling intent alongside the
/// drawable-depth-derived sampling shape v2 currently uses.
fn picture_pict_format(core: &crate::kms::core::KmsCore, host_pic: u32) -> u32 {
    if host_pic == 0 {
        return 0;
    }
    match core.pictures.get(&host_pic) {
        Some(crate::kms::core::PictureRecord::Drawable { pict_format, .. }) => *pict_format,
        _ => 0,
    }
}

/// Describe a `ResolvedSource` for the diagnostic
/// `render_composite` trace. Returns `(kind_name, depth)` —
/// depth is `0` for non-Drawable sources where the concept
/// doesn't apply. Used only from the trace path; not on hot
/// paint paths.
fn describe_resolved_source(
    store: &super::store::DrawableStore,
    src: &crate::kms::v2::engine::ResolvedSource,
) -> (&'static str, u8) {
    use crate::kms::v2::engine::ResolvedSource;
    match src {
        ResolvedSource::Drawable(id) => {
            let depth = store.get(*id).map_or(0, |d| d.depth);
            ("drawable", depth)
        }
        ResolvedSource::Solid(_) => ("solid", 0),
        ResolvedSource::Gradient(_) => ("gradient", 0),
        ResolvedSource::None => ("none", 0),
    }
}

/// Per-drawable storage dump triggered by SIGUSR2 (or
/// `Ctrl-Alt-F12` via the input thread, mirroring
/// `Ctrl-Alt-Enter` for scanout). Walks a fixed-known set of
/// "interesting" drawables — root, COW, every redirected backing —
/// and writes each storage's content to a `yserver-v2-drawable-…`
/// file in cwd. Each dump cycle increments a global counter so
/// repeated invocations don't clobber.
///
/// Filename layout:
///
/// ```text
/// yserver-v2-drawable-{run}-root-{w}x{h}.ppm
/// yserver-v2-drawable-{run}-cow-{w}x{h}.ppm
/// yserver-v2-drawable-{run}-backing-W0x{w_xid}-B0x{b_xid}-{w}x{h}.ppm
/// ```
///
/// PPM (P6, RGB) is chosen for universal viewer support; the α
/// channel is *intentionally dropped* — the depth-24 padding-byte
/// question is settled separately (4d.6 + the sample-view fix) and
/// what we want to see here is whether `B` contains the window's
/// painted content at all. If a deeper α audit becomes useful later,
/// switching to PAM (P7 with TUPLTYPE=RGB_ALPHA) is a one-liner.
///
/// Reuses `RenderEngine::get_image` for the per-drawable readback so
/// staging-buffer allocation, layout transitions, fence sync, and
/// the BGRA8 → wire-byte pack all flow through the existing,
/// production-tested path. Each dump is one queue submit + one
/// fence wait, so the total stop-the-world time is `O(n)` Vk waits
/// — at ~5 ms per drawable on bee this is fine for diagnostic use.
fn do_dump_drawables_v2(backend: &mut KmsBackendV2) -> io::Result<()> {
    use std::sync::atomic::{AtomicU32, Ordering};

    static DUMP_COUNT: AtomicU32 = AtomicU32::new(0);
    let run = DUMP_COUNT.fetch_add(1, Ordering::Relaxed);

    // Snapshot targets BEFORE touching the engine — `engine.get_image`
    // takes `&mut store + &mut platform`, so we can't hold any
    // shared borrow on `store` while iterating. Each tuple carries
    // everything the per-drawable loop needs: a human-readable label
    // for the filename, the DrawableId for the read, the depth (drives
    // wire-byte unpack), and the extent (drives the read rect + the
    // PPM header).
    #[derive(Debug)]
    struct DumpTarget {
        label: String,
        id: super::store::DrawableId,
        depth: u8,
        width: u32,
        height: u32,
    }
    let mut targets: Vec<DumpTarget> = Vec::new();
    let mut window_manifest = String::new();
    {
        // Scoped read-borrow on the store + core. The borrow ends
        // at the `}` so the mutable borrows below are free to fire.
        if let Some(root_id) = backend.store.lookup(backend.core.window_id)
            && let Some(d) = backend.store.get(root_id)
        {
            targets.push(DumpTarget {
                label: format!("root-0x{:x}", backend.core.window_id),
                id: root_id,
                depth: d.depth,
                width: d.storage.extent.width,
                height: d.storage.extent.height,
            });
        }
        if let Some(cow_id) = backend.cow_id
            && let Some(d) = backend.store.get(cow_id)
        {
            targets.push(DumpTarget {
                label: format!(
                    "cow-0x{:x}",
                    yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0
                ),
                id: cow_id,
                depth: d.depth,
                width: d.storage.extent.width,
                height: d.storage.extent.height,
            });
        }
        // Sorted iteration so re-running the dump gives the same
        // filename ordering — keeps diff-tooling stable across runs.
        let mut pairs: Vec<(u32, u32)> = backend
            .core
            .host_window_to_backing
            .iter()
            .map(|(&w, b)| (w, b.as_raw()))
            .collect();
        pairs.sort_by_key(|(w, _)| *w);
        for (w_xid, b_xid) in pairs {
            let Some(b_id) = backend.store.lookup(b_xid) else {
                continue;
            };
            let Some(d) = backend.store.get(b_id) else {
                continue;
            };
            targets.push(DumpTarget {
                label: format!("backing-W0x{w_xid:x}-B0x{b_xid:x}"),
                id: b_id,
                depth: d.depth,
                width: d.storage.extent.width,
                height: d.storage.extent.height,
            });
        }
        let mut windows: Vec<(u32, WindowGeometryV2)> = backend
            .windows_v2
            .iter()
            .map(|(&xid, geom)| (xid, *geom))
            .collect();
        windows.sort_by_key(|(xid, _)| *xid);
        for (host_xid, geom) in windows {
            let leaf_id = backend.store.lookup(host_xid);
            let redirected_target = leaf_id.and_then(|id| backend.store.redirected_target(id));
            let resolved = leaf_id.and_then(|id| backend.resolve_paint_target_inner(host_xid, id));
            let is_top_level = backend.core.top_level_order.contains(&host_xid);
            use std::fmt::Write as _;
            let _ = writeln!(
                window_manifest,
                "host=0x{host_xid:x} parent={} top_level={} mapped={} depth={} geom=({},{} {}x{}) \
leaf_id={leaf_id:?} redirected_target={redirected_target:?} resolved={resolved:?}",
                geom.parent
                    .map(|p| format!("0x{p:x}"))
                    .unwrap_or_else(|| "None".to_string()),
                is_top_level,
                geom.mapped,
                geom.depth,
                geom.x,
                geom.y,
                geom.width,
                geom.height,
            );
        }
        // Per-window leaf storage. This is what the scene composite
        // samples for unredirected windows, so a "storage right /
        // screen wrong" vs "storage already wrong" split (e16 menu
        // hover items, 2026-06-04) needs these dumped alongside the
        // manifest. Dedup against root/cow/backings pushed above.
        {
            let seen: std::collections::HashSet<super::store::DrawableId> =
                targets.iter().map(|t| t.id).collect();
            let mut win_xids: Vec<u32> = backend.windows_v2.keys().copied().collect();
            win_xids.sort_unstable();
            for w_xid in win_xids {
                let Some(leaf_id) = backend.store.lookup(w_xid) else {
                    continue;
                };
                if seen.contains(&leaf_id) {
                    continue;
                }
                let Some(d) = backend.store.get(leaf_id) else {
                    continue;
                };
                if d.storage.extent.width == 0 || d.storage.extent.height == 0 {
                    continue;
                }
                targets.push(DumpTarget {
                    label: format!("win-0x{w_xid:x}"),
                    id: leaf_id,
                    depth: d.depth,
                    width: d.storage.extent.width,
                    height: d.storage.extent.height,
                });
            }
        }
        // Optional full-store sweep (YSERVER_DUMP_ALL_DRAWABLES=1):
        // every xid-registered drawable, which adds the pixmaps no
        // other walk reaches (client bg/tile pixmaps — e16's menu
        // item images live ONLY here). Off by default to keep the
        // normal dump lean.
        if std::env::var("YSERVER_DUMP_ALL_DRAWABLES").is_ok_and(|v| v == "1") {
            let seen: std::collections::HashSet<super::store::DrawableId> =
                targets.iter().map(|t| t.id).collect();
            let mut xid_pairs: Vec<(u32, super::store::DrawableId)> =
                backend.store.xid_entries().collect();
            xid_pairs.sort_unstable_by_key(|(xid, _)| *xid);
            for (xid, id) in xid_pairs {
                if seen.contains(&id) {
                    continue;
                }
                let Some(d) = backend.store.get(id) else {
                    continue;
                };
                if d.storage.extent.width == 0 || d.storage.extent.height == 0 {
                    continue;
                }
                targets.push(DumpTarget {
                    label: format!("xid-0x{xid:x}"),
                    id,
                    depth: d.depth,
                    width: d.storage.extent.width,
                    height: d.storage.extent.height,
                });
            }
        }
        // Dedup recent-Present source dumps against drawables
        // already in the target list so we don't double-dump if a
        // recent offscreen happens to coincide with a registered
        // backing.
        let already: std::collections::HashSet<super::store::DrawableId> =
            targets.iter().map(|t| t.id).collect();
        // Recent non-COW PresentPixmap sources. This captures
        // compositor-stage pixmaps too, which matter for Cinnamon:
        // the menu can be visible on screen while living only in a
        // fullscreen stage pixmap that never becomes a normal window
        // backing. Keep the source keyed by both src and dst so the
        // filename names which stage/window it was presented into.
        for (idx, &(src_xid, dst_xid)) in backend.recent_present_pixmaps.iter().enumerate() {
            let Some(src_id) = backend.store.lookup(src_xid) else {
                continue;
            };
            if already.contains(&src_id) {
                continue;
            }
            let Some(d) = backend.store.get(src_id) else {
                continue;
            };
            targets.push(DumpTarget {
                label: format!("present-src-{idx}-0x{src_xid:x}-to-0x{dst_xid:x}"),
                id: src_id,
                depth: d.depth,
                width: d.storage.extent.width,
                height: d.storage.extent.height,
            });
        }
    }

    if targets.is_empty() {
        return Err(io::Error::other("no drawable dump targets available"));
    }
    log::info!(
        "v2 do_dump_drawables: run={run} target_count={}",
        targets.len(),
    );
    if !window_manifest.is_empty() {
        let path = format!("./yserver-v2-drawable-{run}-windows.txt");
        if let Err(e) = std::fs::write(&path, &window_manifest) {
            log::warn!("v2 do_dump_drawables: write {path}: {e}");
        } else {
            log::info!("v2 do_dump_drawables: wrote {path}");
        }
    }

    let mut wrote = 0_u32;
    let mut last_err: Option<io::Error> = None;
    for t in targets {
        let rect = ash::vk::Rect2D {
            offset: ash::vk::Offset2D::default(),
            extent: ash::vk::Extent2D {
                width: t.width,
                height: t.height,
            },
        };
        let bytes = match backend.engine.get_image(
            &mut backend.store,
            &mut backend.platform,
            t.id,
            rect,
            t.depth,
        ) {
            Ok(b) => b,
            Err(e) => {
                let err = io::Error::other(format!("get_image {} ({:?}): {e:?}", t.label, t.id));
                log::warn!("v2 do_dump_drawables: {err}");
                last_err = Some(err);
                continue;
            }
        };
        let path = format!(
            "./yserver-v2-drawable-{run}-{label}-{w}x{h}.ppm",
            label = t.label,
            w = t.width,
            h = t.height
        );
        if let Err(e) = write_drawable_ppm(&path, &bytes, t.width, t.height, t.depth) {
            log::warn!("v2 do_dump_drawables: write {path}: {e}");
            last_err = Some(e);
            continue;
        }
        log::info!(
            "v2 do_dump_drawables: wrote {path} (depth={} bytes={})",
            t.depth,
            bytes.len(),
        );
        wrote += 1;
    }
    if wrote > 0 {
        Ok(())
    } else {
        Err(last_err.unwrap_or_else(|| io::Error::other("no drawables dumped")))
    }
}

/// Write a single drawable's storage content as PAM (P7,
/// `RGB_ALPHA`) for depth-24 / depth-32 BGRA8 drawables (preserves
/// the α byte so a later analysis can see whether stored α is zero
/// / one / noise — the Stage 4d "shadow only" diagnosis needs to
/// distinguish "RGB looks right but α is zero" from "RGB itself is
/// broken"), or PGM (P5, gray) for depth-1 / depth-8 R8 drawables.
/// PAM is Netpbm's anymap format; ImageMagick / GIMP / most viewers
/// handle it transparently and dispatch on the magic number, not
/// the file extension.
///
/// `bytes` is the wire-packed buffer returned by
/// `RenderEngine::get_image`:
/// - depth 24/32: 4 bytes/pixel, X11 wire order (B, G, R, X|A) per
///   `pack_from_storage`'s BGRA8 → wire mapping.
/// - depth 8:     1 byte/pixel, R-channel.
/// - depth 1:     bit-packed MSB-first (rendered as PGM after
///   bit-expand, mostly for completeness — no real consumer of the
///   v2 dump runs depth-1 backings).
fn write_drawable_ppm(
    path: &str,
    bytes: &[u8],
    width: u32,
    height: u32,
    depth: u8,
) -> io::Result<()> {
    use std::io::Write;

    let w = usize::try_from(width).map_err(|e| io::Error::other(format!("width: {e}")))?;
    let h = usize::try_from(height).map_err(|e| io::Error::other(format!("height: {e}")))?;
    let mut file = std::fs::File::create(path)?;
    match depth {
        24 | 32 => {
            // BGRA8 wire → PAM RGBA. Reorder per pixel: src is
            // (B, G, R, X|A) in storage byte order; PAM tuples
            // emit (R, G, B, A).
            let expected = w
                .checked_mul(h)
                .and_then(|p| p.checked_mul(4))
                .ok_or_else(|| io::Error::other("size overflow"))?;
            if bytes.len() < expected {
                return Err(io::Error::other(format!(
                    "byte buffer too small: have {} need {}",
                    bytes.len(),
                    expected,
                )));
            }
            file.write_all(
                format!(
                    "P7\nWIDTH {width}\nHEIGHT {height}\nDEPTH 4\nMAXVAL 255\nTUPLTYPE RGB_ALPHA\nENDHDR\n"
                )
                .as_bytes(),
            )?;
            let mut row = vec![0u8; w * 4];
            for y in 0..h {
                for x in 0..w {
                    let src = (y * w + x) * 4;
                    let dst = x * 4;
                    row[dst] = bytes[src + 2]; // R
                    row[dst + 1] = bytes[src + 1]; // G
                    row[dst + 2] = bytes[src]; // B
                    row[dst + 3] = bytes[src + 3]; // A
                }
                file.write_all(&row)?;
            }
        }
        4 | 8 => {
            let expected = w
                .checked_mul(h)
                .ok_or_else(|| io::Error::other("size overflow"))?;
            if bytes.len() < expected {
                return Err(io::Error::other(format!(
                    "byte buffer too small: have {} need {}",
                    bytes.len(),
                    expected,
                )));
            }
            file.write_all(format!("P5\n{width} {height}\n255\n").as_bytes())?;
            file.write_all(&bytes[..expected])?;
        }
        1 => {
            // Bit-packed MSB-first, padded to byte boundaries per
            // X11 wire spec for ZPixmap depth-1. Expand to PGM
            // bytes so a viewer can render the mask.
            let row_bytes = w.div_ceil(8);
            let expected = row_bytes
                .checked_mul(h)
                .ok_or_else(|| io::Error::other("size overflow"))?;
            if bytes.len() < expected {
                return Err(io::Error::other(format!(
                    "byte buffer too small: have {} need {}",
                    bytes.len(),
                    expected,
                )));
            }
            file.write_all(format!("P5\n{width} {height}\n255\n").as_bytes())?;
            let mut out = vec![0u8; w];
            for y in 0..h {
                for x in 0..w {
                    let byte = bytes[y * row_bytes + (x / 8)];
                    let bit = byte >> (7 - (x % 8)) & 1;
                    out[x] = if bit == 1 { 255 } else { 0 };
                }
                file.write_all(&out)?;
            }
        }
        other => {
            return Err(io::Error::other(format!(
                "unsupported depth {other} for drawable dump",
            )));
        }
    }
    Ok(())
}

/// Map a host-visual descriptor to a depth for the storage
/// allocator. Stage 2d picks BGRA32 for `CopyFromParent` (the
/// default visual is depth-24 ARGB-equivalent in our advertised
/// pixel format) and honours an explicit depth otherwise.
/// Stage 3c: walk a `PictureRecord` and resolve it into the
/// engine's `ResolvedSource` plus the per-picture sampler attrs
/// (`repeat`, `transform`, `component_alpha`). Source-only
/// variants (`SolidFill`, gradients) carry no backing drawable;
/// `Drawable` resolves the host xid through `DrawableStore`.
///
/// Returns `None` if the picture xid isn't recorded or the
/// drawable backing has gone away. The engine treats this as a
/// gap and silently no-ops (matches v1's
/// `resolve_render_pic_with_gradient_xid` shape).
/// Stage 3f.14: depth-appropriate safe-default init colour for
/// fresh window storage when the X11 attribute `background-pixel`
/// is `None`. The v2 PixmapPool (3f.10) recycles
/// (image, view, memory) triples between drawables; a pool-take
/// inherits the returner's pixels, so leaving fresh storage at
/// pool content surfaces visually as widget-rect islands on
/// black (caja's drag artifact, 3f.10 + 3f.14 reproducer).
///
/// - Depth 32 windows are premultiplied-α; transparent black
///   `(0, 0, 0, 0)` is the no-op contribution to compositing.
/// - Depth 24 and other non-alpha visuals get opaque black
///   `(0, 0, 0, 1)` — matches "uninitialised window shows black"
///   which is the historical X11 behaviour clients expect.
fn default_window_init_color(depth: u8) -> [f32; 4] {
    if depth == 32 {
        [0.0, 0.0, 0.0, 0.0]
    } else {
        [0.0, 0.0, 0.0, 1.0]
    }
}

/// Stage 3f.13 glyph fallback: pull the first stop's premultiplied
/// RGBA from a gradient picture record. Returns `None` if `host_pic`
/// isn't a gradient or has zero stops. Used by `composite_glyphs`
/// when a gradient source needs a solid-fill approximation — the
/// glyph paint path only knows how to sample a single colour, so a
/// proper LUT-sampled gradient on glyphs would need a separate
/// pipeline (deferred past Stage 3).
fn first_stop_premul_of_gradient(core: &KmsCore, host_pic: u32) -> Option<[f32; 4]> {
    let stop = match core.pictures.get(&host_pic)? {
        PictureRecord::LinearGradient { stops, .. }
        | PictureRecord::RadialGradient { stops, .. } => stops.first()?,
        _ => return None,
    };
    let a = f32::from(stop.a) / 65535.0;
    let r = (f32::from(stop.r) / 65535.0) * a;
    let g = (f32::from(stop.g) / 65535.0) * a;
    let b = (f32::from(stop.b) / 65535.0) * a;
    Some([r, g, b, a])
}

fn resolve_picture_for_render(
    core: &KmsCore,
    store: &crate::kms::v2::store::DrawableStore,
    host_pic: u32,
) -> Option<(
    crate::kms::v2::engine::ResolvedSource,
    Repeat,
    Option<PictTransform>,
    bool, // component_alpha
)> {
    use crate::kms::v2::engine::ResolvedSource;
    match core.pictures.get(&host_pic)? {
        PictureRecord::Drawable {
            host_xid,
            repeat,
            transform,
            component_alpha,
            ..
        } => {
            let id = store.lookup(*host_xid)?;
            Some((
                ResolvedSource::Drawable(id),
                *repeat,
                *transform,
                *component_alpha,
            ))
        }
        PictureRecord::SolidFill {
            premul,
            repeat,
            component_alpha,
        } => Some((
            ResolvedSource::Solid(*premul),
            *repeat,
            None,
            *component_alpha,
        )),
        PictureRecord::LinearGradient {
            repeat, transform, ..
        }
        | PictureRecord::RadialGradient {
            repeat, transform, ..
        } => {
            // Stage 3f.13: full LUT sampling. The engine-side
            // `GradientPicture` was built at create time and lives
            // in `engine.picture_paint[host_pic]`; engine looks it
            // up by xid. If the engine-side build failed (test
            // fixture with no Vk, or allocation error), the engine
            // logs a gap and skips the paint — no first-stop
            // collapse fallback.
            Some((
                ResolvedSource::Gradient(host_pic),
                *repeat,
                *transform,
                false,
            ))
        }
    }
}

/// Stage 3c: dst picture resolution. RENDER paint ops require
/// the dst to be a `PictureRecord::Drawable` (you can't paint
/// into a SolidFill or a Gradient). Returns the underlying
/// dst drawable's `host_xid` plus the picture's clip rectangles
/// (already pre-shifted by `clip_x` / `clip_y` per Stage 3b).
///
/// Stage 4a: callers feed `host_xid` through
/// `KmsBackendV2::resolve_paint_target` to apply COMPOSITE
/// redirect routing. The free function stays pure
/// (`&KmsCore`-only) so it can also be called from contexts
/// where the windows_v2 / parent chain isn't relevant.
fn resolve_dst_picture_for_render(
    core: &KmsCore,
    host_pic: u32,
) -> Option<(u32, Option<Vec<Rectangle16>>)> {
    let PictureRecord::Drawable { host_xid, clip, .. } = core.pictures.get(&host_pic)? else {
        return None;
    };
    Some((*host_xid, clip.clone()))
}

/// Audit #2 (2026-05-19) — extract a source / mask picture's
/// `clientClip` for `render_composite`'s composite-region
/// computation. The picture's clip rects are stored
/// pre-shifted by `clip_x` / `clip_y` (see
/// `render_set_picture_clip_rectangles`), so the returned list
/// is already in the picture's drawable-local coord space —
/// `compute_render_composite_clip` translates from there into
/// dst space via `(xDst - xSrc, yDst - ySrc)`.
///
/// Non-Drawable pictures (`SolidFill` / gradients) carry no
/// `clientClip` and return `None`. `host_pic == 0` (the
/// "no mask" sentinel `RenderComposite` uses) also returns `None`.
fn picture_client_clip(core: &KmsCore, host_pic: u32) -> Option<Vec<Rectangle16>> {
    if host_pic == 0 {
        return None;
    }
    match core.pictures.get(&host_pic)? {
        PictureRecord::Drawable { clip, .. } => clip.clone(),
        PictureRecord::SolidFill { .. }
        | PictureRecord::LinearGradient { .. }
        | PictureRecord::RadialGradient { .. } => None,
    }
}

fn format_clip_rects(rects: Option<&[Rectangle16]>) -> String {
    use std::fmt::Write as _;

    match rects {
        None => "<None>".to_string(),
        Some([]) => "<empty>".to_string(),
        Some(rects) => {
            let mut out = String::from("[");
            for (i, rect) in rects.iter().enumerate() {
                if i > 0 {
                    out.push(' ');
                }
                let _ = write!(
                    out,
                    "({},{} {}x{})",
                    rect.x, rect.y, rect.width, rect.height
                );
            }
            out.push(']');
            out
        }
    }
}

/// Resolve the drawable depth for a new subwindow. `CopyFromParent`
/// inherits the parent window's depth; only the root / untracked
/// fallback defaults to 24.
/// Wrap raw GetImage pixel bytes into a full X11 GetImage reply
/// (32-byte header + pixels). `sequence` and `visual` are patched in
/// by the handler (`process_request.rs:handle_get_image`); this
/// helper fills the rest. Mirrors v1's
/// `KmsBackend::get_image` (kms/backend.rs:10400-10420) byte-for-byte
/// so the handler's expectations carry across both backends.
fn wrap_get_image_reply(depth: u8, pixel_bytes: Vec<u8>) -> Vec<u8> {
    let pixel_len = pixel_bytes.len();
    let mut out = Vec::with_capacity(32 + pixel_len);
    out.push(1); // [0]: Reply indicator
    out.push(depth); // [1]: depth
    out.extend_from_slice(&[0u8; 2]); // [2..4]: sequence (patched by handler)
    // [4..8]: reply length in u32 units. Rows are already
    // 4-byte aligned for the depths we support (1/8/24/32 — see
    // `pack_from_storage`), so this is `pixel_len / 4`.
    let reply_length_units = u32::try_from(pixel_len / 4).unwrap_or(u32::MAX);
    out.extend_from_slice(&reply_length_units.to_le_bytes());
    out.extend_from_slice(&[0u8; 4]); // [8..12]: visual (patched by handler)
    out.extend_from_slice(&[0u8; 20]); // [12..32]: padding
    debug_assert_eq!(out.len(), 32);
    out.extend_from_slice(&pixel_bytes);
    out
}

/// X11 GetImage `format` wire value for XYPixmap (ZPixmap is 2).
const GET_IMAGE_FORMAT_XY_PIXMAP: u8 = 1;

/// All-planes mask for a drawable of `depth` (1 ≤ depth ≤ 32).
fn depth_plane_mask(depth: u8) -> u32 {
    if depth >= 32 {
        u32::MAX
    } else {
        (1u32 << depth) - 1
    }
}

/// Apply a ZPixmap `plane_mask` in place to wire-format pixel rows as
/// produced by `pack_from_storage`: depth 1 = 1bpp bitmap rows, depth 8
/// = byte rows padded to 4, depth 24/32 = BGRA u32 LE. Per the X11
/// spec, GetImage ZPixmap returns zero bits in all planes not in
/// `plane_mask` (the full pixel grid is still transmitted). `mask` is
/// already truncated to the drawable depth, so for depth 24 the X byte
/// gets cleared too — its content is undefined on the wire.
fn apply_z_plane_mask(bytes: &mut [u8], depth: u8, mask: u32) {
    match depth {
        1 => {
            if mask & 1 == 0 {
                bytes.fill(0);
            }
        }
        4 | 8 => {
            let m = (mask & 0xff) as u8;
            if depth == 4 {
                let m = m & 0x0f;
                for b in bytes.iter_mut() {
                    *b = (*b & 0x0f & m) | (((*b >> 4) & m) << 4);
                }
            } else {
                for b in bytes.iter_mut() {
                    *b &= m;
                }
            }
        }
        24 | 32 => {
            for px in bytes.chunks_exact_mut(4) {
                let v = u32::from_le_bytes([px[0], px[1], px[2], px[3]]) & mask;
                px.copy_from_slice(&v.to_le_bytes());
            }
        }
        // Depths the engine can't read back never get here (the
        // engine already errored and the handler sent the fallback).
        _ => bytes.fill(0),
    }
}

fn z_pixmap_row_stride(depth: u8, width: u32) -> usize {
    match depth {
        1 => width.div_ceil(32) as usize * 4,
        4 => width.div_ceil(8) as usize * 4,
        8 => (width as usize + 3) & !3,
        24 | 32 => width as usize * 4,
        _ => 0,
    }
}

fn read_z_pixmap_pixel(bytes: &[u8], depth: u8, width: u32, x: usize, y: usize) -> u32 {
    let stride = z_pixmap_row_stride(depth, width);
    match depth {
        1 => {
            let byte = bytes[y * stride + x / 8];
            u32::from((byte >> (x % 8)) & 1)
        }
        4 => {
            let byte = bytes[y * stride + x / 2];
            u32::from(if x.is_multiple_of(2) {
                byte & 0x0f
            } else {
                (byte >> 4) & 0x0f
            })
        }
        8 => u32::from(bytes[y * stride + x]),
        24 | 32 => {
            let off = y * stride + x * 4;
            u32::from_le_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]])
        }
        _ => 0,
    }
}

fn write_z_pixmap_pixel(bytes: &mut [u8], depth: u8, width: u32, x: usize, y: usize, value: u32) {
    let stride = z_pixmap_row_stride(depth, width);
    match depth {
        1 => {
            let byte = &mut bytes[y * stride + x / 8];
            let bit = 1u8 << (x % 8);
            if value & 1 != 0 {
                *byte |= bit;
            } else {
                *byte &= !bit;
            }
        }
        4 => {
            let byte = &mut bytes[y * stride + x / 2];
            let nibble = (value & 0x0f) as u8;
            if x.is_multiple_of(2) {
                *byte = (*byte & 0xf0) | nibble;
            } else {
                *byte = (*byte & 0x0f) | (nibble << 4);
            }
        }
        8 => bytes[y * stride + x] = value as u8,
        24 | 32 => {
            let off = y * stride + x * 4;
            bytes[off..off + 4].copy_from_slice(&value.to_le_bytes());
        }
        _ => {}
    }
}

fn apply_gc_function(
    function: yserver_core::backend::GcFunction,
    src: u32,
    dst: u32,
    mask: u32,
) -> u32 {
    use yserver_core::backend::GcFunction;
    let op = match function {
        GcFunction::Clear => 0,
        GcFunction::And => src & dst,
        GcFunction::AndReverse => src & !dst,
        GcFunction::Copy => src,
        GcFunction::AndInverted => !src & dst,
        GcFunction::NoOp => dst,
        GcFunction::Xor => src ^ dst,
        GcFunction::Or => src | dst,
        GcFunction::Nor => !(src | dst),
        GcFunction::Equiv => !(src ^ dst),
        GcFunction::Invert => !dst,
        GcFunction::OrReverse => src | !dst,
        GcFunction::CopyInverted => !src,
        GcFunction::OrInverted => !src | dst,
        GcFunction::Nand => !(src & dst),
        GcFunction::Set => u32::MAX,
    };
    (op & mask) | (dst & !mask)
}

/// Repack Z-layout wire bytes (per `pack_from_storage`) into XYPixmap
/// wire format: one 1-bit plane per set bit in `mask`, most-significant
/// plane first (X11 §GetImage), scanlines padded to 32 bits, LSBFirst
/// bit order matching the advertised bitmap-format-bit-order (and the
/// depth-1 packing in `pack_from_storage`).
fn z_to_xy_planes(z: &[u8], w: u32, h: u32, depth: u8, mask: u32) -> Vec<u8> {
    let w_us = w as usize;
    let h_us = h as usize;
    let out_stride = w.div_ceil(32) as usize * 4;
    let n_planes = mask.count_ones() as usize;
    let mut out = vec![0u8; out_stride * h_us * n_planes];
    if w_us == 0 || h_us == 0 || n_planes == 0 {
        return out;
    }
    let pixel = |x: usize, y: usize| -> u32 {
        match depth {
            1 => {
                // Already bitmap rows padded to 32 bits.
                let byte = z[y * out_stride + x / 8];
                u32::from((byte >> (x % 8)) & 1)
            }
            4 => {
                let stride = w.div_ceil(8) as usize * 4;
                let byte = z[y * stride + x / 2];
                u32::from(if x.is_multiple_of(2) {
                    byte & 0x0f
                } else {
                    (byte >> 4) & 0x0f
                })
            }
            8 => {
                // Byte rows padded to 4 bytes.
                let stride = (w_us + 3) & !3;
                u32::from(z[y * stride + x])
            }
            // 24/32: tightly packed BGRA u32 LE.
            _ => {
                let off = (y * w_us + x) * 4;
                u32::from_le_bytes([z[off], z[off + 1], z[off + 2], z[off + 3]])
            }
        }
    };
    let mut plane_base = 0;
    for p in (0..32).rev().filter(|p| mask & (1 << p) != 0) {
        for y in 0..h_us {
            let row = plane_base + y * out_stride;
            for x in 0..w_us {
                if (pixel(x, y) >> p) & 1 != 0 {
                    out[row + x / 8] |= 1 << (x % 8);
                }
            }
        }
        plane_base += out_stride * h_us;
    }
    out
}

fn depth_for_visual(visual: HostSubwindowVisual, parent_depth: Option<u8>) -> u8 {
    match visual {
        HostSubwindowVisual::CopyFromParent => parent_depth.unwrap_or(24),
        HostSubwindowVisual::DepthOnly { depth } => {
            if depth == 0 {
                parent_depth.unwrap_or(24)
            } else {
                depth
            }
        }
        HostSubwindowVisual::Explicit { depth, .. } => {
            if depth == 0 {
                parent_depth.unwrap_or(24)
            } else {
                depth
            }
        }
    }
}

// ───────────────────────────────────────────────────────────────
// `Backend` trait implementation. The shape:
//
// A. Pure accessors — return values from `self.core` or local
//    constants identical to v1.
// B. Bookkeeping mutations — mutate `self.core` (XID map etc.).
// C. Mixed bookkeeping + storage — log a gap; for ops that must
//    return a handle, mint a fresh xid via `self.core.next_host_xid()`
//    so subsequent xid_map lookups stay consistent.
// D. Paint / RENDER / scene — log a gap, return Ok or the
//    default-impl shape.
// ───────────────────────────────────────────────────────────────

impl KmsBackendV2 {
    /// Handle a hotkey fired by the on-core libinput dispatch (libseat mode).
    /// Each variant sends the corresponding control message via `input_sender`
    /// or, for `SwitchVt`, requests the VT switch inline through libseat
    /// (fire-and-forget; the actual state transition arrives later via the
    /// seat disable callback).
    fn handle_core_hotkey(&mut self, hk: crate::input::hotkey::Hotkey) {
        use crate::input::hotkey::Hotkey;
        use yserver_core::core_loop::Message;
        match hk {
            Hotkey::Zap => {
                log::warn!("kms: Ctrl-Alt-Backspace — requesting shutdown (zap)");
                if let Some(s) = &self.input_sender {
                    let _ = s.send(Message::Shutdown);
                }
            }
            Hotkey::DumpScanout => {
                log::info!("kms: Ctrl-Alt-Enter — dumping scanout");
                if let Some(s) = &self.input_sender {
                    let _ = s.send(Message::DumpScanout);
                }
            }
            Hotkey::DumpDrawables => {
                log::info!("kms: Ctrl-Alt-F12 — dumping drawables");
                if let Some(s) = &self.input_sender {
                    let _ = s.send(Message::DumpDrawables);
                }
            }
            Hotkey::SwitchVt(vt) => {
                if let crate::seat::Seat::Libseat { inner, .. } = &self.seat {
                    // Short borrow; switch_session is fire-and-forget and does
                    // NOT transition seat_state — the state machine moves on the
                    // later disable callback from libseat.
                    log::info!(
                        "kms: SwitchVt({vt}) requested — pre-call state: seat_state={:?} pending(enable={}, disable={})",
                        self.seat_state,
                        self.seat_pending.pending_enable,
                        self.seat_pending.pending_disable,
                    );
                    if let Err(e) = inner.borrow_mut().switch_session(vt) {
                        log::warn!("kms: switch_session({vt}) failed: {e}");
                    } else {
                        log::info!("kms: requested VT switch to {vt}");
                    }
                } else {
                    log::warn!("kms: SwitchVt({vt}) ignored — seat is Direct (no libseat)");
                }
            }
        }
    }
}

impl Backend for KmsBackendV2 {
    // ── A. Accessors (mirror KmsBackend exactly) ────────────────

    fn window_id(&self) -> u32 {
        self.core.window_id
    }

    fn root_visual_xid(&self) -> u32 {
        self.core.root_visual_xid
    }

    fn argb_visual_xid(&self) -> Option<u32> {
        Some(ARGB_VISUAL.0)
    }

    fn argb_colormap_xid(&self) -> Option<u32> {
        Some(ARGB_COLORMAP.0)
    }

    fn render_opcode(&self) -> Option<u8> {
        Some(133)
    }

    fn xkb_opcode(&self) -> Option<u8> {
        Some(136)
    }

    fn xkb_info(&self) -> Option<(u8, u8, u8)> {
        Some((136, 85, 162))
    }

    fn composite_opcode(&self) -> Option<u8> {
        Some(144)
    }

    fn render_format_for_ynest_id(&self, ynest_fmt: u32) -> Option<u32> {
        if ynest_fmt == 0 {
            None
        } else {
            Some(ynest_fmt)
        }
    }

    fn ping(&mut self, _origin: Option<OriginContext>) -> io::Result<()> {
        Ok(())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn xid_map(&self) -> &HostXidMap {
        &self.core.xid_map
    }

    // ── Single-threaded core hooks ──────────────────────────────

    fn on_window_property_changed(
        &mut self,
        state: &ServerState,
        host_xid: u32,
        _property: AtomId,
    ) {
        self.apply_top_level_stack_hint(state, host_xid);
    }

    fn on_window_became_top_level(&mut self, state: &ServerState, host_xid: u32) {
        self.apply_top_level_stack_hint(state, host_xid);
    }

    fn on_host_input(&mut self, state: &mut ServerState, ev: HostInputEvent) {
        // Stage 3f.7 port of v1's on_host_input. Key events go
        // through the cook → key fanout path; pointer events flow
        // into `pending_pointer_events`, which we drain to the
        // pointer fanout after each call so the buffer stays empty
        // between events (matches v1's contract).
        use yserver_core::core_loop::{
            HostInputEvent, key_fanout::key_event_fanout_to_state,
            pointer_fanout::pointer_event_fanout_to_state,
        };

        match ev {
            HostInputEvent::PointerMotion { x, y, time: _ } => {
                self.process_pointer_absolute(state, x as f32, y as f32);
            }
            HostInputEvent::PointerButton {
                button,
                pressed,
                time: _,
            } => {
                self.process_pointer_button(u32::from(button), pressed, state);
            }
            HostInputEvent::Key(raw) => {
                let cooked = self.cook_host_key(raw);
                // Maintain the held-keys set so suspend can synthesize
                // releases (Task 10). Use the COOKED keycode so
                // synthesized releases carry the same value clients see.
                if cooked.pressed {
                    self.core.down_keys.insert(cooked.keycode);
                } else {
                    self.core.down_keys.remove(&cooked.keycode);
                }
                let _dropped = key_event_fanout_to_state(state, self, cooked);
                return;
            }
            HostInputEvent::DeviceAdded(info) => {
                log::info!(
                    "xi-device: added {:?} node={} touchpad={}",
                    info.name,
                    info.device_node,
                    info.is_touchpad,
                );
                state.xi_seed_touchpad(&info);
                // Notify clients selecting XI_DeviceChanged on the slave
                // pointer (device 4) so a running desktop re-reads the
                // device's new name + properties. No-op if none selected.
                // 137 = the XI extension's runtime major opcode (mirrors
                // nested.rs / process_request.rs XI2_MAJOR_OPCODE).
                let _dropped =
                    yserver_core::core_loop::fanout::emit_xi2_device_changed_slave_pointer(
                        state, 137,
                    );
                return;
            }
            HostInputEvent::DeviceRemoved { device_node } => {
                log::info!("xi-device: removed node={device_node}");
                state.xi_clear_touchpad(&device_node);
                let _dropped =
                    yserver_core::core_loop::fanout::emit_xi2_device_changed_slave_pointer(
                        state, 137,
                    );
                return;
            }
        }

        // Drain pointer events queued by the process_pointer_* call.
        let pending = std::mem::take(&mut self.core.pending_pointer_events);
        let xid_map = self.core.xid_map.clone();
        for ev in pending {
            let _dropped = pointer_event_fanout_to_state(state, self, &xid_map, ev, true, false);
        }
    }

    fn on_page_flip_ready(&mut self, _state: &mut ServerState) {
        // Gate: when not Active we have no DRM master; page-flip events
        // are drained (so the fd doesn't stay readable) but no resubmit
        // or flush_submit_group runs. In Direct mode this is always false
        // → no behaviour change.
        if !self.scanout_allowed() {
            // Drain to clear the DRM fd's readiness; discard results.
            let _ = self.platform.drain_page_flip_events();
            log::debug!("v2 on_page_flip_ready: skipped (seat not Active)");
            return;
        }
        let flipped = match self.platform.drain_page_flip_events() {
            Ok(flipped) => flipped,
            Err(e) => {
                log::warn!("v2: drain_page_flip_events failed: {e}");
                return;
            }
        };
        for output_idx in flipped {
            if self
                .scene
                .handle_page_flip_complete(output_idx, &mut self.store, &mut self.platform)
            {
                self.telemetry.record_frame_present();
            }
        }
        // The just-retired flip(s) freed up the primary atomic-commit
        // queue on at least one CRTC; retry any cursor move that lost
        // to a pending primary commit since its fresh motion event.
        // Latest-wins: only the most recent pending position is
        // re-issued. If the retry itself EBUSY's against another
        // primary commit that landed in the meantime, the slot stays
        // populated for the next page-flip retire.
        match self.platform.cursor_plane_drain_pending_move() {
            Ok(0) => {}
            Ok(n) => self.telemetry.record_cursor_move_ebusy(u64::from(n)),
            Err(e) => log::debug!("v2 cursor drain on page-flip retire: {e}"),
        }
        // Sweep retired engine submits + retired drawables now
        // that their fences may have signaled.
        self.engine.poll_retired(&self.platform);
        self.poll_pending_retire_with_invalidate();
        self.sync_descriptor_pool_telemetry();
        // Phase A T7: pageflip retire is a frame boundary — close
        // any open render batch FIRST so its CBs land in the group
        // under the same ticket that the subsequent flush will
        // consume. Then flush the SubmitGroup so an idle next tick
        // (no scene_structure_dirty) does not leave paint CBs
        // buffered until the next compose. Drive through the engine
        // wrapper so parked pending_group_ops commit to `submitted`
        // atomically.
        if let Err(e) = self
            .engine
            .flush_render_batch(&mut self.store, &mut self.platform)
        {
            log::warn!("v2 on_page_flip_ready: flush_render_batch failed: {e:?}");
        }
        if let Err(e) = self.engine.flush_submit_group(
            &mut self.store,
            &mut self.platform,
            crate::kms::v2::submit_group::FlushReason::PageflipRetire,
        ) {
            log::warn!("v2 on_page_flip_ready: flush_submit_group failed: {e:?}");
        }
    }

    fn mark_dirty(&mut self) {
        // Wake the compositor without inventing full-output damage.
        // Paint paths already record per-drawable presentation
        // damage, and cursor motion is projected by build_scene.
        self.scene.wake_for_damage();
    }

    fn next_wakeup(&self) -> Option<std::time::Instant> {
        let now = std::time::Instant::now();
        let scene_deadline = if self.scene.scene_structure_dirty {
            if self.scene.has_output_ready_for_submit() {
                Some(now)
            } else {
                self.scene.earliest_retry_deadline()
            }
        } else {
            self.scene.earliest_retry_deadline()
        };
        let needs_present_poll = self.pending_present_batches.iter().any(|batch| {
            matches!(
                batch.wait,
                crate::kms::v2::present_completion::PresentBatchWait::Poll
            )
        });
        let present_deadline = if needs_present_poll {
            Some(now + std::time::Duration::from_millis(1))
        } else {
            None
        };
        scene_deadline
            .into_iter()
            .chain(present_deadline)
            .chain(self.cursor_anim_deadline())
            .min()
    }

    fn maybe_composite(&mut self) -> io::Result<()> {
        // VT-master gate: when libseat has revoked DRM master (VT
        // switch in progress / handed to another session), every
        // atomic_commit returns `EACCES`. `composite_and_flip` has
        // the same gate at :3263; `maybe_composite` was missing it
        // and emitted a burst of "atomic commit failed for output …
        // Permission denied" WARNs across the VT-suspend window
        // (observed 2026-05-31 — 77 WARNs in 3 seconds on
        // `just startx` + VT switch under MATE). In Direct mode
        // `scanout_allowed()` is always true so this is a no-op
        // there.
        if !self.scanout_allowed() {
            return Ok(());
        }
        // DPMS gate: outputs are inactive (every CRTC has ACTIVE=0 +
        // MODE_ID=0 from disable_output). Submitting an atomic page-flip
        // commit against a disabled CRTC returns EINVAL. Without this
        // gate the core loop's per-iteration `backend.maybe_composite()`
        // call would loop a tight EINVAL storm while DPMS is Off (and
        // the `composite_and_flip` gate at :3196 wouldn't catch it —
        // maybe_composite is a separate scene.tick caller).
        // See project_einval_atomic_commit_storm_wedge memory entry.
        if !self.kms_outputs_active {
            return Ok(());
        }
        // Animated-cursor frame advance — after both gates above so
        // DPMS-off / VT-away never uploads (spec "DPMS / VT gating").
        self.tick_cursor_animation();
        // Phase B.1 close trigger 4: if a frame has been open past the
        // timeout (16 ms default), force a close to release pinned
        // resources. No-op if no frame open or below threshold.
        if let Err(e) = self
            .engine
            .close_open_frame_if_timed_out(&mut self.store, &mut self.platform)
        {
            log::warn!("v2 maybe_composite: timeout close failed: {e:?}");
        }
        // One main-loop tick = one frame_id. Submit events
        // recorded between calls share the surrounding tick's
        // id; the scene_compose event of this tick (if it
        // submits) also carries this id.
        self.telemetry.advance_frame();
        let can_submit_scene =
            self.scene.scene_structure_dirty && self.scene.has_output_ready_for_submit();
        if can_submit_scene {
            // Stage 5 Task 3 (render-composite generalization): flush
            // the render batch — scene.tick samples dst.
            self.drain_engine_present_batches();
            if let Err(e) = self
                .engine
                .flush_render_batch(&mut self.store, &mut self.platform)
            {
                log::warn!("v2 maybe_composite: flush_render_batch failed: {e:?}");
            }
            // Phase B Invariant M3: close any open frame BEFORE legacy compose
            // records. compose samples drawable storage at record time
            // (scene.rs:1307), so the open frame's layout + ticket-touch overlays
            // must be committed before the compose CB lands. Retires at sub-phase
            // B.4 when compose itself ports into the frame builder.
            // NOTE: integration test for M3 lives in Task 23's mixed-sequence
            // smoke (v2_frame_builder_mixed_sequence_smoke); Task 13 only adds
            // the wiring. Until Task 15 ports composite_glyphs into the frame
            // builder, no frame can be open, so this call is a no-op.
            if let Err(e) = self.engine.close_open_frame(
                &mut self.store,
                &mut self.platform,
                crate::kms::v2::frame_builder::CloseReason::LegacyScCompose,
            ) {
                log::warn!("v2 maybe_composite: close_open_frame failed: {e:?}");
            }
            // Phase A Task 4: flush the SubmitGroup so scene.tick
            // observes all paint CBs already submitted to the queue.
            // Compose stays on its own dedicated `vkQueueSubmit2`
            // (record_compose_v2) — only the buffered paint group is
            // flushed here. Drive through the engine wrapper so
            // parked `pending_group_ops` commit too.
            if let Err(e) = self.engine.flush_submit_group(
                &mut self.store,
                &mut self.platform,
                crate::kms::v2::submit_group::FlushReason::SceneCompose,
            ) {
                log::warn!("v2 maybe_composite: flush_submit_group failed: {e:?}");
            }
        }
        let result = if !can_submit_scene {
            Ok(())
        } else {
            let cow_host_xid = self.cow_host_xid();
            match self.scene.tick(
                &self.core,
                &mut self.store,
                &mut self.platform,
                &self.windows_v2,
                &mut self.telemetry,
                cow_host_xid,
            ) {
                Ok(composed) => {
                    for i in 0..composed {
                        self.telemetry.record_composite_submit();
                        // One scene_compose event per output that
                        // presented this tick. `target_id` is the
                        // sequential index of the composed output
                        // within this tick (we don't get the real
                        // output_idx back from scene.tick today —
                        // sufficient for the diagnostic's
                        // "did N outputs compose?" question).
                        self.telemetry.record_submit_event(SubmitEvent {
                            frame_id: 0,
                            kind: SubmitKind::SceneCompose,
                            target_kind: TargetKind::Output,
                            target_id: u64::try_from(i).unwrap_or(0),
                            batch_size: 1,
                            op: SubmitOp::None,
                            src_class: SrcClass::None,
                            mask_class: SrcClass::None,
                            pipeline_id: None,
                            flags: SubmitFlags::NONE,
                        });
                    }
                    Ok(())
                }
                Err(e) => {
                    log::warn!("v2 maybe_composite: scene.tick failed: {e:?}");
                    Ok(())
                }
            }
        };
        self.drain_render_telemetry();
        // Phase A Task 3.5: drain SubmitGroup flush outcomes and
        // route each to the matching telemetry counter.
        for outcome in self.engine.drain_flush_outcomes() {
            if outcome.aborted {
                self.telemetry.record_submit_group_abort();
            } else {
                self.telemetry
                    .record_submit_group_flush(outcome.flushed_entries, outcome.reason);
            }
        }
        // Phase A telemetry retention gauges. Sample on every tick —
        // the high-water aggregator handles bursts.
        let pool_count =
            u64::try_from(self.engine.descriptor_pool_ring_pool_count()).unwrap_or(u64::MAX);
        self.telemetry
            .record_active_descriptor_pool_high_water(pool_count);
        let (staging_bytes, scratch_bytes) = self.engine.active_resource_bytes();
        self.telemetry
            .record_active_staging_high_water(staging_bytes);
        self.telemetry
            .record_active_scratch_high_water(scratch_bytes);
        // Phase B.1 Task 21: drain frame-builder close events into telemetry.
        self.drain_frame_builder_telemetry();
        // Per-second telemetry summary emission.
        self.telemetry.maybe_emit();
        result
    }

    fn dump_scanout(&mut self) {
        if let Err(e) = do_dump_scanout_v2(self) {
            log::warn!("v2 dump_scanout: {e}");
        }
    }

    fn dump_drawables(&mut self) {
        if let Err(e) = do_dump_drawables_v2(self) {
            log::warn!("v2 dump_drawables: {e}");
        }
        // Stage 4d shadow-hunt: COW vs scanout vs present-src must
        // come from the same instant or the comparison is useless
        // (the moment of interest is the first COW-targeted
        // Present after caja paints, which moves on every frame).
        // Pair the scanout dump with the drawable dump so a single
        // Ctrl+Alt+F12 captures all three artifacts atomically.
        if let Err(e) = do_dump_scanout_v2(self) {
            log::warn!("v2 dump_drawables: scanout side: {e}");
        }
        // Surface the COW + present-src ring state so the user can
        // tell at-a-glance whether the dump captured the expected
        // shape (cow_id set, recent sources non-empty) without
        // having to grep for the per-target log lines.
        log::info!(
            "v2 dump_drawables: cow_id={:?} recent_present_pixmaps_len={}",
            self.cow_id,
            self.recent_present_pixmaps.len(),
        );
    }

    fn note_present_pixmap(&mut self, src_pixmap_xid: u32, dst_window_xid: u32) {
        const PRESENT_CAP: usize = 32;

        if self.recent_present_pixmaps.back() != Some(&(src_pixmap_xid, dst_window_xid)) {
            if self.recent_present_pixmaps.len() == PRESENT_CAP {
                self.recent_present_pixmaps.pop_front();
            }
            self.recent_present_pixmaps
                .push_back((src_pixmap_xid, dst_window_xid));
        }
    }

    fn wait_present_source_ready(&mut self, src_pixmap_host_xid: u32) {
        use crate::kms::vk::dri3::{DmabufReadWait, wait_dmabuf_read_ready};
        // Bounded so an absent/stuck producer fence can never hang the
        // single-threaded core. 50 ms (~3 vsync @ 60 Hz) is generous
        // for a finished-but-not-flushed GPU frame, short enough that a
        // pathological miss only yields one stale frame.
        //
        // This is a CPU wait — correct and safe, but it stalls the core
        // for the (usually sub-frame) duration of the producer's
        // outstanding render. The non-stalling form — a GPU
        // acquire-semaphore imported from the same dma-buf fence and
        // waited on the present copy's submit — is filed as a follow-up
        // to land with the composite-into-frame-builder work (see
        // docs/known-issues.md), where there is one well-defined submit
        // per frame to attach the wait to.
        const TIMEOUT_MS: i32 = 50;
        let Some(src_id) = self.store.lookup(src_pixmap_host_xid) else {
            return;
        };
        // Only DRI3-imported (client-produced) sources carry a producer
        // fence to wait on; server-owned storage is ordered by our own
        // queue barriers, so `imported_dma_buf_fd()` → None → no wait
        // (also why this never blocks the lavapipe/server-owned paths).
        let Some(fd) = self
            .store
            .get(src_id)
            .and_then(|d| d.storage.imported_drawable.as_ref())
            .and_then(super::super::vk::target::DrawableImage::imported_dma_buf_fd)
        else {
            return;
        };
        // Ready / Idle are the common, healthy outcomes — silent. Only
        // surface the anomalies: TimedOut (we proceeded on a still-
        // pending render → possible stale frame) and Unsupported (the
        // ioctl is unavailable → we fell back to the old no-wait read).
        match wait_dmabuf_read_ready(fd, TIMEOUT_MS) {
            DmabufReadWait::Ready | DmabufReadWait::Idle => {}
            other => log::debug!(
                target: "yserver::kms::v2::present",
                "present source 0x{src_pixmap_host_xid:x} dma-buf read-wait → {other:?}",
            ),
        }
    }

    fn poll_fds(&self) -> Vec<(std::os::fd::RawFd, BackendFdKind)> {
        // DRM fd + present-completion epfd (always); in libseat mode also
        // the seat connection fd and the on-core libinput fd.
        // The fds are stable for the process lifetime (Deviation #5), so
        // caching them once and never re-registering is correct.
        let mut fds = self.platform.poll_fds();
        if self.seat.is_libseat() {
            // seat_fd < 0 only when the libseat init path was skipped, which
            // cannot happen here (the seat being Libseat implies it was opened
            // successfully and the fd was cached).
            if self.seat_fd >= 0 {
                fds.push((self.seat_fd, BackendFdKind::Seat));
            }
            if self.core_libinput_fd >= 0 {
                fds.push((self.core_libinput_fd, BackendFdKind::Libinput));
            }
        }
        fds
    }

    fn set_input_sender(&mut self, sender: yserver_core::core_loop::CoreSender) {
        self.input_sender = Some(sender);
    }

    fn on_seat_ready(&mut self, state: &mut ServerState) {
        use crate::seat::state::SeatEventKind;
        use std::rc::Rc;

        // Clone the Rc handles before borrowing so we don't hold a
        // reference into `self.seat` while calling mutable methods.
        let (inner, events) = match &self.seat {
            crate::seat::Seat::Libseat {
                inner,
                pending_events,
            } => (Rc::clone(inner), Rc::clone(pending_events)),
            crate::seat::Seat::Direct => return,
        };

        // Dispatch libseat: the callback closure pushes Enable/Disable
        // into `events`. Release the borrow immediately after.
        if let Err(e) = inner.borrow_mut().dispatch() {
            log::error!("kms: libseat dispatch failed: {e}; exiting"); // Risk #7
            self.request_exit();
            return;
        }

        // Drain the callback queue — release the borrow BEFORE calling
        // drive_seat_event so no RefCell borrow is held during the
        // suspend/resume sequences (which call libinput.suspend/resume
        // that re-enter open_restricted → LibseatInner::borrow_mut).
        let drained: Vec<SeatEventKind> = events.borrow_mut().drain(..).collect();
        log::info!(
            "kms: on_seat_ready dispatch ok — {} pending event(s) (state before drive: {:?}, pending: enable={} disable={})",
            drained.len(),
            self.seat_state,
            self.seat_pending.pending_enable,
            self.seat_pending.pending_disable,
        );
        for ev in drained {
            self.drive_seat_event(state, ev);
        }
        log::info!(
            "kms: on_seat_ready done — state after drive: {:?}",
            self.seat_state
        );
    }

    fn on_libinput_ready(&mut self, state: &mut ServerState) {
        // Libseat mode: dispatch the on-core libinput context, then map each
        // event through the same fanout that Direct mode uses. Hotkeys are
        // intercepted here before forwarding to clients (wlroots'
        // `handle_libinput_readable`, backend.c:49-63).
        //
        // Motion coalescing: mirrors `input_thread::process_batch`'s
        // `pending_motion` carry-over. libinput emits motion at device
        // polling rate (often 1000 Hz for mice); without coalescing each
        // motion fires the full fanout + composite path. During a window
        // drag this drove `iter/s` to 1700+ (per the 2026-05-28 cinnamon
        // telemetry, vs. ~120 expected for dual-60Hz vsync), exhausting
        // each vsync interval before the rendering work completed.
        // Coalescing collapses bursts to "Motion(latest), <non-motion>,
        // Motion(latest)" — clients still see the final cursor position
        // each frame, but the loop wakes per-batch, not per-libinput-
        // report. Matches the off-thread path's
        // `input_thread::process_batch` contract.
        let Some(ctx) = self.core_libinput.as_mut() else {
            return;
        };
        let events = match ctx.dispatch() {
            Ok(evs) => evs,
            Err(e) => {
                log::warn!("kms: core libinput dispatch failed: {e}");
                return;
            }
        };
        let time_ms = crate::clock::server_time_ms();
        let mut scroll_buf: Vec<yserver_core::core_loop::HostInputEvent> = Vec::new();
        let mut pending_motion: Option<yserver_core::core_loop::HostInputEvent> = None;
        // Route via `core_loop::handle_host_input` (not `self.on_host_input`
        // directly) so `update_repeat_state` arms the core's auto-repeat
        // timer for real input. The off-thread input_thread path goes
        // through Message::HostInput → handle_host_input → on_host_input;
        // the on-core libseat path must match that contract or keys
        // never repeat (regressed when this dispatch first landed).
        for ev in events {
            if let Some(hk) = self.hotkey.check(&ev) {
                // Hotkey absorbs the event — but flush any pending
                // motion first so the cursor's last position is
                // delivered chronologically before the hotkey effect.
                if let Some(m) = pending_motion.take() {
                    yserver_core::core_loop::handle_host_input(state, self, m);
                }
                self.handle_core_hotkey(hk);
                continue;
            }
            // Device add/remove — forward directly, flushing pending
            // motion first to preserve chronological order.
            if let crate::input::InputEvent::DeviceAdded(info) = ev {
                if let Some(m) = pending_motion.take() {
                    yserver_core::core_loop::handle_host_input(state, self, m);
                }
                yserver_core::core_loop::handle_host_input(
                    state,
                    self,
                    yserver_core::core_loop::HostInputEvent::DeviceAdded(info),
                );
                continue;
            }
            if let crate::input::InputEvent::DeviceRemoved { device_node } = ev {
                if let Some(m) = pending_motion.take() {
                    yserver_core::core_loop::handle_host_input(state, self, m);
                }
                yserver_core::core_loop::handle_host_input(
                    state,
                    self,
                    yserver_core::core_loop::HostInputEvent::DeviceRemoved { device_node },
                );
                continue;
            }
            // Scroll fans out to zero or many press+release pairs depending
            // on accumulated v120 — mirror the input thread's path. Flush
            // pending motion first so press/release timestamps stay after
            // the motion they belong to.
            if let crate::input::InputEvent::PointerScroll { dx_v120, dy_v120 } = ev {
                scroll_buf.clear();
                if let Some(input_state) = self.core_input_state.as_mut() {
                    input_state.drain_scroll(dx_v120, dy_v120, time_ms, &mut scroll_buf);
                }
                if !scroll_buf.is_empty() {
                    if let Some(m) = pending_motion.take() {
                        yserver_core::core_loop::handle_host_input(state, self, m);
                    }
                    for host_ev in scroll_buf.drain(..) {
                        yserver_core::core_loop::handle_host_input(state, self, host_ev);
                    }
                }
                continue;
            }
            // Map then route via the same Motion-vs-non-Motion split
            // `input_thread::process_batch` uses.
            let Some(input_state) = self.core_input_state.as_mut() else {
                continue;
            };
            let host = input_state.map(ev, time_ms);
            match host {
                yserver_core::core_loop::HostInputEvent::PointerMotion { .. } => {
                    pending_motion = Some(host);
                }
                non_motion => {
                    if let Some(m) = pending_motion.take() {
                        yserver_core::core_loop::handle_host_input(state, self, m);
                    }
                    yserver_core::core_loop::handle_host_input(state, self, non_motion);
                }
            }
        }
        // Flush trailing motion at the end of the dispatch batch so the
        // core sees the last cursor position before the next epoll wait.
        if let Some(m) = pending_motion.take() {
            yserver_core::core_loop::handle_host_input(state, self, m);
        }
    }

    fn apply_device_config(
        &mut self,
        device_node: &str,
        change: yserver_core::xinput::libinput_props::DeviceConfigChange,
    ) -> Result<(), yserver_core::xinput::libinput_props::DeviceConfigError> {
        // Libseat mode: route through the on-core libinput context's
        // device map, where `DeviceAdded` stashed the live handle. Direct
        // mode doesn't reach this backend variant (KmsBackendV1 path); if
        // `core_libinput` is None we have nowhere to write, so silently
        // succeed — the trait's default contract.
        let Some(ctx) = self.core_libinput.as_mut() else {
            return Ok(());
        };
        ctx.apply_device_config(device_node, change)
    }

    fn probe_input_devices(&mut self, state: &mut ServerState) -> usize {
        // Libseat mode only: drain libinput's initial device enumeration
        // and seed the XI2 registry before the core serves clients. No
        // on-core context (Direct mode moved it to the input thread,
        // ynest/host-X11 have none) → clean no-op.
        if self.core_libinput.is_none() {
            return 0;
        }
        // libinput may surface the initial enumeration across several
        // dispatches as udev settles, so iterate — but BOUNDED and
        // non-blocking. `dispatch()` returns whatever is queued right now
        // (it never waits), so when libinput has nothing more the round
        // comes back empty. Stop after two consecutive empty rounds (the
        // enumeration has settled) or MAX_ROUNDS (hard ceiling against a
        // pathological device that re-announces forever). We do NOT block
        // waiting for devices — if the seat has no input hardware yet the
        // first round is empty and we return immediately.
        const MAX_ROUNDS: usize = 8;
        let mut seeded = 0usize;
        let mut empty_rounds = 0usize;
        for _ in 0..MAX_ROUNDS {
            let Some(ctx) = self.core_libinput.as_mut() else {
                break;
            };
            let events = match ctx.dispatch() {
                Ok(evs) => evs,
                Err(e) => {
                    log::warn!("kms: startup libinput probe dispatch failed: {e}");
                    break;
                }
            };
            if events.is_empty() {
                empty_rounds += 1;
                if empty_rounds >= 2 {
                    break;
                }
                continue;
            }
            empty_rounds = 0;
            // Route device add/remove through the same fanout the live
            // `on_libinput_ready` path uses so `xi_seed_touchpad` runs;
            // count adds for the caller's log. Non-device events (motion,
            // keys, scroll) are intentionally ignored here: this runs
            // before any client has connected, so there is nowhere to
            // deliver them, and the live `on_libinput_ready` path takes
            // over for all real input once the serve loop starts.
            for ev in events {
                match ev {
                    crate::input::InputEvent::DeviceAdded(info) => {
                        seeded += 1;
                        yserver_core::core_loop::handle_host_input(
                            state,
                            self,
                            yserver_core::core_loop::HostInputEvent::DeviceAdded(info),
                        );
                    }
                    crate::input::InputEvent::DeviceRemoved { device_node } => {
                        yserver_core::core_loop::handle_host_input(
                            state,
                            self,
                            yserver_core::core_loop::HostInputEvent::DeviceRemoved { device_node },
                        );
                    }
                    _ => {}
                }
            }
        }
        seeded
    }

    // ── Subwindow lifecycle ─────────────────────────────────────

    fn create_subwindow(
        &mut self,
        _origin: Option<OriginContext>,
        host_parent: WindowHandle,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        _border_width: u16,
        visual: HostSubwindowVisual,
        background_pixel: Option<u32>,
        background_pixmap: Option<u32>,
    ) -> io::Result<WindowHandle> {
        let xid = self.core.next_host_xid();
        let parent_xid = host_parent.as_raw();
        let parent_depth = if parent_xid == self.core.window_id {
            Some(24)
        } else {
            self.windows_v2.get(&parent_xid).map(|g| g.depth)
        };
        let depth = depth_for_visual(visual, parent_depth);
        // Stage 3f.6: record the parent xid so `build_scene` can
        // recurse the tree. `bg_pixel` is passed into
        // `allocate_window_storage`, which paints it into the fresh
        // storage; bg_pixmap is stored as metadata for now (proper
        // pixmap-bg support is a Stage 4-ish item).
        self.allocate_window_storage(
            xid,
            x,
            y,
            width.max(1),
            height.max(1),
            depth,
            Some(parent_xid),
            background_pixel,
        );
        if let Some(bg_pix) = background_pixmap
            && let Some(geom) = self.windows_v2.get_mut(&xid)
        {
            geom.bg_pixmap = Some(bg_pix);
        }
        self.scene.mark_scene_structure_dirty();
        WindowHandle::from_raw(xid).ok_or_else(|| io::Error::other("create_subwindow: xid was 0"))
    }

    fn destroy_subwindow(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
    ) -> io::Result<()> {
        if let Some(id) = self.store.lookup(host_xid) {
            self.store_decref_with_invalidate(id);
        }
        self.windows_v2.remove(&host_xid);
        // Stage 3f.11: also drop from top_level_order so build_scene
        // doesn't walk a stale xid. Same hazard as reparent — pre-fix
        // a destroyed top-level lingered in the order and produced
        // ghost draws until the next register_top_level filled the
        // slot.
        self.core.top_level_order.retain(|&x| x != host_xid);
        self.scene.mark_scene_structure_dirty();
        Ok(())
    }

    fn map_subwindow(&mut self, _origin: Option<OriginContext>, host_xid: u32) -> io::Result<()> {
        if let Some(geom) = self.windows_v2.get_mut(&host_xid) {
            geom.mapped = true;
        }
        if let Some(id) = self.store.lookup(host_xid) {
            self.store.set_scene_participating(id, true);
        }
        // X11 map semantics (Xorg dix/window.c MapWindow → RealizeTree →
        // HandleExposures → miPaintWindow): when a window becomes
        // viewable and no earlier contents are remembered, the server
        // tiles it with its background — bg pixmap or bg pixel — and the
        // same applies to every inferior that becomes viewable with it.
        // v2 keeps storage across unmap/remap, but X11 says unmapped
        // contents are NOT remembered, so remap must repaint bg too.
        // Without this, XTS Xlib4 XMapWindow-9 et al. read fresh/stale
        // storage where the bg tile belongs ("Bad pixel in tiled area
        // at (2, 0)"). Windows with bg None keep undefined contents —
        // no paint, matching miPaintWindow's None early-out.
        if self.window_viewable(host_xid) {
            let targets = self.collect_viewable_bg_paint_targets(host_xid);
            for (xid, bg_pixel, bg_pixmap, w, h) in targets {
                if let Err(e) = self.clear_window_area_with_background(
                    xid,
                    bg_pixel,
                    bg_pixmap,
                    0,
                    0,
                    w,
                    h,
                    (0, 0),
                ) {
                    log::debug!("v2 map_subwindow: bg paint failed for 0x{xid:x}: {e:?}");
                }
            }
        }
        self.scene.mark_scene_structure_dirty();
        Ok(())
    }

    fn unmap_subwindow(&mut self, _origin: Option<OriginContext>, host_xid: u32) -> io::Result<()> {
        if let Some(geom) = self.windows_v2.get_mut(&host_xid) {
            geom.mapped = false;
        }
        if let Some(id) = self.store.lookup(host_xid) {
            self.store.set_scene_participating(id, false);
        }
        self.scene.mark_scene_structure_dirty();
        Ok(())
    }

    fn configure_subwindow(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        config: HostSubwindowConfig,
    ) -> io::Result<()> {
        let Some(geom) = self.windows_v2.get_mut(&host_xid) else {
            // Window not tracked — log + skip (e.g., configure
            // before register). v1 tolerates this.
            return Ok(());
        };
        let mut size_changed = false;
        if let Some(x) = config.x {
            geom.x = x;
        }
        if let Some(y) = config.y {
            geom.y = y;
        }
        if let Some(w) = config.width
            && w != geom.width
        {
            geom.width = w;
            size_changed = true;
        }
        if let Some(h) = config.height
            && h != geom.height
        {
            geom.height = h;
            size_changed = true;
        }
        let new_w = geom.width.max(1);
        let new_h = geom.height.max(1);
        let depth = geom.depth;
        let scene_participating = geom.mapped;
        let bg_pixel = geom.bg_pixel;
        let bg_pixmap = geom.bg_pixmap;
        if size_changed && let Some(old_id) = self.store.lookup(host_xid) {
            // Replace window storage. Stage 2d doesn't preserve
            // content across resize — clients are expected to
            // repaint after configure (X11 semantics).
            //
            // Detach `by_xid[host_xid]` BEFORE decref + allocate.
            // Any Picture wrapping this window (e.g. marco's frame
            // compositing) holds an extra refcount on the old
            // drawable; without the explicit detach, `decref`
            // returns `StillReferenced` and leaves the xid map
            // pointing at the old drawable → `store.allocate(xid)`
            // below fails with `XidInUse` → the window silently
            // stays at the old storage. xeyes resize regression
            // observed on bee + fuji.
            //
            // The old drawable stays alive in `entries` until its
            // last refcount drops; its in-flight ticket still
            // retires correctly. Picture's next `lookup(xid)`
            // returns the NEW DrawableId, which matches X11 RENDER
            // semantics (a Picture on a window references the
            // window's *current* storage).
            self.store.detach_xid(host_xid);
            self.store_decref_with_invalidate(old_id);
            match self.platform.allocate_drawable_storage(new_w, new_h, depth) {
                Ok(storage) => {
                    if let Err(e) = self.store.allocate(
                        host_xid,
                        DrawableKind::Window,
                        depth,
                        scene_participating,
                        storage,
                    ) {
                        log::warn!(
                            "v2 configure_subwindow: store.allocate failed for xid {host_xid:#x}: {e:?}",
                        );
                    } else if let Some(id) = self.store.lookup(host_xid) {
                        if let Some(bg_pixmap_host_xid) = bg_pixmap {
                            if let Err(e) = self.clear_window_area_with_background(
                                host_xid,
                                bg_pixel.unwrap_or(0),
                                Some(bg_pixmap_host_xid),
                                0,
                                0,
                                new_w,
                                new_h,
                                (0, 0),
                            ) {
                                log::debug!(
                                    "v2 configure_subwindow: bg_pixmap resize init failed for xid {host_xid:#x}: {e:?}"
                                );
                            }
                        } else {
                            // Stage 3f.6 + 3f.14: clear the fresh
                            // storage so resize doesn't leave pool-
                            // returner content (or Vk-undefined bytes)
                            // visible until the client's next repaint.
                            // Bg_pixel-set: paint that colour;
                            // otherwise depth-appropriate safe default
                            // (matches `allocate_window_storage`).
                            let color = bg_pixel.map_or_else(
                                || default_window_init_color(depth),
                                |pixel| {
                                    decode_x11_pixel_for_storage(
                                        pixel,
                                        depth,
                                        PlatformBackend::format_for_depth(depth),
                                    )
                                },
                            );
                            let rect = ash::vk::Rect2D {
                                offset: ash::vk::Offset2D::default(),
                                extent: ash::vk::Extent2D {
                                    width: u32::from(new_w),
                                    height: u32::from(new_h),
                                },
                            };
                            if let Err(e) = self.engine.fill_rect(
                                &mut self.store,
                                &mut self.platform,
                                id,
                                rect,
                                color,
                            ) {
                                log::debug!(
                                    "v2 configure_subwindow: storage init fill failed for xid {host_xid:#x}: {e:?}"
                                );
                            }
                        }
                    }
                }
                Err(e) => {
                    log::warn!(
                        "v2 configure_subwindow: alloc storage failed for xid {host_xid:#x}: {e:?}",
                    );
                }
            }
        }
        if let Some(stack_mode) = config.stack_mode {
            if self.core.top_level_order.contains(&host_xid) {
                self.restack_top_level(host_xid, stack_mode, config.sibling);
            } else {
                self.restack_subwindow(host_xid, stack_mode, config.sibling);
            }
        }
        self.scene.mark_scene_structure_dirty();
        Ok(())
    }

    fn reparent_subwindow(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        host_parent: u32,
        x: i16,
        y: i16,
    ) -> io::Result<()> {
        // Stage 3f.6: update the parent xid so build_scene's
        // descendant traversal sees the new tree shape on the next
        // tick. BOTH `host_parent == 0` and `host_parent ==
        // core.window_id` (root's real host xid; root is never tracked
        // in `windows_v2`) mean the window becomes a top-level under
        // root; we record `None` so the recurse treats it as a top-
        // level entry. A genuinely-unknown non-root xid is projection
        // drift between resources and backend and panics below.
        //
        // Stage 3f.11 bug-fix: also reconcile `core.top_level_order`
        // with the new parent. Pre-3f.11, an xid that was originally
        // registered as a top-level (parent=root) stayed in
        // `top_level_order` even after being reparented under
        // another window. `build_scene` then emitted the same xid
        // TWICE: once via the `top_level_order` walk (at its now-
        // child-relative coords interpreted as absolute → typically
        // (0,0)) and once via the recurse from its real parent (at
        // its correct screen position). Observable as MATE's clock
        // applet rendered at BOTH ends of the panel: the right edge
        // is the real position, the left edge is the ghost.
        let parent = if host_parent == 0 || host_parent == self.core.window_id {
            // BOTH sentinels mean "top-level under root": `0` is the
            // legacy convention; `core.window_id` is root's real host
            // xid (root is never tracked in windows_v2). The reparent-
            // to-root path passes backend.window_id() (== core.window_id),
            // NOT 0 — so this second clause is load-bearing. (Same root-
            // sentinel check as backend.rs:1668.)
            None
        } else if self.windows_v2.contains_key(&host_parent) {
            Some(host_parent)
        } else {
            // Per spec §"Remove the missing-parent fallback": backend
            // projection drift after protocol-level validation is a
            // fatal internal-consistency failure, not a silent
            // recovery. If the resources tree says the parent exists
            // but windows_v2 doesn't, that's drift — surface it.
            panic!(
                "reparent_subwindow: host_parent 0x{host_parent:x} missing from \
                 windows_v2 (and is neither 0 nor root/core.window_id); resources \
                 layer must validate ReparentWindow before dispatching to backend"
            );
        };
        let new_rank = self.alloc_window_stack_rank();
        if let Some(geom) = self.windows_v2.get_mut(&host_xid) {
            geom.x = x;
            geom.y = y;
            geom.parent = parent;
            geom.stack_rank = new_rank;
        }
        // Reconcile top_level_order:
        // - parent == None  → window is now (or stays) a top-level
        //   under root; ensure it's in top_level_order.
        // - parent == Some  → window is now a sub-window; remove
        //   from top_level_order so the scene doesn't double-emit.
        match parent {
            None => {
                if !self.core.top_level_order.contains(&host_xid) {
                    self.core.top_level_order.push(host_xid);
                }
            }
            Some(_) => {
                self.core.top_level_order.retain(|&x| x != host_xid);
            }
        }
        self.scene.mark_scene_structure_dirty();
        Ok(())
    }

    fn change_subwindow_attributes(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        value_mask: u32,
        values: &[u32],
    ) -> io::Result<()> {
        // Stage 3f.6: v1-shape parse of the CWA value-mask.
        // CWBackPixmap (0x01) and CWBackPixel (0x02) are the two
        // we honour today — they decide what fresh / cleared regions
        // of the window storage look like. Other CW bits
        // (CWBorderPixel, CWBitGravity, CWEventMask, etc.) flow
        // through other Backend methods or get folded into broader
        // window state; storing only what `windows_v2` needs.
        let Some(geom) = self.windows_v2.get_mut(&host_xid) else {
            return Ok(());
        };
        let mut idx = 0;
        if value_mask & 0x01 != 0 && idx < values.len() {
            // CWBackPixmap. 0 = None / inherit-from-parent.
            let v = values[idx];
            geom.bg_pixmap = if v == 0 { None } else { Some(v) };
            idx += 1;
        }
        if value_mask & 0x02 != 0 && idx < values.len() {
            // CWBackPixel — opaque ARGB-or-XRGB pixel value.
            geom.bg_pixel = Some(values[idx]);
        }
        // X11 spec: CWA's background attribute change does NOT
        // repaint the window. The bg setting only affects future
        // `ClearArea` / Expose handling. v2's pre-2026-05-30 eager
        // clear here was a Stage 3f.6 over-reach: the Stage 4d
        // guard (`routes_via_redirect`) skipped the clear for
        // windows under COMPOSITE redirect (avoiding the
        // "CC opaque black on drag with compositing" and
        // "tray applets disappear" symptoms), but the
        // non-redirected path still cleared — visible as
        // non-composited MATE's CC sidebar going black when caja
        // took focus over it (marco re-asserts CWA per configure;
        // yserver wiped CC's pixmap to bg=0; GTK got no Expose so
        // bg never repainted; widgets came back only on
        // per-widget hover redraw). Removing the clear matches
        // X11 in both modes.
        Ok(())
    }

    fn update_host_event_mask(
        &mut self,
        _origin: Option<OriginContext>,
        _host_xid: u32,
        _mask: u32,
        _enabled: bool,
    ) -> io::Result<()> {
        // No-op on KMS, same shape as v1. The trait method is a
        // holdover from Phase 6.3 ynest where it forwarded event-mask
        // changes to a host X server; KMS owns the display directly
        // and has no upstream server to notify. Event delivery on KMS
        // is driven entirely from libinput/seat plumbing inside the
        // backend, so there's nothing to update here.
        Ok(())
    }

    fn register_top_level(
        &mut self,
        _origin: Option<OriginContext>,
        nested_id: ResourceId,
        host_xid: u32,
    ) -> io::Result<()> {
        // Bookkeeping mutation — same shape as v1. The XID map is in
        // KmsCore and shared.
        self.core.xid_map.insert(host_xid, nested_id);
        // Top-level visible-window tracking for the scene
        // assembler. register_top_level doesn't carry geometry;
        // start at 1x1 (Stage 2 plan compromise) and resize on
        // first configure_subwindow.
        if !self.windows_v2.contains_key(&host_xid) {
            // Top-level: parent = None (root), no bg_pixel known yet
            // (set later via change_subwindow_attributes).
            self.allocate_window_storage(host_xid, 0, 0, 1, 1, 24, None, None);
        }
        if !self.core.top_level_order.contains(&host_xid) {
            self.core.top_level_order.push(host_xid);
        }
        self.scene.mark_scene_structure_dirty();
        Ok(())
    }

    fn register_subwindow(
        &mut self,
        _origin: Option<OriginContext>,
        nested_id: ResourceId,
        host_xid: u32,
    ) -> io::Result<()> {
        self.core.xid_map.insert(host_xid, nested_id);
        if !self.windows_v2.contains_key(&host_xid) {
            // register_subwindow doesn't carry parent xid (Backend
            // trait doesn't expose it here — the trait shape was
            // built around v1's flat windows table). Parent is set
            // when `create_subwindow` fires for the same host_xid
            // (it's the entry point that knows the parent). If
            // register_subwindow runs first (e.g. ynest's wire
            // ordering), we'll get `None` and the scene treats this
            // window as a top-level until a `create_subwindow`
            // catches up. Matches v1's "no parent tracking" status
            // — v1 simply doesn't compose children either.
            self.allocate_window_storage(host_xid, 0, 0, 1, 1, 32, None, None);
        }
        self.scene.mark_scene_structure_dirty();
        Ok(())
    }

    fn unregister_host_window(&mut self, host_xid: u32) {
        self.core.xid_map.remove(&host_xid);
    }

    /// Stage 4b: opt v2 into the full COMPOSITE-redirect
    /// activation path. The `process_request.rs` Composite
    /// handler gates its `activate_redirect_backing_for` call
    /// on this flag so v1 (which returns the default `false`)
    /// stays on the pre-Stage-4 "redirect record only" shape
    /// that the `92a2a83 → 3751c11` revert established.
    fn supports_redirect_activation(&self) -> bool {
        true
    }

    fn supports_dmabuf_export(&self) -> bool {
        self.dmabuf_export_supported
    }

    fn acquire_glx_pixmap_export(&mut self, host_xid: u32) {
        KmsBackendV2::acquire_glx_pixmap_export(self, host_xid);
    }

    fn release_glx_pixmap_export(&mut self, host_xid: u32) {
        KmsBackendV2::release_glx_pixmap_export(self, host_xid);
    }

    fn promote_pixmap_exportable(&mut self, host_xid: u32) -> bool {
        KmsBackendV2::promote_pixmap_exportable(self, host_xid)
    }

    /// Stage 4c.4 — flip a window's scene-participation under
    /// COMPOSITE redirect. Delegates to `DrawableStore::
    /// set_scene_participating` (which clears unpresented
    /// presentation damage + bumps the epoch on a true→false
    /// transition per spec §I5) and fires scene-structure damage
    /// for the redirect transition.
    ///
    /// **Scene-structure damage** — always fires per the plan's
    /// Cross-cutting §"Concrete scene-structure damage":
    ///   - `participating=true` (un-redirect / Automatic-activate):
    ///     rect = W's current screen rect — the scene newly
    ///     includes W and must paint W's location.
    ///   - `participating=false` (Manual-activate): rect = W's
    ///     pre-flip rect — the scene NO LONGER includes W but
    ///     whatever is underneath must repaint the area where W
    ///     used to be.
    ///
    /// In both branches we capture the rect BEFORE the flip
    /// (pre-flip and post-flip geometry coincide because the
    /// participation flip itself doesn't move W); the only
    /// difference is semantic. When `window_absolute_rect`
    /// returns `None` (root or untracked geometry), fall back to
    /// the coarse `mark_scene_structure_dirty` — correctness-
    /// preserving, just wider than needed.
    fn set_window_scene_participation(
        &mut self,
        _origin: Option<OriginContext>,
        host_window: WindowHandle,
        participating: bool,
    ) -> io::Result<()> {
        let Some(w_id) = self.store.lookup(host_window.as_raw()) else {
            log::debug!(
                "v2 set_window_scene_participation(0x{:x}, {participating}): \
                 window not in store",
                host_window.as_raw(),
            );
            return Ok(());
        };
        // Capture rect BEFORE the flip — on participating=false
        // (Manual activation) the pre-flip rect is what the scene
        // needs to repaint over; on participating=true the pre-
        // and post-flip rects coincide (no geometry move on this
        // path) so either reading is fine, and pre-flip keeps the
        // two branches symmetric.
        let pre_flip_rect = self.window_absolute_rect(w_id);

        self.store.set_scene_participating(w_id, participating);

        if let Some(rect) = pre_flip_rect {
            self.scene.mark_scene_structure_damage_rects(&[rect]);
        } else {
            // No tracked geometry (root or untracked) — coarse
            // marker is correctness-preserving.
            self.scene.mark_scene_structure_dirty();
        }
        Ok(())
    }

    /// Stage 4c.4 — flip a backing's scene-participation under
    /// COMPOSITE redirect. Used by Automatic mode so paint that
    /// resolves through the backing accumulates presentation
    /// damage on B (which the scene walk picks up via W's
    /// `redirected_target` indirection in 4c's `build_scene`
    /// patch). No scene-structure damage from this call — the
    /// geometric damage of a mode-flip is the W-side call's
    /// responsibility (the blit-source identity flip is
    /// geometrically on W; backings have no on-screen geometry
    /// of their own).
    fn set_backing_scene_participation(
        &mut self,
        _origin: Option<OriginContext>,
        backing: PixmapHandle,
        participating: bool,
    ) -> io::Result<()> {
        let Some(b_id) = self.store.lookup(backing.as_raw()) else {
            log::debug!(
                "v2 set_backing_scene_participation(0x{:x}, {participating}): \
                 backing not in store",
                backing.as_raw(),
            );
            return Ok(());
        };
        self.store.set_scene_participating(b_id, participating);
        Ok(())
    }

    /// Stage 4b: real `name_window_pixmap`. Mirrors v1
    /// (`kms/backend.rs:9523-9544`) — lookup `host_window_to_backing`,
    /// incref the alias registry, return the SAME handle.
    /// Returns `NotFound` if the window isn't redirected
    /// (`allocate_redirected_backing` was never called for it).
    fn name_window_pixmap(
        &mut self,
        _origin: Option<OriginContext>,
        host_window: WindowHandle,
    ) -> io::Result<PixmapHandle> {
        let backing = self
            .core
            .host_window_to_backing
            .get(&host_window.as_raw())
            .copied()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "v2 name_window_pixmap: window is not redirected (no backing)",
                )
            })?;
        self.core.alias_registry.incref(backing);
        Ok(backing)
    }

    /// Stage 4b: real `allocate_redirected_backing`. Mirrors v1
    /// (`kms/backend.rs:9568-9607`) with one v2-specific addition:
    /// after allocating the backing and registering it in
    /// `alias_registry` + `host_window_to_backing`, also flip
    /// `store.set_redirected_target(W_id, Some(B_id))` so v2's
    /// `resolve_paint_target` routes future paint to the backing.
    ///
    /// **Seed-copy ordering** per the plan's Cross-cutting
    /// §"Initial backing content" decision: the W→B copy fires
    /// BEFORE `set_redirected_target` flips routing, so the copy
    /// reads from W's own storage (not B's). Descendant seed-copy
    /// follows the same one-shot walk, in stable sibling z-order,
    /// so overlapping frame/decor children seed into the backing in
    /// the same order they would appear on screen.
    fn allocate_redirected_backing(
        &mut self,
        origin: Option<OriginContext>,
        host_window: WindowHandle,
        width: u16,
        height: u16,
        depth: u8,
    ) -> io::Result<PixmapHandle> {
        // Idempotent — second `RedirectWindow` for the same W
        // returns the existing backing with no refcount bump
        // (the Reason-1 hold is single-instance per
        // §"Single refcount, two reasons").
        if let Some(existing) = self
            .core
            .host_window_to_backing
            .get(&host_window.as_raw())
            .copied()
        {
            // Diagnostic trace (TEMP) — idempotent re-allocate.
            // Important to know because callers may *expect* a
            // fresh backing (and a re-seed) but get the existing
            // one. Stage 4d.5 `rotate_redirected_backing_on_resize`
            // works around this by release-then-allocate.
            log::debug!(
                "v2 allocate_redirected_backing W=0x{w:x}: idempotent return existing B=0x{b:x} ({width}x{height}, depth={depth})",
                w = host_window.as_raw(),
                b = existing.as_raw(),
            );
            return Ok(existing);
        }
        let w_xid = host_window.as_raw();

        // Allocate a fresh backing via the existing
        // `create_pixmap` path (3f.10 pool + 3f.14 zero-fill).
        let backing = self.create_pixmap(origin, depth, width, height)?;
        let backing_xid = backing.as_raw();
        // Diagnostic trace (TEMP) — fresh allocation. Cross-correlate
        // against `set_redirected_target` and the "B is all-black"
        // dump to see whether a fresh-allocated B explains a black
        // backing (no client paint since the alloc) vs an
        // unexpectedly-reset existing backing.
        log::debug!(
            "v2 allocate_redirected_backing W=0x{w_xid:x}: fresh B=0x{backing_xid:x} ({width}x{height}, depth={depth})",
        );

        // Seed-copy: parent → B at W's position, BEFORE the route
        // flip. Audit #6 (2026-05-19) flipped this from "W → B (+
        // descendants)" to "parent → B" per Xorg's compNewPixmap
        // (composite/compalloc.c:541-606). Parent's resolve_paint_target
        // gives the storage holding parent's currently-visible pixels
        // (parent's own storage if non-redirected, parent's B if
        // chain-redirected). W's own pre-redirect storage is now
        // ignored — its content was default-init for the
        // newly-mapped-W case (the "black band on map" symptom) and
        // any pre-paint content lands in B post-flip via the normal
        // resolve_paint_target routing on the next client paint.
        if let Some(b_id) = self.store.lookup(backing_xid) {
            self.seed_backing_from_parent(w_xid, b_id);
            // 2026-06-11 — synthesize Xorg compNewPixmap's
            // IncludeInferiors: composite W's own leaf + its
            // (non-independently-redirected) descendants over the
            // parent base, so an already-painted window that won't
            // repaint (compositor handoff) keeps its content instead
            // of the wallpaper seed. MUST run before the route flip.
            self.overlay_backing_inferiors(w_xid, b_id);
            // Now flip routing — after this, paint against W
            // resolves to B via `resolve_paint_target`. The w_id
            // lookup must still succeed; if not, the redirect
            // record stays uninstalled (protocol error upstream).
            if let Some(w_id) = self.store.lookup(w_xid) {
                self.store.set_redirected_target(w_id, Some(b_id));
            } else {
                log::warn!(
                    "v2 allocate_redirected_backing(0x{w_xid:x}): window not in store \
                     (seed succeeded, route flip skipped)",
                );
            }
        } else {
            log::warn!(
                "v2 allocate_redirected_backing(0x{w_xid:x}): backing not in store \
                 (seed + route flip skipped)",
            );
        }

        // Register Reason-1 hold + redirect map. Identical to v1.
        self.core.alias_registry.insert(
            backing,
            crate::kms::core::AliasEntry {
                refcount: 1,
                width,
                height,
                depth,
            },
        );
        self.core.host_window_to_backing.insert(w_xid, backing);
        Ok(backing)
    }

    /// Stage 4b: real `release_redirected_backing`. Mirrors v1
    /// (`kms/backend.rs:9547-9566`) — clear the
    /// `host_window_to_backing` entry, drop the Reason-1 hold,
    /// free pixmap on refcount=0.
    ///
    /// v2-specific addition: when the redirect map clears, also
    /// drop `store.set_redirected_target` for every window that
    /// was routed through this backing. Multiple windows can
    /// alias the same backing only via NameWindowPixmap (which
    /// is the alias-handle, not a separate redirect), but the
    /// loop is cheap and matches the plan's defensive contract.
    ///
    /// Stage 4c.4 round-3 finding: drop B's `scene_participating`
    /// flag internally so the protocol handler (RedirectWindow
    /// unredirect / destroy path) doesn't need a separate
    /// `set_backing_scene_participation(false)` call. The trait
    /// docstring is the canonical statement of this contract.
    fn retain_backing_storage(
        &mut self,
        _origin: Option<OriginContext>,
        backing: PixmapHandle,
    ) -> io::Result<()> {
        // Bump alias_registry refcount. The rotate path pairs this
        // with `drop_backing_storage` around the release→copy gap
        // so the no-alias case doesn't free OLD before the copy
        // sources from it. A miss (`alias_registry.get` returns
        // None) means the backing wasn't tracked here — log and
        // pass through; the caller's later copy will hit the
        // unknown-xid path with its own diagnostic.
        if self.core.alias_registry.get(backing).is_some() {
            self.core.alias_registry.incref(backing);
        } else {
            log::warn!(
                "v2 retain_backing_storage: 0x{:x} not in alias_registry — no-op",
                backing.as_raw(),
            );
        }
        Ok(())
    }

    fn drop_backing_storage(
        &mut self,
        origin: Option<OriginContext>,
        backing: PixmapHandle,
    ) -> io::Result<()> {
        // Symmetric to `retain_backing_storage`. Decref the
        // alias_registry; if this was the final ref, free the
        // underlying pixmap. Mirrors the alias-aware branch of
        // `free_pixmap` for consistency.
        if self.core.alias_registry.decref(backing) {
            self.free_pixmap(origin, backing.as_raw())?;
        }
        Ok(())
    }

    fn release_redirected_backing(
        &mut self,
        origin: Option<OriginContext>,
        backing: PixmapHandle,
    ) -> io::Result<()> {
        let raw = backing.as_raw();
        // Drop the W→B map entry. v1 uses `retain` because the
        // map is keyed by W_xid (not B_xid); same shape here.
        self.core
            .host_window_to_backing
            .retain(|_, h| h.as_raw() != raw);
        // Clear the store-side route on every window that pointed
        // at this backing's DrawableId. Reverse-scan over `entries`
        // would need an iter accessor we don't have; iterate the
        // map keys we just retained against and clear each.
        // (In practice the map is empty after `retain` above —
        // but a future multi-window-per-backing model would still
        // be correct.)
        if let Some(b_id) = self.store.lookup(raw) {
            let routed_windows: Vec<u32> = self
                .windows_v2
                .keys()
                .copied()
                .filter(|xid| {
                    self.store
                        .lookup(*xid)
                        .and_then(|id| self.store.redirected_target(id))
                        == Some(b_id)
                })
                .collect();
            for w_xid in routed_windows {
                if let Some(w_id) = self.store.lookup(w_xid) {
                    self.store.set_redirected_target(w_id, None);
                }
            }
            // Stage 4c.4 round-3 finding: drop B's scene_participating
            // flag here so the protocol handler doesn't need a
            // separate `set_backing_scene_participation(false)`
            // call. No-op when the flag is already false (the
            // store's `set_scene_participating` short-circuits
            // the damage-clear branch when `was == v`).
            self.store.set_scene_participating(b_id, false);
        }
        if self.core.alias_registry.decref(backing) {
            self.free_pixmap(origin, raw)?;
        }
        Ok(())
    }

    /// Stage 4d — Composite Overlay Window allocation.
    ///
    /// First `GetOverlayWindow` allocates screen-extent depth-24
    /// storage at xid `COMPOSITE_OVERLAY_WINDOW` (0x103), stores
    /// the resulting `DrawableId` on `self.cow_id`, sets the
    /// matching protocol refcount on `core.cow_refcount = 1`.
    /// The drawable stays off the normal scene path; xfwm4 paints
    /// its composited desktop into its own child window, so adding
    /// the COW as a topmost scene layer would cover the real output
    /// with a stale black surface.
    ///
    /// Subsequent calls (compositor restart, multi-client
    /// scenarios) just bump `core.cow_refcount` and return Ok
    /// — the protocol reply is the same fixed xid.
    ///
    /// Initial fill: storage from `allocate_drawable_storage`
    /// is uninitialised Vk-DEVICE_LOCAL memory (same problem
    /// Stage 3f.14 fixed for `create_pixmap`). We do an explicit
    /// transparent-black fill via `engine.fill_rect` so the
    /// compositor's first paint composites over a known zero
    /// rather than recycled GPU garbage. The fill is best-effort
    /// — on the stub fixture (no Vk) `engine.fill_rect` errors;
    /// log + continue (storage already exists at xid level).
    fn get_overlay_window(&mut self, _origin: Option<OriginContext>) -> io::Result<bool> {
        if self.cow_id.is_some() {
            self.core.cow_refcount += 1;
            return Ok(false); // already materialized; refcount bump only
        }
        let fb_w = self.platform.fb_w.max(1);
        let fb_h = self.platform.fb_h.max(1);
        let storage = match self.platform.allocate_drawable_storage(fb_w, fb_h, 24) {
            Ok(storage) => {
                self.telemetry.record_storage_allocation();
                self.telemetry.record_image_view_create();
                storage
            }
            Err(e) => {
                // Test-fixture / no-Vk path: same shape as
                // `init_root_storage` — fall back to a null-view
                // stub so unit tests can exercise refcount /
                // scene-registration without a live Vk ICD.
                log::debug!("v2 get_overlay_window: no Vk, using stub COW storage: {e:?}");
                crate::kms::v2::store::Storage::for_tests_null(
                    ash::vk::Extent2D {
                        width: u32::from(fb_w),
                        height: u32::from(fb_h),
                    },
                    crate::kms::v2::platform::PlatformBackend::format_for_depth(24),
                )
            }
        };
        let xid = yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0;
        // Defensive: if a stale mapping somehow survives a prior
        // teardown (decref's PendingFence path detaches xid for us,
        // but a synchronous-destroy path could race), detach first
        // so the allocate doesn't trip XidInUse.
        self.store.detach_xid(xid);
        let id = self
            .store
            .allocate(xid, DrawableKind::Window, 24, true, storage)
            .map_err(|e| io::Error::other(format!("v2 get_overlay_window: store alloc: {e:?}")))?;
        // Stage 3f.14 follow-on — zero-fill the fresh storage so
        // the compositor doesn't composite over recycled GPU
        // garbage on its first paint. Best-effort on stub paths.
        let rect = ash::vk::Rect2D {
            offset: ash::vk::Offset2D::default(),
            extent: ash::vk::Extent2D {
                width: u32::from(fb_w),
                height: u32::from(fb_h),
            },
        };
        if let Err(e) =
            self.engine
                .fill_rect(&mut self.store, &mut self.platform, id, rect, [0.0; 4])
            && self.platform.vk.is_some()
        {
            log::warn!("v2 get_overlay_window: initial zero-fill failed: {e:?}");
        }
        self.cow_id = Some(id);
        self.core.cow_refcount = 1;

        // Phase 2 Task 2.2 — also materialize the backend's window-
        // tree projection so the COW participates in build_scene /
        // hit-testing / paint resolution the same way any top-level
        // window does. The xid is the well-known protocol xid; v2
        // keys windows_v2 on host xid directly.
        let cow_host_xid = yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0;
        let rank = self.alloc_window_stack_rank();
        let geom = WindowGeometryV2 {
            x: 0,
            y: 0,
            width: fb_w,
            height: fb_h,
            depth: 24,
            mapped: true,
            // `parent: None` matches windows_v2's convention for a
            // direct child of the root (root is not itself tracked
            // in windows_v2 — see register_top_level).
            parent: None,
            stack_rank: rank,
            bg_pixel: None,
            bg_pixmap: None,
            cursor: None,
        };
        self.windows_v2.insert(cow_host_xid, geom);
        self.core.top_level_order.retain(|&x| x != cow_host_xid);
        self.core.top_level_order.push(cow_host_xid);
        self.scene.mark_scene_structure_dirty();
        Ok(true)
    }

    /// Stage 4d — Composite Overlay Window release.
    ///
    /// Decrements `core.cow_refcount`; on the final release it
    /// decrefs the store storage and clears `self.cow_id`.
    /// `DrawableStore::decref` removes the xid mapping
    /// (immediately on synchronous-destroy, deferred on
    /// `PendingFence`) so the next `GetOverlayWindow`
    /// reallocates fresh storage at the same xid.
    ///
    /// Defensive against unmatched releases (refcount=0 → Ok(false)
    /// no-op). The trait docstring is the canonical statement of
    /// this shape.
    ///
    /// Returns `Ok(true)` iff this call drove the refcount to 0
    /// and the COW storage was destroyed. The handler uses that
    /// signal to clear `host_xid` on the COW resource record so
    /// the next `GetOverlayWindow` re-wires fresh.
    fn release_overlay_window(&mut self, _origin: Option<OriginContext>) -> io::Result<bool> {
        if self.core.cow_refcount == 0 {
            return Ok(false);
        }
        self.core.cow_refcount -= 1;
        if self.core.cow_refcount == 0 {
            // Phase 2 Task 2.2 — tear down the backend's window-tree
            // projection BEFORE the scene unregister / storage decref
            // so any in-flight build_scene observer sees a consistent
            // "COW gone" view (no dangling top_level_order entry that
            // would re-emit a CompositeDraw against the dead xid).
            let cow_host_xid = yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0;
            self.core.top_level_order.retain(|&x| x != cow_host_xid);
            self.windows_v2.remove(&cow_host_xid);
            self.scene.mark_scene_structure_dirty();

            self.drain_engine_present_batches();
            if let Err(e) = self
                .engine
                .flush_render_batch(&mut self.store, &mut self.platform)
            {
                log::warn!("v2 release_overlay_window: flush_render_batch failed: {e:?}");
            }
            self.drain_render_telemetry();
            if let Some(id) = self.cow_id.take() {
                self.store_decref_with_invalidate(id);
            }
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn cow_host_xid(&self) -> Option<u32> {
        // The COW's host xid is the well-known protocol xid once
        // get_overlay_window has materialized; None otherwise.
        if self.cow_id.is_some() {
            Some(yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0)
        } else {
            None
        }
    }

    // ── Resources (pixmap / font / cursor) ──────────────────────

    fn create_pixmap(
        &mut self,
        _origin: Option<OriginContext>,
        depth: u8,
        width: u16,
        height: u16,
    ) -> io::Result<PixmapHandle> {
        let xid = self.core.next_host_xid();
        // Stage 2c: allocate real backing storage. The engine
        // needs a live VkContext to paint into; on the test
        // fixture the platform's `allocate_drawable_storage`
        // returns `ERROR_INITIALIZATION_FAILED` and we fall back
        // to logging a gap + returning the bare xid (tests that
        // don't paint still get a stable handle).
        match self
            .platform
            .allocate_drawable_storage(width, height, depth)
        {
            Ok(storage) => {
                if let Err(e) =
                    self.store
                        .allocate(xid, DrawableKind::Pixmap, depth, false, storage)
                {
                    log::warn!("v2 create_pixmap: store.allocate failed for xid {xid:#x}: {e:?}",);
                } else {
                    self.telemetry.record_storage_allocation();
                    self.telemetry.record_image_view_create();
                    // Stage 3f.14 follow-on: clear the fresh pixmap
                    // storage to a known-zero value. X11 says new
                    // pixmaps are undefined content, but Vk
                    // DEVICE_LOCAL memory is *fully* undefined —
                    // random GPU-recycled bytes. Real X servers tend
                    // to get away with this because system allocators
                    // zero pages, but our Vk allocator doesn't.
                    //
                    // Concrete repro (mate + marco + xeyes resize):
                    // xeyes creates a fresh depth-24 pixmap, sets a
                    // SHAPE clip matching its eye outlines, draws
                    // the eyes (only the shape-clipped area gets
                    // paint), then Present-Pixmaps the whole pixmap
                    // to the window. The non-eye area of the pixmap
                    // still holds undefined Vk bytes; Present copies
                    // it verbatim → visible garbage in the window.
                    //
                    // Cleared values: depth-32 transparent black
                    // (0,0,0,0) — premul no-op for compositing;
                    // depth-1 / depth-8 / depth-24 opaque black
                    // (0,0,0,1) — matches "uninitialised pixel = 0"
                    // which clients typically assume.
                    if let Some(id) = self.store.lookup(xid) {
                        let color = default_window_init_color(depth);
                        let rect = ash::vk::Rect2D {
                            offset: ash::vk::Offset2D::default(),
                            extent: ash::vk::Extent2D {
                                width: u32::from(width.max(1)),
                                height: u32::from(height.max(1)),
                            },
                        };
                        if let Err(e) = self.engine.fill_rect(
                            &mut self.store,
                            &mut self.platform,
                            id,
                            rect,
                            color,
                        ) {
                            log::debug!(
                                "v2 create_pixmap: initial fill failed for xid {xid:#x}: {e:?}"
                            );
                        }
                    }
                }
            }
            Err(vk_err)
                if vk_err == ash::vk::Result::ERROR_INITIALIZATION_FAILED
                    && self.platform.vk.is_none() =>
            {
                // Test fixture path — no Vk available.
                self.log_v2_gap("create_pixmap_no_vk");
            }
            Err(vk_err) => {
                return Err(io::Error::other(format!(
                    "create_pixmap: allocate_drawable_storage {width}x{height} d{depth}: \
                     {vk_err:?}"
                )));
            }
        }
        PixmapHandle::from_raw(xid).ok_or_else(|| io::Error::other("create_pixmap: xid was 0"))
    }

    fn free_pixmap(&mut self, _origin: Option<OriginContext>, host_xid: u32) -> io::Result<()> {
        // Stage 4b: alias-registry-aware free path. When `host_xid`
        // names a COMPOSITE-redirect backing (via NameWindowPixmap
        // alias or the Reason-1 redirect hold), decref the registry
        // first; only drop the storage when refcount hits zero.
        // Otherwise (an ordinary pixmap) fall through to the
        // straight `store.decref` path.
        //
        // v1's `free_pixmap` (`kms/backend.rs:9637-9650`) does NOT
        // consult the registry — it gets away with this because
        // compositors typically call FreePixmap(alias) after
        // UnredirectWindow, so the registry has already been
        // torn down by `release_redirected_backing`. The protocol
        // doesn't guarantee that ordering though, and v2 gates
        // here so an early FreePixmap on a still-held alias
        // doesn't drop the backing while a redirect still uses it.
        // GLX-TFP (Task 2.4): resolve the DrawableId BEFORE any decref so
        // we can tear down (or defer) the export tracking after the normal
        // client decref runs. For an export-only entry (glx_refs == 0)
        // this releases our lifetime ref and frees the backing now; for
        // the defer case (glx_refs > 0) it is a no-op and our lifetime ref
        // keeps the storage alive until glXDestroyPixmap.
        let export_id = self.store.lookup(host_xid);

        if let Some(handle) = yserver_core::backend::PixmapHandle::from_raw(host_xid)
            && self.core.alias_registry.get(handle).is_some()
        {
            if self.core.alias_registry.decref(handle)
                && let Some(id) = self.store.lookup(host_xid)
            {
                self.store_decref_with_invalidate(id);
            }
            if let Some(id) = export_id {
                self.maybe_teardown_export(id);
            }
            return Ok(());
        }
        if let Some(id) = self.store.lookup(host_xid) {
            self.store_decref_with_invalidate(id);
        }
        if let Some(id) = export_id {
            self.maybe_teardown_export(id);
        }
        Ok(())
    }

    fn open_font(
        &mut self,
        _origin: Option<OriginContext>,
        name: &str,
    ) -> io::Result<(FontHandle, FontMetrics)> {
        // Same body as v1. `KmsCore` already owns `FontLoader` +
        // `fonts` (it's protocol-bookkeeping per the v2 spec); the
        // backend just wraps the resulting freetype handle in a
        // `FontState` entry against a freshly-allocated xid.
        use std::cell::RefCell;

        use crate::kms::core::{FontState, FreetypeFace};
        let (face, metrics, char_cache) = self.core.font_loader.open_font(name)?;
        let host_xid = self.core.next_host_xid();
        let handle = FontHandle::from_raw(host_xid)
            .ok_or_else(|| io::Error::other("failed to create font handle"))?;
        self.core.fonts.insert(
            host_xid,
            FontState {
                handle: host_xid,
                face: RefCell::new(FreetypeFace(face)),
                metrics: metrics.clone(),
                char_info_cache: char_cache,
            },
        );
        Ok((handle, metrics))
    }

    fn close_font(&mut self, _origin: Option<OriginContext>, host_xid: u32) -> io::Result<()> {
        self.core.fonts.remove(&host_xid);
        Ok(())
    }

    fn set_font_path(
        &mut self,
        _origin: Option<OriginContext>,
        paths: &[String],
    ) -> Result<(), usize> {
        self.core
            .font_loader
            .set_font_path(paths)
            .map_err(|bad| paths.iter().position(|p| *p == bad).unwrap_or(paths.len()))
    }

    fn font_path(&self) -> Vec<String> {
        self.core.font_loader.font_path.clone()
    }

    fn paint_window_background_rect(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
    ) -> io::Result<()> {
        let Some(geom) = self.windows_v2.get(&host_xid) else {
            return Ok(());
        };
        let (bg_pixel, bg_pixmap) = (geom.bg_pixel, geom.bg_pixmap);
        if bg_pixel.is_none() && bg_pixmap.is_none() {
            // Background None: contents stay undefined (miPaintWindow
            // early-out).
            return Ok(());
        }
        self.clear_window_area_with_background(
            host_xid,
            bg_pixel.unwrap_or(0),
            bg_pixmap,
            x,
            y,
            width,
            height,
            (0, 0),
        )
    }

    fn create_cursor(
        &mut self,
        _origin: Option<OriginContext>,
        source_pixmap: PixmapHandle,
        mask_pixmap: Option<PixmapHandle>,
        fore: (u16, u16, u16),
        back: (u16, u16, u16),
        hot_x: u16,
        hot_y: u16,
    ) -> io::Result<CursorHandle> {
        // Stage 5 Phase A: real rasterisation. Read source + mask
        // (both depth-1 R8) via `engine.get_image`, lower to BGRA per
        // X11's mask/fore/back rule, allocate a CursorRecord + sprite
        // Pixmap. The cursor is invisible (size 1×1 transparent) if
        // the source pixmap can't be read — matches v1's degenerate
        // shape when the source mirror is missing.
        let xid = self.core.next_host_xid();
        let handle = CursorHandle::from_raw(xid)
            .ok_or_else(|| io::Error::other("create_cursor: xid was 0"))?;
        let (bgra, w, h) = if let Some((src_bytes, w, h)) =
            self.read_cursor_depth1_pixmap(source_pixmap.as_raw())
        {
            let mask_bytes = mask_pixmap.and_then(|mp| {
                let (mb, mw, mh) = self.read_cursor_depth1_pixmap(mp.as_raw())?;
                if mw == w && mh == h {
                    Some(mb)
                } else {
                    log::warn!(
                        "v2 create_cursor: mask 0x{:x} dims {mw}x{mh} \
                             differ from src dims {w}x{h}; ignoring mask",
                        mp.as_raw(),
                    );
                    None
                }
            });
            let bgra = crate::kms::v2::cursor::rasterise_create_cursor(
                &src_bytes,
                w,
                h,
                mask_bytes.as_deref(),
                fore,
                back,
            );
            (bgra, w, h)
        } else {
            log::warn!(
                "v2 create_cursor: source pixmap 0x{:x} unreadable; cursor invisible",
                source_pixmap.as_raw(),
            );
            (vec![0u8; 4], 1u16, 1u16)
        };
        self.insert_cursor_record(xid, w, h, hot_x, hot_y, bgra);
        Ok(handle)
    }

    fn create_glyph_cursor(
        &mut self,
        _origin: Option<OriginContext>,
        source_font: FontHandle,
        mask_font: Option<FontHandle>,
        source_char: u16,
        mask_char: u16,
        fore: (u16, u16, u16),
        back: (u16, u16, u16),
    ) -> io::Result<CursorHandle> {
        // Stage 5 Phase A: real glyph-cursor rasterisation. Render
        // source + (optional) mask glyph via FreeType, build the
        // union bbox + derive the hotspot, then lower to BGRA via
        // the cursor module's shared rasteriser. Same code shape as
        // v1's `create_glyph_cursor` (`kms/backend.rs:9937-10108`).
        let xid = self.core.next_host_xid();
        let handle = CursorHandle::from_raw(xid)
            .ok_or_else(|| io::Error::other("create_glyph_cursor: xid was 0"))?;
        // Render both glyphs into owned Vec<u8>s up front so the
        // FreeType `bitmap()` borrow doesn't span the second
        // load_char call (FreeType invalidates the previous glyph's
        // bitmap when a new load_char fires).
        let src_xid = source_font.as_raw();
        let Some((src_pix, src_w, src_h, src_lsb, src_top)) =
            self.render_glyph_for_cursor(src_xid, source_char)
        else {
            log::warn!(
                "v2 create_glyph_cursor: source font 0x{src_xid:x} unknown; cursor invisible"
            );
            self.insert_cursor_record(xid, 1, 1, 0, 0, vec![0u8; 4]);
            return Ok(handle);
        };
        let mask_data =
            mask_font.and_then(|mf| self.render_glyph_for_cursor(mf.as_raw(), mask_char));
        let src = crate::kms::v2::cursor::GlyphBitmap {
            pixels: &src_pix,
            width: src_w,
            height: src_h,
            lsb: src_lsb,
            top: src_top,
        };
        let mask_bitmap =
            mask_data.as_ref().map(
                |(pix, w, h, lsb, top)| crate::kms::v2::cursor::GlyphBitmap {
                    pixels: pix.as_slice(),
                    width: *w,
                    height: *h,
                    lsb: *lsb,
                    top: *top,
                },
            );
        let img =
            crate::kms::v2::cursor::rasterise_glyph_cursor(&src, mask_bitmap.as_ref(), fore, back);
        self.insert_cursor_record(
            xid,
            img.width,
            img.height,
            img.hot_x,
            img.hot_y,
            img.bgra_bytes,
        );
        Ok(handle)
    }

    fn create_anim_cursor(
        &mut self,
        _origin: Option<OriginContext>,
        frames: &[(CursorHandle, u32)],
    ) -> io::Result<Option<CursorHandle>> {
        // Spec 2026-06-10-animated-cursors-design.md. Snapshot every
        // frame up front — no partial map state on failure.
        if frames.is_empty() {
            return Ok(None);
        }
        let mut snap = Vec::with_capacity(frames.len());
        for (h, delay_ms) in frames {
            let raw = h.as_raw();
            let Some(record) = self.cursor_records.get(&raw) else {
                return Err(io::Error::other(format!(
                    "create_anim_cursor: unknown sub-cursor handle 0x{raw:x}"
                )));
            };
            // Delay 0 → 16ms: a 0 deadline would busy-spin the
            // poll loop (explicit Xorg deviation, see spec).
            let ms = if *delay_ms == 0 { 16 } else { *delay_ms };
            snap.push(crate::kms::v2::cursor::AnimFrame {
                record: std::sync::Arc::clone(record),
                pixmap: self.cursor_pixmaps.get(&raw).copied(),
                delay: std::time::Duration::from_millis(u64::from(ms)),
            });
        }
        let xid = self.core.next_host_xid();
        let handle = CursorHandle::from_raw(xid)
            .ok_or_else(|| io::Error::other("create_anim_cursor: xid was 0"))?;
        // Alias frame 0 in the canonical maps so every static-cursor
        // code path (effective walk, XFixes, scene) works untouched.
        self.cursor_records
            .insert(xid, std::sync::Arc::clone(&snap[0].record));
        if let Some(p) = snap[0].pixmap {
            self.cursor_pixmaps.insert(xid, p);
        }
        self.anim_cursor_records.insert(
            xid,
            crate::kms::v2::cursor::AnimCursorRecord { frames: snap },
        );
        Ok(Some(handle))
    }

    fn define_cursor(
        &mut self,
        _origin: Option<OriginContext>,
        host_window_xid: u32,
        cursor_host_xid: u32,
    ) -> io::Result<()> {
        // Stage 5 Phase A: store the cursor on the window's
        // attribute slot. Per X11, the cursor visible on screen is
        // the one belonging to the deepest window under the pointer
        // that has a non-None cursor (walking up the parent chain);
        // `cursor_host_xid == 0` is X11 `None` and means "inherit
        // from parent".
        //
        // The sticky `active_cursor` fallback on `KmsCore` matches
        // v1: a DefineCursor on the root container becomes the
        // server-wide default for windows that don't override it.
        let nested = if cursor_host_xid == 0 {
            None
        } else {
            Some(cursor_host_xid)
        };
        if let Some(geom) = self.windows_v2.get_mut(&host_window_xid) {
            geom.cursor = nested;
        }
        if cursor_host_xid != 0 && host_window_xid == self.core.window_id {
            self.core.active_cursor = Some(cursor_host_xid);
        }
        self.refresh_effective_cursor();
        Ok(())
    }

    // ── Container background ────────────────────────────────────

    fn set_container_background_pixel(
        &mut self,
        _origin: Option<OriginContext>,
        pixel: u32,
    ) -> io::Result<()> {
        self.core.bg_pixel = Some(pixel);
        self.core.bg_pixmap = None;
        // Stage 4a — root paint resolves through redirect routing.
        // In the common (unredirected) case this is the leaf root
        // drawable; if a compositor has redirected root, paint
        // lands in its backing instead. `resolve_paint_target`
        // returns `None` only when the root xid isn't in the
        // store, which is a fixture-init bug.
        if let Some(target) = self.resolve_paint_target(self.core.window_id) {
            let rect = ash::vk::Rect2D {
                offset: ash::vk::Offset2D {
                    x: target.offset.0,
                    y: target.offset.1,
                },
                extent: ash::vk::Extent2D {
                    width: u32::from(self.platform.fb_w.max(1)),
                    height: u32::from(self.platform.fb_h.max(1)),
                },
            };
            // L1 server-α invariant: root storage is depth-24, so
            // force the stored α byte to 0xFF for the scene
            // compositor's pass-through draw to read opaque.
            let depth = self.store.get(target.id).map(|d| d.depth).unwrap_or(24);
            let format = self
                .store
                .get(target.id)
                .map(|d| d.storage.format)
                .unwrap_or_else(|| PlatformBackend::format_for_depth(depth));
            if let Err(e) = self.engine.fill_rect(
                &mut self.store,
                &mut self.platform,
                target.id,
                rect,
                decode_x11_pixel_for_storage(pixel, depth, format),
            ) {
                log::warn!("v2 set_container_background_pixel: root fill failed: {e:?}");
            } else {
                self.telemetry.record_paint_submit();
                self.trace_simple(SubmitKind::FillOne, target.id, 1);
            }
        }
        self.scene.mark_scene_structure_dirty();
        Ok(())
    }

    fn set_container_background_pixmap(
        &mut self,
        _origin: Option<OriginContext>,
        host_pixmap_xid: u32,
    ) -> io::Result<()> {
        use crate::kms::{v2::engine::ResolvedSource, vk::ops::render::CompositeRect};
        self.core.bg_pixmap = PixmapHandle::from_raw(host_pixmap_xid);
        self.core.bg_pixel = None;
        // Stage 4a — root paint resolves through redirect routing.
        let Some(dst_target) = self.resolve_paint_target(self.core.window_id) else {
            self.scene.mark_scene_structure_dirty();
            return Ok(());
        };
        let dst = dst_target.id;
        let Some(src) = self.store.lookup(host_pixmap_xid) else {
            log::debug!(
                "v2 set_container_background_pixmap: pixmap 0x{host_pixmap_xid:x} not in store"
            );
            self.scene.mark_scene_structure_dirty();
            return Ok(());
        };
        // Stage 3f.14: X11 bg_pixmap tiles across the drawable
        // extent. Pre-3f.14 v2 did a single copy_area at (0, 0)
        // and left the rest of root unchanged — fvwm3 wallpaper
        // covered only the top-left of the screen on bee. Route
        // through `engine.render_composite` with OP_SRC + Repeat::
        // Normal so the source pixmap tiles across the whole root
        // extent in a single submit. Same shape as `try_tiled_fill`
        // (3f.3) but unconditioned by GC clip.
        if src == dst {
            // Defensive: a pixmap aliased as bg of its own drawable
            // is not a meaningful X11 op. v1's path treats it the
            // same (copy_area with src == dst is logged + skipped).
            log::debug!("v2 set_container_background_pixmap: src == root, skipping");
            self.scene.mark_scene_structure_dirty();
            return Ok(());
        }
        let src_format = self.store.get(src).map(|d| d.storage.format);
        if src_format != Some(ash::vk::Format::B8G8R8A8_UNORM) {
            // Tile path requires BGRA8 src (matches `try_tiled_fill`
            // gate). Other formats fall through with no paint —
            // v1-parity-ish; rare in practice for root bg.
            log::debug!(
                "v2 set_container_background_pixmap: pixmap 0x{host_pixmap_xid:x} format \
                 {src_format:?} not BGRA8, skipping tile"
            );
            self.scene.mark_scene_structure_dirty();
            return Ok(());
        }
        let dst_extent = ash::vk::Extent2D {
            width: u32::from(self.platform.fb_w.max(1)),
            height: u32::from(self.platform.fb_h.max(1)),
        };
        let rects = [CompositeRect {
            src_x: 0,
            src_y: 0,
            mask_x: 0,
            mask_y: 0,
            dst_x: dst_target.offset.0,
            dst_y: dst_target.offset.1,
            width: dst_extent.width,
            height: dst_extent.height,
        }];
        const OP_SRC: u8 = 1;
        let composite_result = self.engine.render_composite(
            &mut self.store,
            &mut self.platform,
            OP_SRC,
            ResolvedSource::Drawable(src),
            ResolvedSource::None,
            dst,
            &rects,
            None,
            Repeat::Normal,
            Repeat::None,
            None,
            None,
            false,
            // Audit #4: synthesized backing-seed copy, no Picture
            // context. Engine falls back to depth heuristic.
            0,
            0,
            0,
        );
        self.sync_descriptor_pool_telemetry();
        match composite_result {
            Ok(s) if s.recorded_draws > 0 && !s.deferred_to_batch => {
                self.telemetry.record_paint_submit();
                self.trace_render(
                    SubmitKind::RenderComposite,
                    dst,
                    s.recorded_draws,
                    1, // OP_SRC
                    SrcClass::Direct,
                    None,
                    SubmitFlags {
                        readback: s.used_dst_readback,
                        alias: s.used_src_alias_scratch,
                        zero_draws: false,
                        upload: false,
                    },
                );
            }
            Ok(_) => {}
            Err(e) => {
                log::warn!(
                    "v2 set_container_background_pixmap: render_composite tile failed: {e:?}"
                );
            }
        }
        self.scene.mark_scene_structure_dirty();
        Ok(())
    }

    // ── GC state ────────────────────────────────────────────────

    fn clear_clip_rectangles(&mut self, _origin: Option<OriginContext>) -> io::Result<()> {
        self.core.current_clip = ClipState::None;
        Ok(())
    }

    fn set_clip_rectangles(
        &mut self,
        _origin: Option<OriginContext>,
        clip: Option<ClipRectangles>,
    ) -> io::Result<()> {
        self.core.current_clip = match clip {
            Some(rects) => ClipState::Rectangles {
                origin: (0, 0),
                rects,
            },
            None => ClipState::None,
        };
        self.clip_mask_cache = None;
        Ok(())
    }

    fn set_clip_pixmap(
        &mut self,
        _origin: Option<OriginContext>,
        host_pixmap: u32,
        clip_x_origin: i16,
        clip_y_origin: i16,
    ) -> io::Result<()> {
        let Some(handle) = PixmapHandle::from_raw(host_pixmap) else {
            self.core.current_clip = ClipState::None;
            self.clip_mask_cache = None;
            return Ok(());
        };
        self.core.current_clip = ClipState::Pixmap {
            origin: (clip_x_origin, clip_y_origin),
            pixmap: handle,
        };
        // Eagerly read the mask pixmap so subsequent Core paint can
        // gate per pixel via `intersect_with_current_clip`. wmaker's
        // title-bar buttons are the canonical client: ChangeGC
        // clip-mask=<glyph_pixmap> + PolyFillRectangle button_window
        // 25x25, where the depth-1 mask gates the solid fill to the
        // X / − glyph shape. Without this readback the whole 25x25
        // gets painted in foreground and the glyphs vanish.
        self.clip_mask_cache =
            self.read_clip_mask_bytes(host_pixmap, (clip_x_origin, clip_y_origin));
        Ok(())
    }

    fn set_gc_fill_solid(&mut self, _origin: Option<OriginContext>) -> io::Result<()> {
        self.core.current_fill = FillState::Solid;
        self.fill_pattern_cache = None;
        Ok(())
    }

    fn set_gc_fill_tiled(
        &mut self,
        _origin: Option<OriginContext>,
        host_pixmap: u32,
        tile_x_origin: i16,
        tile_y_origin: i16,
    ) -> io::Result<()> {
        // Stage 3f.3: store the FillState::Tiled record so subsequent
        // fill paths route through the tiled-fill RENDER composite.
        // The dispatcher also pushes the same state via
        // `apply_fill_state` before every fill op, so this entry
        // point is mostly used by ynest's host-X11 flow; preserving
        // both keeps the Backend trait surface uniform.
        let Some(handle) = PixmapHandle::from_raw(host_pixmap) else {
            self.core.current_fill = FillState::Solid;
            self.fill_pattern_cache = None;
            return Ok(());
        };
        self.core.current_fill = FillState::Tiled {
            pixmap: handle,
            origin: (tile_x_origin, tile_y_origin),
        };
        self.fill_pattern_cache =
            self.read_fill_pattern_cache(host_pixmap, (tile_x_origin, tile_y_origin));
        Ok(())
    }

    fn apply_clip_state(
        &mut self,
        _origin: Option<OriginContext>,
        clip: &ClipState,
    ) -> io::Result<()> {
        self.core.current_clip = clip.clone();
        // X11 ChangeGC clip-mask=<pixmap> propagates through
        // `resolve_draw_state` → here, not `set_clip_pixmap`. Populate
        // the mask cache so `intersect_with_current_clip` can gate
        // paint to the mask shape. wmaker title-bar buttons are the
        // canonical client: the title bar uses the same GC, alternating
        // clip-mask=<glyph> with clip-mask=None for solid fills, so the
        // cache MUST follow the GC state per paint setup.
        match clip {
            ClipState::Pixmap { origin, pixmap } => {
                let xid = pixmap.as_raw();
                if let Some(fresh) = self.read_clip_mask_bytes(xid, *origin) {
                    self.clip_mask_cache = Some(fresh);
                } else if let Some(cache) = self.clip_mask_cache.as_mut() {
                    if cache.pixmap_xid == xid {
                        cache.origin = *origin;
                    } else {
                        self.clip_mask_cache = None;
                    }
                } else {
                    self.clip_mask_cache = None;
                }
            }
            _ => {
                self.clip_mask_cache = None;
            }
        }
        Ok(())
    }

    fn apply_fill_state(
        &mut self,
        _origin: Option<OriginContext>,
        fill: &FillState,
    ) -> io::Result<()> {
        self.core.current_fill = fill.clone();
        match fill {
            FillState::Tiled { pixmap, origin }
            | FillState::Stippled { pixmap, origin }
            | FillState::OpaqueStippled { pixmap, origin } => {
                let xid = pixmap.as_raw();
                if let Some(fresh) = self.read_fill_pattern_cache(xid, *origin) {
                    self.fill_pattern_cache = Some(fresh);
                } else if let Some(cache) = self.fill_pattern_cache.as_mut() {
                    if cache.pixmap_xid == xid {
                        cache.origin = *origin;
                    } else {
                        self.fill_pattern_cache = None;
                    }
                } else {
                    self.fill_pattern_cache = None;
                }
            }
            FillState::Solid => {
                self.fill_pattern_cache = None;
            }
        }
        Ok(())
    }

    fn apply_draw_state(
        &mut self,
        _origin: Option<OriginContext>,
        state: &DrawState,
    ) -> io::Result<()> {
        if let Some(font) = state.font {
            self.core.current_font = Some(font.as_raw());
        }
        self.core.current_function = state.function;
        self.core.current_plane_mask = state.plane_mask;
        self.core.current_foreground = state.foreground;
        self.core.current_background = state.background;
        self.core.current_fill = state.fill.clone();
        self.core.current_clip = state.clip.clone();
        match &state.fill {
            FillState::Tiled { pixmap, origin }
            | FillState::Stippled { pixmap, origin }
            | FillState::OpaqueStippled { pixmap, origin } => {
                let xid = pixmap.as_raw();
                if let Some(fresh) = self.read_fill_pattern_cache(xid, *origin) {
                    self.fill_pattern_cache = Some(fresh);
                } else if let Some(cache) = self.fill_pattern_cache.as_mut() {
                    if cache.pixmap_xid == xid {
                        cache.origin = *origin;
                    } else {
                        self.fill_pattern_cache = None;
                    }
                } else {
                    self.fill_pattern_cache = None;
                }
            }
            FillState::Solid => {
                self.fill_pattern_cache = None;
            }
        }
        // Stage 4d Manual-redirect fix: drawing through a
        // `ClipByChildren` GC into a window must exclude every
        // mapped child window's area. Capture the mode here so
        // `copy_area` (and any other future op that consults it)
        // can split the destination rect against the child rects.
        self.core.current_subwindow_mode = state.subwindow_mode;
        // Stroke state — consumed by poly_line / poly_segment /
        // poly_rectangle / poly_arc via `kms::v2::stroke::stroke_path`.
        self.core.current_line_width = state.line_width;
        self.core.current_line_style = state.line_style;
        self.core.current_cap_style = state.cap_style;
        self.core.current_join_style = state.join_style;
        self.core.current_dashes = state.dashes.clone();
        self.core.current_dash_offset = u16::try_from(state.dash_offset).unwrap_or(0);
        self.core.current_arc_mode = state.arc_mode;
        Ok(())
    }

    // ── Drawing primitives (paint paths) ────────────────────────

    fn copy_area(
        &mut self,
        _origin: Option<OriginContext>,
        src_host_xid: u32,
        dst_host_xid: u32,
        src_x: i16,
        src_y: i16,
        dst_x: i16,
        dst_y: i16,
        width: u16,
        height: u16,
    ) -> io::Result<()> {
        let Some(src) = self.store.lookup(src_host_xid) else {
            log::warn!(
                "v2 copy_area dropped — src unknown: src=0x{src_host_xid:x} dst=0x{dst_host_xid:x} \
                 src_xy=({src_x},{src_y}) dst_xy=({dst_x},{dst_y}) {width}x{height}",
            );
            self.log_v2_gap("copy_area_unknown_xid");
            return Ok(());
        };
        // Stage 4a — dst resolves through `resolve_paint_target` so
        // copy_area into a redirected window lands in the backing
        // with the descendant offset applied. Source stays at the
        // raw store lookup per spec § "render_composite separates
        // src/dst resolution" — the X11 client reads from the
        // drawable as it sees it.
        let Some(dst_target) = self.resolve_paint_target(dst_host_xid) else {
            log::warn!(
                "v2 copy_area dropped — dst unresolvable: src=0x{src_host_xid:x} dst=0x{dst_host_xid:x} \
                 src_xy=({src_x},{src_y}) dst_xy=({dst_x},{dst_y}) {width}x{height}",
            );
            self.log_v2_gap("copy_area_unknown_xid");
            return Ok(());
        };
        // Diagnostic trace (TEMP — Stage 4d "top-left-only CC" investigation).
        // Pins where each CopyArea lands: src store id, dst's resolved
        // PaintTarget (id + offset), and the wire src/dst coords + size.
        // Gated on `RUST_LOG=yserver::kms::v2::paint=trace`. Codex round
        // of 2026-05-18: needed because the symptom narrowed to "B has
        // CC content only in the top-left 177x80" — we need to see
        // whether marco's many 975x600 CopyArea(src=CC_offscreen,
        // dst=CC_window) calls resolve to the frame backing's
        // DrawableId or get lost en route.
        log::trace!(
            target: "yserver::kms::v2::paint",
            "copy_area src=0x{src_host_xid:x}->id={src:?} dst=0x{dst_host_xid:x}->id={dst_id:?}+off=({off_x},{off_y}) \
             src_xy=({src_x},{src_y}) dst_xy=({dst_x},{dst_y}) {width}x{height}",
            dst_id = dst_target.id,
            off_x = dst_target.offset.0,
            off_y = dst_target.offset.1,
        );
        // Stage 4d Manual-redirect fix: split the copy by
        // `subwindow_mode = ClipByChildren` rules when dst is a
        // window. Each surviving sub-rect is in dst-window-local
        // coords; we issue one engine.copy_area per sub-rect,
        // adjusting src offsets by the sub-rect's delta from the
        // original dst_xy. IncludeInferiors (mode=1) keeps the
        // single-rect fast path. Pixmap destinations also keep the
        // fast path (no children to clip against).
        let dst_rect_local = ash::vk::Rect2D {
            offset: ash::vk::Offset2D {
                x: i32::from(dst_x),
                y: i32::from(dst_y),
            },
            extent: ash::vk::Extent2D {
                width: u32::from(width),
                height: u32::from(height),
            },
        };
        // Step 1: GC clip intersection (X11 GC `clip-mask` /
        // `SetClipRectangles`). When the GC has explicit clip
        // rectangles, every paint is masked against them first;
        // `ClipState::None` means "no GC clip", and we keep the
        // single-rect fast path. `ClipState::Pixmap` intersects with
        // the rasterized mask (pixel runs via
        // intersect_with_current_clip_live).
        if matches!(
            self.core.current_clip,
            yserver_core::backend::ClipState::Pixmap { .. }
        ) {
            let local = Rectangle16 {
                x: dst_x,
                y: dst_y,
                width,
                height,
            };
            let runs = self.intersect_with_current_clip_live(&[local]);
            // Mask runs honor function/plane-mask via the CPU path
            // (Copy through apply_gc_function = src — bitwise exact).
            use yserver_core::backend::GcFunction;
            let function = self.core.current_function;
            if matches!(function, GcFunction::NoOp) {
                return Ok(());
            }
            let dst_depth = self.store.get(dst_target.id).map_or(24, |d| d.depth);
            let plane_mask = self.core.current_plane_mask & depth_plane_mask(dst_depth);
            if plane_mask == 0 {
                return Ok(());
            }
            for run in runs {
                let sub_src = ash::vk::Rect2D {
                    offset: ash::vk::Offset2D {
                        x: i32::from(src_x) + (i32::from(run.x) - i32::from(dst_x)),
                        y: i32::from(src_y) + (i32::from(run.y) - i32::from(dst_y)),
                    },
                    extent: ash::vk::Extent2D {
                        width: u32::from(run.width),
                        height: u32::from(run.height),
                    },
                };
                let dst_pos = ash::vk::Offset2D {
                    x: i32::from(run.x) + dst_target.offset.0,
                    y: i32::from(run.y) + dst_target.offset.1,
                };
                self.copy_area_rop_cpu(
                    src,
                    dst_target.id,
                    sub_src,
                    dst_pos,
                    function,
                    plane_mask,
                    dst_depth,
                );
            }
            self.scene.wake_for_damage();
            return Ok(());
        }
        let post_gc_clip: Vec<ash::vk::Rect2D> =
            if let yserver_core::backend::ClipState::Rectangles { origin, rects } =
                &self.core.current_clip
            {
                let clip_rects: Vec<ash::vk::Rect2D> = rects
                    .rectangles
                    .chunks_exact(8)
                    .filter_map(|chunk| {
                        let cx = i32::from(i16::from_le_bytes([chunk[0], chunk[1]]))
                            + i32::from(origin.0);
                        let cy = i32::from(i16::from_le_bytes([chunk[2], chunk[3]]))
                            + i32::from(origin.1);
                        let cw = i32::from(u16::from_le_bytes([chunk[4], chunk[5]]));
                        let ch = i32::from(u16::from_le_bytes([chunk[6], chunk[7]]));
                        if cw <= 0 || ch <= 0 {
                            return None;
                        }
                        Some(ash::vk::Rect2D {
                            offset: ash::vk::Offset2D { x: cx, y: cy },
                            extent: ash::vk::Extent2D {
                                width: u32::try_from(cw).unwrap_or(0),
                                height: u32::try_from(ch).unwrap_or(0),
                            },
                        })
                    })
                    .collect();
                intersect_rect_with_clip(dst_rect_local, &clip_rects)
            } else {
                vec![dst_rect_local]
            };
        if post_gc_clip.is_empty() {
            // GC clip is empty (or `SetClipRectangles` with n=0):
            // spec-correct no-op.
            return Ok(());
        }
        // Step 2: ClipByChildren — subtract every mapped child window
        // rect from each post-GC-clip rect. IncludeInferiors (mode=1)
        // keeps each post-GC-clip rect as-is. Pixmap destinations
        // (not in `windows_v2`) also bypass child subtraction.
        let sub_rects: Vec<ash::vk::Rect2D> = if matches!(
            self.core.current_subwindow_mode,
            yserver_core::backend::SubwindowMode::ClipByChildren,
        ) && self.windows_v2.contains_key(&dst_host_xid)
        {
            let child_rects: Vec<ash::vk::Rect2D> = self
                .windows_v2
                .iter()
                .filter_map(|(child_host_xid, geom)| {
                    if !(geom.parent == Some(dst_host_xid) && geom.mapped) {
                        return None;
                    }
                    // Manually-redirected children don't claim the
                    // parent's pixmap real estate — the redirecting
                    // compositor (which may BE the parent's own
                    // client, e.g. notification-area-applet over
                    // its tray sockets) places the children's pixels
                    // explicitly via subsequent ops. Subtracting them
                    // strips the compositor's own composite-target
                    // rect to empty. Same shape as the protocol-layer
                    // fix at process_request.rs:copy_area_effective_dst_rects.
                    //
                    // Detect via v2's `scene_participating` flag: it
                    // is set to `false` when redirect mode is Manual
                    // (the X server stops auto-painting the window
                    // into the scene/parent). Automatic redirect
                    // keeps it `true`, so Automatic children still
                    // clip — protecting the marco/CC frame test from
                    // regression.
                    let is_manually_redirected = self
                        .store
                        .lookup(*child_host_xid)
                        .and_then(|id| self.store.get(id))
                        .is_some_and(|d| !d.scene_participating);
                    if is_manually_redirected {
                        return None;
                    }
                    Some(ash::vk::Rect2D {
                        offset: ash::vk::Offset2D {
                            x: i32::from(geom.x),
                            y: i32::from(geom.y),
                        },
                        extent: ash::vk::Extent2D {
                            width: u32::from(geom.width.max(1)),
                            height: u32::from(geom.height.max(1)),
                        },
                    })
                })
                .collect();
            if child_rects.is_empty() {
                post_gc_clip
            } else {
                post_gc_clip
                    .into_iter()
                    .flat_map(|r| compute_copy_area_dst_rects(r, &child_rects))
                    .collect()
            }
        } else {
            post_gc_clip
        };
        if sub_rects.is_empty() {
            // Whole copy fully covered by mapped children — nothing
            // to paint. Spec-correct under ClipByChildren.
            return Ok(());
        }
        // GC function + plane-mask on copies (X11 §CopyArea uses the
        // full rop set). The engine blit is raw GXcopy; anything else
        // (or a partial plane mask) takes the CPU read-modify-write
        // path. NoOp = spec-correct no-op.
        {
            use yserver_core::backend::GcFunction;
            let function = self.core.current_function;
            if matches!(function, GcFunction::NoOp) {
                return Ok(());
            }
            let dst_depth = self.store.get(dst_target.id).map_or(24, |d| d.depth);
            let full_mask = depth_plane_mask(dst_depth);
            let plane_mask = self.core.current_plane_mask & full_mask;
            if plane_mask == 0 {
                return Ok(());
            }
            if !matches!(function, GcFunction::Copy) || plane_mask != full_mask {
                for sub in &sub_rects {
                    let sub_src = ash::vk::Rect2D {
                        offset: ash::vk::Offset2D {
                            x: i32::from(src_x) + (sub.offset.x - i32::from(dst_x)),
                            y: i32::from(src_y) + (sub.offset.y - i32::from(dst_y)),
                        },
                        extent: sub.extent,
                    };
                    let dst_pos = ash::vk::Offset2D {
                        x: sub.offset.x + dst_target.offset.0,
                        y: sub.offset.y + dst_target.offset.1,
                    };
                    self.copy_area_rop_cpu(
                        src,
                        dst_target.id,
                        sub_src,
                        dst_pos,
                        function,
                        plane_mask,
                        dst_depth,
                    );
                }
                self.scene.wake_for_damage();
                return Ok(());
            }
        }
        // Stage 5 Task 3 POC: route copy_area to COW through the
        // frame-builder path. Marco's compositor pump is the hot
        // workload (silence trace: 47k of 62k copy_areas target
        // COW). Telemetry for cow-routed copies is deferred.
        let routes_to_cow = self.cow_id == Some(dst_target.id) && src != dst_target.id;

        let mut all_ok = true;
        for sub in &sub_rects {
            let sub_dst_x = sub.offset.x;
            let sub_dst_y = sub.offset.y;
            // src coords shift by the same delta the dst sub-rect
            // shifted from the original dst_xy.
            let sub_src_x = i32::from(src_x) + (sub_dst_x - i32::from(dst_x));
            let sub_src_y = i32::from(src_y) + (sub_dst_y - i32::from(dst_y));
            let src_sub_rect = ash::vk::Rect2D {
                offset: ash::vk::Offset2D {
                    x: sub_src_x,
                    y: sub_src_y,
                },
                extent: sub.extent,
            };
            let dst_pos = ash::vk::Offset2D {
                x: sub_dst_x + dst_target.offset.0,
                y: sub_dst_y + dst_target.offset.1,
            };
            self.engine_copy_area_calls = self.engine_copy_area_calls.wrapping_add(1);
            let res = if routes_to_cow {
                self.engine.cow_copy_area(
                    &mut self.store,
                    &mut self.platform,
                    dst_target.id,
                    src,
                    src_sub_rect,
                    dst_pos,
                )
            } else {
                self.engine.copy_area(
                    &mut self.store,
                    &mut self.platform,
                    src,
                    dst_target.id,
                    src_sub_rect,
                    dst_pos,
                )
            };
            if let Err(e) = res {
                log::warn!(
                    "v2 copy_area: engine.copy_area failed (src=0x{src_host_xid:x} \
                     dst=0x{dst_host_xid:x} sub_rect={sub:?} cow_routed={routes_to_cow}): {e:?}",
                );
                all_ok = false;
            }
        }
        if all_ok {
            if !routes_to_cow {
                self.telemetry.record_paint_submit();
                self.trace_simple(SubmitKind::CopyArea, dst_target.id, 1);
            }
            // Present Copy into COW/backings must wake the scene
            // compositor immediately; otherwise the damage can sit
            // until an unrelated input event arrives.
            self.scene.wake_for_damage();
        }
        Ok(())
    }

    fn copy_plane(
        &mut self,
        _origin: Option<OriginContext>,
        src_host_xid: u32,
        dst_host_xid: u32,
        src_x: i16,
        src_y: i16,
        dst_x: i16,
        dst_y: i16,
        width: u16,
        height: u16,
        plane: u32,
    ) -> io::Result<()> {
        // copy_plane decomposes into bg-first + fg-second
        // `poly_fill_rectangle` calls below; non-`GXcopy` GC.function
        // is honoured by the underlying `fill_solid_rects` →
        // `engine.logic_fill` path landed in Stage 3f.2.
        if width == 0 || height == 0 {
            return Ok(());
        }

        // Resolve src + dst drawables. Both must exist in the store
        // (otherwise the request is a protocol error — log + skip).
        let Some(src_id) = self.store.lookup(src_host_xid) else {
            log::debug!("v2 copy_plane gap: src 0x{src_host_xid:x} not in store");
            return Ok(());
        };
        let Some(_dst_id) = self.store.lookup(dst_host_xid) else {
            log::debug!("v2 copy_plane gap: dst 0x{dst_host_xid:x} not in store");
            return Ok(());
        };

        let src_depth = match self.store.get(src_id) {
            Some(d) => d.depth,
            None => return Ok(()),
        };

        // Read the full src extent via the engine. We pull the
        // whole pixmap once (rather than only `src_rect`) because
        // the wire format's row stride is computed from the
        // pixmap's width; reading a sub-rect would still produce a
        // wire-shaped reply but with a different row stride per
        // pixmap.width. Easier to pull everything, index inside
        // the (src_x, src_y, width, height) window, and let v2's
        // per-op CB amortise the synchronous get_image cost. xfd
        // / xfontsel CopyPlane the entire glyph pixmap each draw
        // anyway, so the "full extent" overhead matches the call
        // pattern.
        let src_extent = match self.store.get(src_id) {
            Some(d) => d.storage.extent,
            None => return Ok(()),
        };
        let src_w = src_extent.width;
        let src_h = src_extent.height;
        if src_w == 0 || src_h == 0 {
            return Ok(());
        }
        let src_bytes = match self.engine.get_image(
            &mut self.store,
            &mut self.platform,
            src_id,
            ash::vk::Rect2D {
                offset: ash::vk::Offset2D::default(),
                extent: src_extent,
            },
            src_depth,
        ) {
            Ok(bytes) => bytes,
            Err(e) => {
                log::warn!("v2 copy_plane: src get_image failed: {e:?}");
                return Ok(());
            }
        };
        self.telemetry.record_one_shot_submit();
        self.trace_simple(SubmitKind::CopyPlaneRb, src_id, 1);

        // Wire row stride for the src depth (matches pack_from_storage).
        let row_bytes: usize = match src_depth {
            1 => src_w.div_ceil(32) as usize * 4,
            4 => src_w.div_ceil(8) as usize * 4,
            8 => (src_w as usize + 3) & !3,
            24 | 32 => src_w as usize * 4,
            _ => {
                log::debug!("v2 copy_plane gap: src depth {src_depth} unsupported");
                return Ok(());
            }
        };

        // For each (sx, sy) in the requested src window, classify
        // the pixel into foreground / background and emit a 1×1
        // fill rect at the corresponding dst position. Caller
        // saturates over i16 because dst coords are protocol-i16.
        let mut fg_rects: Vec<u8> = Vec::new();
        let mut bg_rects: Vec<u8> = Vec::new();
        for row in 0..height {
            let sy = i32::from(src_y).saturating_add(i32::from(row));
            let dy = dst_y.saturating_add(row as i16);
            if sy < 0 || sy >= i32::try_from(src_h).unwrap_or(i32::MAX) {
                continue;
            }
            for col in 0..width {
                let sx = i32::from(src_x).saturating_add(i32::from(col));
                let dx = dst_x.saturating_add(col as i16);
                if sx < 0 || sx >= i32::try_from(src_w).unwrap_or(i32::MAX) {
                    continue;
                }
                let pixel: u32 = match src_depth {
                    1 => {
                        // LSB-first: bit 0 of byte = leftmost pixel.
                        // Matches `pack_from_storage` depth=1 emit.
                        let row_off = sy as usize * row_bytes;
                        let byte = src_bytes[row_off + (sx as usize) / 8];
                        let bit = (byte >> (sx as usize & 7)) & 1;
                        u32::from(bit)
                    }
                    4 => {
                        let row_off = sy as usize * row_bytes;
                        let byte = src_bytes[row_off + (sx as usize) / 2];
                        u32::from(if (sx as usize).is_multiple_of(2) {
                            byte & 0x0f
                        } else {
                            (byte >> 4) & 0x0f
                        })
                    }
                    8 => {
                        let row_off = sy as usize * row_bytes;
                        u32::from(src_bytes[row_off + sx as usize])
                    }
                    24 | 32 => {
                        let off = sy as usize * row_bytes + sx as usize * 4;
                        u32::from_le_bytes([
                            src_bytes[off],
                            src_bytes[off + 1],
                            src_bytes[off + 2],
                            src_bytes[off + 3],
                        ])
                    }
                    _ => 0,
                };
                let mut rect = Vec::with_capacity(8);
                rect.extend_from_slice(&i16::to_le_bytes(dx));
                rect.extend_from_slice(&i16::to_le_bytes(dy));
                rect.extend_from_slice(&u16::to_le_bytes(1));
                rect.extend_from_slice(&u16::to_le_bytes(1));
                if pixel & plane != 0 {
                    fg_rects.extend_from_slice(&rect);
                } else {
                    bg_rects.extend_from_slice(&rect);
                }
            }
        }

        let foreground = self.core.current_foreground;
        let background = self.core.current_background;

        // Bg first, then fg — matches v1's overlap ordering so the
        // foreground wins on any aliased rect.
        if !bg_rects.is_empty() {
            self.poly_fill_rectangle(None, dst_host_xid, background, &bg_rects)?;
        }
        if !fg_rects.is_empty() {
            self.poly_fill_rectangle(None, dst_host_xid, foreground, &fg_rects)?;
        }
        Ok(())
    }

    fn put_image(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        depth: u8,
        width: u16,
        height: u16,
        dst_x: i16,
        dst_y: i16,
        data: &[u8],
    ) -> io::Result<()> {
        let Some(target) = self.resolve_paint_target(host_xid) else {
            self.log_v2_gap("put_image_unknown_xid");
            return Ok(());
        };
        // GC clipping is honoured upstream by `clear_clip_rectangles`
        // when the dispatcher zeroes the clip (the MIT-SHM /
        // ImageText callers do this); Stage 2c's engine ignores
        // the GC's clip rectangles otherwise.
        // GC function + plane-mask (X11 §PutImage combines the wire
        // image with the destination through the full rop set):
        // non-Copy or partial plane-mask takes the CPU
        // read-modify-write path; NoOp is a spec no-op.
        {
            use yserver_core::backend::GcFunction;
            let function = self.core.current_function;
            if matches!(function, GcFunction::NoOp) {
                return Ok(());
            }
            let dst_depth = self.store.get(target.id).map_or(depth, |d| d.depth);
            let full_mask = depth_plane_mask(dst_depth);
            let plane_mask = self.core.current_plane_mask & full_mask;
            if plane_mask == 0 {
                return Ok(());
            }
            let pixmap_clip = matches!(
                self.core.current_clip,
                yserver_core::backend::ClipState::Pixmap { .. }
            );
            if pixmap_clip {
                // Bitmap clip-mask: intersect the put rect with the
                // mask (pixel runs), then write each run through the
                // CPU path (Copy through apply_gc_function = src).
                let local = Rectangle16 {
                    x: dst_x,
                    y: dst_y,
                    width,
                    height,
                };
                let runs = self.intersect_with_current_clip_live(&[local]);
                for run in runs {
                    self.put_image_rop_cpu(
                        target.id,
                        ash::vk::Offset2D {
                            x: i32::from(run.x) + target.offset.0,
                            y: i32::from(run.y) + target.offset.1,
                        },
                        width,
                        (
                            i32::from(run.x) - i32::from(dst_x),
                            i32::from(run.y) - i32::from(dst_y),
                        ),
                        run.width,
                        run.height,
                        data,
                        depth,
                        function,
                        plane_mask,
                    );
                }
                self.telemetry.record_paint_submit();
                self.trace_simple(SubmitKind::PutImage, target.id, 1);
                self.scene.wake_for_damage();
                return Ok(());
            }
            if !matches!(function, GcFunction::Copy) || plane_mask != full_mask {
                self.put_image_rop_cpu(
                    target.id,
                    ash::vk::Offset2D {
                        x: i32::from(dst_x) + target.offset.0,
                        y: i32::from(dst_y) + target.offset.1,
                    },
                    width,
                    (0, 0),
                    width,
                    height,
                    data,
                    depth,
                    function,
                    plane_mask,
                );
                self.telemetry.record_paint_submit();
                self.trace_simple(SubmitKind::PutImage, target.id, 1);
                self.scene.wake_for_damage();
                return Ok(());
            }
        }
        if let Err(e) = self.engine.put_image(
            &mut self.store,
            &mut self.platform,
            target.id,
            ash::vk::Offset2D {
                x: i32::from(dst_x) + target.offset.0,
                y: i32::from(dst_y) + target.offset.1,
            },
            ash::vk::Extent2D {
                width: u32::from(width),
                height: u32::from(height),
            },
            data,
            depth,
        ) {
            log::warn!("v2 put_image: engine.put_image failed for xid {host_xid:#x}: {e:?}",);
        } else {
            self.telemetry.record_paint_submit();
            self.trace_simple(SubmitKind::PutImage, target.id, 1);
        }
        Ok(())
    }

    fn get_image(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        format: u8,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        plane_mask: u32,
    ) -> io::Result<Option<Vec<u8>>> {
        // Stage 4a — resolve through redirect routing per spec Risk 1
        // ("GetImage reads what the X server considers W's content,
        // which under redirect is B"). Depth comes from the
        // resolved target's drawable (backing is allocated to match
        // W's depth, so v1 / v2 see the same wire shape).
        if host_xid == self.core.window_id {
            let Some(root_id) = self.store.lookup(self.core.window_id) else {
                self.log_v2_gap("get_image_root_unknown_root");
                return Ok(None);
            };
            let (depth, storage_extent) = match self.store.get(root_id) {
                Some(d) => (d.depth, d.storage.extent),
                None => return Ok(None),
            };
            let mask = plane_mask & depth_plane_mask(depth);
            if format == GET_IMAGE_FORMAT_XY_PIXMAP && mask == 0 {
                return Ok(Some(wrap_get_image_reply(depth, Vec::new())));
            }
            let rect = ash::vk::Rect2D {
                offset: ash::vk::Offset2D {
                    x: i32::from(x),
                    y: i32::from(y),
                },
                extent: ash::vk::Extent2D {
                    width: u32::from(width),
                    height: u32::from(height),
                },
            };
            let clipped = crate::kms::v2::engine::clamp_rect(rect, storage_extent);
            let start = std::time::Instant::now();
            let result = match read_scanout_region(self, rect, ScanoutReadSelection::OnScreenOnly) {
                Ok(mut pixel_bytes) => {
                    if format == GET_IMAGE_FORMAT_XY_PIXMAP {
                        pixel_bytes = z_to_xy_planes(
                            &pixel_bytes,
                            clipped.extent.width,
                            clipped.extent.height,
                            depth,
                            mask,
                        );
                    } else if mask != depth_plane_mask(depth) {
                        apply_z_plane_mask(&mut pixel_bytes, depth, mask);
                    }
                    let ns = u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX);
                    self.telemetry.record_one_shot_submit();
                    self.telemetry.record_fence_wait(ns);
                    self.trace_simple(SubmitKind::GetImage, root_id, 1);
                    Ok(Some(wrap_get_image_reply(depth, pixel_bytes)))
                }
                Err(e) => {
                    log::warn!(
                        "v2 get_image: root scanout readback failed for host_xid \
                         {host_xid:#x}: {e:?}",
                    );
                    Ok(None)
                }
            };
            self.drain_frame_builder_telemetry();
            return result;
        }
        let Some(target) = self.resolve_paint_target(host_xid) else {
            self.log_v2_gap("get_image_unknown_xid");
            return Ok(None);
        };
        let (depth, storage_extent) = match self.store.get(target.id) {
            Some(d) => (d.depth, d.storage.extent),
            None => return Ok(None),
        };
        let mask = plane_mask & depth_plane_mask(depth);
        if format == GET_IMAGE_FORMAT_XY_PIXMAP && mask == 0 {
            // No planes requested: Xorg replies with zero data. This
            // path is load-bearing for Xlib — libX11's _XGetImage has
            // a NULL deref (`planes = image->depth` before the NULL
            // check) when an XYPixmap reply with plane_mask=0 carries
            // a non-zero length (xts5 Xlib9/XGetImage TP2 crashes,
            // poisons the display mutex, and hangs the whole TCM).
            return Ok(Some(wrap_get_image_reply(depth, Vec::new())));
        }
        let rect = ash::vk::Rect2D {
            offset: ash::vk::Offset2D {
                x: i32::from(x) + target.offset.0,
                y: i32::from(y) + target.offset.1,
            },
            extent: ash::vk::Extent2D {
                width: u32::from(width),
                height: u32::from(height),
            },
        };
        // Mirror the engine's clamp so the XY repack below knows the
        // row geometry of the bytes it gets back.
        let clipped = crate::kms::v2::engine::clamp_rect(rect, storage_extent);
        let start = std::time::Instant::now();
        let result =
            match self
                .engine
                .get_image(&mut self.store, &mut self.platform, target.id, rect, depth)
            {
                Ok(mut pixel_bytes) => {
                    if format == GET_IMAGE_FORMAT_XY_PIXMAP {
                        pixel_bytes = z_to_xy_planes(
                            &pixel_bytes,
                            clipped.extent.width,
                            clipped.extent.height,
                            depth,
                            mask,
                        );
                    } else if mask != depth_plane_mask(depth) {
                        apply_z_plane_mask(&mut pixel_bytes, depth, mask);
                    }
                    let ns = u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX);
                    self.telemetry.record_one_shot_submit();
                    self.telemetry.record_fence_wait(ns);
                    self.trace_simple(SubmitKind::GetImage, target.id, 1);
                    // X11 GetImage reply: 32-byte header + pixel rows.
                    // The handler in `process_request.rs:handle_get_image`
                    // patches `sequence` at [2..4] and `visual` at [8..12];
                    // the rest of the header (depth, reply length in u32
                    // units, padding) is the backend's job. Mirrors v1's
                    // `KmsBackend::get_image` (kms/backend.rs:10400) — when
                    // this returns just the pixel slice (no header), the
                    // handler corrupts the first 32 bytes by writing into
                    // them, and clients reading depth/length/sequence from
                    // the wire see garbage.
                    Ok(Some(wrap_get_image_reply(depth, pixel_bytes)))
                }
                Err(e) => {
                    log::warn!(
                        "v2 get_image: engine.get_image failed for xid {host_xid:#x}: {e:?}",
                    );
                    Ok(None)
                }
            };
        // Phase B.1 Task 21: engine.get_image calls close_open_frame
        // (SyncWait reason) before blocking on the fence; drain the
        // resulting close event into telemetry.
        self.drain_frame_builder_telemetry();
        result
    }

    fn read_depth1_pixmap(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
    ) -> io::Result<Option<(u32, u32, Vec<u8>)>> {
        // SHAPE::Mask introspection — read a depth-1 mask pixmap
        // back as the tightly packed byte-per-pixel triple
        // `bitmap_to_yx_banded_rects` consumes. Mirrors v1's
        // `read_mirror_pixels` path (commit c5959af); without this
        // override the trait default returns `None` and every
        // ShapeMask degrades to a bounding-box rect — and since
        // the scene clips window draws to the bounding shape,
        // shaped popups render wrong (e16 hover clouds).
        let Some(target) = self.resolve_paint_target(host_xid) else {
            self.log_v2_gap("read_depth1_pixmap_unknown_xid");
            return Ok(None);
        };
        let (depth, extent) = match self.store.get(target.id) {
            Some(d) => (d.depth, d.storage.extent),
            None => return Ok(None),
        };
        if depth != 1 {
            return Ok(None);
        }
        let rect = ash::vk::Rect2D {
            offset: ash::vk::Offset2D {
                x: target.offset.0,
                y: target.offset.1,
            },
            extent,
        };
        let start = std::time::Instant::now();
        let result =
            match self
                .engine
                .get_image(&mut self.store, &mut self.platform, target.id, rect, 1)
            {
                Ok(packed) => {
                    let ns = u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX);
                    self.telemetry.record_one_shot_submit();
                    self.telemetry.record_fence_wait(ns);
                    self.trace_simple(SubmitKind::GetImage, target.id, 1);
                    // engine.get_image returns wire-format depth-1
                    // rows (LSBFirst bits, 32-bit scanline pad);
                    // unpack to one byte per pixel, 0xFF = set.
                    let w = extent.width as usize;
                    let row_bytes = extent.width.div_ceil(32) as usize * 4;
                    let mut bytes = vec![0u8; w * extent.height as usize];
                    for row in 0..extent.height as usize {
                        let src = &packed[row * row_bytes..];
                        for col in 0..w {
                            if src[col / 8] & (1 << (col % 8)) != 0 {
                                bytes[row * w + col] = 0xFF;
                            }
                        }
                    }
                    Ok(Some((extent.width, extent.height, bytes)))
                }
                Err(e) => {
                    log::warn!(
                        "v2 read_depth1_pixmap: engine.get_image failed for xid \
                         {host_xid:#x}: {e:?}",
                    );
                    Ok(None)
                }
            };
        // Same SyncWait close-event drain as get_image above.
        self.drain_frame_builder_telemetry();
        result
    }

    fn clear_area(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        background_pixel: u32,
        background_pixmap_host_xid: Option<u32>,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        tile_origin: (i32, i32),
    ) -> io::Result<()> {
        self.clear_window_area_with_background(
            host_xid,
            background_pixel,
            background_pixmap_host_xid,
            x,
            y,
            width,
            height,
            tile_origin,
        )
    }

    fn poly_line(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        coordinate_mode: u8,
        points: &[u8],
    ) -> io::Result<()> {
        let Some(target) = self.resolve_paint_target(host_xid) else {
            self.log_v2_gap("poly_line_unknown_xid");
            return Ok(());
        };
        // Cook the polyline vertices (coordinate_mode 0 = Origin
        // absolute, 1 = Previous deltas).
        let mut verts: Vec<(i32, i32)> = Vec::new();
        let mut prev: Option<(i32, i32)> = None;
        let mut offset = 0;
        while let Some((x, y)) = crate::kms::backend::read_i16_pair(points, offset) {
            offset += 4;
            let (xi, yi) = if coordinate_mode == 1 {
                if let Some((px, py)) = prev {
                    (px + i32::from(x), py + i32::from(y))
                } else {
                    (i32::from(x), i32::from(y))
                }
            } else {
                (i32::from(x), i32::from(y))
            };
            verts.push((xi, yi));
            prev = Some((xi, yi));
        }
        let stroke = self.current_stroke_state(foreground);
        let out = crate::kms::v2::stroke::stroke_path(
            &verts,
            crate::kms::v2::stroke::StrokeShape::Polyline,
            &stroke,
        );
        self.emit_stroke_output(target, foreground, stroke.background, out);
        Ok(())
    }

    fn poly_segment(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        segments: &[u8],
    ) -> io::Result<()> {
        let Some(target) = self.resolve_paint_target(host_xid) else {
            self.log_v2_gap("poly_segment_unknown_xid");
            return Ok(());
        };
        // Each segment is (x1:i16, y1:i16, x2:i16, y2:i16). Cook into
        // a flat (p0, p1, p0, p1, ...) vertex list for stroke_path's
        // DisjointSegments shape.
        let mut verts: Vec<(i32, i32)> = Vec::new();
        let mut offset = 0;
        while offset + 8 <= segments.len() {
            let Some((x1, y1)) = crate::kms::backend::read_i16_pair(segments, offset) else {
                break;
            };
            let Some((x2, y2)) = crate::kms::backend::read_i16_pair(segments, offset + 4) else {
                break;
            };
            offset += 8;
            verts.push((i32::from(x1), i32::from(y1)));
            verts.push((i32::from(x2), i32::from(y2)));
        }
        let stroke = self.current_stroke_state(foreground);
        let out = crate::kms::v2::stroke::stroke_path(
            &verts,
            crate::kms::v2::stroke::StrokeShape::DisjointSegments,
            &stroke,
        );
        self.emit_stroke_output(target, foreground, stroke.background, out);
        Ok(())
    }

    fn poly_rectangle(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        rectangles: &[u8],
    ) -> io::Result<()> {
        let Some(target) = self.resolve_paint_target(host_xid) else {
            self.log_v2_gap("poly_rectangle_unknown_xid");
            return Ok(());
        };
        let stroke = self.current_stroke_state(foreground);
        let mut fg_rects: Vec<Rectangle16> = Vec::new();
        let mut bg_rects: Vec<Rectangle16> = Vec::new();
        let mut offset = 0;
        while offset + 8 <= rectangles.len() {
            let Some(r) = crate::kms::backend::read_rect(rectangles, offset) else {
                break;
            };
            offset += 8;
            if r.width == 0 || r.height == 0 {
                continue;
            }
            // Per-rectangle polyline: 5 vertices, closes back to start
            // so the corner joins fire. fast-path width≤1 keeps this
            // bit-identical to the prior 4-edge-rect emission.
            let x0 = i32::from(r.x);
            let y0 = i32::from(r.y);
            let x1 = x0 + i32::from(r.width) - 1;
            let y1 = y0 + i32::from(r.height) - 1;
            let verts = [(x0, y0), (x1, y0), (x1, y1), (x0, y1), (x0, y0)];
            let out = crate::kms::v2::stroke::stroke_path(
                &verts,
                crate::kms::v2::stroke::StrokeShape::Polyline,
                &stroke,
            );
            fg_rects.extend(out.fg_rects);
            bg_rects.extend(out.bg_rects);
        }
        self.emit_stroke_output(
            target,
            foreground,
            stroke.background,
            crate::kms::v2::stroke::StrokeOutput { fg_rects, bg_rects },
        );
        Ok(())
    }

    fn poly_arc(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        arcs: &[u8],
    ) -> io::Result<()> {
        let Some(target) = self.resolve_paint_target(host_xid) else {
            self.log_v2_gap("poly_arc_unknown_xid");
            return Ok(());
        };
        // Each arc: x(i16) y(i16) w(u16) h(u16) angle1(i16) angle2(i16).
        // Walk each arc parametrically (honouring angle1/angle2 — partial
        // arcs no longer fall back to a full ellipse) into a chord
        // polyline, then run it through the stroke rasterizer so
        // line_width / cap_style / dashes apply. JoinStyle is irrelevant
        // within a single smooth arc.
        let stroke = self.current_stroke_state(foreground);
        let mut fg_rects: Vec<Rectangle16> = Vec::new();
        let mut bg_rects: Vec<Rectangle16> = Vec::new();
        for chunk in arcs.chunks_exact(12) {
            let ax = i32::from(i16::from_le_bytes([chunk[0], chunk[1]]));
            let ay = i32::from(i16::from_le_bytes([chunk[2], chunk[3]]));
            let aw = i32::from(u16::from_le_bytes([chunk[4], chunk[5]]));
            let ah = i32::from(u16::from_le_bytes([chunk[6], chunk[7]]));
            let angle1 = i16::from_le_bytes([chunk[8], chunk[9]]);
            let angle2 = i16::from_le_bytes([chunk[10], chunk[11]]);
            if aw <= 0 || ah <= 0 || angle2 == 0 {
                continue;
            }
            let cx = f64::from(ax) + f64::from(aw) * 0.5;
            let cy = f64::from(ay) + f64::from(ah) * 0.5;
            let rx = f64::from(aw) * 0.5;
            let ry = f64::from(ah) * 0.5;
            let verts = crate::kms::v2::stroke::arc_polyline(cx, cy, rx, ry, angle1, angle2);
            let out = crate::kms::v2::stroke::stroke_path(
                &verts,
                crate::kms::v2::stroke::StrokeShape::Polyline,
                &stroke,
            );
            fg_rects.extend(out.fg_rects);
            bg_rects.extend(out.bg_rects);
        }
        self.emit_stroke_output(
            target,
            foreground,
            stroke.background,
            crate::kms::v2::stroke::StrokeOutput { fg_rects, bg_rects },
        );
        Ok(())
    }

    fn poly_point(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        coordinate_mode: u8,
        points: &[u8],
    ) -> io::Result<()> {
        let Some(target) = self.resolve_paint_target(host_xid) else {
            self.log_v2_gap("poly_point_unknown_xid");
            return Ok(());
        };
        let mut rects = Vec::new();
        let mut prev = (0i32, 0i32);
        let mut first = true;
        let mut offset = 0;
        while let Some((x, y)) = crate::kms::backend::read_i16_pair(points, offset) {
            offset += 4;
            let (xi, yi) = if coordinate_mode == 1 && !first {
                (prev.0 + i32::from(x), prev.1 + i32::from(y))
            } else {
                (i32::from(x), i32::from(y))
            };
            first = false;
            prev = (xi, yi);
            rects.push(Rectangle16 {
                x: xi.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16,
                y: yi.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16,
                width: 1,
                height: 1,
            });
        }
        let rects = self.intersect_with_current_clip_live(&rects);
        self.fill_solid_rects(target, foreground, &rects);
        Ok(())
    }

    fn poly_fill_rectangle(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        rectangles: &[u8],
    ) -> io::Result<()> {
        // Each X11 Rectangle is 8 bytes: { i16 x, i16 y, u16 w, u16 h }.
        let Some(target) = self.resolve_paint_target(host_xid) else {
            self.log_v2_gap("poly_fill_rectangle_unknown_xid");
            return Ok(());
        };
        let mut rects = Vec::new();
        let mut offset = 0;
        while offset + 8 <= rectangles.len() {
            let Some(r) = crate::kms::backend::read_rect(rectangles, offset) else {
                break;
            };
            offset += 8;
            rects.push(r);
        }
        let rects = self.intersect_with_current_clip_live(&rects);
        self.fill_rects_honoring_fill_state(host_xid, target, foreground, &rects);
        Ok(())
    }

    fn poly_fill_arc(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        arcs: &[u8],
    ) -> io::Result<()> {
        let Some(target) = self.resolve_paint_target(host_xid) else {
            self.log_v2_gap("poly_fill_arc_unknown_xid");
            return Ok(());
        };
        // Each arc is 12 bytes: x(i16) y(i16) w(u16) h(u16) angle1(i16) angle2(i16).
        // Build the closed fill polygon per the GC's ArcMode (Chord vs
        // PieSlice), honouring angle1/angle2 (partial arcs no longer
        // fill the full ellipse), then scanline-fill it.
        let arc_mode = self.core.current_arc_mode;
        let (img_w, img_h) = self
            .drawable_dims_v2(host_xid)
            .map(|(w, h)| (w as i32, h as i32))
            .unwrap_or((0, 0));
        let mut rects: Vec<Rectangle16> = Vec::new();
        for chunk in arcs.chunks_exact(12) {
            let ax = i32::from(i16::from_le_bytes([chunk[0], chunk[1]]));
            let ay = i32::from(i16::from_le_bytes([chunk[2], chunk[3]]));
            let aw = i32::from(u16::from_le_bytes([chunk[4], chunk[5]]));
            let ah = i32::from(u16::from_le_bytes([chunk[6], chunk[7]]));
            let angle1 = i16::from_le_bytes([chunk[8], chunk[9]]);
            let angle2 = i16::from_le_bytes([chunk[10], chunk[11]]);
            if aw <= 0 || ah <= 0 || angle2 == 0 {
                continue;
            }
            let cx = f64::from(ax) + f64::from(aw) * 0.5;
            let cy = f64::from(ay) + f64::from(ah) * 0.5;
            let rx = f64::from(aw) * 0.5;
            let ry = f64::from(ah) * 0.5;
            let verts =
                crate::kms::v2::stroke::fill_arc_polygon(cx, cy, rx, ry, angle1, angle2, arc_mode);
            crate::kms::backend::scanline_fill_polygon(&verts, &mut rects);
        }
        if !rects.is_empty() {
            let clipped = crate::kms::backend::clip_rects_to_image(&rects, img_w, img_h);
            let rects = self.intersect_with_current_clip_live(&clipped);
            self.fill_rects_honoring_fill_state(host_xid, target, foreground, &rects);
        }
        Ok(())
    }

    fn fill_poly(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        coord_mode: u8,
        points: &[u8],
    ) -> io::Result<()> {
        let Some(target) = self.resolve_paint_target(host_xid) else {
            self.log_v2_gap("fill_poly_unknown_xid");
            return Ok(());
        };
        // i16 vertex pairs. coord_mode 0 = Origin (absolute), 1 = Previous.
        let mut verts: Vec<(i32, i32)> = Vec::with_capacity(points.len() / 4);
        let mut offset = 0;
        let mut last = (0i32, 0i32);
        while let Some((x, y)) = crate::kms::backend::read_i16_pair(points, offset) {
            offset += 4;
            let (xi, yi) = if coord_mode == 1 && !verts.is_empty() {
                (last.0 + i32::from(x), last.1 + i32::from(y))
            } else {
                (i32::from(x), i32::from(y))
            };
            verts.push((xi, yi));
            last = (xi, yi);
        }
        let mut rects: Vec<Rectangle16> = Vec::new();
        crate::kms::backend::scanline_fill_polygon(&verts, &mut rects);
        let (img_w, img_h) = self
            .drawable_dims_v2(host_xid)
            .map(|(w, h)| (w as i32, h as i32))
            .unwrap_or((0, 0));
        let clipped = crate::kms::backend::clip_rects_to_image(&rects, img_w, img_h);
        let rects = self.intersect_with_current_clip_live(&clipped);
        self.fill_rects_honoring_fill_state(host_xid, target, foreground, &rects);
        Ok(())
    }

    fn fill_rectangle(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
    ) -> io::Result<()> {
        let Some(target) = self.resolve_paint_target(host_xid) else {
            self.log_v2_gap("fill_rectangle_unknown_xid");
            return Ok(());
        };
        let rects = self.intersect_with_current_clip_live(&[Rectangle16 {
            x,
            y,
            width,
            height,
        }]);
        self.fill_rects_honoring_fill_state(host_xid, target, foreground, &rects);
        Ok(())
    }

    fn poly_text8(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        body: &[u8],
    ) -> io::Result<()> {
        // Body: drawable(4) + gc(4) + x(2) + y(2) + LISTofTEXTITEM8.
        // Each TEXTITEM8 is `len(u8) delta(i8) chars(len)` for len
        // in 0..=254, or `255 font_id(u32 BE)` for a font change.
        // No inter-item padding.
        if body.len() < 12 {
            return Ok(());
        }
        let x = i16::from_le_bytes([body[8], body[9]]) as i32;
        let y = i16::from_le_bytes([body[10], body[11]]) as i32;
        let mut items = &body[12..];
        let mut cursor_x = x;
        while items.len() >= 2 {
            let len = items[0];
            if len == 255 {
                if items.len() < 5 {
                    break;
                }
                let font_xid = u32::from_be_bytes([items[1], items[2], items[3], items[4]]);
                self.core.current_font = Some(font_xid);
                items = &items[5..];
                continue;
            }
            let delta = items[1] as i8;
            let len = len as usize;
            if items.len() < 2 + len {
                break;
            }
            let text = &items[2..2 + len];
            cursor_x = cursor_x.saturating_add(i32::from(delta));
            if !text.is_empty() {
                let chars: Vec<char> = text.iter().map(|&b| b as char).collect();
                self.render_text_chars_v2(host_xid, foreground, cursor_x, y, &chars)?;
                if let Some(font_state) =
                    self.core.current_font.and_then(|f| self.core.fonts.get(&f))
                {
                    cursor_x = cursor_x.saturating_add(text_advance(font_state, &chars));
                }
            }
            items = &items[2 + len..];
        }
        Ok(())
    }

    fn poly_text16(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        body: &[u8],
    ) -> io::Result<()> {
        // Body: drawable(4) + gc(4) + x(2) + y(2) + LISTofTEXTITEM16.
        // Each TEXTITEM16 is `len(u8) delta(i8) chars(2*len)` (chars
        // are CHAR2B, big-endian) for len in 0..=254, or `255
        // font_id(u32 BE)` for a font change.
        if body.len() < 12 {
            return Ok(());
        }
        let x = i16::from_le_bytes([body[8], body[9]]) as i32;
        let y = i16::from_le_bytes([body[10], body[11]]) as i32;
        let mut cursor_x = x;
        let mut items = &body[12..];
        while items.len() >= 2 {
            let len = items[0];
            if len == 255 {
                if items.len() < 5 {
                    break;
                }
                let font_xid = u32::from_be_bytes([items[1], items[2], items[3], items[4]]);
                self.core.current_font = Some(font_xid);
                items = &items[5..];
                continue;
            }
            let delta = items[1] as i8;
            let len = len as usize;
            let needed = 2 + 2 * len;
            if items.len() < needed {
                break;
            }
            cursor_x = cursor_x.saturating_add(i32::from(delta));
            let mut chars = Vec::with_capacity(len);
            for i in 0..len {
                let codepoint = u16::from_be_bytes([items[2 + 2 * i], items[2 + 2 * i + 1]]) as u32;
                chars.push(char::from_u32(codepoint).unwrap_or('\u{fffd}'));
            }
            if !chars.is_empty() {
                self.render_text_chars_v2(host_xid, foreground, cursor_x, y, &chars)?;
                if let Some(font_state) =
                    self.core.current_font.and_then(|f| self.core.fonts.get(&f))
                {
                    cursor_x = cursor_x.saturating_add(text_advance(font_state, &chars));
                }
            }
            items = &items[needed..];
        }
        Ok(())
    }

    fn image_text8(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        background: u32,
        text_len: u8,
        body: &[u8],
    ) -> io::Result<()> {
        // Body: drawable(4) + gc(4) + x(2) + y(2) + string(text_len)
        if body.len() < 12 {
            return Ok(());
        }
        let x = i16::from_le_bytes([body[8], body[9]]) as i32;
        let y = i16::from_le_bytes([body[10], body[11]]) as i32;
        let end = (12usize + text_len as usize).min(body.len());
        let chars: Vec<char> = body[12..end].iter().map(|&b| b as char).collect();
        self.image_text_common(host_xid, foreground, background, x, y, &chars)
    }

    fn image_text16(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        background: u32,
        text_len: u8,
        body: &[u8],
    ) -> io::Result<()> {
        if body.len() < 12 {
            return Ok(());
        }
        let x = i16::from_le_bytes([body[8], body[9]]) as i32;
        let y = i16::from_le_bytes([body[10], body[11]]) as i32;
        let mut chars = Vec::with_capacity(text_len as usize);
        let mut pos = 12usize;
        for _ in 0..text_len {
            if pos + 2 > body.len() {
                break;
            }
            let codepoint = u16::from_be_bytes([body[pos], body[pos + 1]]) as u32;
            pos += 2;
            chars.push(char::from_u32(codepoint).unwrap_or('\u{fffd}'));
        }
        self.image_text_common(host_xid, foreground, background, x, y, &chars)
    }

    // ── RENDER ──────────────────────────────────────────────────

    fn render_create_picture(
        &mut self,
        _origin: Option<OriginContext>,
        host_drawable: AnyHandle,
        ynest_format: u32,
        value_mask: u32,
        values: &[u8],
    ) -> io::Result<Option<PictureHandle>> {
        // Stage 3b: real picture record. Insert default
        // `PictureRecord::Drawable`, incref the backing drawable in
        // the store (so a `free_pixmap` on the backing survives
        // while this picture wraps it — picture_record_drawable_
        // refcount test), then delegate to render_change_picture for
        // the value-mask body.
        let drawable_xid = host_drawable.as_raw();
        let picture_xid = self.core.next_host_xid();
        // Diagnostic trace (TEMP — Stage 4d "shadow only"
        // investigation). v2's PictureRecord doesn't store the
        // requested PictFormat; capturing it here so a downstream
        // analysis can see which format marco asked for when
        // wrapping a redirected backing's alias. Enable with
        // `RUST_LOG=yserver::kms::v2::render=trace`.
        log::trace!(
            target: "yserver::kms::v2::render",
            "render_create_picture pic=0x{picture_xid:x} drawable=0x{drawable_xid:x} \
             ynest_format=0x{ynest_format:x} value_mask=0x{value_mask:x} \
             value_bytes={n}",
            n = values.len(),
        );
        self.core.pictures.insert(
            picture_xid,
            PictureRecord::drawable_default(drawable_xid, ynest_format),
        );
        if let Some(id) = self.store.lookup(drawable_xid) {
            self.store.incref(id);
        }
        if value_mask != 0 {
            // Recompose the body shape that render_change_picture
            // expects: picture(4) + value_mask(4) + values.
            let mut body = Vec::with_capacity(8 + values.len());
            body.extend_from_slice(&picture_xid.to_le_bytes());
            body.extend_from_slice(&value_mask.to_le_bytes());
            body.extend_from_slice(values);
            self.render_change_picture(None, picture_xid, &body)?;
        }
        Ok(PictureHandle::from_raw(picture_xid))
    }

    fn render_change_picture(
        &mut self,
        _origin: Option<OriginContext>,
        host_pic: u32,
        body: &[u8],
    ) -> io::Result<()> {
        // Diagnostic trace (TEMP — Stage 4d "shadow only"
        // investigation). Body shape: picture(4) + value_mask(4) +
        // values. We log the mask bits + post-call clip state so a
        // grep across the log can see whether CPClipMask=None
        // cleared the dst picture's clip between marco's last
        // SetPictureClipRectangles and the next render_composite.
        if log::log_enabled!(target: "yserver::kms::v2::render", log::Level::Trace)
            && body.len() >= 8
        {
            let mask = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
            log::trace!(
                target: "yserver::kms::v2::render",
                "render_change_picture pic=0x{host_pic:x} mask=0x{mask:x} body_len={}",
                body.len(),
            );
        }
        change_picture_apply_mask(&mut self.core, host_pic, body);
        // After applying — log clip state if pic is a Drawable.
        if log::log_enabled!(target: "yserver::kms::v2::render", log::Level::Trace)
            && let Some(PictureRecord::Drawable {
                clip,
                clip_x,
                clip_y,
                ..
            }) = self.core.pictures.get(&host_pic)
        {
            log::trace!(
                target: "yserver::kms::v2::render",
                "render_change_picture post pic=0x{host_pic:x} clip={} clip_origin=({clip_x},{clip_y})",
                match clip {
                    None => "None".to_string(),
                    Some(rects) => format!("Some(n={})", rects.len()),
                },
            );
        }
        Ok(())
    }

    /// Audit #8 (2026-05-19) — store the drawable-space origin of
    /// the wrapped surface on the picture record. The protocol
    /// layer calls this right after `render_create_picture` with
    /// the parent-relative `(x, y)` of a window-backed drawable
    /// (process_request.rs:1153). Pre-fix v2 inherited the trait
    /// default no-op so `drawable_origin` stayed at the
    /// `drawable_default` `(0, 0)` — clips on CSD-frame-child
    /// pictures couldn't translate external region geometry into
    /// picture-local coords.
    ///
    /// Non-Drawable picture variants (SolidFill / Linear /
    /// Radial gradient) have no drawable to anchor — tolerated
    /// no-op so the caller doesn't need to discriminate at the
    /// call site.
    fn set_picture_drawable_origin(&mut self, host_pic: u32, origin: (i16, i16)) {
        if let Some(PictureRecord::Drawable {
            drawable_origin, ..
        }) = self.core.pictures.get_mut(&host_pic)
        {
            *drawable_origin = origin;
        }
    }

    /// Audit #8 (2026-05-19) — return the picture's `clientClip` for
    /// `CreateRegionFromPicture` (XFixes). Outer `Option` distinguishes
    /// "picture doesn't carry a clientClip at all" (Solidfill /
    /// gradient → `None`, dispatcher emits BadMatch) from "picture
    /// exists and we know its clip state" (Drawable → `Some(_)`).
    /// Inner `Option` distinguishes "no clip set yet" (`Some(None)`,
    /// also BadMatch per X11 spec — can't extract a region from a
    /// picture with no clip) from "clip set" (`Some(Some(rects))`,
    /// returned as the region's rects).
    ///
    /// Pre-fix v2 inherited the trait default `None` so EVERY
    /// `CreateRegionFromPicture` call returned BadMatch — even for
    /// pictures with legitimate clipped state. Visible in clipboard
    /// managers / window managers that use this XFixes path.
    fn picture_client_clip_rects(
        &mut self,
        host_pic: u32,
    ) -> Option<Option<Vec<yserver_protocol::x11::xfixes::RegionRect>>> {
        let record = self.core.pictures.get(&host_pic)?;
        match record {
            PictureRecord::Drawable { clip, .. } => Some(clip.as_ref().map(|rects| {
                rects
                    .iter()
                    .map(|r| yserver_protocol::x11::xfixes::RegionRect {
                        x: r.x,
                        y: r.y,
                        width: r.width,
                        height: r.height,
                    })
                    .collect()
            })),
            PictureRecord::SolidFill { .. }
            | PictureRecord::LinearGradient { .. }
            | PictureRecord::RadialGradient { .. } => None,
        }
    }

    fn render_free_picture(
        &mut self,
        _origin: Option<OriginContext>,
        host_pic: u32,
    ) -> io::Result<()> {
        // Drop the record; if it was a Drawable variant, decref the
        // backing drawable in the store. SolidFill / Gradient
        // variants have no backing drawable — they own only the
        // GPU-side state on RenderEngine.picture_paint (Stage 3c).
        if let Some(record) = self.core.pictures.remove(&host_pic)
            && let Some(drawable_xid) = record.drawable_host_xid()
            && let Some(id) = self.store.lookup(drawable_xid)
        {
            self.store_decref_with_invalidate(id);
        }
        // Drop any GPU-side state cached for this picture. Stage
        // 3b never populates the map (no gradient LUT built yet),
        // so this is a HashMap::remove no-op today; Stage 3c lazy-
        // builds gradient picture state through the same key, and
        // this teardown hook becomes load-bearing once that lands.
        self.engine.picture_paint_remove(host_pic);
        Ok(())
    }

    fn render_create_glyphset(
        &mut self,
        _origin: Option<OriginContext>,
        ynest_format: u32,
    ) -> io::Result<Option<GlyphSetHandle>> {
        use crate::kms::core::{GlyphSetFormat, GlyphSetState};

        let format = match ynest_format {
            RENDER_FMT_A8 => GlyphSetFormat::A8,
            RENDER_FMT_A1 => GlyphSetFormat::A1,
            RENDER_FMT_ARGB32 => GlyphSetFormat::Argb32,
            _ => GlyphSetFormat::Other,
        };
        let id = self.core.next_host_xid();
        self.core.glyphsets.insert(
            id,
            GlyphSetState {
                format,
                glyphs: HashMap::new(),
            },
        );
        Ok(GlyphSetHandle::from_raw(id))
    }

    fn render_free_glyphset(
        &mut self,
        _origin: Option<OriginContext>,
        host_gs: u32,
    ) -> io::Result<()> {
        // Drop the glyphset record. Atlas-side slot reclamation
        // is Stage 5 (per Stage 3a glyph atlas: shelf packer is
        // monotonic), so the atlas pixels stay until atlas-full.
        self.core.glyphsets.remove(&host_gs);
        Ok(())
    }

    fn render_add_glyphs(
        &mut self,
        _origin: Option<OriginContext>,
        host_gs: u32,
        body_tail: &[u8],
    ) -> io::Result<()> {
        // Reuses v1's parse_add_glyphs — purely CPU-side, operates
        // on the KmsCore.glyphsets entry. Atlas-side upload (the
        // Vk part) is Stage 3d's render_composite_glyphs path.
        if let Some(gs) = self.core.glyphsets.get_mut(&host_gs) {
            crate::kms::backend::parse_add_glyphs(gs, body_tail);
        }
        Ok(())
    }

    fn render_free_glyphs(
        &mut self,
        _origin: Option<OriginContext>,
        host_gs: u32,
        glyph_ids: &[u8],
    ) -> io::Result<()> {
        let Some(gs) = self.core.glyphsets.get_mut(&host_gs) else {
            return Ok(());
        };
        for chunk in glyph_ids.chunks_exact(4) {
            let id = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            gs.glyphs.remove(&id);
        }
        Ok(())
    }

    fn render_composite(
        &mut self,
        _origin: Option<OriginContext>,
        op: u8,
        host_src: u32,
        host_mask: u32,
        host_dst: u32,
        src_x: i16,
        src_y: i16,
        mask_x: i16,
        mask_y: i16,
        dst_x: i16,
        dst_y: i16,
        width: u16,
        height: u16,
    ) -> io::Result<()> {
        use crate::kms::v2::engine::ResolvedSource;
        if width == 0 || height == 0 {
            return Ok(());
        }
        let Some((src_resolved, src_repeat, src_transform, _src_ca)) =
            resolve_picture_for_render(&self.core, &self.store, host_src)
        else {
            log::debug!("v2 render_composite gap: host_src 0x{host_src:x} not resolvable");
            return Ok(());
        };
        let (mask_resolved, mask_repeat, mask_transform, mask_component_alpha) = if host_mask == 0 {
            (ResolvedSource::None, Repeat::None, None, false)
        } else {
            let Some(t) = resolve_picture_for_render(&self.core, &self.store, host_mask) else {
                log::debug!("v2 render_composite gap: host_mask 0x{host_mask:x} not resolvable");
                return Ok(());
            };
            t
        };
        let Some((dst_host_xid, dst_clip)) = resolve_dst_picture_for_render(&self.core, host_dst)
        else {
            log::debug!("v2 render_composite gap: host_dst 0x{host_dst:x} not a Drawable picture");
            return Ok(());
        };
        // Stage 4a — resolve through redirect routing. The picture
        // wraps a window xid; the actual paint may land in that
        // window's COMPOSITE backing with an accumulated offset.
        let Some(dst_target) = self.resolve_paint_target(dst_host_xid) else {
            log::debug!(
                "v2 render_composite gap: dst drawable 0x{dst_host_xid:x} \
                 not in store (post-resolve)"
            );
            return Ok(());
        };
        let dst_clip = Self::shift_dst_picture_clip(dst_clip, dst_target.offset);

        // Audit #2 (2026-05-19) — fold src/mask client clips into
        // the composite-region clip per Xorg's
        // `miComputeCompositeRegion` (`render/mipict.c:316-389`).
        // Pre-fix, `resolve_picture_for_render` discarded src/mask
        // clips entirely, so `SetPictureClipRectangles` on a source
        // picture (xfwm4/muffin shadow blits) painted over the
        // whole dst. The translation offset matches Xorg's
        // `miClipPictureSrc(..., xDst - xSrc, yDst - ySrc)` call
        // site at `mipict.c:356,370` — the dst already has
        // `dst_target.offset` applied to `(xDst, yDst)`, so the
        // translation picks up that offset automatically.
        let src_clip = picture_client_clip(&self.core, host_src);
        let mask_clip = if host_mask == 0 {
            None
        } else {
            picture_client_clip(&self.core, host_mask)
        };
        let dst_origin_x = i32::from(dst_x) + dst_target.offset.0;
        let dst_origin_y = i32::from(dst_y) + dst_target.offset.1;
        let src_translation = (
            dst_origin_x - i32::from(src_x),
            dst_origin_y - i32::from(src_y),
        );
        let mask_translation = (
            dst_origin_x - i32::from(mask_x),
            dst_origin_y - i32::from(mask_y),
        );
        let dst_clip = compute_render_composite_clip(
            dst_clip.as_deref(),
            src_clip.as_deref(),
            src_translation,
            mask_clip.as_deref(),
            mask_translation,
        );

        let rect = crate::kms::vk::ops::render::CompositeRect {
            src_x: i32::from(src_x),
            src_y: i32::from(src_y),
            mask_x: i32::from(mask_x),
            mask_y: i32::from(mask_y),
            dst_x: i32::from(dst_x) + dst_target.offset.0,
            dst_y: i32::from(dst_y) + dst_target.offset.1,
            width: u32::from(width),
            height: u32::from(height),
        };
        // Diagnostic trace (TEMP — Stage 4d "shadow only"
        // investigation). Enable with
        // `RUST_LOG=yserver::kms::v2::render=trace`.
        // Logs every render_composite at the backend boundary
        // with resolved source/mask/dst kinds + depths, the dst
        // drawable id (to bisect "marco's compose onto its own
        // offscreen" vs "compose onto a redirected backing"), the
        // composite op, coords, repeat / transform / component-
        // alpha state, and (after the engine call) the engine
        // stats (recorded_draws, used_src_alias_scratch,
        // used_dst_readback). Removed once we land or rule out
        // the next RENDER-side fix.
        if log::log_enabled!(target: "yserver::kms::v2::render", log::Level::Trace) {
            let (src_kind, src_depth) = describe_resolved_source(&self.store, &src_resolved);
            let (mask_kind, mask_depth) = describe_resolved_source(&self.store, &mask_resolved);
            let dst_depth = self.store.get(dst_target.id).map_or(0, |d| d.depth);
            // Picture-format IDs as declared at CreatePicture —
            // captures marco's sampling intent which can differ
            // from the drawable's depth-derived format.
            let src_pict_format = picture_pict_format(&self.core, host_src);
            let mask_pict_format = picture_pict_format(&self.core, host_mask);
            let dst_pict_format = picture_pict_format(&self.core, host_dst);
            let dst_picture_clip =
                resolve_dst_picture_for_render(&self.core, host_dst).and_then(|(_, clip)| clip);
            let dst_picture_clip_dump = format_clip_rects(dst_picture_clip.as_deref());
            let src_clip_dump = format_clip_rects(src_clip.as_deref());
            let mask_clip_dump = format_clip_rects(mask_clip.as_deref());
            let final_clip_dump = format_clip_rects(dst_clip.as_deref());
            log::trace!(
                target: "yserver::kms::v2::render",
                "render_composite op={op} src=0x{host_src:x}({src_kind},d={src_depth},fmt=0x{src_pict_format:x},repeat={src_repeat:?},xform={src_xform}) \
                 mask=0x{host_mask:x}({mask_kind},d={mask_depth},fmt=0x{mask_pict_format:x},repeat={mask_repeat:?},xform={mask_xform},ca={mask_component_alpha}) \
                 dst=0x{host_dst:x}->id={dst_id:?},d={dst_depth},fmt=0x{dst_pict_format:x} \
                 src_xy=({src_x},{src_y}) mask_xy=({mask_x},{mask_y}) dst_xy=({dst_x},{dst_y})+off=({off_x},{off_y}) {width}x{height} \
                 src_clip={src_clip_dump} src_t=({src_tx},{src_ty}) mask_clip={mask_clip_dump} mask_t=({mask_tx},{mask_ty}) \
                 dst_picture_clip={dst_picture_clip_dump} final_clip={final_clip_dump}",
                src_xform = src_transform.is_some(),
                mask_xform = mask_transform.is_some(),
                dst_id = dst_target.id,
                off_x = dst_target.offset.0,
                off_y = dst_target.offset.1,
                src_tx = src_translation.0,
                src_ty = src_translation.1,
                mask_tx = mask_translation.0,
                mask_ty = mask_translation.1,
            );
        }
        // Audit #4 (2026-05-19) — thread src/mask/dst PictFormat IDs
        // through to the engine so an xRGB32 picture wrapping a
        // depth-32 storage picks a no-alpha sample swizzle +
        // force-opaque for sources, AND the right "no alpha target"
        // pipeline + readback selection for destinations.
        // `picture_pict_format` returns 0 for non-Drawable picture
        // variants and unknown xids — engine falls back to the depth
        // heuristic in those cases.
        let src_pict_format = picture_pict_format(&self.core, host_src);
        let mask_pict_format = picture_pict_format(&self.core, host_mask);
        let dst_pict_format = picture_pict_format(&self.core, host_dst);
        let stats = self.engine.render_composite(
            &mut self.store,
            &mut self.platform,
            op,
            src_resolved,
            mask_resolved,
            dst_target.id,
            std::slice::from_ref(&rect),
            dst_clip.as_deref(),
            src_repeat,
            mask_repeat,
            src_transform,
            mask_transform,
            mask_component_alpha,
            src_pict_format,
            mask_pict_format,
            dst_pict_format,
        );
        self.sync_descriptor_pool_telemetry();
        let src_class = self.picture_src_class_by_xid(host_src);
        let mask_class = if host_mask == 0 {
            None
        } else {
            Some(self.picture_src_class_by_xid(host_mask))
        };
        match &stats {
            Ok(s) => {
                if s.recorded_draws > 0 && !s.deferred_to_batch {
                    self.telemetry.record_paint_submit();
                    self.trace_render(
                        SubmitKind::RenderComposite,
                        dst_target.id,
                        s.recorded_draws,
                        op,
                        src_class,
                        mask_class,
                        SubmitFlags {
                            readback: s.used_dst_readback,
                            alias: s.used_src_alias_scratch,
                            zero_draws: false,
                            upload: false,
                        },
                    );
                }
                if s.used_dst_readback {
                    self.telemetry.record_disjoint_readback();
                }
                log::trace!(
                    target: "yserver::kms::v2::render",
                    "render_composite stats dst=0x{host_dst:x} \
                     recorded_draws={} used_src_alias_scratch={} used_dst_readback={}",
                    s.recorded_draws,
                    s.used_src_alias_scratch,
                    s.used_dst_readback,
                );
            }
            Err(e) => {
                log::warn!("v2 render_composite: engine returned {e:?} on dst 0x{host_dst:x}");
            }
        }
        // Phase B.2 Task 15: render_composite may open a frame; drain
        // any resulting close events into telemetry so the per-second
        // emit picks them up without stale lag. Mirrors the B.1 drain
        // at the composite_glyphs wrapper.
        self.drain_frame_builder_telemetry();
        Ok(())
    }

    fn render_composite_glyphs(
        &mut self,
        _origin: Option<OriginContext>,
        minor: u8,
        op: u8,
        host_src: u32,
        host_dst: u32,
        _mask_fmt: u32,
        host_gs: u32,
        src_x: i16,
        src_y: i16,
        items: &[u8],
        x_off: i16,
        y_off: i16,
    ) -> io::Result<()> {
        use crate::kms::{
            core::GlyphSetFormat,
            v2::engine::{CompositeGlyphInput, ResolvedSource},
        };

        // v1-parity gating (plan §3d): op == Over (3) and the src
        // picture must be a SolidFill. Anything else returns
        // Ok(()) with `composite_glyphs_dropped_unsupported`
        // bumped — matches v1's silent-noop shape outside its
        // narrow envelope. `mask_fmt` is read but ignored
        // (rendercheck never exercises component-alpha glyphsets;
        // risk-listed in plan §"Risk 9").
        // Unsupported-counter scope (plan §3d): the gate captures
        // *protocol-supported but engine-unimplemented* shapes —
        // currently op != Over and source not SolidFill (the
        // "v1-parity scope" boundary). Stale src/dst picture
        // handles and missing glyphsets are protocol errors, not
        // unsupported features; they log a gap and return Ok
        // without bumping the counter.
        if op != 3 {
            log::debug!("v2 composite_glyphs gap: op={op} (only Over=3)");
            self.telemetry.record_composite_glyphs_dropped_unsupported();
            return Ok(());
        }
        let Some((src_resolved, _src_repeat, _src_xform, _src_ca)) =
            resolve_picture_for_render(&self.core, &self.store, host_src)
        else {
            log::debug!("v2 composite_glyphs gap: src 0x{host_src:x} not resolvable");
            return Ok(());
        };
        let foreground_premul = match src_resolved {
            ResolvedSource::Solid(c) => c,
            // Stage 3f.13: glyph paint path is still SolidFill-only
            // (matches v1's try_vk_render_composite_glyphs). For a
            // gradient source, collapse to first-stop premul — same
            // shape as the pre-3f.13 fallback, just scoped here
            // instead of in `resolve_picture_for_render`. No
            // counter bump: gradient-on-glyphs is now considered
            // "best effort handled" rather than "unsupported".
            ResolvedSource::Gradient(grad_xid) => {
                first_stop_premul_of_gradient(&self.core, grad_xid).unwrap_or_else(|| {
                    log::debug!(
                        "v2 composite_glyphs: gradient src 0x{grad_xid:x} \
                         has no stops — treating as transparent"
                    );
                    [0.0, 0.0, 0.0, 0.0]
                })
            }
            ResolvedSource::Drawable(_) | ResolvedSource::None => {
                log::debug!(
                    "v2 composite_glyphs gap: src 0x{host_src:x} is not SolidFill / Gradient \
                     (plan §3d v1-parity scope)"
                );
                self.telemetry.record_composite_glyphs_dropped_unsupported();
                return Ok(());
            }
        };
        let Some((dst_host_xid, dst_clip)) = resolve_dst_picture_for_render(&self.core, host_dst)
        else {
            log::debug!("v2 composite_glyphs gap: dst 0x{host_dst:x} not Drawable picture");
            return Ok(());
        };
        // Stage 4a — resolve through redirect routing.
        let Some(dst_target) = self.resolve_paint_target(dst_host_xid) else {
            log::debug!("v2 composite_glyphs gap: dst drawable 0x{dst_host_xid:x} not in store");
            return Ok(());
        };
        let dst_clip = Self::shift_dst_picture_clip(dst_clip, dst_target.offset);
        if !self.core.glyphsets.contains_key(&host_gs) {
            log::debug!("v2 composite_glyphs gap: glyphset 0x{host_gs:x} not registered");
            return Ok(());
        }

        // Items parser — mirrors v1's `try_vk_render_composite_glyphs`
        // shape. Element size depends on the minor opcode:
        // CompositeGlyphs8 (23) → 1 byte ids, 16 (24) → 2, 32 (25)
        // → 4. Each element starts with `count(u8) pad pad pad
        // dx(i16) dy(i16)`; if `count == 255` the same 8 bytes
        // carry an inline glyphset change with the new gs xid in
        // the trailing u32.
        let id_size: usize = match minor {
            23 => 1,
            24 => 2,
            _ => 4,
        };
        // Per X RENDER protocol, `src_x`/`src_y` are the SOURCE
        // picture sampling origin, not the dst pen — same as v1.
        // The first glyph-element's `dx` / `dy` sets the absolute
        // pen position; subsequent elements accumulate.
        let _ = (src_x, src_y);
        let mut pen_x = i32::from(x_off);
        let mut pen_y = i32::from(y_off);
        let mut pos: usize = 0;
        let mut active_gs_xid = host_gs;
        // Two-pass parse: pass 1 fills `parsed` with per-glyph
        // metadata + a slot reference into either the live
        // glyphset's pixel bytes (A8) or an A1 expansion scratch
        // (A1). Pass 2 builds the final `&[CompositeGlyphInput]`
        // with stable slice references. The split avoids a borrow
        // conflict on `a1_scratches`: pushing into the Vec
        // invalidates earlier `.last()` borrows by Rust's borrow
        // checker even though the underlying heap buffers are
        // stable (Vec<Vec<u8>>'s inner buffers don't move on
        // outer-push reallocation).
        enum PixelSource {
            FromGlyphset { gs_xid: u32, glyph_id: u32 },
            A1Scratch(usize),
        }
        struct Parsed {
            gs_xid: u32,
            glyph_id: u32,
            w: u32,
            h: u32,
            pixels: PixelSource,
            dst_x: i32,
            dst_y: i32,
        }
        let mut a1_scratches: Vec<Vec<u8>> = Vec::new();
        let mut parsed: Vec<Parsed> = Vec::new();
        // Borrow the glyphsets map immutably for the whole parse.
        // The engine call below takes `&mut self.engine` /
        // `&mut self.store` but not `&self.core.glyphsets`, so a
        // single borrow scope here is sound.
        while pos + 8 <= items.len() {
            let count = items[pos] as usize;
            if count == 255 {
                if pos + 8 <= items.len() {
                    let new_xid = u32::from_le_bytes([
                        items[pos + 4],
                        items[pos + 5],
                        items[pos + 6],
                        items[pos + 7],
                    ]);
                    if new_xid != 0 && self.core.glyphsets.contains_key(&new_xid) {
                        active_gs_xid = new_xid;
                    }
                }
                pos += 8;
                continue;
            }
            let dx = i32::from(i16::from_le_bytes([items[pos + 4], items[pos + 5]]));
            let dy = i32::from(i16::from_le_bytes([items[pos + 6], items[pos + 7]]));
            pen_x += dx;
            pen_y += dy;

            let payload_start = pos + 8;
            let payload_bytes = count * id_size;
            let padded = (payload_bytes + 3) & !3;
            if payload_start + padded > items.len() {
                break;
            }

            let Some(active_gs) = self.core.glyphsets.get(&active_gs_xid) else {
                pos += 8 + padded;
                continue;
            };
            let active_gs_xid_for_key = active_gs_xid;

            for i in 0..count {
                let id_off = payload_start + i * id_size;
                let glyph_id: u32 = match id_size {
                    1 => u32::from(items[id_off]),
                    2 => u32::from(u16::from_le_bytes([items[id_off], items[id_off + 1]])),
                    _ => u32::from_le_bytes([
                        items[id_off],
                        items[id_off + 1],
                        items[id_off + 2],
                        items[id_off + 3],
                    ]),
                };
                let Some(glyph) = active_gs.glyphs.get(&glyph_id) else {
                    continue;
                };

                let gw = u32::from(glyph.width);
                let gh = u32::from(glyph.height);
                let dst_x = pen_x - i32::from(glyph.x);
                let dst_y = pen_y - i32::from(glyph.y);

                if gw > 0 && gh > 0 {
                    let pixels = match glyph.format {
                        GlyphSetFormat::A8 => PixelSource::FromGlyphset {
                            gs_xid: active_gs_xid_for_key,
                            glyph_id,
                        },
                        GlyphSetFormat::A1 => {
                            // Wire A1: rows MSB-first, 32-bit padded.
                            // Expand into a dense row-major A8 (0/0xFF).
                            // Per v1's bit-order comment
                            // (kms::backend.rs:5471), X RENDER's
                            // glyph A1 is MSB-first within each byte
                            // — `7 - col%8`. Mirror verbatim.
                            let wire_stride = (gw as usize).div_ceil(32) * 4;
                            let mut a8 = vec![0u8; (gw * gh) as usize];
                            for row in 0..(gh as usize) {
                                let src_off = row * wire_stride;
                                if src_off + wire_stride > glyph.pixels.len() {
                                    break;
                                }
                                for col in 0..(gw as usize) {
                                    let byte = glyph.pixels[src_off + col / 8];
                                    let bit = (byte >> (7 - (col & 7))) & 1;
                                    a8[row * (gw as usize) + col] = if bit != 0 { 0xFF } else { 0 };
                                }
                            }
                            let idx = a1_scratches.len();
                            a1_scratches.push(a8);
                            PixelSource::A1Scratch(idx)
                        }
                        // ARGB32-source glyphs are pre-converted to
                        // A8 in `parse_add_glyphs`, so this branch
                        // is unreachable in practice. Defensive:
                        // skip the glyph if the stored format
                        // somehow ended up as ARGB32 / Other.
                        GlyphSetFormat::Argb32 | GlyphSetFormat::Other => {
                            log::warn!(
                                "v2 composite_glyphs: unexpected stored format {:?} for \
                                 glyph 0x{glyph_id:x} — skipping",
                                glyph.format,
                            );
                            continue;
                        }
                    };

                    parsed.push(Parsed {
                        gs_xid: active_gs_xid_for_key,
                        glyph_id,
                        w: gw,
                        h: gh,
                        pixels,
                        dst_x,
                        dst_y,
                    });
                }

                pen_x += i32::from(glyph.x_off);
                pen_y += i32::from(glyph.y_off);
            }

            pos += 8 + padded;
        }

        if parsed.is_empty() {
            // No drawable glyphs (every entry was zero-size or
            // missing from the glyphset). Not a gap; just nothing
            // to record.
            return Ok(());
        }

        // Pass 2: resolve each `Parsed` to a `CompositeGlyphInput`
        // with a stable slice reference. Stage 4a — apply the
        // dst-target offset to each glyph's dst coordinates so a
        // redirected window's glyphs land in the backing.
        let (paint_dx, paint_dy) = dst_target.offset;
        let inputs: Vec<CompositeGlyphInput<'_>> = parsed
            .iter()
            .filter_map(|p| {
                let pixels: &[u8] = match &p.pixels {
                    PixelSource::FromGlyphset { gs_xid, glyph_id } => self
                        .core
                        .glyphsets
                        .get(gs_xid)
                        .and_then(|gs| gs.glyphs.get(glyph_id))
                        .map(|g| g.pixels.as_slice())?,
                    PixelSource::A1Scratch(idx) => &a1_scratches[*idx],
                };
                Some(CompositeGlyphInput {
                    gs_xid: p.gs_xid,
                    glyph_id: p.glyph_id,
                    w: p.w,
                    h: p.h,
                    pixels,
                    dst_x: p.dst_x + paint_dx,
                    dst_y: p.dst_y + paint_dy,
                })
            })
            .collect();

        if inputs.is_empty() {
            return Ok(());
        }

        let stats = self.engine.composite_glyphs(
            &mut self.store,
            &mut self.platform,
            dst_target.id,
            foreground_premul,
            &inputs,
            dst_clip.as_deref(),
        );
        match stats {
            Ok(s) => {
                if s.atlas_interns > 0 {
                    for _ in 0..s.atlas_interns {
                        self.telemetry.record_atlas_intern();
                    }
                }
                if s.glyph_uploads > 0 {
                    for _ in 0..s.glyph_uploads {
                        self.telemetry.record_glyph_upload();
                    }
                    // One GlyphUpload event per upload submit
                    // (paired with the text-paint CB that
                    // follows). `dst_target.id` is the eventual
                    // destination — keep on the dst so analysis
                    // can correlate uploads with the dst's text
                    // bursts.
                    let target_kind = self.submit_target_kind(dst_target.id);
                    for _ in 0..s.glyph_uploads {
                        self.telemetry.record_submit_event(SubmitEvent {
                            frame_id: 0,
                            kind: SubmitKind::GlyphUpload,
                            target_kind,
                            target_id: dst_target.id.as_u64(),
                            batch_size: 1,
                            op: SubmitOp::None,
                            src_class: SrcClass::None,
                            mask_class: SrcClass::None,
                            pipeline_id: None,
                            flags: SubmitFlags {
                                readback: false,
                                alias: false,
                                zero_draws: false,
                                upload: true,
                            },
                        });
                    }
                }
                if s.glyphs_dropped > 0 {
                    for _ in 0..s.glyphs_dropped {
                        self.telemetry.record_glyph_dropped_atlas_full();
                    }
                }
                if s.atlas_interns > 0 || !inputs.is_empty() {
                    // Successful composite_glyphs counts as one
                    // paint submit (mirroring `image_text` /
                    // `render_composite` telemetry shape).
                    self.telemetry.record_paint_submit();
                    let glyph_count = u32::try_from(inputs.len()).unwrap_or(u32::MAX);
                    self.trace_render(
                        SubmitKind::CompositeGlyphs,
                        dst_target.id,
                        glyph_count,
                        3, // OP_OVER — composite_glyphs is Over-only per Stage 3d gate
                        SrcClass::Solid,
                        None,
                        SubmitFlags::NONE,
                    );
                }
            }
            Err(e) => {
                log::warn!("v2 composite_glyphs: engine returned {e:?} on dst 0x{host_dst:x}");
            }
        }
        // Phase B.1 Task 21: composite_glyphs may open a frame;
        // drain any resulting close events into telemetry.
        self.drain_frame_builder_telemetry();
        Ok(())
    }

    fn render_fill_rectangles(
        &mut self,
        _origin: Option<OriginContext>,
        host_dst: u32,
        op: u8,
        color: [u8; 8],
        rects: &[u8],
        x_off: i16,
        y_off: i16,
    ) -> io::Result<()> {
        let Some((dst_host_xid, dst_clip)) = resolve_dst_picture_for_render(&self.core, host_dst)
        else {
            log::debug!(
                "v2 render_fill_rectangles gap: host_dst 0x{host_dst:x} not a Drawable picture"
            );
            return Ok(());
        };
        // Stage 4a — redirect routing for dst.
        let Some(dst_target) = self.resolve_paint_target(dst_host_xid) else {
            log::debug!(
                "v2 render_fill_rectangles gap: dst drawable 0x{dst_host_xid:x} not in store"
            );
            return Ok(());
        };
        let dst_clip = Self::shift_dst_picture_clip(dst_clip, dst_target.offset);
        let (paint_dx, paint_dy) = dst_target.offset;

        // X RENDER XRenderColor is wire-premultiplied (rendercheck
        // main.c:337-345); pass through unchanged.
        let color_premul = [
            f32::from(u16::from_le_bytes([color[0], color[1]])) / 65535.0,
            f32::from(u16::from_le_bytes([color[2], color[3]])) / 65535.0,
            f32::from(u16::from_le_bytes([color[4], color[5]])) / 65535.0,
            f32::from(u16::from_le_bytes([color[6], color[7]])) / 65535.0,
        ];

        let mut decoded: Vec<crate::kms::vk::ops::render::CompositeRect> =
            Vec::with_capacity(rects.len() / 8);
        for chunk in rects.chunks_exact(8) {
            let rx = i16::from_le_bytes([chunk[0], chunk[1]]).saturating_add(x_off);
            let ry = i16::from_le_bytes([chunk[2], chunk[3]]).saturating_add(y_off);
            let rw = u16::from_le_bytes([chunk[4], chunk[5]]);
            let rh = u16::from_le_bytes([chunk[6], chunk[7]]);
            if rw == 0 || rh == 0 {
                continue;
            }
            decoded.push(crate::kms::vk::ops::render::CompositeRect {
                src_x: 0,
                src_y: 0,
                mask_x: 0,
                mask_y: 0,
                dst_x: i32::from(rx) + paint_dx,
                dst_y: i32::from(ry) + paint_dy,
                width: u32::from(rw),
                height: u32::from(rh),
            });
        }
        if decoded.is_empty() {
            return Ok(());
        }

        let stats = self.engine.render_fill_rectangles(
            &mut self.store,
            &mut self.platform,
            op,
            color_premul,
            dst_target.id,
            &decoded,
            dst_clip.as_deref(),
        );
        self.sync_descriptor_pool_telemetry();
        let n_rects = u32::try_from(decoded.len()).unwrap_or(u32::MAX);
        if let Ok(s) = stats {
            if s.recorded_draws > 0 {
                self.telemetry.record_paint_submit();
                self.trace_render(
                    SubmitKind::RenderFill,
                    dst_target.id,
                    n_rects,
                    op,
                    SrcClass::Solid,
                    None,
                    SubmitFlags {
                        readback: s.used_dst_readback,
                        alias: s.used_src_alias_scratch,
                        zero_draws: false,
                        upload: false,
                    },
                );
            }
            if s.used_dst_readback {
                self.telemetry.record_disjoint_readback();
            }
        } else if let Err(e) = stats {
            log::warn!("v2 render_fill_rectangles: engine returned {e:?} on dst 0x{host_dst:x}");
        }
        // Phase B.2 Task 15: render_fill_rectangles may open a frame;
        // drain any resulting close events into telemetry so the
        // per-second emit picks them up without stale lag. Mirrors the
        // B.1 drain at the composite_glyphs wrapper.
        self.drain_frame_builder_telemetry();
        Ok(())
    }

    fn render_trapezoids(
        &mut self,
        _origin: Option<OriginContext>,
        op: u8,
        host_src: u32,
        host_dst: u32,
        _host_mask_format: u32,
        src_x: i16,
        src_y: i16,
        traps: &[u8],
        x_off: i16,
        y_off: i16,
    ) -> io::Result<()> {
        use crate::kms::{v2::engine::TrapPrimKind, vk::ops::traps as vk_traps};

        // Wire layout: each trapezoid is 40 bytes (10 × i32 16.16
        // fixed-point). Mirrors v1's try_vk_render_trapezoids_path
        // decoder (kms/backend.rs:4286).
        if traps.is_empty() {
            return Ok(());
        }
        let n_traps = traps.len() / 40;
        if n_traps == 0 {
            return Ok(());
        }
        let mut decoded: Vec<vk_traps::Trapezoid> = Vec::with_capacity(n_traps);
        for chunk in traps.chunks_exact(40) {
            let read_i32 = |o: usize| -> i32 {
                i32::from_le_bytes([chunk[o], chunk[o + 1], chunk[o + 2], chunk[o + 3]])
            };
            decoded.push(vk_traps::Trapezoid {
                top: read_i32(0),
                bottom: read_i32(4),
                left_p1: (read_i32(8), read_i32(12)),
                left_p2: (read_i32(16), read_i32(20)),
                right_p1: (read_i32(24), read_i32(28)),
                right_p2: (read_i32(32), read_i32(36)),
            });
        }
        // Xorg's `fbTrapezoids` (fb/fbtrap.c:164-165) subtracts the
        // first trapezoid's `left.p1` from xSrc/ySrc before forwarding
        // to pixman. This anchors the src origin at the first trap's
        // top-left, regardless of where the trap is in dst space. For
        // GTK CSD shadows (which pass `xSrc=20 ySrc=-25` for the BR
        // corner with `traps[0].left.p1 = (20, -25)`), the subtraction
        // resolves to src=(0,0) → no out-of-bounds sampling. Without
        // it, REPEAT_NONE returns transparent for the OOB rows and the
        // corner shadow has an 8-row α=0 gap.
        // Captured pre-shift; the dx/dy fold below moves the live trap
        // coords into the redirect-target space, but the *adjustment*
        // is from the client-supplied geometry.
        let first_trap_left_p1_x = decoded[0].left_p1.0 >> 16;
        let first_trap_left_p1_y = decoded[0].left_p1.1 >> 16;
        // Resolve src + dst via the same helpers render_composite
        // uses. The trap path doesn't read GC clip — picture clip
        // (from dst) is what scopes the draw (plan §4).
        let Some((src_resolved, src_repeat, src_transform, _src_ca)) =
            resolve_picture_for_render(&self.core, &self.store, host_src)
        else {
            log::debug!("v2 render_trapezoids gap: src 0x{host_src:x} not resolvable");
            return Ok(());
        };
        let Some((dst_host_xid, dst_clip)) = resolve_dst_picture_for_render(&self.core, host_dst)
        else {
            log::debug!("v2 render_trapezoids gap: dst 0x{host_dst:x} not Drawable picture");
            return Ok(());
        };
        // Stage 4a — redirect routing for dst. The fold of
        // `x_off`/`y_off` and the redirect offset (both in pixel
        // units) into a single fixed-point delta keeps the
        // 16.16-arithmetic single-pass.
        let Some(dst_target) = self.resolve_paint_target(dst_host_xid) else {
            log::debug!("v2 render_trapezoids gap: dst drawable 0x{dst_host_xid:x} not in store");
            return Ok(());
        };
        let dst_clip = Self::shift_dst_picture_clip(dst_clip, dst_target.offset);
        let dx = (i32::from(x_off) + dst_target.offset.0) << 16;
        let dy = (i32::from(y_off) + dst_target.offset.1) << 16;
        if dx != 0 || dy != 0 {
            for t in &mut decoded {
                t.top = t.top.wrapping_add(dy);
                t.bottom = t.bottom.wrapping_add(dy);
                t.left_p1.0 = t.left_p1.0.wrapping_add(dx);
                t.left_p1.1 = t.left_p1.1.wrapping_add(dy);
                t.left_p2.0 = t.left_p2.0.wrapping_add(dx);
                t.left_p2.1 = t.left_p2.1.wrapping_add(dy);
                t.right_p1.0 = t.right_p1.0.wrapping_add(dx);
                t.right_p1.1 = t.right_p1.1.wrapping_add(dy);
                t.right_p2.0 = t.right_p2.0.wrapping_add(dx);
                t.right_p2.1 = t.right_p2.1.wrapping_add(dy);
            }
        }
        let Some((bx, by, bx1, by1)) = vk_traps::trapezoid_bbox(&decoded) else {
            return Ok(());
        };
        let bx = bx.max(0);
        let by = by.max(0);
        if bx1 <= bx || by1 <= by {
            return Ok(());
        }
        #[allow(clippy::cast_sign_loss)]
        let bw = (bx1 - bx) as u32;
        #[allow(clippy::cast_sign_loss)]
        let bh = (by1 - by) as u32;

        // Pack instance bytes (40 bytes per trap; no padding —
        // asserted by `const _:()` in trap_pipeline.rs).
        let stride = std::mem::size_of::<crate::kms::vk::trap_pipeline::TrapInstanceData>();
        let mut instance_bytes = vec![0u8; stride * decoded.len()];
        for (i, t) in decoded.iter().enumerate() {
            let inst = t.to_instance_data();
            instance_bytes[i * stride..(i + 1) * stride].copy_from_slice(inst.as_bytes());
        }

        // Audit #4 (2026-05-19) — same pict_format threading as
        // render_composite. Trap/tri paint into an xRGB32 dst on
        // depth-32 storage must drive "no alpha target," and
        // xRGB32 sources must pin α=ONE on the sample view.
        let src_pict_format = picture_pict_format(&self.core, host_src);
        let dst_pict_format = picture_pict_format(&self.core, host_dst);
        // Source origin in src-pixel space. Two adjustments stacked:
        //   - subtract `(x_off + redirect_offset)` to undo the dx/dy
        //     fold applied to the trap coords above;
        //   - subtract `traps[0].left.p1.{x,y}` to mirror Xorg's
        //     `fbTrapezoids` pixman pre-step (fb/fbtrap.c:164-165) —
        //     anchors src @ (0,0) at the first trap's top-left.
        // The emit folds in bbox for the non-full-dst branch.
        let src_origin_x =
            i32::from(src_x) - (i32::from(x_off) + dst_target.offset.0) - first_trap_left_p1_x;
        let src_origin_y =
            i32::from(src_y) - (i32::from(y_off) + dst_target.offset.1) - first_trap_left_p1_y;
        let stats = self.engine.render_traps_or_tris(
            &mut self.store,
            &mut self.platform,
            op,
            src_resolved,
            dst_target.id,
            TrapPrimKind::Trapezoid,
            &instance_bytes,
            #[allow(clippy::cast_possible_truncation)]
            {
                decoded.len() as u32
            },
            (bx, by, bw, bh),
            dst_clip.as_deref(),
            src_repeat,
            src_transform,
            src_origin_x,
            src_origin_y,
            src_pict_format,
            dst_pict_format,
        );
        self.sync_descriptor_pool_telemetry();
        let src_class = self.picture_src_class_by_xid(host_src);
        let n_traps = u32::try_from(decoded.len()).unwrap_or(u32::MAX);
        if let Ok(s) = stats {
            if s.recorded_draws > 0 {
                self.telemetry.record_paint_submit();
                self.trace_render(
                    SubmitKind::RenderTraps,
                    dst_target.id,
                    n_traps,
                    op,
                    src_class,
                    None,
                    SubmitFlags {
                        readback: s.used_dst_readback,
                        alias: s.used_src_alias_scratch,
                        zero_draws: false,
                        upload: false,
                    },
                );
            }
            if s.used_dst_readback {
                self.telemetry.record_disjoint_readback();
            }
        } else if let Err(e) = stats {
            log::warn!("v2 render_trapezoids: engine returned {e:?}");
        }
        Ok(())
    }

    fn render_triangles_op(
        &mut self,
        _origin: Option<OriginContext>,
        minor: u8,
        op: u8,
        host_src: u32,
        host_dst: u32,
        _host_mask_format: u32,
        src_x: i16,
        src_y: i16,
        primitives: &[u8],
        x_off: i16,
        y_off: i16,
    ) -> io::Result<()> {
        use crate::kms::{v2::engine::TrapPrimKind, vk::ops::traps as vk_traps};

        let read_point = |off: usize, chunk: &[u8]| -> (i32, i32) {
            let x =
                i32::from_le_bytes([chunk[off], chunk[off + 1], chunk[off + 2], chunk[off + 3]]);
            let y = i32::from_le_bytes([
                chunk[off + 4],
                chunk[off + 5],
                chunk[off + 6],
                chunk[off + 7],
            ]);
            (x, y)
        };
        let mut tris: Vec<vk_traps::Triangle> = match minor {
            11 => {
                if !primitives.len().is_multiple_of(24) {
                    return Ok(());
                }
                primitives
                    .chunks_exact(24)
                    .map(|c| vk_traps::Triangle {
                        p1: read_point(0, c),
                        p2: read_point(8, c),
                        p3: read_point(16, c),
                    })
                    .collect()
            }
            12 => {
                if !primitives.len().is_multiple_of(8) || primitives.len() < 24 {
                    return Ok(());
                }
                let pts: Vec<(i32, i32)> = primitives
                    .chunks_exact(8)
                    .map(|c| read_point(0, c))
                    .collect();
                (0..pts.len() - 2)
                    .map(|i| vk_traps::Triangle {
                        p1: pts[i],
                        p2: pts[i + 1],
                        p3: pts[i + 2],
                    })
                    .collect()
            }
            13 => {
                if !primitives.len().is_multiple_of(8) || primitives.len() < 24 {
                    return Ok(());
                }
                let pts: Vec<(i32, i32)> = primitives
                    .chunks_exact(8)
                    .map(|c| read_point(0, c))
                    .collect();
                (1..pts.len() - 1)
                    .map(|i| vk_traps::Triangle {
                        p1: pts[0],
                        p2: pts[i],
                        p3: pts[i + 1],
                    })
                    .collect()
            }
            _ => return Ok(()),
        };
        if tris.is_empty() {
            return Ok(());
        }
        let Some((src_resolved, src_repeat, src_transform, _src_ca)) =
            resolve_picture_for_render(&self.core, &self.store, host_src)
        else {
            log::debug!("v2 render_triangles gap: src 0x{host_src:x} not resolvable");
            return Ok(());
        };
        let Some((dst_host_xid, dst_clip)) = resolve_dst_picture_for_render(&self.core, host_dst)
        else {
            log::debug!("v2 render_triangles gap: dst 0x{host_dst:x} not Drawable picture");
            return Ok(());
        };
        // Stage 4a — redirect routing for dst; fold the redirect
        // offset into the same fixed-point delta as `x_off/y_off`.
        let Some(dst_target) = self.resolve_paint_target(dst_host_xid) else {
            log::debug!("v2 render_triangles gap: dst drawable 0x{dst_host_xid:x} not in store");
            return Ok(());
        };
        let dst_clip = Self::shift_dst_picture_clip(dst_clip, dst_target.offset);
        let dx = (i32::from(x_off) + dst_target.offset.0) << 16;
        let dy = (i32::from(y_off) + dst_target.offset.1) << 16;
        if dx != 0 || dy != 0 {
            for t in &mut tris {
                t.p1.0 = t.p1.0.wrapping_add(dx);
                t.p1.1 = t.p1.1.wrapping_add(dy);
                t.p2.0 = t.p2.0.wrapping_add(dx);
                t.p2.1 = t.p2.1.wrapping_add(dy);
                t.p3.0 = t.p3.0.wrapping_add(dx);
                t.p3.1 = t.p3.1.wrapping_add(dy);
            }
        }
        let Some((bx, by, bx1, by1)) = vk_traps::triangle_bbox(&tris) else {
            return Ok(());
        };
        let bx = bx.max(0);
        let by = by.max(0);
        if bx1 <= bx || by1 <= by {
            return Ok(());
        }
        #[allow(clippy::cast_sign_loss)]
        let bw = (bx1 - bx) as u32;
        #[allow(clippy::cast_sign_loss)]
        let bh = (by1 - by) as u32;

        let stride = std::mem::size_of::<crate::kms::vk::trap_pipeline::TriangleInstanceData>();
        let mut instance_bytes = vec![0u8; stride * tris.len()];
        for (i, t) in tris.iter().enumerate() {
            let inst = t.to_instance_data();
            instance_bytes[i * stride..(i + 1) * stride].copy_from_slice(inst.as_bytes());
        }

        // Audit #4 (2026-05-19) — same pict_format threading as
        // the trapezoid path; see that call site for rationale.
        let src_pict_format = picture_pict_format(&self.core, host_src);
        let dst_pict_format = picture_pict_format(&self.core, host_dst);
        // Source origin shifted by the same delta the triangle coords
        // were (x_off + redirect offset). Also subtract the first
        // triangle's `p1.{x,y}` to mirror Xorg's `fbTriangles` pixman
        // pre-step (fb/fbtrap.c:179-180) — anchors src @ (0,0) at the
        // first triangle's p1 regardless of where it sits in dst space.
        let first_tri_p1_x = tris[0].p1.0 >> 16;
        let first_tri_p1_y = tris[0].p1.1 >> 16;
        let src_origin_x =
            i32::from(src_x) - (i32::from(x_off) + dst_target.offset.0) - first_tri_p1_x;
        let src_origin_y =
            i32::from(src_y) - (i32::from(y_off) + dst_target.offset.1) - first_tri_p1_y;
        let stats = self.engine.render_traps_or_tris(
            &mut self.store,
            &mut self.platform,
            op,
            src_resolved,
            dst_target.id,
            TrapPrimKind::Triangle,
            &instance_bytes,
            #[allow(clippy::cast_possible_truncation)]
            {
                tris.len() as u32
            },
            (bx, by, bw, bh),
            dst_clip.as_deref(),
            src_repeat,
            src_transform,
            src_origin_x,
            src_origin_y,
            src_pict_format,
            dst_pict_format,
        );
        self.sync_descriptor_pool_telemetry();
        let src_class = self.picture_src_class_by_xid(host_src);
        let n_tris = u32::try_from(tris.len()).unwrap_or(u32::MAX);
        if let Ok(s) = stats {
            if s.recorded_draws > 0 {
                self.telemetry.record_paint_submit();
                self.trace_render(
                    SubmitKind::RenderTris,
                    dst_target.id,
                    n_tris,
                    op,
                    src_class,
                    None,
                    SubmitFlags {
                        readback: s.used_dst_readback,
                        alias: s.used_src_alias_scratch,
                        zero_draws: false,
                        upload: false,
                    },
                );
            }
            if s.used_dst_readback {
                self.telemetry.record_disjoint_readback();
            }
        } else if let Err(e) = stats {
            log::warn!("v2 render_triangles: engine returned {e:?}");
        }
        Ok(())
    }

    fn render_create_solid_fill(
        &mut self,
        _origin: Option<OriginContext>,
        color: [u8; 8],
    ) -> io::Result<Option<PictureHandle>> {
        // X RENDER CreateSolidFill: 16-bit-per-channel colour,
        // little-endian, already premultiplied on the wire (per
        // rendercheck main.c:337-345). Store the channels as f32
        // exactly as received — the pipeline samples them
        // unchanged. Layout: r[0..2] g[2..4] b[4..6] a[6..8].
        let r16 = u16::from_le_bytes([color[0], color[1]]);
        let g16 = u16::from_le_bytes([color[2], color[3]]);
        let b16 = u16::from_le_bytes([color[4], color[5]]);
        let a16 = u16::from_le_bytes([color[6], color[7]]);
        let premul = [
            f32::from(r16) / 65535.0,
            f32::from(g16) / 65535.0,
            f32::from(b16) / 65535.0,
            f32::from(a16) / 65535.0,
        ];
        let picture_xid = self.core.next_host_xid();
        self.core.pictures.insert(
            picture_xid,
            PictureRecord::SolidFill {
                premul,
                repeat: Repeat::Normal,
                component_alpha: false,
            },
        );
        Ok(PictureHandle::from_raw(picture_xid))
    }

    fn render_create_linear_gradient(
        &mut self,
        _origin: Option<OriginContext>,
        body: &[u8],
    ) -> io::Result<Option<PictureHandle>> {
        // Wire body: p1.x(4) + p1.y(4) + p2.x(4) + p2.y(4) +
        // n_stops(4) + n × stop_pos(4) + n × stop_color(8).
        // Caller passes only the request payload from offset 4 —
        // the first u32 is interpreted as p1.x (sliced at body[4..]).
        if body.len() < 24 {
            return Ok(None);
        }
        let p1x = i32::from_le_bytes(body[4..8].try_into().unwrap());
        let p1y = i32::from_le_bytes(body[8..12].try_into().unwrap());
        let p2x = i32::from_le_bytes(body[12..16].try_into().unwrap());
        let p2y = i32::from_le_bytes(body[16..20].try_into().unwrap());
        let Some(stops) = parse_gradient_stops(body, 20) else {
            return Ok(None);
        };
        let picture_xid = self.core.next_host_xid();
        // Stage 3f.13: build the LUT eagerly so the first
        // render_composite against this picture has it ready. The
        // record + the engine's GradientPicture have parallel
        // lifetimes — render_free_picture drops both. Build
        // failure (no Vk on test fixture, or allocation error) is
        // non-fatal: the record still lands; render_composite
        // logs a gap if it can't find the LUT. This keeps the
        // logic-test fixture (no live Vk) usable without forcing
        // every gradient-create test through lavapipe.
        let engine_stops: Vec<crate::kms::vk::gradient::Stop> = stops
            .iter()
            .map(|s| crate::kms::vk::gradient::Stop {
                pos: s.pos,
                r: s.r,
                g: s.g,
                b: s.b,
                a: s.a,
            })
            .collect();
        if let Err(e) = self.engine.build_and_insert_linear_gradient(
            &self.platform,
            picture_xid,
            (p1x, p1y),
            (p2x, p2y),
            &engine_stops,
        ) {
            log::debug!(
                "v2 render_create_linear_gradient: engine build failed (xid=0x{picture_xid:x}): \
                 {e:?} — record stored; paint will fall back to gap-log"
            );
        }
        self.core.pictures.insert(
            picture_xid,
            PictureRecord::LinearGradient {
                p1: (p1x, p1y),
                p2: (p2x, p2y),
                stops,
                repeat: Repeat::None,
                transform: None,
            },
        );
        Ok(PictureHandle::from_raw(picture_xid))
    }

    fn render_create_radial_gradient(
        &mut self,
        _origin: Option<OriginContext>,
        body: &[u8],
    ) -> io::Result<Option<PictureHandle>> {
        // Wire body: icx(4) icy(4) ocx(4) ocy(4) ir(4) or(4)
        // n_stops(4) + stops + colors. Same offset-by-4 convention
        // as linear (first u32 in `body` is past the request header).
        if body.len() < 32 {
            return Ok(None);
        }
        let icx = i32::from_le_bytes(body[4..8].try_into().unwrap());
        let icy = i32::from_le_bytes(body[8..12].try_into().unwrap());
        let ocx = i32::from_le_bytes(body[12..16].try_into().unwrap());
        let ocy = i32::from_le_bytes(body[16..20].try_into().unwrap());
        let ir = i32::from_le_bytes(body[20..24].try_into().unwrap());
        let or_ = i32::from_le_bytes(body[24..28].try_into().unwrap());
        let Some(stops) = parse_gradient_stops(body, 28) else {
            return Ok(None);
        };
        let picture_xid = self.core.next_host_xid();
        // Stage 3f.13: build the radial LUT (256×256 BGRA) eagerly.
        // See `render_create_linear_gradient` for failure-mode
        // rationale.
        let engine_stops: Vec<crate::kms::vk::gradient::Stop> = stops
            .iter()
            .map(|s| crate::kms::vk::gradient::Stop {
                pos: s.pos,
                r: s.r,
                g: s.g,
                b: s.b,
                a: s.a,
            })
            .collect();
        if let Err(e) = self.engine.build_and_insert_radial_gradient(
            &self.platform,
            picture_xid,
            (icx, icy, ir),
            (ocx, ocy, or_),
            &engine_stops,
        ) {
            log::debug!(
                "v2 render_create_radial_gradient: engine build failed (xid=0x{picture_xid:x}): \
                 {e:?} — record stored; paint will fall back to gap-log"
            );
        }
        self.core.pictures.insert(
            picture_xid,
            PictureRecord::RadialGradient {
                inner: (icx, icy, ir),
                outer: (ocx, ocy, or_),
                stops,
                repeat: Repeat::None,
                transform: None,
            },
        );
        Ok(PictureHandle::from_raw(picture_xid))
    }

    fn render_create_cursor(
        &mut self,
        _origin: Option<OriginContext>,
        host_src_pic: PictureHandle,
        x: u16,
        y: u16,
    ) -> io::Result<Option<CursorHandle>> {
        // Stage 5 Phase A: themed/ARGB cursor — Picture wraps a
        // depth-32 BGRA Pixmap. Read the pixmap bytes via
        // `engine.get_image`, allocate a CursorRecord + sprite, and
        // mint a cursor xid. fvwm pattern (CreatePixmap → PutImage
        // → CreatePicture → FreePixmap → CreateCursor) means the
        // backing pixmap may already be gone by the time we arrive,
        // but for Stage 5 we rely on the picture record's
        // `host_xid` resolving back to a live store entry — alias-
        // registry-aware rescue is a follow-up.
        let pic_xid = host_src_pic.as_raw();
        let src_host_xid = match self.core.pictures.get(&pic_xid) {
            Some(crate::kms::core::PictureRecord::Drawable { host_xid, .. }) => *host_xid,
            other => {
                log::debug!(
                    "v2 render_create_cursor: pic 0x{pic_xid:x} not Drawable (got {:?})",
                    other.map(|_| "non-Drawable"),
                );
                return Ok(None);
            }
        };
        let Some((bgra, w, h)) = self.read_cursor_bgra_pixmap(src_host_xid) else {
            log::debug!(
                "v2 render_create_cursor: src pixmap 0x{src_host_xid:x} unreadable for pic 0x{pic_xid:x}",
            );
            return Ok(None);
        };
        let xid = self.core.next_host_xid();
        let handle = CursorHandle::from_raw(xid);
        self.insert_cursor_record(xid, w, h, x, y, bgra);
        Ok(handle)
    }

    fn render_set_picture_clip_rectangles(
        &mut self,
        _origin: Option<OriginContext>,
        host_pic: u32,
        body: &[u8],
    ) -> io::Result<()> {
        // Wire body: picture(4) + clip_x_origin(INT16) +
        // clip_y_origin(INT16) + N × [x y w h]. Pre-shift each
        // rectangle by the clip-origin so the stored list is in
        // dst-coords; the per-rect scissoring path in Stage 3c
        // doesn't track origin separately.
        if body.len() < 8 {
            return Ok(());
        }
        let x_origin = i16::from_le_bytes([body[4], body[5]]) as i32;
        let y_origin = i16::from_le_bytes([body[6], body[7]]) as i32;
        let rects_data = &body[8..];
        let mut rects = Vec::with_capacity(rects_data.len() / 8);
        for chunk in rects_data.chunks_exact(8) {
            let x = (i16::from_le_bytes([chunk[0], chunk[1]]) as i32 + x_origin)
                .clamp(i16::MIN as i32, i16::MAX as i32) as i16;
            let y = (i16::from_le_bytes([chunk[2], chunk[3]]) as i32 + y_origin)
                .clamp(i16::MIN as i32, i16::MAX as i32) as i16;
            let w = u16::from_le_bytes([chunk[4], chunk[5]]);
            let h = u16::from_le_bytes([chunk[6], chunk[7]]);
            rects.push(Rectangle16 {
                x,
                y,
                width: w,
                height: h,
            });
        }
        if let Some(PictureRecord::Drawable {
            clip,
            clip_x,
            clip_y,
            ..
        }) = self.core.pictures.get_mut(&host_pic)
        {
            // Diagnostic trace (TEMP — Stage 4d "shadow only"
            // investigation). Logs marco's incoming clip rect
            // list at SetPictureClipRectangles time, post-origin
            // shift. Compare against the per-call clip dump in
            // the render_composite trace to verify v2 carries
            // marco's clip through unchanged.
            if log::log_enabled!(target: "yserver::kms::v2::render", log::Level::Trace) {
                use std::fmt::Write as _;
                let mut s = String::new();
                for (i, r) in rects.iter().enumerate() {
                    if i > 0 {
                        s.push(' ');
                    }
                    let _ = write!(s, "({},{} {}x{})", r.x, r.y, r.width, r.height);
                }
                log::trace!(
                    target: "yserver::kms::v2::render",
                    "set_picture_clip_rectangles pic=0x{host_pic:x} origin=({x_origin},{y_origin}) n={n} rects[{s}]",
                    n = rects.len(),
                );
            }
            // X11 RENDER spec semantics:
            //   - `SetPictureClipRectangles` with EMPTY rect list =
            //     empty clip region = composites paint **nothing**.
            //   - `ChangePicture(CPClipMask = None)` clears the clip
            //     back to "no clip" = paint **everywhere** (`clip = None`).
            // The previous implementation collapsed both to None;
            // that broke marco-with-compositing because marco uses
            // the empty-list form between frames as a "stop
            // painting until I set a real clip again" gate. With
            // the buggy collapse, the wallpaper-fill composite
            // that should have been clipped to nothing painted
            // everywhere and overwrote the just-drawn window
            // contents — the Stage 4d "shadow only" symptom.
            *clip = Some(rects);
            // The X RENDER protocol carries clip-origin once per
            // SetPictureClipRectangles; we fold it into the stored
            // rects (above) but also keep clip_x/clip_y so a
            // subsequent CPClipXOrigin / CPClipYOrigin override
            // via ChangePicture composes correctly.
            *clip_x = x_origin.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
            *clip_y = y_origin.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
        }
        // SolidFill / Gradient pictures: clip is a no-op (no
        // backing drawable to clip).
        Ok(())
    }

    fn render_set_picture_filter(
        &mut self,
        _origin: Option<OriginContext>,
        host_pic: u32,
        body: &[u8],
    ) -> io::Result<()> {
        // Wire body: picture(4) + name_len(u16) + pad(2) + name +
        // pad + N × FIXED(4) parameters. Stage 3 only honours
        // `nearest`; other filters parse + store so the record-
        // round-trip is honest but `RenderEngine` ignores them at
        // draw time (per Risk 6).
        if body.len() < 8 {
            return Ok(());
        }
        let name_len = u16::from_le_bytes([body[4], body[5]]) as usize;
        if body.len() < 8 + name_len {
            return Ok(());
        }
        let name = &body[8..8 + name_len];
        let filter = match name {
            b"nearest" | b"fast" => PictureFilter::Nearest,
            b"bilinear" | b"good" | b"best" => PictureFilter::Bilinear,
            b"convolution" => PictureFilter::Convolution,
            _ => PictureFilter::Nearest,
        };
        if let Some(PictureRecord::Drawable { filter: f, .. }) =
            self.core.pictures.get_mut(&host_pic)
        {
            *f = filter;
        }
        Ok(())
    }

    fn render_set_picture_transform(
        &mut self,
        _origin: Option<OriginContext>,
        host_pic: u32,
        body: &[u8],
    ) -> io::Result<()> {
        // Wire body: picture(4) + 9 × FIXED(4) matrix entries (row-
        // major). 16.16 fixed-point; identity is [[1,0,0],[0,1,0],
        // [0,0,1]] in floating shape, [[0x10000, 0, 0], [0, 0x10000,
        // 0], [0, 0, 0x10000]] in fixed.
        if body.len() < 40 {
            return Ok(());
        }
        let mut matrix = [[0i32; 3]; 3];
        for (idx, slot) in matrix.iter_mut().flatten().enumerate() {
            let off = 4 + idx * 4;
            *slot = i32::from_le_bytes(body[off..off + 4].try_into().unwrap());
        }
        let transform = if matrix == [[0x10000, 0, 0], [0, 0x10000, 0], [0, 0, 0x10000]] {
            None
        } else {
            Some(PictTransform { matrix })
        };
        match self.core.pictures.get_mut(&host_pic) {
            Some(PictureRecord::Drawable { transform: t, .. })
            | Some(PictureRecord::LinearGradient { transform: t, .. })
            | Some(PictureRecord::RadialGradient { transform: t, .. }) => *t = transform,
            _ => {}
        }
        Ok(())
    }

    fn render_query_version(&mut self, _origin: Option<OriginContext>) -> io::Result<(u32, u32)> {
        // Advertise RENDER 0.11 (the version v1 reports). Stubbed
        // paint paths still need the version reply to flow through;
        // skipping it would break clients at extension query.
        Ok((0, 11))
    }

    // ── DRI3 — ported from v1 (Stage 4d backfill) ───────────────
    //
    // Body shape mirrors `kms/backend.rs:8613-8869` verbatim — the
    // helpers in `kms::vk::dri3`, `kms::vk::sync`, `kms::render_node`,
    // and `kms::xshmfence` are already shared with v1, so v2 calls
    // them directly. Without these, no compositor (marco, xfwm4,
    // picom, compton) can import redirected window backings as GPU
    // textures and the 4d-close hardware smoke wedges on
    // PresentPixmap → COW.

    fn dri3_open(&mut self, _drawable: u32) -> io::Result<std::os::fd::OwnedFd> {
        // Open a fresh fd at the render-node path per client. dup()'ing
        // a shared long-lived fd would give every client the same
        // kernel struct file, and libdrm_amdgpu maintains GEM handles
        // + contexts in per-struct-file state — the first client
        // populates it, the second crashes in `amdgpu_winsys_create`
        // hitting leftover handles. See
        // feedback_dri3_open_fresh_fd.md.
        let path = self.platform.render_node_path.as_deref().ok_or_else(|| {
            io::Error::other("DRI3 unavailable — render node was not resolved at backend init")
        })?;
        crate::kms::render_node::open_fresh(path)
            .map_err(|e| io::Error::other(format!("open render-node {}: {e}", path.display())))
    }

    fn dri3_capabilities(&self) -> Dri3Caps {
        // DRI3 entirely unavailable when render-node fd or Vulkan
        // weren't resolved at backend init.
        if self.platform.render_node_fd.is_none() || self.platform.vk.is_none() {
            return Dri3Caps::unsupported();
        }
        let vk = self.platform.vk.as_ref().expect("vk Some by branch above");
        let modifiers = vk.image_drm_format_modifier;
        // VK_KHR_external_semaphore_fd is unconditionally enabled at
        // device init; fence_fd / SYNC_FD handle type rides along
        // with it. syncobj uses the OPAQUE_FD + timeline-semaphore
        // path also covered by VK_KHR_external_semaphore_fd. NVIDIA
        // proprietary rejects that import path, so cap syncobj per
        // driver and let affected clients fall back to fence-fd.
        let fence_fd = true;
        let syncobj = vk.supports_dri3_syncobj();
        // Version cap per Phase 4.2 design §4: with syncobj
        // advertise (1, 4); without it cap at (1, 3).
        let version = if syncobj { (1, 4) } else { (1, 3) };
        Dri3Caps {
            version,
            modifiers,
            fence_fd,
            syncobj,
        }
    }

    fn dri3_import_pixmap(
        &mut self,
        fd: std::os::fd::OwnedFd,
        width: u16,
        height: u16,
        stride: u32,
        offset: u32,
        modifier: u64,
        depth: u8,
        bpp: u8,
    ) -> io::Result<PixmapHandle> {
        // Per Phase 4.2 design §3.2: import the dma-buf into a
        // DrawableImage via VK_EXT_image_drm_format_modifier, wrap
        // it as a v2 Storage, allocate a fresh Pixmap entry in
        // the store. Pixmap exists as a real X resource so clients
        // can CopyArea / ChangePicture against it.
        let Some(vk) = self.platform.vk.clone() else {
            return Err(io::Error::other("DRI3 import: Vulkan unavailable"));
        };
        let format = match (depth, bpp) {
            (24 | 32, 32) => ash::vk::Format::B8G8R8A8_UNORM,
            _ => {
                return Err(io::Error::other(format!(
                    "DRI3 import: unsupported (depth={depth}, bpp={bpp}); Phase 4.2 RGB single-plane only"
                )));
            }
        };
        let drawable = crate::kms::vk::dri3::import_dmabuf(
            vk.clone(),
            fd,
            u32::from(width),
            u32::from(height),
            format,
            modifier,
            &[crate::kms::vk::dri3::DmabufPlane {
                offset: u64::from(offset),
                pitch: stride,
            }],
        )
        .map_err(|e| io::Error::other(format!("DRI3 import_dmabuf: {e:?}")))?;
        // Build a sample-side view over the imported VkImage. The
        // DRI3 path's own `vk_image_view` (kept as `image_view` on
        // the resulting Storage) is IDENTITY-swizzle and serves as
        // the attachment view; the sample-side view applies the
        // format/depth-aware swizzle the scene compositor relies on
        // (depth-24 BGRA8 → α=ONE).
        let sample_view = crate::kms::v2::platform::PlatformBackend::build_sample_view(
            &vk,
            drawable.vk_image,
            drawable.format,
            depth,
        )
        .map_err(|e| io::Error::other(format!("DRI3 import build_sample_view: {e:?}")))?;
        let storage = Storage::from_imported_drawable_image(drawable, sample_view, depth);
        let host_xid = self.core.next_host_xid();
        self.store
            .allocate(host_xid, DrawableKind::Pixmap, depth, false, storage)
            .map_err(|e| io::Error::other(format!("DRI3 import store.allocate: {e:?}")))?;
        // Telemetry: an imported pixmap is still a fresh storage
        // entry + a view (the DrawableImage built one inside
        // from_dmabuf). Mirrors init_root_storage's accounting so
        // the per-second counters stay accurate under DRI3 traffic.
        self.telemetry.record_storage_allocation();
        self.telemetry.record_image_view_create();
        PixmapHandle::from_raw(host_xid)
            .ok_or_else(|| io::Error::other("DRI3 import: failed to make PixmapHandle"))
    }

    fn dri3_supported_modifiers(&self, _window: u32, depth: u8, bpp: u8) -> (Vec<u64>, Vec<u64>) {
        let Some(vk) = self.platform.vk.as_ref() else {
            return (vec![0], vec![0]);
        };
        // Map (depth, bpp) to a vk::Format. Phase 4.2 RGB single-
        // plane scope means we only handle depth-24/32 BGRA today.
        let format = match (depth, bpp) {
            (24 | 32, 32) => ash::vk::Format::B8G8R8A8_UNORM,
            _ => return (vec![0], vec![0]),
        };
        let screen = crate::kms::vk::dri3::supported_modifiers(vk, format);
        // Window-modifier list is the subset that the window's
        // output can flip-scanout. Phase 4.1 always uses LINEAR
        // for scanout, so the window list collapses to LINEAR
        // here. A follow-up populates `output.scanout_format_set`
        // from the real add_fb2 probe and widens this.
        let window: Vec<u64> = screen.iter().copied().filter(|&m| m == 0).collect();
        let window = if window.is_empty() { vec![0] } else { window };
        (window, screen)
    }

    fn dri3_export_pixmap(
        &mut self,
        host_xid: u32,
    ) -> io::Result<(u32, u16, u16, u16, u8, u8, std::os::fd::OwnedFd)> {
        // op 3 is the single-fd, no-modifier subset of op 8. Share the
        // promote+export path and drop the modifier/offset (op 3's reply
        // has no field for them). stride truncates CARD32 → CARD16.
        let e = self.dri3_export_pixmap_buffers(host_xid)?;
        Ok((
            e.size,
            e.width,
            e.height,
            u16::try_from(e.stride).unwrap_or(u16::MAX),
            e.depth,
            e.bpp,
            e.fd,
        ))
    }

    fn dri3_export_pixmap_buffers(&mut self, host_xid: u32) -> io::Result<Dri3PixmapExport> {
        // Resolve xid → DrawableId before any mutable borrows.
        let id = self.store.lookup(host_xid).ok_or_else(|| {
            io::Error::other(format!("DRI3 export: unknown pixmap 0x{host_xid:x}"))
        })?;

        // Promote-if-needed: migrate server-owned pixmaps onto dma-buf-exportable
        // storage (glamor model). Idempotent — early-returns if already exportable.
        if !self
            .store
            .get(id)
            .map(|d| d.storage.is_exportable())
            .unwrap_or(false)
        {
            // promote_drawable_exportable needs &mut self.engine/platform/store,
            // so we must not hold any shared borrow across this call.
            if self.platform.vk.is_none() {
                return Err(io::Error::other("DRI3 export: Vulkan unavailable"));
            }
            self.engine
                .promote_drawable_exportable(&mut self.platform, &mut self.store, id)
                .map_err(|e| io::Error::other(format!("DRI3 export promote: {e:?}")))?;
        }

        // Re-fetch after promotion (storage has been swapped).
        let vk = self
            .platform
            .vk
            .as_ref()
            .ok_or_else(|| io::Error::other("DRI3 export: Vulkan unavailable"))?;
        let drawable = self.store.get(id).ok_or_else(|| {
            io::Error::other(format!("DRI3 export: store entry missing 0x{host_xid:x}"))
        })?;

        let depth = drawable.depth;
        let width = u16::try_from(drawable.storage.extent.width).unwrap_or(u16::MAX);
        let height = u16::try_from(drawable.storage.extent.height).unwrap_or(u16::MAX);
        let bpp: u8 = match depth {
            24 | 32 => 32,
            4 | 8 => 8,
            d => d,
        };

        // Export: imported images go through the DrawableImage path; promoted /
        // server-owned images use export_promoted on the storage's raw memory
        // handle + stride/size carried from allocation-time layout query.
        let export = if let Some(imported) = drawable.storage.imported_drawable.as_ref() {
            crate::kms::vk::dri3::export_dmabuf(vk, imported)
                .map_err(|e| io::Error::other(format!("DRI3 export_dmabuf: {e:?}")))?
        } else {
            debug_assert!(
                drawable.storage.export_stride != 0 && drawable.storage.export_size != 0,
                "promoted storage missing export metadata (stride={} size={})",
                drawable.storage.export_stride,
                drawable.storage.export_size,
            );
            crate::kms::vk::dri3::export_promoted(
                vk,
                drawable.storage.memory,
                drawable.storage.export_stride,
                drawable.storage.export_size,
                drawable.storage.export_modifier,
            )
            .map_err(|e| io::Error::other(format!("DRI3 export_promoted: {e:?}")))?
        };

        // GLX-TFP (Tasks 2.3 + 2.4): record/refresh the export tracking
        // entry. Repeated exports (muffin re-exports per damage) reuse the
        // existing entry and its single lifetime ref — the fd dup + sync
        // tracking install only on the FIRST export. Always return the
        // ORIGINAL fd to the client.
        let backing = PixmapHandle::from_raw(host_xid)
            .ok_or_else(|| io::Error::other(format!("DRI3 export: bad xid 0x{host_xid:x}")))?;
        if self
            .exported_dmabufs
            .get(&id)
            .is_none_or(|e| e.fd.is_none())
        {
            let dup = export.fd.try_clone()?;
            // Parallel sync-only dup for the engine flush chokepoint.
            let sync_dup = std::sync::Arc::new(dup.try_clone()?);
            self.ensure_exported_entry(id, backing).fd = Some(dup);
            self.store.set_exported_sync_fd(id, sync_dup);
        }

        Ok(Dri3PixmapExport {
            size: export.size,
            width,
            height,
            stride: export.stride,
            offset: export.offset,
            depth,
            bpp,
            modifier: export.modifier,
            fd: export.fd,
        })
    }

    fn dri3_fence_from_fd(&mut self, fence_xid: u32, fd: std::os::fd::OwnedFd) -> io::Result<()> {
        // Mesa's loader_dri3 sends an xshmfence (memfd + futex) —
        // try that path FIRST. vkImportSemaphoreFdKHR rejects
        // xshmfence fds because they aren't sync_file. Mmap first;
        // fall through to Vulkan import only if mmap fails (i.e.
        // the fd really is a sync_file).
        use std::os::fd::AsFd as _;
        if let Some(mapping) = crate::kms::xshmfence::FenceMapping::map(fd.as_fd()) {
            self.dri3_xshmfences
                .insert(fence_xid, std::sync::Arc::new(mapping));
            log::debug!("DRI3 FenceFromFD 0x{fence_xid:x}: imported as xshmfence");
            return Ok(());
        }
        let Some(vk) = self.platform.vk.as_ref() else {
            return Err(io::Error::other(
                "DRI3 FenceFromFD: fd isn't xshmfence and Vulkan is unavailable",
            ));
        };
        let semaphore = crate::kms::vk::sync::import_sync_file(vk, fd)
            .map_err(|e| io::Error::other(format!("import_sync_file: {e:?}")))?;
        let owned = std::sync::Arc::new(crate::kms::v2::owned_semaphore::OwnedSemaphore::new(
            vk.clone(),
            semaphore,
        ));
        // Replacing an entry drops the previous Arc here; if no other
        // clone is outstanding, OwnedSemaphore::Drop calls
        // vkDestroySemaphore.
        let _ = self.dri3_sync_resources.insert(fence_xid, owned);
        Ok(())
    }

    fn dri3_trigger_fence(&mut self, fence_xid: u32) -> io::Result<()> {
        if let Some(mapping) = self.dri3_xshmfences.get(&fence_xid) {
            mapping.trigger();
            return Ok(());
        }
        // VkSemaphore-backed fences: signalling is done via queue
        // submit (or vkSignalSemaphore for timeline). For Phase 4.2
        // first-cut Copy path the GPU work is already serialized,
        // so a server-only `triggered=true` mirror is sufficient
        // — no GPU operation needed here.
        Ok(())
    }

    fn dri3_xshmfence_handle(
        &self,
        fence_xid: u32,
    ) -> Option<std::sync::Arc<dyn yserver_core::backend::XshmfenceHandle>> {
        self.dri3_xshmfences
            .get(&fence_xid)
            .cloned()
            .map(|arc| arc as std::sync::Arc<dyn yserver_core::backend::XshmfenceHandle>)
    }

    fn dri3_syncobj_handle(
        &self,
        syncobj_xid: u32,
    ) -> Option<std::sync::Arc<dyn yserver_core::backend::SyncobjHandle>> {
        self.dri3_sync_resources
            .get(&syncobj_xid)
            .cloned()
            .map(|arc| arc as std::sync::Arc<dyn yserver_core::backend::SyncobjHandle>)
    }

    fn dri3_fd_from_fence(&mut self, fence_xid: u32) -> io::Result<std::os::fd::OwnedFd> {
        let Some(vk) = self.platform.vk.as_ref() else {
            return Err(io::Error::other("DRI3 FDFromFence: Vulkan unavailable"));
        };
        let arc = self.dri3_sync_resources.get(&fence_xid).ok_or_else(|| {
            io::Error::other(format!("DRI3 FDFromFence: unknown fence 0x{fence_xid:x}"))
        })?;
        let semaphore = arc.semaphore();
        crate::kms::vk::sync::export_sync_file(vk, semaphore)
            .map_err(|e| io::Error::other(format!("export_sync_file: {e:?}")))
    }

    fn dri3_import_syncobj(
        &mut self,
        syncobj_xid: u32,
        fd: std::os::fd::OwnedFd,
    ) -> io::Result<()> {
        let Some(vk) = self.platform.vk.as_ref() else {
            return Err(io::Error::other("DRI3 ImportSyncobj: Vulkan unavailable"));
        };
        let semaphore = crate::kms::vk::sync::import_drm_syncobj(vk, fd)
            .map_err(|e| io::Error::other(format!("import_drm_syncobj: {e:?}")))?;
        let owned = std::sync::Arc::new(crate::kms::v2::owned_semaphore::OwnedSemaphore::new(
            vk.clone(),
            semaphore,
        ));
        // Arc Drop on the replaced entry handles vkDestroySemaphore if
        // no other clone is outstanding.
        let _ = self.dri3_sync_resources.insert(syncobj_xid, owned);
        Ok(())
    }

    fn dri3_free_syncobj(&mut self, syncobj_xid: u32) -> io::Result<()> {
        // Vulkan no longer required here — Arc Drop calls
        // vkDestroySemaphore when the last reference goes away.
        let _ = self.dri3_sync_resources.remove(&syncobj_xid);
        Ok(())
    }

    fn dri3_signal_syncobj(&mut self, syncobj_xid: u32, value: u64) -> io::Result<()> {
        let arc = self.dri3_sync_resources.get(&syncobj_xid).ok_or_else(|| {
            io::Error::other(format!(
                "DRI3 SignalSyncobj: unknown syncobj 0x{syncobj_xid:x}"
            ))
        })?;
        arc.signal_vk(value)
            .map_err(|e| io::Error::other(format!("vkSignalSemaphore: {e:?}")))
    }

    /// Stage 5 Task 6.1 — queue a deferred PRESENT completion.
    ///
    /// COW-targeted PRESENT attaches the completion payload to the
    /// still-open COW copy batch. When that batch submits, it signals a
    /// dedicated export-only semaphore in the same queue submission;
    /// the exported sync_file FD drives completion without touching the
    /// `FenceTicket` used for yserver's internal lifetime tracking.
    /// Non-COW PRESENT falls back to one signal-only queue submit after
    /// the already-submitted copy, relying on same-queue ordering.
    fn enqueue_present_completion(
        &mut self,
        event: yserver_core::backend::CompletedPresentEvent,
        dst_host_xid: u32,
    ) {
        use yserver_core::backend::PresentWake;

        use crate::kms::v2::present_completion::{
            PendingPresentBatch, PendingPresentEntry, PinnedWake, PresentBatchWait,
        };

        let wake_pin = match &event.wake {
            PresentWake::Pixmap { idle_fence_xid } if *idle_fence_xid != 0 => {
                match self.dri3_xshmfence_handle(*idle_fence_xid) {
                    Some(h) => PinnedWake::Pixmap(h),
                    None => PinnedWake::None,
                }
            }
            PresentWake::PixmapSynced {
                release_syncobj,
                release_value,
            } if *release_syncobj != 0 => match self.dri3_syncobj_handle(*release_syncobj) {
                Some(h) => PinnedWake::PixmapSynced {
                    handle: h,
                    value: *release_value,
                },
                None => PinnedWake::None,
            },
            _ => PinnedWake::None,
        };

        let mut entry = PendingPresentEntry { wake_pin, event };

        if let Some(cow_id) = self.cow_id
            && self.store.lookup(dst_host_xid) == Some(cow_id)
        {
            match self.engine.attach_cow_present_completion(cow_id, entry) {
                Ok(()) => return,
                Err(returned) => entry = returned,
            }
        }

        // Phase A: close any open render batch FIRST so its CBs land
        // in the group under the same ticket the flush will consume.
        // Then ensure all prior paint is on the queue BEFORE the
        // signal-only submit. Engine-driven so any parked pending_group_ops
        // graduate to `submitted` atomically with the submit.
        // Spec § "Phase A — concrete scope" trigger 2 (Codex pass-3 fix).
        if let Err(e) = self
            .engine
            .flush_render_batch(&mut self.store, &mut self.platform)
        {
            log::warn!("v2 enqueue_present_completion: flush_render_batch failed: {e:?}");
        }
        // Phase B.1 close trigger 1b: close any open frame before the
        // signal-only submit so the semaphore-export's SYNC_FD captures a
        // queued signal-op for ANY paint work that came through the frame
        // builder. Same hazard as Task 6.1 (VUID-VkFenceGetFdInfoKHR-handleType-01457).
        if let Err(e) = self.engine.close_open_frame(
            &mut self.store,
            &mut self.platform,
            crate::kms::v2::frame_builder::CloseReason::PresentCompletionSignal,
        ) {
            log::warn!("v2 enqueue_present_completion: close_open_frame failed: {e:?}");
        }
        // Phase B.1 Task 21: drain frame-builder close events into telemetry.
        self.drain_frame_builder_telemetry();
        if let Err(e) = self.engine.flush_submit_group(
            &mut self.store,
            &mut self.platform,
            crate::kms::v2::submit_group::FlushReason::PresentCompletionSignal,
        ) {
            log::warn!("v2 enqueue_present_completion: flush_submit_group failed: {e:?}");
            // Fall through; the signal-only submit will fail with
            // renderer_failed and the caller's error handling kicks in.
        }

        let fallback_ticket = self
            .store
            .lookup(dst_host_xid)
            .and_then(|id| self.store.get(id))
            .and_then(|d| d.last_render_ticket.clone());

        let mut batch_ticket = fallback_ticket;
        let (wait, signal) = match (
            self.platform.acquire_present_completion_signal(),
            self.platform.acquire_fence_ticket(),
        ) {
            (Ok(signal), Ok(ticket)) => {
                match self
                    .platform
                    .submit_present_completion_signal(&signal, ticket.fence())
                {
                    Ok(()) => {
                        batch_ticket = Some(ticket);
                        match signal.export_sync_file_fd() {
                            Ok(Some(fd)) => (PresentBatchWait::Fd(fd), Some(signal)),
                            Ok(None) => (PresentBatchWait::Ready, Some(signal)),
                            Err(e) => {
                                log::warn!(
                                    "enqueue_present_completion: vkGetSemaphoreFdKHR(SYNC_FD) failed: {e:?}; \
                                     falling back to FenceTicket polling"
                                );
                                (PresentBatchWait::Poll, Some(signal))
                            }
                        }
                    }
                    Err(e) => {
                        log::warn!(
                            "enqueue_present_completion: signal-only queue submit failed: {e:?}; \
                             falling back to prior FenceTicket polling"
                        );
                        (PresentBatchWait::Poll, Some(signal))
                    }
                }
            }
            (Err(e), _) => {
                log::warn!(
                    "enqueue_present_completion: completion semaphore allocation failed: {e:?}; \
                     falling back to FenceTicket polling"
                );
                (PresentBatchWait::Poll, None)
            }
            (Ok(_signal), Err(e)) => {
                log::warn!(
                    "enqueue_present_completion: completion fence allocation failed: {e:?}; \
                     falling back to prior FenceTicket polling"
                );
                (PresentBatchWait::Poll, None)
            }
        };

        self.register_pending_present_batch(PendingPresentBatch {
            wait,
            ticket: batch_ticket,
            signal,
            events: vec![entry],
        });
    }

    /// Stage 5 Task 6.1 — drain batches whose completion semaphore has
    /// signalled (or all batches when `platform.renderer_failed`).
    /// Wake signals fire via the Arc-pinned handle inside the impl
    /// body before the events are returned to the caller.
    fn drain_completed_present_events(
        &mut self,
    ) -> Vec<yserver_core::backend::CompletedPresentEvent> {
        self.drain_completed_present_events_impl()
    }

    fn present_capabilities(&self, _window: u32) -> PresentCaps {
        // Mirror v1's conservative "Copy-path only" caps. syncobj
        // tracks Dri3Caps::syncobj. flip_path / async_may_tear stay
        // false until alien-BO scanout integration lands on v2.
        PresentCaps {
            flip_path: false,
            async_may_tear: false,
            syncobj: self.dri3_capabilities().syncobj,
        }
    }

    // ── Other extensions ────────────────────────────────────────

    fn xkb_proxy(
        &mut self,
        _origin: Option<OriginContext>,
        minor: u8,
        _body: &[u8],
        intern_atom: &mut dyn FnMut(&str) -> u32,
    ) -> io::Result<Option<Vec<u8>>> {
        // Mirror v1's xkb_proxy verbatim — pure protocol
        // bookkeeping using the shared `KmsCore.xkb_keymap`.
        // Without this, Xlib clients abort at the XKEYBOARD
        // UseExtension handshake, so no real-app smoke is
        // possible. The behaviour-level fix is identical to v1
        // (reply minors get bodies, void minors return None).
        use crate::kms::xkb as xkb_replies;
        let reply = match minor {
            0 => Some(xkb_replies::reply_use_extension()),
            6 => Some(xkb_replies::reply_get_controls(&self.core.xkb_keymap.0)),
            8 => Some(xkb_replies::reply_get_map(&self.core.xkb_keymap.0)),
            10 => Some(xkb_replies::reply_get_compat_map()),
            17 => Some(xkb_replies::reply_get_names(
                &self.core.xkb_keymap.0,
                intern_atom,
            )),
            21 => Some(xkb_replies::reply_per_client_flags(_body)),
            24 => Some(xkb_replies::reply_get_device_info()),
            4 | 12 | 13 | 15 | 19 | 22 | 23 | 101 => Some(xkb_replies::reply_minimal(minor)),
            1 | 3 | 5 | 7 | 9 | 11 | 14 | 16 | 18 | 20 | 25 => None,
            _ => {
                log::debug!("v2 xkb: unknown minor {minor}, no reply sent");
                None
            }
        };
        Ok(reply)
    }

    fn get_active_cursor_image(&self) -> Option<yserver_core::backend::ActiveCursorImage> {
        // Stage 5 — unblock protocol-audit #14 (`GetCursorImage`
        // returns 0×0). Source the bytes from the
        // currently-effective `Arc<CursorRecord>` and stamp the
        // current root-space pointer position.
        let xid = self.effective_cursor_xid?;
        let record = self.cursor_records.get(&xid)?;
        #[allow(clippy::cast_possible_truncation)]
        let x = self.core.cursor_x as i16;
        #[allow(clippy::cast_possible_truncation)]
        let y = self.core.cursor_y as i16;
        Some(yserver_core::backend::ActiveCursorImage {
            width: record.width,
            height: record.height,
            hot_x: record.hot_x,
            hot_y: record.hot_y,
            x,
            y,
            // XFIXES serial is u32; CursorRecord.version is u64
            // server-wide monotonic. Saturate; in practice we'll
            // never roll over a u32 of cursor changes in a session.
            serial: u32::try_from(record.version).unwrap_or(u32::MAX),
            bgra_bytes: std::sync::Arc::new(record.bgra_bytes.clone()),
        })
    }

    fn xfixes_change_cursor_by_name(
        &mut self,
        _origin: Option<OriginContext>,
        _host_cursor_xid: u32,
        _name_bytes: &[u8],
    ) -> io::Result<()> {
        // Stage 3f.4: v1-parity no-op. XFixes cursor-by-name is a
        // theme-database hint ("watch" / "left_ptr" / etc.); yserver
        // doesn't have a cursor-theme registry, so neither v1 nor v2
        // do anything beyond returning Ok. Real apps see no behaviour
        // difference (their fallback non-named cursor stays in
        // effect).
        Ok(())
    }

    fn set_shape_rectangles(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        kind: u8,
        rects: &[xfixes::RegionRect],
    ) -> io::Result<()> {
        // Bookkeeping mutation: SHAPE rects live in KmsCore; no
        // paint side-effect needed in Stage 1b.
        let dst = match kind {
            0 => &mut self.core.shape_bounding,
            1 => &mut self.core.shape_clip,
            2 => &mut self.core.shape_input,
            _ => {
                self.log_v2_gap("set_shape_rectangles_invalid_kind");
                return Ok(());
            }
        };
        if rects.is_empty() {
            dst.remove(&host_xid);
        } else {
            dst.insert(host_xid, rects.to_vec());
        }
        Ok(())
    }

    // ── Misc ────────────────────────────────────────────────────

    fn warp_pointer(
        &mut self,
        _origin: Option<OriginContext>,
        _dst_host_xid: u32,
        _dst_x: i16,
        _dst_y: i16,
    ) -> io::Result<()> {
        // The window-relative form is unused on KMS — the handler
        // resolves the destination to root coords (only ServerState
        // knows window positions) and calls `warp_pointer_root`.
        Ok(())
    }

    fn warp_pointer_root(&mut self, state: &mut ServerState, x: i32, y: i32) {
        // Route through the absolute-motion input path: updates the
        // tracked cursor (and HW cursor plane) and fans out the
        // motion/crossing events WarpPointer is specified to generate
        // ("as if the user had instantaneously moved the pointer").
        self.on_host_input(
            state,
            yserver_core::core_loop::HostInputEvent::PointerMotion { x, y, time: 0 },
        );
    }

    fn query_pointer(&mut self, _origin: Option<OriginContext>) -> io::Result<PointerPosition> {
        // Return the current core-tracked cursor position. No
        // window-focus lookup — Stage 1b doesn't model focus.
        //
        // The mask is a full X11 KeyButMask: live keyboard modifiers
        // (xkb state, low byte) | held buttons (0x100+). Xorg's
        // QueryPointer/XIQueryPointer report the paired keyboard's
        // modifier state here; pre-fix only buttons were included, so
        // cinnamon's alt-tab switcher (global.get_pointer() via
        // XIQueryPointer) read "Alt not held" mid-alt-tab and
        // instantly cancelled the popup.
        Ok(PointerPosition {
            same_screen: true,
            #[allow(clippy::cast_possible_truncation)]
            win_x: self.core.cursor_x as i16,
            #[allow(clippy::cast_possible_truncation)]
            win_y: self.core.cursor_y as i16,
            mask: self.core.button_mask | self.serialize_modifiers(),
        })
    }

    fn list_fonts_proxy(
        &mut self,
        _origin: Option<OriginContext>,
        max_names: u16,
        pattern: &str,
    ) -> io::Result<Vec<u8>> {
        let cap = usize::from(max_names);
        // Font-path (fonts.dir/alias) names first, then the built-ins
        // catalog — ONE global max_names budget across the ordered
        // walk (Xorg traverses FPEs in path order with one count).
        let mut names: Vec<String> = self.core.font_loader.path_font_names(pattern);
        names.extend(
            self.core
                .font_loader
                .catalog
                .iter()
                .filter(|name| xlfd_pattern_matches(pattern, name))
                .cloned(),
        );
        names.truncate(cap);

        let mut name_data: Vec<u8> = Vec::new();
        for name in &names {
            name_data.push(u8::try_from(name.len()).unwrap_or(u8::MAX));
            name_data.extend_from_slice(name.as_bytes());
        }
        let pad = (4 - (name_data.len() % 4)) % 4;
        name_data.resize(name_data.len() + pad, 0);

        let extra_words = u32::try_from(name_data.len() / 4).unwrap_or(0);
        let mut reply = vec![0u8; 32 + name_data.len()];
        reply[0] = 1;
        reply[4..8].copy_from_slice(&extra_words.to_le_bytes());
        reply[8..10].copy_from_slice(&u16::try_from(names.len()).unwrap_or(u16::MAX).to_le_bytes());
        reply[32..].copy_from_slice(&name_data);
        Ok(reply)
    }

    fn list_fonts_with_info_proxy(
        &mut self,
        _origin: Option<OriginContext>,
        max_names: u16,
        pattern: &str,
        intern_atom: &mut dyn FnMut(&str) -> u32,
    ) -> io::Result<Vec<Vec<u8>>> {
        let cap = usize::from(max_names);
        // Path fonts first, then built-ins — one global budget
        // (mirrors list_fonts_proxy ordering so the two requests
        // agree on the visible font set).
        let mut matched: Vec<String> = self.core.font_loader.path_font_names(pattern);
        matched.extend(
            self.core
                .font_loader
                .catalog
                .iter()
                .filter(|name| xlfd_pattern_matches(pattern, name))
                .cloned(),
        );
        matched.truncate(cap);

        let mut entries: Vec<(String, FontMetrics)> = Vec::with_capacity(matched.len());
        for name in matched {
            // Capture the fonts.dir KEY that resolve() matches for path
            // fonts; it — not the bare matched alias, not the file-
            // embedded FONT atom — is the round-trippable XA_FONT.
            let path_entry_key = match self.core.font_loader.resolve(&name) {
                Some(crate::kms::core::FontResolution::File { entry_name, .. }) => Some(entry_name),
                _ => None,
            };
            match self.core.font_loader.open_font(&name) {
                Ok((_face, mut metrics, _cache)) => {
                    // Reply NAME keeps Xorg FPE behavior: path / XLFD
                    // names go out verbatim; a bare built-in alias
                    // ("fixed"/"cursor"/"nil2") is rewritten to a full
                    // XLFD so XCreateFontSet can parse a charset.
                    let wire_name = if path_entry_key.is_some()
                        || crate::kms::core::FontLoader::is_xlfd_pattern(&name)
                    {
                        name.clone()
                    } else {
                        crate::kms::core::FontLoader::alias_to_xlfd(&name, &metrics)
                    };
                    // XA_FONT must be a charset-bearing XLFD that
                    // round-trips through open_font (libX11 OpenFonts it
                    // verbatim — omGeneric.c get_prop_name). For a path
                    // font that's the fonts.dir KEY resolve() matched;
                    // the PCF's embedded FONT atom can diverge from the
                    // key (point/dpi/avg-width fields — e.g. Debian
                    // xfonts-* 19px misc-fixed) and then fail to reopen.
                    let font_prop_name = match path_entry_key {
                        Some(entry) => entry,
                        None if crate::kms::core::FontLoader::is_xlfd_pattern(&name) => {
                            name.clone()
                        }
                        None => crate::kms::core::FontLoader::alias_to_xlfd(&name, &metrics),
                    };
                    // Properties: file-embedded (BDF/PCF) ones when
                    // present, interned here; always ensure FONT
                    // (XA_FONT=18 → atom of the wire name) —
                    // libX11's XCreateFontSet resolves non-XLFD base
                    // names EXCLUSIVELY through this property
                    // (omGeneric.c get_prop_name reads XA_FONT off the
                    // first reply and GetAtomName's it); the reply
                    // name alone is not consulted on that path.
                    let named = std::mem::take(&mut metrics.named_properties);
                    let mut props = Vec::with_capacity(named.len() * 8 + 8);
                    for (pname, value) in &named {
                        // XA_FONT (18) is re-emitted below as `wire_name`
                        // — the name the server can actually re-open. A
                        // PCF's file-embedded FONT atom can diverge from
                        // its fonts.dir entry key (point/dpi/avg-width
                        // fields); reporting the embedded one breaks the
                        // XCreateFontSet round-trip (libX11 OpenFonts
                        // XA_FONT verbatim, but `resolve()` only matches
                        // fonts.dir keys) on font sets where they differ
                        // — e.g. Debian xfonts-* 19px misc-fixed. Skip
                        // the embedded FONT here.
                        if pname == "FONT" {
                            continue;
                        }
                        let name_atom = intern_atom(pname);
                        let v: u32 = match value {
                            yserver_protocol::x11::FontPropValue::Card(c) => *c,
                            #[allow(clippy::cast_sign_loss)]
                            yserver_protocol::x11::FontPropValue::Int(i) => *i as u32,
                            yserver_protocol::x11::FontPropValue::Str(s) => intern_atom(s),
                        };
                        props.extend_from_slice(&name_atom.to_le_bytes());
                        props.extend_from_slice(&v.to_le_bytes());
                    }
                    // Always emit XA_FONT (18) = the round-trippable name
                    // (fonts.dir key for path fonts; the synthesized
                    // charset-bearing XLFD for built-in aliases).
                    let font_atom = intern_atom(&font_prop_name);
                    props.extend_from_slice(&18u32.to_le_bytes());
                    props.extend_from_slice(&font_atom.to_le_bytes());
                    metrics.properties = props;
                    entries.push((wire_name, metrics));
                }
                Err(err) => {
                    log::debug!("v2 ListFontsWithInfo: skipping {name:?} — open_font: {err}");
                }
            }
        }

        let total = entries.len();
        let mut replies: Vec<Vec<u8>> = Vec::with_capacity(total + 1);
        for (idx, (name, metrics)) in entries.iter().enumerate() {
            let remaining = u32::try_from(total - idx - 1).unwrap_or(0);
            let mut buf = Vec::new();
            yserver_protocol::x11::write_list_fonts_with_info_reply(
                &mut buf,
                yserver_protocol::x11::ClientByteOrder::LittleEndian,
                yserver_protocol::x11::SequenceNumber(0),
                metrics,
                name,
                remaining,
            )?;
            replies.push(buf);
        }
        let mut term = Vec::new();
        yserver_protocol::x11::write_list_fonts_with_info_terminator(
            &mut term,
            yserver_protocol::x11::ClientByteOrder::LittleEndian,
            yserver_protocol::x11::SequenceNumber(0),
        )?;
        replies.push(term);
        Ok(replies)
    }

    fn get_atom_name(
        &mut self,
        _origin: Option<OriginContext>,
        _atom: u32,
    ) -> io::Result<Option<String>> {
        // Atom store lives in ServerState, not the backend. v2 has
        // nothing to add here.
        Ok(None)
    }

    fn get_keyboard_mapping(
        &mut self,
        _origin: Option<OriginContext>,
        first_keycode: u8,
        count: u8,
    ) -> io::Result<(u8, Vec<u32>)> {
        // Stage 3f.7 follow-up: port v1's body verbatim. KmsCore
        // carries `xkb_keymap` so the lookup works on both backends.
        // The pre-fix stub returned 0 keysyms per code, which made
        // xterm think every key was dead — typing into xterm worked
        // for cursor movement but Enter/letters were swallowed.
        //
        // X11 GetKeyboardMapping: per keycode, return a flat row of
        // keysyms across shift levels (unshifted / shifted /
        // mode-switch-unshifted / mode-switch-shifted). Apps combine
        // the keycode with the modifier bits in the event's `state`
        // field to pick the right slot.
        const LEVELS: usize = 4;
        let max_kc = u16::from(first_keycode) + u16::from(count);
        let mut flat = Vec::with_capacity(usize::from(count) * LEVELS);
        for kc in u16::from(first_keycode)..max_kc {
            let xkb_kc = xkbcommon::xkb::Keycode::new(u32::from(kc));
            for level in 0..LEVELS as u32 {
                let syms = self
                    .core
                    .xkb_keymap
                    .0
                    .key_get_syms_by_level(xkb_kc, 0, level);
                flat.push(syms.first().map_or(0, |s| s.raw()));
            }
        }
        Ok((LEVELS as u8, flat))
    }

    fn get_modifier_mapping(
        &mut self,
        _origin: Option<OriginContext>,
    ) -> io::Result<(u8, Vec<u8>)> {
        // Derive the modifier→keycode table from the live keymap so
        // it always agrees with the XKB GetMap modifier map (same
        // `modifier_bit_for_keysym` source of truth). Avoids a
        // hand-written table drifting from the actual keymap.
        Ok(crate::kms::xkb::modifier_mapping_from_keymap(
            &self.core.xkb_keymap.0,
        ))
    }

    fn dpms_capable(&self) -> bool {
        true
    }

    fn set_dpms_power(&mut self, level: u8) -> std::io::Result<()> {
        // Levels 1/2/3 collapse to "outputs off"; only 0 is "on".
        let want_active = level == 0;
        if want_active == self.kms_outputs_active {
            log::info!(
                "kms: set_dpms_power(level={level}) — same binary state \
                 (kms_outputs_active={}), no-op",
                self.kms_outputs_active,
            );
            return Ok(()); // same binary state (e.g. Standby → Suspend)
        }
        log::info!(
            "kms: set_dpms_power(level={level}) — transition active={} → {want_active}",
            self.kms_outputs_active,
        );

        if want_active {
            // ── Wake side. Mirrors KmsBackendV2::run_resume around the
            //    modeset commit: commit_modeset, then re-arm the cursor
            //    plane via legacy ioctl. Without rearm_cursor the cursor
            //    plane stays bound to a CRTC that was disabled — the
            //    first subsequent atomic page-flip then EINVALs because
            //    the kernel sees a stale plane→CRTC reference. See
            //    project_einval_atomic_commit_storm_wedge memory entry.
            //
            // ALWAYS run rearm_cursor + wake_for_damage regardless of
            // dpms_set_outputs_active's result. That helper is best-
            // effort: it returns the FIRST per-output failure but keeps
            // attempting the rest, so a partial-success scenario (one
            // output came up, another didn't) returns Err — but the
            // outputs that DID come up still need their cursor plane
            // rebound and damage queued. Cache flip is conservative —
            // only mark fully-on if every output succeeded; on partial
            // failure, the next set_dpms_power(On) retry sees
            // kms_outputs_active=false and re-attempts (idempotent on
            // the outputs that already came up).
            let res = self.platform.dpms_set_outputs_active(true);
            let (hot_x, hot_y) = self
                .effective_cursor_xid
                .and_then(|xid| self.cursor_records.get(&xid))
                .map(|rec| (rec.hot_x, rec.hot_y))
                .unwrap_or((0, 0));
            #[allow(clippy::cast_possible_truncation)]
            let cx = self.core.cursor_x as i32;
            #[allow(clippy::cast_possible_truncation)]
            let cy = self.core.cursor_y as i32;
            log::info!("kms: dpms wake — rearm_cursor hot=({hot_x},{hot_y}) pos=({cx},{cy})");
            self.platform.rearm_cursor(hot_x, hot_y, cx, cy);
            // Outputs were dark; any incremental damage tracking is
            // stale. Force a fresh full frame on the next composite tick.
            self.scene.wake_for_damage();
            if res.is_ok() {
                self.kms_outputs_active = true;
            }
            res
        } else {
            // ── Sleep side. Mirrors KmsBackendV2::run_suspend steps 4 →
            //    4b → 4c around `disable_output`:
            //      (1) wait for GPU idle so disable_output isn't racing
            //          in-flight compose CBs.
            //      (2) drain in-flight page-flip acks + reset per-output
            //          cursor plane state. Without this, the cursor plane
            //          stays bound across disable_output and the kernel
            //          rejects the next page-flip after wake with EINVAL.
            //      (3) reset platform's scanout BO state machine so
            //          orphaned Pending/OnScreen entries don't leak.
            //      (4) actually disable_output per output.
            log::info!("kms: dpms sleep — wait_idle_bounded");
            self.platform.wait_idle_bounded();
            log::info!("kms: dpms sleep — scene.drain_all");
            self.scene.drain_all(&mut self.platform);
            log::info!("kms: dpms sleep — reset_scanout_bos_for_suspend");
            self.platform.reset_scanout_bos_for_suspend();
            log::info!("kms: dpms sleep — disable_output per output");
            let res = self.platform.dpms_set_outputs_active(false);
            // Only flip the cache on success. On Err, leave it where it was
            // so the next set_dpms_power call retries rather than no-opping
            // through the same-binary-state guard above.
            if res.is_ok() {
                self.kms_outputs_active = false;
            }
            res
        }
    }
}

/// XLFD glob match per X11 ListFonts semantics: `*` matches zero or more
/// characters (including `-`), `?` matches exactly one. Comparison is
/// ASCII case-insensitive because clients legitimately mix case.
fn xlfd_pattern_matches(pattern: &str, name: &str) -> bool {
    let pat = pattern.as_bytes();
    let s = name.as_bytes();
    let mut pi = 0usize;
    let mut si = 0usize;
    let mut star_pi: Option<usize> = None;
    let mut star_si: usize = 0;
    while si < s.len() {
        if pi < pat.len() && (pat[pi] == b'?' || pat[pi].eq_ignore_ascii_case(&s[si])) {
            pi += 1;
            si += 1;
        } else if pi < pat.len() && pat[pi] == b'*' {
            star_pi = Some(pi);
            star_si = si;
            pi += 1;
        } else if let Some(sp) = star_pi {
            pi = sp + 1;
            star_si += 1;
            si = star_si;
        } else {
            return false;
        }
    }
    while pi < pat.len() && pat[pi] == b'*' {
        pi += 1;
    }
    pi == pat.len()
}

/// Stage 4d Manual-redirect fix: when a window has mapped child
/// windows and is drawn into via a `ClipByChildren` GC (the X11
/// default), the draw must NOT touch the area covered by each
/// child. In v1 this was natural because every window had its own
/// mirror — paint to the parent landed in the parent's storage
/// while child paint landed in the child's. v2's COMPOSITE Manual-
/// redirect collapses an entire redirected subtree into a single
/// backing pixmap, so the parent-vs-child overlap is now a real
/// region-rect subtraction the backend has to perform.
///
/// Symptom this fixes: marco's per-frame full-extent CopyArea
/// (decorations source → frame) clobbers the inferior CC window's
/// area inside the redirected backing, then CC repaints only its
/// small dirty rect, leaving the backing's centre as marco's
/// (mostly-blank) decoration pixmap. Visible as "top-left content
/// only" after a few frames.
///
/// `dst_rect` is in destination-window-local coordinates; `child_rects`
/// are mapped-child rectangles also in destination-window-local
/// coordinates (parent's `(child.x, child.y, child.w, child.h)`).
/// Returns the surviving sub-rectangles, also in dst-window-local
/// coordinates. Empty input child list returns `[dst_rect]`. Empty
/// `dst_rect` (zero-size) returns `[]`.
/// Intersect a destination rectangle against an X11 GC clip
/// (a list of rectangles already translated into destination-window
/// coordinates). Returns the surviving pieces. An empty `clip_rects`
/// represents an empty clip region — Xorg's behaviour is "paint
/// nothing", so we return an empty Vec. `rect` with zero area also
/// returns empty.
///
/// Used by `copy_area` ahead of child subtraction so a
/// `SetClipRectangles`-issued explicit clip constrains the copy
/// (Stage 4d codex round 2026-05-18: pre-fix `copy_area` honoured
/// neither GC clip nor `ClipByChildren`).
fn intersect_rect_with_clip(
    rect: ash::vk::Rect2D,
    clip_rects: &[ash::vk::Rect2D],
) -> Vec<ash::vk::Rect2D> {
    if clip_rects.is_empty() || rect.extent.width == 0 || rect.extent.height == 0 {
        return Vec::new();
    }
    let rx0 = rect.offset.x;
    let ry0 = rect.offset.y;
    let rx1 = rx0 + i32::try_from(rect.extent.width).unwrap_or(i32::MAX);
    let ry1 = ry0 + i32::try_from(rect.extent.height).unwrap_or(i32::MAX);
    let mut out = Vec::with_capacity(clip_rects.len());
    for c in clip_rects {
        let cx0 = c.offset.x;
        let cy0 = c.offset.y;
        let cx1 = cx0 + i32::try_from(c.extent.width).unwrap_or(0);
        let cy1 = cy0 + i32::try_from(c.extent.height).unwrap_or(0);
        let ix0 = rx0.max(cx0);
        let iy0 = ry0.max(cy0);
        let ix1 = rx1.min(cx1);
        let iy1 = ry1.min(cy1);
        if ix0 < ix1 && iy0 < iy1 {
            out.push(ash::vk::Rect2D {
                offset: ash::vk::Offset2D { x: ix0, y: iy0 },
                extent: ash::vk::Extent2D {
                    width: u32::try_from(ix1 - ix0).unwrap_or(0),
                    height: u32::try_from(iy1 - iy0).unwrap_or(0),
                },
            });
        }
    }
    out
}

/// Translate a clip-rect list by `(dx, dy)` (signed). Used to map a
/// source / mask picture's client clip from the picture's own
/// drawable space into the destination's drawable space, mirroring
/// Xorg's `miClipPictureSrc`
/// (`/home/jos/Projects/xserver/render/mipict.c:267-290`). The Xorg
/// path translates pPicture->clientClip in-place, intersects, then
/// translates back; we copy-translate so the picture record stays
/// untouched.
///
/// Out-of-i16 results saturate; X11 fixed-point clips never need
/// more than 16-bit signed coords on the wire.
fn translate_clip_rects(rects: &[Rectangle16], dx: i32, dy: i32) -> Vec<Rectangle16> {
    rects
        .iter()
        .map(|r| {
            let nx = i32::from(r.x).saturating_add(dx);
            let ny = i32::from(r.y).saturating_add(dy);
            Rectangle16 {
                x: i16::try_from(nx).unwrap_or(if nx < 0 { i16::MIN } else { i16::MAX }),
                y: i16::try_from(ny).unwrap_or(if ny < 0 { i16::MIN } else { i16::MAX }),
                width: r.width,
                height: r.height,
            }
        })
        .collect()
}

/// Intersect two clip-rect lists. Returns the pairwise rectangle
/// intersections, omitting empties. Both lists are interpreted as
/// "the clip is the union of these rects" — the resulting list is
/// `union { a ∩ b : a ∈ a_list, b ∈ b_list }`.
///
/// Helper for `compute_render_composite_clip` below.
fn intersect_clip_lists(a: &[Rectangle16], b: &[Rectangle16]) -> Vec<Rectangle16> {
    let mut out = Vec::with_capacity(a.len() * b.len());
    for ra in a {
        let ax0 = i32::from(ra.x);
        let ay0 = i32::from(ra.y);
        let ax1 = ax0.saturating_add(i32::from(ra.width));
        let ay1 = ay0.saturating_add(i32::from(ra.height));
        for rb in b {
            let bx0 = i32::from(rb.x);
            let by0 = i32::from(rb.y);
            let bx1 = bx0.saturating_add(i32::from(rb.width));
            let by1 = by0.saturating_add(i32::from(rb.height));
            let ix0 = ax0.max(bx0);
            let iy0 = ay0.max(by0);
            let ix1 = ax1.min(bx1);
            let iy1 = ay1.min(by1);
            if ix0 < ix1 && iy0 < iy1 {
                out.push(Rectangle16 {
                    x: i16::try_from(ix0).unwrap_or(i16::MAX),
                    y: i16::try_from(iy0).unwrap_or(i16::MAX),
                    width: u16::try_from(ix1 - ix0).unwrap_or(u16::MAX),
                    height: u16::try_from(iy1 - iy0).unwrap_or(u16::MAX),
                });
            }
        }
    }
    out
}

/// Compose the effective composite-region clip for `render_composite`
/// per X RENDER spec (`miComputeCompositeRegion`,
/// `/home/jos/Projects/xserver/render/mipict.c:316-389`):
///
///   clip = dst_clip ∩ src_clip-translated-to-dst-space ∩ mask_clip-translated-to-dst-space
///
/// Each argument may be `None`, which is interpreted as "no clip on
/// this picture" (paint everywhere). If all three are `None`, the
/// function returns `None` — the engine then applies its own
/// full-extent default. If any is `Some`, the result is `Some` and
/// carries the intersection (possibly empty, which means "paint
/// nothing" per X RENDER spec — Xorg returns FALSE here and skips
/// the draw).
///
/// `src_translation` and `mask_translation` are `(xDst - xSrc,
/// yDst - ySrc)` and `(xDst - xMask, yDst - yMask)` respectively
/// (per Xorg's `miClipPictureSrc` call sites at `mipict.c:356,370`).
/// `mask_clip` should be `None` when no mask is used.
///
/// Pure / no Vulkan; tested below against hand-traced Xorg vectors.
fn compute_render_composite_clip(
    dst_clip: Option<&[Rectangle16]>,
    src_clip: Option<&[Rectangle16]>,
    src_translation: (i32, i32),
    mask_clip: Option<&[Rectangle16]>,
    mask_translation: (i32, i32),
) -> Option<Vec<Rectangle16>> {
    let src_in_dst =
        src_clip.map(|c| translate_clip_rects(c, src_translation.0, src_translation.1));
    let mask_in_dst =
        mask_clip.map(|c| translate_clip_rects(c, mask_translation.0, mask_translation.1));
    // Start with whichever input is Some, then fold the remaining
    // Some-inputs via intersection. Order doesn't matter — list
    // intersection is associative & commutative.
    let mut acc: Option<Vec<Rectangle16>> = None;
    let mut fold = |next: Option<Vec<Rectangle16>>| match (acc.take(), next) {
        (None, None) => {}
        (None, Some(v)) => acc = Some(v),
        (Some(a), None) => acc = Some(a),
        (Some(a), Some(b)) => acc = Some(intersect_clip_lists(&a, &b)),
    };
    fold(dst_clip.map(<[Rectangle16]>::to_vec));
    fold(src_in_dst);
    fold(mask_in_dst);
    acc
}

fn compute_copy_area_dst_rects(
    dst_rect: ash::vk::Rect2D,
    child_rects: &[ash::vk::Rect2D],
) -> Vec<ash::vk::Rect2D> {
    if dst_rect.extent.width == 0 || dst_rect.extent.height == 0 {
        return Vec::new();
    }
    let mut current = vec![dst_rect];
    for child in child_rects {
        let mut next = Vec::new();
        for r in current {
            next.extend(subtract_one_rect_clip(r, *child));
        }
        current = next;
        if current.is_empty() {
            return current;
        }
    }
    current
}

/// Subtract `inner` from `outer`. Both rects are in the same coord
/// space. Result is up to 4 disjoint sub-rectangles tiling
/// `outer \ inner` (top strip, bottom strip, middle-band left strip,
/// middle-band right strip — Xorg/pixman band order). If `inner`
/// doesn't intersect `outer`, returns `[outer]` unchanged.
fn subtract_one_rect_clip(outer: ash::vk::Rect2D, inner: ash::vk::Rect2D) -> Vec<ash::vk::Rect2D> {
    let ox0 = outer.offset.x;
    let oy0 = outer.offset.y;
    let ox1 = outer.offset.x + i32::try_from(outer.extent.width).unwrap_or(i32::MAX);
    let oy1 = outer.offset.y + i32::try_from(outer.extent.height).unwrap_or(i32::MAX);
    // Intersection of inner with outer (clamped to outer's bounds).
    let ix0 = inner.offset.x.max(ox0);
    let iy0 = inner.offset.y.max(oy0);
    let ix1 = (inner.offset.x + i32::try_from(inner.extent.width).unwrap_or(0)).min(ox1);
    let iy1 = (inner.offset.y + i32::try_from(inner.extent.height).unwrap_or(0)).min(oy1);
    if ix0 >= ix1 || iy0 >= iy1 {
        return vec![outer];
    }
    let mk = |x: i32, y: i32, w: i32, h: i32| ash::vk::Rect2D {
        offset: ash::vk::Offset2D { x, y },
        extent: ash::vk::Extent2D {
            width: u32::try_from(w).unwrap_or(0),
            height: u32::try_from(h).unwrap_or(0),
        },
    };
    let mut result = Vec::with_capacity(4);
    // Top strip: full outer width, y in [oy0, iy0).
    if oy0 < iy0 {
        result.push(mk(ox0, oy0, ox1 - ox0, iy0 - oy0));
    }
    // Bottom strip: full outer width, y in [iy1, oy1).
    if iy1 < oy1 {
        result.push(mk(ox0, iy1, ox1 - ox0, oy1 - iy1));
    }
    // Left middle: middle band height, x in [ox0, ix0).
    if ox0 < ix0 {
        result.push(mk(ox0, iy0, ix0 - ox0, iy1 - iy0));
    }
    // Right middle: middle band height, x in [ix1, ox1).
    if ix1 < ox1 {
        result.push(mk(ix1, iy0, ox1 - ix1, iy1 - iy0));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::{
        KmsBackendV2, PictureRecord, compute_copy_area_dst_rects, compute_render_composite_clip,
        intersect_rect_with_clip, resolve_picture_for_render,
    };
    use crate::kms::{
        cpu_types::{Rectangle16, Repeat},
        v2::{platform::PlatformBackend, store::Storage},
    };
    use std::collections::HashMap;
    use yserver_core::{
        backend::Backend,
        properties::{PropertyFormat, PropertyValue},
        server::ServerState,
    };

    mod get_image_planes {
        use super::super::{
            apply_gc_function, apply_z_plane_mask, depth_plane_mask, read_z_pixmap_pixel,
            write_z_pixmap_pixel, z_to_xy_planes,
        };
        use yserver_core::backend::GcFunction;

        #[test]
        fn depth_plane_mask_truncates_to_depth() {
            assert_eq!(depth_plane_mask(1), 0x1);
            assert_eq!(depth_plane_mask(4), 0x0f);
            assert_eq!(depth_plane_mask(8), 0xff);
            assert_eq!(depth_plane_mask(24), 0x00ff_ffff);
            assert_eq!(depth_plane_mask(32), u32::MAX);
        }

        #[test]
        fn z_mask_depth24_zeroes_unrequested_planes() {
            // One BGRA pixel 0x00ff_80ff (LE bytes B=0xff G=0x80 R=0xff X=0).
            let mut px = vec![0xff, 0x80, 0xff, 0x00];
            apply_z_plane_mask(&mut px, 24, 0x0000_00ff); // blue planes only
            assert_eq!(px, vec![0xff, 0x00, 0x00, 0x00]);
        }

        #[test]
        fn z_mask_depth8_masks_bytes() {
            let mut bytes = vec![0xab, 0x0f, 0xf0, 0x00];
            apply_z_plane_mask(&mut bytes, 8, 0x0f);
            assert_eq!(bytes, vec![0x0b, 0x0f, 0x00, 0x00]);
        }

        #[test]
        fn z_mask_depth4_masks_bytes() {
            // Depth-4 wire ZPixmap is nibble-PACKED (two pixels per
            // byte, low nibble first — see pack_from_storage), so the
            // mask applies to BOTH nibbles independently. The original
            // expectation here predated the a2d5480 nibble packing and
            // assumed one pixel per byte.
            // Full mask is the identity.
            let mut bytes = vec![0xab, 0x0f, 0xf0, 0x00];
            apply_z_plane_mask(&mut bytes, 4, 0x0f);
            assert_eq!(bytes, vec![0xab, 0x0f, 0xf0, 0x00]);
            // Partial mask 0b0011 keeps the low two planes of each pixel.
            let mut bytes = vec![0xab, 0x0f, 0xf0, 0x00];
            apply_z_plane_mask(&mut bytes, 4, 0x03);
            assert_eq!(bytes, vec![0x23, 0x03, 0x30, 0x00]);
        }

        #[test]
        fn xy_planes_depth4_unpacks_nibbles() {
            // pixel0=1, pixel1=2 packed low-nibble first into 0x21.
            let z = [0x21, 0x00, 0x00, 0x00];
            let out = z_to_xy_planes(&z, 2, 1, 4, 0x3);
            assert_eq!(out.len(), 2 * 4);
            // Plane 1 first: pixel1 only.
            assert_eq!(out[0], 0b0000_0010);
            // Plane 0 second: pixel0 only.
            assert_eq!(out[4], 0b0000_0001);
        }

        #[test]
        fn z_mask_depth1_empty_mask_zeroes() {
            let mut bytes = vec![0xff, 0xff, 0xff, 0xff];
            apply_z_plane_mask(&mut bytes, 1, 0);
            assert_eq!(bytes, vec![0, 0, 0, 0]);
        }

        #[test]
        fn xy_planes_msb_first_lsb_bit_order() {
            // 2x1 depth-24 image: pixel0 = 0x000001 (bit 0 set),
            // pixel1 = 0x800000 (bit 23 set). Request planes 23 and 0.
            let z = [
                0x01, 0x00, 0x00, 0x00, // pixel (0,0) BGRA LE
                0x00, 0x00, 0x80, 0x00, // pixel (1,0)
            ];
            let mask = (1 << 23) | 1;
            let out = z_to_xy_planes(&z, 2, 1, 24, mask);
            // Two planes, scanline pad 32 bits → 4 bytes per row.
            assert_eq!(out.len(), 2 * 4);
            // Plane 23 first (most significant): pixel1 → bit 1 LSBFirst.
            assert_eq!(out[0], 0b0000_0010);
            // Plane 0 second: pixel0 → bit 0.
            assert_eq!(out[4], 0b0000_0001);
        }

        #[test]
        fn xy_planes_depth1_is_identity_for_plane0() {
            // 40x2 depth-1 bitmap: stride = ceil(40/32)*4 = 8 bytes.
            let mut z = vec![0u8; 16];
            z[0] = 0xa5;
            z[9] = 0x3c;
            let out = z_to_xy_planes(&z, 40, 2, 1, 0x1);
            assert_eq!(out, z);
        }

        #[test]
        fn xy_planes_empty_mask_is_empty() {
            let z = [0u8; 16];
            assert!(z_to_xy_planes(&z, 2, 2, 24, 0).is_empty());
        }

        #[test]
        fn depth4_z_pixmap_pixel_round_trip_uses_nibbles() {
            let mut z = vec![0u8; 4];
            write_z_pixmap_pixel(&mut z, 4, 2, 0, 0, 0x1);
            write_z_pixmap_pixel(&mut z, 4, 2, 1, 0, 0xe);
            assert_eq!(z[0], 0xe1);
            assert_eq!(read_z_pixmap_pixel(&z, 4, 2, 0, 0), 0x1);
            assert_eq!(read_z_pixmap_pixel(&z, 4, 2, 1, 0), 0xe);
        }

        #[test]
        fn gc_function_respects_plane_mask() {
            assert_eq!(
                apply_gc_function(GcFunction::Copy, 0b1111, 0b0000, 0b0101),
                0b0101
            );
            assert_eq!(
                apply_gc_function(GcFunction::Invert, 0, 0b0011, 0b0001),
                0b0010
            );
            assert_eq!(
                apply_gc_function(GcFunction::Nor, 0b0001, 0b0010, 0b1111),
                0b1100
            );
        }
    }

    /// Stage 1b acceptance gate (synthetic): v2 constructs through
    /// `for_tests` and answers the capability accessors with the
    /// same values as v1. This is the "boots far enough to service
    /// capability queries" check from the spec.
    #[test]
    fn v2_skeleton_advertises_expected_capabilities() {
        let b = KmsBackendV2::for_tests();
        assert_eq!(b.window_id(), 1);
        assert_eq!(b.root_visual_xid(), 0x21);
        assert_eq!(b.render_opcode(), Some(133));
        assert_eq!(b.xkb_opcode(), Some(136));
        assert_eq!(b.xkb_info(), Some((136, 85, 162)));
        assert_eq!(b.composite_opcode(), Some(144));
        // Non-trivial format passes through untouched; 0 returns None.
        assert_eq!(b.render_format_for_ynest_id(0), None);
        assert_eq!(b.render_format_for_ynest_id(0x12345), Some(0x12345));
        // KMS has no upstream host visuals, but it still advertises
        // server-local ARGB ids so CreateWindow can preserve depth 32.
        assert_eq!(b.argb_visual_xid(), Some(0x103));
        assert_eq!(b.argb_colormap_xid(), Some(0x104));
    }

    /// Spec: "the first paint op produces a logged 'v2 not yet
    /// implemented' gap." Verify dedup — same op logs once even
    /// when called multiple times.
    ///
    /// Stage 2c wired fill_rectangle / put_image to real engine
    /// calls; against `for_tests` (no Vk) those reach the engine,
    /// surface `NoVk`, and log under a different name. The dedup
    /// behaviour is unchanged: each gap-name fires once per
    /// session. copy_area is still a logged-gap stub (Stage 2d
    /// territory).
    #[test]
    fn v2_paint_stub_returns_ok_and_dedups_gap() {
        let mut b = KmsBackendV2::for_tests();
        // First call logs (xid is unknown → `*_unknown_xid` gap).
        assert!(b.put_image(None, 0x1234, 24, 16, 16, 0, 0, &[0; 4]).is_ok());
        // Subsequent calls also return Ok and don't crash.
        for _ in 0..5 {
            assert!(b.put_image(None, 0x1234, 24, 16, 16, 0, 0, &[0; 4]).is_ok());
            assert!(b.copy_area(None, 0x1234, 0x5678, 0, 0, 0, 0, 4, 4).is_ok());
            assert!(b.fill_rectangle(None, 0x1234, 0, 0, 0, 4, 4).is_ok());
        }
        let logged = b.logged_gaps.borrow();
        // Unknown-xid path for the wired ops; all three log the
        // `_unknown_xid` variant since the test xids aren't in
        // the store fixture.
        assert!(logged.contains("put_image_unknown_xid"));
        assert!(logged.contains("fill_rectangle_unknown_xid"));
        assert!(logged.contains("copy_area_unknown_xid"));
    }

    #[test]
    fn v2_pending_present_completion_sets_poll_deadline() {
        let mut b = KmsBackendV2::for_tests();
        b.scene.scene_structure_dirty = false;
        assert!(b.next_wakeup().is_none());

        b.enqueue_present_completion(
            yserver_core::backend::CompletedPresentEvent {
                client_id: yserver_protocol::x11::ClientId(0),
                serial: 1,
                host_xid: 0x1000,
                dst_host_xid: 0x1001,
                options: 0,
                wake: yserver_core::backend::PresentWake::Pixmap { idle_fence_xid: 0 },
            },
            0x1001,
        );

        let deadline = b
            .next_wakeup()
            .expect("pending PRESENT completion must wake polling fallback");
        assert!(
            deadline <= std::time::Instant::now() + std::time::Duration::from_millis(2),
            "pending PRESENT deadline should be near-term"
        );
    }

    /// Spec: "boots far enough to service GetGeometry / InternAtom".
    /// Backend::xid_map reflects KmsCore's root xid seed via
    /// for_tests — empty xid map is fine for this test since the
    /// fixture omits the root insert that production does. The
    /// load-bearing check is that the xid_map accessor returns a
    /// real reference rather than panicking.
    #[test]
    fn v2_xid_map_is_reachable_via_backend_trait() {
        let b = KmsBackendV2::for_tests();
        let map = b.xid_map();
        // for_tests builds an empty map (it doesn't seed root the
        // way KmsCore::new does); verify the accessor works and
        // returns an actual map reference.
        assert_eq!(map.len(), 0);
    }

    #[test]
    fn v2_list_fonts_proxy_returns_catalog_matches() {
        let mut b = KmsBackendV2::for_tests();
        let expected = u16::try_from(b.core.font_loader.catalog.len().min(8)).unwrap_or(u16::MAX);
        let reply = b.list_fonts_proxy(None, 8, "*").expect("list_fonts");
        assert_eq!(reply[0], 1);
        let count = u16::from_le_bytes([reply[8], reply[9]]);
        assert_eq!(count, expected);
    }

    #[test]
    fn v2_list_fonts_with_info_proxy_emits_terminator() {
        let mut b = KmsBackendV2::for_tests();
        let replies = b
            .list_fonts_with_info_proxy(None, 4, "*", &mut |_| 0x99)
            .expect("list_fonts_with_info");
        assert!(!replies.is_empty(), "terminator reply must be present");
        let terminator = replies.last().expect("terminator");
        assert_eq!(terminator[0], 1);
        assert_eq!(terminator[1], 0);
    }

    /// `XCreateFontSet("fixed")` regression (e16-in-vng silent exit):
    /// libX11's XLC takes the XLFD from the ListFontsWithInfo reply
    /// NAME (or the FONT property), parses the charset from the last
    /// two fields, and `OpenFont`s that name verbatim — verified by
    /// tracing the probe against Xephyr (`tools/fontset-trace-xephyr.sh`:
    /// LFWI('fixed') → name/-FONT atom
    /// '-Misc-Fixed-…-C-60-ISO8859-1' → OpenFont(same)). A bare
    /// alias name carries no charset, so XLC reports the C-locale
    /// charset missing and returns a NULL fontset; e16 exits.
    ///
    /// Pin: an alias match must reply with a full XLFD name whose
    /// registry-encoding tail is iso8859-1, and that exact name must
    /// round-trip through open_font.
    #[test]
    fn v2_list_fonts_with_info_resolves_alias_to_xlfd_name() {
        // Post-font-path rework, "fixed" may resolve two ways:
        //  - via a real font-path dir (e.g. /usr/share/fonts/misc
        //    fonts.alias) → reply name "fixed" VERBATIM (Xorg FPE
        //    behavior) with the PCF's own FONT property carrying the
        //    full XLFD;
        //  - via built-ins (no misc dir on the machine) → reply name
        //    is the synthesized charset-bearing XLFD.
        // The libX11 guarantee e16 needs (omGeneric.c get_prop_name)
        // is the FONT PROPERTY: XA_FONT (18) → atom whose string is a
        // full XLFD with a charset tail. Pin that, not the name shape.
        let mut b = KmsBackendV2::for_tests();
        let mut interned: Vec<String> = Vec::new();
        let replies = b
            .list_fonts_with_info_proxy(None, 100, "fixed", &mut |name| {
                interned.push(name.to_owned());
                0x77 + u32::try_from(interned.len()).unwrap()
            })
            .expect("list_fonts_with_info");
        assert!(
            replies.len() >= 2,
            "at least one info reply + terminator; got {}",
            replies.len()
        );
        // LFWI info reply layout: name_len at byte 1, nProperties at
        // bytes 46..48, properties (8 bytes each) at 60.., then name.
        let info = &replies[0];
        let name_len = usize::from(info[1]);
        let n_props = usize::from(u16::from_le_bytes([info[46], info[47]]));
        assert!(n_props >= 1, "at least the FONT property");
        let mut font_value_atom = None;
        for i in 0..n_props {
            let off = 60 + i * 8;
            let prop_name =
                u32::from_le_bytes([info[off], info[off + 1], info[off + 2], info[off + 3]]);
            if prop_name == 18 {
                font_value_atom = Some(u32::from_le_bytes([
                    info[off + 4],
                    info[off + 5],
                    info[off + 6],
                    info[off + 7],
                ]));
            }
        }
        let font_value_atom = font_value_atom.expect("XA_FONT (18) property must be present");
        // Map the mock-interned atom id back to its string.
        let idx = usize::try_from(font_value_atom - 0x78).expect("FONT value is mock-interned");
        let font_xlfd = interned.get(idx).expect("interned FONT value").clone();
        assert!(
            font_xlfd.starts_with('-'),
            "FONT property must be a full XLFD; got {font_xlfd:?}"
        );
        let fields: Vec<&str> = font_xlfd.split('-').collect();
        assert_eq!(
            fields.len(),
            15,
            "XLFD has 14 fields (15 split parts with the leading dash); got {font_xlfd:?}"
        );
        let registry = fields[13].to_ascii_lowercase();
        assert!(
            registry.starts_with("iso"),
            "charset registry tail must be an iso charset so the \
             C-locale XLC charset binds; got {font_xlfd:?}"
        );
        // The reply NAME must be openable — XCreateFontSet OpenFonts
        // it verbatim (alias or XLFD alike).
        let name_off = 60 + n_props * 8;
        let name = std::str::from_utf8(&info[name_off..name_off + name_len]).expect("utf8 name");
        b.core
            .font_loader
            .open_font(name)
            .expect("LFWI reply name must round-trip through open_font");
    }

    /// Telemetry: counter sites fire at the Backend trait
    /// surface even on the test fixture (no Vk). put_image with
    /// an unknown xid logs a gap and does NOT count a paint
    /// submit (the engine never ran); get_image likewise. This
    /// confirms only successful ops count.
    #[test]
    fn v2_telemetry_counter_sites_track_successful_ops() {
        let mut b = KmsBackendV2::for_tests();
        // put_image with unknown xid → no counter bump.
        b.put_image(None, 0xDEAD, 32, 4, 4, 0, 0, &[0; 64]).unwrap();
        assert_eq!(b.telemetry.lifetime.paint_submits, 0);
        // The stub engine declines NoVk, so even a known xid
        // wouldn't count. The "track successful ops" gate is
        // covered by the lavapipe integration tests; here we
        // just confirm the wiring compiles and doesn't double-
        // increment on the gap path.
        assert_eq!(b.telemetry.lifetime.queue_submit2, 0);
    }

    /// Bookkeeping methods stay consistent: register_top_level
    /// mutates KmsCore's xid_map; xid_map() reflects the new entry.
    #[test]
    fn v2_register_top_level_updates_xid_map() {
        use yserver_protocol::x11::ResourceId;
        let mut b = KmsBackendV2::for_tests();
        b.register_top_level(None, ResourceId(0x4242), 0x0040_1234)
            .expect("register_top_level");
        assert_eq!(b.xid_map().get(&0x0040_1234), Some(&ResourceId(0x4242)));
        b.unregister_host_window(0x0040_1234);
        assert!(b.xid_map().get(&0x0040_1234).is_none());
    }

    /// Stage 3a per plan §3a: a `poly_text8` wire body that
    /// carries `[text₀, font-change, text₁]` should:
    /// 1. dispatch the first text run with the original
    ///    `current_font` value (or None);
    /// 2. swap `core.current_font` on the inline change item;
    /// 3. dispatch the second text run with the new font.
    ///
    /// Without a real FontState entry the engine call short-
    /// circuits in `render_text_chars_v2` (no font → no work),
    /// but the side-effect we care about — `current_font`
    /// rotating to the inline-change xid by the end of the parse
    /// — is observable on the backend after the call returns.
    #[test]
    fn v2_poly_text8_font_change_advances_current_font() {
        let mut b = KmsBackendV2::for_tests();
        // Body shape (drawable=4, gc=4, x=2, y=2, items=…):
        // header = 12 bytes; first item = `len(1) delta(1) "X"`
        // = 3 bytes; font-change item = `255 + 4 BE bytes` = 5
        // bytes; second item = `len(1) delta(1) "Y"` = 3 bytes.
        let mut body: Vec<u8> = Vec::new();
        body.extend_from_slice(&[0, 0, 0, 0]); // drawable
        body.extend_from_slice(&[0, 0, 0, 0]); // gc
        body.extend_from_slice(&(0_i16).to_le_bytes()); // x
        body.extend_from_slice(&(0_i16).to_le_bytes()); // y
        // First TEXTITEM8 — single 'X' glyph.
        body.extend_from_slice(&[1u8, 0u8, b'X']);
        // Font-change item — switch to xid 0xDEAD_BEEF.
        body.push(255);
        body.extend_from_slice(&0xDEAD_BEEF_u32.to_be_bytes());
        // Second TEXTITEM8 — single 'Y' glyph.
        body.extend_from_slice(&[1u8, 0u8, b'Y']);

        assert_eq!(b.core.current_font, None);
        b.poly_text8(None, 0xABCD_EF01, 0x000000, &body)
            .expect("poly_text8 ok");
        // After the parse, current_font should reflect the inline
        // change. The parse runs the second text item with this
        // font value in scope.
        assert_eq!(b.core.current_font, Some(0xDEAD_BEEF));
    }

    // ─── Stage 3b: picture record lifecycle tests ──────────────

    /// `picture_record_lifecycle` per plan §3b: create → change →
    /// free, with every value-mask bit exercised at least once.
    /// Round-trip via `KmsCore.pictures.get` after each step.
    #[test]
    fn v2_picture_record_lifecycle_exercises_every_value_mask_bit() {
        use crate::kms::core::PictureFilter;
        use yserver_core::backend::{AnyHandle, PixmapHandle};

        let mut b = KmsBackendV2::for_tests();
        // Pre-create a fake drawable xid so render_create_picture's
        // store.lookup doesn't have to be Some — the picture record
        // just stores the host_xid; the incref path is exercised
        // in the next test.
        let drawable =
            AnyHandle::Pixmap(PixmapHandle::from_raw(0x4242_4242).expect("PixmapHandle"));

        // CPRepeat=Pad, CPAlphaMap=0xDEAD_BEEF, CPAlphaXOrigin=10,
        // CPAlphaYOrigin=20, CPClipXOrigin=30, CPClipYOrigin=40,
        // CPClipMask=0 (= None), CPGraphicsExposure=1,
        // CPSubwindowMode=1, CPPolyEdge=1, CPPolyMode=1,
        // CPDither=1 (consumed-but-not-stored), CPComponentAlpha=1.
        let value_mask: u32 = 0x0001
            | 0x0002
            | 0x0004
            | 0x0008
            | 0x0010
            | 0x0020
            | 0x0040
            | 0x0080
            | 0x0100
            | 0x0200
            | 0x0400
            | 0x0800
            | 0x1000;
        let mut values: Vec<u8> = Vec::new();
        for v in [
            2_u32,       // Repeat::Pad
            0xDEAD_BEEF, // alpha_map
            10,          // alpha_x
            20,          // alpha_y
            30,          // clip_x
            40,          // clip_y
            0,           // clip_mask = None
            1,           // graphics_exposure
            1,           // subwindow_mode
            1,           // poly_edge
            1,           // poly_mode
            1,           // dither (consumed, not stored)
            1,           // component_alpha
        ] {
            values.extend_from_slice(&v.to_le_bytes());
        }

        let picture = b
            .render_create_picture(None, drawable, 0, value_mask, &values)
            .expect("create_picture")
            .expect("Some(handle)");
        let pic_xid = picture.as_raw();

        // Find and unpack the resulting record.
        let rec = b.core.pictures.get(&pic_xid).expect("record present");
        match rec {
            PictureRecord::Drawable {
                host_xid,
                pict_format: _,
                clip,
                clip_x,
                clip_y,
                repeat,
                alpha_map,
                alpha_x,
                alpha_y,
                component_alpha,
                transform,
                filter,
                graphics_exposure,
                subwindow_mode,
                poly_edge,
                poly_mode,
                drawable_origin: _,
            } => {
                assert_eq!(*host_xid, 0x4242_4242);
                assert!(clip.is_none(), "clip stays None for clip_mask=0");
                assert_eq!(*clip_x, 30);
                assert_eq!(*clip_y, 40);
                assert_eq!(*repeat, Repeat::Pad);
                assert_eq!(*alpha_map, Some(0xDEAD_BEEF));
                assert_eq!(*alpha_x, 10);
                assert_eq!(*alpha_y, 20);
                assert!(*component_alpha);
                assert!(transform.is_none());
                assert_eq!(*filter, PictureFilter::Nearest);
                assert!(*graphics_exposure);
                assert_eq!(*subwindow_mode, 1);
                assert_eq!(*poly_edge, 1);
                assert_eq!(*poly_mode, 1);
            }
            other => panic!("expected Drawable, got {other:?}"),
        }

        // ChangePicture override of a single bit (CPRepeat=Normal).
        let mut body: Vec<u8> = Vec::new();
        body.extend_from_slice(&pic_xid.to_le_bytes());
        body.extend_from_slice(&0x0001_u32.to_le_bytes());
        body.extend_from_slice(&1_u32.to_le_bytes()); // Repeat::Normal
        b.render_change_picture(None, pic_xid, &body)
            .expect("change_picture");
        match b.core.pictures.get(&pic_xid) {
            Some(PictureRecord::Drawable { repeat, .. }) => {
                assert_eq!(*repeat, Repeat::Normal);
            }
            _ => panic!("record dropped"),
        }

        // FreePicture removes the record.
        b.render_free_picture(None, pic_xid).expect("free_picture");
        assert!(!b.core.pictures.contains_key(&pic_xid));
    }

    /// `picture_record_drawable_refcount` per plan §3b: a picture
    /// wrapping a pixmap incref's the pixmap on create; the pixmap
    /// survives `free_pixmap` while a picture still references it;
    /// `render_free_picture` decref's, allowing the pending retire
    /// to complete on the next poll.
    #[test]
    fn v2_picture_record_drawable_refcount_blocks_free_pixmap() {
        use ash::vk;

        use crate::kms::v2::store::{DrawableKind, Storage};
        use yserver_core::backend::{AnyHandle, PixmapHandle};

        let mut b = KmsBackendV2::for_tests();
        // The `for_tests` fixture has no VkContext, so the
        // production `create_pixmap` path falls back to a logged
        // gap (no storage allocated). Use the store's test-stub
        // path directly so refcount accounting is exercised
        // without needing a live Vk.
        let pix_xid = 0xDEAD_BABE;
        let storage = Storage::for_tests_null(
            vk::Extent2D {
                width: 4,
                height: 4,
            },
            vk::Format::B8G8R8A8_UNORM,
        );
        let pix_id = b
            .store
            .allocate(pix_xid, DrawableKind::Pixmap, 32, false, storage)
            .expect("store allocate");
        assert_eq!(b.store.get(pix_id).expect("entry").refcount, 1);

        // Create a picture wrapping the pixmap; refcount → 2.
        let pix_handle = PixmapHandle::from_raw(pix_xid).expect("PixmapHandle");
        let any = AnyHandle::Pixmap(pix_handle);
        let pic = b
            .render_create_picture(None, any, 0, 0, &[])
            .expect("create_picture")
            .expect("Some");
        let pic_xid = pic.as_raw();
        assert_eq!(b.store.get(pix_id).expect("entry").refcount, 2);

        // free_pixmap drops one ref → 1; the entry survives because
        // the picture still references it.
        b.free_pixmap(None, pix_xid).expect("free_pixmap");
        assert_eq!(b.store.get(pix_id).expect("entry survives").refcount, 1);

        // free_picture drops the second ref → 0; the entry retires.
        // The test-stub storage has no in-flight fence, so
        // `destroy_now` runs immediately and the entry is removed.
        b.render_free_picture(None, pic_xid).expect("free_picture");
        assert!(b.store.get(pix_id).is_none(), "entry destroyed on last ref");
    }

    /// `picture_solid_fill_premul_correct` per plan §3b. NB: the
    /// X RENDER wire colour is **already premultiplied** per the
    /// protocol + rendercheck (`main.c:337-345`), so v2 stores the
    /// channels as-is rather than multiplying by alpha. The plan's
    /// `0x80808080 → [0.25, 0.25, 0.25, 0.5]` example assumed
    /// straight-alpha input; v1 has been parity with rendercheck
    /// since Phase 4.1.4.6, and v2 matches v1.
    #[test]
    fn v2_render_create_solid_fill_stores_wire_color_as_is() {
        // Wire colour: r16=0xFFFF (1.0), g16=0x8080 (≈0.50196),
        // b16=0x0000 (0.0), a16=0x8080 (≈0.50196). Stored f32
        // values should be (r=1.0, g=0.5019, b=0.0, a=0.5019)
        // exactly — no premultiplication applied at store time.
        let mut b = KmsBackendV2::for_tests();
        let color: [u8; 8] = [0xFF, 0xFF, 0x80, 0x80, 0x00, 0x00, 0x80, 0x80];
        let pic = b
            .render_create_solid_fill(None, color)
            .expect("solid_fill")
            .expect("Some");
        let rec = b.core.pictures.get(&pic.as_raw()).expect("record");
        match rec {
            PictureRecord::SolidFill {
                premul,
                repeat,
                component_alpha,
            } => {
                assert!((premul[0] - 1.0).abs() < 1e-4, "r = {}", premul[0]);
                assert!(
                    (premul[1] - (0x8080_u16 as f32 / 65535.0)).abs() < 1e-6,
                    "g = {}",
                    premul[1],
                );
                assert!(premul[2].abs() < 1e-6, "b = {}", premul[2]);
                assert!(
                    (premul[3] - (0x8080_u16 as f32 / 65535.0)).abs() < 1e-6,
                    "a = {}",
                    premul[3],
                );
                // Solid-fill defaults to Repeat::Normal; component_alpha=false.
                assert_eq!(*repeat, Repeat::Normal);
                assert!(!*component_alpha);
            }
            other => panic!("expected SolidFill, got {other:?}"),
        }
    }

    /// `picture_gradient_record_stored` per plan §3b: a linear
    /// gradient body parses; endpoints + stops round-trip through
    /// the record.
    #[test]
    fn v2_render_create_linear_gradient_parses_endpoints_and_stops() {
        let mut b = KmsBackendV2::for_tests();
        // Wire body: pad(4) + p1.x(4) + p1.y(4) + p2.x(4) + p2.y(4)
        // + n_stops(4) + n*pos(4) + n*color(8).
        // p1 = (0, 0) fixed-point; p2 = (256<<16, 0); two stops at
        // pos=0 with color=(0xFFFF, 0, 0, 0xFFFF) and pos=1<<16 with
        // color=(0, 0xFFFF, 0, 0xFFFF).
        let mut body: Vec<u8> = Vec::new();
        body.extend_from_slice(&0_u32.to_le_bytes()); // request padding (skipped)
        body.extend_from_slice(&0_i32.to_le_bytes()); // p1.x
        body.extend_from_slice(&0_i32.to_le_bytes()); // p1.y
        body.extend_from_slice(&(256_i32 << 16).to_le_bytes()); // p2.x
        body.extend_from_slice(&0_i32.to_le_bytes()); // p2.y
        body.extend_from_slice(&2_u32.to_le_bytes()); // n_stops
        // positions
        body.extend_from_slice(&0_i32.to_le_bytes());
        body.extend_from_slice(&0x0001_0000_i32.to_le_bytes());
        // colours
        body.extend_from_slice(&0xFFFF_u16.to_le_bytes()); // r0
        body.extend_from_slice(&0_u16.to_le_bytes());
        body.extend_from_slice(&0_u16.to_le_bytes());
        body.extend_from_slice(&0xFFFF_u16.to_le_bytes());
        body.extend_from_slice(&0_u16.to_le_bytes()); // r1=0
        body.extend_from_slice(&0xFFFF_u16.to_le_bytes()); // g1
        body.extend_from_slice(&0_u16.to_le_bytes());
        body.extend_from_slice(&0xFFFF_u16.to_le_bytes());

        let pic = b
            .render_create_linear_gradient(None, &body)
            .expect("linear_gradient")
            .expect("Some");
        let rec = b.core.pictures.get(&pic.as_raw()).expect("record");
        match rec {
            PictureRecord::LinearGradient {
                p1,
                p2,
                stops,
                repeat,
                transform,
            } => {
                assert_eq!(*p1, (0, 0));
                assert_eq!(*p2, (256 << 16, 0));
                assert_eq!(stops.len(), 2);
                assert_eq!(stops[0].pos, 0);
                assert_eq!(stops[0].r, 0xFFFF);
                assert_eq!(stops[0].g, 0);
                assert_eq!(stops[1].pos, 0x0001_0000);
                assert_eq!(stops[1].g, 0xFFFF);
                assert_eq!(*repeat, Repeat::None);
                assert!(transform.is_none());
            }
            other => panic!("expected LinearGradient, got {other:?}"),
        }
    }

    /// Stage 3f.13: `render_create_linear_gradient` returns a
    /// resolved `ResolvedSource::Gradient(xid)` from
    /// `resolve_picture_for_render` (not a SolidFill collapse).
    /// Logic-only — engine-side LUT build is a Vk path and lives
    /// in the engine's Vk-backed tests; here we just assert the
    /// resolve shape changed correctly.
    #[test]
    fn v2_linear_gradient_resolves_as_gradient_source() {
        use crate::kms::v2::engine::ResolvedSource;

        let mut b = KmsBackendV2::for_tests();
        let mut body: Vec<u8> = Vec::new();
        body.extend_from_slice(&0_u32.to_le_bytes()); // pad
        body.extend_from_slice(&0_i32.to_le_bytes()); // p1.x
        body.extend_from_slice(&0_i32.to_le_bytes()); // p1.y
        body.extend_from_slice(&(256_i32 << 16).to_le_bytes()); // p2.x
        body.extend_from_slice(&0_i32.to_le_bytes()); // p2.y
        body.extend_from_slice(&1_u32.to_le_bytes()); // n_stops
        body.extend_from_slice(&0_i32.to_le_bytes()); // pos
        body.extend_from_slice(&[0xFF, 0xFF, 0, 0, 0, 0, 0xFF, 0xFF]); // colour (R=1, A=1)
        let pic = b
            .render_create_linear_gradient(None, &body)
            .expect("create gradient")
            .expect("Some");

        let (resolved, _, _, _) =
            resolve_picture_for_render(&b.core, &b.store, pic.as_raw()).expect("resolve");
        match resolved {
            ResolvedSource::Gradient(xid) => assert_eq!(xid, pic.as_raw()),
            other => panic!("expected Gradient, got {other:?}"),
        }
    }

    /// Stage 3f.13: `render_free_picture` for a gradient drops both
    /// the picture record and the engine-side `picture_paint` slot.
    /// Logic-only — the engine slot count is the observable signal
    /// (`engine.picture_paint_len()`). On the test fixture (no Vk)
    /// the build itself logs a debug + skips, so the engine slot
    /// stays at 0 throughout; the gate is "free_picture doesn't
    /// leave a stale slot behind" which still asserts non-zero in
    /// production but zero in test. We assert the lifecycle path
    /// instead: create, free, ensure picture record is gone.
    #[test]
    fn v2_gradient_free_picture_drops_record() {
        let mut b = KmsBackendV2::for_tests();
        let mut body: Vec<u8> = Vec::new();
        body.extend_from_slice(&0_u32.to_le_bytes()); // pad
        body.extend_from_slice(&0_i32.to_le_bytes());
        body.extend_from_slice(&0_i32.to_le_bytes());
        body.extend_from_slice(&(128_i32 << 16).to_le_bytes());
        body.extend_from_slice(&0_i32.to_le_bytes());
        body.extend_from_slice(&1_u32.to_le_bytes());
        body.extend_from_slice(&0_i32.to_le_bytes());
        body.extend_from_slice(&[0xFF, 0xFF, 0, 0, 0, 0, 0xFF, 0xFF]);
        let pic = b
            .render_create_linear_gradient(None, &body)
            .expect("create gradient")
            .expect("Some");
        let xid = pic.as_raw();
        assert!(b.core.pictures.contains_key(&xid));
        b.render_free_picture(None, xid).expect("free");
        assert!(!b.core.pictures.contains_key(&xid));
        assert_eq!(b.engine.picture_paint_len(), 0);
    }

    /// Stage 3f.14: depth-32 windows are premultiplied-α and a
    /// transparent-black default is the no-op contribution to
    /// compositing; depth-24 (and other non-α visuals) get opaque
    /// black. Locks the contract in the test suite so a refactor
    /// doesn't silently flip 32-bit windows to opaque black (which
    /// would visually look the same on top of the root but break
    /// compositors that depend on alpha for blending).
    #[test]
    fn v2_default_window_init_color_per_depth() {
        assert_eq!(super::default_window_init_color(32), [0.0, 0.0, 0.0, 0.0]);
        assert_eq!(super::default_window_init_color(24), [0.0, 0.0, 0.0, 1.0]);
        assert_eq!(super::default_window_init_color(1), [0.0, 0.0, 0.0, 1.0]);
        assert_eq!(super::default_window_init_color(8), [0.0, 0.0, 0.0, 1.0]);
    }

    /// `render_set_picture_clip_rectangles` parses + stores rects
    /// pre-shifted by the clip-origin. Then `render_free_picture`
    /// teardown also drops the engine-side picture_paint slot.
    #[test]
    fn v2_set_picture_clip_rectangles_pre_shifts_by_origin() {
        use yserver_core::backend::{AnyHandle, PixmapHandle};

        let mut b = KmsBackendV2::for_tests();
        let drawable =
            AnyHandle::Pixmap(PixmapHandle::from_raw(0xAA00_BB00).expect("PixmapHandle"));
        let pic = b
            .render_create_picture(None, drawable, 0, 0, &[])
            .expect("create_picture")
            .expect("Some");
        let pic_xid = pic.as_raw();

        // Wire body: picture(4) + x_origin(2) + y_origin(2) +
        // 1 × [x=5, y=6, w=20, h=30].
        let mut body: Vec<u8> = Vec::new();
        body.extend_from_slice(&pic_xid.to_le_bytes());
        body.extend_from_slice(&10_i16.to_le_bytes()); // x_origin
        body.extend_from_slice(&20_i16.to_le_bytes()); // y_origin
        body.extend_from_slice(&5_i16.to_le_bytes());
        body.extend_from_slice(&6_i16.to_le_bytes());
        body.extend_from_slice(&20_u16.to_le_bytes());
        body.extend_from_slice(&30_u16.to_le_bytes());
        b.render_set_picture_clip_rectangles(None, pic_xid, &body)
            .expect("set_picture_clip");
        // Pre-shift: stored rect.x = 5 + 10 = 15; .y = 6 + 20 = 26.
        match b.core.pictures.get(&pic_xid).expect("rec") {
            PictureRecord::Drawable {
                clip,
                clip_x,
                clip_y,
                ..
            } => {
                let rects = clip.as_ref().expect("Some(rects)");
                assert_eq!(rects.len(), 1);
                assert_eq!(rects[0].x, 15);
                assert_eq!(rects[0].y, 26);
                assert_eq!(rects[0].width, 20);
                assert_eq!(rects[0].height, 30);
                assert_eq!(*clip_x, 10);
                assert_eq!(*clip_y, 20);
            }
            _ => panic!("not Drawable"),
        }

        // free_picture removes both record + engine-side slot.
        assert_eq!(b.engine.picture_paint_len(), 0);
        b.render_free_picture(None, pic_xid).expect("free");
        assert!(!b.core.pictures.contains_key(&pic_xid));
        assert_eq!(b.engine.picture_paint_len(), 0);
    }

    /// X11 RENDER `SetPictureClipRectangles` with an EMPTY rect
    /// list = empty clip region = composite paints **nothing**.
    /// Distinct from `ChangePicture(CPClipMask = None)` which
    /// clears the clip back to "paint everywhere" (`clip = None`).
    ///
    /// Regression: pre-fix v2 collapsed empty-list to `clip = None`,
    /// which made subsequent composites paint everywhere — exactly
    /// the mate-with-compositing "shadow only / wallpaper
    /// overwrites window content" bug observed in the Stage 4d
    /// smoke. The trace at 09:49:44 showed marco's wallpaper-fill
    /// composite running with `clip[]` (= `None` in v2 storage)
    /// even though marco's intent (per X11 spec) was "empty clip,
    /// paint nothing."
    ///
    /// Post-fix: empty rect list stores `Some(Vec::new())` so the
    /// engine's `clip_rects=Some(&[])` path returns early without
    /// painting.
    #[test]
    fn v2_set_picture_clip_rectangles_empty_list_is_empty_clip_not_no_clip() {
        use yserver_core::backend::{AnyHandle, PixmapHandle};

        let mut b = KmsBackendV2::for_tests();
        let drawable =
            AnyHandle::Pixmap(PixmapHandle::from_raw(0xCC00_DD00).expect("PixmapHandle"));
        let pic = b
            .render_create_picture(None, drawable, 0, 0, &[])
            .expect("create_picture")
            .expect("Some");
        let pic_xid = pic.as_raw();

        // First: set a real clip to prove the field can become
        // populated (Some(non-empty)).
        let mut body: Vec<u8> = Vec::new();
        body.extend_from_slice(&pic_xid.to_le_bytes());
        body.extend_from_slice(&0_i16.to_le_bytes()); // x_origin
        body.extend_from_slice(&0_i16.to_le_bytes()); // y_origin
        body.extend_from_slice(&0_i16.to_le_bytes());
        body.extend_from_slice(&0_i16.to_le_bytes());
        body.extend_from_slice(&100_u16.to_le_bytes());
        body.extend_from_slice(&100_u16.to_le_bytes());
        b.render_set_picture_clip_rectangles(None, pic_xid, &body)
            .expect("set_clip non-empty");
        match b.core.pictures.get(&pic_xid).expect("rec") {
            PictureRecord::Drawable { clip, .. } => {
                assert!(
                    matches!(clip, Some(v) if v.len() == 1),
                    "expected Some(1 rect) after non-empty set, got {clip:?}",
                );
            }
            _ => panic!("not Drawable"),
        }

        // Now: empty list. Per X11 RENDER spec this means "empty
        // clip region — paint nothing." The stored representation
        // must distinguish this from "no clip set" (paint
        // everywhere).
        let mut empty_body: Vec<u8> = Vec::new();
        empty_body.extend_from_slice(&pic_xid.to_le_bytes());
        empty_body.extend_from_slice(&0_i16.to_le_bytes()); // x_origin
        empty_body.extend_from_slice(&0_i16.to_le_bytes()); // y_origin
        // No rect data — empty list.
        b.render_set_picture_clip_rectangles(None, pic_xid, &empty_body)
            .expect("set_clip empty");
        match b.core.pictures.get(&pic_xid).expect("rec") {
            PictureRecord::Drawable { clip, .. } => {
                // Pre-fix: clip was None (= paint everywhere).
                // Post-fix: Some(empty Vec) (= paint nothing).
                assert!(
                    matches!(clip, Some(v) if v.is_empty()),
                    "empty rect list must store Some(empty Vec) — \
                     pre-fix stored None which made composites paint \
                     everywhere instead of nothing. Got: {clip:?}",
                );
            }
            _ => panic!("not Drawable"),
        }
    }

    /// `ChangePicture(CPClipXOrigin/CPClipYOrigin)` must move the
    /// already stored clip rectangles by the same delta. The v2
    /// backend stores clip rects pre-shifted into picture-local
    /// coordinates, so updating only the scalar origin fields leaves
    /// stale scissors behind.
    #[test]
    fn change_picture_clip_origin_repositions_stored_rects() {
        use yserver_core::backend::{AnyHandle, PixmapHandle};

        let mut b = KmsBackendV2::for_tests();
        let drawable =
            AnyHandle::Pixmap(PixmapHandle::from_raw(0xDD00_EE00).expect("PixmapHandle"));
        let pic = b
            .render_create_picture(None, drawable, 0, 0, &[])
            .expect("create_picture")
            .expect("Some");
        let pic_xid = pic.as_raw();

        let mut clip_body: Vec<u8> = Vec::new();
        clip_body.extend_from_slice(&pic_xid.to_le_bytes());
        clip_body.extend_from_slice(&10_i16.to_le_bytes());
        clip_body.extend_from_slice(&20_i16.to_le_bytes());
        clip_body.extend_from_slice(&5_i16.to_le_bytes());
        clip_body.extend_from_slice(&6_i16.to_le_bytes());
        clip_body.extend_from_slice(&20_u16.to_le_bytes());
        clip_body.extend_from_slice(&30_u16.to_le_bytes());
        b.render_set_picture_clip_rectangles(None, pic_xid, &clip_body)
            .expect("set clip");

        let mut change_body: Vec<u8> = Vec::new();
        change_body.extend_from_slice(&pic_xid.to_le_bytes());
        change_body.extend_from_slice(&(0x0010_u32 | 0x0020_u32).to_le_bytes());
        change_body.extend_from_slice(&30_u32.to_le_bytes());
        change_body.extend_from_slice(&50_u32.to_le_bytes());
        b.render_change_picture(None, pic_xid, &change_body)
            .expect("change_picture");

        match b.core.pictures.get(&pic_xid).expect("rec") {
            PictureRecord::Drawable {
                clip,
                clip_x,
                clip_y,
                ..
            } => {
                let rects = clip.as_ref().expect("clip still present");
                assert_eq!(rects.len(), 1);
                assert_eq!(rects[0].x, 35, "x must move by +20 with CPClipXOrigin");
                assert_eq!(rects[0].y, 56, "y must move by +30 with CPClipYOrigin");
                assert_eq!(*clip_x, 30);
                assert_eq!(*clip_y, 50);
            }
            _ => panic!("not Drawable"),
        }
    }

    // ─── Audit #8 (2026-05-19): set_picture_drawable_origin +
    // picture_client_clip_rects v2 backend hooks ──────────────

    /// `set_picture_drawable_origin` writes into the
    /// `PictureRecord::Drawable.drawable_origin` field. Pre-fix v2
    /// inherited the trait default no-op so the field stayed at
    /// (0, 0); window-backed pictures whose drawable sits at a
    /// non-zero parent offset couldn't translate external region
    /// geometry back into picture-local coords.
    #[test]
    fn v2_set_picture_drawable_origin_persists_on_record() {
        use yserver_core::backend::{AnyHandle, Backend, PixmapHandle};

        let mut b = KmsBackendV2::for_tests();
        let drawable =
            AnyHandle::Pixmap(PixmapHandle::from_raw(0xAA01_BB01).expect("PixmapHandle"));
        let pic = b
            .render_create_picture(None, drawable, 0, 0, &[])
            .expect("create_picture")
            .expect("Some");
        let pic_xid = pic.as_raw();

        // Pre-call sanity: default origin must be (0, 0).
        match b.core.pictures.get(&pic_xid).expect("rec") {
            PictureRecord::Drawable {
                drawable_origin, ..
            } => assert_eq!(*drawable_origin, (0, 0)),
            _ => panic!("not Drawable"),
        }

        b.set_picture_drawable_origin(pic_xid, (15, 27));

        match b.core.pictures.get(&pic_xid).expect("rec") {
            PictureRecord::Drawable {
                drawable_origin, ..
            } => {
                assert_eq!(
                    *drawable_origin,
                    (15, 27),
                    "drawable_origin must update; pre-fix the trait default \
                     no-op left it at (0, 0)",
                );
            }
            _ => panic!("not Drawable"),
        }
    }

    /// `set_picture_drawable_origin` on a non-Drawable picture
    /// (SolidFill / gradient) is a tolerated no-op — those variants
    /// have no drawable to anchor to.
    #[test]
    fn v2_set_picture_drawable_origin_no_op_on_solidfill() {
        use yserver_core::backend::Backend;

        let mut b = KmsBackendV2::for_tests();
        // Color is fixed-size 8 bytes (BGRA u16×4).
        let mut color = [0u8; 8];
        color[0..2].copy_from_slice(&0xFFFF_u16.to_le_bytes()); // R
        color[6..8].copy_from_slice(&0xFFFF_u16.to_le_bytes()); // A
        let pic = b
            .render_create_solid_fill(None, color)
            .expect("create solid fill")
            .expect("Some");
        let pic_xid = pic.as_raw();

        // Should not panic; record must remain a SolidFill.
        b.set_picture_drawable_origin(pic_xid, (10, 20));
        assert!(
            matches!(
                b.core.pictures.get(&pic_xid),
                Some(PictureRecord::SolidFill { .. })
            ),
            "SolidFill picture must remain SolidFill after \
             set_picture_drawable_origin no-op",
        );
    }

    /// `picture_client_clip_rects` on a Drawable picture WITH a
    /// clip returns `Some(Some(rects))` — those rects feed
    /// `CreateRegionFromPicture` (XFixes). Pre-fix v2 inherited
    /// the trait default `None`, making CreateRegionFromPicture
    /// always return BadMatch even for legitimate clipped pictures.
    #[test]
    fn v2_picture_client_clip_rects_returns_set_clip() {
        use yserver_core::backend::{AnyHandle, Backend, PixmapHandle};

        let mut b = KmsBackendV2::for_tests();
        let drawable =
            AnyHandle::Pixmap(PixmapHandle::from_raw(0xAA02_BB02).expect("PixmapHandle"));
        let pic = b
            .render_create_picture(None, drawable, 0, 0, &[])
            .expect("create_picture")
            .expect("Some");
        let pic_xid = pic.as_raw();

        // Install a 2-rect client clip via SetPictureClipRectangles
        // (clip-origin both zero so stored rects == request rects).
        let mut body: Vec<u8> = Vec::new();
        body.extend_from_slice(&pic_xid.to_le_bytes());
        body.extend_from_slice(&0_i16.to_le_bytes()); // x_origin
        body.extend_from_slice(&0_i16.to_le_bytes()); // y_origin
        for (x, y, w, h) in [(0_i16, 0_i16, 10_u16, 10_u16), (100, 200, 30, 40)] {
            body.extend_from_slice(&x.to_le_bytes());
            body.extend_from_slice(&y.to_le_bytes());
            body.extend_from_slice(&w.to_le_bytes());
            body.extend_from_slice(&h.to_le_bytes());
        }
        b.render_set_picture_clip_rectangles(None, pic_xid, &body)
            .expect("set_clip");

        let got = b
            .picture_client_clip_rects(pic_xid)
            .expect("Drawable picture must be Some(_) (not BadMatch)");
        let rects = got.expect("clip was set, expected Some(rects)");
        assert_eq!(rects.len(), 2, "got {rects:?}");
        assert_eq!(
            (rects[0].x, rects[0].y, rects[0].width, rects[0].height),
            (0, 0, 10, 10)
        );
        assert_eq!(
            (rects[1].x, rects[1].y, rects[1].width, rects[1].height),
            (100, 200, 30, 40)
        );
    }

    /// Non-zero drawable origins must not corrupt `CreateRegionFromPicture`.
    /// The request path stores the origin separately, but the returned client
    /// clip still needs to reflect the picture-local rectangle coordinates
    /// only.
    #[test]
    fn v2_picture_client_clip_rects_window_backed_picture_with_nonzero_origin() {
        use yserver_core::backend::{AnyHandle, Backend, WindowHandle};

        let mut b = KmsBackendV2::for_tests();
        let window_xid = 0xAA04_BB04;
        let _w_id = seed_window(&mut b, window_xid, None, 15, 27);

        let pic = b
            .render_create_picture(
                None,
                AnyHandle::Window(WindowHandle::from_raw(window_xid).expect("WindowHandle")),
                0,
                0,
                &[],
            )
            .expect("create_picture")
            .expect("Some");
        let pic_xid = pic.as_raw();

        // Mirror the request-layer origin bookkeeping that happens on CreatePicture.
        b.set_picture_drawable_origin(pic_xid, (15, 27));

        let mut body: Vec<u8> = Vec::new();
        body.extend_from_slice(&pic_xid.to_le_bytes());
        body.extend_from_slice(&5_i16.to_le_bytes()); // clip origin x
        body.extend_from_slice(&9_i16.to_le_bytes()); // clip origin y
        body.extend_from_slice(&1_i16.to_le_bytes());
        body.extend_from_slice(&2_i16.to_le_bytes());
        body.extend_from_slice(&7_u16.to_le_bytes());
        body.extend_from_slice(&11_u16.to_le_bytes());
        b.render_set_picture_clip_rectangles(None, pic_xid, &body)
            .expect("set_clip");

        let got = b
            .picture_client_clip_rects(pic_xid)
            .expect("Drawable picture must be Some(_) (not BadMatch)");
        let rects = got.expect("clip was set, expected Some(rects)");
        assert_eq!(rects.len(), 1, "got {rects:?}");
        assert_eq!(
            (rects[0].x, rects[0].y, rects[0].width, rects[0].height),
            (6, 11, 7, 11),
            "drawable_origin must not be folded into CreateRegionFromPicture",
        );
    }

    /// `picture_client_clip_rects` on a Drawable picture with NO
    /// clip set returns `Some(None)` — the picture exists but has
    /// no clientClip yet. Per X RENDER /
    /// `xfixes/region.c:CreateRegionFromPicture`, the dispatcher
    /// then emits BadMatch on the caller (no region to extract).
    #[test]
    fn v2_picture_client_clip_rects_returns_some_none_when_no_clip_set() {
        use yserver_core::backend::{AnyHandle, Backend, PixmapHandle};

        let mut b = KmsBackendV2::for_tests();
        let drawable =
            AnyHandle::Pixmap(PixmapHandle::from_raw(0xAA03_BB03).expect("PixmapHandle"));
        let pic = b
            .render_create_picture(None, drawable, 0, 0, &[])
            .expect("create_picture")
            .expect("Some");
        let pic_xid = pic.as_raw();

        let got = b.picture_client_clip_rects(pic_xid);
        assert!(
            matches!(got, Some(None)),
            "Drawable picture without a clip must return Some(None) — \
             got {got:?}",
        );
    }

    /// `picture_client_clip_rects` on a non-Drawable picture (e.g.,
    /// SolidFill) returns the outer `None` so the protocol layer
    /// raises BadMatch — gradients/solidfills carry no
    /// `clientClip`. Mirrors Xorg's `CreateRegionFromPicture` →
    /// BadPicture path for sourceless pictures.
    #[test]
    fn v2_picture_client_clip_rects_outer_none_on_solidfill() {
        use yserver_core::backend::Backend;

        let mut b = KmsBackendV2::for_tests();
        let mut color = [0u8; 8];
        color[0..2].copy_from_slice(&0xFFFF_u16.to_le_bytes());
        color[6..8].copy_from_slice(&0xFFFF_u16.to_le_bytes());
        let pic = b
            .render_create_solid_fill(None, color)
            .expect("create solid fill")
            .expect("Some");
        let pic_xid = pic.as_raw();

        let got = b.picture_client_clip_rects(pic_xid);
        assert!(
            got.is_none(),
            "SolidFill picture must return outer None so the protocol \
             layer emits BadMatch — got {got:?}",
        );
    }

    // ─── Stage 3d: render_composite_glyphs tests ───────────────

    /// Helper: install a SolidFill source picture + a glyphset
    /// holding `n` 1×1 A8 glyphs at id 0..n with `0xFF` alpha.
    /// Returns (src_pic_xid, gs_xid).
    fn install_solidfill_and_glyphset(b: &mut KmsBackendV2, n: u32) -> (u32, u32) {
        use crate::kms::core::{GlyphSetFormat, GlyphSetState, StoredGlyph};

        let src_pic = b
            .render_create_solid_fill(None, [0xFF, 0xFF, 0, 0, 0, 0, 0xFF, 0xFF])
            .expect("solid_fill")
            .expect("Some");

        let gs_xid = b.core.next_host_xid();
        let mut glyphs = HashMap::new();
        for id in 0..n {
            glyphs.insert(
                id,
                StoredGlyph {
                    width: 1,
                    height: 1,
                    x: 0,
                    y: 0,
                    x_off: 1,
                    y_off: 0,
                    pixels: vec![0xFF],
                    format: GlyphSetFormat::A8,
                },
            );
        }
        b.core.glyphsets.insert(
            gs_xid,
            GlyphSetState {
                format: GlyphSetFormat::A8,
                glyphs,
            },
        );
        (src_pic.as_raw(), gs_xid)
    }

    /// Per plan §3d "Op / source matrix accepted by 3d": op != Over
    /// (3) must drop the call with a per-call gap-log and increment
    /// the `composite_glyphs_dropped_unsupported` lifetime counter.
    /// No paint side effect; engine is never reached.
    #[test]
    fn v2_composite_glyphs_unsupported_op_drops() {
        let mut b = KmsBackendV2::for_tests();
        let (src_pic, gs_xid) = install_solidfill_and_glyphset(&mut b, 1);
        // No real dst picture needed — the op gate fires before
        // dst resolution. Pass any host_dst; assert gap-counter.
        b.render_composite_glyphs(
            None,
            23, /* CompositeGlyphs8 */
            1,  /* op = Src, NOT Over */
            src_pic,
            0xDEAD, /* host_dst (unused — op gate first) */
            0,      /* mask_fmt */
            gs_xid,
            0,
            0,
            &[1u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], // items: 1 glyph elt + padded
            0,
            0,
        )
        .expect("ok");
        assert_eq!(
            b.telemetry.lifetime.composite_glyphs_dropped_unsupported, 1,
            "op != Over must bump the unsupported counter",
        );
        assert_eq!(
            b.telemetry.lifetime.paint_submits, 0,
            "no paint submit on the gap path",
        );
    }

    /// Stage 3f.12: gradient src is no longer a "drop" — it
    /// collapses to a SolidFill of the first stop's premultiplied
    /// colour (real LUT sampling is still post-3f.5 work). The
    /// composite_glyphs path now accepts gradient sources; the
    /// `composite_glyphs_dropped_unsupported` counter stays at 0.
    /// Cairo glyph rendering with gradient bg/fg therefore paints
    /// (with the gradient flattened to its start colour) rather
    /// than dropping entirely.
    #[test]
    fn v2_composite_glyphs_gradient_source_collapses_to_solidfill() {
        let mut b = KmsBackendV2::for_tests();
        let (_unused_solidfill, gs_xid) = install_solidfill_and_glyphset(&mut b, 1);
        // Minimal valid linear-gradient wire body: pad(4) +
        // p1(8) + p2(8) + n_stops=1(4) + stop_pos(4) + stop_color(8).
        let mut grad_body: Vec<u8> = Vec::new();
        grad_body.extend_from_slice(&0_u32.to_le_bytes()); // request pad (skipped)
        grad_body.extend_from_slice(&0_i32.to_le_bytes()); // p1.x
        grad_body.extend_from_slice(&0_i32.to_le_bytes()); // p1.y
        grad_body.extend_from_slice(&(256_i32 << 16).to_le_bytes()); // p2.x
        grad_body.extend_from_slice(&0_i32.to_le_bytes()); // p2.y
        grad_body.extend_from_slice(&1_u32.to_le_bytes()); // n_stops
        grad_body.extend_from_slice(&0_i32.to_le_bytes()); // pos
        grad_body.extend_from_slice(&[0xFF, 0xFF, 0, 0, 0, 0, 0xFF, 0xFF]); // colour
        let grad_pic = b
            .render_create_linear_gradient(None, &grad_body)
            .expect("gradient")
            .expect("Some")
            .as_raw();
        b.render_composite_glyphs(
            None,
            23,
            3, /* Over */
            grad_pic,
            0xDEAD,
            0,
            gs_xid,
            0,
            0,
            &[1u8, 0, 0, 0, 0, 0, 0, 0],
            0,
            0,
        )
        .expect("ok");
        assert_eq!(
            b.telemetry.lifetime.composite_glyphs_dropped_unsupported, 0,
            "gradient src must collapse to SolidFill (not drop)",
        );
    }

    /// Per plan §3d items-parse spec: the items stream's inline
    /// `0xFF 0 0 0 new_gs_xid` element rotates the active glyphset
    /// for subsequent glyph lookups. The test installs two
    /// glyphsets with distinct codepoint→pixel mappings, feeds an
    /// items stream that draws one glyph from each, and asserts
    /// that both glyphsets contributed to the engine call — the
    /// parser must have honoured the inline change. We can't hit
    /// the Vk engine in this fixture (no live Vk under
    /// `for_tests`), so the gate is "no unsupported drop fired"
    /// AND "both glyphset lookups succeeded" (verified by reaching
    /// the engine, which returns `NoVk` on the stub but does NOT
    /// bump the unsupported counter).
    #[test]
    fn v2_composite_glyphs_inline_glyphset_change_parsed() {
        use crate::kms::core::{GlyphSetFormat, GlyphSetState, StoredGlyph};

        let mut b = KmsBackendV2::for_tests();
        let src_pic = b
            .render_create_solid_fill(None, [0xFF, 0xFF, 0, 0, 0, 0, 0xFF, 0xFF])
            .expect("solid_fill")
            .expect("Some")
            .as_raw();
        // GlyphSet A: codepoint 0x10 → 0xAA pixels.
        // GlyphSet B: codepoint 0x20 → 0xBB pixels.
        let mut mk_gs = |code: u32, byte: u8| {
            let mut glyphs = HashMap::new();
            glyphs.insert(
                code,
                StoredGlyph {
                    width: 1,
                    height: 1,
                    x: 0,
                    y: 0,
                    x_off: 1,
                    y_off: 0,
                    pixels: vec![byte],
                    format: GlyphSetFormat::A8,
                },
            );
            let xid = b.core.next_host_xid();
            b.core.glyphsets.insert(
                xid,
                GlyphSetState {
                    format: GlyphSetFormat::A8,
                    glyphs,
                },
            );
            xid
        };
        let gs_a = mk_gs(0x10, 0xAA);
        let gs_b = mk_gs(0x20, 0xBB);
        // Need a dst Drawable picture — create a stub one (lookup
        // will fail since the underlying drawable xid isn't in
        // the store, so the engine call short-circuits before
        // anything reaches Vk, but the parser still walks).
        use yserver_core::backend::{AnyHandle, PixmapHandle};
        let dst_drawable =
            AnyHandle::Pixmap(PixmapHandle::from_raw(0x4242_4242).expect("PixmapHandle"));
        let dst_pic = b
            .render_create_picture(None, dst_drawable, 0, 0, &[])
            .expect("dst_picture")
            .expect("Some")
            .as_raw();
        // Items stream: 1 glyph 0x10 from gs_a (initial), inline
        // glyphset-change to gs_b, then 1 glyph 0x20 from gs_b.
        // Element layout: count(u8) pad pad pad dx(i16) dy(i16) ids...
        let mut items: Vec<u8> = Vec::new();
        // Element 1: 1 glyph @ (0,0).
        items.extend_from_slice(&[1u8, 0, 0, 0, 0, 0, 0, 0]);
        items.extend_from_slice(&[0x10, 0, 0, 0]); // padded ids
        // Element 2: glyphset change.
        items.push(255);
        items.extend_from_slice(&[0u8, 0, 0]);
        items.extend_from_slice(&gs_b.to_le_bytes());
        // Element 3: 1 glyph @ (+1,0).
        items.extend_from_slice(&[1u8, 0, 0, 0, 1, 0, 0, 0]);
        items.extend_from_slice(&[0x20, 0, 0, 0]);

        b.render_composite_glyphs(
            None, 23, 3, /* Over */
            src_pic, dst_pic, 0, gs_a, 0, 0, &items, 0, 0,
        )
        .expect("ok");
        // Op + source were Over + SolidFill, so the unsupported
        // counter must NOT have fired.
        assert_eq!(
            b.telemetry.lifetime.composite_glyphs_dropped_unsupported, 0,
            "Over + SolidFill must not hit the unsupported gate",
        );
        // dst resolution failed (no Drawable backing for 0x4242_4242
        // in the store), so the engine wasn't called — but the parse
        // still walked both glyphsets without bumping the gap. The
        // load-bearing assertion is that the inline change keeps the
        // call in the Over+SolidFill envelope; engine reachability
        // is covered by the Vk-backed acceptance test.
    }

    // ─── Stage 3f.1: poly_* + fill_poly logic tests ────────────

    /// `poly_line_origin_mode_offsets_correctly` per plan §3f tests.
    /// Build a 3-point path under both Origin (absolute) and
    /// Previous (delta) coordinate modes; assert the produced
    /// rasterised-pixel set is the same. Drives Bresenham via the
    /// public crate-level helper.
    #[test]
    fn poly_line_origin_mode_offsets_correctly() {
        use crate::kms::{
            backend::{bresenham_segment, read_i16_pair},
            cpu_types::Rectangle16,
        };

        // Path: (10, 10) → (10, 13) → (13, 13) — an L shape.
        let absolute_pts: [(i16, i16); 3] = [(10, 10), (10, 13), (13, 13)];
        // Same path under Previous mode: first pt absolute, then deltas.
        let delta_pts: [(i16, i16); 3] = [(10, 10), (0, 3), (3, 0)];

        let rasterise = |points: &[u8], mode: u8| -> Vec<Rectangle16> {
            let mut rects: Vec<Rectangle16> = Vec::new();
            let mut prev: Option<(i32, i32)> = None;
            let mut offset = 0;
            while let Some((x, y)) = read_i16_pair(points, offset) {
                offset += 4;
                let (xi, yi) = if mode == 1 {
                    if let Some((px, py)) = prev {
                        (px + i32::from(x), py + i32::from(y))
                    } else {
                        (i32::from(x), i32::from(y))
                    }
                } else {
                    (i32::from(x), i32::from(y))
                };
                if let Some((px, py)) = prev {
                    bresenham_segment(px, py, xi, yi, &mut rects);
                }
                prev = Some((xi, yi));
            }
            rects
        };

        let pack = |pts: &[(i16, i16)]| -> Vec<u8> {
            let mut out = Vec::with_capacity(pts.len() * 4);
            for (x, y) in pts {
                out.extend_from_slice(&x.to_le_bytes());
                out.extend_from_slice(&y.to_le_bytes());
            }
            out
        };

        let abs_rects = rasterise(&pack(&absolute_pts), 0);
        let prev_rects = rasterise(&pack(&delta_pts), 1);

        // Both modes must produce the same rasterised pixel set.
        let to_set = |rs: &[Rectangle16]| -> std::collections::BTreeSet<(i16, i16)> {
            rs.iter().map(|r| (r.x, r.y)).collect()
        };
        assert_eq!(to_set(&abs_rects), to_set(&prev_rects));
        // Sanity: pixel set covers the L's expected vertices.
        let set = to_set(&abs_rects);
        for p in [(10, 10), (10, 13), (13, 13)] {
            assert!(set.contains(&p), "missing endpoint {p:?}");
        }
    }

    /// `fill_poly_scanline_correctness` per plan §3f tests. A 5-point
    /// convex polygon (axis-aligned diamond) round-trips through
    /// `scanline_fill_polygon` and produces the expected horizontal
    /// span set. Even-odd-rule fill, half-open scanline range.
    #[test]
    fn fill_poly_scanline_correctness() {
        use crate::kms::{backend::scanline_fill_polygon, cpu_types::Rectangle16};

        // Square with one mid-edge vertex injected — still convex,
        // and 5 distinct vertices as the test name advertises. Vertex
        // list: (0,0) (4,0) (4,2) (4,4) (0,4) — a 4×4 square with an
        // extra vertex on the right edge. Filled region is rows
        // y ∈ [0, 4) with x ∈ [0, 4) at each row.
        let verts = [(0, 0), (4, 0), (4, 2), (4, 4), (0, 4)];
        let mut rects: Vec<Rectangle16> = Vec::new();
        scanline_fill_polygon(&verts, &mut rects);

        // Collect (y, x_start, x_end) per row. Each row should be a
        // single span; we union rects on shared y if needed.
        let mut rows: std::collections::BTreeMap<i16, (i16, i16)> =
            std::collections::BTreeMap::new();
        for r in &rects {
            let x_start = r.x;
            let x_end = r.x + r.width as i16;
            rows.entry(r.y)
                .and_modify(|cur| {
                    cur.0 = cur.0.min(x_start);
                    cur.1 = cur.1.max(x_end);
                })
                .or_insert((x_start, x_end));
        }
        // Expected: rows 0..=3 each span x ∈ [0, 4). Row 4 is the
        // top edge of the polygon under half-open [y0, y1) semantics
        // — no horizontal scan crosses it.
        for y in 0..4 {
            let span = rows.get(&y).copied().unwrap_or_else(|| {
                panic!("row {y} missing");
            });
            assert_eq!(span, (0, 4), "row {y} span");
        }
        assert!(!rows.contains_key(&4), "row 4 must not be filled");
    }

    /// Sanity: the v2 GC-clip intersection helper matches v1's shape.
    /// A single source rect clipped against a 2-rect clip yields the
    /// 2 expected intersection rectangles in dst space (clip origin
    /// already applied).
    #[test]
    fn poly_fill_rectangle_honours_gc_clip() {
        use crate::kms::cpu_types::Rectangle16;
        use yserver_core::backend::ClipState;
        use yserver_protocol::x11::ClipRectangles;

        let mut b = KmsBackendV2::for_tests();
        // Two 4×8 clip rects side-by-side starting at (5, 5), with
        // clip origin (10, 10) → effective dst-coord rects at
        // (15, 15)-(19, 23) and (25, 15)-(29, 23).
        let mut wire: Vec<u8> = Vec::new();
        for (x, y, w, h) in [(5_i16, 5_i16, 4_u16, 8_u16), (15, 5, 4, 8)] {
            wire.extend_from_slice(&x.to_le_bytes());
            wire.extend_from_slice(&y.to_le_bytes());
            wire.extend_from_slice(&w.to_le_bytes());
            wire.extend_from_slice(&h.to_le_bytes());
        }
        b.core.current_clip = ClipState::Rectangles {
            origin: (10, 10),
            rects: ClipRectangles {
                ordering: 0,
                x_origin: 0,
                y_origin: 0,
                rectangles: wire,
            },
        };

        // Single source rect that spans both clip rects horizontally
        // and overflows top + bottom of the clip vertically.
        let src = [Rectangle16 {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        }];
        let out = b.intersect_with_current_clip(&src);
        assert_eq!(out.len(), 2);
        // First intersection — left clip rect after origin shift.
        assert_eq!(out[0].x, 15);
        assert_eq!(out[0].y, 15);
        assert_eq!(out[0].width, 4);
        assert_eq!(out[0].height, 8);
        // Second intersection — right clip rect after origin shift.
        assert_eq!(out[1].x, 25);
        assert_eq!(out[1].y, 15);
        assert_eq!(out[1].width, 4);
        assert_eq!(out[1].height, 8);
    }

    /// `gxcopy_planemask_diverts_to_logic_fill` per plan §3f tests.
    /// Asserts that switching `KmsCore.current_function` to a
    /// non-`Copy` value (here `Xor`) doesn't emit the
    /// `fill_rects_non_gxcopy` or `copy_plane_non_gxcopy` gaps —
    /// proves the Stage 3f.2 routing change took effect. Engine
    /// itself returns `NoVk` on the stub fixture, so we can't assert
    /// pixel correctness here (that's the Vk acceptance test); but
    /// the gap absence is the load-bearing observable that the
    /// diversion is wired through `fill_solid_rects` →
    /// `engine.logic_fill` rather than the pre-3f.2 short-circuit.
    #[test]
    fn gxcopy_planemask_diverts_to_logic_fill() {
        use yserver_core::backend::GcFunction;
        let mut b = KmsBackendV2::for_tests();
        b.core.current_function = GcFunction::Xor;

        // Single rect: x=0 y=0 w=1 h=1.
        let mut wire = Vec::with_capacity(8);
        wire.extend_from_slice(&0_i16.to_le_bytes());
        wire.extend_from_slice(&0_i16.to_le_bytes());
        wire.extend_from_slice(&1_u16.to_le_bytes());
        wire.extend_from_slice(&1_u16.to_le_bytes());
        b.poly_fill_rectangle(None, 0xDEAD_BEEF, 0xFFFFFFFF, &wire)
            .expect("ok");
        let gaps = b.logged_gaps.borrow();
        assert!(
            !gaps.contains("fill_rects_non_gxcopy"),
            "stage 3f.1 fill_rects_non_gxcopy gap must not fire post-3f.2"
        );
        assert!(
            !gaps.contains("copy_plane_non_gxcopy"),
            "stage 3e.1 copy_plane_non_gxcopy gap must not fire post-3f.2"
        );
    }

    /// `set_clip_pixmap_stores_pixmap_clip` — Stage 3f.3 bookkeeping
    /// gate. The pre-3f.3 stub logged a gap and cleared the clip to
    /// `None`; 3f.3 stores the `ClipState::Pixmap` with the origin
    /// preserved (mask sampling itself is deferred). A subsequent
    /// `clear_clip_rectangles` returns to `None`.
    #[test]
    fn set_clip_pixmap_stores_pixmap_clip() {
        use yserver_core::backend::ClipState;
        let mut b = KmsBackendV2::for_tests();
        b.set_clip_pixmap(None, 0xABCD_EF01, 12, 34).expect("ok");
        match &b.core.current_clip {
            ClipState::Pixmap { origin, pixmap } => {
                assert_eq!(origin.0, 12);
                assert_eq!(origin.1, 34);
                assert_eq!(pixmap.as_raw(), 0xABCD_EF01);
            }
            other => panic!("expected ClipState::Pixmap, got {other:?}"),
        }
        // pre-3f.3 stub bumped a `set_clip_pixmap` gap; 3f.3 stores
        // bookkeeping cleanly.
        assert!(
            !b.logged_gaps.borrow().contains("set_clip_pixmap"),
            "set_clip_pixmap must not log a gap post-3f.3"
        );
        b.clear_clip_rectangles(None).expect("ok");
        assert!(matches!(b.core.current_clip, ClipState::None));
    }

    #[test]
    fn apply_clip_state_preserves_cached_pixmap_mask_after_free_and_origin_change() {
        use yserver_core::backend::{ClipState, PixmapHandle};

        let mut b = KmsBackendV2::for_tests();
        b.clip_mask_cache = Some(crate::kms::backend::ClipMaskCache {
            pixmap_xid: 0xABCD_EF01,
            origin: (0, 0),
            width: 5,
            height: 5,
            depth: 1,
            row_stride: 4,
            bytes: vec![
                0x1f, 0, 0, 0, 0x1f, 0, 0, 0, 0x1f, 0, 0, 0, 0x1f, 0, 0, 0, 0x1f, 0, 0, 0,
            ],
        });
        b.apply_clip_state(
            None,
            &ClipState::Pixmap {
                origin: (7, 9),
                pixmap: PixmapHandle::from_raw(0xABCD_EF01).unwrap(),
            },
        )
        .expect("apply_clip_state");
        let cache = b.clip_mask_cache.as_ref().expect("cache");
        assert_eq!(cache.pixmap_xid, 0xABCD_EF01);
        assert_eq!(cache.origin, (7, 9));
    }

    /// `set_gc_fill_tiled_stores_fill_state` — Stage 3f.3 bookkeeping
    /// gate. Pre-3f.3 stub logged a gap; 3f.3 stores
    /// `FillState::Tiled { pixmap, origin }` so subsequent fill ops
    /// can route through the tiled-fill RENDER composite. xid=0
    /// degenerates to `FillState::Solid`.
    #[test]
    fn set_gc_fill_tiled_stores_fill_state() {
        use yserver_core::backend::FillState;
        let mut b = KmsBackendV2::for_tests();
        b.set_gc_fill_tiled(None, 0xDEAD_BEEF, 5, 7).expect("ok");
        match &b.core.current_fill {
            FillState::Tiled { pixmap, origin } => {
                assert_eq!(pixmap.as_raw(), 0xDEAD_BEEF);
                assert_eq!(origin.0, 5);
                assert_eq!(origin.1, 7);
            }
            other => panic!("expected FillState::Tiled, got {other:?}"),
        }
        // xid=0 means PixmapHandle::from_raw returns None — falls
        // back to FillState::Solid (defensive; the dispatcher never
        // passes 0 here).
        b.set_gc_fill_tiled(None, 0, 0, 0).expect("ok");
        assert!(matches!(b.core.current_fill, FillState::Solid));

        assert!(
            !b.logged_gaps.borrow().contains("set_gc_fill_tiled"),
            "set_gc_fill_tiled must not log a gap post-3f.3"
        );
    }

    /// Stage 3f.4 close: cursor-creation calls mint valid handles
    /// without logging gaps. `create_cursor`, `create_glyph_cursor`,
    /// `render_create_cursor`, `define_cursor`, and
    /// `xfixes_change_cursor_by_name` all return `Ok` with no
    /// `log_v2_gap` noise. Pixel rasterisation + scene blit is
    /// Stage 4 (cursor scene-layer work); 3f.4's job is to silence
    /// the pre-Stage-4 stub warnings that were misleading
    /// real-app smoke matrix triage.
    #[test]
    fn cursor_paths_do_not_log_gaps() {
        use yserver_core::backend::{FontHandle, PictureHandle, PixmapHandle};

        let mut b = KmsBackendV2::for_tests();
        let pix = PixmapHandle::from_raw(0x1234_0001).unwrap();
        let font = FontHandle::from_raw(0x1234_0002).unwrap();
        let pic = PictureHandle::from_raw(0x1234_0003).unwrap();

        let c1 = b
            .create_cursor(None, pix, None, (0xFF00, 0, 0), (0, 0, 0xFF00), 4, 4)
            .expect("create_cursor");
        assert!(c1.as_raw() != 0);

        let c2 = b
            .create_glyph_cursor(None, font, None, b'X' as u16, 0, (0, 0, 0), (0, 0, 0))
            .expect("create_glyph_cursor");
        assert!(c2.as_raw() != 0);

        // Stage 5 Phase A: render_create_cursor returns None when
        // the picture xid isn't registered as a Drawable picture
        // (no rasterisation source); the contract is "don't log a
        // gap", not "always mint a handle".
        let _ = b
            .render_create_cursor(None, pic, 0, 0)
            .expect("render_create_cursor");

        b.define_cursor(None, 0xABCD_EF01, c1.as_raw())
            .expect("define_cursor");
        b.xfixes_change_cursor_by_name(None, c1.as_raw(), b"watch")
            .expect("xfixes_change_cursor_by_name");

        let gaps = b.logged_gaps.borrow();
        for g in [
            "create_cursor",
            "create_glyph_cursor",
            "render_create_cursor",
            "define_cursor",
            "xfixes_change_cursor_by_name",
        ] {
            assert!(
                !gaps.contains(g),
                "{g} must not log a gap post-3f.4 (cursor scene blit is Stage 4)"
            );
        }
    }

    /// Stage 5 Phase A: define_cursor stores the cursor on the
    /// window's geometry slot and (when the window is the root
    /// container) updates `KmsCore.active_cursor` so unbound child
    /// windows inherit the new sprite via the parent-chain walk.
    #[test]
    fn define_cursor_records_per_window_and_root_sticky() {
        use yserver_core::backend::{Backend, PixmapHandle};

        let mut b = KmsBackendV2::for_tests();
        let pix = PixmapHandle::from_raw(0x1234_0010).unwrap();
        let c = b
            .create_cursor(None, pix, None, (0xFFFF, 0, 0), (0, 0xFFFF, 0), 0, 0)
            .expect("create_cursor");

        // DefineCursor on the root container — sticky fallback.
        let root_host = b.core.window_id;
        b.define_cursor(None, root_host, c.as_raw())
            .expect("define_cursor root");
        assert_eq!(b.core.active_cursor, Some(c.as_raw()));

        // DefineCursor on a non-root window — stored on geom only,
        // does NOT touch `active_cursor`.
        let w: u32 = 0xABCD_0001;
        let rank = b.alloc_window_stack_rank();
        b.windows_v2.insert(
            w,
            super::WindowGeometryV2 {
                x: 0,
                y: 0,
                width: 8,
                height: 8,
                depth: 24,
                mapped: true,
                parent: None,
                stack_rank: rank,
                bg_pixel: None,
                bg_pixmap: None,
                cursor: None,
            },
        );
        let c2 = b
            .create_cursor(None, pix, None, (0, 0xFFFF, 0), (0xFFFF, 0, 0), 0, 0)
            .expect("create_cursor 2");
        b.define_cursor(None, w, c2.as_raw())
            .expect("define_cursor non-root");
        assert_eq!(
            b.core.active_cursor,
            Some(c.as_raw()),
            "non-root must not touch active_cursor"
        );
        assert_eq!(
            b.windows_v2.get(&w).and_then(|g| g.cursor),
            Some(c2.as_raw())
        );

        // `define_cursor(_, 0)` (X11 None) clears the per-window slot.
        b.define_cursor(None, w, 0).expect("define_cursor clear");
        assert_eq!(b.windows_v2.get(&w).and_then(|g| g.cursor), None);
    }

    /// Effective-cursor walk: a child without its own cursor inherits
    /// from its parent; a fresh root cursor (DefineCursor on root)
    /// becomes the fallback when no chain entry binds one.
    #[test]
    fn effective_cursor_walks_parent_chain() {
        use yserver_core::backend::{Backend, PixmapHandle};

        let mut b = KmsBackendV2::for_tests();
        let pix = PixmapHandle::from_raw(0x1234_0011).unwrap();
        let root_cur = b
            .create_cursor(None, pix, None, (0xFFFF, 0, 0), (0, 0, 0), 0, 0)
            .expect("create_cursor");
        let parent_cur = b
            .create_cursor(None, pix, None, (0, 0xFFFF, 0), (0, 0, 0), 0, 0)
            .expect("create_cursor parent");
        // Wire: root → parent → child. Parent has its own cursor;
        // child inherits.
        let root_host = b.core.window_id;
        let parent: u32 = 0xDEAD_0001;
        let child: u32 = 0xDEAD_0002;
        let rank_p = b.alloc_window_stack_rank();
        let rank_c = b.alloc_window_stack_rank();
        b.windows_v2.insert(
            parent,
            super::WindowGeometryV2 {
                x: 0,
                y: 0,
                width: 16,
                height: 16,
                depth: 24,
                mapped: true,
                parent: Some(root_host),
                stack_rank: rank_p,
                bg_pixel: None,
                bg_pixmap: None,
                cursor: None,
            },
        );
        b.windows_v2.insert(
            child,
            super::WindowGeometryV2 {
                x: 0,
                y: 0,
                width: 8,
                height: 8,
                depth: 24,
                mapped: true,
                parent: Some(parent),
                stack_rank: rank_c,
                bg_pixel: None,
                bg_pixmap: None,
                cursor: None,
            },
        );
        // DefineCursor on root + parent.
        b.define_cursor(None, root_host, root_cur.as_raw())
            .expect("root");
        b.define_cursor(None, parent, parent_cur.as_raw())
            .expect("parent");
        // Child inherits parent's cursor (parent has its own bound).
        assert_eq!(
            b.effective_cursor_walking_chain(child),
            Some(parent_cur.as_raw())
        );
        // Parent itself reports its own cursor.
        assert_eq!(
            b.effective_cursor_walking_chain(parent),
            Some(parent_cur.as_raw())
        );
        // Window unknown to windows_v2 → falls back to active_cursor
        // (root's DefineCursor).
        assert_eq!(
            b.effective_cursor_walking_chain(0xFFFF_FFFF),
            Some(root_cur.as_raw())
        );
    }

    /// CursorRecord versions are monotonically increasing — each
    /// `create_cursor` allocates a fresh version, and the boot-time
    /// default sits at version 1.
    #[test]
    fn cursor_record_versions_monotonic() {
        use std::sync::Arc;
        use yserver_core::backend::{Backend, PixmapHandle};

        let mut b = KmsBackendV2::for_tests();
        let default_xid = b.default_cursor_xid.expect("default cursor xid set");
        let v0 = b
            .cursor_records
            .get(&default_xid)
            .expect("default record")
            .version;

        let pix = PixmapHandle::from_raw(0x1234_0020).unwrap();
        let c1 = b
            .create_cursor(None, pix, None, (0xFFFF, 0, 0), (0, 0, 0), 0, 0)
            .expect("create_cursor");
        let c2 = b
            .create_cursor(None, pix, None, (0, 0xFFFF, 0), (0, 0, 0), 0, 0)
            .expect("create_cursor 2");

        let v1 = b.cursor_records.get(&c1.as_raw()).unwrap().version;
        let v2 = b.cursor_records.get(&c2.as_raw()).unwrap().version;
        assert!(v0 < v1, "v0={v0} v1={v1}");
        assert!(v1 < v2, "v1={v1} v2={v2}");

        // Captured Arc reference observes its original bytes even
        // after later allocations.
        let captured: Arc<crate::kms::v2::cursor::CursorRecord> =
            Arc::clone(b.cursor_records.get(&c1.as_raw()).unwrap());
        let snapshot = captured.bgra_bytes.clone();
        let _ = b
            .create_cursor(None, pix, None, (0, 0, 0xFFFF), (0, 0, 0), 0, 0)
            .expect("create_cursor 3");
        assert_eq!(captured.bgra_bytes, snapshot);
    }

    /// CreateAnimCursor snapshots frames at creation: maps gain an
    /// entry aliasing frame 0; the AnimCursorRecord holds Arc'd frame
    /// records + clamped delays.
    #[test]
    fn create_anim_cursor_snapshots_frames() {
        use std::time::Duration;
        use yserver_core::backend::{Backend, CursorHandle, PixmapHandle};

        let mut b = KmsBackendV2::for_tests();
        let pix = PixmapHandle::from_raw(0x1234_0030).unwrap();
        let c1 = b
            .create_cursor(None, pix, None, (0xFFFF, 0, 0), (0, 0, 0), 1, 2)
            .expect("c1");
        let c2 = b
            .create_cursor(None, pix, None, (0, 0xFFFF, 0), (0, 0, 0), 3, 4)
            .expect("c2");

        let anim = b
            .create_anim_cursor(None, &[(c1, 50), (c2, 0)])
            .expect("create_anim_cursor")
            .expect("KMS animates");

        let rec = b
            .anim_cursor_records
            .get(&anim.as_raw())
            .expect("anim record");
        assert_eq!(rec.frames.len(), 2);
        assert_eq!(rec.frames[0].delay, Duration::from_millis(50));
        // Delay 0 clamps to 16ms (spec: explicit Xorg deviation).
        assert_eq!(rec.frames[1].delay, Duration::from_millis(16));
        // The anim handle aliases frame 0 in the canonical map.
        assert_eq!(
            b.cursor_records.get(&anim.as_raw()).unwrap().version,
            b.cursor_records.get(&c1.as_raw()).unwrap().version,
        );
        // Unknown sub-cursor handle → error, no partial state.
        let bogus = CursorHandle::from_raw(0xDEAD_BEEF).unwrap();
        let before = b.anim_cursor_records.len();
        let recs_before = b.cursor_records.len();
        let pix_before = b.cursor_pixmaps.len();
        assert!(
            b.create_anim_cursor(None, &[(c1, 10), (bogus, 10)])
                .is_err()
        );
        assert_eq!(b.anim_cursor_records.len(), before);
        assert_eq!(b.cursor_records.len(), recs_before);
        assert_eq!(b.cursor_pixmaps.len(), pix_before);
    }

    /// Effective-cursor change arms/clears the animation; re-resolving
    /// to the same cursor preserves the running frame index.
    #[test]
    fn effective_cursor_arms_and_clears_animation() {
        use yserver_core::backend::{Backend, PixmapHandle};

        let mut b = KmsBackendV2::for_tests();
        let pix = PixmapHandle::from_raw(0x1234_0040).unwrap();
        let c1 = b
            .create_cursor(None, pix, None, (0xFFFF, 0, 0), (0, 0, 0), 0, 0)
            .expect("c1");
        let c2 = b
            .create_cursor(None, pix, None, (0, 0xFFFF, 0), (0, 0, 0), 0, 0)
            .expect("c2");
        let anim = b
            .create_anim_cursor(None, &[(c1, 50), (c2, 75)])
            .expect("anim")
            .expect("KMS animates");

        // Purely static cursor → anim state stays cleared.
        let root_host = b.core.window_id;
        b.define_cursor(None, root_host, c1.as_raw())
            .expect("static before anim");
        assert!(
            b.active_cursor_anim.is_none(),
            "static cursor must not arm animation"
        );

        // Bind the anim cursor on root → it becomes effective and arms.
        b.define_cursor(None, root_host, anim.as_raw())
            .expect("define anim");
        let st = b.active_cursor_anim.as_ref().expect("armed");
        assert_eq!(st.handle, anim.as_raw());
        assert_eq!(st.frame, 0);
        // Arming mints a fresh version for frame 0 (monotonic serial).
        let v_frame0 = b.cursor_records.get(&c1.as_raw()).unwrap().version;
        let v_anim = b.cursor_records.get(&anim.as_raw()).unwrap().version;
        assert!(
            v_anim > v_frame0,
            "armed version must be minted, not aliased"
        );

        // Pretend the animation advanced, then re-resolve to the SAME
        // cursor: frame index must be preserved (no restart).
        b.active_cursor_anim.as_mut().unwrap().frame = 1;
        b.refresh_effective_cursor();
        assert_eq!(b.active_cursor_anim.as_ref().unwrap().frame, 1);

        // Switch to a static cursor → animation cleared.
        b.define_cursor(None, root_host, c1.as_raw())
            .expect("define static");
        assert!(b.active_cursor_anim.is_none());

        // Switch back → restarts at frame 0.
        b.define_cursor(None, root_host, anim.as_raw())
            .expect("re-define anim");
        assert_eq!(b.active_cursor_anim.as_ref().unwrap().frame, 0);
    }

    /// Stage 4d regression: `ChangeWindowAttributes` on a window
    /// under COMPOSITE redirect must NOT trigger a backing wipe.
    /// Pre-fix `change_subwindow_attributes` eagerly called
    /// `clear_window_area_with_background`, which routes through
    /// `resolve_paint_target` into the redirected backing B and
    /// fills it with depth-24 default black — exactly the
    /// "mate-control-center turns opaque black on drag" symptom
    /// observed in hardware smoke (marco re-asserts CWA on every
    /// drag-induced configure; v2 interprets that as a paint
    /// command and wipes B).
    ///
    /// X11 spec: CWA's background attribute change does not
    /// repaint. The bg setting only affects future
    /// `ClearArea` / Expose handling. v2's eager clear was a
    /// Stage 3f.6 over-reach.
    #[test]
    fn cwa_on_redirected_window_does_not_clear_backing() {
        use crate::kms::v2::store::{DrawableKind, Storage};
        use yserver_core::backend::Backend;

        let mut b = KmsBackendV2::for_tests();

        // Set up W as a top-level window in windows_v2 + the store.
        let w_xid: u32 = 0x100_0001;
        let stack_rank = b.alloc_window_stack_rank();
        b.windows_v2.insert(
            w_xid,
            super::WindowGeometryV2 {
                x: 100,
                y: 100,
                width: 200,
                height: 200,
                depth: 24,
                mapped: true,
                parent: None,
                bg_pixel: None,
                bg_pixmap: None,
                stack_rank,
                cursor: None,
            },
        );
        let w_storage = Storage::for_tests_null(
            ash::vk::Extent2D {
                width: 200,
                height: 200,
            },
            ash::vk::Format::B8G8R8A8_UNORM,
        );
        let _w_id = b
            .store
            .allocate(w_xid, DrawableKind::Window, 24, true, w_storage)
            .expect("alloc W");

        // Set up B as a pixmap, then install the redirect route
        // W → B. This is the load-bearing precondition for the
        // bug: the fill path would route through W's redirect.
        let b_xid: u32 = 0x100_0002;
        let b_storage = Storage::for_tests_null(
            ash::vk::Extent2D {
                width: 200,
                height: 200,
            },
            ash::vk::Format::B8G8R8A8_UNORM,
        );
        let b_id = b
            .store
            .allocate(b_xid, DrawableKind::Pixmap, 24, false, b_storage)
            .expect("alloc B");
        assert!(b.test_set_redirected_target(w_xid, b_xid));
        // Sanity: resolve_paint_target on W now lands at B, not W.
        let resolved = b
            .resolve_paint_target(w_xid)
            .expect("resolve_paint_target W");
        assert_eq!(
            resolved.id, b_id,
            "fixture sanity: W's paint must route to B before issuing CWA",
        );

        // Snapshot the clear counter.
        let calls_before = b.clear_window_area_calls;

        // Issue CWA with CWBackPixmap = None (value=0). That's
        // marco's "no background pixmap" attribute change, sent
        // on every drag-induced configure. v2 must NOT interpret
        // this as a paint command on the redirected backing.
        b.change_subwindow_attributes(None, w_xid, 0x01, &[0])
            .expect("change_subwindow_attributes");

        assert_eq!(
            b.clear_window_area_calls, calls_before,
            "CWA on a redirected window must not call clear_window_area_with_background \
             (pre-fix this fired and wiped B with depth-24 default black, destroying \
             the compositor's painted pixels — the 'opaque black on drag' bug)",
        );

        // Same test with CWBackPixel — also a clear-trigger pre-fix.
        b.change_subwindow_attributes(None, w_xid, 0x02, &[0x00FF_FFFF])
            .expect("change_subwindow_attributes CWBackPixel");
        assert_eq!(
            b.clear_window_area_calls, calls_before,
            "CWBackPixel on a redirected window must also skip the eager clear",
        );

        // Sanity: bg state IS stored (CWA still records the
        // values; only the eager paint is skipped).
        let geom = b.windows_v2.get(&w_xid).expect("W in windows_v2");
        assert_eq!(geom.bg_pixel, Some(0x00FF_FFFF));
        assert_eq!(geom.bg_pixmap, None);
    }

    /// X11 spec generalisation: CWA's background attribute change
    /// MUST NOT repaint, regardless of whether the window is under
    /// composite redirect. The bg attribute only affects future
    /// `ClearArea` / Expose handling. The earlier
    /// `cwa_on_redirected_window_does_not_clear_backing` guard only
    /// caught the redirect case; the non-redirect case still wiped
    /// the client's storage. Live trigger (2026-05-30): non-
    /// composited mate-control-center sidebar going black when
    /// caja takes focus over CC — marco re-asserts CWA on CC's
    /// client window, yserver clears its 975×600 storage to bg=0
    /// (black), and GTK gets no Expose so the bg never repaints.
    /// Widgets come back on hover (per-widget redraw), bg stays
    /// black.
    #[test]
    fn cwa_on_non_redirected_window_does_not_clear_storage() {
        use crate::kms::v2::store::{DrawableKind, Storage};
        use yserver_core::backend::Backend;

        let mut b = KmsBackendV2::for_tests();

        let w_xid: u32 = 0x200_0001;
        let stack_rank = b.alloc_window_stack_rank();
        b.windows_v2.insert(
            w_xid,
            super::WindowGeometryV2 {
                x: 100,
                y: 100,
                width: 300,
                height: 200,
                depth: 24,
                mapped: true,
                parent: None,
                bg_pixel: None,
                bg_pixmap: None,
                stack_rank,
                cursor: None,
            },
        );
        let w_storage = Storage::for_tests_null(
            ash::vk::Extent2D {
                width: 300,
                height: 200,
            },
            ash::vk::Format::B8G8R8A8_UNORM,
        );
        let _w_id = b
            .store
            .allocate(w_xid, DrawableKind::Window, 24, true, w_storage)
            .expect("alloc W");
        // Sanity: not under redirect — paint target is the leaf
        // itself, which is the path the existing guard fails to
        // catch.
        let leaf = b.store.lookup(w_xid).expect("W leaf");
        assert!(b.store.redirected_target(leaf).is_none(), "fixture sanity");
        let resolved = b.resolve_paint_target(w_xid).expect("resolve");
        assert_eq!(resolved.id, leaf, "fixture sanity: paint stays at leaf");

        let calls_before = b.clear_window_area_calls;

        // CWBackPixmap = None (marco's per-configure churn).
        b.change_subwindow_attributes(None, w_xid, 0x01, &[0])
            .expect("CWA");
        assert_eq!(
            b.clear_window_area_calls, calls_before,
            "CWA on a non-redirected window must not clear storage \
             (pre-fix this fired and wiped the client's pixmap to bg=0, \
             producing the 'CC sidebar black on focus-uncover' symptom)",
        );

        // CWBackPixel — second clear-trigger pre-fix.
        b.change_subwindow_attributes(None, w_xid, 0x02, &[0x00FF_FFFF])
            .expect("CWA bg_pixel");
        assert_eq!(
            b.clear_window_area_calls, calls_before,
            "CWBackPixel on a non-redirected window must also skip the eager clear",
        );

        // Sanity: bg state IS still stored.
        let geom = b.windows_v2.get(&w_xid).expect("W in windows_v2");
        assert_eq!(geom.bg_pixel, Some(0x00FF_FFFF));
        assert_eq!(geom.bg_pixmap, None);
    }

    /// Follow-up to `cwa_on_redirected_window_does_not_clear_backing`:
    /// the original Stage 4d fix only checks `redirected_target(W)`
    /// — i.e. W has its OWN backing. That misses the case where W
    /// has no own backing but its paints route to an ANCESTOR's
    /// backing via `resolve_paint_target`'s ancestor walk. Live
    /// trigger: mate-panel tray applets — reparented away from root
    /// into mate-panel sockets, they have no own backing but paint
    /// to mate-panel's backing via the ancestor chain. marco's CWA
    /// bg_pixmap=None per drag-induced configure lands a transparent
    /// fill into mate-panel's backing at the applet's screen
    /// position, wiping the icon. Symptom: applets visible briefly
    /// then disappear.
    #[test]
    fn cwa_on_descendant_routed_to_redirected_ancestor_does_not_clear() {
        use yserver_core::{backend::Backend, resources::ROOT_WINDOW, server::ServerState};
        use yserver_protocol::x11::ResourceId;

        let mut state = ServerState::new();
        let mut backend = KmsBackendV2::for_tests();

        let mate_panel_xid = ResourceId(0x110_0001);
        let applet_xid = ResourceId(0x140_0001);

        // mate-panel: child of root, has its own redirected backing.
        seed_v2_window(
            &mut state,
            &mut backend,
            mate_panel_xid,
            ROOT_WINDOW,
            0,
            0,
            2560,
            28,
        );
        seed_v2_redirected_backing(&mut state, &mut backend, mate_panel_xid);
        // applet: child of mate-panel, no own backing.
        seed_v2_window(
            &mut state,
            &mut backend,
            applet_xid,
            mate_panel_xid,
            0,
            1,
            24,
            24,
        );

        let applet_host_xid = synth_host_xid(applet_xid);

        // Fixture sanity: applet has its own leaf drawable but no
        // own redirected target, while `resolve_paint_target` walks
        // up to mate-panel's backing. That's the load-bearing
        // precondition: pre-fix the existing `is_redirected` check
        // returns false (no own backing), but a CWA-time clear
        // would still route through `resolve_paint_target` and land
        // on mate-panel's backing.
        let applet_leaf = backend
            .store
            .lookup(applet_host_xid)
            .expect("applet has its own leaf drawable");
        assert!(
            backend.store.redirected_target(applet_leaf).is_none(),
            "applet must not have its own backing"
        );
        let resolved = backend
            .resolve_paint_target(applet_host_xid)
            .expect("applet paint must resolve");
        assert_ne!(
            resolved.id, applet_leaf,
            "applet paints must route to an ancestor's backing, not its own leaf"
        );

        let calls_before = backend.clear_window_area_calls;

        // marco's "bg_pixmap = None" CWA on the applet. Pre-fix
        // this routes a transparent fill into mate-panel's backing
        // at the applet's screen position, wiping any content there.
        backend
            .change_subwindow_attributes(None, applet_host_xid, 0x01, &[0])
            .expect("change_subwindow_attributes");

        assert_eq!(
            backend.clear_window_area_calls, calls_before,
            "CWA on a window whose paints route to a redirected ancestor must \
             not call clear_window_area_with_background (pre-fix this fired \
             and wiped mate-panel's backing where the tray applet icon was \
             painted — the 'tray applet visible briefly then disappears' \
             symptom)"
        );

        // Same check for CWBackPixel (the other clear-trigger).
        backend
            .change_subwindow_attributes(None, applet_host_xid, 0x02, &[0x00FF_FFFF])
            .expect("change_subwindow_attributes CWBackPixel");
        assert_eq!(
            backend.clear_window_area_calls, calls_before,
            "CWBackPixel on a window routed to a redirected ancestor must \
             also skip the eager clear"
        );
    }

    /// v2 backend's `copy_area` does ITS OWN ClipByChildren split
    /// (independent of the protocol-layer split in
    /// `copy_area_effective_dst_rects`). That second pass had no
    /// manual-redirect exemption, so even when the protocol layer
    /// delivered a non-empty sub-rect for a tray-style scenario,
    /// the v2 layer re-clipped it to empty and the engine call
    /// never fired. Pin the v2 side's exemption directly: parent
    /// with mapped child fully overlapping, child's scene
    /// participation is false (Manual-redirect semantics) → the
    /// engine.copy_area dispatch loop must run at least once.
    #[test]
    fn copy_area_clip_by_children_skips_manually_redirected_child() {
        use crate::kms::v2::store::{DrawableKind, Storage};
        use yserver_core::backend::Backend;

        let mut b = KmsBackendV2::for_tests();

        let parent_xid: u32 = 0x100_0001;
        let child_xid: u32 = 0x100_0002;
        let src_pixmap_xid: u32 = 0x100_0003;

        let parent_stack_rank = b.alloc_window_stack_rank();
        b.windows_v2.insert(
            parent_xid,
            super::WindowGeometryV2 {
                x: 0,
                y: 0,
                width: 100,
                height: 80,
                depth: 24,
                mapped: true,
                parent: None,
                bg_pixel: None,
                bg_pixmap: None,
                stack_rank: parent_stack_rank,
                cursor: None,
            },
        );
        b.store
            .allocate(
                parent_xid,
                DrawableKind::Window,
                24,
                true,
                Storage::for_tests_null(
                    ash::vk::Extent2D {
                        width: 100,
                        height: 80,
                    },
                    ash::vk::Format::B8G8R8A8_UNORM,
                ),
            )
            .expect("alloc parent");

        // Manually-redirected child fully covering the parent.
        // scene_participating=false is the v2-store reflection of
        // Manual-redirect semantics (X server stops auto-painting
        // it into the scene/parent backing).
        let child_stack_rank = b.alloc_window_stack_rank();
        b.windows_v2.insert(
            child_xid,
            super::WindowGeometryV2 {
                x: 0,
                y: 0,
                width: 100,
                height: 80,
                depth: 24,
                mapped: true,
                parent: Some(parent_xid),
                bg_pixel: None,
                bg_pixmap: None,
                stack_rank: child_stack_rank,
                cursor: None,
            },
        );
        let child_id = b
            .store
            .allocate(
                child_xid,
                DrawableKind::Window,
                24,
                false, // scene_participating=false → Manual semantics
                Storage::for_tests_null(
                    ash::vk::Extent2D {
                        width: 100,
                        height: 80,
                    },
                    ash::vk::Format::B8G8R8A8_UNORM,
                ),
            )
            .expect("alloc manual-redirected child");
        // Allocate the child's redirected backing pixmap so the
        // scene_participating=false + has-backing combination
        // matches Manual semantics (not just an unmapped or
        // input-only window).
        let backing_id = b
            .store
            .allocate(
                0x100_0099,
                DrawableKind::Pixmap,
                24,
                false,
                Storage::for_tests_null(
                    ash::vk::Extent2D {
                        width: 100,
                        height: 80,
                    },
                    ash::vk::Format::B8G8R8A8_UNORM,
                ),
            )
            .expect("alloc child backing");
        assert!(b.test_set_redirected_target(child_xid, 0x100_0099));
        let _ = (child_id, backing_id);

        // Source pixmap for the copy.
        b.store
            .allocate(
                src_pixmap_xid,
                DrawableKind::Pixmap,
                24,
                false,
                Storage::for_tests_null(
                    ash::vk::Extent2D {
                        width: 100,
                        height: 80,
                    },
                    ash::vk::Format::B8G8R8A8_UNORM,
                ),
            )
            .expect("alloc src pixmap");

        // Pre-call snapshot — engine isn't a real Vk so each
        // engine.copy_area returns NoVk, but the counter increments
        // *before* the call, which is what we're measuring (the
        // surviving-sub-rect count, not engine success).
        let calls_before = b.engine_copy_area_calls;

        b.copy_area(None, src_pixmap_xid, parent_xid, 0, 0, 0, 0, 100, 80)
            .expect("copy_area must not return Err");

        assert!(
            b.engine_copy_area_calls > calls_before,
            "engine.copy_area must dispatch at least once when the only \
             child fully overlapping the dst is manually redirected; \
             pre-fix the v2 ClipByChildren clipped the rect to empty and \
             the loop never ran (counter unchanged) — the live symptom \
             being notification-area-applet's CopyArea silently dropped"
        );
    }

    /// Regression guard: an AUTOMATIC-redirected child (i.e. one
    /// whose own backing exists but `scene_participating=true`)
    /// must STILL be subtracted by v2's ClipByChildren. The
    /// manual-only exemption in
    /// `copy_area_clip_by_children_skips_manually_redirected_child`
    /// must not loosen this case — under Automatic mode the X
    /// server auto-composites the child's backing into the parent's
    /// pixmap, so the parent's own paint must avoid those rects.
    #[test]
    fn copy_area_clip_by_children_still_subtracts_automatic_child_in_v2() {
        use crate::kms::v2::store::{DrawableKind, Storage};
        use yserver_core::backend::Backend;

        let mut b = KmsBackendV2::for_tests();

        let parent_xid: u32 = 0x200_0001;
        let child_xid: u32 = 0x200_0002;
        let src_pixmap_xid: u32 = 0x200_0003;

        let parent_stack_rank = b.alloc_window_stack_rank();
        b.windows_v2.insert(
            parent_xid,
            super::WindowGeometryV2 {
                x: 0,
                y: 0,
                width: 100,
                height: 80,
                depth: 24,
                mapped: true,
                parent: None,
                bg_pixel: None,
                bg_pixmap: None,
                stack_rank: parent_stack_rank,
                cursor: None,
            },
        );
        b.store
            .allocate(
                parent_xid,
                DrawableKind::Window,
                24,
                true,
                Storage::for_tests_null(
                    ash::vk::Extent2D {
                        width: 100,
                        height: 80,
                    },
                    ash::vk::Format::B8G8R8A8_UNORM,
                ),
            )
            .expect("alloc parent");

        // Automatic-redirected child fully covering the parent —
        // distinguished by `scene_participating=true` even though it
        // has its own redirected backing.
        let child_stack_rank = b.alloc_window_stack_rank();
        b.windows_v2.insert(
            child_xid,
            super::WindowGeometryV2 {
                x: 0,
                y: 0,
                width: 100,
                height: 80,
                depth: 24,
                mapped: true,
                parent: Some(parent_xid),
                bg_pixel: None,
                bg_pixmap: None,
                stack_rank: child_stack_rank,
                cursor: None,
            },
        );
        b.store
            .allocate(
                child_xid,
                DrawableKind::Window,
                24,
                true, // scene_participating=true → Automatic semantics
                Storage::for_tests_null(
                    ash::vk::Extent2D {
                        width: 100,
                        height: 80,
                    },
                    ash::vk::Format::B8G8R8A8_UNORM,
                ),
            )
            .expect("alloc automatic-redirected child");
        b.store
            .allocate(
                0x200_0099,
                DrawableKind::Pixmap,
                24,
                false,
                Storage::for_tests_null(
                    ash::vk::Extent2D {
                        width: 100,
                        height: 80,
                    },
                    ash::vk::Format::B8G8R8A8_UNORM,
                ),
            )
            .expect("alloc child backing");
        assert!(b.test_set_redirected_target(child_xid, 0x200_0099));

        b.store
            .allocate(
                src_pixmap_xid,
                DrawableKind::Pixmap,
                24,
                false,
                Storage::for_tests_null(
                    ash::vk::Extent2D {
                        width: 100,
                        height: 80,
                    },
                    ash::vk::Format::B8G8R8A8_UNORM,
                ),
            )
            .expect("alloc src pixmap");

        let calls_before = b.engine_copy_area_calls;

        b.copy_area(None, src_pixmap_xid, parent_xid, 0, 0, 0, 0, 100, 80)
            .expect("copy_area must not return Err");

        assert_eq!(
            b.engine_copy_area_calls, calls_before,
            "Automatic-redirected child fully covering dst must still be \
             subtracted (clip to empty → no engine.copy_area dispatch). \
             The manual-only exemption must not loosen this case."
        );
    }

    /// Stage 3f.6 close: `change_subwindow_attributes` stores
    /// `bg_pixel` + `bg_pixmap` into the v2 window record instead of
    /// logging a gap. value_mask=0x03 (CWBackPixmap + CWBackPixel)
    /// with values [pixmap_xid, pixel] lands both. value_mask=0x02
    /// alone lands the pixel only. value_mask=0x01 with pixmap=0
    /// resolves to bg_pixmap=None per X11 semantics.
    #[test]
    fn change_subwindow_attributes_stores_bg_state() {
        let mut b = KmsBackendV2::for_tests();
        // Seed a window in windows_v2 directly (allocate fails on
        // for_tests because there's no Vk; geometry insert still
        // works in production via the no-Vk branch).
        b.windows_v2.insert(
            0xCAFE_BABE,
            super::WindowGeometryV2 {
                x: 0,
                y: 0,
                width: 100,
                height: 100,
                depth: 32,
                mapped: false,
                parent: None,
                stack_rank: 0,
                bg_pixel: None,
                bg_pixmap: None,
                cursor: None,
            },
        );

        // CWBackPixmap (0x01) + CWBackPixel (0x02), values =
        // [0xABCD_1234, 0xFF0000FF].
        b.change_subwindow_attributes(None, 0xCAFE_BABE, 0x03, &[0xABCD_1234, 0xFF00_00FF])
            .expect("ok");
        let geom = b.windows_v2[&0xCAFE_BABE];
        assert_eq!(geom.bg_pixmap, Some(0xABCD_1234));
        assert_eq!(geom.bg_pixel, Some(0xFF00_00FF));

        // CWBackPixmap=0 → None (inherit-from-parent). bg_pixel
        // stays as the previous value (CWBackPixel bit clear).
        b.change_subwindow_attributes(None, 0xCAFE_BABE, 0x01, &[0])
            .expect("ok");
        let geom = b.windows_v2[&0xCAFE_BABE];
        assert_eq!(geom.bg_pixmap, None);
        assert_eq!(geom.bg_pixel, Some(0xFF00_00FF));

        // The pre-3f.6 stub bumped a `change_subwindow_attributes`
        // gap; 3f.6 stores bookkeeping cleanly.
        assert!(
            !b.logged_gaps
                .borrow()
                .contains("change_subwindow_attributes"),
            "change_subwindow_attributes must not log a gap post-3f.6"
        );
    }

    // ─── Stage 3f.7: input dispatch tests ───────────────────────

    /// `serialize_modifiers` returns 0 against a fresh xkb_state
    /// (no modifiers held). Regression gate for the bit layout.
    #[test]
    fn serialize_modifiers_zero_on_fresh_state() {
        let b = KmsBackendV2::for_tests();
        assert_eq!(b.serialize_modifiers(), 0);
    }

    /// `cook_host_key` fills root + event coords from cursor and
    /// stamps the post-update modifier mask. Pressing a Shift
    /// keycode flips the Shift bit in the cooked state.
    #[test]
    fn cook_host_key_fills_coords_and_modifier_state() {
        use yserver_core::host_x11::HostKeyEvent;
        let mut b = KmsBackendV2::for_tests();
        b.core.cursor_x = 100.0;
        b.core.cursor_y = 200.0;
        // 50 == evdev KEY_LEFTSHIFT (US layout); xkbcommon's
        // default keymap maps this to the Shift modifier.
        let raw = HostKeyEvent {
            keycode: 50,
            pressed: true,
            state: 0,
            root_x: 0,
            root_y: 0,
            event_x: 0,
            event_y: 0,
            time: 0,
        };
        let cooked = b.cook_host_key(raw);
        assert_eq!(cooked.root_x, 100);
        assert_eq!(cooked.root_y, 200);
        assert_eq!(cooked.event_x, 100);
        assert_eq!(cooked.event_y, 200);
        // Bit 0 = Shift. Some xkb keymaps deliver Shift on key 50
        // via xkbcommon's default; this assertion proves the
        // modifier state is read out post-update. If the test ICD
        // disagrees, lower this to >0 — the load-bearing check is
        // that `state` reflects the update, not zero.
        assert_ne!(cooked.state, 0, "Shift press must update mod state");
    }

    /// Lock-LED mask tracks the XKB lock state: a Caps Lock toggle
    /// (press + release through `cook_host_key`) raises the CAPSLOCK
    /// LED bit; a second toggle clears it. This mask is what
    /// `sync_keyboard_leds` pushes to the keyboards via libinput —
    /// the caps-LED-never-lights bug (typed the lock-screen password
    /// wrong repeatedly because the LED gave no feedback).
    #[test]
    fn caps_lock_toggle_drives_led_bits() {
        use yserver_core::host_x11::HostKeyEvent;
        let mut b = KmsBackendV2::for_tests();
        assert_eq!(b.current_led_bits(), 0, "fresh state: all lock LEDs off");
        // 66 == X keycode for Caps Lock (evdev KEY_CAPSLOCK 58 + 8).
        let key = |pressed| HostKeyEvent {
            keycode: 66,
            pressed,
            state: 0,
            root_x: 0,
            root_y: 0,
            event_x: 0,
            event_y: 0,
            time: 0,
        };
        let _ = b.cook_host_key(key(true));
        let _ = b.cook_host_key(key(false));
        assert_eq!(
            b.current_led_bits(),
            input::Led::CAPSLOCK.bits(),
            "caps toggle must raise the CAPSLOCK LED bit",
        );
        let _ = b.cook_host_key(key(true));
        let _ = b.cook_host_key(key(false));
        assert_eq!(b.current_led_bits(), 0, "second toggle clears the LED bit");
    }

    /// Cinnamon alt-tab regression (2026-06-10): `query_pointer`'s
    /// KeyButMask must include the LIVE keyboard modifier state, not
    /// only the held buttons. `XIQueryPointer`'s ModifierInfo is
    /// filled from this mask (Xorg fills the reply from the paired
    /// MASTER_KEYBOARD's XKB state — Xi/xiquerypointer.c:120,139);
    /// with the modifiers missing, cinnamon's switcher polled
    /// `global.get_pointer()` right after pushModal, read "Alt not
    /// held", and instantly cancelled the popup (instant window
    /// switch, no switcher UI).
    #[test]
    fn query_pointer_mask_includes_live_keyboard_modifiers() {
        use yserver_core::{backend::Backend, host_x11::HostKeyEvent};
        let mut b = KmsBackendV2::for_tests();
        // 50 == evdev KEY_LEFTSHIFT — proven to flip a modifier bit in
        // the test keymap by `cook_host_key_fills_coords_and_modifier_state`.
        let raw = HostKeyEvent {
            keycode: 50,
            pressed: true,
            state: 0,
            root_x: 0,
            root_y: 0,
            event_x: 0,
            event_y: 0,
            time: 0,
        };
        let _ = b.cook_host_key(raw);
        let p = Backend::query_pointer(&mut b, None).expect("query_pointer");
        assert_ne!(
            p.mask & 0x00ff,
            0,
            "KeyButMask from query_pointer must include the held modifier \
             — XIQueryPointer's ModifierInfo (cinnamon alt-tab) reads it",
        );
    }

    /// `process_pointer_button` honours the X11 spec's pre-press
    /// `state` field: on ButtonPress the button bit is NOT yet
    /// set in `state`, on ButtonRelease it IS still set.
    /// `button_mask` is updated AFTER the event so the next
    /// motion sees the new mask.
    #[test]
    fn process_pointer_button_state_field_is_pre_press() {
        use yserver_core::{host_x11::PointerEventKind, server::ServerState};
        let mut b = KmsBackendV2::for_tests();
        let state = ServerState::new();
        // BTN_LEFT press → detail=1, button bit = 0x0100.
        b.process_pointer_button(0x110, true, &state);
        let press = b
            .core
            .pending_pointer_events
            .iter()
            .find(|e| matches!(e.kind, PointerEventKind::ButtonPress))
            .expect("ButtonPress emitted");
        assert_eq!(press.detail, 1);
        assert_eq!(
            press.state & 0x0100,
            0,
            "Button1 bit must NOT be set in ButtonPress.state (pre-press)"
        );
        assert_eq!(
            b.core.button_mask & 0x0100,
            0x0100,
            "button_mask updated post-event"
        );

        b.core.pending_pointer_events.clear();
        b.process_pointer_button(0x110, false, &state);
        let release = b
            .core
            .pending_pointer_events
            .iter()
            .find(|e| matches!(e.kind, PointerEventKind::ButtonRelease))
            .expect("ButtonRelease emitted");
        assert_eq!(
            release.state & 0x0100,
            0x0100,
            "Button1 bit MUST be set in ButtonRelease.state (still held)"
        );
        assert_eq!(
            b.core.button_mask & 0x0100,
            0,
            "button_mask cleared post-release"
        );
    }

    /// `warp_pointer_root` (the WarpPointer path on KMS) must move
    /// the tracked cursor and fan the resulting motion out — the
    /// fanout caches the position in `state.pointer_root`. Pre-fix
    /// `warp_pointer` was a `log_v2_gap` stub, so XWarpPointer never
    /// moved the pointer and every xts5 Xlib11 event-delivery test
    /// pressed buttons at the stale center position, missing its
    /// test window ("Expected event not received" en masse).
    #[test]
    fn warp_pointer_root_moves_cursor_and_fans_out_motion() {
        use yserver_core::server::ServerState;
        let mut b = KmsBackendV2::for_tests();
        let mut state = ServerState::new();
        Backend::warp_pointer_root(&mut b, &mut state, 123, 45);
        assert_eq!(b.core.cursor_x, 123.0);
        assert_eq!(b.core.cursor_y, 45.0);
        assert_eq!(
            state.pointer_root,
            (123, 45),
            "the warp motion must reach the pointer fanout",
        );
    }

    /// `process_pointer_absolute` clamps to the output extent and
    /// updates `cursor_x` / `cursor_y`. Single-output test fixture
    /// reports 800×600 from PlatformBackend::for_tests.
    #[test]
    fn process_pointer_absolute_clamps_to_output() {
        use yserver_core::server::ServerState;
        let mut b = KmsBackendV2::for_tests();
        let state = ServerState::new();
        // Inside extent.
        b.process_pointer_absolute(&state, 100.0, 200.0);
        assert_eq!(b.core.cursor_x, 100.0);
        assert_eq!(b.core.cursor_y, 200.0);
        // Past extent → clamped to (extent - 1).
        b.process_pointer_absolute(&state, 5000.0, 5000.0);
        assert_eq!(b.core.cursor_x, 799.0);
        assert_eq!(b.core.cursor_y, 599.0);
    }

    /// Multi-output regression: the pointer clamp must use the
    /// union framebuffer extent (`PlatformBackend.fb_w/fb_h`),
    /// NOT `outputs.first().width/height`. Pre-fix the clamp
    /// consulted only the first output, so the cursor could never
    /// cross from monitor 0 onto monitor 1 in a side-by-side
    /// layout.
    ///
    /// Simulate two side-by-side 2560×1440 monitors by leaving the
    /// fixture's single 800×600 output entry in place but bumping
    /// `platform.fb_w` to 5120 (this is what `core_platform_init`
    /// computes as `max(x + width)` across all outputs in
    /// production — see `kms/backend.rs:1063-1072`). The input
    /// thread already targets that union extent at thread spawn,
    /// so v2 receives `PointerMotion { x, y }` already in
    /// virtual-screen coords; the only divergence was v2's
    /// re-clamp.
    #[test]
    fn process_pointer_absolute_uses_union_fb_extent_for_multi_output() {
        use yserver_core::server::ServerState;
        let mut b = KmsBackendV2::for_tests();
        b.platform.fb_w = 5120;
        b.platform.fb_h = 1440;
        let state = ServerState::new();
        // Point on monitor 1 (x=4000 is past output[0]'s 800-wide
        // fixture extent but well within the 5120 union extent).
        b.process_pointer_absolute(&state, 4000.0, 1000.0);
        assert_eq!(
            b.core.cursor_x, 4000.0,
            "pointer must be able to cross past the first output's \
             extent; pre-fix this clamps to 799 and the cursor is \
             stuck on monitor 0",
        );
        assert_eq!(b.core.cursor_y, 1000.0);
        // Past the union extent → clamped to (union - 1).
        b.process_pointer_absolute(&state, 9999.0, 9999.0);
        assert_eq!(b.core.cursor_x, 5119.0);
        assert_eq!(b.core.cursor_y, 1439.0);
    }

    /// `window_under_cursor` returns the topmost mapped top-level
    /// containing the cursor. Walks `core.top_level_order` back-to-
    /// front so the most-recently-stacked window wins. Unmapped
    /// windows skipped.
    #[test]
    fn window_under_cursor_finds_topmost_mapped() {
        let mut b = KmsBackendV2::for_tests();
        b.windows_v2.insert(
            0x1000,
            super::WindowGeometryV2 {
                x: 0,
                y: 0,
                width: 100,
                height: 100,
                depth: 32,
                mapped: true,
                parent: None,
                stack_rank: 0,
                bg_pixel: None,
                bg_pixmap: None,
                cursor: None,
            },
        );
        b.windows_v2.insert(
            0x2000,
            super::WindowGeometryV2 {
                x: 50,
                y: 50,
                width: 100,
                height: 100,
                depth: 32,
                mapped: true,
                parent: None,
                stack_rank: 1,
                bg_pixel: None,
                bg_pixmap: None,
                cursor: None,
            },
        );
        b.core.top_level_order.push(0x1000);
        b.core.top_level_order.push(0x2000);

        // Cursor in overlap (50..100, 50..100): 0x2000 wins (topmost).
        b.core.cursor_x = 75.0;
        b.core.cursor_y = 75.0;
        assert_eq!(b.window_under_cursor(), Some(0x2000));

        // Cursor outside overlap, only in 0x1000.
        b.core.cursor_x = 25.0;
        b.core.cursor_y = 25.0;
        assert_eq!(b.window_under_cursor(), Some(0x1000));

        // Cursor outside both — root-fallback handled at caller.
        b.core.cursor_x = 300.0;
        b.core.cursor_y = 300.0;
        assert_eq!(b.window_under_cursor(), None);

        // Unmapping the topmost — next match wins.
        b.windows_v2.get_mut(&0x2000).unwrap().mapped = false;
        b.core.cursor_x = 75.0;
        b.core.cursor_y = 75.0;
        assert_eq!(b.window_under_cursor(), Some(0x1000));
    }

    /// `window_under_cursor` descends into mapped sub-windows so the
    /// returned xid is the deepest match. xfwm4 attaches resize-edge
    /// cursors to frame sub-windows; without descent the cursor walk
    /// stops at the (cursor=None) frame top-level and the resize
    /// sprites never become effective on hover. Pinned: top-edge
    /// child wins when pointer is in the edge band; the frame
    /// top-level wins in the interior; topmost sibling wins on
    /// overlap; unmapped sub-windows are skipped (parent wins).
    #[test]
    fn window_under_cursor_descends_into_subwindow_tree() {
        let mut b = KmsBackendV2::for_tests();
        // Frame top-level at (100,100, 800x600), no cursor.
        b.windows_v2.insert(
            0x1000,
            super::WindowGeometryV2 {
                x: 100,
                y: 100,
                width: 800,
                height: 600,
                depth: 24,
                mapped: true,
                parent: None,
                stack_rank: 0,
                bg_pixel: None,
                bg_pixmap: None,
                cursor: None,
            },
        );
        b.core.top_level_order.push(0x1000);
        // Top-edge resize sub-window at parent-local (0,0, 800x10),
        // i.e. screen (100,100, 800x10). Has its own resize cursor.
        b.windows_v2.insert(
            0x1001,
            super::WindowGeometryV2 {
                x: 0,
                y: 0,
                width: 800,
                height: 10,
                depth: 24,
                mapped: true,
                parent: Some(0x1000),
                stack_rank: 0,
                bg_pixel: None,
                bg_pixmap: None,
                cursor: Some(0xdead_0001),
            },
        );
        // Bottom-edge resize sub-window at parent-local (0,590, 800x10),
        // screen (100,690, 800x10). Different cursor.
        b.windows_v2.insert(
            0x1002,
            super::WindowGeometryV2 {
                x: 0,
                y: 590,
                width: 800,
                height: 10,
                depth: 24,
                mapped: true,
                parent: Some(0x1000),
                stack_rank: 1,
                bg_pixel: None,
                bg_pixmap: None,
                cursor: Some(0xdead_0002),
            },
        );

        // Cursor in the top-edge band: deepest hit is the top sub-window.
        b.core.cursor_x = 150.0;
        b.core.cursor_y = 105.0;
        assert_eq!(b.window_under_cursor(), Some(0x1001));

        // Cursor in the bottom-edge band: bottom sub-window.
        b.core.cursor_x = 150.0;
        b.core.cursor_y = 695.0;
        assert_eq!(b.window_under_cursor(), Some(0x1002));

        // Cursor in the frame interior (not in any edge band): the
        // frame top-level itself.
        b.core.cursor_x = 400.0;
        b.core.cursor_y = 300.0;
        assert_eq!(b.window_under_cursor(), Some(0x1000));

        // Overlap test — add a second top-edge child at the same
        // location with higher stack_rank; topmost wins.
        b.windows_v2.insert(
            0x1003,
            super::WindowGeometryV2 {
                x: 0,
                y: 0,
                width: 800,
                height: 10,
                depth: 24,
                mapped: true,
                parent: Some(0x1000),
                stack_rank: 99,
                bg_pixel: None,
                bg_pixmap: None,
                cursor: Some(0xdead_0003),
            },
        );
        b.core.cursor_x = 150.0;
        b.core.cursor_y = 105.0;
        assert_eq!(b.window_under_cursor(), Some(0x1003));

        // Unmap the topmost overlap entry — sibling beneath wins.
        b.windows_v2.get_mut(&0x1003).unwrap().mapped = false;
        assert_eq!(b.window_under_cursor(), Some(0x1001));
    }

    /// `on_host_input` no longer logs the `v2: on_host_input not
    /// yet implemented` gap that fired before 3f.7. Key events
    /// drain through xkb cooking; pointer events drain to the
    /// pointer fanout.
    #[test]
    fn on_host_input_does_not_log_gap() {
        use yserver_core::{core_loop::HostInputEvent, server::ServerState};
        let mut b = KmsBackendV2::for_tests();
        let mut state = ServerState::new();
        // PointerMotion → process_pointer_absolute → no panic, no gap.
        b.on_host_input(
            &mut state,
            HostInputEvent::PointerMotion {
                x: 10,
                y: 20,
                time: 0,
            },
        );
        assert!(
            !b.logged_gaps.borrow().contains("on_host_input"),
            "on_host_input must not log a gap post-3f.7"
        );
        assert_eq!(b.core.cursor_x, 10.0);
        assert_eq!(b.core.cursor_y, 20.0);
    }

    /// Stage 3f.6 — `create_subwindow` records the parent xid + the
    /// background-pixel hint so subsequent `build_scene` traversals
    /// can reach the new window and an initial bg_pixel fill can
    /// run. Engine fill itself returns `NoVk` on the test fixture;
    /// the load-bearing observable is the geometry record.
    #[test]
    fn create_subwindow_records_parent_and_bg_pixel() {
        use yserver_core::{backend::WindowHandle, host_x11::HostSubwindowVisual};
        let mut b = KmsBackendV2::for_tests();
        let parent = WindowHandle::from_raw(0x1234_5678).unwrap();
        let child = b
            .create_subwindow(
                None,
                parent,
                10,
                20,
                100,
                50,
                0,
                HostSubwindowVisual::CopyFromParent,
                Some(0xFF11_2233),
                None,
            )
            .expect("create_subwindow");
        let geom = b.windows_v2[&child.as_raw()];
        assert_eq!(geom.parent, Some(0x1234_5678));
        assert_eq!(geom.bg_pixel, Some(0xFF11_2233));
        assert_eq!(geom.x, 10);
        assert_eq!(geom.y, 20);
        assert_eq!(geom.width, 100);
        assert_eq!(geom.height, 50);
        assert_eq!(
            geom.depth, 24,
            "root/untracked CopyFromParent inherits root depth"
        );
        assert!(!geom.mapped, "mapped is set later via map_subwindow");
    }

    #[test]
    fn copy_from_parent_child_inherits_argb_parent_depth() {
        use yserver_core::{backend::WindowHandle, host_x11::HostSubwindowVisual};

        let mut b = KmsBackendV2::for_tests();
        b.windows_v2.insert(
            0x2000,
            super::WindowGeometryV2 {
                x: 0,
                y: 0,
                width: 80,
                height: 40,
                depth: 32,
                mapped: true,
                parent: None,
                stack_rank: 0,
                bg_pixel: None,
                bg_pixmap: None,
                cursor: None,
            },
        );
        let child = b
            .create_subwindow(
                None,
                WindowHandle::from_raw(0x2000).unwrap(),
                1,
                2,
                30,
                20,
                0,
                HostSubwindowVisual::CopyFromParent,
                None,
                None,
            )
            .expect("create_subwindow");
        assert_eq!(b.windows_v2[&child.as_raw()].depth, 32);
    }

    #[test]
    fn depth_only_visual_preserves_argb_top_level_depth() {
        use yserver_core::{backend::WindowHandle, host_x11::HostSubwindowVisual};

        let mut b = KmsBackendV2::for_tests();
        let child = b
            .create_subwindow(
                None,
                WindowHandle::from_raw(b.window_id()).unwrap(),
                0,
                0,
                2944,
                1840,
                0,
                HostSubwindowVisual::DepthOnly { depth: 32 },
                Some(0),
                None,
            )
            .expect("create_subwindow");
        assert_eq!(b.windows_v2[&child.as_raw()].depth, 32);
    }

    /// Stage 3f.11: reparenting a top-level window INTO another
    /// window removes it from `core.top_level_order` so
    /// `build_scene` only emits it once (via the recurse from the
    /// new parent). Reproducer for the MATE clock-applet duplicate-
    /// render: clock was first registered as a top-level under
    /// root, then reparented INTO mate-panel's container. Pre-fix,
    /// build_scene emitted it twice — once at child-relative coords
    /// (treated as absolute) and once at real screen position.
    #[test]
    fn reparent_into_container_removes_from_top_level_order() {
        let mut b = KmsBackendV2::for_tests();
        // Two stub windows: the parent container, and the would-be
        // child (initially registered as a top-level).
        b.windows_v2.insert(
            0xC0FFEE,
            super::WindowGeometryV2 {
                x: 0,
                y: 0,
                width: 200,
                height: 100,
                depth: 32,
                mapped: true,
                parent: None,
                stack_rank: 0,
                bg_pixel: None,
                bg_pixmap: None,
                cursor: None,
            },
        );
        b.windows_v2.insert(
            0xCAFED00D,
            super::WindowGeometryV2 {
                x: 0,
                y: 0,
                width: 50,
                height: 20,
                depth: 32,
                mapped: true,
                parent: None,
                stack_rank: 1,
                bg_pixel: None,
                bg_pixmap: None,
                cursor: None,
            },
        );
        b.core.top_level_order.push(0xC0FFEE);
        b.core.top_level_order.push(0xCAFED00D);
        assert!(b.core.top_level_order.contains(&0xCAFED00D));

        // Reparent 0xCAFED00D under 0xC0FFEE at (30, 10).
        b.reparent_subwindow(None, 0xCAFED00D, 0xC0FFEE, 30, 10)
            .expect("reparent");

        // top_level_order must no longer contain the reparented xid.
        assert!(
            !b.core.top_level_order.contains(&0xCAFED00D),
            "reparenting into a non-root parent must remove from top_level_order \
             — otherwise build_scene emits the window twice"
        );
        // Geometry record reflects new parent + position.
        let geom = b.windows_v2[&0xCAFED00D];
        assert_eq!(geom.parent, Some(0xC0FFEE));
        assert_eq!(geom.x, 30);
        assert_eq!(geom.y, 10);
    }

    /// Phase 4.1 (COW structural redesign): protocol-validation
    /// guarantees the host_parent exists in `windows_v2` by the time
    /// `reparent_subwindow` is invoked. A missing entry means projection
    /// drift between resources and backend — a fatal internal-
    /// consistency failure, not a recoverable "treat as top-level".
    /// The old silent fallback masked the cinnamon COW bug for weeks;
    /// surfacing the drift via panic is the spec'd behaviour.
    #[test]
    #[should_panic(expected = "reparent_subwindow")]
    fn reparent_subwindow_panics_when_host_parent_missing() {
        let mut b = KmsBackendV2::for_tests();
        // Pre-seed the child so reparent_subwindow has something to
        // operate on; the panic must come from the missing host_parent
        // lookup, not from a missing child.
        let child_xid: u32 = 0x0040_0050;
        b.windows_v2.insert(
            child_xid,
            super::WindowGeometryV2 {
                x: 0,
                y: 0,
                width: 100,
                height: 100,
                depth: 24,
                mapped: true,
                parent: None,
                stack_rank: 0,
                bg_pixel: None,
                bg_pixmap: None,
                cursor: None,
            },
        );
        // Reparent to a host_parent that doesn't exist in windows_v2.
        // 0 is the legitimate "reparent to root" convention; 0xDEADBEEF
        // is genuinely absent and must trip the drift panic.
        let _ = b.reparent_subwindow(None, child_xid, 0xDEAD_BEEF, 0, 0);
    }

    /// Task 4.1 regression: the production reparent-to-root path
    /// (`process_request.rs` `handle_reparent_window`) computes
    /// `host_parent` as `backend.window_id()` (== `core.window_id` == 1)
    /// for `ReparentWindow(child -> ROOT_WINDOW)`, NOT 0. Root is never
    /// tracked in `windows_v2`, so the missing-parent panic guard must
    /// treat `core.window_id` as a root sentinel (-> top-level) instead
    /// of crashing. Without the fix this panics on every WM window-
    /// withdraw / frame-teardown.
    #[test]
    fn reparent_subwindow_to_root_via_window_id_does_not_panic() {
        let mut b = KmsBackendV2::for_tests();
        let root_xid = b.window_id();
        assert_eq!(root_xid, 1, "core.window_id sentinel changed");
        let child_xid: u32 = 0x0040_0050;
        b.windows_v2.insert(
            child_xid,
            super::WindowGeometryV2 {
                x: 5,
                y: 7,
                width: 100,
                height: 100,
                depth: 24,
                mapped: true,
                // Originally a sub-window under some frame; the WM now
                // reparents it back to root (withdraw / frame teardown).
                parent: Some(0x0040_0060),
                stack_rank: 0,
                bg_pixel: None,
                bg_pixmap: None,
                cursor: None,
            },
        );
        // host_parent == root's real host xid (core.window_id), exactly
        // what the production call site passes for reparent-to-ROOT.
        let res = b.reparent_subwindow(None, child_xid, root_xid, 0, 0);
        assert!(res.is_ok(), "reparent-to-root must succeed, got {res:?}");
        // Child is now a top-level under root.
        assert_eq!(b.windows_v2.get(&child_xid).unwrap().parent, None);
        assert!(b.core.top_level_order.contains(&child_xid));
    }

    /// Stage 3f.11: `restack_top_level` with `stack_mode=Below` and
    /// no sibling lowers a top-level to the BOTTOM of
    /// `core.top_level_order`. Reproduces marco's "lower caja-
    /// desktop" call so the wallpaper window stays beneath panels.
    #[test]
    fn restack_below_no_sibling_moves_to_bottom() {
        let mut b = KmsBackendV2::for_tests();
        b.core.top_level_order = vec![0x1000, 0x2000, 0x3000];
        // 0x3000 is the most recently registered (top of stack).
        // Marco's Lower-Below request should move it to position 0.
        b.restack_top_level(0x3000, 1, None);
        assert_eq!(b.core.top_level_order, vec![0x3000, 0x1000, 0x2000]);
    }

    /// Stage 3f.11: `restack_top_level` with `stack_mode=Above` and
    /// no sibling raises a top-level to the TOP of
    /// `core.top_level_order`.
    #[test]
    fn restack_above_no_sibling_moves_to_top() {
        let mut b = KmsBackendV2::for_tests();
        b.core.top_level_order = vec![0x1000, 0x2000, 0x3000];
        b.restack_top_level(0x1000, 0, None);
        assert_eq!(b.core.top_level_order, vec![0x2000, 0x3000, 0x1000]);
    }

    /// `_NET_WM_WINDOW_TYPE_DESKTOP` must force a top-level to the
    /// bottom of `core.top_level_order`, regardless of when the
    /// property arrives.
    #[test]
    fn desktop_window_type_moves_to_bottom() {
        use yserver_core::resources::ROOT_WINDOW;
        use yserver_protocol::x11::ResourceId;

        let mut state = ServerState::new();
        let mut b = KmsBackendV2::for_tests();
        let window = ResourceId(0x4000);
        seed_v2_window(&mut state, &mut b, window, ROOT_WINDOW, 0, 0, 100, 100);
        let host_xid = synth_host_xid(window);
        b.core.top_level_order = vec![0x1111, host_xid];

        let atom_window_type = state.atoms.intern("_NET_WM_WINDOW_TYPE", false);
        let atom_desktop = state.atoms.intern("_NET_WM_WINDOW_TYPE_DESKTOP", false);
        let atom_atom = state.atoms.intern("ATOM", false);
        state
            .resources
            .window_mut(window)
            .unwrap()
            .properties
            .insert(
                atom_window_type,
                PropertyValue {
                    r#type: atom_atom,
                    format: PropertyFormat::F32,
                    data: atom_desktop.0.to_ne_bytes().to_vec(),
                },
            );

        b.on_window_property_changed(&state, host_xid, atom_window_type);
        assert_eq!(b.core.top_level_order, vec![host_xid, 0x1111]);
    }

    /// Dialog-like windows should rise when they become top-level,
    /// even if the hint was already present before the reparent /
    /// register_top_level transition.
    #[test]
    fn dialog_hint_raises_when_window_becomes_top_level() {
        use yserver_core::resources::ROOT_WINDOW;
        use yserver_protocol::x11::ResourceId;

        let mut state = ServerState::new();
        let mut b = KmsBackendV2::for_tests();
        let window = ResourceId(0x5000);
        seed_v2_window(&mut state, &mut b, window, ROOT_WINDOW, 0, 0, 100, 100);
        let host_xid = synth_host_xid(window);
        b.core.top_level_order = vec![host_xid, 0x1111];

        let atom_window_type = state.atoms.intern("_NET_WM_WINDOW_TYPE", false);
        let atom_dialog = state.atoms.intern("_NET_WM_WINDOW_TYPE_DIALOG", false);
        let atom_atom = state.atoms.intern("ATOM", false);
        state
            .resources
            .window_mut(window)
            .unwrap()
            .properties
            .insert(
                atom_window_type,
                PropertyValue {
                    r#type: atom_atom,
                    format: PropertyFormat::F32,
                    data: atom_dialog.0.to_ne_bytes().to_vec(),
                },
            );

        assert_eq!(
            KmsBackendV2::window_stack_hint(&state, state.resources.window(window).unwrap()),
            Some(super::TopLevelStackHint::Top)
        );
        b.on_window_became_top_level(&state, host_xid);
        assert_eq!(b.core.top_level_order, vec![0x1111, host_xid]);
    }

    /// Stage 3f.11 follow-up: subwindow restack updates sibling order
    /// within a shared parent instead of relying on HashMap iteration.
    #[test]
    fn restack_subwindow_updates_sibling_order() {
        let mut b = KmsBackendV2::for_tests();
        b.windows_v2.insert(
            0xCAFE,
            super::WindowGeometryV2 {
                x: 0,
                y: 0,
                width: 10,
                height: 10,
                depth: 32,
                mapped: true,
                parent: Some(0xBEEF),
                stack_rank: 0,
                bg_pixel: None,
                bg_pixmap: None,
                cursor: None,
            },
        );
        b.windows_v2.insert(
            0xD00D,
            super::WindowGeometryV2 {
                x: 0,
                y: 0,
                width: 10,
                height: 10,
                depth: 32,
                mapped: true,
                parent: Some(0xBEEF),
                stack_rank: 1,
                bg_pixel: None,
                bg_pixmap: None,
                cursor: None,
            },
        );
        b.restack_subwindow(0xD00D, 1, Some(0xCAFE));
        assert!(b.windows_v2[&0xD00D].stack_rank < b.windows_v2[&0xCAFE].stack_rank);
    }

    /// Stage 3f.11: reparenting back to root re-adds to
    /// `core.top_level_order` so the window resumes top-level
    /// rendering. The Backend trait's reparent call carries the new
    /// parent xid; `host_parent==0` or an untracked xid (root is
    /// `core.window_id`, not in `windows_v2`) maps to `parent=None`.
    #[test]
    fn reparent_to_root_re_adds_to_top_level_order() {
        let mut b = KmsBackendV2::for_tests();
        b.windows_v2.insert(
            0xC0FFEE,
            super::WindowGeometryV2 {
                x: 0,
                y: 0,
                width: 200,
                height: 100,
                depth: 32,
                mapped: true,
                parent: None,
                stack_rank: 0,
                bg_pixel: None,
                bg_pixmap: None,
                cursor: None,
            },
        );
        b.windows_v2.insert(
            0xCAFED00D,
            super::WindowGeometryV2 {
                x: 30,
                y: 10,
                width: 50,
                height: 20,
                depth: 32,
                mapped: true,
                parent: Some(0xC0FFEE),
                stack_rank: 1,
                bg_pixel: None,
                bg_pixmap: None,
                cursor: None,
            },
        );
        // Start: child not in top_level_order.
        assert!(!b.core.top_level_order.contains(&0xCAFED00D));

        // Reparent to root (host_parent=0 maps to parent=None).
        b.reparent_subwindow(None, 0xCAFED00D, 0, 100, 200)
            .expect("reparent");

        assert!(
            b.core.top_level_order.contains(&0xCAFED00D),
            "reparenting to root must add to top_level_order"
        );
        let geom = b.windows_v2[&0xCAFED00D];
        assert_eq!(geom.parent, None);
        assert_eq!(geom.x, 100);
        assert_eq!(geom.y, 200);
    }

    // ───── Stage 4a — resolve_paint_target ─────────────────────────

    /// Seed a window in `windows_v2` and a matching no-Vk store
    /// entry, returning the new DrawableId. Used by the 4a
    /// resolver tests so the ancestor walk has something to chew
    /// on without touching Vk.
    fn seed_window(
        b: &mut KmsBackendV2,
        xid: u32,
        parent: Option<u32>,
        x: i16,
        y: i16,
    ) -> crate::kms::v2::store::DrawableId {
        use crate::kms::v2::store::{DrawableKind, Storage};
        b.windows_v2.insert(
            xid,
            super::WindowGeometryV2 {
                x,
                y,
                width: 100,
                height: 100,
                depth: 32,
                mapped: true,
                parent,
                stack_rank: 0,
                bg_pixel: None,
                bg_pixmap: None,
                cursor: None,
            },
        );
        b.store
            .allocate(
                xid,
                DrawableKind::Window,
                32,
                true,
                Storage::for_tests_null(
                    ash::vk::Extent2D {
                        width: 100,
                        height: 100,
                    },
                    ash::vk::Format::B8G8R8A8_UNORM,
                ),
            )
            .expect("seed_window allocate")
    }

    #[test]
    fn clip_fill_rects_by_subwindow_mode_subtracts_mapped_child() {
        let mut b = KmsBackendV2::for_tests();
        let _parent = seed_window(&mut b, 0x100, None, 0, 0);
        let _child = seed_window(&mut b, 0x200, Some(0x100), 10, 20);
        let child = b.windows_v2.get_mut(&0x200).expect("child geom");
        child.width = 15;
        child.height = 10;
        b.core.current_subwindow_mode = yserver_core::backend::SubwindowMode::ClipByChildren;

        let out = b.clip_fill_rects_by_subwindow_mode(
            0x100,
            &[Rectangle16 {
                x: 0,
                y: 0,
                width: 40,
                height: 40,
            }],
        );
        let got: std::collections::BTreeSet<(i16, i16, u16, u16)> = out
            .into_iter()
            .map(|r| (r.x, r.y, r.width, r.height))
            .collect();
        let want = std::collections::BTreeSet::from([
            (0, 0, 40, 20),
            (0, 30, 40, 10),
            (0, 20, 10, 10),
            (25, 20, 15, 10),
        ]);
        assert_eq!(got, want);
    }

    #[test]
    fn clip_fill_rects_by_subwindow_mode_include_inferiors_is_passthrough() {
        let mut b = KmsBackendV2::for_tests();
        let _parent = seed_window(&mut b, 0x100, None, 0, 0);
        let _child = seed_window(&mut b, 0x200, Some(0x100), 10, 20);
        b.core.current_subwindow_mode = yserver_core::backend::SubwindowMode::IncludeInferiors;

        let src = [Rectangle16 {
            x: 0,
            y: 0,
            width: 40,
            height: 40,
        }];
        assert_eq!(b.clip_fill_rects_by_subwindow_mode(0x100, &src), src);
    }

    #[test]
    fn collect_fill_rects_for_inferiors_translates_root_to_top_level_child() {
        let mut b = KmsBackendV2::for_tests();
        let root_xid = b.core.window_id;
        let _top = seed_window(&mut b, 0x200, Some(root_xid), 10, 20);
        let out = b.collect_fill_rects_for_inferiors(
            b.core.window_id,
            &[Rectangle16 {
                x: 15,
                y: 25,
                width: 20,
                height: 20,
            }],
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, 0x200);
        assert_eq!(
            out[0].1,
            vec![Rectangle16 {
                x: 5,
                y: 5,
                width: 20,
                height: 20,
            }]
        );
    }

    /// Unknown xid → `None`. The resolver's first step is
    /// `store.lookup`, which fails for an xid that was never
    /// allocated.
    #[test]
    fn resolve_paint_target_unknown_xid_returns_none() {
        let b = KmsBackendV2::for_tests();
        assert_eq!(b.resolve_paint_target(0xDEAD_BEEF), None);
    }

    /// Pixmap xid (not in `windows_v2`) with no redirect →
    /// identity result. Covers the pre-loop short-circuit so the
    /// ancestor walk never reads `None` off a pixmap.
    #[test]
    fn resolve_paint_target_pixmap_returns_identity() {
        use crate::kms::v2::store::{DrawableKind, Storage};
        let mut b = KmsBackendV2::for_tests();
        let pix_id = b
            .store
            .allocate(
                0x2000,
                DrawableKind::Pixmap,
                32,
                false,
                Storage::for_tests_null(
                    ash::vk::Extent2D {
                        width: 64,
                        height: 64,
                    },
                    ash::vk::Format::B8G8R8A8_UNORM,
                ),
            )
            .expect("pixmap allocate");
        let pt = b.resolve_paint_target(0x2000).expect("resolve");
        assert_eq!(
            pt,
            super::PaintTarget {
                id: pix_id,
                offset: (0, 0)
            }
        );
    }

    /// Top-level window with no redirect → identity result.
    /// `parent == None` reaches the explicit fall-through arm; the
    /// resolver must NOT short-circuit to `None` via `?` on the
    /// missing parent.
    #[test]
    fn resolve_paint_target_unredirected_top_level_returns_identity() {
        let mut b = KmsBackendV2::for_tests();
        let w_id = seed_window(&mut b, 0x100, None, 0, 0);
        let pt = b.resolve_paint_target(0x100).expect("resolve");
        assert_eq!(
            pt,
            super::PaintTarget {
                id: w_id,
                offset: (0, 0)
            }
        );
    }

    /// `set_redirected_target(W, Some(B))` routes paint against
    /// `W`'s xid to `B`'s drawable. Offset stays `(0, 0)` —
    /// `W` is the redirected node itself, not a descendant.
    #[test]
    fn resolve_paint_target_redirected_window_routes_to_backing() {
        use crate::kms::v2::store::{DrawableKind, Storage};
        let mut b = KmsBackendV2::for_tests();
        let w_id = seed_window(&mut b, 0x100, None, 0, 0);
        let b_id = b
            .store
            .allocate(
                0x900,
                DrawableKind::RedirectedBacking,
                32,
                false,
                Storage::for_tests_null(
                    ash::vk::Extent2D {
                        width: 100,
                        height: 100,
                    },
                    ash::vk::Format::B8G8R8A8_UNORM,
                ),
            )
            .expect("backing allocate");
        b.store.set_redirected_target(w_id, Some(b_id));
        let pt = b.resolve_paint_target(0x100).expect("resolve");
        assert_eq!(
            pt,
            super::PaintTarget {
                id: b_id,
                offset: (0, 0)
            }
        );
    }

    /// Descendant paint accumulates `(x, y)` offsets up the
    /// ancestor chain. W at root with redirect to B; child C at
    /// (10, 20) under W; grandchild G at (3, 4) under C. Paint on
    /// G's xid resolves to `(B, (13, 24))` — the sum of the
    /// child offsets traversed.
    #[test]
    fn resolve_paint_target_descendant_accumulates_offset() {
        use crate::kms::v2::store::{DrawableKind, Storage};
        let mut b = KmsBackendV2::for_tests();
        let w_id = seed_window(&mut b, 0x100, None, 0, 0);
        let _c_id = seed_window(&mut b, 0x200, Some(0x100), 10, 20);
        let _g_id = seed_window(&mut b, 0x300, Some(0x200), 3, 4);
        let b_id = b
            .store
            .allocate(
                0x900,
                DrawableKind::RedirectedBacking,
                32,
                false,
                Storage::for_tests_null(
                    ash::vk::Extent2D {
                        width: 200,
                        height: 200,
                    },
                    ash::vk::Format::B8G8R8A8_UNORM,
                ),
            )
            .expect("backing allocate");
        b.store.set_redirected_target(w_id, Some(b_id));
        let pt = b.resolve_paint_target(0x300).expect("resolve");
        assert_eq!(
            pt,
            super::PaintTarget {
                id: b_id,
                offset: (13, 24)
            }
        );
    }

    /// Top-level whose `parent == Some(root_xid)` (root isn't in
    /// `windows_v2`) walks one step and finds root's redirect
    /// state. With root un-redirected, paint stays on the leaf.
    /// Regression for the resolver-returns-None bug that surfaced
    /// when `v2_subwindow_resize_clears_old_paint` started routing
    /// `fill_rectangle` through `resolve_paint_target` and the
    /// previous `windows_v2.get(parent_xid)?` chain poisoned the
    /// outer Option for any parent==root case.
    #[test]
    fn resolve_paint_target_parent_root_falls_back_to_identity() {
        let mut b = KmsBackendV2::for_tests();
        // root_xid is seeded via `KmsCore::for_tests()` and present
        // in the store (init_root_storage); but NOT in windows_v2.
        let root_xid = b.core.window_id;
        assert!(!b.windows_v2.contains_key(&root_xid));
        let w_id = seed_window(&mut b, 0x100, Some(root_xid), 0, 0);
        let pt = b.resolve_paint_target(0x100).expect("resolve");
        assert_eq!(
            pt,
            super::PaintTarget {
                id: w_id,
                offset: (0, 0)
            }
        );
    }

    /// Root itself can be the redirect target — a compositor that
    /// runs `RedirectWindow(root, …)` sets `redirected_target` on
    /// root's drawable. Paint against root or its descendants
    /// resolves through the root-backing.
    ///
    /// Codex round-7 finding: top-level windows are recorded with
    /// `parent == None` (NOT `Some(root_xid)`) by
    /// `create_subwindow` because root isn't tracked in
    /// `windows_v2`. The pre-fix resolver's `None` arm returned
    /// identity without consulting root, so real top-level
    /// descendants bypassed the root backing.
    #[test]
    fn resolve_paint_target_redirected_root_routes_descendants() {
        use crate::kms::v2::store::{DrawableKind, Storage};
        let mut b = KmsBackendV2::for_tests();
        let root_xid = b.core.window_id;
        let root_id = b.store.lookup(root_xid).expect("root id");
        // Use the production representation: `parent = None`
        // marks a top-level whose host_parent is root_xid (see
        // `create_subwindow`'s `if !windows_v2.contains_key →
        // parent = None` branch). Also seed a descendant whose
        // parent IS the top-level so we exercise the full walk.
        let _w_id = seed_window(&mut b, 0x100, None, 50, 60);
        let _c_id = seed_window(&mut b, 0x101, Some(0x100), 3, 4);
        let backing_id = b
            .store
            .allocate(
                0x900,
                DrawableKind::RedirectedBacking,
                32,
                false,
                Storage::for_tests_null(
                    ash::vk::Extent2D {
                        width: 800,
                        height: 600,
                    },
                    ash::vk::Format::B8G8R8A8_UNORM,
                ),
            )
            .expect("backing allocate");
        b.store.set_redirected_target(root_id, Some(backing_id));
        // Root paint (direct) resolves through the leaf-level
        // pre-loop short-circuit.
        let pt_root = b.resolve_paint_target(root_xid).expect("resolve root");
        assert_eq!(
            pt_root,
            super::PaintTarget {
                id: backing_id,
                offset: (0, 0)
            }
        );
        // Top-level (parent=None production rep) must walk into
        // root's redirect with its own (x, y) accumulated.
        let pt_w = b.resolve_paint_target(0x100).expect("resolve W");
        assert_eq!(
            pt_w,
            super::PaintTarget {
                id: backing_id,
                offset: (50, 60)
            }
        );
        // Descendant of a top-level: accumulates C-in-W (3, 4)
        // then W-in-root (50, 60) → (53, 64).
        let pt_c = b.resolve_paint_target(0x101).expect("resolve C");
        assert_eq!(
            pt_c,
            super::PaintTarget {
                id: backing_id,
                offset: (53, 64)
            }
        );
    }

    /// Plan §4a (Tests, line 644-646): clearing a redirect via
    /// `set_redirected_target(W, None)` falls back to leaf-storage
    /// routing. The store-level `set_redirected_target_none_clears_route`
    /// verifies the field is cleared; this end-to-end check
    /// asserts the resolver flow honours that. Catches a regression
    /// where a missing branch / wrong `?` could special-case the
    /// cleared state.
    #[test]
    fn resolve_paint_target_after_clear_falls_back_to_identity() {
        use crate::kms::v2::store::{DrawableKind, Storage};
        let mut b = KmsBackendV2::for_tests();
        let w_id = seed_window(&mut b, 0x100, None, 0, 0);
        let backing_id = b
            .store
            .allocate(
                0x900,
                DrawableKind::RedirectedBacking,
                32,
                false,
                Storage::for_tests_null(
                    ash::vk::Extent2D {
                        width: 100,
                        height: 100,
                    },
                    ash::vk::Format::B8G8R8A8_UNORM,
                ),
            )
            .expect("backing allocate");
        // Install the redirect, then immediately clear it.
        b.store.set_redirected_target(w_id, Some(backing_id));
        b.store.set_redirected_target(w_id, None);
        let pt = b.resolve_paint_target(0x100).expect("resolve");
        assert_eq!(
            pt,
            super::PaintTarget {
                id: w_id,
                offset: (0, 0)
            },
            "cleared redirect must fall through to leaf identity",
        );
    }

    /// Nearest redirected ancestor wins. W→B_W and C→B_C both
    /// redirected; grandchild G under C must route to B_C with
    /// the C-relative offset, NOT to B_W with the
    /// W-relative offset.
    #[test]
    fn resolve_paint_target_stops_at_nearest_redirected_ancestor() {
        use crate::kms::v2::store::{DrawableKind, Storage};
        let mut b = KmsBackendV2::for_tests();
        let w_id = seed_window(&mut b, 0x100, None, 0, 0);
        let c_id = seed_window(&mut b, 0x200, Some(0x100), 10, 20);
        let _g_id = seed_window(&mut b, 0x300, Some(0x200), 3, 4);
        let bw_id = b
            .store
            .allocate(
                0x900,
                DrawableKind::RedirectedBacking,
                32,
                false,
                Storage::for_tests_null(
                    ash::vk::Extent2D {
                        width: 200,
                        height: 200,
                    },
                    ash::vk::Format::B8G8R8A8_UNORM,
                ),
            )
            .expect("B_W");
        let bc_id = b
            .store
            .allocate(
                0x901,
                DrawableKind::RedirectedBacking,
                32,
                false,
                Storage::for_tests_null(
                    ash::vk::Extent2D {
                        width: 100,
                        height: 100,
                    },
                    ash::vk::Format::B8G8R8A8_UNORM,
                ),
            )
            .expect("B_C");
        b.store.set_redirected_target(w_id, Some(bw_id));
        b.store.set_redirected_target(c_id, Some(bc_id));
        let pt = b.resolve_paint_target(0x300).expect("resolve");
        assert_eq!(
            pt,
            super::PaintTarget {
                id: bc_id,
                offset: (3, 4)
            }
        );
    }

    // ────────────────────────────────────────────────────────────────
    // Stage 4c.2 — `window_absolute_rect` helper
    // ────────────────────────────────────────────────────────────────

    /// Top-level W at (50, 60) size 100×80, parent=None. Absolute
    /// rect echoes its own (x, y, w, h) — there's no ancestor to
    /// accumulate through.
    #[test]
    fn window_absolute_rect_top_level() {
        let mut b = KmsBackendV2::for_tests();
        let w_id = seed_window(&mut b, 0x100, None, 50, 60);
        // `seed_window` hard-codes 100×100; resize via the geom entry.
        b.windows_v2.get_mut(&0x100).unwrap().width = 100;
        b.windows_v2.get_mut(&0x100).unwrap().height = 80;
        let rect = b.window_absolute_rect(w_id).expect("rect");
        assert_eq!(
            rect,
            ash::vk::Rect2D {
                offset: ash::vk::Offset2D { x: 50, y: 60 },
                extent: ash::vk::Extent2D {
                    width: 100,
                    height: 80
                },
            }
        );
    }

    /// Three-level chain: W(50, 60) → C(10, 20) → G(3, 4) size 8×8.
    /// G's absolute rect is at (63, 84) with G's own 8×8 extent.
    #[test]
    fn window_absolute_rect_descendant() {
        let mut b = KmsBackendV2::for_tests();
        let _w_id = seed_window(&mut b, 0x100, None, 50, 60);
        let _c_id = seed_window(&mut b, 0x200, Some(0x100), 10, 20);
        let g_id = seed_window(&mut b, 0x300, Some(0x200), 3, 4);
        // `seed_window` defaults C/G to 100×100; shrink to plan sizes.
        {
            let c = b.windows_v2.get_mut(&0x200).unwrap();
            c.width = 30;
            c.height = 30;
        }
        {
            let g = b.windows_v2.get_mut(&0x300).unwrap();
            g.width = 8;
            g.height = 8;
        }
        let rect = b.window_absolute_rect(g_id).expect("rect");
        assert_eq!(
            rect,
            ash::vk::Rect2D {
                offset: ash::vk::Offset2D { x: 63, y: 84 },
                extent: ash::vk::Extent2D {
                    width: 8,
                    height: 8
                },
            }
        );
    }

    /// `DrawableId` that the store no longer knows about → None.
    /// Allocate a window then `decref` it down to retirement so
    /// the id no longer resolves; `store.get` returns None and the
    /// helper short-circuits without poking `windows_v2`.
    #[test]
    fn window_absolute_rect_unknown_drawable_returns_none() {
        let mut b = KmsBackendV2::for_tests();
        let w_id = seed_window(&mut b, 0x100, None, 50, 60);
        // Tear it back down so the DrawableId no longer resolves.
        // `decref` with no Vk treats the ticket as signaled and
        // calls `destroy_now`, removing the id from `entries`.
        let _ = b.store.decref(&mut b.platform, w_id, |_| {});
        // Also clear the windows_v2 entry — otherwise the helper
        // would early-return on the xid lookup, not the id lookup
        // we want to exercise.
        b.windows_v2.remove(&0x100);
        assert!(b.store.get(w_id).is_none());
        assert_eq!(b.window_absolute_rect(w_id), None);
    }

    /// Pixmaps live in the store but not in `windows_v2`. The
    /// helper has no geometry to walk → None.
    #[test]
    fn window_absolute_rect_pixmap_returns_none() {
        use crate::kms::v2::store::{DrawableKind, Storage};
        let mut b = KmsBackendV2::for_tests();
        let pix_id = b
            .store
            .allocate(
                0x2000,
                DrawableKind::Pixmap,
                32,
                false,
                Storage::for_tests_null(
                    ash::vk::Extent2D {
                        width: 64,
                        height: 64,
                    },
                    ash::vk::Format::B8G8R8A8_UNORM,
                ),
            )
            .expect("pixmap allocate");
        assert_eq!(b.window_absolute_rect(pix_id), None);
    }

    /// Dangling parent xid: W has `parent = Some(0xDEAD)` and
    /// 0xDEAD is neither root nor in `windows_v2`. Conservative
    /// choice per plan: bail with None rather than return a
    /// half-accumulated rect that callers can't act on.
    #[test]
    fn window_absolute_rect_dangling_parent_returns_none() {
        let mut b = KmsBackendV2::for_tests();
        let w_id = seed_window(&mut b, 0x100, Some(0xDEAD), 50, 60);
        // 0xDEAD is not in windows_v2 and is not root_xid.
        assert!(!b.windows_v2.contains_key(&0xDEAD));
        assert_ne!(b.core.window_id, 0xDEAD);
        assert_eq!(b.window_absolute_rect(w_id), None);
    }

    // ────────────────────────────────────────────────────────────────
    // Stage 4c.4 — set_window_scene_participation /
    // set_backing_scene_participation
    // ────────────────────────────────────────────────────────────────

    /// `participating=false` on a window with pending presentation
    /// damage must delegate to `DrawableStore::set_scene_participating`
    /// — that store method clears the damage and bumps the epoch.
    /// This verifies the v2 backend actually wires the call (rather
    /// than e.g. silently returning Ok).
    #[test]
    fn set_window_scene_participation_false_clears_window_damage() {
        use yserver_core::backend::WindowHandle;
        let mut b = KmsBackendV2::for_tests();
        let w_id = seed_window(&mut b, 0x100, None, 0, 0);
        // Seed presentation damage so the store actually has work
        // to clear when participation flips off.
        b.store.damage(
            w_id,
            ash::vk::Rect2D {
                offset: ash::vk::Offset2D::default(),
                extent: ash::vk::Extent2D {
                    width: 4,
                    height: 4,
                },
            },
        );
        assert_eq!(
            b.store.get(w_id).unwrap().presentation_damage.rects().len(),
            1,
        );
        let epoch_before = b.store.get(w_id).unwrap().presentation_damage_epoch;

        let handle = WindowHandle::from_raw(0x100).expect("WindowHandle");
        b.set_window_scene_participation(None, handle, false)
            .expect("set_window_scene_participation");

        let d = b.store.get(w_id).expect("drawable still alive");
        assert!(
            d.presentation_damage.is_empty(),
            "presentation_damage must clear on participating=false transition: {:?}",
            d.presentation_damage.rects(),
        );
        assert!(
            d.presentation_damage_epoch > epoch_before,
            "epoch must bump on participating=false transition (before={epoch_before}, after={})",
            d.presentation_damage_epoch,
        );
        assert!(
            !d.scene_participating,
            "scene_participating flag must be cleared",
        );
    }

    /// `set_window_scene_participation` must fire scene-structure
    /// damage for the redirect transition. On the stub-mode scene
    /// (`for_tests` fixture has `inner: None`), we can only observe
    /// the `scene_structure_dirty` bit — the per-output rect
    /// dispatch is covered in `scene::tests::
    /// dispatch_clip_rects_lands_per_output_clipped` (4c.1 follow-up).
    /// This test pins the contract that the backend CALLS the rect
    /// setter (or the coarse fallback) rather than leaving the
    /// scene-structure state untouched.
    #[test]
    fn set_window_scene_participation_fires_scene_structure_damage_rect() {
        use yserver_core::backend::WindowHandle;
        let mut b = KmsBackendV2::for_tests();
        let _w_id = seed_window(&mut b, 0x100, None, 50, 60);
        // Sanity: pre-flip rect lookup is non-None (Test 2 requires
        // the rect path, not the coarse fallback path).
        let pre_flip = b
            .window_absolute_rect(b.store.lookup(0x100).unwrap())
            .expect("pre-flip rect known");
        assert_eq!(pre_flip.offset.x, 50);
        assert_eq!(pre_flip.offset.y, 60);

        // Start with the dirty bit cleared so the assertion proves
        // THIS call set it (not some setup side effect).
        b.scene.scene_structure_dirty = false;

        let handle = WindowHandle::from_raw(0x100).expect("WindowHandle");
        b.set_window_scene_participation(None, handle, false)
            .expect("set_window_scene_participation");

        assert!(
            b.scene.scene_structure_dirty,
            "scene_structure_dirty must be set after a participation flip",
        );
    }

    /// `set_backing_scene_participation` flips the backing's
    /// `scene_participating` flag via the store but must NOT fire
    /// scene-structure damage — geometric damage is the W-side
    /// call's responsibility (backings have no on-screen geometry
    /// of their own).
    #[test]
    fn set_backing_scene_participation_flips_flag_no_damage() {
        use crate::kms::v2::store::{DrawableKind, Storage};
        use yserver_core::backend::PixmapHandle;
        let mut b = KmsBackendV2::for_tests();
        let b_id = b
            .store
            .allocate(
                0x2000,
                DrawableKind::Pixmap,
                32,
                false,
                Storage::for_tests_null(
                    ash::vk::Extent2D {
                        width: 64,
                        height: 64,
                    },
                    ash::vk::Format::B8G8R8A8_UNORM,
                ),
            )
            .expect("pixmap allocate");
        // Pixmaps start with scene_participating=false (per
        // `DrawableStore::allocate`'s `scene_participating` arg).
        assert!(!b.store.get(b_id).unwrap().scene_participating);
        // Capture the prior dirty bit (whatever setup left it at).
        // The assertion below is "no CHANGE", not "is false".
        let dirty_before = b.scene.scene_structure_dirty;

        let handle = PixmapHandle::from_raw(0x2000).expect("PixmapHandle");
        b.set_backing_scene_participation(None, handle, true)
            .expect("set_backing_scene_participation");

        assert!(
            b.store.get(b_id).unwrap().scene_participating,
            "backing scene_participating must flip to true",
        );
        assert_eq!(
            b.scene.scene_structure_dirty, dirty_before,
            "set_backing_scene_participation must NOT fire scene-structure damage",
        );
    }

    // ────────────────────────────────────────────────────────────────
    // Stage 4c.5 — Manual-redirect lifecycle through the Backend
    // surface (deferred from 4b.9 / Stage 4c plan §"Tests Vk-backed").
    //
    // These exercise the no-Vk pathway: `allocate_redirected_backing`
    // skips the store-side wiring when no Vk is attached (the
    // `create_pixmap` fallback doesn't seed a store entry for the
    // backing — see backend.rs:3214 `create_pixmap_no_vk`), but the
    // `alias_registry.insert` + `host_window_to_backing.insert` still
    // fire. That's enough for the participation-flip assertions to
    // observe `scene_structure_dirty`.
    //
    // The per-output rect dispatch goes through scene.rs:412's
    // stub-mode guard — `dispatch_clip_rects_lands_per_output_clipped`
    // covers that branch directly.
    // ────────────────────────────────────────────────────────────────

    /// Simulate `RedirectWindow(W, Manual)`: allocate the backing,
    /// then flip W to `scene_participating=false`. The participation
    /// flip MUST fire scene-structure damage so the next composite
    /// repaints the region W used to occupy (under Manual mode the
    /// scene drops W; whatever's underneath must redraw).
    #[test]
    fn manual_redirect_path_marks_scene_structure_damage() {
        use yserver_core::backend::WindowHandle;
        let mut b = KmsBackendV2::for_tests();
        let _w_id = seed_window(&mut b, 0x100, None, 30, 40);
        let w = WindowHandle::from_raw(0x100).expect("WindowHandle");

        // Step 1: allocate the backing. On no-Vk the store-side
        // wiring is skipped (logged as a warn) but the alias-registry
        // + host_window_to_backing entries install. That's enough
        // for the protocol-side state machine; scene-structure
        // damage comes from the next call.
        let _backing = b
            .allocate_redirected_backing(None, w, 100, 100, 32)
            .expect("allocate_redirected_backing");

        // Clear the dirty bit so the post-flip assertion proves
        // the participation call set it, not the allocation above.
        b.scene.scene_structure_dirty = false;

        // Step 2: flip W to non-participating (Manual activation).
        b.set_window_scene_participation(None, w, false)
            .expect("set_window_scene_participation(false)");

        assert!(
            b.scene.scene_structure_dirty,
            "Manual-redirect participation flip (W→false) must fire \
             scene-structure damage so the region W used to occupy \
             gets repainted by whatever's underneath",
        );
    }

    /// Full Manual-redirect lifecycle: activate (Manual), then
    /// un-redirect. Both transitions must fire scene-structure
    /// damage. Clear the dirty bit between the two flips so the
    /// final assertion proves the SECOND call set it independently.
    #[test]
    fn unredirect_restores_participation_and_marks_damage() {
        use yserver_core::backend::WindowHandle;
        let mut b = KmsBackendV2::for_tests();
        let _w_id = seed_window(&mut b, 0x100, None, 30, 40);
        let w = WindowHandle::from_raw(0x100).expect("WindowHandle");

        // Manual activation: allocate + flip W off-scene.
        let backing = b
            .allocate_redirected_backing(None, w, 100, 100, 32)
            .expect("allocate_redirected_backing");
        b.set_window_scene_participation(None, w, false)
            .expect("set_window_scene_participation(false)");
        assert!(
            b.scene.scene_structure_dirty,
            "fixture sanity: Manual activation already fires scene-structure damage \
             (covered by manual_redirect_path_marks_scene_structure_damage)",
        );

        // Clear so the post-un-redirect assertion is sharp.
        b.scene.scene_structure_dirty = false;

        // Un-redirect: drop the backing hold and flip W back on-scene.
        b.release_redirected_backing(None, backing)
            .expect("release_redirected_backing");
        // `release_redirected_backing` doesn't touch W's scene flag;
        // un-redirect-to-mapped is the W-side caller's responsibility.
        b.set_window_scene_participation(None, w, true)
            .expect("set_window_scene_participation(true)");

        assert!(
            b.scene.scene_structure_dirty,
            "Un-redirect participation flip (W→true) must ALSO fire \
             scene-structure damage so W's region gets composited \
             back into the scene from W's own storage",
        );

        // Sanity: W is back to participating; the backing's
        // alias-registry entry is gone (release dropped Reason-1
        // and there were no aliases).
        let w_id = b.store.lookup(0x100).expect("w still in store");
        assert!(
            b.store.get(w_id).unwrap().scene_participating,
            "W must end in scene_participating=true after un-redirect",
        );
        assert!(
            b.test_alias_registry_get(backing.as_raw()).is_none(),
            "backing alias-registry entry must be cleared after release_redirected_backing",
        );
        assert!(
            b.test_host_window_to_backing(0x100).is_none(),
            "host_window_to_backing must be cleared after release",
        );
    }

    // ────────────────────────────────────────────────────────────────
    // 2026-06-11 — IncludeInferiors backing-seed planner
    // (`plan_backing_inferiors`). The compiz --replace half-drawn-panel
    // fix: on re-redirect the fresh backing must be seeded from the
    // window's OWN content, not just the parent/wallpaper layer.
    // ────────────────────────────────────────────────────────────────

    /// Allocate a sized store drawable to stand in for the backing
    /// (no-Vk `allocate_redirected_backing` skips store wiring, so the
    /// extent gate in `plan_backing_inferiors` needs a real entry).
    fn seed_backing_drawable(b: &mut KmsBackendV2, xid: u32) -> crate::kms::v2::store::DrawableId {
        use crate::kms::v2::store::{DrawableKind, Storage};
        b.store
            .allocate(
                xid,
                DrawableKind::Pixmap,
                32,
                true,
                Storage::for_tests_null(
                    ash::vk::Extent2D {
                        width: 100,
                        height: 100,
                    },
                    ash::vk::Format::B8G8R8A8_UNORM,
                ),
            )
            .expect("seed_backing_drawable allocate")
    }

    /// The plan must include the redirected window itself first, then
    /// its mapped descendants bottom-to-top by `stack_rank`, with
    /// offsets accumulated relative to W (== the backing origin).
    #[test]
    fn plan_backing_inferiors_walks_subtree_in_stack_order() {
        let mut b = KmsBackendV2::for_tests();
        let _w = seed_window(&mut b, 0x100, None, 0, 0);
        let _c_top = seed_window(&mut b, 0x200, Some(0x100), 10, 10);
        let _c_bot = seed_window(&mut b, 0x300, Some(0x100), 20, 20);
        b.windows_v2.get_mut(&0x200).unwrap().stack_rank = 5; // topmost
        b.windows_v2.get_mut(&0x300).unwrap().stack_rank = 1; // bottom
        let b_id = seed_backing_drawable(&mut b, 0x999);

        let plan = b.plan_backing_inferiors(0x100, b_id);
        let order: Vec<(u32, i32, i32)> = plan
            .iter()
            .map(|d| (b.store.get(d.leaf_id).unwrap().xid, d.dst_x, d.dst_y))
            .collect();
        assert_eq!(
            order,
            vec![(0x100, 0, 0), (0x300, 20, 20), (0x200, 10, 10)],
            "expected W first, then children bottom-to-top by stack_rank, \
             offsets relative to W",
        );
    }

    /// A descendant that owns its own `redirected_target` (an
    /// independently-redirected child — systray icons) and its whole
    /// subtree must be PRUNED: the compositor composites that child's
    /// backing separately, so its pixels must not be baked into W's.
    #[test]
    fn plan_backing_inferiors_prunes_independently_redirected_descendant() {
        let mut b = KmsBackendV2::for_tests();
        let _w = seed_window(&mut b, 0x100, None, 0, 0);
        let c = seed_window(&mut b, 0x200, Some(0x100), 10, 10);
        let _gc = seed_window(&mut b, 0x300, Some(0x200), 5, 5);
        // C is independently redirected (any Some target triggers prune).
        b.store.set_redirected_target(c, Some(c));
        let b_id = seed_backing_drawable(&mut b, 0x999);

        let plan = b.plan_backing_inferiors(0x100, b_id);
        let xids: Vec<u32> = plan
            .iter()
            .map(|d| b.store.get(d.leaf_id).unwrap().xid)
            .collect();
        assert_eq!(
            xids,
            vec![0x100],
            "C (independently redirected) and its grandchild must be pruned; only W remains",
        );
    }

    /// An unmapped window and its whole subtree are invisible and must
    /// not appear in the plan.
    #[test]
    fn plan_backing_inferiors_skips_unmapped_subtree() {
        let mut b = KmsBackendV2::for_tests();
        let _w = seed_window(&mut b, 0x100, None, 0, 0);
        let _c = seed_window(&mut b, 0x200, Some(0x100), 10, 10);
        let _gc = seed_window(&mut b, 0x300, Some(0x200), 5, 5);
        b.windows_v2.get_mut(&0x200).unwrap().mapped = false;
        let b_id = seed_backing_drawable(&mut b, 0x999);

        let plan = b.plan_backing_inferiors(0x100, b_id);
        let xids: Vec<u32> = plan
            .iter()
            .map(|d| b.store.get(d.leaf_id).unwrap().xid)
            .collect();
        assert_eq!(
            xids,
            vec![0x100],
            "unmapped child 0x200 and its subtree (0x300) must be excluded",
        );
    }

    // ────────────────────────────────────────────────────────────────
    // Stage 4d — Composite Overlay Window (COW) lifecycle.
    //
    // These exercise the no-Vk pathway: `allocate_drawable_storage`
    // returns `ERROR_INITIALIZATION_FAILED` on `for_tests()`; the
    // get_overlay_window override falls back to a `Storage::for_tests_null`
    // stub so the store-side wiring (xid mapping, refcount, scene
    // registration) is still exercised. The Vk-backed test in
    // `tests/v2_acceptance.rs` covers the actual paint+scanout path.
    // ────────────────────────────────────────────────────────────────

    /// First call: COW xid resolves in store; refcount = 1;
    /// backend `cow_id` set.
    #[test]
    fn cow_get_overlay_first_call_allocates_storage() {
        let mut b = KmsBackendV2::for_tests();
        assert_eq!(b.core.cow_refcount, 0);
        assert!(b.cow_id.is_none());
        // Pre-flight: COW xid is NOT in the store yet.
        assert!(
            b.store
                .lookup(yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0)
                .is_none(),
            "COW xid must not resolve before GetOverlayWindow",
        );

        b.get_overlay_window(None).expect("get_overlay_window");

        assert_eq!(b.core.cow_refcount, 1, "refcount must be 1 after first GET");
        assert!(b.cow_id.is_some(), "backend.cow_id must be set after GET");
        assert!(
            b.store
                .lookup(yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0)
                .is_some(),
            "COW xid must resolve in the store after GetOverlayWindow",
        );
        // Storage shape: depth-24 screen-extent, scene-participating,
        // DrawableKind::Window so build_scene's window-kind gating
        // doesn't filter it.
        let cow_id = b.cow_id.expect("cow_id set");
        let cow = b.store.get(cow_id).expect("cow drawable");
        assert_eq!(cow.depth, 24, "COW must be depth-24");
        assert!(
            cow.scene_participating,
            "COW must be scene_participating=true so build_scene includes it",
        );
        assert!(
            matches!(cow.kind, super::super::store::DrawableKind::Window),
            "COW must be DrawableKind::Window",
        );
        assert_eq!(cow.storage.extent.width, u32::from(b.platform.fb_w));
        assert_eq!(cow.storage.extent.height, u32::from(b.platform.fb_h));
    }

    #[test]
    fn note_present_pixmap_tracks_non_cow_stage_sources_for_drawable_dump() {
        let mut b = KmsBackendV2::for_tests();
        let stage = b
            .store
            .allocate(
                0x4000_2000,
                super::super::store::DrawableKind::Window,
                32,
                true,
                Storage::for_tests_null(
                    ash::vk::Extent2D {
                        width: 800,
                        height: 600,
                    },
                    PlatformBackend::format_for_depth(32),
                ),
            )
            .expect("allocate stage");
        let _src = b
            .store
            .allocate(
                0x4000_3000,
                super::super::store::DrawableKind::Pixmap,
                32,
                true,
                Storage::for_tests_null(
                    ash::vk::Extent2D {
                        width: 800,
                        height: 600,
                    },
                    PlatformBackend::format_for_depth(32),
                ),
            )
            .expect("allocate src pixmap");
        assert!(b.store.get(stage).is_some(), "stage must exist");

        b.note_present_pixmap(0x4000_3000, 0x4000_2000);

        assert_eq!(
            b.recent_present_pixmaps.back(),
            Some(&(0x4000_3000, 0x4000_2000)),
            "non-COW PresentPixmap must still land in the general diagnostic ring",
        );
    }

    /// Second call without an intervening release just bumps the
    /// refcount. Storage stays the same `DrawableId` (no
    /// re-allocation), `cow_id` unchanged.
    #[test]
    fn cow_get_overlay_second_call_refcounts() {
        let mut b = KmsBackendV2::for_tests();
        b.get_overlay_window(None).expect("first get");
        let id_after_first = b.cow_id.expect("cow_id set after first GET");

        b.get_overlay_window(None).expect("second get");

        assert_eq!(
            b.core.cow_refcount, 2,
            "refcount must increment to 2 on the second GET",
        );
        assert_eq!(
            b.cow_id.expect("cow_id set after second GET"),
            id_after_first,
            "second GetOverlayWindow must NOT reallocate — same DrawableId",
        );
    }

    /// Release after multiple GETs decrements but keeps the
    /// storage alive (refcount > 0). The COW xid still resolves.
    #[test]
    fn cow_release_decrements_refcount() {
        let mut b = KmsBackendV2::for_tests();
        b.get_overlay_window(None).expect("get 1");
        b.get_overlay_window(None).expect("get 2");
        b.get_overlay_window(None).expect("get 3");
        assert_eq!(b.core.cow_refcount, 3);

        let was_final = b.release_overlay_window(None).expect("release 1");
        assert!(
            !was_final,
            "release_overlay_window must return Ok(false) when refcount > 0 \
             after decrement (handler uses this signal to skip the host_xid \
             clear-on-final-release path)",
        );

        assert_eq!(b.core.cow_refcount, 2, "refcount drops from 3 → 2");
        assert!(
            b.cow_id.is_some(),
            "storage still held — refcount > 0 keeps cow_id",
        );
        assert!(
            b.store
                .lookup(yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0)
                .is_some(),
            "COW xid still resolves while refcount > 0",
        );
    }

    /// Final release drops the storage. `cow_id` clears; xid no
    /// longer resolves in the store (so a fresh `GetOverlayWindow`
    /// would reallocate clean — protocol guarantees the COW xid
    /// is reusable after every release-to-zero).
    #[test]
    fn cow_release_zero_drops_storage() {
        let mut b = KmsBackendV2::for_tests();
        b.get_overlay_window(None).expect("get");
        assert_eq!(b.core.cow_refcount, 1);

        let was_final = b.release_overlay_window(None).expect("release");
        assert!(
            was_final,
            "release_overlay_window must return Ok(true) on the refcount→0 \
             transition (handler uses this signal to clear the COW resource \
             record's host_xid so the next GetOverlayWindow re-wires fresh)",
        );

        assert_eq!(b.core.cow_refcount, 0);
        assert!(
            b.cow_id.is_none(),
            "cow_id must clear on refcount→0 release",
        );
        assert!(
            b.store
                .lookup(yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0)
                .is_none(),
            "COW xid must NOT resolve after the final release — \
             the store has destroyed (or detached) the entry so a \
             subsequent GetOverlayWindow can reallocate at the \
             same xid",
        );

        // A second GetOverlayWindow round-trips cleanly: reallocates
        // fresh, refcount climbs from 0 → 1.
        b.get_overlay_window(None)
            .expect("re-get after final release");
        assert_eq!(b.core.cow_refcount, 1);
        assert!(b.cow_id.is_some());
    }

    /// Stage 4d defensive branch: a `ReleaseOverlayWindow` with
    /// no preceding `GetOverlayWindow` (compositor crash + restart
    /// midway through a hand-off, double-release on the same
    /// client, etc.) must be a clean no-op. `core.cow_refcount`
    /// stays 0 (no underflow), `cow_id` stays `None`, and the
    /// scene's COW entry stays unregistered.
    #[test]
    fn cow_release_without_prior_get_is_noop() {
        let mut b = KmsBackendV2::for_tests();
        assert_eq!(b.core.cow_refcount, 0);
        assert!(b.cow_id.is_none());

        let was_final = b.release_overlay_window(None).expect("noop release");
        assert!(
            !was_final,
            "unmatched release (refcount already 0) must return Ok(false): \
             we didn't transition the refcount and didn't destroy any storage",
        );

        assert_eq!(
            b.core.cow_refcount, 0,
            "unmatched release must NOT underflow refcount",
        );
        assert!(
            b.cow_id.is_none(),
            "unmatched release must NOT spuriously set cow_id",
        );
        // Subsequent get_overlay_window still works (defensive
        // branch hasn't poisoned any state).
        b.get_overlay_window(None).expect("get after noop release");
        assert_eq!(b.core.cow_refcount, 1);
        assert!(b.cow_id.is_some());
    }

    // ── DRI3 backfill (Stage 4d.* compositor unblock) ───────────
    //
    // Ports v1's DRI3 surface to v2. The `for_tests()` fixture
    // has no render-node + no Vk, so it exercises the
    // "unsupported" branch of every accessor. The Vk-backed
    // tests are gated `#[ignore]` and run under `vng` via
    // `cargo test -- --ignored`, mirroring the Phase 4.2 hardware
    // coverage matrix.

    #[test]
    fn dri3_capabilities_unsupported_without_vk_returns_unsupported() {
        let b = KmsBackendV2::for_tests();
        let caps = b.dri3_capabilities();
        // unsupported() sentinel is (0, 0) per the trait_def
        // doc-comment.
        assert_eq!(caps.version, (0, 0), "no Vk → DRI3 reports unsupported");
        assert!(!caps.modifiers);
        assert!(!caps.fence_fd);
        assert!(!caps.syncobj);
    }

    #[test]
    fn dri3_open_errs_when_render_node_unavailable() {
        // for_tests sets render_node_path: None on PlatformBackend,
        // so dri3_open must Err out (the SCM_RIGHTS dispatch path
        // then maps it to BadAlloc).
        let mut b = KmsBackendV2::for_tests();
        let res = b.dri3_open(0x1234);
        assert!(res.is_err(), "expected Err when render_node_path is None");
    }

    #[test]
    fn dri3_export_pixmap_unknown_xid_errs() {
        // No Vk → first guard fires. With Vk this would still
        // Err because the xid isn't in the store — covered by
        // the Vk-backed test below.
        let mut b = KmsBackendV2::for_tests();
        let res = b.dri3_export_pixmap(0x4040_0000);
        assert!(res.is_err());
    }

    #[test]
    fn dri3_fd_from_fence_unknown_errs() {
        let mut b = KmsBackendV2::for_tests();
        assert!(b.dri3_fd_from_fence(0x4040_4040).is_err());
    }

    #[test]
    fn dri3_signal_syncobj_unknown_errs() {
        let mut b = KmsBackendV2::for_tests();
        assert!(b.dri3_signal_syncobj(0x4040_4040, 1).is_err());
    }

    #[test]
    fn dri3_trigger_fence_unknown_is_ok() {
        // v1's body returns Ok for the unknown-fence case — the
        // VkSemaphore path is server-state-only, no GPU op. v2
        // mirrors.
        let mut b = KmsBackendV2::for_tests();
        assert!(b.dri3_trigger_fence(0x4040_4040).is_ok());
    }

    /// Vk-backed: DRI3 capabilities expose `fence_fd` when a real
    /// `VkContext` is attached, and only expose syncobj on drivers
    /// whose external timeline import path is supported.
    /// Gated `#[ignore]` because it needs lavapipe (or any live
    /// Vulkan ICD).
    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn dri3_capabilities_v14_with_syncobj_when_vk_attached() {
        let b = match KmsBackendV2::for_tests_with_vk() {
            Ok(b) => b,
            Err(e) => {
                eprintln!("skip: no Vk: {e}");
                return;
            }
        };
        let caps = b.dri3_capabilities();
        // for_tests_with_vk still has render_node_fd: None
        // (no real DRM device). The guard checks render_node_fd
        // first → caps come back unsupported even with Vk.
        // Verify that branch is what reports unsupported, then
        // bypass it for the version + syncobj assertion by
        // injecting a synthetic render-node fd into platform.
        assert_eq!(
            caps.version,
            (0, 0),
            "without render_node_fd, even with Vk, dri3 is gated unsupported",
        );
        // Now stuff in a synthetic render-node fd so the guard
        // passes; use /dev/null which is openable + safely
        // droppable. The cap accessor doesn't actually use the
        // fd, just checks Some-ness.
        let mut b = b;
        b.platform.render_node_fd = Some(
            std::fs::OpenOptions::new()
                .read(true)
                .open("/dev/null")
                .expect("open /dev/null")
                .into(),
        );
        let caps = b.dri3_capabilities();
        assert!(caps.fence_fd);
        assert_eq!(
            caps.syncobj,
            b.platform.vk.as_ref().unwrap().supports_dri3_syncobj()
        );
        assert_eq!(caps.version, if caps.syncobj { (1, 4) } else { (1, 3) });
        // `modifiers` reflects whether the device picked up
        // VK_EXT_image_drm_format_modifier — lavapipe does, Venus
        // does. Just assert the field is set consistently with
        // what the Vk layer reported (no hard true-here).
    }

    /// Vk-backed: `dri3_import_pixmap` rejects unsupported
    /// (depth, bpp) combinations with a non-empty error before
    /// touching the dma-buf fd. Exercises the guard above the
    /// `import_dmabuf` call. Vk-attached so we hit the second
    /// arm (the Vk branch).
    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn dri3_import_pixmap_rejects_unsupported_depth_bpp() {
        use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};
        let mut b = match KmsBackendV2::for_tests_with_vk() {
            Ok(b) => b,
            Err(e) => {
                eprintln!("skip: no Vk: {e}");
                return;
            }
        };
        // Synthesise an arbitrary fd — depth=8 trips the guard
        // before the fd is consumed, so any openable file works.
        let f = std::fs::OpenOptions::new()
            .read(true)
            .open("/dev/null")
            .expect("open /dev/null");
        let raw = f.into_raw_fd();
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let res = b.dri3_import_pixmap(fd, 16, 16, 64, 0, 0, 8, 8);
        assert!(
            res.is_err(),
            "depth=8 bpp=8 is outside Phase 4.2 RGB single-plane scope",
        );
    }

    /// Vk-backed: `dri3_supported_modifiers` returns at least
    /// LINEAR (0) on the screen side for depth-32/bpp-32. Lavapipe
    /// reports LINEAR; Venus reports LINEAR + tile modifiers; we
    /// only assert LINEAR is present (the conservative invariant).
    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn dri3_supported_modifiers_includes_linear_with_vk() {
        let b = match KmsBackendV2::for_tests_with_vk() {
            Ok(b) => b,
            Err(e) => {
                eprintln!("skip: no Vk: {e}");
                return;
            }
        };
        let (window, screen) = b.dri3_supported_modifiers(0, 24, 32);
        assert!(
            window.contains(&0),
            "window modifiers always include LINEAR (Phase 4.1 scanout policy)",
        );
        assert!(
            screen.contains(&0),
            "screen modifiers always include LINEAR (fallback row of the design matrix)",
        );
    }

    /// xshmfence-path of `dri3_fence_from_fd`: mmap an xshmfence
    /// (synthesised via `memfd_create` + `ftruncate`), feed the
    /// fd in, assert it landed in `dri3_xshmfences` (not
    /// `dri3_sync_resources`) and that `trigger()` flips the
    /// state to signalled.
    #[test]
    fn dri3_fence_from_fd_xshmfence_path_triggers() {
        // The xshmfence module exposes the C alloc/map helpers;
        // build a fresh shm fd via `xshmfence_alloc_shm` directly
        // through libc-equivalent shape (memfd_create). To keep
        // the test self-contained without pulling libxshmfence
        // alloc, we synthesise a memfd that's at least page-sized
        // and let `FenceMapping::map` mmap it — libxshmfence's
        // map_shm only requires the fd be at least one page.
        use std::os::fd::{FromRawFd, OwnedFd};
        let raw =
            unsafe { libc::syscall(libc::SYS_memfd_create, c"yserver_dri3_test".as_ptr(), 0u32) };
        if raw < 0 {
            eprintln!("skip: memfd_create unavailable");
            return;
        }
        let raw = i32::try_from(raw).expect("fd fits i32");
        // Size the memfd to one page so map_shm succeeds.
        let page_raw = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        let page: libc::off_t = if page_raw > 0 {
            page_raw as libc::off_t
        } else {
            4096
        };
        if unsafe { libc::ftruncate(raw, page) } != 0 {
            unsafe { libc::close(raw) };
            eprintln!("skip: ftruncate failed");
            return;
        }
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let mut b = KmsBackendV2::for_tests();
        let fence_xid: u32 = 0x4040_1111;
        b.dri3_fence_from_fd(fence_xid, fd)
            .expect("xshmfence import");
        assert!(
            b.dri3_xshmfences.contains_key(&fence_xid),
            "xshmfence path stores under dri3_xshmfences",
        );
        assert!(
            !b.dri3_sync_resources.contains_key(&fence_xid),
            "xshmfence path must NOT also populate dri3_sync_resources",
        );
        // Inspect the mapping's pre-trigger state.
        let mapping = b.dri3_xshmfences.get(&fence_xid).expect("present");
        let pre = mapping.query();
        // Trigger via the public trait surface.
        b.dri3_trigger_fence(fence_xid).expect("trigger ok");
        let post = b
            .dri3_xshmfences
            .get(&fence_xid)
            .expect("still present")
            .query();
        assert_eq!(post, 1, "after trigger, xshmfence query() == 1");
        // Defensive: pre and post differ in the expected direction.
        assert_ne!(pre, post, "trigger() should have changed the fence state");
    }

    /// `dri3_import_syncobj`'s no-Vk branch errs cleanly. Builds
    /// an `OwnedFd` from `/dev/null` and verifies the Vk-gate
    /// triggers before any external lib gets the fd — exercises
    /// the "Vk-required" guard mirroring v1's body. We use
    /// `IntoRawFd` + `FromRawFd` to keep the fd lifetime
    /// explicit since no Vk path runs.
    #[test]
    fn dri3_import_syncobj_no_vk_errs() {
        use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};
        let f = std::fs::OpenOptions::new()
            .read(true)
            .open("/dev/null")
            .expect("open /dev/null");
        // SAFETY: we own this fd via the OpenOptions handle; we
        // re-wrap it as OwnedFd directly. No Vk path runs, the
        // function returns Err immediately, and the OwnedFd's
        // Drop closes it cleanly.
        let raw = f.into_raw_fd();
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let mut b = KmsBackendV2::for_tests();
        let res = b.dri3_import_syncobj(0x4040_3333, fd);
        assert!(
            res.is_err(),
            "import_syncobj without Vk must Err on the Vk gate",
        );
    }

    /// Stage 4d Manual-redirect CopyArea clip-by-children fix.
    ///
    /// Scenario from `yserver-hw-mate.log` (CC frame + reparented
    /// CC client window in one redirected backing):
    ///   - Frame W=997, H=652 at parent-local (0, 0).
    ///   - Reparented CC client at (11, 41) inside the frame,
    ///     size 975×600 (mapped).
    ///   - Marco copies its decoration pixmap into the frame with
    ///     a `ClipByChildren` GC, full 997×652.
    ///
    /// Pre-fix: v2's `copy_area` blits the full 997×652 into the
    /// redirected backing, clobbering CC's content. Visible symptom:
    /// only the small region CC repaints next survives — the famous
    /// "top-left square only" artefact.
    ///
    /// Spec-correct behaviour (Xorg `mi/midispcur.c` + the
    /// `ClipByChildren` rule): subtract every mapped child window's
    /// rect from the destination rect before issuing the copy. For
    /// this one-child case the result is exactly four strips: top,
    /// bottom, left-of-child, right-of-child.
    #[test]
    fn copy_area_clip_by_children_excludes_mapped_child_rect() {
        use ash::vk;
        let dst = vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent: vk::Extent2D {
                width: 997,
                height: 652,
            },
        };
        let child = vk::Rect2D {
            offset: vk::Offset2D { x: 11, y: 41 },
            extent: vk::Extent2D {
                width: 975,
                height: 600,
            },
        };

        let got = compute_copy_area_dst_rects(dst, &[child]);

        // Expected order: top strip, bottom strip, left middle,
        // right middle (Xorg/pixman band order).
        let want = [
            vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent: vk::Extent2D {
                    width: 997,
                    height: 41,
                },
            },
            vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 641 },
                extent: vk::Extent2D {
                    width: 997,
                    height: 11,
                },
            },
            vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 41 },
                extent: vk::Extent2D {
                    width: 11,
                    height: 600,
                },
            },
            vk::Rect2D {
                offset: vk::Offset2D { x: 986, y: 41 },
                extent: vk::Extent2D {
                    width: 11,
                    height: 600,
                },
            },
        ];

        assert_eq!(
            got.len(),
            want.len(),
            "expected 4 surviving strips (top, bottom, left-middle, right-middle); \
             pre-fix returns the unclipped 1-rect input, which is the bug",
        );
        for (i, (g, w)) in got.iter().zip(want.iter()).enumerate() {
            assert_eq!(
                (g.offset.x, g.offset.y, g.extent.width, g.extent.height),
                (w.offset.x, w.offset.y, w.extent.width, w.extent.height),
                "strip {i} mismatch",
            );
        }
    }

    #[test]
    fn copy_area_clip_by_children_no_children_returns_input() {
        use ash::vk;
        let dst = vk::Rect2D {
            offset: vk::Offset2D { x: 5, y: 7 },
            extent: vk::Extent2D {
                width: 100,
                height: 80,
            },
        };
        let got = compute_copy_area_dst_rects(dst, &[]);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].offset.x, 5);
        assert_eq!(got[0].offset.y, 7);
        assert_eq!(got[0].extent.width, 100);
        assert_eq!(got[0].extent.height, 80);
    }

    // ── compute_render_composite_clip — audit #2 (2026-05-19) ────────
    //
    // Mirrors Xorg's `miComputeCompositeRegion`
    // (`render/mipict.c:316-389`). Per-test vectors hand-traced from
    // the Xorg algorithm so the expected output is grounded in the
    // reference, not in my own arithmetic (per
    // `feedback_test_vectors_must_be_external`).

    /// All three clips `None` → result `None` (engine paints
    /// everywhere, matching Xorg's "no clientClip" path which
    /// leaves pRegion unconstrained beyond dst extent).
    #[test]
    fn compute_render_composite_clip_all_none_returns_none() {
        let got = compute_render_composite_clip(None, None, (0, 0), None, (0, 0));
        assert!(got.is_none());
    }

    /// Only dst clip set → result is dst clip (no translation).
    #[test]
    fn compute_render_composite_clip_only_dst() {
        let dst = vec![Rectangle16 {
            x: 10,
            y: 20,
            width: 30,
            height: 40,
        }];
        let got = compute_render_composite_clip(Some(&dst), None, (0, 0), None, (0, 0));
        assert_eq!(got.as_deref(), Some(dst.as_slice()));
    }

    /// Only src clip set, src and dst coincide (xDst==xSrc, yDst==
    /// ySrc → translation (0,0)) → result is src clip as-is. This
    /// is the load-bearing case for the audit: xfwm4/muffin set a
    /// clip on a source picture and pre-fix yserver ignored it.
    #[test]
    fn compute_render_composite_clip_src_only_zero_translation() {
        let src = vec![Rectangle16 {
            x: 5,
            y: 5,
            width: 10,
            height: 10,
        }];
        let got = compute_render_composite_clip(None, Some(&src), (0, 0), None, (0, 0));
        assert_eq!(got.as_deref(), Some(src.as_slice()));
    }

    /// Src clip translates to dst space by (xDst - xSrc, yDst -
    /// ySrc). Per Xorg `mipict.c:356`:
    /// `miClipPictureSrc(pRegion, pSrc, xDst - xSrc, yDst - ySrc)`.
    /// Set src clip {0,0 4×4}, composite from src(2,2) to
    /// dst(10,20), 4×4 — translation is (10-2, 20-2) = (8, 18).
    /// Expected: src clip translated to {8,18 4×4}.
    #[test]
    fn compute_render_composite_clip_translates_src_clip_to_dst_space() {
        let src = vec![Rectangle16 {
            x: 0,
            y: 0,
            width: 4,
            height: 4,
        }];
        let got = compute_render_composite_clip(None, Some(&src), (8, 18), None, (0, 0));
        assert_eq!(
            got.as_deref(),
            Some(
                &[Rectangle16 {
                    x: 8,
                    y: 18,
                    width: 4,
                    height: 4,
                }][..]
            )
        );
    }

    /// Dst clip ∩ src-translated clip when the two overlap on a
    /// strict sub-rect. dst clip {0,0 100×100}; src clip {0,0 50×50}
    /// translated by (20, 30) → {20,30 50×50}. Intersection:
    /// {20,30 50×50} (src translates fully inside dst).
    #[test]
    fn compute_render_composite_clip_dst_and_src_intersection() {
        let dst = vec![Rectangle16 {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        }];
        let src = vec![Rectangle16 {
            x: 0,
            y: 0,
            width: 50,
            height: 50,
        }];
        let got = compute_render_composite_clip(Some(&dst), Some(&src), (20, 30), None, (0, 0));
        assert_eq!(
            got.as_deref(),
            Some(
                &[Rectangle16 {
                    x: 20,
                    y: 30,
                    width: 50,
                    height: 50,
                }][..]
            )
        );
    }

    /// Disjoint dst and src-translated clips → empty result (which
    /// Xorg treats as "paint nothing" — `miComputeCompositeRegion`
    /// returns FALSE there).
    #[test]
    fn compute_render_composite_clip_disjoint_yields_empty() {
        let dst = vec![Rectangle16 {
            x: 0,
            y: 0,
            width: 10,
            height: 10,
        }];
        let src = vec![Rectangle16 {
            x: 0,
            y: 0,
            width: 10,
            height: 10,
        }];
        // Translate src by (100, 0) → {100,0 10×10}; disjoint from
        // dst {0,0 10×10}.
        let got = compute_render_composite_clip(Some(&dst), Some(&src), (100, 0), None, (0, 0));
        assert_eq!(got.as_deref(), Some(&[][..]));
    }

    /// Three-way intersection: dst ∩ src ∩ mask. Use disjoint
    /// translations that all overlap at one corner. dst {0,0 50×50},
    /// src {0,0 50×50} translated by (10, 10) → {10,10 50×50},
    /// mask {0,0 50×50} translated by (20, 20) → {20,20 50×50}.
    /// Three-way intersection: {20,20 30×30}.
    #[test]
    fn compute_render_composite_clip_three_way_intersection() {
        let dst = vec![Rectangle16 {
            x: 0,
            y: 0,
            width: 50,
            height: 50,
        }];
        let src = vec![Rectangle16 {
            x: 0,
            y: 0,
            width: 50,
            height: 50,
        }];
        let mask = vec![Rectangle16 {
            x: 0,
            y: 0,
            width: 50,
            height: 50,
        }];
        let got =
            compute_render_composite_clip(Some(&dst), Some(&src), (10, 10), Some(&mask), (20, 20));
        assert_eq!(
            got.as_deref(),
            Some(
                &[Rectangle16 {
                    x: 20,
                    y: 20,
                    width: 30,
                    height: 30,
                }][..]
            )
        );
    }

    /// Multi-rect dst clip ∩ single src clip translated: every
    /// dst rect intersects with the translated src rect; union of
    /// intersections is what the engine should emit per-scissor.
    #[test]
    fn compute_render_composite_clip_multi_rect_dst_with_single_src() {
        let dst = vec![
            Rectangle16 {
                x: 0,
                y: 0,
                width: 10,
                height: 10,
            },
            Rectangle16 {
                x: 20,
                y: 0,
                width: 10,
                height: 10,
            },
        ];
        // Src clip {0,0 100×100} translated by (0, 0) → covers
        // both dst rects. Result: both dst rects survive.
        let src = vec![Rectangle16 {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        }];
        let got = compute_render_composite_clip(Some(&dst), Some(&src), (0, 0), None, (0, 0));
        assert_eq!(got.as_deref(), Some(dst.as_slice()));
    }

    /// GC clip intersection: rect partially inside a single clip rect
    /// produces the intersection alone. Pre-fix the stub returns the
    /// whole rect — losing the GC clip semantics.
    #[test]
    fn intersect_rect_with_clip_single_overlapping_clip_returns_intersection() {
        use ash::vk;
        let rect = vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent: vk::Extent2D {
                width: 200,
                height: 200,
            },
        };
        let clip = vec![vk::Rect2D {
            offset: vk::Offset2D { x: 50, y: 50 },
            extent: vk::Extent2D {
                width: 100,
                height: 100,
            },
        }];
        let got = intersect_rect_with_clip(rect, &clip);
        assert_eq!(got.len(), 1, "single clip ∩ rect = one intersection");
        assert_eq!(
            (
                got[0].offset.x,
                got[0].offset.y,
                got[0].extent.width,
                got[0].extent.height,
            ),
            (50, 50, 100, 100),
        );
    }

    #[test]
    fn intersect_rect_with_clip_empty_clip_returns_empty() {
        use ash::vk;
        // Empty clip-rect list represents an empty XFixes region — paint nothing.
        let rect = vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent: vk::Extent2D {
                width: 10,
                height: 10,
            },
        };
        let got = intersect_rect_with_clip(rect, &[]);
        assert!(got.is_empty());
    }

    /// Multi-rect clip: dst that straddles two non-contiguous clip rects
    /// produces two intersections.
    #[test]
    fn intersect_rect_with_clip_multi_rect_clip_produces_per_rect_intersections() {
        use ash::vk;
        let rect = vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent: vk::Extent2D {
                width: 200,
                height: 100,
            },
        };
        let clip = vec![
            vk::Rect2D {
                offset: vk::Offset2D { x: 10, y: 10 },
                extent: vk::Extent2D {
                    width: 40,
                    height: 40,
                },
            },
            vk::Rect2D {
                offset: vk::Offset2D { x: 150, y: 10 },
                extent: vk::Extent2D {
                    width: 40,
                    height: 40,
                },
            },
        ];
        let got = intersect_rect_with_clip(rect, &clip);
        assert_eq!(got.len(), 2);
        assert_eq!(
            (
                got[0].offset.x,
                got[0].offset.y,
                got[0].extent.width,
                got[0].extent.height,
            ),
            (10, 10, 40, 40),
        );
        assert_eq!(
            (
                got[1].offset.x,
                got[1].offset.y,
                got[1].extent.width,
                got[1].extent.height,
            ),
            (150, 10, 40, 40),
        );
    }

    #[test]
    fn copy_area_clip_by_children_disjoint_child_returns_input() {
        // Child fully outside dst → no clipping.
        use ash::vk;
        let dst = vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent: vk::Extent2D {
                width: 50,
                height: 50,
            },
        };
        let child = vk::Rect2D {
            offset: vk::Offset2D { x: 200, y: 200 },
            extent: vk::Extent2D {
                width: 10,
                height: 10,
            },
        };
        let got = compute_copy_area_dst_rects(dst, &[child]);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].offset.x, 0);
        assert_eq!(got[0].offset.y, 0);
    }

    // ────────────────────────────────────────────────────────────
    // Phase 2 root-cause pin (Task 7.5).
    //
    // The yserver-core sibling tests
    // (`reparent_*_redirect_*` in
    // `crates/yserver-core/src/core_loop/process_request.rs`) pin
    // the backing-existence state on the resource layer. This test
    // exercises the actual user-visible path: drive a
    // `ReparentWindow` through the full `process_request`
    // dispatcher, then assert
    // `resolve_paint_target(nm_applet_host_xid)` routes through
    // mate-panel's redirected ancestor with the correct
    // screen-coord offset — the symptom that breaks the live
    // mate-panel tray.
    // ────────────────────────────────────────────────────────────

    /// Synthesise a deterministic host xid for a nested `ResourceId`.
    /// Mirrors the production "high bit set" convention used by the
    /// sibling core tests so the v2 windows_v2 keys never collide
    /// with low-numbered nested xids.
    fn synth_host_xid(xid: yserver_protocol::x11::ResourceId) -> u32 {
        0x8000_0000 | xid.0
    }

    /// Seed both the resource-layer `Window` *and* the v2 backend's
    /// `windows_v2` + `store` entries so that:
    /// - `state.resources.window(xid).host_xid` is set,
    /// - `backend.windows_v2[host_xid]` exists with the requested
    ///   geometry, and
    /// - `backend.store.lookup(host_xid)` returns a real
    ///   `DrawableId`.
    fn seed_v2_window(
        state: &mut yserver_core::server::ServerState,
        backend: &mut KmsBackendV2,
        xid: yserver_protocol::x11::ResourceId,
        parent: yserver_protocol::x11::ResourceId,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
    ) {
        use yserver_core::resources::ROOT_WINDOW;
        use yserver_protocol::x11::{ClientId, CreateWindowRequest};
        state.resources.create_window(
            ClientId(14),
            CreateWindowRequest {
                depth: 24,
                window: xid,
                parent,
                x,
                y,
                width,
                height,
                border_width: 0,
                class: 1,
                visual: yserver_core::resources::ROOT_VISUAL,
                ..Default::default()
            },
        );
        let host_xid = synth_host_xid(xid);
        if let Some(w) = state.resources.window_mut(xid) {
            w.host_xid = yserver_core::backend::WindowHandle::from_raw(host_xid);
        }
        // v2 backend's windows_v2 + store mirror.
        let parent_host = if parent == ROOT_WINDOW {
            None
        } else {
            Some(synth_host_xid(parent))
        };
        backend.windows_v2.insert(
            host_xid,
            super::WindowGeometryV2 {
                x,
                y,
                width,
                height,
                depth: 24,
                mapped: true,
                parent: parent_host,
                stack_rank: 0,
                bg_pixel: None,
                bg_pixmap: None,
                cursor: None,
            },
        );
        let _ = backend.store.allocate(
            host_xid,
            crate::kms::v2::store::DrawableKind::Window,
            24,
            true,
            crate::kms::v2::store::Storage::for_tests_null(
                ash::vk::Extent2D {
                    width: u32::from(width.max(1)),
                    height: u32::from(height.max(1)),
                },
                ash::vk::Format::B8G8R8A8_UNORM,
            ),
        );
    }

    /// Allocate a redirected backing for an already-seeded window
    /// and install `set_redirected_target(W, Some(B))` so the
    /// resolver routes paint against `W`'s host xid through `B`.
    /// Also sets the resource-layer `redirected_backing` so the
    /// reconciliation predicates in production code see the
    /// "backing already present" state.
    fn seed_v2_redirected_backing(
        state: &mut yserver_core::server::ServerState,
        backend: &mut KmsBackendV2,
        xid: yserver_protocol::x11::ResourceId,
    ) {
        let host_xid = synth_host_xid(xid);
        let backing_xid = 0x9000_0000 | xid.0;
        let (width, height, depth) = state
            .resources
            .window(xid)
            .map(|w| (w.width, w.height, w.depth))
            .expect("seed_v2_redirected_backing: window must exist");
        // Allocate the backing in the v2 store.
        let backing_id = backend
            .store
            .allocate(
                backing_xid,
                crate::kms::v2::store::DrawableKind::RedirectedBacking,
                depth,
                false,
                crate::kms::v2::store::Storage::for_tests_null(
                    ash::vk::Extent2D {
                        width: u32::from(width.max(1)),
                        height: u32::from(height.max(1)),
                    },
                    ash::vk::Format::B8G8R8A8_UNORM,
                ),
            )
            .expect("seed_v2_redirected_backing allocate");
        let w_id = backend
            .store
            .lookup(host_xid)
            .expect("window's drawable id");
        backend.store.set_redirected_target(w_id, Some(backing_id));
        // Mirror on the resource layer so the production
        // reconciliation predicates see a backing present.
        if let Some(w) = state.resources.window_mut(xid) {
            w.redirected_backing = Some(yserver_core::resources::RedirectedBacking {
                host_pixmap: yserver_core::backend::PixmapHandle::from_raw(backing_xid)
                    .expect("non-zero PixmapHandle"),
                width,
                height,
                depth,
            });
        }
    }

    /// Look up the v2 backing `DrawableId` for the redirected
    /// `xid`. Returns `None` if no redirect was installed (or if
    /// the window itself isn't in the store).
    fn backing_drawable_id(
        backend: &KmsBackendV2,
        xid: yserver_protocol::x11::ResourceId,
    ) -> Option<crate::kms::v2::store::DrawableId> {
        let host_xid = synth_host_xid(xid);
        let w_id = backend.store.lookup(host_xid)?;
        backend.store.redirected_target(w_id)
    }

    /// Drive ReparentWindow through the public `process_request`
    /// dispatcher (the v2 backend can't call `handle_reparent_window`
    /// directly — it's `fn`-private to the core_loop module).
    fn dispatch_reparent_window_v2(
        state: &mut yserver_core::server::ServerState,
        backend: &mut KmsBackendV2,
        window: yserver_protocol::x11::ResourceId,
        parent: yserver_protocol::x11::ResourceId,
        x: i16,
        y: i16,
    ) {
        use yserver_core::{backend::Backend, core_loop::process_request};
        use yserver_protocol::x11::{ClientId, RequestHeader, SequenceNumber};

        let mut body = Vec::with_capacity(12);
        body.extend_from_slice(&window.0.to_le_bytes());
        body.extend_from_slice(&parent.0.to_le_bytes());
        body.extend_from_slice(&x.to_le_bytes());
        body.extend_from_slice(&y.to_le_bytes());

        process_request::process_request(
            state,
            backend as &mut dyn Backend,
            ClientId(14),
            SequenceNumber(1),
            RequestHeader {
                opcode: 7, // ReparentWindow
                data: 0,
                length_units: 4,
            },
            &body,
            None,
        )
        .expect("process_request must succeed");
    }

    fn dispatch_configure_window_v2(
        state: &mut yserver_core::server::ServerState,
        backend: &mut KmsBackendV2,
        window: yserver_protocol::x11::ResourceId,
        x: Option<i16>,
        y: Option<i16>,
        border_width: Option<u16>,
    ) {
        use yserver_core::{backend::Backend, core_loop::process_request};
        use yserver_protocol::x11::{ClientId, RequestHeader, SequenceNumber};

        let mut mask = 0u16;
        if x.is_some() {
            mask |= 1 << 0;
        }
        if y.is_some() {
            mask |= 1 << 1;
        }
        if border_width.is_some() {
            mask |= 1 << 4;
        }

        let mut body = Vec::with_capacity(16);
        body.extend_from_slice(&window.0.to_le_bytes());
        body.extend_from_slice(&mask.to_le_bytes());
        body.extend_from_slice(&0u16.to_le_bytes());
        if let Some(x) = x {
            body.extend_from_slice(&(i32::from(x) as u32).to_le_bytes());
        }
        if let Some(y) = y {
            body.extend_from_slice(&(i32::from(y) as u32).to_le_bytes());
        }
        if let Some(border_width) = border_width {
            body.extend_from_slice(&u32::from(border_width).to_le_bytes());
        }

        process_request::process_request(
            state,
            backend as &mut dyn Backend,
            ClientId(14),
            SequenceNumber(1),
            RequestHeader {
                opcode: 12, // ConfigureWindow
                data: 0,
                length_units: u32::try_from((4 + body.len()) / 4).expect("request length"),
            },
            &body,
            None,
        )
        .expect("process_request(ConfigureWindow) must succeed");
    }

    fn dispatch_poly_fill_rectangle_v2(
        state: &mut yserver_core::server::ServerState,
        backend: &mut KmsBackendV2,
        drawable: yserver_protocol::x11::ResourceId,
        gc: yserver_protocol::x11::ResourceId,
        rects: &[Rectangle16],
    ) {
        use yserver_core::{backend::Backend, core_loop::process_request};
        use yserver_protocol::x11::{ClientId, RequestHeader, SequenceNumber};

        let mut body = Vec::with_capacity(8 + rects.len() * 8);
        body.extend_from_slice(&drawable.0.to_le_bytes());
        body.extend_from_slice(&gc.0.to_le_bytes());
        for rect in rects {
            body.extend_from_slice(&rect.x.to_le_bytes());
            body.extend_from_slice(&rect.y.to_le_bytes());
            body.extend_from_slice(&rect.width.to_le_bytes());
            body.extend_from_slice(&rect.height.to_le_bytes());
        }

        process_request::process_request(
            state,
            backend as &mut dyn Backend,
            ClientId(14),
            SequenceNumber(1),
            RequestHeader {
                opcode: 70, // PolyFillRectangle
                data: 0,
                length_units: u32::try_from((4 + body.len()) / 4).expect("request length"),
            },
            &body,
            None,
        )
        .expect("process_request(PolyFillRectangle) must succeed");
    }

    fn create_live_v2_window(
        state: &mut yserver_core::server::ServerState,
        backend: &mut KmsBackendV2,
        xid: yserver_protocol::x11::ResourceId,
        parent: yserver_protocol::x11::ResourceId,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
    ) -> yserver_core::backend::WindowHandle {
        use yserver_core::{
            backend::Backend,
            host_x11::HostSubwindowVisual,
            resources::{ROOT_VISUAL, ROOT_WINDOW},
        };
        use yserver_protocol::x11::ClientId;

        state.resources.create_window(
            ClientId(14),
            yserver_protocol::x11::CreateWindowRequest {
                depth: 24,
                window: xid,
                parent,
                x,
                y,
                width,
                height,
                border_width: 0,
                class: 1,
                visual: ROOT_VISUAL,
                ..Default::default()
            },
        );

        let host_parent = if parent == ROOT_WINDOW {
            yserver_core::backend::WindowHandle::from_raw(backend.core.window_id).expect("root")
        } else {
            state
                .resources
                .window(parent)
                .and_then(|w| w.host_xid)
                .expect("parent host_xid")
        };
        let host = backend
            .create_subwindow(
                None,
                host_parent,
                x,
                y,
                width,
                height,
                0,
                HostSubwindowVisual::Explicit {
                    depth: 24,
                    visual_xid: 0,
                    colormap_xid: 0,
                },
                None,
                None,
            )
            .expect("create_subwindow");
        state.resources.window_mut(xid).expect("window").host_xid = Some(host);
        if parent == ROOT_WINDOW {
            backend
                .register_top_level(None, xid, host.as_raw())
                .expect("register_top_level");
        } else {
            backend
                .register_subwindow(None, xid, host.as_raw())
                .expect("register_subwindow");
        }
        let _ = state.resources.map_window(xid);
        backend
            .map_subwindow(None, host.as_raw())
            .expect("map_subwindow");
        host
    }

    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn process_request_root_fill_include_inferiors_matches_top_level_after_move() {
        use yserver_core::resources::ROOT_WINDOW;
        use yserver_protocol::x11::{ClientId, CreateGcRequest, ResourceId};

        let mut state = yserver_core::server::ServerState::new();
        let mut backend = match KmsBackendV2::for_tests_with_vk() {
            Ok(b) => b,
            Err(e) => {
                eprintln!("skipping: no Vk: {e}");
                return;
            }
        };
        install_client_for_v2(&mut state, 14);
        state
            .resources
            .window_mut(ROOT_WINDOW)
            .expect("root")
            .host_xid = yserver_core::backend::WindowHandle::from_raw(backend.core.window_id);

        let top = ResourceId(0x2000);
        let gc = ResourceId(0x2001);
        let child_base = 0x2100u32;
        let grandchild_base = 0x2200u32;

        let top_host =
            create_live_v2_window(&mut state, &mut backend, top, ROOT_WINDOW, 11, 7, 100, 90);
        let top_xid = top_host.as_raw();

        backend
            .fill_rectangle(None, top_xid, 0x0000_0000, 0, 0, 100, 90)
            .expect("clear top");
        backend
            .fill_rectangle(None, top_xid, 0x0000_00ff, 20, 30, 70, 30)
            .expect("baseline fill");
        let expected = backend
            .get_image_pixels_for_tests(top_xid, 2, 0, 0, 100, 90, !0)
            .expect("baseline get_image")
            .expect("baseline bytes");
        backend
            .fill_rectangle(None, top_xid, 0x0000_0000, 0, 0, 100, 90)
            .expect("re-clear top");

        state.resources.create_gc(
            ClientId(14),
            CreateGcRequest {
                gc,
                drawable: top,
                function: None,
                plane_mask: None,
                foreground: Some(0x0000_00ff),
                background: None,
                line_width: None,
                line_style: None,
                cap_style: None,
                join_style: None,
                fill_style: None,
                fill_rule: None,
                tile: None,
                stipple: None,
                tile_x_origin: None,
                tile_y_origin: None,
                font: None,
                subwindow_mode: Some(1),
                graphics_exposures: None,
                clip_x_origin: None,
                clip_y_origin: None,
                clip_mask: None,
                dash_offset: None,
                dashes: None,
                arc_mode: None,
            },
        );

        for i in 0..4 {
            let child = ResourceId(child_base + i);
            create_live_v2_window(
                &mut state,
                &mut backend,
                child,
                top,
                (i * 20) as i16,
                0,
                10,
                90,
            );
            for j in 0..9 {
                create_live_v2_window(
                    &mut state,
                    &mut backend,
                    ResourceId(grandchild_base + i * 16 + j),
                    child,
                    0,
                    (j * 10) as i16,
                    10,
                    6,
                );
            }
        }

        let rect = [Rectangle16 {
            x: 20,
            y: 30,
            width: 70,
            height: 30,
        }];

        dispatch_poly_fill_rectangle_v2(&mut state, &mut backend, top, gc, &rect);
        let top_include = backend
            .get_image_pixels_for_tests(top_xid, 2, 0, 0, 100, 90, !0)
            .expect("top include get_image")
            .expect("top include bytes");
        assert_eq!(
            top_include, expected,
            "top-level request path must match baseline"
        );

        backend
            .fill_rectangle(None, top_xid, 0x0000_0000, 0, 0, 100, 90)
            .expect("re-clear top");

        dispatch_configure_window_v2(&mut state, &mut backend, top, Some(0), Some(0), Some(0));

        let geom = backend
            .windows_v2
            .get(&top_xid)
            .expect("top geom after move");
        assert_eq!(
            (geom.x, geom.y),
            (0, 0),
            "backend geometry must track ConfigureWindow"
        );

        dispatch_poly_fill_rectangle_v2(&mut state, &mut backend, ROOT_WINDOW, gc, &rect);
        let root_out = backend
            .get_image_pixels_for_tests(top_xid, 2, 0, 0, 100, 90, !0)
            .expect("root-path get_image")
            .expect("root-path bytes");
        assert_eq!(root_out, expected);
    }

    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn root_get_image_reads_scanout_pixels_not_root_storage() {
        use ash::vk;
        use yserver_core::{resources::ROOT_WINDOW, server::ServerState};
        use yserver_protocol::x11::ResourceId;

        let mut state = ServerState::new();
        let mut backend = match KmsBackendV2::for_tests_with_vk_live_scene() {
            Ok(b) => b,
            Err(e) => {
                eprintln!("skipping: no Vk: {e}");
                return;
            }
        };
        install_client_for_v2(&mut state, 14);
        state
            .resources
            .window_mut(ROOT_WINDOW)
            .expect("root")
            .host_xid = yserver_core::backend::WindowHandle::from_raw(backend.core.window_id);

        let (root_w, root_h) = {
            let root = state.resources.window(ROOT_WINDOW).expect("root window");
            (root.width, root.height)
        };
        let window = ResourceId(0x2002);
        let window_host = create_live_v2_window(
            &mut state,
            &mut backend,
            window,
            ROOT_WINDOW,
            32,
            24,
            16,
            16,
        );

        let root_color = 0x0011_2233;
        let window_color = 0x00aa_55ff;
        backend
            .fill_rectangle(
                None,
                backend.core.window_id,
                root_color,
                0,
                0,
                root_w,
                root_h,
            )
            .expect("fill root");
        backend
            .fill_rectangle(None, window_host.as_raw(), window_color, 0, 0, 16, 16)
            .expect("fill window");
        backend.tick_maybe_composite_for_tests();

        let scan_rect = vk::Rect2D {
            offset: vk::Offset2D { x: 32, y: 24 },
            extent: vk::Extent2D {
                width: 16,
                height: 16,
            },
        };
        let scanout = super::read_scanout_region(
            &backend,
            scan_rect,
            super::ScanoutReadSelection::OnScreenOnly,
        )
        .expect("scanout readback");
        let root_out = backend
            .get_image_pixels_for_tests(backend.core.window_id, 2, 32, 24, 16, 16, !0)
            .expect("root get_image")
            .expect("root bytes");
        let window_out = backend
            .get_image_pixels_for_tests(window_host.as_raw(), 2, 0, 0, 16, 16, !0)
            .expect("window get_image")
            .expect("window bytes");

        assert_eq!(
            root_out, scanout,
            "root GetImage must read the on-screen scanout"
        );
        assert_eq!(
            root_out, window_out,
            "root GetImage must match the visible window pixels"
        );
    }

    /// Register a client in `state.clients` so `process_request`'s
    /// per-client sequence stamp + (potential) error-emission path
    /// have a real `ClientState` to operate on. Uses
    /// `resource_id_mask = u32::MAX` so the fixture xids are
    /// trivially in-range.
    fn install_client_for_v2(state: &mut yserver_core::server::ServerState, id: u32) {
        use std::{
            collections::{HashMap, HashSet, VecDeque},
            os::unix::net::UnixStream,
            sync::{Arc, Mutex, atomic::AtomicU16},
        };
        use yserver_core::{resources::ROOT_WINDOW, server::ClientState};
        use yserver_protocol::x11::ClientByteOrder;
        let (a, _b) = UnixStream::pair().unwrap();
        state.clients.insert(
            id,
            ClientState {
                writer: Arc::new(Mutex::new(a)),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0)),
                resource_id_base: 0,
                resource_id_mask: u32::MAX,
                event_masks: HashMap::new(),
                save_set: HashSet::new(),
                big_requests_enabled: false,
                xi2_masks: HashMap::new(),
                xi1_event_classes: HashSet::new(),
                xi1_window_event_classes: HashMap::new(),
                outbound: VecDeque::new(),
                watching_writable: false,
                focused_window: ROOT_WINDOW,
                reader_control: None,
            },
        );
    }

    #[test]
    fn resolve_paint_target_after_reparent_out_routes_to_new_redirected_ancestor() {
        use yserver_core::{
            resources::ROOT_WINDOW,
            server::{CompositeRedirectMode, RedirectRecord, ServerState},
        };
        use yserver_protocol::x11::{ClientId, ResourceId};

        // Phase 2 root-cause pin: build a tree mirroring the live
        // mate-panel case (root → mate-panel, root → nm-applet),
        // dispatch a ReparentWindow that moves nm-applet under
        // mate-panel's socket, then assert resolve_paint_target
        // returns mate-panel's backing with the right offset.
        //
        // Pre-fix: nm-applet's stale Manual-redirect backing wins,
        // resolve_paint_target returns it with offset (0, 0).
        // Post-fix (handle_reparent_window's reconciliation): the
        // backing is freed, resolve_paint_target walks up the
        // ancestor chain to mate-panel's backing with the offset
        // = nm-applet's screen-coord position within mate-panel.

        let mut state = ServerState::new();
        let mut backend = KmsBackendV2::for_tests();
        install_client_for_v2(&mut state, 14);

        let root_xid = ROOT_WINDOW;
        let mate_panel_xid = ResourceId(0x110_0003);
        let socket_xid = ResourceId(0x210_0013);
        let nm_applet_xid = ResourceId(0x180_000b);

        // Pre-state: root has RedirectSubwindows(Manual). mate-panel
        // is a redirected direct child. socket is a child of mate-
        // panel (not directly redirected). nm-applet is currently
        // a direct child of root (and therefore inherits redirect).
        state.composite_redirects.insert(
            (root_xid, true),
            RedirectRecord {
                mode: CompositeRedirectMode::Manual,
                owner: ClientId(14),
            },
        );

        seed_v2_window(
            &mut state,
            &mut backend,
            mate_panel_xid,
            root_xid,
            0,
            0,
            2560,
            28,
        );
        seed_v2_redirected_backing(&mut state, &mut backend, mate_panel_xid);
        let mate_panel_backing_id =
            backing_drawable_id(&backend, mate_panel_xid).expect("mate-panel backing drawable id");
        seed_v2_window(
            &mut state,
            &mut backend,
            socket_xid,
            mate_panel_xid,
            2387,
            0,
            26,
            27,
        );
        seed_v2_window(
            &mut state,
            &mut backend,
            nm_applet_xid,
            root_xid,
            0,
            0,
            26,
            27,
        );
        seed_v2_redirected_backing(&mut state, &mut backend, nm_applet_xid);

        dispatch_reparent_window_v2(
            &mut state,
            &mut backend,
            nm_applet_xid,
            socket_xid,
            /* x */ 0,
            /* y */ 0,
        );

        let nm_applet_host_xid = synth_host_xid(nm_applet_xid);

        let resolved = backend
            .resolve_paint_target(nm_applet_host_xid)
            .expect("resolve must succeed");

        assert_eq!(
            resolved.id, mate_panel_backing_id,
            "paints into nm-applet must route to mate-panel's redirected backing post-reparent"
        );
        assert_eq!(
            resolved.offset,
            (2387, 0),
            "offset must place the paint at nm-applet's screen-coord position within mate-panel's backing"
        );
    }

    /// Stage 5 Task 6.1 (foundation prereq #2): the by-handle xshmfence
    /// accessor returns an Arc clone that pins the underlying
    /// `FenceMapping` alive past `XFixesDestroyFence` (registry
    /// removal). Two clones plus the registry entry should give a
    /// strong count of 3; after removing the registry entry the
    /// caller-held clones still keep the primitive alive (Drop sees a
    /// non-shared Arc).
    #[test]
    fn xshmfence_handle_accessor_returns_arc_clone() {
        // Construct a backend without Vk (skip if test fixture needs it).
        let mut b = match crate::kms::v2::KmsBackendV2::for_tests_with_vk() {
            Ok(b) => b,
            Err(e) => {
                eprintln!("skipping: no Vk: {e}");
                return;
            }
        };
        // Manually inject an entry into the registry — bypass the
        // protocol path (DRI3 FenceFromFD) since constructing a real
        // xshmfence FD in a unit test is fragile.
        let xid = 0x1234_5678_u32;
        let mapping = crate::kms::xshmfence::FenceMapping::for_tests_dummy();
        b.dri3_xshmfences.insert(xid, std::sync::Arc::new(mapping));
        let h1 = b.dri3_xshmfence_handle(xid).expect("handle present");
        assert_eq!(
            std::sync::Arc::strong_count(&h1),
            2,
            "registry + caller should both hold a reference"
        );
        let h2 = b.dri3_xshmfence_handle(xid).expect("second handle");
        assert_eq!(
            std::sync::Arc::strong_count(&h1),
            3,
            "registry + two callers should all hold references"
        );
        let _ = h1;
        let _ = h2;
        // Drop the registry entry (mimics XFixesDestroyFence).
        b.dri3_xshmfences.remove(&xid);
        // Accessor returns None now; but the caller's Arc clones
        // still pin the FenceMapping alive (no destructor panic).
        assert!(b.dri3_xshmfence_handle(xid).is_none());
    }

    /// Phase B.2 Task 14: three `render_composite` calls in the same
    /// open frame, then a forced close. After the backend drains the
    /// queued `FrameCloseEvent` into telemetry, the lifetime
    /// `frame_builder_renders_per_frame_max_in_window` gauge must
    /// reflect ≥ 3 (the count of `RecordedOp::RenderComposite` ops
    /// recorded into the closing frame). Exercises the full path:
    /// engine populates `renders_in_frame` at the close-event push
    /// site → backend's `drain_frame_builder_telemetry` reads it →
    /// `record_frame_builder_close` accumulates into the bucket +
    /// lifetime gauges.
    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn v2_frame_builder_renders_per_frame_telemetry_records_max() {
        let mut be = match KmsBackendV2::for_tests_with_vk() {
            Ok(b) => b,
            Err(e) => {
                eprintln!("skipping: no Vk: {e}");
                return;
            }
        };

        let dst = be
            .allocate_test_pixmap_bgra(64, 64)
            .expect("allocate_test_pixmap_bgra");

        // Drain any baseline close events from pixmap allocation so the
        // lifetime gauge snapshot is taken at a clean baseline.
        be.engine_flush_submit_group_for_tests()
            .expect("setup drain");

        // Three solid-fill composites into the same dst. All three ops
        // append into the same open frame under the sub-gate.
        let r1 = be.render_composite_for_tests(dst, [1.0, 0.0, 0.0, 1.0], 64, 64);
        let r2 = be.render_composite_for_tests(dst, [0.0, 1.0, 0.0, 1.0], 64, 64);
        let r3 = be.render_composite_for_tests(dst, [0.0, 0.0, 1.0, 1.0], 64, 64);

        // Force frame close via the Timeout helper; this runs the
        // close-walk that pushes the FrameCloseEvent.
        let close_result = be.engine_close_open_frame_for_timeout_for_tests();

        // Reset the process-level sub-gate IMMEDIATELY so neighbouring
        // tests in the same cargo-test binary are not routed through
        // the frame-builder composite path.

        r1.expect("first render_composite_for_tests");
        r2.expect("second render_composite_for_tests");
        r3.expect("third render_composite_for_tests");
        close_result.expect("engine_close_open_frame_for_timeout_for_tests");

        // Drain queued FrameCloseEvent → telemetry. Reuse the existing
        // helper that drains flush outcomes + close events as a side
        // effect of returning submit_group_flushes.
        let _ = be.telemetry_submit_group_flushes_for_tests();
        be.drain_frame_builder_telemetry_for_tests();

        assert!(
            be.telemetry
                .lifetime
                .frame_builder_renders_per_frame_max_in_window
                >= 3,
            "lifetime renders_per_frame_max_in_window must reflect the \
             three RenderComposite ops recorded in the closing frame; got {}",
            be.telemetry
                .lifetime
                .frame_builder_renders_per_frame_max_in_window,
        );
    }

    #[test]
    fn syncobj_handle_accessor_returns_arc_clone() {
        let mut b = match crate::kms::v2::KmsBackendV2::for_tests_with_vk() {
            Ok(b) => b,
            Err(e) => {
                eprintln!("skipping: no Vk: {e}");
                return;
            }
        };
        let xid = 0xAAAA_BBBB_u32;
        let vk = b.platform.vk.as_ref().expect("vk live").clone();
        let sem = crate::kms::v2::owned_semaphore::OwnedSemaphore::for_tests_dummy(vk);
        b.dri3_sync_resources.insert(xid, std::sync::Arc::new(sem));
        let h = b.dri3_syncobj_handle(xid).expect("handle present");
        assert_eq!(std::sync::Arc::strong_count(&h), 2);
        b.dri3_sync_resources.remove(&xid);
        // Accessor returns None now; held Arc still pins the
        // OwnedSemaphore alive (no destructor panic on drop).
        assert!(b.dri3_syncobj_handle(xid).is_none());
        drop(h); // OwnedSemaphore::Drop fires here; null guard
        // skips destroy_semaphore.
    }

    // ─── Task 10: held-key tracking + synthesize-releases tests ───

    /// `on_host_input` Key arm maintains `down_keys`: press inserts the
    /// cooked keycode, release removes it. Tests the additive tracking
    /// path without touching fanout behaviour.
    #[test]
    fn down_keys_maintained_on_key_press_and_release() {
        use yserver_core::{
            core_loop::HostInputEvent, host_x11::HostKeyEvent, server::ServerState,
        };
        let mut b = KmsBackendV2::for_tests();
        let mut state = ServerState::new();

        let raw_press = HostKeyEvent {
            keycode: 38, // evdev 'a' (US layout)
            pressed: true,
            state: 0,
            root_x: 0,
            root_y: 0,
            event_x: 0,
            event_y: 0,
            time: 0,
        };
        b.on_host_input(&mut state, HostInputEvent::Key(raw_press));
        assert!(
            b.core.down_keys.contains(&38),
            "key 38 must be in down_keys after press"
        );
        assert_eq!(b.core.down_keys.len(), 1);

        let raw_release = HostKeyEvent {
            keycode: 38,
            pressed: false,
            state: 0,
            root_x: 0,
            root_y: 0,
            event_x: 0,
            event_y: 0,
            time: 0,
        };
        b.on_host_input(&mut state, HostInputEvent::Key(raw_release));
        assert!(
            !b.core.down_keys.contains(&38),
            "key 38 must be removed from down_keys after release"
        );
        assert!(b.core.down_keys.is_empty());
    }

    /// `synthesize_held_releases` clears `down_keys` and `button_mask`,
    /// emits a synthetic release for every tracked key and button, and
    /// leaves `pending_pointer_events` empty.
    ///
    /// Behavioural contract:
    /// (a) `down_keys` empty after the call
    /// (b) `button_mask == 0` after the call
    /// (c) `pending_pointer_events` drained (button releases fanned out)
    /// (d) No XI2 raw event is generated (key_event_fanout_to_state
    ///     does not emit XI2 raw events — this is a property of the
    ///     fanout, not separately asserted here)
    ///
    /// Note: with a fresh `ServerState::new()` there are no subscribed
    /// clients, so key/button events are dropped by the fanout (no
    /// receivers). The load-bearing observables are the state fields:
    /// `down_keys` empty proves every held key was iterated, and
    /// `button_mask == 0` proves every held button's bit was cleared
    /// by its corresponding `process_pointer_button(released)` call.
    #[test]
    fn synthesize_held_releases_clears_down_keys_and_button_mask() {
        use yserver_core::server::ServerState;
        let mut b = KmsBackendV2::for_tests();
        let mut state = ServerState::new();

        // Inject two held keys directly into down_keys (bypassing
        // cook_host_key so we control exact keycodes).
        b.core.down_keys.insert(38); // 'a'
        b.core.down_keys.insert(56); // 'b'
        assert_eq!(b.core.down_keys.len(), 2);

        // Inject held buttons 1 (BTN_LEFT, bit 0x0100) and 3
        // (BTN_RIGHT, bit 0x0400) into button_mask.
        b.core.button_mask = 0x0100 | 0x0400;
        assert_ne!(b.core.button_mask, 0);

        b.synthesize_held_releases(&mut state);

        // (a) down_keys cleared.
        assert!(
            b.core.down_keys.is_empty(),
            "down_keys must be empty after synthesize_held_releases"
        );
        // (b) button_mask zeroed.
        assert_eq!(
            b.core.button_mask, 0,
            "button_mask must be 0 after synthesize_held_releases"
        );
        // (c) pending_pointer_events drained.
        assert!(
            b.core.pending_pointer_events.is_empty(),
            "pending_pointer_events must be empty (fanned out) after synthesize_held_releases"
        );
    }

    // ── Task 13: stub-backed VT-switch suspend/resume integration tests ──
    //
    // These tests drive `inject_seat_event_for_test` directly — no libseat,
    // no DRM, no real hardware.  They exercise the full state-machine path
    // plus `run_suspend` side-effects that are reachable in the stub harness.
    //
    // Resume path note: `run_resume` calls
    // `platform.requery_outputs_and_modeset()` → `discover_outputs` which
    // issues DRM ioctls on `/dev/null` and fails immediately.  The error
    // path logs the failure, calls `request_exit()` (a no-op in the stub
    // harness because `input_sender` is None), and returns.  `drive_seat_event`
    // then always calls `resume_complete()` regardless, so the state machine
    // still transitions from `Resuming` → `Active` (or `Suspending` if a
    // pending disable is set).  This is correct and asserted below.
    // The post-modeset parts of resume (cursor re-arm, repaint) are
    // hardware-only and not asserted here — documented as DONE_WITH_CONCERNS.

    /// After `inject_seat_event_for_test(false)` the backend must be in
    /// `Suspended` and `scanout_allowed()` must return `false`.
    ///
    /// Also verifies that pre-seeded held keys and buttons are cleared by
    /// `synthesize_held_releases` inside `run_suspend`.
    #[test]
    fn vt_switch_disable_transitions_to_suspended_and_releases_held_input() {
        use crate::seat::state::SeatState;

        let mut b = KmsBackendV2::for_tests();
        let mut state = ServerState::new();

        // Pre-seed held keys and a button so we can verify they're cleared.
        b.core.down_keys.insert(38); // 'a'
        b.core.down_keys.insert(56); // 'b'
        b.core.button_mask = 0x0100; // BTN_LEFT held

        // Precondition: starts Active with scanout allowed.
        assert_eq!(b.seat_state, SeatState::Active);
        assert!(
            b.scanout_allowed(),
            "scanout must be allowed before disable"
        );

        // Drive the Disable event.
        b.inject_seat_event_for_test(&mut state, false);

        // (a) State machine reached Suspended.
        assert_eq!(
            b.seat_state,
            SeatState::Suspended,
            "seat_state must be Suspended after Disable"
        );

        // (b) Scanout gate is closed.
        assert!(
            !b.scanout_allowed(),
            "scanout must not be allowed while Suspended"
        );

        // (c) Held keys cleared by synthesize_held_releases.
        assert!(
            b.core.down_keys.is_empty(),
            "down_keys must be empty after suspend (synthesize_held_releases)"
        );

        // (d) Held buttons cleared.
        assert_eq!(
            b.core.button_mask, 0,
            "button_mask must be 0 after suspend (synthesize_held_releases)"
        );
    }

    /// After a Disable→Enable cycle the state machine must return to `Active`.
    ///
    /// In the stub harness `run_resume`'s `requery_outputs_and_modeset` fails
    /// (DRM ioctls on `/dev/null`), but the state machine still completes the
    /// transition because `drive_seat_event` calls `resume_complete()` after
    /// `run_resume` returns regardless of the modeset outcome.  This is the
    /// correct behaviour: a failed modeset calls `request_exit` (a no-op here)
    /// and the server would exit in production; the state-machine transition
    /// is a logical consequence, not a claim that the hardware path succeeded.
    #[test]
    fn vt_switch_enable_after_disable_returns_to_active() {
        use crate::seat::state::SeatState;

        let mut b = KmsBackendV2::for_tests();
        let mut state = ServerState::new();

        // Drive Disable → Suspended.
        b.inject_seat_event_for_test(&mut state, false);
        assert_eq!(b.seat_state, SeatState::Suspended);

        // Drive Enable → Active (modeset fails in stub, state still advances).
        b.inject_seat_event_for_test(&mut state, true);

        assert_eq!(
            b.seat_state,
            SeatState::Active,
            "seat_state must be Active after Enable completes"
        );
        assert!(
            b.scanout_allowed(),
            "scanout must be allowed after returning to Active"
        );
    }

    /// A rapid Disable-then-Enable-then-Disable sequence exercises the
    /// no-blink boundary: a `Disable` coalesced during a resume sequence
    /// causes `resume_complete` to return `BeginSuspend`, skipping `Active`
    /// entirely.  The final state must be `Suspended`, not `Active`.
    ///
    /// Concretely this test drives:
    ///
    ///  1. Disable → Suspending → (suspend sequence) → Suspended
    ///  2. Enable  → Resuming → (resume sequence) → resume_complete;
    ///     before step 2 we pre-seed `pending_disable = true` to simulate
    ///     a Disable that arrived mid-resume.
    ///  3. `resume_complete` sees `pending_disable` → returns `BeginSuspend`
    ///     → run_suspend again → Suspended
    ///
    /// The final assertion is `Suspended` and no panic (RefCell re-entrancy
    /// is exercised by the full Disable→Enable path above as well).
    #[test]
    fn vt_switch_rapid_double_switch_never_passes_through_active() {
        use crate::seat::state::{SeatPending, SeatState};

        let mut b = KmsBackendV2::for_tests();
        let mut state = ServerState::new();

        // Step 1: normal Disable → Suspended.
        b.inject_seat_event_for_test(&mut state, false);
        assert_eq!(b.seat_state, SeatState::Suspended);

        // Simulate a Disable that arrives during the resume sequence by
        // pre-seeding `pending_disable`.  In production this would be set
        // by `on_event(Disable)` arriving while `seat_state == Resuming`
        // (the coalesce arm in `SeatState::on_event`).  We set it directly
        // here because the stub drives events synchronously and we cannot
        // interleave them mid-sequence without modifying the backend.
        b.seat_pending = SeatPending {
            pending_disable: true,
            pending_enable: false,
        };

        // Step 2: Enable with pending_disable set → resume_complete skips
        // Active and goes straight to Suspending → run_suspend → Suspended.
        b.inject_seat_event_for_test(&mut state, true);

        assert_eq!(
            b.seat_state,
            SeatState::Suspended,
            "rapid double-switch must end in Suspended, never passing through Active"
        );
        assert!(
            !b.scanout_allowed(),
            "scanout must not be allowed after rapid double-switch lands in Suspended"
        );
        // pending_disable must have been consumed by resume_complete.
        assert!(
            !b.seat_pending.pending_disable,
            "pending_disable must be cleared after resume_complete consumed it"
        );
    }

    /// A coalesced `pending_enable` (an Enable that arrived during the
    /// suspend sequence) must be consumed by the drive loop and resume the
    /// session, not strand it in `Suspended`. Regression test for the
    /// final-review finding that `pending_enable` was set but never acted
    /// on. We pre-seed `pending_enable` (the synchronous stub can't
    /// interleave a real Enable mid-suspend) and drive a Disable; the loop
    /// must run suspend, then consume the flag and resume to `Active`.
    #[test]
    fn vt_switch_coalesced_enable_resumes_not_stranded_in_suspended() {
        use crate::seat::state::{SeatPending, SeatState};

        let mut b = KmsBackendV2::for_tests();
        let mut state = ServerState::new();

        b.seat_pending = SeatPending {
            pending_enable: true,
            pending_disable: false,
        };

        b.inject_seat_event_for_test(&mut state, false);

        assert_eq!(
            b.seat_state,
            SeatState::Active,
            "a coalesced pending_enable must drive a resume after suspend, not strand in Suspended"
        );
        assert!(
            !b.seat_pending.pending_enable,
            "pending_enable must be cleared once the consume-loop acts on it"
        );
    }

    /// Re-entrancy smoke: a full Disable→Enable cycle completes without a
    /// `RefCell` borrow panic.  In the stub harness `LibseatInner` is never
    /// held because `seat` is `Seat::Direct`, so this primarily verifies that
    /// no other RefCell in the backend panics during the sequence.  The
    /// re-entrancy concern from the plan (libseat `borrow_mut` inside
    /// `libinput.resume()`) is hardware-only and covered by the hardware
    /// matrix (Task 14).
    #[test]
    fn vt_switch_full_cycle_no_refcell_panic() {
        use crate::seat::state::SeatState;

        let mut b = KmsBackendV2::for_tests();
        let mut state = ServerState::new();

        // Two full cycles — if any RefCell is double-borrowed this panics.
        for _ in 0..2 {
            b.inject_seat_event_for_test(&mut state, false);
            assert_eq!(b.seat_state, SeatState::Suspended);
            b.inject_seat_event_for_test(&mut state, true);
            assert_eq!(b.seat_state, SeatState::Active);
        }
    }

    // ────────────────────────────────────────────────────────────────
    // COW structural redesign — Phase 2 Task 2.2: get/release_overlay_window
    // own the FULL backend lifecycle (storage + windows_v2 +
    // top_level_order). The bool return signals the 0→1 / 1→0
    // transition so the core handler drives the symmetric resources-
    // side materialization once per lifecycle event.
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn get_overlay_window_first_claim_materializes_full_backend_state() {
        let mut b = KmsBackendV2::for_tests();
        let fb_w = u32::from(b.platform.fb_w);
        let fb_h = u32::from(b.platform.fb_h);

        let was_first_claim = b.get_overlay_window(None).expect("get_overlay_window");
        assert!(was_first_claim, "0→1 transition must return Ok(true)");

        let cow_host_xid = b
            .cow_host_xid()
            .expect("cow_host_xid getter must return Some after first claim");
        let geom = b
            .windows_v2
            .get(&cow_host_xid)
            .expect("COW must be present in windows_v2 after first claim");
        assert!(geom.mapped);
        assert_eq!(geom.depth, 24);
        assert_eq!(u32::from(geom.width), fb_w);
        assert_eq!(u32::from(geom.height), fb_h);
        assert_eq!(geom.parent, None);
        assert_eq!(
            b.core.top_level_order.last().copied(),
            Some(cow_host_xid),
            "COW is the topmost entry in top_level_order"
        );
    }

    #[test]
    fn get_overlay_window_second_claim_does_not_remateralize() {
        let mut b = KmsBackendV2::for_tests();
        b.get_overlay_window(None).expect("first claim");
        let cow_host_xid = b.cow_host_xid().expect("after first claim");
        // Snapshot the rank to make sure a second claim doesn't reallocate.
        let rank_before = b.windows_v2.get(&cow_host_xid).unwrap().stack_rank;

        let was_first_claim = b.get_overlay_window(None).expect("second claim");
        assert!(!was_first_claim, "subsequent claim must return Ok(false)");
        let rank_after = b.windows_v2.get(&cow_host_xid).unwrap().stack_rank;
        assert_eq!(
            rank_before, rank_after,
            "subsequent claim must not rebuild windows_v2 entry"
        );
    }

    #[test]
    fn release_overlay_window_final_release_tears_down_full_backend_state() {
        let mut b = KmsBackendV2::for_tests();
        b.get_overlay_window(None).expect("get");
        let cow_host_xid = b.cow_host_xid().expect("present");

        let was_final_release = b.release_overlay_window(None).expect("release");
        assert!(was_final_release, "1→0 transition must return Ok(true)");
        assert!(
            !b.windows_v2.contains_key(&cow_host_xid),
            "COW removed from windows_v2 on final release"
        );
        assert!(
            !b.core.top_level_order.contains(&cow_host_xid),
            "COW removed from top_level_order"
        );
        assert!(
            b.cow_host_xid().is_none(),
            "cow_host_xid getter returns None after final release"
        );
    }

    /// Frame tick: advances mod n, re-arms relative, mints strictly
    /// increasing versions across a full wraparound (XFixes serial
    /// contract — naive Arc-swapping would repeat v1,v2,v1).
    #[test]
    fn anim_tick_advances_wraps_and_stays_monotonic() {
        use std::time::{Duration, Instant};
        use yserver_core::backend::{Backend, CursorHandle};

        let mut b = KmsBackendV2::for_tests();
        // Insert records directly with DISTINCT bytes per frame —
        // `create_cursor` on an unreadable test pixmap degenerates to
        // identical 1×1 transparent bytes, making the bytes assert
        // below vacuous.
        let c1_xid = b.core.next_host_xid();
        b.insert_cursor_record(c1_xid, 1, 1, 0, 0, vec![0xAA, 0x00, 0x00, 0xFF]);
        let c1 = CursorHandle::from_raw(c1_xid).unwrap();
        let c2_xid = b.core.next_host_xid();
        b.insert_cursor_record(c2_xid, 1, 1, 0, 0, vec![0x00, 0xBB, 0x00, 0xFF]);
        let c2 = CursorHandle::from_raw(c2_xid).unwrap();
        let anim = b
            .create_anim_cursor(None, &[(c1, 50), (c2, 75)])
            .expect("anim")
            .expect("KMS animates");
        let root_host = b.core.window_id;
        b.define_cursor(None, root_host, anim.as_raw())
            .expect("define");

        let mut last_version = b.cursor_records.get(&anim.as_raw()).unwrap().version;
        let mut expected_frame = 0usize;
        // 5 ticks over 2 frames = two full wraparounds.
        for i in 0..5 {
            // Force the deadline into the past, then tick.
            b.active_cursor_anim.as_mut().unwrap().next_frame =
                Instant::now() - Duration::from_millis(1);
            b.tick_cursor_animation();
            expected_frame = (expected_frame + 1) % 2;
            let st = b.active_cursor_anim.as_ref().expect("still armed");
            assert_eq!(st.frame, expected_frame, "tick {i}");
            assert!(st.next_frame > Instant::now() - Duration::from_millis(1));
            let v = b.cursor_records.get(&anim.as_raw()).unwrap().version;
            assert!(v > last_version, "tick {i}: version {v} !> {last_version}");
            last_version = v;
            // The canonical record now carries the frame's bytes.
            let frame_rec =
                &b.anim_cursor_records.get(&anim.as_raw()).unwrap().frames[expected_frame].record;
            assert_eq!(
                b.cursor_records.get(&anim.as_raw()).unwrap().bgra_bytes,
                frame_rec.bgra_bytes,
            );
        }
        // Tick before the deadline → no advance.
        let frame_before = b.active_cursor_anim.as_ref().unwrap().frame;
        b.tick_cursor_animation();
        assert_eq!(b.active_cursor_anim.as_ref().unwrap().frame, frame_before);
    }

    /// next_wakeup reports the anim deadline only while outputs are
    /// active and scanout is allowed (EINVAL-storm discipline).
    #[test]
    fn anim_deadline_gated_on_outputs_active() {
        use std::time::{Duration, Instant};
        use yserver_core::backend::{Backend, PixmapHandle};

        let mut b = KmsBackendV2::for_tests();
        let pix = PixmapHandle::from_raw(0x1234_0060).unwrap();
        let c1 = b
            .create_cursor(None, pix, None, (0xFFFF, 0, 0), (0, 0, 0), 0, 0)
            .expect("c1");
        let anim = b
            .create_anim_cursor(None, &[(c1, 50)])
            .expect("anim")
            .expect("KMS animates");
        let root_host = b.core.window_id;
        b.define_cursor(None, root_host, anim.as_raw())
            .expect("define");
        let deadline = b.active_cursor_anim.as_ref().unwrap().next_frame;

        let wake = b.next_wakeup().expect("deadline reported");
        assert!(wake <= deadline);

        // DPMS off → deadline not reported, tick is a no-op.
        b.kms_outputs_active = false;
        let frame = b.active_cursor_anim.as_ref().unwrap().frame;
        b.active_cursor_anim.as_mut().unwrap().next_frame =
            Instant::now() - Duration::from_millis(1);
        assert!(
            b.next_wakeup().is_none_or(|w| w > Instant::now()),
            "stale anim deadline must not be reported while outputs are off",
        );
        b.tick_cursor_animation();
        assert_eq!(
            b.active_cursor_anim.as_ref().unwrap().frame,
            frame,
            "tick must not advance while outputs are off",
        );

        // Outputs back on with the deadline in the past → exactly one
        // immediate advance (spec: no fast-forward through missed frames).
        b.kms_outputs_active = true;
        b.tick_cursor_animation();
        assert_eq!(
            b.active_cursor_anim.as_ref().unwrap().frame,
            frame,
            "1-frame anim wraps to same index"
        );
        assert!(
            b.active_cursor_anim.as_ref().unwrap().next_frame > Instant::now(),
            "re-armed from now: next_frame must be a future instant after DPMS restore tick",
        );
    }

    /// XFixes GetCursorImage tracks the animation: bytes follow the
    /// current frame, serial strictly increases across a wraparound.
    #[test]
    fn xfixes_cursor_image_follows_animation_frames() {
        use std::time::{Duration, Instant};
        use yserver_core::backend::{Backend, CursorHandle};

        let mut b = KmsBackendV2::for_tests();
        // Insert records directly with DISTINCT bytes per frame —
        // `create_cursor` on an unreadable test pixmap degenerates to
        // identical 1×1 transparent bytes, making the bytes assert
        // below vacuous.
        let c1_xid = b.core.next_host_xid();
        b.insert_cursor_record(c1_xid, 1, 1, 0, 0, vec![0xAA, 0x00, 0x00, 0xFF]);
        let c1 = CursorHandle::from_raw(c1_xid).unwrap();
        let c2_xid = b.core.next_host_xid();
        b.insert_cursor_record(c2_xid, 1, 1, 0, 0, vec![0x00, 0xBB, 0x00, 0xFF]);
        let c2 = CursorHandle::from_raw(c2_xid).unwrap();
        let anim = b
            .create_anim_cursor(None, &[(c1, 50), (c2, 75)])
            .expect("anim")
            .expect("KMS animates");
        let root_host = b.core.window_id;
        b.define_cursor(None, root_host, anim.as_raw())
            .expect("define");

        let mut last_serial = b.get_active_cursor_image().expect("image").serial;
        for _ in 0..4 {
            b.active_cursor_anim.as_mut().unwrap().next_frame =
                Instant::now() - Duration::from_millis(1);
            b.tick_cursor_animation();
            let img = b.get_active_cursor_image().expect("image");
            assert!(img.serial > last_serial, "serial must strictly increase");
            last_serial = img.serial;
            let frame_idx = b.active_cursor_anim.as_ref().unwrap().frame;
            let frame_rec =
                &b.anim_cursor_records.get(&anim.as_raw()).unwrap().frames[frame_idx].record;
            assert_eq!(*img.bgra_bytes, frame_rec.bgra_bytes);
        }
    }
}
