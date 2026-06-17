use super::{
    ClientByteOrder, SequenceNumber,
    wire::{write_i16, write_u16, write_u32},
};

pub const QUERY_VERSION: u8 = 0;
pub const CHANGE_SAVE_SET: u8 = 1;
pub const SELECT_SELECTION_INPUT: u8 = 2;
pub const SELECT_CURSOR_INPUT: u8 = 3;
pub const GET_CURSOR_IMAGE: u8 = 4;
pub const CREATE_REGION: u8 = 5;
pub const CREATE_REGION_FROM_BITMAP: u8 = 6;
pub const CREATE_REGION_FROM_WINDOW: u8 = 7;
pub const CREATE_REGION_FROM_GC: u8 = 8;
pub const CREATE_REGION_FROM_PICTURE: u8 = 9;
pub const DESTROY_REGION: u8 = 10;
pub const SET_REGION: u8 = 11;
pub const COPY_REGION: u8 = 12;
pub const UNION_REGION: u8 = 13;
pub const INTERSECT_REGION: u8 = 14;
pub const SUBTRACT_REGION: u8 = 15;
pub const INVERT_REGION: u8 = 16;
pub const TRANSLATE_REGION: u8 = 17;
pub const REGION_EXTENTS: u8 = 18;
pub const FETCH_REGION: u8 = 19;
pub const SET_GC_CLIP_REGION: u8 = 20;
pub const SET_WINDOW_SHAPE_REGION: u8 = 21;
pub const SET_PICTURE_CLIP_REGION: u8 = 22;
pub const SET_CURSOR_NAME: u8 = 23;
pub const GET_CURSOR_NAME: u8 = 24;
pub const GET_CURSOR_IMAGE_AND_NAME: u8 = 25;
pub const CHANGE_CURSOR_BY_NAME: u8 = 27;
pub const HIDE_CURSOR: u8 = 29;
pub const SHOW_CURSOR: u8 = 30;
pub const CREATE_POINTER_BARRIER: u8 = 31;
pub const DELETE_POINTER_BARRIER: u8 = 32;

// XFixesSelectionNotify subtypes (Xorg `xfixes/select.c`).
pub const SELECTION_NOTIFY_SET_OWNER: u8 = 0;
pub const SELECTION_NOTIFY_WINDOW_DESTROY: u8 = 1;
pub const SELECTION_NOTIFY_CLIENT_CLOSE: u8 = 2;

// XFixesSelectionNotify event-mask bits — passed by the client to
// `SelectSelectionInput` and matched against subtypes when fanning
// the event out. One bit per subtype.
pub const SELECTION_MASK_SET_OWNER: u32 = 1 << SELECTION_NOTIFY_SET_OWNER;
pub const SELECTION_MASK_WINDOW_DESTROY: u32 = 1 << SELECTION_NOTIFY_WINDOW_DESTROY;
pub const SELECTION_MASK_CLIENT_CLOSE: u32 = 1 << SELECTION_NOTIFY_CLIENT_CLOSE;

/// Union of all known XFixesSelectionNotify mask bits. A
/// `SelectSelectionInput` request whose `event_mask` has any bit
/// outside this union must return BadValue (matches Xorg's
/// `SelectionAllEvents` at `xfixes/select.c:115-117`).
pub const SELECTION_ALL_EVENTS_MASK: u32 =
    SELECTION_MASK_SET_OWNER | SELECTION_MASK_WINDOW_DESTROY | SELECTION_MASK_CLIENT_CLOSE;

// Mutter/muffin refuses to start as a WM unless XFIXES advertises >= 5.0
// (`Window manager error: Mutter requires XFixes 5.0`). QueryVersion echoes
// min(client, server) per axis, so older clients still negotiate down.
// Stage-5+ opcodes (`CreatePointerBarrier` etc.) are reply-less and fall
// through `handle_xfixes_request`'s `other` arm without breaking clients.
pub const MAJOR_VERSION: u32 = 5;
pub const MINOR_VERSION: u32 = 0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RegionRect {
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
}

impl RegionRect {
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.width == 0 || self.height == 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SelectSelectionInputRequest {
    pub window: u32,
    pub selection: u32,
    pub event_mask: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SelectCursorInputRequest {
    pub window: u32,
    pub event_mask: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChangeSaveSetRequest {
    /// 0 = Insert, 1 = Delete.
    pub mode: u8,
    /// 0 = Nearest, 1 = Root.
    pub target: u8,
    /// 0 = Map, 1 = Unmap.
    pub map: u8,
    pub window: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SetGcClipRegionRequest {
    pub gc: u32,
    /// 0 = None (clear the clip).
    pub region: u32,
    pub x_origin: i16,
    pub y_origin: i16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SetWindowShapeRegionRequest {
    pub dest: u32,
    /// 0 = Bounding, 1 = Clip, 2 = Input. Matches `shape::KIND_*`.
    pub dest_kind: u8,
    pub x_offset: i16,
    pub y_offset: i16,
    /// 0 = None (clear the shape for this kind).
    pub region: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SetPictureClipRegionRequest {
    pub picture: u32,
    /// 0 = None (clear the picture clip).
    pub region: u32,
    pub x_origin: i16,
    pub y_origin: i16,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreatePointerBarrierRequest {
    pub barrier: u32,
    pub window: u32,
    pub x1: i16,
    pub y1: i16,
    pub x2: i16,
    pub y2: i16,
    pub directions: u32,
    pub devices: Vec<u16>,
}

fn read_u16_le(bytes: &[u8]) -> u16 {
    u16::from_le_bytes([bytes[0], bytes[1]])
}

fn read_i16_le(bytes: &[u8]) -> i16 {
    i16::from_le_bytes([bytes[0], bytes[1]])
}

fn read_u32_le(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn fixed_reply(byte_order: ClientByteOrder, sequence: SequenceNumber, length: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(32);
    out.push(1);
    out.push(0);
    write_u16(byte_order, &mut out, sequence.0);
    write_u32(byte_order, &mut out, length);
    out
}

#[must_use]
pub fn parse_u32_pair(body: &[u8]) -> Option<(u32, u32)> {
    if body.len() < 8 {
        return None;
    }
    Some((read_u32_le(body), read_u32_le(&body[4..])))
}

#[must_use]
pub fn parse_u32_triplet(body: &[u8]) -> Option<(u32, u32, u32)> {
    if body.len() < 12 {
        return None;
    }
    Some((
        read_u32_le(body),
        read_u32_le(&body[4..]),
        read_u32_le(&body[8..]),
    ))
}

#[must_use]
pub fn parse_select_selection_input(body: &[u8]) -> Option<SelectSelectionInputRequest> {
    if body.len() < 12 {
        return None;
    }
    Some(SelectSelectionInputRequest {
        window: read_u32_le(body),
        selection: read_u32_le(&body[4..]),
        event_mask: read_u32_le(&body[8..]),
    })
}

#[must_use]
pub fn parse_select_cursor_input(body: &[u8]) -> Option<SelectCursorInputRequest> {
    if body.len() < 8 {
        return None;
    }
    Some(SelectCursorInputRequest {
        window: read_u32_le(body),
        event_mask: read_u32_le(&body[4..]),
    })
}

#[must_use]
pub fn parse_create_region(body: &[u8]) -> Option<(u32, Vec<RegionRect>)> {
    if body.len() < 4 {
        return None;
    }
    Some((read_u32_le(body), parse_rectangles(&body[4..])))
}

#[must_use]
pub fn parse_create_region_from_window(body: &[u8]) -> Option<(u32, u32, u8)> {
    if body.len() < 12 {
        return None;
    }
    Some((read_u32_le(body), read_u32_le(&body[4..]), body[8]))
}

#[must_use]
pub fn parse_create_region_from_picture(body: &[u8]) -> Option<(u32, u32)> {
    if body.len() < 8 {
        return None;
    }
    Some((read_u32_le(body), read_u32_le(&body[4..])))
}

#[must_use]
pub fn parse_translate_region(body: &[u8]) -> Option<(u32, i16, i16)> {
    if body.len() < 8 {
        return None;
    }
    Some((
        read_u32_le(body),
        read_i16_le(&body[4..]),
        read_i16_le(&body[6..]),
    ))
}

#[must_use]
pub fn parse_invert_region(body: &[u8]) -> Option<(u32, RegionRect, u32)> {
    if body.len() < 16 {
        return None;
    }
    Some((
        read_u32_le(body),
        RegionRect {
            x: read_i16_le(&body[4..]),
            y: read_i16_le(&body[6..]),
            width: read_u16_le(&body[8..]),
            height: read_u16_le(&body[10..]),
        },
        read_u32_le(&body[12..]),
    ))
}

#[must_use]
pub fn parse_change_save_set(body: &[u8]) -> Option<ChangeSaveSetRequest> {
    if body.len() < 8 {
        return None;
    }
    Some(ChangeSaveSetRequest {
        mode: body[0],
        target: body[1],
        map: body[2],
        window: read_u32_le(&body[4..]),
    })
}

#[must_use]
pub fn parse_set_gc_clip_region(body: &[u8]) -> Option<SetGcClipRegionRequest> {
    if body.len() < 12 {
        return None;
    }
    Some(SetGcClipRegionRequest {
        gc: read_u32_le(body),
        region: read_u32_le(&body[4..]),
        x_origin: read_i16_le(&body[8..]),
        y_origin: read_i16_le(&body[10..]),
    })
}

#[must_use]
pub fn parse_set_window_shape_region(body: &[u8]) -> Option<SetWindowShapeRegionRequest> {
    if body.len() < 16 {
        return None;
    }
    Some(SetWindowShapeRegionRequest {
        dest: read_u32_le(body),
        dest_kind: body[4],
        // bytes 5..8 are padding.
        x_offset: read_i16_le(&body[8..]),
        y_offset: read_i16_le(&body[10..]),
        region: read_u32_le(&body[12..]),
    })
}

#[must_use]
pub fn parse_set_picture_clip_region(body: &[u8]) -> Option<SetPictureClipRegionRequest> {
    if body.len() < 12 {
        return None;
    }
    Some(SetPictureClipRegionRequest {
        picture: read_u32_le(body),
        region: read_u32_le(&body[4..]),
        x_origin: read_i16_le(&body[8..]),
        y_origin: read_i16_le(&body[10..]),
    })
}

/// Parse XFIXES `ChangeCursorByName` (minor 27). Returns the cursor XID and
/// the raw name bytes (no UTF-8 validation, no theme lookup — the caller
/// forwards the exact bytes to the host).
#[must_use]
pub fn parse_change_cursor_by_name(body: &[u8]) -> Option<(u32, &[u8])> {
    if body.len() < 8 {
        return None;
    }
    let cursor = read_u32_le(body);
    let nbytes = read_u16_le(&body[4..6]) as usize;
    // bytes 6..8 are padding.
    let name = body.get(8..8 + nbytes)?;
    Some((cursor, name))
}

/// Parse XFIXES `SetCursorName` (minor 23). Wire shape matches
/// `ChangeCursorByName`: cursor(4) + nbytes(2) + pad(2) + name(STRING8).
/// Returns the cursor XID and the raw name bytes.
#[must_use]
pub fn parse_set_cursor_name(body: &[u8]) -> Option<(u32, &[u8])> {
    parse_change_cursor_by_name(body)
}

#[must_use]
pub fn parse_create_pointer_barrier(body: &[u8]) -> Option<CreatePointerBarrierRequest> {
    if body.len() < 24 {
        return None;
    }
    let num_devices = read_u16_le(&body[22..]) as usize;
    let need = 24 + num_devices * 2;
    if body.len() < need {
        return None;
    }
    let devices = (0..num_devices)
        .map(|i| read_u16_le(&body[24 + i * 2..]))
        .collect();
    Some(CreatePointerBarrierRequest {
        barrier: read_u32_le(body),
        window: read_u32_le(&body[4..]),
        x1: read_i16_le(&body[8..]),
        y1: read_i16_le(&body[10..]),
        x2: read_i16_le(&body[12..]),
        y2: read_i16_le(&body[14..]),
        directions: read_u32_le(&body[16..]) & 0x0f,
        devices,
    })
}

#[must_use]
pub fn parse_delete_pointer_barrier(body: &[u8]) -> Option<u32> {
    if body.len() < 4 {
        return None;
    }
    Some(read_u32_le(body))
}

/// Encode the XFIXES `GetCursorName` reply (minor 24). Per Xorg
/// `xfixes/cursor.c:ProcXFixesGetCursorName`:
/// 32-byte header containing the cursor's name atom + nbytes, followed
/// by the raw name bytes and 4-byte alignment padding. The `length`
/// field in the reply header counts 4-byte words of payload after the
/// 32-byte header — that's `(nbytes + pad) / 4` where
/// `pad = (4 - nbytes % 4) % 4`.
#[must_use]
pub fn encode_get_cursor_name_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    atom: u32,
    name: &[u8],
) -> Vec<u8> {
    let nbytes = name.len();
    let pad = (4 - nbytes % 4) % 4;
    #[allow(clippy::cast_possible_truncation)]
    let length = ((nbytes + pad) / 4) as u32;
    let mut out = fixed_reply(byte_order, sequence, length);
    write_u32(byte_order, &mut out, atom);
    #[allow(clippy::cast_possible_truncation)]
    let nbytes_u16 = nbytes.min(usize::from(u16::MAX)) as u16;
    write_u16(byte_order, &mut out, nbytes_u16);
    out.extend_from_slice(&[0u8; 18]);
    debug_assert_eq!(out.len(), 32);
    out.extend_from_slice(name);
    out.extend_from_slice(&vec![0u8; pad]);
    debug_assert_eq!(out.len(), 32 + nbytes + pad);
    out
}

#[must_use]
pub fn parse_rectangles(mut bytes: &[u8]) -> Vec<RegionRect> {
    let mut rects = Vec::new();
    while bytes.len() >= 8 {
        let rect = RegionRect {
            x: read_i16_le(bytes),
            y: read_i16_le(&bytes[2..]),
            width: read_u16_le(&bytes[4..]),
            height: read_u16_le(&bytes[6..]),
        };
        if !rect.is_empty() {
            rects.push(rect);
        }
        bytes = &bytes[8..];
    }
    rects
}

/// `XFixesSelectionNotify` event (subtype 0 = SetSelectionOwner,
/// 1 = SelectionWindowDestroy, 2 = SelectionClientClose).
///
/// 32-byte fixed event per Xorg `xfixes/xfixesint.h`. `first_event`
/// is the dynamically-allocated XFIXES base event code (yserver
/// uses 87 for XFixesSelectionNotify, 88 for XFixesCursorNotify —
/// matches Xorg's `XFixesNumberEvents` layout).
pub fn encode_selection_notify_event(
    out: &mut Vec<u8>,
    order: ClientByteOrder,
    first_event: u8,
    subtype: u8,
    seq: SequenceNumber,
    window: u32,
    owner: u32,
    selection: u32,
    timestamp: u32,
    selection_timestamp: u32,
) {
    out.push(first_event); // type = first_event + 0
    out.push(subtype);
    write_u16(order, out, seq.0);
    write_u32(order, out, window);
    write_u32(order, out, owner);
    write_u32(order, out, selection);
    write_u32(order, out, timestamp);
    write_u32(order, out, selection_timestamp);
    out.extend_from_slice(&[0u8; 8]); // pad to 32
    debug_assert_eq!(out.len(), 32);
}

#[must_use]
pub fn encode_query_version_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    major: u32,
    minor: u32,
) -> Vec<u8> {
    let mut out = fixed_reply(byte_order, sequence, 0);
    write_u32(byte_order, &mut out, major);
    write_u32(byte_order, &mut out, minor);
    out.extend_from_slice(&[0u8; 16]);
    debug_assert_eq!(out.len(), 32);
    out
}

#[must_use]
pub fn encode_get_cursor_image_empty_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
) -> Vec<u8> {
    let mut out = fixed_reply(byte_order, sequence, 0);
    write_i16(byte_order, &mut out, 0); // x
    write_i16(byte_order, &mut out, 0); // y
    write_u16(byte_order, &mut out, 0); // width
    write_u16(byte_order, &mut out, 0); // height
    write_u16(byte_order, &mut out, 0); // xhot
    write_u16(byte_order, &mut out, 0); // yhot
    write_u32(byte_order, &mut out, 0); // cursor serial
    out.extend_from_slice(&[0u8; 8]);
    debug_assert_eq!(out.len(), 32);
    out
}

/// Per X11 XFIXES GetCursorImageReply (Xorg `xfixes/cursor.c:381-413`):
/// 32-byte header containing position/dims/hotspot/serial followed by
/// `width × height` × 4-byte pixels in **premultiplied ARGB32** (host
/// byte order, but the wire is always 32-bit native — we encode
/// big-endian to match the protocol for big-endian clients).
///
/// `bgra_straight` is the cursor image in straight-alpha BGRA8 (the
/// canonical [`CursorRecord`](../../../yserver/src/kms/v2/cursor.rs)
/// representation in v2). This routine premultiplies and flips byte
/// order to produce the ARGB the X11 wire expects.
///
/// Length encoding: reply `length` field counts 4-byte words after
/// the 32-byte fixed header.
#[must_use]
pub fn encode_get_cursor_image_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    x: i16,
    y: i16,
    width: u16,
    height: u16,
    xhot: u16,
    yhot: u16,
    cursor_serial: u32,
    bgra_straight: &[u8],
) -> Vec<u8> {
    let pixel_count = usize::from(width) * usize::from(height);
    let payload_bytes = pixel_count * 4;
    debug_assert_eq!(bgra_straight.len(), payload_bytes);
    #[allow(clippy::cast_possible_truncation)]
    let length = pixel_count as u32;
    let mut out = fixed_reply(byte_order, sequence, length);
    write_i16(byte_order, &mut out, x);
    write_i16(byte_order, &mut out, y);
    write_u16(byte_order, &mut out, width);
    write_u16(byte_order, &mut out, height);
    write_u16(byte_order, &mut out, xhot);
    write_u16(byte_order, &mut out, yhot);
    write_u32(byte_order, &mut out, cursor_serial);
    out.extend_from_slice(&[0u8; 8]);
    debug_assert_eq!(out.len(), 32);
    // Premultiply + byte-order conversion. Source is straight BGRA
    // (B, G, R, A); X11 wire wants ARGB packed as a 32-bit native
    // word — encode as four bytes in big- or little-endian per the
    // client's byte order so the reader interpretation as `u32 ARGB`
    // matches its endianness.
    for px in bgra_straight.chunks_exact(4) {
        let b = px[0];
        let g = px[1];
        let r = px[2];
        let a = px[3];
        // Straight → premultiplied alpha. Each channel × (a / 255),
        // rounded to nearest. Xorg uses bit-shift premul which is
        // equivalent for u8.
        let pm = |c: u8| -> u8 {
            let prod = u16::from(c) * u16::from(a) + 127;
            ((prod + (prod >> 8)) >> 8) as u8
        };
        let pr = pm(r);
        let pg = pm(g);
        let pb = pm(b);
        let argb: u32 =
            (u32::from(a) << 24) | (u32::from(pr) << 16) | (u32::from(pg) << 8) | u32::from(pb);
        write_u32(byte_order, &mut out, argb);
    }
    debug_assert_eq!(out.len(), 32 + payload_bytes);
    out
}

/// Straight-BGRA8 → premultiplied-ARGB32 wire pixels, appended to
/// `out` in the client's byte order. Shared by the GetCursorImage and
/// GetCursorImageAndName encoders.
fn append_premultiplied_argb(byte_order: ClientByteOrder, out: &mut Vec<u8>, bgra_straight: &[u8]) {
    for px in bgra_straight.chunks_exact(4) {
        let (b, g, r, a) = (px[0], px[1], px[2], px[3]);
        let pm = |c: u8| -> u8 {
            let prod = u16::from(c) * u16::from(a) + 127;
            ((prod + (prod >> 8)) >> 8) as u8
        };
        let argb: u32 = (u32::from(a) << 24)
            | (u32::from(pm(r)) << 16)
            | (u32::from(pm(g)) << 8)
            | u32::from(pm(b));
        write_u32(byte_order, out, argb);
    }
}

/// XFIXES `GetCursorImageAndName` reply (opcode 25, Xorg
/// `xfixes/cursor.c` `ProcXFixesGetCursorImageAndName`). Identical to
/// [`encode_get_cursor_image_reply`] except the 8 trailing header pad
/// bytes carry the cursor-name atom + name length, and the cursor
/// name string follows the (premultiplied ARGB) image, padded to a
/// 4-byte boundary. `length` counts image words + name words.
///
/// **Missing-reply hang fix (2026-06-11):** real X screen recorders
/// (e.g. gpu-screen-recorder) call this to grab the cursor; without a
/// reply they block forever in `poll()`. `cursor_atom`/`name` may be
/// `0`/empty when the active cursor is unnamed (a valid X state).
#[must_use]
pub fn encode_get_cursor_image_and_name_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    x: i16,
    y: i16,
    width: u16,
    height: u16,
    xhot: u16,
    yhot: u16,
    cursor_serial: u32,
    cursor_atom: u32,
    name: &[u8],
    bgra_straight: &[u8],
) -> Vec<u8> {
    let pixel_count = usize::from(width) * usize::from(height);
    let payload_bytes = pixel_count * 4;
    debug_assert_eq!(bgra_straight.len(), payload_bytes);
    let name_padded = (name.len() + 3) & !3;
    #[allow(clippy::cast_possible_truncation)]
    let length = (pixel_count + name_padded / 4) as u32;
    let mut out = fixed_reply(byte_order, sequence, length);
    write_i16(byte_order, &mut out, x);
    write_i16(byte_order, &mut out, y);
    write_u16(byte_order, &mut out, width);
    write_u16(byte_order, &mut out, height);
    write_u16(byte_order, &mut out, xhot);
    write_u16(byte_order, &mut out, yhot);
    write_u32(byte_order, &mut out, cursor_serial);
    write_u32(byte_order, &mut out, cursor_atom);
    #[allow(clippy::cast_possible_truncation)]
    write_u16(byte_order, &mut out, name.len() as u16);
    write_u16(byte_order, &mut out, 0); // pad
    debug_assert_eq!(out.len(), 32);
    append_premultiplied_argb(byte_order, &mut out, bgra_straight);
    out.extend_from_slice(name);
    out.resize(32 + payload_bytes + name_padded, 0);
    out
}

/// Empty `GetCursorImageAndName` reply (0×0 cursor, no name) — for
/// backends without a live cursor source (`ynest`, `RecordingBackend`).
#[must_use]
pub fn encode_get_cursor_image_and_name_empty_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
) -> Vec<u8> {
    encode_get_cursor_image_and_name_reply(byte_order, sequence, 0, 0, 0, 0, 0, 0, 0, 0, &[], &[])
}

#[must_use]
pub fn encode_fetch_region_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    extents: RegionRect,
    rects: &[RegionRect],
) -> Vec<u8> {
    #[allow(clippy::cast_possible_truncation)]
    let length = (rects.len() * 2) as u32;
    let mut out = fixed_reply(byte_order, sequence, length);
    write_i16(byte_order, &mut out, extents.x);
    write_i16(byte_order, &mut out, extents.y);
    write_u16(byte_order, &mut out, extents.width);
    write_u16(byte_order, &mut out, extents.height);
    out.extend_from_slice(&[0u8; 16]);
    for rect in rects {
        write_i16(byte_order, &mut out, rect.x);
        write_i16(byte_order, &mut out, rect.y);
        write_u16(byte_order, &mut out, rect.width);
        write_u16(byte_order, &mut out, rect.height);
    }
    debug_assert_eq!(out.len(), 32 + rects.len() * 8);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stage 5 unblock — `GetCursorImage` real reply carries the
    /// cursor pixels with premultiplied ARGB (host-native u32 per
    /// pixel) after the 32-byte header. Tests on a 1×1 fully-opaque
    /// red sprite at (10, 20) with hotspot (1, 2) and serial 99.
    #[test]
    fn get_cursor_image_reply_encodes_premul_argb() {
        // 1×1 sprite, opaque red. Straight BGRA: B=0, G=0, R=0xFF,
        // A=0xFF. Premultiplied = same (α=1.0). Wire ARGB =
        // 0xFFFF0000.
        let bgra = [0u8, 0, 0xFF, 0xFF];
        let reply = encode_get_cursor_image_reply(
            ClientByteOrder::LittleEndian,
            SequenceNumber(3),
            10,
            20,
            1,
            1,
            1,
            2,
            99,
            &bgra,
        );
        assert_eq!(reply.len(), 32 + 4, "header + 1 pixel");
        // length field = 1 word.
        assert_eq!(u32::from_le_bytes(reply[4..8].try_into().unwrap()), 1);
        // x, y, w, h, xhot, yhot.
        assert_eq!(i16::from_le_bytes([reply[8], reply[9]]), 10);
        assert_eq!(i16::from_le_bytes([reply[10], reply[11]]), 20);
        assert_eq!(u16::from_le_bytes([reply[12], reply[13]]), 1);
        assert_eq!(u16::from_le_bytes([reply[14], reply[15]]), 1);
        assert_eq!(u16::from_le_bytes([reply[16], reply[17]]), 1);
        assert_eq!(u16::from_le_bytes([reply[18], reply[19]]), 2);
        assert_eq!(u32::from_le_bytes(reply[20..24].try_into().unwrap()), 99);
        // Pixel 0 — premul ARGB(0xFFFF0000). On LE wire: 00 00 FF FF.
        assert_eq!(&reply[32..36], &[0x00, 0x00, 0xFF, 0xFF]);
    }

    /// Half-alpha sprite — straight BGRA(0, 0, 255, 128) premultiplies
    /// to ARGB(128, 128, 0, 0) on the wire (red × α/255 ≈ 128).
    #[test]
    fn get_cursor_image_reply_premultiplies_alpha() {
        let bgra = [0u8, 0, 0xFF, 0x80];
        let reply = encode_get_cursor_image_reply(
            ClientByteOrder::LittleEndian,
            SequenceNumber(0),
            0,
            0,
            1,
            1,
            0,
            0,
            1,
            &bgra,
        );
        // ARGB packed LE: B, G, R, A. R = 0xFF * 0x80 / 255 ≈ 0x80.
        assert_eq!(reply[32], 0x00); // B premul
        assert_eq!(reply[33], 0x00); // G premul
        assert_eq!(reply[34], 0x80); // R premul (within ±1 ULP)
        assert_eq!(reply[35], 0x80); // A untouched
    }

    /// GetCursorImageAndName (opcode 25) — the missing-reply gsr hang
    /// fix. Header carries the name atom + length; the name follows the
    /// image, padded to 4 bytes; `length` counts image + name words.
    #[test]
    fn get_cursor_image_and_name_reply_carries_name_after_image() {
        let bgra = [0u8, 0, 0xFF, 0xFF]; // 1×1 opaque red
        let name = b"left_ptr"; // 8 bytes → already 4-aligned
        let reply = encode_get_cursor_image_and_name_reply(
            ClientByteOrder::LittleEndian,
            SequenceNumber(7),
            10,
            20,
            1,
            1,
            1,
            2,
            99,
            0x2a,
            name,
            &bgra,
        );
        // length = 1 image word + 2 name words (8 bytes / 4).
        assert_eq!(u32::from_le_bytes(reply[4..8].try_into().unwrap()), 3);
        assert_eq!(u32::from_le_bytes(reply[20..24].try_into().unwrap()), 99); // serial
        assert_eq!(u32::from_le_bytes(reply[24..28].try_into().unwrap()), 0x2a); // atom
        assert_eq!(u16::from_le_bytes([reply[28], reply[29]]), 8); // nbytes
        // Header (32) + 1 pixel (4) + name (8).
        assert_eq!(reply.len(), 32 + 4 + 8);
        assert_eq!(&reply[32..36], &[0x00, 0x00, 0xFF, 0xFF]); // premul ARGB pixel
        assert_eq!(&reply[36..44], name); // name string after the image
    }

    /// Empty variant (no live cursor) — valid 0×0/unnamed reply, no
    /// payload, so a None-cursor backend still answers (no client hang).
    #[test]
    fn get_cursor_image_and_name_empty_reply_is_header_only() {
        let reply = encode_get_cursor_image_and_name_empty_reply(
            ClientByteOrder::LittleEndian,
            SequenceNumber(1),
        );
        assert_eq!(reply.len(), 32);
        assert_eq!(u32::from_le_bytes(reply[4..8].try_into().unwrap()), 0); // length
        assert_eq!(u16::from_le_bytes([reply[28], reply[29]]), 0); // nbytes
    }

    #[test]
    fn query_version_reply_shape() {
        let reply = encode_query_version_reply(
            ClientByteOrder::LittleEndian,
            SequenceNumber(7),
            MAJOR_VERSION,
            MINOR_VERSION,
        );
        assert_eq!(reply.len(), 32);
        assert_eq!(reply[0], 1);
        assert_eq!(u16::from_le_bytes([reply[2], reply[3]]), 7);
        assert_eq!(u32::from_le_bytes(reply[4..8].try_into().unwrap()), 0);
        assert_eq!(
            u32::from_le_bytes(reply[8..12].try_into().unwrap()),
            MAJOR_VERSION
        );
        assert_eq!(u32::from_le_bytes(reply[12..16].try_into().unwrap()), 0);
    }

    #[test]
    fn opcodes_match_xfixes_xml() {
        // Pinned against /usr/share/xcb/xfixes.xml. Today's session
        // shipped CHANGE_CURSOR_BY_NAME=23, which is actually
        // SetCursorName. Real ChangeCursorByName is opcode 27.
        assert_eq!(QUERY_VERSION, 0);
        assert_eq!(CHANGE_SAVE_SET, 1);
        assert_eq!(SELECT_SELECTION_INPUT, 2);
        assert_eq!(SELECT_CURSOR_INPUT, 3);
        assert_eq!(GET_CURSOR_IMAGE, 4);
        assert_eq!(CREATE_REGION, 5);
        assert_eq!(DESTROY_REGION, 10);
        assert_eq!(CREATE_REGION_FROM_PICTURE, 9);
        assert_eq!(COPY_REGION, 12);
        assert_eq!(FETCH_REGION, 19);
        assert_eq!(SET_GC_CLIP_REGION, 20);
        assert_eq!(SET_WINDOW_SHAPE_REGION, 21);
        assert_eq!(SET_PICTURE_CLIP_REGION, 22);
        assert_eq!(SET_CURSOR_NAME, 23);
        assert_eq!(CHANGE_CURSOR_BY_NAME, 27);
        assert_eq!(HIDE_CURSOR, 29);
        assert_eq!(SHOW_CURSOR, 30);
    }

    #[test]
    fn parse_create_pointer_barrier_basic() {
        // body (after generic header): barrier@0, window@4, x1@8,y1@10,
        // x2@12,y2@14, directions@16, pad@20, num_devices@22, devices@24.
        let mut body = Vec::new();
        body.extend_from_slice(&0x00aa_bbccu32.to_le_bytes()); // barrier
        body.extend_from_slice(&0x0000_0001u32.to_le_bytes()); // window (root)
        body.extend_from_slice(&100i16.to_le_bytes()); // x1
        body.extend_from_slice(&0i16.to_le_bytes()); // y1
        body.extend_from_slice(&100i16.to_le_bytes()); // x2 (vertical)
        body.extend_from_slice(&200i16.to_le_bytes()); // y2
        body.extend_from_slice(&0x0000_00FFu32.to_le_bytes()); // directions (only low 4 kept)
        body.extend_from_slice(&0u16.to_le_bytes()); // pad
        body.extend_from_slice(&2u16.to_le_bytes()); // num_devices
        body.extend_from_slice(&2u16.to_le_bytes()); // device 2
        body.extend_from_slice(&0u16.to_le_bytes()); // device 0 (XIAllDevices)

        let b = parse_create_pointer_barrier(&body).expect("parse");
        assert_eq!(b.barrier, 0x00aa_bbcc);
        assert_eq!(b.window, 1);
        assert_eq!((b.x1, b.y1, b.x2, b.y2), (100, 0, 100, 200));
        assert_eq!(b.directions, 0x0f, "only low 4 bits kept");
        assert_eq!(b.devices, vec![2u16, 0]);
    }

    #[test]
    fn parse_create_pointer_barrier_truncated_is_none() {
        assert!(parse_create_pointer_barrier(&[0u8; 10]).is_none());
        // num_devices says 3 but body has only 1 device worth of bytes
        let mut body = vec![0u8; 24];
        body[22] = 3; // num_devices = 3
        assert!(parse_create_pointer_barrier(&body).is_none());
    }

    #[test]
    fn parse_delete_pointer_barrier_reads_xid() {
        let body = 0xdead_beefu32.to_le_bytes();
        assert_eq!(parse_delete_pointer_barrier(&body), Some(0xdead_beef));
        assert_eq!(parse_delete_pointer_barrier(&[0u8; 2]), None);
    }

    #[test]
    fn major_version_meets_mutter_floor() {
        // Mutter/muffin bails with `Window manager error: Mutter requires
        // XFixes 5.0` if the negotiated major version is below 5.
        const { assert!(MAJOR_VERSION >= 5) }
    }

    /// XFixesSelectionNotify wire format (Xorg `xfixes/xfixesint.h`'s
    /// `xXFixesSelectionNotifyEvent` and `xfixes/select.c:158-210`):
    ///   type=first_event+0 (87), subtype, sequence,
    ///   window, owner, selection, timestamp, selection_timestamp,
    ///   12 bytes pad → 32 bytes total.
    ///
    /// Test fixes `first_event = 87` per yserver's XFIXES_FIRST_EVENT
    /// (matches Xorg's allocation for SYNC=83-86, XFIXES=87-88).
    #[test]
    fn selection_notify_set_owner_encodes_full_event() {
        let mut out = Vec::new();
        encode_selection_notify_event(
            &mut out,
            ClientByteOrder::LittleEndian,
            87, // first_event base for XFIXES
            SELECTION_NOTIFY_SET_OWNER,
            SequenceNumber(0x1234),
            0xaabb_ccdd, // window (subscriber)
            0x1122_3344, // owner (new)
            0x5566_7788, // selection
            0xdead_beef, // timestamp
            0xcafe_babe, // selection_timestamp
        );
        assert_eq!(out.len(), 32, "XFixes events are fixed 32 bytes");
        assert_eq!(out[0], 87, "type = first_event + 0 (SelectionNotify)");
        assert_eq!(out[1], 0, "subtype = SetSelectionOwnerNotify (0)");
        assert_eq!(&out[2..4], &0x1234u16.to_le_bytes(), "sequence");
        assert_eq!(&out[4..8], &0xaabb_ccddu32.to_le_bytes(), "window");
        assert_eq!(&out[8..12], &0x1122_3344u32.to_le_bytes(), "owner");
        assert_eq!(&out[12..16], &0x5566_7788u32.to_le_bytes(), "selection");
        assert_eq!(&out[16..20], &0xdead_beefu32.to_le_bytes(), "timestamp");
        assert_eq!(
            &out[20..24],
            &0xcafe_babeu32.to_le_bytes(),
            "selection_timestamp",
        );
        assert_eq!(&out[24..32], &[0u8; 8], "tail padding to 32 bytes");
    }

    /// Subtype byte routes between SetOwner / WindowDestroy /
    /// ClientClose; the rest of the wire is identical.
    #[test]
    fn selection_notify_subtype_byte_routes_three_variants() {
        for subtype in [
            SELECTION_NOTIFY_SET_OWNER,
            SELECTION_NOTIFY_WINDOW_DESTROY,
            SELECTION_NOTIFY_CLIENT_CLOSE,
        ] {
            let mut out = Vec::new();
            encode_selection_notify_event(
                &mut out,
                ClientByteOrder::LittleEndian,
                87,
                subtype,
                SequenceNumber(1),
                1,
                2,
                3,
                4,
                5,
            );
            assert_eq!(out[1], subtype, "subtype byte = {subtype}");
            assert_eq!(out[0], 87, "type stays first_event+0 across subtypes");
        }
    }

    /// Mask bits pair with subtype values (bit N = subtype N).
    #[test]
    fn selection_mask_bits_align_with_subtypes() {
        assert_eq!(SELECTION_MASK_SET_OWNER, 1 << SELECTION_NOTIFY_SET_OWNER);
        assert_eq!(
            SELECTION_MASK_WINDOW_DESTROY,
            1 << SELECTION_NOTIFY_WINDOW_DESTROY,
        );
        assert_eq!(
            SELECTION_MASK_CLIENT_CLOSE,
            1 << SELECTION_NOTIFY_CLIENT_CLOSE,
        );
    }

    #[test]
    fn change_save_set_parses_canonical_layout() {
        // mode(1) | target(1) | map(1) | pad(1) | window(4)
        let body = [0u8, 1, 0, 0, 0xef, 0xbe, 0xad, 0xde];
        let req = parse_change_save_set(&body).unwrap();
        assert_eq!(req.mode, 0);
        assert_eq!(req.target, 1);
        assert_eq!(req.map, 0);
        assert_eq!(req.window, 0xdead_beef);
    }

    #[test]
    fn set_gc_clip_region_parses_canonical_layout() {
        // gc(4) | region(4) | x_origin(2) | y_origin(2)
        let mut body = Vec::new();
        body.extend_from_slice(&0xaa_u32.to_le_bytes());
        body.extend_from_slice(&0xbb_u32.to_le_bytes());
        body.extend_from_slice(&(-3_i16).to_le_bytes());
        body.extend_from_slice(&7_i16.to_le_bytes());
        let req = parse_set_gc_clip_region(&body).unwrap();
        assert_eq!(req.gc, 0xaa);
        assert_eq!(req.region, 0xbb);
        assert_eq!(req.x_origin, -3);
        assert_eq!(req.y_origin, 7);
    }

    #[test]
    fn set_window_shape_region_parses_canonical_layout() {
        // dest(4) | kind(1) | pad(3) | x_off(2) | y_off(2) | region(4)
        let mut body = Vec::new();
        body.extend_from_slice(&0x1234_u32.to_le_bytes());
        body.push(1); // KIND_CLIP
        body.extend_from_slice(&[0u8; 3]);
        body.extend_from_slice(&5_i16.to_le_bytes());
        body.extend_from_slice(&(-9_i16).to_le_bytes());
        body.extend_from_slice(&0x5678_u32.to_le_bytes());
        let req = parse_set_window_shape_region(&body).unwrap();
        assert_eq!(req.dest, 0x1234);
        assert_eq!(req.dest_kind, 1);
        assert_eq!(req.x_offset, 5);
        assert_eq!(req.y_offset, -9);
        assert_eq!(req.region, 0x5678);
    }

    #[test]
    fn set_picture_clip_region_parses_canonical_layout() {
        // picture(4) | region(4) | x_origin(2) | y_origin(2)
        let mut body = Vec::new();
        body.extend_from_slice(&0xcc_u32.to_le_bytes());
        body.extend_from_slice(&0_u32.to_le_bytes()); // None
        body.extend_from_slice(&0_i16.to_le_bytes());
        body.extend_from_slice(&0_i16.to_le_bytes());
        let req = parse_set_picture_clip_region(&body).unwrap();
        assert_eq!(req.picture, 0xcc);
        assert_eq!(req.region, 0); // None — caller clears the picture clip.
    }

    #[test]
    fn create_region_from_window_parses_canonical_layout() {
        // region(4) | window(4) | kind(1) | pad(3)
        let mut body = Vec::new();
        body.extend_from_slice(&0x1234_u32.to_le_bytes());
        body.extend_from_slice(&0xabcd_u32.to_le_bytes());
        body.push(1); // KIND_CLIP
        body.extend_from_slice(&[0u8; 3]);
        let req = parse_create_region_from_window(&body).unwrap();
        assert_eq!(req.0, 0x1234);
        assert_eq!(req.1, 0xabcd);
        assert_eq!(req.2, 1);
    }

    #[test]
    fn create_region_from_picture_parses_canonical_layout() {
        // region(4) | picture(4)
        let mut body = Vec::new();
        body.extend_from_slice(&0x1234_u32.to_le_bytes());
        body.extend_from_slice(&0xabcd_u32.to_le_bytes());
        let req = parse_create_region_from_picture(&body).unwrap();
        assert_eq!(req.0, 0x1234);
        assert_eq!(req.1, 0xabcd);
    }

    #[test]
    fn set_gc_clip_region_rejects_short_body() {
        assert!(parse_set_gc_clip_region(&[0u8; 11]).is_none());
    }

    #[test]
    fn cursor_image_empty_reply_shape() {
        let reply =
            encode_get_cursor_image_empty_reply(ClientByteOrder::LittleEndian, SequenceNumber(9));
        assert_eq!(reply.len(), 32);
        assert_eq!(u32::from_le_bytes(reply[4..8].try_into().unwrap()), 0);
        assert_eq!(u16::from_le_bytes(reply[12..14].try_into().unwrap()), 0);
        assert_eq!(u16::from_le_bytes(reply[14..16].try_into().unwrap()), 0);
    }

    #[test]
    fn change_cursor_by_name_parses_unpadded_name() {
        // body: cursor(4) | nbytes(2) | pad(2) | name(nbytes) padded to 4
        let mut body = Vec::new();
        body.extend_from_slice(&0xdead_beef_u32.to_le_bytes());
        let name = b"left_ptr"; // 8 bytes — already 4-byte aligned, no pad
        body.extend_from_slice(&(name.len() as u16).to_le_bytes());
        body.extend_from_slice(&0u16.to_le_bytes());
        body.extend_from_slice(name);

        let (cursor, parsed_name) = parse_change_cursor_by_name(&body).unwrap();
        assert_eq!(cursor, 0xdead_beef);
        assert_eq!(parsed_name, name);
    }

    #[test]
    fn change_cursor_by_name_handles_4_byte_padding() {
        // 6-byte name → 2 padding bytes follow.
        let mut body = Vec::new();
        body.extend_from_slice(&1_u32.to_le_bytes());
        let name = b"hand1\0".strip_suffix(b"\0").unwrap(); // 5 bytes
        body.extend_from_slice(&(name.len() as u16).to_le_bytes());
        body.extend_from_slice(&0u16.to_le_bytes());
        body.extend_from_slice(name);
        body.push(0); // pad to 4-byte alignment
        body.push(0); // pad to 4-byte alignment
        body.push(0); // pad to 4-byte alignment

        let (cursor, parsed_name) = parse_change_cursor_by_name(&body).unwrap();
        assert_eq!(cursor, 1);
        assert_eq!(parsed_name, name, "padding bytes must not leak into name");
    }

    #[test]
    fn change_cursor_by_name_rejects_truncated_input() {
        // Body claims 100 name bytes but provides 4.
        let mut body = Vec::new();
        body.extend_from_slice(&1_u32.to_le_bytes());
        body.extend_from_slice(&100_u16.to_le_bytes());
        body.extend_from_slice(&0u16.to_le_bytes());
        body.extend_from_slice(b"oops");
        assert!(parse_change_cursor_by_name(&body).is_none());
    }

    #[test]
    fn change_cursor_by_name_rejects_short_header() {
        // Less than 8 bytes — header itself doesn't fit.
        assert!(parse_change_cursor_by_name(&[1, 2, 3, 4]).is_none());
    }

    #[test]
    fn fetch_region_reply_shape() {
        let rects = [RegionRect {
            x: 1,
            y: 2,
            width: 3,
            height: 4,
        }];
        let reply = encode_fetch_region_reply(
            ClientByteOrder::LittleEndian,
            SequenceNumber(3),
            rects[0],
            &rects,
        );
        assert_eq!(reply.len(), 40);
        assert_eq!(u32::from_le_bytes(reply[4..8].try_into().unwrap()), 2);
        assert_eq!(i16::from_le_bytes(reply[8..10].try_into().unwrap()), 1);
        assert_eq!(i16::from_le_bytes(reply[32..34].try_into().unwrap()), 1);
    }
}
