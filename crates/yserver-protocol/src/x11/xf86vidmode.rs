//! Read-only XFree86-VidModeExtension protocol surface.
//!
//! Mesa implements `glXGetMscRateOML` entirely client-side. It obtains the
//! active mode timing through `XF86VidModeQueryVersion` followed by
//! `XF86VidModeGetModeLine`, then derives the MSC rate from
//! `dotclock * 1000 / (htotal * vtotal)`. Yserver exposes those read-only
//! requests plus `SetClientVersion`, which selects the legacy or v2 reply
//! layout exactly as Xorg does.
//!
//! Xorg serves VidMode's read requests even to clients without mode-setting
//! permission. Yserver mirrors that read surface from the selected RANDR
//! output: one active mode, monitor/timing information, viewport `(0, 0)`,
//! and the same gamma ramp exposed through RANDR. Mode-setting and gamma
//! writes remain disabled; the core dispatcher rejects them with VidMode's
//! `ClientNotLocal` error, matching the read-only permission model.
//!
//! Request bodies reach the parsers here already in host byte order —
//! `request_swap` has swapped them for a big-endian client — so the
//! ordinary `parse_*` helpers read little-endian unconditionally.
//! `ValidateModeLine` is the exception: its legacy/v2 layout depends on
//! per-client state unavailable to the generic swap table, so its parser
//! reads the original client byte order explicitly.

use super::{
    ClientByteOrder, SequenceNumber,
    wire::{pad_vec4, read_u16, read_u32, write_u16, write_u32},
};

pub const MAJOR_VERSION: u16 = 2;
pub const MINOR_VERSION: u16 = 2;

pub const QUERY_VERSION: u8 = 0;
pub const GET_MODE_LINE: u8 = 1;
pub const MOD_MODE_LINE: u8 = 2;
pub const SWITCH_MODE: u8 = 3;
pub const GET_MONITOR: u8 = 4;
pub const LOCK_MODE_SWITCH: u8 = 5;
pub const GET_ALL_MODE_LINES: u8 = 6;
pub const ADD_MODE_LINE: u8 = 7;
pub const DELETE_MODE_LINE: u8 = 8;
pub const VALIDATE_MODE_LINE: u8 = 9;
pub const SWITCH_TO_MODE: u8 = 10;
pub const GET_VIEW_PORT: u8 = 11;
pub const SET_VIEW_PORT: u8 = 12;
pub const GET_DOT_CLOCKS: u8 = 13;
pub const SET_CLIENT_VERSION: u8 = 14;
pub const SET_GAMMA: u8 = 15;
pub const GET_GAMMA: u8 = 16;
pub const GET_GAMMA_RAMP: u8 = 17;
pub const SET_GAMMA_RAMP: u8 = 18;
pub const GET_GAMMA_RAMP_SIZE: u8 = 19;
pub const GET_PERMISSIONS: u8 = 20;

/// Relative extension error used by Xorg's non-local write gate.
pub const CLIENT_NOT_LOCAL: u8 = 5;
/// Xorg `ModeStatus::MODE_OK`.
pub const MODE_OK: u32 = 0;
/// Xorg `ModeStatus::MODE_BAD` (`-2` on the CARD32 wire).
pub const MODE_BAD: u32 = 0xffff_fffe;
/// Xorg's `CLKFLAG_PROGRAMABLE` (historical spelling).
pub const CLOCK_FLAG_PROGRAMMABLE: u32 = 1;
/// Xorg's fixed upper bound for a legacy hardware clock table.
pub const MAX_CLOCKS: u32 = 128;

/// `XF86VM_READ_PERMISSION`. Yserver never grants
/// `XF86VM_WRITE_PERMISSION` (2): no VidMode mode-setting request exists.
pub const PERMISSION_READ: u32 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClientVersion {
    pub major: u16,
    pub minor: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GammaRampRequest {
    pub screen: u16,
    pub size: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ValidateModeLineRequest {
    pub screen: u32,
    pub mode: ModeLine,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ModeLine {
    /// Pixel clock in kHz, matching `xXF86VidModeGetModeLineReply`.
    pub dot_clock: u32,
    pub hdisplay: u16,
    pub hsync_start: u16,
    pub hsync_end: u16,
    pub htotal: u16,
    pub hskew: u16,
    pub vdisplay: u16,
    pub vsync_start: u16,
    pub vsync_end: u16,
    pub vtotal: u16,
    pub flags: u32,
}

#[must_use]
pub fn parse_screen(body: &[u8]) -> Option<u16> {
    (body.len() == 4).then(|| u16::from_le_bytes([body[0], body[1]]))
}

#[must_use]
pub fn parse_client_version(body: &[u8]) -> Option<ClientVersion> {
    (body.len() == 4).then(|| ClientVersion {
        major: u16::from_le_bytes([body[0], body[1]]),
        minor: u16::from_le_bytes([body[2], body[3]]),
    })
}

/// Parse `GetGamma`, whose historical request is padded to 32 bytes.
#[must_use]
pub fn parse_gamma_screen(body: &[u8]) -> Option<u16> {
    (body.len() == 28).then(|| u16::from_le_bytes([body[0], body[1]]))
}

#[must_use]
pub fn parse_gamma_ramp_request(body: &[u8]) -> Option<GammaRampRequest> {
    (body.len() == 4).then(|| GammaRampRequest {
        screen: u16::from_le_bytes([body[0], body[1]]),
        size: u16::from_le_bytes([body[2], body[3]]),
    })
}

/// Parse the version-dependent `ValidateModeLine` request.
///
/// The fixed request is 52 bytes for protocol v2 and 36 bytes for the legacy
/// layout, including the 4-byte X request header. `privsize` counts trailing
/// `CARD32`s. Unlike other parsers, `body` has deliberately not passed through
/// the generic byte-swap table; see the module-level note.
#[must_use]
pub fn parse_validate_mode_line_request(
    byte_order: ClientByteOrder,
    body: &[u8],
    version_2: bool,
) -> Option<ValidateModeLineRequest> {
    let (fixed_body_len, privsize_offset) = if version_2 { (48, 44) } else { (32, 28) };
    if body.len() < fixed_body_len {
        return None;
    }
    let privsize = usize::try_from(read_u32(
        byte_order,
        body.get(privsize_offset..privsize_offset + 4)?,
    ))
    .ok()?;
    let expected_len = fixed_body_len.checked_add(privsize.checked_mul(4)?)?;
    if body.len() != expected_len {
        return None;
    }

    let mode = ModeLine {
        dot_clock: read_u32(byte_order, &body[4..8]),
        hdisplay: read_u16(byte_order, &body[8..10]),
        hsync_start: read_u16(byte_order, &body[10..12]),
        hsync_end: read_u16(byte_order, &body[12..14]),
        htotal: read_u16(byte_order, &body[14..16]),
        hskew: if version_2 {
            read_u16(byte_order, &body[16..18])
        } else {
            0
        },
        vdisplay: read_u16(byte_order, &body[if version_2 { 18..20 } else { 16..18 }]),
        vsync_start: read_u16(byte_order, &body[if version_2 { 20..22 } else { 18..20 }]),
        vsync_end: read_u16(byte_order, &body[if version_2 { 22..24 } else { 20..22 }]),
        vtotal: read_u16(byte_order, &body[if version_2 { 24..26 } else { 22..24 }]),
        flags: read_u32(byte_order, &body[if version_2 { 28..32 } else { 24..28 }]),
    };
    Some(ValidateModeLineRequest {
        screen: read_u32(byte_order, body),
        mode,
    })
}

fn reply_prefix(byte_order: ClientByteOrder, sequence: SequenceNumber, length: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(32 + 4 * length as usize);
    out.push(1);
    out.push(0);
    write_u16(byte_order, &mut out, sequence.0);
    write_u32(byte_order, &mut out, length);
    out
}

#[must_use]
pub fn encode_validate_mode_line_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    status: u32,
) -> Vec<u8> {
    encode_u32_reply(byte_order, sequence, status)
}

/// Encode `GetMonitor`: sync ranges precede individually padded vendor and
/// model strings, as consumed by libXxf86vm's `_XReadPad` calls.
#[must_use]
pub fn encode_get_monitor_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    vendor: &[u8],
    model: &[u8],
    hsync_ranges: &[u32],
    vsync_ranges: &[u32],
) -> Vec<u8> {
    let vendor = &vendor[..vendor.len().min(usize::from(u8::MAX))];
    let model = &model[..model.len().min(usize::from(u8::MAX))];
    let hsync_ranges = &hsync_ranges[..hsync_ranges.len().min(usize::from(u8::MAX))];
    let vsync_ranges = &vsync_ranges[..vsync_ranges.len().min(usize::from(u8::MAX))];
    let payload_len = (hsync_ranges.len() + vsync_ranges.len()) * 4
        + vendor.len().next_multiple_of(4)
        + model.len().next_multiple_of(4);
    let length = u32::try_from(payload_len / 4).unwrap_or(u32::MAX);
    let mut out = reply_prefix(byte_order, sequence, length);
    out.push(u8::try_from(vendor.len()).unwrap_or(u8::MAX));
    out.push(u8::try_from(model.len()).unwrap_or(u8::MAX));
    out.push(u8::try_from(hsync_ranges.len()).unwrap_or(u8::MAX));
    out.push(u8::try_from(vsync_ranges.len()).unwrap_or(u8::MAX));
    out.resize(32, 0);
    for &range in hsync_ranges.iter().chain(vsync_ranges) {
        write_u32(byte_order, &mut out, range);
    }
    out.extend_from_slice(vendor);
    pad_vec4(&mut out);
    out.extend_from_slice(model);
    pad_vec4(&mut out);
    debug_assert_eq!(out.len(), 32 + payload_len);
    out
}

#[must_use]
pub fn encode_get_view_port_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    x: u32,
    y: u32,
) -> Vec<u8> {
    let mut out = reply_prefix(byte_order, sequence, 0);
    write_u32(byte_order, &mut out, x);
    write_u32(byte_order, &mut out, y);
    out.resize(32, 0);
    out
}

/// Encode Xorg's `GetDotClocks` reply for a programmable-clock driver.
///
/// Xorg's KMS modesetting driver sets `progClock`, so the reply advertises
/// that capability and carries no legacy fixed-clock table. The active mode's
/// actual dot clock is reported by `GetModeLine`.
#[must_use]
pub fn encode_get_dot_clocks_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
) -> Vec<u8> {
    let mut out = reply_prefix(byte_order, sequence, 0);
    write_u32(byte_order, &mut out, CLOCK_FLAG_PROGRAMMABLE);
    write_u32(byte_order, &mut out, 0); // no fixed clocks
    write_u32(byte_order, &mut out, MAX_CLOCKS);
    out.resize(32, 0);
    out
}

/// `GetGamma` reports per-screen scalar values, not the CRTC LUT. Because
/// yserver rejects `SetGamma`, the state remains Xorg's identity default.
#[must_use]
pub fn encode_get_gamma_reply(byte_order: ClientByteOrder, sequence: SequenceNumber) -> Vec<u8> {
    let mut out = reply_prefix(byte_order, sequence, 0);
    for _ in 0..3 {
        write_u32(byte_order, &mut out, 10_000); // 1.0 * 10000
    }
    out.resize(32, 0);
    out
}

/// Encode a 32-byte reply whose only payload is a single `CARD32` at the
/// usual post-header offset (`GetPermissions`).
fn encode_u32_reply(byte_order: ClientByteOrder, sequence: SequenceNumber, value: u32) -> Vec<u8> {
    let mut out = reply_prefix(byte_order, sequence, 0);
    write_u32(byte_order, &mut out, value);
    out.resize(32, 0);
    out
}

#[must_use]
pub fn encode_query_version_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
) -> Vec<u8> {
    let mut out = reply_prefix(byte_order, sequence, 0);
    write_u16(byte_order, &mut out, MAJOR_VERSION);
    write_u16(byte_order, &mut out, MINOR_VERSION);
    out.resize(32, 0);
    out
}

/// Encode Xorg's v2 (52-byte) or legacy v0/v1 (36-byte) GetModeLine reply.
#[must_use]
pub fn encode_get_mode_line_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    mode: ModeLine,
    version_2: bool,
) -> Vec<u8> {
    let mut out = reply_prefix(byte_order, sequence, if version_2 { 5 } else { 1 });
    write_u32(byte_order, &mut out, mode.dot_clock);
    write_u16(byte_order, &mut out, mode.hdisplay);
    write_u16(byte_order, &mut out, mode.hsync_start);
    write_u16(byte_order, &mut out, mode.hsync_end);
    write_u16(byte_order, &mut out, mode.htotal);
    if version_2 {
        write_u16(byte_order, &mut out, mode.hskew);
    }
    write_u16(byte_order, &mut out, mode.vdisplay);
    write_u16(byte_order, &mut out, mode.vsync_start);
    write_u16(byte_order, &mut out, mode.vsync_end);
    write_u16(byte_order, &mut out, mode.vtotal);
    if version_2 {
        write_u16(byte_order, &mut out, 0);
    }
    write_u32(byte_order, &mut out, mode.flags);
    if version_2 {
        write_u32(byte_order, &mut out, 0);
        write_u32(byte_order, &mut out, 0);
        write_u32(byte_order, &mut out, 0);
    }
    // Xorg pretends that no server-private mode data exists.
    write_u32(byte_order, &mut out, 0);
    debug_assert_eq!(out.len(), if version_2 { 52 } else { 36 });
    out
}

/// Encode `GetAllModeLines` reporting the single active mode.
///
/// Yserver has no VidMode mode list of its own: RANDR owns mode selection
/// and none of VidMode's switching requests are implemented. Advertising
/// exactly one mode is the honest answer and, unlike a longer list, does
/// not invite a client to attempt a `SwitchToMode` that can only fail.
///
/// Note `xXF86VidModeModeInfo` widens `hskew` to a `CARD32` — it is a
/// `CARD16` in the `GetModeLine` reply — and carries a `pad1` word plus
/// three reserved words before `privsize`.
#[must_use]
pub fn encode_get_all_mode_lines_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    mode: ModeLine,
    version_2: bool,
) -> Vec<u8> {
    let info_size = if version_2 { 48 } else { 28 };
    let mut out = reply_prefix(byte_order, sequence, info_size / 4);
    write_u32(byte_order, &mut out, 1); // modecount
    out.resize(32, 0);

    write_u32(byte_order, &mut out, mode.dot_clock);
    write_u16(byte_order, &mut out, mode.hdisplay);
    write_u16(byte_order, &mut out, mode.hsync_start);
    write_u16(byte_order, &mut out, mode.hsync_end);
    write_u16(byte_order, &mut out, mode.htotal);
    if version_2 {
        write_u32(byte_order, &mut out, u32::from(mode.hskew));
    }
    write_u16(byte_order, &mut out, mode.vdisplay);
    write_u16(byte_order, &mut out, mode.vsync_start);
    write_u16(byte_order, &mut out, mode.vsync_end);
    write_u16(byte_order, &mut out, mode.vtotal);
    if version_2 {
        write_u32(byte_order, &mut out, 0); // pad1
    }
    write_u32(byte_order, &mut out, mode.flags);
    if version_2 {
        write_u32(byte_order, &mut out, 0); // reserved1
        write_u32(byte_order, &mut out, 0); // reserved2
        write_u32(byte_order, &mut out, 0); // reserved3
    }
    write_u32(byte_order, &mut out, 0); // privsize
    debug_assert_eq!(out.len(), 32 + info_size as usize);
    out
}

/// Encode the selected output's real RANDR/CRTC gamma-ramp size.
#[must_use]
pub fn encode_get_gamma_ramp_size_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    size: u16,
) -> Vec<u8> {
    let mut out = reply_prefix(byte_order, sequence, 0);
    write_u16(byte_order, &mut out, size);
    out.resize(32, 0);
    out
}

/// Encode `GetGammaRamp` using Xorg's per-channel even-`CARD16` stride.
#[must_use]
pub fn encode_get_gamma_ramp_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    red: &[u16],
    green: &[u16],
    blue: &[u16],
) -> Vec<u8> {
    debug_assert_eq!(red.len(), green.len());
    debug_assert_eq!(red.len(), blue.len());
    let size = u16::try_from(red.len()).unwrap_or(u16::MAX);
    let padded_entries = red.len().next_multiple_of(2);
    let payload_len = padded_entries.saturating_mul(3).saturating_mul(2);
    let length = u32::try_from(payload_len / 4).unwrap_or(u32::MAX);
    let mut out = reply_prefix(byte_order, sequence, length);
    write_u16(byte_order, &mut out, size);
    out.resize(32, 0);
    for channel in [red, green, blue] {
        for &entry in channel {
            write_u16(byte_order, &mut out, entry);
        }
        if channel.len() != padded_entries {
            write_u16(byte_order, &mut out, 0);
        }
    }
    debug_assert_eq!(out.len(), 32 + payload_len);
    out
}

/// Encode `GetPermissions` — read-only, never `XF86VM_WRITE_PERMISSION`.
#[must_use]
pub fn encode_get_permissions_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
) -> Vec<u8> {
    encode_u32_reply(byte_order, sequence, PERMISSION_READ)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> ModeLine {
        ModeLine {
            dot_clock: 241_500,
            hdisplay: 2560,
            hsync_start: 2608,
            hsync_end: 2640,
            htotal: 2720,
            hskew: 0,
            vdisplay: 1440,
            vsync_start: 1443,
            vsync_end: 1448,
            vtotal: 1481,
            flags: 5,
        }
    }

    #[test]
    fn query_version_reply_matches_v2_2_wire_layout() {
        let reply =
            encode_query_version_reply(ClientByteOrder::LittleEndian, SequenceNumber(0x1234));
        assert_eq!(reply.len(), 32);
        assert_eq!(&reply[0..8], &[1, 0, 0x34, 0x12, 0, 0, 0, 0]);
        assert_eq!(&reply[8..12], &[2, 0, 2, 0]);
    }

    #[test]
    fn mode_line_v2_reply_matches_xf86vmproto_layout() {
        let reply = encode_get_mode_line_reply(
            ClientByteOrder::LittleEndian,
            SequenceNumber(9),
            fixture(),
            true,
        );
        assert_eq!(reply.len(), 52);
        assert_eq!(u32::from_le_bytes(reply[4..8].try_into().unwrap()), 5);
        assert_eq!(
            u32::from_le_bytes(reply[8..12].try_into().unwrap()),
            241_500
        );
        assert_eq!(u16::from_le_bytes(reply[12..14].try_into().unwrap()), 2560);
        assert_eq!(u16::from_le_bytes(reply[18..20].try_into().unwrap()), 2720);
        assert_eq!(u16::from_le_bytes(reply[22..24].try_into().unwrap()), 1440);
        assert_eq!(u16::from_le_bytes(reply[28..30].try_into().unwrap()), 1481);
        assert_eq!(u32::from_le_bytes(reply[32..36].try_into().unwrap()), 5);
        assert_eq!(&reply[36..52], &[0; 16]);
    }

    #[test]
    fn legacy_mode_line_reply_omits_hskew_and_reserved_fields() {
        let reply = encode_get_mode_line_reply(
            ClientByteOrder::LittleEndian,
            SequenceNumber(9),
            fixture(),
            false,
        );
        assert_eq!(reply.len(), 36);
        assert_eq!(u32::from_le_bytes(reply[4..8].try_into().unwrap()), 1);
        assert_eq!(u16::from_le_bytes(reply[20..22].try_into().unwrap()), 1440);
        assert_eq!(u32::from_le_bytes(reply[28..32].try_into().unwrap()), 5);
        assert_eq!(&reply[32..36], &[0; 4]);
    }

    #[test]
    fn big_endian_reply_swaps_every_multibyte_field() {
        let reply = encode_get_mode_line_reply(
            ClientByteOrder::BigEndian,
            SequenceNumber(0x1234),
            fixture(),
            true,
        );
        assert_eq!(&reply[2..4], &[0x12, 0x34]);
        assert_eq!(&reply[4..8], &[0, 0, 0, 5]);
        assert_eq!(&reply[8..12], &241_500_u32.to_be_bytes());
        assert_eq!(&reply[12..14], &2560_u16.to_be_bytes());
        assert_eq!(&reply[28..30], &1481_u16.to_be_bytes());
        assert_eq!(&reply[32..36], &5_u32.to_be_bytes());
    }

    #[test]
    fn all_mode_lines_v2_reply_matches_xf86vidmodemodeinfo_layout() {
        let reply = encode_get_all_mode_lines_reply(
            ClientByteOrder::LittleEndian,
            SequenceNumber(7),
            fixture(),
            true,
        );
        // 32-byte header + one 48-byte xXF86VidModeModeInfo.
        assert_eq!(reply.len(), 80);
        assert_eq!(u32::from_le_bytes(reply[4..8].try_into().unwrap()), 12);
        assert_eq!(u32::from_le_bytes(reply[8..12].try_into().unwrap()), 1);
        assert_eq!(&reply[12..32], &[0; 20]);
        assert_eq!(
            u32::from_le_bytes(reply[32..36].try_into().unwrap()),
            241_500
        );
        assert_eq!(u16::from_le_bytes(reply[36..38].try_into().unwrap()), 2560);
        assert_eq!(u16::from_le_bytes(reply[42..44].try_into().unwrap()), 2720);
        // hskew is a CARD32 here, not the CARD16 of the GetModeLine reply.
        assert_eq!(u32::from_le_bytes(reply[44..48].try_into().unwrap()), 0);
        assert_eq!(u16::from_le_bytes(reply[48..50].try_into().unwrap()), 1440);
        assert_eq!(u16::from_le_bytes(reply[54..56].try_into().unwrap()), 1481);
        assert_eq!(u32::from_le_bytes(reply[56..60].try_into().unwrap()), 0);
        assert_eq!(u32::from_le_bytes(reply[60..64].try_into().unwrap()), 5);
        assert_eq!(&reply[64..80], &[0; 16]);
    }

    #[test]
    fn legacy_all_mode_lines_reply_drops_the_hskew_word() {
        let reply = encode_get_all_mode_lines_reply(
            ClientByteOrder::LittleEndian,
            SequenceNumber(7),
            fixture(),
            false,
        );
        // 32-byte header + one 28-byte xXF86OldVidModeModeInfo.
        assert_eq!(reply.len(), 60);
        assert_eq!(u32::from_le_bytes(reply[4..8].try_into().unwrap()), 7);
        assert_eq!(u32::from_le_bytes(reply[8..12].try_into().unwrap()), 1);
        assert_eq!(u16::from_le_bytes(reply[42..44].try_into().unwrap()), 2720);
        // vdisplay follows htotal directly with no hskew in between.
        assert_eq!(u16::from_le_bytes(reply[44..46].try_into().unwrap()), 1440);
        assert_eq!(u16::from_le_bytes(reply[50..52].try_into().unwrap()), 1481);
        assert_eq!(u32::from_le_bytes(reply[52..56].try_into().unwrap()), 5);
        assert_eq!(u32::from_le_bytes(reply[56..60].try_into().unwrap()), 0);
    }

    #[test]
    fn big_endian_all_mode_lines_reply_swaps_every_field() {
        let reply = encode_get_all_mode_lines_reply(
            ClientByteOrder::BigEndian,
            SequenceNumber(0x1234),
            fixture(),
            true,
        );
        assert_eq!(&reply[2..4], &[0x12, 0x34]);
        assert_eq!(&reply[4..8], &12_u32.to_be_bytes());
        assert_eq!(&reply[8..12], &1_u32.to_be_bytes());
        assert_eq!(&reply[32..36], &241_500_u32.to_be_bytes());
        assert_eq!(&reply[36..38], &2560_u16.to_be_bytes());
        assert_eq!(&reply[56..60], &0_u32.to_be_bytes());
        assert_eq!(&reply[60..64], &5_u32.to_be_bytes());
        assert_eq!(&reply[64..80], &[0; 16]);
    }

    #[test]
    fn permissions_reply_is_read_only() {
        let reply = encode_get_permissions_reply(ClientByteOrder::LittleEndian, SequenceNumber(3));
        assert_eq!(reply.len(), 32);
        assert_eq!(u32::from_le_bytes(reply[4..8].try_into().unwrap()), 0);
        // READ without WRITE — mode setting is not implemented.
        assert_eq!(u32::from_le_bytes(reply[8..12].try_into().unwrap()), 1);
        assert_eq!(&reply[12..32], &[0; 20]);
    }

    #[test]
    fn monitor_viewport_dotclock_and_gamma_replies_match_wire_layouts() {
        let monitor = encode_get_monitor_reply(
            ClientByteOrder::LittleEndian,
            SequenceNumber(3),
            b"DEL",
            b"U2723QE",
            &[0x1234_1234],
            &[0x5678_5678],
        );
        assert_eq!(monitor.len(), 52);
        assert_eq!(u32::from_le_bytes(monitor[4..8].try_into().unwrap()), 5);
        assert_eq!(&monitor[8..12], &[3, 7, 1, 1]);
        assert_eq!(&monitor[32..36], &0x1234_1234_u32.to_le_bytes());
        assert_eq!(&monitor[36..40], &0x5678_5678_u32.to_le_bytes());
        assert_eq!(&monitor[40..44], b"DEL\0");
        assert_eq!(&monitor[44..52], b"U2723QE\0");

        let viewport =
            encode_get_view_port_reply(ClientByteOrder::LittleEndian, SequenceNumber(4), 0, 0);
        assert_eq!(viewport.len(), 32);
        assert_eq!(&viewport[8..16], &[0; 8]);

        let clocks = encode_get_dot_clocks_reply(ClientByteOrder::LittleEndian, SequenceNumber(5));
        assert_eq!(clocks.len(), 32);
        assert_eq!(u32::from_le_bytes(clocks[4..8].try_into().unwrap()), 0);
        assert_eq!(
            u32::from_le_bytes(clocks[8..12].try_into().unwrap()),
            CLOCK_FLAG_PROGRAMMABLE
        );
        assert_eq!(u32::from_le_bytes(clocks[12..16].try_into().unwrap()), 0);
        assert_eq!(
            u32::from_le_bytes(clocks[16..20].try_into().unwrap()),
            MAX_CLOCKS
        );

        let gamma = encode_get_gamma_reply(ClientByteOrder::LittleEndian, SequenceNumber(6));
        assert_eq!(gamma.len(), 32);
        assert_eq!(&gamma[8..20], &[0x10, 0x27, 0, 0].repeat(3));
    }

    #[test]
    fn gamma_ramp_replies_report_real_size_and_xorg_channel_padding() {
        let reply =
            encode_get_gamma_ramp_size_reply(ClientByteOrder::LittleEndian, SequenceNumber(3), 256);
        assert_eq!(reply.len(), 32);
        assert_eq!(u32::from_le_bytes(reply[4..8].try_into().unwrap()), 0);
        assert_eq!(u16::from_le_bytes(reply[8..10].try_into().unwrap()), 256);
        assert_eq!(&reply[10..32], &[0; 22]);

        let ramp = encode_get_gamma_ramp_reply(
            ClientByteOrder::LittleEndian,
            SequenceNumber(4),
            &[1, 2, 3],
            &[4, 5, 6],
            &[7, 8, 9],
        );
        // Each odd-sized channel has one CARD16 of padding: 4 * 2 * 3 bytes.
        assert_eq!(ramp.len(), 56);
        assert_eq!(u32::from_le_bytes(ramp[4..8].try_into().unwrap()), 6);
        assert_eq!(u16::from_le_bytes(ramp[8..10].try_into().unwrap()), 3);
        assert_eq!(&ramp[32..40], &[1, 0, 2, 0, 3, 0, 0, 0]);
        assert_eq!(&ramp[40..48], &[4, 0, 5, 0, 6, 0, 0, 0]);
        assert_eq!(&ramp[48..56], &[7, 0, 8, 0, 9, 0, 0, 0]);
    }

    #[test]
    fn request_parsers_require_exact_bodies() {
        assert_eq!(parse_screen(&[0, 0, 0, 0]), Some(0));
        assert_eq!(
            parse_client_version(&[2, 0, 1, 0]),
            Some(ClientVersion { major: 2, minor: 1 })
        );
        assert_eq!(parse_screen(&[0, 0]), None);
        assert_eq!(parse_client_version(&[2, 0, 1, 0, 0]), None);

        let mut gamma = [0u8; 28];
        gamma[0..2].copy_from_slice(&1u16.to_le_bytes());
        assert_eq!(parse_gamma_screen(&gamma), Some(1));
        assert_eq!(parse_gamma_screen(&gamma[..4]), None);
        assert_eq!(
            parse_gamma_ramp_request(&[0, 0, 0, 1]),
            Some(GammaRampRequest {
                screen: 0,
                size: 256
            })
        );

        let mut legacy_validate = [0u8; 36];
        legacy_validate[0..4].copy_from_slice(&1u32.to_be_bytes());
        legacy_validate[28..32].copy_from_slice(&1u32.to_be_bytes());
        assert_eq!(
            parse_validate_mode_line_request(ClientByteOrder::BigEndian, &legacy_validate, false),
            Some(ValidateModeLineRequest {
                screen: 1,
                mode: ModeLine {
                    dot_clock: 0,
                    hdisplay: 0,
                    hsync_start: 0,
                    hsync_end: 0,
                    htotal: 0,
                    hskew: 0,
                    vdisplay: 0,
                    vsync_start: 0,
                    vsync_end: 0,
                    vtotal: 0,
                    flags: 0,
                },
            })
        );
        assert_eq!(
            parse_validate_mode_line_request(
                ClientByteOrder::BigEndian,
                &legacy_validate[..32],
                false
            ),
            None,
            "privsize=1 requires one trailing CARD32"
        );

        let mut v2_validate = [0u8; 48];
        v2_validate[0..4].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(
            parse_validate_mode_line_request(ClientByteOrder::LittleEndian, &v2_validate, true),
            Some(ValidateModeLineRequest {
                screen: 0,
                mode: ModeLine {
                    dot_clock: 0,
                    hdisplay: 0,
                    hsync_start: 0,
                    hsync_end: 0,
                    htotal: 0,
                    hskew: 0,
                    vdisplay: 0,
                    vsync_start: 0,
                    vsync_end: 0,
                    vtotal: 0,
                    flags: 0,
                },
            })
        );
    }
}
