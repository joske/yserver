use super::*;

/// XIWarpPointer (XI 2.0 minor 41) — Xorg `ProcXIWarpPointer`
/// (Xi/xiwarppointer.c): core WarpPointer for one device, with FP16.16
/// coordinates. Only a master pointer or a floating slave may be warped;
/// yserver's only candidate is the master pointer (2), whose sprite is the
/// core one, so this runs the core warp path with the XI source-rectangle
/// rule.
pub(super) fn handle_xi_warp_pointer(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    // Length is gated to exactly 9 units above, so the body is 32 bytes.
    let u32_at = |o: usize| u32::from_le_bytes([body[o], body[o + 1], body[o + 2], body[o + 3]]);
    let fp1616_pixels = |o: usize| u32_at(o).cast_signed() / 65536;
    let u16_at = |o: usize| u16::from_le_bytes([body[o], body[o + 1]]);
    let deviceid = u16_at(28);
    if deviceid != crate::xinput::DEVICEID_MASTER_POINTER {
        return xi1_error(
            state,
            client_id,
            sequence,
            XI1_ERROR_BAD_DEVICE,
            u32::from(deviceid),
            41,
        );
    }
    let req = WarpRequest {
        src: ResourceId(u32_at(0)),
        dst: ResourceId(u32_at(4)),
        src_x: fp1616_pixels(8),
        src_y: fp1616_pixels(12),
        src_w: u16_at(16),
        src_h: u16_at(18),
        dst_x: fp1616_pixels(20),
        dst_y: fp1616_pixels(24),
    };
    if let Some(bad) = warp_pointer_bad_window(state, &req) {
        return xi1_error(state, client_id, sequence, x11::error::BAD_WINDOW, bad, 41);
    }
    if warp_pointer_src_allows(state, &req, WarpSrcRule::XInput2) {
        apply_pointer_warp(state, backend, origin, &req);
    }
    debug!(
        "client {} #{} XIWarpPointer dst=0x{:x} ({}, {})",
        client_id.0, sequence.0, req.dst.0, req.dst_x, req.dst_y
    );
    Ok(RequestOutcome::Handled)
}

pub(super) fn handle_get_input_focus(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
) -> io::Result<RequestOutcome> {
    debug!("client {} #{} GetInputFocus", client_id.0, sequence.0);
    let focus = state.core_focus;
    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return Ok(RequestOutcome::Handled);
    };
    let byte_order = client.byte_order;
    let mut buf: Vec<u8> = Vec::with_capacity(32);
    x11::write_get_input_focus_reply(
        &mut buf,
        byte_order,
        sequence,
        ResourceId(focus.raw),
        focus.revert_to,
    )?;
    Ok(write_to_client(client, client_id, &buf))
}

pub(super) fn handle_set_input_focus(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    if let Some(window) = x11::input_focus_window(body) {
        // Validation per Xorg SetInputFocus (dix/events.c): revert_to
        // ∈ {None, PointerRoot, Parent} else BadValue; a focus window
        // (anything other than None(0)/PointerRoot(1)) must exist
        // (BadWindow) and be viewable (BadMatch).
        let revert_to = header.data;
        if revert_to > 2 {
            return emit_x11_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_VALUE,
                u32::from(revert_to),
                42,
            );
        }
        if let Some((code, value)) = core_focus_window_error(state, window) {
            return emit_x11_error(state, client_id, sequence, code, value, 42);
        }
        let time = body
            .get(4..8)
            .map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
        apply_core_input_focus(state, client_id, sequence, window, revert_to, time);
    }
    debug!("client {} #{} SetInputFocus", client_id.0, sequence.0);
    Ok(RequestOutcome::Handled)
}

/// The window check of Xorg `SetInputFocus`: a focus other than
/// None(0)/PointerRoot(1) must be an existing window (BadWindow) that is
/// viewable (BadMatch). Returns `(error code, errorValue)`.
pub(super) fn core_focus_window_error(
    state: &ServerState,
    window: ResourceId,
) -> Option<(u8, u32)> {
    if window.0 <= 1 {
        return None;
    }
    match state.resources.window(window) {
        None => Some((x11::error::BAD_WINDOW, window.0)),
        Some(w) if w.map_state != crate::resources::MapState::Viewable => {
            Some((x11::error::BAD_MATCH, window.0))
        }
        Some(_) => None,
    }
}

/// The rest of Xorg `SetInputFocus` for the master keyboard, whose focus
/// is the core focus (core SetInputFocus and XISetFocus on device 3 both
/// land here): the timestamp gate, the FocusOut/FocusIn chain, and the
/// new focus/revert_to/time. `window` has passed
/// [`core_focus_window_error`].
pub(super) fn apply_core_input_focus(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    window: ResourceId,
    revert_to: u8,
    time: u32,
) {
    // Xorg SetInputFocus: requests with a time LATER than the
    // current time or EARLIER than the last focus time are
    // silently ignored.
    let now = state
        .timestamp_now()
        .max(state.xi1_last_input_time)
        .max(state.core_focus.time);
    if time != 0
        && (crate::core_loop::xi1_focus::time_after(time, now)
            || crate::core_loop::xi1_focus::time_after(state.core_focus.time, time))
    {
        debug!(
            "client {} #{} SetInputFocus ignored (time {time} outside [{}, {now}])",
            client_id.0, sequence.0, state.core_focus.time
        );
        return;
    }
    debug!(
        "focus decision: client {} SetInputFocus 0x{:x} revert_to={revert_to}",
        client_id.0, window.0
    );
    log::trace!(
        target: "yserver::input::focus",
        "SetInputFocus client={} window={} revert_to={revert_to}",
        state.debug_client_label(client_id),
        state.debug_window_label(window),
    );
    let from_raw = state.core_focus.raw;
    let to_raw = window.0;
    if from_raw != to_raw {
        // Xorg SetInputFocus (dix/events.c:4923): the focus transition
        // is reported NotifyWhileGrabbed (3) while a keyboard grab is
        // active, else NotifyNormal (0). bspwm focuses a newly-mapped
        // window WHILE sxhkd's synchronous passive key grab is still
        // active; emitting NotifyNormal there told GLFW/kitty it had
        // genuinely taken focus mid-grab, and its focus state machine
        // then ignored every typed key (the keys still routed
        // correctly to its window — only the FocusIn mode was wrong).
        // Mirrors the same gate in `revert_core_focus_from`.
        let mode = if state.active_keyboard_grab.is_some() {
            3 // NotifyWhileGrabbed
        } else {
            0 // NotifyNormal
        };
        emit_core_focus_transition(state, from_raw, to_raw, mode);
    }
    state.core_focus = crate::server::CoreFocus {
        raw: to_raw,
        revert_to,
        time: if time == 0 { now } else { time },
    };
    // Legacy mirror — the key fanout's `current_focus` and other
    // readers still consult the per-client field; None/PointerRoot
    // map to ROOT_WINDOW there (the fanout resolves PointerRoot
    // through `state.core_focus` directly).
    let mirror = match to_raw {
        0 | 1 => ROOT_WINDOW,
        w => ResourceId(w),
    };
    for c in state.clients.values_mut() {
        c.focused_window = mirror;
    }
}

/// Focus revert when the focus window (or an ancestor) becomes
/// unviewable or is destroyed — Xorg `DeleteWindowFromAnyEvents`
/// (dix/events.c:5889-5931), reached from both `UnrealizeTree`
/// (unmap) and window deletion. Reverts per `revert_to`: None →
/// None; PointerRoot → PointerRoot; Parent → nearest viewable
/// ancestor, and `revert_to` collapses to RevertToNone.
pub(super) fn revert_core_focus_from(
    state: &mut ServerState,
    win: ResourceId,
    dying: &[ResourceId],
) {
    if state.core_focus.raw != win.0 || win == ROOT_WINDOW {
        return;
    }
    let mode = if state.active_keyboard_grab.is_some() {
        3 // NotifyWhileGrabbed
    } else {
        0 // NotifyNormal
    };
    let (to_raw, new_revert) = match state.core_focus.revert_to {
        2 => {
            // RevertToParent: the nearest surviving viewable ancestor
            // (Xorg's `while (!parent->realized)` walk — `dying`
            // marks windows in a subtree being destroyed, which are
            // still Viewable at this point but won't survive).
            let mut parent = state
                .resources
                .window(win)
                .map_or(ROOT_WINDOW, |w| w.parent);
            loop {
                if parent == ROOT_WINDOW {
                    break;
                }
                let Some(w) = state.resources.window(parent) else {
                    parent = ROOT_WINDOW;
                    break;
                };
                if w.map_state == crate::resources::MapState::Viewable && !dying.contains(&parent) {
                    break;
                }
                if w.parent == parent {
                    break;
                }
                parent = w.parent;
            }
            (parent.0, 0)
        }
        1 => (1, state.core_focus.revert_to),
        _ => (0, state.core_focus.revert_to),
    };
    emit_core_focus_transition(state, win.0, to_raw, mode);
    state.core_focus.raw = to_raw;
    state.core_focus.revert_to = new_revert;
    let mirror = match to_raw {
        0 | 1 => ROOT_WINDOW,
        w => ResourceId(w),
    };
    for c in state.clients.values_mut() {
        c.focused_window = mirror;
    }
}

/// Unmap-side focus revert: fire `revert_core_focus_from` when the
/// focus window stopped being viewable (covers an unmapped ancestor —
/// Xorg unrealizes the whole tree and hits the focus check per child).
pub(super) fn revert_core_focus_if_unviewable(state: &mut ServerState) {
    let raw = state.core_focus.raw;
    if raw <= 1 {
        return;
    }
    let viewable = state
        .resources
        .window(ResourceId(raw))
        .is_some_and(|w| w.map_state == crate::resources::MapState::Viewable);
    if !viewable {
        revert_core_focus_from(state, ResourceId(raw), &[]);
    }
}

/// Emit the FocusOut/FocusIn chain for a core focus transition
/// (`crate::crossings::focus_transition_events`) through the normal
/// per-window event-mask filter, plus the matching XI2 focus events
/// on the endpoint windows. `mode` is the wire NotifyNormal(0) /
/// NotifyWhileGrabbed(3) / NotifyGrab(1) / NotifyUngrab(2) value.
pub(crate) fn emit_core_focus_transition(
    state: &mut ServerState,
    from_raw: u32,
    to_raw: u32,
    mode: u8,
) {
    log::trace!(
        target: "yserver::input::focus",
        "FOCUS-EMIT from={} to={} mode={mode}",
        state.debug_window_label(ResourceId(from_raw)),
        state.debug_window_label(ResourceId(to_raw)),
    );
    let pointer_win = crate::core_loop::key_fanout::deepest_window_at_pointer(state);
    // Xorg DoFocusEvents (dix/enterleave.c:1557) short-circuits a
    // same-window transition ONLY for non-grab modes:
    //   `if (from == to && mode != NotifyGrab && mode != NotifyUngrab) return;`
    // For NotifyGrab/NotifyUngrab with from == to (the grab window is
    // already the focus), CoreFocusEvents still runs and reaches
    // CoreFocusNonLinear(W, W) — CommonAncestor(W,W) is W's parent, so
    // both the Out/In endpoint events fire on W with NotifyNonlinear
    // (the intermediate walks are empty). focus_transition_events
    // returns nothing for from == to, so synthesize that pair here.
    // (Grab windows are always real windows, so the None/PointerRoot
    // same-window branch is unreachable from our call sites.)
    let events = if from_raw == to_raw && (mode == 1 || mode == 2) && to_raw > 1 {
        vec![
            crate::crossings::FocusEvent {
                window: ResourceId(to_raw),
                focus_in: false,
                detail: crate::crossings::NOTIFY_NONLINEAR,
            },
            crate::crossings::FocusEvent {
                window: ResourceId(to_raw),
                focus_in: true,
                detail: crate::crossings::NOTIFY_NONLINEAR,
            },
        ]
    } else {
        crate::crossings::focus_transition_events(state, from_raw, to_raw, pointer_win)
    };
    let (ptr_x, ptr_y) = state.pointer_root;
    for e in events {
        let _dropped =
            emit_window_event_to_state(state, e.window, FOCUS_CHANGE_MASK, |buf, seq, order| {
                x11::encode_focus_event_with_mode_detail(
                    buf, seq, order, e.focus_in, e.window, mode, e.detail,
                );
            });
    }
    // Xorg DoFocusEvents: the whole core sequence, then the XI2 one
    // (DeviceFocusEvents), which has its own windows.
    for e in crate::crossings::device_focus_transition_events(state, from_raw, to_raw, pointer_win)
    {
        let evtype = if e.focus_in { 9 } else { 10 };
        let _dropped = emit_xi2_focus_event_to_state(
            state,
            e.window,
            evtype,
            XI2_MAJOR_OPCODE,
            mode,
            e.detail,
            ptr_x,
            ptr_y,
        );
    }
}

pub(super) fn handle_query_pointer(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    let queried_window = if body.len() >= 4 {
        ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]))
    } else {
        ROOT_WINDOW
    };
    let pointer = backend.query_pointer(origin).ok().filter(|p| p.same_screen);
    let reply_data = if let Some(pointer) = pointer {
        // The KMS backend's PointerPosition carries cursor coordinates
        // in root-absolute screen space (win_x/win_y are misnamed
        // historical fields). Compute win-relative by subtracting the
        // queried window's absolute origin. Without this xeyes (and
        // any other client doing per-window pointer queries) sees
        // win_x/win_y unchanged after the WM moves the window, so
        // the iris locks to a fixed offset and stops tracking.
        let (origin_x, origin_y) = state.resources.window_absolute_position(queried_window);
        let root_x = pointer.win_x;
        let root_y = pointer.win_y;
        let win_x = i16::try_from(i32::from(root_x).saturating_sub(origin_x)).unwrap_or(i16::MAX);
        let win_y = i16::try_from(i32::from(root_y).saturating_sub(origin_y)).unwrap_or(i16::MAX);
        // Per X11 spec, `child` is the direct child of `queried_window`
        // that contains the pointer, or None (0) if the pointer is on
        // the queried window itself or on no descendant. Previously
        // this was hardcoded to ROOT_WINDOW, which made GTK's
        // QueryPointer-based hover/tooltip polling never see a
        // transition between children — xfce4-panel spun 3500 times
        // per second on QueryPointer, starving its event loop and
        // dropping clicks on every panel applet.
        let child = state
            .direct_child_at(queried_window, win_x, win_y)
            .unwrap_or(ResourceId(0));
        x11::QueryPointerReply {
            root: ROOT_WINDOW,
            child,
            root_x,
            root_y,
            win_x,
            win_y,
            mask: pointer.mask,
        }
    } else {
        x11::QueryPointerReply {
            root: ROOT_WINDOW,
            child: ResourceId(0),
            ..Default::default()
        }
    };
    debug!(
        "client {} #{} QueryPointer queried=0x{:x} -> root=({},{}) win=({},{}) child=0x{:x} mask=0x{:x}",
        client_id.0,
        sequence.0,
        queried_window.0,
        reply_data.root_x,
        reply_data.root_y,
        reply_data.win_x,
        reply_data.win_y,
        reply_data.child.0,
        reply_data.mask,
    );
    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return Ok(RequestOutcome::Handled);
    };
    let byte_order = client.byte_order;
    let mut buf: Vec<u8> = Vec::with_capacity(32);
    x11::write_query_pointer_reply(&mut buf, byte_order, sequence, reply_data)?;
    Ok(write_to_client(client, client_id, &buf))
}

pub(super) fn handle_warp_pointer(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    if body.len() >= 20 {
        let rd16 = |o: usize| i16::from_le_bytes([body[o], body[o + 1]]);
        let rdu16 = |o: usize| u16::from_le_bytes([body[o], body[o + 1]]);
        let req = WarpRequest {
            src: ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]])),
            dst: ResourceId(u32::from_le_bytes([body[4], body[5], body[6], body[7]])),
            src_x: i32::from(rd16(8)),
            src_y: i32::from(rd16(10)),
            src_w: rdu16(12),
            src_h: rdu16(14),
            dst_x: i32::from(rd16(16)),
            dst_y: i32::from(rd16(18)),
        };
        if let Some(bad) = warp_pointer_bad_window(state, &req) {
            return emit_x11_error(state, client_id, sequence, x11::error::BAD_WINDOW, bad, 41);
        }
        if !warp_pointer_src_allows(state, &req, WarpSrcRule::Core) {
            debug!(
                "client {} #{} WarpPointer no-op (pointer outside src rect)",
                client_id.0, sequence.0
            );
            return Ok(RequestOutcome::Handled);
        }
        apply_pointer_warp(state, backend, origin, &req);
    }
    debug!("client {} #{} WarpPointer", client_id.0, sequence.0);
    Ok(RequestOutcome::Handled)
}

/// Which of Xorg's two source-rectangle tests a warp uses. Xorg keeps
/// two copies that differ in one term (see [`warp_pointer_src_allows`]).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum WarpSrcRule {
    /// `ProcWarpPointer` (dix/events.c).
    Core,
    /// `ProcXIWarpPointer` (Xi/xiwarppointer.c).
    XInput2,
}

/// A decoded core `WarpPointer` or `XIWarpPointer`. The XI FP16.16
/// coordinates are already converted to whole pixels the way Xorg does
/// it (`int src_x = stuff->src_x / (double)(1 << 16)`, which truncates
/// toward zero).
pub(super) struct WarpRequest {
    src: ResourceId,
    dst: ResourceId,
    src_x: i32,
    src_y: i32,
    src_w: u16,
    src_h: u16,
    dst_x: i32,
    dst_y: i32,
}

/// The first nonexistent window of a warp, in Xorg's lookup order: the
/// destination is looked up before the source in both ProcWarpPointer
/// and ProcXIWarpPointer.
pub(super) fn warp_pointer_bad_window(state: &ServerState, req: &WarpRequest) -> Option<u32> {
    [req.dst, req.src]
        .into_iter()
        .find(|w| w.0 != 0 && state.resources.window(*w).is_none())
        .map(|w| w.0)
}

/// Whether the pointer is inside the warp's source rectangle, so the
/// warp goes ahead (always true without a source window).
///
/// Ported term for term, including the XI copy's slip: its right-edge
/// test compares against 0 instead of the pointer x
/// (`winX + src_x + src_width < 0`, Xi/xiwarppointer.c), so an XI warp
/// ignores the right edge of the rectangle — only the window's own
/// extent (via the visibility test) bounds it. Both copies treat the
/// right/bottom edges as inclusive, and a zero width/height as "to the
/// window's edge". The core copy skips the visibility test for the root
/// window (`source->parent &&`); the XI copy always runs it.
pub(super) fn warp_pointer_src_allows(
    state: &ServerState,
    req: &WarpRequest,
    rule: WarpSrcRule,
) -> bool {
    if req.src.0 == 0 {
        return true;
    }
    let (x, y) = (
        i32::from(state.pointer_root.0),
        i32::from(state.pointer_root.1),
    );
    let (win_x, win_y) = state.resources.window_absolute_position(req.src);
    let left = win_x + req.src_x;
    let top = win_y + req.src_y;
    let right_limit = match rule {
        WarpSrcRule::Core => x,
        WarpSrcRule::XInput2 => 0,
    };
    let outside = x < left
        || y < top
        || (req.src_w != 0 && left + i32::from(req.src_w) < right_limit)
        || (req.src_h != 0 && top + i32::from(req.src_h) < y);
    if outside {
        return false;
    }
    let check_visibility = rule == WarpSrcRule::XInput2 || req.src != ROOT_WINDOW;
    !check_visibility || point_in_window_is_visible(state, req.src, x, y)
}

/// Xorg `PointInWindowIsVisible` (dix/window.c): the window is viewable
/// and the root point lies in its visible border-inclusive region and
/// input shape. The pointer hit test resolves exactly that — the point is
/// in `window`'s region iff the deepest window under it is `window` or
/// one of its inferiors.
fn point_in_window_is_visible(state: &ServerState, window: ResourceId, x: i32, y: i32) -> bool {
    if window == ROOT_WINDOW {
        return true;
    }
    let viewable = state
        .resources
        .window(window)
        .is_some_and(|w| w.map_state == crate::resources::MapState::Viewable);
    if !viewable {
        return false;
    }
    let (Ok(x), Ok(y)) = (i16::try_from(x), i16::try_from(y)) else {
        return false;
    };
    state
        .root_pointer_target_at(x, y)
        .is_some_and(|(hit, _, _)| {
            hit == window || crate::core_loop::xi1_focus::is_ancestor(state, window, hit)
        })
}

/// Move the sprite for a validated warp: relative to the destination
/// window's origin, or to the current position when there is none,
/// clamped to the screen like ProcWarpPointer/ProcXIWarpPointer, then
/// through the backend's absolute-motion path (which generates the
/// crossing and motion events a warp is specified to produce). Warps
/// bypass pointer barriers.
pub(super) fn apply_pointer_warp(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    req: &WarpRequest,
) {
    let (base_x, base_y) = if req.dst.0 == 0 {
        (
            i32::from(state.pointer_root.0),
            i32::from(state.pointer_root.1),
        )
    } else {
        state.resources.window_absolute_position(req.dst)
    };
    let (root_w, root_h) = state
        .resources
        .window(ROOT_WINDOW)
        .map_or((1, 1), |r| (i32::from(r.width), i32::from(r.height)));
    let x = base_x
        .saturating_add(req.dst_x)
        .clamp(0, (root_w - 1).max(0));
    let y = base_y
        .saturating_add(req.dst_y)
        .clamp(0, (root_h - 1).max(0));
    if req.dst.0 != 0 {
        // Proxy backends warp the host pointer relative to the host
        // window (a no-op on KMS, which only uses `warp_pointer_root`).
        let to_i16 = |v: i32| i16::try_from(v).unwrap_or(if v < 0 { i16::MIN } else { i16::MAX });
        if let Some(target) = state.resources.host_drawable_target(req.dst) {
            let _ = backend.warp_pointer(
                origin,
                target.host_xid(),
                to_i16(req.dst_x),
                to_i16(req.dst_y),
            );
        }
    }
    let prev = state.barrier_bypass;
    state.barrier_bypass = true;
    backend.warp_pointer_root(state, x, y);
    state.barrier_bypass = prev;
}
