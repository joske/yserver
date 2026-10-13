//! Parameter types for the Backend trait. Snapshots of state that are
//! resolved by yserver-core once per request and passed to the backend.
//!
//! These are the "by-borrow" GC snapshot the drawing methods read from
//! at draw time. `ResourceTable::resolve_draw_state` builds one in a
//! single locked pass over the GC + pixmap tables; the host then pushes
//! the relevant fields to its shared GC before issuing the draw.

use crate::backend::{FontHandle, PixmapHandle};
use yserver_protocol::x11::ClipRectangles;
pub use yserver_protocol::x11::gc::{ArcMode, CapStyle, GcFunction, JoinStyle, LineStyle};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FillStyle {
    Solid,
    Tiled,
    Stippled,
    OpaqueStippled,
}

impl FillStyle {
    pub fn from_protocol(value: u8) -> Self {
        match value {
            1 => Self::Tiled,
            2 => Self::Stippled,
            3 => Self::OpaqueStippled,
            _ => Self::Solid,
        }
    }

    pub fn protocol_value(self) -> u8 {
        match self {
            Self::Solid => 0,
            Self::Tiled => 1,
            Self::Stippled => 2,
            Self::OpaqueStippled => 3,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FillRule {
    EvenOdd,
    Winding,
}

impl FillRule {
    pub fn from_protocol(value: u8) -> Self {
        match value {
            1 => Self::Winding,
            _ => Self::EvenOdd,
        }
    }

    pub fn protocol_value(self) -> u8 {
        match self {
            Self::EvenOdd => 0,
            Self::Winding => 1,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubwindowMode {
    ClipByChildren,
    IncludeInferiors,
}

impl SubwindowMode {
    pub fn from_protocol(value: u8) -> Self {
        match value {
            1 => Self::IncludeInferiors,
            _ => Self::ClipByChildren,
        }
    }

    pub fn protocol_value(self) -> u8 {
        match self {
            Self::ClipByChildren => 0,
            Self::IncludeInferiors => 1,
        }
    }
}

/// Effective clip-state of a GC at the moment a draw op runs. Either
/// the GC has a `SetClipRectangles` list, a `ChangeGC(clip_mask=Pixmap)`
/// host-mask, or no clip at all. `Rectangles.rects` carries the wire
/// representation (pairs of (i16, i16, u16, u16) packed as bytes), the
/// same `Vec<u8>` form used by `SetClipRectangles` requests.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClipState {
    None,
    Rectangles {
        origin: (i16, i16),
        rects: ClipRectangles,
    },
    Pixmap {
        origin: (i16, i16),
        pixmap: PixmapHandle,
    },
}

/// Effective fill-style of a GC. `Solid` = use foreground; `Tiled` =
/// repeat the named tile pixmap; `Stippled` / `OpaqueStippled` = stencil
/// the foreground (and background, for OpaqueStippled) through a depth-1
/// stipple pixmap. e16 paints popup backgrounds via Tiled.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FillState {
    Solid,
    Tiled {
        pixmap: PixmapHandle,
        origin: (i16, i16),
    },
    Stippled {
        pixmap: PixmapHandle,
        origin: (i16, i16),
    },
    OpaqueStippled {
        pixmap: PixmapHandle,
        origin: (i16, i16),
    },
}

/// Effective background of a window for `ClearArea` / exposes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BgState {
    Pixel(u32),
    Pixmap(PixmapHandle),
    None,
}

/// Snapshot of a GC's entire state at the moment a draw op runs.
/// Resolved by `ResourceTable::resolve_draw_state` once per request.
/// The drawing call sites pass `&DrawState` to the backend, which pushes
/// the relevant fields to its shared GC before issuing the host draw.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DrawState {
    pub foreground: u32,
    pub background: u32,
    pub line_width: u16,
    pub line_style: LineStyle,
    pub cap_style: CapStyle,
    pub join_style: JoinStyle,
    pub fill_style: FillStyle,
    pub fill_rule: FillRule,
    pub function: GcFunction,
    pub plane_mask: u32,
    pub font: Option<FontHandle>,
    pub clip: ClipState,
    pub fill: FillState,
    pub subwindow_mode: SubwindowMode,
    pub graphics_exposures: bool,
    pub dashes: Vec<u8>,
    pub dash_offset: i16,
    pub arc_mode: ArcMode,
}

impl Default for DrawState {
    fn default() -> Self {
        Self {
            foreground: 0,
            background: 0x00ff_ffff,
            line_width: 0,
            line_style: LineStyle::Solid,
            cap_style: CapStyle::Butt,
            join_style: JoinStyle::Miter,
            fill_style: FillStyle::Solid,
            fill_rule: FillRule::EvenOdd,
            function: GcFunction::Copy,
            plane_mask: u32::MAX,
            font: None,
            clip: ClipState::None,
            fill: FillState::Solid,
            subwindow_mode: SubwindowMode::ClipByChildren,
            graphics_exposures: true,
            dashes: vec![4, 4],
            dash_offset: 0,
            arc_mode: ArcMode::PieSlice,
        }
    }
}
