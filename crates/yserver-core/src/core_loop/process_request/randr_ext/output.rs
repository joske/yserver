use crate::{
    access::{AccessMode, RequestOpcodes, ResourceKind},
    backend::Backend,
    core_loop::process_request::{
        RANDR_BAD_OUTPUT, RequestOutcome, check_resource, emit_x11_error_with_minor,
        randr_ext::{RANDR_MAJOR_OPCODE, request_xid},
        write_to_client,
    },
    properties,
    server::ServerState,
};
use std::io;
use yserver_protocol::{
    x11,
    x11::{
        AtomId, ClientByteOrder, ClientId, RequestHeader, ResourceId, SequenceNumber,
        randr as x11randr,
    },
};

fn output_exists(state: &ServerState, output: u32) -> bool {
    state
        .randr
        .outputs
        .iter()
        .any(|candidate| candidate.output_id == output)
}

/// Backend-synthesized read-only identity properties (`EDID` /
/// `EDID_DATA` / `ConnectorType`), resolved live from
/// `Backend::output_identity` rather than stored. Consulted only when
/// `output_id` has no matching entry in `state.randr_output_properties`
/// — a real store entry always shadows the synthesized value, matching
/// Xorg's generic property store (a client `ChangeOutputProperty` on
/// e.g. `EDID` overwrites whatever the driver put there).
fn synthetic_output_property(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    output_id: u32,
    property: AtomId,
) -> Option<(u32, u8, Vec<u8>)> {
    const XA_ATOM: u32 = 4;
    const XA_INTEGER: u32 = 19;
    let prop_name = state.atoms.name(property).map(str::to_owned);
    let identity = backend.output_identity(output_id);
    match (prop_name.as_deref(), identity) {
        (Some("EDID" | "EDID_DATA"), Some((edid, _))) if !edid.is_empty() => {
            Some((XA_INTEGER, 8, edid))
        }
        (Some("ConnectorType"), Some((_, ctype))) if !ctype.is_empty() => {
            let atom = state.atoms.intern(&ctype, false).0;
            Some((XA_ATOM, 32, atom.to_le_bytes().to_vec()))
        }
        _ => None,
    }
}

#[allow(clippy::needless_return)]
pub(super) fn handle_randr_get_output_info(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
    byte_order: ClientByteOrder,
) -> io::Result<RequestOutcome> {
    let Some(req) = x11randr::parse_output_request(body) else {
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            x11::error::BAD_VALUE,
            0,
            u16::from(header.data),
            RANDR_MAJOR_OPCODE,
        );
    };
    let Some(info_data) = state.randr.output_info(req.output, req.config_timestamp) else {
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            RANDR_BAD_OUTPUT,
            req.output,
            u16::from(header.data),
            RANDR_MAJOR_OPCODE,
        );
    };
    // The `crtcs` array is the output's *possible* CRTCs, not the
    // currently-assigned one (which is 0 for a connected-but-off
    // output and would advertise an invalid crtcs=[0]).
    let crtc_ids = info_data.possible_crtcs.as_slice();
    let mode_ids = info_data.mode_ids.as_slice();
    let name_bytes = info_data.name.as_bytes();
    let buf = x11randr::encode_get_output_info_reply(
        byte_order,
        sequence,
        &x11randr::OutputInfoReply {
            timestamp: info_data.timestamp,
            crtc: info_data.crtc,
            width_mm: info_data.width_mm,
            height_mm: info_data.height_mm,
            connection: info_data.connection,
            subpixel_order: 0,
            crtcs: crtc_ids,
            modes: mode_ids,
            num_preferred: info_data.num_preferred,
            clones: &[],
            name: name_bytes,
        },
    );
    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return Ok(RequestOutcome::Handled);
    };
    let _byte_order = client.byte_order;
    return Ok(write_to_client(client, client_id, &buf));
}

#[allow(clippy::needless_return)]
pub(super) fn handle_randr_list_output_properties(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    byte_order: ClientByteOrder,
    minor: u8,
) -> io::Result<RequestOutcome> {
    let output_id = request_xid(body);
    if !output_exists(state, output_id) {
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            RANDR_BAD_OUTPUT,
            output_id,
            u16::from(minor),
            RANDR_MAJOR_OPCODE,
        );
    }
    // Real client-set properties first, then the backend-synthesized
    // identity properties (EDID + EDID_DATA + ConnectorType) iff the
    // backend has EDID for this output and no real entry already
    // shadows that atom name.
    let mut atoms: Vec<u32> = state
        .randr_output_properties
        .get(&output_id)
        .map(|entries| entries.iter().map(|(atom, _)| atom.0).collect())
        .unwrap_or_default();
    if let Some((edid, _)) = backend.output_identity(output_id)
        && !edid.is_empty()
    {
        for name in ["EDID", "EDID_DATA", "ConnectorType"] {
            let atom = state.atoms.intern(name, false).0;
            if !atoms.contains(&atom) {
                atoms.push(atom);
            }
        }
    }
    let buf = x11randr::encode_list_output_properties_reply(byte_order, sequence, &atoms);
    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return Ok(RequestOutcome::Handled);
    };
    let _byte_order = client.byte_order;
    return Ok(write_to_client(client, client_id, &buf));
}

#[allow(clippy::needless_return)]
pub(super) fn handle_randr_query_output_property(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    byte_order: ClientByteOrder,
    minor: u8,
) -> io::Result<RequestOutcome> {
    // Error packets carry the EXTENSION major opcode (128) with
    // the request's minor in the minor field — passing the minor
    // as `major_opcode` (pre-fix) made xtrace print "major=11,
    // minor=0" for this BadName.
    let Some(req) = x11randr::parse_output_property_request(body) else {
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            x11::error::BAD_LENGTH,
            0,
            u16::from(x11randr::RR_QUERY_OUTPUT_PROPERTY),
            RANDR_MAJOR_OPCODE,
        );
    };
    if !output_exists(state, req.output) {
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            RANDR_BAD_OUTPUT,
            req.output,
            u16::from(minor),
            RANDR_MAJOR_OPCODE,
        );
    }
    let property = AtomId(req.property);
    let real = state
        .randr_output_properties
        .get(&req.output)
        .and_then(|entries| entries.iter().find(|(atom, _)| *atom == property));
    let buf = if let Some((_, prop)) = real {
        x11randr::encode_query_output_property_reply(
            byte_order,
            sequence,
            prop.is_pending,
            prop.range,
            prop.immutable,
            &prop.valid_values,
        )
    } else if synthetic_output_property(state, backend, req.output, property).is_some() {
        x11randr::encode_query_output_property_reply(byte_order, sequence, false, false, true, &[])
    } else {
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            x11::error::BAD_NAME,
            req.property,
            u16::from(x11randr::RR_QUERY_OUTPUT_PROPERTY),
            RANDR_MAJOR_OPCODE,
        );
    };
    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return Ok(RequestOutcome::Handled);
    };
    return Ok(write_to_client(client, client_id, &buf));
}

#[allow(clippy::needless_return)]
pub(super) fn handle_randr_configure_output_property(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    minor: u8,
) -> io::Result<RequestOutcome> {
    // No lease check (real output leases can't exist — CreateLease
    // always fails) and no immutable check: Xorg's wire handler
    // hardcodes `immutable = FALSE` for a client-issued Configure, so
    // `prop->immutable && !immutable` can only fire for a
    // driver-marked property, which yserver never creates.
    let Some(req) = x11randr::parse_configure_output_property_request(body) else {
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            x11::error::BAD_LENGTH,
            0,
            u16::from(minor),
            RANDR_MAJOR_OPCODE,
        );
    };
    if !output_exists(state, req.output) {
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            RANDR_BAD_OUTPUT,
            req.output,
            u16::from(minor),
            RANDR_MAJOR_OPCODE,
        );
    }
    // Ranges must have an even number of values (min,max pairs).
    if req.range && req.valid_values.len() % 2 != 0 {
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            x11::error::BAD_MATCH,
            req.output,
            u16::from(minor),
            RANDR_MAJOR_OPCODE,
        );
    }
    let property = AtomId(req.property);
    let entries = state.randr_output_properties.entry(req.output).or_default();
    let prop = match entries.iter_mut().find(|(atom, _)| *atom == property) {
        Some((_, prop)) => prop,
        None => {
            // Prepend, matching Xorg's RRCreateOutputProperty list
            // insertion (see the ChangeOutputProperty handler above).
            entries.insert(0, (property, crate::randr::RandrOutputProperty::default()));
            &mut entries[0].1
        }
    };
    // "Property moving from pending to non-pending loses any pending
    // values" (Xorg RRConfigureOutputProperty).
    if prop.is_pending && !req.pending {
        prop.pending = None;
    }
    prop.is_pending = req.pending;
    prop.range = req.range;
    prop.valid_values = req.valid_values;
    return Ok(RequestOutcome::Handled);
}

#[allow(clippy::needless_return)]
pub(super) fn handle_randr_change_output_property(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    minor: u8,
) -> io::Result<RequestOutcome> {
    // Order mirrors Xorg's ProcRRChangeOutputProperty: mode, then
    // format, then length consistency, then output, then the
    // property/type atoms — a request that violates several of
    // these at once must fail with the same error Xorg reports.
    let Some(req) = x11randr::parse_change_output_property_request(body) else {
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            x11::error::BAD_LENGTH,
            0,
            u16::from(minor),
            RANDR_MAJOR_OPCODE,
        );
    };
    let Some(mode) = properties::ChangeMode::from_protocol(req.mode) else {
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            x11::error::BAD_VALUE,
            u32::from(req.mode),
            u16::from(minor),
            RANDR_MAJOR_OPCODE,
        );
    };
    let Some(format) = properties::PropertyFormat::from_protocol(req.format) else {
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            x11::error::BAD_VALUE,
            u32::from(req.format),
            u16::from(minor),
            RANDR_MAJOR_OPCODE,
        );
    };
    let expected_bytes = (req.n_units as usize).checked_mul(format.bytes());
    if expected_bytes != Some(req.data.len()) {
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            x11::error::BAD_LENGTH,
            0,
            u16::from(minor),
            RANDR_MAJOR_OPCODE,
        );
    }
    if !output_exists(state, req.output) {
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            RANDR_BAD_OUTPUT,
            req.output,
            u16::from(minor),
            RANDR_MAJOR_OPCODE,
        );
    }
    let property = AtomId(req.property);
    if !state.atoms.exists(property) {
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            x11::error::BAD_ATOM,
            req.property,
            u16::from(minor),
            RANDR_MAJOR_OPCODE,
        );
    }
    let prop_type = AtomId(req.prop_type);
    if !state.atoms.exists(prop_type) {
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            x11::error::BAD_ATOM,
            req.prop_type,
            u16::from(minor),
            RANDR_MAJOR_OPCODE,
        );
    }
    let entries = state.randr_output_properties.entry(req.output).or_default();
    let slot = entries.iter().position(|(atom, _)| *atom == property);
    // Only a property previously marked pending-capable via
    // ConfigureOutputProperty writes into `.pending` (and skips the
    // notify) — the wire request itself has no pending flag; Xorg's
    // ProcRRChangeOutputProperty always passes `pending=TRUE`
    // internally, and RRChangeOutputProperty reduces that to
    // `prop->is_pending`.
    let is_pending_configured = slot.is_some_and(|i| entries[i].1.is_pending);
    let existing_value = slot.and_then(|i| {
        if is_pending_configured {
            entries[i].1.pending.clone()
        } else {
            entries[i].1.current.clone()
        }
    });
    let new_value =
        match properties::apply_change(existing_value.as_ref(), mode, prop_type, format, &req.data)
        {
            Ok(v) => v,
            Err(properties::ChangePropertyError::BadMatch) => {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_MATCH,
                    req.output,
                    u16::from(minor),
                    RANDR_MAJOR_OPCODE,
                );
            }
            Err(properties::ChangePropertyError::BadAlloc) => {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_ALLOC,
                    0,
                    u16::from(minor),
                    RANDR_MAJOR_OPCODE,
                );
            }
            Err(properties::ChangePropertyError::BadValue) => {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_VALUE,
                    0,
                    u16::from(minor),
                    RANDR_MAJOR_OPCODE,
                );
            }
        };
    match slot {
        Some(i) => {
            if is_pending_configured {
                entries[i].1.pending = Some(new_value);
            } else {
                entries[i].1.current = Some(new_value);
            }
        }
        // Xorg's RRCreateOutputProperty prepends a newly created
        // property onto the output's property list, so
        // ListOutputProperties enumerates newest-first.
        None => entries.insert(
            0,
            (
                property,
                crate::randr::RandrOutputProperty {
                    current: Some(new_value),
                    ..Default::default()
                },
            ),
        ),
    }
    // Xorg's `sendevent` is unconditional in `RRChangeOutputProperty`
    // (`ProcRRChangeOutputProperty` always passes `sendevent=TRUE`)
    // — only the unrelated `RRNoticePropertyChange` driver hook is
    // gated on `is_pending`. The wire notify fires regardless of
    // whether this write landed in `.current` or `.pending`.
    crate::core_loop::run::notify_randr_output_property_changed(
        state,
        req.output,
        property,
        x11randr::PROPERTY_NEW_VALUE,
    );
    return Ok(RequestOutcome::Handled);
}

#[allow(clippy::needless_return)]
pub(super) fn handle_randr_delete_output_property(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    minor: u8,
) -> io::Result<RequestOutcome> {
    // No lease check: real output leases can't exist.
    let Some(req) = x11randr::parse_output_property_request(body) else {
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            x11::error::BAD_LENGTH,
            0,
            u16::from(minor),
            RANDR_MAJOR_OPCODE,
        );
    };
    if !output_exists(state, req.output) {
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            RANDR_BAD_OUTPUT,
            req.output,
            u16::from(minor),
            RANDR_MAJOR_OPCODE,
        );
    }
    let property = AtomId(req.property);
    if !state.atoms.exists(property) {
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            x11::error::BAD_ATOM,
            req.property,
            u16::from(minor),
            RANDR_MAJOR_OPCODE,
        );
    }
    let entries = state.randr_output_properties.entry(req.output).or_default();
    let Some(index) = entries.iter().position(|(atom, _)| *atom == property) else {
        // Known caveat: a synthetic-only identity atom (EDID /
        // EDID_DATA / ConnectorType) with no real store entry
        // returns BadName here rather than Xorg's BadAccess for an
        // immutable driver property — no real caller deletes
        // output identity metadata.
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            x11::error::BAD_NAME,
            req.property,
            u16::from(minor),
            RANDR_MAJOR_OPCODE,
        );
    };
    if entries[index].1.immutable {
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            x11::error::BAD_ACCESS,
            req.property,
            u16::from(minor),
            RANDR_MAJOR_OPCODE,
        );
    }
    entries.remove(index);
    crate::core_loop::run::notify_randr_output_property_changed(
        state,
        req.output,
        property,
        x11randr::PROPERTY_DELETE,
    );
    return Ok(RequestOutcome::Handled);
}

#[allow(clippy::needless_return)]
pub(super) fn handle_randr_set_output_primary(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
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
    let output = body
        .get(4..8)
        .map(|bytes| u32::from_le_bytes(bytes.try_into().unwrap()))
        .unwrap_or(0);
    if output != 0 && !output_exists(state, output) {
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            RANDR_BAD_OUTPUT,
            output,
            u16::from(minor),
            RANDR_MAJOR_OPCODE,
        );
    }
    // None (0) clears the primary output. A nonzero id is already
    // validated above; yserver has one screen and no leased outputs,
    // so Xorg's remaining cross-screen/lease checks are vacuous.
    let previous = state.randr.primary_output;
    // Even an idempotent request turns the topology-derived default
    // into an explicit client choice that must survive rebuilds.
    state.randr_primary_output_explicit = true;
    if previous == output {
        // Xorg's RRSetPrimaryOutput returns early when nothing moves,
        // so no notify storm from an idempotent set.
        return Ok(RequestOutcome::Handled);
    }
    state.randr.primary_output = output;
    // A primary change is a LAYOUT change in Xorg: RROutputChanged on
    // the affected outputs + layoutChanged, then RRTellChanged
    // (randr/rroutput.c). Both the old and new primary changed, so both
    // are announced; clients learn about this by notify, not polling.
    let changed: Vec<u32> = [previous, output].into_iter().filter(|o| *o != 0).collect();
    crate::core_loop::run::notify_randr_layout_changed(state, &changed);
    return Ok(RequestOutcome::Handled);
}

#[allow(clippy::needless_return)]
pub(super) fn handle_randr_get_output_primary(
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
    let primary = state.randr.primary_output;
    let buf = x11randr::encode_get_output_primary_reply(byte_order, sequence, primary);
    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return Ok(RequestOutcome::Handled);
    };
    let _byte_order = client.byte_order;
    return Ok(write_to_client(client, client_id, &buf));
}

#[allow(clippy::needless_return)]
pub(super) fn handle_randr_get_output_property(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    byte_order: ClientByteOrder,
    minor: u8,
) -> io::Result<RequestOutcome> {
    let Some(req) = x11randr::parse_get_output_property_request(body) else {
        let buf = x11randr::encode_get_output_property_reply(byte_order, sequence, 0, 0, 0, &[]);
        let Some(client) = state.clients.get_mut(&client_id.0) else {
            return Ok(RequestOutcome::Handled);
        };
        return Ok(write_to_client(client, client_id, &buf));
    };
    if !output_exists(state, req.output) {
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            RANDR_BAD_OUTPUT,
            req.output,
            u16::from(minor),
            RANDR_MAJOR_OPCODE,
        );
    }
    let property = AtomId(req.property);
    // Xorg validates the property atom, then the delete BOOL, then
    // the requested type atom, all before ever looking up the
    // property (`ProcRRGetOutputProperty`, randr/rrproperty.c).
    if !state.atoms.exists(property) {
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            x11::error::BAD_ATOM,
            req.property,
            u16::from(minor),
            RANDR_MAJOR_OPCODE,
        );
    }
    if req.delete_raw != 0 && req.delete_raw != 1 {
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            x11::error::BAD_VALUE,
            u32::from(req.delete_raw),
            u16::from(minor),
            RANDR_MAJOR_OPCODE,
        );
    }
    if req.prop_type != 0 && !state.atoms.exists(AtomId(req.prop_type)) {
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            x11::error::BAD_ATOM,
            req.prop_type,
            u16::from(minor),
            RANDR_MAJOR_OPCODE,
        );
    }
    // A real store entry always shadows the backend-synthesized
    // identity properties (EDID / EDID_DATA / ConnectorType).
    let real = state
        .randr_output_properties
        .get(&req.output)
        .and_then(|entries| entries.iter().find(|(atom, _)| *atom == property))
        .map(|(_, prop)| prop.clone());
    let (served, immutable): (Option<(u32, u8, Vec<u8>)>, bool) = if let Some(prop) = real.as_ref()
    {
        let value = if req.pending && prop.is_pending {
            prop.pending.as_ref()
        } else {
            prop.current.as_ref()
        };
        (
            value.map(|v| (v.r#type.0, v.format.protocol_value(), v.data.clone())),
            prop.immutable,
        )
    } else if let Some(syn) = synthetic_output_property(state, backend, req.output, property) {
        (Some(syn), true)
    } else {
        (None, false)
    };
    if req.delete && immutable {
        return emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            x11::error::BAD_ACCESS,
            req.property,
            u16::from(minor),
            RANDR_MAJOR_OPCODE,
        );
    }
    let (buf, bytes_after): (Vec<u8>, u32) = match served {
        // Type-mismatch (client asked for a specific, different type):
        // reply with the real type + empty value + full bytes_after.
        // `bytes_after` is an ELEMENT count here (Xorg:
        // `reply.bytesAfter = prop_value->size`, and `size` is
        // stored in format-units — see `RRChangeOutputProperty`'s
        // `new_value.size = total_len` where `total_len` counts
        // `nUnits`, not bytes), not `full.len()`.
        Some((ptype, format, full)) if req.prop_type != 0 && req.prop_type != ptype => {
            let unit = (format as usize / 8).max(1);
            let bytes_after = u32::try_from(full.len() / unit).unwrap_or(u32::MAX);
            (
                x11randr::encode_get_output_property_reply(
                    byte_order,
                    sequence,
                    ptype,
                    format,
                    bytes_after,
                    &[],
                ),
                bytes_after,
            )
        }
        Some((ptype, format, full)) => {
            // Windowing: long-offset / long-length are in 32-bit units.
            let total = full.len();
            let start = (req.long_offset as usize).saturating_mul(4);
            if start > total {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_VALUE,
                    req.long_offset,
                    u16::from(x11randr::RR_GET_OUTPUT_PROPERTY),
                    RANDR_MAJOR_OPCODE,
                );
            }
            let avail = total - start;
            let take = avail.min((req.long_length as usize).saturating_mul(4));
            let bytes_after = u32::try_from(avail - take).unwrap_or(u32::MAX);
            (
                x11randr::encode_get_output_property_reply(
                    byte_order,
                    sequence,
                    ptype,
                    format,
                    bytes_after,
                    &full[start..start + take],
                ),
                bytes_after,
            )
        }
        None => (
            x11randr::encode_get_output_property_reply(byte_order, sequence, 0, 0, 0, &[]),
            0,
        ),
    };
    // Xorg fires the delete notify before writing the reply, then
    // physically removes the property once bytesAfter==0 — reachable
    // only for a real (non-immutable) store entry, since the
    // immutable check above already rejected a synthetic property.
    if req.delete && bytes_after == 0 && real.is_some() {
        crate::core_loop::run::notify_randr_output_property_changed(
            state,
            req.output,
            property,
            x11randr::PROPERTY_DELETE,
        );
        if let Some(entries) = state.randr_output_properties.get_mut(&req.output) {
            entries.retain(|(atom, _)| *atom != property);
        }
    }
    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return Ok(RequestOutcome::Handled);
    };
    let _byte_order = client.byte_order;
    return Ok(write_to_client(client, client_id, &buf));
}
