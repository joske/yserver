//! Minimal read-only XFree86-VidModeExtension protocol surface.
//!
//! Mesa implements `glXGetMscRateOML` entirely client-side. It obtains the
//! active mode timing through `XF86VidModeQueryVersion` followed by
//! `XF86VidModeGetModeLine`, then derives the MSC rate from
//! `dotclock * 1000 / (htotal * vtotal)`. Yserver exposes those read-only
//! requests plus `SetClientVersion`, which selects the legacy or v2 reply
//! layout exactly as Xorg does.

use super::{
    ClientByteOrder, SequenceNumber,
    wire::{write_u16, write_u32},
};

pub const MAJOR_VERSION: u16 = 2;
pub const MINOR_VERSION: u16 = 2;

pub const QUERY_VERSION: u8 = 0;
pub const GET_MODE_LINE: u8 = 1;
pub const SET_CLIENT_VERSION: u8 = 14;

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
    let mut out = Vec::with_capacity(52);
    out.push(1);
    out.push(0);
    write_u16(byte_order, &mut out, sequence.0);
    write_u32(byte_order, &mut out, length);
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
