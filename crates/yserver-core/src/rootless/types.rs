//! Plain data crossing the core ↔ host boundary (spec §4.2, §4.4).

use std::num::NonZeroU64;

use yserver_protocol::x11::ResourceId;

use super::info::{InfoFields, NetWmState, WindowInfo};
use crate::core_loop::InputOrigin;

/// Names one mapped lifetime of a top-level (spec §4.2). Never an XID.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ToplevelId(NonZeroU64);

impl ToplevelId {
    pub(crate) fn new(raw: NonZeroU64) -> Self {
        Self(raw)
    }

    #[must_use]
    pub fn get(self) -> u64 {
        self.0.get()
    }

    /// An id no core issued, for host tests.
    ///
    /// # Panics
    ///
    /// `raw` is 0.
    #[doc(hidden)]
    #[must_use]
    pub fn fake(raw: u64) -> Self {
        Self(NonZeroU64::new(raw).expect("a non-zero id"))
    }
}

/// Monotonic per host, assigned to every geometry/stacking `HostRequest`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct HostSerial(pub u64);

/// Geometry fields a non-host source set after the last acked host request (spec §4.2.1).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct GeometryFields(u8);

impl GeometryFields {
    pub const X: Self = Self(1);
    pub const Y: Self = Self(1 << 1);
    pub const WIDTH: Self = Self(1 << 2);
    pub const HEIGHT: Self = Self(1 << 3);
    pub const BORDER: Self = Self(1 << 4);

    #[must_use]
    pub fn empty() -> Self {
        Self(0)
    }

    #[must_use]
    pub fn bits(self) -> u8 {
        self.0
    }

    #[must_use]
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    #[must_use]
    pub fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    #[must_use]
    pub fn without(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }

    /// The fields a ConfigureWindow value mask sets (X11 CW bits 0-4).
    #[must_use]
    pub fn from_value_mask(mask: u16) -> Self {
        Self(u8::try_from(mask & 0x1f).unwrap_or(0))
    }
}

impl std::ops::BitOr for GeometryFields {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for GeometryFields {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

/// Spec §4.0: `x, y` = outer corner in root coordinates; `width, height` = X content size.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Geometry {
    pub x: i32,
    pub y: i32,
    pub width: u16,
    pub height: u16,
    pub border: u16,
}

/// A rectangle in backing pixels (spec §4.0).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u16,
    pub height: u16,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ToplevelKind {
    Managed,
    /// override-redirect: placed where X put it, relative to `parent` where the host needs it.
    Popup {
        parent: Option<ToplevelId>,
    },
}

/// `_NET_WM_STATE_ABOVE` / `_BELOW` (EWMH §5.7).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Layer {
    Normal,
    Above,
    Below,
}

/// `_NET_WM_MOVERESIZE` edges (EWMH §4.3 directions 0-7).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ResizeEdge {
    TopLeft,
    Top,
    TopRight,
    Right,
    BottomRight,
    Bottom,
    BottomLeft,
    Left,
}

/// A client asked the WM for something (spec §6.3).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WmRequest {
    SetFullscreen(bool),
    SetMaximized(bool),
    SetLayer(Layer),
    Minimize,
    BeginMove { button: u8 },
    BeginResize { edge: ResizeEdge, button: u8 },
    Activate,
    Close,
}

/// Spec §4.2.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum RootlessEvent {
    Mapped {
        id: ToplevelId,
        window: ResourceId,
        kind: ToplevelKind,
        geometry: Geometry,
        depth: u8,
        info: WindowInfo,
        above: Option<ToplevelId>,
    },
    Unmapped {
        id: ToplevelId,
    },
    Configured {
        id: ToplevelId,
        geometry: Geometry,
        acked: Option<HostSerial>,
        client_fields: GeometryFields,
    },
    Restacked {
        id: ToplevelId,
        above: Option<ToplevelId>,
        acked: Option<HostSerial>,
        from_client: bool,
    },
    Reshaped {
        id: ToplevelId,
        bounding: Option<Vec<Rect>>,
        input: Option<Vec<Rect>>,
    },
    InfoChanged {
        id: ToplevelId,
        info: WindowInfo,
        changed: InfoFields,
    },
    WmRequest {
        id: ToplevelId,
        request: WmRequest,
    },
    /// A popup's placement parent changed (its `WM_TRANSIENT_FOR` changed or
    /// resolved, or the old parent went away); the new `ToplevelKind::Popup`
    /// parent.
    ParentChanged {
        id: ToplevelId,
        parent: Option<ToplevelId>,
    },
    Destroyed {
        id: ToplevelId,
    },
    ScreenChanged {
        root: (u16, u16),
    },
    /// Lock the host's pointer on `id`'s native window (`Some`), or let it
    /// go (`None`): a client grabbed a hidden pointer (`lock.rs`). While it
    /// is held the host sends `RootlessInput::PointerRelative` rather than
    /// positions.
    PointerLock {
        id: Option<ToplevelId>,
    },
}

/// One native output in X pixels, root coordinates (spec §5).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct HostOutput {
    pub x: i32,
    pub y: i32,
    pub width: u16,
    pub height: u16,
}

/// Host → core WM-side actions (spec §4.4; `PopupDismissed` is out of RL0, D8).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum HostRequest {
    Activated {
        id: ToplevelId,
    },
    Deactivated,
    Moved {
        id: ToplevelId,
        x: i32,
        y: i32,
        serial: HostSerial,
    },
    Resized {
        id: ToplevelId,
        width: u16,
        height: u16,
        serial: HostSerial,
    },
    Restacked {
        id: ToplevelId,
        above: Option<ToplevelId>,
        serial: HostSerial,
    },
    StateChanged {
        id: ToplevelId,
        state: NetWmState,
    },
    CloseRequested {
        id: ToplevelId,
    },
    ForceKill {
        id: ToplevelId,
    },
    ScreenLayout {
        outputs: Vec<HostOutput>,
    },
}

/// Local-coordinate pointer input (spec §4.4); `local` is in backing pixels.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum RootlessInput {
    PointerEnter {
        origin: InputOrigin,
        toplevel: ToplevelId,
        local: (f64, f64),
        time: u32,
    },
    PointerMotionLocal {
        origin: InputOrigin,
        toplevel: ToplevelId,
        local: (f64, f64),
        time: u32,
    },
    PointerLeave {
        origin: InputOrigin,
        time: u32,
    },
    /// While the host holds the pointer locked (`RootlessEvent::PointerLock`):
    /// it moved by `delta` (accelerated) and `unaccelerated`, in X pixels.
    /// X's pointer moves by the whole pixels of `delta` from where X has
    /// it; `unaccelerated` is XI2's raw motion. Dropped when no lock is
    /// held.
    PointerRelative {
        origin: InputOrigin,
        delta: (f64, f64),
        unaccelerated: (f64, f64),
        time: u32,
    },
}
