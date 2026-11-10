use crate::{
    access::{AccessMode, RequestOpcodes, ResourceKind},
    backend::Backend,
    core_loop::process_request::{
        RANDR_BAD_PROVIDER, RequestOutcome, check_resource, emit_x11_error_with_minor,
        randr_ext::{RANDR_MAJOR_OPCODE, request_xid, warn_randr_unsupported_once},
        write_to_client,
    },
    properties,
    server::ServerState,
};
use std::io;
use yserver_protocol::{
    x11,
    x11::{ClientByteOrder, ClientId, ResourceId, SequenceNumber, randr as x11randr},
};

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
        | crate::randr::ProviderRelationshipError::NotGpuProvider(_) => (x11::error::BAD_VALUE, 0),
    }
}

#[allow(clippy::needless_return)]
pub(super) fn handle_randr_get_providers(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    byte_order: ClientByteOrder,
    minor: u8,
) -> io::Result<RequestOutcome> {
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
    let timestamp = state.randr.timestamp;
    let providers: Vec<u32> = state
        .randr
        .providers
        .iter()
        .map(|provider| provider.provider_id)
        .collect();
    let buf = x11randr::encode_get_providers_reply(byte_order, sequence, timestamp, &providers);
    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return Ok(RequestOutcome::Handled);
    };
    let _byte_order = client.byte_order;
    return Ok(write_to_client(client, client_id, &buf));
}

#[allow(clippy::needless_return)]
pub(super) fn handle_randr_get_provider_info(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    byte_order: ClientByteOrder,
    minor: u8,
) -> io::Result<RequestOutcome> {
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

#[allow(clippy::needless_return)]
pub(super) fn handle_randr_set_provider_offload_sink(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    minor: u8,
) -> io::Result<RequestOutcome> {
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

#[allow(clippy::needless_return)]
pub(super) fn handle_randr_set_provider_output_source(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    minor: u8,
) -> io::Result<RequestOutcome> {
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

#[allow(clippy::needless_return)]
pub(super) fn handle_randr_provider_property(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    minor: u8,
) -> io::Result<RequestOutcome> {
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
