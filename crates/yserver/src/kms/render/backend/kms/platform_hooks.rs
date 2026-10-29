//! `PlatformBackend` behind the platform seam.

use std::io;

use yserver_core::{
    backend::{
        BackendFdKind, CrtcConfigApply, CrtcConfigToken, Dri3Caps, Dri3ImportModifier,
        Dri3PixmapExport, PixmapHandle, PresentCaps, PresentScanoutCandidate, PresentSourceWait,
    },
    server::ServerState,
};

use crate::kms::render::{
    backend::{DirectScope, KmsBackend, KmsBackendState, seam::Platform},
    platform::PlatformBackend,
    present_batch::{PendingPresentBatch, PresentBatchWait},
    scene::OutputLayout,
    store::DrawableId,
};

impl Platform for PlatformBackend {
    fn client_disconnected(
        state: &mut KmsBackendState,
        client_id: yserver_protocol::x11::ClientId,
    ) {
        state.vrr_exclusion_cache.borrow_mut().remove(&client_id.0);
    }

    type BackendState = KmsBackendState;

    #[inline]
    fn fb_size(&self) -> (u16, u16) {
        (self.fb_w, self.fb_h)
    }

    fn output_layouts(&self) -> Vec<OutputLayout> {
        self.scene_output_layouts()
    }

    fn note_root_pointer_position(b: &mut KmsBackend, x: i32, y: i32) {
        b.platform.note_root_pointer_position(x, y);
    }

    fn note_drawable_retired(st: &mut KmsBackendState, id: DrawableId) {
        st.retire_scanout_drawable(id);
    }

    fn set_keyboard_leds(st: &mut KmsBackendState, lit: u32) {
        let bits = led_bits_for_indicators(lit);
        if bits == st.leds_sent {
            return;
        }
        st.leds_sent = bits;
        if let Some(relay) = st.led_relay.as_ref() {
            relay.set(bits);
        }
    }

    fn direct_scanout_owns_frame(b: &KmsBackend) -> bool {
        !b.pstate.direct.active_scopes().is_empty()
    }

    fn tearing_active(b: &KmsBackend) -> bool {
        b.pstate.tearing.is_on()
    }

    fn cursor_intersects_outputs(
        b: &KmsBackend,
        cursor_x: i64,
        cursor_y: i64,
        hot_x: i64,
        hot_y: i64,
        width: i64,
        height: i64,
    ) -> bool {
        b.platform.outputs.iter().any(|output| {
            let dx = cursor_x - hot_x - i64::from(output.x);
            let dy = cursor_y - hot_y - i64::from(output.y);
            dx < i64::from(output.width)
                && dx + width > 0
                && dy < i64::from(output.height)
                && dy + height > 0
        })
    }

    fn cursor_motion_during_tearing(b: &mut KmsBackend, cursor_visible: bool) {
        if !cursor_visible {
            b.scene.wake_for_damage();
            return;
        }
        if !b.tearing_cursor_compose_cap_applies() {
            b.scene.wake_for_damage();
            return;
        }

        b.scene.wake_for_cursor_damage();
        let now = b.monotonic_now();
        let interval = b.pstate.tearing.cursor_compose_interval(&b.platform);
        match b.pstate.tearing.last_cursor_compose {
            Some(last_compose) if now < last_compose + interval => {
                b.pstate.tearing.deferred_cursor_wake = Some(last_compose + interval);
            }
            _ => {
                b.pstate.tearing.deferred_cursor_wake = None;
                b.scene.wake_for_damage();
            }
        }
    }

    fn tearing_cursor_compose_cap_applies(b: &KmsBackend) -> bool {
        b.tearing_cursor_compose_cap_applies()
    }

    fn note_tearing_copy_damage(b: &mut KmsBackend, dst_window_xid: u32) {
        if b.pstate.tearing.is_on()
            && matches!(
                b.pstate.tearing.state,
                crate::kms::render::backend::TearingState::On { host_window, .. }
                    if dst_window_xid == host_window || dst_window_xid == b.core.window_id
            )
        {
            b.platform.note_tearing_copy_damage(
                std::time::Instant::now(),
                crate::drm::atomic::monotonic_timestamp(),
            );
        }
    }

    fn exit_tearing_for_window(b: &mut KmsBackend, host_xid: u32, reason: &'static str) {
        b.exit_tearing_for_window(host_xid, reason);
    }

    fn on_window_property_changed(
        b: &mut KmsBackend,
        state: &ServerState,
        host_xid: u32,
        property: yserver_protocol::x11::AtomId,
    ) {
        b.backend_scanout_on_window_property_changed(state, host_xid, property);
    }

    fn request_direct_unflip(b: &mut KmsBackend, reason: &'static str) {
        b.request_direct_unflip(reason);
    }

    fn request_direct_unflip_for_host_drawable(
        b: &mut KmsBackend,
        host_xid: u32,
        reason: &'static str,
    ) {
        b.request_direct_unflip_for_host_drawable(host_xid, reason);
    }

    fn on_top_level_order_changed(b: &mut KmsBackend) {
        if !b.direct_frames_are_under_cow() {
            b.request_direct_unflip_for(DirectScope::Root, "top_level_stack_changed");
        }
    }

    fn on_direct_shape_changed(b: &mut KmsBackend) {
        if b.direct_frames_shaped_off_root() {
            b.request_direct_unflip_for(DirectScope::Root, "shape_clips_direct_frame");
        }
    }

    fn request_tearing_cursor_handoff(b: &mut KmsBackend) {
        b.request_tearing_cursor_handoff();
    }

    fn direct_frames_are_under_cow(b: &KmsBackend) -> bool {
        b.direct_frames_are_under_cow()
    }

    fn refresh_direct_cursor(
        b: &mut KmsBackend,
        record: &std::sync::Arc<crate::kms::render::cursor::CursorRecord>,
    ) -> Vec<usize> {
        b.refresh_direct_cursor_on_all_outputs(record)
    }

    fn hide_cursor_under_direct_scanout(b: &mut KmsBackend) {
        for scope in b.pstate.direct.active_scopes() {
            let output_indices = b.direct_scope_outputs(scope);
            if !output_indices
                .iter()
                .any(|&idx| b.platform.cursor_plane_visible_for_output(idx))
            {
                for &output_idx in &output_indices {
                    b.platform.clear_direct_cursor_retry(output_idx);
                }
                if let Some(owner) = b.pstate.direct.get_mut(scope) {
                    owner.clear_all_direct_cursor_refresh_pending();
                }
                if !b.sync_direct_cursor_scene_state(&output_indices) {
                    b.request_direct_unflip_for(scope, "cursor_scene_sync_failed");
                }
                continue;
            }
            let mut failed = false;
            for &output_idx in &output_indices {
                if !b.platform.cursor_plane_visible_for_output(output_idx) {
                    b.platform.clear_direct_cursor_retry(output_idx);
                    if let Some(owner) = b.pstate.direct.get_mut(scope) {
                        owner.clear_direct_cursor_refresh_pending(output_idx);
                    }
                    continue;
                }
                let atomic_path = b
                    .platform
                    .device_for_output(&b.platform.outputs[output_idx].key)
                    .is_some_and(|device| {
                        device.cursor.state_path()
                            == crate::kms::cursor_plane::topology::CursorStatePath::Atomic
                    });
                match b.platform.cursor_plane_hide_on_crtc(output_idx) {
                    Ok(()) => {
                        b.platform.clear_direct_cursor_retry(output_idx);
                        if let Some(owner) = b.pstate.direct.get_mut(scope) {
                            owner.clear_direct_cursor_refresh_pending(output_idx);
                        }
                    }
                    Err(error)
                        if error.kind() == io::ErrorKind::WouldBlock
                            || error.raw_os_error() == Some(libc::EBUSY) =>
                    {
                        b.platform.clear_direct_cursor_retry(output_idx);
                        if let Some(owner) = b.pstate.direct.get_mut(scope) {
                            owner.mark_direct_cursor_refresh_pending(output_idx);
                        }
                        log::debug!(
                            "scanout_m2: direct cursor hide deferred on output {output_idx}; retrying on CRTC event"
                        );
                    }
                    Err(error)
                        if atomic_path
                            && error.raw_os_error() != Some(libc::EINVAL)
                            && error.raw_os_error() != Some(libc::EBUSY) =>
                    {
                        if !b.schedule_direct_cursor_retry(
                            scope,
                            output_idx,
                            "hide",
                            &error,
                            "cursor_hide_failed",
                        ) {
                            failed = true;
                        }
                    }
                    Err(error) => {
                        log::warn!(
                            "scanout_m2: direct cursor hide failed on output {output_idx}: {error}; unflipping"
                        );
                        failed = true;
                    }
                }
            }
            if failed {
                b.request_direct_unflip_for(scope, "cursor_hide_failed");
            }
            if !b.sync_direct_cursor_scene_state(&output_indices) {
                b.request_direct_unflip_for(scope, "cursor_scene_sync_failed");
            }
        }
    }

    fn recover_hidden_cursor_on_resume(b: &mut KmsBackend) {
        for output_idx in 0..b.platform.outputs.len() {
            let output = &b.platform.outputs[output_idx];
            let atomic_path = b
                .platform
                .device_for_key(output.key.device_key)
                .is_some_and(|device| {
                    device.cursor.state_path()
                        == crate::kms::cursor_plane::topology::CursorStatePath::Atomic
                });
            if !atomic_path && !b.platform.cursor_plane_visible_for_output(output_idx) {
                continue;
            }
            if let Err(error) = b.platform.cursor_plane_hide_on_crtc(output_idx) {
                log::warn!(
                    "render resume: hidden cursor plane detach failed on output {output_idx}: {error}"
                );
            }
        }
    }

    fn note_cursor_sprite(b: &mut KmsBackend, width: u16, height: u16, hot_x: u16, hot_y: u16) {
        b.platform
            .cursor_plane_note_sprite_hotspot(width, height, hot_x, hot_y);
    }

    fn move_hw_cursor(
        b: &mut KmsBackend,
        mixed: bool,
        (cx, cy): (i32, i32),
        (hot_x, hot_y): (u16, u16),
        (cw, ch): (i32, i32),
    ) {
        if b.pstate.tearing.is_on()
            && !b
                .pstate
                .direct
                .active_scopes()
                .iter()
                .any(|scope| matches!(scope, DirectScope::Output(_)))
        {
            return;
        }
        let membership_changed = if mixed || cw == 0 || ch == 0 {
            false
        } else {
            let direct_outputs: Vec<_> = b
                .pstate
                .direct
                .active_scopes()
                .into_iter()
                .flat_map(|scope| b.direct_scope_outputs(scope))
                .collect();
            b.platform
                .cursor_crtc_membership_dirty(cx, cy, hot_x, hot_y, cw, ch, &direct_outputs)
        };
        if membership_changed {
            // The cursor footprint crossed onto/off a CRTC the
            // plane isn't bound to. The move-only fast path
            // can't rebind across CRTCs, so route this motion
            // through one compose tick — the scene's
            // `CursorAssignment` then issues the show/hide on
            // retire. Pre-#30 the continuous idle redraw loop
            // masked this; idle desktops now stop compositing,
            // so the seam crossing must be detected here or the
            // cursor stays frozen on the CRTC it was last bound
            // to (invisible on the screen it moved onto).
        }
        // Hardware and software cursor ownership may coexist on arbitrary
        // cards. Always dispatch visible HW positions once, then repaint SW
        // outputs for a mixed cursor or membership transition.
        match b.platform.cursor_plane_move(cx, cy, hot_x, hot_y) {
            Ok(outcome) => b.handle_cursor_move_outcome(outcome),
            Err(error) if mixed => {
                log::debug!("render cursor mixed path: move failed: {error}")
            }
            Err(error) if membership_changed => {
                log::debug!("render cursor membership move: {error}")
            }
            Err(error) => log::debug!("render cursor fast path: move failed: {error}"),
        }
        if mixed || membership_changed {
            b.scene.wake_for_damage();
        }
    }

    fn scanout_allowed(b: &KmsBackend) -> bool {
        b.scanout_allowed()
    }

    fn prepare_cow_release(b: &mut KmsBackend) -> io::Result<()> {
        for scope in b.pstate.direct.active_scopes() {
            // Do this while cow_id and its storage owner are authoritative.
            // Failure leaves the COW and the direct pins untouched, so core
            // can keep the caller's claim and let the compositor retry, or
            // fail safely without freeing a scanned buffer. Success only
            // copies the scanned frame into its fallback target early, as
            // any unflip does (`submit_composed_unflip`); nothing to undo.
            b.materialize_direct_shadow_for_unflip(scope)?;
        }
        Ok(())
    }

    fn defer_cow_release(b: &mut KmsBackend) -> bool {
        if !b.pstate.direct.active_scopes().is_empty() {
            b.request_direct_unflip("release_last_overlay_window");
            b.deferred_cow_release = true;
            return true;
        }
        false
    }

    fn maybe_teardown_export(b: &mut KmsBackend, id: DrawableId) {
        b.maybe_teardown_export(id);
    }

    fn register_present_batch_wait(b: &mut KmsBackend, wait: PresentBatchWait) -> PresentBatchWait {
        b.register_present_batch_wait(wait)
    }

    fn release_present_batch_wait(b: &mut KmsBackend, batch: &PendingPresentBatch) {
        b.unregister_present_batch_fd(batch);
    }

    fn wake_present_completion(b: &KmsBackend, context: &str) {
        b.poke_present_completion_wakeup(context);
    }

    fn clear_present_completion_wake(b: &KmsBackend) {
        b.clear_present_completion_wake();
    }

    fn read_on_screen_region(b: &mut KmsBackend, rect: ash::vk::Rect2D) -> io::Result<Vec<u8>> {
        b.read_on_screen_region(rect)
    }

    fn dump_scanout_files(b: &mut KmsBackend) -> io::Result<()> {
        crate::kms::render::backend::kms::do_dump_scanout(b)
    }

    fn randr_outputs_and_modes(
        b: &mut KmsBackend,
    ) -> (
        Vec<yserver_core::randr::RandrOutput>,
        Vec<yserver_core::randr::RandrMode>,
    ) {
        KmsBackend::randr_outputs_and_modes(b)
    }

    fn randr_providers(b: &mut KmsBackend) -> Vec<yserver_core::randr::RandrProvider> {
        KmsBackend::randr_providers(b)
    }

    fn crtc_gamma_size(b: &KmsBackend, crtc: u32) -> u16 {
        KmsBackend::backend_randr_crtc_gamma_size(b, crtc)
    }

    fn set_crtc_gamma(
        b: &mut KmsBackend,
        crtc: u32,
        red: &[u16],
        green: &[u16],
        blue: &[u16],
    ) -> io::Result<()> {
        KmsBackend::backend_randr_set_crtc_gamma(b, crtc, red, green, blue)
    }

    fn get_crtc_gamma(b: &KmsBackend, crtc: u32) -> (Vec<u16>, Vec<u16>, Vec<u16>) {
        KmsBackend::backend_randr_get_crtc_gamma(b, crtc)
    }

    fn on_scanout_render_completion(b: &mut KmsBackend, state: &mut ServerState) {
        KmsBackend::backend_scanout_on_scanout_render_completion(b, state)
    }

    fn on_page_flip_ready(b: &mut KmsBackend, state: &mut ServerState, drm_fd: std::os::fd::RawFd) {
        KmsBackend::backend_scanout_on_page_flip_ready(b, state, drm_fd)
    }

    fn before_block(b: &mut KmsBackend) {
        b.reclaim_retired_gpu_work();
        b.platform
            .emit_tearing_summary_if_due(b.pstate.tearing.is_on(), b.telemetry.avg_gpu_render_ns());
    }

    fn flush_before_damage_notify(b: &mut KmsBackend) {
        b.flush_before_damage_exposure();
    }

    fn next_wakeup(b: &KmsBackend) -> Option<std::time::Instant> {
        KmsBackend::backend_scanout_next_wakeup(b)
            .into_iter()
            .chain(b.platform.next_software_vblank_deadline())
            .min()
    }

    fn maybe_composite(b: &mut KmsBackend) -> io::Result<()> {
        KmsBackend::backend_scanout_maybe_composite(b)
    }

    fn dump_scanout(b: &mut KmsBackend) {
        KmsBackend::backend_readback_dump_scanout(b)
    }

    fn report_export_holders(
        b: &mut KmsBackend,
        core: &dyn Fn() -> yserver_core::backend::export_holders::CoreHolders,
    ) -> bool {
        KmsBackend::backend_export_report_export_holders(b, core)
    }

    fn note_present_pixmap(b: &mut KmsBackend, src_pixmap_xid: u32, dst_window_xid: u32) {
        KmsBackend::backend_present_note_present_pixmap(b, src_pixmap_xid, dst_window_xid)
    }

    fn note_present_scanout_candidate(b: &mut KmsBackend, candidate: PresentScanoutCandidate) {
        b.observe_scanout_m0(candidate);
    }

    fn try_present_direct(
        b: &mut KmsBackend,
        candidate: PresentScanoutCandidate,
        event: yserver_core::backend::CompletedPresentEvent,
    ) -> io::Result<bool> {
        let result = KmsBackend::backend_present_try_present_direct(b, candidate, event);
        b.note_present_outcome(candidate.src_host_xid, matches!(result, Ok(true)));
        result
    }

    fn arm_present_source_wait(
        b: &mut KmsBackend,
        src_pixmap_host_xid: u32,
        dst_window_host_xid: u32,
    ) -> io::Result<PresentSourceWait> {
        KmsBackend::backend_present_arm_present_source_wait(
            b,
            src_pixmap_host_xid,
            dst_window_host_xid,
        )
    }

    fn arm_present_syncobj_wait(
        b: &mut KmsBackend,
        src_pixmap_host_xid: u32,
        dst_window_host_xid: u32,
        acquire_syncobj: u32,
        acquire_value: u64,
    ) -> io::Result<PresentSourceWait> {
        KmsBackend::backend_present_arm_present_syncobj_wait(
            b,
            src_pixmap_host_xid,
            dst_window_host_xid,
            acquire_syncobj,
            acquire_value,
        )
    }

    fn drain_ready_present_source_waits(b: &mut KmsBackend) -> Vec<u64> {
        KmsBackend::backend_present_drain_ready_present_source_waits(b)
    }

    fn begin_ready_present_destination_write(b: &mut KmsBackend, wait_id: u64) {
        KmsBackend::backend_present_begin_ready_present_destination_write(b, wait_id)
    }

    fn finish_present_source_wait(b: &mut KmsBackend, wait_id: u64) {
        KmsBackend::backend_present_finish_present_source_wait(b, wait_id)
    }

    fn present_flip_in_flight(b: &KmsBackend, crtc_id: u32) -> bool {
        b.present_crtc_output(crtc_id)
            .is_some_and(|(output_idx, _)| b.present_flip_in_flight_for_output(output_idx))
    }

    fn present_display_idle(b: &KmsBackend, crtc_id: u32) -> bool {
        b.present_crtc_key(crtc_id)
            .is_some_and(|crtc_key| b.present_completion_is_idle_for(crtc_key))
    }

    fn present_absolute_vblank_arm_supported(b: &KmsBackend, crtc_id: u32) -> bool {
        KmsBackend::backend_present_present_absolute_vblank_arm_supported(b, crtc_id)
    }

    fn arm_present_absolute_vblank(
        b: &mut KmsBackend,
        randr_crtc_id: u32,
        targets: &[u64],
    ) -> io::Result<usize> {
        KmsBackend::backend_present_arm_present_absolute_vblank(b, randr_crtc_id, targets)
    }

    fn present_scanout_blackout(b: &KmsBackend) -> bool {
        !(b.scanout_allowed() && b.kms_outputs_active)
    }

    fn poll_fds(b: &KmsBackend) -> Vec<(std::os::fd::RawFd, BackendFdKind)> {
        // Direct mode only: DRM fd + present-completion epfd. libinput runs
        // on its own thread, not the core poll.
        b.platform.poll_fds()
    }

    fn vt_switching_armed(b: &KmsBackend) -> bool {
        b.pstate.vt_switching_armed
    }

    fn set_input_sender(b: &mut KmsBackend, sender: yserver_core::core_loop::CoreSender) {
        KmsBackend::backend_session_set_input_sender(b, sender)
    }

    fn request_vt_switch(b: &mut KmsBackend, vt: u32) {
        KmsBackend::backend_session_request_vt_switch(b, vt)
    }

    fn begin_vt_release(b: &mut KmsBackend) -> bool {
        KmsBackend::backend_session_begin_vt_release(b)
    }

    fn finish_vt_release(
        b: &mut KmsBackend,
        state: &mut ServerState,
        input_inventory: &yserver_core::core_loop::input_inventory::InputInventory,
    ) {
        KmsBackend::backend_session_finish_vt_release(b, state, input_inventory)
    }

    fn on_vt_acquire(b: &mut KmsBackend, state: &mut ServerState) {
        KmsBackend::backend_session_on_vt_acquire(b, state)
    }

    fn on_display_hotplug(b: &mut KmsBackend, state: &mut ServerState) {
        KmsBackend::backend_randr_on_display_hotplug(b, state)
    }

    fn reprobe_connectors(b: &mut KmsBackend, state: &mut ServerState) -> io::Result<()> {
        KmsBackend::backend_randr_reprobe_connectors(b, state)
    }

    fn set_provider_output_source(
        b: &mut KmsBackend,
        state: &mut ServerState,
        provider: u32,
        source_provider: Option<u32>,
    ) -> io::Result<bool> {
        KmsBackend::backend_randr_set_provider_output_source(b, state, provider, source_provider)
    }

    fn begin_crtc_config(
        b: &mut KmsBackend,
        output_id: u32,
        connector: &str,
        mode: Option<yserver_core::backend::ModeSpec>,
        x: i32,
        y: i32,
    ) -> io::Result<CrtcConfigApply> {
        KmsBackend::backend_randr_begin_crtc_config(b, output_id, connector, mode, x, y)
    }

    fn drain_ready_crtc_configs(b: &mut KmsBackend) -> Vec<CrtcConfigToken> {
        KmsBackend::backend_randr_drain_ready_crtc_configs(b)
    }

    fn finish_crtc_config(b: &mut KmsBackend, token: CrtcConfigToken) -> io::Result<bool> {
        KmsBackend::backend_randr_finish_crtc_config(b, token)
    }

    fn cancel_crtc_config(b: &mut KmsBackend, token: CrtcConfigToken) {
        KmsBackend::backend_randr_cancel_crtc_config(b, token)
    }

    fn apply_crtc_config(
        b: &mut KmsBackend,
        output_id: u32,
        connector: &str,
        mode: Option<yserver_core::backend::ModeSpec>,
        x: i32,
        y: i32,
    ) -> io::Result<bool> {
        KmsBackend::backend_randr_apply_crtc_config(b, output_id, connector, mode, x, y)
    }

    fn refresh_randr_state_set_time(
        b: &mut KmsBackend,
        state: &mut yserver_core::server::ServerState,
        set_time: u32,
    ) {
        // A CRTC set bumps lastSetTime (to the client timestamp) but NOT
        // lastConfigTime (the available configuration didn't change).
        b.rebuild_randr_state(state, Some(set_time), false);
    }

    fn randr_layout_changed(b: &mut KmsBackend, state: &mut ServerState) {
        KmsBackend::backend_randr_randr_layout_changed(b, state)
    }

    fn output_identity(b: &KmsBackend, output_id: u32) -> Option<(Vec<u8>, String)> {
        b.pstate.output_identity_by_id.get(&output_id).cloned()
    }

    fn set_logical_screen_size(b: &mut KmsBackend, w: u16, h: u16) -> io::Result<()> {
        KmsBackend::backend_randr_set_logical_screen_size(b, w, h)
    }

    fn poll_deferred_input(b: &mut KmsBackend, state: &mut ServerState) {
        b.service_software_vblanks();
        KmsBackend::backend_session_poll_deferred_input(b, state)
    }

    fn start_device_config(
        b: &mut KmsBackend,
        source: yserver_core::xinput::InputSourceId,
        change: yserver_core::xinput::libinput_props::DeviceConfigChange,
        cancel: yserver_core::xinput::libinput_props::DeviceConfigCancelToken,
    ) -> Result<
        yserver_core::xinput::libinput_props::DeviceConfigStart,
        yserver_core::xinput::libinput_props::DeviceConfigError,
    > {
        KmsBackend::backend_session_start_device_config(b, source, change, cancel)
    }

    fn supports_dmabuf_export(b: &KmsBackend) -> bool {
        b.pstate.dmabuf_export_supported
    }

    fn glx_vendor_names(b: &KmsBackend) -> &'static str {
        KmsBackend::backend_export_glx_vendor_names(b)
    }

    fn acquire_glx_pixmap_export(b: &mut KmsBackend, host_xid: u32) {
        KmsBackend::acquire_glx_pixmap_export(b, host_xid);
    }

    fn release_glx_pixmap_export(b: &mut KmsBackend, host_xid: u32) {
        KmsBackend::release_glx_pixmap_export(b, host_xid);
    }

    fn promote_pixmap_exportable(b: &mut KmsBackend, host_xid: u32) -> bool {
        KmsBackend::promote_pixmap_exportable(b, host_xid)
    }

    fn dri3_open(b: &mut KmsBackend, drawable: u32) -> io::Result<std::os::fd::OwnedFd> {
        KmsBackend::backend_export_dri3_open(b, drawable)
    }

    fn glx_available(_b: &KmsBackend) -> bool {
        true
    }

    fn dri3_capabilities(b: &KmsBackend) -> Dri3Caps {
        KmsBackend::backend_export_dri3_capabilities(b)
    }

    fn dri3_import_pixmap(
        b: &mut KmsBackend,
        fd: std::os::fd::OwnedFd,
        width: u16,
        height: u16,
        stride: u32,
        offset: u32,
        modifier: Dri3ImportModifier,
        depth: u8,
        bpp: u8,
    ) -> io::Result<PixmapHandle> {
        KmsBackend::backend_export_dri3_import_pixmap(
            b, fd, width, height, stride, offset, modifier, depth, bpp,
        )
    }

    fn dri3_supported_modifiers(
        b: &KmsBackend,
        window: u32,
        depth: u8,
        bpp: u8,
    ) -> (Vec<u64>, Vec<u64>) {
        KmsBackend::backend_export_dri3_supported_modifiers(b, window, depth, bpp)
    }

    fn dri3_export_pixmap(
        b: &mut KmsBackend,
        host_xid: u32,
    ) -> io::Result<(u32, u16, u16, u16, u8, u8, std::os::fd::OwnedFd)> {
        KmsBackend::backend_export_dri3_export_pixmap(b, host_xid)
    }

    fn dri3_export_pixmap_buffers(
        b: &mut KmsBackend,
        host_xid: u32,
    ) -> io::Result<Dri3PixmapExport> {
        KmsBackend::backend_export_dri3_export_pixmap_buffers(b, host_xid)
    }

    fn dri3_begin_export_pixmap_buffers(
        b: &mut KmsBackend,
        host_xid: u32,
    ) -> io::Result<yserver_core::backend::Dri3ExportBegin> {
        KmsBackend::backend_export_dri3_begin_export(b, host_xid)
    }

    fn drain_ready_dri3_exports(b: &mut KmsBackend) -> Vec<yserver_core::backend::Dri3ExportToken> {
        KmsBackend::backend_export_drain_ready_dri3_exports(b)
    }

    fn finish_dri3_export(
        b: &mut KmsBackend,
        token: yserver_core::backend::Dri3ExportToken,
    ) -> io::Result<Dri3PixmapExport> {
        KmsBackend::backend_export_finish_dri3_export(b, token)
    }

    fn release_dri3_export(b: &mut KmsBackend, token: yserver_core::backend::Dri3ExportToken) {
        KmsBackend::backend_export_release_dri3_export(b, token);
    }

    fn dri3_fence_from_fd(
        b: &mut KmsBackend,
        fence_xid: u32,
        fd: std::os::fd::OwnedFd,
    ) -> io::Result<()> {
        KmsBackend::backend_export_dri3_fence_from_fd(b, fence_xid, fd)
    }

    fn dri3_trigger_fence(b: &mut KmsBackend, fence_xid: u32) -> io::Result<()> {
        KmsBackend::backend_export_dri3_trigger_fence(b, fence_xid)
    }

    fn dri3_fence_triggered(b: &KmsBackend, fence_xid: u32) -> Option<bool> {
        KmsBackend::backend_export_dri3_fence_triggered(b, fence_xid)
    }

    fn dri3_reset_fence(b: &mut KmsBackend, fence_xid: u32) {
        KmsBackend::backend_export_dri3_reset_fence(b, fence_xid)
    }

    fn dri3_destroy_fence(b: &mut KmsBackend, fence_xid: u32) {
        KmsBackend::backend_export_dri3_destroy_fence(b, fence_xid)
    }

    fn dri3_xshmfence_handle(
        b: &KmsBackend,
        fence_xid: u32,
    ) -> Option<std::sync::Arc<dyn yserver_core::backend::XshmfenceHandle>> {
        KmsBackend::backend_export_dri3_xshmfence_handle(b, fence_xid)
    }

    fn dri3_syncobj_handle(
        b: &KmsBackend,
        syncobj_xid: u32,
    ) -> Option<std::sync::Arc<dyn yserver_core::backend::SyncobjHandle>> {
        KmsBackend::backend_export_dri3_syncobj_handle(b, syncobj_xid)
    }

    fn dri3_syncobj_owned(
        b: &KmsBackend,
        client_id: yserver_protocol::x11::ClientId,
        syncobj_xid: u32,
    ) -> bool {
        KmsBackend::backend_export_dri3_syncobj_owned(b, client_id, syncobj_xid)
    }

    fn dri3_fd_from_fence(b: &mut KmsBackend, fence_xid: u32) -> io::Result<std::os::fd::OwnedFd> {
        KmsBackend::backend_export_dri3_fd_from_fence(b, fence_xid)
    }

    fn dri3_import_syncobj(
        b: &mut KmsBackend,
        client_id: yserver_protocol::x11::ClientId,
        syncobj_xid: u32,
        fd: std::os::fd::OwnedFd,
    ) -> io::Result<()> {
        KmsBackend::backend_export_dri3_import_syncobj(b, client_id, syncobj_xid, fd)
    }

    fn dri3_free_syncobj(
        b: &mut KmsBackend,
        client_id: yserver_protocol::x11::ClientId,
        syncobj_xid: u32,
    ) -> io::Result<()> {
        KmsBackend::backend_export_dri3_free_syncobj(b, client_id, syncobj_xid)
    }

    fn dri3_signal_syncobj(b: &mut KmsBackend, syncobj_xid: u32, value: u64) -> io::Result<()> {
        KmsBackend::backend_export_dri3_signal_syncobj(b, syncobj_xid, value)
    }

    fn drain_completed_present_events(
        b: &mut KmsBackend,
    ) -> Vec<yserver_core::backend::CompletedPresentEvent> {
        KmsBackend::backend_present_drain_completed_present_events(b)
    }

    fn drain_retired_present_idle_events(
        b: &mut KmsBackend,
    ) -> Vec<yserver_core::backend::CompletedPresentEvent> {
        let mut idled = std::mem::take(&mut b.pstate.direct.root_mut().idled);
        for owner in &mut b.pstate.direct.outputs {
            idled.append(&mut owner.idled);
        }
        idled
    }

    fn present_crtc_clock_epoch(b: &KmsBackend, crtc_id: u32) -> u64 {
        KmsBackend::backend_present_present_crtc_clock_epoch(b, crtc_id)
    }

    fn present_get_ust_msc(b: &KmsBackend, crtc_id: u32) -> (u64, u64) {
        KmsBackend::backend_present_present_get_ust_msc(b, crtc_id)
    }

    fn present_get_completion_clock(
        b: &KmsBackend,
        crtc_id: u32,
    ) -> yserver_core::backend::PresentClockSample {
        KmsBackend::backend_present_present_get_completion_clock(b, crtc_id)
    }

    fn arm_idle_vblanks(
        b: &mut KmsBackend,
        crtc_id: u32,
        target_mscs: &[u64],
    ) -> std::io::Result<usize> {
        b.arm_idle_vblanks_ioctl(crtc_id, target_mscs)
    }

    fn arm_present_completion_idle_vblanks(
        b: &mut KmsBackend,
        crtc_id: u32,
        target_mscs: &[u64],
    ) -> std::io::Result<usize> {
        KmsBackend::backend_present_arm_present_completion_idle_vblanks(b, crtc_id, target_mscs)
    }

    fn present_capabilities(b: &KmsBackend, window: u32, crtc_id: u32) -> PresentCaps {
        KmsBackend::backend_present_present_capabilities(b, window, crtc_id)
    }

    fn dpms_capable(_b: &KmsBackend) -> bool {
        true
    }

    fn set_dpms_power(b: &mut KmsBackend, level: u8) -> std::io::Result<()> {
        KmsBackend::backend_session_set_dpms_power(b, level)
    }
}

impl KmsBackend {
    /// Direct-mode wiring: hand over the relay the input thread ends
    /// of which `input_thread::run` polls. Called from `lib.rs` when
    /// spawning the libinput thread.
    pub fn set_led_relay(&mut self, relay: std::sync::Arc<crate::input::LedRelay>) {
        self.pstate.led_relay = Some(relay);
    }
}

/// Lit XKB indicators (Caps Lock bit 0, Num Lock bit 1, Scroll Lock bit 2)
/// as `input::Led` bits.
fn led_bits_for_indicators(lit: u32) -> u32 {
    let mut leds = input::Led::empty();
    for (bit, led) in [
        (0, input::Led::CAPSLOCK),
        (1, input::Led::NUMLOCK),
        (2, input::Led::SCROLLLOCK),
    ] {
        if lit & (1 << bit) != 0 {
            leds |= led;
        }
    }
    leds.bits()
}

#[cfg(test)]
impl KmsBackend {
    /// Current lock-LED state derived from `xkb_state`, as `input::Led` bits.
    /// Split from `sync_keyboard_leds` so the no-Vk test fixture can pin the
    /// XKB→LED mapping without a libinput device.
    pub(in crate::kms::render::backend) fn current_led_bits(&self) -> u32 {
        led_bits_for_indicators(self.lit_indicators())
    }
}
