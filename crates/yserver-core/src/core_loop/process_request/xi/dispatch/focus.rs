use crate::{
    access::{AccessMode, RequestOpcodes, ResourceKind},
    backend::{Backend, OriginContext},
    core_loop::process_request::{
        RequestOutcome, XI2_MAJOR_OPCODE, emit_x11_error_with_minor,
        xi::{
            XI1_ERROR_BAD_DEVICE, dispatch::xi_write_reply, xi1_device_has_keys, xi1_device_valid,
            xi1_error,
        },
    },
    resources::ROOT_WINDOW,
    server::ServerState,
};
use log::debug;
use std::io;
use yserver_protocol::{
    x11,
    x11::{ClientByteOrder, ClientId, ResourceId, SequenceNumber},
};

#[allow(clippy::needless_return)]
pub(super) fn handle_xi_change_cursor(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    access_request: RequestOpcodes,
) -> io::Result<RequestOutcome> {
    // XIChangeCursor (XI2 opcode 42). Wire body after the
    // request header: window(4), cursor(4), deviceid(2), pad(2).
    // GTK4 (gnome-text-editor, modern GTK apps) sets per-widget
    // cursors via this rather than core `XDefineCursor`, so
    // pre-fix the I-beam never reached the backend and text
    // areas kept showing the default arrow. Treat it as
    // `XDefineCursor` for now — yserver doesn't yet route
    // per-device cursors, but a single effective cursor on the
    // window matches what XInput2 produces for AllMasterDevices
    // and is what GTK relies on in practice. `cursor = None`
    // (xid 0) means "clear" — same semantics as the CWA cursor
    // path, see Xorg `dix/window.c:1487-1491`.
    if body.len() < 8 {
        debug!(
            "client {} #{} XIChangeCursor (body too short: {})",
            client_id.0,
            sequence.0,
            body.len()
        );
        return Ok(RequestOutcome::Handled);
    }
    let window = ResourceId(u32::from_le_bytes(body[0..4].try_into().expect("4 bytes")));
    let cursor = ResourceId(u32::from_le_bytes(body[4..8].try_into().expect("4 bytes")));
    for (id, kind, mode) in [
        (window, ResourceKind::Window, AccessMode::SET_ATTR),
        (cursor, ResourceKind::Cursor, AccessMode::USE),
    ] {
        crate::core_loop::process_request::return_on_access_denial!(
            crate::core_loop::process_request::validate_resource_if_present(
                state,
                client_id,
                id,
                kind,
                mode,
                access_request,
            ),
            state,
            client_id,
            sequence,
            access_request,
        );
    }
    let host_window_raw = if window == ROOT_WINDOW {
        Some(backend.window_id())
    } else {
        state
            .resources
            .window(window)
            .and_then(|w| w.host_xid)
            .map(|h| h.as_raw())
    };
    let cursor_host_xid = if cursor.0 == 0 {
        Some(0u32)
    } else {
        state.resources.cursor_host_xid(cursor)
    };
    debug!(
        "client {} #{} XIChangeCursor window=0x{:x} cursor=0x{:x}",
        client_id.0, sequence.0, window.0, cursor.0
    );
    if let (Some(hw), Some(ch)) = (host_window_raw, cursor_host_xid) {
        let _ = backend.define_cursor(origin, hw, ch);
    }
    return Ok(RequestOutcome::Handled);
}

pub(super) fn handle_xi_query_pointer(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    byte_order: ClientByteOrder,
    access_request: RequestOpcodes,
) -> io::Result<RequestOutcome> {
    let mut buf: Vec<u8> = Vec::with_capacity(64);
    {
        // XIQueryPointer: GDK calls this to fetch the current
        // pointer position for gesture-drag anchors (caja-desktop
        // marquee, mate-panel applet drags). Previously hardcoded
        // every coordinate to 0, so every drag started from (0,0)
        // regardless of the actual cursor position — caja then
        // rubber-banded from screen origin to the click point on
        // any single click. Wire the real cursor position through
        // from the backend.
        // XIQueryPointer body layout (per xinput.xml):
        //   bytes [0..4] window (WINDOW)
        //   bytes [4..6] deviceid (u16)
        //   bytes [6..8] pad
        // Previously this read body[4..8], which is deviceid + pad
        // — i.e. always interpreted as some sentinel xid (e.g.
        // `0xfd820002`, `0xffff0002`). Then
        // `window_absolute_position` couldn't find the bogus xid
        // and returned `(0, 0)`, so the win-relative coords came
        // out equal to the root-absolute coords. GTK popups in
        // file managers (Thunar, Caja) place themselves at
        // `(window_origin + win_xy + menu_offset)` — with our
        // wrong reply they ended up offset by the window's own
        // origin, and rubber-band drags anchored at root (0,0)
        // instead of at the click point.
        let queried_window = if body.len() >= 4 {
            ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]))
        } else {
            ROOT_WINDOW
        };
        crate::core_loop::process_request::return_on_access_denial!(
            crate::core_loop::process_request::validate_resource_if_present(
                state,
                client_id,
                queried_window,
                ResourceKind::Window,
                AccessMode::GET_ATTR,
                access_request,
            ),
            state,
            client_id,
            sequence,
            access_request,
        );
        let pointer = backend.query_pointer(origin).ok();
        let (root_x_int, root_y_int, mask, same_screen) =
            if let Some(p) = pointer.filter(|p| p.same_screen) {
                (p.win_x, p.win_y, p.mask, 1u8)
            } else {
                (0, 0, 0, 0u8)
            };
        let (origin_x, origin_y) = state.resources.window_absolute_position(queried_window);
        let win_x_int =
            i16::try_from(i32::from(root_x_int).saturating_sub(origin_x)).unwrap_or(i16::MAX);
        let win_y_int =
            i16::try_from(i32::from(root_y_int).saturating_sub(origin_y)).unwrap_or(i16::MAX);
        let child = state
            .direct_child_at(queried_window, win_x_int, win_y_int)
            .unwrap_or(ResourceId(0));
        debug!(
            "client {} #{} XIQueryPointer window=0x{:x} -> root=({},{}) win=({},{}) child=0x{:x} mask=0x{:x}",
            client_id.0,
            sequence.0,
            queried_window.0,
            root_x_int,
            root_y_int,
            win_x_int,
            win_y_int,
            child.0,
            mask,
        );
        // Reply layout (length = 6 units = 24 bytes after the
        // 8-byte header — total 32 + 24 = 56). FP1616 coords are
        // (i32::from(coord) << 16) as u32. Per xinput.xml the
        // BOOL `same_screen` is at offset 32 (NOT in the `pad0`
        // slot at offset 1 — confirmed via x11trace 2026-05-15).
        // GTK uses `same_screen` to pick popup-placement code
        // paths; encoding it as pad0 left every client reading
        // `same_screen=false` and routing through the "different
        // screen" branch, which made Thunar's right-click popup
        // appear at root+window_origin instead of near the click.
        // XI2 button state: fold the KeyButMask button bits into the
        // XI2 button bitmask (button N -> bit N). muffin's
        // `_NET_WM_MOVERESIZE` move grab queries the pointer to learn
        // whether the initiating button is still held — an empty mask
        // makes it abort the move and the window never budges.
        let mut button_mask: u32 = 0;
        for (keybut_bit, button) in [
            (0x0100u16, 1u32),
            (0x0200, 2),
            (0x0400, 3),
            (0x0800, 4),
            (0x1000, 5),
        ] {
            if mask & keybut_bit != 0 {
                button_mask |= 1 << button;
            }
        }
        // length 7 units: 6 coord words + the same_screen/buttons_len/
        // ModifierInfo/GroupInfo block + a 1-unit button bitmask.
        let mut reply = x11::fixed_reply(byte_order, sequence, 0, 7);
        x11::write_u32(byte_order, &mut reply, ROOT_WINDOW.0);
        x11::write_u32(byte_order, &mut reply, child.0);
        x11::write_u32(byte_order, &mut reply, (i32::from(root_x_int) << 16) as u32);
        x11::write_u32(byte_order, &mut reply, (i32::from(root_y_int) << 16) as u32);
        x11::write_u32(byte_order, &mut reply, (i32::from(win_x_int) << 16) as u32);
        x11::write_u32(byte_order, &mut reply, (i32::from(win_y_int) << 16) as u32);
        reply.push(same_screen); // byte 32: same_screen (BOOL)
        reply.push(0); // byte 33: pad
        x11::write_u16(byte_order, &mut reply, 1); // bytes 34-35: buttons_len (1 unit)
        // ModifierInfo: 4× CARD32 = base / latched / locked /
        // effective. `mask` carries the X11 KeyButMask snapshot
        // from the backend: MODIFIERS in the low byte (Shift=0x1 …
        // Mod5=0x80), buttons at 0x100+. Xorg fills `rep.mods`
        // from the paired MASTER_KEYBOARD's XKB state
        // (Xi/xiquerypointer.c:120,139) — base_mods must carry the
        // modifier bits WITHOUT the button bits. Pre-fix base was
        // hardcoded 0: cinnamon's alt-tab switcher polls
        // `global.get_pointer()` (GDK → XIQueryPointer) right
        // after pushModal to check Alt is still held; reading 0 it
        // took the modifier-already-released branch
        // (_activateSelected + destroy) and the switcher popup
        // never appeared. `effective` keeps the full KeyButMask
        // (mods + button bits) — GDK reads effective and the
        // button bits are harmlessly idempotent with the XI2
        // buttons array below.
        let mod_bits = u32::from(mask & 0x00ff);
        x11::write_u32(byte_order, &mut reply, mod_bits); // base_mods
        x11::write_u32(byte_order, &mut reply, 0); // latched_mods
        x11::write_u32(byte_order, &mut reply, 0); // locked_mods
        x11::write_u32(byte_order, &mut reply, u32::from(mask)); // effective_mods
        reply.extend_from_slice(&[0u8; 4]); // group info
        // XI2 button mask is a raw byte array indexed by
        // `XIMaskIsSet(ptr, btn) = ptr[btn>>3] & (1 << (btn & 7))`
        // (X11/extensions/XI2.h) — byte[0] holds buttons 0-7. It is
        // NOT byte-swapped per client order (Xorg leaves the trailing
        // mask bytes untouched in its reply swap), so emit
        // little-endian unconditionally.
        reply.extend_from_slice(&button_mask.to_le_bytes());
        buf.extend_from_slice(&reply);
    }
    xi_write_reply(state, client_id, buf)
}

// GetDeviceFocus: { deviceid }. Reply layout
// (xGetDeviceFocusReply, XIproto.h:709-721): focus CARD32 @8,
// time CARD32 @12, revertTo CARD8 @16, pad to 32. The focus
// field carries the raw stored value — None(0)/PointerRoot(1)/
// FollowKeyboard(3) sentinels included (Xorg Xi/getfocus.c
// maps focus->win back to the sentinel, never resolving it).
pub(super) fn handle_xi1_get_device_focus(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    byte_order: ClientByteOrder,
    minor: u8,
) -> io::Result<RequestOutcome> {
    let mut buf: Vec<u8> = Vec::with_capacity(64);
    {
        let dev = u16::from(*body.first().unwrap_or(&0));
        if !xi1_device_valid(&state.xi_devices, dev) {
            return xi1_error(
                state,
                client_id,
                sequence,
                XI1_ERROR_BAD_DEVICE,
                u32::from(dev),
                minor,
            );
        }
        // NOTE: Xorg's `if (!dev->focus) return BadDevice` is NOT
        // mirrored as a keyboard-only gate: real servers initialise
        // a focus class on extension pointers too (XTS
        // Miscellaneous-3..6 call SetDeviceFocus on Devs.Button /
        // Devs.DvMod and expect Success).
        let f = crate::core_loop::xi1_focus::device_focus(state, dev);
        // Byte 1 = RepType = X_GetDeviceFocus (Xi/getfocus.c).
        let mut reply = x11::fixed_reply(byte_order, sequence, minor, 0);
        x11::write_u32(byte_order, &mut reply, f.focus); // bytes 8-11
        x11::write_u32(byte_order, &mut reply, f.time); // bytes 12-15
        reply.push(f.revert_to); // byte 16
        reply.extend_from_slice(&[0u8; 15]); // bytes 17-31: pad
        debug!(
            "client {} #{} XI1 GetDeviceFocus device={dev} -> focus=0x{:x} revert={}",
            client_id.0, sequence.0, f.focus, f.revert_to
        );
        buf.extend_from_slice(&reply);
    }
    xi_write_reply(state, client_id, buf)
}

// SetDeviceFocus (void): { focus, time, revertTo, device }.
// focus: None(0) / PointerRoot(1) / FollowKeyboard(3) / a valid
// viewable window. revertTo: RevertToNone(0)..FollowKeyboard(3).
// Mirrors Xorg Xi/setfocus.c → dix SetInputFocus
// (dix/events.c:4879): validate, then silently ignore stale /
// future timestamps, then store + emit DeviceFocusIn/Out.
pub(super) fn handle_xi1_set_device_focus(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    minor: u8,
    access_request: RequestOpcodes,
) -> io::Result<RequestOutcome> {
    {
        if body.len() < 10 {
            return Ok(RequestOutcome::Handled);
        }
        let focus = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
        let req_time = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
        let revert_to = body[8];
        let dev = u16::from(body[9]);
        if !xi1_device_valid(&state.xi_devices, dev) {
            return xi1_error(
                state,
                client_id,
                sequence,
                XI1_ERROR_BAD_DEVICE,
                u32::from(dev),
                minor,
            );
        }
        if revert_to > 3 {
            return xi1_error(
                state,
                client_id,
                sequence,
                x11::error::BAD_VALUE,
                u32::from(revert_to),
                minor,
            );
        }
        if !matches!(focus, 0 | 1 | 3) {
            let Some(window) = state.resources.window(ResourceId(focus)) else {
                return xi1_error(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_WINDOW,
                    focus,
                    minor,
                );
            };
            if window.map_state != crate::resources::MapState::Viewable {
                return xi1_error(state, client_id, sequence, x11::error::BAD_MATCH, 0, minor);
            }
        }
        if !matches!(focus, 0 | 1 | 3) {
            crate::core_loop::process_request::return_on_access_denial!(
                crate::core_loop::process_request::validate_resource_if_present(
                    state,
                    client_id,
                    ResourceId(focus),
                    ResourceKind::Window,
                    AccessMode::SET_ATTR,
                    access_request,
                ),
                state,
                client_id,
                sequence,
                access_request,
            );
        }
        // Timestamp gate (dix/events.c:4920-4922): CurrentTime(0)
        // maps to the server clock; a time later than "now" or
        // earlier than the last-focus-change time is a silent no-op.
        let now = state.timestamp_now();
        let time = if req_time == 0 { now } else { req_time };
        let prev = crate::core_loop::xi1_focus::device_focus(state, dev);
        if crate::core_loop::xi1_focus::time_after(time, now)
            || crate::core_loop::xi1_focus::time_after(prev.time, time)
        {
            debug!(
                "client {} #{} XI1 SetDeviceFocus device={dev} stale time {time} \
                     (now={now} last={}) — ignored",
                client_id.0, sequence.0, prev.time
            );
            return Ok(RequestOutcome::Handled);
        }
        crate::core_loop::xi1_focus::set_device_focus(state, dev, focus, revert_to, time);
        debug!(
            "client {} #{} XI1 SetDeviceFocus device={dev} focus=0x{focus:x} \
                 revert={revert_to} time={time}",
            client_id.0, sequence.0
        );
    }
    xi_write_reply(state, client_id, Vec::new())
}

pub(super) fn handle_xi_get_focus(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    byte_order: ClientByteOrder,
    minor: u8,
) -> io::Result<RequestOutcome> {
    let mut buf: Vec<u8> = Vec::with_capacity(64);
    {
        // XIGetFocus { deviceid:CARD16 } -> xXIGetFocusReply
        // { focus:WINDOW @8, 20 bytes pad }. Xorg ProcXIGetFocus
        // (Xi/xisetdevfocus.c) reports the device's raw focus (None=0 /
        // PointerRoot=1 / FollowKeyboard=3 / window) without resolving
        // the sentinels. Only keyboards have a focus class; pointers
        // and unknown ids are BadDevice with no errorValue. The master
        // keyboard's focus is the core focus; the slave keyboard's is
        // its own (shared with XI1 GetDeviceFocus).
        let dev = if body.len() >= 2 {
            u16::from_le_bytes([body[0], body[1]])
        } else {
            0
        };
        if !xi1_device_has_keys(&state.xi_devices, dev) {
            return xi1_error(state, client_id, sequence, XI1_ERROR_BAD_DEVICE, 0, minor);
        }
        let focus =
            if state.xi_devices.role(dev) == Some(crate::xinput::XiDeviceRole::MasterKeyboard) {
                state.core_focus.raw
            } else {
                crate::core_loop::xi1_focus::device_focus(state, dev).focus
            };
        let mut reply = x11::fixed_reply(byte_order, sequence, 0, 0);
        x11::write_u32(byte_order, &mut reply, focus); // bytes 8-11
        reply.extend_from_slice(&[0u8; 20]); // bytes 12-31 pad
        debug!(
            "client {} #{} XIGetFocus device={dev} -> focus=0x{focus:x}",
            client_id.0, sequence.0
        );
        buf.extend_from_slice(&reply);
    }
    xi_write_reply(state, client_id, buf)
}

pub(super) fn handle_xi_barrier_release_pointer(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    access_request: RequestOpcodes,
) -> io::Result<RequestOutcome> {
    {
        if let Some(entries) = x11::parse_xi_barrier_release(body) {
            for (deviceid, barrier_id, event_id) in entries {
                if !(deviceid == 0 || deviceid == 1 || deviceid == 2) {
                    return emit_x11_error_with_minor(
                        state,
                        client_id,
                        sequence,
                        XI1_ERROR_BAD_DEVICE,
                        u32::from(deviceid),
                        61,
                        XI2_MAJOR_OPCODE,
                    );
                }
                if !state.pointer_barriers.contains_key(&barrier_id) {
                    return emit_x11_error_with_minor(
                        state,
                        client_id,
                        sequence,
                        x11::error::BAD_VALUE,
                        barrier_id,
                        61,
                        XI2_MAJOR_OPCODE,
                    );
                }
                crate::core_loop::process_request::return_on_access_denial!(
                    crate::core_loop::process_request::validate_resource_if_present(
                        state,
                        client_id,
                        ResourceId(barrier_id),
                        ResourceKind::PointerBarrier,
                        AccessMode::READ,
                        access_request,
                    ),
                    state,
                    client_id,
                    sequence,
                    access_request,
                );
                let barrier = state
                    .pointer_barriers
                    .get_mut(&barrier_id)
                    .expect("barrier still exists after access check");
                if barrier.owner != client_id {
                    return emit_x11_error_with_minor(
                        state,
                        client_id,
                        sequence,
                        x11::error::BAD_ACCESS,
                        barrier_id,
                        61,
                        XI2_MAJOR_OPCODE,
                    );
                }
                if barrier.event_id == event_id {
                    barrier.release_event_id = event_id;
                }
            }
        }
    }
    xi_write_reply(state, client_id, Vec::new())
}
