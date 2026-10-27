//! XInput requests, XI 1.x and XI2 on the one major opcode:
//! `handle_xi2_request` checks the request length and hands each minor to
//! its own `handle_xi1_*` / `handle_xi_*` fn; replies go out through
//! `xi_write_reply`. XIWarpPointer lives in `focus_pointer`, XISetFocus and
//! XIChangeHierarchy in `xi/mod.rs`. The per-minor fns live in:
//!
//! - `devices`: versions (1, 47), device lists (2, 48), open/close/mode
//!   (3–5), keyboard/pointer device change (11, 12), client pointer
//!   (44, 45).
//! - `control`: motion history (10), feedback (22, 23), key/modifier/button
//!   mappings (24–29), device state (30), bell (32), valuators (33), device
//!   controls (34, 35).
//! - `events`: XI1 selection (6, 7), don't-propagate lists (8, 9),
//!   SendExtensionEvent (31), XI2 selection (46, 60).
//! - `grabs`: XI1 grabs and AllowDeviceEvents (13–19), XI2 grabs and
//!   XIAllowEvents (51–55).
//! - `props`: XI1 (36–39) and XI2 (56–59) device properties.
//! - `focus`: device focus (20, 21, 50), XIQueryPointer (40),
//!   XIChangeCursor (42), barrier release (61).

mod control;
mod devices;
mod events;
mod focus;
mod grabs;
mod props;

pub(in crate::core_loop::process_request) use grabs::deactivate_xi2_device_grab;

use crate::{
    backend::{Backend, OriginContext},
    core_loop::process_request::{
        RequestOutcome, XINPUT_LAST_REQUEST, emit_x11_error_with_minor,
        focus_pointer::handle_xi_warp_pointer,
        write_to_client,
        xi::{
            dispatch::{
                control::{
                    handle_xi1_change_device_control, handle_xi1_change_device_key_mapping,
                    handle_xi1_change_feedback_control, handle_xi1_device_bell,
                    handle_xi1_get_device_button_mapping, handle_xi1_get_device_control,
                    handle_xi1_get_device_key_mapping, handle_xi1_get_device_modifier_mapping,
                    handle_xi1_get_device_motion_events, handle_xi1_get_feedback_control,
                    handle_xi1_query_device_state, handle_xi1_set_device_button_mapping,
                    handle_xi1_set_device_modifier_mapping, handle_xi1_set_device_valuators,
                },
                devices::{
                    handle_xi_get_client_pointer, handle_xi_query_device, handle_xi_query_version,
                    handle_xi_set_client_pointer, handle_xi1_change_keyboard_device,
                    handle_xi1_change_pointer_device, handle_xi1_close_device,
                    handle_xi1_get_extension_version, handle_xi1_list_input_devices,
                    handle_xi1_open_device, handle_xi1_set_device_mode,
                },
                events::{
                    handle_xi_get_selected_events, handle_xi_select_events,
                    handle_xi1_change_device_dont_propagate_list,
                    handle_xi1_get_device_dont_propagate_list,
                    handle_xi1_get_selected_extension_events, handle_xi1_select_extension_event,
                    handle_xi1_send_extension_event,
                },
                focus::{
                    handle_xi_barrier_release_pointer, handle_xi_change_cursor,
                    handle_xi_get_focus, handle_xi_query_pointer, handle_xi1_get_device_focus,
                    handle_xi1_set_device_focus,
                },
                grabs::{
                    handle_xi_allow_events, handle_xi_grab_device, handle_xi_passive_grab_device,
                    handle_xi_passive_ungrab_device, handle_xi_ungrab_device,
                    handle_xi1_allow_device_events, handle_xi1_grab_device,
                    handle_xi1_grab_device_button, handle_xi1_grab_device_key,
                    handle_xi1_ungrab_device, handle_xi1_ungrab_device_button,
                    handle_xi1_ungrab_device_key,
                },
                props::{
                    handle_xi_change_property, handle_xi_delete_property, handle_xi_get_property,
                    handle_xi_list_properties, handle_xi1_change_device_property,
                    handle_xi1_delete_device_property, handle_xi1_get_device_property,
                    handle_xi1_list_device_properties,
                },
            },
            handle_xi_change_hierarchy, handle_xi_set_focus, xi2_request_has_complete_dynamic_tail,
        },
    },
    server::ServerState,
};
use std::io;
use yserver_protocol::{
    x11,
    x11::{ClientByteOrder, ClientId, RequestHeader, SequenceNumber},
};

pub(in crate::core_loop::process_request) fn handle_xi2_request(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    let byte_order = state
        .clients
        .get(&client_id.0)
        .map_or(ClientByteOrder::LittleEndian, |c| c.byte_order);
    let minor = header.data;
    // Per-minor length validation mirroring Xorg's `REQUEST_SIZE_MATCH` /
    // `REQUEST_AT_LEAST_SIZE` macros invoked at the top of each `ProcX*`
    // handler in `xserver/Xi/*.c`. xts5 XIproto probes under-/over-length
    // headers for every XI minor and expects core `BadLength` (code 16),
    // not the extension-level `BadDevice` the handlers would otherwise
    // produce after mis-parsing the truncated/extended body. The XI
    // `minor_opcode` is also stamped into the error reply so the test
    // harness can attribute the failure.
    if !x11::request_lengths::validate_xi_request_length(minor, header.length_units) {
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            x11::error::BAD_LENGTH,
            0,
            u16::from(minor),
            header.opcode,
        );
    }
    // Content-derived exact-length check for variable-length minors.
    // xts5 probes both `length-1` and `length+1`; the AtLeast gate
    // catches the under-length case while this gate catches the
    // over-length one.
    if !x11::request_lengths::validate_xi_exact_request_length(minor, header.length_units, body) {
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            x11::error::BAD_LENGTH,
            0,
            u16::from(minor),
            header.opcode,
        );
    }
    // Xorg's XI swap/dispatch path checks these content-derived tails before
    // handling any selection or grab records. The request swapper has already
    // converted the dynamic length fields to LE, while mask bytes remain
    // opaque. Keep this check ahead of all state changes so a truncated later
    // record cannot leave earlier selections or grabs installed.
    if !xi2_request_has_complete_dynamic_tail(minor, body) {
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            x11::error::BAD_LENGTH,
            0,
            u16::from(minor),
            header.opcode,
        );
    }
    match minor {
        1 => handle_xi1_get_extension_version(state, client_id, sequence, header, body, byte_order),
        42 => handle_xi_change_cursor(state, backend, origin, client_id, sequence, body),
        44 => handle_xi_set_client_pointer(client_id, sequence),
        45 => handle_xi_get_client_pointer(state, client_id, sequence, byte_order),
        46 => handle_xi_select_events(state, backend, origin, client_id, sequence, header, body),
        47 => handle_xi_query_version(state, client_id, sequence, body, byte_order),
        48 => handle_xi_query_device(state, client_id, sequence, body, byte_order),
        56 => handle_xi_list_properties(state, client_id, sequence, body, byte_order),
        57 => handle_xi_change_property(state, backend, client_id, sequence, body, byte_order),
        58 => handle_xi_delete_property(state, client_id, sequence, body),
        59 => handle_xi_get_property(state, client_id, sequence, body, byte_order),
        60 => handle_xi_get_selected_events(state, client_id, sequence, body, byte_order),
        40 => handle_xi_query_pointer(
            state, backend, origin, client_id, sequence, body, byte_order,
        ),
        51 => handle_xi_grab_device(
            state, backend, origin, client_id, sequence, body, byte_order,
        ),
        52 => handle_xi_ungrab_device(state, backend, origin, client_id, sequence, body),
        53 => handle_xi_allow_events(state, backend, client_id, sequence, body),
        54 => handle_xi_passive_grab_device(state, client_id, sequence, body, byte_order),
        55 => handle_xi_passive_ungrab_device(state, client_id, sequence, body),
        2 => handle_xi1_list_input_devices(state, client_id, sequence, byte_order),
        6 => handle_xi1_select_extension_event(state, client_id, sequence, body, minor),
        36 => handle_xi1_list_device_properties(state, client_id, sequence, body, byte_order),
        37 => {
            handle_xi1_change_device_property(state, backend, client_id, sequence, body, byte_order)
        }
        38 => handle_xi1_delete_device_property(state, client_id, sequence, body),
        39 => handle_xi1_get_device_property(state, client_id, sequence, body, byte_order),
        3 => handle_xi1_open_device(state, client_id, sequence, body, byte_order, minor),
        4 => handle_xi1_close_device(state, client_id, sequence, body, minor),
        5 => handle_xi1_set_device_mode(state, client_id, sequence, body, byte_order, minor),
        7 => handle_xi1_get_selected_extension_events(
            state, client_id, sequence, body, byte_order, minor,
        ),
        8 => handle_xi1_change_device_dont_propagate_list(state, client_id, sequence, body, minor),
        9 => handle_xi1_get_device_dont_propagate_list(
            state, client_id, sequence, body, byte_order, minor,
        ),
        10 => {
            handle_xi1_get_device_motion_events(state, client_id, sequence, body, byte_order, minor)
        }
        11 => {
            handle_xi1_change_keyboard_device(state, client_id, sequence, body, byte_order, minor)
        }
        12 => handle_xi1_change_pointer_device(state, client_id, sequence, minor),
        13 => handle_xi1_grab_device(state, client_id, sequence, body, byte_order, minor),
        14 => handle_xi1_ungrab_device(state, client_id, sequence, body, minor),
        15 => handle_xi1_grab_device_key(state, client_id, sequence, body, minor),
        16 => handle_xi1_ungrab_device_key(state, client_id, sequence, body, minor),
        17 => handle_xi1_grab_device_button(state, client_id, sequence, body, minor),
        18 => handle_xi1_ungrab_device_button(state, client_id, sequence, body, minor),
        19 => handle_xi1_allow_device_events(state, backend, client_id, sequence, body, minor),
        20 => handle_xi1_get_device_focus(state, client_id, sequence, body, byte_order, minor),
        21 => handle_xi1_set_device_focus(state, client_id, sequence, body, minor),
        22 => handle_xi1_get_feedback_control(state, client_id, sequence, body, byte_order, minor),
        23 => handle_xi1_change_feedback_control(state, client_id, sequence, body, minor),
        24 => handle_xi1_get_device_key_mapping(
            state, backend, origin, client_id, sequence, body, byte_order, minor,
        ),
        25 => handle_xi1_change_device_key_mapping(
            state, backend, client_id, sequence, body, byte_order, minor,
        ),
        26 => handle_xi1_get_device_modifier_mapping(
            state, backend, origin, client_id, sequence, body, byte_order, minor,
        ),
        27 => handle_xi1_set_device_modifier_mapping(
            state, backend, origin, client_id, sequence, body, byte_order, minor,
        ),
        28 => handle_xi1_get_device_button_mapping(
            state, client_id, sequence, body, byte_order, minor,
        ),
        29 => handle_xi1_set_device_button_mapping(
            state, client_id, sequence, body, byte_order, minor,
        ),
        30 => handle_xi1_query_device_state(state, client_id, sequence, body, byte_order, minor),
        31 => handle_xi1_send_extension_event(state, client_id, sequence, body, byte_order, minor),
        32 => handle_xi1_device_bell(state, client_id, sequence, body, minor),
        33 => handle_xi1_set_device_valuators(state, client_id, sequence, body, byte_order, minor),
        34 => handle_xi1_get_device_control(state, client_id, sequence, body, byte_order, minor),
        35 => handle_xi1_change_device_control(state, client_id, sequence, body, byte_order, minor),
        50 => handle_xi_get_focus(state, client_id, sequence, body, byte_order, minor),
        61 => handle_xi_barrier_release_pointer(state, client_id, sequence, body),
        41 => handle_xi_warp_pointer(state, backend, origin, client_id, sequence, body),
        43 => handle_xi_change_hierarchy(state, client_id, sequence, byte_order, body),
        49 => handle_xi_set_focus(state, client_id, sequence, body),
        // Every minor in 1..=XINPUT_LAST_REQUEST has an arm above; like
        // Xorg's ProcIDispatch, anything else is BadRequest.
        _ => {
            debug_assert!(minor == 0 || minor > XINPUT_LAST_REQUEST);
            emit_x11_error_with_minor(
                state,
                client_id,
                sequence,
                x11::error::BAD_REQUEST,
                0,
                u16::from(minor),
                header.opcode,
            )
        }
    }
}

fn xi_write_reply(
    state: &mut ServerState,
    client_id: ClientId,
    buf: Vec<u8>,
) -> io::Result<RequestOutcome> {
    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return Ok(RequestOutcome::Handled);
    };
    let _byte_order = client.byte_order;
    Ok(write_to_client(client, client_id, &buf))
}
