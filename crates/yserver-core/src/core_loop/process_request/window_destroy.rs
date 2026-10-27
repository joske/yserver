//! The window-destroy executor: Xorg `DeleteWindow` (`dix/window.c:1070`)
//! over one [`PreparedDestroy`], every side effect in Xorg's order.

use std::{collections::HashSet, io};

use yserver_protocol::x11::{self, ClientId, ResourceId, xfixes::SELECTION_NOTIFY_WINDOW_DESTROY};

use crate::{
    backend::{Backend, OriginContext},
    core_loop::{
        fanout::{fanout_event_to_clients, subscribers_by_id},
        process_request::{
            focus_pointer::revert_core_focus_from,
            grabs::{deactivate_core_keyboard_grab, deactivate_core_pointer_grab},
            present_ext::fire_present_window_destroyed,
            selection::drop_selections_owned_by_windows,
            windows::{
                free_pictures_on_destroyed_windows, purge_present_for_destroyed_windows,
                release_dropped_cursors, release_redirects_on_destroyed_windows,
                release_xi1_active_grab, send_exposures,
            },
            xi::deactivate_xi2_device_grab,
        },
    },
    resources::{COMPOSITE_OVERLAY_WINDOW, MapState, PreparedDestroy, ROOT_WINDOW},
    server::ServerState,
};

/// What one executed plan destroyed, for the caller's cleanup that is not
/// window-scoped.
pub(crate) struct DestroyOutcome {
    /// Exactly the plan's windows, CrushTree order.
    pub windows: Vec<ResourceId>,
    /// Background/border host pixmaps the destroyed windows held; candidates
    /// for the orphan gate, not yet freed.
    pub attr_pixmaps: Vec<u32>,
}

/// One planned window as it was before the teardown began.
struct Captured {
    window: ResourceId,
    parent: ResourceId,
    host_xid: Option<crate::backend::WindowHandle>,
    on_window: Vec<ClientId>,
    on_parent: Vec<ClientId>,
}

/// Run one prepared destroy plan to completion.
pub(crate) fn execute_destroy(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    prepared: PreparedDestroy,
) -> DestroyOutcome {
    let plan = prepared.plan();
    let windows: Vec<ResourceId> = plan.windows().collect();
    let top = plan.top();

    // 1. Capture, while the tree is intact.
    let captured: Vec<Captured> = windows
        .iter()
        .map(|&window| {
            let (parent, host_xid) = state
                .resources
                .window(window)
                .map_or((ROOT_WINDOW, None), |w| (w.parent, w.host_xid));
            Captured {
                window,
                parent,
                host_xid,
                on_window: subscribers_by_id(state, window, 0x0002_0000),
                on_parent: subscribers_by_id(state, parent, 0x0008_0000),
            }
        })
        .collect();
    let top_map_state = state
        .resources
        .window(top)
        .map_or(MapState::Unmapped, |w| w.map_state);
    // `ProcDestroyWindow`'s `FreeResource(id, RT_NONE)` frees the newer
    // resources on the top's id first (`dix/resource.c:907`, `:829-833`): its
    // XFIXES selection inputs go before `DeleteWindow` runs.
    drop_xfixes_selection_inputs_on(state, top);

    // 2-4. `UnmapWindow(T, FALSE)` (`dix/window.c:2844-2875`).
    let mut unrealized: HashSet<ResourceId> = HashSet::new();
    if top_map_state != MapState::Unmapped {
        let top_entry = captured.last().expect("a plan has its top window");
        let parent = top_entry.parent;
        let _dropped = fanout_event_to_clients(state, &top_entry.on_window, |buf, seq, order| {
            x11::encode_unmap_notify_event(buf, seq, order, top, top, false);
        });
        let _dropped = fanout_event_to_clients(state, &top_entry.on_parent, |buf, seq, order| {
            x11::encode_unmap_notify_event(buf, seq, order, parent, top, false);
        });
        let realized = top_map_state == MapState::Viewable;
        let clips_before = if realized {
            crate::core_loop::clip_list::with_borders(
                state,
                crate::core_loop::clip_list::clip_lists_under(state, top),
            )
        } else {
            Vec::new()
        };
        if realized {
            // `UnrealizeTree` (`:2780-2820`).
            for w in unrealize_tree_order(state, top) {
                delete_window_from_any_events(state, backend, origin, w, &windows, top);
                unrealized.insert(w);
            }
        }
        let _ = state.resources.unmap_window(top);
        if realized {
            // HandleExposures, then WindowsRestructured (`:2864-2872`).
            let after = crate::core_loop::clip_list::with_borders(
                state,
                crate::core_loop::clip_list::clip_lists_under(state, top),
            );
            send_exposures(
                state,
                backend,
                origin,
                crate::core_loop::clip_list::newly_exposed(&clips_before, after, None),
                None,
            );
            backend.windows_restructured(state);
        }
    }

    // 5. `CrushTree`, then the top (`:1023-1086`): DestroyNotify, then
    // `FreeWindowResources` (`:994-1018`).
    for entry in &captured {
        let (window, parent) = (entry.window, entry.parent);
        let _dropped = fanout_event_to_clients(state, &entry.on_window, |buf, seq, order| {
            x11::encode_destroy_notify_event(buf, seq, order, window, window);
        });
        let _dropped = fanout_event_to_clients(state, &entry.on_parent, |buf, seq, order| {
            x11::encode_destroy_notify_event(buf, seq, order, parent, window);
        });
        // CrushTree's `FreeResource(id, RT_WINDOW)` (`:1043`) frees the
        // inferior's other same-id resources before `FreeWindowResources`.
        drop_xfixes_selection_inputs_on(state, window);
        // `DeleteWindowFromAnySaveSet`, `DeleteWindowFromAnySelections`.
        for client in state.clients.values_mut() {
            client.save_set.remove(&window);
        }
        drop_selections_owned_by_windows(state, &[window], SELECTION_NOTIFY_WINDOW_DESTROY);
        // `DeleteWindowFromAnyEvents(w, TRUE)` (`dix/events.c:5905-6002`).
        if !unrealized.contains(&window) {
            delete_window_from_any_events(state, backend, origin, window, &windows, top);
        }
        state.button_grabs.retain(|g| g.grab_window != window);
        state.key_grabs.retain(|g| g.grab_window != window);
        state.xi1_passive_grabs.retain(|g| g.grab_window != window);
        state.drop_window_subscriptions(&[window]);
        // `(*pScreen->DestroyWindow)` (`dix/window.c:1018`): `present_destroy_window`
        // (`present/present_screen.c:89-120`) sends PresentWindowDestroyed,
        // then frees the window's selections. The purge can trigger idle
        // fences, and a fired SYNC await sends CounterNotify.
        fire_present_window_destroyed(state, window);
        purge_present_for_destroyed_windows(state, backend, &[window]);
    }

    // 6. Resource-level cleanup; nothing here reaches the wire.
    free_pictures_on_destroyed_windows(state, backend, origin, &windows);
    release_redirects_on_destroyed_windows(state, backend, origin, &windows);
    let attr_pixmaps = state.resources.collect_attribute_pixmap_host_xids(top);
    state.resources.apply_destroy(plan);
    if plan.contains_cow() {
        state.destroy_cow_input_shape();
    }
    for entry in &captured {
        // CrushTree order puts the COW's children first, so they are torn
        // down while the backend COW still exists.
        if entry.window == COMPOSITE_OVERLAY_WINDOW && prepared.retires_overlay() {
            if let Some(xid) = entry.host_xid {
                backend.unregister_host_window(xid.as_raw());
            }
            backend.retire_overlay_window(origin);
            continue;
        }
        if let Some(xid) = entry.host_xid {
            backend.unregister_host_window(xid.as_raw());
            let _ = backend.update_host_event_mask(
                origin,
                xid.as_raw(),
                crate::host_x11::POINTER_EVENT_MASK,
                false,
            );
            let _ = backend.update_host_event_mask(
                origin,
                xid.as_raw(),
                crate::host_x11::SUBWINDOW_EVENT_MASK,
                false,
            );
            let _ = backend.destroy_subwindow(origin, xid.as_raw());
        }
    }
    backend.sync_top_level_order(state);
    release_dropped_cursors(state, backend, origin);
    DestroyOutcome {
        windows,
        attr_pixmaps,
    }
}

/// Plan, prepare and execute the destroy of `window`, then free its orphaned
/// attribute pixmaps. Root and unknown ids are a no-op. `Err`: preparation
/// failed and nothing changed.
pub(crate) fn destroy_window_subtree(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    window: ResourceId,
) -> io::Result<()> {
    let Some(plan) = state.resources.plan_destroy(window) else {
        return Ok(());
    };
    let prepared = plan.prepare(backend)?;
    finish_destroy(state, backend, origin, prepared);
    Ok(())
}

/// Execute an explicit destroy and free its orphaned attribute pixmaps.
pub(super) fn finish_destroy(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    prepared: PreparedDestroy,
) {
    let outcome = execute_destroy(state, backend, origin, prepared);
    crate::core_loop::process_disconnect::free_orphaned_host_pixmaps(
        state,
        backend,
        outcome.attr_pixmaps,
        &[],
        None,
    );
}

/// The realized windows of `top`'s subtree in `UnrealizeTree` order
/// (`dix/window.c:2780-2820`): `top` first, then depth-first, topmost child
/// first, descending only into realized windows.
fn unrealize_tree_order(state: &ServerState, top: ResourceId) -> Vec<ResourceId> {
    fn walk(state: &ServerState, w: ResourceId, out: &mut Vec<ResourceId>) {
        let Some(win) = state.resources.window(w) else {
            return;
        };
        if win.map_state != MapState::Viewable {
            return;
        }
        out.push(w);
        for child in win.children.iter().rev() {
            walk(state, *child, out);
        }
    }
    let mut out = Vec::new();
    walk(state, top, &mut out);
    out
}

/// The input half of `DeleteWindowFromAnyEvents(w, …)` (`dix/events.c:5905-5985`,
/// `Xi/exevents.c:3027-3100`): every device's active grab on `w` (window or
/// confine-to), whoever owns it, deactivates; then every keyboard focus on
/// `w` reverts. `dying` is the whole plan, `dying_root` its top.
fn delete_window_from_any_events(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    w: ResourceId,
    dying: &[ResourceId],
    dying_root: ResourceId,
) {
    if let Some(grab) = state.active_pointer_grab
        && (grab.grab_window == w || state.pointer_confine_to == w)
    {
        deactivate_core_pointer_grab(state, backend, grab.owner);
    }
    if let Some(grab) = state.active_keyboard_grab
        && grab.grab_window == w
    {
        deactivate_core_keyboard_grab(state, grab.owner);
    }
    let mut pointer_grabs: Vec<(u16, ClientId)> = state
        .xi2_pointer_grabs
        .iter()
        .filter(|(_, g)| g.grab_window == w)
        .map(|(dev, g)| (*dev, g.owner))
        .collect();
    pointer_grabs.sort_unstable_by_key(|(dev, _)| *dev);
    for (dev, owner) in pointer_grabs {
        deactivate_xi2_device_grab(state, backend, origin, owner, dev, false, false);
    }
    let mut keyboard_grabs: Vec<(u16, ClientId)> = state
        .xi2_keyboard_grabs
        .iter()
        .filter(|(_, g)| g.grab_window == w)
        .map(|(dev, g)| (*dev, g.owner))
        .collect();
    keyboard_grabs.sort_unstable_by_key(|(dev, _)| *dev);
    for (dev, owner) in keyboard_grabs {
        deactivate_xi2_device_grab(state, backend, origin, owner, dev, true, false);
    }
    let mut xi1_grabs: Vec<u16> = state
        .xi1_active_grabs
        .iter()
        .filter(|(_, g)| g.grab_window == w)
        .map(|(dev, _)| *dev)
        .collect();
    xi1_grabs.sort_unstable();
    for dev in xi1_grabs {
        release_xi1_active_grab(state, backend, dev);
    }
    revert_core_focus_from(state, w, dying);
    crate::core_loop::xi1_focus::revert_focus_on_dying_window(state, w, dying_root);
}

/// Every client's XFIXES SelectSelectionInput on `window` (Xorg
/// `SelectionFreeWindow`, `xfixes/select.c:244`).
fn drop_xfixes_selection_inputs_on(state: &mut ServerState, window: ResourceId) {
    state
        .xfixes_selection_masks
        .retain(|(_, w, _), _| *w != window);
}
