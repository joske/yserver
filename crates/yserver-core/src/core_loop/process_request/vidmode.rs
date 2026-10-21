use crate::{
    backend::{
        Backend,
        gamma::{VIDMODE_GAMMA_ENTRIES, VidModeGammaTable},
    },
    core_loop::process_request::{
        CurrentVidModeOutput, RequestOutcome, emit_x11_error_with_minor, write_to_client,
    },
    server::ServerState,
};
use log::debug;
use std::io;
use yserver_protocol::{
    x11,
    x11::{ClientId, RequestHeader, SequenceNumber},
};

pub(super) fn current_vidmode_output(state: &ServerState) -> Option<CurrentVidModeOutput> {
    use yserver_protocol::x11::xf86vidmode::ModeLine;

    // Xwayland's VidMode shim uses its fixed output, or otherwise the first
    // output. Yserver's configured primary is the closest equivalent; if it
    // is off, fall back to the first enabled output.
    let output = state
        .randr
        .outputs
        .iter()
        .find(|output| output.output_id == state.randr.primary_output && output.mode_id != 0)
        .or_else(|| {
            state
                .randr
                .outputs
                .iter()
                .find(|output| output.mode_id != 0)
        })?;

    // Same resolved timing RANDR's ModeInfo reports, so the two extensions
    // can never describe the same mode differently. VidMode carries the
    // pixel clock in whole kHz rather than RANDR's Hz.
    let timing = output.effective_timing();
    // A zero clock would make Mesa derive a zero MSC rate; Xorg's
    // VidModeGetCurrentModeline fails the same way and yields BadValue.
    if timing.dot_clock_hz == 0 {
        return None;
    }
    Some(CurrentVidModeOutput {
        output_id: output.output_id,
        crtc_id: output.crtc_id,
        connector: output.name.clone(),
        mode: ModeLine {
            dot_clock: timing.dot_clock_khz(),
            hdisplay: output.width,
            hsync_start: timing.hsync_start,
            hsync_end: timing.hsync_end,
            htotal: timing.htotal,
            hskew: timing.hskew,
            vdisplay: output.height,
            vsync_start: timing.vsync_start,
            vsync_end: timing.vsync_end,
            vtotal: timing.vtotal,
            flags: timing.mode_flags,
        },
    })
}

#[cfg(test)]
pub(super) fn current_vidmode_mode_line(
    state: &ServerState,
) -> Option<yserver_protocol::x11::xf86vidmode::ModeLine> {
    current_vidmode_output(state).map(|output| output.mode)
}

fn vidmode_timing_order_is_valid(mode: yserver_protocol::x11::xf86vidmode::ModeLine) -> bool {
    mode.hdisplay <= mode.hsync_start
        && mode.hsync_start <= mode.hsync_end
        && mode.hsync_end <= mode.htotal
        && mode.vdisplay <= mode.vsync_start
        && mode.vsync_start <= mode.vsync_end
        && mode.vsync_end <= mode.vtotal
}

/// Derive VidMode's old monitor strings from the same raw EDID RANDR exposes.
/// A missing/invalid EDID leaves the vendor empty and uses the connector name
/// as the model, which is more useful than inventing an identity.
pub(super) fn vidmode_monitor_identity(edid: &[u8], connector: &str) -> (Vec<u8>, Vec<u8>) {
    let vendor = edid
        .get(8..10)
        .and_then(|bytes| {
            let code = u16::from_be_bytes(bytes.try_into().ok()?);
            let letters = [
                u8::try_from((code >> 10) & 0x1f).ok()?,
                u8::try_from((code >> 5) & 0x1f).ok()?,
                u8::try_from(code & 0x1f).ok()?,
            ];
            letters
                .iter()
                .all(|letter| (1..=26).contains(letter))
                .then(|| {
                    letters
                        .into_iter()
                        .map(|letter| b'A' + letter - 1)
                        .collect()
                })
        })
        .unwrap_or_default();

    let model = [54usize, 72, 90, 108]
        .into_iter()
        .filter_map(|offset| edid.get(offset..offset + 18))
        .find_map(|descriptor| {
            if descriptor.get(..5) != Some(&[0, 0, 0, 0xfc, 0]) {
                return None;
            }
            let name = descriptor.get(5..18)?;
            let end = name
                .iter()
                .rposition(|byte| !matches!(byte, 0 | b'\n' | b'\r' | b' '))
                .map_or(0, |index| index + 1);
            (end != 0
                && name[..end]
                    .iter()
                    .all(|byte| byte.is_ascii_graphic() || *byte == b' '))
            .then(|| name[..end].to_vec())
        })
        .unwrap_or_else(|| connector.as_bytes().to_vec());
    (vendor, model)
}

fn vidmode_sync_ranges(mode: yserver_protocol::x11::xf86vidmode::ModeLine) -> ([u32; 1], [u32; 1]) {
    fn rounded_hundredths(numerator: u64, denominator: u64) -> u16 {
        if denominator == 0 {
            return 0;
        }
        let value = numerator.saturating_add(denominator / 2) / denominator;
        u16::try_from(value).unwrap_or(u16::MAX)
    }
    fn singleton_range(value: u16) -> u32 {
        u32::from(value) | (u32::from(value) << 16)
    }

    // Horizontal range is kHz * 100. Vertical range is Hz * 100.
    let hsync = rounded_hundredths(
        u64::from(mode.dot_clock).saturating_mul(100),
        u64::from(mode.htotal),
    );
    let vsync = rounded_hundredths(
        u64::from(mode.dot_clock).saturating_mul(100_000),
        u64::from(mode.htotal).saturating_mul(u64::from(mode.vtotal)),
    );
    ([singleton_range(hsync)], [singleton_range(vsync)])
}

/// Validate the `screen(u16) + pad(u16)` body shared by VidMode's
/// read-only requests. `Err` carries `(error code, errorValue)`.
///
/// Xorg checks `screen >= screenInfo.numScreens => BadValue`; yserver
/// drives exactly one X screen, so anything but 0 is out of range.
fn vidmode_screen_number(screen: u32) -> Result<(), (u8, u32)> {
    if screen != 0 {
        return Err((x11::error::BAD_VALUE, screen));
    }
    Ok(())
}

fn vidmode_screen(body: &[u8]) -> Result<(), (u8, u32)> {
    let screen = yserver_protocol::x11::xf86vidmode::parse_screen(body)
        .ok_or((x11::error::BAD_LENGTH, 0))?;
    vidmode_screen_number(u32::from(screen))
}

fn write_vidmode_reply(
    state: &mut ServerState,
    client_id: ClientId,
    reply: &[u8],
) -> io::Result<RequestOutcome> {
    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return Ok(RequestOutcome::Handled);
    };
    Ok(write_to_client(client, client_id, reply))
}

pub(super) fn handle_xf86vidmode_request(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    use crate::nested::{XF86VIDMODE_FIRST_ERROR, XF86VIDMODE_MAJOR_OPCODE};
    use yserver_protocol::x11::{ClientByteOrder, xf86vidmode as x11vm};

    let byte_order = state
        .clients
        .get(&client_id.0)
        .map_or(ClientByteOrder::LittleEndian, |client| client.byte_order);
    let is_local = state
        .clients
        .get(&client_id.0)
        .is_none_or(|client| client.is_local);
    let version_2 = state
        .vidmode_client_versions
        .get(&client_id)
        .is_some_and(|(major, _)| *major >= 2);

    match header.data {
        x11vm::QUERY_VERSION => {
            if !body.is_empty() {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_LENGTH,
                    0,
                    u16::from(header.data),
                    XF86VIDMODE_MAJOR_OPCODE,
                );
            }
            let reply = x11vm::encode_query_version_reply(byte_order, sequence);
            return write_vidmode_reply(state, client_id, &reply);
        }
        x11vm::SET_CLIENT_VERSION => {
            let Some(version) = x11vm::parse_client_version(body) else {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_LENGTH,
                    0,
                    u16::from(header.data),
                    XF86VIDMODE_MAJOR_OPCODE,
                );
            };
            state
                .vidmode_client_versions
                .insert(client_id, (version.major, version.minor));
            debug!(
                "client {} #{} XFree86-VidMode::SetClientVersion {}.{}",
                client_id.0, sequence.0, version.major, version.minor
            );
        }
        x11vm::VALIDATE_MODE_LINE => {
            let Some(request) =
                x11vm::parse_validate_mode_line_request(byte_order, body, version_2)
            else {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_LENGTH,
                    0,
                    u16::from(header.data),
                    XF86VIDMODE_MAJOR_OPCODE,
                );
            };
            if let Err((code, bad_value)) = vidmode_screen_number(request.screen) {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    code,
                    bad_value,
                    u16::from(header.data),
                    XF86VIDMODE_MAJOR_OPCODE,
                );
            }
            let Some(current) = current_vidmode_output(state) else {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_VALUE,
                    0,
                    u16::from(header.data),
                    XF86VIDMODE_MAJOR_OPCODE,
                );
            };

            // Xorg rejects inverted timing order before invoking the monitor
            // and driver validators. Yserver exposes only the active mode, so
            // that exact advertised line is the only one its read-only
            // VidMode surface can truthfully validate. Legacy clients cannot
            // carry hskew, matching the legacy GetModeLine reply.
            let mut advertised = current.mode;
            if !version_2 {
                advertised.hskew = 0;
            }
            let status =
                if vidmode_timing_order_is_valid(request.mode) && request.mode == advertised {
                    x11vm::MODE_OK
                } else {
                    x11vm::MODE_BAD
                };
            let reply = x11vm::encode_validate_mode_line_reply(byte_order, sequence, status);
            return write_vidmode_reply(state, client_id, &reply);
        }
        x11vm::GET_GAMMA => {
            let Some(screen) = x11vm::parse_gamma_screen(body) else {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_LENGTH,
                    0,
                    u16::from(header.data),
                    XF86VIDMODE_MAJOR_OPCODE,
                );
            };
            if let Err((code, bad_value)) = vidmode_screen_number(u32::from(screen)) {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    code,
                    bad_value,
                    u16::from(header.data),
                    XF86VIDMODE_MAJOR_OPCODE,
                );
            }
            let gamma = state.vidmode_gamma.map(vidmode_gamma_to_wire);
            let reply = x11vm::encode_get_gamma_reply(byte_order, sequence, gamma);
            return write_vidmode_reply(state, client_id, &reply);
        }
        x11vm::GET_GAMMA_RAMP => {
            let Some(request) = x11vm::parse_gamma_ramp_request(body) else {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_LENGTH,
                    0,
                    u16::from(header.data),
                    XF86VIDMODE_MAJOR_OPCODE,
                );
            };
            if let Err((code, bad_value)) = vidmode_screen_number(u32::from(request.screen)) {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    code,
                    bad_value,
                    u16::from(header.data),
                    XF86VIDMODE_MAJOR_OPCODE,
                );
            }
            let Some(current) = current_vidmode_output(state) else {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_VALUE,
                    0,
                    u16::from(header.data),
                    XF86VIDMODE_MAJOR_OPCODE,
                );
            };
            let expected = vidmode_gamma_ramp_size(backend, current.crtc_id);
            if request.size != expected {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_VALUE,
                    u32::from(request.size),
                    u16::from(header.data),
                    XF86VIDMODE_MAJOR_OPCODE,
                );
            }
            // xf86GetGammaRamp: the VidMode table, not a CRTC's RANDR ramp.
            let (red, green, blue) = if expected == 0 {
                (Vec::new(), Vec::new(), Vec::new())
            } else {
                state
                    .vidmode_gamma_table
                    .clone()
                    .unwrap_or_else(VidModeGammaTable::identity)
                    .ramp()
            };
            let reply =
                x11vm::encode_get_gamma_ramp_reply(byte_order, sequence, &red, &green, &blue);
            return write_vidmode_reply(state, client_id, &reply);
        }
        // The ordinary read-only screen-scoped requests all carry
        // `screen(u16) + pad(u16)`.
        x11vm::GET_MODE_LINE
        | x11vm::GET_MONITOR
        | x11vm::GET_ALL_MODE_LINES
        | x11vm::GET_VIEW_PORT
        | x11vm::GET_DOT_CLOCKS
        | x11vm::GET_GAMMA_RAMP_SIZE
        | x11vm::GET_PERMISSIONS => {
            if let Err((code, bad_value)) = vidmode_screen(body) {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    code,
                    bad_value,
                    u16::from(header.data),
                    XF86VIDMODE_MAJOR_OPCODE,
                );
            }
            let reply = match header.data {
                x11vm::GET_PERMISSIONS => {
                    x11vm::encode_get_permissions_reply(byte_order, sequence, is_local)
                }
                x11vm::GET_VIEW_PORT => {
                    x11vm::encode_get_view_port_reply(byte_order, sequence, 0, 0)
                }
                x11vm::GET_DOT_CLOCKS => x11vm::encode_get_dot_clocks_reply(byte_order, sequence),
                _ => {
                    let Some(current) = current_vidmode_output(state) else {
                        // Xorg: VidModeGetCurrentModeline failure => BadValue.
                        return emit_x11_error_with_minor(
                            state,
                            client_id,
                            sequence,
                            x11::error::BAD_VALUE,
                            0,
                            u16::from(header.data),
                            XF86VIDMODE_MAJOR_OPCODE,
                        );
                    };
                    match header.data {
                        x11vm::GET_GAMMA_RAMP_SIZE => x11vm::encode_get_gamma_ramp_size_reply(
                            byte_order,
                            sequence,
                            vidmode_gamma_ramp_size(backend, current.crtc_id),
                        ),
                        x11vm::GET_MONITOR => {
                            let edid = backend
                                .output_identity(current.output_id)
                                .map_or_else(Vec::new, |(edid, _)| edid);
                            let (vendor, model) =
                                vidmode_monitor_identity(&edid, &current.connector);
                            let (hsync, vsync) = vidmode_sync_ranges(current.mode);
                            x11vm::encode_get_monitor_reply(
                                byte_order, sequence, &vendor, &model, &hsync, &vsync,
                            )
                        }
                        x11vm::GET_MODE_LINE | x11vm::GET_ALL_MODE_LINES => {
                            debug!(
                                "client {} #{} XFree86-VidMode::{} \
                                 {}x{} clock={}kHz totals={}x{} flags=0x{:x}",
                                client_id.0,
                                sequence.0,
                                if header.data == x11vm::GET_MODE_LINE {
                                    "GetModeLine"
                                } else {
                                    "GetAllModeLines"
                                },
                                current.mode.hdisplay,
                                current.mode.vdisplay,
                                current.mode.dot_clock,
                                current.mode.htotal,
                                current.mode.vtotal,
                                current.mode.flags
                            );
                            if header.data == x11vm::GET_MODE_LINE {
                                x11vm::encode_get_mode_line_reply(
                                    byte_order,
                                    sequence,
                                    current.mode,
                                    version_2,
                                )
                            } else {
                                x11vm::encode_get_all_mode_lines_reply(
                                    byte_order,
                                    sequence,
                                    current.mode,
                                    version_2,
                                )
                            }
                        }
                        _ => unreachable!("screen-scoped VidMode read"),
                    }
                }
            };
            return write_vidmode_reply(state, client_id, &reply);
        }
        // A local client, told it may write (GetPermissions), has its gamma
        // and lock writes applied as Xorg applies them.
        x11vm::SET_GAMMA | x11vm::SET_GAMMA_RAMP | x11vm::LOCK_MODE_SWITCH if is_local => {
            return handle_vidmode_local_write(
                state,
                backend,
                client_id,
                sequence,
                header.data,
                body,
            );
        }
        // Mode setting stays RANDR's, so it fails with the extension's
        // ClientNotLocal error even for a local client; so does every write
        // from a remote one, as in Xorg's dispatch.
        x11vm::MOD_MODE_LINE
        | x11vm::SWITCH_MODE
        | x11vm::LOCK_MODE_SWITCH
        | x11vm::ADD_MODE_LINE
        | x11vm::DELETE_MODE_LINE
        | x11vm::SWITCH_TO_MODE
        | x11vm::SET_VIEW_PORT
        | x11vm::SET_GAMMA
        | x11vm::SET_GAMMA_RAMP => {
            return emit_x11_error_with_minor(
                state,
                client_id,
                sequence,
                XF86VIDMODE_FIRST_ERROR + x11vm::CLIENT_NOT_LOCAL,
                0,
                u16::from(header.data),
                XF86VIDMODE_MAJOR_OPCODE,
            );
        }
        _ => {
            return emit_x11_error_with_minor(
                state,
                client_id,
                sequence,
                x11::error::BAD_REQUEST,
                0,
                u16::from(header.data),
                XF86VIDMODE_MAJOR_OPCODE,
            );
        }
    }
    Ok(RequestOutcome::Handled)
}

/// A `SetGamma` value as Xorg holds it: `((float) value) / 10000.`, a
/// double narrowed to the float `SetGamma` takes.
fn vidmode_gamma_from_wire(value: u32) -> f32 {
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    let gamma = (f64::from(value as f32) / 10_000.0) as f32;
    gamma
}

/// `GetGamma`'s `(CARD32) (gamma * 10000.)`, truncated.
fn vidmode_gamma_to_wire(gamma: f32) -> u32 {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let value = (f64::from(gamma) * 10_000.0) as u32;
    value
}

/// VidMode's gamma ramp size, as modesetting reports it: the CMap table's
/// `gammaElements`, whatever the hardware LUT's size; 0 when the CRTC has
/// no gamma, as Xwayland's.
fn vidmode_gamma_ramp_size(backend: &dyn Backend, crtc: u32) -> u16 {
    if backend.crtc_gamma_size(crtc) == 0 {
        0
    } else {
        VIDMODE_GAMMA_ENTRIES
    }
}

/// Make `table` the screen's VidMode gamma and hand it to the backend,
/// which loads it under every CRTC's RANDR ramp.
fn install_vidmode_gamma_table(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    table: VidModeGammaTable,
) {
    if let Err(e) = backend.set_vidmode_gamma(Some(&table)) {
        log::warn!("XF86VidMode gamma apply failed: {e}");
    }
    state.vidmode_gamma_table = Some(table);
}

/// A local client's `SetGamma`, `SetGammaRamp` and `LockModeSwitch`, as
/// Xorg's modesetting server answers them: Xext/vidmode.c's checks, then
/// hw/xfree86's `CMapChangeGamma` (`ComputeGamma`) for the gamma,
/// `xf86ChangeGammaRamp` (`ComputeGammaRamp`) for the ramp, both loaded
/// through `xf86RandR12LoadPalette` under every CRTC's RANDR ramp, and
/// `xf86VidModeLockZoom` for the lock.
fn handle_vidmode_local_write(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    client_id: ClientId,
    sequence: SequenceNumber,
    minor: u8,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    use crate::nested::XF86VIDMODE_MAJOR_OPCODE;
    use yserver_protocol::x11::xf86vidmode as x11vm;

    let error = |state: &mut ServerState, code: u8, bad_value: u32| {
        emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            code,
            bad_value,
            u16::from(minor),
            XF86VIDMODE_MAJOR_OPCODE,
        )
    };
    match minor {
        x11vm::LOCK_MODE_SWITCH => {
            let Some(screen) = x11vm::parse_lock_mode_switch_screen(body) else {
                return error(state, x11::error::BAD_LENGTH, 0);
            };
            if let Err((code, bad_value)) = vidmode_screen_number(u32::from(screen)) {
                return error(state, code, bad_value);
            }
            // xf86VidModeLockZoom fails only under the DontZoom option,
            // which yserver has no counterpart of.
        }
        x11vm::SET_GAMMA => {
            let Some(request) = x11vm::parse_set_gamma(body) else {
                return error(state, x11::error::BAD_LENGTH, 0);
            };
            if let Err((code, bad_value)) = vidmode_screen_number(u32::from(request.screen)) {
                return error(state, code, bad_value);
            }
            let gamma = [request.red, request.green, request.blue].map(vidmode_gamma_from_wire);
            // CMapChangeGamma: each within [GAMMA_MIN, GAMMA_MAX], 1/10 to 10.
            if gamma
                .iter()
                .any(|&g| f64::from(g) < 1.0 / 10.0 || f64::from(g) > 10.0)
            {
                return error(state, x11::error::BAD_VALUE, 0);
            }
            state.vidmode_gamma = gamma;
            install_vidmode_gamma_table(state, backend, VidModeGammaTable::from_gamma(gamma));
        }
        x11vm::SET_GAMMA_RAMP => {
            let Some(request) = x11vm::parse_set_gamma_ramp_header(body) else {
                return error(state, x11::error::BAD_LENGTH, 0);
            };
            if let Err((code, bad_value)) = vidmode_screen_number(u32::from(request.screen)) {
                return error(state, code, bad_value);
            }
            let Some(current) = current_vidmode_output(state) else {
                return error(state, x11::error::BAD_VALUE, 0);
            };
            // The size GetGammaRampSize reports, then exactly that ramp.
            // It replaces the table; GetGamma's values stay as they were.
            let size = vidmode_gamma_ramp_size(backend, current.crtc_id);
            if request.size != size {
                return error(state, x11::error::BAD_VALUE, u32::from(request.size));
            }
            let Some((red, green, blue)) = x11vm::parse_set_gamma_ramp_channels(body, size) else {
                return error(state, x11::error::BAD_LENGTH, 0);
            };
            if size > 0 {
                let table = VidModeGammaTable::from_ramp(&red, &green, &blue);
                install_vidmode_gamma_table(state, backend, table);
            }
        }
        _ => unreachable!("not a VidMode gamma or lock write"),
    }
    Ok(RequestOutcome::Handled)
}
