//! The built-in window-management policy (spec §6): EWMH announce, ICCCM
//! `WM_STATE`, client lists, root ClientMessages, SubstructureRedirect refusal.

use yserver_protocol::x11::{
    AtomId, ChangeWindowAttributesRequest, ClientByteOrder, ConfigureWindowRequest,
    CreateWindowRequest, ResourceId,
};

use super::{
    Rootless,
    acks::ConfigureSource,
    info::{current_net_wm_state, transient_for_window},
    topology::is_toplevel,
    types::{Layer, ResizeEdge, RootlessEvent, ToplevelKind, WmRequest},
};
use crate::{
    backend::Backend,
    core_loop::process_request::change_property_as_server,
    properties::PropertyFormat,
    resources::{ROOT_WINDOW, ROOTLESS_WM_CHECK_WINDOW, SERVER_OWNER},
    server::ServerState,
};

/// `_NET_SUPPORTED`: exactly what the policy handles (spec §6.1, §6.3).
const SUPPORTED: &[&str] = &[
    "_NET_SUPPORTED",
    "_NET_SUPPORTING_WM_CHECK",
    "_NET_WM_NAME",
    "_NET_ACTIVE_WINDOW",
    "_NET_CLIENT_LIST",
    "_NET_CLIENT_LIST_STACKING",
    "_NET_WM_STATE",
    "_NET_WM_STATE_FULLSCREEN",
    "_NET_WM_STATE_MAXIMIZED_VERT",
    "_NET_WM_STATE_MAXIMIZED_HORZ",
    "_NET_WM_STATE_ABOVE",
    "_NET_WM_STATE_BELOW",
    "_NET_WM_STATE_HIDDEN",
    "_NET_WM_MOVERESIZE",
    "_NET_CLOSE_WINDOW",
    "_NET_FRAME_EXTENTS",
    "_NET_REQUEST_FRAME_EXTENTS",
];

/// ICCCM §4.1.3.1 states.
const WITHDRAWN_STATE: u32 = 0;
const NORMAL_STATE: u32 = 1;

fn le_words(words: &[u32]) -> Vec<u8> {
    words.iter().flat_map(|w| w.to_le_bytes()).collect()
}

pub(super) fn set_words(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    window: ResourceId,
    property: &str,
    r#type: &str,
    words: &[u32],
) {
    let property = state.atoms.intern(property, false);
    let r#type = state.atoms.intern(r#type, false);
    change_property_as_server(
        state,
        backend,
        window,
        property,
        r#type,
        PropertyFormat::F32,
        le_words(words),
    );
}

/// A server-owned InputOnly window, unmapped (no wire body, no backend
/// window: InputOnly has no storage).
fn create_input_only_as_server(state: &mut ServerState, window: ResourceId) {
    state.resources.create_window(
        SERVER_OWNER,
        CreateWindowRequest {
            window,
            parent: ROOT_WINDOW,
            x: -1,
            y: -1,
            width: 1,
            height: 1,
            class: 2,
            override_redirect: Some(true),
            ..Default::default()
        },
    );
}

/// EWMH §3.1/§3.13 and ICCCM §2.8: announce the built-in WM.
pub(super) fn announce(state: &mut ServerState, backend: &mut dyn Backend) {
    if state.resources.window(ROOTLESS_WM_CHECK_WINDOW).is_none() {
        create_input_only_as_server(state, ROOTLESS_WM_CHECK_WINDOW);
    }
    let check = ROOTLESS_WM_CHECK_WINDOW;
    for window in [ROOT_WINDOW, check] {
        set_words(
            state,
            backend,
            window,
            "_NET_SUPPORTING_WM_CHECK",
            "WINDOW",
            &[check.0],
        );
    }
    let name = state.atoms.intern("_NET_WM_NAME", false);
    let utf8 = state.atoms.intern("UTF8_STRING", false);
    change_property_as_server(
        state,
        backend,
        check,
        name,
        utf8,
        PropertyFormat::F8,
        b"yserver".to_vec(),
    );
    let supported: Vec<u32> = SUPPORTED
        .iter()
        .map(|n| state.atoms.intern(n, false).0)
        .collect();
    set_words(
        state,
        backend,
        ROOT_WINDOW,
        "_NET_SUPPORTED",
        "ATOM",
        &supported,
    );
    let wm_s0 = state.atoms.intern("WM_S0", false);
    let now = state.timestamp_now();
    state.selections.insert(wm_s0, (check, now));
}

/// `_NET_FRAME_EXTENTS` (EWMH §5.11): native decorations sit outside the X
/// window, so left, right, top, bottom are all 0.
fn set_frame_extents(state: &mut ServerState, backend: &mut dyn Backend, window: ResourceId) {
    set_words(
        state,
        backend,
        window,
        "_NET_FRAME_EXTENTS",
        "CARDINAL",
        &[0; 4],
    );
}

/// After reconcile: `WM_STATE` on managed windows and the client lists
/// (ICCCM §4.1.3.1, EWMH §3.2).
pub(super) fn apply_policy(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    rootless: &mut Rootless,
    events: &[RootlessEvent],
) {
    for event in events {
        if let RootlessEvent::Mapped {
            window,
            kind: ToplevelKind::Managed,
            ..
        } = event
        {
            set_words(
                state,
                backend,
                *window,
                "WM_STATE",
                "WM_STATE",
                &[NORMAL_STATE, 0],
            );
            set_frame_extents(state, backend, *window);
        }
    }
    for window in std::mem::take(&mut rootless.withdrawn) {
        if state.resources.window(window).is_some() {
            set_words(
                state,
                backend,
                window,
                "WM_STATE",
                "WM_STATE",
                &[WITHDRAWN_STATE, 0],
            );
        }
    }
    let managed = |w: &ResourceId| {
        rootless
            .toplevels
            .get(w)
            .is_some_and(|t| t.kind == ToplevelKind::Managed)
    };
    rootless.client_list.retain(|w| managed(w));
    let stacking: Vec<u32> = rootless
        .order
        .iter()
        .filter(|w| managed(w))
        .map(|w| w.0)
        .collect();
    let list: Vec<u32> = rootless.client_list.iter().map(|w| w.0).collect();
    if list != rootless.written_client_list {
        set_words(
            state,
            backend,
            ROOT_WINDOW,
            "_NET_CLIENT_LIST",
            "WINDOW",
            &list,
        );
        rootless.written_client_list = list;
    }
    if stacking != rootless.written_stacking {
        set_words(
            state,
            backend,
            ROOT_WINDOW,
            "_NET_CLIENT_LIST_STACKING",
            "WINDOW",
            &stacking,
        );
        rootless.written_stacking = stacking;
    }
}

/// Spec §6.1, Global only: a Managed window about to be reported `Mapped`
/// with `WM_TRANSIENT_FOR` naming a Managed top-level and neither USPosition
/// nor PPosition is centred on its parent's outer rect, by a Policy
/// configure, so `Mapped` already carries the position.
pub(super) fn place_transients(state: &mut ServerState, backend: &mut dyn Backend) {
    let Some(r) = state.rootless.as_ref() else {
        return;
    };
    if r.policy() == super::CoordinatePolicy::Opaque {
        return;
    }
    let managed_toplevel = |w: ResourceId| {
        is_toplevel(state, w)
            && state
                .resources
                .window(w)
                .is_some_and(|w| !w.override_redirect)
    };
    let outer = |w: ResourceId| {
        state.resources.window(w).map(|w| {
            let bw2 = 2 * i32::from(w.border_width);
            (
                i32::from(w.x),
                i32::from(w.y),
                i32::from(w.width) + bw2,
                i32::from(w.height) + bw2,
            )
        })
    };
    let mut moves = Vec::new();
    for w in state.resources.children(ROOT_WINDOW) {
        if r.toplevels.contains_key(w) || !managed_toplevel(*w) {
            continue;
        }
        let Some(parent) = transient_for_window(state, *w).filter(|p| managed_toplevel(*p)) else {
            continue;
        };
        let hints = super::info::size_hints_of(state, *w);
        if hints.us_position || hints.p_position {
            continue;
        }
        let (Some((px, py, pw, ph)), Some((_, _, dw, dh))) = (outer(parent), outer(*w)) else {
            continue;
        };
        moves.push((*w, px + (pw - dw) / 2, py + (ph - dh) / 2));
    }
    for (window, x, y) in moves {
        let clamp = |v: i32| i16::try_from(v).unwrap_or(if v < 0 { i16::MIN } else { i16::MAX });
        let request = ConfigureWindowRequest {
            window,
            value_mask: 0x3,
            x: Some(clamp(x)),
            y: Some(clamp(y)),
            width: None,
            height: None,
            border_width: None,
            sibling: None,
            stack_mode: None,
        };
        super::host::configure_window_as_server(state, backend, request, ConfigureSource::Policy);
    }
}

/// Spec §6.5: in rootless mode nobody may select SubstructureRedirect or
/// ResizeRedirect on the root. Strips CWEventMask and every later attribute,
/// as Xorg's `ChangeWindowAttributes` stops at the failing bit
/// (`dix/window.c:1372-1377`, :1569-1574); `true` means "apply the rest,
/// then BadAccess".
pub(crate) fn refuse_root_redirect(
    state: &ServerState,
    request: &mut ChangeWindowAttributesRequest,
) -> bool {
    const REDIRECT: u32 = 0x0010_0000 | 0x0004_0000;
    if state.rootless.is_none()
        || request.window != ROOT_WINDOW
        || request.event_mask.is_none_or(|m| m & REDIRECT == 0)
    {
        return false;
    }
    request.truncate_at_event_mask();
    true
}

fn word(event: &[u8; 32], at: usize, order: ClientByteOrder) -> u32 {
    let b = [event[at], event[at + 1], event[at + 2], event[at + 3]];
    match order {
        ClientByteOrder::BigEndian => u32::from_be_bytes(b),
        ClientByteOrder::LittleEndian => u32::from_le_bytes(b),
    }
}

/// `_NET_WM_STATE` (EWMH §5.7): l0 remove/add/toggle, l1/l2 the states.
/// Toggle resolves against the window's current `_NET_WM_STATE`.
fn net_wm_state_requests(state: &ServerState, window: ResourceId, l: [u32; 5]) -> Vec<WmRequest> {
    let current = current_net_wm_state(state, window);
    let set = |cur: bool| match l[0] {
        0 => Some(false),
        1 => Some(true),
        2 => Some(!cur),
        _ => None,
    };
    let names: Vec<&str> = [l[1], l[2]]
        .into_iter()
        .filter(|a| *a != 0)
        .filter_map(|a| state.atoms.name(AtomId(a)))
        .collect();
    let has = |n: &str| names.contains(&n);
    let mut out = Vec::new();
    if has("_NET_WM_STATE_FULLSCREEN")
        && let Some(v) = set(current.fullscreen)
    {
        out.push(WmRequest::SetFullscreen(v));
    }
    let (v, h) = (
        has("_NET_WM_STATE_MAXIMIZED_VERT"),
        has("_NET_WM_STATE_MAXIMIZED_HORZ"),
    );
    if v || h {
        let nv = if v {
            set(current.maximized_vert)
        } else {
            Some(current.maximized_vert)
        };
        let nh = if h {
            set(current.maximized_horz)
        } else {
            Some(current.maximized_horz)
        };
        if let (Some(nv), Some(nh)) = (nv, nh)
            && nv == nh
        {
            out.push(WmRequest::SetMaximized(nv));
        }
    }
    for (name, cur, layer) in [
        ("_NET_WM_STATE_ABOVE", current.above, Layer::Above),
        ("_NET_WM_STATE_BELOW", current.below, Layer::Below),
    ] {
        if has(name)
            && let Some(on) = set(cur)
        {
            out.push(WmRequest::SetLayer(if on { layer } else { Layer::Normal }));
        }
    }
    if has("_NET_WM_STATE_HIDDEN") && set(current.hidden) == Some(true) {
        out.push(WmRequest::Minimize);
    }
    out
}

/// `_NET_WM_MOVERESIZE` (EWMH §4.3): l2 direction, l3 button.
fn moveresize_request(l: [u32; 5]) -> Option<WmRequest> {
    const EDGES: [ResizeEdge; 8] = [
        ResizeEdge::TopLeft,
        ResizeEdge::Top,
        ResizeEdge::TopRight,
        ResizeEdge::Right,
        ResizeEdge::BottomRight,
        ResizeEdge::Bottom,
        ResizeEdge::BottomLeft,
        ResizeEdge::Left,
    ];
    let button = u8::try_from(l[3]).unwrap_or(0);
    match l[2] {
        d @ 0..=7 => Some(WmRequest::BeginResize {
            edge: EDGES[d as usize],
            button,
        }),
        8 => Some(WmRequest::BeginMove { button }),
        // _NET_WM_MOVERESIZE_SIZE_KEYBOARD / _MOVE_KEYBOARD
        9 => Some(WmRequest::BeginResize {
            edge: ResizeEdge::BottomRight,
            button: 0,
        }),
        10 => Some(WmRequest::BeginMove { button: 0 }),
        _ => None,
    }
}

/// A ClientMessage sent to the root (spec §6.3), after its normal delivery.
/// The template is in the sender's byte order (`selection.rs`).
pub(crate) fn note_root_client_message(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    destination: ResourceId,
    event: &[u8; 32],
    order: ClientByteOrder,
) {
    if state.rootless.is_none()
        || destination != ROOT_WINDOW
        || event[0] & 0x7f != 33
        || event[1] != 32
    {
        return;
    }
    let window = ResourceId(word(event, 4, order));
    let Some(r#type) = state.atoms.name(AtomId(word(event, 8, order))) else {
        return;
    };
    if r#type == "_NET_REQUEST_FRAME_EXTENTS" {
        // EWMH: allowed before map; the client waits for the property.
        if state.resources.window(window).is_some() {
            set_frame_extents(state, backend, window);
        }
        return;
    }
    let l = [0, 1, 2, 3, 4].map(|i| word(event, 12 + 4 * i, order));
    let requests = match r#type {
        "_NET_WM_STATE" => net_wm_state_requests(state, window, l),
        "WM_CHANGE_STATE" if l[0] == 3 => vec![WmRequest::Minimize],
        "_NET_WM_MOVERESIZE" => moveresize_request(l).into_iter().collect(),
        "_NET_ACTIVE_WINDOW" => vec![WmRequest::Activate],
        "_NET_CLOSE_WINDOW" => vec![WmRequest::Close],
        _ => Vec::new(),
    };
    if let Some(rootless) = state.rootless.as_mut() {
        rootless
            .pending_wm
            .extend(requests.into_iter().map(|r| (window, r)));
    }
}
