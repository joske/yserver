//! `RecordingBackend` — test double for the `Backend` trait. Records
//! every method call into a per-instance log so unit tests can assert
//! the exact host-side request sequence produced by a `nested.rs`
//! request-handler hot-path.
//!
//! Methods that the existing tests don't exercise are
//! `unimplemented!()` — calling them in a test fails loudly. Adding a
//! new test that drives one is the cheap path: implement the recorder
//! variant + impl block inline.
//!
//! The methods we DO implement are picked to cover the
//! CreateWindow → MapWindow → DestroyWindow lifecycle (Phase 3.6
//! invariant: every InputOutput sub-window goes through host
//! create/map/destroy) plus the helpers needed to make the lifecycle
//! tests run end-to-end (`window_id` so `nested::run` can resolve
//! ROOT_WINDOW's host xid; `set_container_background_pixel` because
//! `nested::handle_request`'s ChangeWindowAttributes path on
//! ROOT_WINDOW pokes the container).

#![cfg(test)]

use std::{io, sync::Mutex};

use yserver_protocol::x11::{ClipRectangles, FontMetrics, ResourceId, xfixes};

use crate::{
    backend::{
        AnyHandle, Backend, ClipState, CursorHandle, DrawState, FillState, FontHandle,
        GlyphSetHandle, OriginContext, PictureHandle, PixmapHandle, WindowHandle,
    },
    host_x11::{HostSubwindowConfig, HostSubwindowVisual, HostXidMap, PointerPosition},
};

/// Records each method call. Variants are added on demand; tests
/// assert against `Vec<RecordedCall>` snapshots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordedCall {
    CreateSubwindow {
        parent: u32,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        border_width: u16,
        background_pixel: Option<u32>,
        background_pixmap: Option<u32>,
    },
    DestroySubwindow(u32),
    MapSubwindow(u32),
    UnmapSubwindow(u32),
    ConfigureSubwindow {
        host_xid: u32,
        config: HostSubwindowConfig,
    },
    ReparentSubwindow {
        host_xid: u32,
        host_parent: u32,
        x: i16,
        y: i16,
    },
    ChangeSubwindowAttributes {
        host_xid: u32,
        value_mask: u32,
        values: Vec<u32>,
    },
    UpdateHostEventMask {
        host_xid: u32,
        mask: u32,
        enabled: bool,
    },
    RegisterTopLevel {
        nested_id: ResourceId,
        host_xid: u32,
    },
    RegisterSubwindow {
        nested_id: ResourceId,
        host_xid: u32,
    },
    UnregisterHostWindow(u32),
    CreatePixmap {
        depth: u8,
        width: u16,
        height: u16,
    },
    FreePixmap(u32),
    SetContainerBackgroundPixel(u32),
    SetContainerBackgroundPixmap(u32),
    OpenFont(String),
    CloseFont(u32),
    Ping,
    ReleaseRedirectedBacking(u32),
    RetainBackingStorage(u32),
    DropBackingStorage(u32),
    AllocateRedirectedBacking {
        host_window: u32,
        width: u16,
        height: u16,
        depth: u8,
    },
    SetWindowSceneParticipation {
        host_window: u32,
        participating: bool,
    },
    SetBackingSceneParticipation {
        backing: u32,
        participating: bool,
    },
    CopyArea {
        src_host_xid: u32,
        dst_host_xid: u32,
        src_x: i16,
        src_y: i16,
        dst_x: i16,
        dst_y: i16,
        width: u16,
        height: u16,
    },
    DefineCursor {
        host_window_xid: u32,
        cursor_host_xid: u32,
    },
    SetDpmsPower(u8),
    /// GLX-TFP Task 3.4: `acquire_glx_pixmap_export(host_xid)` called.
    AcquireGlxPixmapExport(u32),
    /// GLX-TFP Task 3.4: `release_glx_pixmap_export(host_xid)` called.
    ReleaseGlxPixmapExport(u32),
    /// GLX-TFP Task 3.5: `promote_pixmap_exportable(host_xid)` called
    /// (the lightweight bind hook — does NOT touch the lifetime refcount).
    PromotePixmapExportable(u32),
}

/// Test double for `Backend`. Auto-allocates host xids from a private
/// counter so create-then-destroy round trips read back the same xid.
pub struct RecordingBackend {
    pub calls: Mutex<Vec<RecordedCall>>,
    next_handle: Mutex<u32>,
    fake_window_id: u32,
    fake_root_visual_xid: u32,
    /// Phase 6.3 Step 4: shared `host_xid → ResourceId` map exposed
    /// through `Backend::xid_map`. Tests inspect it via `Backend`'s
    /// trait surface.
    xid_map: HostXidMap,
    /// E3 liveness counter — incremented every time
    /// `on_page_flip_ready` is invoked. Tests assert back-to-back
    /// PageFlipReady dispatches do not get suppressed by the run_core
    /// dispatch loop.
    pub page_flip_count: std::sync::atomic::AtomicU32,
    /// Counter — incremented every time `before_block` is invoked. Tests
    /// assert the core loop drives per-iteration reclamation even when no
    /// page-flip ever occurs (project_reclamation_starvation_leak).
    pub before_block_count: std::sync::atomic::AtomicU32,
    /// Stage 4d COW: lets tests pretend this backend tracks COW
    /// lifecycle. When true, the next `release_overlay_window` call
    /// returns `Ok(true)` (final release, COW destroyed); otherwise
    /// the default `Ok(false)` no-op semantics apply. Reset to false
    /// after consumed. Plain `bool` not `AtomicBool` — `Backend`
    /// methods take `&mut self`, so the test thread already has
    /// exclusive access.
    pub cow_next_release_is_final: bool,
    /// Stage 4e COW: tracks whether `get_overlay_window` has
    /// materialised the COW (refcount > 0) so the override can
    /// signal the 0→1 transition to the core handler. Mirrors
    /// `KmsBackendV2`'s `core.cow_refcount`-based logic; the
    /// `RecordingBackend` doesn't own GPU storage so a plain bool
    /// suffices. Reset by `release_overlay_window` on the
    /// final-release branch (controlled by
    /// `cow_next_release_is_final`).
    pub cow_materialized: bool,
    /// Phase 2 (reparent reconciliation): lets tests opt in to
    /// claiming `supports_redirect_activation = true` so the
    /// production reconciliation block in `handle_reparent_window`
    /// (gated on the trait method) actually runs. Default `false`
    /// matches the trait default — v1 / host-X11 semantics.
    pub redirect_activation_supported: bool,
    /// KeyButMask returned by `query_pointer` (lets tests model a held
    /// pointer button — e.g. `Button1Mask = 0x0100` — so the
    /// XIQueryPointer reply's button state can be asserted).
    pub query_pointer_mask: u16,
    /// Toggled by tests that want to exercise the ynest path
    /// (kms_capable=false) — default true.
    pub dpms_capable: bool,
    /// When set, `set_dpms_power` returns Err; tests assert the
    /// transition helper advances state anyway.
    pub dpms_set_returns_err: bool,
    /// Startup input-probe model. Each inner `Vec` is one "dispatch
    /// round" the fake libinput would yield; `probe_input_devices`
    /// consumes the front round per iteration and seeds the registry,
    /// mirroring the KMS backend's bounded drain (stop after two
    /// consecutive empty rounds or `PROBE_MAX_ROUNDS`). Empty by
    /// default → the override is a no-op returning 0, matching the
    /// trait default for backends with no on-core libinput.
    pub probe_rounds: std::collections::VecDeque<Vec<crate::core_loop::DeviceInfo>>,
    /// Number of dispatch rounds `probe_input_devices` actually ran —
    /// lets tests assert the bounded loop terminated rather than
    /// spinning to the ceiling.
    pub probe_rounds_run: std::cell::Cell<usize>,
}

impl Default for RecordingBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl RecordingBackend {
    pub fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            next_handle: Mutex::new(0x0001_0000),
            fake_window_id: 0x0000_0100,
            fake_root_visual_xid: 0x0000_0021,
            xid_map: HostXidMap::new(),
            page_flip_count: std::sync::atomic::AtomicU32::new(0),
            before_block_count: std::sync::atomic::AtomicU32::new(0),
            cow_next_release_is_final: false,
            cow_materialized: false,
            redirect_activation_supported: false,
            query_pointer_mask: 0,
            dpms_capable: true,
            dpms_set_returns_err: false,
            probe_rounds: std::collections::VecDeque::new(),
            probe_rounds_run: std::cell::Cell::new(0),
        }
    }

    /// Phase 2: opt in to claiming
    /// `supports_redirect_activation = true`. Used by tests that
    /// exercise the reparent-redirect-reconciliation path
    /// (`handle_reparent_window` gates its reconciliation block
    /// on `backend.supports_redirect_activation()`).
    #[must_use]
    pub fn with_redirect_activation(mut self) -> Self {
        self.redirect_activation_supported = true;
        self
    }

    pub fn calls(&self) -> Vec<RecordedCall> {
        self.calls.lock().unwrap().clone()
    }

    fn record(&self, call: RecordedCall) {
        self.calls.lock().unwrap().push(call);
    }

    fn allocate_handle(&self) -> u32 {
        let mut n = self.next_handle.lock().unwrap();
        let h = *n;
        *n = n.wrapping_add(1);
        h
    }
}

impl Backend for RecordingBackend {
    // State accessors — return fixed sentinels so the call sites that
    // need a real number get a real number; record nothing.

    fn window_id(&self) -> u32 {
        self.fake_window_id
    }

    fn root_visual_xid(&self) -> u32 {
        self.fake_root_visual_xid
    }

    fn argb_visual_xid(&self) -> Option<u32> {
        None
    }

    fn argb_colormap_xid(&self) -> Option<u32> {
        None
    }

    fn render_opcode(&self) -> Option<u8> {
        None
    }

    fn xkb_opcode(&self) -> Option<u8> {
        None
    }

    fn xkb_info(&self) -> Option<(u8, u8, u8)> {
        None
    }

    fn composite_opcode(&self) -> Option<u8> {
        None
    }

    fn supports_redirect_activation(&self) -> bool {
        self.redirect_activation_supported
    }

    fn render_format_for_ynest_id(&self, _ynest_fmt: u32) -> Option<u32> {
        None
    }

    fn ping(&mut self, _origin: Option<OriginContext>) -> io::Result<()> {
        self.record(RecordedCall::Ping);
        Ok(())
    }

    fn on_host_input(
        &mut self,
        _state: &mut crate::server::ServerState,
        _ev: crate::core_loop::HostInputEvent,
    ) {
    }

    fn on_page_flip_ready(&mut self, _state: &mut crate::server::ServerState) {
        self.page_flip_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    fn before_block(&mut self) {
        self.before_block_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    fn probe_input_devices(&mut self, state: &mut crate::server::ServerState) -> usize {
        // Mirror the KMS backend's bounded-drain contract over the
        // test-configured `probe_rounds`: at most `PROBE_MAX_ROUNDS`
        // iterations, stop after two consecutive empty rounds, never
        // block. With no rounds configured this returns 0 immediately,
        // matching the trait default for a backend with no on-core
        // libinput.
        const PROBE_MAX_ROUNDS: usize = 8;
        let mut seeded = 0usize;
        let mut empty_rounds = 0usize;
        let mut rounds_run = 0usize;
        for _ in 0..PROBE_MAX_ROUNDS {
            rounds_run += 1;
            let batch = self.probe_rounds.pop_front().unwrap_or_default();
            if batch.is_empty() {
                empty_rounds += 1;
                if empty_rounds >= 2 {
                    break;
                }
                continue;
            }
            empty_rounds = 0;
            for info in batch {
                seeded += 1;
                state.xi_seed_touchpad(&info);
            }
        }
        self.probe_rounds_run.set(rounds_run);
        seeded
    }

    fn poll_fds(&self) -> Vec<(std::os::fd::RawFd, crate::backend::BackendFdKind)> {
        Vec::new()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    // Subwindow lifecycle

    fn create_subwindow(
        &mut self,
        _origin: Option<OriginContext>,
        host_parent: WindowHandle,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        border_width: u16,
        _visual: HostSubwindowVisual,
        background_pixel: Option<u32>,
        background_pixmap: Option<u32>,
    ) -> io::Result<WindowHandle> {
        let xid = self.allocate_handle();
        self.record(RecordedCall::CreateSubwindow {
            parent: host_parent.as_raw(),
            x,
            y,
            width,
            height,
            border_width,
            background_pixel,
            background_pixmap,
        });
        Ok(WindowHandle::from_raw_panicking(xid))
    }

    fn destroy_subwindow(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
    ) -> io::Result<()> {
        self.record(RecordedCall::DestroySubwindow(host_xid));
        Ok(())
    }

    fn map_subwindow(&mut self, _origin: Option<OriginContext>, host_xid: u32) -> io::Result<()> {
        self.record(RecordedCall::MapSubwindow(host_xid));
        Ok(())
    }

    fn unmap_subwindow(&mut self, _origin: Option<OriginContext>, host_xid: u32) -> io::Result<()> {
        self.record(RecordedCall::UnmapSubwindow(host_xid));
        Ok(())
    }

    fn configure_subwindow(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        config: HostSubwindowConfig,
    ) -> io::Result<()> {
        self.record(RecordedCall::ConfigureSubwindow { host_xid, config });
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
        self.record(RecordedCall::ReparentSubwindow {
            host_xid,
            host_parent,
            x,
            y,
        });
        Ok(())
    }

    fn change_subwindow_attributes(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        value_mask: u32,
        values: &[u32],
    ) -> io::Result<()> {
        self.record(RecordedCall::ChangeSubwindowAttributes {
            host_xid,
            value_mask,
            values: values.to_vec(),
        });
        Ok(())
    }

    fn update_host_event_mask(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        mask: u32,
        enabled: bool,
    ) -> io::Result<()> {
        self.record(RecordedCall::UpdateHostEventMask {
            host_xid,
            mask,
            enabled,
        });
        Ok(())
    }

    fn register_top_level(
        &mut self,
        _origin: Option<OriginContext>,
        nested_id: ResourceId,
        host_xid: u32,
    ) -> io::Result<()> {
        self.xid_map.insert(host_xid, nested_id);
        self.record(RecordedCall::RegisterTopLevel {
            nested_id,
            host_xid,
        });
        Ok(())
    }

    fn register_subwindow(
        &mut self,
        _origin: Option<OriginContext>,
        nested_id: ResourceId,
        host_xid: u32,
    ) -> io::Result<()> {
        self.xid_map.insert(host_xid, nested_id);
        self.record(RecordedCall::RegisterSubwindow {
            nested_id,
            host_xid,
        });
        Ok(())
    }

    fn unregister_host_window(&mut self, host_xid: u32) {
        self.xid_map.remove(&host_xid);
        self.record(RecordedCall::UnregisterHostWindow(host_xid));
    }

    fn allocate_redirected_backing(
        &mut self,
        _origin: Option<OriginContext>,
        host_window: WindowHandle,
        width: u16,
        height: u16,
        depth: u8,
    ) -> io::Result<PixmapHandle> {
        let xid = self.allocate_handle();
        self.record(RecordedCall::AllocateRedirectedBacking {
            host_window: host_window.as_raw(),
            width,
            height,
            depth,
        });
        Ok(PixmapHandle::from_raw_panicking(xid))
    }

    fn release_redirected_backing(
        &mut self,
        _origin: Option<OriginContext>,
        backing: PixmapHandle,
    ) -> io::Result<()> {
        self.record(RecordedCall::ReleaseRedirectedBacking(backing.as_raw()));
        Ok(())
    }

    fn retain_backing_storage(
        &mut self,
        _origin: Option<OriginContext>,
        backing: PixmapHandle,
    ) -> io::Result<()> {
        self.record(RecordedCall::RetainBackingStorage(backing.as_raw()));
        Ok(())
    }

    fn drop_backing_storage(
        &mut self,
        _origin: Option<OriginContext>,
        backing: PixmapHandle,
    ) -> io::Result<()> {
        self.record(RecordedCall::DropBackingStorage(backing.as_raw()));
        Ok(())
    }

    fn set_window_scene_participation(
        &mut self,
        _origin: Option<OriginContext>,
        host_window: WindowHandle,
        participating: bool,
    ) -> io::Result<()> {
        self.record(RecordedCall::SetWindowSceneParticipation {
            host_window: host_window.as_raw(),
            participating,
        });
        Ok(())
    }

    fn set_backing_scene_participation(
        &mut self,
        _origin: Option<OriginContext>,
        backing: PixmapHandle,
        participating: bool,
    ) -> io::Result<()> {
        self.record(RecordedCall::SetBackingSceneParticipation {
            backing: backing.as_raw(),
            participating,
        });
        Ok(())
    }

    fn xid_map(&self) -> &HostXidMap {
        &self.xid_map
    }

    fn name_window_pixmap(
        &mut self,
        _origin: Option<OriginContext>,
        _host_window: WindowHandle,
    ) -> io::Result<PixmapHandle> {
        unimplemented!("RecordingBackend: name_window_pixmap not implemented for the current tests")
    }

    fn create_pixmap(
        &mut self,
        _origin: Option<OriginContext>,
        depth: u8,
        width: u16,
        height: u16,
    ) -> io::Result<PixmapHandle> {
        let xid = self.allocate_handle();
        self.record(RecordedCall::CreatePixmap {
            depth,
            width,
            height,
        });
        Ok(PixmapHandle::from_raw_panicking(xid))
    }

    fn free_pixmap(&mut self, _origin: Option<OriginContext>, host_xid: u32) -> io::Result<()> {
        self.record(RecordedCall::FreePixmap(host_xid));
        Ok(())
    }

    fn open_font(
        &mut self,
        _origin: Option<OriginContext>,
        name: &str,
    ) -> io::Result<(FontHandle, FontMetrics)> {
        let xid = self.allocate_handle();
        self.record(RecordedCall::OpenFont(name.to_string()));
        // FontMetrics is private to the protocol crate; return a Default-ish
        // value via Default::default(). If FontMetrics has no Default we fall
        // back to a zero-initialised one in the unimplemented branch below.
        Ok((FontHandle::from_raw_panicking(xid), FontMetrics::default()))
    }

    fn close_font(&mut self, _origin: Option<OriginContext>, host_xid: u32) -> io::Result<()> {
        self.record(RecordedCall::CloseFont(host_xid));
        Ok(())
    }

    fn create_cursor(
        &mut self,
        _origin: Option<OriginContext>,
        _source_pixmap: PixmapHandle,
        _mask_pixmap: Option<PixmapHandle>,
        _fore: (u16, u16, u16),
        _back: (u16, u16, u16),
        _hot_x: u16,
        _hot_y: u16,
    ) -> io::Result<CursorHandle> {
        let xid = self.allocate_handle();
        Ok(CursorHandle::from_raw_panicking(xid))
    }

    fn create_glyph_cursor(
        &mut self,
        _origin: Option<OriginContext>,
        _source_font: FontHandle,
        _mask_font: Option<FontHandle>,
        _source_char: u16,
        _mask_char: u16,
        _fore: (u16, u16, u16),
        _back: (u16, u16, u16),
    ) -> io::Result<CursorHandle> {
        let xid = self.allocate_handle();
        Ok(CursorHandle::from_raw_panicking(xid))
    }

    fn define_cursor(
        &mut self,
        _origin: Option<OriginContext>,
        host_window_xid: u32,
        cursor_host_xid: u32,
    ) -> io::Result<()> {
        self.record(RecordedCall::DefineCursor {
            host_window_xid,
            cursor_host_xid,
        });
        Ok(())
    }

    fn set_container_background_pixel(
        &mut self,
        _origin: Option<OriginContext>,
        pixel: u32,
    ) -> io::Result<()> {
        self.record(RecordedCall::SetContainerBackgroundPixel(pixel));
        Ok(())
    }

    fn set_container_background_pixmap(
        &mut self,
        _origin: Option<OriginContext>,
        host_pixmap_xid: u32,
    ) -> io::Result<()> {
        self.record(RecordedCall::SetContainerBackgroundPixmap(host_pixmap_xid));
        Ok(())
    }

    // GC state — silently no-op for tests that drive lifecycle paths.

    fn clear_clip_rectangles(&mut self, _origin: Option<OriginContext>) -> io::Result<()> {
        Ok(())
    }

    fn set_clip_rectangles(
        &mut self,
        _origin: Option<OriginContext>,
        _clip: Option<ClipRectangles>,
    ) -> io::Result<()> {
        Ok(())
    }

    fn set_clip_pixmap(
        &mut self,
        _origin: Option<OriginContext>,
        _host_pixmap: u32,
        _clip_x_origin: i16,
        _clip_y_origin: i16,
    ) -> io::Result<()> {
        Ok(())
    }

    fn set_gc_fill_solid(&mut self, _origin: Option<OriginContext>) -> io::Result<()> {
        Ok(())
    }

    fn set_gc_fill_tiled(
        &mut self,
        _origin: Option<OriginContext>,
        _host_pixmap: u32,
        _tile_x_origin: i16,
        _tile_y_origin: i16,
    ) -> io::Result<()> {
        Ok(())
    }

    fn apply_clip_state(
        &mut self,
        _origin: Option<OriginContext>,
        _clip: &ClipState,
    ) -> io::Result<()> {
        Ok(())
    }

    fn apply_fill_state(
        &mut self,
        _origin: Option<OriginContext>,
        _fill: &FillState,
    ) -> io::Result<()> {
        Ok(())
    }

    fn apply_draw_state(
        &mut self,
        _origin: Option<OriginContext>,
        _state: &DrawState,
    ) -> io::Result<()> {
        Ok(())
    }

    // Drawing primitives — `unimplemented!()` so a test that
    // accidentally drives a draw path will surface loudly. Add an
    // implementation when adding a draw-path test.

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
        self.record(RecordedCall::CopyArea {
            src_host_xid,
            dst_host_xid,
            src_x,
            src_y,
            dst_x,
            dst_y,
            width,
            height,
        });
        Ok(())
    }

    fn copy_plane(
        &mut self,
        _origin: Option<OriginContext>,
        _src_host_xid: u32,
        _dst_host_xid: u32,
        _src_x: i16,
        _src_y: i16,
        _dst_x: i16,
        _dst_y: i16,
        _width: u16,
        _height: u16,
        _plane: u32,
    ) -> io::Result<()> {
        unimplemented!("RecordingBackend: copy_plane")
    }

    fn put_image(
        &mut self,
        _origin: Option<OriginContext>,
        _host_xid: u32,
        _depth: u8,
        _width: u16,
        _height: u16,
        _dst_x: i16,
        _dst_y: i16,
        _data: &[u8],
    ) -> io::Result<()> {
        unimplemented!("RecordingBackend: put_image")
    }

    fn get_image(
        &mut self,
        _origin: Option<OriginContext>,
        _host_xid: u32,
        _format: u8,
        _x: i16,
        _y: i16,
        _width: u16,
        _height: u16,
        _plane_mask: u32,
    ) -> io::Result<Option<Vec<u8>>> {
        Ok(None)
    }

    fn poly_line(
        &mut self,
        _origin: Option<OriginContext>,
        _host_xid: u32,
        _foreground: u32,
        _coordinate_mode: u8,
        _points: &[u8],
    ) -> io::Result<()> {
        unimplemented!("RecordingBackend: poly_line")
    }

    fn poly_segment(
        &mut self,
        _origin: Option<OriginContext>,
        _host_xid: u32,
        _foreground: u32,
        _segments: &[u8],
    ) -> io::Result<()> {
        unimplemented!("RecordingBackend: poly_segment")
    }

    fn poly_rectangle(
        &mut self,
        _origin: Option<OriginContext>,
        _host_xid: u32,
        _foreground: u32,
        _rectangles: &[u8],
    ) -> io::Result<()> {
        unimplemented!("RecordingBackend: poly_rectangle")
    }

    fn poly_arc(
        &mut self,
        _origin: Option<OriginContext>,
        _host_xid: u32,
        _foreground: u32,
        _arcs: &[u8],
    ) -> io::Result<()> {
        unimplemented!("RecordingBackend: poly_arc")
    }

    fn poly_point(
        &mut self,
        _origin: Option<OriginContext>,
        _host_xid: u32,
        _foreground: u32,
        _coordinate_mode: u8,
        _points: &[u8],
    ) -> io::Result<()> {
        unimplemented!("RecordingBackend: poly_point")
    }

    fn poly_fill_rectangle(
        &mut self,
        _origin: Option<OriginContext>,
        _host_xid: u32,
        _foreground: u32,
        _rectangles: &[u8],
    ) -> io::Result<()> {
        unimplemented!("RecordingBackend: poly_fill_rectangle")
    }

    fn poly_fill_arc(
        &mut self,
        _origin: Option<OriginContext>,
        _host_xid: u32,
        _foreground: u32,
        _arcs: &[u8],
    ) -> io::Result<()> {
        unimplemented!("RecordingBackend: poly_fill_arc")
    }

    fn fill_poly(
        &mut self,
        _origin: Option<OriginContext>,
        _host_xid: u32,
        _foreground: u32,
        _coord_mode: u8,
        _points: &[u8],
    ) -> io::Result<()> {
        unimplemented!("RecordingBackend: fill_poly")
    }

    fn fill_rectangle(
        &mut self,
        _origin: Option<OriginContext>,
        _host_xid: u32,
        _foreground: u32,
        _x: i16,
        _y: i16,
        _width: u16,
        _height: u16,
    ) -> io::Result<()> {
        unimplemented!("RecordingBackend: fill_rectangle")
    }

    fn poly_text8(
        &mut self,
        _origin: Option<OriginContext>,
        _host_xid: u32,
        _foreground: u32,
        _body: &[u8],
    ) -> io::Result<()> {
        unimplemented!("RecordingBackend: poly_text8")
    }

    fn poly_text16(
        &mut self,
        _origin: Option<OriginContext>,
        _host_xid: u32,
        _foreground: u32,
        _body: &[u8],
    ) -> io::Result<()> {
        unimplemented!("RecordingBackend: poly_text16")
    }

    fn image_text8(
        &mut self,
        _origin: Option<OriginContext>,
        _host_xid: u32,
        _foreground: u32,
        _background: u32,
        _text_len: u8,
        _body: &[u8],
    ) -> io::Result<()> {
        unimplemented!("RecordingBackend: image_text8")
    }

    fn image_text16(
        &mut self,
        _origin: Option<OriginContext>,
        _host_xid: u32,
        _foreground: u32,
        _background: u32,
        _text_len: u8,
        _body: &[u8],
    ) -> io::Result<()> {
        unimplemented!("RecordingBackend: image_text16")
    }

    // RENDER — `unimplemented!()`; render_opcode() returns None so call
    // sites fast-path out before reaching these.

    fn render_create_picture(
        &mut self,
        _origin: Option<OriginContext>,
        _host_drawable: AnyHandle,
        _ynest_format: u32,
        _value_mask: u32,
        _values: &[u8],
    ) -> io::Result<Option<PictureHandle>> {
        Ok(None)
    }

    fn render_change_picture(
        &mut self,
        _origin: Option<OriginContext>,
        _host_pic: u32,
        _body: &[u8],
    ) -> io::Result<()> {
        Ok(())
    }

    fn render_free_picture(
        &mut self,
        _origin: Option<OriginContext>,
        _host_pic: u32,
    ) -> io::Result<()> {
        Ok(())
    }

    fn render_create_glyphset(
        &mut self,
        _origin: Option<OriginContext>,
        _ynest_format: u32,
    ) -> io::Result<Option<GlyphSetHandle>> {
        Ok(None)
    }

    fn render_free_glyphset(
        &mut self,
        _origin: Option<OriginContext>,
        _host_gs: u32,
    ) -> io::Result<()> {
        Ok(())
    }

    fn render_add_glyphs(
        &mut self,
        _origin: Option<OriginContext>,
        _host_gs: u32,
        _body_tail: &[u8],
    ) -> io::Result<()> {
        Ok(())
    }

    fn render_free_glyphs(
        &mut self,
        _origin: Option<OriginContext>,
        _host_gs: u32,
        _glyph_ids: &[u8],
    ) -> io::Result<()> {
        Ok(())
    }

    fn render_composite(
        &mut self,
        _origin: Option<OriginContext>,
        _op: u8,
        _host_src: u32,
        _host_mask: u32,
        _host_dst: u32,
        _src_x: i16,
        _src_y: i16,
        _mask_x: i16,
        _mask_y: i16,
        _dst_x: i16,
        _dst_y: i16,
        _width: u16,
        _height: u16,
    ) -> io::Result<()> {
        Ok(())
    }

    fn render_composite_glyphs(
        &mut self,
        _origin: Option<OriginContext>,
        _minor: u8,
        _op: u8,
        _host_src: u32,
        _host_dst: u32,
        _mask_fmt: u32,
        _host_gs: u32,
        _src_x: i16,
        _src_y: i16,
        _items: &[u8],
        _x_off: i16,
        _y_off: i16,
    ) -> io::Result<()> {
        Ok(())
    }

    fn render_fill_rectangles(
        &mut self,
        _origin: Option<OriginContext>,
        _host_dst: u32,
        _op: u8,
        _color: [u8; 8],
        _rects: &[u8],
        _x_off: i16,
        _y_off: i16,
    ) -> io::Result<()> {
        Ok(())
    }

    fn render_trapezoids(
        &mut self,
        _origin: Option<OriginContext>,
        _op: u8,
        _host_src: u32,
        _host_dst: u32,
        _host_mask_format: u32,
        _src_x: i16,
        _src_y: i16,
        _traps: &[u8],
        _x_off: i16,
        _y_off: i16,
    ) -> io::Result<()> {
        Ok(())
    }

    fn render_create_solid_fill(
        &mut self,
        _origin: Option<OriginContext>,
        _color: [u8; 8],
    ) -> io::Result<Option<PictureHandle>> {
        Ok(None)
    }

    fn render_create_linear_gradient(
        &mut self,
        _origin: Option<OriginContext>,
        _body: &[u8],
    ) -> io::Result<Option<PictureHandle>> {
        Ok(None)
    }

    fn render_create_radial_gradient(
        &mut self,
        _origin: Option<OriginContext>,
        _body: &[u8],
    ) -> io::Result<Option<PictureHandle>> {
        Ok(None)
    }

    fn render_create_cursor(
        &mut self,
        _origin: Option<OriginContext>,
        _host_src_pic: PictureHandle,
        _x: u16,
        _y: u16,
    ) -> io::Result<Option<CursorHandle>> {
        Ok(None)
    }

    fn render_set_picture_clip_rectangles(
        &mut self,
        _origin: Option<OriginContext>,
        _host_pic: u32,
        _body: &[u8],
    ) -> io::Result<()> {
        Ok(())
    }

    fn render_set_picture_filter(
        &mut self,
        _origin: Option<OriginContext>,
        _host_pic: u32,
        _body: &[u8],
    ) -> io::Result<()> {
        Ok(())
    }

    fn render_set_picture_transform(
        &mut self,
        _origin: Option<OriginContext>,
        _host_pic: u32,
        _body: &[u8],
    ) -> io::Result<()> {
        Ok(())
    }

    fn render_query_version(&mut self, _origin: Option<OriginContext>) -> io::Result<(u32, u32)> {
        Ok((0, 11))
    }

    fn xkb_proxy(
        &mut self,
        _origin: Option<OriginContext>,
        _minor: u8,
        _body: &[u8],
        _intern_atom: &mut dyn FnMut(&str) -> u32,
    ) -> io::Result<Option<Vec<u8>>> {
        Ok(None)
    }

    fn xfixes_change_cursor_by_name(
        &mut self,
        _origin: Option<OriginContext>,
        _host_cursor_xid: u32,
        _name_bytes: &[u8],
    ) -> io::Result<()> {
        Ok(())
    }

    fn set_shape_rectangles(
        &mut self,
        _origin: Option<OriginContext>,
        _host_xid: u32,
        _kind: u8,
        _rects: &[xfixes::RegionRect],
    ) -> io::Result<()> {
        Ok(())
    }

    fn warp_pointer(
        &mut self,
        _origin: Option<OriginContext>,
        _dst_host_xid: u32,
        _dst_x: i16,
        _dst_y: i16,
    ) -> io::Result<()> {
        Ok(())
    }

    fn query_pointer(&mut self, _origin: Option<OriginContext>) -> io::Result<PointerPosition> {
        Ok(PointerPosition {
            same_screen: true,
            win_x: 0,
            win_y: 0,
            mask: self.query_pointer_mask,
        })
    }

    fn list_fonts_proxy(
        &mut self,
        _origin: Option<OriginContext>,
        _max_names: u16,
        _pattern: &str,
    ) -> io::Result<Vec<u8>> {
        // 32-byte stub reply header that downstream parsers can ignore.
        Ok(vec![0u8; 32])
    }

    fn list_fonts_with_info_proxy(
        &mut self,
        _origin: Option<OriginContext>,
        _max_names: u16,
        _pattern: &str,
        _intern_atom: &mut dyn FnMut(&str) -> u32,
    ) -> io::Result<Vec<Vec<u8>>> {
        Ok(Vec::new())
    }

    fn get_atom_name(
        &mut self,
        _origin: Option<OriginContext>,
        _atom: u32,
    ) -> io::Result<Option<String>> {
        Ok(None)
    }

    fn get_keyboard_mapping(
        &mut self,
        _origin: Option<OriginContext>,
        _first_keycode: u8,
        count: u8,
    ) -> io::Result<(u8, Vec<u32>)> {
        // Two keysyms per code, all set to NoSymbol.
        Ok((2, vec![0; usize::from(count) * 2]))
    }

    fn get_modifier_mapping(
        &mut self,
        _origin: Option<OriginContext>,
    ) -> io::Result<(u8, Vec<u8>)> {
        Ok((0, Vec::new()))
    }

    /// Stage 4e COW: override to model the 0→1 transition so the
    /// core handler can drive `materialize_cow_resource`. Returns
    /// `Ok(true)` on first claim (cow_materialized was false),
    /// `Ok(false)` on subsequent claims. Mirrors `KmsBackendV2`'s
    /// semantics — single backend hook owns the full COW lifecycle.
    fn get_overlay_window(&mut self, _origin: Option<OriginContext>) -> io::Result<bool> {
        if self.cow_materialized {
            return Ok(false);
        }
        self.cow_materialized = true;
        Ok(true)
    }

    /// Stage 4e COW: return the well-known COW host xid while
    /// materialised. The core handler reads this to populate the
    /// resources COW record's `host_xid` after `get_overlay_window`'s
    /// 0→1 return.
    fn cow_host_xid(&self) -> Option<u32> {
        if self.cow_materialized {
            Some(crate::resources::COMPOSITE_OVERLAY_WINDOW.0)
        } else {
            None
        }
    }

    /// Stage 4d COW: override only to honor the
    /// `cow_next_release_is_final` knob set by tests. Default trait
    /// impl returns `Ok(false)` ("I didn't destroy anything"); tests
    /// that exercise the handler-side teardown path flip the knob
    /// first. On final release also clears `cow_materialized` so
    /// `cow_host_xid` reverts to `None` and the next
    /// `get_overlay_window` re-signals a 0→1 transition.
    fn release_overlay_window(&mut self, _origin: Option<OriginContext>) -> io::Result<bool> {
        let final_release = self.cow_next_release_is_final;
        self.cow_next_release_is_final = false;
        if final_release {
            self.cow_materialized = false;
        }
        Ok(final_release)
    }

    fn dpms_capable(&self) -> bool {
        // Test default: pretend we can drive DPMS so tests can
        // exercise the wake/transition path. Individual tests
        // override by mutating a field on the backend if they need
        // the ynest path.
        self.dpms_capable
    }

    fn set_dpms_power(&mut self, level: u8) -> std::io::Result<()> {
        self.calls
            .lock()
            .unwrap()
            .push(RecordedCall::SetDpmsPower(level));
        if self.dpms_set_returns_err {
            Err(std::io::Error::other("test-injected dpms error"))
        } else {
            Ok(())
        }
    }

    fn acquire_glx_pixmap_export(&mut self, host_xid: u32) {
        self.calls
            .lock()
            .unwrap()
            .push(RecordedCall::AcquireGlxPixmapExport(host_xid));
    }

    fn release_glx_pixmap_export(&mut self, host_xid: u32) {
        self.calls
            .lock()
            .unwrap()
            .push(RecordedCall::ReleaseGlxPixmapExport(host_xid));
    }

    fn promote_pixmap_exportable(&mut self, host_xid: u32) -> bool {
        self.calls
            .lock()
            .unwrap()
            .push(RecordedCall::PromotePixmapExportable(host_xid));
        // RecordingBackend has no real GPU storage; report not-exportable.
        // Tests assert on the recorded call, not the return value.
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Dyn-coercion smoke test: confirm the recorder can be parked
    /// behind `Arc<Mutex<dyn Backend>>` exactly the way `nested::run`
    /// holds the production backend. This is the *existence proof*
    /// that the trait carve from Step 5 works for non-HostX11 impls.
    #[test]
    fn recording_backend_is_dyn_safe() {
        use std::sync::{Arc, Mutex};
        let rec = Arc::new(Mutex::new(RecordingBackend::new()));
        let dyn_arc: Arc<Mutex<dyn Backend>> = rec;
        // Drive a few methods through the dyn pointer to confirm vtable
        // dispatch works at runtime.
        let mut g = dyn_arc.lock().unwrap();
        let parent = WindowHandle::from_raw_panicking(g.window_id());
        let child = g
            .create_subwindow(
                None,
                parent,
                10,
                20,
                100,
                80,
                0,
                HostSubwindowVisual::CopyFromParent,
                None,
                None,
            )
            .unwrap();
        g.map_subwindow(None, child.as_raw()).unwrap();
        g.unmap_subwindow(None, child.as_raw()).unwrap();
        g.destroy_subwindow(None, child.as_raw()).unwrap();
    }

    #[test]
    fn recording_backend_records_basic_lifecycle() {
        let mut rec = RecordingBackend::new();
        let parent = WindowHandle::from_raw_panicking(rec.window_id());
        let a = rec
            .create_subwindow(
                None,
                parent,
                0,
                0,
                50,
                50,
                0,
                HostSubwindowVisual::CopyFromParent,
                None,
                None,
            )
            .unwrap();
        let b = rec
            .create_subwindow(
                None,
                parent,
                0,
                0,
                30,
                30,
                1,
                HostSubwindowVisual::CopyFromParent,
                Some(0xff0000),
                None,
            )
            .unwrap();
        rec.map_subwindow(None, a.as_raw()).unwrap();
        rec.map_subwindow(None, b.as_raw()).unwrap();
        rec.destroy_subwindow(None, a.as_raw()).unwrap();

        assert_ne!(a.as_raw(), b.as_raw(), "fresh handles each create");
        let calls = rec.calls();
        assert_eq!(calls.len(), 5, "5 calls recorded, got {calls:#?}");
        assert!(matches!(
            calls[0],
            RecordedCall::CreateSubwindow {
                width: 50,
                height: 50,
                ..
            }
        ));
        assert!(matches!(
            calls[1],
            RecordedCall::CreateSubwindow {
                background_pixel: Some(0xff0000),
                ..
            }
        ));
        assert!(matches!(calls[2], RecordedCall::MapSubwindow(_)));
        assert!(matches!(calls[3], RecordedCall::MapSubwindow(_)));
        assert!(matches!(calls[4], RecordedCall::DestroySubwindow(_)));
    }

    /// Phase 6.3 Step 4: `register_top_level` records the call AND
    /// inserts into the shared `xid_map` so the dispatcher's sink
    /// sees the new mapping. Replicates the contract `nested::run`
    /// relies on after the merge.
    #[test]
    fn register_top_level_updates_xid_map_and_records() {
        let mut rec = RecordingBackend::new();
        let nested_id = ResourceId(0x100);
        let host_xid = 0xdead_beef;
        rec.register_top_level(None, nested_id, host_xid)
            .expect("register_top_level");
        // xid_map sees the new entry.
        let map = rec.xid_map();
        assert_eq!(map.get(&host_xid).copied(), Some(nested_id));
        // Call is recorded with the same nested_id / host_xid.
        let calls = rec.calls();
        assert!(matches!(
            calls.last().unwrap(),
            RecordedCall::RegisterTopLevel {
                nested_id: r,
                host_xid: h
            } if *r == nested_id && *h == host_xid
        ));
    }

    /// Same shape for sub-windows — separate call variant so tests
    /// can distinguish the top-level vs sub-window path.
    #[test]
    fn register_subwindow_updates_xid_map_and_records() {
        let mut rec = RecordingBackend::new();
        let nested_id = ResourceId(0x200);
        let host_xid = 0xc0ff_eecc;
        rec.register_subwindow(None, nested_id, host_xid)
            .expect("register_subwindow");
        let map = rec.xid_map();
        assert_eq!(map.get(&host_xid).copied(), Some(nested_id));
        let calls = rec.calls();
        assert!(matches!(
            calls.last().unwrap(),
            RecordedCall::RegisterSubwindow {
                nested_id: r,
                host_xid: h
            } if *r == nested_id && *h == host_xid
        ));
    }

    /// `unregister_host_window` clears the xid_map entry — stale
    /// host events on a destroyed xid never resolve to a defunct
    /// ResourceId.
    #[test]
    fn unregister_host_window_clears_xid_map_entry() {
        let mut rec = RecordingBackend::new();
        let nested_id = ResourceId(0x300);
        let host_xid = 0xfeed_face;
        rec.register_top_level(None, nested_id, host_xid).unwrap();
        rec.unregister_host_window(host_xid);
        let map = rec.xid_map();
        assert!(map.get(&host_xid).is_none());
        let calls = rec.calls();
        assert!(matches!(
            calls.last().unwrap(),
            RecordedCall::UnregisterHostWindow(h) if *h == host_xid
        ));
    }
}
