//! In-tree X11 / RENDER protocol data types. Used to be a pixman
//! shim layer; the pixman dep is now gone, so this is the
//! authoritative definition.

/// Rectangle in 16-bit signed coords (matches the X11 wire format).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rectangle16 {
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
}

/// X RENDER picture repeat mode. Numeric values match the X11
/// protocol (`RepeatNone=0`, `RepeatNormal=1`, `RepeatPad=2`,
/// `RepeatReflect=3`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Repeat {
    None = 0,
    Normal = 1,
    Pad = 2,
    Reflect = 3,
}

/// 3×3 transform matrix in 16.16 fixed-point. Layout matches X RENDER
/// `XTransform` on the wire (and the historical
/// `pixman::ffi::pixman_transform_t`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PictTransform {
    pub matrix: [[i32; 3]; 3],
}

impl PictTransform {
    pub const IDENTITY: PictTransform = PictTransform {
        matrix: [[0x10000, 0, 0], [0, 0x10000, 0], [0, 0, 0x10000]],
    };

    pub fn is_identity(&self) -> bool {
        *self == Self::IDENTITY
    }
}
