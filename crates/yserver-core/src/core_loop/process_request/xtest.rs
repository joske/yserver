use super::*;

fn effective_window_cursor(state: &ServerState, mut window: ResourceId) -> Option<ResourceId> {
    for _ in 0..256 {
        let current = state.resources.window(window)?;
        if let Some(cursor) = current.cursor {
            return (cursor.0 != 0).then_some(cursor);
        }
        if current.parent == window {
            break;
        }
        window = current.parent;
    }
    None
}

fn current_pointer_cursor(state: &ServerState) -> Option<ResourceId> {
    if let Some(grab) = state.active_pointer_grab
        && grab.cursor.0 != 0
    {
        return Some(grab.cursor);
    }
    let pointer_window = crate::core_loop::key_fanout::deepest_window_at_pointer(state);
    effective_window_cursor(state, pointer_window)
}

pub(super) fn handle_xtest_request(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    use yserver_protocol::x11::xtest as x11xtest;
    let minor = header.data;
    match minor {
        x11xtest::GET_VERSION => {
            let (cmaj, cmin) = x11xtest::parse_get_version(body).unwrap_or((
                u8::try_from(x11xtest::MAJOR_VERSION).unwrap_or(2),
                x11xtest::MINOR_VERSION,
            ));
            debug!(
                "client {} #{} XTEST::GetVersion client={cmaj}.{cmin} -> {}.{}",
                client_id.0,
                sequence.0,
                x11xtest::MAJOR_VERSION,
                x11xtest::MINOR_VERSION,
            );
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            let byte_order = client.byte_order;
            let reply = x11xtest::encode_get_version_reply(
                byte_order,
                sequence,
                u8::try_from(x11xtest::MAJOR_VERSION).unwrap_or(2),
                x11xtest::MINOR_VERSION,
            );
            return Ok(write_to_client(client, client_id, &reply));
        }
        x11xtest::COMPARE_CURSOR => {
            let byte_order = state
                .clients
                .get(&client_id.0)
                .map_or(x11::ClientByteOrder::LittleEndian, |client| {
                    client.byte_order
                });
            let read_u32 = |slice: &[u8]| match byte_order {
                x11::ClientByteOrder::LittleEndian => {
                    u32::from_le_bytes(slice.try_into().expect("four bytes"))
                }
                x11::ClientByteOrder::BigEndian => {
                    u32::from_be_bytes(slice.try_into().expect("four bytes"))
                }
            };
            let window = ResourceId(read_u32(&body[0..4]));
            let cursor = ResourceId(read_u32(&body[4..8]));
            if state.resources.window(window).is_none() {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_WINDOW,
                    window.0,
                    u16::from(minor),
                    header.opcode,
                );
            }
            let comparison = if cursor.0 == 0 {
                None
            } else if cursor.0 == 1 {
                current_pointer_cursor(state)
            } else if state.resources.cursor_exists(cursor) {
                Some(cursor)
            } else {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_CURSOR,
                    cursor.0,
                    u16::from(minor),
                    header.opcode,
                );
            };
            // Xorg compares cursor objects: after XFIXES ChangeCursor two
            // XIDs can name the same cursor, so compare their host handles.
            let effective = effective_window_cursor(state, window);
            let same = effective == comparison
                || matches!((effective, comparison), (Some(a), Some(b))
                    if state.resources.cursor_host_xid(a).is_some()
                        && state.resources.cursor_host_xid(a)
                            == state.resources.cursor_host_xid(b));
            debug!(
                "client {} #{} XTEST::CompareCursor window=0x{:x} cursor=0x{:x} same={same}",
                client_id.0, sequence.0, window.0, cursor.0,
            );
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            let reply = x11xtest::encode_compare_cursor_reply(byte_order, sequence, same);
            return Ok(write_to_client(client, client_id, &reply));
        }
        x11xtest::FAKE_INPUT => {
            let Some(fi) = x11xtest::parse_fake_input(body) else {
                debug!(
                    "client {} #{} XTEST::FakeInput body too short ({} bytes), dropping",
                    client_id.0,
                    sequence.0,
                    body.len()
                );
                return Ok(RequestOutcome::Handled);
            };
            debug!(
                "client {} #{} XTEST::FakeInput type={} detail={} root_xy=({},{})",
                client_id.0, sequence.0, fi.event_type, fi.detail, fi.root_x, fi.root_y
            );
            return dispatch_fake_input_with_body(
                state, backend, client_id, sequence, header, fi, body,
            );
        }
        x11xtest::GRAB_CONTROL => {
            debug!(
                "client {} #{} XTEST::GrabControl (no-op)",
                client_id.0, sequence.0
            );
        }
        other => {
            return emit_x11_error_with_minor(
                state,
                client_id,
                sequence,
                x11::error::BAD_REQUEST,
                0,
                u16::from(other),
                header.opcode,
            );
        }
    }
    Ok(RequestOutcome::Handled)
}

/// `body` is the FakeInput request body (the 32-byte first xEvent plus
/// any follow-up events) — needed for the XI 1.x *device* fakes, where
/// libXtst packs the deviceid into byte 31 of the first event and
/// device-motion axes into trailing deviceValuator events (Xorg
/// Xext/xtest.c ProcXTestFakeInput).
#[allow(clippy::too_many_lines)]
fn dispatch_fake_input_with_body(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    fi: yserver_protocol::x11::xtest::FakeInput,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    use crate::{core_loop::HostInputEvent, host_x11::HostKeyEvent};
    use yserver_protocol::x11::xtest as x11xtest;

    let xi_first = crate::server::XI_FIRST_EVENT;
    let event_type = fi.event_type & 0x7f;
    let xi_device_event = event_type >= xi_first;
    let xi_offset = if xi_device_event {
        Some(event_type - xi_first)
    } else {
        None
    };
    let target_device_id = if xi_device_event {
        // ProcXTestFakeInput first validates that the request contains full
        // xEvents, then masks the device id's MORE_EVENTS bit and looks it up
        // (Xext/xtest.c:170-186).
        if body.len() < 32 || !body.len().is_multiple_of(32) {
            return emit_x11_error_with_minor(
                state,
                client_id,
                sequence,
                x11::error::BAD_LENGTH,
                0,
                u16::from(header.data),
                header.opcode,
            );
        }
        let Some(device_id) = fake_input_device_id(body) else {
            return emit_x11_error_with_minor(
                state,
                client_id,
                sequence,
                x11::error::BAD_LENGTH,
                0,
                u16::from(header.data),
                header.opcode,
            );
        };
        if state.xi_devices.device(device_id).is_none() {
            // XI_BadDevice is the XInput extension's first error.
            return emit_x11_error_with_minor(
                state,
                client_id,
                sequence,
                XI2_FIRST_ERROR,
                u32::from(device_id),
                u16::from(header.data),
                header.opcode,
            );
        }

        // Check the event's device class before the DeviceMotion valuator
        // pairing, as Xorg does at xtest.c:189-226. Class failures carry the
        // original event type in errorValue.
        let class_ok = match xi_offset {
            Some(crate::xinput::XI_DEVICE_KEY_PRESS_OFFSET)
            | Some(crate::xinput::XI_DEVICE_KEY_RELEASE_OFFSET) => {
                xi1_device_has_keys(&state.xi_devices, device_id)
            }
            Some(crate::xinput::XI_DEVICE_BUTTON_PRESS_OFFSET)
            | Some(crate::xinput::XI_DEVICE_BUTTON_RELEASE_OFFSET) => {
                xi1_device_has_buttons(&state.xi_devices, device_id)
            }
            Some(crate::xinput::XI_DEVICE_MOTION_NOTIFY_OFFSET) => {
                xi1_device_has_valuators(&state.xi_devices, device_id)
            }
            _ => false,
        };
        if !class_ok {
            return emit_x11_error_with_minor(
                state,
                client_id,
                sequence,
                x11::error::BAD_VALUE,
                u32::from(fi.event_type),
                u16::from(header.data),
                header.opcode,
            );
        }
        if xi_offset == Some(crate::xinput::XI_DEVICE_MOTION_NOTIFY_OFFSET) && body.len() == 32 {
            return emit_x11_error_with_minor(
                state,
                client_id,
                sequence,
                x11::error::BAD_LENGTH,
                0,
                u16::from(header.data),
                header.opcode,
            );
        }

        match xi_offset {
            Some(crate::xinput::XI_DEVICE_KEY_PRESS_OFFSET)
            | Some(crate::xinput::XI_DEVICE_KEY_RELEASE_OFFSET)
                if !fake_input_keycode_valid(fi.detail) =>
            {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_VALUE,
                    u32::from(fi.detail),
                    u16::from(header.data),
                    header.opcode,
                );
            }
            Some(crate::xinput::XI_DEVICE_BUTTON_PRESS_OFFSET)
            | Some(crate::xinput::XI_DEVICE_BUTTON_RELEASE_OFFSET)
                if !fake_input_button_valid(&state.xi_devices, device_id, fi.detail) =>
            {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_VALUE,
                    u32::from(fi.detail),
                    u16::from(header.data),
                    header.opcode,
                );
            }
            _ => {}
        }
        device_id
    } else {
        // Core FakeInput selects the XTEST keyboard or pointer from the event
        // type before checking its class/detail (xtest.c:282-326, 357-410).
        let device_id = match event_type {
            x11xtest::FAKE_KEY_PRESS | x11xtest::FAKE_KEY_RELEASE => {
                crate::xinput::DEVICEID_XTEST_KEYBOARD
            }
            x11xtest::FAKE_BUTTON_PRESS
            | x11xtest::FAKE_BUTTON_RELEASE
            | x11xtest::FAKE_MOTION_NOTIFY => crate::xinput::DEVICEID_XTEST_POINTER,
            _ => {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_VALUE,
                    u32::from(fi.event_type),
                    u16::from(header.data),
                    header.opcode,
                );
            }
        };
        let class_ok = match event_type {
            x11xtest::FAKE_KEY_PRESS | x11xtest::FAKE_KEY_RELEASE => {
                xi1_device_has_keys(&state.xi_devices, device_id)
            }
            x11xtest::FAKE_BUTTON_PRESS | x11xtest::FAKE_BUTTON_RELEASE => {
                xi1_device_has_buttons(&state.xi_devices, device_id)
            }
            x11xtest::FAKE_MOTION_NOTIFY => xi1_device_has_valuators(&state.xi_devices, device_id),
            _ => false,
        };
        if !class_ok {
            return emit_x11_error_with_minor(
                state,
                client_id,
                sequence,
                XI2_FIRST_ERROR,
                0,
                u16::from(header.data),
                header.opcode,
            );
        }
        match event_type {
            x11xtest::FAKE_KEY_PRESS | x11xtest::FAKE_KEY_RELEASE
                if !fake_input_keycode_valid(fi.detail) =>
            {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_VALUE,
                    u32::from(fi.detail),
                    u16::from(header.data),
                    header.opcode,
                );
            }
            x11xtest::FAKE_BUTTON_PRESS | x11xtest::FAKE_BUTTON_RELEASE
                if !fake_input_button_valid(&state.xi_devices, device_id, fi.detail) =>
            {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_VALUE,
                    u32::from(fi.detail),
                    u16::from(header.data),
                    header.opcode,
                );
            }
            x11xtest::FAKE_MOTION_NOTIFY if fi.detail != 0 && fi.detail != 1 => {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_VALUE,
                    u32::from(fi.detail),
                    u16::from(header.data),
                    header.opcode,
                );
            }
            _ => {}
        }
        device_id
    };

    let fi = yserver_protocol::x11::xtest::FakeInput { event_type, ..fi };
    let origin = crate::core_loop::InputOrigin::XTest(target_device_id);

    if let Some(offset) = xi_offset {
        match offset {
            crate::xinput::XI_DEVICE_KEY_PRESS_OFFSET
            | crate::xinput::XI_DEVICE_KEY_RELEASE_OFFSET => {
                let pressed = offset == crate::xinput::XI_DEVICE_KEY_PRESS_OFFSET;
                backend.on_host_input(
                    state,
                    HostInputEvent::Key(HostKeyEvent {
                        origin,
                        pressed,
                        keycode: fi.detail,
                        time: fi.time,
                        root_x: 0,
                        root_y: 0,
                        event_x: 0,
                        event_y: 0,
                        state: 0,
                    }),
                );
            }
            crate::xinput::XI_DEVICE_BUTTON_PRESS_OFFSET
            | crate::xinput::XI_DEVICE_BUTTON_RELEASE_OFFSET => {
                let pressed = offset == crate::xinput::XI_DEVICE_BUTTON_PRESS_OFFSET;
                let button = fake_input_button_code(fi.detail)
                    .expect("validated XI device button number has an input code");
                backend.on_host_input(
                    state,
                    HostInputEvent::PointerButton {
                        origin,
                        button,
                        pressed,
                        time: fi.time,
                    },
                );
            }
            crate::xinput::XI_DEVICE_MOTION_NOTIFY_OFFSET => {
                // Device-motion fakes are DEVICE-coordinate space and do
                // NOT move the sprite — Xorg uses POINTER_ABSOLUTE without
                // POINTER_DESKTOP (Xext/xtest.c:265).
                let mut axes = crate::server::Xi1MotionAxes {
                    first: 0,
                    count: 0,
                    values: [0; 6],
                };
                let mut chunk = 32;
                let mut got_first = false;
                while let Some(dv) = body.get(chunk..chunk + 32) {
                    let num = usize::from(dv[6].min(6));
                    if !got_first {
                        axes.first = dv[7];
                        got_first = true;
                    }
                    for i in 0..num {
                        let off = 8 + i * 4;
                        let slot = usize::from(axes.count);
                        if slot >= 6 {
                            break;
                        }
                        axes.values[slot] =
                            i32::from_le_bytes([dv[off], dv[off + 1], dv[off + 2], dv[off + 3]]);
                        #[allow(clippy::cast_possible_truncation)]
                        {
                            axes.count += 1;
                        }
                    }
                    chunk += 32;
                }
                let target = crate::core_loop::key_fanout::deepest_window_at_pointer(state);
                let (ox, oy) = state.resources.window_absolute_position(target);
                let (root_x, root_y) = state.pointer_root;
                let event_x = i16::try_from(i32::from(root_x) - ox).unwrap_or(0);
                let event_y = i16::try_from(i32::from(root_y) - oy).unwrap_or(0);
                let _ = crate::core_loop::pointer_fanout::xi1_route_device_event(
                    state,
                    crate::server::Xi1QueuedEvent {
                        deviceid: target_device_id,
                        evcode: crate::server::XI_FIRST_EVENT
                            + crate::xinput::XI_DEVICE_MOTION_NOTIFY_OFFSET,
                        detail: 0,
                        time: fi.time,
                        root_x,
                        root_y,
                        event_x,
                        event_y,
                        state_mask: 0,
                        natural_target: target,
                        focus_route: crate::server::Xi1FocusRoute::Walk,
                        axes: (axes.count != 0).then_some(axes),
                        replay_floor: None,
                    },
                    true,
                );
            }
            _ => unreachable!("XI event type was validated before injection"),
        }
        return Ok(RequestOutcome::Handled);
    }

    match fi.event_type {
        x11xtest::FAKE_KEY_PRESS | x11xtest::FAKE_KEY_RELEASE => {
            let pressed = fi.event_type == x11xtest::FAKE_KEY_PRESS;
            backend.on_host_input(
                state,
                HostInputEvent::Key(HostKeyEvent {
                    origin,
                    pressed,
                    keycode: fi.detail,
                    time: fi.time,
                    root_x: fi.root_x,
                    root_y: fi.root_y,
                    event_x: 0,
                    event_y: 0,
                    state: 0,
                }),
            );
        }
        x11xtest::FAKE_BUTTON_PRESS | x11xtest::FAKE_BUTTON_RELEASE => {
            let pressed = fi.event_type == x11xtest::FAKE_BUTTON_PRESS;
            // Xorg's XTEST pointer has 10 buttons (dix/devices.c:655-700);
            // ProcXTestFakeInput validates 1..numButtons and passes the detail
            // unchanged to GetPointerEvents (Xext/xtest.c:401-425). BTN_FORWARD
            // (0x115) maps to X button 10 via libinput's btn_linux2xorg
            // (xf86-input-libinput/src/xf86libinput.c:253-272).
            let linux_code = fake_input_button_code(fi.detail)
                .expect("validated core button number has an input code");
            backend.on_host_input(
                state,
                HostInputEvent::PointerButton {
                    origin,
                    button: linux_code,
                    pressed,
                    time: fi.time,
                },
            );
        }
        x11xtest::FAKE_MOTION_NOTIFY => {
            // detail 0 = absolute, detail 1 = relative (Xorg xtest.c: rootX/
            // rootY become the valuators, POINTER_ABSOLUTE only for 0). A
            // relative fake moves the sprite by the delta from its current
            // position, unaccelerated (XTEST doesn't set POINTER_ACCELERATE);
            // the backend clips to the screen. The delta also rides as the
            // raw relative motion, so XI2 RawMotion reports it.
            let motion = if fi.detail == 0 {
                HostInputEvent::PointerMotion {
                    origin,
                    x: i32::from(fi.root_x),
                    y: i32::from(fi.root_y),
                    time: fi.time,
                    relative: false,
                    dx: 0,
                    dy: 0,
                    motion_delta: None,
                }
            } else {
                let (x, y) = state.pointer_root;
                let (dx, dy) = (i32::from(fi.root_x), i32::from(fi.root_y));
                HostInputEvent::PointerMotion {
                    origin,
                    x: i32::from(x) + dx,
                    y: i32::from(y) + dy,
                    time: fi.time,
                    relative: true,
                    dx,
                    dy,
                    motion_delta: None,
                }
            };
            backend.on_host_input(state, motion);
            // Like WarpPointer: the physical-input tracker must continue
            // from the faked position, or the next real motion jumps back.
            backend.resync_input_position();
        }
        other => {
            unreachable!("core FakeInput event type was validated: {other}");
        }
    }
    Ok(RequestOutcome::Handled)
}

fn fake_input_keycode_valid(keycode: u8) -> bool {
    // QueryDevice's KeyClass is shared by the master, XTEST, and physical
    // keyboard facets: keycodes 8..=255 (xinput/query.rs:259-272).
    keycode >= 8
}

fn fake_input_button_count(devices: &crate::xinput::XiRegistry, device_id: u16) -> Option<u8> {
    if !xi1_device_has_buttons(devices, device_id) {
        return None;
    }
    xi1_device_button_count(devices, device_id)
}

fn fake_input_button_valid(
    devices: &crate::xinput::XiRegistry,
    device_id: u16,
    button: u8,
) -> bool {
    button != 0 && fake_input_button_count(devices, device_id).is_some_and(|count| button <= count)
}

fn fake_input_button_code(button: u8) -> Option<u16> {
    Some(match button {
        1 => 0x110,  // BTN_LEFT
        2 => 0x112,  // BTN_MIDDLE
        3 => 0x111,  // BTN_RIGHT
        4 => 0x180,  // SYNTH_SCROLL_UP
        5 => 0x181,  // SYNTH_SCROLL_DOWN
        6 => 0x182,  // SYNTH_SCROLL_LEFT
        7 => 0x183,  // SYNTH_SCROLL_RIGHT
        8 => 0x113,  // BTN_SIDE
        9 => 0x114,  // BTN_EXTRA
        10 => 0x115, // BTN_FORWARD -> X 10 via btn_linux2xorg (xf86-input-libinput/src/xf86libinput.c:253-272)
        _ => return None,
    })
}

fn fake_input_device_id(body: &[u8]) -> Option<u16> {
    if body.first().copied()? & 0x7f < crate::server::XI_FIRST_EVENT {
        return None;
    }
    // XI1 reserves the top device-byte bit for MORE_EVENTS. Keep a missing or
    // unknown device distinct from a core FakeInput's default 4/5 target.
    body.get(31).map(|device_id| u16::from(device_id & 0x7f))
}
