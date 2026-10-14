use crate::{
    backend::{Backend, CrtcConfigApply, ModeSpec},
    core_loop::process_request::{
        RequestOutcome, emit_x11_error_with_minor,
        randr_ext::{
            CrtcConfigCompletion, CrtcConfigReply, RANDR_MAJOR_OPCODE, crtc::start_crtc_config,
            request_xid,
        },
        write_to_client,
    },
    resources::ROOT_WINDOW,
    server::ServerState,
};
use std::io;
use yserver_protocol::{
    x11,
    x11::{
        ClientByteOrder, ClientId, RequestHeader, ResourceId, SequenceNumber, randr as x11randr,
    },
};

#[allow(clippy::needless_return)]
pub(super) fn handle_randr_query_version(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    byte_order: ClientByteOrder,
) -> io::Result<RequestOutcome> {
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
    let buf = x11randr::encode_query_version_reply(byte_order, sequence, reply_major, reply_minor);
    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return Ok(RequestOutcome::Handled);
    };
    let _byte_order = client.byte_order;
    return Ok(write_to_client(client, client_id, &buf));
}

#[allow(clippy::needless_return)]
pub(super) fn handle_randr_get_screen_size_range(
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

#[allow(clippy::needless_return)]
pub(super) fn handle_randr_get_screen_resources(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
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
    let buf = x11randr::encode_get_screen_resources_current_reply(byte_order, sequence, &resources);
    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return Ok(RequestOutcome::Handled);
    };
    let _byte_order = client.byte_order;
    return Ok(write_to_client(client, client_id, &buf));
}

#[allow(clippy::needless_return)]
pub(super) fn handle_randr_get_screen_resources_current(
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
    let resources = state.randr.screen_resources_current();
    let buf = x11randr::encode_get_screen_resources_current_reply(byte_order, sequence, &resources);
    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return Ok(RequestOutcome::Handled);
    };
    let _byte_order = client.byte_order;
    return Ok(write_to_client(client, client_id, &buf));
}

pub(super) fn handle_randr_select_input(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    minor: u8,
) -> io::Result<RequestOutcome> {
    {
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
    Ok(RequestOutcome::Handled)
}

#[allow(clippy::needless_return)]
pub(super) fn handle_randr_get_screen_info(
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
            rotations: state.randr.limits.rotations as u8,
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

#[allow(clippy::needless_return)]
pub(super) fn handle_randr_set_screen_size(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
    minor: u8,
) -> io::Result<RequestOutcome> {
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

/// `ProcRRSetScreenConfig` (rrscreen.c:903-1130): the RANDR 1.0 size,
/// rotation and rate, applied to `RRFirstOutput`'s CRTC at 0,0 through the
/// SetCrtcConfig path.
pub(super) fn set_screen_config(
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
    if !state.randr.limits.rotations & rotation != 0 {
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

/// `RRClientKnowsRates` (rrdispatch.c:27): the client's QueryVersion was
/// 1.1 or newer.
fn randr_client_knows_rates(state: &ServerState, client_id: ClientId) -> bool {
    state
        .randr_client_versions
        .get(&client_id)
        .is_some_and(|&version| version >= (1, 1))
}

/// The `SetScreenConfig` reply for `status`: `lastSetTime`,
/// `lastConfigTime` and the root, as they stand.
pub(super) fn x11randr_encode_set_screen_config(
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
