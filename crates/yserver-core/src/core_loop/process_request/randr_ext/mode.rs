//! RANDR user modes: CreateMode (16), DestroyMode (17), AddOutputMode (18)
//! and DeleteOutputMode (19), in Xorg's check order (`rrmode.c:284-380`,
//! `rroutput.c:199-262`). The store is `ServerState::randr_user_modes`.
//!
//! errorValue: `dixLookupResourceByType` leaves the last id it looked up in
//! `client->errorValue` (`dix/resource.c:1216-1218`), so an error after the
//! lookups carries the window (CreateMode) or the mode (the others), as
//! measured on Xvfb.

use crate::{
    access::{AccessMode, Creation, RequestOpcodes, ResourceKind, ResourceTarget},
    core_loop::process_request::{
        RANDR_BAD_MODE, RANDR_BAD_OUTPUT, RequestOutcome, check_resource,
        emit_x11_error_with_minor, randr_ext::RANDR_MAJOR_OPCODE, return_on_access_denial,
        validate_resource_access, write_to_client,
    },
    server::ServerState,
};
use std::io;
use yserver_protocol::{
    x11,
    x11::{
        ClientByteOrder, ClientId, RequestHeader, ResourceId, SequenceNumber, randr as x11randr,
    },
};

/// `sizeof(xRRCreateModeReq)` in 4-byte units.
const CREATE_MODE_UNITS: u32 = 10;

fn u32_at(body: &[u8], offset: usize) -> u32 {
    body.get(offset..offset + 4)
        .map_or(0, |bytes| u32::from_le_bytes(bytes.try_into().unwrap()))
}

fn mode_error(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    minor: u8,
    code: u8,
    value: u32,
) -> io::Result<RequestOutcome> {
    emit_x11_error_with_minor(
        state,
        client_id,
        sequence,
        code,
        value,
        u16::from(minor),
        RANDR_MAJOR_OPCODE,
    )
}

/// `ProcRRCreateMode` (`rrmode.c:284-330`).
#[allow(clippy::needless_return)]
pub(super) fn handle_randr_create_mode(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
    byte_order: ClientByteOrder,
) -> io::Result<RequestOutcome> {
    let minor = header.data;
    let request = RequestOpcodes {
        major: RANDR_MAJOR_OPCODE,
        minor,
    };
    // REQUEST_AT_LEAST_SIZE(xRRCreateModeReq).
    let parsed = (header.length_units >= CREATE_MODE_UNITS)
        .then(|| x11randr::parse_create_mode_request(body))
        .flatten();
    let Some((window, info)) = parsed else {
        return mode_error(state, client_id, sequence, minor, x11::error::BAD_LENGTH, 0);
    };
    // dixLookupWindow(DixGetAttrAccess).
    if state.resources.window(ResourceId(window)).is_none() {
        return mode_error(
            state,
            client_id,
            sequence,
            minor,
            x11::error::BAD_WINDOW,
            window,
        );
    }
    check_resource!(
        state,
        client_id,
        sequence,
        request,
        ResourceId(window),
        ResourceKind::Window,
        AccessMode::GET_ATTR
    );
    // The name must fit in the request.
    let name_len = usize::from(info.name_len);
    let units_after = header.length_units - CREATE_MODE_UNITS;
    let name_start = x11randr::CREATE_MODE_FIXED_BODY;
    let name = body.get(name_start..name_start + name_len);
    let Some(name) = name.filter(|_| u32::from(info.name_len).div_ceil(4) <= units_after) else {
        return mode_error(
            state,
            client_id,
            sequence,
            minor,
            x11::error::BAD_LENGTH,
            window,
        );
    };
    // RRModeCreateUser: a mode of that name anywhere is BadName.
    if state.randr_view().mode_name_in_use(name) {
        return mode_error(
            state,
            client_id,
            sequence,
            minor,
            x11::error::BAD_NAME,
            window,
        );
    }
    // The server picks the id (`FakeClientID(0)`); the request's is ignored.
    let Some(id) = state.randr_user_modes.next_free_id() else {
        return mode_error(
            state,
            client_id,
            sequence,
            minor,
            x11::error::BAD_ALLOC,
            window,
        );
    };
    return_on_access_denial!(
        validate_resource_access(
            state,
            client_id,
            ResourceTarget {
                id: ResourceId(id),
                kind: ResourceKind::RandrMode,
                creation: Some(Creation {
                    parent: None,
                    owner: crate::resources::SERVER_OWNER,
                }),
                owner: None,
            },
            AccessMode::CREATE,
            request,
        ),
        state,
        client_id,
        sequence,
        request,
    );
    state.randr_user_modes.insert(id, name.to_vec(), info);
    log::debug!(
        "client {} #{} RANDR::CreateMode {:?} {}x{} -> 0x{id:x}",
        client_id.0,
        sequence.0,
        String::from_utf8_lossy(name),
        info.width,
        info.height,
    );
    let mut reply = x11::fixed_reply(byte_order, sequence, 0, 0);
    x11::write_u32(byte_order, &mut reply, id);
    reply.extend_from_slice(&[0u8; 20]);
    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return Ok(RequestOutcome::Handled);
    };
    return Ok(write_to_client(client, client_id, &reply));
}

/// `ProcRRDestroyMode` (`rrmode.c:332-347`).
pub(super) fn handle_randr_destroy_mode(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    let minor = header.data;
    let request = RequestOpcodes {
        major: RANDR_MAJOR_OPCODE,
        minor,
    };
    // REQUEST_SIZE_MATCH(xRRDestroyModeReq).
    if header.length_units != 2 {
        return mode_error(state, client_id, sequence, minor, x11::error::BAD_LENGTH, 0);
    }
    let mode = u32_at(body, 0);
    if !state.randr_view().mode_exists(mode) {
        return mode_error(state, client_id, sequence, minor, RANDR_BAD_MODE, mode);
    }
    check_resource!(
        state,
        client_id,
        sequence,
        request,
        ResourceId(mode),
        ResourceKind::RandrMode,
        AccessMode::DESTROY
    );
    // Not a user mode (`!mode->userScreen`).
    if state.randr_user_modes.get(mode).is_none() {
        return mode_error(
            state,
            client_id,
            sequence,
            minor,
            x11::error::BAD_MATCH,
            mode,
        );
    }
    // `refcnt > 1`: on an output's user list or a CRTC's current mode.
    let view = state.randr_view();
    if view.user.on_any_output(mode) || view.mode_is_current(mode) {
        return mode_error(
            state,
            client_id,
            sequence,
            minor,
            x11::error::BAD_ACCESS,
            mode,
        );
    }
    state.randr_user_modes.remove(mode);
    Ok(RequestOutcome::Handled)
}

/// The output and mode of AddOutputMode / DeleteOutputMode, after
/// `VERIFY_RR_OUTPUT(DixReadAccess)` and `VERIFY_RR_MODE(DixUseAccess)`.
/// `Err` carries the request's outcome (an error already sent).
fn verify_output_mode(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> Result<(u32, u32), io::Result<RequestOutcome>> {
    let minor = header.data;
    let request = RequestOpcodes {
        major: RANDR_MAJOR_OPCODE,
        minor,
    };
    let check = |state: &mut ServerState, id: u32, kind: ResourceKind, mode: AccessMode| {
        crate::core_loop::process_request::validate_resource_if_present(
            state,
            client_id,
            ResourceId(id),
            kind,
            mode,
            request,
        )
        .map_err(|(code, value)| mode_error(state, client_id, sequence, minor, code, value))
    };
    // REQUEST_SIZE_MATCH.
    if header.length_units != 3 {
        return Err(mode_error(
            state,
            client_id,
            sequence,
            minor,
            x11::error::BAD_LENGTH,
            0,
        ));
    }
    let output = u32_at(body, 0);
    let mode = u32_at(body, 4);
    if output == 0 || !state.randr.outputs.iter().any(|o| o.output_id == output) {
        return Err(mode_error(
            state,
            client_id,
            sequence,
            minor,
            RANDR_BAD_OUTPUT,
            output,
        ));
    }
    check(state, output, ResourceKind::RandrOutput, AccessMode::READ)?;
    if !state.randr_view().mode_exists(mode) {
        return Err(mode_error(
            state,
            client_id,
            sequence,
            minor,
            RANDR_BAD_MODE,
            mode,
        ));
    }
    check(state, mode, ResourceKind::RandrMode, AccessMode::USE)?;
    // No leases (CreateLease is BadImplementation), so no leased-output
    // BadAccess.
    Ok((output, mode))
}

/// `ProcRRAddOutputMode` / `RROutputAddUserMode` (`rroutput.c:199-235`).
pub(super) fn handle_randr_add_output_mode(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    let (output, mode) = match verify_output_mode(state, client_id, sequence, header, body) {
        Ok(pair) => pair,
        Err(outcome) => return outcome,
    };
    // Already listed, probed or user: Success, nothing changes.
    if state.randr_view().output_lists_mode(output, mode) {
        return Ok(RequestOutcome::Handled);
    }
    // The DDX's mode_valid accepts everything on modesetting
    // (drmmode_display.c:2547-2550); the kernel decides at set time.
    state.randr_user_modes.add_to_output(output, mode);
    // RROutputChanged(output, TRUE) + RRTellChanged: lastConfigTime moves,
    // ScreenChangeNotify and this output's OutputChangeNotify, no CRTC.
    state.randr.config_timestamp = state.timestamp_now().max(1);
    let changed: Vec<(u32, u32, u32)> = state
        .randr
        .outputs
        .iter()
        .find(|o| o.output_id == output)
        .map(|o| (o.output_id, o.crtc_id, o.mode_id))
        .into_iter()
        .collect();
    crate::core_loop::run::emit_randr_connector_change_notifications(state, &[], &changed);
    Ok(RequestOutcome::Handled)
}

/// `ProcRRDeleteOutputMode` / `RROutputDeleteUserMode`
/// (`rroutput.c:238-262`).
pub(super) fn handle_randr_delete_output_mode(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    let minor = header.data;
    let (output, mode) = match verify_output_mode(state, client_id, sequence, header, body) {
        Ok(pair) => pair,
        Err(outcome) => return outcome,
    };
    // Not on this output's user list (a probed mode, or never added).
    if !state.randr_user_modes.output_list(output).contains(&mode) {
        return mode_error(
            state,
            client_id,
            sequence,
            minor,
            x11::error::BAD_ACCESS,
            mode,
        );
    }
    // The output's CRTC shows it.
    if state.randr_view().output_crtc_mode(output) == mode {
        return mode_error(
            state,
            client_id,
            sequence,
            minor,
            x11::error::BAD_MATCH,
            mode,
        );
    }
    state.randr_user_modes.remove_from_output(output, mode);
    Ok(RequestOutcome::Handled)
}
