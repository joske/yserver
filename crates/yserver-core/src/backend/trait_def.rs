//! `Backend` trait — the surface `process_request` and the core loop
//! call on the host backend. Exists primarily as a seam for testing
//! (`RecordingBackend` lives next door, gated `#[cfg(test)]`) and so
//! that the KMS backend can sit alongside the host-X11 backend without
//! touching every call site.
//!
//! Method signatures mirror the existing `HostX11Backend::*` methods
//! 1:1 — no `Param` structs, no parameter renaming. Several methods
//! still take raw `u32` host xids rather than handle newtypes — call
//! sites pass `u32` from the `ResourceTable`'s `host_xid` field, and
//! rewrapping/unwrapping at every call boundary is noise.

use std::{any::Any, io};

use yserver_protocol::x11::{AtomId, ClipRectangles, FontMetrics, xfixes};

use crate::{
    backend::{
        AnyHandle, ClipState, CursorHandle, DrawState, FillState, FontHandle, GlyphSetHandle,
        OriginContext, PictureHandle, PixmapHandle, WindowHandle,
    },
    core_loop::HostInputEvent,
    host_x11::{HostEvent, HostSubwindowConfig, HostSubwindowVisual, HostXidMap, PointerPosition},
    server::ServerState,
};

use yserver_protocol::x11::ResourceId;

/// Categorises the raw fds a backend wants the core's mio poller to
/// watch on its behalf (returned by `Backend::poll_fds`).
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum BackendFdKind {
    /// libinput's epoll fd. Readiness is dispatched to the libinput
    /// thread (KMS) — but the fd inventory still flows through the
    /// trait so the core's poller can register it uniformly.
    Libinput,
    /// DRM device fd; readiness drives `on_page_flip_ready`.
    Drm,
    /// Host X11 connection fd (ynest only); readiness drives
    /// `Backend::drain_host_socket` on the core thread.
    HostX11,
    /// Stage 5 Task 6.1: backend-internal epoll FD aggregating
    /// per-batch sync_file FDs for deferred PRESENT completion +
    /// a fallback wakeup_eventfd. Readiness drives
    /// `Backend::drain_completed_present_events`. Spec
    /// `2026-05-23-deferred-present-completion-design.md`.
    PresentCompletion,
    /// libseat connection fd (KMS + libseat mode only). Readiness
    /// drives `Backend::on_seat_ready` → `seat.dispatch()`.
    Seat,
}

/// Outcome of a single `Backend::drain_host_socket` pass. Re-exported
/// from `host_x11` so non-host-X11 backends can spell the type
/// without depending on the host module.
pub use crate::host_x11::HostSocketStatus;

/// Stage 5 unblock for protocol-audit item #14 — snapshot of the
/// active on-screen cursor returned by `Backend::get_active_cursor_image`.
/// `bgra_bytes` is straight-alpha BGRA8 (the canonical
/// `CursorRecord` representation); the XFIXES `GetCursorImage`
/// encoder premultiplies + byte-swaps at the wire boundary.
#[derive(Debug, Clone)]
pub struct ActiveCursorImage {
    pub width: u16,
    pub height: u16,
    pub hot_x: u16,
    pub hot_y: u16,
    pub x: i16,
    pub y: i16,
    /// Monotonically-increasing serial — XFIXES clients compare
    /// this against the previous reply's serial to detect cursor
    /// changes. Backed by `Arc<CursorRecord>.version` in v2.
    pub serial: u32,
    pub bgra_bytes: std::sync::Arc<Vec<u8>>,
}

/// Present capability surface. Phase 4.2 design §4. Per-window
/// because Present's `QueryCapabilities` is per-window in the wire
/// protocol; in single-output single-GPU configurations the same
/// values are returned for every window.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PresentCaps {
    /// Both KMS plane `IN_FENCE_FD` property *and* CRTC `OUT_FENCE_PTR`
    /// are available; explicit-fence flip handshake works. When
    /// false, the path selector short-circuits to `Copy` regardless
    /// of pixmap/window match per design §3.3.1.
    pub flip_path: bool,
    /// `DRM_MODE_ATOMIC_NONBLOCK` accepted by the kernel — the
    /// `PresentOptionAsyncMayTear` bit is honourable. When false,
    /// the bit is **silently cleared** on incoming requests (per
    /// design §4 row "Kernel rejects DRM_MODE_ATOMIC_NONBLOCK"),
    /// not folded into `PresentOptionAsync`.
    pub async_may_tear: bool,
    /// `Dri3Caps::syncobj` mirror — Present syncobj cap requires
    /// DRI3 syncobj support to be useful, so we don't advertise it
    /// without DRI3-side timeline plumbing.
    pub syncobj: bool,
}

impl PresentCaps {
    /// Encode as a `u32` for the `QueryCapabilities` reply.
    /// `presentproto` bit assignments:
    ///  - `Async` (0x1)        — Phase 4.2 advertises if `flip_path`.
    ///  - `Fence` (0x2)        — XSync `Fence` always supported once
    ///    DRI3 fence_fd cap is true; Phase 4.2 always advertises.
    ///  - `UST` (0x4)          — vblank UST timestamps; Phase 4.1's
    ///    pageflip path already produces these.
    ///  - `AsyncMayTear` (0x8) — `async_may_tear`.
    ///  - `Syncobj` (0x10)     — `syncobj`.
    #[must_use]
    pub fn encode(self) -> u32 {
        const ASYNC: u32 = 0x1;
        const FENCE: u32 = 0x2;
        const UST: u32 = 0x4;
        const ASYNC_MAY_TEAR: u32 = 0x8;
        const SYNCOBJ: u32 = 0x10;
        let mut out = FENCE | UST;
        if self.flip_path {
            out |= ASYNC;
        }
        if self.async_may_tear {
            out |= ASYNC_MAY_TEAR;
        }
        if self.syncobj {
            out |= SYNCOBJ;
        }
        out
    }
}

/// Stage 5 Task 6.1: payload of a deferred PRESENT completion
/// returned by [`Backend::drain_completed_present_events`]. Carries
/// everything the main loop needs to fan out `IdleNotify` +
/// `CompleteNotify { mode: Copy }` events plus trigger the X11
/// resource-id-keyed wake objects (xshmfence / DRM syncobj). The
/// backend has already signalled the underlying primitive via the
/// `Arc`-pinned handle before returning this struct.
#[derive(Debug, Clone)]
pub struct CompletedPresentEvent {
    pub client_id: yserver_protocol::x11::ClientId,
    pub serial: u32,
    pub host_xid: u32,
    pub dst_host_xid: u32,
    pub options: u32,
    pub wake: PresentWake,
}

/// Per-PRESENT-path wake target. Surfaces the original
/// PRESENT::Pixmap (xshmfence-driven) vs PRESENT::PixmapSynced
/// (DRM syncobj timeline) distinction back to the loop. The xids
/// in each variant are for X11-protocol bookkeeping
/// (`state.sync_fences[xid].triggered = true`) only — the actual
/// signal call against the underlying primitive happens inside the
/// backend at drain time.
#[derive(Debug, Clone, Copy)]
pub enum PresentWake {
    Pixmap {
        idle_fence_xid: u32,
    },
    PixmapSynced {
        release_syncobj: u32,
        release_value: u64,
    },
}

#[cfg(test)]
mod present_caps_tests {
    use super::PresentCaps;

    #[test]
    fn default_advertises_fence_and_ust() {
        // Default = all-false: still advertises Fence + UST per
        // Phase 4.2 (Fence is from XSync, UST from existing pageflip
        // path), but not Async / AsyncMayTear / Syncobj.
        let bits = PresentCaps::default().encode();
        assert_eq!(bits & 0x1, 0, "Async unset without flip_path");
        assert_eq!(bits & 0x2, 0x2, "Fence advertised");
        assert_eq!(bits & 0x4, 0x4, "UST advertised");
        assert_eq!(bits & 0x8, 0, "AsyncMayTear unset");
        assert_eq!(bits & 0x10, 0, "Syncobj unset");
    }

    #[test]
    fn full_caps_set_all_bits() {
        let caps = PresentCaps {
            flip_path: true,
            async_may_tear: true,
            syncobj: true,
        };
        assert_eq!(caps.encode(), 0x1 | 0x2 | 0x4 | 0x8 | 0x10);
    }
}

/// DRI3 capability surface. Phase 4.2 design §4. `version == (0, 0)`
/// is the "DRI3 unsupported" sentinel; any other value advertises
/// DRI3 to clients. The other booleans gate individual request types
/// rather than the whole extension.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Dri3Caps {
    /// Max DRI3 version this backend can serve. `(0, 0)` = unsupported.
    pub version: (u32, u32),
    /// `VK_EXT_image_drm_format_modifier` enabled — non-LINEAR
    /// modifiers can be imported. When false, `GetSupportedModifiers`
    /// only advertises `DRM_FORMAT_MOD_LINEAR`.
    pub modifiers: bool,
    /// `VK_KHR_external_semaphore_fd` with `SYNC_FD` handle type
    /// available — XSync `Fence` import via `FenceFromFD` works.
    /// When false, those requests reject with `BadImplementation`
    /// per the §4 fallback matrix.
    pub fence_fd: bool,
    /// `VK_KHR_external_semaphore_fd` `OPAQUE_FD` +
    /// `VK_KHR_timeline_semaphore` + DRM_SYNCOBJ ioctls all
    /// available. When false, `ImportSyncobj` / `FreeSyncobj` reject
    /// and the advertised version caps at `(1, 3)`.
    pub syncobj: bool,
}

impl Dri3Caps {
    /// The "DRI3 unsupported" sentinel. Used as the default-impl
    /// return value on backends that don't speak DRI3.
    #[must_use]
    pub const fn unsupported() -> Self {
        Self {
            version: (0, 0),
            modifiers: false,
            fence_fd: false,
            syncobj: false,
        }
    }
}

/// Result of a DRI3 `BuffersFromPixmap` (op 8) export — the single-plane
/// modifier-aware superset of what `BufferFromPixmap` (op 3) returns.
///
/// yserver exports are single-plane BGRA8, so `offset` is 0 and there is
/// one `(stride, offset)` pair; the struct exists so the op-8 reply can
/// carry the DRM `modifier`, which op 3 cannot. `stride` is `u32` (op 8
/// uses CARD32 strides; op 3 truncates to CARD16). The `fd`'s ownership
/// transfers to the caller.
#[derive(Debug)]
pub struct Dri3PixmapExport {
    pub size: u32,
    pub width: u16,
    pub height: u16,
    pub stride: u32,
    pub offset: u32,
    pub depth: u8,
    pub bpp: u8,
    pub modifier: u64,
    pub fd: std::os::fd::OwnedFd,
}

/// Stage 5 Task 6.1: opaque handle to a DRI3 xshmfence's underlying
/// shared-memory segment. Concrete impl in `yserver::kms::xshmfence::
/// FenceMapping`. Held as `Arc<dyn XshmfenceHandle>` by the deferred
/// PRESENT completion path so the underlying primitive can be
/// lifetime-pinned past `XFixesDestroyFence` while the resource id's
/// registry entry is dropped.
pub trait XshmfenceHandle: std::fmt::Debug + Send + Sync {
    /// Set the shared-memory fence's counter so any futex waiter on
    /// the client side wakes up. Mirrors the body of the existing
    /// `Backend::dri3_trigger_fence(xid)` after its registry lookup.
    fn trigger(&self) -> std::io::Result<()>;
}

/// Stage 5 Task 6.1: opaque handle to a DRI3 syncobj's underlying
/// VkSemaphore. Concrete impl in `yserver::kms::v2::owned_semaphore::
/// OwnedSemaphore`. Held as `Arc<dyn SyncobjHandle>` by the deferred
/// PRESENT completion path so the underlying semaphore can be
/// lifetime-pinned past `FreeSyncobj`.
pub trait SyncobjHandle: std::fmt::Debug + Send + Sync {
    /// Signal the timeline-semaphore at the given value. Mirrors
    /// the body of the existing `Backend::dri3_signal_syncobj(xid,
    /// value)` after its registry lookup.
    fn signal(&self, value: u64) -> std::io::Result<()>;
}

/// The dynamic backend surface. `Send` is required so that
/// `Arc<Mutex<dyn Backend>>` is `Send + Sync` (`Mutex<T>` is Sync iff
/// `T: Send`). `Sync` on the trait itself is not required because all
/// `Backend` access is mediated through a `Mutex`.
pub trait Backend: Send {
    // ──────────────────────────────────────────────────────────────
    // Lifecycle / state accessors
    // ──────────────────────────────────────────────────────────────

    fn window_id(&self) -> u32;
    fn root_visual_xid(&self) -> u32;
    fn argb_visual_xid(&self) -> Option<u32>;
    fn argb_colormap_xid(&self) -> Option<u32>;
    fn render_opcode(&self) -> Option<u8>;
    fn xkb_opcode(&self) -> Option<u8>;
    fn xkb_info(&self) -> Option<(u8, u8, u8)>;
    fn composite_opcode(&self) -> Option<u8>;
    fn render_format_for_ynest_id(&self, ynest_fmt: u32) -> Option<u32>;
    fn ping(&mut self, origin: Option<OriginContext>) -> io::Result<()>;

    // ──────────────────────────────────────────────────────────────
    // Single-threaded core hooks
    // ──────────────────────────────────────────────────────────────

    /// Dispatch a host input event the core received over the
    /// `Message::HostInput` channel. The backend produces zero or more
    /// X11 wire events via `state` fanout helpers. Filled in by E2
    /// (KMS) and F2 (host-X11); inert until then.
    fn on_host_input(&mut self, state: &mut ServerState, ev: HostInputEvent);

    /// DRM page-flip completion fd is readable. The backend should
    /// drain completion events and submit the next composite/flip.
    fn on_page_flip_ready(&mut self, state: &mut ServerState);

    /// The libseat connection fd is readable. The KMS backend dispatches
    /// libseat (which may fire enable/disable callbacks synchronously)
    /// and runs any resulting suspend/resume sequence. Default: no-op
    /// (ynest, host-X11, recording have no seat).
    fn on_seat_ready(&mut self, _state: &mut ServerState) {}

    /// Whether the backend has armed VT switching in direct mode.
    /// Default: false.
    fn vt_switching_armed(&self) -> bool {
        false
    }

    /// Direct-mode VT release signal. Default no-op.
    fn on_vt_release(&mut self, _state: &mut ServerState) {}

    /// Direct-mode VT acquire signal. Default no-op.
    fn on_vt_acquire(&mut self, _state: &mut ServerState) {}

    /// Request a switch to VT `n` (from a `Ctrl+Alt+F<n>` hotkey), via
    /// `VT_ACTIVATE`. Default no-op (only direct-mode KMS acts on it).
    fn request_vt_switch(&mut self, _vt: u32) {}

    /// The libinput fd is readable AND libinput is owned by the core
    /// loop (libseat mode). Dispatch libinput inline. Default: no-op —
    /// in Direct mode the dedicated input thread owns the fd and this is
    /// never registered.
    fn on_libinput_ready(&mut self, _state: &mut ServerState) {}

    /// Drain libinput's INITIAL device enumeration and seed the XI2
    /// device registry (`state.xi_devices`) BEFORE the core serves any
    /// client — the Xorg model of probing input at startup.
    ///
    /// Without this, the registry carries only the static {2,3,4,5}
    /// model until libinput's first `DeviceAdded` burst is dispatched
    /// from the main loop. On real hardware the desktop's clients can
    /// enumerate devices (`XIQueryDevice` / `XListInputDevices`) seconds
    /// before that burst lands, so they cache a plain pointer and never
    /// recognise the touchpad (observed on M2/Asahi MATE: all four
    /// libinput devices first processed ~2s AFTER the clients queried
    /// device 4).
    ///
    /// Implementations MUST be bounded and non-blocking: if libinput has
    /// nothing yet, return immediately rather than waiting. Only backends
    /// that own a libinput context on the core thread (libseat mode) do
    /// any work; every other backend (Direct mode — the context was moved
    /// to the input thread; host-X11/nested — no libinput) is a clean
    /// no-op via this default.
    ///
    /// Returns the number of devices seeded so the caller can log that
    /// the probe ran before clients connected.
    fn probe_input_devices(&mut self, _state: &mut ServerState) -> usize {
        0
    }

    /// Apply a decoded touchpad config change to the live input device
    /// identified by `device_node`. `Ok` = applied (or nothing to apply
    /// on this backend). `Err(Unsupported)` → BadMatch, `Err(Invalid)`
    /// → BadValue.
    ///
    /// Default: no-op success — only backends that own a live libinput
    /// context write through to a device.
    ///
    /// # Errors
    ///
    /// Returns `DeviceConfigError::Unsupported` when the setting isn't
    /// available on the addressed device, or `Invalid` when the value
    /// is out of range / not a legal one-hot.
    fn apply_device_config(
        &mut self,
        _device_node: &str,
        _change: crate::xinput::libinput_props::DeviceConfigChange,
    ) -> Result<(), crate::xinput::libinput_props::DeviceConfigError> {
        Ok(())
    }

    /// Hand the backend a core-channel sender so that, when it owns
    /// input on the core thread (libseat mode), it can emit the same
    /// control Messages the input thread would (Shutdown, DumpScanout,
    /// DumpDrawables). Default: no-op.
    fn set_input_sender(&mut self, _sender: crate::core_loop::CoreSender) {}

    /// Tell the backend that something that could affect on-screen
    /// pixels has changed since the last composite. KMS uses this to
    /// gate `composite_and_flip` so an idle server doesn't burn
    /// vsync-paced composite cycles. Default is a no-op for backends
    /// (e.g. host-X11) that don't drive their own composite loop.
    fn mark_dirty(&mut self) {}

    /// Earliest backend-owned deadline the core loop must wake for
    /// even when no fd is readable. Backends use this for retry
    /// timers that would otherwise be stranded behind an indefinite
    /// `poll()`. Default: no deadline.
    fn next_wakeup(&self) -> Option<std::time::Instant> {
        None
    }

    /// If the backend has pending damage and no flip is currently in
    /// flight, kick the next composite. Called once per core-loop
    /// iteration so a backend that went dormant after the last
    /// pageflip-complete (because nothing was dirty) wakes back up
    /// when fresh damage arrives. Default no-op.
    ///
    /// # Errors
    ///
    /// Propagates whatever the backend's composite path returns
    /// (e.g. DRM/Vulkan submit errors on KMS).
    fn maybe_composite(&mut self) -> io::Result<()> {
        Ok(())
    }

    /// Dump the current scanout buffer to a file in the process's
    /// cwd. Diagnostic-only — fired from `Message::DumpScanout`
    /// (SIGUSR1 on KMS). Default no-op for backends that don't
    /// drive their own composite.
    fn dump_scanout(&mut self) {}

    /// Dump the storage content of every "interesting" drawable
    /// (root, COW, every redirected backing) to files in cwd.
    /// Diagnostic-only — fired from `Message::DumpDrawables`
    /// (SIGUSR2 on KMS). Default no-op for backends that don't
    /// own per-drawable storage (v1 KmsBackend, ynest's
    /// HostX11Backend, RecordingBackend).
    fn dump_drawables(&mut self) {}

    /// Notify the backend that a window property changed or was
    /// deleted. KMS uses this to re-evaluate EWMH stack hints
    /// (`_NET_WM_WINDOW_TYPE`, `_NET_WM_STATE`, `WM_TRANSIENT_FOR`).
    /// Default no-op.
    fn on_window_property_changed(
        &mut self,
        _state: &ServerState,
        _host_xid: u32,
        _property: AtomId,
    ) {
    }

    /// Notify the backend that a window has become top-level under
    /// root. This lets backends re-apply stack hints that were set
    /// before the window re-entered `core.top_level_order`.
    /// Default no-op.
    fn on_window_became_top_level(&mut self, _state: &ServerState, _host_xid: u32) {}

    /// Observer hook fired on every PRESENT::Pixmap request, with
    /// the source pixmap xid and destination window xid the
    /// compositor named in the request. Backends use it to track
    /// which client offscreens drive presents to which targets —
    /// notably, the diagnostic drawable-dump (v2) uses the COW-
    /// targeted sources to extend its dump set to "marco's
    /// offscreen of the most recent compose," so a bisect of the
    /// "shadow only" bug can see the pixmap marco actually
    /// PresentPixmap'd. Default no-op; not part of paint
    /// correctness.
    fn note_present_pixmap(&mut self, _src_pixmap_xid: u32, _dst_window_xid: u32) {}

    /// CONFIRMATION hook (wezterm/Firefox transparent-content
    /// investigation 2026-06-03): before a `PresentPixmap` copy reads
    /// `src_pixmap_host_xid`, wait for the producing client's
    /// outstanding GPU writes to that buffer to complete. Only
    /// meaningful for DRI3-imported dma-buf sources where the present
    /// carried `wait_fence=0` and relied on implicit dma-buf sync that
    /// some GPU stacks (Turnip/Adreno, Apple) don't honour for
    /// yserver's read queue — yielding a partly-rendered (transparent)
    /// copy. Default no-op; the v2 backend bounds the wait with a
    /// timeout (never a hang) and falls back to a no-op for
    /// server-owned sources. The production fix replaces this CPU wait
    /// with a GPU acquire-semaphore on the frame-builder submit.
    fn wait_present_source_ready(&mut self, _src_pixmap_host_xid: u32) {}

    /// Raw fds the core's poller should watch on this backend's behalf.
    /// The core registers each fd against the matching token derived
    /// from `BackendFdKind`.
    fn poll_fds(&self) -> Vec<(std::os::fd::RawFd, BackendFdKind)>;

    /// Downcast to `Any` for backend-specific operations (e.g. KMS composite).
    fn as_any(&self) -> &dyn Any;
    /// Mutable downcast to `Any` for backend-specific operations.
    fn as_any_mut(&mut self) -> &mut dyn Any;

    // ──────────────────────────────────────────────────────────────
    // Subwindow lifecycle
    // ──────────────────────────────────────────────────────────────

    #[allow(clippy::too_many_arguments)]
    fn create_subwindow(
        &mut self,
        origin: Option<OriginContext>,
        host_parent: WindowHandle,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        border_width: u16,
        visual: HostSubwindowVisual,
        background_pixel: Option<u32>,
        background_pixmap: Option<u32>,
    ) -> io::Result<WindowHandle>;

    fn destroy_subwindow(&mut self, origin: Option<OriginContext>, host_xid: u32)
    -> io::Result<()>;

    fn map_subwindow(&mut self, origin: Option<OriginContext>, host_xid: u32) -> io::Result<()>;

    fn unmap_subwindow(&mut self, origin: Option<OriginContext>, host_xid: u32) -> io::Result<()>;

    fn configure_subwindow(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        config: HostSubwindowConfig,
    ) -> io::Result<()>;

    fn reparent_subwindow(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        host_parent: u32,
        x: i16,
        y: i16,
    ) -> io::Result<()>;

    fn change_subwindow_attributes(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        value_mask: u32,
        values: &[u32],
    ) -> io::Result<()>;

    fn update_host_event_mask(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        mask: u32,
        enabled: bool,
    ) -> io::Result<()>;

    /// Phase 6.3 Step 4: register a freshly-created host top-level so
    /// pointer / expose events on it route to `nested_id` through
    /// `pointer_event_fanout` / `expose_event_fanout`. Replaces the
    /// pre-Step-4 pump-handle `register_top_level`.
    fn register_top_level(
        &mut self,
        origin: Option<OriginContext>,
        nested_id: ResourceId,
        host_xid: u32,
    ) -> io::Result<()>;

    /// Phase 6.3 Step 4: same as [`Backend::register_top_level`] but
    /// for sub-windows — selects `Exposure` only so pointer events
    /// bubble up to the top-level ancestor where dispatch lives.
    fn register_subwindow(
        &mut self,
        origin: Option<OriginContext>,
        nested_id: ResourceId,
        host_xid: u32,
    ) -> io::Result<()>;

    /// Phase 6.3 Step 4: drop a `host_xid → ResourceId` mapping at
    /// DestroyWindow / Reparent-out so stale host events never
    /// misroute.
    fn unregister_host_window(&mut self, host_xid: u32);

    /// View of the `host_xid → ResourceId` map. F2: plain HashMap —
    /// the core thread is the only writer/reader, so no Arc/Mutex.
    fn xid_map(&self) -> &HostXidMap;

    /// F2: drain whatever the kernel has buffered on the host fd and
    /// classify into `pending_replies` / `pending_events`. KMS
    /// backend has no host fd; the default no-op suits.
    fn drain_host_socket(&mut self) -> io::Result<HostSocketStatus> {
        Ok(HostSocketStatus::WouldBlock)
    }

    /// F2: pop the next decoded host event so the core can fan out at
    /// the outer-loop boundary. Default `None` — only HostX11Backend
    /// produces these.
    fn pop_pending_host_event(&mut self) -> Option<HostEvent> {
        None
    }

    /// F2: did the host close the connection? `run_core` posts
    /// `Message::Shutdown` once this flips true.
    fn host_socket_eof(&self) -> bool {
        false
    }

    fn name_window_pixmap(
        &mut self,
        origin: Option<OriginContext>,
        host_window: WindowHandle,
    ) -> io::Result<PixmapHandle>;

    /// L2 plan B.6a: allocate the off-screen mirror that backs a
    /// newly redirected window. The backend allocates a fresh pixmap
    /// handle, sets up a mirror at `(width, height)` for `depth`,
    /// and seeds the alias registry's refcount=1 hold (reason 1 —
    /// "window is redirected"). Subsequent
    /// [`Self::name_window_pixmap`] calls on the same `host_window`
    /// return this same handle with an incremented refcount.
    ///
    /// Default `Unsupported` so test doubles and host-X11 backends
    /// don't have to implement it until they grow real redirect
    /// support.
    ///
    /// # Errors
    ///
    /// Returns `io::ErrorKind::Unsupported` when the backend hasn't
    /// implemented redirect activation. KMS overrides this in
    /// `crates/yserver/src/kms/backend.rs`.
    fn allocate_redirected_backing(
        &mut self,
        origin: Option<OriginContext>,
        host_window: WindowHandle,
        width: u16,
        height: u16,
        depth: u8,
    ) -> io::Result<PixmapHandle> {
        let _ = (origin, host_window, width, height, depth);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "allocate_redirected_backing not supported",
        ))
    }

    /// L2 plan B.6c: release a redirect's reason-1 hold on a
    /// backing pixmap. The backend decrements the alias registry's
    /// refcount and frees the underlying pixmap if no
    /// `NameWindowPixmap` alias holds it. Surviving aliases keep
    /// it alive until their own `FreePixmap`.
    ///
    /// Default no-op so test doubles and host-X11 backends without
    /// redirect support don't have to track aliases.
    ///
    /// # Errors
    ///
    /// Propagates backend errors from the underlying free path
    /// (e.g. KMS's `free_pixmap` on the final reference drop).
    fn release_redirected_backing(
        &mut self,
        origin: Option<OriginContext>,
        backing: PixmapHandle,
    ) -> io::Result<()> {
        let _ = (origin, backing);
        Ok(())
    }

    /// Take an extra alias_registry reference on a redirected
    /// backing. Used by `rotate_redirected_backing_on_resize` to
    /// keep OLD's storage alive across the release→copy gap when
    /// no `NameWindowPixmap` aliases hold it; paired with
    /// [`Self::drop_backing_storage`] after the copy.
    ///
    /// Default no-op so test doubles and non-redirect backends
    /// don't need to model the alias registry.
    ///
    /// # Errors
    /// Propagates backend-internal failures (none today; reserved).
    fn retain_backing_storage(
        &mut self,
        origin: Option<OriginContext>,
        backing: PixmapHandle,
    ) -> io::Result<()> {
        let _ = (origin, backing);
        Ok(())
    }

    /// Drop the extra alias_registry reference taken by
    /// [`Self::retain_backing_storage`]. Frees the underlying
    /// storage iff no other holds remain (no Reason-1 redirect
    /// hold, no `NameWindowPixmap` aliases).
    ///
    /// Default no-op.
    ///
    /// # Errors
    /// Propagates backend-internal failures from the free path on
    /// the final-ref drop.
    fn drop_backing_storage(
        &mut self,
        origin: Option<OriginContext>,
        backing: PixmapHandle,
    ) -> io::Result<()> {
        let _ = (origin, backing);
        Ok(())
    }

    /// Stage 4b — capability flag: returns `true` when the backend
    /// implements the full COMPOSITE-redirect activation path
    /// (`allocate_redirected_backing` + `release_redirected_backing` +
    /// `name_window_pixmap` + the scene-participation flips). The
    /// `process_request.rs` Composite handlers gate the activation
    /// path on this so v1 (still on the pre-Stage-4 "redirect record
    /// but no backing routing" shape) doesn't regress on MATE the
    /// way commit `92a2a83` did before being reverted at `3751c11`.
    ///
    /// Default `false` covers v1 + the host-X11 test backends.
    /// v2 (`KmsBackendV2`) overrides to `true`.
    fn supports_redirect_activation(&self) -> bool {
        false
    }

    /// Whether this backend can allocate and export a BGRA8 dma-buf via
    /// the Vulkan external-memory path.  When `true`, the GLX extension
    /// string includes `GLX_EXT_texture_from_pixmap`.
    ///
    /// **Cached at construction** (real probe in `KmsBackendV2::new`);
    /// this getter is a trivial field read — no Vulkan calls.
    /// Default `false` covers all non-KMS backends.
    fn supports_dmabuf_export(&self) -> bool {
        false
    }

    /// Whether this backend can actually power-cycle a display.
    /// `yserver-hw` returns `true`; ynest and recording backends
    /// return `false`. `ServerState` snapshots this at init so the
    /// idle timer never fires on a backend that can't act on it.
    fn dpms_capable(&self) -> bool {
        false
    }

    /// Drive the display to a DPMS power level (`0=On, 1=Standby,
    /// 2=Suspend, 3=Off`). `yserver-hw` collapses non-On levels to a
    /// single "outputs disabled" KMS state. Default is a no-op so
    /// ynest, host-X11, and recording backends remain protocol-
    /// compliant without touching the host display.
    ///
    /// # Errors
    ///
    /// May return DRM atomic-commit errors; the caller (the
    /// `apply_dpms_transition` helper) logs and advances the
    /// in-memory state regardless of the result.
    fn set_dpms_power(&mut self, _level: u8) -> std::io::Result<()> {
        Ok(())
    }

    /// Stage 4b — flip a window's scene-participation under
    /// COMPOSITE redirect. Manual mode: `participating = false`
    /// removes the window from the scene walk so the external
    /// compositor (reading via `NameWindowPixmap`) drives the
    /// on-screen presentation; the I5 transactional rule on
    /// `set_scene_participating` clears unpresented presentation
    /// damage + bumps the epoch. Automatic mode: stays `true`.
    /// On unredirect, restored to the window's `mapped` state.
    ///
    /// Default no-op for backends without scene-state (v1's
    /// per-window-mirror model has no equivalent flip; the test
    /// `RecordingBackend` keeps the default too).
    ///
    /// # Errors
    /// Propagates backend-internal failures on the participation
    /// flip path (no path errors today, but reserved).
    fn set_window_scene_participation(
        &mut self,
        origin: Option<OriginContext>,
        host_window: WindowHandle,
        participating: bool,
    ) -> io::Result<()> {
        let _ = (origin, host_window, participating);
        Ok(())
    }

    /// Stage 4b — flip a backing's scene-participation under
    /// COMPOSITE redirect. Used by Automatic mode so paint that
    /// resolves through the backing accumulates presentation
    /// damage on B (which the scene walk picks up via W's
    /// `redirected_target` indirection in 4c's `build_scene`
    /// patch). Manual backings stay `false`; only Automatic
    /// activations call this with `true`. On unredirect /
    /// destroy, `release_redirected_backing` drops the flag
    /// internally so the protocol handler doesn't need to fire
    /// a separate `false` call.
    ///
    /// Default no-op as for `set_window_scene_participation`.
    ///
    /// # Errors
    /// Same shape as `set_window_scene_participation`.
    fn set_backing_scene_participation(
        &mut self,
        origin: Option<OriginContext>,
        backing: PixmapHandle,
        participating: bool,
    ) -> io::Result<()> {
        let _ = (origin, backing, participating);
        Ok(())
    }

    /// Stage 4d — Composite Overlay Window allocation + materialization.
    /// On the 0→1 refcount transition (first claim), allocate backing
    /// storage AND populate the backend's window-tree projection: a
    /// `windows_v2` entry sized to full screen extent (mapped=true,
    /// depth=24, parent=root) and a slot at the top of
    /// `top_level_order`. Returns `Ok(true)` on first claim, `Ok(false)`
    /// on subsequent claims (refcount bump only — no new
    /// materialization).
    ///
    /// The core handler uses the bool to drive the symmetric resources-
    /// side materialization (`materialize_cow_resource`). Both halves
    /// must succeed or both must rollback; an Err here is a fatal
    /// internal-consistency failure per the spec.
    ///
    /// Compositors (marco, xfwm4 with compositing on, etc.)
    /// `XSelectInput` on this XID and paint directly into it;
    /// pre-Stage-4d the xid resolved to nothing in v2's
    /// `DrawableStore` so every `render_composite` against it
    /// gap-logged and dropped paint — the load-bearing gate for
    /// compositing WMs (Stage 4 plan §4d).
    ///
    /// Default no-op covers v1 (uses its own per-window mirror
    /// model and never reaches this trait) plus the test
    /// `RecordingBackend` / ynest. v2 (`KmsBackendV2`) overrides.
    ///
    /// # Errors
    ///
    /// Backend-internal failures on storage allocation or store
    /// insertion. Per protocol the `GetOverlayWindow` reply must
    /// still go out on the wire when this errors — callers
    /// (`process_request.rs`) log + continue.
    fn get_overlay_window(&mut self, origin: Option<OriginContext>) -> io::Result<bool> {
        let _ = origin;
        Ok(false)
    }

    /// Stage 4d — Composite Overlay Window release hook.
    ///
    /// Called on each `XComposite::ReleaseOverlayWindow`
    /// request. Decrements the COW refcount; on the final
    /// release the backend unregisters the scene entry and
    /// frees the storage. Defensive against unmatched releases
    /// (refcount=0 → no-op).
    ///
    /// Returns `Ok(true)` when **this** release was the final
    /// one (refcount transitioned to 0 and the backend
    /// destroyed the COW storage); `Ok(false)` otherwise
    /// (refcount > 0 after decrement, defensive no-op, or
    /// backends that don't track COW lifecycle — v1, ynest).
    /// The handler uses this signal to drive
    /// `destroy_cow_resource()` in lockstep: the resources-side
    /// Window record and its slot in `root.children` come down
    /// together with the backend storage. The next
    /// `GetOverlayWindow` re-materializes the whole thing fresh
    /// via `materialize_cow_resource()`.
    ///
    /// The v2 impl (`KmsBackendV2`) also tears down its
    /// `windows_v2` entry and the COW's slot in
    /// `top_level_order` on the final release — the mirror of
    /// the materialization that `get_overlay_window`'s 0→1
    /// branch installs. `RecordingBackend` similarly tracks
    /// 1→0 via its `cow_materialized` flag for handler-wiring
    /// tests. Single backend hook owns the full lifecycle in
    /// both directions; there is no separate `destroy_cow` API.
    ///
    /// Default no-op as for `get_overlay_window`: returns
    /// `Ok(false)` because "I didn't destroy anything."
    ///
    /// # Errors
    ///
    /// Same shape as `get_overlay_window`.
    fn release_overlay_window(&mut self, origin: Option<OriginContext>) -> io::Result<bool> {
        let _ = origin;
        Ok(false)
    }

    /// Stage 4e — return the host xid the backend has assigned to the
    /// COW, or `None` if the COW is not currently materialized. The
    /// core handler reads this to populate the resources COW record's
    /// `host_xid` field after `get_overlay_window`'s 0→1 return.
    fn cow_host_xid(&self) -> Option<u32> {
        None
    }

    // ──────────────────────────────────────────────────────────────
    // Resources (pixmap, font, cursor)
    // ──────────────────────────────────────────────────────────────

    fn create_pixmap(
        &mut self,
        origin: Option<OriginContext>,
        depth: u8,
        width: u16,
        height: u16,
    ) -> io::Result<PixmapHandle>;

    fn free_pixmap(&mut self, origin: Option<OriginContext>, host_xid: u32) -> io::Result<()>;

    fn open_font(
        &mut self,
        origin: Option<OriginContext>,
        name: &str,
    ) -> io::Result<(FontHandle, FontMetrics)>;

    /// SetFontPath (opcode 51): validate + install the server font
    /// path. `Err(index)` = the offending element's position (handler
    /// emits BadValue; old path must be kept). Empty list = reset to
    /// the backend's default path.
    ///
    /// Default impl accepts and ignores: host-proxy backends (ynest)
    /// must NOT forward this to the host — the host font path is
    /// global state shared with every other host client.
    fn set_font_path(
        &mut self,
        _origin: Option<OriginContext>,
        _paths: &[String],
    ) -> Result<(), usize> {
        Ok(())
    }

    /// Current font path for GetFontPath (opcode 52).
    fn font_path(&self) -> Vec<String> {
        vec!["built-ins".to_string()]
    }

    /// Paint a window's background (pixel or tile) into a
    /// window-local rect. Used by CopyArea/CopyPlane missing-source
    /// semantics (X11 §CopyArea: when the destination is a window
    /// with bg != None, destination regions corresponding to
    /// unavailable source are tiled with the background, GXcopy +
    /// all-ones plane-mask). Default no-op; KMS implements via its
    /// window bg machinery. TODO(ynest): forward as a host ClearArea
    /// so the host repaints its own bg (we clamp the copy before the
    /// host sees it, so the host no longer bg-tiles on its own).
    fn paint_window_background_rect(
        &mut self,
        _origin: Option<OriginContext>,
        _host_xid: u32,
        _x: i16,
        _y: i16,
        _width: u16,
        _height: u16,
    ) -> io::Result<()> {
        Ok(())
    }

    fn close_font(&mut self, origin: Option<OriginContext>, host_xid: u32) -> io::Result<()>;

    fn create_cursor(
        &mut self,
        origin: Option<OriginContext>,
        source_pixmap: PixmapHandle,
        mask_pixmap: Option<PixmapHandle>,
        fore: (u16, u16, u16),
        back: (u16, u16, u16),
        hot_x: u16,
        hot_y: u16,
    ) -> io::Result<CursorHandle>;

    /// Free a host cursor. Counterpart to `create_cursor`. Default impl
    /// is a no-op so backends without a real X-server peer (KMS) need
    /// no boilerplate.
    fn free_cursor(&mut self, _origin: Option<OriginContext>, _host_xid: u32) -> io::Result<()> {
        Ok(())
    }

    /// RENDER `CreateAnimCursor` (opcode 31). `frames` pairs each
    /// sub-cursor's host handle with its delay in milliseconds.
    /// Returns the new animated cursor's handle, or `Ok(None)` when
    /// the backend does not animate — the caller then falls back to
    /// the static degeneration (cursor aliases frame 0's handle).
    /// Spec: `docs/superpowers/specs/2026-06-10-animated-cursors-design.md`.
    fn create_anim_cursor(
        &mut self,
        _origin: Option<OriginContext>,
        _frames: &[(CursorHandle, u32)],
    ) -> io::Result<Option<CursorHandle>> {
        Ok(None)
    }

    /// Create a cursor from two glyph indices in two fonts (X11 core
    /// `CreateGlyphCursor`, opcode 94). The protocol does not carry an
    /// explicit hotspot — the backend computes one from the source
    /// glyph's metrics (origin point of the source glyph in the
    /// rendered cursor pixmap).
    ///
    /// `mask_font` is `None` when the wire request had `mask = None`,
    /// in which case the source glyph doubles as the mask: cursor
    /// pixel is visible iff the source bit is set, and visible pixels
    /// always carry `fore`.
    fn create_glyph_cursor(
        &mut self,
        origin: Option<OriginContext>,
        source_font: FontHandle,
        mask_font: Option<FontHandle>,
        source_char: u16,
        mask_char: u16,
        fore: (u16, u16, u16),
        back: (u16, u16, u16),
    ) -> io::Result<CursorHandle>;

    fn define_cursor(
        &mut self,
        origin: Option<OriginContext>,
        host_window_xid: u32,
        cursor_host_xid: u32,
    ) -> io::Result<()>;

    // ──────────────────────────────────────────────────────────────
    // Container background (root-mapped helpers)
    // ──────────────────────────────────────────────────────────────

    fn set_container_background_pixel(
        &mut self,
        origin: Option<OriginContext>,
        pixel: u32,
    ) -> io::Result<()>;

    fn set_container_background_pixmap(
        &mut self,
        origin: Option<OriginContext>,
        host_pixmap_xid: u32,
    ) -> io::Result<()>;

    // ──────────────────────────────────────────────────────────────
    // GC state (sync points feeding the host's shared GC)
    // ──────────────────────────────────────────────────────────────

    fn clear_clip_rectangles(&mut self, origin: Option<OriginContext>) -> io::Result<()>;

    fn set_clip_rectangles(
        &mut self,
        origin: Option<OriginContext>,
        clip: Option<ClipRectangles>,
    ) -> io::Result<()>;

    fn set_clip_pixmap(
        &mut self,
        origin: Option<OriginContext>,
        host_pixmap: u32,
        clip_x_origin: i16,
        clip_y_origin: i16,
    ) -> io::Result<()>;

    fn set_gc_fill_solid(&mut self, origin: Option<OriginContext>) -> io::Result<()>;

    fn set_gc_fill_tiled(
        &mut self,
        origin: Option<OriginContext>,
        host_pixmap: u32,
        tile_x_origin: i16,
        tile_y_origin: i16,
    ) -> io::Result<()>;

    fn apply_clip_state(
        &mut self,
        origin: Option<OriginContext>,
        clip: &ClipState,
    ) -> io::Result<()>;

    fn apply_fill_state(
        &mut self,
        origin: Option<OriginContext>,
        fill: &FillState,
    ) -> io::Result<()>;

    fn apply_draw_state(
        &mut self,
        origin: Option<OriginContext>,
        state: &DrawState,
    ) -> io::Result<()>;

    // ──────────────────────────────────────────────────────────────
    // Drawing primitives
    //
    // These match the existing `HostX11Backend::*` signatures: they
    // take raw `u32` host xids and a foreground colour (the Phase 6.2
    // additive scope adds `&DrawState` propagation through the
    // `apply_draw_state` sync point above; the methods themselves are
    // unchanged). Phase 6.3+ may collapse the surface as the trait
    // grows additional impls, but for now matching the existing shape
    // keeps Step 5 churn-free at every call site.
    // ──────────────────────────────────────────────────────────────

    #[allow(clippy::too_many_arguments)]
    fn copy_area(
        &mut self,
        origin: Option<OriginContext>,
        src_host_xid: u32,
        dst_host_xid: u32,
        src_x: i16,
        src_y: i16,
        dst_x: i16,
        dst_y: i16,
        width: u16,
        height: u16,
    ) -> io::Result<()>;

    #[allow(clippy::too_many_arguments)]
    fn copy_plane(
        &mut self,
        origin: Option<OriginContext>,
        src_host_xid: u32,
        dst_host_xid: u32,
        src_x: i16,
        src_y: i16,
        dst_x: i16,
        dst_y: i16,
        width: u16,
        height: u16,
        plane: u32,
    ) -> io::Result<()>;

    #[allow(clippy::too_many_arguments)]
    fn put_image(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        depth: u8,
        width: u16,
        height: u16,
        dst_x: i16,
        dst_y: i16,
        data: &[u8],
    ) -> io::Result<()>;

    #[allow(clippy::too_many_arguments)]
    fn get_image(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        format: u8,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        plane_mask: u32,
    ) -> io::Result<Option<Vec<u8>>>;

    /// `tile_origin` shifts where the bg tile is sampled — nonzero
    /// for ParentRelative backgrounds, whose pattern aligns to the
    /// OWNING ancestor's origin (X11 §window background).
    #[allow(clippy::too_many_arguments)]
    fn clear_area(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        background_pixel: u32,
        background_pixmap_host_xid: Option<u32>,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        tile_origin: (i32, i32),
    ) -> io::Result<()> {
        self.clear_clip_rectangles(origin)?;
        if let Some(bg_host_xid) = background_pixmap_host_xid {
            let sx = i16::try_from(i32::from(x) + tile_origin.0).unwrap_or(x);
            let sy = i16::try_from(i32::from(y) + tile_origin.1).unwrap_or(y);
            self.copy_area(origin, bg_host_xid, host_xid, sx, sy, x, y, width, height)
        } else {
            self.fill_rectangle(origin, host_xid, background_pixel, x, y, width, height)
        }
    }

    /// Read a depth-1 pixmap's pixels as a tightly packed `(width,
    /// height, bytes)` triple where each byte is non-zero for set
    /// pixels and zero for clear. Used by SHAPE `Mask` to convert a
    /// mask pixmap into YX-banded rectangles. Default impl returns
    /// `None` so backends without bitmap-introspection (host-X11
    /// proxy) can stay best-effort.
    fn read_depth1_pixmap(
        &mut self,
        _origin: Option<OriginContext>,
        _host_xid: u32,
    ) -> io::Result<Option<(u32, u32, Vec<u8>)>> {
        Ok(None)
    }

    fn poly_line(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        coordinate_mode: u8,
        points: &[u8],
    ) -> io::Result<()>;

    fn poly_segment(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        segments: &[u8],
    ) -> io::Result<()>;

    fn poly_rectangle(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        rectangles: &[u8],
    ) -> io::Result<()>;

    fn poly_arc(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        arcs: &[u8],
    ) -> io::Result<()>;

    fn poly_point(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        coordinate_mode: u8,
        points: &[u8],
    ) -> io::Result<()>;

    fn poly_fill_rectangle(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        rectangles: &[u8],
    ) -> io::Result<()>;

    fn poly_fill_arc(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        arcs: &[u8],
    ) -> io::Result<()>;

    fn fill_poly(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        coord_mode: u8,
        points: &[u8],
    ) -> io::Result<()>;

    fn fill_rectangle(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
    ) -> io::Result<()>;

    fn poly_text8(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        body: &[u8],
    ) -> io::Result<()>;

    fn poly_text16(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        body: &[u8],
    ) -> io::Result<()>;

    fn image_text8(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        background: u32,
        text_len: u8,
        body: &[u8],
    ) -> io::Result<()>;

    fn image_text16(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        background: u32,
        text_len: u8,
        body: &[u8],
    ) -> io::Result<()>;

    // ──────────────────────────────────────────────────────────────
    // RENDER
    // ──────────────────────────────────────────────────────────────

    fn render_create_picture(
        &mut self,
        origin: Option<OriginContext>,
        host_drawable: AnyHandle,
        ynest_format: u32,
        value_mask: u32,
        values: &[u8],
    ) -> io::Result<Option<PictureHandle>>;

    /// Store the drawable-space origin for a picture so that window-backed
    /// pictures can translate external region geometry into picture-local
    /// coordinates. Default no-op for backends that don't track this.
    fn set_picture_drawable_origin(&mut self, _host_pic: u32, _origin: (i16, i16)) {}

    /// Return the client-set clip rectangles for a picture (if any).
    /// Used by `CreateRegionFromPicture`. Default returns `None`.
    fn picture_client_clip_rects(
        &mut self,
        _host_pic: u32,
    ) -> Option<Option<Vec<xfixes::RegionRect>>> {
        None
    }

    fn render_change_picture(
        &mut self,
        origin: Option<OriginContext>,
        host_pic: u32,
        body: &[u8],
    ) -> io::Result<()>;

    fn render_free_picture(
        &mut self,
        origin: Option<OriginContext>,
        host_pic: u32,
    ) -> io::Result<()>;

    fn render_create_glyphset(
        &mut self,
        origin: Option<OriginContext>,
        ynest_format: u32,
    ) -> io::Result<Option<GlyphSetHandle>>;

    fn render_free_glyphset(
        &mut self,
        origin: Option<OriginContext>,
        host_gs: u32,
    ) -> io::Result<()>;

    fn render_add_glyphs(
        &mut self,
        origin: Option<OriginContext>,
        host_gs: u32,
        body_tail: &[u8],
    ) -> io::Result<()>;

    fn render_free_glyphs(
        &mut self,
        origin: Option<OriginContext>,
        host_gs: u32,
        glyph_ids: &[u8],
    ) -> io::Result<()>;

    #[allow(clippy::too_many_arguments)]
    fn render_composite(
        &mut self,
        origin: Option<OriginContext>,
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
    ) -> io::Result<()>;

    #[allow(clippy::too_many_arguments)]
    fn render_composite_glyphs(
        &mut self,
        origin: Option<OriginContext>,
        minor: u8,
        op: u8,
        host_src: u32,
        host_dst: u32,
        mask_fmt: u32,
        host_gs: u32,
        src_x: i16,
        src_y: i16,
        items: &[u8],
        x_off: i16,
        y_off: i16,
    ) -> io::Result<()>;

    fn render_fill_rectangles(
        &mut self,
        origin: Option<OriginContext>,
        host_dst: u32,
        op: u8,
        color: [u8; 8],
        rects: &[u8],
        x_off: i16,
        y_off: i16,
    ) -> io::Result<()>;

    #[allow(clippy::too_many_arguments)]
    fn render_trapezoids(
        &mut self,
        origin: Option<OriginContext>,
        op: u8,
        host_src: u32,
        host_dst: u32,
        host_mask_format: u32,
        src_x: i16,
        src_y: i16,
        traps: &[u8],
        x_off: i16,
        y_off: i16,
    ) -> io::Result<()>;

    /// RENDER `Triangles` (minor 11), `TriStrip` (12), `TriFan` (13).
    /// `primitives` is the wire body after the fixed prefix:
    /// 24-byte `XTriangle`s for minor 11, packed `XPointFixed`s
    /// (8 bytes each) for 12 and 13.
    #[allow(clippy::too_many_arguments)]
    fn render_triangles_op(
        &mut self,
        _origin: Option<OriginContext>,
        _minor: u8,
        _op: u8,
        _host_src: u32,
        _host_dst: u32,
        _host_mask_format: u32,
        _src_x: i16,
        _src_y: i16,
        _primitives: &[u8],
        _x_off: i16,
        _y_off: i16,
    ) -> io::Result<()> {
        Ok(())
    }

    fn render_create_solid_fill(
        &mut self,
        origin: Option<OriginContext>,
        color: [u8; 8],
    ) -> io::Result<Option<PictureHandle>>;

    fn render_create_linear_gradient(
        &mut self,
        origin: Option<OriginContext>,
        body: &[u8],
    ) -> io::Result<Option<PictureHandle>>;

    fn render_create_radial_gradient(
        &mut self,
        origin: Option<OriginContext>,
        body: &[u8],
    ) -> io::Result<Option<PictureHandle>>;

    fn render_create_cursor(
        &mut self,
        origin: Option<OriginContext>,
        host_src_pic: PictureHandle,
        x: u16,
        y: u16,
    ) -> io::Result<Option<CursorHandle>>;

    fn render_set_picture_clip_rectangles(
        &mut self,
        origin: Option<OriginContext>,
        host_pic: u32,
        body: &[u8],
    ) -> io::Result<()>;

    fn render_set_picture_filter(
        &mut self,
        origin: Option<OriginContext>,
        host_pic: u32,
        body: &[u8],
    ) -> io::Result<()>;

    fn render_set_picture_transform(
        &mut self,
        origin: Option<OriginContext>,
        host_pic: u32,
        body: &[u8],
    ) -> io::Result<()>;

    fn render_query_version(&mut self, origin: Option<OriginContext>) -> io::Result<(u32, u32)>;

    // ──────────────────────────────────────────────────────────────
    // DRI3 (Phase 4.2)
    // ──────────────────────────────────────────────────────────────

    /// Hand the client a render-node fd matching the scanout device.
    /// Default impl: DRI3 unsupported on this backend (HostX11Backend
    /// and `RecordingBackend` keep the default; KmsBackend overrides).
    /// On success the returned fd's ownership transfers to the caller,
    /// which dispatches it to the client via `SCM_RIGHTS`.
    ///
    /// `_drawable` is currently unused (single-GPU only — the fd is
    /// the same regardless of which drawable triggered the request).
    fn dri3_open(&mut self, _drawable: u32) -> io::Result<std::os::fd::OwnedFd> {
        Err(io::Error::other("DRI3 unsupported on this backend"))
    }

    /// DRI3 capability surface. The `(0, 0)` sentinel for `version`
    /// signals "DRI3 entirely unsupported"; the dispatcher's
    /// extension-query path filters DRI3 out of `EXTENSIONS` in that
    /// case. `modifiers`, `fence_fd`, `syncobj` are sub-capabilities
    /// that gate individual requests rather than the whole extension
    /// — see the §4 fallback matrix.
    fn dri3_capabilities(&self) -> Dri3Caps {
        Dri3Caps::unsupported()
    }

    /// Import a client-supplied dma-buf as a server-owned pixmap and
    /// return the host xid the dispatcher should bind. The `fd`
    /// ownership transfers into the backend on success; on Err the
    /// fd is dropped (closed) by the OwnedFd's normal drop path.
    ///
    /// Phase 4.2 only handles single-plane (RGB) imports; the
    /// multi-plane variant of `PixmapFromBuffers` is rejected at the
    /// dispatcher.
    #[allow(clippy::too_many_arguments)]
    fn dri3_import_pixmap(
        &mut self,
        _fd: std::os::fd::OwnedFd,
        _width: u16,
        _height: u16,
        _stride: u32,
        _offset: u32,
        _modifier: u64,
        _depth: u8,
        _bpp: u8,
    ) -> io::Result<PixmapHandle> {
        Err(io::Error::other("DRI3 import unsupported on this backend"))
    }

    /// Return `(window_modifiers, screen_modifiers)` for the
    /// `(depth, bpp)` X format under the given window. Per design
    /// §3.2: screen list is the full GPU-supported set; window list
    /// is the subset that the window's output can flip-scanout (so
    /// `window_modifiers ⊆ screen_modifiers` always holds).
    ///
    /// Default impl returns `[LINEAR]` for both lists — backends
    /// that lack `VK_EXT_image_drm_format_modifier` end up here per
    /// the design §4 fallback matrix row 1.
    fn dri3_supported_modifiers(&self, _window: u32, _depth: u8, _bpp: u8) -> (Vec<u64>, Vec<u64>) {
        // 0 == DRM_FORMAT_MOD_LINEAR.
        (vec![0], vec![0])
    }

    /// Export an existing pixmap's backing memory as a fresh dma-buf
    /// fd plus the metadata `BufferFromPixmap` reply needs. Phase 4.2
    /// design §3.2. Returns `(size, width, height, stride, depth, bpp,
    /// fd)`. The fd's ownership transfers to the caller.
    ///
    /// Default impl is unsupported.
    fn dri3_export_pixmap(
        &mut self,
        _host_xid: u32,
    ) -> io::Result<(u32, u16, u16, u16, u8, u8, std::os::fd::OwnedFd)> {
        Err(io::Error::other("DRI3 export unsupported on this backend"))
    }

    /// Export a pixmap for DRI3 `BuffersFromPixmap` (op 8): the same
    /// dma-buf as [`Self::dri3_export_pixmap`] plus the DRM modifier and
    /// per-plane offset the multi-plane reply carries. Mesa's loader_dri3
    /// uses op 8 (not op 3) whenever the server advertises DRI3 ≥ 1.2, so
    /// a modifier-tiled backing MUST be exported through here.
    ///
    /// Default impl is unsupported.
    fn dri3_export_pixmap_buffers(&mut self, _host_xid: u32) -> io::Result<Dri3PixmapExport> {
        Err(io::Error::other(
            "DRI3 BuffersFromPixmap unsupported on this backend",
        ))
    }

    /// Import a `sync_file` fd as the backing of an XSync `Fence`
    /// resource. The fd ownership transfers into the backend; on
    /// success the backend owns a `VkSemaphore` keyed by `fence_xid`.
    ///
    /// Default impl is unsupported.
    fn dri3_fence_from_fd(&mut self, _fence_xid: u32, _fd: std::os::fd::OwnedFd) -> io::Result<()> {
        Err(io::Error::other("DRI3 FenceFromFD unsupported"))
    }

    /// Export the `VkSemaphore` backing `fence_xid` as a fresh
    /// `sync_file` fd. Returned fd's ownership transfers to the
    /// caller.
    ///
    /// Default impl is unsupported.
    fn dri3_fd_from_fence(&mut self, _fence_xid: u32) -> io::Result<std::os::fd::OwnedFd> {
        Err(io::Error::other("DRI3 FDFromFence unsupported"))
    }

    /// Import a `DRM_SYNCOBJ` fd as a timeline `VkSemaphore` keyed by
    /// `syncobj_xid`. fd ownership transfers in.
    ///
    /// Default impl is unsupported.
    fn dri3_import_syncobj(
        &mut self,
        _syncobj_xid: u32,
        _fd: std::os::fd::OwnedFd,
    ) -> io::Result<()> {
        Err(io::Error::other("DRI3 ImportSyncobj unsupported"))
    }

    /// Drop the timeline `VkSemaphore` keyed by `syncobj_xid`.
    fn dri3_free_syncobj(&mut self, _syncobj_xid: u32) -> io::Result<()> {
        Err(io::Error::other("DRI3 FreeSyncobj unsupported"))
    }

    /// Host-signal the timeline `VkSemaphore` keyed by `syncobj_xid`
    /// to `value`. Used by `PresentPixmapSynced`'s Copy path: when
    /// the synchronous CopyArea completes, signal the client's
    /// `release_syncobj` so Mesa's `vkAcquireNextImage` wakes up.
    /// Phase 4.2 design §3.3.2.
    fn dri3_signal_syncobj(&mut self, _syncobj_xid: u32, _value: u64) -> io::Result<()> {
        Err(io::Error::other("DRI3 SignalSyncobj unsupported"))
    }

    /// Stage 5 Task 6.1: take an Arc clone of the syncobj's
    /// underlying VkSemaphore wrapper, suitable for deferred
    /// completion paths that need to survive an intervening
    /// `FreeSyncobj`.
    fn dri3_syncobj_handle(&self, _syncobj_xid: u32) -> Option<std::sync::Arc<dyn SyncobjHandle>> {
        None
    }

    /// Stage 5 Task 6.1: signal the syncobj timeline point via a held
    /// Arc clone, bypassing xid lookup.
    fn dri3_signal_syncobj_via_handle(
        &mut self,
        handle: &std::sync::Arc<dyn SyncobjHandle>,
        value: u64,
    ) -> std::io::Result<()> {
        handle.signal(value)
    }

    /// Trigger an XSync `Fence` resource imported via DRI3
    /// `FenceFromFD`. For xshmfence-backed fences (Mesa's
    /// loader_dri3) this calls `xshmfence_trigger`, atomically
    /// setting the shared counter and waking any process waiting on
    /// the futex. Used by `PresentPixmap`'s Copy path to signal
    /// `idle_fence` when the GPU finishes reading.
    fn dri3_trigger_fence(&mut self, _fence_xid: u32) -> io::Result<()> {
        Ok(())
    }

    /// Stage 5 Task 6.1: take an Arc clone of the xshmfence's
    /// underlying primitive, suitable for deferred completion paths
    /// that need to survive an intervening `XFixesDestroyFence`.
    /// Returns `None` if the xid isn't in the registry at call time.
    fn dri3_xshmfence_handle(
        &self,
        _fence_xid: u32,
    ) -> Option<std::sync::Arc<dyn XshmfenceHandle>> {
        None
    }

    /// Stage 5 Task 6.1: trigger the xshmfence via a held Arc clone,
    /// bypassing xid lookup. Used by the deferred PRESENT completion
    /// drain when the resource id may have been freed mid-flight.
    fn dri3_trigger_fence_via_handle(
        &mut self,
        handle: &std::sync::Arc<dyn XshmfenceHandle>,
    ) -> std::io::Result<()> {
        handle.trigger()
    }

    /// Per-window Present capability surface. Default impl: all-false
    /// (Copy-only, no syncobj). KmsBackend overrides;
    /// `HostX11Backend` and `RecordingBackend` keep the default.
    fn present_capabilities(&self, _window: u32) -> PresentCaps {
        PresentCaps::default()
    }

    /// Stage 5 Task 6.1: enqueue a deferred PRESENT completion. The
    /// backend pins the wake primitive, binds the payload to the GPU
    /// work that makes `dst_host_xid` idle, and returns immediately.
    /// The drain hook later fires the wake signal + the event payload.
    ///
    /// `dst_host_xid` is the backend's drawable-lookup key — server-
    /// internal host xid, NOT the client xid carried by `event.dst_
    /// host_xid` (which is the client-side fan-out match key). The
    /// two arguments are separate so the field semantics inside
    /// `CompletedPresentEvent` stay client-facing for the event
    /// payload.
    ///
    /// Default impl is no-op so non-v2 backends opt out.
    fn enqueue_present_completion(&mut self, _event: CompletedPresentEvent, _dst_host_xid: u32) {
        // no-op
    }

    /// Stage 5 Task 6.1: drain entries whose cow_batch fence has
    /// signalled. The backend internally fires the xshmfence /
    /// syncobj signal via the Arc-pinned handle before returning
    /// the events. Caller is responsible for X11 event fan-out +
    /// `state.sync_fences` bookkeeping based on the returned
    /// `PresentWake` variant. Default impl returns empty so non-v2
    /// backends opt out.
    fn drain_completed_present_events(&mut self) -> Vec<CompletedPresentEvent> {
        Vec::new()
    }

    // ──────────────────────────────────────────────────────────────
    // GLX_EXT_texture_from_pixmap export-lifetime management
    // ──────────────────────────────────────────────────────────────

    /// Increment the GLX-consumer refcount on the exported backing for
    /// `host_xid`. Called when `glXCreatePixmap` (CREATE_PIXMAP) records
    /// a new `GlxDrawable` that wraps an X pixmap. The KMS backend
    /// (`KmsBackendV2`) routes this to `acquire_glx_pixmap_export` which
    /// calls `ensure_exported_entry` (takes the one-time lifetime ref)
    /// and increments `glx_refs`. Non-KMS backends (host-X11,
    /// `RecordingBackend`) keep the default no-op.
    fn acquire_glx_pixmap_export(&mut self, _host_xid: u32) {
        // no-op for backends without GLX-TFP export tracking
    }

    /// Decrement the GLX-consumer refcount on the exported backing for
    /// `host_xid` and tear it down when `glx_refs` reaches 0. Called
    /// from three sites: `glXDestroyPixmap`, the `DESTROY_PIXMAP`
    /// catch-all, and the per-client disconnect cleanup. Non-KMS backends
    /// keep the default no-op.
    fn release_glx_pixmap_export(&mut self, _host_xid: u32) {
        // no-op for backends without GLX-TFP export tracking
    }

    /// GLX-TFP Task 3.5: promote the backing for `host_xid` to
    /// dma-buf-exportable storage (idempotent) WITHOUT touching the
    /// lifetime refcount (`glx_refs`) or allocating/exporting a dmabuf fd.
    ///
    /// Called from the indirect-context `glXBindTexImageEXT` path. The
    /// GLXPixmap's create-time lifetime ref (taken by
    /// `acquire_glx_pixmap_export` at `glXCreatePixmap`) already keeps the
    /// backing alive until `glXDestroyPixmap`; bind only needs the storage
    /// promoted so indirect sampling reads live content. Crucially this is
    /// SEPARATE from the lifetime refcount: compositors call
    /// `BindTexImageEXT` repeatedly (per-frame / on damage, with implicit
    /// rebind), so coupling it to `glx_refs` would leak the backing.
    ///
    /// Returns `true` if the backing is exportable after the call. The KMS
    /// backend (`KmsBackendV2`) routes this to the engine's idempotent
    /// `promote_drawable_exportable` (its `is_exportable()` check
    /// short-circuits a no-op rebind). Non-KMS / `RecordingBackend` default
    /// to `false`.
    fn promote_pixmap_exportable(&mut self, _host_xid: u32) -> bool {
        false
    }

    // ──────────────────────────────────────────────────────────────
    // Other extensions
    // ──────────────────────────────────────────────────────────────

    /// `intern_atom` interns an atom name into the server's atom table
    /// and returns its id. Used to populate `VirtualModNames` in the
    /// XKB GetNames reply (the core loop owns the atom table; the
    /// backend builds the reply, so the interner is threaded through).
    fn xkb_proxy(
        &mut self,
        origin: Option<OriginContext>,
        minor: u8,
        body: &[u8],
        intern_atom: &mut dyn FnMut(&str) -> u32,
    ) -> io::Result<Option<Vec<u8>>>;

    fn xfixes_change_cursor_by_name(
        &mut self,
        origin: Option<OriginContext>,
        host_cursor_xid: u32,
        name_bytes: &[u8],
    ) -> io::Result<()>;

    /// Stage 5 unblock — XFIXES `GetCursorImage` data source for the
    /// active on-screen cursor. Returns the straight-alpha BGRA
    /// pixels + dimensions + hotspot + monotonic serial of the
    /// currently-effective cursor, plus the pointer position. `None`
    /// when no cursor is registered (test fixtures, pre-init).
    ///
    /// Default returns `None` — pre-Stage-5 backends (`ynest`,
    /// `RecordingBackend`) don't track per-cursor records.
    fn get_active_cursor_image(&self) -> Option<ActiveCursorImage> {
        None
    }

    fn set_shape_rectangles(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        kind: u8,
        rects: &[xfixes::RegionRect],
    ) -> io::Result<()>;

    // ──────────────────────────────────────────────────────────────
    // Misc
    // ──────────────────────────────────────────────────────────────

    fn warp_pointer(
        &mut self,
        origin: Option<OriginContext>,
        dst_host_xid: u32,
        dst_x: i16,
        dst_y: i16,
    ) -> io::Result<()>;

    /// Move the logical pointer to root-absolute coordinates,
    /// generating motion/crossing events exactly as if the user had
    /// instantaneously moved the pointer there (X11 §WarpPointer).
    /// The handler resolves the destination window to root coords
    /// (only `ServerState` knows window positions) and calls this
    /// alongside [`Backend::warp_pointer`]. Backends that forward
    /// WarpPointer to a host server keep the default no-op — the host
    /// echoes the resulting motion back as input events. The KMS
    /// backend implements it through its absolute-motion input path,
    /// which updates the tracked cursor and fans out the events.
    /// Without this, WarpPointer was a silent no-op on KMS and every
    /// xts5 event-delivery test (Xlib11 ButtonPress/MotionNotify/…)
    /// pressed buttons at the never-moved pointer position, missing
    /// its test window ("Expected event not received" en masse).
    fn warp_pointer_root(&mut self, _state: &mut ServerState, _x: i32, _y: i32) {}

    fn query_pointer(&mut self, origin: Option<OriginContext>) -> io::Result<PointerPosition>;

    fn list_fonts_proxy(
        &mut self,
        origin: Option<OriginContext>,
        max_names: u16,
        pattern: &str,
    ) -> io::Result<Vec<u8>>;

    /// `intern_atom` lets the backend attach font properties whose
    /// values are atoms — most importantly `FONT` (XA_FONT=18)
    /// carrying the resolved XLFD name. libX11's `XCreateFontSet`
    /// resolves non-XLFD base names ("fixed") EXCLUSIVELY through the
    /// first reply's FONT property (`omGeneric.c get_prop_name` →
    /// `XGetFontProperty(fs, XA_FONT)` + `XGetAtomName`); without it
    /// the locale charset reports missing and the fontset is NULL.
    fn list_fonts_with_info_proxy(
        &mut self,
        origin: Option<OriginContext>,
        max_names: u16,
        pattern: &str,
        intern_atom: &mut dyn FnMut(&str) -> u32,
    ) -> io::Result<Vec<Vec<u8>>>;

    fn get_atom_name(
        &mut self,
        origin: Option<OriginContext>,
        atom: u32,
    ) -> io::Result<Option<String>>;

    fn get_keyboard_mapping(
        &mut self,
        origin: Option<OriginContext>,
        first_keycode: u8,
        count: u8,
    ) -> io::Result<(u8, Vec<u32>)>;

    fn get_modifier_mapping(&mut self, origin: Option<OriginContext>) -> io::Result<(u8, Vec<u8>)>;
}

#[cfg(test)]
mod tests {
    use super::BackendFdKind;

    #[test]
    fn present_completion_kind_distinct_from_existing_kinds() {
        // Sanity: the new variant exists and isn't accidentally
        // aliased to an existing one.
        assert_ne!(BackendFdKind::PresentCompletion, BackendFdKind::Libinput);
        assert_ne!(BackendFdKind::PresentCompletion, BackendFdKind::Drm);
        assert_ne!(BackendFdKind::PresentCompletion, BackendFdKind::HostX11);
        assert_ne!(BackendFdKind::Seat, BackendFdKind::Libinput);
        assert_ne!(BackendFdKind::Seat, BackendFdKind::Drm);
        assert_ne!(BackendFdKind::Seat, BackendFdKind::HostX11);
        assert_ne!(BackendFdKind::Seat, BackendFdKind::PresentCompletion);
    }
}

// Compile-time assertion that `Backend` is object-safe and that the
// `Arc<Mutex<dyn Backend>>` shape used by the hot-path call sites is
// `Send + Sync` (so worker threads can hold it).
const _: fn() = || {
    fn assert_obj_safe(_: &dyn Backend) {}
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<std::sync::Arc<std::sync::Mutex<dyn Backend>>>();
    let _ = assert_obj_safe;
};

#[cfg(test)]
mod present_completion_trait_tests {
    use super::*;

    // Use the existing RecordingBackend to verify default trait impls
    // compile + behave as expected.

    #[test]
    fn default_drain_completed_present_events_returns_empty() {
        let mut backend = crate::backend::recording::RecordingBackend::new();
        let events = backend.drain_completed_present_events();
        assert!(events.is_empty());
    }

    #[test]
    fn default_enqueue_present_completion_is_noop() {
        let mut backend = crate::backend::recording::RecordingBackend::new();
        backend.enqueue_present_completion(
            CompletedPresentEvent {
                client_id: yserver_protocol::x11::ClientId(0),
                serial: 1,
                host_xid: 0,
                dst_host_xid: 0,
                options: 0,
                wake: PresentWake::Pixmap { idle_fence_xid: 0 },
            },
            /* dst_host_xid */ 0,
        );
        // No drain triggered; default impl swallows.
        assert!(backend.drain_completed_present_events().is_empty());
    }
}
