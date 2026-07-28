//! Minimal read-only XFree86-VidModeExtension protocol surface.
//!
//! Mesa implements `glXGetMscRateOML` entirely client-side. It obtains the
//! active mode timing through `XF86VidModeQueryVersion` followed by
//! `XF86VidModeGetModeLine`, then derives the MSC rate from
//! `dotclock * 1000 / (htotal * vtotal)`. Yserver exposes those read-only
//! requests plus `SetClientVersion`, which selects the legacy or v2 reply
//! layout exactly as Xorg does.
//!
//! `GetAllModeLines`, `GetGammaRampSize` and `GetPermissions` round the
//! surface out so that a client probing VidMode for something other than
//! the MSC rate gets an honest read-only answer (one mode, no gamma ramp,
//! no write permission) instead of a `BadRequest` it did not expect from an
//! extension it just saw advertised. Every other request — mode setting,
//! gamma, viewport and the remaining monitor/dot-clock queries — stays
//! unimplemented.
//!
//! Request bodies reach the parsers here already in host byte order —
//! `request_swap` has swapped them for a big-endian client — so the
//! `parse_*` helpers read little-endian unconditionally.

use super::{
    ClientByteOrder, SequenceNumber,
    wire::{write_u16, write_u32},
};

pub const MAJOR_VERSION: u16 = 2;
pub const MINOR_VERSION: u16 = 2;

pub const QUERY_VERSION: u8 = 0;
pub const GET_MODE_LINE: u8 = 1;
pub const GET_ALL_MODE_LINES: u8 = 6;
pub const SET_CLIENT_VERSION: u8 = 14;
pub const GET_GAMMA_RAMP_SIZE: u8 = 19;
pub const GET_PERMISSIONS: u8 = 20;

/// `XF86VM_READ_PERMISSION`. Yserver never grants
/// `XF86VM_WRITE_PERMISSION` (2): no VidMode mode-setting request exists.
pub const PERMISSION_READ: u32 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClientVersion {
    pub major: u16,
    pub minor: u16,
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

fn reply_prefix(byte_order: ClientByteOrder, sequence: SequenceNumber, length: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(32 + 4 * length as usize);
    out.push(1);
    out.push(0);
    write_u16(byte_order, &mut out, sequence.0);
    write_u32(byte_order, &mut out, length);
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

/// Encode `GetGammaRampSize`. Yserver implements no gamma ramp, and Xorg
/// reports size 0 for a screen without one; `XF86VidModeGetGammaRampSize`
/// hands that straight to the caller, which is the documented way for a
/// client to learn the ramp is unavailable.
#[must_use]
pub fn encode_get_gamma_ramp_size_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
) -> Vec<u8> {
    let mut out = reply_prefix(byte_order, sequence, 0);
    write_u16(byte_order, &mut out, 0); // size
    out.resize(32, 0);
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
    fn gamma_ramp_size_reply_reports_no_ramp() {
        let reply =
            encode_get_gamma_ramp_size_reply(ClientByteOrder::LittleEndian, SequenceNumber(3));
        assert_eq!(reply.len(), 32);
        assert_eq!(u32::from_le_bytes(reply[4..8].try_into().unwrap()), 0);
        assert_eq!(u16::from_le_bytes(reply[8..10].try_into().unwrap()), 0);
        assert_eq!(&reply[10..32], &[0; 22]);
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
    }
}
