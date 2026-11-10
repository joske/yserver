//! RANDR and Xinerama requests: `handle_randr_request` hands each RANDR
//! minor to its own `handle_randr_*` fn. This file holds the CRTC-config
//! continuation types, the helpers several groups share and the
//! CreateMode/CreateLease and FreeLease stubs; the rest lives in:
//!
//! - `screen`: version, SelectInput, screen info, size and resources (0,
//!   4–8, 25), RANDR 1.0 SetScreenConfig (2).
//! - `output`: output info and primary (9, 30, 31), output properties
//!   (10–15).
//! - `crtc`: CRTC info and config (20, 21), gamma (22–24), transforms
//!   (26, 27), panning (28, 29), the asynchronous CRTC-config
//!   start/complete/reply.
//! - `provider`: providers (32–35), provider properties (36–41).
//! - `monitor`: monitors (42–44).
//! - `xinerama`: the Xinerama extension.

mod crtc;
mod monitor;
mod output;
mod provider;
mod screen;
mod xinerama;

use crate::{
    access::{AccessMode, RequestOpcodes, ResourceKind},
    backend::{Backend, CrtcConfigToken},
    core_loop::process_request::{
        RANDR_REQUEST_COUNT, RequestOutcome, check_resource, emit_x11_error_with_minor,
        randr_ext::{
            monitor::{
                handle_randr_delete_monitor, handle_randr_get_monitors, handle_randr_set_monitor,
            },
            output::{
                handle_randr_change_output_property, handle_randr_configure_output_property,
                handle_randr_delete_output_property, handle_randr_get_output_info,
                handle_randr_get_output_primary, handle_randr_get_output_property,
                handle_randr_list_output_properties, handle_randr_query_output_property,
                handle_randr_set_output_primary,
            },
            provider::{
                handle_randr_get_provider_info, handle_randr_get_providers,
                handle_randr_provider_property, handle_randr_set_provider_offload_sink,
                handle_randr_set_provider_output_source,
            },
            screen::{
                handle_randr_get_screen_info, handle_randr_get_screen_resources,
                handle_randr_get_screen_resources_current, handle_randr_get_screen_size_range,
                handle_randr_query_version, handle_randr_select_input,
                handle_randr_set_screen_size, set_screen_config,
            },
        },
    },
    server::ServerState,
};
pub(crate) use crtc::*;
use log::debug;
use std::io;
pub(super) use xinerama::*;
use yserver_protocol::{
    x11,
    x11::{ClientByteOrder, ClientId, RequestHeader, SequenceNumber, randr as x11randr},
};

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

const RANDR_MAJOR_OPCODE: u8 = 128;

fn request_xid(body: &[u8]) -> u32 {
    body.get(0..4)
        .map(|bytes| u32::from_le_bytes(bytes.try_into().unwrap()))
        .unwrap_or(0)
}

pub(super) fn handle_randr_request(
    state: &mut ServerState,
    backend: &mut dyn Backend,
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
    match minor {
        x11randr::RR_QUERY_VERSION => {
            handle_randr_query_version(state, client_id, sequence, body, byte_order)
        }
        x11randr::RR_GET_SCREEN_SIZE_RANGE => {
            handle_randr_get_screen_size_range(state, client_id, sequence, body, byte_order, minor)
        }
        x11randr::RR_GET_SCREEN_RESOURCES => handle_randr_get_screen_resources(
            state, backend, client_id, sequence, header, body, byte_order, minor,
        ),
        x11randr::RR_GET_SCREEN_RESOURCES_CURRENT => handle_randr_get_screen_resources_current(
            state, client_id, sequence, body, byte_order, minor,
        ),
        x11randr::RR_GET_OUTPUT_INFO => {
            handle_randr_get_output_info(state, client_id, sequence, header, body, byte_order)
        }
        x11randr::RR_GET_CRTC_INFO => {
            handle_randr_get_crtc_info(state, client_id, sequence, header, body, byte_order)
        }
        x11randr::RR_SET_CRTC_TRANSFORM => {
            handle_randr_set_crtc_transform(state, client_id, sequence, body, minor)
        }
        x11randr::RR_GET_CRTC_TRANSFORM => {
            handle_randr_get_crtc_transform(state, client_id, sequence, body, byte_order, minor)
        }
        x11randr::RR_LIST_OUTPUT_PROPERTIES => handle_randr_list_output_properties(
            state, backend, client_id, sequence, body, byte_order, minor,
        ),
        x11randr::RR_QUERY_OUTPUT_PROPERTY => handle_randr_query_output_property(
            state, backend, client_id, sequence, body, byte_order, minor,
        ),
        x11randr::RR_CONFIGURE_OUTPUT_PROPERTY => {
            handle_randr_configure_output_property(state, client_id, sequence, body, minor)
        }
        x11randr::RR_CHANGE_OUTPUT_PROPERTY => {
            handle_randr_change_output_property(state, client_id, sequence, body, minor)
        }
        x11randr::RR_DELETE_OUTPUT_PROPERTY => {
            handle_randr_delete_output_property(state, client_id, sequence, body, minor)
        }
        x11randr::RR_GET_PANNING => {
            handle_randr_get_panning(state, client_id, sequence, body, byte_order, minor)
        }
        x11randr::RR_SET_PANNING => {
            handle_randr_set_panning(state, client_id, sequence, body, byte_order, minor)
        }
        x11randr::RR_SET_OUTPUT_PRIMARY => {
            handle_randr_set_output_primary(state, client_id, sequence, body, minor)
        }
        x11randr::RR_GET_OUTPUT_PRIMARY => {
            handle_randr_get_output_primary(state, client_id, sequence, body, byte_order, minor)
        }
        x11randr::RR_GET_PROVIDERS => {
            handle_randr_get_providers(state, client_id, sequence, body, byte_order, minor)
        }
        x11randr::RR_GET_PROVIDER_INFO => {
            handle_randr_get_provider_info(state, client_id, sequence, body, byte_order, minor)
        }
        x11randr::RR_SET_PROVIDER_OFFLOAD_SINK => {
            handle_randr_set_provider_offload_sink(state, client_id, sequence, body, minor)
        }
        x11randr::RR_SET_PROVIDER_OUTPUT_SOURCE => handle_randr_set_provider_output_source(
            state, backend, client_id, sequence, body, minor,
        ),
        x11randr::RR_GET_MONITORS => {
            handle_randr_get_monitors(state, client_id, sequence, body, byte_order, minor)
        }
        x11randr::RR_SET_MONITOR => {
            handle_randr_set_monitor(state, client_id, sequence, body, minor)
        }
        x11randr::RR_DELETE_MONITOR => {
            handle_randr_delete_monitor(state, client_id, sequence, body, minor)
        }
        x11randr::RR_GET_CRTC_GAMMA_SIZE => handle_randr_get_crtc_gamma_size(
            state, backend, client_id, sequence, header, body, byte_order,
        ),
        x11randr::RR_GET_CRTC_GAMMA => handle_randr_get_crtc_gamma(
            state, backend, client_id, sequence, header, body, byte_order,
        ),
        x11randr::RR_SET_CRTC_GAMMA => {
            handle_randr_set_crtc_gamma(state, backend, client_id, sequence, header, body)
        }
        x11randr::RR_GET_OUTPUT_PROPERTY => handle_randr_get_output_property(
            state, backend, client_id, sequence, body, byte_order, minor,
        ),
        x11randr::RR_SELECT_INPUT => {
            handle_randr_select_input(state, client_id, sequence, body, minor)
        }
        x11randr::RR_GET_SCREEN_INFO => {
            handle_randr_get_screen_info(state, client_id, sequence, body, byte_order, minor)
        }
        x11randr::RR_SET_SCREEN_SIZE => {
            handle_randr_set_screen_size(state, backend, client_id, sequence, header, body, minor)
        }
        x11randr::RR_SET_SCREEN_CONFIG => {
            set_screen_config(state, backend, client_id, sequence, byte_order, body)
        }
        x11randr::RR_SET_CRTC_CONFIG => handle_randr_set_crtc_config(
            state, backend, client_id, sequence, header, body, byte_order,
        ),
        16 | 45 => handle_randr_create_mode_or_lease(state, client_id, sequence, minor),
        x11randr::RR_LIST_PROVIDER_PROPERTIES
        | x11randr::RR_QUERY_PROVIDER_PROPERTY
        | x11randr::RR_CONFIGURE_PROVIDER_PROPERTY
        | x11randr::RR_CHANGE_PROVIDER_PROPERTY
        | x11randr::RR_DELETE_PROVIDER_PROPERTY
        | x11randr::RR_GET_PROVIDER_PROPERTY => {
            handle_randr_provider_property(state, client_id, sequence, body, minor)
        }
        46 => handle_randr_free_lease(state, client_id, sequence, body, minor),
        // 1 RROldGetScreenInfo and 3 RROldScreenChangeSelectInput are literal
        // NULL entries in Xorg's ProcRandrVector (randr/rrdispatch.c), and
        // ProcRRDispatch rejects a NULL slot exactly like an out-of-range
        // minor: `if (stuff->data >= RRNumberRequests ||
        // !ProcRandrVector[stuff->data]) return BadRequest;` (randr/randr.c).
        // They are request numbers that were never assigned, not requests we
        // have yet to write.
        1 | 3 => emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            x11::error::BAD_REQUEST,
            0,
            u16::from(minor),
            header.opcode,
        ),
        other if other >= RANDR_REQUEST_COUNT => emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            x11::error::BAD_REQUEST,
            0,
            u16::from(other),
            header.opcode,
        ),
        other => {
            let request = RequestOpcodes {
                major: RANDR_MAJOR_OPCODE,
                minor: other,
            };
            match other {
                17 => {
                    let mode = request_xid(body);
                    if mode != 0 {
                        check_resource!(
                            state,
                            client_id,
                            sequence,
                            request,
                            yserver_protocol::x11::ResourceId(mode),
                            ResourceKind::RandrMode,
                            AccessMode::DESTROY
                        );
                    }
                }
                18 | 19 => {
                    let output = request_xid(body);
                    let mode = body
                        .get(4..8)
                        .map(|bytes| u32::from_le_bytes(bytes.try_into().unwrap()))
                        .unwrap_or(0);
                    if output != 0 {
                        check_resource!(
                            state,
                            client_id,
                            sequence,
                            request,
                            yserver_protocol::x11::ResourceId(output),
                            ResourceKind::RandrOutput,
                            AccessMode::READ
                        );
                    }
                    if mode != 0 {
                        check_resource!(
                            state,
                            client_id,
                            sequence,
                            request,
                            yserver_protocol::x11::ResourceId(mode),
                            ResourceKind::RandrMode,
                            AccessMode::USE
                        );
                    }
                }
                _ => {}
            }
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
            Ok(RequestOutcome::Handled)
        }
    }
}

#[allow(clippy::needless_return)]
fn handle_randr_create_mode_or_lease(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    minor: u8,
) -> io::Result<RequestOutcome> {
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

#[allow(clippy::needless_return)]
fn handle_randr_free_lease(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    body: &[u8],
    minor: u8,
) -> io::Result<RequestOutcome> {
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
