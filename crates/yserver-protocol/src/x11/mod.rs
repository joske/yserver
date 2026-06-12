use std::io::{self, ErrorKind, Read, Write};

mod atoms;
pub use atoms::{well_known_atom, well_known_atom_name};

mod colors;
pub use colors::lookup_color_name;

mod keysyms;
use keysyms::keysyms_for_keycode;

mod wire;
pub use wire::*;

pub mod composite;
pub mod damage;
pub mod dpms;
pub mod dri3;
pub mod glx;
pub mod mit_shm;
pub mod present;
pub mod randr;
pub mod request_lengths;
pub mod request_swap;
pub mod screensaver;
pub mod shape;
pub mod sync;
pub mod wire_swap;
pub mod x_resource;
pub mod xfixes;
pub mod xinerama;
pub mod xtest;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClientByteOrder {
    LittleEndian,
    BigEndian,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ClientId(pub u32);

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct ResourceId(pub u32);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct AtomId(pub u32);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SequenceNumber(pub u16);

impl SequenceNumber {
    pub fn next(self) -> Self {
        Self(self.0.wrapping_add(1))
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Rgb16 {
    pub red: u16,
    pub green: u16,
    pub blue: u16,
}

#[derive(Debug)]
pub struct SetupRequest {
    pub byte_order: ClientByteOrder,
    pub protocol_major: u16,
    pub protocol_minor: u16,
    pub auth_protocol_name: Vec<u8>,
    pub auth_protocol_data: Vec<u8>,
}

#[derive(Debug)]
pub struct SetupSuccess<'a> {
    pub protocol_major: u16,
    pub protocol_minor: u16,
    pub release_number: u32,
    pub resource_id_base: u32,
    pub resource_id_mask: u32,
    pub motion_buffer_size: u32,
    pub maximum_request_length: u16,
    pub image_byte_order: ClientByteOrder,
    pub bitmap_format_bit_order: ClientByteOrder,
    pub bitmap_format_scanline_unit: u8,
    pub bitmap_format_scanline_pad: u8,
    pub min_keycode: u8,
    pub max_keycode: u8,
    pub vendor: &'a str,
    pub root: Screen,
}

#[derive(Clone, Copy, Debug)]
pub struct Screen {
    pub root: ResourceId,
    pub default_colormap: ResourceId,
    pub white_pixel: u32,
    pub black_pixel: u32,
    pub current_input_masks: u32,
    pub width_px: u16,
    pub height_px: u16,
    pub width_mm: u16,
    pub height_mm: u16,
    pub min_installed_maps: u16,
    pub max_installed_maps: u16,
    pub root_visual: ResourceId,
    pub argb_visual: ResourceId,
    pub root_depth: u8,
}

#[derive(Clone, Copy, Debug)]
pub struct RequestHeader {
    pub opcode: u8,
    pub data: u8,
    pub length_units: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct KeyEvent {
    pub pressed: bool,
    pub keycode: u8,
    pub sequence: SequenceNumber,
    pub time: u32,
    pub root: ResourceId,
    pub event: ResourceId,
    pub root_x: i16,
    pub root_y: i16,
    pub event_x: i16,
    pub event_y: i16,
    pub state: u16,
}

#[derive(Clone, Copy, Debug)]
pub struct PointerEvent {
    pub sequence: SequenceNumber,
    pub detail: u8,
    pub time: u32,
    pub root: ResourceId,
    pub event: ResourceId,
    /// Topmost child of `event` that contains the pointer (i.e. the
    /// immediate descendant of the propagation target on the path to
    /// the source window). `ResourceId(0)` (the X11 `None` sentinel)
    /// when the source IS the event window — this is what window
    /// managers use to distinguish bare-root clicks from clicks
    /// propagated up from an app window.
    pub child: ResourceId,
    pub root_x: i16,
    pub root_y: i16,
    pub event_x: i16,
    pub event_y: i16,
    pub state: u16,
}

#[derive(Clone, Copy, Debug)]
pub struct CrossingEvent {
    pub sequence: SequenceNumber,
    pub time: u32,
    pub root: ResourceId,
    pub event: ResourceId,
    /// X11 EnterNotify/LeaveNotify `child`: if the source window is an
    /// inferior of `event`, this is the child of `event` on the path to
    /// the source (or the source itself if it IS a direct child of
    /// `event`). `ResourceId(0)` (the X11 `None` sentinel) when the
    /// source IS the event window. WMs that select Enter/Leave on the
    /// root gate hover behavior on whether `child == None` (pointer on
    /// bare root) vs. some xid (pointer over a child of root).
    pub child: ResourceId,
    pub root_x: i16,
    pub root_y: i16,
    pub event_x: i16,
    pub event_y: i16,
    pub state: u16,
    /// X11 detail: 0=NotifyAncestor, 1=NotifyVirtual, 2=NotifyInferior,
    /// 3=NotifyNonlinear, 4=NotifyNonlinearVirtual.
    pub detail: u8,
    /// X11 mode: 0=NotifyNormal, 1=NotifyGrab, 2=NotifyUngrab.
    pub mode: u8,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct CreateWindowRequest {
    pub depth: u8,
    pub window: ResourceId,
    pub parent: ResourceId,
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
    pub border_width: u16,
    pub class: u16,
    pub visual: ResourceId,
    /// Raw CW value_mask preserved for downstream validation (e.g. the
    /// InputOnly legal-mask check needs to know exactly which bits the
    /// client set, not just which typed fields the parser decoded).
    pub value_mask: u32,
    /// CW bit 0. 0 = None, 1 = ParentRelative, else a pixmap XID.
    pub background_pixmap: Option<ResourceId>,
    pub background_pixel: Option<u32>,
    pub bit_gravity: Option<u8>,
    pub win_gravity: Option<u8>,
    pub backing_store: Option<u8>,
    pub backing_planes: Option<u32>,
    pub backing_pixel: Option<u32>,
    pub override_redirect: Option<bool>,
    pub save_under: Option<bool>,
    pub event_mask: Option<u32>,
    pub do_not_propagate_mask: Option<u16>,
    /// `Some(None)` = explicit `CopyFromParent` (XID 0); `Some(Some(_))` =
    /// concrete colormap; `None` = bit not set.
    pub colormap: Option<Option<ResourceId>>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ChangeWindowAttributesRequest {
    pub window: ResourceId,
    /// Raw CW value_mask preserved for InputOnly legal-mask validation.
    pub value_mask: u32,
    pub background_pixmap: Option<ResourceId>,
    pub background_pixel: Option<u32>,
    pub bit_gravity: Option<u8>,
    pub win_gravity: Option<u8>,
    pub backing_store: Option<u8>,
    pub backing_planes: Option<u32>,
    pub backing_pixel: Option<u32>,
    pub override_redirect: Option<bool>,
    pub save_under: Option<bool>,
    pub event_mask: Option<u32>,
    pub do_not_propagate_mask: Option<u16>,
    pub colormap: Option<Option<ResourceId>>,
    pub cursor: Option<ResourceId>,
}

#[derive(Clone, Copy, Debug)]
pub struct ConfigureWindowRequest {
    pub window: ResourceId,
    pub value_mask: u16,
    pub x: Option<i16>,
    pub y: Option<i16>,
    pub width: Option<u16>,
    pub height: Option<u16>,
    pub border_width: Option<u16>,
    pub sibling: Option<ResourceId>,
    pub stack_mode: Option<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReparentWindowRequest {
    pub window: ResourceId,
    pub parent: ResourceId,
    pub x: i16,
    pub y: i16,
}

#[derive(Clone, Copy, Debug)]
pub struct CreatePixmapRequest {
    pub depth: u8,
    pub pixmap: ResourceId,
    pub drawable: ResourceId,
    pub width: u16,
    pub height: u16,
}

/// All 23 attribute slots of an X11 CreateGC request, post-mask-parse.
/// `None` means the value-mask bit was clear and the attribute was not
/// supplied. The enum-typed fields carry the raw protocol byte; the
/// caller (yserver-core) maps it into its `LineStyle` / `CapStyle` /
/// etc. enums, with unknown values clamped to the X11 default.
#[derive(Clone, Copy, Debug)]
pub struct CreateGcRequest {
    pub gc: ResourceId,
    pub drawable: ResourceId,
    pub function: Option<u8>,
    pub plane_mask: Option<u32>,
    pub foreground: Option<u32>,
    pub background: Option<u32>,
    pub line_width: Option<u16>,
    pub line_style: Option<u8>,
    pub cap_style: Option<u8>,
    pub join_style: Option<u8>,
    pub fill_style: Option<u8>,
    pub fill_rule: Option<u8>,
    pub tile: Option<ResourceId>,
    pub stipple: Option<ResourceId>,
    pub tile_x_origin: Option<i16>,
    pub tile_y_origin: Option<i16>,
    pub font: Option<ResourceId>,
    pub subwindow_mode: Option<u8>,
    pub graphics_exposures: Option<bool>,
    pub clip_x_origin: Option<i16>,
    pub clip_y_origin: Option<i16>,
    pub clip_mask: Option<Option<ResourceId>>,
    pub dash_offset: Option<u16>,
    pub dashes: Option<u8>,
    pub arc_mode: Option<u8>,
}

#[derive(Clone, Copy, Debug)]
pub struct GcChange {
    pub gc: ResourceId,
    pub function: Option<u8>,
    pub plane_mask: Option<u32>,
    pub foreground: Option<u32>,
    pub background: Option<u32>,
    pub line_width: Option<u16>,
    pub line_style: Option<u8>,
    pub cap_style: Option<u8>,
    pub join_style: Option<u8>,
    pub fill_style: Option<u8>,
    pub fill_rule: Option<u8>,
    pub tile: Option<ResourceId>,
    pub stipple: Option<ResourceId>,
    pub tile_x_origin: Option<i16>,
    pub tile_y_origin: Option<i16>,
    pub font: Option<ResourceId>,
    pub subwindow_mode: Option<u8>,
    pub graphics_exposures: Option<bool>,
    pub clip_mask: Option<Option<ResourceId>>,
    pub clip_x_origin: Option<i16>,
    pub clip_y_origin: Option<i16>,
    pub dash_offset: Option<u16>,
    pub dashes: Option<u8>,
    pub arc_mode: Option<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClipRectangles {
    pub ordering: u8,
    pub x_origin: i16,
    pub y_origin: i16,
    pub rectangles: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SetClipRectanglesRequest {
    pub gc: ResourceId,
    pub clip: ClipRectangles,
}

#[derive(Clone, Copy, Debug)]
pub struct ClearAreaRequest {
    pub window: ResourceId,
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CopyAreaRequest {
    pub src: ResourceId,
    pub dst: ResourceId,
    pub gc: ResourceId,
    pub src_x: i16,
    pub src_y: i16,
    pub dst_x: i16,
    pub dst_y: i16,
    pub width: u16,
    pub height: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImageFormat {
    XyBitmap,
    XyPixmap,
    ZPixmap,
    Unknown(u8),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PutImageRequest<'a> {
    pub format: ImageFormat,
    pub drawable: ResourceId,
    pub gc: ResourceId,
    pub width: u16,
    pub height: u16,
    pub dst_x: i16,
    pub dst_y: i16,
    pub left_pad: u8,
    pub depth: u8,
    pub data: &'a [u8],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SendEventRequest<'a> {
    pub propagate: bool,
    pub destination: ResourceId,
    pub event_mask: u32,
    pub event: &'a [u8; 32],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClientMessageEvent {
    pub sequence: SequenceNumber,
    pub send_event: bool,
    pub format: u8,
    pub window: ResourceId,
    pub r#type: AtomId,
    pub data: [u8; 20],
}

#[derive(Clone, Debug)]
pub struct OpenFontRequest {
    pub font: ResourceId,
    pub name: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CharInfo {
    pub left_side_bearing: i16,
    pub right_side_bearing: i16,
    pub character_width: i16,
    pub ascent: i16,
    pub descent: i16,
    pub attributes: u16,
}

/// A font property value as read from the font file (BDF/PCF
/// properties). String values become atoms at the request layer
/// (the server's atom table isn't visible to backends).
#[derive(Clone, Debug)]
pub enum FontPropValue {
    Card(u32),
    Int(i32),
    Str(String),
}

#[derive(Clone, Debug, Default)]
pub struct FontMetrics {
    pub min_bounds: CharInfo,
    pub max_bounds: CharInfo,
    pub min_char_or_byte2: u16,
    pub max_char_or_byte2: u16,
    pub default_char: u16,
    pub draw_direction: u8,
    pub min_byte1: u8,
    pub max_byte1: u8,
    pub all_chars_exist: bool,
    pub font_ascent: i16,
    pub font_descent: i16,
    /// Wire-format (atom, value) u32 pairs, little-endian — filled at
    /// the request layer (atoms need the server table).
    pub properties: Vec<u8>,
    /// Named properties straight from the font file (BDF/PCF);
    /// converted to wire `properties` by the request layer. Empty for
    /// faces without embedded properties (scalables) — the request
    /// layer then synthesizes XLFD-derived properties instead.
    pub named_properties: Vec<(String, FontPropValue)>,
    pub char_infos: Vec<CharInfo>,
}

impl FontMetrics {
    pub fn char_info(&self, byte1: u8, byte2: u8) -> Option<&CharInfo> {
        let byte2_u16 = u16::from(byte2);
        if u16::from(byte1) < u16::from(self.min_byte1)
            || u16::from(byte1) > u16::from(self.max_byte1)
        {
            return None;
        }
        if byte2_u16 < self.min_char_or_byte2 || byte2_u16 > self.max_char_or_byte2 {
            return None;
        }

        if self.min_byte1 == 0 && self.max_byte1 == 0 {
            let index = usize::from(byte2_u16 - self.min_char_or_byte2);
            return self.char_infos.get(index);
        }

        let row_len = usize::from(self.max_char_or_byte2 - self.min_char_or_byte2 + 1);
        let row = usize::from(byte1 - self.min_byte1);
        let col = usize::from(byte2_u16 - self.min_char_or_byte2);
        self.char_infos.get(row * row_len + col)
    }

    pub fn text_extents(&self, chars: &[(u8, u8)]) -> TextExtents {
        let mut extents = TextExtents {
            draw_direction: self.draw_direction,
            font_ascent: self.font_ascent,
            font_descent: self.font_descent,
            ..Default::default()
        };
        if chars.is_empty() {
            return extents;
        }

        let fallback = if self.all_chars_exist {
            None
        } else {
            self.char_info(
                u8::try_from(self.default_char >> 8).unwrap_or(0),
                u8::try_from(self.default_char & 0xff).unwrap_or(0),
            )
        };

        let mut running_width: i32 = 0;
        let mut overall_left = i32::MAX;
        let mut overall_right = i32::MIN;
        let mut overall_ascent: i16 = 0;
        let mut overall_descent: i16 = 0;

        for &(byte1, byte2) in chars {
            let info = self.char_info(byte1, byte2).or(fallback);
            let Some(info) = info else {
                continue;
            };
            let left = running_width + i32::from(info.left_side_bearing);
            let right = running_width + i32::from(info.right_side_bearing);
            if left < overall_left {
                overall_left = left;
            }
            if right > overall_right {
                overall_right = right;
            }
            if info.ascent > overall_ascent {
                overall_ascent = info.ascent;
            }
            if info.descent > overall_descent {
                overall_descent = info.descent;
            }
            running_width += i32::from(info.character_width);
        }

        extents.overall_width = running_width;
        extents.overall_ascent = overall_ascent;
        extents.overall_descent = overall_descent;
        extents.overall_left = if overall_left == i32::MAX {
            0
        } else {
            overall_left
        };
        extents.overall_right = if overall_right == i32::MIN {
            0
        } else {
            overall_right
        };
        extents
    }
}

#[derive(Clone, Copy, Debug)]
pub struct WindowAttributes {
    pub visual: ResourceId,
    pub class: u16,
    pub bit_gravity: u8,
    pub win_gravity: u8,
    pub backing_store: u8,
    pub backing_planes: u32,
    pub backing_pixel: u32,
    pub save_under: bool,
    pub map_is_installed: bool,
    pub map_state: u8,
    pub override_redirect: bool,
    pub colormap: ResourceId,
    pub all_event_masks: u32,
    pub your_event_mask: u32,
    pub do_not_propagate_mask: u16,
}

#[derive(Clone, Copy, Debug)]
pub struct Geometry {
    pub root: ResourceId,
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
    pub border_width: u16,
    pub depth: u8,
}

pub fn read_setup_request(reader: &mut impl Read) -> io::Result<SetupRequest> {
    let mut header = [0; 12];
    reader.read_exact(&mut header)?;

    let byte_order = match header[0] {
        b'l' => ClientByteOrder::LittleEndian,
        b'B' => ClientByteOrder::BigEndian,
        byte => {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                format!("invalid X11 byte order marker {byte}"),
            ));
        }
    };

    let protocol_major = read_u16(byte_order, &header[2..4]);
    let protocol_minor = read_u16(byte_order, &header[4..6]);
    let auth_name_len = read_u16(byte_order, &header[6..8]) as usize;
    let auth_data_len = read_u16(byte_order, &header[8..10]) as usize;

    let mut auth_protocol_name = vec![0; pad4(auth_name_len)];
    reader.read_exact(&mut auth_protocol_name)?;
    auth_protocol_name.truncate(auth_name_len);

    let mut auth_protocol_data = vec![0; pad4(auth_data_len)];
    reader.read_exact(&mut auth_protocol_data)?;
    auth_protocol_data.truncate(auth_data_len);

    Ok(SetupRequest {
        byte_order,
        protocol_major,
        protocol_minor,
        auth_protocol_name,
        auth_protocol_data,
    })
}

pub fn write_setup_failed(
    writer: &mut impl Write,
    byte_order: ClientByteOrder,
    reason: &str,
) -> io::Result<()> {
    let reason_len = reason.len().min(u8::MAX as usize);
    let mut body = Vec::new();
    body.push(0);
    body.push(reason_len as u8);
    write_u16(byte_order, &mut body, 11);
    write_u16(byte_order, &mut body, 0);
    body.extend_from_slice(&reason.as_bytes()[..reason_len]);
    pad_vec4(&mut body);
    writer.write_all(&body)
}

pub fn write_error(
    writer: &mut impl Write,
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    error_code: u8,
    bad_value: u32,
    minor_opcode: u16,
    major_opcode: u8,
) -> io::Result<()> {
    let mut error = Vec::with_capacity(32);
    error.push(0);
    error.push(error_code);
    write_u16(byte_order, &mut error, sequence.0);
    write_u32(byte_order, &mut error, bad_value);
    write_u16(byte_order, &mut error, minor_opcode);
    error.push(major_opcode);
    error.extend_from_slice(&[0; 21]);
    writer.write_all(&error)
}

pub fn write_setup_success(
    writer: &mut impl Write,
    byte_order: ClientByteOrder,
    setup: SetupSuccess<'_>,
) -> io::Result<()> {
    let vendor = setup.vendor.as_bytes();

    let mut extra = Vec::new();
    write_u32(byte_order, &mut extra, setup.release_number);
    write_u32(byte_order, &mut extra, setup.resource_id_base);
    write_u32(byte_order, &mut extra, setup.resource_id_mask);
    write_u32(byte_order, &mut extra, setup.motion_buffer_size);
    write_u16(byte_order, &mut extra, vendor.len() as u16);
    write_u16(byte_order, &mut extra, setup.maximum_request_length);
    extra.push(1); // roots
    extra.push(7); // pixmap formats: depth=1, 4, 8, 15, 16, 24, 32
    extra.push(byte_order_value(setup.image_byte_order));
    extra.push(byte_order_value(setup.bitmap_format_bit_order));
    extra.push(setup.bitmap_format_scanline_unit);
    extra.push(setup.bitmap_format_scanline_pad);
    extra.push(setup.min_keycode);
    extra.push(setup.max_keycode);
    extra.extend_from_slice(&[0; 4]);

    extra.extend_from_slice(vendor);
    pad_vec4(&mut extra);

    // pixmap format: depth=1, bits-per-pixel=1, scanline-pad=32
    extra.push(1);
    extra.push(1);
    extra.push(32);
    extra.extend_from_slice(&[0; 5]);

    // pixmap format: depth=4, bits-per-pixel=4, scanline-pad=32
    extra.push(4);
    extra.push(4);
    extra.push(32);
    extra.extend_from_slice(&[0; 5]);

    // pixmap format: depth=8, bits-per-pixel=8, scanline-pad=32
    extra.push(8);
    extra.push(8);
    extra.push(32);
    extra.extend_from_slice(&[0; 5]);

    // pixmap format: depth=15, bits-per-pixel=16, scanline-pad=32
    extra.push(15);
    extra.push(16);
    extra.push(32);
    extra.extend_from_slice(&[0; 5]);

    // pixmap format: depth=16, bits-per-pixel=16, scanline-pad=32
    extra.push(16);
    extra.push(16);
    extra.push(32);
    extra.extend_from_slice(&[0; 5]);

    // pixmap format: depth=24, bits-per-pixel=32, scanline-pad=32
    extra.push(24);
    extra.push(32);
    extra.push(32);
    extra.extend_from_slice(&[0; 5]);

    // pixmap format: depth=32, bits-per-pixel=32, scanline-pad=32
    extra.push(32);
    extra.push(32);
    extra.push(32);
    extra.extend_from_slice(&[0; 5]);

    write_screen(byte_order, &mut extra, setup.root);

    let length_units = checked_units(extra.len())?;
    let mut reply = Vec::with_capacity(8 + extra.len());
    reply.push(1);
    reply.push(0);
    write_u16(byte_order, &mut reply, setup.protocol_major);
    write_u16(byte_order, &mut reply, setup.protocol_minor);
    write_u16(byte_order, &mut reply, length_units);
    reply.extend_from_slice(&extra);
    writer.write_all(&reply)
}

fn write_screen(byte_order: ClientByteOrder, out: &mut Vec<u8>, screen: Screen) {
    write_u32(byte_order, out, screen.root.0);
    write_u32(byte_order, out, screen.default_colormap.0);
    write_u32(byte_order, out, screen.white_pixel);
    write_u32(byte_order, out, screen.black_pixel);
    write_u32(byte_order, out, screen.current_input_masks);
    write_u16(byte_order, out, screen.width_px);
    write_u16(byte_order, out, screen.height_px);
    write_u16(byte_order, out, screen.width_mm);
    write_u16(byte_order, out, screen.height_mm);
    write_u16(byte_order, out, screen.min_installed_maps);
    write_u16(byte_order, out, screen.max_installed_maps);
    write_u32(byte_order, out, screen.root_visual.0);
    out.push(0); // backing stores: Never
    out.push(0); // save unders: false
    out.push(screen.root_depth);
    out.push(5); // allowed depths: depth=1, depth=4, depth=8, depth=24, depth=32

    // depth=1, no visuals
    out.push(1);
    out.push(0);
    write_u16(byte_order, out, 0);
    write_u32(byte_order, out, 0);

    // depth=4, no visuals
    out.push(4);
    out.push(0);
    write_u16(byte_order, out, 0);
    write_u32(byte_order, out, 0);

    // depth=8, no visuals
    out.push(8);
    out.push(0);
    write_u16(byte_order, out, 0);
    write_u32(byte_order, out, 0);

    // depth=24 (root depth), 1 visual: TrueColor RGB
    out.push(24);
    out.push(0);
    write_u16(byte_order, out, 1);
    write_u32(byte_order, out, 0);
    write_u32(byte_order, out, screen.root_visual.0);
    out.push(4); // TrueColor
    out.push(8); // bits per rgb
    write_u16(byte_order, out, 256);
    write_u32(byte_order, out, 0x00ff_0000);
    write_u32(byte_order, out, 0x0000_ff00);
    write_u32(byte_order, out, 0x0000_00ff);
    write_u32(byte_order, out, 0);

    // depth=32, 1 visual: TrueColor ARGB
    out.push(32);
    out.push(0);
    write_u16(byte_order, out, 1);
    write_u32(byte_order, out, 0);
    write_u32(byte_order, out, screen.argb_visual.0);
    out.push(4); // TrueColor
    out.push(8); // bits per rgb
    write_u16(byte_order, out, 256);
    write_u32(byte_order, out, 0x00ff_0000);
    write_u32(byte_order, out, 0x0000_ff00);
    write_u32(byte_order, out, 0x0000_00ff);
    write_u32(byte_order, out, 0xff00_0000); // alpha mask
}

pub fn read_request(
    reader: &mut impl Read,
    byte_order: ClientByteOrder,
    big_requests_enabled: bool,
) -> io::Result<Option<(RequestHeader, Vec<u8>)>> {
    let mut header = [0; 4];
    match reader.read_exact(&mut header) {
        Ok(()) => {}
        Err(err)
            if matches!(
                err.kind(),
                ErrorKind::UnexpectedEof | ErrorKind::ConnectionReset | ErrorKind::BrokenPipe
            ) =>
        {
            return Ok(None);
        }
        Err(err) => return Err(err),
    }

    let mut length_units = u32::from(read_u16(byte_order, &header[2..4]));
    let body_len;
    let mut malformed = false;

    if length_units == 0 && big_requests_enabled {
        let mut big_len = [0; 4];
        reader.read_exact(&mut big_len)?;
        length_units = read_u32(byte_order, &big_len);
        if length_units < 2 {
            // Malformed BIG-REQUESTS: the 4-byte std header + 4-byte
            // big-length field is itself 2 units, so a claimed total
            // < 2 cannot fit the header. xts5 ListInputDevices-2
            // sends `length=0, big=1` (a "1-unit BIG") and expects
            // BadLength. Flag it so the dispatcher emits BadLength.
            // No body to drain — for sub-header lengths the test
            // client sends exactly the 8-byte (std + big) header.
            malformed = true;
            body_len = 0;
        } else {
            body_len = (length_units as usize * 4) - 8;
        }
    } else {
        if length_units < 1 {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                format!("invalid request length {}", length_units),
            ));
        }
        body_len = (length_units as usize * 4) - 4;
    }

    // Cap absurd lengths so we don't allocate gigabytes of zeros for
    // a request the dispatcher is about to reject. xts5's "length one
    // greater than the maximum" probes (TOO_LONG) send
    // `bigRequestLength = server_max + 1` AND the corresponding
    // payload bytes on the wire; we have to drain those bytes to keep
    // the socket in sync, but we don't have to allocate a buffer the
    // size of the payload to do it. Flag the request as malformed so
    // the dispatcher's max-length gate emits BadLength.
    let max_units: u32 = if big_requests_enabled {
        256 * 1024
    } else {
        u32::from(u16::MAX)
    };
    if !malformed && length_units > max_units {
        malformed = true;
    }

    let request = RequestHeader {
        opcode: header[0],
        data: header[1],
        // Override length to a sentinel that the dispatcher's
        // `header.length_units > max_length_units` check will reject
        // with BadLength. Preserves opcode/minor so the error reply
        // names the right request.
        length_units: if malformed { u32::MAX } else { length_units },
    };

    if malformed {
        // Drain the claimed body in 64 KiB chunks so we don't
        // allocate body_len bytes just to throw them away. xts5
        // TOO_LONG actually sends the over-max payload, so the
        // bytes WILL arrive — we just don't care what they are.
        if body_len > 0 {
            let mut sink = [0u8; 65_536];
            let mut remaining = body_len;
            while remaining > 0 {
                let take = remaining.min(sink.len());
                reader.read_exact(&mut sink[..take])?;
                remaining -= take;
            }
        }
        return Ok(Some((request, Vec::new())));
    }

    let mut body = vec![0; body_len];
    reader.read_exact(&mut body)?;
    Ok(Some((request, body)))
}

pub fn intern_atom_name(body: &[u8]) -> String {
    if body.len() < 4 {
        return String::new();
    }
    let len = u16::from_le_bytes([body[0], body[1]]) as usize;
    let name = body.get(4..4 + len).unwrap_or_default();
    String::from_utf8_lossy(name).into_owned()
}

pub fn request_atom(body: &[u8]) -> AtomId {
    if body.len() < 4 {
        return AtomId(0);
    }
    AtomId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]))
}

pub fn create_window_request(depth: u8, body: &[u8]) -> Option<CreateWindowRequest> {
    let value_mask = read_u32_le(body.get(24..28)?);
    let values = value_list(value_mask, body.get(28..)?);
    Some(CreateWindowRequest {
        depth,
        window: ResourceId(read_u32_le(body.get(0..4)?)),
        parent: ResourceId(read_u32_le(body.get(4..8)?)),
        x: read_i16_le(body.get(8..10)?),
        y: read_i16_le(body.get(10..12)?),
        width: read_u16_le(body.get(12..14)?),
        height: read_u16_le(body.get(14..16)?),
        border_width: read_u16_le(body.get(16..18)?),
        class: read_u16_le(body.get(18..20)?),
        visual: ResourceId(read_u32_le(body.get(20..24)?)),
        value_mask,
        background_pixmap: values.value(0).map(ResourceId),
        background_pixel: values.value(1),
        bit_gravity: values.value(4).map(|v| v as u8),
        win_gravity: values.value(5).map(|v| v as u8),
        backing_store: values.value(6).map(|v| v as u8),
        backing_planes: values.value(7),
        backing_pixel: values.value(8),
        override_redirect: values.value(9).map(|v| v != 0),
        save_under: values.value(10).map(|v| v != 0),
        event_mask: values.value(11),
        do_not_propagate_mask: values.value(12).map(|v| v as u16),
        colormap: values
            .value(13)
            .map(|v| if v == 0 { None } else { Some(ResourceId(v)) }),
    })
}

pub fn change_window_attributes_request(body: &[u8]) -> Option<ChangeWindowAttributesRequest> {
    let value_mask = read_u32_le(body.get(4..8)?);
    let values = value_list(value_mask, body.get(8..)?);
    Some(ChangeWindowAttributesRequest {
        window: ResourceId(read_u32_le(body.get(0..4)?)),
        value_mask,
        background_pixmap: values.value(0).map(ResourceId),
        background_pixel: values.value(1),
        bit_gravity: values.value(4).map(|v| v as u8),
        win_gravity: values.value(5).map(|v| v as u8),
        backing_store: values.value(6).map(|v| v as u8),
        backing_planes: values.value(7),
        backing_pixel: values.value(8),
        override_redirect: values.value(9).map(|v| v != 0),
        save_under: values.value(10).map(|v| v != 0),
        event_mask: values.value(11),
        do_not_propagate_mask: values.value(12).map(|v| v as u16),
        colormap: values
            .value(13)
            .map(|v| if v == 0 { None } else { Some(ResourceId(v)) }),
        cursor: values.value(14).map(ResourceId),
    })
}

#[must_use]
pub fn poly_segment_data(body: &[u8]) -> Option<(u32, &[u8])> {
    let gc_id = read_u32_le(body.get(4..8)?);
    let segments = body.get(8..)?;
    if segments.len() % 8 != 0 {
        return None;
    }
    Some((gc_id, segments))
}

pub fn configure_window_request(body: &[u8]) -> Option<ConfigureWindowRequest> {
    let window = ResourceId(read_u32_le(body.get(0..4)?));
    let value_mask = read_u16_le(body.get(4..6)?);
    let values = value_list(u32::from(value_mask), body.get(8..)?);
    Some(ConfigureWindowRequest {
        window,
        value_mask,
        x: values.value(0).map(|value| value as i16),
        y: values.value(1).map(|value| value as i16),
        width: values.value(2).map(|value| value as u16),
        height: values.value(3).map(|value| value as u16),
        border_width: values.value(4).map(|value| value as u16),
        sibling: values.value(5).map(ResourceId),
        stack_mode: values.value(6).map(|value| value as u8),
    })
}

pub fn create_pixmap_request(depth: u8, body: &[u8]) -> Option<CreatePixmapRequest> {
    Some(CreatePixmapRequest {
        depth,
        pixmap: ResourceId(read_u32_le(body.get(0..4)?)),
        drawable: ResourceId(read_u32_le(body.get(4..8)?)),
        width: read_u16_le(body.get(8..10)?),
        height: read_u16_le(body.get(10..12)?),
    })
}

#[derive(Clone, Debug, PartialEq)]
pub struct ChangePropertyRequest {
    pub mode: u8,
    pub window: ResourceId,
    pub property: AtomId,
    pub r#type: AtomId,
    pub format: u8,
    pub data: Vec<u8>,
    pub length: u32,
}

#[must_use]
pub fn change_property_request(header_data: u8, body: &[u8]) -> Option<ChangePropertyRequest> {
    let window = ResourceId(read_u32_le(body.get(0..4)?));
    let property = AtomId(read_u32_le(body.get(4..8)?));
    let r#type = AtomId(read_u32_le(body.get(8..12)?));
    let format = *body.get(12)?;
    let length = read_u32_le(body.get(16..20)?);
    // Tolerate invalid format here so the handler can emit BadValue.
    // For valid formats, also validate body length against length * unit.
    // For invalid formats, leave `data` as the remaining body bytes; the
    // handler rejects before touching it.
    let unit_opt = match format {
        8 => Some(1usize),
        16 => Some(2),
        32 => Some(4),
        _ => None,
    };
    let data = if let Some(unit) = unit_opt {
        let data_bytes = (length as usize).checked_mul(unit)?;
        body.get(20..20 + data_bytes)?.to_vec()
    } else {
        // Invalid format — capture whatever's there; handler rejects format
        // before reading `data`.
        body.get(20..)?.to_vec()
    };
    Some(ChangePropertyRequest {
        mode: header_data,
        window,
        property,
        r#type,
        format,
        data,
        length,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeletePropertyRequest {
    pub window: ResourceId,
    pub property: AtomId,
}

#[must_use]
pub fn delete_property_request(body: &[u8]) -> Option<DeletePropertyRequest> {
    Some(DeletePropertyRequest {
        window: ResourceId(read_u32_le(body.get(0..4)?)),
        property: AtomId(read_u32_le(body.get(4..8)?)),
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GetPropertyRequest {
    pub delete: bool,
    pub window: ResourceId,
    pub property: AtomId,
    pub r#type: AtomId,
    pub long_offset: u32,
    pub long_length: u32,
}

#[must_use]
pub fn get_property_request(header_data: u8, body: &[u8]) -> Option<GetPropertyRequest> {
    Some(GetPropertyRequest {
        delete: header_data != 0,
        window: ResourceId(read_u32_le(body.get(0..4)?)),
        property: AtomId(read_u32_le(body.get(4..8)?)),
        r#type: AtomId(read_u32_le(body.get(8..12)?)),
        long_offset: read_u32_le(body.get(12..16)?),
        long_length: read_u32_le(body.get(16..20)?),
    })
}

pub fn free_resource_id(body: &[u8]) -> Option<ResourceId> {
    Some(ResourceId(read_u32_le(body.get(0..4)?)))
}

pub fn create_gc_request(body: &[u8]) -> Option<CreateGcRequest> {
    let value_mask = read_u32_le(body.get(8..12)?);
    let values = value_list(value_mask, body.get(12..)?);
    Some(CreateGcRequest {
        gc: ResourceId(read_u32_le(body.get(0..4)?)),
        drawable: ResourceId(read_u32_le(body.get(4..8)?)),
        function: values.value(0).map(|v| v as u8),
        plane_mask: values.value(1),
        foreground: values.value(2),
        background: values.value(3),
        line_width: values.value(4).map(|value| value as u16),
        line_style: values.value(5).map(|v| v as u8),
        cap_style: values.value(6).map(|v| v as u8),
        join_style: values.value(7).map(|v| v as u8),
        fill_style: values.value(8).map(|v| v as u8),
        fill_rule: values.value(9).map(|v| v as u8),
        tile: values.value(10).map(ResourceId),
        stipple: values.value(11).map(ResourceId),
        tile_x_origin: values.value(12).map(|v| v as i16),
        tile_y_origin: values.value(13).map(|v| v as i16),
        font: values.value(14).map(ResourceId),
        subwindow_mode: values.value(15).map(|v| v as u8),
        graphics_exposures: values.value(16).map(|v| v != 0),
        clip_x_origin: values.value(17).map(|v| v as i16),
        clip_y_origin: values.value(18).map(|v| v as i16),
        clip_mask: values
            .value(19)
            .map(|value| (value != 0).then_some(ResourceId(value))),
        dash_offset: values.value(20).map(|v| v as u16),
        dashes: values.value(21).map(|v| v as u8),
        arc_mode: values.value(22).map(|v| v as u8),
    })
}

pub fn change_gc_request(body: &[u8]) -> Option<GcChange> {
    let value_mask = read_u32_le(body.get(4..8)?);
    let values = value_list(value_mask, body.get(8..)?);
    Some(GcChange {
        gc: ResourceId(read_u32_le(body.get(0..4)?)),
        function: values.value(0).map(|v| v as u8),
        plane_mask: values.value(1),
        foreground: values.value(2),
        background: values.value(3),
        line_width: values.value(4).map(|value| value as u16),
        line_style: values.value(5).map(|v| v as u8),
        cap_style: values.value(6).map(|v| v as u8),
        join_style: values.value(7).map(|v| v as u8),
        fill_style: values.value(8).map(|v| v as u8),
        fill_rule: values.value(9).map(|v| v as u8),
        tile: values.value(10).map(ResourceId),
        stipple: values.value(11).map(ResourceId),
        tile_x_origin: values.value(12).map(|v| v as i16),
        tile_y_origin: values.value(13).map(|v| v as i16),
        font: values.value(14).map(ResourceId),
        subwindow_mode: values.value(15).map(|v| v as u8),
        graphics_exposures: values.value(16).map(|v| v != 0),
        clip_x_origin: values.value(17).map(|v| v as i16),
        clip_y_origin: values.value(18).map(|v| v as i16),
        clip_mask: values
            .value(19)
            .map(|value| (value != 0).then_some(ResourceId(value))),
        dash_offset: values.value(20).map(|v| v as u16),
        dashes: values.value(21).map(|v| v as u8),
        arc_mode: values.value(22).map(|v| v as u8),
    })
}

pub fn set_clip_rectangles_request(ordering: u8, body: &[u8]) -> Option<SetClipRectanglesRequest> {
    let rectangles = body.get(8..)?.to_vec();
    if !rectangles.len().is_multiple_of(8) {
        return None;
    }
    Some(SetClipRectanglesRequest {
        gc: ResourceId(read_u32_le(body.get(0..4)?)),
        clip: ClipRectangles {
            ordering,
            x_origin: read_i16_le(body.get(4..6)?),
            y_origin: read_i16_le(body.get(6..8)?),
            rectangles,
        },
    })
}

pub fn drawable_request_id(body: &[u8]) -> Option<ResourceId> {
    Some(ResourceId(read_u32_le(body.get(0..4)?)))
}

pub fn reparent_window_request(body: &[u8]) -> Option<ReparentWindowRequest> {
    Some(ReparentWindowRequest {
        window: ResourceId(read_u32_le(body.get(0..4)?)),
        parent: ResourceId(read_u32_le(body.get(4..8)?)),
        x: read_i16_le(body.get(8..10)?),
        y: read_i16_le(body.get(10..12)?),
    })
}

pub fn clear_area_request(body: &[u8]) -> Option<ClearAreaRequest> {
    Some(ClearAreaRequest {
        window: ResourceId(read_u32_le(body.get(0..4)?)),
        x: read_i16_le(body.get(4..6)?),
        y: read_i16_le(body.get(6..8)?),
        width: read_u16_le(body.get(8..10)?),
        height: read_u16_le(body.get(10..12)?),
    })
}

#[must_use]
pub fn copy_area_request(body: &[u8]) -> Option<CopyAreaRequest> {
    Some(CopyAreaRequest {
        src: ResourceId(read_u32_le(body.get(0..4)?)),
        dst: ResourceId(read_u32_le(body.get(4..8)?)),
        gc: ResourceId(read_u32_le(body.get(8..12)?)),
        src_x: read_i16_le(body.get(12..14)?),
        src_y: read_i16_le(body.get(14..16)?),
        dst_x: read_i16_le(body.get(16..18)?),
        dst_y: read_i16_le(body.get(18..20)?),
        width: read_u16_le(body.get(20..22)?),
        height: read_u16_le(body.get(22..24)?),
    })
}

fn image_format(value: u8) -> ImageFormat {
    match value {
        0 => ImageFormat::XyBitmap,
        1 => ImageFormat::XyPixmap,
        2 => ImageFormat::ZPixmap,
        other => ImageFormat::Unknown(other),
    }
}

#[must_use]
pub fn put_image_request(format: u8, body: &[u8]) -> Option<PutImageRequest<'_>> {
    Some(PutImageRequest {
        format: image_format(format),
        drawable: ResourceId(read_u32_le(body.get(0..4)?)),
        gc: ResourceId(read_u32_le(body.get(4..8)?)),
        width: read_u16_le(body.get(8..10)?),
        height: read_u16_le(body.get(10..12)?),
        dst_x: read_i16_le(body.get(12..14)?),
        dst_y: read_i16_le(body.get(14..16)?),
        left_pad: *body.get(16)?,
        depth: *body.get(17)?,
        data: body.get(20..)?,
    })
}

pub fn send_event_request(propagate: u8, body: &[u8]) -> Option<SendEventRequest<'_>> {
    let event: &[u8; 32] = body.get(8..40)?.try_into().ok()?;
    Some(SendEventRequest {
        propagate: propagate != 0,
        destination: ResourceId(read_u32_le(body.get(0..4)?)),
        event_mask: read_u32_le(body.get(4..8)?),
        event,
    })
}

pub fn open_font_request(body: &[u8]) -> Option<OpenFontRequest> {
    let font = ResourceId(read_u32_le(body.get(0..4)?));
    let name_len = read_u16_le(body.get(4..6)?) as usize;
    let name = body.get(8..8 + name_len)?;
    Some(OpenFontRequest {
        font,
        name: String::from_utf8_lossy(name).into_owned(),
    })
}

#[derive(Clone, Debug)]
pub struct QueryTextExtentsRequest {
    pub fontable: ResourceId,
    pub chars: Vec<(u8, u8)>,
}

pub fn query_text_extents_request(odd_length: u8, body: &[u8]) -> Option<QueryTextExtentsRequest> {
    let fontable = ResourceId(read_u32_le(body.get(0..4)?));
    let string_bytes = body.get(4..)?;
    let pad = if odd_length != 0 { 2 } else { 0 };
    let useful = string_bytes.len().checked_sub(pad)?;
    let mut chars = Vec::with_capacity(useful / 2);
    let mut i = 0;
    while i + 2 <= useful {
        chars.push((string_bytes[i], string_bytes[i + 1]));
        i += 2;
    }
    Some(QueryTextExtentsRequest { fontable, chars })
}

#[derive(Clone, Copy, Debug, Default)]
pub struct TextExtents {
    pub draw_direction: u8,
    pub font_ascent: i16,
    pub font_descent: i16,
    pub overall_ascent: i16,
    pub overall_descent: i16,
    pub overall_width: i32,
    pub overall_left: i32,
    pub overall_right: i32,
}

#[derive(Clone, Debug)]
pub struct ListFontsRequest {
    pub max_names: u16,
    pub pattern: String,
}

pub fn list_fonts_request(body: &[u8]) -> Option<ListFontsRequest> {
    let max_names = read_u16_le(body.get(0..2)?);
    let pattern_len = read_u16_le(body.get(2..4)?) as usize;
    let pattern = body.get(4..4 + pattern_len)?;
    Some(ListFontsRequest {
        max_names,
        pattern: String::from_utf8_lossy(pattern).into_owned(),
    })
}

pub fn create_glyph_cursor_id(body: &[u8]) -> Option<ResourceId> {
    Some(ResourceId(read_u32_le(body.get(0..4)?)))
}

pub fn poly_fill_arc_data(body: &[u8]) -> Option<(u32, &[u8])> {
    arc_request_data(body)
}

pub fn poly_arc_data(body: &[u8]) -> Option<(u32, &[u8])> {
    arc_request_data(body)
}

pub fn poly_fill_rectangle_data(body: &[u8]) -> Option<(u32, &[u8])> {
    let gc_id = read_u32_le(body.get(4..8)?);
    let rectangles = body.get(8..)?;
    if rectangles.len() % 8 != 0 {
        return None;
    }
    Some((gc_id, rectangles))
}

pub fn poly_line_data(body: &[u8]) -> Option<(u32, &[u8])> {
    let gc_id = read_u32_le(body.get(4..8)?);
    let points = body.get(8..)?;
    if !points.len().is_multiple_of(4) {
        return None;
    }
    Some((gc_id, points))
}

pub fn image_text8_data(body: &[u8]) -> Option<(u32, u32, &[u8])> {
    let drawable = read_u32_le(body.get(0..4)?);
    let gc_id = read_u32_le(body.get(4..8)?);
    Some((drawable, gc_id, body))
}

pub fn poly_text_data(body: &[u8]) -> Option<(u32, u32, &[u8])> {
    let drawable = read_u32_le(body.get(0..4)?);
    let gc_id = read_u32_le(body.get(4..8)?);
    Some((drawable, gc_id, body))
}

pub fn map_window_id(body: &[u8]) -> Option<ResourceId> {
    Some(ResourceId(read_u32_le(body.get(0..4)?)))
}

pub fn input_focus_window(body: &[u8]) -> Option<ResourceId> {
    Some(ResourceId(read_u32_le(body.get(0..4)?)))
}

pub fn write_key_event(
    writer: &mut impl Write,
    byte_order: ClientByteOrder,
    event: KeyEvent,
) -> io::Result<()> {
    let mut out = Vec::with_capacity(32);
    encode_key_event(&mut out, byte_order, event);
    writer.write_all(&out)
}

/// Encode a `KeyPress` (`event.pressed = true`) or `KeyRelease` event
/// against `order`. Mirrors [`write_key_event`] but produces a buffer
/// instead of writing — used by the state-borrowing fanout helpers.
pub fn encode_key_event(out: &mut Vec<u8>, order: ClientByteOrder, event: KeyEvent) {
    out.push(if event.pressed { 2 } else { 3 });
    out.push(event.keycode);
    write_u16(order, out, event.sequence.0);
    write_u32(order, out, event.time);
    write_u32(order, out, event.root.0);
    write_u32(order, out, event.event.0);
    write_u32(order, out, 0); // child
    write_i16(order, out, event.root_x);
    write_i16(order, out, event.root_y);
    write_i16(order, out, event.event_x);
    write_i16(order, out, event.event_y);
    write_u16(order, out, event.state);
    out.push(1); // same-screen
    out.push(0);
}

pub fn encode_focus_event(
    out: &mut Vec<u8>,
    sequence: SequenceNumber,
    order: ClientByteOrder,
    focus_in: bool,
    window: ResourceId,
) {
    encode_focus_event_with_mode_detail(out, sequence, order, focus_in, window, 0, 0);
}

pub fn encode_focus_event_with_mode_detail(
    out: &mut Vec<u8>,
    sequence: SequenceNumber,
    order: ClientByteOrder,
    focus_in: bool,
    window: ResourceId,
    mode: u8,
    detail: u8,
) {
    out.push(if focus_in { 9 } else { 10 });
    out.push(detail);
    write_u16(order, out, sequence.0);
    write_u32(order, out, window.0);
    out.push(mode);
    out.extend_from_slice(&[0; 23]);
}

pub fn encode_expose_event(
    out: &mut Vec<u8>,
    sequence: SequenceNumber,
    order: ClientByteOrder,
    window: ResourceId,
    x: u16,
    y: u16,
    width: u16,
    height: u16,
    count: u16,
) {
    out.push(12); // Expose
    out.push(0);
    write_u16(order, out, sequence.0);
    write_u32(order, out, window.0);
    write_u16(order, out, x);
    write_u16(order, out, y);
    write_u16(order, out, width);
    write_u16(order, out, height);
    write_u16(order, out, count);
    out.extend_from_slice(&[0; 14]);
}

/// VisibilityNotify (event type 15). `state` is 0=Unobscured,
/// 1=PartiallyObscured, 2=FullyObscured (X.h `VisibilityUnobscured`…).
/// GTK3 suppresses frame-clock paints for a window it believes is
/// `FullyObscured`; clients that select `VisibilityChangeMask` need
/// `Unobscured` to keep repainting past the first expose-driven frame.
pub fn encode_visibility_notify_event(
    out: &mut Vec<u8>,
    sequence: SequenceNumber,
    order: ClientByteOrder,
    window: ResourceId,
    state: u8,
) {
    out.push(15); // VisibilityNotify
    out.push(0);
    write_u16(order, out, sequence.0);
    write_u32(order, out, window.0);
    out.push(state);
    out.extend_from_slice(&[0; 23]);
}

/// MIT-SHM `ShmCompletion` event (`xShmCompletionEvent`, shmproto.h).
/// The server must send this after a `ShmPutImage` issued with
/// `send_event=true`, so the client knows the shared-memory segment
/// is free to reuse. GTK/GDK keeps a pool of SHM segments and blocks
/// the next frame until the completion arrives — without it the
/// render loop stalls once the pool is exhausted. `event_code` is the
/// extension's first-event base + `ShmCompletion`(0); `minor_event`
/// is `X_ShmPutImage`(3); `major_opcode` is the MIT-SHM major.
#[allow(clippy::too_many_arguments)]
pub fn encode_shm_completion_event(
    out: &mut Vec<u8>,
    order: ClientByteOrder,
    sequence: SequenceNumber,
    event_code: u8,
    drawable: ResourceId,
    minor_event: u16,
    major_opcode: u8,
    shmseg: u32,
    offset: u32,
) {
    out.push(event_code);
    out.push(0); // bpad0
    write_u16(order, out, sequence.0);
    write_u32(order, out, drawable.0);
    write_u16(order, out, minor_event);
    out.push(major_opcode);
    out.push(0); // bpad1
    write_u32(order, out, shmseg);
    write_u32(order, out, offset);
    out.extend_from_slice(&[0; 12]); // pad0/pad1/pad2
}

/// GraphicsExpose (event type 13). Sent in response to CopyArea/CopyPlane
/// when graphics-exposures is True for source regions that aren't
/// guaranteed visible. We always emit a single event covering the full
/// destination rectangle since we don't track source obscurity.
#[allow(clippy::too_many_arguments)]
pub fn encode_graphics_expose_event(
    out: &mut Vec<u8>,
    sequence: SequenceNumber,
    order: ClientByteOrder,
    drawable: ResourceId,
    x: u16,
    y: u16,
    width: u16,
    height: u16,
    minor_opcode: u16,
    count: u16,
    major_opcode: u8,
) {
    out.push(13); // GraphicsExpose
    out.push(0);
    write_u16(order, out, sequence.0);
    write_u32(order, out, drawable.0);
    write_u16(order, out, x);
    write_u16(order, out, y);
    write_u16(order, out, width);
    write_u16(order, out, height);
    write_u16(order, out, minor_opcode);
    write_u16(order, out, count);
    out.push(major_opcode);
    out.extend_from_slice(&[0; 11]);
}

pub fn encode_no_exposure_event(
    out: &mut Vec<u8>,
    sequence: SequenceNumber,
    order: ClientByteOrder,
    drawable: ResourceId,
    minor_opcode: u16,
    major_opcode: u8,
) {
    out.push(14); // NoExposure
    out.push(0);
    write_u16(order, out, sequence.0);
    write_u32(order, out, drawable.0);
    write_u16(order, out, minor_opcode);
    out.push(major_opcode);
    out.extend_from_slice(&[0; 21]);
}

pub fn encode_map_notify_event(
    out: &mut Vec<u8>,
    sequence: SequenceNumber,
    order: ClientByteOrder,
    event_window: ResourceId,
    window: ResourceId,
    override_redirect: bool,
) {
    out.push(19); // MapNotify
    out.push(0);
    write_u16(order, out, sequence.0);
    write_u32(order, out, event_window.0);
    write_u32(order, out, window.0);
    out.push(u8::from(override_redirect));
    out.extend_from_slice(&[0; 19]);
}

/// Encode a ColormapNotify event (type 32, 32 bytes).
///
/// Generated when a colormap is installed or uninstalled, or when a
/// window's `CWColormap` attribute changes. `colormap = ResourceId(0)`
/// indicates the attribute changed to `None`.
///
/// `new = true` means the event is reporting that the window's
/// `colormap` attribute was changed (e.g. via `ChangeWindowAttributes`);
/// `false` reports an install / uninstall on the existing attribute.
pub fn encode_colormap_notify_event(
    out: &mut Vec<u8>,
    sequence: SequenceNumber,
    order: ClientByteOrder,
    window: ResourceId,
    colormap: ResourceId,
    new: bool,
    installed: bool,
) {
    out.push(32); // ColormapNotify
    out.push(0);
    write_u16(order, out, sequence.0);
    write_u32(order, out, window.0);
    write_u32(order, out, colormap.0);
    out.push(u8::from(new));
    out.push(u8::from(installed)); // state: 1 = Installed, 0 = Uninstalled
    out.extend_from_slice(&[0; 18]);
}

pub fn encode_create_notify_event(
    out: &mut Vec<u8>,
    sequence: SequenceNumber,
    order: ClientByteOrder,
    parent: ResourceId,
    window: ResourceId,
    geometry: Geometry,
    override_redirect: bool,
) {
    out.push(16); // CreateNotify
    out.push(0);
    write_u16(order, out, sequence.0);
    write_u32(order, out, parent.0);
    write_u32(order, out, window.0);
    write_i16(order, out, geometry.x);
    write_i16(order, out, geometry.y);
    write_u16(order, out, geometry.width);
    write_u16(order, out, geometry.height);
    write_u16(order, out, geometry.border_width);
    out.push(u8::from(override_redirect));
    out.extend_from_slice(&[0; 9]);
}

pub fn encode_configure_notify_event(
    out: &mut Vec<u8>,
    sequence: SequenceNumber,
    order: ClientByteOrder,
    event_window: ResourceId,
    window: ResourceId,
    above_sibling: Option<ResourceId>,
    geometry: Geometry,
    override_redirect: bool,
) {
    out.push(22); // ConfigureNotify
    out.push(0);
    write_u16(order, out, sequence.0);
    write_u32(order, out, event_window.0);
    write_u32(order, out, window.0);
    write_u32(order, out, above_sibling.unwrap_or(ResourceId(0)).0);
    write_i16(order, out, geometry.x);
    write_i16(order, out, geometry.y);
    write_u16(order, out, geometry.width);
    write_u16(order, out, geometry.height);
    write_u16(order, out, geometry.border_width);
    out.push(u8::from(override_redirect));
    out.extend_from_slice(&[0; 5]);
}

pub fn encode_map_request_event(
    out: &mut Vec<u8>,
    sequence: SequenceNumber,
    order: ClientByteOrder,
    parent: ResourceId,
    window: ResourceId,
) {
    out.push(20); // MapRequest
    out.push(0);
    write_u16(order, out, sequence.0);
    write_u32(order, out, parent.0);
    write_u32(order, out, window.0);
    out.extend_from_slice(&[0; 20]);
}

pub fn encode_configure_request_event(
    out: &mut Vec<u8>,
    sequence: SequenceNumber,
    order: ClientByteOrder,
    parent: ResourceId,
    window: ResourceId,
    request: &ConfigureWindowRequest,
) {
    out.push(23); // ConfigureRequest
    out.push(request.stack_mode.unwrap_or(0));
    write_u16(order, out, sequence.0);
    write_u32(order, out, parent.0);
    write_u32(order, out, window.0);
    write_u32(order, out, request.sibling.unwrap_or(ResourceId(0)).0);
    write_i16(order, out, request.x.unwrap_or(0));
    write_i16(order, out, request.y.unwrap_or(0));
    write_u16(order, out, request.width.unwrap_or(0));
    write_u16(order, out, request.height.unwrap_or(0));
    write_u16(order, out, request.border_width.unwrap_or(0));
    write_u16(order, out, request.value_mask);
    out.extend_from_slice(&[0; 4]);
}

fn arc_request_data(body: &[u8]) -> Option<(u32, &[u8])> {
    let gc_id = read_u32_le(body.get(4..8)?);
    let arcs = body.get(8..)?;
    if arcs.len() % 12 != 0 {
        return None;
    }
    Some((gc_id, arcs))
}

pub fn query_colors_pixels(body: &[u8]) -> Vec<u32> {
    if body.len() <= 4 {
        return Vec::new();
    }

    body[4..]
        .chunks_exact(4)
        .map(|chunk| u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect()
}

pub fn alloc_color_request(body: &[u8]) -> Option<Rgb16> {
    Some(Rgb16 {
        red: read_u16_le(body.get(4..6)?),
        green: read_u16_le(body.get(6..8)?),
        blue: read_u16_le(body.get(8..10)?),
    })
}

pub fn alloc_named_color_name(body: &[u8]) -> String {
    let Some(name_len) = body.get(4..6).map(read_u16_le) else {
        return String::new();
    };
    let name = body.get(8..8 + name_len as usize).unwrap_or_default();
    String::from_utf8_lossy(name).into_owned()
}

#[derive(Clone, Copy, Debug)]
struct ValueList<'a> {
    value_mask: u32,
    values: &'a [u8],
}

impl ValueList<'_> {
    fn value(self, target_bit: u8) -> Option<u32> {
        let mut offset = 0;
        for bit in 0..32 {
            if self.value_mask & (1 << bit) == 0 {
                continue;
            }

            let value = read_u32_le(self.values.get(offset..offset + 4)?);
            if bit == target_bit {
                return Some(value);
            }
            offset += 4;
        }
        None
    }
}

fn value_list(value_mask: u32, values: &[u8]) -> ValueList<'_> {
    ValueList { value_mask, values }
}

pub fn query_extension_name(body: &[u8]) -> String {
    if body.len() < 4 {
        return String::new();
    }
    let len = u16::from_le_bytes([body[0], body[1]]) as usize;
    let name = body.get(4..4 + len).unwrap_or_default();
    String::from_utf8_lossy(name).into_owned()
}

pub fn write_get_window_attributes_reply(
    writer: &mut impl Write,
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    attributes: WindowAttributes,
) -> io::Result<()> {
    // GetWindowAttributes reply: the `data` byte after the response code
    // is `backing_store` per X protocol §10.
    let mut reply = fixed_reply(byte_order, sequence, attributes.backing_store, 3);
    write_u32(byte_order, &mut reply, attributes.visual.0);
    write_u16(byte_order, &mut reply, attributes.class);
    reply.push(attributes.bit_gravity);
    reply.push(attributes.win_gravity);
    write_u32(byte_order, &mut reply, attributes.backing_planes);
    write_u32(byte_order, &mut reply, attributes.backing_pixel);
    reply.push(u8::from(attributes.save_under));
    reply.push(u8::from(attributes.map_is_installed));
    reply.push(attributes.map_state);
    reply.push(u8::from(attributes.override_redirect));
    write_u32(byte_order, &mut reply, attributes.colormap.0);
    write_u32(byte_order, &mut reply, attributes.all_event_masks);
    write_u32(byte_order, &mut reply, attributes.your_event_mask);
    write_u16(byte_order, &mut reply, attributes.do_not_propagate_mask);
    write_u16(byte_order, &mut reply, 0);
    writer.write_all(&reply)
}

pub fn write_get_geometry_reply(
    writer: &mut impl Write,
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    geometry: Geometry,
) -> io::Result<()> {
    let mut reply = fixed_reply(byte_order, sequence, geometry.depth, 0);
    write_u32(byte_order, &mut reply, geometry.root.0);
    write_i16(byte_order, &mut reply, geometry.x);
    write_i16(byte_order, &mut reply, geometry.y);
    write_u16(byte_order, &mut reply, geometry.width);
    write_u16(byte_order, &mut reply, geometry.height);
    write_u16(byte_order, &mut reply, geometry.border_width);
    reply.extend_from_slice(&[0; 10]);
    writer.write_all(&reply)
}

pub fn write_query_tree_reply(
    writer: &mut impl Write,
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    root: ResourceId,
    parent: ResourceId,
    children: &[ResourceId],
) -> io::Result<()> {
    let mut reply = fixed_reply(byte_order, sequence, 0, children.len() as u32);
    write_u32(byte_order, &mut reply, root.0);
    write_u32(byte_order, &mut reply, parent.0);
    write_u16(byte_order, &mut reply, children.len() as u16);
    reply.extend_from_slice(&[0; 14]);
    for child in children {
        write_u32(byte_order, &mut reply, child.0);
    }
    writer.write_all(&reply)
}

pub fn write_intern_atom_reply(
    writer: &mut impl Write,
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    atom: AtomId,
) -> io::Result<()> {
    let mut reply = fixed_reply(byte_order, sequence, 0, 0);
    write_u32(byte_order, &mut reply, atom.0);
    reply.extend_from_slice(&[0; 20]);
    writer.write_all(&reply)
}

pub fn write_get_atom_name_reply(
    writer: &mut impl Write,
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    name: &str,
) -> io::Result<()> {
    let mut extra = name.as_bytes().to_vec();
    pad_vec4(&mut extra);
    let mut reply = fixed_reply(byte_order, sequence, 0, checked_units(extra.len())? as u32);
    write_u16(byte_order, &mut reply, name.len() as u16);
    reply.extend_from_slice(&[0; 22]);
    reply.extend_from_slice(&extra);
    writer.write_all(&reply)
}

#[derive(Clone, Copy, Debug)]
pub struct GetPropertyReply<'a> {
    pub format: u8,
    pub r#type: AtomId,
    pub bytes_after: u32,
    pub value_len: u32,  // in format units
    pub value: &'a [u8], // padded to 4 bytes here
}

pub fn write_get_property_reply(
    writer: &mut impl Write,
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    reply: GetPropertyReply<'_>,
) -> io::Result<()> {
    let mut padded = reply.value.to_vec();
    pad_vec4(&mut padded);
    // GetProperty's reply `length` field is 32-bit (in 4-byte units). The
    // pre-BIG-REQUESTS u16 limit on *requests* doesn't apply here. Don't
    // route through `checked_units` (u16) — capped values like 64 KiB
    // truncate icons, _NET_WM_ICON, fontset data, etc. Marco's
    // _NET_WM_ICON GetProperty (~343 KB) used to fail here and leave the
    // WM hung in _XReply.
    let length_units = u32::try_from(padded.len() / 4)
        .map_err(|_| io::Error::new(ErrorKind::InvalidData, "GetProperty reply too large"))?;
    if !padded.len().is_multiple_of(4) {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "GetProperty reply not 4-byte aligned",
        ));
    }
    let mut out = fixed_reply(byte_order, sequence, reply.format, length_units);
    write_u32(byte_order, &mut out, reply.r#type.0);
    write_u32(byte_order, &mut out, reply.bytes_after);
    write_u32(byte_order, &mut out, reply.value_len);
    out.extend_from_slice(&[0; 12]);
    out.extend_from_slice(&padded);
    writer.write_all(&out)
}

pub fn write_list_properties_reply(
    writer: &mut impl Write,
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    atoms: &[AtomId],
) -> io::Result<()> {
    let n = u16::try_from(atoms.len()).unwrap_or(u16::MAX);
    // length = number of extra 4-byte units beyond the fixed 32-byte header
    let mut reply = wire::fixed_reply(byte_order, sequence, 0, u32::from(n));
    wire::write_u16(byte_order, &mut reply, n);
    reply.resize(32, 0);
    writer.write_all(&reply)?;
    for atom in &atoms[..usize::from(n)] {
        writer.write_all(&atom.0.to_le_bytes())?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
/// XI2 device-event flags. Bit 16 (`XIPointerEmulated`) marks a button
/// or key event that's a legacy emulation of an already-delivered
/// device-class event (smooth-scroll axis change for buttons 4..7,
/// keyboard touch emulation, etc.). Without this flag set on the
/// scroll-emulated XI_ButtonPress/Release(4..7), XI2-aware clients
/// double-handle the wheel input — process the smooth-scroll
/// XI_Motion AND the button "click," with the click landing on the
/// app's real button-5/6/7 handler (back/forward nav, custom JS).
/// Release Chrome stack-smashed on rapid scroll into yserver because
/// of this. Xorg sets the flag on every emulated button event.
pub const XI_POINTER_EMULATED: u32 = 0x0001_0000;

#[allow(clippy::too_many_arguments)]
pub fn encode_xi2_device_event(
    out: &mut Vec<u8>,
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    major_opcode: u8,
    evtype: u16,
    deviceid: u16,
    time: u32,
    root: ResourceId,
    event: ResourceId,
    child: ResourceId,
    root_x: i16,
    root_y: i16,
    event_x: i16,
    event_y: i16,
    state: u16,
    detail: u32,
    sourceid: u16,
    flags: u32,
) {
    // Per the X.Org reference trace (thunar inside MATE, sampled
    // 2026-05-15), XI2 Button events carry NO axis values: valuator
    // mask is all-zero and no axisvalues follow. Only Motion events
    // carry the X/Y axes. Per X11 spec, valuator events should reflect
    // only the axes that actually changed since the last event; for
    // Press/Release the cursor hasn't moved (libinput delivers
    // motion+button as separate events), so the mask is empty.
    //
    // yserver previously sent X/Y axis values on every Press/Release
    // to fix a caja rubber-band-from-(0,0) bug; the real cause was
    // GDK reading axis values from preceding Motion events for the
    // gesture anchor, and the Press-side axis values just happened
    // to also work. Matching Xorg here lets thunar's tree-view
    // expanders register subsequent clicks (GDK was treating the
    // axis-bearing Press events differently from Xorg-style
    // axis-free Presses).
    let include_axes = evtype == 6; // XI_Motion only
    // Match Xorg's wire-format widths byte-for-byte: 8 u32s of buttons
    // mask (covers 256 buttons; we only use bits 1..7 but pad to match
    // Xorg's modesetting/evdev declaration) and 2 u32s of valuators
    // mask (covers 64 valuators; we use bits 0..3 in the first u32).
    //
    // Release Chrome (Ozone X11 path) crashes on yserver scroll when
    // event widths differ from Xorg's — our 1 u32 of buttons mask + 0
    // u32 of valuators mask (on button events) under-fills Chrome's
    // pre-sized struct from XIQueryDevice and walks into adjacent
    // memory. Other XCB/Xlib-based clients track width per-event and
    // are unaffected, so the bug was Chrome-specific. See
    // 2026-05-29 chrome-scroll bug investigation (mate-xorg vs
    // mate xtrace diff identified the width mismatch).
    const BUTTONS_LEN_U16: u16 = 8;
    const VALUATORS_LEN_U16: u16 = 2;
    let valuator_mask: u32 = if include_axes { 0x0000_0003 } else { 0 };
    // Tail layout: 4*FP1616(coords) + 12 (lens/sourceid/pad/flags) +
    //   16 (mods) + 4 (group) + 4*BUTTONS_LEN_U16 (buttons mask) +
    //   4*VALUATORS_LEN_U16 (valuator mask) +
    //   {2*FP3232 axisvalues if include_axes, else 0}.
    let extra_bytes: u32 =
        16 + 12 + 16 + 4 + 4 * u32::from(BUTTONS_LEN_U16) + 4 * u32::from(VALUATORS_LEN_U16);
    let axes_bytes: u32 = if include_axes { 16 } else { 0 };
    let length_units = (extra_bytes + axes_bytes) / 4;

    let start = out.len();
    out.push(35); // GenericEvent
    out.push(major_opcode);
    write_u16(byte_order, out, sequence.0);
    write_u32(byte_order, out, length_units);

    write_u16(byte_order, out, evtype);
    write_u16(byte_order, out, deviceid);
    write_u32(byte_order, out, time);
    write_u32(byte_order, out, detail);
    write_u32(byte_order, out, root.0);
    write_u32(byte_order, out, event.0);
    write_u32(byte_order, out, child.0);

    // Coordinates are FP16.16
    write_u32(byte_order, out, (i32::from(root_x) << 16) as u32);
    write_u32(byte_order, out, (i32::from(root_y) << 16) as u32);
    write_u32(byte_order, out, (i32::from(event_x) << 16) as u32);
    write_u32(byte_order, out, (i32::from(event_y) << 16) as u32);

    write_u16(byte_order, out, BUTTONS_LEN_U16);
    write_u16(byte_order, out, VALUATORS_LEN_U16);
    write_u16(byte_order, out, sourceid);
    write_u16(byte_order, out, 0); // pad
    write_u32(byte_order, out, flags);

    // mods: base, latched, locked, effective. Per XI2 / XKB spec these
    // are KEYBOARD modifier bits only (Shift/Lock/Control/Mod1..Mod5 in
    // bits 0..=7). The X11 KeyButMask `state` value passed in carries
    // pointer-button bits in 8..=12 alongside modifier bits in 0..=7;
    // mask down to the modifier byte so GDK doesn't see button bits
    // leaking into mods.effective (which it ORs with the separate
    // `buttons` mask to reconstruct GdkEvent.state — double-counting
    // is harmless but writing button bits into modifier fields is
    // spec-incorrect).
    //
    // base / latched / locked / effective: Xorg (dix/eventconvert.c:720-723)
    // fills `base_mods` from the modifiers of keys logically down, not only
    // `effective`. yserver tracks only the cooked `state` (= effective), so
    // mirror it into `base` too (latched/locked unmodelled → 0). With base
    // left at 0, a WM whose keybinding matcher reads `mods.base`
    // (mutter/muffin) sees a grabbed Ctrl-Alt-<key> carrying NO modifiers
    // and never fires the binding — dead Ctrl-Alt-arrow workspace switching
    // while plain typing (modifier-independent) still works.
    let modifier_bits = u32::from(state & 0x00FF);
    write_u32(byte_order, out, modifier_bits); // base
    write_u32(byte_order, out, 0); // latched
    write_u32(byte_order, out, 0); // locked
    write_u32(byte_order, out, modifier_bits); // effective

    out.extend_from_slice(&[0; 4]); // group base/latched/locked/effective

    // X11 XInput2 `buttons` mask: bit N corresponds to button N (1-indexed).
    // Bit 0 is reserved per spec and is always 0. Verified against real
    // Xorg trace of thunar in MATE: ButtonPress shows buttons=0x00
    // (pre-event, no buttons held), ButtonRelease shows buttons=0x02
    // (pre-event, button 1 still held — bit 1, not bit 0).
    //
    // The mask reports PRE-event button state — buttons held coming
    // INTO this event. Same semantics for Press/Release/Motion/crossings.
    // `state`'s KeyButMask carries the pre-event button state in
    // bits 8..=12 (Button1..5); shift down by 7 to align state-bit-8
    // (button 1) → mask-bit-1, and mask off the reserved bit-0.
    let pre_buttons: u32 = u32::from((state >> 7) & 0x3e);
    write_u32(byte_order, out, pre_buttons);
    // Buttons mask padding to BUTTONS_LEN_U16 u32s — Xorg pads to 8
    // even when only bits 1..7 are meaningful. See header comment.
    for _ in 1..BUTTONS_LEN_U16 {
        write_u32(byte_order, out, 0);
    }

    // Valuator mask of VALUATORS_LEN_U16 u32s. Bit 0 (X), 1 (Y), 2/3
    // (scroll axes) live in the first u32; second u32 is always 0.
    // Motion events set X/Y bits; button/crossing events set no bits
    // (mask = 0).
    write_u32(byte_order, out, valuator_mask);
    for _ in 1..VALUATORS_LEN_U16 {
        write_u32(byte_order, out, 0);
    }
    if include_axes {
        // Axis values: FP3232 (signed i32 integer + u32 fraction).
        // Master pointer is in absolute mode; X/Y carry the
        // root-relative position. Fraction is 0 because libinput
        // reports integer coords post-clamp.
        write_u32(byte_order, out, i32::from(root_x) as u32);
        write_u32(byte_order, out, 0); // X fraction
        write_u32(byte_order, out, i32::from(root_y) as u32);
        write_u32(byte_order, out, 0); // Y fraction
    }

    debug_assert_eq!(out.len() - start, 32 + (length_units as usize) * 4);
}

/// Encode an XInput2 `XI_Motion` event carrying a scroll-axis update
/// for one of the master-pointer's scroll valuators (axis 2 =
/// vertical, axis 3 = horizontal). Tail layout matches
/// `encode_xi2_device_event` except the valuator mask covers bits
/// 0/1/`scroll_axis` and the axisvalue list carries three FP3232
/// entries: X, Y, then the scroll axis's cumulative value. Length
/// goes up by one FP3232 (8 bytes / 2 units) over the regular
/// device event → 20 units. GDK's XI2 backend reads the cumulative
/// value off this event and computes a scroll delta from the
/// previous sample.
#[allow(clippy::too_many_arguments)]
pub fn encode_xi2_motion_with_scroll(
    out: &mut Vec<u8>,
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    major_opcode: u8,
    deviceid: u16,
    time: u32,
    root: ResourceId,
    event: ResourceId,
    root_x: i16,
    root_y: i16,
    event_x: i16,
    event_y: i16,
    state: u16,
    sourceid: u16,
    scroll_axis: u8,
    scroll_value: i32,
) {
    let start = out.len();
    out.push(35); // GenericEvent
    out.push(major_opcode);
    write_u16(byte_order, out, sequence.0);
    // Tail = base 72 (see encode_xi2_device_event) + 8 (extra
    // FP3232 axis value) = 80 bytes = 20 units.
    // Match `encode_xi2_device_event`'s padded layout: 8 u32 buttons,
    // 2 u32 valuators. Tail = 16(coords) + 12(lens) + 16(mods) +
    // 4(group) + 32(8 u32 buttons mask) + 8(2 u32 valuator mask) +
    // 24(3 FP3232 axisvalues: X, Y, scroll) = 112 bytes = 28 units.
    write_u32(byte_order, out, 28);

    write_u16(byte_order, out, 6); // evtype = XI_Motion
    write_u16(byte_order, out, deviceid);
    write_u32(byte_order, out, time);
    write_u32(byte_order, out, 0); // detail = 0 for motion
    write_u32(byte_order, out, root.0);
    write_u32(byte_order, out, event.0);
    write_u32(byte_order, out, 0); // child

    write_u32(byte_order, out, (i32::from(root_x) << 16) as u32);
    write_u32(byte_order, out, (i32::from(root_y) << 16) as u32);
    write_u32(byte_order, out, (i32::from(event_x) << 16) as u32);
    write_u32(byte_order, out, (i32::from(event_y) << 16) as u32);

    write_u16(byte_order, out, 8); // buttons_len — match Xorg's wire width
    write_u16(byte_order, out, 2); // valuators_len — match Xorg's wire width
    write_u16(byte_order, out, sourceid);
    write_u16(byte_order, out, 0); // pad
    write_u32(byte_order, out, 0); // flags

    // mods: base, latched, locked, effective — KEYBOARD modifier bits
    // only (0x00FF mask); button bits in state[8..=12] go into the
    // separate `buttons` mask below, not mods. `base` mirrors `effective`
    // (Xorg fills base from held-key modifiers; see encode_xi2_device_event).
    let modifier_bits = u32::from(state & 0x00FF);
    write_u32(byte_order, out, modifier_bits); // base
    write_u32(byte_order, out, 0); // latched
    write_u32(byte_order, out, 0); // locked
    write_u32(byte_order, out, modifier_bits); // effective

    out.extend_from_slice(&[0; 4]); // group

    // Button mask (8 u32s of pre-event button state; only first carries
    // meaningful bits — pad the rest with 0).
    let pre_buttons: u32 = u32::from((state >> 7) & 0x3e);
    write_u32(byte_order, out, pre_buttons);
    for _ in 1..8 {
        write_u32(byte_order, out, 0);
    }

    // Valuator mask (2 u32s). First u32 holds bits 0(X), 1(Y), and the
    // scroll axis bit (2=vert or 3=horiz); second u32 is 0.
    let mask: u32 = 0x3 | (1u32 << u32::from(scroll_axis));
    write_u32(byte_order, out, mask);
    write_u32(byte_order, out, 0);

    // Axis values (FP3232: signed i32 integer + u32 fraction).
    // Order matches mask LSB-first: axis 0 (X), 1 (Y), then scroll.
    write_u32(byte_order, out, i32::from(root_x) as u32);
    write_u32(byte_order, out, 0); // X fraction
    write_u32(byte_order, out, i32::from(root_y) as u32);
    write_u32(byte_order, out, 0); // Y fraction
    write_u32(byte_order, out, scroll_value as u32);
    write_u32(byte_order, out, 0); // scroll fraction

    debug_assert_eq!(out.len() - start, 144);
}

/// Encode an XInput2 raw device event (XI_RawKeyPress / XI_RawKeyRelease /
/// XI_RawButtonPress / XI_RawButtonRelease / XI_RawMotion / XI_RawTouch*).
/// Includes X and Y valuators with the supplied root-coordinate values
/// stored as FP3232 (signed 32-bit integer + unsigned 32-bit fraction).
/// xeyes selects XI_RawMotion as a "cursor moved" notification and then
/// calls XIQueryPointer for the actual position; we only need to supply
/// enough payload to wake the client.
#[allow(clippy::too_many_arguments)]
pub fn encode_xi2_raw_event(
    out: &mut Vec<u8>,
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    major_opcode: u8,
    evtype: u16,
    deviceid: u16,
    time: u32,
    detail: u32,
    sourceid: u16,
    raw_x: i32,
    raw_y: i32,
) {
    out.push(35); // GenericEvent
    out.push(major_opcode);
    write_u16(byte_order, out, sequence.0);
    // length in 4-byte units beyond the 32-byte header.
    // Tail = valuator_mask(4) + axisvalues(2 * 8) + axisvalues_raw(2 * 8) = 36 bytes = 9 units.
    write_u32(byte_order, out, 9);

    write_u16(byte_order, out, evtype);
    write_u16(byte_order, out, deviceid);
    write_u32(byte_order, out, time);
    write_u32(byte_order, out, detail);
    write_u16(byte_order, out, sourceid);
    write_u16(byte_order, out, 2); // valuators_len: X and Y
    write_u32(byte_order, out, 0); // flags
    out.extend_from_slice(&[0; 4]); // pad to 32-byte fixed area

    debug_assert_eq!(out.len(), 32);

    // Variable tail: valuator_mask + axisvalues + axisvalues_raw.
    write_u32(byte_order, out, 0x3); // valuator_mask: bits 0+1 (X, Y)

    // FP3232 values: integer part (i32) + fractional part (u32 = 0).
    write_u32(byte_order, out, raw_x as u32);
    write_u32(byte_order, out, 0); // X fractional
    write_u32(byte_order, out, raw_y as u32);
    write_u32(byte_order, out, 0); // Y fractional

    // axisvalues_raw — same as axisvalues for our purposes.
    write_u32(byte_order, out, raw_x as u32);
    write_u32(byte_order, out, 0);
    write_u32(byte_order, out, raw_y as u32);
    write_u32(byte_order, out, 0);

    debug_assert_eq!(out.len(), 68);
}

/// XInput 1.x `ListInputDevices` (opcode minor 2) reply.
///
/// Real clients call `XListInputDevices` (XI 1.x) AND `XIQueryDevice`
/// (XI2) and cross-check the two. Chromium/Electron's Ozone-X11
/// device-list cache fatal-`CHECK`s (SIGTRAP) if XI2 reports a master
/// pointer while XI1 reports zero devices — which an empty stub reply
/// does. This mirrors the 4-device model emitted by the XIQueryDevice
/// handler so the two agree: core pointer (2), core keyboard (3),
/// slave pointer (4), slave keyboard (5).
///
/// Wire layout per the XInput 1.x protocol (NOTE: class `length`
/// fields here are in BYTES, unlike the 4-byte-unit lengths in XI2):
/// ```text
///   reply header (32B): type=1, ndevices, sequence, length(4B units of data)
///   data:
///     [xDeviceInfo; ndevices]      8B each: type ATOM(4), id, num_classes, use, pad
///     [class infos, per device, concatenated]
///         xButtonInfo   (4B):  class=1, length=4, num_buttons:u16
///         xKeyInfo      (8B):  class=0, length=8, min_kc:u8, max_kc:u8, num_keys:u16, pad:u16
///         xValuatorInfo (8+12n): class=2, length, num_axes:u8, mode:u8,
///                                motion_buffer_size:u32, [resolution:u32, min:i32, max:i32]×n
///     [name STR list, per device]  1 length byte + name bytes
///     pad to 4-byte boundary
/// ```
/// Constants from `XI.h`: `use` codes IsXPointer=0, IsXKeyboard=1,
/// IsXExtensionKeyboard=3, IsXExtensionPointer=4; class ids KeyClass=0,
/// ButtonClass=1, ValuatorClass=2; valuator mode Relative=0, Absolute=1.
/// `names` supplies the device names for ids `[2, 3, 4, 5]` (master
/// pointer, master keyboard, slave pointer, slave keyboard) in that
/// order. They MUST come from the same source the XI2 `XIQueryDevice`
/// handler reads (the `ServerState::xi_devices` registry) so the two
/// enumerations report identical names — clients cross-check them and
/// fatal-`CHECK` on a mismatch.
///
/// `types` supplies the XI 1.x device-type `Atom` value for each of the
/// same four devices in the same order (`[2, 3, 4, 5]`).  Pass the atom
/// ids interned at server startup: `MOUSE` for pointer devices,
/// `KEYBOARD` for keyboard devices, and `TOUCHPAD` for the slave pointer
/// when a touchpad is active.  The atom ids are looked up from
/// `ServerState::atoms` by name at the call site.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn encode_list_input_devices_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    names: [&str; 4],
    types: [u32; 4],
) -> Vec<u8> {
    // Pointer button/axis shape mirrors the XIQueryDevice handler:
    // 7 buttons (left/middle/right + 4 wheel directions), 4 relative
    // valuators (X, Y, vert-scroll, horiz-scroll). min/max −1/−1 (and
    // −1/0 for scroll) and mode=Relative match what Xorg's modesetting
    // core pointer reports for relative axes.
    const NUM_BUTTONS: u16 = 7;
    const MODE_RELATIVE: u8 = 0;
    // Relative pointer axes carry no fixed range; Xorg reports −1/−1.
    const POINTER_AXES: [(i32, i32); 4] = [(-1, -1), (-1, -1), (-1, 0), (-1, 0)];
    // Core keyboard: keycodes 8..=255 (matches the setup reply's
    // min/max-keycode and the XIQueryDevice key class).
    const KEY_MIN: u8 = 8;
    const KEY_MAX: u8 = 255;
    const NUM_KEYS: u16 = 248;

    // Device descriptor: id, XI-1.x `use` code, name, and class count.
    struct Dev<'a> {
        id: u8,
        use_code: u8,
        name: &'a str,
        num_classes: u8,
    }

    fn push_button_info(out: &mut Vec<u8>, bo: ClientByteOrder, num_buttons: u16) {
        out.push(1); // class = ButtonClass
        out.push(4); // length in BYTES
        write_u16(bo, out, num_buttons);
    }

    fn push_key_info(
        out: &mut Vec<u8>,
        bo: ClientByteOrder,
        min_kc: u8,
        max_kc: u8,
        num_keys: u16,
    ) {
        out.push(0); // class = KeyClass
        out.push(8); // length in BYTES
        out.push(min_kc);
        out.push(max_kc);
        write_u16(bo, out, num_keys);
        write_u16(bo, out, 0); // pad
    }

    fn push_valuator_info(out: &mut Vec<u8>, bo: ClientByteOrder, mode: u8, axes: &[(i32, i32)]) {
        let num_axes = u8::try_from(axes.len()).unwrap_or(u8::MAX);
        let byte_len = 8 + 12 * axes.len();
        out.push(2); // class = ValuatorClass
        out.push(u8::try_from(byte_len).unwrap_or(u8::MAX)); // length in BYTES
        out.push(num_axes);
        out.push(mode);
        write_u32(bo, out, 0); // motion_buffer_size
        for &(min, max) in axes {
            write_u32(bo, out, 0); // resolution
            write_u32(bo, out, min.cast_unsigned());
            write_u32(bo, out, max.cast_unsigned());
        }
    }

    // Names are threaded in from the XI2 registry (ids 2, 3, 4, 5 in
    // order) so XI1 and XI2 never disagree on a device name.
    let [name2, name3, name4, name5] = names;
    let [type2, type3, type4, type5] = types;
    let devices = [
        Dev {
            id: 2,
            use_code: 0,
            name: name2,
            num_classes: 2,
        },
        Dev {
            id: 3,
            use_code: 1,
            name: name3,
            num_classes: 1,
        },
        Dev {
            id: 4,
            use_code: 4,
            name: name4,
            num_classes: 2,
        },
        Dev {
            id: 5,
            use_code: 3,
            name: name5,
            num_classes: 1,
        },
    ];
    let type_atoms = [type2, type3, type4, type5];

    // Class-info block for a given device id (matches num_classes above).
    let push_classes = |out: &mut Vec<u8>, id: u8| match id {
        2 | 4 => {
            push_button_info(out, byte_order, NUM_BUTTONS);
            push_valuator_info(out, byte_order, MODE_RELATIVE, &POINTER_AXES);
        }
        _ => push_key_info(out, byte_order, KEY_MIN, KEY_MAX, NUM_KEYS),
    };

    // 1. Device-info array.
    let mut data = Vec::new();
    for (d, &type_atom) in devices.iter().zip(type_atoms.iter()) {
        write_u32(byte_order, &mut data, type_atom); // type ATOM (XI device-type)
        data.push(d.id);
        data.push(d.num_classes);
        data.push(d.use_code);
        data.push(0); // attached (master device id; 0 = none, per XIproto.h)
    }
    // 2. Class-info blocks, per device, concatenated.
    for d in &devices {
        push_classes(&mut data, d.id);
    }
    // 3. Device names as a STR list (1 length byte + bytes each), in
    //    device order. Xorg's `CopyDeviceName` leaves one trailing NUL
    //    after the last name (its `strcpy` NUL is not overwritten by a
    //    following length byte); match that for byte-for-byte parity
    //    before the 4-byte pad.
    for d in &devices {
        // XI1's STR length is a single byte; clamp the emitted bytes to it
        // so the length and the payload always agree on the wire (a real
        // libinput device name is far below 255 bytes, but a name longer
        // than that must not desync the reply).
        let bytes = d.name.as_bytes();
        let len = bytes.len().min(usize::from(u8::MAX));
        data.push(u8::try_from(len).unwrap_or(u8::MAX));
        data.extend_from_slice(&bytes[..len]);
    }
    data.push(0); // trailing NUL after the names blob (Xorg parity)
    pad_vec4(&mut data);

    let ndevices = u8::try_from(devices.len()).unwrap_or(u8::MAX);
    let length = u32::try_from(data.len() / 4).unwrap_or(u32::MAX);
    // `xListInputDevicesReply` layout (XIproto.h): byte 0 = reply type,
    // byte 1 = RepType (client-ignored echo; 0 is safe), bytes 2-3 =
    // sequence, bytes 4-7 = length, **byte 8 = ndevices**, bytes 9-31
    // pad. ndevices is NOT in the standard reply "data" byte (byte 1) —
    // putting it there makes every client read ndevices=0 from byte 8
    // (the first device-info `type` ATOM byte) and ignore the whole
    // list. `fixed_reply` fills bytes 0-7; we then place ndevices at
    // byte 8 explicitly.
    let mut reply = fixed_reply(byte_order, sequence, 0, length);
    reply.push(ndevices); // byte 8
    reply.extend_from_slice(&[0u8; 23]); // bytes 9-31 pad
    reply.extend_from_slice(&data);
    reply
}

pub fn encode_xi2_device_changed_event(
    out: &mut Vec<u8>,
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    major_opcode: u8,
    deviceid: u16,
    time: u32,
    num_classes: u16,
    sourceid: u16,
    reason: u8,
    classes: &[u8],
) {
    out.push(35); // GenericEvent
    out.push(major_opcode);
    write_u16(byte_order, out, sequence.0);
    write_u32(
        byte_order,
        out,
        u32::try_from(classes.len() / 4).unwrap_or(u32::MAX),
    );
    write_u16(byte_order, out, 1); // XI_DeviceChanged
    write_u16(byte_order, out, deviceid);
    write_u32(byte_order, out, time);
    write_u16(byte_order, out, num_classes);
    write_u16(byte_order, out, sourceid);
    out.push(reason);
    out.push(0); // pad0
    write_u16(byte_order, out, 0); // pad1
    write_u32(byte_order, out, 0); // pad2
    write_u32(byte_order, out, 0); // pad3
    out.extend_from_slice(classes);
    debug_assert_eq!(out.len(), 32 + classes.len());
}

#[allow(clippy::too_many_arguments)]
pub fn encode_xi2_crossing_event(
    out: &mut Vec<u8>,
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    major_opcode: u8,
    evtype: u16,
    deviceid: u16,
    time: u32,
    root: ResourceId,
    event: ResourceId,
    root_x: i16,
    root_y: i16,
    event_x: i16,
    event_y: i16,
    state: u16,
    mode: u8,
    detail: u8,
    sourceid: u16,
) {
    out.push(35); // GenericEvent
    out.push(major_opcode);
    write_u16(byte_order, out, sequence.0);
    write_u32(byte_order, out, 11);

    write_u16(byte_order, out, evtype);
    write_u16(byte_order, out, deviceid);
    write_u32(byte_order, out, time);
    write_u16(byte_order, out, sourceid);
    out.push(mode);
    out.push(detail);
    write_u32(byte_order, out, root.0);
    write_u32(byte_order, out, event.0);
    write_u32(byte_order, out, 0); // child

    write_u32(byte_order, out, (i32::from(root_x) << 16) as u32);
    write_u32(byte_order, out, (i32::from(root_y) << 16) as u32);
    write_u32(byte_order, out, (i32::from(event_x) << 16) as u32);
    write_u32(byte_order, out, (i32::from(event_y) << 16) as u32);

    out.push(1); // same_screen
    out.push(u8::from(matches!(evtype, 9 | 10))); // focus
    write_u16(byte_order, out, 1); // buttons_len

    // mods: base, latched, locked, effective — KEYBOARD modifier bits
    // only (0x00FF mask); button bits in state[8..=12] go into the
    // separate `buttons` mask below, not mods. `base` mirrors `effective`
    // (Xorg fills base from held-key modifiers; see encode_xi2_device_event).
    let modifier_bits = u32::from(state & 0x00FF);
    write_u32(byte_order, out, modifier_bits); // base
    write_u32(byte_order, out, 0); // latched
    write_u32(byte_order, out, 0); // locked
    write_u32(byte_order, out, modifier_bits); // effective

    out.extend_from_slice(&[0; 4]); // group base/latched/locked/effective
    write_u32(byte_order, out, 0); // button mask

    debug_assert_eq!(out.len(), 76);
}

#[allow(clippy::too_many_arguments)]
pub fn encode_xi2_focus_event(
    out: &mut Vec<u8>,
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    major_opcode: u8,
    evtype: u16,
    deviceid: u16,
    time: u32,
    event: ResourceId,
    root_x: i16,
    root_y: i16,
    event_x: i16,
    event_y: i16,
    mode: u8,
    detail: u8,
) {
    encode_xi2_crossing_event(
        out,
        byte_order,
        sequence,
        major_opcode,
        evtype,
        deviceid,
        time,
        ResourceId(0x100),
        event,
        root_x,
        root_y,
        event_x,
        event_y,
        0,
        mode,
        detail,
        deviceid,
    );
}

pub fn write_get_selection_owner_reply(
    writer: &mut impl Write,
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    owner: ResourceId,
) -> io::Result<()> {
    let mut reply = fixed_reply(byte_order, sequence, 0, 0);
    write_u32(byte_order, &mut reply, owner.0);
    reply.extend_from_slice(&[0; 20]);
    writer.write_all(&reply)
}

pub fn write_grab_reply(
    writer: &mut impl Write,
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    status: u8,
) -> io::Result<()> {
    let mut reply = fixed_reply(byte_order, sequence, status, 0);
    reply.extend_from_slice(&[0; 24]);
    writer.write_all(&reply)
}

#[derive(Clone, Copy, Debug, Default)]
pub struct QueryPointerReply {
    pub root: ResourceId,
    pub child: ResourceId,
    pub root_x: i16,
    pub root_y: i16,
    pub win_x: i16,
    pub win_y: i16,
    pub mask: u16,
}

pub fn write_query_pointer_reply(
    writer: &mut impl Write,
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    reply_data: QueryPointerReply,
) -> io::Result<()> {
    let mut reply = fixed_reply(byte_order, sequence, 1, 0);
    write_u32(byte_order, &mut reply, reply_data.root.0);
    write_u32(byte_order, &mut reply, reply_data.child.0);
    write_i16(byte_order, &mut reply, reply_data.root_x);
    write_i16(byte_order, &mut reply, reply_data.root_y);
    write_i16(byte_order, &mut reply, reply_data.win_x);
    write_i16(byte_order, &mut reply, reply_data.win_y);
    write_u16(byte_order, &mut reply, reply_data.mask);
    reply.extend_from_slice(&[0; 6]);
    writer.write_all(&reply)
}

pub fn write_translate_coordinates_reply(
    writer: &mut impl Write,
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    child: ResourceId,
    dst_x: i16,
    dst_y: i16,
) -> io::Result<()> {
    let mut reply = fixed_reply(byte_order, sequence, 1, 0);
    write_u32(byte_order, &mut reply, child.0);
    write_i16(byte_order, &mut reply, dst_x);
    write_i16(byte_order, &mut reply, dst_y);
    reply.extend_from_slice(&[0; 16]);
    writer.write_all(&reply)
}

pub fn write_get_input_focus_reply(
    writer: &mut impl Write,
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    focus: ResourceId,
    revert_to: u8,
) -> io::Result<()> {
    let mut reply = fixed_reply(byte_order, sequence, revert_to, 0);
    write_u32(byte_order, &mut reply, focus.0);
    reply.extend_from_slice(&[0; 20]);
    writer.write_all(&reply)
}

pub fn write_query_keymap_reply(
    writer: &mut impl Write,
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
) -> io::Result<()> {
    let mut reply = fixed_reply(byte_order, sequence, 0, 2);
    reply.extend_from_slice(&[0; 32]);
    writer.write_all(&reply)
}

pub fn write_alloc_color_reply(
    writer: &mut impl Write,
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    color: Rgb16,
) -> io::Result<()> {
    let mut reply = fixed_reply(byte_order, sequence, 0, 0);
    write_u16(byte_order, &mut reply, color.red);
    write_u16(byte_order, &mut reply, color.green);
    write_u16(byte_order, &mut reply, color.blue);
    write_u16(byte_order, &mut reply, 0);
    write_u32(byte_order, &mut reply, rgb16_to_pixel(color));
    reply.extend_from_slice(&[0; 12]);
    writer.write_all(&reply)
}

pub fn write_lookup_color_reply(
    writer: &mut impl Write,
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    color: Rgb16,
) -> io::Result<()> {
    let mut reply = fixed_reply(byte_order, sequence, 0, 0);
    write_u16(byte_order, &mut reply, color.red);
    write_u16(byte_order, &mut reply, color.green);
    write_u16(byte_order, &mut reply, color.blue);
    write_u16(byte_order, &mut reply, color.red);
    write_u16(byte_order, &mut reply, color.green);
    write_u16(byte_order, &mut reply, color.blue);
    reply.extend_from_slice(&[0; 12]);
    writer.write_all(&reply)
}

pub fn write_alloc_named_color_reply(
    writer: &mut impl Write,
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    color: Rgb16,
) -> io::Result<()> {
    let mut reply = fixed_reply(byte_order, sequence, 0, 0);
    write_u32(byte_order, &mut reply, rgb16_to_pixel(color));
    write_u16(byte_order, &mut reply, color.red);
    write_u16(byte_order, &mut reply, color.green);
    write_u16(byte_order, &mut reply, color.blue);
    write_u16(byte_order, &mut reply, color.red);
    write_u16(byte_order, &mut reply, color.green);
    write_u16(byte_order, &mut reply, color.blue);
    reply.extend_from_slice(&[0; 8]);
    writer.write_all(&reply)
}

fn rgb16_to_pixel(color: Rgb16) -> u32 {
    ((u32::from(color.red) >> 8) << 16)
        | ((u32::from(color.green) >> 8) << 8)
        | (u32::from(color.blue) >> 8)
}

pub struct GetImageRequest {
    pub format: u8,
    pub drawable: ResourceId,
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
    pub plane_mask: u32,
}

pub fn get_image_request(format: u8, body: &[u8]) -> Option<GetImageRequest> {
    Some(GetImageRequest {
        format,
        drawable: ResourceId(read_u32_le(body.get(0..4)?)),
        x: read_i16_le(body.get(4..6)?),
        y: read_i16_le(body.get(6..8)?),
        width: read_u16_le(body.get(8..10)?),
        height: read_u16_le(body.get(10..12)?),
        plane_mask: read_u32_le(body.get(12..16)?),
    })
}

/// Return a blank (zeroed) image of the requested size at depth 24
/// (used when the backend can't resolve the drawable, so the true
/// depth is unknown). The data size must be consistent with the
/// format + plane_mask the client asked for, or Xlib misbehaves:
/// - ZPixmap: full pixel grid at 32 bpp.
/// - XYPixmap: one bitmap plane (scanline pad 32) per plane_mask bit;
///   an empty mask gets a 0-length reply (Xorg behavior — libX11's
///   `_XGetImage` NULL-derefs on a non-empty reply to a plane_mask=0
///   XYPixmap request).
pub fn write_get_image_reply(
    writer: &mut impl Write,
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    request: &GetImageRequest,
    visual_id: u32,
) -> io::Result<()> {
    const DEPTH: u8 = 24;
    let data_bytes: u32 = if request.format == 2 {
        // ZPixmap: 32 bits per pixel at depth 24
        let raw = u32::from(request.width)
            .saturating_mul(u32::from(request.height))
            .saturating_mul(4);
        (raw + 3) & !3 // round up to 4-byte boundary
    } else {
        // XYPixmap: planes truncated to the reply depth, each plane a
        // 1bpp bitmap with scanlines padded to 32 bits.
        let planes = (request.plane_mask & ((1u32 << DEPTH) - 1)).count_ones();
        let stride = u32::from(request.width).div_ceil(32) * 4;
        planes
            .saturating_mul(stride)
            .saturating_mul(u32::from(request.height))
    };
    let mut reply = fixed_reply(byte_order, sequence, DEPTH, data_bytes / 4);
    write_u32(byte_order, &mut reply, visual_id);
    reply.extend_from_slice(&[0u8; 20]);
    reply.extend(std::iter::repeat_n(0u8, data_bytes as usize));
    writer.write_all(&reply)
}

pub fn write_query_colors_reply(
    writer: &mut impl Write,
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    pixels: &[u32],
) -> io::Result<()> {
    let mut reply = fixed_reply(byte_order, sequence, 0, (pixels.len() * 2) as u32);
    write_u16(byte_order, &mut reply, pixels.len() as u16);
    reply.extend_from_slice(&[0; 22]);

    for pixel in pixels {
        let red = (((pixel >> 16) & 0xff) as u16) * 257;
        let green = (((pixel >> 8) & 0xff) as u16) * 257;
        let blue = ((pixel & 0xff) as u16) * 257;
        write_u16(byte_order, &mut reply, red);
        write_u16(byte_order, &mut reply, green);
        write_u16(byte_order, &mut reply, blue);
        write_u16(byte_order, &mut reply, 0);
    }

    writer.write_all(&reply)
}

pub fn write_query_extension_reply(
    writer: &mut impl Write,
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    present: bool,
    major_opcode: u8,
    first_event: u8,
    first_error: u8,
) -> io::Result<()> {
    let mut reply = fixed_reply(byte_order, sequence, 0, 0);
    reply.push(u8::from(present));
    reply.push(major_opcode);
    reply.push(first_event);
    reply.push(first_error);
    reply.extend_from_slice(&[0; 20]);
    writer.write_all(&reply)
}

pub fn write_ge_query_version_reply(
    writer: &mut impl Write,
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
) -> io::Result<()> {
    // GEQueryVersion reply: major=1, minor=0, rest padding
    let mut reply = fixed_reply(byte_order, sequence, 0, 0);
    reply.extend_from_slice(&[1, 0]); // major_version = 1
    reply.extend_from_slice(&[0, 0]); // minor_version = 0
    reply.extend_from_slice(&[0; 20]);
    writer.write_all(&reply)
}

pub fn write_big_requests_enable_reply(
    writer: &mut impl Write,
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    max_request_length: u32,
) -> io::Result<()> {
    let mut reply = fixed_reply(byte_order, sequence, 0, 0);
    write_u32(byte_order, &mut reply, max_request_length);
    reply.extend_from_slice(&[0; 20]);
    writer.write_all(&reply)
}

pub fn write_list_extensions_reply(
    writer: &mut impl Write,
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    names: &[&str],
) -> io::Result<()> {
    let mut names_raw = Vec::new();
    for name in names {
        let bytes = name.as_bytes();
        names_raw.push(bytes.len() as u8);
        names_raw.extend_from_slice(bytes);
    }
    pad_vec4(&mut names_raw);

    let mut reply = fixed_reply(
        byte_order,
        sequence,
        names.len() as u8,
        checked_units(names_raw.len())? as u32,
    );
    reply.extend_from_slice(&[0; 24]);
    reply.extend_from_slice(&names_raw);
    writer.write_all(&reply)
}

pub fn write_get_keyboard_mapping_reply(
    writer: &mut impl Write,
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    first_keycode: u8,
    keycode_count: u8,
    keysyms_per_keycode: u8,
) -> io::Result<()> {
    let keysym_count = u32::from(keycode_count) * u32::from(keysyms_per_keycode);
    let mut reply = fixed_reply(
        byte_order,
        sequence,
        keysyms_per_keycode,
        keysyms_per_keycode as u32,
    );
    reply.extend_from_slice(&[0; 24]);
    reply.truncate(32);
    reply[4..8].copy_from_slice(&keysym_count.to_le_bytes());

    for offset in 0..keycode_count {
        let keycode = first_keycode.wrapping_add(offset);
        let (base, shifted) = keysyms_for_keycode(keycode);
        write_u32(byte_order, &mut reply, base);
        if keysyms_per_keycode > 1 {
            write_u32(byte_order, &mut reply, shifted);
        }
        for _ in 2..keysyms_per_keycode {
            write_u32(byte_order, &mut reply, 0);
        }
    }
    writer.write_all(&reply)
}

pub fn write_query_font_reply(
    writer: &mut impl Write,
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    metrics: &FontMetrics,
) -> io::Result<()> {
    // Reply length is in 4-byte units beyond the 32-byte minimum reply.
    // Body after the 32-byte header carries 8n bytes of properties
    // (n = properties.len() / 8) plus 12m bytes of CHARINFOs.
    let n = metrics.properties.len() / 8;
    let m = metrics.char_infos.len();
    let reply_length = 7 + 2 * u32::try_from(n).unwrap_or(0) + 3 * u32::try_from(m).unwrap_or(0);

    let mut reply = fixed_reply(byte_order, sequence, 0, reply_length);
    write_full_char_info(&mut reply, &metrics.min_bounds);
    reply.extend_from_slice(&[0; 4]);
    write_full_char_info(&mut reply, &metrics.max_bounds);
    reply.extend_from_slice(&[0; 4]);
    write_u16(byte_order, &mut reply, metrics.min_char_or_byte2);
    write_u16(byte_order, &mut reply, metrics.max_char_or_byte2);
    write_u16(byte_order, &mut reply, metrics.default_char);
    write_u16(byte_order, &mut reply, u16::try_from(n).unwrap_or(0));
    reply.push(metrics.draw_direction);
    reply.push(metrics.min_byte1);
    reply.push(metrics.max_byte1);
    reply.push(u8::from(metrics.all_chars_exist));
    write_i16(byte_order, &mut reply, metrics.font_ascent);
    write_i16(byte_order, &mut reply, metrics.font_descent);
    write_u32(byte_order, &mut reply, u32::try_from(m).unwrap_or(0));
    reply.extend_from_slice(&metrics.properties[..n * 8]);
    for char_info in &metrics.char_infos {
        write_full_char_info(&mut reply, char_info);
    }
    writer.write_all(&reply)
}

/// Build a single ListFontsWithInfo reply from real font metrics.
/// Mirrors `write_query_font_reply` field-for-field through the
/// `font_descent` slot — the only divergence is the trailing
/// `replies-hint + name` instead of QueryFont's `char-infos count + char-
/// infos`. `remaining` is the count of further font replies still to
/// follow (excluding this reply and the terminator), per X11 spec.
pub fn write_list_fonts_with_info_reply(
    writer: &mut impl Write,
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    metrics: &FontMetrics,
    name: &str,
    remaining: u32,
) -> io::Result<()> {
    let nb = name.as_bytes();
    let nl = nb.len();
    let pad = (4usize.wrapping_sub(nl)) & 3;
    let n = metrics.properties.len() / 8;
    let body_after_header = 28 + 8 * n + nl + pad; // 28 byte LFWI tail + props + name(+pad)
    let reply_length = u32::try_from(body_after_header / 4).unwrap_or(0);

    let mut reply = fixed_reply(
        byte_order,
        sequence,
        u8::try_from(nl).unwrap_or(u8::MAX),
        reply_length,
    );
    write_full_char_info(&mut reply, &metrics.min_bounds);
    reply.extend_from_slice(&[0; 4]);
    write_full_char_info(&mut reply, &metrics.max_bounds);
    reply.extend_from_slice(&[0; 4]);
    write_u16(byte_order, &mut reply, metrics.min_char_or_byte2);
    write_u16(byte_order, &mut reply, metrics.max_char_or_byte2);
    write_u16(byte_order, &mut reply, metrics.default_char);
    write_u16(byte_order, &mut reply, u16::try_from(n).unwrap_or(0));
    reply.push(metrics.draw_direction);
    reply.push(metrics.min_byte1);
    reply.push(metrics.max_byte1);
    reply.push(u8::from(metrics.all_chars_exist));
    write_i16(byte_order, &mut reply, metrics.font_ascent);
    write_i16(byte_order, &mut reply, metrics.font_descent);
    write_u32(byte_order, &mut reply, remaining);
    reply.extend_from_slice(&metrics.properties[..n * 8]);
    reply.extend_from_slice(nb);
    reply.resize(reply.len() + pad, 0);
    writer.write_all(&reply)
}

/// LFWI terminator reply (`name_length == 0`). Used to mark end of the
/// per-font reply stream — mandatory whether or not any matches were
/// found.
pub fn write_list_fonts_with_info_terminator(
    writer: &mut impl Write,
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
) -> io::Result<()> {
    // Total reply is 60 bytes: 8-byte standard prefix from fixed_reply +
    // 52 bytes of zeroed CHARINFO/charset/metric fields (no properties,
    // no name). reply_length = 7 = (60 - 32) / 4.
    let reply = fixed_reply(byte_order, sequence, 0, 7);
    debug_assert_eq!(reply.len(), 8);
    let padding = [0u8; 52];
    writer.write_all(&reply)?;
    writer.write_all(&padding)
}

pub fn write_query_text_extents_reply(
    writer: &mut impl Write,
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    extents: TextExtents,
) -> io::Result<()> {
    let mut reply = fixed_reply(byte_order, sequence, extents.draw_direction, 0);
    write_i16(byte_order, &mut reply, extents.font_ascent);
    write_i16(byte_order, &mut reply, extents.font_descent);
    write_i16(byte_order, &mut reply, extents.overall_ascent);
    write_i16(byte_order, &mut reply, extents.overall_descent);
    write_u32(byte_order, &mut reply, extents.overall_width as u32);
    write_u32(byte_order, &mut reply, extents.overall_left as u32);
    write_u32(byte_order, &mut reply, extents.overall_right as u32);
    reply.extend_from_slice(&[0; 4]);
    writer.write_all(&reply)
}

pub fn write_list_hosts_reply(
    writer: &mut impl Write,
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
) -> io::Result<()> {
    let mut reply = fixed_reply(byte_order, sequence, 0, 0);
    write_u16(byte_order, &mut reply, 0);
    reply.extend_from_slice(&[0; 22]);
    writer.write_all(&reply)
}

pub fn write_query_best_size_reply(
    writer: &mut impl Write,
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    width: u16,
    height: u16,
) -> io::Result<()> {
    let mut reply = fixed_reply(byte_order, sequence, 0, 0);
    write_u16(byte_order, &mut reply, width);
    write_u16(byte_order, &mut reply, height);
    reply.extend_from_slice(&[0; 20]);
    writer.write_all(&reply)
}

pub fn write_get_pointer_mapping_reply(
    writer: &mut impl Write,
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
) -> io::Result<()> {
    let mut reply = fixed_reply(byte_order, sequence, 3, 1);
    reply.extend_from_slice(&[0; 24]);
    reply.extend_from_slice(&[1, 2, 3, 0]);
    writer.write_all(&reply)
}

/// Degenerate `GetModifierMapping` reply used only when the backend
/// cannot supply a real mapping. Emits `keycodes_per_modifier = 0`
/// (an empty, wire-valid table) rather than inventing keycodes — the
/// real mapping comes from `write_get_modifier_mapping_reply_with_keycodes`
/// fed by the backend's keymap-derived table.
pub fn write_get_modifier_mapping_reply(
    writer: &mut impl Write,
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
) -> io::Result<()> {
    let mut reply = fixed_reply(byte_order, sequence, 0, 0);
    reply.extend_from_slice(&[0; 24]);
    writer.write_all(&reply)
}

fn write_full_char_info(out: &mut Vec<u8>, info: &CharInfo) {
    write_i16(ClientByteOrder::LittleEndian, out, info.left_side_bearing);
    write_i16(ClientByteOrder::LittleEndian, out, info.right_side_bearing);
    write_i16(ClientByteOrder::LittleEndian, out, info.character_width);
    write_i16(ClientByteOrder::LittleEndian, out, info.ascent);
    write_i16(ClientByteOrder::LittleEndian, out, info.descent);
    write_u16(ClientByteOrder::LittleEndian, out, info.attributes);
}

fn read_char_info(bytes: &[u8]) -> Option<CharInfo> {
    Some(CharInfo {
        left_side_bearing: read_i16_le(bytes.get(0..2)?),
        right_side_bearing: read_i16_le(bytes.get(2..4)?),
        character_width: read_i16_le(bytes.get(4..6)?),
        ascent: read_i16_le(bytes.get(6..8)?),
        descent: read_i16_le(bytes.get(8..10)?),
        attributes: read_u16_le(bytes.get(10..12)?),
    })
}

/// Parse the body of a `QueryFont` reply (the 60 bytes after the standard
/// 8-byte reply header, plus the trailing properties and CHARINFOs).
///
/// `body` must start at byte 8 of the reply (immediately after the
/// `reply length` field) and span the rest of the reply payload.
pub fn parse_query_font_reply(body: &[u8]) -> Option<FontMetrics> {
    let min_bounds = read_char_info(body.get(0..12)?)?;
    let max_bounds = read_char_info(body.get(16..28)?)?;
    let min_char_or_byte2 = read_u16_le(body.get(32..34)?);
    let max_char_or_byte2 = read_u16_le(body.get(34..36)?);
    let default_char = read_u16_le(body.get(36..38)?);
    let n = usize::from(read_u16_le(body.get(38..40)?));
    let draw_direction = *body.get(40)?;
    let min_byte1 = *body.get(41)?;
    let max_byte1 = *body.get(42)?;
    let all_chars_exist = *body.get(43)? != 0;
    let font_ascent = read_i16_le(body.get(44..46)?);
    let font_descent = read_i16_le(body.get(46..48)?);
    let m = read_u32_le(body.get(48..52)?) as usize;

    let props_offset = 52;
    let props_len = n.checked_mul(8)?;
    let properties = body.get(props_offset..props_offset + props_len)?.to_vec();

    let chars_offset = props_offset + props_len;
    let mut char_infos = Vec::with_capacity(m);
    for i in 0..m {
        let start = chars_offset + i * 12;
        char_infos.push(read_char_info(body.get(start..start + 12)?)?);
    }

    Some(FontMetrics {
        min_bounds,
        max_bounds,
        min_char_or_byte2,
        max_char_or_byte2,
        default_char,
        draw_direction,
        min_byte1,
        max_byte1,
        all_chars_exist,
        font_ascent,
        font_descent,
        properties,
        named_properties: Vec::new(),
        char_infos,
    })
}

pub fn encode_property_notify_event(
    out: &mut Vec<u8>,
    sequence: SequenceNumber,
    order: ClientByteOrder,
    window: ResourceId,
    atom: AtomId,
    timestamp: u32,
    deleted: bool,
) {
    out.push(28); // PropertyNotify
    out.push(0);
    write_u16(order, out, sequence.0);
    write_u32(order, out, window.0);
    write_u32(order, out, atom.0);
    write_u32(order, out, timestamp);
    out.push(u8::from(deleted));
    out.extend_from_slice(&[0; 15]);
}

pub fn write_property_notify_event(
    writer: &mut impl Write,
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    window: ResourceId,
    atom: AtomId,
    timestamp: u32,
    deleted: bool,
) -> io::Result<()> {
    let mut out = Vec::with_capacity(32);
    encode_property_notify_event(
        &mut out, sequence, byte_order, window, atom, timestamp, deleted,
    );
    writer.write_all(&out)
}

pub fn encode_destroy_notify_event(
    out: &mut Vec<u8>,
    sequence: SequenceNumber,
    order: ClientByteOrder,
    event_window: ResourceId,
    window: ResourceId,
) {
    out.push(17); // DestroyNotify
    out.push(0);
    write_u16(order, out, sequence.0);
    write_u32(order, out, event_window.0);
    write_u32(order, out, window.0);
    out.extend_from_slice(&[0; 20]);
}

pub fn encode_unmap_notify_event(
    out: &mut Vec<u8>,
    sequence: SequenceNumber,
    order: ClientByteOrder,
    event_window: ResourceId,
    window: ResourceId,
    from_configure: bool,
) {
    out.push(18); // UnmapNotify
    out.push(0);
    write_u16(order, out, sequence.0);
    write_u32(order, out, event_window.0);
    write_u32(order, out, window.0);
    out.push(u8::from(from_configure));
    out.extend_from_slice(&[0; 19]);
}

#[allow(clippy::too_many_arguments)]
pub fn encode_reparent_notify_event(
    out: &mut Vec<u8>,
    sequence: SequenceNumber,
    order: ClientByteOrder,
    event_window: ResourceId,
    window: ResourceId,
    parent: ResourceId,
    x: i16,
    y: i16,
    override_redirect: bool,
) {
    out.push(21); // ReparentNotify
    out.push(0);
    write_u16(order, out, sequence.0);
    write_u32(order, out, event_window.0);
    write_u32(order, out, window.0);
    write_u32(order, out, parent.0);
    write_i16(order, out, x);
    write_i16(order, out, y);
    out.push(u8::from(override_redirect));
    out.extend_from_slice(&[0; 11]);
}

pub fn encode_client_message_event(
    out: &mut Vec<u8>,
    order: ClientByteOrder,
    event: ClientMessageEvent,
) {
    out.push(33 | if event.send_event { 0x80 } else { 0 });
    out.push(event.format);
    write_u16(order, out, event.sequence.0);
    write_u32(order, out, event.window.0);
    write_u32(order, out, event.r#type.0);
    out.extend_from_slice(&event.data);
}

fn encode_pointer_event(
    out: &mut Vec<u8>,
    event_code: u8,
    order: ClientByteOrder,
    event: PointerEvent,
) {
    out.push(event_code);
    out.push(event.detail);
    write_u16(order, out, event.sequence.0);
    write_u32(order, out, event.time);
    write_u32(order, out, event.root.0);
    write_u32(order, out, event.event.0);
    write_u32(order, out, event.child.0);
    write_i16(order, out, event.root_x);
    write_i16(order, out, event.root_y);
    write_i16(order, out, event.event_x);
    write_i16(order, out, event.event_y);
    write_u16(order, out, event.state);
    out.push(1); // same_screen
    out.push(0); // pad
}

pub fn encode_button_press_event(out: &mut Vec<u8>, order: ClientByteOrder, event: PointerEvent) {
    encode_pointer_event(out, 4, order, event);
}

pub fn encode_button_release_event(out: &mut Vec<u8>, order: ClientByteOrder, event: PointerEvent) {
    encode_pointer_event(out, 5, order, event);
}

pub fn encode_motion_notify_event(out: &mut Vec<u8>, order: ClientByteOrder, event: PointerEvent) {
    encode_pointer_event(out, 6, order, event);
}

fn encode_crossing_event(
    out: &mut Vec<u8>,
    event_code: u8,
    order: ClientByteOrder,
    event: CrossingEvent,
) {
    out.push(event_code);
    out.push(event.detail);
    write_u16(order, out, event.sequence.0);
    write_u32(order, out, event.time);
    write_u32(order, out, event.root.0);
    write_u32(order, out, event.event.0);
    write_u32(order, out, event.child.0);
    write_i16(order, out, event.root_x);
    write_i16(order, out, event.root_y);
    write_i16(order, out, event.event_x);
    write_i16(order, out, event.event_y);
    write_u16(order, out, event.state);
    out.push(event.mode);
    out.push(0x03); // same_screen + focus
}

pub fn encode_enter_notify_event(out: &mut Vec<u8>, order: ClientByteOrder, event: CrossingEvent) {
    encode_crossing_event(out, 7, order, event);
}

pub fn encode_leave_notify_event(out: &mut Vec<u8>, order: ClientByteOrder, event: CrossingEvent) {
    encode_crossing_event(out, 8, order, event);
}

pub fn encode_selection_request_event(
    out: &mut Vec<u8>,
    seq: SequenceNumber,
    order: ClientByteOrder,
    time: u32,
    owner: ResourceId,
    requestor: ResourceId,
    selection: AtomId,
    target: AtomId,
    property: AtomId,
) {
    out.push(30); // SelectionRequest
    out.push(0); // pad
    write_u16(order, out, seq.0);
    write_u32(order, out, time);
    write_u32(order, out, owner.0);
    write_u32(order, out, requestor.0);
    write_u32(order, out, selection.0);
    write_u32(order, out, target.0);
    write_u32(order, out, property.0);
    out.extend_from_slice(&[0u8; 4]); // pad to 32
    debug_assert!(out.len() >= 32);
}

pub fn encode_selection_clear_event(
    out: &mut Vec<u8>,
    seq: SequenceNumber,
    order: ClientByteOrder,
    time: u32,
    owner: ResourceId,
    selection: AtomId,
) {
    out.push(29); // SelectionClear
    out.push(0); // pad
    write_u16(order, out, seq.0);
    write_u32(order, out, time);
    write_u32(order, out, owner.0);
    write_u32(order, out, selection.0);
    out.extend_from_slice(&[0u8; 16]); // pad to 32
    debug_assert!(out.len() >= 32);
}

#[derive(Debug, Clone, Copy)]
pub struct GrabKeyRequest {
    pub owner_events: bool,
    pub grab_window: u32,
    pub modifiers: u16,
    pub keycode: u8,
    pub pointer_mode: u8,
    pub keyboard_mode: u8,
}

#[must_use]
pub fn parse_grab_key(body: &[u8], owner_events: bool) -> Option<GrabKeyRequest> {
    if body.len() < 12 {
        return None;
    }
    Some(GrabKeyRequest {
        owner_events,
        grab_window: u32::from_le_bytes([body[0], body[1], body[2], body[3]]),
        modifiers: u16::from_le_bytes([body[4], body[5]]),
        keycode: body[6],
        pointer_mode: body[7],
        keyboard_mode: body[8],
    })
}

#[derive(Debug, Clone, Copy)]
pub struct UngrabKeyRequest {
    pub keycode: u8,
    pub grab_window: u32,
    pub modifiers: u16,
}

#[must_use]
pub fn parse_ungrab_key(body: &[u8], keycode_in_header_data: u8) -> Option<UngrabKeyRequest> {
    if body.len() < 6 {
        return None;
    }
    Some(UngrabKeyRequest {
        keycode: keycode_in_header_data,
        grab_window: u32::from_le_bytes([body[0], body[1], body[2], body[3]]),
        modifiers: u16::from_le_bytes([body[4], body[5]]),
    })
}

pub fn write_mapping_notify_event(
    writer: &mut impl Write,
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    request: u8,
    first_keycode: u8,
    count: u8,
) -> io::Result<()> {
    let mut buf = [0u8; 32];
    buf[0] = 34;
    let mut seq_buf = Vec::with_capacity(2);
    write_u16(byte_order, &mut seq_buf, sequence.0);
    buf[2..4].copy_from_slice(&seq_buf);
    buf[4] = request;
    buf[5] = first_keycode;
    buf[6] = count;
    writer.write_all(&buf)
}

pub fn write_circulate_notify_event(
    writer: &mut impl Write,
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    event_window: ResourceId,
    window: ResourceId,
    place: u8,
) -> io::Result<()> {
    let mut buf = [0u8; 32];
    buf[0] = 26;
    let mut seq_buf = Vec::with_capacity(2);
    write_u16(byte_order, &mut seq_buf, sequence.0);
    buf[2..4].copy_from_slice(&seq_buf);
    let mut u32_buf = Vec::with_capacity(4);
    write_u32(byte_order, &mut u32_buf, event_window.0);
    buf[4..8].copy_from_slice(&u32_buf);
    u32_buf.clear();
    write_u32(byte_order, &mut u32_buf, window.0);
    buf[8..12].copy_from_slice(&u32_buf);
    buf[16] = place;
    writer.write_all(&buf)
}

pub fn write_circulate_request_event(
    writer: &mut impl Write,
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    parent: ResourceId,
    window: ResourceId,
    place: u8,
) -> io::Result<()> {
    let mut buf = [0u8; 32];
    buf[0] = 27;
    let mut seq_buf = Vec::with_capacity(2);
    write_u16(byte_order, &mut seq_buf, sequence.0);
    buf[2..4].copy_from_slice(&seq_buf);
    let mut u32_buf = Vec::with_capacity(4);
    write_u32(byte_order, &mut u32_buf, parent.0);
    buf[4..8].copy_from_slice(&u32_buf);
    u32_buf.clear();
    write_u32(byte_order, &mut u32_buf, window.0);
    buf[8..12].copy_from_slice(&u32_buf);
    buf[16] = place;
    writer.write_all(&buf)
}

pub fn write_get_keyboard_mapping_reply_from_keysyms(
    writer: &mut impl Write,
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    keysyms_per_keycode: u8,
    keysyms: &[u32],
) -> io::Result<()> {
    let length_words = u32::try_from(keysyms.len()).unwrap_or(0);
    let mut reply = fixed_reply(byte_order, sequence, keysyms_per_keycode, length_words);
    reply.extend_from_slice(&[0u8; 24]);
    reply.truncate(32);
    for k in keysyms {
        let mut tmp = Vec::with_capacity(4);
        write_u32(byte_order, &mut tmp, *k);
        reply.extend_from_slice(&tmp);
    }
    writer.write_all(&reply)
}

pub fn write_get_modifier_mapping_reply_with_keycodes(
    writer: &mut impl Write,
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    keycodes_per_modifier: u8,
    keycodes: &[u8],
) -> io::Result<()> {
    debug_assert_eq!(
        keycodes.len(),
        8 * keycodes_per_modifier as usize,
        "GetModifierMapping payload must be exactly 8 * keycodes_per_modifier"
    );
    let total = 8 * u32::from(keycodes_per_modifier);
    let length_words = total / 4;
    let mut reply = fixed_reply(byte_order, sequence, keycodes_per_modifier, length_words);
    reply.extend_from_slice(&[0u8; 24]);
    reply.truncate(32);
    reply.extend_from_slice(keycodes);
    writer.write_all(&reply)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fallback GetImage reply must keep its length field and
    /// payload consistent with the requested format + plane_mask:
    /// XYPixmap carries popcount(mask) bitmap planes, and an empty
    /// mask yields a 0-length reply (libX11 NULL-derefs otherwise).
    #[test]
    fn get_image_fallback_reply_sizes_match_format() {
        let req = |format: u8, plane_mask: u32| GetImageRequest {
            format,
            drawable: ResourceId(1),
            x: 0,
            y: 0,
            width: 40,
            height: 2,
            plane_mask,
        };
        let write = |r: &GetImageRequest| {
            let mut buf = Vec::new();
            write_get_image_reply(
                &mut buf,
                ClientByteOrder::LittleEndian,
                SequenceNumber(1),
                r,
                0x21,
            )
            .unwrap();
            buf
        };

        // XYPixmap, plane_mask=0 → header only, length 0.
        let buf = write(&req(1, 0));
        assert_eq!(buf.len(), 32);
        assert_eq!(u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]), 0);

        // XYPixmap, 3 planes → 3 × stride(40px→8B) × 2 rows = 48 bytes.
        let buf = write(&req(1, 0b111));
        assert_eq!(buf.len(), 32 + 48);
        assert_eq!(u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]), 12);

        // ZPixmap → full 32bpp grid regardless of mask.
        let buf = write(&req(2, 0));
        assert_eq!(buf.len(), 32 + 40 * 2 * 4);
    }

    #[test]
    fn encode_shm_completion_event_wire_layout() {
        // Ground-truthed against `xShmCompletionEvent` (shmproto.h):
        // type, bpad, seq, drawable, minorEvent, majorEvent, bpad,
        // shmseg, offset, 12 pad — 32 bytes total.
        let mut buf = Vec::new();
        encode_shm_completion_event(
            &mut buf,
            ClientByteOrder::LittleEndian,
            SequenceNumber(0x1234),
            65, // event_code = first_event + ShmCompletion(0)
            ResourceId(0xDEAD_BEEF),
            3,           // X_ShmPutImage
            130,         // MIT-SHM major
            0x00AB_CDEF, // shmseg
            0x0001_0000, // offset
        );
        assert_eq!(buf.len(), 32, "ShmCompletion is 32 bytes");
        assert_eq!(buf[0], 65, "event code");
        assert_eq!(u16::from_le_bytes([buf[2], buf[3]]), 0x1234, "sequence");
        assert_eq!(
            u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]),
            0xDEAD_BEEF,
            "drawable"
        );
        assert_eq!(u16::from_le_bytes([buf[8], buf[9]]), 3, "minorEvent");
        assert_eq!(buf[10], 130, "majorEvent");
        assert_eq!(
            u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]),
            0x00AB_CDEF,
            "shmseg"
        );
        assert_eq!(
            u32::from_le_bytes([buf[16], buf[17], buf[18], buf[19]]),
            0x0001_0000,
            "offset"
        );
    }

    #[test]
    fn encode_configure_notify_event_writes_above_sibling() {
        let mut buf = Vec::new();
        encode_configure_notify_event(
            &mut buf,
            SequenceNumber(0x1234),
            ClientByteOrder::LittleEndian,
            ResourceId(0x100),
            ResourceId(0x200),
            Some(ResourceId(0x300)),
            Geometry {
                root: ResourceId(0x100),
                x: 10,
                y: 20,
                width: 640,
                height: 480,
                border_width: 2,
                depth: 24,
            },
            false,
        );

        assert_eq!(buf[0], 22);
        assert_eq!(u32::from_le_bytes(buf[12..16].try_into().unwrap()), 0x300);
    }

    #[test]
    fn write_list_fonts_with_info_reply_round_trip() {
        // Matches the byte layout libXt's xcb_list_fonts_with_info_reply_t
        // expects. We assert each field's offset explicitly because LFWI
        // sits in the same wire-format family as QueryFont but with a
        // name+padding tail instead of charinfo data — easy to get the
        // tail length wrong.
        let metrics = FontMetrics {
            min_bounds: CharInfo {
                left_side_bearing: 0,
                right_side_bearing: 6,
                character_width: 6,
                ascent: 12,
                descent: 4,
                attributes: 0,
            },
            max_bounds: CharInfo {
                left_side_bearing: 0,
                right_side_bearing: 7,
                character_width: 7,
                ascent: 13,
                descent: 4,
                attributes: 0,
            },
            min_char_or_byte2: 32,
            max_char_or_byte2: 126,
            default_char: 32,
            draw_direction: 0,
            min_byte1: 0,
            max_byte1: 0,
            all_chars_exist: true,
            font_ascent: 13,
            font_descent: 4,
            properties: Vec::new(),
            named_properties: Vec::new(),
            char_infos: Vec::new(),
        };
        let name = "-fc-dejavu sans mono-medium-r-normal--12-120-75-75-m-72-iso8859-1";
        let mut buf = Vec::new();
        write_list_fonts_with_info_reply(
            &mut buf,
            ClientByteOrder::LittleEndian,
            SequenceNumber(0xabcd),
            &metrics,
            name,
            7,
        )
        .unwrap();

        assert_eq!(buf[0], 1, "reply type");
        assert_eq!(buf[1] as usize, name.len(), "name_len");
        // sequence at [2..4]
        assert_eq!(u16::from_le_bytes([buf[2], buf[3]]), 0xabcd);
        // reply_length: word count after the 32-byte header.
        let words = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
        assert_eq!(words as usize * 4 + 32, buf.len());
        // min_bounds.character_width at [8+4..8+6] = [12..14]
        assert_eq!(i16::from_le_bytes([buf[12], buf[13]]), 6);
        // max_bounds.character_width at [24+4..24+6] = [28..30]
        assert_eq!(i16::from_le_bytes([buf[28], buf[29]]), 7);
        // all-chars-exist at [51]
        assert_eq!(buf[51], 1);
        // font-ascent at [52..54], font-descent at [54..56]
        assert_eq!(i16::from_le_bytes([buf[52], buf[53]]), 13);
        assert_eq!(i16::from_le_bytes([buf[54], buf[55]]), 4);
        // replies-hint at [56..60]
        assert_eq!(u32::from_le_bytes([buf[56], buf[57], buf[58], buf[59]]), 7);
        // Name follows (no FONTPROPs, so it starts at offset 60).
        assert_eq!(&buf[60..60 + name.len()], name.as_bytes());
        // Trailing padding is zero.
        for &b in &buf[60 + name.len()..] {
            assert_eq!(b, 0, "padding must be zero");
        }
    }

    #[test]
    fn write_list_fonts_with_info_terminator_layout() {
        let mut buf = Vec::new();
        write_list_fonts_with_info_terminator(
            &mut buf,
            ClientByteOrder::LittleEndian,
            SequenceNumber(0x1234),
        )
        .unwrap();
        assert_eq!(buf.len(), 60, "fixed 60-byte LFWI terminator");
        assert_eq!(buf[0], 1, "reply type");
        assert_eq!(buf[1], 0, "name_len = 0 → terminator");
        assert_eq!(u16::from_le_bytes([buf[2], buf[3]]), 0x1234);
        assert_eq!(u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]), 7);
    }

    #[test]
    fn read_request_rejects_zero_length_without_big_requests() {
        let mut input = std::io::Cursor::new([1, 2, 0, 0]);
        let err = read_request(&mut input, ClientByteOrder::LittleEndian, false)
            .expect_err("zero length must be invalid");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn read_request_accepts_big_requests_extended_length() {
        let mut input = std::io::Cursor::new([
            1, 2, 0, 0, // normal header with extended length marker
            3, 0, 0, 0, // 12-byte total request: header + big length + 4 body bytes
            0xaa, 0xbb, 0xcc, 0xdd,
        ]);
        let (header, body) = read_request(&mut input, ClientByteOrder::LittleEndian, true)
            .expect("read should succeed")
            .expect("request should be present");

        assert_eq!(header.opcode, 1);
        assert_eq!(header.data, 2);
        assert_eq!(header.length_units, 3);
        assert_eq!(body, [0xaa, 0xbb, 0xcc, 0xdd]);
    }

    #[test]
    fn read_request_be_normal_length() {
        // Opcode 1, data 2, length_units = 3 in BE (00 03), body = 8 bytes.
        let mut input = std::io::Cursor::new([
            1, 2, 0x00, 0x03, // BE-encoded length = 3 units
            0xaa, 0xbb, 0xcc, 0xdd, 0x11, 0x22, 0x33, 0x44,
        ]);
        let (header, body) = read_request(&mut input, ClientByteOrder::BigEndian, false)
            .expect("read should succeed")
            .expect("request should be present");
        assert_eq!(header.opcode, 1);
        assert_eq!(header.data, 2);
        assert_eq!(header.length_units, 3);
        assert_eq!(body.len(), 8);
        assert_eq!(body, [0xaa, 0xbb, 0xcc, 0xdd, 0x11, 0x22, 0x33, 0x44]);
    }

    #[test]
    fn read_request_be_big_requests_extended_length() {
        // Opcode 1, data 2, length_units = 0 (BE) → BIG-REQUESTS path,
        // big length = 3 (BE), so total = 12 bytes, body = 4 bytes.
        let mut input = std::io::Cursor::new([
            1, 2, 0x00, 0x00, // BE-encoded length = 0 → BIG path
            0x00, 0x00, 0x00, 0x03, // BE-encoded big length = 3 units
            0xaa, 0xbb, 0xcc, 0xdd,
        ]);
        let (header, body) = read_request(&mut input, ClientByteOrder::BigEndian, true)
            .expect("read should succeed")
            .expect("request should be present");
        assert_eq!(header.opcode, 1);
        assert_eq!(header.length_units, 3);
        assert_eq!(body, [0xaa, 0xbb, 0xcc, 0xdd]);
    }

    #[test]
    fn read_request_flags_malformed_big_length_under_two_units() {
        // xts5 ListInputDevices-2 sends `length=0, big=1` to probe
        // BadLength on sub-header BIG-REQUESTS. We must NOT disconnect
        // — emit a sentinel `length_units = u32::MAX` so the
        // dispatcher's max-length gate fires BadLength.
        let mut input = std::io::Cursor::new([
            2, 0, 0, 0, // ListInputDevices opcode, length=0
            1, 0, 0, 0, // BIG length = 1 (LE) — sub-header
        ]);
        let (header, body) = read_request(&mut input, ClientByteOrder::LittleEndian, true)
            .expect("read should succeed")
            .expect("request should be present");
        assert_eq!(header.opcode, 2);
        assert_eq!(header.length_units, u32::MAX);
        assert!(body.is_empty());
    }

    #[test]
    fn read_request_flags_over_max_big_length_and_drains_body() {
        // xts5 TOO_LONG: bigRequestLength = max+1 = 262145 with the
        // matching payload bytes on the wire. We override length to
        // the BadLength sentinel and drain the payload to keep the
        // socket aligned for the next request.
        const OVER_MAX: u32 = 256 * 1024 + 1; // 262145 units
        const BODY_BYTES: usize = (OVER_MAX as usize * 4) - 8;
        let mut input = Vec::with_capacity(8 + BODY_BYTES + 4);
        input.extend_from_slice(&[1, 2, 0, 0]); // opcode, data, length=0
        input.extend_from_slice(&OVER_MAX.to_le_bytes()); // BIG length
        input.resize(input.len() + BODY_BYTES, 0xaa); // claimed payload
        // Sentinel bytes after the malformed request — must be intact
        // for the next read_request call.
        input.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef]);

        let mut cursor = std::io::Cursor::new(input);
        let (header, body) = read_request(&mut cursor, ClientByteOrder::LittleEndian, true)
            .expect("read should succeed")
            .expect("request should be present");
        assert_eq!(header.opcode, 1);
        assert_eq!(header.length_units, u32::MAX);
        assert!(body.is_empty());

        // The cursor should now be positioned at the sentinel.
        assert_eq!(cursor.position() as usize, 8 + BODY_BYTES);
        let mut next = [0u8; 4];
        std::io::Read::read_exact(&mut cursor, &mut next).unwrap();
        assert_eq!(next, [0xde, 0xad, 0xbe, 0xef]);
    }

    #[test]
    fn write_error_be_encodes_fields_in_big_endian() {
        let mut buf = Vec::new();
        write_error(
            &mut buf,
            ClientByteOrder::BigEndian,
            SequenceNumber(0x1234),
            error::BAD_LENGTH,
            0xdead_beef,
            0x4321,
            42,
        )
        .unwrap();
        assert_eq!(buf.len(), 32);
        assert_eq!(buf[0], 0); // error response_type
        assert_eq!(buf[1], error::BAD_LENGTH);
        // sequence is u16 BE at [2..4]
        assert_eq!(&buf[2..4], &[0x12, 0x34]);
        // bad_value is u32 BE at [4..8]
        assert_eq!(&buf[4..8], &[0xde, 0xad, 0xbe, 0xef]);
        // minor_opcode is u16 BE at [8..10]
        assert_eq!(&buf[8..10], &[0x43, 0x21]);
        // major_opcode is u8 at [10]
        assert_eq!(buf[10], 42);
    }

    #[test]
    fn xi2_motion_event_carries_axes() {
        let mut out = Vec::new();
        encode_xi2_device_event(
            &mut out,
            ClientByteOrder::LittleEndian,
            SequenceNumber(7),
            137,
            6, // XI_Motion
            2,
            123,
            ResourceId(0x100),
            ResourceId(0x200),
            ResourceId(0x300),
            1,
            2,
            3,
            4,
            5,
            0, // detail = 0 for motion
            2,
            0, // flags
        );

        // Width-padded to match Xorg byte-for-byte: 32 header + 16 coords
        // + 12 lens/sourceid/pad/flags + 16 mods + 4 group +
        // 32 buttons mask (8 u32s) + 8 valuator mask (2 u32s) +
        // 16 axisvalues (2 FP3232 X/Y) = 136 bytes.
        assert_eq!(out.len(), 136);
        assert_eq!(&out[0..4], &[35, 137, 7, 0]);
        // length-in-4-byte-units beyond the 32-byte header: (136 - 32) / 4 = 26.
        assert_eq!(u32::from_le_bytes(out[4..8].try_into().unwrap()), 26);
        // Valuator mask at offset 112 (after group at 80 + 32 button mask): X+Y bits.
        assert_eq!(u32::from_le_bytes(out[112..116].try_into().unwrap()), 0x3);
        // X axis integer part at offset 120 (after 8 byte valuator mask): root_x = 1.
        assert_eq!(u32::from_le_bytes(out[120..124].try_into().unwrap()), 1);
        // X fraction at offset 124: 0.
        assert_eq!(u32::from_le_bytes(out[124..128].try_into().unwrap()), 0);
        // Y axis integer part at offset 128: root_y = 2.
        assert_eq!(u32::from_le_bytes(out[128..132].try_into().unwrap()), 2);
    }

    #[test]
    fn xi2_button_event_has_no_axes() {
        // Matches the Xorg reference: XI_ButtonPress / XI_ButtonRelease
        // carry valuators_len=0, no valuator mask, no axisvalues.
        // Sampled from thunar-inside-MATE 2026-05-15.
        let mut out = Vec::new();
        encode_xi2_device_event(
            &mut out,
            ClientByteOrder::LittleEndian,
            SequenceNumber(7),
            137,
            4, // XI_ButtonPress
            2,
            123,
            ResourceId(0x100),
            ResourceId(0x200),
            ResourceId(0x300),
            1,
            2,
            3,
            4,
            5,
            1,
            2,
            0, // flags
        );

        // Width-padded to match Xorg: 32 header + 16 coords + 12 lens +
        // 16 mods + 4 group + 32 buttons mask (8 u32s) + 8 valuator
        // mask (2 u32s) + 0 axes = 120 bytes.
        assert_eq!(out.len(), 120);
        assert_eq!(&out[0..4], &[35, 137, 7, 0]);
        // length units: (120 - 32) / 4 = 22.
        assert_eq!(u32::from_le_bytes(out[4..8].try_into().unwrap()), 22);
        // buttons_len at offset 48: 8 (Xorg-matching pad width).
        assert_eq!(u16::from_le_bytes(out[48..50].try_into().unwrap()), 8);
        // valuators_len at offset 50: 2 (Xorg-matching pad width).
        assert_eq!(u16::from_le_bytes(out[50..52].try_into().unwrap()), 2);
        // Valuator mask at offset 112 (after 32-byte buttons mask): 0 — button
        // events change no axes.
        assert_eq!(u32::from_le_bytes(out[112..116].try_into().unwrap()), 0);
    }

    #[test]
    fn xi2_crossing_event_has_expected_wire_size() {
        let mut out = Vec::new();
        encode_xi2_crossing_event(
            &mut out,
            ClientByteOrder::LittleEndian,
            SequenceNumber(8),
            137,
            7,
            2,
            123,
            ResourceId(0x100),
            ResourceId(0x200),
            1,
            2,
            3,
            4,
            5,
            0,
            0,
            2,
        );

        assert_eq!(out.len(), 76);
        assert_eq!(&out[0..4], &[35, 137, 8, 0]);
        assert_eq!(u32::from_le_bytes(out[4..8].try_into().unwrap()), 11);
    }

    mod change_property_tests {
        use super::*;
        use proptest::prelude::*;

        fn encode(req: &ChangePropertyRequest) -> (u8, Vec<u8>) {
            let mut body = Vec::new();
            write_u32(ClientByteOrder::LittleEndian, &mut body, req.window.0);
            write_u32(ClientByteOrder::LittleEndian, &mut body, req.property.0);
            write_u32(ClientByteOrder::LittleEndian, &mut body, req.r#type.0);
            body.push(req.format);
            body.extend_from_slice(&[0; 3]);
            write_u32(ClientByteOrder::LittleEndian, &mut body, req.length);
            body.extend_from_slice(&req.data);
            pad_vec4(&mut body);
            (req.mode, body)
        }

        proptest! {
            #[test]
            fn round_trip(
                mode in 0u8..=2,
                window in any::<u32>(),
                property in 1u32..0xFFFF,
                r#type in 1u32..0xFFFF,
                format_choice in 0u8..3,
                length in 0u32..256,
            ) {
                let format = [8u8, 16, 32][format_choice as usize];
                let unit = match format { 8 => 1, 16 => 2, _ => 4 };
                let data = vec![0xAB; (length as usize) * unit];
                let req = ChangePropertyRequest {
                    mode,
                    window: ResourceId(window),
                    property: AtomId(property),
                    r#type: AtomId(r#type),
                    format,
                    data: data.clone(),
                    length,
                };
                let (header_data, body) = encode(&req);
                let parsed = change_property_request(header_data, &body).unwrap();
                prop_assert_eq!(parsed, req);
            }
        }

        #[test]
        fn invalid_format_passes_through_for_handler_to_reject() {
            let mut body = Vec::new();
            write_u32(ClientByteOrder::LittleEndian, &mut body, 0x100);
            write_u32(ClientByteOrder::LittleEndian, &mut body, 31);
            write_u32(ClientByteOrder::LittleEndian, &mut body, 31);
            body.push(7); // invalid format byte
            body.extend_from_slice(&[0; 3]);
            write_u32(ClientByteOrder::LittleEndian, &mut body, 0); // length = 0
            let req = change_property_request(0, &body)
                .expect("parser should pass through invalid format");
            assert_eq!(req.format, 7);
            assert_eq!(req.length, 0);
        }
    }

    mod delete_property_tests {
        use super::*;
        use proptest::prelude::*;
        proptest! {
            #[test]
            fn round_trip(window in any::<u32>(), property in any::<u32>()) {
                let mut body = Vec::new();
                write_u32(ClientByteOrder::LittleEndian, &mut body, window);
                write_u32(ClientByteOrder::LittleEndian, &mut body, property);
                let req = delete_property_request(&body).unwrap();
                prop_assert_eq!(req, DeletePropertyRequest {
                    window: ResourceId(window), property: AtomId(property),
                });
            }
        }
    }

    mod render_reply_tests {
        use super::*;

        #[test]
        fn query_pict_index_values_empty_reply_shape() {
            let mut out = Vec::new();
            write_render_query_pict_index_values_reply(
                &mut out,
                ClientByteOrder::LittleEndian,
                SequenceNumber(0x1234),
            )
            .unwrap();

            assert_eq!(out.len(), 32);
            assert_eq!(out[0], 1);
            assert_eq!(out[1], 0);
            assert_eq!(&out[2..4], &0x1234u16.to_le_bytes());
            assert_eq!(&out[4..8], &0u32.to_le_bytes());
            assert_eq!(&out[8..12], &0u32.to_le_bytes());
            assert!(out[12..].iter().all(|b| *b == 0));
        }

        #[test]
        fn query_filters_advertises_standard_xorg_set() {
            // Mirrors X.Org's render.c QueryFilters encoding: three
            // canonical filters (nearest/bilinear/convolution) with
            // FilterAliasNone (0xFFFF) alias slots, then three aliases
            // (fast→nearest, good→bilinear, best→bilinear) with their
            // canonical filter's INDEX in the alias slot.
            let mut out = Vec::new();
            write_render_query_filters_reply(
                &mut out,
                ClientByteOrder::LittleEndian,
                SequenceNumber(0x1234),
            )
            .unwrap();

            // 32-byte header + 12-byte alias array (6 × u16) + 44-byte
            // names list (5+5+5+8+9+12 names, no trailing pad needed).
            assert_eq!(out.len(), 32 + 12 + 44);
            assert_eq!(out[0], 1);
            assert_eq!(out[1], 0);
            assert_eq!(&out[2..4], &0x1234u16.to_le_bytes());
            // length_words = (12 + 44) / 4 = 14
            assert_eq!(&out[4..8], &14u32.to_le_bytes());
            // num_aliases = num_filters = 6
            assert_eq!(&out[8..12], &6u32.to_le_bytes());
            assert_eq!(&out[12..16], &6u32.to_le_bytes());
            assert!(out[16..32].iter().all(|b| *b == 0));

            // aliases[0..3] = FilterAliasNone for the canonical filters.
            assert_eq!(&out[32..34], &0xFFFFu16.to_le_bytes());
            assert_eq!(&out[34..36], &0xFFFFu16.to_le_bytes());
            assert_eq!(&out[36..38], &0xFFFFu16.to_le_bytes());
            // aliases[3..6]: fast→0 (nearest), good→1 (bilinear), best→1.
            assert_eq!(&out[38..40], &0u16.to_le_bytes());
            assert_eq!(&out[40..42], &1u16.to_le_bytes());
            assert_eq!(&out[42..44], &1u16.to_le_bytes());

            // Name list: canonical first, then aliases.
            let names = &out[44..];
            assert_eq!(names[0], 7);
            assert_eq!(&names[1..8], b"nearest");
            assert_eq!(names[8], 8);
            assert_eq!(&names[9..17], b"bilinear");
            assert_eq!(names[17], 11);
            assert_eq!(&names[18..29], b"convolution");
            assert_eq!(names[29], 4);
            assert_eq!(&names[30..34], b"fast");
            assert_eq!(names[34], 4);
            assert_eq!(&names[35..39], b"good");
            assert_eq!(names[39], 4);
            assert_eq!(&names[40..44], b"best");
        }
    }

    mod get_property_tests {
        use super::*;
        use proptest::prelude::*;
        proptest! {
            #[test]
            fn round_trip(
                delete: bool,
                window in any::<u32>(),
                property in any::<u32>(),
                r#type in any::<u32>(),
                long_offset in any::<u32>(),
                long_length in any::<u32>(),
            ) {
                let mut body = Vec::new();
                write_u32(ClientByteOrder::LittleEndian, &mut body, window);
                write_u32(ClientByteOrder::LittleEndian, &mut body, property);
                write_u32(ClientByteOrder::LittleEndian, &mut body, r#type);
                write_u32(ClientByteOrder::LittleEndian, &mut body, long_offset);
                write_u32(ClientByteOrder::LittleEndian, &mut body, long_length);
                let req = get_property_request(if delete { 1 } else { 0 }, &body).unwrap();
                prop_assert_eq!(req, GetPropertyRequest {
                    delete,
                    window: ResourceId(window),
                    property: AtomId(property),
                    r#type: AtomId(r#type),
                    long_offset,
                    long_length,
                });
            }
        }
    }

    mod get_property_reply_tests {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn shape(
                format_choice in 0u8..3,
                r#type in any::<u32>(),
                bytes_after in any::<u32>(),
                len_units in 0u32..256,
            ) {
                let format = [8u8, 16, 32][format_choice as usize];
                let unit = match format { 8 => 1, 16 => 2, _ => 4 };
                let value: Vec<u8> = (0..len_units as usize * unit)
                    .map(|i| (i & 0xff) as u8)
                    .collect();
                let value_len = len_units;

                let mut buf = Vec::new();
                write_get_property_reply(
                    &mut buf,
                    ClientByteOrder::LittleEndian,
                    SequenceNumber(0xdead),
                    GetPropertyReply {
                        format,
                        r#type: AtomId(r#type),
                        bytes_after,
                        value_len,
                        value: &value,
                    },
                )
                .unwrap();

                let pad = (4 - value.len() % 4) % 4;
                let payload = value.len() + pad;
                prop_assert_eq!(buf.len(), 32 + payload);
                // wire length field (4..8) equals payload/4
                let wire_len = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
                prop_assert_eq!(wire_len as usize * 4, payload);
                // value_len field (16..20) is in format units
                let wire_value_len = u32::from_le_bytes([buf[16], buf[17], buf[18], buf[19]]);
                prop_assert_eq!(wire_value_len, value_len);
            }
        }
    }

    mod property_notify_tests {
        use super::*;
        #[test]
        fn shape() {
            let mut buf = Vec::new();
            encode_property_notify_event(
                &mut buf,
                SequenceNumber(0x1234),
                ClientByteOrder::LittleEndian,
                ResourceId(0x100002),
                AtomId(0x42),
                0xdead_beef,
                true,
            );
            assert_eq!(buf.len(), 32);
            assert_eq!(buf[0], 28);
            assert_eq!(&buf[2..4], &[0x34, 0x12]);
            assert_eq!(&buf[4..8], &0x100002u32.to_le_bytes());
            assert_eq!(&buf[8..12], &0x42u32.to_le_bytes());
            assert_eq!(&buf[12..16], &0xdead_beefu32.to_le_bytes());
            assert_eq!(buf[16], 1);
        }
    }

    mod destroy_notify_tests {
        use super::*;
        #[test]
        fn shape() {
            let mut buf = Vec::new();
            encode_destroy_notify_event(
                &mut buf,
                SequenceNumber(0x1234),
                ClientByteOrder::LittleEndian,
                ResourceId(0x100),
                ResourceId(0x100002),
            );
            assert_eq!(buf.len(), 32);
            assert_eq!(buf[0], 17);
            assert_eq!(&buf[4..8], &0x100u32.to_le_bytes());
            assert_eq!(&buf[8..12], &0x100002u32.to_le_bytes());
        }
    }

    mod unmap_notify_tests {
        use super::*;
        use proptest::prelude::*;
        #[test]
        fn shape() {
            let mut buf = Vec::new();
            encode_unmap_notify_event(
                &mut buf,
                SequenceNumber(0x1234),
                ClientByteOrder::LittleEndian,
                ResourceId(0x100),
                ResourceId(0x100002),
                false,
            );
            assert_eq!(buf.len(), 32);
            assert_eq!(buf[0], 18);
            assert_eq!(buf[1], 0);
            assert_eq!(&buf[2..4], &[0x34, 0x12]);
            assert_eq!(&buf[4..8], &0x100u32.to_le_bytes());
            assert_eq!(&buf[8..12], &0x100002u32.to_le_bytes());
            assert_eq!(buf[12], 0);
            assert!(buf[13..32].iter().all(|&b| b == 0));
        }

        proptest! {
            #[test]
            fn encoder_round_trip(
                sequence in any::<u16>(),
                event_window in any::<u32>(),
                window in any::<u32>(),
                from_configure: bool,
                big_endian: bool,
            ) {
                let order = if big_endian {
                    ClientByteOrder::BigEndian
                } else {
                    ClientByteOrder::LittleEndian
                };
                let mut buf = Vec::new();
                encode_unmap_notify_event(
                    &mut buf,
                    SequenceNumber(sequence),
                    order,
                    ResourceId(event_window),
                    ResourceId(window),
                    from_configure,
                );
                prop_assert_eq!(buf.len(), 32);
                prop_assert_eq!(buf[0], 18);
                prop_assert_eq!(buf[1], 0);

                let seq_bytes = if big_endian {
                    sequence.to_be_bytes()
                } else {
                    sequence.to_le_bytes()
                };
                prop_assert_eq!(&buf[2..4], &seq_bytes[..]);

                let ew_bytes = if big_endian {
                    event_window.to_be_bytes()
                } else {
                    event_window.to_le_bytes()
                };
                prop_assert_eq!(&buf[4..8], &ew_bytes[..]);

                let w_bytes = if big_endian {
                    window.to_be_bytes()
                } else {
                    window.to_le_bytes()
                };
                prop_assert_eq!(&buf[8..12], &w_bytes[..]);

                prop_assert_eq!(buf[12], u8::from(from_configure));
                prop_assert!(buf[13..32].iter().all(|&b| b == 0));
            }
        }
    }

    mod reparent_tests {
        use super::*;

        #[test]
        fn reparent_window_request_parses_all_fields() {
            let mut body = Vec::new();
            write_u32(ClientByteOrder::LittleEndian, &mut body, 0x100002);
            write_u32(ClientByteOrder::LittleEndian, &mut body, 0x100003);
            write_i16(ClientByteOrder::LittleEndian, &mut body, -10);
            write_i16(ClientByteOrder::LittleEndian, &mut body, 20);

            let req = reparent_window_request(&body).unwrap();
            assert_eq!(
                req,
                ReparentWindowRequest {
                    window: ResourceId(0x100002),
                    parent: ResourceId(0x100003),
                    x: -10,
                    y: 20,
                }
            );
        }

        #[test]
        fn reparent_window_request_rejects_short_body() {
            assert!(reparent_window_request(&[0; 11]).is_none());
        }

        #[test]
        fn reparent_notify_shape() {
            let mut buf = Vec::new();
            encode_reparent_notify_event(
                &mut buf,
                SequenceNumber(0x1234),
                ClientByteOrder::LittleEndian,
                ResourceId(0x100),
                ResourceId(0x100002),
                ResourceId(0x100003),
                -5,
                7,
                true,
            );

            assert_eq!(buf.len(), 32);
            assert_eq!(buf[0], 21);
            assert_eq!(&buf[2..4], &0x1234u16.to_le_bytes());
            assert_eq!(&buf[4..8], &0x100u32.to_le_bytes());
            assert_eq!(&buf[8..12], &0x100002u32.to_le_bytes());
            assert_eq!(&buf[12..16], &0x100003u32.to_le_bytes());
            assert_eq!(&buf[16..18], &(-5i16).to_le_bytes());
            assert_eq!(&buf[18..20], &7i16.to_le_bytes());
            assert_eq!(buf[20], 1);
            assert!(buf[21..].iter().all(|byte| *byte == 0));
        }
    }

    mod send_event_tests {
        use super::*;

        #[test]
        fn send_event_request_parses_payload() {
            let mut body = Vec::new();
            write_u32(ClientByteOrder::LittleEndian, &mut body, 0x100002);
            write_u32(ClientByteOrder::LittleEndian, &mut body, 0x00ff_0000);
            let event = [0xabu8; 32];
            body.extend_from_slice(&event);

            let req = send_event_request(1, &body).unwrap();
            assert!(req.propagate);
            assert_eq!(req.destination, ResourceId(0x100002));
            assert_eq!(req.event_mask, 0x00ff_0000);
            assert_eq!(req.event, &event);
        }

        #[test]
        fn send_event_request_rejects_short_body() {
            assert!(send_event_request(0, &[0; 39]).is_none());
        }

        #[test]
        fn client_message_encoder_shape() {
            let mut data = [0u8; 20];
            data[0] = 0xaa;
            data[19] = 0xbb;
            let mut buf = Vec::new();
            encode_client_message_event(
                &mut buf,
                ClientByteOrder::LittleEndian,
                ClientMessageEvent {
                    sequence: SequenceNumber(0x1234),
                    send_event: true,
                    format: 32,
                    window: ResourceId(0x100002),
                    r#type: AtomId(0x44),
                    data,
                },
            );

            assert_eq!(buf.len(), 32);
            assert_eq!(buf[0], 33 | 0x80);
            assert_eq!(buf[1], 32);
            assert_eq!(&buf[2..4], &0x1234u16.to_le_bytes());
            assert_eq!(&buf[4..8], &0x100002u32.to_le_bytes());
            assert_eq!(&buf[8..12], &0x44u32.to_le_bytes());
            assert_eq!(&buf[12..32], &data);
        }
    }

    mod pointer_event_tests {
        use super::*;
        use proptest::prelude::*;

        #[test]
        fn button_press_event_shape() {
            let mut buf = Vec::new();
            encode_button_press_event(
                &mut buf,
                ClientByteOrder::LittleEndian,
                PointerEvent {
                    sequence: SequenceNumber(0x1234),
                    detail: 1,
                    time: 0xdead_beef,
                    root: ResourceId(0x100),
                    event: ResourceId(0x0010_0002),
                    child: ResourceId(0),
                    root_x: 100,
                    root_y: 200,
                    event_x: 10,
                    event_y: 20,
                    state: 0x0010,
                },
            );
            assert_eq!(buf.len(), 32);
            assert_eq!(buf[0], 4); // ButtonPress
            assert_eq!(buf[1], 1); // detail
            assert_eq!(&buf[2..4], &0x1234u16.to_le_bytes());
            assert_eq!(&buf[4..8], &0xdead_beefu32.to_le_bytes());
            assert_eq!(&buf[8..12], &0x100u32.to_le_bytes());
            assert_eq!(&buf[12..16], &0x0010_0002u32.to_le_bytes());
            assert_eq!(&buf[16..20], &0u32.to_le_bytes()); // child = 0
            assert_eq!(&buf[20..22], &100i16.to_le_bytes());
            assert_eq!(&buf[22..24], &200i16.to_le_bytes());
            assert_eq!(&buf[24..26], &10i16.to_le_bytes());
            assert_eq!(&buf[26..28], &20i16.to_le_bytes());
            assert_eq!(&buf[28..30], &0x0010u16.to_le_bytes());
            assert_eq!(buf[30], 1); // same_screen
            assert_eq!(buf[31], 0); // pad
        }

        #[test]
        fn button_release_event_shape() {
            let mut buf = Vec::new();
            encode_button_release_event(
                &mut buf,
                ClientByteOrder::LittleEndian,
                PointerEvent {
                    sequence: SequenceNumber(0),
                    detail: 2,
                    time: 0,
                    root: ResourceId(0x100),
                    event: ResourceId(0x0010_0002),
                    child: ResourceId(0),
                    root_x: 0,
                    root_y: 0,
                    event_x: 0,
                    event_y: 0,
                    state: 0,
                },
            );
            assert_eq!(buf.len(), 32);
            assert_eq!(buf[0], 5); // ButtonRelease
            assert_eq!(buf[1], 2); // detail
            assert_eq!(buf[30], 1); // same_screen
        }

        #[test]
        fn motion_notify_event_shape() {
            let mut buf = Vec::new();
            encode_motion_notify_event(
                &mut buf,
                ClientByteOrder::LittleEndian,
                PointerEvent {
                    sequence: SequenceNumber(0),
                    detail: 0,
                    time: 0,
                    root: ResourceId(0x100),
                    event: ResourceId(0x0010_0002),
                    child: ResourceId(0),
                    root_x: 0,
                    root_y: 0,
                    event_x: 0,
                    event_y: 0,
                    state: 0,
                },
            );
            assert_eq!(buf.len(), 32);
            assert_eq!(buf[0], 6); // MotionNotify
            assert_eq!(buf[1], 0); // detail = 0 for motion
            assert_eq!(buf[30], 1); // same_screen
        }

        #[test]
        fn enter_notify_event_shape() {
            let mut buf = Vec::new();
            encode_enter_notify_event(
                &mut buf,
                ClientByteOrder::LittleEndian,
                CrossingEvent {
                    sequence: SequenceNumber(0x1234),
                    time: 0xdead_beef,
                    root: ResourceId(0x100),
                    event: ResourceId(0x0010_0002),
                    child: ResourceId(0x0010_0042),
                    root_x: 100,
                    root_y: 200,
                    event_x: 10,
                    event_y: 20,
                    state: 0,
                    detail: 0,
                    mode: 0,
                },
            );
            assert_eq!(buf.len(), 32);
            assert_eq!(buf[0], 7); // EnterNotify
            assert_eq!(buf[1], 0); // detail = NotifyAncestor
            assert_eq!(&buf[2..4], &0x1234u16.to_le_bytes());
            assert_eq!(&buf[4..8], &0xdead_beefu32.to_le_bytes());
            assert_eq!(&buf[8..12], &0x100u32.to_le_bytes());
            assert_eq!(&buf[12..16], &0x0010_0002u32.to_le_bytes());
            assert_eq!(&buf[16..20], &0x0010_0042u32.to_le_bytes()); // child
            assert_eq!(&buf[20..22], &100i16.to_le_bytes());
            assert_eq!(&buf[22..24], &200i16.to_le_bytes());
            assert_eq!(&buf[24..26], &10i16.to_le_bytes());
            assert_eq!(&buf[26..28], &20i16.to_le_bytes());
            assert_eq!(&buf[28..30], &0u16.to_le_bytes());
            assert_eq!(buf[30], 0); // mode = NotifyNormal
            assert_eq!(buf[31], 0x03); // same_screen,focus = 0x01 | 0x02
        }

        #[test]
        fn leave_notify_event_shape() {
            let mut buf = Vec::new();
            encode_leave_notify_event(
                &mut buf,
                ClientByteOrder::LittleEndian,
                CrossingEvent {
                    sequence: SequenceNumber(0),
                    time: 0,
                    root: ResourceId(0x100),
                    event: ResourceId(0x0010_0002),
                    child: ResourceId(0),
                    root_x: 0,
                    root_y: 0,
                    event_x: 0,
                    event_y: 0,
                    state: 0,
                    detail: 0,
                    mode: 0,
                },
            );
            assert_eq!(buf.len(), 32);
            assert_eq!(buf[0], 8); // LeaveNotify
            assert_eq!(buf[1], 0); // detail = NotifyAncestor
            assert_eq!(buf[30], 0); // mode = NotifyNormal
            assert_eq!(buf[31], 0x03); // same_screen,focus
        }

        proptest! {
            #[test]
            fn pointer_encoder_round_trip(
                sequence in any::<u16>(),
                detail in any::<u8>(),
                time in any::<u32>(),
                root in any::<u32>(),
                event_window in any::<u32>(),
                child_xid in any::<u32>(),
                root_x in any::<i16>(),
                root_y in any::<i16>(),
                event_x in any::<i16>(),
                event_y in any::<i16>(),
                state in any::<u16>(),
                big_endian: bool,
                encoder_choice in 0u8..5,
            ) {
                let order = if big_endian {
                    ClientByteOrder::BigEndian
                } else {
                    ClientByteOrder::LittleEndian
                };
                let mut buf = Vec::new();
                let expected_code: u8;
                let expected_detail: u8;
                let expected_state_offset: usize = 28;
                match encoder_choice {
                    0 => {
                        expected_code = 4;
                        expected_detail = detail;
                        encode_button_press_event(
                            &mut buf,
                            order,
                            PointerEvent {
                                sequence: SequenceNumber(sequence),
                                detail,
                                time,
                                root: ResourceId(root),
                                event: ResourceId(event_window),
                                root_x,
                                root_y,
                                event_x,
                                event_y,
                                child: ResourceId(child_xid),
                                state,
                            },
                        );
                    }
                    1 => {
                        expected_code = 5;
                        expected_detail = detail;
                        encode_button_release_event(
                            &mut buf,
                            order,
                            PointerEvent {
                                sequence: SequenceNumber(sequence),
                                detail,
                                time,
                                root: ResourceId(root),
                                event: ResourceId(event_window),
                                root_x,
                                root_y,
                                event_x,
                                event_y,
                                child: ResourceId(child_xid),
                                state,
                            },
                        );
                    }
                    2 => {
                        expected_code = 6;
                        expected_detail = detail;
                        encode_motion_notify_event(
                            &mut buf,
                            order,
                            PointerEvent {
                                sequence: SequenceNumber(sequence),
                                detail,
                                time,
                                root: ResourceId(root),
                                event: ResourceId(event_window),
                                root_x,
                                root_y,
                                event_x,
                                event_y,
                                child: ResourceId(child_xid),
                                state,
                            },
                        );
                    }
                    3 => {
                        expected_code = 7;
                        expected_detail = 0;
                        encode_enter_notify_event(
                            &mut buf,
                            order,
                            CrossingEvent {
                                sequence: SequenceNumber(sequence),
                                time,
                                root: ResourceId(root),
                                event: ResourceId(event_window),
                                child: ResourceId(child_xid),
                                root_x,
                                root_y,
                                event_x,
                                event_y,
                                state,
                                detail: 0,
                                mode: 0,
                            },
                        );
                    }
                    _ => {
                        expected_code = 8;
                        expected_detail = 0;
                        encode_leave_notify_event(
                            &mut buf,
                            order,
                            CrossingEvent {
                                sequence: SequenceNumber(sequence),
                                time,
                                root: ResourceId(root),
                                event: ResourceId(event_window),
                                child: ResourceId(child_xid),
                                root_x,
                                root_y,
                                event_x,
                                event_y,
                                state,
                                detail: 0,
                                mode: 0,
                            },
                        );
                    }
                }

                prop_assert_eq!(buf.len(), 32);
                prop_assert_eq!(buf[0], expected_code);
                prop_assert_eq!(buf[1], expected_detail);

                let seq_bytes = if big_endian {
                    sequence.to_be_bytes()
                } else {
                    sequence.to_le_bytes()
                };
                prop_assert_eq!(&buf[2..4], &seq_bytes[..]);

                let time_bytes = if big_endian { time.to_be_bytes() } else { time.to_le_bytes() };
                prop_assert_eq!(&buf[4..8], &time_bytes[..]);

                let root_bytes = if big_endian { root.to_be_bytes() } else { root.to_le_bytes() };
                prop_assert_eq!(&buf[8..12], &root_bytes[..]);

                let event_bytes = if big_endian { event_window.to_be_bytes() } else { event_window.to_le_bytes() };
                prop_assert_eq!(&buf[12..16], &event_bytes[..]);

                let child_bytes = if big_endian {
                    child_xid.to_be_bytes()
                } else {
                    child_xid.to_le_bytes()
                };
                prop_assert_eq!(&buf[16..20], &child_bytes[..]);

                let rx = if big_endian { root_x.to_be_bytes() } else { root_x.to_le_bytes() };
                prop_assert_eq!(&buf[20..22], &rx[..]);
                let ry = if big_endian { root_y.to_be_bytes() } else { root_y.to_le_bytes() };
                prop_assert_eq!(&buf[22..24], &ry[..]);
                let ex = if big_endian { event_x.to_be_bytes() } else { event_x.to_le_bytes() };
                prop_assert_eq!(&buf[24..26], &ex[..]);
                let ey = if big_endian { event_y.to_be_bytes() } else { event_y.to_le_bytes() };
                prop_assert_eq!(&buf[26..28], &ey[..]);

                let state_bytes = if big_endian { state.to_be_bytes() } else { state.to_le_bytes() };
                prop_assert_eq!(&buf[expected_state_offset..expected_state_offset + 2], &state_bytes[..]);

                match expected_code {
                    4..=6 => {
                        prop_assert_eq!(buf[30], 1); // same_screen
                        prop_assert_eq!(buf[31], 0); // pad
                    }
                    7 | 8 => {
                        prop_assert_eq!(buf[30], 0); // mode = NotifyNormal
                        prop_assert_eq!(buf[31], 0x03); // same_screen + focus
                    }
                    _ => unreachable!(),
                }
            }
        }
    }

    mod copy_area_tests {
        use super::*;

        #[test]
        fn all_fields_parse_correctly() {
            let mut body = Vec::new();
            write_u32(ClientByteOrder::LittleEndian, &mut body, 0x11111111);
            write_u32(ClientByteOrder::LittleEndian, &mut body, 0x22222222);
            write_u32(ClientByteOrder::LittleEndian, &mut body, 0x33333333);
            write_i16(ClientByteOrder::LittleEndian, &mut body, 100);
            write_i16(ClientByteOrder::LittleEndian, &mut body, 200);
            write_i16(ClientByteOrder::LittleEndian, &mut body, 300);
            write_i16(ClientByteOrder::LittleEndian, &mut body, 400);
            write_u16(ClientByteOrder::LittleEndian, &mut body, 500);
            write_u16(ClientByteOrder::LittleEndian, &mut body, 600);

            let req = copy_area_request(&body).unwrap();
            assert_eq!(req.src, ResourceId(0x11111111));
            assert_eq!(req.dst, ResourceId(0x22222222));
            assert_eq!(req.gc, ResourceId(0x33333333));
            assert_eq!(req.src_x, 100);
            assert_eq!(req.src_y, 200);
            assert_eq!(req.dst_x, 300);
            assert_eq!(req.dst_y, 400);
            assert_eq!(req.width, 500);
            assert_eq!(req.height, 600);
        }

        #[test]
        fn short_body_returns_none() {
            let body = [0u8; 23]; // 1 byte short
            assert!(copy_area_request(&body).is_none());
        }
    }

    mod put_image_tests {
        use super::*;

        #[test]
        fn z_pixmap_parses_all_scalar_fields_and_preserves_data_slice() {
            let mut body = Vec::new();
            write_u32(ClientByteOrder::LittleEndian, &mut body, 0x12345678);
            write_u32(ClientByteOrder::LittleEndian, &mut body, 0x9abcdef0);
            write_u16(ClientByteOrder::LittleEndian, &mut body, 100);
            write_u16(ClientByteOrder::LittleEndian, &mut body, 200);
            write_i16(ClientByteOrder::LittleEndian, &mut body, 50);
            write_i16(ClientByteOrder::LittleEndian, &mut body, 75);
            body.push(5); // left_pad
            body.push(32); // depth
            body.extend_from_slice(&[0, 0]); // padding
            let data_slice = [0xAA, 0xBB, 0xCC, 0xDD];
            body.extend_from_slice(&data_slice);

            let req = put_image_request(2, &body).unwrap();
            assert_eq!(req.format, ImageFormat::ZPixmap);
            assert_eq!(req.drawable, ResourceId(0x12345678));
            assert_eq!(req.gc, ResourceId(0x9abcdef0));
            assert_eq!(req.width, 100);
            assert_eq!(req.height, 200);
            assert_eq!(req.dst_x, 50);
            assert_eq!(req.dst_y, 75);
            assert_eq!(req.left_pad, 5);
            assert_eq!(req.depth, 32);
            assert_eq!(req.data, &data_slice);
        }

        fn minimal_body() -> Vec<u8> {
            let mut body = Vec::new();
            write_u32(ClientByteOrder::LittleEndian, &mut body, 1);
            write_u32(ClientByteOrder::LittleEndian, &mut body, 2);
            write_u16(ClientByteOrder::LittleEndian, &mut body, 3);
            write_u16(ClientByteOrder::LittleEndian, &mut body, 4);
            write_i16(ClientByteOrder::LittleEndian, &mut body, 5);
            write_i16(ClientByteOrder::LittleEndian, &mut body, 6);
            body.push(7);
            body.push(8);
            body.extend_from_slice(&[0, 0]);
            body.push(0xAB);
            body
        }

        #[test]
        fn format_byte_0_maps_to_xy_bitmap() {
            let body = minimal_body();
            let req = put_image_request(0, &body).unwrap();
            assert_eq!(req.format, ImageFormat::XyBitmap);
        }

        #[test]
        fn format_byte_1_maps_to_xy_pixmap() {
            let body = minimal_body();
            let req = put_image_request(1, &body).unwrap();
            assert_eq!(req.format, ImageFormat::XyPixmap);
        }

        #[test]
        fn unknown_format_maps_to_unknown_value() {
            let body = minimal_body();
            let req = put_image_request(42, &body).unwrap();
            assert_eq!(req.format, ImageFormat::Unknown(42));
        }

        #[test]
        fn short_body_returns_none() {
            let body = [0u8; 19]; // 1 byte short of required 20 bytes
            assert!(put_image_request(2, &body).is_none());
        }
    }

    mod phase2_keyboard_tests {
        use super::*;

        #[test]
        fn parse_grab_key_request_basic() {
            let body = [
                0x12, 0x34, 0x00, 0x00, // grab_window 0x3412
                0x40, 0x00, // modifiers 0x0040
                24,   // keycode 24
                1,    // pointer_mode async
                1,    // keyboard_mode async
                0, 0, 0, // pad
            ];
            let parsed = parse_grab_key(&body, false).unwrap();
            assert_eq!(parsed.grab_window, 0x3412);
            assert_eq!(parsed.modifiers, 0x0040);
            assert_eq!(parsed.keycode, 24);
            assert_eq!(parsed.pointer_mode, 1);
            assert_eq!(parsed.keyboard_mode, 1);
            assert!(!parsed.owner_events);
        }

        #[test]
        fn parse_ungrab_key_request_basic() {
            let body = [0x12, 0x34, 0x00, 0x00, 0x40, 0x00, 0, 0];
            let parsed = parse_ungrab_key(&body, 24).unwrap();
            assert_eq!(parsed.grab_window, 0x3412);
            assert_eq!(parsed.keycode, 24);
            assert_eq!(parsed.modifiers, 0x0040);
        }

        #[test]
        fn mapping_notify_event_layout() {
            let mut buf = Vec::new();
            write_mapping_notify_event(
                &mut buf,
                ClientByteOrder::LittleEndian,
                SequenceNumber(0),
                1,
                8,
                248,
            )
            .unwrap();
            assert_eq!(buf.len(), 32);
            assert_eq!(buf[0], 34);
            assert_eq!(buf[4], 1);
            assert_eq!(buf[5], 8);
            assert_eq!(buf[6], 248);
        }

        #[test]
        fn circulate_notify_event_layout() {
            let mut buf = Vec::new();
            write_circulate_notify_event(
                &mut buf,
                ClientByteOrder::LittleEndian,
                SequenceNumber(0),
                ResourceId(0x100),
                ResourceId(0x200),
                0,
            )
            .unwrap();
            assert_eq!(buf.len(), 32);
            assert_eq!(buf[0], 26);
            assert_eq!(u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]), 0x100);
            assert_eq!(
                u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]),
                0x200
            );
            assert_eq!(buf[16], 0);
        }

        #[test]
        fn circulate_request_event_layout() {
            let mut buf = Vec::new();
            write_circulate_request_event(
                &mut buf,
                ClientByteOrder::LittleEndian,
                SequenceNumber(0),
                ResourceId(0x100),
                ResourceId(0x200),
                1,
            )
            .unwrap();
            assert_eq!(buf[0], 27);
            assert_eq!(buf[16], 1);
        }

        #[test]
        fn keyboard_mapping_reply_from_keysyms_layout() {
            let keysyms: &[u32] = &[0x71, 0x51, 0, 0, 0x77, 0x57, 0, 0];
            let mut buf = Vec::new();
            write_get_keyboard_mapping_reply_from_keysyms(
                &mut buf,
                ClientByteOrder::LittleEndian,
                SequenceNumber(7),
                4,
                keysyms,
            )
            .unwrap();
            assert_eq!(buf[0], 1);
            assert_eq!(buf[1], 4);
            assert_eq!(u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]), 8);
            assert_eq!(buf.len(), 32 + 8 * 4);
            assert_eq!(
                u32::from_le_bytes([buf[32], buf[33], buf[34], buf[35]]),
                0x71
            );
        }

        #[test]
        fn modifier_mapping_reply_layout_kpm_2() {
            let kpm = 2u8;
            let kc: Vec<u8> = (0..(8 * kpm)).map(|i| i + 8).collect();
            let mut buf = Vec::new();
            write_get_modifier_mapping_reply_with_keycodes(
                &mut buf,
                ClientByteOrder::LittleEndian,
                SequenceNumber(3),
                kpm,
                &kc,
            )
            .unwrap();
            assert_eq!(buf[0], 1);
            assert_eq!(buf[1], kpm);
            let length = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
            assert_eq!(length, (8 * u32::from(kpm)) / 4);
            assert_eq!(&buf[32..32 + 8 * kpm as usize], &kc[..]);
        }

        #[test]
        fn modifier_mapping_reply_layout_kpm_4() {
            let kpm = 4u8;
            let kc: Vec<u8> = (0..(8 * kpm)).map(|i| i + 8).collect();
            let mut buf = Vec::new();
            write_get_modifier_mapping_reply_with_keycodes(
                &mut buf,
                ClientByteOrder::LittleEndian,
                SequenceNumber(3),
                kpm,
                &kc,
            )
            .unwrap();
            let length = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
            assert_eq!(length, (8 * u32::from(kpm)) / 4);
            assert_eq!(&buf[32..32 + 8 * kpm as usize], &kc[..]);
        }

        /// Walk the XI 1.x `ListInputDevices` reply byte-for-byte and
        /// assert it decodes to the 4-device model with consistent
        /// class/name framing. Guards against the empty-stub regression
        /// that crashed Chromium/Electron (it cross-checks this reply
        /// against XIQueryDevice).
        #[test]
        fn list_input_devices_reply_decodes_to_four_devices() {
            let le = ClientByteOrder::LittleEndian;
            // Simulated type atoms: MOUSE=69, KEYBOARD=70, TOUCHPAD=71
            // (the exact values are allocated at ServerState::with_geometry;
            // here we pass them explicitly to keep the test self-contained).
            const MOUSE: u32 = 69;
            const KEYBOARD: u32 = 70;
            let buf = encode_list_input_devices_reply(
                le,
                SequenceNumber(0x1234),
                [
                    "Virtual core pointer",
                    "Virtual core keyboard",
                    "Virtual core slave pointer",
                    "Virtual core slave keyboard",
                ],
                [MOUSE, KEYBOARD, MOUSE, KEYBOARD],
            );

            // Header. Per XIproto.h, ndevices is at byte 8 — NOT the
            // standard reply "data" byte (byte 1). Byte 1 is RepType,
            // which clients ignore; a regression that put ndevices at
            // byte 1 made every client read 0 devices from byte 8.
            assert_eq!(buf[0], 1, "reply type");
            assert_eq!(u16::from_le_bytes([buf[2], buf[3]]), 0x1234, "sequence");
            let length = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
            assert_eq!(buf.len(), 32 + length * 4, "length covers data, 4-aligned");
            let ndevices = buf[8];
            assert_eq!(ndevices, 4, "ndevices at byte 8");

            // Device-info array (8B each), immediately after the 32B header.
            let mut off = 32;
            let mut dev = Vec::new();
            let expected_types = [MOUSE, KEYBOARD, MOUSE, KEYBOARD];
            for &expected_type in &expected_types {
                let type_atom =
                    u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]]);
                let id = buf[off + 4];
                let num_classes = buf[off + 5];
                let use_code = buf[off + 6];
                assert_eq!(
                    type_atom, expected_type,
                    "type ATOM for device {id} must match passed value"
                );
                assert_eq!(buf[off + 7], 0, "device-info pad");
                dev.push((id, num_classes, use_code));
                off += 8;
            }
            assert_eq!(
                dev,
                vec![(2, 2, 0), (3, 1, 1), (4, 2, 4), (5, 1, 3)],
                "(id, num_classes, use) per device matches XIQueryDevice model",
            );

            // Class-info blocks, per device, walked by the BYTE length
            // field (XI 1.x semantics, not 4-byte units).
            for &(id, num_classes, _) in &dev {
                for _ in 0..num_classes {
                    let class = buf[off];
                    let len = buf[off + 1] as usize;
                    assert!(len >= 4 && off + len <= buf.len(), "class len in range");
                    match class {
                        0 => {
                            // KeyClass: min, max, num_keys.
                            assert_eq!(len, 8);
                            assert_eq!(buf[off + 2], 8, "min keycode");
                            assert_eq!(buf[off + 3], 255, "max keycode");
                            assert_eq!(
                                u16::from_le_bytes([buf[off + 4], buf[off + 5]]),
                                248,
                                "num_keys",
                            );
                        }
                        1 => {
                            // ButtonClass: num_buttons.
                            assert_eq!(len, 4);
                            assert_eq!(
                                u16::from_le_bytes([buf[off + 2], buf[off + 3]]),
                                7,
                                "num_buttons",
                            );
                        }
                        2 => {
                            // ValuatorClass: num_axes, mode, 8 + 12n bytes.
                            let num_axes = buf[off + 2] as usize;
                            let mode = buf[off + 3];
                            assert_eq!(len, 8 + 12 * num_axes, "valuator byte length");
                            assert_eq!(num_axes, 4, "X, Y, vscroll, hscroll");
                            assert_eq!(mode, 0, "mode = Relative");
                        }
                        other => panic!("unexpected class id {other} for device {id}"),
                    }
                    off += len;
                }
            }

            // Name STR list: 1 length byte + bytes, in device order.
            let names = [
                "Virtual core pointer",
                "Virtual core keyboard",
                "Virtual core slave pointer",
                "Virtual core slave keyboard",
            ];
            for name in names {
                let n = buf[off] as usize;
                assert_eq!(n, name.len(), "name length byte");
                assert_eq!(&buf[off + 1..off + 1 + n], name.as_bytes(), "name bytes");
                off += 1 + n;
            }
            // After the names: one trailing NUL (Xorg parity) plus up
            // to 3 pad bytes, all zero. The per-name length bytes fully
            // describe the names, so a correct parser never reads here.
            assert!(buf.len() - off <= 4, "only trailing NUL + pad remains");
            assert!(
                buf[off..].iter().all(|&b| b == 0),
                "trailing bytes are zero"
            );
        }

        /// A seeded touchpad renames the slave pointer (device 4); the
        /// XI1 reply must carry that name in the STR list (and the
        /// length byte must match), proving names thread through from
        /// the registry rather than being hardcoded.
        #[test]
        fn list_input_devices_reply_reflects_seeded_slave_pointer_name() {
            let le = ClientByteOrder::LittleEndian;
            let touchpad = "SynPS/2 Synaptics TouchPad";
            let buf = encode_list_input_devices_reply(
                le,
                SequenceNumber(1),
                [
                    "Virtual core pointer",
                    "Virtual core keyboard",
                    touchpad,
                    "Virtual core slave keyboard",
                ],
                [69, 70, 71, 70], // MOUSE=69, KBD=70, TOUCHPAD=71
            );
            let ndevices = buf[8] as usize;
            assert_eq!(ndevices, 4);

            // Skip the device-info array.
            let mut off = 32 + 8 * ndevices;
            // Skip the class-info blocks (walk by byte length).
            for &num_classes in &[2u8, 1, 2, 1] {
                for _ in 0..num_classes {
                    let len = buf[off + 1] as usize;
                    off += len;
                }
            }
            // Name STR list, device order: the 3rd name is the touchpad.
            let expected = [
                "Virtual core pointer",
                "Virtual core keyboard",
                touchpad,
                "Virtual core slave keyboard",
            ];
            for name in expected {
                let n = buf[off] as usize;
                assert_eq!(n, name.len(), "name length byte for {name}");
                assert_eq!(&buf[off + 1..off + 1 + n], name.as_bytes());
                off += 1 + n;
            }
        }
    }
}

// RENDER extension protocol: ynest format IDs
pub const RENDER_FMT_A1: u32 = 1;
pub const RENDER_FMT_A8: u32 = 2;
pub const RENDER_FMT_RGB24: u32 = 3;
pub const RENDER_FMT_ARGB32: u32 = 4;
/// Audit #4 (2026-05-19): depth-32 r8g8b8 picture format with
/// `alpha_mask=0`. Lets clients sample a depth-32 storage as
/// fully opaque without depending on the storage's padding-alpha
/// bytes (which start as 0 and would otherwise leak into the
/// composite as α=0 → invisible). Mirrors Xorg's `PICT_x8r8g8b8`.
pub const RENDER_FMT_XRGB32: u32 = 5;

pub struct RenderCreatePictureRequest {
    pub picture: ResourceId,
    pub drawable: ResourceId,
    pub format: u32,
    pub value_mask: u32,
    pub values: Vec<u8>,
}

pub struct RenderCompositeGlyphsRequest {
    pub op: u8,
    pub src: ResourceId,
    pub dst: ResourceId,
    pub mask_format: u32,
    pub glyphset: ResourceId,
    pub src_x: i16,
    pub src_y: i16,
    pub items: Vec<u8>,
}

pub struct RenderFillRectanglesRequest {
    pub op: u8,
    pub dst: ResourceId,
    pub color: [u8; 8],
    pub rects: Vec<u8>,
}

pub struct RenderCompositeRequest {
    pub op: u8,
    pub src: ResourceId,
    pub mask: ResourceId,
    pub dst: ResourceId,
    pub src_x: i16,
    pub src_y: i16,
    pub mask_x: i16,
    pub mask_y: i16,
    pub dst_x: i16,
    pub dst_y: i16,
    pub width: u16,
    pub height: u16,
}

pub fn render_create_picture_request(body: &[u8]) -> Option<RenderCreatePictureRequest> {
    if body.len() < 16 {
        return None;
    }
    let picture = ResourceId(read_u32_le(body.get(0..4)?));
    let drawable = ResourceId(read_u32_le(body.get(4..8)?));
    let format = read_u32_le(body.get(8..12)?);
    let value_mask = read_u32_le(body.get(12..16)?);
    let values = body.get(16..).unwrap_or(&[]).to_vec();
    Some(RenderCreatePictureRequest {
        picture,
        drawable,
        format,
        value_mask,
        values,
    })
}

pub fn render_free_resource_id(body: &[u8]) -> Option<ResourceId> {
    Some(ResourceId(read_u32_le(body.get(0..4)?)))
}

pub fn render_create_glyphset_request(body: &[u8]) -> Option<(ResourceId, u32)> {
    if body.len() < 8 {
        return None;
    }
    let gs = ResourceId(read_u32_le(body.get(0..4)?));
    let fmt = read_u32_le(body.get(4..8)?);
    Some((gs, fmt))
}

pub fn render_reference_glyphset_request(body: &[u8]) -> Option<(ResourceId, ResourceId)> {
    if body.len() < 8 {
        return None;
    }
    let new_glyphset = ResourceId(read_u32_le(body.get(0..4)?));
    let existing = ResourceId(read_u32_le(body.get(4..8)?));
    Some((new_glyphset, existing))
}

pub fn render_add_glyphs_request(body: &[u8]) -> Option<(ResourceId, Vec<u8>)> {
    if body.len() < 8 {
        return None;
    }
    let gs = ResourceId(read_u32_le(body.get(0..4)?));
    let tail = body.get(4..).unwrap_or(&[]).to_vec();
    Some((gs, tail))
}

pub fn render_free_glyphs_request(body: &[u8]) -> Option<(ResourceId, Vec<u8>)> {
    if body.len() < 4 {
        return None;
    }
    let gs = ResourceId(read_u32_le(body.get(0..4)?));
    let glyph_ids = body.get(4..).unwrap_or(&[]).to_vec();
    Some((gs, glyph_ids))
}

pub fn render_composite_glyphs_request(body: &[u8]) -> Option<RenderCompositeGlyphsRequest> {
    if body.len() < 24 {
        return None;
    }
    let op = body[0];
    let src = ResourceId(read_u32_le(body.get(4..8)?));
    let dst = ResourceId(read_u32_le(body.get(8..12)?));
    let mask_format = read_u32_le(body.get(12..16)?);
    let glyphset = ResourceId(read_u32_le(body.get(16..20)?));
    let src_x = i16::from_le_bytes(body.get(20..22)?.try_into().ok()?);
    let src_y = i16::from_le_bytes(body.get(22..24)?.try_into().ok()?);
    let items = body.get(24..).unwrap_or(&[]).to_vec();
    Some(RenderCompositeGlyphsRequest {
        op,
        src,
        dst,
        mask_format,
        glyphset,
        src_x,
        src_y,
        items,
    })
}

pub fn render_fill_rectangles_request(body: &[u8]) -> Option<RenderFillRectanglesRequest> {
    if body.len() < 16 {
        return None;
    }
    let op = body[0];
    let dst = ResourceId(read_u32_le(body.get(4..8)?));
    let color: [u8; 8] = body.get(8..16)?.try_into().ok()?;
    let rects = body.get(16..).unwrap_or(&[]).to_vec();
    Some(RenderFillRectanglesRequest {
        op,
        dst,
        color,
        rects,
    })
}

pub fn render_create_solid_fill_request(body: &[u8]) -> Option<(ResourceId, [u8; 8])> {
    if body.len() < 12 {
        return None;
    }
    let picture = ResourceId(read_u32_le(body.get(0..4)?));
    let color: [u8; 8] = body.get(4..12)?.try_into().ok()?;
    Some((picture, color))
}

pub fn render_composite_request(body: &[u8]) -> Option<RenderCompositeRequest> {
    // op(1) + pad(3) + src(4) + mask(4) + dst(4) + src_xy(4) + mask_xy(4)
    // + dst_xy(4) + size(4) = 32 bytes after the 4-byte request header.
    if body.len() < 32 {
        return None;
    }
    let op = body[0];
    let src = ResourceId(read_u32_le(body.get(4..8)?));
    let mask = ResourceId(read_u32_le(body.get(8..12)?));
    let dst = ResourceId(read_u32_le(body.get(12..16)?));
    let src_x = i16::from_le_bytes(body.get(16..18)?.try_into().ok()?);
    let src_y = i16::from_le_bytes(body.get(18..20)?.try_into().ok()?);
    let mask_x = i16::from_le_bytes(body.get(20..22)?.try_into().ok()?);
    let mask_y = i16::from_le_bytes(body.get(22..24)?.try_into().ok()?);
    let dst_x = i16::from_le_bytes(body.get(24..26)?.try_into().ok()?);
    let dst_y = i16::from_le_bytes(body.get(26..28)?.try_into().ok()?);
    let width = u16::from_le_bytes(body.get(28..30)?.try_into().ok()?);
    let height = u16::from_le_bytes(body.get(30..32)?.try_into().ok()?);
    Some(RenderCompositeRequest {
        op,
        src,
        mask,
        dst,
        src_x,
        src_y,
        mask_x,
        mask_y,
        dst_x,
        dst_y,
        width,
        height,
    })
}

/// Write QueryPictFormats reply. Advertises 5 picture formats (A1,
/// A8, X8R8G8B8, A8R8G8B8, **X8R8G8B8 at depth-32**) and maps the
/// root visual (depth 24) to X8R8G8B8 and the ARGB visual (depth 32)
/// to BOTH A8R8G8B8 (primary, with alpha) and X8R8G8B8-at-depth-32
/// (alternate, no alpha — `RENDER_FMT_XRGB32`). The alternate lets
/// compositors and Cairo wrap a depth-32 storage as opaque without
/// depending on the storage's padding-alpha bytes — see audit #4.
pub fn write_render_query_pict_formats_reply(
    writer: &mut impl Write,
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    root_visual: ResourceId,
    argb_visual: ResourceId,
) -> io::Result<()> {
    // 5 formats × 28 bytes = 140 bytes
    // 1 screen: nDepth(4) + fallback(4) + depth24-section(8 + 8 = 16) +
    //   depth32-section(8 header + 2 × 8 visual-format pairs = 24) = 48 bytes
    // Total body = 188 bytes = 47 × 4-byte units
    let num_formats: u32 = 5;
    let num_screens: u32 = 1;
    let num_depths: u32 = 2;
    // Total (visual,format) pairs across all depths: depth-24 has 1,
    // depth-32 has 2 (ARGB32 + XRGB32-depth32). Sum = 3.
    let num_visuals: u32 = 3;
    let body_units: u32 = (140 + 48) / 4; // 47

    let mut out = Vec::new();
    out.push(1u8); // Reply
    out.push(0u8); // unused
    write_u16(byte_order, &mut out, sequence.0);
    write_u32(byte_order, &mut out, body_units);
    write_u32(byte_order, &mut out, num_formats);
    write_u32(byte_order, &mut out, num_screens);
    write_u32(byte_order, &mut out, num_depths);
    write_u32(byte_order, &mut out, num_visuals);
    write_u32(byte_order, &mut out, 0); // num_subpixel
    write_u32(byte_order, &mut out, 0); // pad

    // Format 1: A1 (depth=1, alpha only)
    write_u32(byte_order, &mut out, RENDER_FMT_A1);
    out.push(1); // type=Direct
    out.push(1); // depth=1
    out.extend_from_slice(&[0, 0]); // pad
    write_u16(byte_order, &mut out, 0); // red-shift
    write_u16(byte_order, &mut out, 0); // red-mask
    write_u16(byte_order, &mut out, 0); // green-shift
    write_u16(byte_order, &mut out, 0); // green-mask
    write_u16(byte_order, &mut out, 0); // blue-shift
    write_u16(byte_order, &mut out, 0); // blue-mask
    write_u16(byte_order, &mut out, 0); // alpha-shift
    write_u16(byte_order, &mut out, 1); // alpha-mask
    write_u32(byte_order, &mut out, 0); // colormap

    // Format 2: A8 (depth=8, alpha only)
    write_u32(byte_order, &mut out, RENDER_FMT_A8);
    out.push(1); // type=Direct
    out.push(8); // depth=8
    out.extend_from_slice(&[0, 0]);
    write_u16(byte_order, &mut out, 0);
    write_u16(byte_order, &mut out, 0);
    write_u16(byte_order, &mut out, 0);
    write_u16(byte_order, &mut out, 0);
    write_u16(byte_order, &mut out, 0);
    write_u16(byte_order, &mut out, 0);
    write_u16(byte_order, &mut out, 0); // alpha-shift=0
    write_u16(byte_order, &mut out, 0xFF); // alpha-mask=0xFF
    write_u32(byte_order, &mut out, 0);

    // Format 3: X8R8G8B8 (depth=24, no alpha)
    write_u32(byte_order, &mut out, RENDER_FMT_RGB24);
    out.push(1); // type=Direct
    out.push(24); // depth=24
    out.extend_from_slice(&[0, 0]);
    write_u16(byte_order, &mut out, 16); // red-shift
    write_u16(byte_order, &mut out, 0xFF); // red-mask
    write_u16(byte_order, &mut out, 8); // green-shift
    write_u16(byte_order, &mut out, 0xFF); // green-mask
    write_u16(byte_order, &mut out, 0); // blue-shift
    write_u16(byte_order, &mut out, 0xFF); // blue-mask
    write_u16(byte_order, &mut out, 0); // alpha-shift
    write_u16(byte_order, &mut out, 0); // alpha-mask=0 (no alpha)
    write_u32(byte_order, &mut out, 0);

    // Format 4: A8R8G8B8 (depth=32, with alpha)
    write_u32(byte_order, &mut out, RENDER_FMT_ARGB32);
    out.push(1); // type=Direct
    out.push(32); // depth=32
    out.extend_from_slice(&[0, 0]);
    write_u16(byte_order, &mut out, 16); // red-shift
    write_u16(byte_order, &mut out, 0xFF);
    write_u16(byte_order, &mut out, 8); // green-shift
    write_u16(byte_order, &mut out, 0xFF);
    write_u16(byte_order, &mut out, 0); // blue-shift
    write_u16(byte_order, &mut out, 0xFF);
    write_u16(byte_order, &mut out, 24); // alpha-shift
    write_u16(byte_order, &mut out, 0xFF); // alpha-mask
    write_u32(byte_order, &mut out, 0);

    // Format 5: X8R8G8B8 at depth=32 (no alpha) — audit #4 / Xorg
    // PICT_x8r8g8b8. Wrap a depth-32 storage as opaque without
    // sampling the padding bytes as if they were real alpha.
    write_u32(byte_order, &mut out, RENDER_FMT_XRGB32);
    out.push(1); // type=Direct
    out.push(32); // depth=32
    out.extend_from_slice(&[0, 0]);
    write_u16(byte_order, &mut out, 16); // red-shift
    write_u16(byte_order, &mut out, 0xFF); // red-mask
    write_u16(byte_order, &mut out, 8); // green-shift
    write_u16(byte_order, &mut out, 0xFF); // green-mask
    write_u16(byte_order, &mut out, 0); // blue-shift
    write_u16(byte_order, &mut out, 0xFF); // blue-mask
    write_u16(byte_order, &mut out, 0); // alpha-shift
    write_u16(byte_order, &mut out, 0); // alpha-mask=0 (no alpha)
    write_u32(byte_order, &mut out, 0);

    // Screen info: 1 screen with 2 depths
    write_u32(byte_order, &mut out, num_depths); // nDepth per screen
    write_u32(byte_order, &mut out, RENDER_FMT_RGB24); // fallback
    // Depth 24 entry: 8-byte header + 1 visual (8 bytes)
    out.push(24);
    out.push(0);
    write_u16(byte_order, &mut out, 1); // 1 visual
    write_u32(byte_order, &mut out, 0); // pad
    write_u32(byte_order, &mut out, root_visual.0);
    write_u32(byte_order, &mut out, RENDER_FMT_RGB24);
    // Depth 32 entry: 8-byte header + 2 (visual, format) pairs.
    // The same ARGB visual is listed twice — once with ARGB32 (primary,
    // with-alpha) and once with XRGB32 (alternate, no-alpha). Per
    // X RENDER spec, multiple format associations for the same visual
    // are spec-legal and how Xorg advertises depth-32 — see
    // `render/picture.c` PICT_x8r8g8b8 + PICT_a8r8g8b8 entries.
    out.push(32);
    out.push(0);
    write_u16(byte_order, &mut out, 2); // 2 (visual, format) pairs
    write_u32(byte_order, &mut out, 0); // pad
    write_u32(byte_order, &mut out, argb_visual.0);
    write_u32(byte_order, &mut out, RENDER_FMT_ARGB32);
    write_u32(byte_order, &mut out, argb_visual.0);
    write_u32(byte_order, &mut out, RENDER_FMT_XRGB32);

    writer.write_all(&out)
}

pub fn write_render_query_pict_index_values_reply(
    writer: &mut impl Write,
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
) -> io::Result<()> {
    let mut out = vec![1u8, 0];
    write_u16(byte_order, &mut out, sequence.0);
    write_u32(byte_order, &mut out, 0); // length
    write_u32(byte_order, &mut out, 0); // num_values
    out.extend_from_slice(&[0u8; 20]); // pad to 32 bytes
    debug_assert_eq!(out.len(), 32);
    writer.write_all(&out)
}

pub fn write_render_query_filters_reply(
    writer: &mut impl Write,
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
) -> io::Result<()> {
    // Advertise the standard X.Org RENDER filter set: three canonical
    // filters (`nearest`, `bilinear`, `convolution`) plus three named
    // aliases (`fast` → nearest, `good` / `best` → bilinear). Clients
    // like picom v13's xrender backend consult this list before
    // emitting `SetPictureFilter`: when `convolution` is absent, they
    // silently disable kernel blur — so the reply itself is
    // load-bearing for any RENDER-based convolution work.
    //
    // Wire layout matches X.Org `render/render.c:ProcRenderQueryFilters`:
    // canonical filters come first, aliases after. The parallel
    // `aliases` array stores `FilterAliasNone` (= -1, wire 0xFFFF) for
    // canonical entries and the canonical filter's INDEX for alias
    // entries. See `xserver/render/render.c:1707-1726` for the
    // reference encoding.
    //
    // Reply wire layout (per Xrender protocol spec):
    //   1    reply opcode (1)
    //   1    unused
    //   2    sequence
    //   4    reply length (4-byte units of post-header data)
    //   4    num_aliases (= num_filters; one alias entry per name)
    //   4    num_filters
    //   16   unused
    //   2n   aliases: LISTofCARD16 — FilterAliasNone for canonical,
    //        canonical index for aliases; padded to multiple of 4
    //   m    filters: LISTofSTRING8 — each entry is u8 length + name,
    //        whole list padded to multiple of 4
    const CANONICAL: &[&[u8]] = &[b"nearest", b"bilinear", b"convolution"];
    const ALIAS_NAMES: &[&[u8]] = &[b"fast", b"good", b"best"];
    // Canonical filter each alias resolves to (index into CANONICAL).
    const ALIAS_TARGETS: &[u16] = &[0, 1, 1];
    // X.Org `#define FilterAliasNone -1` (render.h:194), wire-encoded
    // as 0xFFFF in the INT16 alias slot.
    const FILTER_ALIAS_NONE: u16 = 0xFFFF;

    let mut payload: Vec<u8> = Vec::new();
    for _ in CANONICAL {
        write_u16(byte_order, &mut payload, FILTER_ALIAS_NONE);
    }
    for &target in ALIAS_TARGETS {
        write_u16(byte_order, &mut payload, target);
    }
    // Pad aliases section to multiple of 4 bytes.
    while !payload.len().is_multiple_of(4) {
        payload.push(0);
    }
    for name in CANONICAL.iter().chain(ALIAS_NAMES.iter()) {
        let len = u8::try_from(name.len()).expect("filter name fits in u8");
        payload.push(len);
        payload.extend_from_slice(name);
    }
    // Pad filters section to multiple of 4 bytes.
    while !payload.len().is_multiple_of(4) {
        payload.push(0);
    }

    let length_words =
        u32::try_from(payload.len() / 4).expect("query-filters reply length fits in u32");

    let mut out: Vec<u8> = Vec::with_capacity(32 + payload.len());
    out.extend_from_slice(&[1u8, 0]);
    write_u16(byte_order, &mut out, sequence.0);
    write_u32(byte_order, &mut out, length_words);
    let total_names = CANONICAL.len() + ALIAS_NAMES.len();
    write_u32(byte_order, &mut out, total_names as u32); // num_aliases
    write_u32(byte_order, &mut out, total_names as u32); // num_filters
    out.extend_from_slice(&[0u8; 16]); // pad to 32-byte header
    debug_assert_eq!(out.len(), 32);
    out.extend_from_slice(&payload);
    writer.write_all(&out)
}

pub fn write_render_query_version_reply(
    writer: &mut impl Write,
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    major: u32,
    minor: u32,
) -> io::Result<()> {
    let mut out = vec![1u8, 0];
    write_u16(byte_order, &mut out, sequence.0);
    write_u32(byte_order, &mut out, 0); // length
    write_u32(byte_order, &mut out, major);
    write_u32(byte_order, &mut out, minor);
    out.extend_from_slice(&[0u8; 16]); // pad to 32 bytes
    writer.write_all(&out)
}

pub mod error {
    pub const BAD_REQUEST: u8 = 1;
    pub const BAD_VALUE: u8 = 2;
    pub const BAD_WINDOW: u8 = 3;
    pub const BAD_PIXMAP: u8 = 4;
    pub const BAD_ATOM: u8 = 5;
    pub const BAD_CURSOR: u8 = 6;
    pub const BAD_FONT: u8 = 7;
    pub const BAD_MATCH: u8 = 8;
    pub const BAD_DRAWABLE: u8 = 9;
    pub const BAD_ACCESS: u8 = 10;
    pub const BAD_ALLOC: u8 = 11;
    pub const BAD_COLORMAP: u8 = 12;
    pub const BAD_GC: u8 = 13;
    pub const BAD_ID_CHOICE: u8 = 14;
    pub const BAD_NAME: u8 = 15;
    pub const BAD_LENGTH: u8 = 16;
    pub const BAD_IMPLEMENTATION: u8 = 17;
}
