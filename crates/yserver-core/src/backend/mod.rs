//! Backend abstraction. Currently `HostX11Backend` is the sole impl;
//! Phase 6.3+ will add a KMS backend.

pub mod handles;
pub mod params;
mod trait_def;

#[cfg(test)]
pub mod recording;

pub use handles::{
    AnyHandle, ColormapHandle, CursorHandle, FontHandle, GlyphSetHandle, HandleKind, PictureHandle,
    PixmapHandle, VisualHandle, WindowHandle,
};
pub use params::{
    ArcMode, BgState, CapStyle, ClipState, DrawState, FillRule, FillState, FillStyle, GcFunction,
    JoinStyle, LineStyle, SubwindowMode,
};
pub use trait_def::{
    ActiveCursorImage, Backend, BackendFdKind, CompletedPresentEvent, Dri3Caps, Dri3PixmapExport,
    HostSocketStatus, ModeSpec, PresentCaps, PresentWake, SyncobjHandle, XshmfenceHandle,
};

use yserver_protocol::x11::ClientId;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OriginContext {
    pub client_id: ClientId,
    pub nested_seq: u16,
    pub opcode: u8,
}
