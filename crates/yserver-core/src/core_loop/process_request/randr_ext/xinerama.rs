use crate::{
    access::{AccessMode, RequestOpcodes, ResourceKind},
    core_loop::process_request::{
        RequestOutcome, check_resource, emit_x11_error_with_minor, randr_ext::active_monitors,
        write_to_client,
    },
    server::ServerState,
};
use std::io;
use yserver_protocol::x11::{ClientId, RequestHeader, ResourceId, SequenceNumber};

pub(in crate::core_loop::process_request) fn handle_xinerama_request(
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
    let request = RequestOpcodes {
        major: XINERAMA_MAJOR_OPCODE,
        minor,
    };

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
            check_resource!(
                state,
                client_id,
                sequence,
                request,
                ResourceId(window),
                ResourceKind::Window,
                AccessMode::GET_ATTR
            );
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
            check_resource!(
                state,
                client_id,
                sequence,
                request,
                ResourceId(window),
                ResourceKind::Window,
                AccessMode::GET_ATTR
            );
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
            check_resource!(
                state,
                client_id,
                sequence,
                request,
                ResourceId(window),
                ResourceKind::Window,
                AccessMode::GET_ATTR
            );
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
