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

    let mut owned_roots: Vec<ResourceId> = Vec::new();
    state
        .resources
        .collect_owned_window_roots(client_id, &mut owned_roots);

    let mut pending: Vec<PendingDestroy> = Vec::new();
    let mut all_destroyed: Vec<ResourceId> = Vec::new();
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
        let _ = state.resources.destroy_window(root);
        all_destroyed.extend(order);
    }
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
    state
        .present_msc
        .retain(|window, _| !dead_windows.contains(window));
    state
        .mit_shm_segments
        .retain(|_, seg| seg.owner != client_id);
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
    if state
        .pointer_grab
        .is_some_and(|(owner, _)| owner == client_id)
    {
        state.pointer_grab = None;
        state.pointer_grab_is_passive = false;
        state.frozen_pointer_event = None;
        state.frozen_pointer_queue.clear();
    }
    // Xorg ReleaseActiveGrabs (CloseDownClient): a disconnecting
    // client's ACTIVE grabs must go too, or the stale record makes
    // every later GrabPointer/GrabKeyboard return AlreadyGrabbed.
    if state
        .active_pointer_grab
        .is_some_and(|g| g.owner == client_id)
    {
        state.active_pointer_grab = None;
    }
    if state
        .active_keyboard_grab
        .is_some_and(|g| g.owner == client_id)
    {
        state.active_keyboard_grab = None;
        state.frozen_keyboard_event = None;
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
            crate::core_loop::pointer_fanout::xi1_thaw_device(state, dev);
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
    for xid in removed.closed_fonts {
        let _ = backend.close_font(None, xid);
    }
    for xid in removed.freed_pixmaps {
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
        let _ = state.resources.destroy_window(root);
        all_destroyed.extend(order);
    }
    state.drop_window_subscriptions(&all_destroyed);

    let removed = state.resources.remove_non_window_resources_owned_by(zombie);

    for entry in pending {
        if let Some(xid) = entry.host_xid {
            let _ = backend.destroy_subwindow(None, xid.as_raw());
            backend.unregister_host_window(xid.as_raw());
        }
        fanout_destroy_sequence(state, &entry);
    }
    for xid in removed.closed_fonts {
        let _ = backend.close_font(None, xid);
    }
    for xid in removed.freed_pixmaps {
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
                writer: Arc::new(Mutex::new(a)),
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
            },
        );
    }

    fn install_client_with_writer(state: &mut ServerState, id: u32, writer: UnixStream) {
        state.clients.insert(
            id,
            ClientState {
                writer: Arc::new(Mutex::new(writer)),
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
}
