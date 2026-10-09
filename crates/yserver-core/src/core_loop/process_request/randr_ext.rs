use super::*;

/// Continuation data needed to finish an asynchronous `RRSetCrtcConfig`
/// without redispatching the original request.
#[derive(Debug, Clone)]
pub struct PendingCrtcConfig {
    pub token: CrtcConfigToken,
    pub completion: CrtcConfigCompletion,
}

/// Protocol continuation shared by synchronous and asynchronous CRTC apply
/// paths. It contains no backend token, so immediate completion never needs a
/// sentinel token value.
#[derive(Debug, Clone)]
pub struct CrtcConfigCompletion {
    pub output_id: u32,
    pub set_time: u32,
    pub output_bbox_before: Option<(u16, u16)>,
    pub byte_order: yserver_protocol::x11::ClientByteOrder,
    /// The pending transform this enable applies, snapshotted at request
    /// time, when it differs from the current one (`RRCrtcPendingTransform`,
    /// rrcrtc.c:765).
    pub apply_transform: Option<Box<crate::randr::CrtcTransform>>,
    /// The rotation this enable sets, when it differs from the CRTC's
    /// (`RRCrtcSet`'s `rotation != crtc->rotation`, rrcrtc.c:749). A
    /// disable keeps the rotation, as `xf86RandR12CrtcSet` does.
    pub apply_rotation: Option<u16>,
    /// Which request is waiting for the reply.
    pub reply: CrtcConfigReply,
}

/// The request a CRTC configuration answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrtcConfigReply {
    /// `RRSetCrtcConfig`.
    CrtcConfig,
    /// RANDR 1.0 `RRSetScreenConfig`: its own reply, and `lastSetTime`
    /// moves on every success (rrscreen.c:1099-1101).
    ScreenConfig,
}

/// The protocol-visible monitor list (`RRMonitorMakeList`), shared by RANDR
/// `GetMonitors` and XINERAMA so their counts/order cannot diverge.
pub(super) fn active_monitors(state: &ServerState, get_active: bool) -> Vec<crate::randr::Monitor> {
    state
        .randr
        .monitors(&state.randr_client_monitors, get_active)
}

/// `RRSendConfigNotify` (rrscreen.c): a core ConfigureNotify on the root
/// carrying its current geometry — all `SetMonitor`/`DeleteMonitor` send.
/// No RANDR event: the monitor list has none of its own.
fn send_root_config_notify(state: &mut ServerState) {
    let Some(root) = state.resources.window(crate::resources::ROOT_WINDOW) else {
        return;
    };
    let geometry = x11::Geometry {
        root: crate::resources::ROOT_WINDOW,
        x: 0,
        y: 0,
        width: root.width,
        height: root.height,
        border_width: root.border_width,
        depth: root.depth,
    };
    let override_redirect = root.override_redirect;
    let _dropped = crate::core_loop::fanout::emit_window_event_to_state(
        state,
        crate::resources::ROOT_WINDOW,
        0x0002_0000, // StructureNotifyMask
        |buf, seq, order| {
            x11::encode_configure_notify_event(
                buf,
                seq,
                order,
                crate::resources::ROOT_WINDOW,
                crate::resources::ROOT_WINDOW,
                None,
                geometry,
                override_redirect,
            );
        },
    );
}

/// Record an unsupported RANDR minor and report whether this is the first
/// occurrence for this server instance.
pub(super) fn mark_randr_unsupported_warned(state: &mut ServerState, minor: u8) -> bool {
    let Some(bit) = 1u64.checked_shl(u32::from(minor)) else {
        return true;
    };
    let first = state.randr_unsupported_warned_mask & bit == 0;
    state.randr_unsupported_warned_mask |= bit;
    first
}

fn warn_randr_unsupported_once(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    minor: u8,
    request_name: &str,
    action: &str,
) {
    if mark_randr_unsupported_warned(state, minor) {
        log::warn!(
            "client {} #{} RANDR::{} is unsupported; {}",
            client_id.0,
            sequence.0,
            request_name,
            action,
        );
    } else {
        debug!(
            "client {} #{} RANDR::{} remains unsupported; {}",
            client_id.0, sequence.0, request_name, action,
        );
    }
}

pub(super) fn handle_randr_request(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    use yserver_protocol::x11::{ClientByteOrder, randr as x11randr};
    const RANDR_MAJOR_OPCODE: u8 = 128;
    fn crtc_is_leased(_state: &ServerState, _crtc: u32) -> bool {
        false
    }
    fn provider_relationship_protocol_error(
        error: crate::randr::ProviderRelationshipError,
    ) -> (u8, u32) {
        match error {
            crate::randr::ProviderRelationshipError::UnknownProvider(provider) => {
                (RANDR_BAD_PROVIDER, provider)
            }
            // Xorg returns BadValue for a missing provider capability or an
            // initiating provider that is not a GPU screen without assigning
            // client->errorValue, so the wire value remains zero.
            crate::randr::ProviderRelationshipError::MissingCapability(_)
            | crate::randr::ProviderRelationshipError::NotGpuProvider(_) => {
                (x11::error::BAD_VALUE, 0)
            }
        }
    }
    fn request_xid(body: &[u8]) -> u32 {
        body.get(0..4)
            .map(|bytes| u32::from_le_bytes(bytes.try_into().unwrap()))
            .unwrap_or(0)
    }
    fn output_exists(state: &ServerState, output: u32) -> bool {
        state
            .randr
            .outputs
            .iter()
            .any(|candidate| candidate.output_id == output)
    }
    fn crtc_exists(state: &ServerState, crtc: u32) -> bool {
        state
            .randr
            .outputs
            .iter()
            .any(|output| output.crtc_id == crtc)
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
    let byte_order = state
        .clients
        .get(&client_id.0)
        .map_or(ClientByteOrder::LittleEndian, |c| c.byte_order);
    let minor = header.data;
    match minor {
        x11randr::RR_QUERY_VERSION => {
            if let Some(r) = x11randr::parse_query_version(body) {
                state
                    .randr_client_versions
                    .insert(client_id, (r.major, r.minor));
            }
            let (reply_major, reply_minor) = x11randr::parse_query_version(body)
                .map(|r| {
                    let reply_major = x11randr::MAJOR_VERSION;
                    let reply_minor = if r.major < x11randr::MAJOR_VERSION {
                        r.minor
                    } else {
                        x11randr::MINOR_VERSION
                    };
                    (reply_major, reply_minor)
                })
                .unwrap_or((x11randr::MAJOR_VERSION, x11randr::MINOR_VERSION));
            let buf = x11randr::encode_query_version_reply(
                byte_order,
                sequence,
                reply_major,
                reply_minor,
            );
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            let _byte_order = client.byte_order;
            return Ok(write_to_client(client, client_id, &buf));
        }
        x11randr::RR_GET_SCREEN_SIZE_RANGE => {
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
            let (min_w, min_h, max_w, max_h) = state.randr.screen_size_range();
            let buf = x11randr::encode_get_screen_size_range_reply(
                byte_order, sequence, min_w, min_h, max_w, max_h,
            );
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            let _byte_order = client.byte_order;
            return Ok(write_to_client(client, client_id, &buf));
        }
        x11randr::RR_GET_SCREEN_RESOURCES => {
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
            // Force a connector re-probe before replying (Xorg
            // RRGetInfo force_query=TRUE). A probe failure surfaces as
            // BadAlloc, matching Xorg. GetScreenResourcesCurrent below
            // skips this and serves the cached view.
            if let Err(e) = backend.reprobe_connectors(state) {
                log::warn!("RRGetScreenResources reprobe failed: {e}");
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_ALLOC,
                    0,
                    u16::from(header.data),
                    RANDR_MAJOR_OPCODE,
                );
            }
            let resources = state.randr.screen_resources_current();
            let buf = x11randr::encode_get_screen_resources_current_reply(
                byte_order, sequence, &resources,
            );
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            let _byte_order = client.byte_order;
            return Ok(write_to_client(client, client_id, &buf));
        }
        x11randr::RR_GET_SCREEN_RESOURCES_CURRENT => {
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
            let resources = state.randr.screen_resources_current();
            let buf = x11randr::encode_get_screen_resources_current_reply(
                byte_order, sequence, &resources,
            );
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            let _byte_order = client.byte_order;
            return Ok(write_to_client(client, client_id, &buf));
        }
        x11randr::RR_GET_OUTPUT_INFO => {
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
        x11randr::RR_GET_CRTC_INFO => {
            let Some(req) = x11randr::parse_crtc_request(body) else {
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
            let Some(crtc_data) = state.randr.crtc_info(req.crtc, req.config_timestamp) else {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    RANDR_BAD_CRTC,
                    req.crtc,
                    u16::from(header.data),
                    RANDR_MAJOR_OPCODE,
                );
            };
            let buf = x11randr::encode_get_crtc_info_reply(
                byte_order,
                sequence,
                &x11randr::CrtcInfoReply {
                    timestamp: crtc_data.timestamp,
                    x: crtc_data.x,
                    y: crtc_data.y,
                    width: crtc_data.width,
                    height: crtc_data.height,
                    mode: crtc_data.mode_id,
                    rotation: state.randr.crtc_rotation(req.crtc),
                    rotations: crate::randr::SUPPORTED_ROTATIONS,
                    outputs: &crtc_data.outputs,
                    possible: &crtc_data.possible_outputs,
                },
            );
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            let _byte_order = client.byte_order;
            return Ok(write_to_client(client, client_id, &buf));
        }
        x11randr::RR_SET_CRTC_TRANSFORM => {
            // ProcRRSetCrtcTransform (rrcrtc.c:1755-1785) + RRCrtcTransformSet
            // (rrcrtc.c:1091-1128), then the spec's D2 contract. Every yserver
            // CRTC supports transforms, so Xorg's `!crtc->transforms`
            // BadValue has no counterpart.
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
            let Some(req) = x11randr::parse_set_crtc_transform_request(body) else {
                return error(state, x11::error::BAD_LENGTH, 0);
            };
            if !crtc_exists(state, req.crtc) {
                return error(state, RANDR_BAD_CRTC, req.crtc);
            }
            if crtc_is_leased(state, req.crtc) {
                return error(state, x11::error::BAD_ACCESS, 0);
            }
            if !crate::randr::CrtcTransform::invertible(&req.transform) {
                return error(state, x11::error::BAD_MATCH, 0);
            }
            let Some(spec) = req.filter else {
                return error(state, x11::error::BAD_LENGTH, 0);
            };
            let filter = if spec.name.is_empty() {
                if !spec.params.is_empty() {
                    return error(state, x11::error::BAD_MATCH, 0);
                }
                None
            } else {
                let Some(filter) = crate::randr::Filter::from_name(&spec.name) else {
                    return error(state, x11::error::BAD_NAME, 0);
                };
                if !filter.params_valid(&spec.params) {
                    return error(state, x11::error::BAD_MATCH, 0);
                }
                Some(filter)
            };
            let Some(transform) =
                crate::randr::CrtcTransform::new(req.transform, filter, spec.params)
            else {
                return error(state, x11::error::BAD_MATCH, 0);
            };
            // D2: pure scale, nearest/bilinear only; the rest is refused on
            // purpose rather than rendered approximately.
            if !(transform.is_identity() || transform.is_pure_scale())
                || filter == Some(crate::randr::Filter::Convolution)
            {
                return error(state, x11::error::BAD_MATCH, 0);
            }
            if let Some(output) = state
                .randr
                .outputs
                .iter_mut()
                .find(|o| o.crtc_id == req.crtc)
            {
                output.pending_transform = transform;
            }
            return Ok(RequestOutcome::Handled);
        }
        x11randr::RR_GET_CRTC_TRANSFORM => {
            // REQUEST_SIZE_MATCH(xRRGetCrtcTransformReq) before the lookup.
            if body.len() != 4 {
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
            let crtc = request_xid(body);
            if !crtc_exists(state, crtc) {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    RANDR_BAD_CRTC,
                    crtc,
                    u16::from(minor),
                    RANDR_MAJOR_OPCODE,
                );
            }
            let Some(output) = state.randr.outputs.iter().find(|o| o.crtc_id == crtc) else {
                return Ok(RequestOutcome::Handled);
            };
            // `transform_filter_encode`: no filter, no name and no params.
            fn part(t: &crate::randr::CrtcTransform) -> x11randr::CrtcTransformReplyPart<'_> {
                x11randr::CrtcTransformReplyPart {
                    matrix: t.matrix,
                    filter_name: t.filter.map_or(&[][..], |f| f.canonical_name().as_bytes()),
                    params: if t.filter.is_some() { &t.params } else { &[] },
                }
            }
            let buf = x11randr::encode_get_crtc_transform_reply(
                byte_order,
                sequence,
                true,
                part(&output.pending_transform),
                part(&output.current_transform),
            );
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            let _byte_order = client.byte_order;
            return Ok(write_to_client(client, client_id, &buf));
        }
        x11randr::RR_LIST_OUTPUT_PROPERTIES => {
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
        x11randr::RR_QUERY_OUTPUT_PROPERTY => {
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
                x11randr::encode_query_output_property_reply(
                    byte_order,
                    sequence,
                    false,
                    false,
                    true,
                    &[],
                )
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
        x11randr::RR_CONFIGURE_OUTPUT_PROPERTY => {
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
        x11randr::RR_CHANGE_OUTPUT_PROPERTY => {
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
            let new_value = match properties::apply_change(
                existing_value.as_ref(),
                mode,
                prop_type,
                format,
                &req.data,
            ) {
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
        x11randr::RR_DELETE_OUTPUT_PROPERTY => {
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
        x11randr::RR_GET_PANNING => {
            let crtc = request_xid(body);
            if !crtc_exists(state, crtc) {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    RANDR_BAD_CRTC,
                    crtc,
                    u16::from(minor),
                    RANDR_MAJOR_OPCODE,
                );
            }
            let timestamp = state.randr.timestamp;
            let buf = x11randr::encode_get_panning_reply(byte_order, sequence, timestamp);
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            let _byte_order = client.byte_order;
            return Ok(write_to_client(client, client_id, &buf));
        }
        x11randr::RR_SET_PANNING => {
            let Some(req) = x11randr::parse_set_panning_request(body) else {
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
            if !crtc_exists(state, req.crtc) {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    RANDR_BAD_CRTC,
                    req.crtc,
                    u16::from(minor),
                    RANDR_MAJOR_OPCODE,
                );
            }
            let status = if req.is_disabled() {
                x11randr::SET_CONFIG_SUCCESS
            } else {
                // A live panning viewport needs transformed/composited
                // scanout. Report a RANDR configuration failure rather than
                // claiming the requested geometry was installed.
                warn_randr_unsupported_once(
                    state,
                    client_id,
                    sequence,
                    minor,
                    "SetPanning",
                    "returning SetConfigFailed for active panning",
                );
                x11randr::SET_CONFIG_FAILED
            };
            let buf = x11randr::encode_set_panning_reply(
                byte_order,
                sequence,
                status,
                state.randr.timestamp,
            );
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            return Ok(write_to_client(client, client_id, &buf));
        }
        x11randr::RR_SET_OUTPUT_PRIMARY => {
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
        x11randr::RR_GET_OUTPUT_PRIMARY => {
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
            let primary = state.randr.primary_output;
            let buf = x11randr::encode_get_output_primary_reply(byte_order, sequence, primary);
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            let _byte_order = client.byte_order;
            return Ok(write_to_client(client, client_id, &buf));
        }
        x11randr::RR_GET_PROVIDERS => {
            // Xorg uses REQUEST_SIZE_MATCH for this fixed-size request.
            if body.len() != 4 {
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
            let timestamp = state.randr.timestamp;
            let providers: Vec<u32> = state
                .randr
                .providers
                .iter()
                .map(|provider| provider.provider_id)
                .collect();
            let buf =
                x11randr::encode_get_providers_reply(byte_order, sequence, timestamp, &providers);
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            let _byte_order = client.byte_order;
            return Ok(write_to_client(client, client_id, &buf));
        }
        x11randr::RR_GET_PROVIDER_INFO => {
            let Some(req) = x11randr::parse_provider_info_request(body) else {
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
            let Some(provider) = state.randr.provider(req.provider) else {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    RANDR_BAD_PROVIDER,
                    req.provider,
                    u16::from(minor),
                    RANDR_MAJOR_OPCODE,
                );
            };
            // Xorg accepts config_timestamp but does not use it for this
            // request; a live provider always reports RRSetConfigSuccess.
            let _ = req.config_timestamp;
            let associated_providers: Vec<u32> = provider
                .associations
                .iter()
                .map(|association| association.provider_id)
                .collect();
            let associated_capabilities: Vec<u32> = provider
                .associations
                .iter()
                .map(|association| association.capability)
                .collect();
            let buf = x11randr::encode_get_provider_info_reply(
                byte_order,
                sequence,
                &x11randr::ProviderInfoReply {
                    status: x11randr::SET_CONFIG_SUCCESS,
                    timestamp: state.randr.timestamp,
                    capabilities: provider.capabilities,
                    crtcs: &provider.crtcs,
                    outputs: &provider.outputs,
                    associated_providers: &associated_providers,
                    associated_capabilities: &associated_capabilities,
                    name: provider.name.as_bytes(),
                },
            );
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            return Ok(write_to_client(client, client_id, &buf));
        }
        x11randr::RR_SET_PROVIDER_OFFLOAD_SINK => {
            let Some(req) = x11randr::parse_set_provider_offload_sink_request(body) else {
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
            if let Err(error) = state
                .randr
                .validate_provider_offload_sink(req.provider, req.sink_provider)
            {
                let (error_code, error_value) = provider_relationship_protocol_error(error);
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    error_code,
                    error_value,
                    u16::from(minor),
                    RANDR_MAJOR_OPCODE,
                );
            }
            let _ = req.config_timestamp;
            warn_randr_unsupported_once(
                state,
                client_id,
                sequence,
                minor,
                "SetProviderOffloadSink",
                "rejecting a validated relationship because offload transport is unavailable",
            );
            return emit_x11_error_with_minor(
                state,
                client_id,
                sequence,
                x11::error::BAD_IMPLEMENTATION,
                0,
                u16::from(minor),
                RANDR_MAJOR_OPCODE,
            );
        }
        x11randr::RR_SET_PROVIDER_OUTPUT_SOURCE => {
            let Some(req) = x11randr::parse_set_provider_output_source_request(body) else {
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
            if let Err(error) = state
                .randr
                .validate_provider_output_source(req.provider, req.source_provider)
            {
                let (error_code, error_value) = provider_relationship_protocol_error(error);
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    error_code,
                    error_value,
                    u16::from(minor),
                    RANDR_MAJOR_OPCODE,
                );
            }
            let _ = req.config_timestamp;
            let changed = match backend.set_provider_output_source(
                state,
                req.provider,
                (req.source_provider != 0).then_some(req.source_provider),
            ) {
                Ok(changed) => changed,
                Err(err) => {
                    log::warn!(
                        "client {} #{} RANDR::SetProviderOutputSource provider={} source={} failed: {err}",
                        client_id.0,
                        sequence.0,
                        req.provider,
                        req.source_provider,
                    );
                    let (error_code, error_value) = if err.kind() == io::ErrorKind::InvalidInput {
                        (x11::error::BAD_MATCH, req.provider)
                    } else {
                        (x11::error::BAD_IMPLEMENTATION, 0)
                    };
                    return emit_x11_error_with_minor(
                        state,
                        client_id,
                        sequence,
                        error_code,
                        error_value,
                        u16::from(minor),
                        RANDR_MAJOR_OPCODE,
                    );
                }
            };
            if changed {
                crate::core_loop::run::notify_randr_provider_changed(state, req.provider);
            }
            return Ok(RequestOutcome::Handled);
        }
        x11randr::RR_GET_MONITORS => {
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
                        crate::randr::MonitorName::Output(name) => {
                            state.atoms.intern(&name, false).0
                        }
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
        x11randr::RR_SET_MONITOR => {
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
        x11randr::RR_DELETE_MONITOR => {
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
        x11randr::RR_GET_CRTC_GAMMA_SIZE => {
            let Some(req) = x11randr::parse_crtc_id_request(body) else {
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
            if !crtc_exists(state, req.crtc) {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    RANDR_BAD_CRTC,
                    req.crtc,
                    u16::from(header.data),
                    RANDR_MAJOR_OPCODE,
                );
            }
            let buf = x11randr::encode_get_crtc_gamma_size_reply(
                byte_order,
                sequence,
                backend.crtc_gamma_size(req.crtc),
            );
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            let _byte_order = client.byte_order;
            return Ok(write_to_client(client, client_id, &buf));
        }
        x11randr::RR_GET_CRTC_GAMMA => {
            let Some(req) = x11randr::parse_crtc_id_request(body) else {
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
            if !crtc_exists(state, req.crtc) {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    RANDR_BAD_CRTC,
                    req.crtc,
                    u16::from(header.data),
                    RANDR_MAJOR_OPCODE,
                );
            }
            let (red, green, blue) = backend.get_crtc_gamma(req.crtc);
            let buf =
                x11randr::encode_get_crtc_gamma_reply(byte_order, sequence, &red, &green, &blue);
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            let _byte_order = client.byte_order;
            return Ok(write_to_client(client, client_id, &buf));
        }
        x11randr::RR_SET_CRTC_GAMMA => {
            let Some(crtc_bytes) = body.get(0..4) else {
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
            let crtc = u32::from_le_bytes(crtc_bytes.try_into().unwrap());
            if !crtc_exists(state, crtc) {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    RANDR_BAD_CRTC,
                    crtc,
                    u16::from(header.data),
                    RANDR_MAJOR_OPCODE,
                );
            }
            if crtc_is_leased(state, crtc) {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_ACCESS,
                    crtc,
                    u16::from(header.data),
                    RANDR_MAJOR_OPCODE,
                );
            }
            let Some(size_bytes) = body.get(4..6) else {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_LENGTH,
                    0,
                    u16::from(header.data),
                    RANDR_MAJOR_OPCODE,
                );
            };
            let size = u16::from_le_bytes(size_bytes.try_into().unwrap());
            let expected_units = (3 * u32::from(size) + 1) >> 1;
            let expected_bytes = 8usize.saturating_add(usize::from(size).saturating_mul(6));
            if header.length_units.saturating_sub(3) < expected_units || body.len() < expected_bytes
            {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_LENGTH,
                    0,
                    u16::from(header.data),
                    RANDR_MAJOR_OPCODE,
                );
            }
            let gamma_size = backend.crtc_gamma_size(crtc);
            if size != gamma_size {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_MATCH,
                    0,
                    u16::from(header.data),
                    RANDR_MAJOR_OPCODE,
                );
            }

            let channel_bytes = usize::from(size) * 2;
            let red: Vec<u16> = body[8..8 + channel_bytes]
                .chunks_exact(2)
                .map(|chunk| u16::from_le_bytes(chunk.try_into().unwrap()))
                .collect();
            let green_start = 8 + channel_bytes;
            let green_end = green_start + channel_bytes;
            let green: Vec<u16> = body[green_start..green_end]
                .chunks_exact(2)
                .map(|chunk| u16::from_le_bytes(chunk.try_into().unwrap()))
                .collect();
            let blue: Vec<u16> = body[green_end..green_end + channel_bytes]
                .chunks_exact(2)
                .map(|chunk| u16::from_le_bytes(chunk.try_into().unwrap()))
                .collect();

            if let Err(e) = backend.set_crtc_gamma(crtc, &red, &green, &blue) {
                log::warn!("RRSetCrtcGamma apply failed for CRTC 0x{crtc:x}: {e}");
            }
            return Ok(RequestOutcome::Handled);
        }
        x11randr::RR_GET_OUTPUT_PROPERTY => {
            let Some(req) = x11randr::parse_get_output_property_request(body) else {
                let buf =
                    x11randr::encode_get_output_property_reply(byte_order, sequence, 0, 0, 0, &[]);
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
            let (served, immutable): (Option<(u32, u8, Vec<u8>)>, bool) =
                if let Some(prop) = real.as_ref() {
                    let value = if req.pending && prop.is_pending {
                        prop.pending.as_ref()
                    } else {
                        prop.current.as_ref()
                    };
                    (
                        value.map(|v| (v.r#type.0, v.format.protocol_value(), v.data.clone())),
                        prop.immutable,
                    )
                } else if let Some(syn) =
                    synthetic_output_property(state, backend, req.output, property)
                {
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
        x11randr::RR_SELECT_INPUT => {
            if let Some(req) = x11randr::parse_select_input(body) {
                if state.resources.window(ResourceId(req.window)).is_none() {
                    return emit_x11_error_with_minor(
                        state,
                        client_id,
                        sequence,
                        x11::error::BAD_WINDOW,
                        req.window,
                        u16::from(minor),
                        RANDR_MAJOR_OPCODE,
                    );
                }
                if req.enable == 0 {
                    state
                        .randr_select_masks
                        .remove(&(client_id.0, ResourceId(req.window)));
                } else {
                    state
                        .randr_select_masks
                        .insert((client_id.0, ResourceId(req.window)), req.enable);
                }
            }
        }
        x11randr::RR_GET_SCREEN_INFO => {
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
            // ProcRRGetScreenInfo (rrscreen.c:760-898) over RR10GetData.
            let has_rate = randr_client_knows_rates(state, client_id);
            let data = state.randr.rr10_data();
            let rates: Vec<Vec<u16>> = data.as_ref().map_or_else(Vec::new, |d| {
                d.sizes
                    .iter()
                    .map(|s| s.rates.iter().map(|(r, _)| *r).collect())
                    .collect()
            });
            let sizes: Vec<x11randr::ScreenInfoSize<'_>> = data
                .as_ref()
                .map(|d| {
                    d.sizes
                        .iter()
                        .zip(&rates)
                        .map(|(s, rates)| x11randr::ScreenInfoSize {
                            width: s.width,
                            height: s.height,
                            mm_width: s.mm_width,
                            mm_height: s.mm_height,
                            rates,
                        })
                        .collect()
                })
                .unwrap_or_default();
            let now = state.timestamp_now();
            #[allow(clippy::cast_possible_truncation)]
            let info = match &data {
                Some(d) => x11randr::ScreenInfoReply {
                    root: ROOT_WINDOW.0,
                    timestamp: state.randr.timestamp,
                    config_timestamp: state.randr.config_timestamp,
                    // setOfRotations is a CARD8.
                    rotations: crate::randr::SUPPORTED_ROTATIONS as u8,
                    rotation: state.randr.first_output_rotation(),
                    size_id: d.size_id,
                    rate: d.rate,
                    sizes: &sizes,
                    has_rate,
                },
                // No output with a CRTC: Rotate_0, no sizes, current time.
                None => x11randr::ScreenInfoReply {
                    root: ROOT_WINDOW.0,
                    timestamp: now,
                    config_timestamp: now,
                    rotations: crate::randr::RR_ROTATE_0 as u8,
                    rotation: crate::randr::RR_ROTATE_0,
                    size_id: 0,
                    rate: 0,
                    sizes: &[],
                    has_rate,
                },
            };
            let buf = x11randr::encode_get_screen_info_reply(byte_order, sequence, &info);
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            let _byte_order = client.byte_order;
            return Ok(write_to_client(client, client_id, &buf));
        }
        x11randr::RR_SET_SCREEN_SIZE => {
            let Some(req) = x11randr::parse_set_screen_size_request(body) else {
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
            if state.resources.window(ResourceId(req.window)).is_none() {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_WINDOW,
                    req.window,
                    u16::from(minor),
                    RANDR_MAJOR_OPCODE,
                );
            }
            // Validation order matches rrscreen.c: width range →
            // height range → crop → zero-mm. errorValue is the
            // offending dimension (width vs height reported
            // separately), not always width.
            let (min_w, min_h, max_w, max_h) = state.randr.screen_size_range();
            if req.width < min_w || req.width > max_w {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_VALUE,
                    u32::from(req.width),
                    u16::from(header.data),
                    RANDR_MAJOR_OPCODE,
                );
            }
            if req.height < min_h || req.height > max_h {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_VALUE,
                    u32::from(req.height),
                    u16::from(header.data),
                    RANDR_MAJOR_OPCODE,
                );
            }
            if state.randr.screen_size_would_crop(req.width, req.height) {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_MATCH,
                    0,
                    u16::from(header.data),
                    RANDR_MAJOR_OPCODE,
                );
            }
            if req.mm_width == 0 || req.mm_height == 0 {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_VALUE,
                    0,
                    u16::from(header.data),
                    RANDR_MAJOR_OPCODE,
                );
            }
            if let Err(e) = backend.set_logical_screen_size(req.width, req.height) {
                log::warn!("RRSetScreenSize: backend resize failed: {e}");
                // Xorg ProcRRSetScreenSize returns BadMatch when
                // RRScreenSizeSet fails (rrscreen.c). Silently
                // succeeding would make the client believe the
                // resize took. Leave prior state intact + report.
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_MATCH,
                    0,
                    u16::from(header.data),
                    RANDR_MAJOR_OPCODE,
                );
            }
            // RRScreenSizeNotify (rrscreen.c) returns before any event when
            // pixel size and mm both equal the last notified ones.
            let unchanged = (
                state.randr.screen_width,
                state.randr.screen_height,
                state.randr.width_mm,
                state.randr.height_mm,
            ) == (req.width, req.height, req.mm_width, req.mm_height);
            state
                .randr
                .set_logical_size(req.width, req.height, req.mm_width, req.mm_height);
            // Pure screen-size change: fire root ConfigureNotify +
            // ScreenChangeNotify ONLY — no per-CRTC/Output change
            // (CRTC positions are unchanged). Pass an empty changed
            // list so only ScreenChangeNotify + root ConfigureNotify fire.
            if !unchanged {
                crate::core_loop::run::apply_screen_size_side_effects(
                    state,
                    backend,
                    req.width,
                    req.height,
                    &[],
                );
            }
            backend.randr_layout_changed(state);
            // RRSetScreenSize has NO reply (it is a void request).
            return Ok(RequestOutcome::Handled);
        }
        x11randr::RR_SET_SCREEN_CONFIG => {
            return set_screen_config(state, backend, client_id, sequence, byte_order, body);
        }
        x11randr::RR_SET_CRTC_CONFIG => {
            // Body layout (post-header):
            //   crtc(4) timestamp(4) config_timestamp(4) x(2) y(2)
            //   mode(4) rotation(2) pad(2) outputs(4*N)
            // config_timestamp is parsed but NOT validated — rrcrtc.c
            // does not gate SetCrtcConfig on it (only the 1.0
            // SetScreenConfig uses InvalidConfigTime/InvalidTime).
            let Some(crtc_bytes) = body.get(0..4) else {
                return Ok(RequestOutcome::Handled);
            };
            let crtc = u32::from_le_bytes(crtc_bytes.try_into().unwrap());
            let req_timestamp = body
                .get(4..8)
                .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
                .unwrap_or(0);
            let x = body
                .get(12..14)
                .map(|b| i16::from_le_bytes(b.try_into().unwrap()))
                .unwrap_or(0);
            let y = body
                .get(14..16)
                .map(|b| i16::from_le_bytes(b.try_into().unwrap()))
                .unwrap_or(0);
            let mode = body
                .get(16..20)
                .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
                .unwrap_or(0);
            let rotation = body
                .get(20..22)
                .map(|b| u16::from_le_bytes(b.try_into().unwrap()))
                .unwrap_or(1);
            let outputs: Vec<u32> = body
                .get(24..)
                .map(|tail| {
                    tail.chunks_exact(4)
                        .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
                        .collect()
                })
                .unwrap_or_default();

            // (1) arity + output + mode resolution FIRST (Xorg rrcrtc.c order).
            let resolved = match state.randr.validate_set_crtc_config(crtc, mode, &outputs) {
                Err((code, error_value)) => {
                    return emit_x11_error_with_minor(
                        state,
                        client_id,
                        sequence,
                        code,
                        error_value,
                        u16::from(header.data),
                        RANDR_MAJOR_OPCODE,
                    );
                }
                Ok(r) => r,
            };
            // (2) rotation only when enabling.
            if resolved.is_some() {
                if !matches!(rotation & 0xf, 1 | 2 | 4 | 8) {
                    return emit_x11_error_with_minor(
                        state,
                        client_id,
                        sequence,
                        x11::error::BAD_VALUE,
                        u32::from(rotation),
                        u16::from(header.data),
                        RANDR_MAJOR_OPCODE,
                    );
                }
                if !crate::randr::SUPPORTED_ROTATIONS & rotation != 0 {
                    // `(~crtc->rotations) & rotation` (rrcrtc.c:1403).
                    return emit_x11_error_with_minor(
                        state,
                        client_id,
                        sequence,
                        x11::error::BAD_MATCH,
                        u32::from(rotation),
                        u16::from(header.data),
                        RANDR_MAJOR_OPCODE,
                    );
                }
                // No screen-bounds check: Xorg skips it for a CRTC with
                // transform support (rrcrtc.c:1436), and every yserver CRTC
                // has it; a screen may crop a CRTC.
            }

            // Resolve connector name from crtc_id (validated above →
            // guaranteed to exist).
            let output_row = state.randr.outputs.iter().find(|o| o.crtc_id == crtc);
            let Some(output_row) = output_row else {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_MATCH,
                    crtc,
                    u16::from(header.data),
                    RANDR_MAJOR_OPCODE,
                );
            };
            let output_id = output_row.output_id;
            let connector = output_row.name.clone();
            // A disable keeps the current transform (xf86RandR12CrtcSet
            // only installs one with a mode).
            let apply_transform = (resolved.is_some()
                && !output_row
                    .pending_transform
                    .equivalent(&output_row.current_transform))
            .then(|| Box::new(output_row.pending_transform.applied()));
            let apply_rotation =
                (resolved.is_some() && rotation != output_row.rotation).then_some(rotation);
            // The combined matrix drives the footprint and the scale pass; a
            // pixman overflow (Xorg's rescaled projective fallback) is not
            // rendered.
            if let Some(m) = resolved
                && crate::randr::crtc_matrix(
                    rotation,
                    m.width,
                    m.height,
                    &output_row.pending_transform.applied(),
                )
                .is_none()
            {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_MATCH,
                    u32::from(rotation),
                    u16::from(header.data),
                    RANDR_MAJOR_OPCODE,
                );
            }
            let mode_spec = resolved.map(|m| ModeSpec {
                width: m.width,
                height: m.height,
                vrefresh: m.vrefresh,
            });
            // lastSetTime = client timestamp (0/CurrentTime ⇒ server now).
            let set_time = if req_timestamp == 0 {
                state.timestamp_now()
            } else {
                req_timestamp
            };
            let output_bbox_before = crate::core_loop::run::enabled_output_bbox(state);
            let completion = CrtcConfigCompletion {
                output_id,
                set_time,
                output_bbox_before,
                byte_order,
                apply_transform,
                apply_rotation,
                reply: CrtcConfigReply::CrtcConfig,
            };
            return start_crtc_config(
                state,
                backend,
                client_id,
                sequence,
                &connector,
                mode_spec,
                (i32::from(x), i32::from(y)),
                completion,
            );
        }
        16 | 45 => {
            // TODO(unimplemented): RRCreateMode (16) / RRCreateLease (45)
            // are NOT actually implemented. This
            // BadImplementation is a STOPGAP to stop the client hanging on a
            // reply that never comes — it is NOT protocol-correct: Xorg
            // implements both and returns real data. Replace with real
            // implementations (custom modes / DRM lease).
            //
            // Void unimplemented RANDR requests remain wire-success no-ops
            // via `other` below — Xorg implements those as success, so
            // erroring them would be a new Xorg-divergence. Their first use
            // is nevertheless warned so the unsupported behavior is visible.
            return emit_x11_error_with_minor(
                state,
                client_id,
                sequence,
                x11::error::BAD_IMPLEMENTATION,
                0,
                u16::from(minor),
                RANDR_MAJOR_OPCODE,
            );
        }
        x11randr::RR_LIST_PROVIDER_PROPERTIES
        | x11randr::RR_QUERY_PROVIDER_PROPERTY
        | x11randr::RR_CONFIGURE_PROVIDER_PROPERTY
        | x11randr::RR_CHANGE_PROVIDER_PROPERTY
        | x11randr::RR_DELETE_PROVIDER_PROPERTY
        | x11randr::RR_GET_PROVIDER_PROPERTY => {
            // These requests remain deliberately unsupported, but now that
            // providers are live we must preserve Xorg's validation order:
            // request shape first, then provider lookup, then the unsupported
            // result. ChangeProviderProperty additionally validates mode and
            // format before its computed-length check and provider lookup.
            let provider = match minor {
                x11randr::RR_LIST_PROVIDER_PROPERTIES if body.len() == 4 => request_xid(body),
                x11randr::RR_QUERY_PROVIDER_PROPERTY | x11randr::RR_DELETE_PROVIDER_PROPERTY
                    if body.len() == 8 =>
                {
                    request_xid(body)
                }
                x11randr::RR_CONFIGURE_PROVIDER_PROPERTY
                    if body.len() >= 12 && body.len().is_multiple_of(4) =>
                {
                    request_xid(body)
                }
                x11randr::RR_GET_PROVIDER_PROPERTY if body.len() == 24 => request_xid(body),
                x11randr::RR_CHANGE_PROVIDER_PROPERTY => {
                    let Some(req) = x11randr::parse_change_provider_property_header(body) else {
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
                    let Some(_mode) = properties::ChangeMode::from_protocol(req.mode) else {
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
                    let expected_len = usize::try_from(req.n_units)
                        .ok()
                        .and_then(|units| units.checked_mul(format.bytes()))
                        .and_then(|bytes| bytes.checked_add(3))
                        .map(|bytes| bytes & !3)
                        .and_then(|bytes| 20usize.checked_add(bytes));
                    if expected_len != Some(body.len()) {
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
                    req.provider
                }
                _ => {
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
            };
            if state.randr.provider(provider).is_none() {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    RANDR_BAD_PROVIDER,
                    provider,
                    u16::from(minor),
                    RANDR_MAJOR_OPCODE,
                );
            }
            let request_name = match minor {
                x11randr::RR_LIST_PROVIDER_PROPERTIES => "ListProviderProperties",
                x11randr::RR_QUERY_PROVIDER_PROPERTY => "QueryProviderProperty",
                x11randr::RR_CONFIGURE_PROVIDER_PROPERTY => "ConfigureProviderProperty",
                x11randr::RR_CHANGE_PROVIDER_PROPERTY => "ChangeProviderProperty",
                x11randr::RR_DELETE_PROVIDER_PROPERTY => "DeleteProviderProperty",
                x11randr::RR_GET_PROVIDER_PROPERTY => "GetProviderProperty",
                _ => unreachable!(),
            };
            warn_randr_unsupported_once(
                state,
                client_id,
                sequence,
                minor,
                request_name,
                "rejecting request because provider properties are unavailable",
            );
            return emit_x11_error_with_minor(
                state,
                client_id,
                sequence,
                x11::error::BAD_IMPLEMENTATION,
                0,
                u16::from(minor),
                RANDR_MAJOR_OPCODE,
            );
        }
        46 => {
            // CreateLease is not implemented and cannot create a live lease,
            // so FreeLease always follows Xorg's failed lease lookup.
            //
            // That lookup yields plain `BadValue`, NOT `BadRRLease`. Xorg
            // registers RRLeaseType with `CreateNewResourceType` (rrlease.c)
            // and never calls `SetResourceTypeErrorValue` for it, so the type
            // keeps dix's default `errorValue = BadValue` (dix/resource.c);
            // `BadRRLease` is defined in randr.h but referenced nowhere in the
            // Xorg tree. Measured on real Xorg with `tools/randr-probe`:
            // RRFreeLease(bogus) -> code=2 (BadValue), not 151.
            let lease = body
                .get(0..4)
                .map(|bytes| u32::from_le_bytes(bytes.try_into().unwrap()))
                .unwrap_or(0);
            return emit_x11_error_with_minor(
                state,
                client_id,
                sequence,
                x11::error::BAD_VALUE,
                lease,
                u16::from(minor),
                RANDR_MAJOR_OPCODE,
            );
        }
        // 1 RROldGetScreenInfo and 3 RROldScreenChangeSelectInput are literal
        // NULL entries in Xorg's ProcRandrVector (randr/rrdispatch.c), and
        // ProcRRDispatch rejects a NULL slot exactly like an out-of-range
        // minor: `if (stuff->data >= RRNumberRequests ||
        // !ProcRandrVector[stuff->data]) return BadRequest;` (randr/randr.c).
        // They are request numbers that were never assigned, not requests we
        // have yet to write.
        1 | 3 => {
            return emit_x11_error_with_minor(
                state,
                client_id,
                sequence,
                x11::error::BAD_REQUEST,
                0,
                u16::from(minor),
                header.opcode,
            );
        }
        other if other >= RANDR_REQUEST_COUNT => {
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
        other => {
            let request_name = match other {
                17 => "DestroyMode",
                18 => "AddOutputMode",
                19 => "DeleteOutputMode",
                _ => "known request",
            };
            warn_randr_unsupported_once(
                state,
                client_id,
                sequence,
                other,
                request_name,
                "accepting void request as a compatibility no-op",
            );
        }
    }
    Ok(RequestOutcome::Handled)
}

/// `ProcRRSetScreenConfig` (rrscreen.c:903-1130): the RANDR 1.0 size,
/// rotation and rate, applied to `RRFirstOutput`'s CRTC at 0,0 through the
/// SetCrtcConfig path.
fn set_screen_config(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    client_id: ClientId,
    sequence: SequenceNumber,
    byte_order: yserver_protocol::x11::ClientByteOrder,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    use yserver_protocol::x11::randr as x11randr;
    const STATUS_INVALID_CONFIG_TIME: u8 = 1;
    const STATUS_INVALID_TIME: u8 = 2;
    const STATUS_FAILED: u8 = 3;
    let error = |state: &mut ServerState, code: u8, value: u32| {
        emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            code,
            value,
            u16::from(x11randr::RR_SET_SCREEN_CONFIG),
            128,
        )
    };
    let status_reply = |state: &mut ServerState, status: u8| {
        let reply = x11randr_encode_set_screen_config(state, byte_order, sequence, status);
        let Some(client) = state.clients.get_mut(&client_id.0) else {
            return Ok(RequestOutcome::Handled);
        };
        Ok(write_to_client(client, client_id, &reply))
    };
    // REQUEST_SIZE_MATCH: the rate field exists only for a 1.1+ client.
    let has_rate = randr_client_knows_rates(state, client_id);
    if body.len() != if has_rate { 20 } else { 16 } {
        return error(state, x11::error::BAD_LENGTH, 0);
    }
    // A DRAWABLE, not a window (`dixLookupDrawable`): a pixmap resolves to
    // its screen, and an unknown xid is BadDrawable (measured with
    // `tools/randr-probe`).
    let drawable = u32::from_le_bytes(body[0..4].try_into().unwrap());
    let id = ResourceId(drawable);
    if state.resources.window(id).is_none() && state.resources.pixmap(id).is_none() {
        return error(state, x11::error::BAD_DRAWABLE, drawable);
    }
    let u16_at = |o: usize| u16::from_le_bytes([body[o], body[o + 1]]);
    let u32_at = |o: usize| u32::from_le_bytes(body[o..o + 4].try_into().unwrap());
    let (timestamp, config_timestamp) = (u32_at(4), u32_at(8));
    let (size_id, rotation) = (u16_at(12), u16_at(14));
    let rate = if has_rate { u16_at(16) } else { 0 };
    // ClientTimeToServerTime: CurrentTime is now.
    let time = if timestamp == 0 {
        state.timestamp_now()
    } else {
        timestamp
    };
    let Some(data) = state.randr.rr10_data() else {
        return status_reply(state, STATUS_FAILED);
    };
    if config_timestamp != state.randr.config_timestamp {
        return status_reply(state, STATUS_INVALID_CONFIG_TIME);
    }
    let Some(size) = data.sizes.get(usize::from(size_id)) else {
        return error(state, x11::error::BAD_VALUE, u32::from(size_id));
    };
    if !matches!(rotation & 0xf, 1 | 2 | 4 | 8) {
        return error(state, x11::error::BAD_VALUE, u32::from(rotation));
    }
    if !crate::randr::SUPPORTED_ROTATIONS & rotation != 0 {
        return error(state, x11::error::BAD_MATCH, u32::from(rotation));
    }
    let mode_id = if rate == 0 {
        size.rates.first().map(|&(_, mode)| mode)
    } else {
        let Some(&(_, mode)) = size.rates.iter().find(|&&(r, _)| r == rate) else {
            return error(state, x11::error::BAD_VALUE, u32::from(rate));
        };
        Some(mode)
    };
    let Some(mode) = mode_id.and_then(|id| {
        state
            .randr
            .mode_table
            .iter()
            .find(|m| m.mode_id == id)
            .copied()
    }) else {
        return status_reply(state, STATUS_FAILED);
    };
    if time < state.randr.timestamp {
        return status_reply(state, STATUS_INVALID_TIME);
    }
    let (min_w, min_h, max_w, max_h) = state.randr.screen_size_range();
    if !(min_w..=max_w).contains(&mode.width) {
        return error(state, x11::error::BAD_VALUE, u32::from(mode.width));
    }
    if !(min_h..=max_h).contains(&mode.height) {
        return error(state, x11::error::BAD_VALUE, u32::from(mode.height));
    }
    let (width, height) = if crate::randr::rotation_swaps_axes(rotation) {
        (mode.height, mode.width)
    } else {
        (mode.width, mode.height)
    };
    let Some(target) = state
        .randr
        .outputs
        .iter()
        .find(|o| o.output_id == data.output_id)
        .cloned()
    else {
        return status_reply(state, STATUS_FAILED);
    };
    if (width, height) != (state.randr.screen_width, state.randr.screen_height) {
        // Every other CRTC goes off and the screen takes the new size, mm
        // unchanged. The first output's own CRTC is reconfigured in place
        // below rather than lit off and on.
        let others: Vec<(u32, String)> = state
            .randr
            .enabled_outputs()
            .filter(|o| o.output_id != target.output_id)
            .map(|o| (o.output_id, o.name.clone()))
            .collect();
        let mut disabled = Vec::new();
        for (output_id, name) in others {
            match backend.begin_crtc_config(output_id, &name, None, 0, 0) {
                Ok(CrtcConfigApply::Applied(_)) => disabled.push(output_id),
                Ok(CrtcConfigApply::Pending(_)) | Err(_) => {
                    return status_reply(state, STATUS_FAILED);
                }
            }
        }
        if !disabled.is_empty() {
            let changed: Vec<(u32, u32, u32)> = state
                .randr
                .outputs
                .iter()
                .filter(|o| disabled.contains(&o.output_id))
                .map(|o| (o.output_id, o.crtc_id, 0))
                .collect();
            backend.refresh_randr_state_set_time(state, time);
            backend.randr_layout_changed(state);
            crate::core_loop::run::emit_randr_change_notifications(state, &changed);
        }
        if let Err(e) = backend.set_logical_screen_size(width, height) {
            log::warn!("RRSetScreenConfig: backend resize failed: {e}");
            return status_reply(state, STATUS_FAILED);
        }
        let (mm_w, mm_h) = (state.randr.width_mm, state.randr.height_mm);
        state.randr.set_logical_size(width, height, mm_w, mm_h);
        crate::core_loop::run::apply_screen_size_side_effects(state, backend, width, height, &[]);
        backend.randr_layout_changed(state);
    }
    // RRCrtcSet(crtc, mode, 0, 0, rotation, 1, &output) with the pending
    // client transform, as SetCrtcConfig.
    let Some(target) = state
        .randr
        .outputs
        .iter()
        .find(|o| o.output_id == data.output_id)
        .cloned()
    else {
        return status_reply(state, STATUS_FAILED);
    };
    let pending = target.pending_transform.applied();
    if crate::randr::crtc_matrix(rotation, mode.width, mode.height, &pending).is_none() {
        return status_reply(state, STATUS_FAILED);
    }
    let completion = CrtcConfigCompletion {
        output_id: target.output_id,
        set_time: time,
        output_bbox_before: crate::core_loop::run::enabled_output_bbox(state),
        byte_order,
        apply_transform: (!target
            .pending_transform
            .equivalent(&target.current_transform))
        .then(|| Box::new(pending)),
        apply_rotation: (rotation != target.rotation).then_some(rotation),
        reply: CrtcConfigReply::ScreenConfig,
    };
    let mode_spec = ModeSpec {
        width: mode.width,
        height: mode.height,
        vrefresh: mode.vrefresh,
    };
    let connector = target.name.clone();
    start_crtc_config(
        state,
        backend,
        client_id,
        sequence,
        &connector,
        Some(mode_spec),
        (0, 0),
        completion,
    )
}

/// Hand one CRTC configuration to the backend and complete it now, or park
/// it until an asynchronous qualification finishes.
#[allow(clippy::too_many_arguments)]
fn start_crtc_config(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    client_id: ClientId,
    sequence: SequenceNumber,
    connector: &str,
    mode_spec: Option<ModeSpec>,
    (x, y): (i32, i32),
    completion: CrtcConfigCompletion,
) -> io::Result<RequestOutcome> {
    match backend.begin_crtc_config(completion.output_id, connector, mode_spec, x, y) {
        Ok(CrtcConfigApply::Applied(changed)) => {
            complete_crtc_config(state, backend, client_id, sequence, completion, Ok(changed))
        }
        Ok(CrtcConfigApply::Pending(token)) => {
            Ok(RequestOutcome::PendingCrtcConfig(PendingCrtcConfig {
                token,
                completion,
            }))
        }
        Err(e) => complete_crtc_config(state, backend, client_id, sequence, completion, Err(e)),
    }
}

/// `RRClientKnowsRates` (rrdispatch.c:27): the client's QueryVersion was
/// 1.1 or newer.
fn randr_client_knows_rates(state: &ServerState, client_id: ClientId) -> bool {
    state
        .randr_client_versions
        .get(&client_id)
        .is_some_and(|&version| version >= (1, 1))
}

/// Complete the protocol-visible half of `RRSetCrtcConfig` after either a
/// synchronous apply or an asynchronous backend result. Keeping this as one
/// continuation prevents the async path from redispatching validation or
/// accidentally diverging in notification/timestamp behavior.
pub(crate) fn complete_crtc_config(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    client_id: ClientId,
    sequence: SequenceNumber,
    completion: CrtcConfigCompletion,
    result: io::Result<bool>,
) -> io::Result<RequestOutcome> {
    let status = match result {
        // A new transform is a change even with identical mode/x/y.
        Ok(changed)
            if changed
                || completion.apply_transform.is_some()
                || completion.apply_rotation.is_some() =>
        {
            // Something actually changed. Single rebuild path: a CRTC set
            // bumps lastSetTime (to the client timestamp) but NOT
            // lastConfigTime.
            backend.refresh_randr_state_set_time(state, completion.set_time);
            if let Some(transform) = completion.apply_transform
                && let Some(output) = state
                    .randr
                    .outputs
                    .iter_mut()
                    .find(|o| o.output_id == completion.output_id)
            {
                // RRCrtcNotify: RRTransformCopy of pending into current.
                output.current_transform = *transform;
            }
            if let Some(rotation) = completion.apply_rotation
                && let Some(output) = state
                    .randr
                    .outputs
                    .iter_mut()
                    .find(|o| o.output_id == completion.output_id)
            {
                output.rotation = rotation;
            }
            backend.randr_layout_changed(state);
            let changed: Vec<(u32, u32, u32)> = state
                .randr
                .outputs
                .iter()
                .find(|o| o.output_id == completion.output_id)
                .map(|o| (o.output_id, o.crtc_id, o.mode_id))
                .into_iter()
                .collect();
            crate::core_loop::run::emit_randr_change_notifications(state, &changed);
            crate::core_loop::run::emit_screen_resize_window_notifications_if_outputs_caught_up(
                state,
                completion.output_bbox_before,
            );
            0
        }
        Ok(_) => {
            // A no-op succeeds without a rebuild or change notification.
            0
        }
        Err(e) => {
            log::warn!("RRSetCrtcConfig apply failed: {e}");
            // RRSetConfigFailed=3 (a status reply, not a protocol error).
            3
        }
    };
    if completion.reply == CrtcConfigReply::ScreenConfig {
        if status == 0 {
            state.randr.timestamp = completion.set_time;
        }
        let reply =
            x11randr_encode_set_screen_config(state, completion.byte_order, sequence, status);
        let Some(client) = state.clients.get_mut(&client_id.0) else {
            return Ok(RequestOutcome::Handled);
        };
        return Ok(write_to_client(client, client_id, &reply));
    }
    let timestamp = state.randr.timestamp;
    reply_set_crtc_config(
        state,
        client_id,
        sequence,
        completion.byte_order,
        status,
        timestamp,
    )
}

/// The `SetScreenConfig` reply for `status`: `lastSetTime`,
/// `lastConfigTime` and the root, as they stand.
fn x11randr_encode_set_screen_config(
    state: &ServerState,
    byte_order: yserver_protocol::x11::ClientByteOrder,
    sequence: SequenceNumber,
    status: u8,
) -> Vec<u8> {
    yserver_protocol::x11::randr::encode_set_screen_config_reply(
        byte_order,
        sequence,
        status,
        state.randr.timestamp,
        state.randr.config_timestamp,
        ROOT_WINDOW.0,
    )
}

/// Build and send the 32-byte `SetCrtcConfig` reply.
///
/// Wire format: `status` (data byte) + length=0 + `new_timestamp`(4) +
/// pad(20). On success `new_timestamp` is the post-set `state.randr.timestamp`;
/// on failure it is the unmodified existing value (caller resolves which).
fn reply_set_crtc_config(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    byte_order: yserver_protocol::x11::ClientByteOrder,
    status: u8,
    new_timestamp: u32,
) -> io::Result<RequestOutcome> {
    let mut reply = x11::fixed_reply(byte_order, sequence, status, 0);
    x11::write_u32(byte_order, &mut reply, new_timestamp);
    reply.extend_from_slice(&[0u8; 20]);
    debug_assert_eq!(reply.len(), 32);
    debug!(
        "client {} #{} RANDR::SetCrtcConfig -> status={} new_timestamp={}",
        client_id.0, sequence.0, status, new_timestamp,
    );
    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return Ok(RequestOutcome::Handled);
    };
    Ok(write_to_client(client, client_id, &reply))
}

pub(super) fn handle_xinerama_request(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    use yserver_protocol::x11::{ClientByteOrder, error, read_u32, xinerama as xin};

    const XINERAMA_MAJOR_OPCODE: u8 = 151;

    let byte_order = state
        .clients
        .get(&client_id.0)
        .map_or(ClientByteOrder::LittleEndian, |c| c.byte_order);
    let minor = header.data;

    // RRXineramaScreenCount counts every monitor (`RRMonitorCountList`,
    // get_active FALSE); QueryScreens lists only the non-empty ones
    // (rrxinerama.c) — a 0x0 client monitor is counted but not listed.
    let screen_count = active_monitors(state, false).len();
    let screens: Vec<xin::ScreenInfo> = active_monitors(state, true)
        .into_iter()
        .map(|monitor| xin::ScreenInfo {
            x_org: monitor.x,
            y_org: monitor.y,
            width: monitor.width,
            height: monitor.height,
        })
        .collect();

    macro_rules! require_len {
        ($n:expr) => {
            if body.len() != $n {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    error::BAD_LENGTH,
                    0,
                    u16::from(minor),
                    XINERAMA_MAJOR_OPCODE,
                );
            }
        };
    }

    let buf = match minor {
        xin::QUERY_VERSION => {
            require_len!(4);
            xin::encode_query_version_reply(byte_order, sequence)
        }
        xin::IS_ACTIVE => {
            require_len!(0);
            xin::encode_is_active_reply(byte_order, sequence, screen_count > 0)
        }
        xin::QUERY_SCREENS => {
            require_len!(0);
            xin::encode_query_screens_reply(byte_order, sequence, &screens)
        }
        xin::GET_STATE => {
            require_len!(4);
            let window = read_u32(byte_order, body);
            if state.resources.window(ResourceId(window)).is_none() {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    error::BAD_WINDOW,
                    window,
                    u16::from(minor),
                    XINERAMA_MAJOR_OPCODE,
                );
            }
            xin::encode_get_state_reply(byte_order, sequence, true, window)
        }
        xin::GET_SCREEN_COUNT => {
            require_len!(4);
            let window = read_u32(byte_order, body);
            if state.resources.window(ResourceId(window)).is_none() {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    error::BAD_WINDOW,
                    window,
                    u16::from(minor),
                    XINERAMA_MAJOR_OPCODE,
                );
            }
            #[allow(clippy::cast_possible_truncation)]
            let count = screen_count as u8;
            xin::encode_get_screen_count_reply(byte_order, sequence, count, window)
        }
        xin::GET_SCREEN_SIZE => {
            require_len!(8);
            let window = read_u32(byte_order, body);
            let screen = read_u32(byte_order, &body[4..]);
            if state.resources.window(ResourceId(window)).is_none() {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    error::BAD_WINDOW,
                    window,
                    u16::from(minor),
                    XINERAMA_MAJOR_OPCODE,
                );
            }
            xin::encode_get_screen_size_reply(
                byte_order,
                sequence,
                u32::from(state.randr.screen_width),
                u32::from(state.randr.screen_height),
                window,
                screen,
            )
        }
        _ => {
            return emit_x11_error_with_minor(
                state,
                client_id,
                sequence,
                error::BAD_REQUEST,
                0,
                u16::from(minor),
                XINERAMA_MAJOR_OPCODE,
            );
        }
    };

    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return Ok(RequestOutcome::Handled);
    };
    Ok(write_to_client(client, client_id, &buf))
}
