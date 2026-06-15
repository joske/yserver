// F2: ynest drives the single-threaded core loop. This module retains
// `pub fn run` (the binary entry) plus the helpers process_request and
// the host-X11 backend still pull in (extension registry, fanout
// glue, region/shape ops). The legacy `handle_client`/`handle_request`
// path was deleted in H1.

use std::{
    fs,
    io::{self, ErrorKind},
    os::unix::net::UnixListener,
    path::PathBuf,
};

use log::{error, info};
use yserver_protocol::x11::{ResourceId, shape as x11shape, xfixes as x11xfixes};

use crate::{
    backend::{Backend, WindowHandle},
    host_x11::HostX11Backend,
    resources::ROOT_WINDOW,
    server::ServerState,
};

// Extension error-code bases. INVARIANT: each extension's reserved range
// `[first_error, first_error + error_count)` must NOT overlap any other
// extension's range — clients (notably Google Chrome's X11 layer) build an
// error-code→extension map at connection setup and a strict build aborts
// (SIGILL/ud2) on overlapping ranges. Extensions with zero errors use 0.
// Current allocation (error counts in parens):
//   RANDR 147 (5) · RENDER 152 (5) · XInput 157 (5) · [host XKB 162 (1)]
//   · XFIXES 163 (1) · SYNC 164 (3) · DAMAGE 167 (1) · MIT-SHM 168 (1)
//   · GLX 169 (13, →181). SHAPE/Composite/Present/DPMS/screensaver: 0.
// Do NOT pack these 1 apart — a multi-error extension (RENDER=5, GLX=13)
// would then overlap its neighbours.
const RANDR_MAJOR_OPCODE: u8 = 128;
const RANDR_FIRST_EVENT: u8 = 89;
const RANDR_FIRST_ERROR: u8 = 147;

const RENDER_MAJOR_OPCODE: u8 = 133;
const RENDER_FIRST_EVENT: u8 = 0;
pub(crate) const RENDER_FIRST_ERROR: u8 = 152;

const GE_MAJOR_OPCODE: u8 = 138;

const BIG_REQUESTS_MAJOR_OPCODE: u8 = 135;
const BIG_REQUESTS_FIRST_EVENT: u8 = 0;
const BIG_REQUESTS_FIRST_ERROR: u8 = 0;

const XKB_MAJOR_OPCODE: u8 = 136;

const XI2_MAJOR_OPCODE: u8 = 137;
/// First event code reserved for the XInput extension. Despite the name,
/// this is the same base for XI1 *and* XI2 — Xorg assigns one contiguous
/// `first_event..first_event+16` block to XInput. XI1 event codes 0..16
/// live at `first_event..first_event+16` (e.g. `DevicePropertyNotify=16`
/// at byte type `first_event + 16 = 82`); XI2 uses `GenericEvent` (35)
/// with `evtype` discrimination. Shared by the XI1 selection plumbing
/// (re-exported as [`crate::server::XI_FIRST_EVENT`]) and the XI2 event
/// fan-outs.
pub(crate) const XI2_FIRST_EVENT: u8 = 66;
pub(crate) const XI2_FIRST_ERROR: u8 = 157;

const XFIXES_MAJOR_OPCODE: u8 = 140;
pub(crate) const XFIXES_FIRST_EVENT: u8 = 87; // matches Xorg: Selection=87, Cursor=88
const XFIXES_FIRST_ERROR: u8 = 163;

const SHAPE_MAJOR_OPCODE: u8 = 141;
const SHAPE_FIRST_EVENT: u8 = 64; // matches Xorg: ShapeNotify=64
const SHAPE_FIRST_ERROR: u8 = 0; // SHAPE defines no errors (match Xorg: first_error=0)

const SYNC_MAJOR_OPCODE: u8 = 142;
pub(crate) const SYNC_FIRST_EVENT: u8 = 83; // matches Xorg: Counter=83, Alarm=84
const SYNC_FIRST_ERROR: u8 = 164;

const DAMAGE_MAJOR_OPCODE: u8 = 143;
pub(crate) const DAMAGE_FIRST_EVENT: u8 = 91; // matches Xorg: DamageNotify=91
const DAMAGE_FIRST_ERROR: u8 = 167;

const COMPOSITE_MAJOR_OPCODE: u8 = 144;
const COMPOSITE_FIRST_EVENT: u8 = 0;
const COMPOSITE_FIRST_ERROR: u8 = 0; // Composite defines no errors

const PRESENT_MAJOR_OPCODE: u8 = 145;
const PRESENT_FIRST_EVENT: u8 = 0; // Present uses XGE, no sequential event codes
const PRESENT_FIRST_ERROR: u8 = 0; // Present defines no errors

pub(crate) const DPMS_MAJOR_OPCODE: u8 = 134;

pub(crate) const MIT_SCREEN_SAVER_MAJOR_OPCODE: u8 = 150;
pub(crate) const XINERAMA_MAJOR_OPCODE: u8 = 151;
pub(crate) const XCMISC_MAJOR_OPCODE: u8 = 152;
// MUST be <= 127: X event codes are 7-bit (2..=127); bit 0x80 is the
// SendEvent/synthetic flag, so a base first_event with the high bit set is
// illegal and strict clients (Google Chrome's X11 layer) abort on it. 92 is
// the free slot after DAMAGE=91, before GLX=95. (Was 162 — an error-range
// value mistakenly used here; it crashed google-chrome with SIGILL/ud2 right
// after the QueryExtension batch.)
pub(crate) const MIT_SCREEN_SAVER_FIRST_EVENT: u8 = 92;

const MIT_SHM_MAJOR_OPCODE: u8 = 130;
const MIT_SHM_FIRST_EVENT: u8 = 65; // matches Xorg: ShmCompletion=65
const MIT_SHM_FIRST_ERROR: u8 = 168;

const XTEST_MAJOR_OPCODE: u8 = 146;

pub(crate) const DRI3_MAJOR_OPCODE: u8 = 147;

pub(crate) const GLX_MAJOR_OPCODE: u8 = 148;
pub(crate) const GLX_FIRST_EVENT: u8 = 95; // matches Xorg: Pbuffer=95, BufferSwap=96
pub(crate) const GLX_FIRST_ERROR: u8 = 169; // 13 errors (169-181); routed past host XKB error base 162

pub(crate) const X_RESOURCE_MAJOR_OPCODE: u8 = 149;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExtensionAvailability {
    Always,
    HostRender,
    HostXkb,
    /// DRI3 — gated on `Backend::dri3_capabilities().version != (0, 0)`.
    /// Task 5 treats this as always-true (DRI3 surface is wired but the
    /// backend has no real caps yet). Task 11 will narrow this to the
    /// real capability check.
    Dri3,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UnsupportedMinorPolicy {
    HandledInline,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ExtensionMetadata {
    pub(crate) name: &'static str,
    pub(crate) major_opcode: u8,
    pub(crate) first_event: u8,
    /// Number of sequential event codes reserved starting at first_event.
    /// 0 for extensions that use XGE or have no events (first_event must be 0).
    pub(crate) event_count: u8,
    pub(crate) first_error: u8,
    pub(crate) availability: ExtensionAvailability,
    #[allow(dead_code)]
    pub(crate) unsupported_minor_policy: UnsupportedMinorPolicy,
}

pub(crate) const EXTENSIONS: &[ExtensionMetadata] = &[
    ExtensionMetadata {
        name: "RANDR",
        major_opcode: RANDR_MAJOR_OPCODE,
        first_event: RANDR_FIRST_EVENT,
        event_count: 2, // ScreenChangeNotify=0, Notify=1
        first_error: RANDR_FIRST_ERROR,
        availability: ExtensionAvailability::Always,
        unsupported_minor_policy: UnsupportedMinorPolicy::HandledInline,
    },
    ExtensionMetadata {
        name: "RENDER",
        major_opcode: RENDER_MAJOR_OPCODE,
        first_event: RENDER_FIRST_EVENT,
        event_count: 0,
        first_error: RENDER_FIRST_ERROR,
        availability: ExtensionAvailability::HostRender,
        unsupported_minor_policy: UnsupportedMinorPolicy::HandledInline,
    },
    ExtensionMetadata {
        name: "Generic Event Extension",
        major_opcode: GE_MAJOR_OPCODE,
        first_event: 0,
        event_count: 0,
        first_error: 0,
        availability: ExtensionAvailability::Always,
        unsupported_minor_policy: UnsupportedMinorPolicy::HandledInline,
    },
    ExtensionMetadata {
        name: "BIG-REQUESTS",
        major_opcode: BIG_REQUESTS_MAJOR_OPCODE,
        first_event: BIG_REQUESTS_FIRST_EVENT,
        event_count: 0,
        first_error: BIG_REQUESTS_FIRST_ERROR,
        availability: ExtensionAvailability::Always,
        unsupported_minor_policy: UnsupportedMinorPolicy::HandledInline,
    },
    ExtensionMetadata {
        name: "XINERAMA",
        major_opcode: XINERAMA_MAJOR_OPCODE,
        first_event: 0,
        event_count: 0,
        first_error: 0,
        availability: ExtensionAvailability::Always,
        unsupported_minor_policy: UnsupportedMinorPolicy::HandledInline,
    },
    ExtensionMetadata {
        name: "XKEYBOARD",
        major_opcode: XKB_MAJOR_OPCODE,
        first_event: 0, // passed through from host via xkb_info()
        event_count: 0,
        first_error: 0,
        availability: ExtensionAvailability::HostXkb,
        unsupported_minor_policy: UnsupportedMinorPolicy::HandledInline,
    },
    ExtensionMetadata {
        name: "XInputExtension",
        major_opcode: XI2_MAJOR_OPCODE,
        first_event: XI2_FIRST_EVENT,
        event_count: 17, // XI1 legacy: DeviceValuator..DevicePropertyNotify (66-82)
        first_error: XI2_FIRST_ERROR,
        availability: ExtensionAvailability::Always,
        unsupported_minor_policy: UnsupportedMinorPolicy::HandledInline,
    },
    ExtensionMetadata {
        name: "XFIXES",
        major_opcode: XFIXES_MAJOR_OPCODE,
        first_event: XFIXES_FIRST_EVENT,
        event_count: 2, // SelectionNotify=0, CursorNotify=1
        first_error: XFIXES_FIRST_ERROR,
        availability: ExtensionAvailability::Always,
        unsupported_minor_policy: UnsupportedMinorPolicy::HandledInline,
    },
    ExtensionMetadata {
        name: "SHAPE",
        major_opcode: SHAPE_MAJOR_OPCODE,
        first_event: SHAPE_FIRST_EVENT,
        event_count: 1, // ShapeNotify=0
        first_error: SHAPE_FIRST_ERROR,
        availability: ExtensionAvailability::Always,
        unsupported_minor_policy: UnsupportedMinorPolicy::HandledInline,
    },
    ExtensionMetadata {
        name: "SYNC",
        major_opcode: SYNC_MAJOR_OPCODE,
        first_event: SYNC_FIRST_EVENT,
        event_count: 2, // CounterNotify=0, AlarmNotify=1
        first_error: SYNC_FIRST_ERROR,
        availability: ExtensionAvailability::Always,
        unsupported_minor_policy: UnsupportedMinorPolicy::HandledInline,
    },
    ExtensionMetadata {
        name: "DAMAGE",
        major_opcode: DAMAGE_MAJOR_OPCODE,
        first_event: DAMAGE_FIRST_EVENT,
        event_count: 1, // Notify=0
        first_error: DAMAGE_FIRST_ERROR,
        availability: ExtensionAvailability::Always,
        unsupported_minor_policy: UnsupportedMinorPolicy::HandledInline,
    },
    ExtensionMetadata {
        name: "Composite",
        major_opcode: COMPOSITE_MAJOR_OPCODE,
        first_event: COMPOSITE_FIRST_EVENT,
        event_count: 0,
        first_error: COMPOSITE_FIRST_ERROR,
        availability: ExtensionAvailability::Always,
        unsupported_minor_policy: UnsupportedMinorPolicy::HandledInline,
    },
    ExtensionMetadata {
        name: "Present",
        major_opcode: PRESENT_MAJOR_OPCODE,
        first_event: PRESENT_FIRST_EVENT,
        event_count: 0, // Present uses XGE, not sequential event codes
        first_error: PRESENT_FIRST_ERROR,
        availability: ExtensionAvailability::Always,
        unsupported_minor_policy: UnsupportedMinorPolicy::HandledInline,
    },
    ExtensionMetadata {
        name: "DPMS",
        major_opcode: DPMS_MAJOR_OPCODE,
        first_event: 0, // XGE — no sequential event codes
        event_count: 0,
        first_error: 0, // uses core BadValue / BadMatch
        availability: ExtensionAvailability::Always,
        unsupported_minor_policy: UnsupportedMinorPolicy::HandledInline,
    },
    ExtensionMetadata {
        name: "MIT-SCREEN-SAVER",
        major_opcode: MIT_SCREEN_SAVER_MAJOR_OPCODE,
        first_event: MIT_SCREEN_SAVER_FIRST_EVENT,
        event_count: 1, // ScreenSaverNotify
        first_error: 0, // uses core BadValue / BadAccess / BadLength
        availability: ExtensionAvailability::Always,
        unsupported_minor_policy: UnsupportedMinorPolicy::HandledInline,
    },
    ExtensionMetadata {
        name: "MIT-SHM",
        major_opcode: MIT_SHM_MAJOR_OPCODE,
        first_event: MIT_SHM_FIRST_EVENT,
        event_count: 1, // ShmCompletion=0
        first_error: MIT_SHM_FIRST_ERROR,
        availability: ExtensionAvailability::Always,
        unsupported_minor_policy: UnsupportedMinorPolicy::HandledInline,
    },
    ExtensionMetadata {
        name: "XTEST",
        major_opcode: XTEST_MAJOR_OPCODE,
        first_event: 0,
        event_count: 0,
        first_error: 0,
        availability: ExtensionAvailability::Always,
        unsupported_minor_policy: UnsupportedMinorPolicy::HandledInline,
    },
    ExtensionMetadata {
        name: "DRI3",
        major_opcode: DRI3_MAJOR_OPCODE,
        first_event: 0,
        event_count: 0,
        first_error: 0,
        availability: ExtensionAvailability::Dri3,
        unsupported_minor_policy: UnsupportedMinorPolicy::HandledInline,
    },
    ExtensionMetadata {
        name: "GLX",
        major_opcode: GLX_MAJOR_OPCODE,
        first_event: GLX_FIRST_EVENT,
        event_count: 2, // PbufferClobber=0, BufferSwapComplete=1
        first_error: GLX_FIRST_ERROR,
        availability: ExtensionAvailability::Always,
        unsupported_minor_policy: UnsupportedMinorPolicy::HandledInline,
    },
    ExtensionMetadata {
        name: "X-Resource",
        major_opcode: X_RESOURCE_MAJOR_OPCODE,
        first_event: 0,
        event_count: 0,
        first_error: 0,
        availability: ExtensionAvailability::Always,
        unsupported_minor_policy: UnsupportedMinorPolicy::HandledInline,
    },
    ExtensionMetadata {
        name: "XC-MISC",
        major_opcode: XCMISC_MAJOR_OPCODE,
        first_event: 0,
        event_count: 0,
        first_error: 0,
        availability: ExtensionAvailability::Always,
        unsupported_minor_policy: UnsupportedMinorPolicy::HandledInline,
    },
];

pub fn run(display: u16, width: u16, height: u16) -> io::Result<()> {
    let socket_dir = PathBuf::from("/tmp/.X11-unix");
    fs::create_dir_all(&socket_dir)?;

    let socket_path = socket_dir.join(format!("X{display}"));
    match fs::remove_file(&socket_path) {
        Ok(()) => {}
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }

    let listener = UnixListener::bind(&socket_path)?;
    info!("ynest listening on DISPLAY=:{display}");

    // The single-threaded core loop owns the host X11 connection
    // directly — no dispatcher thread, no per-client kb pumps. Host
    // events come off the host fd via the core's mio poller
    // (`HOST_X11_TOKEN`) and fan out at the outer-loop boundary.
    let mut backend = match HostX11Backend::open_from_env(width, height) {
        Ok(opened) => {
            info!(
                "host X11 container window: 0x{:x} ({width}x{height})",
                opened.window_id()
            );
            opened
        }
        Err(err) => {
            error!("could not open host X11 window: {err}");
            return Err(err);
        }
    };

    // Sanity ping so any lingering host-side error from init lands
    // before clients connect.
    let _ = crate::backend::Backend::ping(&mut backend, None);

    let host_window_id = backend.window_id();

    // Build the synthetic ynest output explicitly. The exact integer
    // IDs (output=1, crtc=2, mode=3) and `ynest-0` name are
    // load-bearing for existing xts wire-byte fixtures.
    let synthetic = crate::randr::RandrOutput {
        name: "ynest-0".to_string(),
        output_id: 1,
        crtc_id: 2,
        mode_id: 3,
        x: 0,
        y: 0,
        width,
        height,
        vrefresh: 60,
        // Nested backend has no EDID; `RandrState::output_info` falls
        // back to 96-DPI synthesis from pixel dims.
        mm_width: 0,
        mm_height: 0,
    };
    let mut state = ServerState::with_randr_outputs(width, height, vec![synthetic]);
    // Snapshot the backend's DPMS capability into the state. host-X11
    // inherits the trait default (`false`), so this matches the
    // ServerState::with_geometry default — but keeping the call here
    // keeps the two entry points symmetric: if a future host backend
    // overrides `dpms_capable()`, the snapshot picks it up
    // automatically.
    state.dpms = crate::server::DpmsState::new(backend.dpms_capable());
    state.glx_tfp_supported = backend.supports_dmabuf_export();
    // Route root-window drawing/clearing to the host container window
    // so clients that paint the root (e.g. fvwm3 setting its desktop
    // bg pixmap) produce visible output in the nested viewport.
    if let Some(root) = state.resources.window_mut(ROOT_WINDOW) {
        root.host_xid = WindowHandle::from_raw(host_window_id);
    }

    // Push host visual / colormap xids into the resource table so that
    // CreateWindow forwarding can translate our visual ids to host ones.
    state
        .resources
        .set_visual_host_xid(crate::resources::ROOT_VISUAL, backend.root_visual_xid());
    if let Some(host_colormap) = backend.argb_colormap_xid() {
        state
            .resources
            .set_colormap_host_xid(crate::resources::ARGB_COLORMAP, host_colormap);
    }
    if let Some(host_argb_visual) = backend.argb_visual_xid() {
        state
            .resources
            .set_visual_host_xid(crate::resources::ARGB_VISUAL, host_argb_visual);
    }

    let (poll, sender, rx) = crate::core_loop::sender::channel()?;
    let allocator = crate::core_loop::poll_tokens::ClientIdAllocator::new();
    crate::core_loop::run::run_core(
        poll,
        rx,
        sender,
        &mut state,
        &mut backend,
        Some(listener),
        &allocator,
    )
}

/// The two ChangePicture attribute kinds whose value is an XID and therefore
/// needs translation between client and host atom spaces before we can
/// forward the request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ChangePictureAttr {
    /// CPAlphaMap (bit 1) — value is a `Picture` XID.
    AlphaMap,
    /// CPClipMask (bit 6) — value is a `Pixmap` XID (or 0 for None).
    ClipMask,
}

/// Translate any XID-valued attributes in a `ChangePicture` `values` slice.
///
/// Walks the encoded values in attribute-bit order; for each attribute whose
/// value is a non-zero XID (`CPAlphaMap` and `CPClipMask`), invokes
/// `translate(attr, value)` to obtain the host XID. Returns a fresh `Vec<u8>`
/// with the host XIDs substituted, or `None` if any translator returns
/// `None` (caller drops the request) or the input is shorter than
/// `value_mask` requires.
///
/// Scalar attributes and explicit `None` (zero) XID values are passed
/// through unchanged.
pub(crate) fn change_picture_translate_xids<F>(
    value_mask: u32,
    values: &[u8],
    mut translate: F,
) -> Option<Vec<u8>>
where
    F: FnMut(ChangePictureAttr, u32) -> Option<u32>,
{
    const CP_ALPHA_MAP: u32 = 1 << 1;
    const CP_CLIP_MASK: u32 = 1 << 6;

    let nvalues = value_mask.count_ones() as usize;
    if values.len() < nvalues * 4 {
        return None;
    }
    let mut out = values[..nvalues * 4].to_vec();
    let mut idx = 0usize;
    for bit in 0..32u32 {
        if value_mask & (1 << bit) == 0 {
            continue;
        }
        let attr = match 1 << bit {
            CP_ALPHA_MAP => Some(ChangePictureAttr::AlphaMap),
            CP_CLIP_MASK => Some(ChangePictureAttr::ClipMask),
            _ => None,
        };
        if let Some(attr) = attr {
            let v = u32::from_le_bytes([
                out[idx * 4],
                out[idx * 4 + 1],
                out[idx * 4 + 2],
                out[idx * 4 + 3],
            ]);
            if v != 0 {
                let host = translate(attr, v)?;
                out[idx * 4..idx * 4 + 4].copy_from_slice(&host.to_le_bytes());
            }
        }
        idx += 1;
    }
    Some(out)
}

pub(crate) fn normalize_region_rects(
    mut rects: Vec<x11xfixes::RegionRect>,
) -> Vec<x11xfixes::RegionRect> {
    rects.retain(|rect| !rect.is_empty());
    if rects.is_empty() {
        return rects;
    }

    let mut y_edges: Vec<i32> = Vec::with_capacity(rects.len() * 2);
    for r in &rects {
        let y0 = i32::from(r.y);
        let y1 = y0 + i32::from(r.height);
        y_edges.push(y0);
        y_edges.push(y1);
    }
    y_edges.sort_unstable();
    y_edges.dedup();

    let mut out = Vec::new();
    for band in y_edges.windows(2) {
        let y0 = band[0];
        let y1 = band[1];
        if y1 <= y0 {
            continue;
        }
        let mut spans: Vec<(i32, i32)> = rects
            .iter()
            .filter_map(|r| {
                let rx0 = i32::from(r.x);
                let ry0 = i32::from(r.y);
                let rx1 = rx0 + i32::from(r.width);
                let ry1 = ry0 + i32::from(r.height);
                if ry0 <= y0 && ry1 >= y1 {
                    Some((rx0, rx1))
                } else {
                    None
                }
            })
            .collect();
        if spans.is_empty() {
            continue;
        }
        spans.sort_unstable_by_key(|span| (span.0, span.1));
        let mut current = spans[0];
        for span in spans.into_iter().skip(1) {
            if span.0 <= current.1 {
                current.1 = current.1.max(span.1);
            } else {
                out.push(x11xfixes::RegionRect {
                    x: current.0.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16,
                    y: y0.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16,
                    width: (current.1 - current.0).clamp(0, i32::from(u16::MAX)) as u16,
                    height: (y1 - y0).clamp(0, i32::from(u16::MAX)) as u16,
                });
                current = span;
            }
        }
        out.push(x11xfixes::RegionRect {
            x: current.0.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16,
            y: y0.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16,
            width: (current.1 - current.0).clamp(0, i32::from(u16::MAX)) as u16,
            height: (y1 - y0).clamp(0, i32::from(u16::MAX)) as u16,
        });
    }
    out
}

pub(crate) fn region_extents(rects: &[x11xfixes::RegionRect]) -> x11xfixes::RegionRect {
    if rects.is_empty() {
        return x11xfixes::RegionRect {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        };
    }
    let mut x1 = i32::from(rects[0].x);
    let mut y1 = i32::from(rects[0].y);
    let mut x2 = i32::from(rects[0].x) + i32::from(rects[0].width);
    let mut y2 = i32::from(rects[0].y) + i32::from(rects[0].height);
    for rect in &rects[1..] {
        x1 = x1.min(i32::from(rect.x));
        y1 = y1.min(i32::from(rect.y));
        x2 = x2.max(i32::from(rect.x) + i32::from(rect.width));
        y2 = y2.max(i32::from(rect.y) + i32::from(rect.height));
    }
    x11xfixes::RegionRect {
        x: x1.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16,
        y: y1.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16,
        width: (x2 - x1).clamp(0, i32::from(u16::MAX)) as u16,
        height: (y2 - y1).clamp(0, i32::from(u16::MAX)) as u16,
    }
}

fn intersect_rect(
    a: x11xfixes::RegionRect,
    b: x11xfixes::RegionRect,
) -> Option<x11xfixes::RegionRect> {
    let x1 = i32::from(a.x).max(i32::from(b.x));
    let y1 = i32::from(a.y).max(i32::from(b.y));
    let x2 = (i32::from(a.x) + i32::from(a.width)).min(i32::from(b.x) + i32::from(b.width));
    let y2 = (i32::from(a.y) + i32::from(a.height)).min(i32::from(b.y) + i32::from(b.height));
    if x2 <= x1 || y2 <= y1 {
        return None;
    }
    Some(x11xfixes::RegionRect {
        x: x1.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16,
        y: y1.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16,
        width: (x2 - x1).clamp(0, i32::from(u16::MAX)) as u16,
        height: (y2 - y1).clamp(0, i32::from(u16::MAX)) as u16,
    })
}

pub(crate) fn intersect_regions(
    a: &[x11xfixes::RegionRect],
    b: &[x11xfixes::RegionRect],
) -> Vec<x11xfixes::RegionRect> {
    let mut out = Vec::new();
    for ar in a {
        for br in b {
            if let Some(rect) = intersect_rect(*ar, *br) {
                out.push(rect);
            }
        }
    }
    normalize_region_rects(out)
}

/// Union two normalized region rect sets.
///
/// This keeps the result YX-banded by subtracting the already-covered
/// area from each new source rect before appending it. That is enough
/// for the compositor/shape workloads here, which only ever feed us
/// normalized region lists.
pub(crate) fn union_regions(
    current: &[x11xfixes::RegionRect],
    source: &[x11xfixes::RegionRect],
) -> Vec<x11xfixes::RegionRect> {
    let mut result: Vec<x11xfixes::RegionRect> = current.to_vec();
    for s in source {
        let mut pieces = vec![*s];
        for existing in &result {
            let mut next = Vec::new();
            for piece in pieces {
                next.extend(subtract_rect(piece, *existing));
            }
            pieces = next;
            if pieces.is_empty() {
                break;
            }
        }
        result.extend(pieces);
    }
    normalize_region_rects(result)
}

/// Subtract one rectangle `b` from another `a`, returning the parts of
/// `a` not covered by `b`. Up to four sub-rectangles (top/bottom/left/right
/// strips) per call.
fn subtract_rect(a: x11xfixes::RegionRect, b: x11xfixes::RegionRect) -> Vec<x11xfixes::RegionRect> {
    let Some(isect) = intersect_rect(a, b) else {
        return vec![a];
    };
    let mut out = Vec::new();
    let a_right = i32::from(a.x) + i32::from(a.width);
    let a_bottom = i32::from(a.y) + i32::from(a.height);
    let isect_right = i32::from(isect.x) + i32::from(isect.width);
    let isect_bottom = i32::from(isect.y) + i32::from(isect.height);
    if i32::from(a.y) < i32::from(isect.y) {
        out.push(x11xfixes::RegionRect {
            x: a.x,
            y: a.y,
            width: a.width,
            height: (i32::from(isect.y) - i32::from(a.y)).clamp(0, i32::from(u16::MAX)) as u16,
        });
    }
    if isect_bottom < a_bottom {
        out.push(x11xfixes::RegionRect {
            x: a.x,
            y: isect_bottom.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16,
            width: a.width,
            height: (a_bottom - isect_bottom).clamp(0, i32::from(u16::MAX)) as u16,
        });
    }
    if i32::from(a.x) < i32::from(isect.x) {
        out.push(x11xfixes::RegionRect {
            x: a.x,
            y: isect.y,
            width: (i32::from(isect.x) - i32::from(a.x)).clamp(0, i32::from(u16::MAX)) as u16,
            height: isect.height,
        });
    }
    if isect_right < a_right {
        out.push(x11xfixes::RegionRect {
            x: isect_right.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16,
            y: isect.y,
            width: (a_right - isect_right).clamp(0, i32::from(u16::MAX)) as u16,
            height: isect.height,
        });
    }
    out
}

/// Subtract `source` (treated as a region union) from `current`.
/// Implements the X11 SHAPE Subtract op correctly: for each rect in
/// source, walk every accumulated rect and split off the parts not
/// covered. e16's rounded-corner popups rely on this — they Set 6 rects
/// for the body, then Subtract 6 small rects from the corners; the prior
/// implementation collapsed the result to either the unchanged input or
/// the empty set, which made the host see no shape at all.
pub(crate) fn subtract_regions(
    current: &[x11xfixes::RegionRect],
    source: &[x11xfixes::RegionRect],
) -> Vec<x11xfixes::RegionRect> {
    let mut result: Vec<x11xfixes::RegionRect> = current.to_vec();
    for s in source {
        let mut next = Vec::new();
        for r in result {
            next.extend(subtract_rect(r, *s));
        }
        result = next;
    }
    normalize_region_rects(result)
}

pub(crate) fn translate_region(rects: &mut [x11xfixes::RegionRect], dx: i16, dy: i16) {
    for rect in rects {
        rect.x = rect.x.saturating_add(dx);
        rect.y = rect.y.saturating_add(dy);
    }
}

/// Handle `GetAtomName` (opcode 17). Atom IDs in our protocol stream can come
/// from host-proxied replies (notably the `FONTPROP` atoms inside
/// `ListFontsWithInfo`), so a client can legitimately ask us about an atom
/// we never interned ourselves. Fall back to the host before returning
pub(crate) fn offset_rects(
    mut rects: Vec<x11xfixes::RegionRect>,
    dx: i16,
    dy: i16,
) -> Vec<x11xfixes::RegionRect> {
    translate_region(&mut rects, dx, dy);
    normalize_region_rects(rects)
}

pub(crate) fn default_shape_rect(
    server: &ServerState,
    window: ResourceId,
    kind: u8,
) -> x11xfixes::RegionRect {
    server.resources.window(window).map_or(
        x11xfixes::RegionRect {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        },
        |w| {
            // Per X11 SHAPE spec the default bounding region of an
            // unshaped window includes its border — origin
            // (-border_width, -border_width), extents
            // (width + 2*bw, height + 2*bw). The clip and input
            // regions exclude the border — origin (0, 0), extents
            // (width, height).
            if kind == x11shape::KIND_BOUNDING {
                let bw = i32::from(w.border_width);
                let x = (-bw).clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
                let y = x;
                let width = (i32::from(w.width) + 2 * bw).clamp(0, i32::from(u16::MAX)) as u16;
                let height = (i32::from(w.height) + 2 * bw).clamp(0, i32::from(u16::MAX)) as u16;
                x11xfixes::RegionRect {
                    x,
                    y,
                    width,
                    height,
                }
            } else {
                x11xfixes::RegionRect {
                    x: 0,
                    y: 0,
                    width: w.width,
                    height: w.height,
                }
            }
        },
    )
}

pub(crate) fn shape_rects_for(
    server: &ServerState,
    window: ResourceId,
    kind: u8,
) -> Vec<x11xfixes::RegionRect> {
    if let Some(rects) = server
        .shape_windows
        .get(&window)
        .and_then(|state| state.rects(kind).cloned())
    {
        return rects;
    }
    normalize_region_rects(vec![default_shape_rect(server, window, kind)])
}

pub(crate) fn shape_mask_source_rects(
    server: &ServerState,
    source: ResourceId,
) -> Vec<x11xfixes::RegionRect> {
    server
        .resources
        .pixmap(source)
        .map(|pixmap| {
            normalize_region_rects(vec![x11xfixes::RegionRect {
                x: 0,
                y: 0,
                width: pixmap.width,
                height: pixmap.height,
            }])
        })
        .unwrap_or_default()
}

/// Convert a depth-1 bitmap into YX-banded rectangles describing the set
/// pixels — Xorg's `BitmapToRegion` equivalent. `bytes` is tightly
/// packed, one byte per pixel (non-zero = set, zero = clear), row-major.
/// Returned rects are sorted by `y` then `x`, with rectangles in the
/// same horizontal band sharing identical `y`/`height`.
///
/// This is the rect representation X11 SHAPE expects clients to receive
/// from `GetRectangles` after `Mask` — without it, a mask of a circle
/// stored as a single bounding-box rect makes e16 fall back to a
/// recovery path that re-clears the shape.
#[must_use]
pub(crate) fn bitmap_to_yx_banded_rects(
    bytes: &[u8],
    width: u32,
    height: u32,
) -> Vec<x11xfixes::RegionRect> {
    let w = width as usize;
    let h = height as usize;
    if w == 0 || h == 0 || bytes.len() < w.saturating_mul(h) {
        return Vec::new();
    }

    // Per-row run-length encode the set pixels.
    let mut row_runs: Vec<Vec<(u32, u32)>> = Vec::with_capacity(h);
    for y in 0..h {
        let row = &bytes[y * w..y * w + w];
        let mut runs = Vec::new();
        let mut x = 0;
        while x < w {
            if row[x] != 0 {
                let start = x;
                while x < w && row[x] != 0 {
                    x += 1;
                }
                runs.push((start as u32, (x - start) as u32));
            } else {
                x += 1;
            }
        }
        row_runs.push(runs);
    }

    // Merge consecutive rows with identical run lists into bands.
    let mut rects = Vec::new();
    let mut y = 0usize;
    while y < h {
        if row_runs[y].is_empty() {
            y += 1;
            continue;
        }
        let mut y_end = y + 1;
        while y_end < h && row_runs[y_end] == row_runs[y] {
            y_end += 1;
        }
        let band_h = (y_end - y) as u16;
        for &(start, run_w) in &row_runs[y] {
            rects.push(x11xfixes::RegionRect {
                x: start as i16,
                y: y as i16,
                width: run_w as u16,
                height: band_h,
            });
        }
        y = y_end;
    }
    rects
}

pub(crate) fn shape_kind_is_set(server: &ServerState, window: ResourceId, kind: u8) -> bool {
    server
        .shape_windows
        .get(&window)
        .and_then(|state| state.rects(kind))
        .is_some()
}

pub(crate) fn apply_shape_op(
    current: Vec<x11xfixes::RegionRect>,
    source: Vec<x11xfixes::RegionRect>,
    op: u8,
) -> Vec<x11xfixes::RegionRect> {
    match op {
        x11shape::OP_SET => normalize_region_rects(source),
        x11shape::OP_UNION => union_regions(&current, &source),
        x11shape::OP_INTERSECT => intersect_regions(&current, &source),
        x11shape::OP_SUBTRACT => subtract_regions(&current, &source),
        x11shape::OP_INVERT => normalize_region_rects(source),
        _ => current,
    }
}

pub(crate) fn set_shape_rects(
    server: &mut ServerState,
    window: ResourceId,
    kind: u8,
    rects: Vec<x11xfixes::RegionRect>,
) {
    let state = server.shape_windows.entry(window).or_default();
    if let Some(slot) = state.rects_mut(kind) {
        *slot = Some(normalize_region_rects(rects));
    }
}

/// Resolve `window`'s host XID and current per-kind rect list, then forward
/// the resolved list to the host's SHAPE extension. No-op when the window has
/// no host backing (sub-windows below top-levels keep their local-only
/// behavior — the parent's host shape already clips them).
/// region. Used by e16 menu reparenting.
pub(crate) fn clear_shape_rects(server: &mut ServerState, window: ResourceId, kind: u8) {
    let Some(state) = server.shape_windows.get_mut(&window) else {
        return;
    };
    let Some(slot) = state.rects_mut(kind) else {
        return;
    };
    *slot = None;
    if state.bounding.is_none() && state.clip.is_none() && state.input.is_none() {
        server.shape_windows.remove(&window);
    }
}

// Subtract resets the per-object `pending_notify_fired` flag.

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
#[cfg(test)]
mod tests {
    use super::EXTENSIONS;

    #[test]
    fn extension_registry_major_opcodes_are_unique() {
        let major_opcodes = EXTENSIONS
            .iter()
            .map(|ext| ext.major_opcode)
            .collect::<Vec<_>>();
        let mut sorted = major_opcodes.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), major_opcodes.len());
    }

    #[test]
    fn extension_registry_event_ranges_do_not_overlap() {
        // Collect all (first_event, last_event_inclusive, name) for extensions
        // that have event_count > 0. Sort by first_event and verify each range
        // ends before the next one starts.
        let mut ranges: Vec<(u8, u8, &str)> = EXTENSIONS
            .iter()
            .filter(|ext| ext.event_count > 0 && ext.first_event > 0)
            .map(|ext| {
                (
                    ext.first_event,
                    ext.first_event + ext.event_count - 1,
                    ext.name,
                )
            })
            .collect();
        ranges.sort_by_key(|(base, _, _)| *base);
        for pair in ranges.windows(2) {
            let (_, end_a, name_a) = pair[0];
            let (start_b, _, name_b) = pair[1];
            assert!(
                end_a < start_b,
                "Extension event range collision: {name_a} ends at {end_a}, \
                 {name_b} starts at {start_b}"
            );
        }

        // Error bases must also be unique.
        let non_zero_error_bases: Vec<u8> = EXTENSIONS
            .iter()
            .map(|ext| ext.first_error)
            .filter(|base| *base != 0)
            .collect();
        let mut sorted = non_zero_error_bases.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            non_zero_error_bases.len(),
            "error base collision"
        );
    }

    #[test]
    fn xinerama_is_advertised() {
        let ext = EXTENSIONS
            .iter()
            .find(|ext| ext.name == "XINERAMA")
            .expect("XINERAMA in EXTENSIONS");
        assert_eq!(ext.major_opcode, 151);
        assert_eq!(ext.first_event, 0);
        assert_eq!(ext.first_error, 0);
    }

    mod render {
        use super::super::change_picture_translate_xids;

        // Helper to build a ChangePicture values slice with one CARD32 value.
        fn one_val(v: u32) -> [u8; 4] {
            v.to_le_bytes()
        }

        fn two_vals(a: u32, b: u32) -> [u8; 8] {
            let mut buf = [0u8; 8];
            buf[0..4].copy_from_slice(&a.to_le_bytes());
            buf[4..8].copy_from_slice(&b.to_le_bytes());
            buf
        }

        // ── ChangePicture XID translation ──────────────────────────────────

        #[test]
        fn translate_xids_passes_scalar_attrs_through() {
            // CPRepeat (bit 0) only — translator must never be invoked.
            let mut translator_called = false;
            let out = change_picture_translate_xids(0x01, &one_val(7), |_, _| {
                translator_called = true;
                Some(0)
            });
            assert_eq!(out, Some(one_val(7).to_vec()));
            assert!(!translator_called);
        }

        #[test]
        fn translate_xids_leaves_none_value_unchanged() {
            // CPClipMask=None (value=0) — no translation needed, just forward as-is.
            let out = change_picture_translate_xids(0x40, &one_val(0), |_, _| {
                panic!("translator should not be called for None XID")
            });
            assert_eq!(out, Some(one_val(0).to_vec()));
        }

        #[test]
        fn translate_xids_swaps_clip_mask_pixmap_to_host() {
            // CPClipMask = client pixmap 0x1234; translator returns host 0x4242.
            // The patched values slice should carry 0x4242 in the same slot.
            let out = change_picture_translate_xids(0x40, &one_val(0x1234), |attr, v| {
                assert!(matches!(attr, super::super::ChangePictureAttr::ClipMask));
                assert_eq!(v, 0x1234);
                Some(0x4242)
            });
            assert_eq!(out, Some(one_val(0x4242).to_vec()));
        }

        #[test]
        fn translate_xids_swaps_alpha_map_picture_to_host() {
            let out = change_picture_translate_xids(0x02, &one_val(0xdead), |attr, _| {
                assert!(matches!(attr, super::super::ChangePictureAttr::AlphaMap));
                Some(0xbeef)
            });
            assert_eq!(out, Some(one_val(0xbeef).to_vec()));
        }

        #[test]
        fn translate_xids_drops_when_translator_returns_none() {
            // Unknown XID → drop the request rather than forwarding a stale value.
            let out = change_picture_translate_xids(0x40, &one_val(0x9999), |_, _| None::<u32>);
            assert_eq!(out, None);
        }

        #[test]
        fn translate_xids_handles_repeat_plus_clip_mask_pixmap() {
            // CPRepeat (bit 0) + CPClipMask (bit 6): values in bit order are
            // [repeat, clip]. Translation must hit only the clip slot.
            let out = change_picture_translate_xids(0x41, &two_vals(1, 0x1234), |attr, _| {
                if matches!(attr, super::super::ChangePictureAttr::ClipMask) {
                    Some(0xbeef)
                } else {
                    panic!("only ClipMask should hit translator")
                }
            });
            assert_eq!(out, Some(two_vals(1, 0xbeef).to_vec()));
        }

        #[test]
        fn translate_xids_handles_alpha_map_and_clip_mask_together() {
            // CPAlphaMap (bit 1) + CPClipMask (bit 6): values in bit order:
            // [alpha_map, clip_mask]. Both XIDs should be translated.
            let out = change_picture_translate_xids(
                (1 << 1) | (1 << 6),
                &two_vals(0xa1, 0xc1),
                |attr, v| match attr {
                    super::super::ChangePictureAttr::AlphaMap => {
                        assert_eq!(v, 0xa1);
                        Some(0xa2)
                    }
                    super::super::ChangePictureAttr::ClipMask => {
                        assert_eq!(v, 0xc1);
                        Some(0xc2)
                    }
                },
            );
            assert_eq!(out, Some(two_vals(0xa2, 0xc2).to_vec()));
        }

        #[test]
        fn translate_xids_returns_none_on_short_values_with_xid_bit() {
            // value_mask has CPClipMask (bit 6) but values slice is empty.
            let out = change_picture_translate_xids(0x40, &[], |_, _| Some(0));
            assert_eq!(out, None);
        }

        // ── XIQueryPointer reply length ────────────────────────────────────────

        #[test]
        fn xi_query_pointer_extra_bytes_fit_6_length_units() {
            // GroupInfo is 4×CARD8 = 4 bytes (NOT 16 like ModifierInfo).
            // Extra payload: buttons_len(2) + pad(2) + ModifierInfo(16) + GroupInfo(4) = 24 bytes.
            // 24 bytes / 4 = 6 length units.
            let buttons_len_field = 2usize;
            let pad = 2usize;
            let modifier_info = 16usize; // base(4)+latched(4)+locked(4)+effective(4)
            let group_info = 4usize; // base(1)+latched(1)+locked(1)+effective(1)
            let extra = buttons_len_field + pad + modifier_info + group_info;
            assert_eq!(extra, 24, "extra payload must be 24 bytes");
            assert_eq!(extra % 4, 0, "must be 4-byte aligned");
            assert_eq!(extra / 4, 6, "length field must be 6");
        }

        // ── SetPictureClipRectangles offset adjustment ────────────────────────

        #[test]
        fn clip_origin_adjusted_by_window_offset() {
            // When the host picture sits at (x_off, y_off) inside the host container,
            // clip_x_origin and clip_y_origin must be adjusted so the clip aligns with
            // Composite's dst_x/dst_y which are also shifted by (x_off, y_off).
            let x_off: i16 = 100;
            let y_off: i16 = 50;
            let mut body = [0u8; 16];
            body[4..6].copy_from_slice(&10i16.to_le_bytes());
            body[6..8].copy_from_slice(&20i16.to_le_bytes());
            let adj_x = i16::from_le_bytes([body[4], body[5]]).wrapping_add(x_off);
            let adj_y = i16::from_le_bytes([body[6], body[7]]).wrapping_add(y_off);
            assert_eq!(adj_x, 110);
            assert_eq!(adj_y, 70);
        }

        #[test]
        fn clip_origin_zero_offset_unchanged() {
            // Pixmap-backed pictures have x_off=y_off=0; clip must pass through unmodified.
            let x_off: i16 = 0;
            let y_off: i16 = 0;
            let mut body = [0u8; 16];
            body[4..6].copy_from_slice(&(-5i16).to_le_bytes());
            body[6..8].copy_from_slice(&30i16.to_le_bytes());
            let adj_x = i16::from_le_bytes([body[4], body[5]]).wrapping_add(x_off);
            let adj_y = i16::from_le_bytes([body[6], body[7]]).wrapping_add(y_off);
            assert_eq!(adj_x, -5);
            assert_eq!(adj_y, 30);
        }
    }

    mod xfixes_ops {
        use super::super::{
            clear_shape_rects, intersect_regions, normalize_region_rects, region_extents,
            shape_kind_is_set, shape_mask_source_rects, shape_rects_for, translate_region,
            union_regions,
        };
        use crate::{resources::ROOT_WINDOW, server::ServerState};
        use yserver_protocol::x11::{
            ClientId, CreatePixmapRequest, ResourceId, shape, xfixes::RegionRect,
        };

        fn r(x: i16, y: i16, w: u16, h: u16) -> RegionRect {
            RegionRect {
                x,
                y,
                width: w,
                height: h,
            }
        }

        #[test]
        fn normalize_removes_empty_rects() {
            let input = vec![r(0, 0, 0, 5), r(1, 2, 3, 4), r(5, 5, 1, 0)];
            assert_eq!(normalize_region_rects(input), vec![r(1, 2, 3, 4)]);
        }

        #[test]
        fn normalize_preserves_rects_beyond_old_cap() {
            let rects: Vec<RegionRect> = (0..4097).map(|i| r(0, i as i16, 1, 1)).collect();
            let normalized = normalize_region_rects(rects);
            assert_eq!(normalized.len(), 4097);
            assert_eq!(normalized.first().copied(), Some(r(0, 0, 1, 1)));
            assert_eq!(normalized.last().copied(), Some(r(0, 4096, 1, 1)));
        }

        #[test]
        fn normalize_dedups_exact_duplicates() {
            let input = vec![r(1, 2, 3, 4), r(1, 2, 3, 4), r(5, 6, 7, 8)];
            assert_eq!(
                normalize_region_rects(input),
                vec![r(1, 2, 3, 4), r(5, 6, 7, 8)]
            );
        }

        #[test]
        fn normalize_merges_overlapping_bands() {
            let input = vec![r(0, 0, 10, 10), r(5, 0, 10, 10)];
            assert_eq!(normalize_region_rects(input), vec![r(0, 0, 15, 10)]);
        }

        #[test]
        fn region_extents_empty_returns_zero() {
            assert_eq!(region_extents(&[]), r(0, 0, 0, 0));
        }

        #[test]
        fn region_extents_single_passthrough() {
            let rect = r(3, 4, 10, 20);
            assert_eq!(region_extents(&[rect]), rect);
        }

        #[test]
        fn region_extents_bounding_box() {
            let rects = vec![r(0, 0, 10, 10), r(5, 5, 10, 10)];
            assert_eq!(region_extents(&rects), r(0, 0, 15, 15));
        }

        #[test]
        fn intersect_overlapping() {
            let a = vec![r(0, 0, 10, 10)];
            let b = vec![r(5, 5, 10, 10)];
            assert_eq!(intersect_regions(&a, &b), vec![r(5, 5, 5, 5)]);
        }

        #[test]
        fn intersect_non_overlapping_is_empty() {
            let a = vec![r(0, 0, 5, 5)];
            let b = vec![r(10, 10, 5, 5)];
            assert!(intersect_regions(&a, &b).is_empty());
        }

        #[test]
        fn intersect_with_empty_region_is_empty() {
            let empty: Vec<RegionRect> = vec![];
            let nonempty = vec![r(0, 0, 10, 10)];
            assert!(intersect_regions(&empty, &nonempty).is_empty());
            assert!(intersect_regions(&nonempty, &empty).is_empty());
        }

        #[test]
        fn union_merges_disjoint_rects() {
            let a = vec![r(0, 0, 10, 10)];
            let b = vec![r(20, 0, 10, 10)];
            assert_eq!(
                union_regions(&a, &b),
                vec![r(0, 0, 10, 10), r(20, 0, 10, 10)]
            );
        }

        #[test]
        fn union_preserves_containing_rect() {
            let a = vec![r(0, 0, 100, 100)];
            let b = vec![r(10, 10, 5, 5)];
            assert_eq!(union_regions(&a, &b), vec![r(0, 0, 100, 100)]);
        }

        #[test]
        fn translate_shifts_coords() {
            let mut rects = vec![r(10, 20, 5, 5)];
            translate_region(&mut rects, 3, -5);
            assert_eq!(rects[0], r(13, 15, 5, 5));
        }

        #[test]
        fn translate_saturates_at_bounds() {
            let mut rects = vec![r(i16::MAX, i16::MIN, 1, 1)];
            translate_region(&mut rects, 100, -100);
            assert_eq!(rects[0].x, i16::MAX);
            assert_eq!(rects[0].y, i16::MIN);
        }

        #[test]
        fn shape_mask_source_uses_pixmap_geometry() {
            let mut server = ServerState::new();
            let pixmap = ResourceId(0x200);
            server.resources.create_pixmap(
                ClientId(1),
                CreatePixmapRequest {
                    depth: 1,
                    pixmap,
                    drawable: ROOT_WINDOW,
                    width: 17,
                    height: 23,
                },
            );

            assert_eq!(
                shape_mask_source_rects(&server, pixmap),
                vec![r(0, 0, 17, 23)]
            );
        }

        #[test]
        fn clear_shape_rects_reverts_to_default_region() {
            let mut server = ServerState::new();
            let window = ROOT_WINDOW;
            server.shape_windows.entry(window).or_default().bounding = Some(vec![r(1, 2, 3, 4)]);

            assert!(shape_kind_is_set(&server, window, shape::KIND_BOUNDING));
            clear_shape_rects(&mut server, window, shape::KIND_BOUNDING);

            assert!(!shape_kind_is_set(&server, window, shape::KIND_BOUNDING));
            assert_eq!(
                shape_rects_for(&server, window, shape::KIND_BOUNDING),
                vec![r(0, 0, 800, 600)]
            );
        }

        #[test]
        fn cow_input_shape_defaults_to_full_extent_like_xorg() {
            use crate::resources::COMPOSITE_OVERLAY_WINDOW as COW;
            let mut server = ServerState::new();
            let host_xid = crate::backend::WindowHandle::from_raw_panicking(0x4000_0103);
            server.resources.materialize_cow_resource(host_xid);

            assert_eq!(
                shape_rects_for(&server, COW, shape::KIND_INPUT),
                vec![r(0, 0, 800, 600)]
            );

            server.shape_windows.entry(COW).or_default().input = Some(vec![r(0, 0, 2560, 28)]);
            assert_eq!(
                shape_rects_for(&server, COW, shape::KIND_INPUT),
                vec![r(0, 0, 2560, 28)]
            );

            assert_eq!(
                shape_rects_for(&server, ROOT_WINDOW, shape::KIND_INPUT),
                vec![r(0, 0, 800, 600)]
            );
        }
    }

    mod root_resize {
        //! `handle_host_container_resize` post-conditions, including
        //! `ConfigureNotify` delivery to clients that selected
        //! `StructureNotify` on root *without* `RRSelectInput`.
        use std::{
            collections::{HashMap, HashSet},
            io::Read,
            os::unix::net::UnixStream,
            sync::{Arc, Mutex, atomic::AtomicU16},
        };

        use crate::{
            core_loop::run::handle_host_container_resize,
            host_x11::HostConfigureEvent,
            resources::ROOT_WINDOW,
            server::{ClientState, ServerState},
        };
        use yserver_protocol::x11::ClientByteOrder;

        const STRUCTURE_NOTIFY_MASK: u32 = 0x0002_0000;

        fn server_with_root_listener() -> (ServerState, UnixStream) {
            let mut state = ServerState::new();
            let (writer_local, reader_remote) = UnixStream::pair().expect("socketpair");
            state.clients.insert(
                1,
                ClientState {
                    writer: Arc::new(Mutex::new(writer_local)),
                    byte_order: ClientByteOrder::LittleEndian,
                    last_sequence: Arc::new(AtomicU16::new(0)),
                    resource_id_base: 0x0010_0000,
                    resource_id_mask: 0x000F_FFFF,
                    event_masks: HashMap::from([(ROOT_WINDOW, STRUCTURE_NOTIFY_MASK)]),
                    save_set: HashSet::new(),
                    big_requests_enabled: false,
                    xi2_masks: HashMap::new(),
                    xi1_event_classes: HashSet::new(),
                    xi1_window_event_classes: HashMap::new(),
                    outbound: std::collections::VecDeque::new(),
                    watching_writable: false,
                    focused_window: crate::resources::ROOT_WINDOW,
                    reader_control: None,
                },
            );
            (state, reader_remote)
        }

        #[test]
        fn resize_updates_state_and_root_geometry() {
            let (mut state, _reader) = server_with_root_listener();
            handle_host_container_resize(
                &mut state,
                HostConfigureEvent {
                    host_xid: 0xdead_beef,
                    x: 0,
                    y: 0,
                    width: 1024,
                    height: 768,
                },
            );
            assert_eq!(state.randr.screen_width, 1024);
            assert_eq!(state.randr.screen_height, 768);
            let root = state.resources.window(ROOT_WINDOW).expect("root window");
            assert_eq!(root.width, 1024);
            assert_eq!(root.height, 768);
        }

        #[test]
        fn structure_notify_listener_gets_configure_notify() {
            let (mut state, mut reader) = server_with_root_listener();

            handle_host_container_resize(
                &mut state,
                HostConfigureEvent {
                    host_xid: 0xdead_beef,
                    x: 0,
                    y: 0,
                    width: 1024,
                    height: 768,
                },
            );

            // Drain everything currently buffered. The first 32 bytes must be a
            // ConfigureNotify (event type 22) on root with the new dimensions.
            reader.set_nonblocking(true).expect("set non-blocking");
            let mut buf = [0u8; 32];
            reader.read_exact(&mut buf).expect("event byte block");
            assert_eq!(buf[0], 22, "event type 22 = ConfigureNotify");
            // Bytes 4..8 = event_window, 8..12 = window. Both must be ROOT_WINDOW.
            let event_window = u32::from_le_bytes(buf[4..8].try_into().unwrap());
            let window = u32::from_le_bytes(buf[8..12].try_into().unwrap());
            assert_eq!(event_window, ROOT_WINDOW.0);
            assert_eq!(window, ROOT_WINDOW.0);
            // Width @ bytes 20..22, height @ bytes 22..24 (after above_sibling
            // u32 + x i16 + y i16).
            let width = u16::from_le_bytes(buf[20..22].try_into().unwrap());
            let height = u16::from_le_bytes(buf[22..24].try_into().unwrap());
            assert_eq!(width, 1024);
            assert_eq!(height, 768);
        }
    }
}
