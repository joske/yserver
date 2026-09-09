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

use crate::{
    backend::Backend,
    core_loop::fanout::{fanout_event_to_clients, subscribers_by_id},
    resources::{MapState, ROOT_WINDOW},
    server::ServerState,
};

/// One window's identity captured before the resource table forgets
/// it, so the post-mutation UnmapNotify+DestroyNotify fanout has
/// stable subscriber lists.
struct PendingDestroy {
    window: ResourceId,
    parent: ResourceId,
    was_mapped: bool,
    host_xid: Option<crate::backend::WindowHandle>,
    on_window: Vec<ClientId>,
    on_parent: Vec<ClientId>,
}

fn collect_destroy_order(
    table: &crate::resources::ResourceTable,
    root: ResourceId,
    out: &mut Vec<ResourceId>,
) {
    let Some(w) = table.window(root) else {
        return;
    };
    for child in w.children.clone() {
        collect_destroy_order(table, child, out);
    }
    out.push(root);
}

fn fanout_destroy_sequence(state: &mut ServerState, pending: &PendingDestroy) {
    let window = pending.window;
    let parent = pending.parent;
    if pending.was_mapped {
        let _dropped = fanout_event_to_clients(state, &pending.on_window, |buf, seq, order| {
            yserver_protocol::x11::encode_unmap_notify_event(
                buf, seq, order, window, window, false,
            );
        });
        let _dropped = fanout_event_to_clients(state, &pending.on_parent, |buf, seq, order| {
            yserver_protocol::x11::encode_unmap_notify_event(
                buf, seq, order, parent, window, false,
            );
        });
    }
    let _dropped = fanout_event_to_clients(state, &pending.on_window, |buf, seq, order| {
        yserver_protocol::x11::encode_destroy_notify_event(buf, seq, order, window, window);
    });
    let _dropped = fanout_event_to_clients(state, &pending.on_parent, |buf, seq, order| {
        yserver_protocol::x11::encode_destroy_notify_event(buf, seq, order, parent, window);
    });
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
    // Idempotent: a client can be disconnected twice in quick succession
    // (write-side EPIPE from process_request races the reader thread's
    // EOF → Message::ClientDisconnected). The first call removes the
    // entry from state.clients; the second sees None and bails.
    if !state.clients.contains_key(&client_id.0) {
        return;
    }
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

    let mut owned_roots: Vec<ResourceId> = Vec::new();
    state
        .resources
        .collect_owned_window_roots(client_id, &mut owned_roots);

    let mut pending: Vec<PendingDestroy> = Vec::new();
    let mut all_destroyed: Vec<ResourceId> = Vec::new();
    // Attribute pixmaps of the dying subtrees, snapshotted BEFORE the windows
    // go away — the same thing `destroy_window_subtree` does. Without it a
    // tile whose only reference was a destroyed window's background or border
    // leaks: its pixmap resource may already be gone (FreePixmap retained it
    // through the window), so `remove_non_window_resources_owned_by` has
    // nothing to hand back and nothing is left to notice (#133).
    let mut attr_pixmap_xids: Vec<u32> = Vec::new();
    for root in owned_roots {
        // XI1 device focus on a window in this dying subtree reverts
        // while the tree is still intact (RevertToParent walks the
        // surviving ancestors). A leaked focus on a destroyed window
        // would silently eat all later DeviceKey events.
        crate::core_loop::xi1_focus::revert_focus_for_dying_subtree(state, root);
        let mut order: Vec<ResourceId> = Vec::new();
        collect_destroy_order(&state.resources, root, &mut order);
        for w in &order {
            let (parent, was_mapped, host_xid) =
                state
                    .resources
                    .window(*w)
                    .map_or((ROOT_WINDOW, false, None), |win| {
                        (
                            win.parent,
                            win.map_state != MapState::Unmapped,
                            win.host_xid,
                        )
                    });
            let on_window = subscribers_by_id(state, *w, 0x0002_0000);
            let on_parent = subscribers_by_id(state, parent, 0x0008_0000);
            pending.push(PendingDestroy {
                window: *w,
                parent,
                was_mapped,
                host_xid,
                on_window,
                on_parent,
            });
        }
        attr_pixmap_xids.extend(state.resources.collect_attribute_pixmap_host_xids(root));
        let _ = state.resources.destroy_window(root);
        all_destroyed.extend(order);
    }
    crate::core_loop::process_request::purge_present_for_destroyed_windows(
        state,
        backend,
        &all_destroyed,
    );
    state.drop_window_subscriptions(&all_destroyed);

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
    state
        .shape_windows
        .retain(|window, _| !dead_windows.contains(window));
    state
        .shape_select_masks
        .retain(|(owner, window), _| *owner != client_id.0 && !dead_windows.contains(window));
    state
        .sync_counters
        .retain(|_, counter| counter.owner != client_id);
    state
        .sync_alarms
        .retain(|_, alarm| alarm.owner != client_id);
    state
        .sync_fences
        .retain(|_, fence| fence.owner != client_id);
    state.sync_pending_awaits.retain(|a| a.client != client_id);
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
    // L2 plan B.1b: walk redirects owned by the departing client and
    // tear each one down (the helper handles `Window.redirected_backing`
    // reset + alias_registry refcount decrement when B.6c lands; for
    // now it's a logged no-op so the wiring is in place when the
    // backing-allocation tasks land). Then filter by both ownership
    // and dead-window so any leftovers caught by the previous rule
    // are still removed.
    let owned_redirects: Vec<(ResourceId, bool)> = state
        .composite_redirects
        .iter()
        .filter(|(_, rec)| rec.owner == client_id)
        .map(|((win, sub), _)| (*win, *sub))
        .collect();
    // Stage 4b: symmetric to the COMPOSITE `UnredirectSubwindows`
    // dispatch arm in `process_request.rs` — a subtree entry tears
    // down each *child*, not the parent itself (the parent's own
    // `redirected_backing` belongs to a separate `(parent, false)`
    // entry, if any).
    for (window, subwindows) in &owned_redirects {
        if *subwindows {
            let kids: Vec<ResourceId> = state.resources.children(*window).to_vec();
            for child in kids {
                teardown_redirect_for_window(state, backend, None, child);
            }
        } else {
            teardown_redirect_for_window(state, backend, None, *window);
        }
    }
    state
        .composite_redirects
        .retain(|(window, _), rec| rec.owner != client_id && !dead_windows.contains(window));
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
    state
        .randr_select_masks
        .retain(|(owner, window), _| *owner != client_id.0 && !dead_windows.contains(window));
    state
        .xkb_select_event_masks
        .retain(|(owner, _), _| *owner != client_id.0);
    state.dpms.selected_by.remove(&client_id);
    state.screensaver.selected_by.remove(&client_id);
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
    state.button_grabs.retain(|g| g.owner != client_id);
    state.key_grabs.retain(|g| g.owner != client_id);
    let released_pointer_grab = state
        .active_pointer_grab
        .is_some_and(|grab| grab.owner == client_id);
    if released_pointer_grab {
        state.clear_pointer_grab();
        if let Some(freeze) = state
            .xi1_frozen
            .get_mut(&crate::xinput::DEVICEID_SLAVE_POINTER)
        {
            freeze.stored = None;
            freeze.state = crate::server::Xi1SyncState::Thawed;
            freeze.other = None;
        }
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
            .get_mut(&crate::xinput::DEVICEID_SLAVE_KEYBOARD)
        {
            freeze.stored = None;
        }
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
    state
        .selections
        .retain(|_, entry| !dead_windows.contains(&entry.0));

    // Host-side teardown. Order matches `nested::handle_client`'s tail
    // so behavior is bit-identical.
    for entry in pending {
        if let Some(xid) = entry.host_xid {
            let _ = backend.destroy_subwindow(None, xid.as_raw());
            backend.unregister_host_window(xid.as_raw());
        }
        fanout_destroy_sequence(state, &entry);
    }
    // Step 2 (DRIFT 2): destroyed top-levels changed root's child set;
    // reproject the backend top-level order from core.
    backend.sync_top_level_order(state);
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
    let mut freeable = removed.freed_pixmaps;
    freeable.extend(attr_pixmap_xids);
    freeable.sort_unstable();
    freeable.dedup();
    for xid in freeable {
        if crate::backend::PixmapHandle::from_raw(xid)
            .is_some_and(|handle| state.resources.host_xid_still_referenced(handle))
        {
            continue;
        }
        let _ = backend.free_pixmap(None, xid);
    }
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
    let (host_window, backing) = {
        let Some(w) = state.resources.window_mut(window) else {
            return;
        };
        (w.host_xid, w.redirected_backing.take())
    };
    let Some(backing) = backing else {
        return;
    };
    if let Err(err) = backend.release_redirected_backing(origin, backing.host_pixmap) {
        log::warn!(
            "release_redirected_backing(0x{:x}) failed: {err}",
            backing.host_pixmap.as_raw()
        );
    }
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
    let Some(host_window) = host_window else {
        log::debug!(
            "teardown_redirect_for_window(0x{:x}): no host_xid; skipping participation restore",
            window.0
        );
        return;
    };
    if let Err(err) = backend.set_window_scene_participation(origin, host_window, true) {
        log::warn!(
            "teardown_redirect_for_window: set_window_scene_participation(0x{:x}, true) failed: {err}",
            window.0
        );
    }
}

/// Destroy every resource owned by a zombie client — invoked by
/// `KillClient(AllTemporary)` (for each RetainTemporary zombie) and by
/// `KillClient(resource_owned_by_a_zombie)`. Mirrors the resource-
/// destroy half of `process_disconnect`, minus the live-client setup
/// (there is no `state.clients` entry, no reader thread, no
/// connection-tied extension state — those were torn down at the
/// original disconnect). The caller must remove `zombie` from
/// `state.zombie_clients` after this returns.
pub fn destroy_zombie_resources(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    zombie: ClientId,
) {
    let mut owned_roots: Vec<ResourceId> = Vec::new();
    state
        .resources
        .collect_owned_window_roots(zombie, &mut owned_roots);

    let mut pending: Vec<PendingDestroy> = Vec::new();
    let mut all_destroyed: Vec<ResourceId> = Vec::new();
    // Attribute pixmaps of the dying subtrees, snapshotted BEFORE the windows
    // go away — the same thing `destroy_window_subtree` does. Without it a
    // tile whose only reference was a destroyed window's background or border
    // leaks: its pixmap resource may already be gone (FreePixmap retained it
    // through the window), so `remove_non_window_resources_owned_by` has
    // nothing to hand back and nothing is left to notice (#133).
    let mut attr_pixmap_xids: Vec<u32> = Vec::new();
    for root in owned_roots {
        // XI1 device focus on a window in this dying subtree reverts
        // while the tree is still intact (RevertToParent walks the
        // surviving ancestors). A leaked focus on a destroyed window
        // would silently eat all later DeviceKey events.
        crate::core_loop::xi1_focus::revert_focus_for_dying_subtree(state, root);
        let mut order: Vec<ResourceId> = Vec::new();
        collect_destroy_order(&state.resources, root, &mut order);
        for w in &order {
            let (parent, was_mapped, host_xid) =
                state
                    .resources
                    .window(*w)
                    .map_or((ROOT_WINDOW, false, None), |win| {
                        (
                            win.parent,
                            win.map_state != MapState::Unmapped,
                            win.host_xid,
                        )
                    });
            let on_window = subscribers_by_id(state, *w, 0x0002_0000);
            let on_parent = subscribers_by_id(state, parent, 0x0008_0000);
            pending.push(PendingDestroy {
                window: *w,
                parent,
                was_mapped,
                host_xid,
                on_window,
                on_parent,
            });
        }
        attr_pixmap_xids.extend(state.resources.collect_attribute_pixmap_host_xids(root));
        let _ = state.resources.destroy_window(root);
        all_destroyed.extend(order);
    }
    crate::core_loop::process_request::purge_present_for_destroyed_windows(
        state,
        backend,
        &all_destroyed,
    );
    state.drop_window_subscriptions(&all_destroyed);

    let removed = state.resources.remove_non_window_resources_owned_by(zombie);

    for entry in pending {
        if let Some(xid) = entry.host_xid {
            let _ = backend.destroy_subwindow(None, xid.as_raw());
            backend.unregister_host_window(xid.as_raw());
        }
        fanout_destroy_sequence(state, &entry);
    }
    // Step 2 (DRIFT 2): destroyed top-levels changed root's child set;
    // reproject the backend top-level order from core.
    backend.sync_top_level_order(state);
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
    let mut freeable = removed.freed_pixmaps;
    freeable.extend(attr_pixmap_xids);
    freeable.sort_unstable();
    freeable.dedup();
    for xid in freeable {
        if crate::backend::PixmapHandle::from_raw(xid)
            .is_some_and(|handle| state.resources.host_xid_still_referenced(handle))
        {
            continue;
        }
        let _ = backend.free_pixmap(None, xid);
    }
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
    for syncobj_xid in removed.freed_dri3_syncobjs {
        if let Err(e) = backend.dri3_free_syncobj(zombie, syncobj_xid) {
            log::warn!(
                "zombie cleanup: free DRI3 syncobj 0x{syncobj_xid:x} for client {} failed: {e}",
                zombie.0
            );
        }
    }
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
                focused_window: ROOT_WINDOW,
                reader_control: None,
                is_local: true,
                fd_passing: true,
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
                focused_window: ROOT_WINDOW,
                reader_control: None,
                is_local: true,
                fd_passing: true,
            },
        );
    }

    #[test]
    fn disconnect_leaves_other_clients_redirects_intact() {
        let mut state = ServerState::new();
        install_client(&mut state, 1);
        install_client(&mut state, 2);
        // Client A redirects window W.
        state.composite_redirects.insert(
            (ResourceId(0x1234), false),
            RedirectRecord {
                mode: CompositeRedirectMode::Manual,
                owner: ClientId(1),
            },
        );
        let mut backend = RecordingBackend::new();
        process_disconnect(&mut state, &mut backend, ClientId(2));
        assert!(
            state
                .composite_redirects
                .contains_key(&(ResourceId(0x1234), false))
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
        state.composite_redirects.insert(
            (ResourceId(0x5678), false),
            RedirectRecord {
                mode: CompositeRedirectMode::Manual,
                owner: ClientId(1),
            },
        );
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
        destroy_zombie_resources(&mut state, &mut backend, ClientId(7));
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

        destroy_zombie_resources(&mut state, &mut backend, ClientId(7));
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
        destroy_zombie_resources(&mut state, &mut backend, ClientId(7));
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

        destroy_zombie_resources(&mut state, &mut backend, ClientId(32));

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
        state.composite_redirects.insert(
            (ROOT_WINDOW, true),
            RedirectRecord {
                mode: CompositeRedirectMode::Manual,
                owner: ClientId(compositor),
            },
        );

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
}
