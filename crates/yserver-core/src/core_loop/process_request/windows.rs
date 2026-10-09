use super::*;

fn resolve_host_subwindow_visual_to_state(
    state: &ServerState,
    window: ResourceId,
) -> crate::host_x11::HostSubwindowVisual {
    use crate::host_x11::HostSubwindowVisual;
    let Some(window) = state.resources.window(window) else {
        return HostSubwindowVisual::CopyFromParent;
    };
    if window.visual == crate::resources::ROOT_VISUAL {
        return HostSubwindowVisual::CopyFromParent;
    }
    let Some(visual) = state.resources.visual(window.visual) else {
        return HostSubwindowVisual::CopyFromParent;
    };
    let Some(visual_xid) = visual.host_visual_xid else {
        return HostSubwindowVisual::DepthOnly {
            depth: window.depth,
        };
    };
    let Some(colormap) = state.resources.colormap_for_visual(window.visual) else {
        return HostSubwindowVisual::DepthOnly {
            depth: window.depth,
        };
    };
    let Some(colormap_xid) = colormap.host_colormap_xid else {
        return HostSubwindowVisual::DepthOnly {
            depth: window.depth,
        };
    };
    HostSubwindowVisual::Explicit {
        depth: visual.depth,
        visual_xid: visual_xid.as_raw(),
        colormap_xid: colormap_xid.as_raw(),
    }
}

/// Xorg's `miWindowExposures` (`mi/miexpose.c:375-410`) for `region` of
/// `window` (its content space): its background over the region when
/// `paint` (unless None), and Expose events to the clients that selected
/// them, one per rect with the count still to follow, or the extents
/// alone past `RECTLIMIT` rects.
pub(super) fn send_window_exposures(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    window: ResourceId,
    region: &[x11::xfixes::RegionRect],
    paint: bool,
) {
    if region.is_empty() {
        return;
    }
    if paint
        && let Some(bg) = state.resources.window_resolved_background(window)
        && let Some(target) = state.resources.host_drawable_target(window)
    {
        for r in region {
            let _ = backend.clear_area(
                origin,
                target.host_xid(),
                bg.background_pixel,
                bg.background_pixmap_host_xid.map(|h| h.as_raw()),
                r.x,
                r.y,
                r.width,
                r.height,
                bg.tile_origin_offset,
            );
            let _dropped = accumulate_damage_to_state(state, window, r.x, r.y, r.width, r.height);
        }
    }
    if subscribers_by_id(state, window, 0x0000_8000).is_empty() {
        return;
    }
    let extents;
    let events = if region.len() > crate::core_loop::clip_list::RECTLIMIT {
        extents = [crate::nested::region_extents(region)];
        &extents[..]
    } else {
        region
    };
    let last = events.len() - 1;
    for (i, r) in events.iter().enumerate() {
        let count = u16::try_from(last - i).unwrap_or(u16::MAX);
        let r = *r;
        let _dropped = emit_window_event_to_state(state, window, 0x0000_8000, |buf, seq, order| {
            x11::encode_expose_event(
                buf,
                seq,
                order,
                window,
                u16::try_from(r.x).unwrap_or(0),
                u16::try_from(r.y).unwrap_or(0),
                r.width,
                r.height,
                count,
            );
        });
    }
}

/// The Expose events of windows becoming viewable: each of `tops`, then
/// its inferiors, a window before its children and children top-most
/// first, each for its whole clip list (`miHandleValidateExposures`,
/// `mi/miwindow.c`, after `miComputeClips` gives a newly viewable window
/// its clip list as exposed). Their backgrounds are painted on realize.
fn send_map_exposures(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    tops: &[ResourceId],
) {
    for top in tops {
        for (w, region) in crate::core_loop::clip_list::subtree_clip_lists(state, *top) {
            send_window_exposures(state, backend, origin, w, &region, false);
        }
    }
}

/// One window's worth of identity captured *before* destroy_window
/// runs, so the subsequent UnmapNotify+DestroyNotify fanout has stable
/// targets even though the resource table has already let go of the
/// Window struct.
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
    // Xorg CrushTree (`dix/window.c:1023`): inferiors first, topmost first.
    for child in w.children.clone().into_iter().rev() {
        collect_destroy_order(table, child, out);
    }
    out.push(root);
}

/// DestroyNotify only: the destroyed window's UnmapNotify went out before the
/// teardown, and its inferiors get none (Xorg CrushTree).
fn fanout_destroy_sequence_to_state(state: &mut ServerState, pending: &PendingDestroy) {
    let window = pending.window;
    let parent = pending.parent;
    let _dropped = fanout_event_to_clients(state, &pending.on_window, |buf, seq, order| {
        x11::encode_destroy_notify_event(buf, seq, order, window, window);
    });
    let _dropped = fanout_event_to_clients(state, &pending.on_parent, |buf, seq, order| {
        x11::encode_destroy_notify_event(buf, seq, order, parent, window);
    });
}

/// Purge every core-visible Present population targeting a destroyed window
/// subtree. Backend-hidden GPU/direct events cannot be cancelled here; the
/// removed window generation makes their later drain discard wire delivery and
/// release only at a safe retirement (direct sources wait for retired-idle).
/// Shared by explicit/COW destruction and both disconnect/zombie paths so
/// numeric XID reuse always starts with fresh CRTC/MSC state.
pub(crate) fn purge_present_for_destroyed_windows(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    windows: &[ResourceId],
) {
    let destroyed: HashSet<u32> = windows.iter().map(|window| window.0).collect();
    if destroyed.is_empty() {
        return;
    }

    state
        .present_pending_msc
        .retain(|pending| !destroyed.contains(&pending.window));
    state
        .present_window_msc
        .retain(|window, _| !destroyed.contains(window));
    state
        .present_window_generations
        .retain(|window, _| !destroyed.contains(window));
    state
        .present_event_selections
        .retain(|_, selection| !destroyed.contains(&selection.window.0));

    let stale_present_ids: Vec<u64> = state
        .present_pending_exec
        .iter()
        .filter_map(|(&pid, entry)| {
            destroyed
                .contains(&entry.pending.request.window())
                .then_some(pid)
        })
        .collect();
    for pid in stale_present_ids {
        let Some(entry) = state.present_pending_exec.remove(&pid) else {
            continue;
        };
        match &entry.pending.wake {
            crate::backend::PresentWake::Pixmap { idle_fence_xid } if *idle_fence_xid != 0 => {
                if let Err(error) = backend.dri3_trigger_fence(*idle_fence_xid) {
                    log::warn!(
                        "PRESENT teardown: trigger idle fence 0x{idle_fence_xid:x} failed: {error}"
                    );
                }
                crate::core_loop::sync_await::fence_triggered(state, *idle_fence_xid);
            }
            crate::backend::PresentWake::PixmapSynced {
                release,
                release_value,
                release_syncobj,
            } => {
                if let Err(error) = release.signal(*release_value) {
                    log::warn!(
                        "PRESENT teardown: signal release syncobj 0x{release_syncobj:x}@\
                         {release_value} failed: {error}"
                    );
                }
            }
            _ => {}
        }
        if let Some(wait_id) = entry.wait_id {
            backend.finish_present_source_wait(wait_id);
            state.present_wait_to_id.remove(&wait_id);
        }
        if let Some(pin) = entry.pin {
            backend.release_present_source(pin);
        }
    }

    let stale_completions: Vec<_> = state
        .present_pending_complete
        .iter()
        .filter(|pending| destroyed.contains(&pending.event.dst_host_xid))
        .map(|pending| pending.event.clone())
        .collect();
    for event in &stale_completions {
        // Copy completions (`emit_idle=true`) have already retired their GPU
        // read and may release now. Direct completions deliberately remain
        // busy until their later retired-idle event; releasing them here could
        // hand a still-scanned buffer back to the client.
        discard_stale_present_event(state, backend, event, false);
    }
    state
        .present_pending_complete
        .retain(|pending| !destroyed.contains(&pending.event.dst_host_xid));

    let stale_gate_ids: Vec<u64> = state
        .present_complete_gate
        .iter()
        .filter_map(|(&id, gate)| destroyed.contains(&gate.dst_window_xid).then_some(id))
        .collect();
    for id in stale_gate_ids {
        // The backend completion is still in flight. It is not safe to release
        // either a copy source (GPU may still read it) or a direct source
        // (scanout may still own it). The generation check in the backend-event
        // drain discards its eventual wire event and releases at the matching
        // retirement instead.
        state.present_complete_gate.remove(&id);
    }
}

/// Common subtree-destroy used by both DestroyWindow (root = the
/// requested window) and DestroySubwindows (each child of the
/// requested parent).
/// Free every Picture on the doomed `windows`, whichever client owns it, while the
/// window records still exist (Xorg `PictureDestroyWindow`, `render/picture.c:67`).
pub(crate) fn free_pictures_on_destroyed_windows(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    windows: &[ResourceId],
) {
    for (pic_xid, owned_pix) in state.resources.remove_pictures_on_windows(windows) {
        let _ = backend.render_free_picture(origin, pic_xid);
        if let Some(pix_xid) = owned_pix {
            let _ = backend.free_pixmap(origin, pix_xid);
        }
    }
}

/// Release each doomed window's redirect backing and drop every redirect record keyed on it,
/// while the window records still exist (Xorg `compDestroyWindow`, `composite/compwindow.c:600`).
/// Surviving `NameWindowPixmap` aliases keep the backing alive until their `FreePixmap`.
pub(crate) fn release_redirects_on_destroyed_windows(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    windows: &[ResourceId],
) {
    for window in windows {
        crate::core_loop::process_disconnect::unrealize_redirect_backing(
            state, backend, origin, *window,
        );
    }
    state.composite_redirects.forget_windows(windows);
}

pub(super) fn destroy_window_subtree(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    root: ResourceId,
) {
    // XI1 device focus inside the dying subtree reverts before the
    // tree is torn down (the RevertToParent walk needs the surviving
    // ancestors) — Xorg DeleteWindowFromAnyEvents shape.
    crate::core_loop::xi1_focus::revert_focus_for_dying_subtree(state, root);
    let mut order: Vec<ResourceId> = Vec::new();
    collect_destroy_order(&state.resources, root, &mut order);
    // Core focus inside the dying subtree reverts before teardown —
    // Xorg DeleteWindowFromAnyEvents, same shape as the XI1 revert
    // above (the RevertToParent walk needs the surviving ancestors).
    if state.core_focus.raw > 1 {
        let focus_win = ResourceId(state.core_focus.raw);
        if order.contains(&focus_win) {
            revert_core_focus_from(state, focus_win, &order);
        }
    }
    let mut pending: Vec<PendingDestroy> = Vec::new();
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
    // Xorg DeleteWindow unmaps the window first (`dix/window.c:1075`):
    // UnmapNotify, then WindowsRestructured while the subtree still exists,
    // so the pointer's Leave reaches the dying windows before any
    // DestroyNotify. `order` ends with `root`.
    if let Some(top) = pending.last()
        && top.was_mapped
    {
        let (window, parent) = (top.window, top.parent);
        let _dropped = fanout_event_to_clients(state, &top.on_window, |buf, seq, order| {
            x11::encode_unmap_notify_event(buf, seq, order, window, window, false);
        });
        let _dropped = fanout_event_to_clients(state, &top.on_parent, |buf, seq, order| {
            x11::encode_unmap_notify_event(buf, seq, order, parent, window, false);
        });
        let viewable_before = state
            .resources
            .window(window)
            .is_some_and(|w| w.map_state == MapState::Viewable);
        let clips_before = if viewable_before {
            crate::core_loop::clip_list::clip_lists_under(state, window)
        } else {
            Vec::new()
        };
        let _ = state.resources.unmap_window(window);
        // UnmapWindow exposes what the window covered (`dix/window.c:2856-2866`).
        if viewable_before {
            let after = crate::core_loop::clip_list::clip_lists_under(state, window);
            for (w, region) in
                crate::core_loop::clip_list::newly_exposed(&clips_before, after, None)
            {
                send_window_exposures(state, backend, origin, w, &region, true);
            }
        }
        backend.windows_restructured(state);
    }
    let attr_pixmap_xids = state.resources.collect_attribute_pixmap_host_xids(root);
    free_pictures_on_destroyed_windows(state, backend, origin, &order);
    purge_present_for_destroyed_windows(state, backend, &order);
    release_redirects_on_destroyed_windows(state, backend, origin, &order);
    // Audit #9 — selections owned by any destroyed window in this
    // subtree must fire `XFixesSelectionNotify(SelectionWindowDestroy)`
    // and clear ownership (Xorg `xfixes/select.c` registers a
    // `SelectionWindowDestroy` callback against the resource-delete
    // path). MUST run before `destroy_window` drops the resource (so
    // subscribers don't reference a half-destroyed window) and BEFORE
    // `drop_window_subscriptions` (which clears xfixes_selection_masks
    // entries owned by the destroyed window).
    drop_selections_owned_by_windows(
        state,
        &order,
        yserver_protocol::x11::xfixes::SELECTION_NOTIFY_WINDOW_DESTROY,
    );
    let _ = state.resources.destroy_window(root);
    state.drop_window_subscriptions(&order);
    // Step 2 (DRIFT 2): the destroyed subtree may have removed a root
    // child; reproject the backend top-level order from core (the backend
    // backing teardown already ran above).
    backend.sync_top_level_order(state);

    // Same orphan rule as every other release site: the destroyed subtree's
    // retained background AND border pixmaps may still be client-owned
    // (FreePixmap not yet issued) or referenced by windows OUTSIDE the subtree
    // (`resources.destroy_window` already ran, so the check sees only
    // survivors). Free only the fully orphaned ones.
    //
    // Borders used to be collected by neither half of this (#133): the
    // subtree walk skipped them and so did the gate, so a tile whose only
    // remaining reference was a border died with its window and leaked.
    for xid in &attr_pixmap_xids {
        let Some(handle) = crate::backend::PixmapHandle::from_raw(*xid) else {
            continue;
        };
        if state.resources.host_xid_still_referenced(handle) {
            continue;
        }
        let _ = backend.free_pixmap(origin, *xid);
        state.resources.host_pixmap_freed(*xid);
    }
    for entry in pending {
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
        fanout_destroy_sequence_to_state(state, &entry);
    }
    // Active core grabs whose grab window just died deactivate (Xorg
    // DeleteWindowFromAnyEvents) — the destroyed windows are gone from
    // the resource table now, so the viewability probe sees them off.
    release_core_grabs_for_unviewable(state, backend);
    release_dropped_cursors(state, backend, origin);
}

/// Free on the backend each host cursor a window let go of that nothing
/// references any more — a cursor freed while a window still used it goes
/// when that window changes cursor or dies (Xorg `FreeCursor` on
/// `refcnt == 0`, `dix/window.c:968`/`:1559`).
pub(crate) fn release_dropped_cursors(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
) {
    for host in state.resources.take_unreferenced_cursor_hosts() {
        let _ = backend.free_cursor(origin, host);
    }
}

pub(super) fn handle_reparent_window(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    let Some(request) = x11::reparent_window_request(body) else {
        return Ok(RequestOutcome::Handled);
    };
    // Xorg ReparentWindow unmaps a mapped window first (`dix/window.c:2519`):
    // UnmapNotify, and the pointer leaves it while it is still in the old
    // place. Its map state is only hidden for that hit-test; the reparent
    // below keeps the window mapped, and the MapNotify after ReparentNotify
    // stands for Xorg's MapWindow.
    let unmapped = if state.resources.check_reparent_window(request).is_ok() {
        state.resources.window(request.window).and_then(|w| {
            (w.map_state != MapState::Unmapped).then_some((
                w.parent,
                w.map_state,
                w.override_redirect,
            ))
        })
    } else {
        None
    };
    if let Some((old_parent, map_state, _)) = unmapped {
        let window = request.window;
        let clips_before = if map_state == MapState::Viewable {
            crate::core_loop::clip_list::clip_lists_under(state, window)
        } else {
            Vec::new()
        };
        let _dropped = emit_window_event_to_state(state, window, 0x0002_0000, |buf, seq, order| {
            x11::encode_unmap_notify_event(buf, seq, order, window, window, false);
        });
        let _dropped =
            emit_window_event_to_state(state, old_parent, 0x0008_0000, |buf, seq, order| {
                x11::encode_unmap_notify_event(buf, seq, order, old_parent, window, false);
            });
        if let Some(w) = state.resources.window_mut(window) {
            w.map_state = MapState::Unmapped;
        }
        // What it covered in the old place is exposed (`UnmapWindow`,
        // `dix/window.c:2856-2866`).
        if map_state == MapState::Viewable {
            let after = crate::core_loop::clip_list::clip_lists_under(state, window);
            for (w, region) in
                crate::core_loop::clip_list::newly_exposed(&clips_before, after, None)
            {
                send_window_exposures(state, backend, origin, w, &region, true);
            }
        }
        backend.windows_restructured(state);
        if let Some(w) = state.resources.window_mut(window) {
            w.map_state = map_state;
        }
    }
    let result = match state.resources.reparent_window(request) {
        Ok(result) => result,
        Err(crate::resources::ReparentWindowError::BadWindow) => {
            return emit_x11_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_WINDOW,
                request.window.0,
                7,
            );
        }
        Err(crate::resources::ReparentWindowError::BadMatch) => {
            return emit_x11_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_MATCH,
                request.window.0,
                7,
            );
        }
    };
    let on_window = subscribers_by_id(state, result.window, 0x0002_0000);
    let on_old_parent = subscribers_by_id(state, result.old_parent, 0x0008_0000);
    let on_new_parent = if result.old_parent == result.new_parent {
        Vec::new()
    } else {
        subscribers_by_id(state, result.new_parent, 0x0008_0000)
    };
    debug!(
        "client {} #{} ReparentWindow 0x{:x}: 0x{:x}->0x{:x} pos=({},{}) host_xid={:?} map_state={:?}->{:?} viewable+{} viewable-{}",
        client_id.0,
        sequence.0,
        result.window.0,
        result.old_parent.0,
        result.new_parent.0,
        result.x,
        result.y,
        result.host_xid,
        result.old_map_state,
        result.new_map_state,
        result.delta.became_viewable.len(),
        result.delta.became_unviewable.len(),
    );
    if let Some(xid) = result.host_xid {
        let new_host_parent = if result.new_parent == ROOT_WINDOW {
            Some(backend.window_id())
        } else {
            state
                .resources
                .window(result.new_parent)
                .and_then(|w| w.host_xid)
                .map(|h| h.as_raw())
        };
        if let Some(host_parent) = new_host_parent {
            let child = xid.as_raw();
            if let Err(e) =
                backend.reparent_subwindow(origin, child, host_parent, result.x, result.y)
            {
                panic!(
                    "reparent_subwindow failed after protocol validation: child=0x{child:x} \
                     parent=0x{host_parent:x}: {e}"
                );
            }
        }
        if result.old_parent == ROOT_WINDOW && result.new_parent != ROOT_WINDOW {
            let _ = backend.update_host_event_mask(
                origin,
                xid.as_raw(),
                crate::host_x11::POINTER_EVENT_MASK,
                false,
            );
            if let Err(err) = backend.register_subwindow(origin, result.window, xid.as_raw()) {
                log::warn!(
                    "client {} register_subwindow for 0x{:x} on reparent failed: {err}",
                    client_id.0,
                    result.window.0
                );
            }
        }
        if result.old_parent != ROOT_WINDOW && result.new_parent == ROOT_WINDOW {
            let _ = backend.update_host_event_mask(
                origin,
                xid.as_raw(),
                crate::host_x11::SUBWINDOW_EVENT_MASK,
                false,
            );
            if let Err(err) = backend.register_top_level(origin, result.window, xid.as_raw()) {
                log::warn!(
                    "client {} register_top_level for 0x{:x} on reparent failed: {err}",
                    client_id.0,
                    result.window.0
                );
            } else {
                backend.on_window_became_top_level(state, xid.as_raw());
            }
        }
        // Step 2 (DRIFT 2): a reparent across the root boundary changes
        // root's child set; reproject the backend top-level order from core.
        if result.old_parent == ROOT_WINDOW || result.new_parent == ROOT_WINDOW {
            backend.sync_top_level_order(state);
        }
    }
    // Before the reconcile below, so a GRANT backing finds the leaf that carries its route.
    realize_storage_for_delta(state, backend, origin, &result.delta);
    let window = result.window;
    let new_parent = result.new_parent;
    let old_parent = result.old_parent;
    let rx = result.x;
    let ry = result.y;
    let override_redirect = result.override_redirect;
    // Xorg compReparentWindow (composite/compwindow.c:453-454): the old
    // parent's subwindows records leave the window, the new parent's join
    // it; its own RedirectWindow records stay. A root child that leaves
    // muffin's RedirectSubwindows(root, Manual) for a frame is no longer
    // redirected, so no stale inner backing keeps its paints from the
    // compositor (nm-applet into mate-panel's tray socket; Warframe
    // fullscreen -> windowed: black).
    let before = state.composite_redirects.window_mode(window);
    state
        .composite_redirects
        .reparent_subwindow(old_parent, new_parent, window);
    log::debug!(
        "reparent reconcile: window=0x{:x} old_parent=0x{:x} new_parent=0x{:x} \
         mode {before:?} -> {:?}",
        window.0,
        old_parent.0,
        new_parent.0,
        state.composite_redirects.window_mode(window),
    );
    if backend.supports_redirect_activation() {
        sync_redirect_backing(state, backend, origin, window, before);
    }
    // After the reconcile, so a revoke still sees the backing it tears down.
    apply_viewability_delta_to_redirects(state, backend, origin, &result.delta);
    release_storage_for_delta(state, backend, origin, &result.delta);
    let _dropped = fanout_event_to_clients(state, &on_window, |buf, seq, order| {
        x11::encode_reparent_notify_event(
            buf,
            seq,
            order,
            window,
            window,
            new_parent,
            rx,
            ry,
            override_redirect,
        );
    });
    let _dropped = fanout_event_to_clients(state, &on_old_parent, |buf, seq, order| {
        x11::encode_reparent_notify_event(
            buf,
            seq,
            order,
            old_parent,
            window,
            new_parent,
            rx,
            ry,
            override_redirect,
        );
    });
    let _dropped = fanout_event_to_clients(state, &on_new_parent, |buf, seq, order| {
        x11::encode_reparent_notify_event(
            buf,
            seq,
            order,
            new_parent,
            window,
            new_parent,
            rx,
            ry,
            override_redirect,
        );
    });
    if let Some((_, _, override_redirect)) = unmapped {
        let _dropped = emit_window_event_to_state(state, window, 0x0002_0000, |buf, seq, order| {
            x11::encode_map_notify_event(buf, seq, order, window, window, override_redirect);
        });
        let _dropped =
            emit_window_event_to_state(state, new_parent, 0x0008_0000, |buf, seq, order| {
                x11::encode_map_notify_event(
                    buf,
                    seq,
                    order,
                    new_parent,
                    window,
                    override_redirect,
                );
            });
        // MapWindow exposes it in the new place, its background painted
        // there: the storage it kept is not what Xorg shows.
        for (w, region) in crate::core_loop::clip_list::subtree_clip_lists(state, window) {
            send_window_exposures(state, backend, origin, w, &region, true);
        }
    }
    // The window moved in the tree; Xorg's ReparentWindow re-evaluates the
    // pointer through its MapWindow (`dix/window.c:2695`).
    backend.windows_restructured(state);
    Ok(RequestOutcome::Handled)
}

pub(super) fn handle_create_window(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    let Some(request) = x11::create_window_request(header.data, body) else {
        return Ok(RequestOutcome::Handled);
    };
    debug!(
        "client {} create window 0x{:x} parent=0x{:x} pos=({},{}) size={}x{} mask=0x{:x}",
        client_id.0,
        request.window.0,
        request.parent.0,
        request.x,
        request.y,
        request.width,
        request.height,
        request.event_mask.unwrap_or(0)
    );
    let new_id = request.window.0;
    let mask = request.event_mask.unwrap_or(0);
    let window_id = request.window;
    let parent = request.parent;
    let geometry = (request.x, request.y, request.width, request.height);
    // Xorg `dix/dispatch.c::ProcCreateWindow` calls dixLookupWindow on
    // the parent first; BadWindow if the parent xid is stale. xts5
    // Xlib4 probes XCreateWindow / XCreateSimpleWindow with
    // parent=badwin() and expects the protocol error.
    if state.resources.window(parent).is_none() {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_WINDOW,
            parent.0,
            1,
        );
    }
    // Xorg `dix/window.c::CreateWindow:769-786` resolves
    // `class == CopyFromParent` against the parent, then enforces:
    //   - class must be InputOutput or InputOnly → BadValue.
    //   - parent class InputOnly + child class != InputOnly → BadMatch.
    //   - class InputOnly + (border_width != 0 || depth != 0) → BadMatch.
    // xts5 Xlib4: XCreateSimpleWindow-10 (parent InputOnly path),
    // XCreateWindow-31 (InputOnly+border_width), XCreateWindow-43
    // (parent InputOnly + InputOutput child), XCreateWindow-44
    // (InputOnly + non-zero depth).
    let parent_class = state
        .resources
        .window(parent)
        .map(|w| w.class)
        .expect("parent existence verified above");
    let parent_class_value = parent_class.protocol_value();
    let effective_class = match request.class {
        0 => parent_class_value, // CopyFromParent → parent's class
        v => v,
    };
    if effective_class != 1 && effective_class != 2 {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_VALUE,
            u32::from(request.class),
            1,
        );
    }
    if matches!(parent_class, crate::resources::WindowClass::InputOnly) && effective_class != 2 {
        return emit_x11_error(state, client_id, sequence, x11::error::BAD_MATCH, 0, 1);
    }
    if effective_class == 2 && (request.border_width != 0 || request.depth != 0) {
        return emit_x11_error(state, client_id, sequence, x11::error::BAD_MATCH, 0, 1);
    }
    // InputOnly value-mask check, same legal-mask as
    // ChangeWindowAttributes — xts5 Xlib4 XCreateWindow-24 cycles
    // every illegal bit through value_mask and expects BadMatch.
    if effective_class == 2 {
        const INPUTONLY_LEGAL_MASK: u32 = 0x0020   // CWWinGravity
            | 0x0200                                // CWOverrideRedirect
            | 0x0800                                // CWEventMask
            | 0x1000                                // CWDontPropagate
            | 0x4000; // CWCursor
        if (request.value_mask & !INPUTONLY_LEGAL_MASK) != 0 {
            return emit_x11_error(state, client_id, sequence, x11::error::BAD_MATCH, 0, 1);
        }
    }
    // BadIdChoice / BadMatch validation. The server creates its own
    // windows (the screensaver's) under `SERVER_OWNER`, with no client.
    let validation_failed = client_id != crate::resources::SERVER_OWNER && {
        let handle = state.clients.get(&client_id.0).expect("client registered");
        let owned = crate::server::IdAllocator::validate_owned(
            new_id,
            handle.resource_id_base,
            handle.resource_id_mask,
        );
        let in_use = state.xid_occupied(request.window.0);
        !owned || in_use
    };
    if validation_failed {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_ID_CHOICE,
            new_id,
            1,
        );
    }
    let visual_known = request.visual.0 == 0 || state.resources.is_known_visual(request.visual);
    if !visual_known {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_MATCH,
            request.visual.0,
            1,
        );
    }
    // Attribute value-id validation, matching Xorg's
    // `dix/window.c::CreateWindow`. CWBackPixmap values 0/1 are
    // reserved (None / ParentRelative); CWColormap value 0 in the
    // typed enum is CopyFromParent.
    if let Some(bg_pixmap) = request.background_pixmap
        && bg_pixmap.0 > 1
        && state.resources.pixmap(bg_pixmap).is_none()
    {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_PIXMAP,
            bg_pixmap.0,
            1,
        );
    }
    if let Some(Some(colormap)) = request.colormap
        && state.resources.colormap(colormap).is_none()
    {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_COLORMAP,
            colormap.0,
            1,
        );
    }
    if let Some(cursor) = request.cursor
        && cursor.0 != 0
        && !state.resources.cursor_exists(cursor)
    {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_CURSOR,
            cursor.0,
            1,
        );
    }
    // Border validation. The effective depth resolves CopyFromParent
    // (depth 0) against the parent, matching
    // `ResourceTable::create_window`.
    let parent_depth = state
        .resources
        .window(parent)
        .map(|w| w.depth)
        .expect("parent existence verified above");
    let effective_depth = if request.depth == 0 {
        parent_depth
    } else {
        request.depth
    };
    // Xorg `dix/window.c:818`: a window whose depth differs from its
    // parent's MUST supply a border attribute, because the inherited
    // border (`:879`) would otherwise carry the parent's depth. InputOnly
    // has no border at all and is exempt.
    const CW_BORDER_PIXMAP: u32 = 0x0004;
    const CW_BORDER_PIXEL: u32 = 0x0008;
    if (request.value_mask & (CW_BORDER_PIXMAP | CW_BORDER_PIXEL)) == 0
        && effective_class != 2
        && effective_depth != parent_depth
    {
        return emit_x11_error(state, client_id, sequence, x11::error::BAD_MATCH, 0, 1);
    }
    // `dix/window.c:1251` CWBorderPixmap. CWBorderPixel overrides it and
    // clears the bit before the pixmap is ever resolved (`:1298`), so a
    // request carrying both is not validated against the pixmap.
    if request.border_pixel.is_none()
        && let Some(border_pixmap) = request.border_pixmap
    {
        if border_pixmap.0 == 0 {
            // CopyFromParent: BadMatch unless the depths match. (The
            // no-parent case cannot arise here — the parent xid was
            // validated above.)
            if effective_depth != parent_depth {
                return emit_x11_error(state, client_id, sequence, x11::error::BAD_MATCH, 0, 1);
            }
        } else {
            match state.resources.pixmap(border_pixmap) {
                None => {
                    return emit_x11_error(
                        state,
                        client_id,
                        sequence,
                        x11::error::BAD_PIXMAP,
                        border_pixmap.0,
                        1,
                    );
                }
                Some(pixmap) if pixmap.depth != effective_depth => {
                    return emit_x11_error(state, client_id, sequence, x11::error::BAD_MATCH, 0, 1);
                }
                Some(_) => {}
            }
        }
    }
    state.resources.create_window(client_id, request);
    if mask != 0
        && let Some(client) = state.clients.get_mut(&client_id.0)
    {
        client.event_masks.insert(window_id, mask);
    }
    let needs_host_xid = state
        .resources
        .window(window_id)
        .is_some_and(|w| w.class != crate::resources::WindowClass::InputOnly);
    if needs_host_xid {
        let host_visual = resolve_host_subwindow_visual_to_state(state, window_id);
        let host_parent: Option<crate::backend::WindowHandle> = if parent == ROOT_WINDOW {
            crate::backend::WindowHandle::from_raw(backend.window_id())
        } else {
            state.resources.window(parent).and_then(|w| w.host_xid)
        };
        let local = state.resources.window(window_id);
        let resolved_bg = state.resources.window_resolved_background(window_id);
        // `window_resolved_background` returning None is MEANINGFUL: it
        // says this window has no background, so the server paints
        // nothing (X11 §CreateWindow — background-pixmap defaults to
        // None). It returns None in exactly four cases, and a stored
        // pixel is the wrong answer in all of them: a ParentRelative
        // cycle; the window absent from the map (then `local` is None
        // too); a ParentRelative chain that resolved to no background;
        // and `background_none` itself.
        //
        // So DO NOT fall back to `w.background_pixel` here. For a
        // background-None window that field holds the 0x00ffffff
        // PLACEHOLDER `create_window` stores when the request carries no
        // background attribute, and passing it on painted every such
        // window's fresh storage WHITE — the storage half of
        // `window_storage_init_covers_the_whole_allocation`, and the
        // reason `default_window_init_color`'s None branch was dead for
        // client windows. None reaches that safe default instead.
        let host_bg_pixel = resolved_bg.map(|bg| bg.background_pixel);
        let host_bg_pixmap = resolved_bg
            .and_then(|bg| bg.background_pixmap_host_xid)
            .map(|h| h.as_raw())
            .or_else(|| {
                local
                    .and_then(|w| w.background_pixmap_host_xid)
                    .map(|h| h.as_raw())
            });
        let allocated = host_parent.and_then(|host_parent| {
            match backend.create_subwindow(
                origin,
                host_parent,
                geometry.0,
                geometry.1,
                geometry.2,
                geometry.3,
                request.border_width,
                host_visual,
                host_bg_pixel,
                host_bg_pixmap,
            ) {
                Ok(handle) => Some(handle),
                Err(err) => {
                    log::warn!(
                        "client {} create_subwindow for 0x{:x} failed: {err}",
                        client_id.0,
                        new_id
                    );
                    None
                }
            }
        });
        if let Some(host_handle) = allocated {
            if let Some(w) = state.resources.window_mut(window_id) {
                w.host_xid = Some(host_handle);
            }
            let host_xid = host_handle.as_raw();
            // #133 step 2 (P3): `create_subwindow` carries `border_width`
            // on the wire but not the border SOURCE, so forward it now
            // through the generic attribute route. Without this a window
            // created with a border pixel that is never changed again
            // would leave the backend mirror with no source at all —
            // and CreateWindow inherits the parent's border
            // (`dix/window.c:879`), so even a client that supplies no
            // border attribute can start out with a non-default one.
            if let Some(border) = state.resources.window(window_id).map(|w| w.border) {
                let (value_mask, values) = border_source_cwa_values(border);
                let _ = backend.change_subwindow_attributes(origin, host_xid, value_mask, &values);
            }
            // CWCursor on CreateWindow, as ChangeWindowAttributes forwards it.
            if let Some(cursor_host) = request
                .cursor
                .filter(|c| c.0 != 0)
                .and_then(|c| state.resources.cursor_host_xid(c))
            {
                let _ = backend.define_cursor(origin, host_xid, cursor_host);
            }
            let result = if parent == ROOT_WINDOW {
                backend.register_top_level(origin, window_id, host_xid)
            } else {
                backend.register_subwindow(origin, window_id, host_xid)
            };
            if let Err(err) = result {
                log::warn!(
                    "client {} register host window for 0x{:x} failed: {err}",
                    client_id.0,
                    new_id
                );
            } else if parent == ROOT_WINDOW {
                backend.on_window_became_top_level(state, host_xid);
                // Step 2 (DRIFT 2): a new root child changes the top-level
                // set; reproject the backend order from core so the new
                // window lands where core put it (e.g. below the COW).
                backend.sync_top_level_order(state);
            }
        }
    }
    // Xorg compCreateWindow (composite/compwindow.c:579-589): a child of a
    // RedirectSubwindows parent gets that client's record. It is unmapped,
    // so the backing waits for realize.
    state
        .composite_redirects
        .redirect_new_subwindow(parent, window_id);
    // CreateNotify on parent (SubstructureNotify subscribers).
    let create_notify_targets = subscribers_by_id(state, parent, 0x0008_0000);
    if !create_notify_targets.is_empty() {
        let geometry_opt = state.resources.window(window_id).map(window_geometry);
        if let Some(geometry) = geometry_opt {
            let override_redir = request.override_redirect.unwrap_or(false);
            let _dropped =
                fanout_event_to_clients(state, &create_notify_targets, |buf, seq, order| {
                    x11::encode_create_notify_event(
                        buf,
                        seq,
                        order,
                        parent,
                        window_id,
                        geometry,
                        override_redir,
                    );
                });
        }
    }
    debug!("client {} #{} CreateWindow", client_id.0, sequence.0);
    Ok(RequestOutcome::Handled)
}

pub(super) fn handle_change_window_attributes(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    let Some(request) = x11::change_window_attributes_request(body) else {
        return Ok(RequestOutcome::Handled);
    };
    if state.resources.window(request.window).is_none() {
        // Xorg `dix/dispatch.c::ProcChangeWindowAttributes` returns
        // BadWindow via dixLookupWindow before applying any attribute
        // changes. xts5 Xlib4 probes XChangeWindowAttributes and its
        // wrappers (XSetWindowBackground / *BorderPixmap / *Colormap /
        // XDefineCursor / XUndefineCursor / …) on badwin() and expects
        // the protocol error.
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_WINDOW,
            request.window.0,
            2,
        );
    }
    // Xorg `dix/window.c::ChangeWindowAttributes:1174`: an InputOnly
    // window may only carry the `INPUTONLY_LEGAL_MASK` bits
    // (CWWinGravity, CWEventMask, CWDontPropagate, CWOverrideRedirect,
    // CWCursor); any other bit → BadMatch. xts5 Xlib4
    // XChangeWindowAttributes-26 cycles every illegal bit through the
    // value_mask and expects BadMatch from each.
    const INPUTONLY_LEGAL_MASK: u32 = 0x0020   // CWWinGravity
        | 0x0200                                // CWOverrideRedirect
        | 0x0800                                // CWEventMask
        | 0x1000                                // CWDontPropagate
        | 0x4000; // CWCursor
    let target_class = state
        .resources
        .window(request.window)
        .map(|w| w.class)
        .expect("window existence verified above");
    if matches!(target_class, crate::resources::WindowClass::InputOnly)
        && (request.value_mask & !INPUTONLY_LEGAL_MASK) != 0
    {
        return emit_x11_error(state, client_id, sequence, x11::error::BAD_MATCH, 0, 2);
    }
    // Resource-id validation of attribute values, mirroring Xorg's
    // `dix/window.c::ChangeWindowAttributes` per-attribute
    // `dixLookupResource`. Values 0 and 1 on CWBackPixmap are reserved
    // (None / ParentRelative); colormap value 0 in the typed enum
    // means CopyFromParent and is resolved against the parent.
    if let Some(bg_pixmap) = request.background_pixmap
        && bg_pixmap.0 > 1
        && state.resources.pixmap(bg_pixmap).is_none()
    {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_PIXMAP,
            bg_pixmap.0,
            2,
        );
    }
    if let Some(Some(colormap)) = request.colormap
        && state.resources.colormap(colormap).is_none()
    {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_COLORMAP,
            colormap.0,
            2,
        );
    }
    if let Some(cursor) = request.cursor
        && cursor.0 != 0
        && !state.resources.cursor_exists(cursor)
    {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_CURSOR,
            cursor.0,
            2,
        );
    }
    // `dix/window.c:1251` CWBorderPixmap. CWBorderPixel overrides it and
    // clears the bit before the pixmap is resolved (`:1298`), so a
    // request carrying both is never validated against the pixmap.
    if request.border_pixel.is_none()
        && let Some(border_pixmap) = request.border_pixmap
    {
        let (own_depth, parent_depth) = {
            let window = state
                .resources
                .window(request.window)
                .expect("window existence verified above");
            let parent_depth = if request.window == ROOT_WINDOW {
                None
            } else {
                state.resources.window(window.parent).map(|p| p.depth)
            };
            (window.depth, parent_depth)
        };
        if border_pixmap.0 == 0 {
            // CopyFromParent: BadMatch with no parent, or when the
            // depths differ — the parent's border would carry the wrong
            // depth for this window.
            if parent_depth != Some(own_depth) {
                return emit_x11_error(state, client_id, sequence, x11::error::BAD_MATCH, 0, 2);
            }
        } else {
            match state.resources.pixmap(border_pixmap) {
                None => {
                    return emit_x11_error(
                        state,
                        client_id,
                        sequence,
                        x11::error::BAD_PIXMAP,
                        border_pixmap.0,
                        2,
                    );
                }
                Some(pixmap) if pixmap.depth != own_depth => {
                    return emit_x11_error(state, client_id, sequence, x11::error::BAD_MATCH, 0, 2);
                }
                Some(_) => {}
            }
        }
    }
    if let Some(bg_pixmap) = request.background_pixmap {
        debug!(
            "client {} CWA bg_pixmap: window 0x{:x} ← 0x{:x} ({})",
            client_id.0,
            request.window.0,
            bg_pixmap.0,
            match bg_pixmap.0 {
                0 => "None",
                1 => "ParentRelative",
                _ => "pixmap",
            },
        );
    }
    if let Some(event_mask) = request.event_mask {
        debug!(
            "client {} attrs window 0x{:x} mask=0x{:x}",
            client_id.0, request.window.0, event_mask
        );
        let entry = state
            .clients
            .get_mut(&client_id.0)
            .expect("client registered");
        if event_mask == 0 {
            entry.event_masks.remove(&request.window);
        } else {
            entry.event_masks.insert(request.window, event_mask);
        }
    }
    let target_window = request.window;
    let cursor_id = request.cursor;
    let released = state.resources.change_window_attributes(request);
    // Release a replaced bg / border pixmap on the host ONLY if it is
    // fully orphaned: no other window background OR border references it
    // AND the client no longer owns it (FreePixmap already happened — at
    // which point handle_free_pixmap skipped the host free because the
    // attribute reference was still live). Freeing while the client still
    // owns the pixmap destroyed it server-side: e16 menu items blanked on
    // hover because the bg swap to the hilite pixmap host-freed the kept
    // "normal" pixmap the un-hover restore then pointed back at.
    //
    // Both reference checks gate BOTH releases: one host handle can be a
    // background on one window and a border on another (nothing stops a
    // client naming the same pixmap for both), so testing only the
    // matching attribute would free storage the other kind still samples.
    //
    // DEDUPLICATED: one request can replace both attributes at once, and if
    // both named the same tile then `background` and `border` hand back the
    // SAME handle. Freeing it twice happens to be inert on KMS (the store
    // entry is gone by the second call) but it is still a broken backend
    // contract, and a recording or host-X11 backend sees the duplicate.
    let mut released_hosts = [released.background, released.border]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    released_hosts.dedup_by_key(|h| h.as_raw());
    for old_host_xid in released_hosts {
        if !state.resources.host_xid_still_referenced(old_host_xid) {
            let _ = backend.free_pixmap(origin, old_host_xid.as_raw());
            state.resources.host_pixmap_freed(old_host_xid.as_raw());
        }
    }

    if target_window == ROOT_WINDOW {
        let root_bg = state.resources.window_resolved_background(ROOT_WINDOW);
        let root_bg_host_xid = root_bg.and_then(|bg| bg.background_pixmap_host_xid);
        debug!(
            "client {} CWA(root) bg_pixmap={:?} bg_pixel={:?} root_bg_host_xid={:?}",
            client_id.0, request.background_pixmap, request.background_pixel, root_bg_host_xid,
        );
        if request.background_pixmap.is_some() {
            if let Some(host_bg) = root_bg_host_xid {
                let _ = backend.set_container_background_pixmap(origin, host_bg.as_raw());
            } else if let Some(bg_pixel) = root_bg.map(|bg| bg.background_pixel) {
                let _ = backend.set_container_background_pixel(origin, bg_pixel);
            }
        } else if let Some(pixel) = request.background_pixel {
            let _ = backend.set_container_background_pixel(origin, pixel);
        }
    }

    // Only a CWA that (re)selects input may promote focus — i.e. a
    // window "advertising input later" by adding key events to its event
    // mask. A CWA changing an unrelated attribute (cursor, background,
    // ...) must never move the input focus; in X11 focus changes only
    // via SetInputFocus / grabs. Without this gate, muffin setting the
    // "fleur" move cursor on its guard window during a
    // `_NET_WM_MOVERESIZE` drag stole focus to that window, which then
    // turned the dragged app's own child-focus move into a
    // `FocusOut(Nonlinear)` (GTK4 backdrop) and broke the drag.

    if target_window != ROOT_WINDOW
        && (request.background_pixel.is_some() || request.background_pixmap.is_some())
    {
        let host_xid = state
            .resources
            .window(target_window)
            .and_then(|w| w.host_xid);
        let resolved_bg = state.resources.window_resolved_background(target_window);
        if let Some(host_xid) = host_xid {
            let mut value_mask: u32 = 0;
            let mut values: Vec<u32> = Vec::with_capacity(1);
            if request.background_pixmap.is_some() {
                if let Some(bg_pixmap_host_xid) =
                    resolved_bg.and_then(|bg| bg.background_pixmap_host_xid)
                {
                    value_mask |= 1 << 0;
                    values.push(bg_pixmap_host_xid.as_raw());
                } else {
                    value_mask |= 1 << 1;
                    values.push(resolved_bg.map(|bg| bg.background_pixel).unwrap_or(0));
                }
            } else if request.background_pixel.is_some() {
                value_mask |= 1 << 1;
                values.push(resolved_bg.map(|bg| bg.background_pixel).unwrap_or(0));
            }
            let _ =
                backend.change_subwindow_attributes(origin, host_xid.as_raw(), value_mask, &values);
        }
    }

    // #133 step 2 (P3): the border source needs its OWN forward. The
    // background block above fires only on a background change, so
    // before this a `CWBorderPixel` / `CWBorderPixmap` reached the
    // backend by no route at all — awesome's focus recolour (its only
    // border request shape) was dropped on the floor. Read the window's
    // CURRENT `border` rather than the request fields: the resource
    // layer has already applied CopyFromParent resolution and
    // pixel-overrides-pixmap, so this sends the state that actually
    // took effect. Unhosted / InputOnly windows have no host xid and
    // are silently skipped, same as the background path.
    if target_window != ROOT_WINDOW
        && (request.border_pixel.is_some() || request.border_pixmap.is_some())
    {
        let host_xid = state
            .resources
            .window(target_window)
            .and_then(|w| w.host_xid);
        let border = state.resources.window(target_window).map(|w| w.border);
        if let (Some(host_xid), Some(border)) = (host_xid, border) {
            let (value_mask, values) = border_source_cwa_values(border);
            let _ =
                backend.change_subwindow_attributes(origin, host_xid.as_raw(), value_mask, &values);
        }
        // #143 — report protocol DAMAGE for the ring the forward above
        // just repainted. Xorg does exactly this, in
        // `ChangeWindowAttributes` itself and on the same condition
        // (`(vmaskCopy & (CWBorderPixel | CWBorderPixmap)) && pWin->viewable
        // && HasBorder(pWin)`, `dix/window.c:1581-1589`): it subtracts
        // `winSize` from `borderClip` and `PaintWindow(..., PW_BORDER)`s the
        // difference, which lands as a `PolyFillRect` on the window's
        // (composite backing) pixmap and so goes through `damagePolyFillRect`
        // (`miext/damage/damage.c:1194`). Xorg does NOT compare old-vs-new
        // border source, and neither does our backend forward
        // (`backend.rs:20110` repaints on any `value_mask & 0x0c`), so the
        // damage has to fire on exactly the same trigger or the reported
        // region and the painted region drift apart.
        //
        // Without this, awesome's focus recolour repainted our backing
        // correctly but told no compositor: picom, on `EXT_buffer_age`
        // partial repaint, kept one ring colour per back buffer and
        // alternated between them at frame rate (#143's border flicker).
        //
        // No-op for unbordered windows, for the root and for a
        // non-viewable window.
        let _dropped = accumulate_damage_border_to_state(state, target_window);
    }

    if let Some(cid) = cursor_id {
        let host_window_raw = if target_window == ROOT_WINDOW {
            Some(backend.window_id())
        } else {
            state
                .resources
                .window(target_window)
                .and_then(|w| w.host_xid)
                .map(|w| w.as_raw())
        };
        // X11 `cursor = None` (xid 0) means "clear the per-window
        // cursor so the effective cursor inherits from the parent
        // chain." Match Xorg `dix/window.c:1487-1491`, which sets
        // `pCursor = (CursorPtr) None` in that case. Pre-fix, we
        // silently dropped CWA cursor=0 because `cursor_host_xid(0)`
        // returns None and the (Some, Some) match below failed —
        // marco's resize-frame XDefineCursor(frame, None) reset never
        // propagated to the backend, so the resize cursor sprite
        // stayed visible after the pointer moved off the edge into
        // the frame interior.
        let cursor_host_xid = if cid.0 == 0 {
            Some(0u32)
        } else {
            state.resources.cursor_host_xid(cid)
        };
        if let (Some(hw), Some(ch)) = (host_window_raw, cursor_host_xid) {
            let _ = backend.define_cursor(origin, hw, ch);
        } else if host_window_raw.is_none() {
            // No backend window (InputOnly): the backend reads the cursor
            // from the tree when it re-resolves the window under the
            // pointer.
            backend.windows_restructured(state);
        }
        release_dropped_cursors(state, backend, origin);
    }
    debug!(
        "client {} #{} ChangeWindowAttributes",
        client_id.0, sequence.0
    );
    Ok(RequestOutcome::Handled)
}

/// #133 step 2 (P3): flatten a window's border source into the
/// `(value_mask, values)` pair `Backend::change_subwindow_attributes`
/// takes. The mask uses real X11 CW bit numbering — `0x04`
/// CWBorderPixmap, `0x08` CWBorderPixel — because the host-X11 backend
/// forwards mask and values verbatim to a real X server
/// (`host_x11/request.rs:1046`), so the correct bits make the nested
/// path work with no translation.
///
/// A border is an either/or in Xorg (`PixUnion border` +
/// `borderIsPixel`, `include/windowstr.h:146`), so exactly one bit is
/// ever set. `Window::border` already has CopyFromParent resolved
/// eagerly (`dix/window.c:1251`), so this is a plain read with no
/// ParentRelative-style walk — unlike a background, a border has no
/// inherit-at-paint-time sentinel.
///
/// A tile pixmap with no host storage degrades to the pixel bit with
/// value 0, mirroring the background block's "no host pixmap → send a
/// pixel" shape: a backend that cannot sample the tile is better off
/// with a defined solid colour than with a dangling xid.
fn border_source_cwa_values(border: BorderSource) -> (u32, Vec<u32>) {
    match border {
        BorderSource::Pixmap {
            host_xid: Some(host),
            ..
        } => (0x04, vec![host.as_raw()]),
        BorderSource::Pixmap { host_xid: None, .. } => (0x08, vec![0]),
        BorderSource::Pixel(pixel) => (0x08, vec![pixel]),
    }
}

fn reset_damage_notify_cycle_for_drawable(state: &mut ServerState, drawable: ResourceId) {
    let mut drawables = vec![drawable];
    if let Some(window) = state.resources.window(drawable) {
        drawables.extend(
            window
                .composite_named_pixmaps
                .iter()
                .map(|p| p.client_pixmap),
        );
    }
    for damage in state.damage_objects.values_mut() {
        if drawables.contains(&damage.drawable) {
            damage.pending_notify_fired = false;
        }
    }
}

pub(super) fn handle_configure_window(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    let Some(request) = x11::configure_window_request(body) else {
        return Ok(RequestOutcome::Handled);
    };
    log::trace!(
        target: "yserver::input::restack",
        "CONFIGURE-REQ client={} win={} stack_mode={:?} sibling={}",
        state.debug_client_label(client_id),
        state.debug_window_label(request.window),
        request.stack_mode,
        request
            .sibling
            .map_or_else(|| "None".to_string(), |s| state.debug_window_label(s)),
    );
    // X11 spec / Xorg `dix/window.c::ConfigureWindow`: ConfigureWindow
    // on the root window is accepted (no error) but has no visible
    // effect — root geometry is owned by the screen/RandR, not by the
    // client. Without this short-circuit we apply the client's
    // width/height to `state.resources.window(ROOT_WINDOW)` and every
    // subsequent on-screen check (e.g. GetImage's BadMatch validation)
    // breaks because root reports the client's tiny dimensions instead
    // of the KMS scanout dims. xts5 produced a 70x61 root resize that
    // cascaded into ~1000 UNRES.
    if request.window == ROOT_WINDOW {
        debug!(
            "client {} #{} ConfigureWindow on root — dropped (matches Xorg semantics)",
            client_id.0, sequence.0
        );
        return Ok(RequestOutcome::Handled);
    }
    if state.resources.window(request.window).is_none() {
        // Xorg `dix/dispatch.c::ProcConfigureWindow` returns BadWindow
        // via dixLookupWindow before applying any geometry change.
        // xts5 Xlib4 probes XConfigureWindow and its Xlib wrappers
        // (XMoveWindow / XResizeWindow / XMoveResizeWindow /
        // XRaiseWindow / XLowerWindow / XMapRaised — Xlib9 XRestack /
        // XSetWindowBorderWidth) on badwin() and expects the protocol
        // error.
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_WINDOW,
            request.window.0,
            12,
        );
    }
    // Xorg `dix/window.c::ConfigureWindow` checks (in order, before
    // any geometry change or SubstructureRedirect dispatch):
    //   1. InputOnly + CWBorderWidth → BadMatch (xts5 Xlib4
    //      XConfigureWindow-33 / XSetWindowBorderWidth-6).
    //   2. CWSibling without CWStackMode → BadMatch (XConfigureWindow-30).
    //   3. CWStackMode value > 4 → BadValue.
    //   4. CWSibling: sibling xid unknown → BadWindow; sibling not
    //      actually a sibling of `window` or sibling==window → BadMatch
    //      (XConfigureWindow-31).
    //   5. CWWidth=0 or CWHeight=0 → BadValue (XConfigureWindow-32).
    const CW_BORDER_WIDTH: u16 = 0x0010;
    const CW_SIBLING: u16 = 0x0020;
    const CW_STACK_MODE: u16 = 0x0040;
    let window_class = state
        .resources
        .window(request.window)
        .map(|w| w.class)
        .expect("window existence verified above");
    let window_parent = state
        .resources
        .window(request.window)
        .map(|w| w.parent)
        .expect("window existence verified above");
    if matches!(window_class, crate::resources::WindowClass::InputOnly)
        && (request.value_mask & CW_BORDER_WIDTH) != 0
    {
        return emit_x11_error(state, client_id, sequence, x11::error::BAD_MATCH, 0, 12);
    }
    if (request.value_mask & CW_SIBLING) != 0 && (request.value_mask & CW_STACK_MODE) == 0 {
        return emit_x11_error(state, client_id, sequence, x11::error::BAD_MATCH, 0, 12);
    }
    if let Some(mode) = request.stack_mode
        && mode > 4
    {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_VALUE,
            u32::from(mode),
            12,
        );
    }
    if let Some(sibling) = request.sibling {
        let sibling_window = state.resources.window(sibling);
        if sibling_window.is_none() {
            return emit_x11_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_WINDOW,
                sibling.0,
                12,
            );
        }
        let sibling_parent = sibling_window.map(|w| w.parent);
        if sibling_parent != Some(window_parent) || sibling == request.window {
            return emit_x11_error(state, client_id, sequence, x11::error::BAD_MATCH, 0, 12);
        }
    }
    if request.width == Some(0) || request.height == Some(0) {
        return emit_x11_error(state, client_id, sequence, x11::error::BAD_VALUE, 0, 12);
    }
    let pre = state
        .resources
        .window(request.window)
        .map(|w| (w.parent, w.override_redirect));
    let redirect_targets: Vec<ClientId> = if let Some((parent, false)) = pre {
        let requester_has = state
            .clients
            .get(&client_id.0)
            .and_then(|c| c.event_masks.get(&parent).copied())
            .is_some_and(|m| m & 0x0010_0000 != 0);
        if requester_has {
            Vec::new()
        } else {
            subscribers_by_id(state, parent, 0x0010_0000)
        }
    } else {
        Vec::new()
    };
    if !redirect_targets.is_empty() {
        let parent = pre.map(|(p, _)| p).unwrap_or(ROOT_WINDOW);
        let _dropped = fanout_event_to_clients(state, &redirect_targets, |buf, seq, order| {
            x11::encode_configure_request_event(buf, seq, order, parent, request.window, &request);
        });
        return Ok(RequestOutcome::Handled);
    }
    let old_size = state
        .resources
        .window(request.window)
        .map(|w| (w.width, w.height));
    // X11 dix/window.c::ConfigureWindow: a ConfigureNotify is emitted
    // only when the window is *actually* reconfigured. Snapshot the
    // fields the event reports (geometry + above-sibling) before the
    // change so we can suppress the notify for a no-op (e.g. a
    // stacking-only request that doesn't reorder, or a same-value
    // geometry). Emitting on a no-op restack makes Enlightenment
    // re-restack forever (~8000 req/s).
    let before_geom = state
        .resources
        .window(request.window)
        .map(|w| (w.x, w.y, w.width, w.height, w.border_width));
    let before_above = state
        .resources
        .configure_notify_above_sibling(request.window);
    let sibling_host_xid = request
        .sibling
        .and_then(|sibling| state.resources.window(sibling))
        .and_then(|w| w.host_xid)
        .map(|h| h.as_raw());
    // The clip lists the change can alter, for its exposures below.
    let tree_change = before_geom.and_then(|(x, y, w, h, bw)| {
        let bw2 = request.border_width.unwrap_or(bw).saturating_mul(2);
        let reach = x11::xfixes::RegionRect {
            x: request.x.unwrap_or(x),
            y: request.y.unwrap_or(y),
            width: request.width.unwrap_or(w).saturating_add(bw2),
            height: request.height.unwrap_or(h).saturating_add(bw2),
        };
        crate::core_loop::clip_list::TreeChange::begin(state, request.window, Some(reach))
    });
    let configure = state
        .resources
        .configure_window(request)
        .map(|w| (w.id, window_geometry(w), w.override_redirect));
    let host_xid = configure
        .as_ref()
        .and_then(|(id, _, _)| state.resources.window(*id).and_then(|w| w.host_xid));
    let parent = configure
        .as_ref()
        .and_then(|(id, _, _)| state.resources.window(*id).map(|w| w.parent));
    debug!(
        "client {} #{} ConfigureWindow 0x{:x} mask=0x{:x} x={:?} y={:?} w={:?} h={:?} host_xid={:?}",
        client_id.0,
        sequence.0,
        request.window.0,
        request.value_mask,
        request.x,
        request.y,
        request.width,
        request.height,
        host_xid,
    );
    if let Some(xid) = host_xid {
        let _ = backend.configure_subwindow(
            origin,
            xid.as_raw(),
            crate::host_x11::HostSubwindowConfig {
                x: request.x,
                y: request.y,
                width: request.width,
                height: request.height,
                border_width: request.border_width,
                sibling: sibling_host_xid,
                stack_mode: request.stack_mode,
            },
        );
    }
    // Step 2 (DRIFT 2): a stack_mode restack changed root's child order in
    // the core tree (with full occlusion + COW cap). Reproject the
    // backend's top-level z-order from core so the two cannot drift.
    if request.stack_mode.is_some() {
        backend.sync_top_level_order(state);
    }
    if let Some((window_id, geometry, override_redirect)) = configure {
        let above_sibling = state.resources.configure_notify_above_sibling(window_id);
        // Only emit ConfigureNotify when the window was actually
        // reconfigured (geometry or stacking changed), per Xorg
        // dix/window.c::ConfigureWindow. A stacking-only request that
        // doesn't reorder, or a configure to identical geometry, is a
        // no-op and must stay silent — otherwise a WM that re-applies its
        // stacking policy on every ConfigureNotify (Enlightenment) spins
        // forever.
        let after_geom = state
            .resources
            .window(window_id)
            .map(|w| (w.x, w.y, w.width, w.height, w.border_width));
        let configure_changed = before_geom != after_geom || before_above != above_sibling;
        if configure_changed {
            // Xorg calls the screen's ConfigNotify hook before it delivers
            // the core ConfigureNotify (dix/window.c::ConfigureWindow).
            // Present wraps that hook, so a client selecting both events
            // observes Present::ConfigureNotify first. Keep this wire order:
            // Mesa/CEF consume both streams for the same DRI3 window, and
            // reversing them briefly left Steam's popup surface with its
            // parent-relative coordinates while the native root child had
            // already moved.
            fire_present_configure_notify_for_window(state, window_id, geometry);
            let _dropped =
                emit_window_event_to_state(state, window_id, 0x0002_0000, |buf, seq, order| {
                    x11::encode_configure_notify_event(
                        buf,
                        seq,
                        order,
                        window_id,
                        window_id,
                        above_sibling,
                        geometry,
                        override_redirect,
                    );
                });
            if let Some(parent) = parent {
                let _dropped =
                    emit_window_event_to_state(state, parent, 0x0008_0000, |buf, seq, order| {
                        x11::encode_configure_notify_event(
                            buf,
                            seq,
                            order,
                            parent,
                            window_id,
                            above_sibling,
                            geometry,
                            override_redirect,
                        );
                    });
            }
        }
        // X11 window gravity: resizing a window repositions its children
        // per each child's `win_gravity` (Xorg dix ResizeChildrenWinSize).
        // Reposition in the host mirror and send GravityNotify. fvwm's
        // window-shade relies on this — it parks a South-gravity client
        // above the fold and reveals it by growing the client's holder;
        // without gravity the client stays parked and renders as a black
        // rectangle over the frame (air/silence HW 2026-07-02).
        if let Some((ow, oh)) = old_size {
            let moved = state.resources.apply_win_gravity(
                window_id,
                ow,
                oh,
                geometry.width,
                geometry.height,
            );
            for (child, nx, ny) in moved {
                if let Some(h) = state.resources.window(child).and_then(|w| w.host_xid) {
                    let _ = backend.configure_subwindow(
                        origin,
                        h.as_raw(),
                        crate::host_x11::HostSubwindowConfig {
                            x: Some(nx),
                            y: Some(ny),
                            width: None,
                            height: None,
                            border_width: None,
                            sibling: None,
                            stack_mode: None,
                        },
                    );
                }
                // GravityNotify → child (StructureNotify) and its parent
                // (SubstructureNotify), mirroring ConfigureNotify delivery.
                let _dropped =
                    emit_window_event_to_state(state, child, 0x0002_0000, |buf, seq, order| {
                        x11::encode_gravity_notify_event(buf, seq, order, child, child, nx, ny);
                    });
                let _dropped =
                    emit_window_event_to_state(state, window_id, 0x0008_0000, |buf, seq, order| {
                        x11::encode_gravity_notify_event(buf, seq, order, window_id, child, nx, ny);
                    });
            }
        }
        let resized =
            old_size.is_some_and(|(ow, oh)| geometry.width != ow || geometry.height != oh);
        let old_border_width = before_geom.map_or(geometry.border_width, |g| g.4);
        if resized || old_border_width != geometry.border_width {
            rotate_redirected_backing_on_resize(
                state,
                backend,
                origin,
                window_id,
                geometry.width,
                geometry.height,
                false,
                old_border_width,
            );
        }
        // NOTE (Issue 2 — 2026-07-01): a pure move MUST NOT rotate the
        // redirected backing. picom glx holds a one-time `NameWindowPixmap`
        // alias to the original backing (its GLX texture never re-binds
        // on move), so any swap breaks the alias: the live client content
        // starts landing in a new backing picom never samples, and the
        // frame visually freezes ("btop stops animating when dragged").
        // The WIP `else if moved { rotate(..., true) }` that briefly lived
        // here in commit bb3cc70d was this regression — reverted.
        // X11 Composite + DAMAGE interaction: configuring a redirected
        // window (move / resize / border / stack-order) changes its
        // screen-space presentation. The pixel content of the
        // redirected backing doesn't change, but the compositor MUST
        // recomposite the moved-from + moved-to regions. Xorg
        // signals this by emitting a `DamageNotify` on the window
        // for the full window-local extent at the new geometry
        // (xserver/composite/compwindow.c). Measured divergence on a
        // mate-with-compositing drag: Xephyr emitted 776 DamageNotify
        // events to marco over the run; yserver emitted 0. Without
        // these events marco's compositor never marked the moved
        // window dirty, its SetPictureClipRectangles excluded the
        // window's region, and composites against the redirected
        // backing no-op'd — producing the "CC disappears on drag /
        // muddy bands on caja-redraw" symptom.
        //
        // Apply to the window itself. `accumulate_damage_full_to_state`
        // walks the ancestor chain so any compositor subscribed
        // higher in the tree also gets a translated rect. Only fires
        // when a damage object exists on the drawable (the helper
        // filters on `damage_object.drawable == this`), so cost is
        // zero for unredirected/uncomposited windows.
        // Xorg's Composite wakeup on ConfigureWindow is about visible
        // geometry changes of redirected windows. Pure restacks
        // (CWSibling/CWStackMode only) do affect stacking order, but
        // they are not modeled as "whole window damaged" events. Doing
        // that here seeds bogus fullscreen damage on marco's desktop
        // window (0x01300005), which then collapses the compositor's
        // update region before later dialog composites run.
        const CONFIGURE_DAMAGE_GEOMETRY_MASK: u16 = 0x001f; // x|y|w|h|border
        let geometry_changed = (request.value_mask & CONFIGURE_DAMAGE_GEOMETRY_MASK) != 0;
        // Gate on Viewable, mirroring the Expose path below. Xorg
        // damages a redirected window on configure only when it is
        // realized/viewable (compWindowUpdateAutomatic runs on realized
        // windows). i3's floating-drag creates a "floatingcon" root
        // child, sizes/moves it during the drag, but never maps it —
        // under an inherited RedirectSubwindows this path would emit
        // full-window DamageNotify for that unmapped window, and the
        // compositor (fastcompmgr) NameWindowPixmaps it and composites
        // its stale backing at the drag position (the smear trail).
        let viewable = state
            .resources
            .window(window_id)
            .is_some_and(|w| w.map_state == crate::resources::MapState::Viewable);
        if geometry_changed
            && viewable
            && let Some(mode) = effective_redirect_mode_for_window(state, window_id)
        {
            log::trace!(
                target: "yserver_core::core_loop::damage_fanout",
                "configure_damage_emit: window=0x{:x} geom=({},{} {}x{}) old_size={:?} resized={} mode={:?} mask=0x{:x}",
                window_id.0,
                geometry.x,
                geometry.y,
                geometry.width,
                geometry.height,
                old_size,
                resized,
                mode,
                request.value_mask,
            );
            let _dropped = accumulate_damage_full_to_state(state, window_id);
        } else if viewable
            && !resized
            && let Some((old_x, old_y, _, _, _)) = before_geom
            && (old_x, old_y) != (geometry.x, geometry.y)
            && (has_redirected_ancestor(state, window_id)
                || parent.is_some_and(|p| p != crate::resources::ROOT_WINDOW))
        {
            // A pure move of an UNREDIRECTED subwindow (inside a
            // redirected ancestor, the backend also carries its pixels to the new
            // position in the ancestor's backing, Xorg `fbCopyWindow`):
            // Xorg reports that copy through `damageCopyWindow`
            // (`miext/damage/damage.c`); a damage object on any ancestor
            // sees it, since window damage includes inferiors. (The
            // vacated area is reported by its exposure paint below.)
            // Without this the compositor's damage region is just the
            // parent's own ClipByChildren repaint, which excludes the
            // moved window — MATE's notification area stayed blank after
            // a panel drag until something else damaged that spot.
            log::trace!(
                target: "yserver_core::core_loop::damage_fanout",
                "configure_damage_emit_inferior_move: window=0x{:x} ({},{}) -> ({},{})",
                window_id.0,
                old_x,
                old_y,
                geometry.x,
                geometry.y,
            );
            let _dropped = accumulate_damage_full_to_state(state, window_id);
        } else {
            log::trace!(
                target: "yserver_core::core_loop::damage_fanout",
                "configure_damage_skip: window=0x{:x} geom=({},{} {}x{}) old_size={:?} resized={} geometry_changed={} mask=0x{:x}",
                window_id.0,
                geometry.x,
                geometry.y,
                geometry.width,
                geometry.height,
                old_size,
                resized,
                geometry_changed,
                request.value_mask,
            );
        }
        // Xorg validates the tree after a move, resize, restack or border
        // change and exposes what each window shows that it did not
        // (`miMoveWindow`, `miResizeWindow`, `ReflectStackChange`): its
        // background painted there, unless None, and Expose sent
        // (`miWindowExposures`, `mi/miexpose.c:375-410`). A resized window
        // gets its whole clip list: "the entire window is trashed unless
        // bitGravity recovers portions of it" (`mi/miwindow.c:466-472`);
        // this server keeps no bit-gravity bits, so its window is exposed
        // whole for every gravity. A top-level that is raised from under
        // another is exposed like any other window (#213: awesome maps
        // mpv's frame under the terminal, then raises it).
        if let Some(change) = tree_change {
            for (w, region) in change.exposed(state, resized.then_some(window_id)) {
                send_window_exposures(state, backend, origin, w, &region, true);
            }
        }
    }
    // Xorg miMoveWindow / miResizeWindow / ReflectStackChange end in
    // WindowsRestructured (`mi/miwindow.c:302,620`, `dix/window.c:2179`).
    backend.windows_restructured(state);
    // A confined pointer follows its confine window — re-clamp after
    // any geometry change (Xorg ConfineCursorToWindow on configure;
    // XGrabButton-25 moves confine_to and expects the pointer pulled
    // along).
    if state.pointer_confine_to.0 != 0 {
        confine_pointer_now(state, backend);
    }
    Ok(RequestOutcome::Handled)
}

pub(super) fn handle_destroy_window(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    if let Some(window) = x11::free_resource_id(body) {
        // Xorg `dix/dispatch.c::ProcDestroyWindow` returns BadWindow
        // via dixLookupWindow before tearing the subtree down. xts5
        // Xlib4 probes XDestroyWindow on badwin() and expects the
        // protocol error.
        if state.resources.window(window).is_none() {
            return emit_x11_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_WINDOW,
                window.0,
                4,
            );
        }
        destroy_window_subtree(state, backend, origin, window);
    }
    debug!("client {} #{} DestroyWindow", client_id.0, sequence.0);
    Ok(RequestOutcome::Handled)
}

pub(super) fn handle_destroy_subwindows(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    if body.len() >= 4 {
        let parent = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
        // Xorg `dix/dispatch.c::ProcDestroySubwindows` returns
        // BadWindow for an unknown parent xid.
        if state.resources.window(parent).is_none() {
            return emit_x11_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_WINDOW,
                parent.0,
                5,
            );
        }
        // Xorg unmaps them all first, so all UnmapNotifies and one exposure
        // of the parent precede the DestroyNotifies (`dix/window.c:1104-1124`).
        unmap_subwindows_with_delta(state, backend, origin, client_id, sequence, body)?;
        let kids: Vec<ResourceId> = state.resources.children(parent).to_vec();
        for k in kids {
            destroy_window_subtree(state, backend, origin, k);
        }
    }
    debug!("client {} #{} DestroySubwindows", client_id.0, sequence.0);
    Ok(RequestOutcome::Handled)
}

pub(super) fn handle_get_geometry(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    let drawable = x11::drawable_request_id(body).unwrap_or(ROOT_WINDOW);
    let geometry = state
        .resources
        .window(drawable)
        .map(window_geometry)
        .or_else(|| state.resources.pixmap(drawable).map(pixmap_geometry))
        // #96: a GLX pbuffer is a drawable but lives only in glx_drawables,
        // not the core resource store. Mesa's loader_dri3 calls GetGeometry on
        // the pbuffer XID to size its DRI3 backing; without this it gets
        // BadDrawable → "failed to create drawable" → ANGLE (Chromium) can't
        // create its init pbuffer → GPU process dies.
        .or_else(|| glx_pbuffer_geometry(state, drawable));
    let Some(geometry) = geometry else {
        // Spec: BadDrawable on unknown drawable. xts probes
        // destroyed/stale IDs and expects the protocol error.
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_DRAWABLE,
            drawable.0,
            14,
        );
    };
    debug!(
        "client {} #{} GetGeometry 0x{:x} -> root=0x{:x} pos=({},{}) size=({}x{}) border={} depth={}",
        client_id.0,
        sequence.0,
        drawable.0,
        geometry.root.0,
        geometry.x,
        geometry.y,
        geometry.width,
        geometry.height,
        geometry.border_width,
        geometry.depth,
    );
    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return Ok(RequestOutcome::Handled);
    };
    let byte_order = client.byte_order;
    let mut buf: Vec<u8> = Vec::with_capacity(32);
    x11::write_get_geometry_reply(&mut buf, byte_order, sequence, geometry)?;
    Ok(write_to_client(client, client_id, &buf))
}

pub(super) fn handle_query_tree(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    let window = x11::drawable_request_id(body).unwrap_or(ROOT_WINDOW);
    debug!(
        "client {} #{} QueryTree 0x{:x}",
        client_id.0, sequence.0, window.0
    );
    let Some(window_state) = state.resources.window(window) else {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_WINDOW,
            window.0,
            15,
        );
    };
    // X11 spec: the root window's parent is None. Internally we store
    // root.parent = root (self) and other code relies on that invariant, so
    // translate the self-parent to None only here on the QueryTree wire reply.
    // Reporting the root as its own parent makes Chromium/Ozone's GeometryCache
    // recurse forever (infinite QueryTree(root)+GetGeometry(root)), so Electron
    // apps (VS Code) spin and never map their window.
    let parent = if window_state.parent == window {
        ResourceId(0)
    } else {
        window_state.parent
    };
    // Xorg's QueryTree stops at RealChildHead (dix/dispatch.c:1078), which
    // Composite points at the overlay window while it tops the root
    // (composite/compwindow.c:762): the COW is a root child nobody lists.
    let mut children = window_state.children.clone();
    if window == ROOT_WINDOW {
        children.retain(|child| *child != COMPOSITE_OVERLAY_WINDOW);
    }
    debug!(
        "client {} #{} QueryTree reply 0x{:x}: parent=0x{:x} children={}",
        client_id.0,
        sequence.0,
        window.0,
        parent.0,
        children.len(),
    );
    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return Ok(RequestOutcome::Handled);
    };
    let byte_order = client.byte_order;
    let mut buf: Vec<u8> = Vec::with_capacity(64);
    x11::write_query_tree_reply(
        &mut buf,
        byte_order,
        sequence,
        ROOT_WINDOW,
        parent,
        &children,
    )?;
    Ok(write_to_client(client, client_id, &buf))
}

pub(super) fn handle_map_window(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    let Some(window) = x11::map_window_id(body) else {
        return Ok(RequestOutcome::Handled);
    };
    let Some((parent, override_redirect, current_map_state)) = state
        .resources
        .window(window)
        .map(|w| (w.parent, w.override_redirect, w.map_state))
    else {
        // Xorg `dix/dispatch.c::ProcMapWindow` does `dixLookupWindow`
        // first and returns BadWindow on lookup failure. xts5 Xlib4
        // probes XMapWindow on a freed xid (badwin()) and expects the
        // protocol error.
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_WINDOW,
            window.0,
            8,
        );
    };

    // Per X11 protocol (Xorg `dix/window.c:2661`): "If the window is
    // already mapped, this request has no effect." This MUST guard
    // the MapRequest dispatch below, not just the local mapping —
    // otherwise (a) the root window, which is permanently
    // `Viewable` and whose `parent` field points to itself, fans a
    // phantom MapRequest(parent=root, window=root) to any WM with
    // SubstructureRedirect on root (mate-screensaver hits this on
    // activation; marco's panic-response drops its substructure
    // subscription and breaks compositing of every subsequent
    // top-level); and (b) brisk-menu's ~50 Hz
    // `gtk_window_present`/`XMapRaised` loop on its already-viewable
    // popup re-fans MapNotify → Expose → full-extent damage,
    // triggering marco's per-MapNotify `COMPOSITE::NameWindowPixmap`
    // + recomposite (visible as menu flicker on KMS).
    if current_map_state != MapState::Unmapped {
        debug!(
            "client {} #{} MapWindow 0x{:x} (already mapped — no-op)",
            client_id.0, sequence.0, window.0
        );
        return Ok(RequestOutcome::Handled);
    }

    // SubstructureRedirect on the parent: forward MapRequest to the
    // first subscriber that isn't the requester (the WM), and skip
    // mapping locally.
    let redirect_targets: Vec<ClientId> = if !override_redirect {
        let requester_has = state
            .clients
            .get(&client_id.0)
            .and_then(|c| c.event_masks.get(&parent).copied())
            .is_some_and(|m| m & 0x0010_0000 != 0);
        if requester_has {
            Vec::new()
        } else {
            subscribers_by_id(state, parent, 0x0010_0000)
        }
    } else {
        Vec::new()
    };

    if !redirect_targets.is_empty() {
        debug!(
            "client {} MapWindow 0x{:x} -> MapRequest to WM",
            client_id.0, window.0
        );
        let _dropped = fanout_event_to_clients(state, &redirect_targets, |buf, seq, order| {
            x11::encode_map_request_event(buf, seq, order, parent, window);
        });
        return Ok(RequestOutcome::Handled);
    }

    let transition = state.resources.map_window(window);
    debug_assert!(
        transition.mapping_changed,
        "current_map_state == Unmapped guard above was checked; \
         map_window should now transition",
    );
    let host_xid = state.resources.window(window).and_then(|w| w.host_xid);
    let map_info = state
        .resources
        .window(window)
        .map(|w| (w.parent, w.override_redirect));
    if let Some(xid) = host_xid {
        let _ = backend.map_subwindow(origin, xid.as_raw());
    }
    realize_storage_for_delta(state, backend, origin, &transition.delta);
    // Every window that became viewable under a redirect (its own, or its
    // parent's RedirectSubwindows) gets a backing, as Xorg allocates on
    // realize (`compwindow.c:274`). AFTER `map_subwindow` per the plan's
    // codex-round-6 ordering fix — `map_subwindow` blindly flips
    // `scene_participating = true`, so the Manual participation flip
    // inside `activate_redirect_backing_for` must land last.
    apply_viewability_delta_to_redirects(state, backend, origin, &transition.delta);
    if host_xid.is_some() {
        reapply_redirect_mode_after_map(state, backend, origin, window);
    }

    if let Some((parent, override_redir)) = map_info {
        let _dropped = emit_window_event_to_state(state, window, 0x0002_0000, |buf, seq, order| {
            x11::encode_map_notify_event(buf, seq, order, window, window, override_redir);
        });
        let _dropped = emit_window_event_to_state(state, parent, 0x0008_0000, |buf, seq, order| {
            x11::encode_map_notify_event(buf, seq, order, parent, window, override_redir);
        });
        // Emit VisibilityNotify(Unobscured) then Expose on the window
        // itself when it becomes viewable. Subscribed clients want the
        // newly-viewable window to redraw.
        let viewable = state
            .resources
            .window(window)
            .is_some_and(|w| w.map_state == crate::resources::MapState::Viewable);
        if viewable {
            // VisibilityNotify(Unobscured) for clients selecting
            // VisibilityChangeMask (0x10000). Under yserver's compositor
            // model every viewable top-level is effectively unobscured.
            // Without this, GTK3 leaves the window in a non-paintable
            // visibility state and suppresses all frame-clock paints
            // after the first expose-driven frame — the
            // cinnamon-settings "title changes but content never
            // repaints" freeze. Xorg sends Unobscured on map; we mirror
            // that. (We never send Obscured: a composited top-level is
            // always paintable, and unmap is signalled by UnmapNotify
            // per spec, not VisibilityNotify.)
            let _dropped =
                emit_window_event_to_state(state, window, 0x0001_0000, |buf, seq, order| {
                    x11::encode_visibility_notify_event(buf, seq, order, window, 0);
                });
            // Descendants that were MapWindow'd while this window was
            // still unmapped were Unviewable; mapping this window
            // transitions them to Viewable. Xorg fires
            // VisibilityNotify(Unobscured) on every such descendant
            // (mate-xorg.xtrace seq 0x0147 shows VisibilityNotify on
            // both the FF main window and its child 0x02400010).
            // yserver previously only notified the directly-mapped
            // window, so GTK3's frame clock for the child never woke
            // and the profile chooser stayed blank — the "empty shadow"
            // FF bug on bee. Order with the Expose subtree walk below
            // doesn't matter; both are subtree-wide and idempotent.
            let _dropped = emit_visibility_unobscured_subtree_to_state(state, window);
            send_map_exposures(state, backend, origin, &[window]);
        }
    }
    // Audit #11: Xorg's `miPaintWindow` fires damage on the window's
    // extent when the window becomes viewable (the server-background
    // fill is itself a paint, and DAMAGE hooks every paint).
    // Compositors that subscribe via `XDamageCreate(window)` rely on
    // that first DamageNotify to read the freshly-mapped window's
    // pixels into their own offscreen.
    //
    // Order matters: this MUST come AFTER the MapNotify emissions
    // above. Marco subscribes to SubstructureNotify on root, registers
    // the window in its compositor tree on receiving MapNotify, and
    // only then accepts DAMAGE-Notify on it as a NameWindowPixmap
    // trigger. Pre-fix (damage emitted before MapNotify) marco saw the
    // damage on a window it didn't yet track and silently dropped it,
    // causing mate-panel-top to render blank until a later resize
    // damage finally landed. See mate.xtrace lines 4938→4940 vs
    // mate-xorg.xtrace lines 5164→5173.
    if host_xid.is_some() {
        let _dropped = accumulate_damage_full_to_state(state, window);
        // The mapped window itself keeps its just-fired notify cycle.
        for promoted in transition
            .delta
            .became_viewable
            .iter()
            .filter(|w| **w != window)
        {
            reset_damage_notify_cycle_for_drawable(state, *promoted);
        }
        accumulate_damage_viewable_descendants_to_state(state, window);
    }
    // Xorg MapWindow ends in WindowsRestructured (`dix/window.c:2695`): the
    // pointer's crossings follow MapNotify and Expose within the request.
    backend.windows_restructured(state);
    debug!(
        "client {} #{} MapWindow 0x{:x} viewable+{}",
        client_id.0,
        sequence.0,
        window.0,
        transition.delta.became_viewable.len()
    );
    Ok(RequestOutcome::Handled)
}

fn accumulate_damage_viewable_descendants_to_state(state: &mut ServerState, root: ResourceId) {
    let children: Vec<ResourceId> = state.resources.children(root).to_vec();
    for child in children {
        let child_window = state.resources.window(child);
        let viewable = child_window.is_some_and(|w| w.map_state == MapState::Viewable);
        if !viewable {
            continue;
        }
        let _dropped = accumulate_damage_full_to_state(state, child);
        accumulate_damage_viewable_descendants_to_state(state, child);
    }
}

pub(super) fn handle_map_subwindows(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    map_subwindows_with_delta(state, backend, origin, client_id, sequence, body)
        .map(|(outcome, _delta)| outcome)
}

/// MapSubwindows; also returns the union of the children's viewability deltas.
pub(super) fn map_subwindows_with_delta(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<(RequestOutcome, ViewabilityDelta)> {
    let mut delta = ViewabilityDelta::default();
    let Some(parent) = x11::map_window_id(body) else {
        return Ok((RequestOutcome::Handled, delta));
    };
    if state.resources.window(parent).is_none() {
        // Xorg `dix/dispatch.c::ProcMapSubwindows` returns BadWindow
        // for an unknown parent xid. xts5 Xlib4 probes XMapSubwindows
        // on badwin() and expects the protocol error.
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_WINDOW,
            parent.0,
            9,
        )
        .map(|outcome| (outcome, delta));
    }
    let children: Vec<ResourceId> = state.resources.children(parent).to_vec();
    let mut newly_viewable = Vec::new();
    for child in children {
        let transition = state.resources.map_window(child);
        let was_unmapped = transition.mapping_changed;
        let host_xid = state.resources.window(child).and_then(|w| w.host_xid);
        let override_redirect = state
            .resources
            .window(child)
            .is_some_and(|w| w.override_redirect);
        if let Some(xid) = host_xid {
            let _ = backend.map_subwindow(origin, xid.as_raw());
        }
        realize_storage_for_delta(state, backend, origin, &transition.delta);
        // Same post-hook as `handle_map_window`: AFTER `map_subwindow`.
        apply_viewability_delta_to_redirects(state, backend, origin, &transition.delta);
        delta.extend(transition.delta);
        if host_xid.is_some() {
            reapply_redirect_mode_after_map(state, backend, origin, child);
            // Audit #11: see `handle_map_window` for the rationale.
            // Mirror the damage bump so MapSubwindows-driven mass
            // maps (mate-panel applet realize cascade, GTK
            // children-on-show) also notify subscribed compositors.
            let _dropped = accumulate_damage_full_to_state(state, child);
        }
        if was_unmapped {
            let _dropped =
                emit_window_event_to_state(state, child, 0x0002_0000, |buf, seq, order| {
                    x11::encode_map_notify_event(buf, seq, order, child, child, override_redirect);
                });
            let _dropped =
                emit_window_event_to_state(state, parent, 0x0008_0000, |buf, seq, order| {
                    x11::encode_map_notify_event(buf, seq, order, parent, child, override_redirect);
                });
        }
        // Only a child now Viewable (its parent mapped too) is exposed,
        // with the descendants that mapping it promoted to Viewable.
        let viewable = state
            .resources
            .window(child)
            .is_some_and(|w| w.map_state == MapState::Viewable);
        if was_unmapped && viewable {
            newly_viewable.push(child);
        }
    }
    // Xorg maps them all, then validates and exposes once: top-most first,
    // each clipped by the siblings mapped with it (`dix/window.c:2760-2775`).
    newly_viewable.reverse();
    send_map_exposures(state, backend, origin, &newly_viewable);
    // Xorg MapSubwindows: one WindowsRestructured after the batch
    // (`dix/window.c:2775`).
    backend.windows_restructured(state);
    debug!(
        "client {} #{} MapSubwindows viewable+{}",
        client_id.0,
        sequence.0,
        delta.became_viewable.len()
    );
    Ok((RequestOutcome::Handled, delta))
}

pub(super) fn handle_unmap_window(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    if let Some(window) = x11::map_window_id(body) {
        if state.resources.window(window).is_none() {
            // Xorg `dix/dispatch.c::ProcUnmapWindow` returns BadWindow
            // via dixLookupWindow before unmapping. xts5 Xlib4 probes
            // XUnmapWindow on badwin() and expects the protocol error.
            return emit_x11_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_WINDOW,
                window.0,
                10,
            );
        }
        let host_xid = state.resources.window(window).and_then(|w| w.host_xid);
        let viewable_before = state
            .resources
            .window(window)
            .is_some_and(|w| w.map_state == MapState::Viewable);
        let clips_before = if viewable_before {
            crate::core_loop::clip_list::clip_lists_under(state, window)
        } else {
            Vec::new()
        };
        let transition = state.resources.unmap_window(window);
        let was_mapped = transition.mapping_changed;
        let parent = if was_mapped {
            state
                .resources
                .window(window)
                .map_or(ROOT_WINDOW, |w| w.parent)
        } else {
            ROOT_WINDOW
        };
        if let Some(xid) = host_xid {
            let _ = backend.unmap_subwindow(origin, xid.as_raw());
        }
        // Xorg frees each redirect pixmap at unrealize (`compwindow.c:291`).
        apply_viewability_delta_to_redirects(state, backend, origin, &transition.delta);
        release_storage_for_delta(state, backend, origin, &transition.delta);
        // XI1: an active device grab is released automatically when its
        // grab window becomes not viewable (XTS XGrabDeviceKey-9; Xorg
        // DeactivateGrabsOnWindowUnmap shape).
        if was_mapped {
            let released: Vec<u16> = state
                .xi1_active_grabs
                .iter()
                .filter(|(_, g)| {
                    state
                        .resources
                        .window(g.grab_window)
                        .is_none_or(|w| w.map_state != crate::resources::MapState::Viewable)
                })
                .map(|(d, _)| *d)
                .collect();
            for dev in released {
                state.xi1_active_grabs.remove(&dev);
                let xid_map = backend.xid_map().clone();
                crate::core_loop::pointer_fanout::xi1_thaw_device(state, backend, &xid_map, dev);
            }
        }
        if was_mapped {
            let _dropped =
                emit_window_event_to_state(state, window, 0x0002_0000, |buf, seq, order| {
                    x11::encode_unmap_notify_event(buf, seq, order, window, window, false);
                });
            let _dropped =
                emit_window_event_to_state(state, parent, 0x0008_0000, |buf, seq, order| {
                    x11::encode_unmap_notify_event(buf, seq, order, parent, window, false);
                });
            // What it covered is exposed: its parent's and lower
            // siblings' backgrounds painted there, and Expose sent
            // (`UnmapWindow`, `dix/window.c:2856-2866`).
            if viewable_before {
                let after = crate::core_loop::clip_list::clip_lists_under(state, window);
                for (w, region) in
                    crate::core_loop::clip_list::newly_exposed(&clips_before, after, None)
                {
                    send_window_exposures(state, backend, origin, w, &region, true);
                }
            }
            // XI1: a device focus on a window that just became
            // unviewable reverts per its revert_to, emitting
            // DeviceFocusIn/Out (Xi/exevents.c
            // DeleteDeviceFromAnyExtEvents). After UnmapNotify, matching
            // Xorg's UnmapWindow → UnrealizeTree ordering.
            crate::core_loop::xi1_focus::revert_unviewable_focus(state);
            // Core focus likewise (Xorg UnrealizeTree →
            // DeleteWindowFromAnyEvents): covers the focus window
            // itself or any unmapped ancestor.
            revert_core_focus_if_unviewable(state);
            // Active grabs on a window that just became unviewable
            // deactivate too (same Xorg path).
            release_core_grabs_for_unviewable(state, backend);
            // Then WindowsRestructured (`dix/window.c:2871`).
            backend.windows_restructured(state);
        }
    }
    debug!("client {} #{} UnmapWindow", client_id.0, sequence.0);
    Ok(RequestOutcome::Handled)
}

pub(super) fn handle_unmap_subwindows(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    unmap_subwindows_with_delta(state, backend, origin, client_id, sequence, body)
        .map(|(outcome, _delta)| outcome)
}

/// UnmapSubwindows; also returns the union of the children's viewability deltas.
pub(super) fn unmap_subwindows_with_delta(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<(RequestOutcome, ViewabilityDelta)> {
    let mut delta = ViewabilityDelta::default();
    let Some(parent) = x11::map_window_id(body) else {
        return Ok((RequestOutcome::Handled, delta));
    };
    struct PendingUnmap {
        child: ResourceId,
        host_xid: Option<crate::backend::WindowHandle>,
    }
    let Some(mut children) = state.resources.mapped_children_bottom_to_top(parent) else {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_WINDOW,
            parent.0,
            11,
        )
        .map(|outcome| (outcome, delta));
    };
    // UnmapSubwindows stops at RealChildHead too (dix/window.c:2897): the
    // overlay window stays mapped.
    if parent == ROOT_WINDOW {
        children.retain(|child| *child != COMPOSITE_OVERLAY_WINDOW);
    }
    let clip_before = crate::core_loop::clip_list::clip_list(state, parent);
    // Snapshot mapping order + collect host xids; unmap each in the
    // resource table.
    let mut pending: Vec<PendingUnmap> = Vec::new();
    for child in children {
        let host_xid = state.resources.window(child).and_then(|w| w.host_xid);
        let transition = state.resources.unmap_window(child);
        delta.extend(transition.delta);
        if transition.mapping_changed {
            pending.push(PendingUnmap { child, host_xid });
        }
    }
    for item in pending {
        if let Some(xid) = item.host_xid {
            let _ = backend.unmap_subwindow(origin, xid.as_raw());
        }
        let child = item.child;
        let _dropped = emit_window_event_to_state(state, child, 0x0002_0000, |buf, seq, order| {
            x11::encode_unmap_notify_event(buf, seq, order, child, child, false);
        });
        let _dropped = emit_window_event_to_state(state, parent, 0x0008_0000, |buf, seq, order| {
            x11::encode_unmap_notify_event(buf, seq, order, parent, child, false);
        });
    }
    // Xorg frees each redirect pixmap at unrealize (`compwindow.c:291`).
    apply_viewability_delta_to_redirects(state, backend, origin, &delta);
    release_storage_for_delta(state, backend, origin, &delta);
    // The parent is exposed where its children were (`dix/window.c:2925-2933`).
    let gained = crate::core_loop::clip_list::subtract(
        &crate::core_loop::clip_list::clip_list(state, parent),
        &clip_before,
    );
    send_window_exposures(state, backend, origin, parent, &gained, true);
    crate::core_loop::xi1_focus::revert_unviewable_focus(state);
    revert_core_focus_if_unviewable(state);
    release_core_grabs_for_unviewable(state, backend);
    // Xorg UnmapSubwindows: one WindowsRestructured (`dix/window.c:2939`).
    backend.windows_restructured(state);
    debug!(
        "client {} #{} UnmapSubwindows viewable-{}",
        client_id.0,
        sequence.0,
        delta.became_unviewable.len()
    );
    Ok((RequestOutcome::Handled, delta))
}

pub(super) fn handle_get_window_attributes(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    let id = x11::drawable_request_id(body).unwrap_or(ROOT_WINDOW);
    debug!(
        "client {} #{} GetWindowAttributes 0x{:x}",
        client_id.0, sequence.0, id.0,
    );
    // Spec: BadWindow on unknown window ID. Don't silently fall back to
    // ROOT_WINDOW — xts probes stale/destroyed XIDs and expects the
    // protocol error.
    if state.resources.window(id).is_none() {
        return emit_x11_error(state, client_id, sequence, x11::error::BAD_WINDOW, id.0, 3);
    }
    let target = id;
    let your_event_mask = state
        .clients
        .get(&client_id.0)
        .and_then(|c| c.event_masks.get(&target).copied())
        .unwrap_or(0);
    let all_event_masks: u32 = state
        .clients
        .values()
        .filter_map(|c| c.event_masks.get(&target).copied())
        .fold(0u32, |a, b| a | b);
    let attrs = window_attributes(
        state.resources.window(target),
        all_event_masks,
        your_event_mask,
    );
    if let Some(window) = state.resources.window(target) {
        debug!(
            "client {} #{} GetWindowAttributes reply 0x{:x}: parent=0x{:x} map_state={} override_redirect={}",
            client_id.0,
            sequence.0,
            target.0,
            window.parent.0,
            attrs.map_state,
            attrs.override_redirect,
        );
    }
    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return Ok(RequestOutcome::Handled);
    };
    let byte_order = client.byte_order;
    let mut buf: Vec<u8> = Vec::with_capacity(48);
    x11::write_get_window_attributes_reply(&mut buf, byte_order, sequence, attrs)?;
    Ok(write_to_client(client, client_id, &buf))
}

fn window_attributes(
    window: Option<&Window>,
    all_event_masks: u32,
    your_event_mask: u32,
) -> x11::WindowAttributes {
    let window = window.expect("root window exists");
    x11::WindowAttributes {
        visual: window.visual,
        class: window.class.protocol_value(),
        bit_gravity: window.bit_gravity,
        win_gravity: window.win_gravity,
        backing_store: window.backing_store,
        backing_planes: window.backing_planes,
        backing_pixel: window.backing_pixel,
        save_under: window.save_under,
        map_is_installed: true,
        map_state: window.map_state.protocol_value(),
        override_redirect: window.override_redirect,
        colormap: window.colormap,
        all_event_masks,
        your_event_mask,
        do_not_propagate_mask: window.do_not_propagate_mask,
    }
}

pub(super) fn handle_circulate_window(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    if body.len() < 4 {
        debug!(
            "client {} #{} CirculateWindow (short body)",
            client_id.0, sequence.0
        );
        return Ok(RequestOutcome::Handled);
    }
    let container = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
    let direction = header.data;
    if state.resources.window(container).is_none() {
        // Xorg `dix/dispatch.c::ProcCirculateWindow` returns BadWindow
        // via dixLookupWindow on an unknown xid. xts5 Xlib4 probes
        // XCirculateSubwindows{,Up,Down} on badwin() and expects it.
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_WINDOW,
            container.0,
            13,
        );
    }
    let Some(child) = state.circulate_candidate(container, direction) else {
        return Ok(RequestOutcome::Handled);
    };
    // SubstructureRedirect on the container by another client turns it into
    // a CirculateRequest (Xorg MaybeDeliverEventsToClient skips the
    // requester).
    let redirect_target = subscribers_by_id(state, container, 0x0010_0000)
        .into_iter()
        .find(|c| *c != client_id);
    if let Some(target) = redirect_target {
        let _dropped = fanout_event_to_clients(state, &[target], |buf, seq, order| {
            let _ =
                x11::write_circulate_request_event(buf, order, seq, container, child, direction);
        });
    } else {
        let tree_change = crate::core_loop::clip_list::TreeChange::begin(state, child, None);
        state.resources.circulate_child(child, direction == 0);
        if let Some(xid) = state.resources.window(child).and_then(|w| w.host_xid) {
            let _ = backend.configure_subwindow(
                None,
                xid.as_raw(),
                crate::host_x11::HostSubwindowConfig {
                    x: None,
                    y: None,
                    width: None,
                    height: None,
                    border_width: None,
                    sibling: None,
                    stack_mode: Some(if direction == 0 { 0 } else { 1 }),
                },
            );
        }
        backend.sync_top_level_order(state);
        // CirculateNotify to the window's StructureNotify and the parent's
        // SubstructureNotify selectors, each with its own event window; the
        // place (OnTop 0 / OnBottom 1) equals the direction.
        let _dropped = emit_window_event_to_state(state, child, 0x0002_0000, |buf, seq, order| {
            let _ = x11::write_circulate_notify_event(buf, order, seq, child, child, direction);
        });
        let _dropped =
            emit_window_event_to_state(state, container, 0x0008_0000, |buf, seq, order| {
                let _ =
                    x11::write_circulate_notify_event(buf, order, seq, container, child, direction);
            });
        // Then ReflectStackChange validates and exposes (`dix/window.c:2149-2176`).
        if let Some(change) = tree_change {
            for (w, region) in change.exposed(state, None) {
                send_window_exposures(state, backend, None, w, &region, true);
            }
        }
        // Xorg ReflectStackChange (`dix/window.c:2179`).
        backend.windows_restructured(state);
    }
    debug!("client {} #{} CirculateWindow", client_id.0, sequence.0);
    Ok(RequestOutcome::Handled)
}

pub(super) fn handle_change_save_set(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    if body.len() >= 4
        && let Some(c) = state.clients.get_mut(&client_id.0)
    {
        let win = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
        match header.data {
            0 => {
                c.save_set.insert(win);
            }
            1 => {
                c.save_set.remove(&win);
            }
            _ => {}
        }
    }
    debug!("client {} #{} ChangeSaveSet", client_id.0, sequence.0);
    Ok(RequestOutcome::Handled)
}

fn window_geometry(window: &Window) -> x11::Geometry {
    x11::Geometry {
        root: ROOT_WINDOW,
        x: window.x,
        y: window.y,
        width: window.width,
        height: window.height,
        border_width: window.border_width,
        depth: window.depth,
    }
}

fn pixmap_geometry(pixmap: &Pixmap) -> x11::Geometry {
    x11::Geometry {
        root: ROOT_WINDOW,
        x: 0,
        y: 0,
        width: pixmap.width,
        height: pixmap.height,
        border_width: 0,
        depth: pixmap.depth,
    }
}

/// Geometry of a GLX pbuffer (#96). Pbuffers are tracked in `glx_drawables`
/// with their `CreatePbuffer` size but are absent from the core resource
/// store, so `GetGeometry` must resolve them here. A 0×0 pbuffer returns
/// `None` (size guard preserved): the caller then falls through to the
/// backing pixmap clamped to 1×1 by `CREATE_PBUFFER`, and a missing
/// drawable stays `BadDrawable` instead of becoming a Success(0×0).
fn glx_pbuffer_geometry(state: &ServerState, drawable: ResourceId) -> Option<x11::Geometry> {
    let d = state.glx_drawables.get(&drawable.0)?;
    if d.kind != crate::server::GlxDrawableKind::Pbuffer || (d.width == 0 && d.height == 0) {
        return None;
    }
    Some(x11::Geometry {
        root: ROOT_WINDOW,
        x: 0,
        y: 0,
        width: u16::try_from(d.width).unwrap_or(u16::MAX),
        height: u16::try_from(d.height).unwrap_or(u16::MAX),
        border_width: 0,
        depth: glx_fbconfig_depth(d.fbconfig),
    })
}

#[allow(clippy::cast_possible_truncation)]
pub(super) fn handle_translate_coordinates(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    let (child, dst_x, dst_y, log_src, log_dst, log_src_xy) = if body.len() >= 12 {
        let src_window = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
        let dst_window = ResourceId(u32::from_le_bytes([body[4], body[5], body[6], body[7]]));
        let src_x = i16::from_le_bytes([body[8], body[9]]);
        let src_y = i16::from_le_bytes([body[10], body[11]]);
        let (src_abs_x, src_abs_y) = state.resources.window_absolute_position(src_window);
        let abs_x = src_abs_x + i32::from(src_x);
        let abs_y = src_abs_y + i32::from(src_y);
        let (dst_abs_x, dst_abs_y) = state.resources.window_absolute_position(dst_window);
        let dst_x = (abs_x - dst_abs_x) as i16;
        let dst_y = (abs_y - dst_abs_y) as i16;
        // Xorg ProcTranslateCoords checks both bounding and input shapes
        // while walking the destination's direct children.  Reuse the
        // server's shape-aware hit test so the Composite Overlay Window's
        // empty input shape does not hide a mapped popup beneath it.
        let child = state
            .direct_child_at(dst_window, dst_x, dst_y)
            .unwrap_or(ResourceId(0));
        (
            child,
            dst_x,
            dst_y,
            src_window.0,
            dst_window.0,
            (src_x, src_y),
        )
    } else {
        (ResourceId(0), 0i16, 0i16, 0u32, 0u32, (0i16, 0i16))
    };
    debug!(
        "client {} #{} TranslateCoordinates src=0x{:x} dst=0x{:x} src_xy=({},{}) -> dst_xy=({},{}) child=0x{:x}",
        client_id.0,
        sequence.0,
        log_src,
        log_dst,
        log_src_xy.0,
        log_src_xy.1,
        dst_x,
        dst_y,
        child.0,
    );
    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return Ok(RequestOutcome::Handled);
    };
    let byte_order = client.byte_order;
    let mut buf: Vec<u8> = Vec::with_capacity(32);
    x11::write_translate_coordinates_reply(&mut buf, byte_order, sequence, child, dst_x, dst_y)?;
    Ok(write_to_client(client, client_id, &buf))
}
