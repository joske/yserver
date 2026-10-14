//! The platform seam: what the portable half of the backend needs from the
//! platform under it (KMS today). No DRM, GBM or window-system type crosses it.
//!
//! Accessors come first. The hooks after them are the `Backend` methods
//! whose behaviour depends on the platform; each takes the whole backend and
//! defaults to the plain `Backend` default (no VT, no DPMS, no DRI3, one
//! fixed output).

use std::io;

use yserver_core::{
    backend::{
        Backend, BackendFdKind, CrtcConfigApply, CrtcConfigToken, Dri3Caps, Dri3ImportModifier,
        Dri3PixmapExport, PixmapHandle, PresentCaps, PresentClockSample, PresentClockSource,
        PresentScanoutCandidate, PresentSourceWait,
    },
    server::ServerState,
};

use crate::kms::render::{
    backend::XBackend,
    present_batch::{PendingPresentBatch, PresentBatchWait},
    scene::{OutputLayout, OutputSink},
    store::DrawableId,
};

/// A platform the backend runs on. The GPU comes through [`OutputSink`].
pub trait Platform: OutputSink + Sized + 'static {
    /// Platform-owned backend state, a sibling of `platform` on the backend
    /// so platform code keeps disjoint field borrows.
    type BackendState;

    /// The logical screen size in pixels.
    fn fb_size(&self) -> (u16, u16);
    /// What the scene reads of every live output, in output order.
    fn output_layouts(&self) -> Vec<OutputLayout>;

    /// A drawable's storage is gone; forget platform caches keyed by it.
    fn note_drawable_retired(_st: &mut Self::BackendState, _id: DrawableId) {}
    /// The keyboard LEDs should show `lit`, the lit XKB indicators (bit 0
    /// Caps Lock, bit 1 Num Lock, bit 2 Scroll Lock).
    fn set_keyboard_leds(_st: &mut Self::BackendState, _lit: u32) {}
    /// Whether the screen shows a client buffer directly, bypassing the scene.
    fn direct_scanout_owns_frame(_b: &XBackend<Self>) -> bool {
        false
    }
    /// Leave direct scanout for the composed scene.
    fn request_direct_unflip(_b: &mut XBackend<Self>, _reason: &'static str) {}
    /// Whether every direct frame is the COW or one of its descendants.
    fn direct_frames_are_under_cow(_b: &XBackend<Self>) -> bool {
        false
    }
    /// Whether a direct frame scans out storage of `host_xid`.
    fn direct_frame_references_host_drawable(_b: &XBackend<Self>, _host_xid: u32) -> bool {
        false
    }
    /// Whether a SHAPE now clips a direct frame off the root.
    fn direct_frames_shaped_off_root(_b: &XBackend<Self>) -> bool {
        false
    }
    /// Rebind the cursor planes to `record` under direct scanout; `false` on
    /// failure.
    fn refresh_direct_cursor(
        _b: &mut XBackend<Self>,
        _record: &std::sync::Arc<crate::kms::render::cursor::CursorRecord>,
    ) -> bool {
        false
    }
    /// XFIXES HideCursor while no compose runs: detach the cursor planes.
    fn hide_cursor_under_direct_scanout(_b: &mut XBackend<Self>) {}
    /// The cursor sprite's size or hotspot changed.
    fn note_cursor_sprite(
        _b: &mut XBackend<Self>,
        _width: u16,
        _height: u16,
        _hot_x: u16,
        _hot_y: u16,
    ) {
    }
    /// `XWarpPointer` moved the X pointer to root `(x, y)`; a windowed
    /// platform moves the host pointer to match.
    fn host_pointer_warped(_b: &mut XBackend<Self>, _x: i32, _y: i32) {}
    /// Move a hardware-plane cursor (scene cursor mode Hw, or Mixed).
    fn move_hw_cursor(
        b: &mut XBackend<Self>,
        _mixed: bool,
        _pos: (i32, i32),
        _hot: (u16, u16),
        _size: (i32, i32),
    ) {
        b.scene.wake_for_damage();
    }
    /// Whether the platform may present now (KMS: holds DRM master).
    fn scanout_allowed(_b: &XBackend<Self>) -> bool {
        true
    }
    /// The final COW release while a direct frame still owns the screen:
    /// defer the teardown and return `true`, or `false` to tear down now.
    fn defer_cow_release(_b: &mut XBackend<Self>) -> io::Result<bool> {
        Ok(false)
    }
    /// A pixmap backing lost a reference; drop its dma-buf export if unused.
    fn maybe_teardown_export(_b: &mut XBackend<Self>, _id: DrawableId) {}
    /// Watch a Present batch's wait; returns the wait the queue keeps.
    fn register_present_batch_wait(
        _b: &mut XBackend<Self>,
        wait: PresentBatchWait,
    ) -> PresentBatchWait {
        match wait {
            PresentBatchWait::Fd(_) => PresentBatchWait::Poll,
            other => other,
        }
    }
    /// Stop watching a drained batch's wait.
    fn release_present_batch_wait(_b: &mut XBackend<Self>, _batch: &PendingPresentBatch) {}
    /// Wake the core loop to drain Present completions.
    fn wake_present_completion(_b: &XBackend<Self>, _context: &str) {}
    /// Reset the Present completion wake before a drain.
    fn clear_present_completion_wake(_b: &XBackend<Self>) {}
    /// What is on screen under the root rect `rect`, which lies in one output.
    fn read_on_screen_region(
        _b: &mut XBackend<Self>,
        _rect: ash::vk::Rect2D,
    ) -> io::Result<Vec<u8>> {
        Err(io::Error::other("no on-screen output to read"))
    }
    /// Diagnostic dump of what each output scans out.
    fn dump_scanout_files(_b: &mut XBackend<Self>) -> io::Result<()> {
        Ok(())
    }

    fn randr_outputs_and_modes(
        b: &mut XBackend<Self>,
    ) -> (
        Vec<yserver_core::randr::RandrOutput>,
        Vec<yserver_core::randr::RandrMode>,
    ) {
        crate::kms::render::backend::seam::single_output_randr(b.platform.fb_size(), 60)
    }

    fn randr_providers(_b: &mut XBackend<Self>) -> Vec<yserver_core::randr::RandrProvider> {
        Vec::new()
    }

    fn crtc_gamma_size(_b: &XBackend<Self>, _crtc: u32) -> u16 {
        0
    }

    fn set_crtc_gamma(
        _b: &mut XBackend<Self>,
        _crtc: u32,
        _red: &[u16],
        _green: &[u16],
        _blue: &[u16],
    ) -> io::Result<()> {
        Ok(())
    }

    fn get_crtc_gamma(_b: &XBackend<Self>, _crtc: u32) -> (Vec<u16>, Vec<u16>, Vec<u16>) {
        (Vec::new(), Vec::new(), Vec::new())
    }

    fn on_scanout_render_completion(_b: &mut XBackend<Self>, _state: &mut ServerState) {}

    fn on_page_flip_ready(
        _b: &mut XBackend<Self>,
        _state: &mut ServerState,
        _drm_fd: std::os::fd::RawFd,
    ) {
    }

    fn before_block(_b: &mut XBackend<Self>) {}

    fn flush_before_damage_notify(_b: &mut XBackend<Self>) {}

    fn next_wakeup(_b: &XBackend<Self>) -> Option<std::time::Instant> {
        None
    }

    fn maybe_composite(_b: &mut XBackend<Self>) -> io::Result<()> {
        Ok(())
    }

    /// Drain the platform's queue after it posted `Message::PlatformEvent`.
    fn on_platform_event(_b: &mut XBackend<Self>, _state: &mut ServerState) {}

    /// The core's established-client count changed.
    fn note_client_count(_b: &mut XBackend<Self>, _count: usize) {}

    fn dump_scanout(_b: &mut XBackend<Self>) {}

    fn report_export_holders(
        _b: &mut XBackend<Self>,
        _core: &dyn Fn() -> yserver_core::backend::export_holders::CoreHolders,
    ) -> bool {
        false
    }

    fn note_present_pixmap(_b: &mut XBackend<Self>, _src_pixmap_xid: u32, _dst_window_xid: u32) {}

    fn note_present_scanout_candidate(
        _b: &mut XBackend<Self>,
        _candidate: PresentScanoutCandidate,
    ) {
    }

    fn try_present_direct(
        _b: &mut XBackend<Self>,
        _candidate: PresentScanoutCandidate,
        _event: yserver_core::backend::CompletedPresentEvent,
    ) -> io::Result<bool> {
        Ok(false)
    }

    fn arm_present_source_wait(
        _b: &mut XBackend<Self>,
        _src_pixmap_host_xid: u32,
        _dst_window_host_xid: u32,
    ) -> io::Result<PresentSourceWait> {
        Ok(PresentSourceWait::Ready)
    }

    fn arm_present_syncobj_wait(
        _b: &mut XBackend<Self>,
        _src_pixmap_host_xid: u32,
        _dst_window_host_xid: u32,
        _acquire_syncobj: u32,
        _acquire_value: u64,
    ) -> io::Result<PresentSourceWait> {
        Ok(PresentSourceWait::Ready)
    }

    fn drain_ready_present_source_waits(_b: &mut XBackend<Self>) -> Vec<u64> {
        Vec::new()
    }

    fn begin_ready_present_destination_write(_b: &mut XBackend<Self>, _wait_id: u64) {}

    fn finish_present_source_wait(_b: &mut XBackend<Self>, _wait_id: u64) {}

    fn present_flip_in_flight(_b: &XBackend<Self>, _crtc_id: u32) -> bool {
        false
    }

    fn present_display_idle(_b: &XBackend<Self>, _crtc_id: u32) -> bool {
        true
    }

    fn present_absolute_vblank_arm_supported(_b: &XBackend<Self>, _crtc_id: u32) -> bool {
        false
    }

    fn arm_present_absolute_vblank(
        _b: &mut XBackend<Self>,
        _randr_crtc_id: u32,
        _targets: &[u64],
    ) -> io::Result<usize> {
        Ok(0)
    }

    fn present_scanout_blackout(_b: &XBackend<Self>) -> bool {
        false
    }

    fn poll_fds(_b: &XBackend<Self>) -> Vec<(std::os::fd::RawFd, BackendFdKind)> {
        Vec::new()
    }

    fn vt_switching_armed(_b: &XBackend<Self>) -> bool {
        false
    }

    fn set_input_sender(_b: &mut XBackend<Self>, _sender: yserver_core::core_loop::CoreSender) {}

    fn request_vt_switch(_b: &mut XBackend<Self>, _vt: u32) {}

    fn begin_vt_release(_b: &mut XBackend<Self>) -> bool {
        false
    }

    fn finish_vt_release(
        b: &mut XBackend<Self>,
        state: &mut ServerState,
        _input_inventory: &yserver_core::core_loop::input_inventory::InputInventory,
    ) {
        Backend::on_vt_release(b, state);
    }

    fn on_vt_acquire(_b: &mut XBackend<Self>, _state: &mut ServerState) {}

    fn on_display_hotplug(_b: &mut XBackend<Self>, _state: &mut ServerState) {}

    fn reprobe_connectors(_b: &mut XBackend<Self>, _state: &mut ServerState) -> io::Result<()> {
        Ok(())
    }

    fn set_provider_output_source(
        _b: &mut XBackend<Self>,
        _state: &mut ServerState,
        _provider: u32,
        _source_provider: Option<u32>,
    ) -> io::Result<bool> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "provider output-source relationships are unsupported",
        ))
    }

    fn begin_crtc_config(
        b: &mut XBackend<Self>,
        output_id: u32,
        connector: &str,
        mode: Option<yserver_core::backend::ModeSpec>,
        x: i32,
        y: i32,
    ) -> io::Result<CrtcConfigApply> {
        Self::apply_crtc_config(b, output_id, connector, mode, x, y).map(CrtcConfigApply::Applied)
    }

    fn drain_ready_crtc_configs(_b: &mut XBackend<Self>) -> Vec<CrtcConfigToken> {
        Vec::new()
    }

    fn finish_crtc_config(_b: &mut XBackend<Self>, _token: CrtcConfigToken) -> io::Result<bool> {
        Err(io::Error::other(
            "unknown asynchronous CRTC configuration token",
        ))
    }

    fn cancel_crtc_config(_b: &mut XBackend<Self>, _token: CrtcConfigToken) {}

    fn apply_crtc_config(
        _b: &mut XBackend<Self>,
        _output_id: u32,
        _connector: &str,
        _mode: Option<yserver_core::backend::ModeSpec>,
        _x: i32,
        _y: i32,
    ) -> io::Result<bool> {
        Ok(false)
    }

    fn refresh_randr_state_set_time(
        _b: &mut XBackend<Self>,
        _state: &mut yserver_core::server::ServerState,
        _set_time: u32,
    ) {
    }

    fn randr_layout_changed(_b: &mut XBackend<Self>, _state: &mut ServerState) {}

    fn output_identity(_b: &XBackend<Self>, _output_id: u32) -> Option<(Vec<u8>, String)> {
        None
    }

    fn set_logical_screen_size(_b: &mut XBackend<Self>, _w: u16, _h: u16) -> io::Result<()> {
        Ok(())
    }

    fn randr_limits(_b: &XBackend<Self>) -> yserver_core::randr::RandrLimits {
        yserver_core::randr::RandrLimits::default()
    }

    fn poll_deferred_input(_b: &mut XBackend<Self>, _state: &mut ServerState) {}

    fn start_device_config(
        _b: &mut XBackend<Self>,
        _source: yserver_core::xinput::InputSourceId,
        _change: yserver_core::xinput::libinput_props::DeviceConfigChange,
        _cancel: yserver_core::xinput::libinput_props::DeviceConfigCancelToken,
    ) -> Result<
        yserver_core::xinput::libinput_props::DeviceConfigStart,
        yserver_core::xinput::libinput_props::DeviceConfigError,
    > {
        Ok(yserver_core::xinput::libinput_props::DeviceConfigStart::Applied)
    }

    fn supports_dmabuf_export(_b: &XBackend<Self>) -> bool {
        false
    }

    fn glx_vendor_names(_b: &XBackend<Self>) -> &'static str {
        yserver_protocol::x11::glx::VENDOR_NAMES
    }

    fn acquire_glx_pixmap_export(_b: &mut XBackend<Self>, _host_xid: u32) {}

    fn release_glx_pixmap_export(_b: &mut XBackend<Self>, _host_xid: u32) {}

    fn promote_pixmap_exportable(_b: &mut XBackend<Self>, _host_xid: u32) -> bool {
        false
    }

    fn dri3_open(_b: &mut XBackend<Self>, _drawable: u32) -> io::Result<std::os::fd::OwnedFd> {
        Err(io::Error::other("DRI3 unsupported on this backend"))
    }

    /// Whether GLX is advertised: only where a GL client stack can use it.
    fn glx_available(_b: &XBackend<Self>) -> bool {
        false
    }

    fn dri3_capabilities(_b: &XBackend<Self>) -> Dri3Caps {
        Dri3Caps::unsupported()
    }

    fn dri3_import_pixmap(
        _b: &mut XBackend<Self>,
        _fd: std::os::fd::OwnedFd,
        _width: u16,
        _height: u16,
        _stride: u32,
        _offset: u32,
        _modifier: Dri3ImportModifier,
        _depth: u8,
        _bpp: u8,
    ) -> io::Result<PixmapHandle> {
        Err(io::Error::other("DRI3 import unsupported on this backend"))
    }

    fn dri3_supported_modifiers(
        _b: &XBackend<Self>,
        _window: u32,
        _depth: u8,
        _bpp: u8,
    ) -> (Vec<u64>, Vec<u64>) {
        // 0 == DRM_FORMAT_MOD_LINEAR.
        (vec![0], vec![0])
    }

    fn dri3_export_pixmap(
        _b: &mut XBackend<Self>,
        _host_xid: u32,
    ) -> io::Result<(u32, u16, u16, u16, u8, u8, std::os::fd::OwnedFd)> {
        Err(io::Error::other("DRI3 export unsupported on this backend"))
    }

    fn dri3_export_pixmap_buffers(
        _b: &mut XBackend<Self>,
        _host_xid: u32,
    ) -> io::Result<Dri3PixmapExport> {
        Err(io::Error::other(
            "DRI3 BuffersFromPixmap unsupported on this backend",
        ))
    }

    fn dri3_fence_from_fd(
        _b: &mut XBackend<Self>,
        _fence_xid: u32,
        _fd: std::os::fd::OwnedFd,
    ) -> io::Result<()> {
        Err(io::Error::other("DRI3 FenceFromFD unsupported"))
    }

    fn dri3_trigger_fence(_b: &mut XBackend<Self>, _fence_xid: u32) -> io::Result<()> {
        Ok(())
    }

    fn dri3_fence_triggered(_b: &XBackend<Self>, _fence_xid: u32) -> Option<bool> {
        None
    }

    fn dri3_reset_fence(_b: &mut XBackend<Self>, _fence_xid: u32) {}

    fn dri3_destroy_fence(_b: &mut XBackend<Self>, _fence_xid: u32) {}

    fn dri3_xshmfence_handle(
        _b: &XBackend<Self>,
        _fence_xid: u32,
    ) -> Option<std::sync::Arc<dyn yserver_core::backend::XshmfenceHandle>> {
        None
    }

    fn dri3_syncobj_handle(
        _b: &XBackend<Self>,
        _syncobj_xid: u32,
    ) -> Option<std::sync::Arc<dyn yserver_core::backend::SyncobjHandle>> {
        None
    }

    fn dri3_syncobj_owned(
        _b: &XBackend<Self>,
        _client_id: yserver_protocol::x11::ClientId,
        _syncobj_xid: u32,
    ) -> bool {
        false
    }

    fn dri3_fd_from_fence(
        _b: &mut XBackend<Self>,
        _fence_xid: u32,
    ) -> io::Result<std::os::fd::OwnedFd> {
        Err(io::Error::other("DRI3 FDFromFence unsupported"))
    }

    fn dri3_import_syncobj(
        _b: &mut XBackend<Self>,
        _client_id: yserver_protocol::x11::ClientId,
        _syncobj_xid: u32,
        _fd: std::os::fd::OwnedFd,
    ) -> io::Result<()> {
        Err(io::Error::other("DRI3 ImportSyncobj unsupported"))
    }

    fn dri3_free_syncobj(
        _b: &mut XBackend<Self>,
        _client_id: yserver_protocol::x11::ClientId,
        _syncobj_xid: u32,
    ) -> io::Result<()> {
        Err(io::Error::other("DRI3 FreeSyncobj unsupported"))
    }

    fn dri3_signal_syncobj(
        _b: &mut XBackend<Self>,
        _syncobj_xid: u32,
        _value: u64,
    ) -> io::Result<()> {
        Err(io::Error::other("DRI3 SignalSyncobj unsupported"))
    }

    fn drain_completed_present_events(
        b: &mut XBackend<Self>,
    ) -> Vec<yserver_core::backend::CompletedPresentEvent> {
        b.drain_completed_present_events_impl()
    }

    fn drain_retired_present_idle_events(
        _b: &mut XBackend<Self>,
    ) -> Vec<yserver_core::backend::CompletedPresentEvent> {
        Vec::new()
    }

    fn present_crtc_clock_epoch(_b: &XBackend<Self>, _crtc_id: u32) -> u64 {
        0
    }

    fn present_get_ust_msc(_b: &XBackend<Self>, _crtc_id: u32) -> (u64, u64) {
        (0, 0)
    }

    fn present_get_completion_clock(
        b: &XBackend<Self>,
        crtc_id: u32,
    ) -> yserver_core::backend::PresentClockSample {
        let (msc, ust) = Self::present_get_ust_msc(b, crtc_id);
        PresentClockSample {
            msc,
            ust,
            source: PresentClockSource::BackendVblank,
        }
    }

    fn arm_idle_vblanks(
        _b: &mut XBackend<Self>,
        _crtc_id: u32,
        _target_mscs: &[u64],
    ) -> std::io::Result<usize> {
        Ok(0)
    }

    fn arm_present_completion_idle_vblanks(
        _b: &mut XBackend<Self>,
        _crtc_id: u32,
        _target_mscs: &[u64],
    ) -> std::io::Result<usize> {
        Ok(0)
    }

    fn present_capabilities(_b: &XBackend<Self>, _window: u32) -> PresentCaps {
        PresentCaps::default()
    }

    fn dpms_capable(_b: &XBackend<Self>) -> bool {
        false
    }

    fn set_dpms_power(_b: &mut XBackend<Self>, _level: u8) -> std::io::Result<()> {
        Ok(())
    }
}

/// RANDR for a platform with one fixed output covering the screen.
#[must_use]
pub(crate) fn single_output_randr(
    (width, height): (u16, u16),
    vrefresh: u32,
) -> (
    Vec<yserver_core::randr::RandrOutput>,
    Vec<yserver_core::randr::RandrMode>,
) {
    let outputs = vec![yserver_core::randr::RandrOutput {
        name: "WINDOW-0".to_string(),
        output_id: 1,
        crtc_id: 2,
        mode_id: 3,
        connected: true,
        x: 0,
        y: 0,
        width,
        height,
        vrefresh,
        timing: None,
        // No EDID: `RandrState::output_info` synthesises 96 DPI.
        mm_width: 0,
        mm_height: 0,
        mode_ids: vec![3],
        num_preferred: 1,
        pending_transform: Default::default(),
        current_transform: Default::default(),
        rotation: yserver_core::randr::RR_ROTATE_0,
    }];
    let modes = yserver_core::randr::RandrState::from_outputs(0, outputs.clone()).mode_table;
    (outputs, modes)
}

/// A prepare stage of a screen resize that a test makes fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResizeFault {
    /// Allocating root, COW or slot images.
    Alloc,
    /// Filling or tiling the new root or COW.
    Fill,
    /// Building the scene's new output state.
    #[cfg_attr(all(kms, not(test)), allow(dead_code))] // The surface's.
    Outputs,
}

#[cfg(test)]
thread_local! {
    /// The stage a resize fails at, after passing it this many times.
    static RESIZE_FAULT: std::cell::Cell<Option<(ResizeFault, u32)>> =
        const { std::cell::Cell::new(None) };
}

/// Make the resize step at `stage` fail once on this thread, after `skip`
/// passes of it (tests).
#[cfg(test)]
pub(crate) fn inject_resize_fault(stage: ResizeFault, skip: u32) {
    RESIZE_FAULT.with(|fault| fault.set(Some((stage, skip))));
}

/// Whether a test asked this pass of `stage` to fail; the fault is consumed.
#[must_use]
pub(crate) fn resize_fault(stage: ResizeFault) -> bool {
    #[cfg(test)]
    {
        RESIZE_FAULT.with(|fault| match fault.get() {
            Some((at, 0)) if at == stage => {
                fault.set(None);
                true
            }
            Some((at, skip)) if at == stage => {
                fault.set(Some((at, skip - 1)));
                false
            }
            _ => false,
        })
    }
    #[cfg(not(test))]
    {
        let _ = stage;
        false
    }
}
