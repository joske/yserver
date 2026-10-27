//! Per-client disconnect cleanup, lifted out of `nested::handle_client`'s
//! closing block. Tears down every piece of state that referenced the
//! departing client (resources owned by it, per-client event masks,
//! grabs, selections, MIT-SHM segments, …) plus their host counterparts
//! (subwindows, fonts, pixmaps, RENDER pictures + glyphsets).
//!
//! Invoked from `run_core` on `Message::ClientDisconnected`, and also
//! when `process_request` reports `RequestOutcome::Disconnect` for a
//! peer that overflowed its outbound buffer.

use yserver_protocol::x11::{ClientId, ResourceId};

use crate::{backend::Backend, resources::MapState, server::ServerState};

/// The host-pixmap frees one teardown made: `freed` went back to the backend,
/// `deferred` was held because [`ResourceTable::host_xid_still_referenced`]
/// found a surviving reference.
///
/// [`ResourceTable::host_xid_still_referenced`]: crate::resources::ResourceTable::host_xid_still_referenced
#[derive(Debug, Default)]
pub struct HostPixmapFrees {
    pub freed: Vec<u32>,
    pub deferred: Vec<u32>,
}

/// The single host-pixmap orphan gate. Every teardown path routes its
/// candidates through here so the decision stays
/// `ResourceTable::host_xid_still_referenced` and never a per-call-site
/// subset — the omissions were the #133 use-after-free/leak pair.
/// `last_names` carry a freed name's last alias ref; a deferred one is recorded.
pub(crate) fn free_orphaned_host_pixmaps(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    mut candidates: Vec<u32>,
    last_names: &[u32],
    mut report: Option<&mut HostPixmapFrees>,
) {
    // Deduplicated, because a tile can arrive from more than one source — a
    // freed pixmap resource AND a destroyed window's background or border.
    // Freeing it twice is the same broken contract the CWA path had.
    //
    // Neither source may be freed unconditionally: another client's window may
    // still name the tile as its background or border, and a GC may still hold
    // it as a tile / stipple / clip mask. The disconnect path checked nothing
    // at all, so `A` creating a tile, `B` bordering with it and `A`
    // disconnecting left `B` sampling freed GPU storage (#133).
    candidates.sort_unstable();
    candidates.dedup();
    for xid in candidates {
        if let Some(handle) = crate::backend::PixmapHandle::from_raw(xid)
            && state.resources.host_xid_still_referenced(handle)
        {
            if last_names.contains(&xid) {
                state.resources.defer_name_ref(handle);
            }
            if let Some(report) = report.as_deref_mut() {
                report.deferred.push(xid);
            }
            continue;
        }
        match crate::backend::PixmapHandle::from_raw(xid) {
            Some(handle) if last_names.contains(&xid) => {
                let _ = backend.release_window_pixmap_name(None, handle);
            }
            _ => {
                let _ = backend.free_pixmap(None, xid);
            }
        }
        state.resources.host_pixmap_freed(xid);
        if let Some(report) = report.as_deref_mut() {
            report.freed.push(xid);
        }
    }
}

/// One alias ref per removed name (Xorg `compext.c:260`); each LAST ref goes to the orphan gate.
pub(crate) fn release_removed_names(
    state: &ServerState,
    backend: &mut dyn Backend,
    origin: Option<crate::backend::OriginContext>,
    names: &[crate::backend::PixmapHandle],
) -> Vec<u32> {
    let mut per_backing: std::collections::BTreeMap<u32, usize> = std::collections::BTreeMap::new();
    for name in names {
        *per_backing.entry(name.as_raw()).or_default() += 1;
    }
    let mut last = Vec::new();
    for (xid, mut refs) in per_backing {
        let Some(handle) = crate::backend::PixmapHandle::from_raw(xid) else {
            continue;
        };
        // Another live name, or an already-deferred ref, keeps the backing alive for the gate.
        if !state.resources.host_xid_named_by_pixmap(handle)
            && !state.resources.has_deferred_name_ref(handle)
        {
            refs -= 1;
            last.push(xid);
        }
        for _ in 0..refs {
            if let Err(err) = backend.release_window_pixmap_name(origin, handle) {
                log::warn!("release_window_pixmap_name(0x{xid:x}) failed: {err}");
            }
        }
    }
    last
}

/// Drop every server-side resource owned by `client_id` and free the
/// corresponding host objects.
///
/// If the client previously set `SetCloseDownMode(RetainPermanent |
/// RetainTemporary)`, its non-window resources (pixmaps, GCs, fonts,
/// cursors, pictures, glyphsets) survive with their original `owner:
/// ClientId` intact and the client_id is recorded in
/// `state.zombie_clients`. Connection-tied state (event masks, grabs,
/// selections, extension tables) is always torn down — a retained
/// client has no socket to receive on. The retained resources stay
/// findable by ID until either `KillClient(resource_owned_by_this_id)`
/// or, for RetainTemporary only, `KillClient(AllTemporary)`.
pub fn process_disconnect(state: &mut ServerState, backend: &mut dyn Backend, client_id: ClientId) {
    process_disconnect_reporting(state, backend, client_id, None);
}

/// [`process_disconnect`], additionally reporting which host pixmaps it freed
/// and which it held back because a surviving reference still named them.
///
/// Only the reset path needs the report: it tears down every client, so a
/// deferral taken while a later client still held the tile has to be
/// re-examined once that client is gone too. The normal disconnect passes
/// `None` and behaves exactly as before.
pub fn process_disconnect_reporting(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    client_id: ClientId,
    host_pixmap_frees: Option<&mut HostPixmapFrees>,
) {
    // Idempotent: a client can be disconnected twice in quick succession
    // (write-side EPIPE from process_request races the reader thread's
    // EOF → Message::ClientDisconnected). The first call removes the
    // entry from state.clients; the second sees None and bails.
    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return;
    };
    // Already gone and still here: an earlier teardown of this client hit a
    // fatal failure and stopped (`fatal_teardown`); the server is shutting
    // down, and nothing may run that teardown again.
    if client.gone {
        return;
    }
    // Xorg `clientGone` (`dix/dispatch.c:3518`): nothing emitted from here on
    // reaches the departing client.
    client.gone = true;
    let close_mode = state.close_down_modes.remove(&client_id.0).unwrap_or(0);
    let retain = close_mode == 1 || close_mode == 2;
    if state.server_grab_owner == Some(client_id) {
        state.server_grab_owner = None;
    }
    log::debug!(
        "process_disconnect: client {} close_mode={}",
        client_id.0,
        close_mode
    );
    // Force the kernel socket closed so a blocked reader thread and the
    // peer both observe EOF even if other UnixStream clones still exist.
    if let Some(client) = state.clients.get(&client_id.0) {
        if let Ok(writer) = client.writer.lock() {
            let _ = writer.shutdown(std::net::Shutdown::Both);
        }
        if let Some(ctrl) = &client.reader_control {
            let _ = ctrl.send(crate::server::ReaderControl::Shutdown);
        }
    }

    // RECORD: Xorg's ClientStateGone callback runs before any resource is
    // freed, and needs the client's base and sequence.
    crate::core_loop::record::client_disconnected(state, client_id);

    // Audit #9 (docs/protocol-audit-2026-05-19.md) — before the
    // disconnecting client's windows are destroyed, fire
    // `XFixesSelectionNotify(SelectionClientClose)` to any subscriber
    // whose mask includes the ClientClose bit, then clear those
    // ownership entries. This must run BEFORE the destroy loop below
    // because `fanout_xfixes_selection_client_close_for_client`
    // resolves selection owners via `state.resources.window_owner`,
    // and the destroy loop is about to evict those windows.
    crate::core_loop::process_request::fanout_xfixes_selection_client_close_for_client(
        state, client_id,
    );

    // Xorg `ReleaseActiveGrabs` (`dix/dispatch.c:3496`) runs before
    // `FreeClientResources` (`:3551`): the client's own grabs are gone before
    // any of its windows.
    state.button_grabs.retain(|g| g.owner != client_id);
    state.key_grabs.retain(|g| g.owner != client_id);
    let dynamic_pointer_grabs: Vec<u16> = state
        .xi2_pointer_grabs
        .iter()
        .filter_map(|(device, grab)| (grab.owner == client_id).then_some(*device))
        .collect();
    let dynamic_keyboard_grabs: Vec<u16> = state
        .xi2_keyboard_grabs
        .iter()
        .filter_map(|(device, grab)| (grab.owner == client_id).then_some(*device))
        .collect();
    for device in dynamic_pointer_grabs {
        state.xi2_pointer_grabs.remove(&device);
        if let Some(freeze) = state.xi1_frozen.get_mut(&device) {
            freeze.state = crate::server::Xi1SyncState::Thawed;
            freeze.stored = None;
        }
        crate::core_loop::pointer_fanout::xi1_core_grab_bridge_release(state, device, client_id);
        state.reattach_xi2_slave(device);
    }
    for device in dynamic_keyboard_grabs {
        state.xi2_keyboard_grabs.remove(&device);
        if let Some(freeze) = state.xi1_frozen.get_mut(&device) {
            freeze.state = crate::server::Xi1SyncState::Thawed;
            freeze.stored = None;
        }
        crate::core_loop::pointer_fanout::xi1_core_grab_bridge_release(state, device, client_id);
        state.reattach_xi2_slave(device);
    }
    let released_pointer_grab = state
        .active_pointer_grab
        .is_some_and(|grab| grab.owner == client_id);
    if released_pointer_grab {
        state.clear_pointer_grab();
        if let Some(freeze) = state
            .xi1_frozen
            .get_mut(&crate::xinput::DEVICEID_MASTER_POINTER)
        {
            freeze.stored = None;
            freeze.state = crate::server::Xi1SyncState::Thawed;
            freeze.other = None;
        }
        crate::core_loop::pointer_fanout::xi1_core_grab_bridge_release(
            state,
            crate::xinput::DEVICEID_MASTER_POINTER,
            client_id,
        );
    }
    // Xorg ReleaseActiveGrabs (CloseDownClient): a disconnecting
    // client's ACTIVE grabs must go too, or the stale record makes
    // every later GrabPointer/GrabKeyboard return AlreadyGrabbed.
    if released_pointer_grab {
        // Xorg DeactivatePointerGrab on client teardown reverts the
        // sprite from the grab cursor back to the window/default cursor.
        let _ = backend.set_grab_cursor(None, None);
    }
    if state
        .active_keyboard_grab
        .is_some_and(|g| g.owner == client_id)
    {
        state.active_keyboard_grab = None;
        if let Some(freeze) = state
            .xi1_frozen
            .get_mut(&crate::xinput::DEVICEID_MASTER_KEYBOARD)
        {
            freeze.stored = None;
            freeze.state = crate::server::Xi1SyncState::Thawed;
            freeze.other = None;
        }
        crate::core_loop::pointer_fanout::xi1_core_grab_bridge_release(
            state,
            crate::xinput::DEVICEID_MASTER_KEYBOARD,
            client_id,
        );
    }
    // XI 1.x grab teardown: drop the client's passive grabs, release
    // its active device grabs, and thaw any devices its grabs froze —
    // a leaked freeze queues device events forever and hangs every
    // later test/client waiting on input.
    state.xi1_passive_grabs.retain(|g| g.owner != client_id);
    let released: Vec<u16> = state
        .xi1_active_grabs
        .iter()
        .filter(|(_, g)| g.owner == client_id)
        .map(|(d, _)| *d)
        .collect();
    for dev in &released {
        // Deactivation resets the device's sync state, releases the
        // paired device if it was held on this grab's behalf
        // (other_devices_mode), and flushes the queues.
        crate::core_loop::pointer_fanout::xi1_deactivate_device_grab(state, *dev);
    }
    if released.is_empty() && state.xi1_active_grabs.is_empty() {
        // No active grabs anywhere, yet freezes linger (e.g. a sync
        // passive grab's owner died between activation and release):
        // clear them, or device events queue forever and every later
        // client waiting on input hangs.
        let devs: Vec<u16> = state
            .xi1_frozen
            .iter()
            .filter(|(_, f)| f.frozen())
            .map(|(d, _)| *d)
            .collect();
        for dev in devs {
            let xid_map = backend.xid_map().clone();
            crate::core_loop::pointer_fanout::xi1_thaw_device(state, backend, &xid_map, dev);
        }
    }
    let xid_map = backend.xid_map().clone();
    crate::core_loop::pointer_fanout::xi1_compute_freezes(state, backend, &xid_map);

    let hit_barriers: Vec<(u32, crate::server::PointerBarrier)> = state
        .pointer_barriers
        .iter()
        .filter(|(_, barrier)| barrier.owner == client_id && barrier.hit)
        .map(|(barrier_xid, barrier)| (*barrier_xid, barrier.clone()))
        .collect();
    // Xorg BarrierFreeBarrier emits the released leave with the CURRENT
    // time + sprite position (xibarriers.c:668/760), not the last-hit
    // values. Capture once; they don't change across the loop.
    let leave_time = state.timestamp_now();
    let (leave_rx, leave_ry) = state.pointer_root;
    for (barrier_xid, barrier) in hit_barriers {
        let _dropped = crate::core_loop::pointer_fanout::emit_barrier_event(
            state,
            barrier_xid,
            barrier.owner,
            barrier.window,
            26,
            leave_time,
            barrier.event_id,
            0,
            1,
            0,
            i32::from(leave_rx),
            i32::from(leave_ry),
            0.0,
            0.0,
        );
    }

    let (all_destroyed, attr_pixmap_xids) = match destroy_owned_windows(state, backend, client_id) {
        Ok(destroyed) => destroyed,
        Err(err) => {
            abort_teardown(state, "disconnect", client_id, &err);
            return;
        }
    };

    let removed = if retain {
        // Resources keep their original `owner: ClientId`. Tracking
        // the client_id in zombie_clients lets KillClient resolve
        // ownership back to this specific creator.
        state.zombie_clients.insert(client_id.0, close_mode);
        crate::resources::ClientRemovedResources::default()
    } else {
        state
            .resources
            .remove_non_window_resources_owned_by(client_id)
    };
    // Snapshot the resource-id base before the client entry is removed.
    // Recycled below only for `!retain` clients — see IdAllocator::release.
    let released_base = state.clients.get(&client_id.0).map(|c| c.resource_id_base);
    state.clients.remove(&client_id.0);
    if !retain && let Some(base) = released_base {
        state.id_allocator.release(base);
    }

    let dead_windows: std::collections::HashSet<ResourceId> =
        all_destroyed.iter().copied().collect();
    state
        .xfixes_regions
        .retain(|_, region| region.owner != client_id);
    state
        .pointer_barriers
        .retain(|_, barrier| barrier.owner != client_id);
    state
        .xfixes_selection_masks
        .retain(|(owner, _, _), _| *owner != client_id.0);
    state
        .xfixes_cursor_masks
        .retain(|(owner, _), _| *owner != client_id.0);
    crate::core_loop::process_request::release_xfixes_client_state(state, backend, client_id);
    state
        .shape_windows
        .retain(|window, _| !dead_windows.contains(window));
    state
        .shape_select_masks
        .retain(|(owner, window), _| *owner != client_id.0 && !dead_windows.contains(window));
    // SYNC: the client's own await dies with it (Xorg `FreeAwait`); its
    // alarms are destroyed, telling the other clients that selected them
    // (`FreeAlarm`), and it leaves other alarms' event lists
    // (`FreeAlarmClient`); its counters and fences are destroyed, which
    // fires other clients' awaits on them with destroyed CounterNotify and
    // deactivates alarms watching its counters (Xorg `FreeCounter` /
    // `miSyncDestroyFence`).
    state.sync_awaits.remove(&client_id.0);
    crate::core_loop::sync_await::release_client_alarms(state, client_id);
    let mut dead_counters: Vec<(u32, i64)> = state
        .sync_counters
        .iter()
        .filter(|(_, counter)| counter.owner == client_id)
        .map(|(id, counter)| (*id, counter.value))
        .collect();
    dead_counters.sort_unstable();
    for (counter, last) in dead_counters {
        state.sync_counters.remove(&counter);
        crate::core_loop::sync_await::counter_destroyed(state, counter, last);
    }
    let mut dead_fences: Vec<u32> = state
        .sync_fences
        .iter()
        .filter(|(_, fence)| fence.owner == client_id)
        .map(|(id, _)| *id)
        .collect();
    dead_fences.sort_unstable();
    for fence in dead_fences {
        crate::core_loop::sync_await::fence_destroyed(state, fence);
        state.sync_fences.remove(&fence);
        backend.dri3_destroy_fence(fence);
    }
    state.glx_contexts.retain(|_, c| c.owner != client_id);
    // Release export-lifetime refs for any GLXPixmaps the client still held.
    // Use the host_xid stored at glXCreatePixmap acquire time — NOT a
    // re-resolution via resources.pixmap(x_drawable). The X pixmap may have
    // been freed (FreePixmap) before disconnect, in which case re-resolution
    // would return None and the export ref would leak forever.
    let owned_glx_export_host_xids: Vec<u32> = state
        .glx_drawables
        .values()
        .filter(|d| d.owner == client_id)
        .filter_map(|d| d.glx_export_host_xid)
        .collect();
    for host_xid in owned_glx_export_host_xids {
        backend.release_glx_pixmap_export(host_xid);
    }
    state.glx_drawables.retain(|_, d| d.owner != client_id);
    state
        .damage_objects
        .retain(|_, damage| damage.owner != client_id && !dead_windows.contains(&damage.drawable));
    // The departing client's redirect records go with its resources (Xorg
    // frees each CompClientWindowRec / CompSubwindowsRec resource); every
    // window that lost one is re-checked, so one left unredirected gets its
    // scene participation back (an abnormal compositor exit would otherwise
    // leave every Manual window invisible).
    let dead: Vec<ResourceId> = dead_windows.iter().copied().collect();
    state.composite_redirects.forget_windows(&dead);
    let parents = state.composite_redirects.subwindows_parents_of(client_id);
    let released = crate::core_loop::process_request::manual_subwindows_release_regions(
        state, client_id, &parents,
    );
    let changed = state.composite_redirects.forget_client(client_id);
    crate::core_loop::process_request::clear_after_manual_subwindows_release(
        state, backend, None, &released,
    );
    for (window, before) in changed {
        crate::core_loop::process_request::sync_redirect_backing(
            state,
            backend,
            None,
            window,
            before,
            crate::core_loop::process_request::BackingDamage::Allocation,
        );
    }
    state.present_event_selections.retain(|_, selection| {
        selection.owner != client_id && !dead_windows.contains(&selection.window)
    });
    // Parked NotifyMSC requests from this client would otherwise be re-scanned
    // every vblank forever (the client is gone and can never be satisfied-away).
    state.present_pending_msc.retain(|p| p.owner != client_id);
    // Release + drop any parked/gated Present completions this client owns.
    for p in state
        .present_pending_complete
        .iter()
        .filter(|p| p.event.client_id == client_id)
    {
        backend.signal_present_wake(p.event.present_id);
    }
    state
        .present_pending_complete
        .retain(|p| p.event.client_id != client_id);
    for (&id, g) in state.present_complete_gate.iter() {
        if g.owner == client_id {
            backend.signal_present_wake(id);
        }
    }
    state
        .present_complete_gate
        .retain(|_, g| g.owner != client_id);
    // A producer fence may never signal after its client disappears (GPU
    // reset, killed process). Abandon those parked copies now and release the
    // backend's exact source-drawable pins instead of leaking them forever.
    let abandoned_present_ids: Vec<u64> = state
        .present_pending_exec
        .iter()
        .filter_map(|(&pid, entry)| (entry.pending.client_id == client_id).then_some(pid))
        .collect();
    for pid in abandoned_present_ids {
        if let Some(entry) = state.present_pending_exec.remove(&pid) {
            if let Some(wid) = entry.wait_id {
                backend.finish_present_source_wait(wid);
                state.present_wait_to_id.remove(&wid);
            }
            if let Some(pin) = entry.pin {
                backend.release_present_source(pin);
            }
        }
    }
    state
        .mit_shm_segments
        .retain(|_, seg| seg.owner != client_id);
    state.vidmode_client_versions.remove(&client_id);
    state.randr_client_versions.remove(&client_id);
    state.xi2_client_versions.remove(&client_id);
    state
        .randr_select_masks
        .retain(|(owner, window), _| *owner != client_id.0 && !dead_windows.contains(window));
    state.xkb_clients.remove(&client_id.0);
    state
        .xkb_interests
        .retain(|(owner, _), _| *owner != client_id.0);
    state.dpms.selected_by.remove(&client_id);
    state.screensaver.selected_by.remove(&client_id);
    crate::core_loop::process_request::unset_screen_saver_attributes(state, backend, client_id);
    let was_suspending = state
        .screensaver
        .suspend_counts
        .remove(&client_id)
        .is_some();
    if was_suspending
        && state.screensaver.suspend_counts.is_empty()
        && matches!(
            state.screensaver.active,
            crate::server::ScreenSaverActive::Off
        )
        && state.dpms.power_level == 0
    {
        // Mirrors ScreenSaverFreeSuspend (saver.c:343-378): on the
        // last suspender going away, restart the idle clock so the
        // saver doesn't immediately fire from a stale baseline.
        state.dpms.last_activity = std::time::Instant::now();
        crate::core_loop::process_request::reset_idletime_state_after_suspend_release(state);
    }

    for xid in removed.closed_fonts {
        let _ = backend.close_font(None, xid);
    }
    // One candidate list for the host free: handles whose pixmap RESOURCE just
    // went away, plus handles that were being kept alive only by a destroyed
    // window's background or border. Deduplicated, because a tile can be in
    // both — freeing it twice is the same broken contract the CWA path had.
    //
    // Neither half may be freed unconditionally: another client's window may
    // still name the tile as its background or border, and a GC may still hold
    // it as a tile / stipple / clip mask. This path checked nothing at all, so
    // `A` creating a tile, `B` bordering with it and `A` disconnecting left
    // `B` sampling freed GPU storage (#133). The client's own windows and GCs
    // are gone by this point, so the gate sees only survivors.
    let last_names = release_removed_names(state, backend, None, &removed.freed_names);
    let mut freeable = removed.freed_pixmaps;
    freeable.extend(attr_pixmap_xids);
    freeable.extend(&last_names);
    free_orphaned_host_pixmaps(state, backend, freeable, &last_names, host_pixmap_frees);
    for (pic_xid, owned_pix) in removed.freed_pictures {
        let _ = backend.render_free_picture(None, pic_xid);
        if let Some(pix_xid) = owned_pix {
            let _ = backend.free_pixmap(None, pix_xid);
        }
    }
    for gs_xid in removed.freed_glyphsets {
        let _ = backend.render_free_glyphset(None, gs_xid);
    }
    for cursor_xid in removed.freed_cursors {
        let _ = backend.free_cursor(None, cursor_xid);
    }
    crate::core_loop::process_request::release_dropped_cursors(state, backend, None);
    for syncobj_xid in removed.freed_dri3_syncobjs {
        if let Err(e) = backend.dri3_free_syncobj(client_id, syncobj_xid) {
            log::warn!(
                "disconnect: free DRI3 syncobj 0x{syncobj_xid:x} for client {} failed: {e}",
                client_id.0
            );
        }
    }
    // Release the departing client's COMPOSITE overlay claims, tearing the
    // overlay down if it held the last one. This lives here, in
    // `process_disconnect` itself, rather than in
    // `disconnect_with_pending_cleanup`: `KillClient` on another client's
    // resource calls this function inline (see `handle_kill_client`) and
    // bypasses that funnel, so a killed compositor would keep its claim.
    //
    // Released whatever the close-down mode — see the helper for why a
    // retained client is not allowed to keep a screen-wide singleton.
    //
    // NOT the same thing as `backend.client_disconnected` below, which
    // clears the scene's `root_overlay` contribution: a different concept
    // with a confusingly similar name.
    crate::core_loop::composite_overlay::release_client_overlay_claims(state, backend, client_id);
    backend.sync_floating_keyboard_states(state);
    // Drop any per-client transient backend state (e.g. the root-overlay
    // contribution) so a crashed/killed client can't strand it.
    backend.client_disconnected(client_id);
}

/// L2 plan B.6c — release the redirect's reason-1 hold on a
/// window's off-screen backing. Takes `Window.redirected_backing`
/// off the resource record; asks the backend to decref the
/// alias-registry entry (and free the underlying pixmap when the
/// last ref drops). Surviving `NameWindowPixmap` aliases keep the
/// backing alive until their `FreePixmap` lands.
///
/// Shared by both the COMPOSITE `UnredirectWindow` /
/// `UnredirectSubwindows` dispatch arm in
/// `crates/yserver-core/src/core_loop/process_request.rs` and the
/// per-client disconnect cleanup above.
pub(crate) fn teardown_redirect_for_window(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<crate::backend::OriginContext>,
    window: ResourceId,
) {
    unrealize_redirect_backing(state, backend, origin, window);
    // Restore W's scene-participation. The matching
    // `activate_redirect_backing_for` in `process_request.rs` flipped
    // it to false for Manual mode (and true for Automatic — a no-op
    // restore in that case). Symmetric to the `UnredirectWindow` /
    // `UnredirectSubwindows` arm in `process_request.rs`, which
    // performs the same restore on the protocol path. Without this,
    // an abnormal compositor disconnect (e.g. marco crashes mid
    // session) leaves every Manually-redirected window with
    // `scene_participating=false` — i.e. invisible — for the rest of
    // the session.
    // A mapped window hidden by an unmapped ancestor has no backing but is still excluded.
    let Some((host_window, mapped)) = state
        .resources
        .window(window)
        .map(|w| (w.host_xid, w.map_state != MapState::Unmapped))
    else {
        return;
    };
    let Some(host_window) = host_window else {
        log::debug!(
            "teardown_redirect_for_window(0x{:x}): no host_xid; skipping participation restore",
            window.0
        );
        return;
    };
    if let Err(err) = backend.set_window_scene_participation(origin, host_window, mapped) {
        log::warn!(
            "teardown_redirect_for_window: set_window_scene_participation(0x{:x}, {mapped}) failed: {err}",
            window.0
        );
    }
}

/// Drop a redirected window's backing when it becomes unviewable, as Xorg's
/// `compUnrealizeWindow` → `compCheckRedirect` does
/// (`composite/compwindow.c:291`, `:176-181`). Unlike
/// [`teardown_redirect_for_window`] the redirect record and the window's
/// scene exclusion stay: a later realize allocates a fresh backing for the
/// same redirect. Named pixmaps keep the old backing alive through the
/// backend's alias registry and stop following the window, as Xorg's
/// `DestroyPixmap` only drops the window's own reference (`:181`).
/// Returns the released backing, if the window had one.
pub(crate) fn unrealize_redirect_backing(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<crate::backend::OriginContext>,
    window: ResourceId,
) -> Option<crate::backend::PixmapHandle> {
    let backing = {
        let w = state.resources.window_mut(window)?;
        let backing = w.redirected_backing.take()?;
        w.composite_named_pixmaps
            .retain(|alias| alias.host_pixmap != backing.host_pixmap);
        backing
    };
    // compSetParentPixmap (`composite/compalloc.c:641-665`).
    crate::core_loop::damage_fanout::move_window_damage_to_list_head(state, window);
    if let Err(err) = backend.release_redirected_backing(origin, backing.host_pixmap) {
        log::warn!(
            "release_redirected_backing(0x{:x}) failed: {err}",
            backing.host_pixmap.as_raw()
        );
    }
    Some(backing.host_pixmap)
}

/// Destroy every window `client` owns, one owned root at a time, each run to
/// completion before the next (Xorg `FreeClientResources` deletes each window
/// resource through `DeleteWindow`, `dix/resource.c:1146-1154`). Returns the
/// destroyed windows, survivors' windows under them included, and their
/// attribute pixmaps for the orphan gate.
///
/// # Errors
/// A plan holding the COW (the compositor reparented it under one of
/// `client`'s windows) failed to prepare. That root goes first, so nothing
/// was destroyed.
fn destroy_owned_windows(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    client: ClientId,
) -> std::io::Result<(Vec<ResourceId>, Vec<u32>)> {
    let mut owned_roots: Vec<ResourceId> = Vec::new();
    state
        .resources
        .collect_owned_window_roots(client, &mut owned_roots);
    // Root order is free (hash order in Xorg too); the only fallible one
    // goes first.
    owned_roots.sort_by_key(|root| {
        !state
            .resources
            .plan_destroy(*root)
            .is_some_and(|plan| plan.contains_cow())
    });
    let mut destroyed = Vec::new();
    let mut attr_pixmaps = Vec::new();
    for root in owned_roots {
        // An earlier root's plan already took a root nested under it.
        let Some(plan) = state.resources.plan_destroy(root) else {
            continue;
        };
        let prepared = plan.prepare(backend).map_err(|err| {
            std::io::Error::other(format!(
                "cannot destroy window 0x{:x}, which holds the composite overlay: {err}",
                root.0
            ))
        })?;
        let outcome =
            crate::core_loop::process_request::execute_destroy(state, backend, None, prepared);
        destroyed.extend(outcome.windows);
        attr_pixmaps.extend(outcome.attr_pixmaps);
    }
    Ok((destroyed, attr_pixmaps))
}

/// A client teardown that cannot complete: leave its record, resources and
/// resource-ID range in place and have the core loop shut the server down.
fn abort_teardown(state: &mut ServerState, what: &str, client: ClientId, err: &std::io::Error) {
    let cause = format!("{what} of client {}: {err}", client.0);
    log::error!("{cause}; teardown stopped, shutting the server down");
    state.fatal_teardown.get_or_insert(cause);
}

/// Destroy every resource owned by a zombie client — invoked by
/// `KillClient(AllTemporary)` (for each RetainTemporary zombie) and by
/// `KillClient(resource_owned_by_a_zombie)`. Mirrors the resource-
/// destroy half of `process_disconnect`, minus the live-client setup
/// (there is no `state.clients` entry, no reader thread, no
/// connection-tied extension state — those were torn down at the
/// original disconnect). The caller must remove `zombie` from
/// `state.zombie_clients` after this returns `Ok`.
///
/// # Errors
/// The zombie's windows could not be destroyed; nothing of it was freed and
/// `state.fatal_teardown` is set. The zombie must stay recorded.
pub fn destroy_zombie_resources(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    zombie: ClientId,
) -> std::io::Result<()> {
    destroy_zombie_resources_reporting(state, backend, zombie, None)
}

/// [`destroy_zombie_resources`], reporting its host-pixmap frees the same way
/// [`process_disconnect_reporting`] does.
///
/// # Errors
/// As [`destroy_zombie_resources`].
pub fn destroy_zombie_resources_reporting(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    zombie: ClientId,
    host_pixmap_frees: Option<&mut HostPixmapFrees>,
) -> std::io::Result<()> {
    let (_destroyed, attr_pixmap_xids) = match destroy_owned_windows(state, backend, zombie) {
        Ok(destroyed) => destroyed,
        Err(err) => {
            abort_teardown(state, "zombie cleanup", zombie, &err);
            return Err(err);
        }
    };

    let removed = state.resources.remove_non_window_resources_owned_by(zombie);

    for xid in removed.closed_fonts {
        let _ = backend.close_font(None, xid);
    }
    // One candidate list for the host free: handles whose pixmap RESOURCE just
    // went away, plus handles that were being kept alive only by a destroyed
    // window's background or border. Deduplicated, because a tile can be in
    // both — freeing it twice is the same broken contract the CWA path had.
    //
    // Neither half may be freed unconditionally: another client's window may
    // still name the tile as its background or border, and a GC may still hold
    // it as a tile / stipple / clip mask. This path checked nothing at all, so
    // `A` creating a tile, `B` bordering with it and `A` disconnecting left
    // `B` sampling freed GPU storage (#133). The client's own windows and GCs
    // are gone by this point, so the gate sees only survivors.
    let last_names = release_removed_names(state, backend, None, &removed.freed_names);
    let mut freeable = removed.freed_pixmaps;
    freeable.extend(attr_pixmap_xids);
    freeable.extend(&last_names);
    free_orphaned_host_pixmaps(state, backend, freeable, &last_names, host_pixmap_frees);
    for (pic_xid, owned_pix) in removed.freed_pictures {
        let _ = backend.render_free_picture(None, pic_xid);
        if let Some(pix_xid) = owned_pix {
            let _ = backend.free_pixmap(None, pix_xid);
        }
    }
    for gs_xid in removed.freed_glyphsets {
        let _ = backend.render_free_glyphset(None, gs_xid);
    }
    for cursor_xid in removed.freed_cursors {
        let _ = backend.free_cursor(None, cursor_xid);
    }
    crate::core_loop::process_request::release_dropped_cursors(state, backend, None);
    for syncobj_xid in removed.freed_dri3_syncobjs {
        if let Err(e) = backend.dri3_free_syncobj(zombie, syncobj_xid) {
            log::warn!(
                "zombie cleanup: free DRI3 syncobj 0x{syncobj_xid:x} for client {} failed: {e}",
                zombie.0
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{HashMap, HashSet, VecDeque},
        io::Read,
        os::unix::net::UnixStream,
        sync::{Arc, Mutex, atomic::AtomicU16},
    };

    use yserver_protocol::x11::{
        ClientByteOrder, ClientId, CreatePixmapRequest, CreateWindowRequest, ResourceId,
    };

    use super::{destroy_zombie_resources, process_disconnect};
    use crate::{
        backend::recording::{RecordedCall, RecordingBackend},
        resources::ROOT_WINDOW,
        server::{ClientState, CompositeRedirectMode, RedirectRecord, ServerState},
    };

    fn install_client(state: &mut ServerState, id: u32) {
        let (a, _b) = UnixStream::pair().expect("socketpair");
        state.clients.insert(
            id,
            ClientState {
                writer: Arc::new(Mutex::new(crate::transport::Transport::Unix(a))),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0)),
                resource_id_base: id << 20,
                resource_id_mask: 0x000F_FFFF,
                event_masks: HashMap::new(),
                save_set: HashSet::new(),
                big_requests_enabled: false,
                xi2_masks: HashMap::new(),
                xi1_event_classes: HashSet::new(),
                xi1_window_event_classes: HashMap::new(),
                outbound: VecDeque::new(),
                watching_writable: false,
                write_failed: false,
                gone: false,
                focused_window: ROOT_WINDOW,
                reader_control: None,
                is_local: true,
                fd_passing: true,
                internal: false,
            },
        );
    }

    fn install_client_with_writer(state: &mut ServerState, id: u32, writer: UnixStream) {
        state.clients.insert(
            id,
            ClientState {
                writer: Arc::new(Mutex::new(crate::transport::Transport::Unix(writer))),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0)),
                resource_id_base: id << 20,
                resource_id_mask: 0x000F_FFFF,
                event_masks: HashMap::new(),
                save_set: HashSet::new(),
                big_requests_enabled: false,
                xi2_masks: HashMap::new(),
                xi1_event_classes: HashSet::new(),
                xi1_window_event_classes: HashMap::new(),
                outbound: VecDeque::new(),
                watching_writable: false,
                write_failed: false,
                gone: false,
                focused_window: ROOT_WINDOW,
                reader_control: None,
                is_local: true,
                fd_passing: true,
                internal: false,
            },
        );
    }

    #[test]
    fn disconnect_leaves_other_clients_redirects_intact() {
        let mut state = ServerState::new();
        install_client(&mut state, 1);
        install_client(&mut state, 2);
        // Client A redirects window W.
        state
            .composite_redirects
            .redirect_window(
                ResourceId(0x1234),
                RedirectRecord {
                    mode: CompositeRedirectMode::Manual,
                    owner: ClientId(1),
                },
            )
            .unwrap();
        let mut backend = RecordingBackend::new();
        process_disconnect(&mut state, &mut backend, ClientId(2));
        assert!(
            state
                .composite_redirects
                .window_mode(ResourceId(0x1234))
                .is_some()
        );
    }

    #[test]
    fn disconnect_releases_owned_server_grab() {
        let mut state = ServerState::new();
        install_client(&mut state, 1);
        install_client(&mut state, 2);
        state.server_grab_owner = Some(ClientId(1));
        let mut backend = RecordingBackend::new();

        process_disconnect(&mut state, &mut backend, ClientId(1));

        assert_eq!(state.server_grab_owner, None);
        assert!(state.clients.contains_key(&2));
    }

    #[test]
    fn disconnect_tears_down_owned_redirect() {
        let mut state = ServerState::new();
        install_client(&mut state, 1);
        state
            .composite_redirects
            .redirect_window(
                ResourceId(0x5678),
                RedirectRecord {
                    mode: CompositeRedirectMode::Manual,
                    owner: ClientId(1),
                },
            )
            .unwrap();
        let mut backend = RecordingBackend::new();
        process_disconnect(&mut state, &mut backend, ClientId(1));
        assert!(state.composite_redirects.is_empty());
    }

    #[test]
    fn disconnect_purges_parked_present_wait_releasing_both_pins_exactly_once() {
        // Task 5 (unified pending-present store): a client whose producer
        // fence may never signal after it disappears (GPU reset, killed
        // process) must not leak the parked entry. Unlike window-destroy,
        // disconnect does NOT release by-XID (the socket is going away —
        // no receiver), but it must still drop the entry, its side-map
        // row, the WAIT pin (`finish_present_source_wait`) and the
        // distinct ENTRY pin (`release_present_source`) exactly once
        // each.
        use crate::server::{PendingPresentEntry, PendingPresentPixmap, PendingPresentRequest};
        use yserver_protocol::x11::present::PixmapRequest;

        const CLIENT: u32 = 9;
        const WINDOW_XID: u32 = 0x0000_0909;
        const WAIT_ID: u64 = 55;
        const PRESENT_ID: u64 = 9191;
        const PIN_ID: u64 = 4141;

        let mut state = ServerState::new();
        install_client(&mut state, CLIENT);
        let mut backend = RecordingBackend::new();

        let pending = PendingPresentPixmap {
            origin: None,
            client_id: ClientId(CLIENT),
            request: PendingPresentRequest::Pixmap(PixmapRequest {
                window: WINDOW_XID,
                pixmap: 0x0000_090a,
                serial: 1,
                valid: 0,
                update: 0,
                x_off: 0,
                y_off: 0,
                target_crtc: 0,
                wait_fence: 0,
                idle_fence: 0x0000_0999,
                options: 0,
                target_msc: 0,
                divisor: 0,
                remainder: 0,
                notifies: Vec::new(),
            }),
            wake: crate::backend::PresentWake::Pixmap {
                idle_fence_xid: 0x0000_0999,
            },
            masked_options: 0,
            src_host_xid: 0x0040_0102,
            paint_dst_host_xid: 0x0040_0101,
            completion_dst_host_xid: 0x0040_0101,
            src_width: 100,
            src_height: 100,
            update_rects: None,
            present_id: PRESENT_ID,
            window_generation: 0,
            crtc_id: 0,
            crtc_epoch: 0,
            msc_offset: 0,
            effective_target_msc: None,
        };
        state.present_wait_to_id.insert(WAIT_ID, PRESENT_ID);
        state.present_pending_exec.insert(
            PRESENT_ID,
            PendingPresentEntry {
                pending,
                source_ready: false,
                superseded_by_executed_present_id: None,
                wait_id: Some(WAIT_ID),
                pin: Some(PIN_ID),
            },
        );

        process_disconnect(&mut state, &mut backend, ClientId(CLIENT));

        assert!(
            state.present_pending_exec.is_empty(),
            "parked entry purged on disconnect"
        );
        assert!(
            state.present_wait_to_id.is_empty(),
            "side-map row dropped alongside the entry"
        );
        assert_eq!(
            backend.finished_present_source_waits,
            vec![WAIT_ID],
            "wait pin dropped exactly once on disconnect"
        );
        assert_eq!(
            backend.released_present_sources,
            vec![PIN_ID],
            "entry pin dropped exactly once on disconnect"
        );
        assert!(
            backend.triggered_dri3_fences.is_empty(),
            "disconnect abandons the parked copy silently — no receiver left \
             to signal a by-XID idle fence to"
        );
    }

    #[test]
    fn disconnect_with_retain_permanent_keeps_pixmap_owned_by_original_client() {
        let mut state = ServerState::new();
        let mut backend = RecordingBackend::new();
        install_client(&mut state, 7);
        state.resources.create_pixmap(
            ClientId(7),
            CreatePixmapRequest {
                pixmap: ResourceId(0x0070_0001),
                drawable: ROOT_WINDOW,
                width: 16,
                height: 16,
                depth: 24,
            },
        );
        state.close_down_modes.insert(7, 1);

        process_disconnect(&mut state, &mut backend, ClientId(7));

        // Pixmap survives, owner field unchanged.
        assert_eq!(
            state.resources.resource_owner(ResourceId(0x0070_0001)),
            Some(ClientId(7)),
        );
        // Client is gone from the live map, recorded as zombie with
        // the original close-down mode (RetainPermanent = 1).
        assert!(!state.clients.contains_key(&7));
        assert!(!state.close_down_modes.contains_key(&7));
        assert_eq!(state.zombie_clients.get(&7).copied(), Some(1));
    }

    #[test]
    fn disconnect_with_destroy_default_frees_pixmap() {
        let mut state = ServerState::new();
        let mut backend = RecordingBackend::new();
        install_client(&mut state, 7);
        state.resources.create_pixmap(
            ClientId(7),
            CreatePixmapRequest {
                pixmap: ResourceId(0x0070_0001),
                drawable: ROOT_WINDOW,
                width: 16,
                height: 16,
                depth: 24,
            },
        );

        process_disconnect(&mut state, &mut backend, ClientId(7));

        assert!(
            state
                .resources
                .resource_owner(ResourceId(0x0070_0001))
                .is_none()
        );
        assert!(!state.zombie_clients.contains_key(&7));
    }

    /// #133: a disconnecting client's pixmaps are not unconditionally
    /// free-able. `A` creates a tile, `B` borders a window with it, `A`
    /// disconnects — the host storage must survive, because `B` is still
    /// sampling it. This path used to check nothing at all, so `B` was left
    /// bordering freed GPU storage.
    #[test]
    fn disconnect_keeps_a_pixmap_another_clients_border_still_uses() {
        const HOST_TILE: u32 = 0x9999_0031;
        let tile = ResourceId(0x0070_0031);
        let mut state = ServerState::new();
        let mut backend = RecordingBackend::new();
        install_client(&mut state, 7);
        install_client(&mut state, 8);

        state.resources.create_pixmap(
            ClientId(7),
            CreatePixmapRequest {
                pixmap: tile,
                drawable: ROOT_WINDOW,
                width: 16,
                height: 16,
                depth: 24,
            },
        );
        assert!(state.resources.set_pixmap_host_xid(
            tile,
            crate::backend::PixmapHandle::from_raw(HOST_TILE).expect("non-zero"),
        ));

        // Client 8's window borders with client 7's tile.
        let window = ResourceId(0x0080_0031);
        state.resources.create_window(
            ClientId(8),
            CreateWindowRequest {
                depth: 24,
                window,
                parent: ROOT_WINDOW,
                width: 100,
                height: 100,
                border_width: 4,
                class: 1,
                visual: crate::resources::ROOT_VISUAL,
                border_pixmap: Some(tile),
                ..Default::default()
            },
        );
        assert!(
            state.resources.host_xid_referenced_by_window_border(
                crate::backend::PixmapHandle::from_raw(HOST_TILE).expect("non-zero")
            ),
            "client 8's window must hold the tile as its border"
        );

        process_disconnect(&mut state, &mut backend, ClientId(7));

        assert!(
            state.resources.resource_owner(tile).is_none(),
            "the resource id must still be reclaimed"
        );
        assert!(
            !backend
                .calls()
                .iter()
                .any(|call| matches!(call, RecordedCall::FreePixmap(HOST_TILE))),
            "the host tile must survive: another client's window still borders with it"
        );

        // Positive control: with the border gone the same disconnect path
        // does free it, so the assertion above cannot hold vacuously.
        let mut state2 = ServerState::new();
        let mut backend2 = RecordingBackend::new();
        install_client(&mut state2, 7);
        state2.resources.create_pixmap(
            ClientId(7),
            CreatePixmapRequest {
                pixmap: tile,
                drawable: ROOT_WINDOW,
                width: 16,
                height: 16,
                depth: 24,
            },
        );
        assert!(state2.resources.set_pixmap_host_xid(
            tile,
            crate::backend::PixmapHandle::from_raw(HOST_TILE).expect("non-zero"),
        ));
        process_disconnect(&mut state2, &mut backend2, ClientId(7));
        assert!(
            backend2
                .calls()
                .iter()
                .any(|call| matches!(call, RecordedCall::FreePixmap(HOST_TILE))),
            "an unreferenced tile must still be freed on disconnect"
        );
    }

    /// A disconnect releases one ref per name on a shared backing; a background defers only the last.
    #[test]
    fn disconnect_releases_one_ref_per_window_pixmap_name() {
        for as_background in [false, true] {
            const HOST: u32 = 0x9999_0051;
            let host = crate::backend::PixmapHandle::from_raw(HOST).expect("non-zero");
            let names = [0x0070_0051, 0x0070_0052, 0x0070_0053].map(ResourceId);
            let mut state = ServerState::new();
            let mut backend = RecordingBackend::new();
            install_client(&mut state, 7);
            install_client(&mut state, 8);
            for name in names {
                state.resources.create_pixmap(
                    ClientId(7),
                    CreatePixmapRequest {
                        pixmap: name,
                        drawable: ROOT_WINDOW,
                        width: 16,
                        height: 16,
                        depth: 24,
                    },
                );
                assert!(state.resources.set_pixmap_host_xid(name, host));
                state.resources.mark_pixmap_composite_name(name);
            }
            if as_background {
                state.resources.create_window(
                    ClientId(8),
                    CreateWindowRequest {
                        depth: 24,
                        window: ResourceId(0x0080_0001),
                        parent: ROOT_WINDOW,
                        width: 10,
                        height: 10,
                        class: 1,
                        visual: crate::resources::ROOT_VISUAL,
                        background_pixmap: Some(names[1]),
                        ..Default::default()
                    },
                );
                assert!(state.resources.host_xid_still_referenced(host));
            }
            let frees = |backend: &RecordingBackend| {
                backend
                    .calls()
                    .iter()
                    .filter(|call| matches!(call, RecordedCall::FreePixmap(HOST)))
                    .count()
            };
            process_disconnect(&mut state, &mut backend, ClientId(7));
            let held = usize::from(as_background);
            assert_eq!(frees(&backend), 3 - held, "background={as_background}");
            process_disconnect(&mut state, &mut backend, ClientId(8));
            assert_eq!(frees(&backend), 3, "background={as_background}");
        }
    }

    /// #133: the leak on the other side of the same gate. A tile kept alive
    /// past its `FreePixmap` by the client's OWN window border must be
    /// released when disconnect destroys that window. The disconnect paths
    /// destroy windows directly rather than through
    /// `destroy_window_subtree`, so they never collected attribute pixmaps
    /// and this leaked: the pixmap resource was already gone, so
    /// `remove_non_window_resources_owned_by` had nothing to hand back.
    #[test]
    fn disconnect_releases_a_border_tile_retained_past_free_pixmap() {
        for retain in [false, true] {
            const HOST_TILE: u32 = 0x9999_0041;
            let host = crate::backend::PixmapHandle::from_raw(HOST_TILE).expect("non-zero");
            let tile = ResourceId(0x0070_0041);
            let window = ResourceId(0x0070_0042);
            let mut state = ServerState::new();
            let mut backend = RecordingBackend::new();
            install_client(&mut state, 7);
            if retain {
                // RetainPermanent: the windows are still destroyed, so the
                // border reference still disappears and the tile is still
                // orphaned — but no pixmap resource is reclaimed, which is
                // exactly the case a `freed_pixmaps`-only release misses.
                state.close_down_modes.insert(7, 1);
            }

            state.resources.create_pixmap(
                ClientId(7),
                CreatePixmapRequest {
                    pixmap: tile,
                    drawable: ROOT_WINDOW,
                    width: 16,
                    height: 16,
                    depth: 24,
                },
            );
            assert!(state.resources.set_pixmap_host_xid(tile, host));
            state.resources.create_window(
                ClientId(7),
                CreateWindowRequest {
                    depth: 24,
                    window,
                    parent: ROOT_WINDOW,
                    width: 100,
                    height: 100,
                    border_width: 4,
                    class: 1,
                    visual: crate::resources::ROOT_VISUAL,
                    border_pixmap: Some(tile),
                    ..Default::default()
                },
            );
            // FreePixmap: retained, because the border still names it.
            assert!(state.resources.free_pixmap(tile).is_some());
            assert!(
                state.resources.host_xid_still_referenced(host),
                "retain={retain}: the border must be the last reference"
            );

            process_disconnect(&mut state, &mut backend, ClientId(7));

            assert!(
                !state.resources.host_xid_still_referenced(host),
                "retain={retain}: nothing may reference the tile after disconnect"
            );
            assert!(
                backend
                    .calls()
                    .iter()
                    .any(|call| matches!(call, RecordedCall::FreePixmap(HOST_TILE))),
                "retain={retain}: the orphaned border tile must be released"
            );
            assert_eq!(
                backend
                    .calls()
                    .iter()
                    .filter(|call| matches!(call, RecordedCall::FreePixmap(HOST_TILE)))
                    .count(),
                1,
                "retain={retain}: released exactly once"
            );
        }
    }

    /// `HostPixmapFrees` for a tile that only a destroyed window's border
    /// still held: freed when nothing else names it, deferred while another
    /// client's border does.
    #[test]
    fn disconnect_reports_a_border_only_tile_freed_or_deferred() {
        for shared in [false, true] {
            const HOST_TILE: u32 = 0x9999_0061;
            let host = crate::backend::PixmapHandle::from_raw(HOST_TILE).expect("non-zero");
            let tile = ResourceId(0x0070_0061);
            let mut state = ServerState::new();
            let mut backend = RecordingBackend::new();
            install_client(&mut state, 7);
            install_client(&mut state, 8);
            state.resources.create_pixmap(
                ClientId(7),
                CreatePixmapRequest {
                    pixmap: tile,
                    drawable: ROOT_WINDOW,
                    width: 16,
                    height: 16,
                    depth: 24,
                },
            );
            assert!(state.resources.set_pixmap_host_xid(tile, host));
            let windows: &[(u32, u32)] = if shared {
                &[(7, 0x0070_0062), (8, 0x0080_0062)]
            } else {
                &[(7, 0x0070_0062)]
            };
            for &(owner, window) in windows {
                state.resources.create_window(
                    ClientId(owner),
                    CreateWindowRequest {
                        depth: 24,
                        window: ResourceId(window),
                        parent: ROOT_WINDOW,
                        width: 100,
                        height: 100,
                        border_width: 4,
                        class: 1,
                        visual: crate::resources::ROOT_VISUAL,
                        border_pixmap: Some(tile),
                        ..Default::default()
                    },
                );
            }
            assert!(state.resources.free_pixmap(tile).is_some());

            let mut frees = super::HostPixmapFrees::default();
            super::process_disconnect_reporting(
                &mut state,
                &mut backend,
                ClientId(7),
                Some(&mut frees),
            );

            let (freed, deferred) = if shared {
                (vec![], vec![HOST_TILE])
            } else {
                (vec![HOST_TILE], vec![])
            };
            assert_eq!(frees.freed, freed, "shared={shared}");
            assert_eq!(frees.deferred, deferred, "shared={shared}");
        }
    }

    /// #196: a pixmap the client freed while its GC still held it (as tile,
    /// stipple or clip mask) dies with the GC at disconnect — Xorg's `FreeGC`
    /// drops those refs (`dix/gc.c:776-781`). The pixmap resource was already
    /// gone, so `freed_pixmaps` never named it and it leaked for the session.
    #[test]
    fn disconnect_releases_pixmaps_its_gc_held_past_free_pixmap() {
        let mut state = ServerState::new();
        let mut backend = RecordingBackend::new();
        install_client(&mut state, 7);
        let mut hosts = Vec::new();
        for (i, depth) in [24u8, 1, 1].into_iter().enumerate() {
            let pixmap = ResourceId(0x0070_0051 + i as u32);
            let host = 0x9999_0051 + i as u32;
            state.resources.create_pixmap(
                ClientId(7),
                CreatePixmapRequest {
                    pixmap,
                    drawable: ROOT_WINDOW,
                    width: 16,
                    height: 16,
                    depth,
                },
            );
            assert!(state.resources.set_pixmap_host_xid(
                pixmap,
                crate::backend::PixmapHandle::from_raw(host).expect("non-zero")
            ));
            hosts.push(host);
        }
        state.resources.create_gc(
            ClientId(7),
            yserver_protocol::x11::CreateGcRequest {
                gc: ResourceId(0x0070_0050),
                drawable: ROOT_WINDOW,
                function: None,
                plane_mask: None,
                foreground: None,
                background: None,
                line_width: None,
                line_style: None,
                cap_style: None,
                join_style: None,
                fill_style: None,
                fill_rule: None,
                tile: Some(ResourceId(0x0070_0051)),
                stipple: Some(ResourceId(0x0070_0052)),
                tile_x_origin: None,
                tile_y_origin: None,
                font: None,
                subwindow_mode: None,
                graphics_exposures: None,
                clip_x_origin: None,
                clip_y_origin: None,
                clip_mask: Some(Some(ResourceId(0x0070_0053))),
                dash_offset: None,
                dashes: None,
                arc_mode: None,
            },
        );
        for i in 0..3 {
            assert!(
                state
                    .resources
                    .free_pixmap(ResourceId(0x0070_0051 + i))
                    .is_some()
            );
        }
        process_disconnect(&mut state, &mut backend, ClientId(7));
        let mut freed: Vec<u32> = backend
            .calls()
            .iter()
            .filter_map(|call| match call {
                RecordedCall::FreePixmap(xid) => Some(*xid),
                _ => None,
            })
            .collect();
        freed.sort_unstable();
        assert_eq!(freed, hosts, "each released exactly once");
    }

    /// The zombie variant: a RetainPermanent client's pixmap survives its
    /// disconnect, and `KillClient` later runs `destroy_zombie_resources`.
    /// That release path must apply the same gate — another client's window
    /// may be bordering with the retained tile.
    #[test]
    fn zombie_destruction_keeps_a_pixmap_another_clients_border_still_uses() {
        const HOST_TILE: u32 = 0x9999_0051;
        let host = crate::backend::PixmapHandle::from_raw(HOST_TILE).expect("non-zero");
        let tile = ResourceId(0x0070_0051);
        let mut state = ServerState::new();
        let mut backend = RecordingBackend::new();
        install_client(&mut state, 7);
        install_client(&mut state, 8);
        state.close_down_modes.insert(7, 1); // RetainPermanent

        state.resources.create_pixmap(
            ClientId(7),
            CreatePixmapRequest {
                pixmap: tile,
                drawable: ROOT_WINDOW,
                width: 16,
                height: 16,
                depth: 24,
            },
        );
        assert!(state.resources.set_pixmap_host_xid(tile, host));

        // Client 8 borders with client 7's tile.
        state.resources.create_window(
            ClientId(8),
            CreateWindowRequest {
                depth: 24,
                window: ResourceId(0x0080_0051),
                parent: ROOT_WINDOW,
                width: 100,
                height: 100,
                border_width: 4,
                class: 1,
                visual: crate::resources::ROOT_VISUAL,
                border_pixmap: Some(tile),
                ..Default::default()
            },
        );

        process_disconnect(&mut state, &mut backend, ClientId(7));
        assert!(
            state.zombie_clients.contains_key(&7),
            "RetainPermanent must leave a zombie"
        );

        let mut backend = RecordingBackend::new();
        destroy_zombie_resources(&mut state, &mut backend, ClientId(7)).expect("zombie cleanup");
        assert!(
            !backend
                .calls()
                .iter()
                .any(|call| matches!(call, RecordedCall::FreePixmap(HOST_TILE))),
            "zombie destruction must not free a tile another client borders with"
        );
        assert!(
            state.resources.host_xid_still_referenced(host),
            "client 8's border must still hold it"
        );
    }

    #[test]
    fn disconnect_destroy_all_frees_dri3_syncobj_in_core_and_backend() {
        const XID: u32 = 0x0070_0091;
        let mut state = ServerState::new();
        let mut backend = RecordingBackend::new();
        install_client(&mut state, 7);
        assert!(
            state
                .resources
                .register_dri3_syncobj(ResourceId(XID), ClientId(7))
        );
        backend.seed_dri3_syncobj_for_test(XID, ClientId(7));

        process_disconnect(&mut state, &mut backend, ClientId(7));

        assert!(!state.resources.xid_in_use(ResourceId(XID)));
        assert!(!backend.dri3_syncobj_owners.contains_key(&XID));
    }

    #[test]
    fn retained_dri3_syncobj_survives_disconnect_until_zombie_destruction() {
        const XID: u32 = 0x0070_0092;
        let mut state = ServerState::new();
        let mut backend = RecordingBackend::new();
        install_client(&mut state, 7);
        state.close_down_modes.insert(7, 1); // RetainPermanent
        assert!(
            state
                .resources
                .register_dri3_syncobj(ResourceId(XID), ClientId(7))
        );
        backend.seed_dri3_syncobj_for_test(XID, ClientId(7));

        process_disconnect(&mut state, &mut backend, ClientId(7));

        assert_eq!(
            state.resources.resource_owner(ResourceId(XID)),
            Some(ClientId(7)),
            "KillClient(syncobj XID) must still resolve the zombie owner"
        );
        assert!(backend.dri3_syncobj_owners.contains_key(&XID));
        assert_eq!(state.zombie_clients.get(&7), Some(&1));

        destroy_zombie_resources(&mut state, &mut backend, ClientId(7)).expect("zombie cleanup");
        assert!(!state.resources.xid_in_use(ResourceId(XID)));
        assert!(!backend.dri3_syncobj_owners.contains_key(&XID));
    }

    #[test]
    fn temporary_dri3_syncobj_survives_until_all_temporary_cleanup() {
        const XID: u32 = 0x0070_0093;
        let mut state = ServerState::new();
        let mut backend = RecordingBackend::new();
        install_client(&mut state, 7);
        state.close_down_modes.insert(7, 2); // RetainTemporary
        assert!(
            state
                .resources
                .register_dri3_syncobj(ResourceId(XID), ClientId(7))
        );
        backend.seed_dri3_syncobj_for_test(XID, ClientId(7));

        process_disconnect(&mut state, &mut backend, ClientId(7));
        assert!(state.resources.xid_in_use(ResourceId(XID)));
        assert!(backend.dri3_syncobj_owners.contains_key(&XID));

        // This is the operation KillClient(AllTemporary) performs for each
        // mode-2 zombie.
        destroy_zombie_resources(&mut state, &mut backend, ClientId(7)).expect("zombie cleanup");
        assert!(!state.resources.xid_in_use(ResourceId(XID)));
        assert!(!backend.dri3_syncobj_owners.contains_key(&XID));
    }

    #[test]
    fn destroy_zombie_resources_frees_only_targeted_clients_resources() {
        // Regression: previously two retained clients (32 and 33) shared
        // a single retain bucket, so killing one leaked the other's
        // resources. Now they keep their original owner — killing one
        // must not touch the other.
        let mut state = ServerState::new();
        let mut backend = RecordingBackend::new();
        state.resources.create_pixmap(
            ClientId(32),
            CreatePixmapRequest {
                pixmap: ResourceId(0x0200_0001),
                drawable: ROOT_WINDOW,
                width: 16,
                height: 16,
                depth: 24,
            },
        );
        state.resources.create_pixmap(
            ClientId(33),
            CreatePixmapRequest {
                pixmap: ResourceId(0x0210_0001),
                drawable: ROOT_WINDOW,
                width: 16,
                height: 16,
                depth: 24,
            },
        );
        state.zombie_clients.insert(32, 1);
        state.zombie_clients.insert(33, 1);

        destroy_zombie_resources(&mut state, &mut backend, ClientId(32)).expect("zombie cleanup");

        assert!(
            state
                .resources
                .resource_owner(ResourceId(0x0200_0001))
                .is_none()
        );
        assert_eq!(
            state.resources.resource_owner(ResourceId(0x0210_0001)),
            Some(ClientId(33)),
        );
    }

    #[test]
    fn disconnect_shuts_down_socket_peer_sees_eof() {
        let (local, peer) = UnixStream::pair().expect("socketpair");
        peer.set_nonblocking(true).expect("peer nonblocking");
        let mut state = ServerState::new();
        install_client_with_writer(&mut state, 7, local);
        let mut backend = RecordingBackend::new();

        process_disconnect(&mut state, &mut backend, ClientId(7));

        let mut buf = [0u8; 1];
        let read = (&peer).read(&mut buf).expect("peer read");
        assert_eq!(read, 0, "peer must observe EOF after disconnect");
    }

    #[test]
    fn disconnect_removes_client_from_dpms_selected_by() {
        let mut state = ServerState::new();
        install_client(&mut state, 7);
        state.dpms.selected_by.insert(ClientId(7));
        assert!(state.dpms.selected_by.contains(&ClientId(7)));

        let mut backend = RecordingBackend::new();
        process_disconnect(&mut state, &mut backend, ClientId(7));

        assert!(
            !state.dpms.selected_by.contains(&ClientId(7)),
            "process_disconnect must remove the client from selected_by"
        );
    }

    #[test]
    fn disconnect_removes_only_the_dead_clients_vidmode_version() {
        // The VidMode reply layout is per-client state keyed by ClientId.
        // Leaking it means a recycled id inherits the previous client's
        // v2-vs-legacy choice and gets a reply of the wrong length.
        let mut state = ServerState::new();
        install_client(&mut state, 7);
        install_client(&mut state, 8);
        state.vidmode_client_versions.insert(ClientId(7), (2, 2));
        state.vidmode_client_versions.insert(ClientId(8), (1, 0));

        let mut backend = RecordingBackend::new();
        process_disconnect(&mut state, &mut backend, ClientId(7));

        assert!(!state.vidmode_client_versions.contains_key(&ClientId(7)));
        assert_eq!(
            state.vidmode_client_versions.get(&ClientId(8)),
            Some(&(1, 0)),
            "a surviving client must keep its negotiated version"
        );
    }

    #[test]
    fn disconnect_removes_only_the_dead_clients_xi2_version() {
        // A recycled id must not inherit a dead client's XI 2.0 version:
        // it would lose XI2 raw events under every keyboard grab.
        let mut state = ServerState::new();
        install_client(&mut state, 7);
        install_client(&mut state, 8);
        state.xi2_client_versions.insert(ClientId(7), (2, 0));
        state.xi2_client_versions.insert(ClientId(8), (2, 2));

        let mut backend = RecordingBackend::new();
        process_disconnect(&mut state, &mut backend, ClientId(7));

        assert!(!state.xi2_client_versions.contains_key(&ClientId(7)));
        assert_eq!(state.xi2_client_versions.get(&ClientId(8)), Some(&(2, 2)));
    }

    #[test]
    fn disconnect_removes_client_from_screensaver_state_and_restarts_timer_if_last_suspender() {
        use std::time::Duration;
        let mut state = ServerState::new();
        install_client(&mut state, 7);
        state.screensaver.selected_by.insert(ClientId(7), 0x01);
        state.screensaver.suspend_counts.insert(ClientId(7), 1);
        state.dpms.last_activity = std::time::Instant::now() - Duration::from_secs(120);
        let stale = state.dpms.last_activity;

        let mut backend = RecordingBackend::new();
        process_disconnect(&mut state, &mut backend, ClientId(7));

        assert!(!state.screensaver.selected_by.contains_key(&ClientId(7)));
        assert!(!state.screensaver.suspend_counts.contains_key(&ClientId(7)));
        assert!(
            state.dpms.last_activity > stale,
            "last_activity must advance — client 7 was the last suspender"
        );
    }

    #[test]
    fn disconnect_restores_window_scene_participation_after_manual_redirect_teardown() {
        // Regression: when a compositor (e.g. marco) crashes mid-session
        // while it had `RedirectSubwindows(root, Manual)` active, every
        // window it had taken over stayed at `scene_participating=false`
        // (set by `activate_redirect_backing_for` on the way in) and
        // remained invisible until end-of-session. The disconnect-side
        // teardown must mirror the symmetric `UnredirectSubwindows`
        // protocol path and restore the windows' scene participation.
        let mut state = ServerState::new();
        let compositor = 9;
        let window_owner = 10;
        install_client(&mut state, compositor);
        install_client(&mut state, window_owner);
        // Create a top-level child W of the root, populate its
        // host_xid and a synthetic redirected_backing (as if a
        // Manual-mode `activate_redirect_backing_for` had run).
        let window_id = ResourceId(0x00a0_0001);
        let host_xid: u32 = 0xC0DE_0001;
        let backing_xid: u32 = 0xBA51_0001;
        state.resources.create_window(
            ClientId(window_owner),
            CreateWindowRequest {
                depth: 24,
                window: window_id,
                parent: ROOT_WINDOW,
                x: 0,
                y: 0,
                width: 100,
                height: 100,
                border_width: 0,
                class: 1,
                visual: crate::resources::ROOT_VISUAL,
                ..Default::default()
            },
        );
        // Viewable: only a viewable redirected window holds a backing.
        let _ = state.resources.map_window(window_id);
        {
            let w = state.resources.window_mut(window_id).unwrap();
            w.host_xid = Some(crate::backend::WindowHandle::from_raw_for_test(host_xid));
            w.redirected_backing = Some(crate::resources::RedirectedBacking {
                host_pixmap: crate::backend::PixmapHandle::from_raw_for_test(backing_xid),
                width: 100,
                height: 100,
                depth: 24,
            });
        }
        // The compositor owns `RedirectSubwindows(root, Manual)`.
        state
            .composite_redirects
            .redirect_subwindows(
                ROOT_WINDOW,
                state.resources.children(ROOT_WINDOW),
                RedirectRecord {
                    mode: CompositeRedirectMode::Manual,
                    owner: ClientId(compositor),
                },
            )
            .unwrap();

        let mut backend = RecordingBackend::new();
        process_disconnect(&mut state, &mut backend, ClientId(compositor));

        // W's redirect state is cleared.
        let w = state.resources.window(window_id).expect("W survives");
        assert!(w.redirected_backing.is_none());
        // The backend saw both the backing release and the
        // participation restore — in that order.
        let calls = backend.calls();
        let release_idx = calls
            .iter()
            .position(
                |c| matches!(c, RecordedCall::ReleaseRedirectedBacking(x) if *x == backing_xid),
            )
            .expect("release_redirected_backing recorded");
        let restore_idx = calls
            .iter()
            .position(|c| {
                matches!(
                    c,
                    RecordedCall::SetWindowSceneParticipation {
                        host_window,
                        participating: true,
                    } if *host_window == host_xid
                )
            })
            .expect("set_window_scene_participation(host, true) recorded");
        assert!(
            release_idx < restore_idx,
            "release must precede participation restore; calls={calls:#?}",
        );
    }

    #[test]
    fn disconnect_of_a_redirected_windows_owner_releases_its_backing() {
        // The app exits without DestroyWindow under the compositor's RedirectSubwindows(root).
        let mut state = ServerState::new();
        let compositor = 9;
        let app = 10;
        install_client(&mut state, compositor);
        install_client(&mut state, app);
        let window_id = ResourceId(0x00a0_0001);
        let backing_xid: u32 = 0xBA51_0001;
        state.resources.create_window(
            ClientId(app),
            CreateWindowRequest {
                depth: 24,
                window: window_id,
                parent: ROOT_WINDOW,
                width: 100,
                height: 100,
                class: 1,
                visual: crate::resources::ROOT_VISUAL,
                ..Default::default()
            },
        );
        let _ = state.resources.map_window(window_id);
        state
            .resources
            .window_mut(window_id)
            .unwrap()
            .redirected_backing = Some(crate::resources::RedirectedBacking {
            host_pixmap: crate::backend::PixmapHandle::from_raw_for_test(backing_xid),
            width: 100,
            height: 100,
            depth: 24,
        });
        let record = RedirectRecord {
            mode: CompositeRedirectMode::Manual,
            owner: ClientId(compositor),
        };
        state
            .composite_redirects
            .redirect_subwindows(ROOT_WINDOW, &[window_id], record)
            .unwrap();

        let mut backend = RecordingBackend::new();
        process_disconnect(&mut state, &mut backend, ClientId(app));

        let releases = backend
            .calls()
            .iter()
            .filter(|c| matches!(c, RecordedCall::ReleaseRedirectedBacking(x) if *x == backing_xid))
            .count();
        assert_eq!(releases, 1, "calls={:#?}", backend.calls());
        assert!(state.resources.window(window_id).is_none());
        assert!(state.composite_redirects.window_mode(window_id).is_none());
        assert!(
            state
                .composite_redirects
                .subwindows_mode(ROOT_WINDOW)
                .is_some(),
            "the compositor's root redirect outlives the app"
        );
    }

    #[test]
    fn disconnect_frees_pointer_barriers() {
        let mut state = ServerState::new();
        let mut backend = RecordingBackend::new();
        install_client(&mut state, 1);
        state.pointer_barriers.insert(
            0x0040_0001,
            crate::server::PointerBarrier {
                owner: ClientId(1),
                window: ROOT_WINDOW,
                x1: 0,
                y1: 0,
                x2: 0,
                y2: 10,
                directions: 0,
                devices: Vec::new(),
                hit: false,
                seen: false,
                event_id: 1,
                release_event_id: 0,
                last_timestamp: 0,
            },
        );
        process_disconnect(&mut state, &mut backend, ClientId(1));
        assert!(state.pointer_barriers.is_empty());
    }

    /// Drive one core or extension request from `client`.
    fn request(
        state: &mut ServerState,
        backend: &mut RecordingBackend,
        client: u32,
        opcode: u8,
        data: u8,
        body: &[u8],
    ) {
        crate::core_loop::process_request::process_request(
            state,
            backend,
            ClientId(client),
            yserver_protocol::x11::SequenceNumber(1),
            yserver_protocol::x11::RequestHeader {
                opcode,
                data,
                length_units: u32::try_from(1 + body.len().div_ceil(4)).expect("length"),
            },
            body,
            None,
        )
        .expect("process_request");
    }

    /// Compositor client 2 takes the overlay and reparents it under `w`
    /// (ReparentWindow, opcode 7); then the overlay release starts failing.
    fn cow_under(state: &mut ServerState, backend: &mut RecordingBackend, w: u32) {
        use crate::resources::COMPOSITE_OVERLAY_WINDOW;
        request(
            state,
            backend,
            2,
            144,
            yserver_protocol::x11::composite::GET_OVERLAY_WINDOW,
            &ROOT_WINDOW.0.to_le_bytes(),
        );
        let mut body = COMPOSITE_OVERLAY_WINDOW.0.to_le_bytes().to_vec();
        body.extend_from_slice(&w.to_le_bytes());
        body.extend_from_slice(&[0; 4]);
        request(state, backend, 2, 7, 0, &body);
        assert_eq!(
            state
                .resources
                .window(COMPOSITE_OVERLAY_WINDOW)
                .map(|cow| cow.parent),
            Some(ResourceId(w)),
            "precondition: the COW is under the client's window"
        );
        backend.cow_teardown_fails = true;
    }

    fn create_window_owned_by(state: &mut ServerState, owner: u32, w: u32, pixmap: u32) {
        state.resources.create_window(
            ClientId(owner),
            CreateWindowRequest {
                depth: 24,
                window: ResourceId(w),
                parent: ROOT_WINDOW,
                width: 100,
                height: 100,
                class: 1,
                visual: crate::resources::ROOT_VISUAL,
                ..Default::default()
            },
        );
        let _ = state.resources.map_window(ResourceId(w));
        state.resources.create_pixmap(
            ClientId(owner),
            CreatePixmapRequest {
                pixmap: ResourceId(pixmap),
                drawable: ROOT_WINDOW,
                width: 16,
                height: 16,
                depth: 24,
            },
        );
    }

    fn assert_teardown_stopped_intact(state: &ServerState, owner: u32, w: u32, pixmap: u32) {
        use crate::resources::COMPOSITE_OVERLAY_WINDOW;
        assert_eq!(
            state.resources.window(ResourceId(w)).map(|w| w.owner),
            Some(ClientId(owner)),
            "the client's window survives, still its own"
        );
        assert_eq!(
            state
                .resources
                .window(COMPOSITE_OVERLAY_WINDOW)
                .map(|cow| cow.parent),
            Some(ResourceId(w)),
            "the COW survives where it was"
        );
        assert_eq!(
            state.resources.resource_owner(ResourceId(pixmap)),
            Some(ClientId(owner)),
            "the client's other resources were not freed"
        );
        assert!(
            state.fatal_teardown.is_some(),
            "the shutdown request is raised"
        );
    }

    /// A disconnect whose window holds the COW, with the overlay release
    /// failing: the teardown stops before anything of the client is freed,
    /// its record and resource-ID range stay, and the server is asked to
    /// shut down (the terminal policy in
    /// `docs/superpowers/plans/2026-10-27-window-destroy-plan.md`).
    #[test]
    fn disconnect_whose_window_holds_a_cow_that_cannot_be_released_stops_and_terminates() {
        let mut state = ServerState::new();
        let mut backend = RecordingBackend::new();
        let (base, mask) = state.id_allocator.allocate().expect("a base");
        install_client(&mut state, 1);
        let client = state.clients.get_mut(&1).expect("client 1");
        client.resource_id_base = base;
        client.resource_id_mask = mask;
        install_client(&mut state, 2);
        let (w, pixmap) = (base | 1, base | 2);
        create_window_owned_by(&mut state, 1, w, pixmap);
        cow_under(&mut state, &mut backend, w);

        process_disconnect(&mut state, &mut backend, ClientId(1));

        assert_teardown_stopped_intact(&state, 1, w, pixmap);
        assert!(state.clients.contains_key(&1), "the client record stays");
        assert_ne!(
            state.id_allocator.allocate().map(|(b, _)| b),
            Some(base),
            "the client's resource-ID range is not handed out again"
        );

        // A second disconnect of the same client (the reader's EOF racing
        // the first) does not run the teardown again.
        backend.calls.lock().expect("calls").clear();
        process_disconnect(&mut state, &mut backend, ClientId(1));
        let calls = backend.calls.lock().expect("calls");
        assert!(calls.is_empty(), "nothing ran: {calls:?}");
    }

    /// The same failure on a zombie's window, through KillClient (opcode
    /// 113) of a resource the zombie owns: nothing is freed and the zombie
    /// stays recorded.
    #[test]
    fn zombie_cleanup_whose_window_holds_a_cow_that_cannot_be_released_stops_and_terminates() {
        const ZOMBIE: u32 = 7;
        let mut state = ServerState::new();
        let mut backend = RecordingBackend::new();
        install_client(&mut state, 2);
        let (w, pixmap) = (0x0070_0001, 0x0070_0002);
        create_window_owned_by(&mut state, ZOMBIE, w, pixmap);
        state.zombie_clients.insert(ZOMBIE, 1);
        cow_under(&mut state, &mut backend, w);

        request(&mut state, &mut backend, 2, 113, 0, &pixmap.to_le_bytes());

        assert_teardown_stopped_intact(&state, ZOMBIE, w, pixmap);
        assert!(
            state.zombie_clients.contains_key(&ZOMBIE),
            "the zombie stays recorded"
        );
    }
}
