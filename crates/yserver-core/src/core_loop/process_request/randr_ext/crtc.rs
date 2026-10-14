use crate::{
    backend::{Backend, CrtcConfigApply, ModeSpec},
    core_loop::process_request::{
        RANDR_BAD_CRTC, RequestOutcome, emit_x11_error_with_minor,
        randr_ext::{
            CrtcConfigCompletion, CrtcConfigReply, PendingCrtcConfig, RANDR_MAJOR_OPCODE,
            request_xid, screen::x11randr_encode_set_screen_config, warn_randr_unsupported_once,
        },
        write_to_client,
    },
    server::ServerState,
};
use log::debug;
use std::io;
use yserver_protocol::{
    x11,
    x11::{ClientByteOrder, ClientId, RequestHeader, SequenceNumber, randr as x11randr},
};

fn crtc_is_leased(_state: &ServerState, _crtc: u32) -> bool {
    false
}

fn crtc_exists(state: &ServerState, crtc: u32) -> bool {
    state
        .randr
        .outputs
        .iter()
        .any(|output| output.crtc_id == crtc)
}

#[allow(clippy::needless_return)]
pub(super) fn handle_randr_get_crtc_info(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
    byte_order: ClientByteOrder,
) -> io::Result<RequestOutcome> {
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
            rotations: state.randr.limits.rotations,
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

#[allow(clippy::needless_return)]
pub(super) fn handle_randr_set_crtc_transform(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    minor: u8,
) -> io::Result<RequestOutcome> {
    // ProcRRSetCrtcTransform (rrcrtc.c:1755-1785) + RRCrtcTransformSet
    // (rrcrtc.c:1091-1128), then the spec's D2 contract.
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
    // `!crtc->transforms` (rrcrtc.c:1088).
    if !state.randr.limits.transforms {
        return error(state, x11::error::BAD_VALUE, 0);
    }
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
    let Some(transform) = crate::randr::CrtcTransform::new(req.transform, filter, spec.params)
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

#[allow(clippy::needless_return)]
pub(super) fn handle_randr_get_crtc_transform(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    byte_order: ClientByteOrder,
    minor: u8,
) -> io::Result<RequestOutcome> {
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
        state.randr.limits.transforms,
        part(&output.pending_transform),
        part(&output.current_transform),
    );
    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return Ok(RequestOutcome::Handled);
    };
    let _byte_order = client.byte_order;
    return Ok(write_to_client(client, client_id, &buf));
}

#[allow(clippy::needless_return)]
pub(super) fn handle_randr_get_panning(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    byte_order: ClientByteOrder,
    minor: u8,
) -> io::Result<RequestOutcome> {
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

#[allow(clippy::needless_return)]
pub(super) fn handle_randr_set_panning(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    byte_order: ClientByteOrder,
    minor: u8,
) -> io::Result<RequestOutcome> {
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
    let buf =
        x11randr::encode_set_panning_reply(byte_order, sequence, status, state.randr.timestamp);
    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return Ok(RequestOutcome::Handled);
    };
    return Ok(write_to_client(client, client_id, &buf));
}

#[allow(clippy::needless_return)]
pub(super) fn handle_randr_get_crtc_gamma_size(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
    byte_order: ClientByteOrder,
) -> io::Result<RequestOutcome> {
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

#[allow(clippy::needless_return)]
pub(super) fn handle_randr_get_crtc_gamma(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
    byte_order: ClientByteOrder,
) -> io::Result<RequestOutcome> {
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
    let buf = x11randr::encode_get_crtc_gamma_reply(byte_order, sequence, &red, &green, &blue);
    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return Ok(RequestOutcome::Handled);
    };
    let _byte_order = client.byte_order;
    return Ok(write_to_client(client, client_id, &buf));
}

#[allow(clippy::needless_return)]
pub(super) fn handle_randr_set_crtc_gamma(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<RequestOutcome> {
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
    if header.length_units.saturating_sub(3) < expected_units || body.len() < expected_bytes {
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

#[allow(clippy::needless_return)]
pub(super) fn handle_randr_set_crtc_config(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
    byte_order: ClientByteOrder,
) -> io::Result<RequestOutcome> {
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
        if !state.randr.limits.rotations & rotation != 0 {
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
        // The screen bounds bind only a CRTC without transform support
        // (rrcrtc.c:1420-1457); with it, a screen may crop a CRTC.
        if let Some(m) = resolved
            && !state.randr.limits.transforms
        {
            let (width, height) = if crate::randr::rotation_swaps_axes(rotation) {
                (m.height, m.width)
            } else {
                (m.width, m.height)
            };
            for (origin, size, screen) in [
                (x, width, state.randr.screen_width),
                (y, height, state.randr.screen_height),
            ] {
                if i32::from(origin) + i32::from(size) > i32::from(screen) {
                    return emit_x11_error_with_minor(
                        state,
                        client_id,
                        sequence,
                        x11::error::BAD_VALUE,
                        i32::from(origin).cast_unsigned(),
                        u16::from(header.data),
                        RANDR_MAJOR_OPCODE,
                    );
                }
            }
        }
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

/// Hand one CRTC configuration to the backend and complete it now, or park
/// it until an asynchronous qualification finishes.
#[allow(clippy::too_many_arguments)]
pub(super) fn start_crtc_config(
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
