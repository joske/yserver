use crate::{
    access::{AccessMode, RequestOpcodes, ResourceKind},
    core_loop::process_request::{
        RequestOutcome, check_resource, emit_x11_error_with_minor,
        randr_ext::{RANDR_MAJOR_OPCODE, active_monitors, request_xid, send_root_config_notify},
        write_to_client,
    },
    server::ServerState,
};
use std::io;
use yserver_protocol::{
    x11,
    x11::{AtomId, ClientByteOrder, ClientId, ResourceId, SequenceNumber, randr as x11randr},
};

#[allow(clippy::needless_return)]
pub(super) fn handle_randr_get_monitors(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    byte_order: ClientByteOrder,
    minor: u8,
) -> io::Result<RequestOutcome> {
    let window = request_xid(body);
    if state.resources.window(ResourceId(window)).is_none() {
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            x11::error::BAD_WINDOW,
            window,
            u16::from(minor),
            RANDR_MAJOR_OPCODE,
        );
    }
    let request = RequestOpcodes {
        major: RANDR_MAJOR_OPCODE,
        minor,
    };
    check_resource!(
        state,
        client_id,
        sequence,
        request,
        ResourceId(window),
        ResourceKind::Window,
        AccessMode::GET_ATTR
    );
    // RRMonitorTimestamp: lastConfigTime, which Set/DeleteMonitor
    // leave alone ("XXX should take client monitor changes into
    // account", rrmonitor.c).
    let t = state.randr.timestamp;
    let get_active = body.get(4).is_some_and(|&b| b != 0);
    struct MonitorRow {
        name_atom: u32,
        primary: bool,
        automatic: bool,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        width_mm: u32,
        height_mm: u32,
        outputs: Vec<u32>,
    }
    let monitors_list = active_monitors(state, get_active);
    let rows: Vec<MonitorRow> = monitors_list
        .into_iter()
        .map(|monitor| MonitorRow {
            name_atom: match monitor.name {
                crate::randr::MonitorName::Atom(atom) => atom,
                crate::randr::MonitorName::Output(name) => state.atoms.intern(&name, false).0,
            },
            primary: monitor.primary,
            automatic: monitor.automatic,
            x: monitor.x,
            y: monitor.y,
            width: monitor.width,
            height: monitor.height,
            width_mm: monitor.width_mm,
            height_mm: monitor.height_mm,
            outputs: monitor.outputs,
        })
        .collect();
    let monitors: Vec<x11randr::MonitorInfo<'_>> = rows
        .iter()
        .map(|r| x11randr::MonitorInfo {
            name: r.name_atom,
            primary: r.primary,
            automatic: r.automatic,
            x: r.x,
            y: r.y,
            width: r.width,
            height: r.height,
            width_mm: r.width_mm,
            height_mm: r.height_mm,
            outputs: &r.outputs,
        })
        .collect();
    let buf = x11randr::encode_get_monitors_reply(byte_order, sequence, t, &monitors);
    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return Ok(RequestOutcome::Handled);
    };
    let _byte_order = client.byte_order;
    return Ok(write_to_client(client, client_id, &buf));
}

#[allow(clippy::needless_return)]
pub(super) fn handle_randr_set_monitor(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    minor: u8,
) -> io::Result<RequestOutcome> {
    // ProcRRSetMonitor + RRMonitorAdd as shipped in Xorg 21.1
    // (rrmonitor.c), measured by tools/vng-scenarios/xrandr-monitors.sh.
    let error = |state: &mut ServerState, code: u8, value: u32| {
        emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            code,
            value,
            u16::from(minor),
            RANDR_MAJOR_OPCODE,
        )
    };
    let Some(req) = x11randr::parse_set_monitor_request(body) else {
        return error(state, x11::error::BAD_LENGTH, 0);
    };
    if usize::from(req.noutput) != req.outputs.len() {
        return error(state, x11::error::BAD_LENGTH, 0);
    }
    if state.resources.window(ResourceId(req.window)).is_none() {
        return error(state, x11::error::BAD_WINDOW, req.window);
    }
    let request = RequestOpcodes {
        major: RANDR_MAJOR_OPCODE,
        minor,
    };
    check_resource!(
        state,
        client_id,
        sequence,
        request,
        ResourceId(req.window),
        ResourceKind::Window,
        AccessMode::GET_ATTR
    );
    // !ValidAtom: Xorg sets no errorValue here, so the wire carries
    // the window id the successful lookup just left in it.
    if req.name == 0 || state.atoms.name(AtomId(req.name)).is_none() {
        return error(state, x11::error::BAD_ATOM, req.window);
    }
    let name = state.atoms.name(AtomId(req.name)).unwrap_or_default();
    // 'name' must match neither an Output nor an existing Monitor.
    // (xserver main replaces a same-named monitor instead, 146bb9b2c;
    // 21.1 refuses it.)
    if state.randr.outputs.iter().any(|output| output.name == name)
        || state
            .randr_client_monitors
            .iter()
            .any(|monitor| monitor.name == req.name)
    {
        return error(state, x11::error::BAD_VALUE, req.name);
    }
    if req.primary {
        for monitor in &mut state.randr_client_monitors {
            monitor.primary = false;
        }
    }
    state
        .randr_client_monitors
        .push(crate::randr::ClientMonitor {
            name: req.name,
            primary: req.primary,
            outputs: req.outputs,
            x: req.x,
            y: req.y,
            width: req.width,
            height: req.height,
            width_mm: req.width_mm,
            height_mm: req.height_mm,
        });
    send_root_config_notify(state);
    return Ok(RequestOutcome::Handled);
}

#[allow(clippy::needless_return)]
pub(super) fn handle_randr_delete_monitor(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    minor: u8,
) -> io::Result<RequestOutcome> {
    // ProcRRDeleteMonitor + RRMonitorDelete (rrmonitor.c).
    let error = |state: &mut ServerState, code: u8, value: u32| {
        emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            code,
            value,
            u16::from(minor),
            RANDR_MAJOR_OPCODE,
        )
    };
    let Some((window, name)) = x11randr::parse_delete_monitor_request(body) else {
        return error(state, x11::error::BAD_LENGTH, 0);
    };
    if state.resources.window(ResourceId(window)).is_none() {
        return error(state, x11::error::BAD_WINDOW, window);
    }
    let request = RequestOpcodes {
        major: RANDR_MAJOR_OPCODE,
        minor,
    };
    check_resource!(
        state,
        client_id,
        sequence,
        request,
        ResourceId(window),
        ResourceKind::Window,
        AccessMode::GET_ATTR
    );
    if name == 0 || state.atoms.name(AtomId(name)).is_none() {
        return error(state, x11::error::BAD_ATOM, name);
    }
    let Some(index) = state
        .randr_client_monitors
        .iter()
        .position(|monitor| monitor.name == name)
    else {
        return error(state, x11::error::BAD_VALUE, name);
    };
    state.randr_client_monitors.remove(index);
    send_root_config_notify(state);
    return Ok(RequestOutcome::Handled);
}
