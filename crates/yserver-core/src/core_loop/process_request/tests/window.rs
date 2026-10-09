use super::*;

fn count_configure_notifies(bytes: &[u8]) -> usize {
    // X11 events are 32 bytes; ConfigureNotify event code is 22.
    bytes.chunks(32).filter(|e| e[0] & 0x7f == 22).count()
}

#[test]
fn configure_window_noop_restack_emits_no_configure_notify() {
    // Regression (Enlightenment e27 restack war): a stacking-only
    // ConfigureWindow that does NOT change the child order must emit
    // zero ConfigureNotify, matching Xorg dix/window.c. A spurious
    // notify makes the WM re-restack forever (~8000 req/s observed),
    // pegging the desktop so clicks never get processed.
    let (mut state, mut peer) = two_children_under_root();
    // 0x300 was created last → already on top. Subscribe to its
    // StructureNotify, then drain any setup traffic.
    state
        .clients
        .get_mut(&1)
        .unwrap()
        .event_masks
        .insert(ResourceId(0x300), 0x0002_0000);
    let _ = read_all_available(&mut peer);

    // Raise the already-top window to the top: a positional no-op.
    let body = cw_restack_body(0x300, 0x0040, &[0]); // CWStackMode=Above
    let mut backend = RecordingBackend::new();
    handle_configure_window(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        &body,
    )
    .expect("handle_configure_window");

    let out = read_all_available(&mut peer);
    assert_eq!(
        count_configure_notifies(&out),
        0,
        "no-op restack must not emit ConfigureNotify (got {} bytes)",
        out.len()
    );
}

#[test]
fn configure_window_real_restack_emits_one_configure_notify() {
    // Guard against over-suppression: a restack that DOES reorder the
    // children must still emit exactly one ConfigureNotify.
    let (mut state, mut peer) = two_children_under_root();
    // 0x200 was created first → currently at the bottom.
    state
        .clients
        .get_mut(&1)
        .unwrap()
        .event_masks
        .insert(ResourceId(0x200), 0x0002_0000);
    let _ = read_all_available(&mut peer);

    // Raise 0x200 to the top: a real reorder.
    let body = cw_restack_body(0x200, 0x0040, &[0]); // CWStackMode=Above
    let mut backend = RecordingBackend::new();
    handle_configure_window(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        &body,
    )
    .expect("handle_configure_window");

    let out = read_all_available(&mut peer);
    assert_eq!(
        count_configure_notifies(&out),
        1,
        "a real restack must emit exactly one ConfigureNotify"
    );
}

// ── #143 — a resize must report the window exposed, either way ──
//
// Xorg's `miResizeWindow` copies the NEW clip list wholesale into
// `after.exposed` — "the entire window is trashed unless bitGravity
// recovers portions of it" (`mi/miwindow.c:466-472`) — and only a
// non-Forget `bitGravity` subtracts the bits it actually moved
// (`:596-599`). There is no grow/shrink branch: under the default
// ForgetGravity the full window is reported in both directions.
// Nor does the background gate the EVENT — `miWindowExposures`
// paints, then sends (`mi/miexpose.c:387-389`), and the
// background-None early-out is inside the paint (`:438-440`).
//
// We emitted this for a grow only. On HW that is the #143 xterm:
// awesome retiles it smaller, the leaf is re-tiled from the
// window's black background, and with no Expose nothing asks xterm
// to repaint its static banner rows — they stay black while the
// prompt line, which xterm redraws unprompted, comes back.

/// One viewable, Expose-selecting child of root at `w`x`h`, plus
/// client 1's already-drained peer socket. `background_pixel` picks
/// the two shapes that matter here: `Some` is the xterm case (the
/// resize re-tiles the leaf and destroys the content), `None` is the
/// background-None case (the content survives).
fn one_viewable_expose_child(
    w: u16,
    h: u16,
    background_pixel: Option<u32>,
) -> (ServerState, UnixStream) {
    const XID: u32 = 0x400;
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    state.resources.create_window(
        ClientId(1),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(XID),
            parent: ROOT_WINDOW,
            width: w,
            height: h,
            background_pixel,
            ..Default::default()
        },
    );
    assert_eq!(
        state
            .resources
            .window(ResourceId(XID))
            .map(|win| win.background_none),
        Some(background_pixel.is_none()),
        "harness sanity: the background shape under test",
    );
    state
        .resources
        .window_mut(ResourceId(XID))
        .expect("child installed")
        .map_state = crate::resources::MapState::Viewable;
    state
        .clients
        .get_mut(&1)
        .expect("test client")
        .event_masks
        .insert(ResourceId(XID), 0x0000_8000); // ExposureMask
    let _ = read_all_available(&mut peer);
    (state, peer)
}

/// Every Expose in `bytes` for `window`, as `(x, y, width, height)`.
fn expose_rects(bytes: &[u8], window: u32) -> Vec<(u16, u16, u16, u16)> {
    bytes
        .chunks(32)
        .filter(|e| {
            e.len() == 32
                && e[0] & 0x7f == 12
                && u32::from_le_bytes([e[4], e[5], e[6], e[7]]) == window
        })
        .map(|e| {
            (
                u16::from_le_bytes([e[8], e[9]]),
                u16::from_le_bytes([e[10], e[11]]),
                u16::from_le_bytes([e[12], e[13]]),
                u16::from_le_bytes([e[14], e[15]]),
            )
        })
        .collect()
}

/// Resize `window` to `w`x`h` (CWWidth|CWHeight) and return whatever
/// reached the client.
fn resize_and_read(
    state: &mut ServerState,
    peer: &mut UnixStream,
    window: u32,
    w: u32,
    h: u32,
) -> Vec<u8> {
    let body = cw_restack_body(window, 0x000C, &[w, h]);
    let mut backend = RecordingBackend::new();
    handle_configure_window(
        state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        &body,
    )
    .expect("handle_configure_window");
    read_all_available(peer)
}

#[test]
fn configure_window_shrink_exposes_the_whole_window() {
    // The #143 case. `configure_subwindow` re-tiles the leaf from
    // the window's background on a shrink, so without this Expose
    // the discarded pixels are unrecoverable.
    let (mut state, mut peer) = one_viewable_expose_child(200, 100, Some(0x0000_0000));
    let out = resize_and_read(&mut state, &mut peer, 0x400, 100, 60);
    assert_eq!(
        expose_rects(&out, 0x400),
        vec![(0, 0, 100, 60)],
        "a shrink must report the whole NEW window exposed, exactly once \
             (`mi/miwindow.c:466-472`); emitting none leaves an idle client \
             showing the background fill forever (#143)",
    );
}

#[test]
fn configure_window_grow_exposes_the_whole_window() {
    // Over-suppression guard: the grow path predates the shrink one
    // and must keep its single full-window Expose.
    let (mut state, mut peer) = one_viewable_expose_child(100, 60, Some(0x0000_0000));
    let out = resize_and_read(&mut state, &mut peer, 0x400, 200, 100);
    assert_eq!(
        expose_rects(&out, 0x400),
        vec![(0, 0, 200, 100)],
        "a grow must still report exactly one full-window Expose",
    );
}

#[test]
fn configure_window_shrink_exposes_a_background_none_window_too() {
    // A background-None window keeps its pixels across the resize
    // (`mi/miexpose.c:438-440` returns before painting) but STILL
    // gets the Expose: `miWindowExposures` calls `PaintWindow` and
    // `miSendExposures` in sequence and only the paint is skipped
    // (`mi/miexpose.c:387-389`). Nothing on this path may start
    // reading the background state to suppress the event.
    let (mut state, mut peer) = one_viewable_expose_child(200, 100, None);
    let out = resize_and_read(&mut state, &mut peer, 0x400, 100, 60);
    assert_eq!(
        expose_rects(&out, 0x400),
        vec![(0, 0, 100, 60)],
        "a background-None window is not painted but is still told what \
             was exposed",
    );
}

#[test]
fn configure_window_pure_move_emits_no_expose() {
    // Under-suppression guard: `after.exposed` is seeded from the
    // clip list only when the size changes; a pure move of an
    // unobscured window exposes nothing of the window itself.
    let (mut state, mut peer) = one_viewable_expose_child(200, 100, Some(0x0000_0000));
    let body = cw_restack_body(0x400, 0x0003, &[37, 41]); // CWX|CWY
    let mut backend = RecordingBackend::new();
    handle_configure_window(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        &body,
    )
    .expect("handle_configure_window");
    let out = read_all_available(&mut peer);
    assert_eq!(
        expose_rects(&out, 0x400),
        Vec::new(),
        "a move is not a resize and must not expose the window",
    );
}

/// ProcTranslateCoords applies child input shapes. The Composite Overlay
/// Window is the top root child and, like Xorg's, starts unshaped, so it
/// is the `child` over a popup until the compositor empties its input
/// region; then the popup below it is (measured on Xvfb 21.1,
/// tools/vng-scenarios/cow-input-shape).
#[test]
fn translate_coordinates_skips_empty_input_shaped_cow() {
    use crate::{backend::WindowHandle, resources::COMPOSITE_OVERLAY_WINDOW};

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let popup = ResourceId(0x200);
    state.resources.create_window(
        ClientId(1),
        CreateWindowRequest {
            depth: 24,
            window: popup,
            parent: ROOT_WINDOW,
            x: 715,
            y: 327,
            width: 132,
            height: 165,
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(popup);
    state
        .resources
        .materialize_cow_resource(WindowHandle::from_raw_for_test(COMPOSITE_OVERLAY_WINDOW.0));
    let _ = read_all_available(&mut peer);

    let mut translate = |state: &mut ServerState| {
        let mut body = Vec::with_capacity(12);
        body.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
        body.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
        body.extend_from_slice(&715i16.to_le_bytes());
        body.extend_from_slice(&327i16.to_le_bytes());
        handle_translate_coordinates(state, ClientId(1), SequenceNumber(1), &body)
            .expect("handle_translate_coordinates");
        let bytes = read_all_available(&mut peer);
        assert_eq!(bytes.len(), 32);
        assert_eq!(bytes[0], 1, "reply");
        assert_eq!(i16::from_le_bytes(bytes[12..14].try_into().unwrap()), 715);
        assert_eq!(i16::from_le_bytes(bytes[14..16].try_into().unwrap()), 327);
        u32::from_le_bytes(bytes[8..12].try_into().unwrap())
    };
    assert_eq!(
        translate(&mut state),
        COMPOSITE_OVERLAY_WINDOW.0,
        "an unshaped COW is the child over the popup",
    );
    state
        .shape_windows
        .entry(COMPOSITE_OVERLAY_WINDOW)
        .or_default()
        .input = Some(Vec::new());
    assert_eq!(
        translate(&mut state),
        popup.0,
        "empty-input COW must not hide the popup child",
    );
}

#[test]
fn configure_window_resize_applies_win_gravity_and_emits_gravity_notify() {
    // X11 window gravity: growing a parent moves a South-gravity child
    // down by the height delta and delivers a GravityNotify (Xorg
    // ResizeChildrenWinSize). fvwm's window-shade relies on exactly
    // this to reveal a client it parked above the fold.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let parent = ResourceId(0x400);
    let child = ResourceId(0x401);
    state.resources.create_window(
        ClientId(1),
        CreateWindowRequest {
            depth: 24,
            window: parent,
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 100,
            height: 100,
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(parent);
    state.resources.create_window(
        ClientId(1),
        CreateWindowRequest {
            depth: 24,
            window: child,
            parent,
            x: 10,
            y: -100,
            width: 100,
            height: 100,
            win_gravity: Some(8), // South
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(child);
    state
        .clients
        .get_mut(&1)
        .unwrap()
        .event_masks
        .insert(child, 0x0002_0000); // StructureNotify
    let _ = read_all_available(&mut peer);

    // Grow the parent 100×100 → 100×200 (dh=+100): CWWidth|CWHeight.
    let body = cw_restack_body(0x400, 0x000C, &[100, 200]);
    let mut backend = RecordingBackend::new();
    handle_configure_window(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        &body,
    )
    .expect("handle_configure_window");

    // South gravity: child.y += 100 → (10, 0).
    let c = state.resources.window(child).expect("child");
    assert_eq!(
        (c.x, c.y),
        (10, 0),
        "South-gravity child follows the parent's height growth",
    );

    // Exactly one GravityNotify (type 24) delivered to the child.
    let out = read_all_available(&mut peer);
    let gravity: Vec<&[u8]> = out
        .chunks(32)
        .filter(|e| e.len() == 32 && e[0] & 0x7f == 24)
        .collect();
    assert_eq!(gravity.len(), 1, "exactly one GravityNotify to the child");
    let g = gravity[0];
    assert_eq!(&g[4..8], &child.0.to_le_bytes(), "event window = child");
    assert_eq!(&g[8..12], &child.0.to_le_bytes(), "reported window = child");
    assert_eq!(i16::from_le_bytes([g[12], g[13]]), 10, "GravityNotify x");
    assert_eq!(i16::from_le_bytes([g[14], g[15]]), 0, "GravityNotify y");
}

/// Per X11 protocol spec: "If the window is already mapped, this
/// request has no effect." brisk-menu (Ubuntu MATE's standalone
/// applications popup) pounds `MapWindow` at ~50 Hz on its
/// already-viewable override-redirect popup via GTK3's
/// `gtk_window_present`/`XMapRaised` loop. Pre-fix every redundant
/// MapWindow re-emitted MapNotify → Expose → full-extent damage,
/// triggering Marco to issue `COMPOSITE::NameWindowPixmap` and a
/// full recomposite of the popup ~50× per second — visible as
/// menu flicker on the COW-authoritative KMS scene path.
#[test]
fn map_window_on_already_mapped_window_is_no_op() {
    use crate::server::DamageObject;
    use std::io::Read;

    const CLIENT_ID: u32 = 1;
    const WINDOW_XID: u32 = 0x0010_0014;
    const HOST_XID: u32 = 0x0040_0014;
    const DAMAGE_ID: u32 = 0x0080_0014;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, CLIENT_ID);
    let mut backend = RecordingBackend::new();

    state.resources.create_window(
        yserver_protocol::x11::ClientId(CLIENT_ID),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(WINDOW_XID),
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 200,
            height: 80,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    {
        let w = state
            .resources
            .window_mut(ResourceId(WINDOW_XID))
            .expect("window installed");
        w.host_xid = Some(crate::backend::WindowHandle::from_raw_for_test(HOST_XID));
    }

    // Marco-like subscriber: SubstructureNotify on root +
    // XDamageCreate on the new window.
    state
        .clients
        .get_mut(&CLIENT_ID)
        .unwrap()
        .event_masks
        .insert(ROOT_WINDOW, 0x0008_0000);
    state.damage_objects.insert(
        DAMAGE_ID,
        DamageObject {
            owner: ClientId(CLIENT_ID),
            drawable: ResourceId(WINDOW_XID),
            level: 0,
            rects: Vec::new(),
            pending_notify_fired: false,
            last_reported_geometry: None,
        },
    );

    let mut body = Vec::with_capacity(4);
    body.extend_from_slice(&WINDOW_XID.to_le_bytes());

    // First map: genuine transition Unmapped → Viewable. Should
    // emit MapNotify + accumulate damage.
    handle_map_window(
        &mut state,
        &mut backend,
        None,
        ClientId(CLIENT_ID),
        SequenceNumber(1),
        &body,
    )
    .expect("handle_map_window first");

    peer.set_nonblocking(true).unwrap();
    let mut tmp = [0u8; 1024];
    let mut first_wire = Vec::new();
    loop {
        match peer.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => first_wire.extend_from_slice(&tmp[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(_) => break,
        }
    }
    let first_map_notifies = first_wire
        .chunks_exact(32)
        .filter(|chunk| chunk[0] & 0x7f == 19)
        .count();
    assert!(
        first_map_notifies >= 1,
        "first MapWindow on an unmapped window must emit MapNotify",
    );
    let first_rect_count = state
        .damage_objects
        .get(&DAMAGE_ID)
        .expect("damage object")
        .rects
        .len();
    assert!(
        first_rect_count > 0,
        "first MapWindow must accumulate damage on the window extent",
    );

    // Second map on the now-already-Viewable window. Per spec this
    // is a no-op: no MapNotify, no extra damage rects.
    handle_map_window(
        &mut state,
        &mut backend,
        None,
        ClientId(CLIENT_ID),
        SequenceNumber(2),
        &body,
    )
    .expect("handle_map_window second");

    let mut second_wire = Vec::new();
    loop {
        match peer.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => second_wire.extend_from_slice(&tmp[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(_) => break,
        }
    }
    let second_map_notifies = second_wire
        .chunks_exact(32)
        .filter(|chunk| chunk[0] & 0x7f == 19)
        .count();
    assert_eq!(
        second_map_notifies, 0,
        "MapWindow on an already-mapped window must NOT re-emit \
             MapNotify — repeated MapNotifies cause Marco to re-issue \
             COMPOSITE::NameWindowPixmap and full recomposite on every \
             redundant map (visible as brisk-menu flicker on KMS).",
    );
    let second_rect_count = state
        .damage_objects
        .get(&DAMAGE_ID)
        .expect("damage object")
        .rects
        .len();
    assert_eq!(
        second_rect_count, first_rect_count,
        "MapWindow on an already-mapped window must NOT accumulate \
             additional damage — full-extent damage on every redundant \
             call is what drives the recompositing flicker storm.",
    );
}

/// Per X11 protocol (Xorg `dix/window.c:2661`): `if (pWin->mapped)
/// return Success;` — `MapWindow` on an already-mapped window is a
/// no-op BEFORE any MapRequest dispatch. The root window is always
/// mapped (initialised `Viewable` at `resources.rs:204`) AND in
/// yserver root's `parent` field points to itself
/// (`resources.rs:194`), so a client calling `MapWindow(root)`
/// pre-fix matched the SubstructureRedirect-on-parent path: the
/// requester wasn't the WM, marco had SubstructureRedirect
/// (`0x0010_0000`) on root, so yserver dispatched a phantom
/// MapRequest event with `parent=root, window=root` to marco.
/// Marco's response (seen on hardware via xtrace seq `0x35e5a`
/// during a mate-screensaver activation): GrabServer →
/// ChangeWindowAttributes(root, event_mask=...) DROPPING
/// SubstructureNotify+SubstructureRedirect → UngrabServer. After
/// that marco no longer receives MapNotify for new top-levels →
/// no `COMPOSITE::NameWindowPixmap` → newly-mapped windows
/// (mate-screensaver overlay, dialogs) are invisible while the
/// cursor still moves. Match Xorg: hoist the "already mapped"
/// short-circuit above the MapRequest emission.
#[test]
fn map_window_on_root_does_not_emit_phantom_map_request() {
    use std::io::Read;

    const WM_ID: u32 = 1;
    const SAVER_ID: u32 = 2;

    let mut state = ServerState::new();
    let mut wm_peer = install_client(&mut state, WM_ID);
    let _saver_peer = install_client(&mut state, SAVER_ID);
    let mut backend = RecordingBackend::new();

    // Marco-like WM: SubstructureRedirect (0x0010_0000) on root.
    state
        .clients
        .get_mut(&WM_ID)
        .unwrap()
        .event_masks
        .insert(ROOT_WINDOW, 0x0010_0000);

    // mate-screensaver-like activation: client 2 calls
    // `MapWindow(root)`. Per X11 spec this must be a silent
    // no-op (root is always mapped).
    let mut body = Vec::with_capacity(4);
    body.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
    handle_map_window(
        &mut state,
        &mut backend,
        None,
        ClientId(SAVER_ID),
        SequenceNumber(1),
        &body,
    )
    .expect("handle_map_window root");

    wm_peer.set_nonblocking(true).unwrap();
    let mut tmp = [0u8; 1024];
    let mut wm_wire = Vec::new();
    loop {
        match wm_peer.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => wm_wire.extend_from_slice(&tmp[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(_) => break,
        }
    }
    let map_requests = wm_wire
        .chunks_exact(32)
        .filter(|chunk| chunk[0] & 0x7f == 20)
        .count();
    assert_eq!(
        map_requests, 0,
        "MapWindow(root) must NOT generate a phantom MapRequest \
             event to the WM. Pre-fix yserver dispatched a bogus \
             MapRequest(parent=root, window=root) because root's parent \
             field points to itself; marco reacted by dropping its \
             SubstructureRedirect subscription, which broke compositing \
             of every subsequent top-level (mate-screensaver overlay \
             and dialogs went invisible while cursor still moved).",
    );
}

/// Per X11 protocol: `cursor = None` (resource id 0) on
/// ChangeWindowAttributes(CWCursor) means "clear this window's
/// cursor so the effective cursor inherits from the parent
/// chain". Pre-fix the handler short-circuited because
/// `resources.cursor_host_xid(ResourceId(0))` returns `None` and
/// the `(Some, Some)` match below dropped the call — marco
/// resets its frame's cursor via `XDefineCursor(frame, None)`
/// when the pointer moves off the resize edge into the frame
/// interior, and that reset never propagated to the backend
/// (resize cursor sprite stayed visible until the pointer left
/// the top-level frame entirely). Matches Xorg
/// `dix/window.c:1487-1491`.
#[test]
fn cwa_cursor_none_propagates_define_cursor_zero_to_backend() {
    use crate::backend::recording::RecordedCall;

    const CLIENT_ID: u32 = 1;
    const WINDOW_XID: u32 = 0x0010_0020;
    const HOST_XID: u32 = 0x0040_0020;

    let mut state = ServerState::new();
    let _peer = install_client(&mut state, CLIENT_ID);
    let mut backend = RecordingBackend::new();

    state.resources.create_window(
        yserver_protocol::x11::ClientId(CLIENT_ID),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(WINDOW_XID),
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 200,
            height: 80,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    {
        let w = state
            .resources
            .window_mut(ResourceId(WINDOW_XID))
            .expect("window installed");
        w.host_xid = Some(crate::backend::WindowHandle::from_raw_for_test(HOST_XID));
    }

    // CWA body: window xid (4) + value_mask (4) + values.
    // CWCursor is bit 14 (mask = 0x4000); value = 0 means
    // X11 None.
    let mut body = Vec::with_capacity(12);
    body.extend_from_slice(&WINDOW_XID.to_le_bytes());
    body.extend_from_slice(&0x4000u32.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());

    handle_change_window_attributes(
        &mut state,
        &mut backend,
        None,
        ClientId(CLIENT_ID),
        SequenceNumber(1),
        &body,
    )
    .expect("handle_change_window_attributes");

    let define_cursor_calls: Vec<_> = backend
        .calls()
        .into_iter()
        .filter(|c| matches!(c, RecordedCall::DefineCursor { .. }))
        .collect();
    assert_eq!(
        define_cursor_calls,
        vec![RecordedCall::DefineCursor {
            host_window_xid: HOST_XID,
            cursor_host_xid: 0,
        }],
        "CWA with CWCursor + value=0 must invoke \
             `backend.define_cursor(window, 0)` so the backend clears \
             its per-window cursor slot and refreshes the effective \
             cursor. Pre-fix this dropped silently, leaving stale \
             cursor state (marco's resize-frame XDefineCursor(frame, \
             None) reset on edge→interior transitions had no effect).",
    );
}

/// A `ChangeWindowAttributes` that only changes the cursor (or any
/// other non-event-mask attribute) must NOT move the input focus.
/// In X11 focus changes only via `SetInputFocus` / grabs, never CWA.
///
/// Regression (the evince / GTK4 titlebar-drag freeze): during a
/// `_NET_WM_MOVERESIZE` move, muffin sets the "fleur" move cursor on
/// its guard window via a cursor-only CWA. The handler promoted
/// keyboard focus to any "focus-wanting" (viewable + key-selecting)
/// window on *any* CWA, so focus was stolen to the guard window. The
/// app's next `SetInputFocus` to its own child was then computed from
/// that unrelated prev focus -> `FocusOut(detail=Nonlinear)` -> GTK4
/// backdrop (greyed CSD buttons) and the drag collapsed.
#[test]
fn cursor_only_cwa_does_not_steal_keyboard_focus() {
    const CLIENT_ID: u32 = 1;
    const GUARD_XID: u32 = 0x0010_0011; // muffin's guard window
    const FOCUSED_XID: u32 = 0x0010_0007; // the real focus window

    let mut state = ServerState::new();
    let _peer = install_client(&mut state, CLIENT_ID);
    let mut backend = RecordingBackend::new();

    for xid in [GUARD_XID, FOCUSED_XID] {
        state.resources.create_window(
            yserver_protocol::x11::ClientId(CLIENT_ID),
            yserver_protocol::x11::CreateWindowRequest {
                depth: 24,
                window: ResourceId(xid),
                parent: ROOT_WINDOW,
                x: 0,
                y: 0,
                width: 100,
                height: 100,
                border_width: 0,
                class: 1,
                visual: crate::resources::ROOT_VISUAL,
                ..Default::default()
            },
        );
        let _ = state.resources.map_window(ResourceId(xid));
    }
    // Guard window is viewable and selects key events, so
    // `window_wants_keyboard_focus` would return true for it.
    state
        .clients
        .get_mut(&CLIENT_ID)
        .expect("client")
        .event_masks
        .insert(ResourceId(GUARD_XID), 0x1); // KeyPress

    // Establish the real focus on FOCUSED_XID before the CWA.
    state.core_focus.raw = FOCUSED_XID;
    for c in state.clients.values_mut() {
        c.focused_window = ResourceId(FOCUSED_XID);
    }

    // Cursor-only CWA on the guard window: CWCursor (bit 14 =
    // 0x4000), value 0 = None. No event-mask change.
    let mut body = Vec::with_capacity(12);
    body.extend_from_slice(&GUARD_XID.to_le_bytes());
    body.extend_from_slice(&0x4000u32.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());

    handle_change_window_attributes(
        &mut state,
        &mut backend,
        None,
        ClientId(CLIENT_ID),
        SequenceNumber(1),
        &body,
    )
    .expect("handle_change_window_attributes");

    assert_eq!(
        state
            .clients
            .get(&CLIENT_ID)
            .expect("client")
            .focused_window,
        ResourceId(FOCUSED_XID),
        "a cursor-only ChangeWindowAttributes must not promote keyboard \
             focus to the target window; focus must stay where SetInputFocus \
             put it",
    );
}

/// F{ A{ A1 }, B{ B1 }, C }: F viewable, A and B unmapped with their
/// child mapped (Unviewable), C mapped.
fn subwindows_delta_fixture(state: &mut ServerState) -> [ResourceId; 6] {
    let [f, a, a1, b, b1, c] = [
        0x0010_0200,
        0x0010_0201,
        0x0010_0202,
        0x0010_0203,
        0x0010_0204,
        0x0010_0205,
    ]
    .map(ResourceId);
    for (id, parent) in [(f, ROOT_WINDOW), (a, f), (a1, a), (b, f), (b1, b), (c, f)] {
        state.resources.create_window(
            yserver_protocol::x11::ClientId(1),
            yserver_protocol::x11::CreateWindowRequest {
                depth: 24,
                window: id,
                parent,
                x: 0,
                y: 0,
                width: 40,
                height: 40,
                border_width: 0,
                class: 1,
                visual: crate::resources::ROOT_VISUAL,
                ..Default::default()
            },
        );
    }
    for w in [a1, b1, f, c] {
        let _ = state.resources.map_window(w);
    }
    [f, a, a1, b, b1, c]
}

/// Window storage follows the delta: realize parent first, release child first, never the COW.
#[test]
fn window_storage_follows_viewability_delta_in_order() {
    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let [f, a, a1, b, b1, c] = subwindows_delta_fixture(&mut state);
    for w in [f, a, a1, b, b1, c] {
        state.resources.window_mut(w).expect("window").host_xid =
            Some(crate::backend::WindowHandle::from_raw_for_test(w.0));
    }
    let _ = state.resources.map_window(a);
    let _ = state.resources.map_window(b);
    let storage_calls = |backend: &RecordingBackend, from: usize| -> Vec<RecordedCall> {
        backend
            .calls()
            .into_iter()
            .skip(from)
            .filter(|c| {
                matches!(
                    c,
                    RecordedCall::RealizeWindowStorage(_) | RecordedCall::ReleaseWindowStorage(_)
                )
            })
            .collect()
    };
    let unmap = |state: &mut ServerState, backend: &mut RecordingBackend, w: ResourceId| {
        handle_unmap_window(
            state,
            backend,
            None,
            ClientId(1),
            SequenceNumber(1),
            &w.0.to_le_bytes(),
        )
        .expect("UnmapWindow");
    };
    let map = |state: &mut ServerState, backend: &mut RecordingBackend, w: ResourceId| {
        handle_map_window(
            state,
            backend,
            None,
            ClientId(1),
            SequenceNumber(2),
            &w.0.to_le_bytes(),
        )
        .expect("MapWindow");
    };

    unmap(&mut state, &mut backend, f);
    assert_eq!(
        storage_calls(&backend, 0),
        [a1, a, b1, b, c, f].map(|w| RecordedCall::ReleaseWindowStorage(w.0)),
        "the whole subtree releases, child first",
    );
    let from = backend.calls().len();
    map(&mut state, &mut backend, f);
    assert_eq!(
        storage_calls(&backend, from),
        [f, a, a1, b, b1, c].map(|w| RecordedCall::RealizeWindowStorage(w.0)),
        "the whole subtree realizes, parent first",
    );
    let realize_at = backend
        .calls()
        .iter()
        .skip(from)
        .position(|c| matches!(c, RecordedCall::RealizeWindowStorage(_)))
        .expect("realize recorded");
    let map_at = backend
        .calls()
        .iter()
        .skip(from)
        .position(|c| matches!(c, RecordedCall::MapSubwindow(_)))
        .expect("map recorded");
    assert!(
        map_at < realize_at,
        "map_subwindow flips `mapped` before realize reads it"
    );

    // A mapped window under an unmapped parent gets nothing; the root and the COW never do.
    unmap(&mut state, &mut backend, a);
    let _ = state.resources.unmap_window(a1);
    let from = backend.calls().len();
    map(&mut state, &mut backend, a1);
    assert!(
        storage_calls(&backend, from).is_empty(),
        "Unmapped -> Unviewable is no transition"
    );
    state
        .resources
        .materialize_cow_resource(crate::backend::WindowHandle::from_raw_for_test(
            COMPOSITE_OVERLAY_WINDOW.0,
        ));
    let from = backend.calls().len();
    unmap(&mut state, &mut backend, COMPOSITE_OVERLAY_WINDOW);
    map(&mut state, &mut backend, COMPOSITE_OVERLAY_WINDOW);
    assert!(
        storage_calls(&backend, from).is_empty(),
        "the COW is outside the lifecycle"
    );
    assert_eq!(storage_lifecycle_host_xid(&state, ROOT_WINDOW), None);
    assert_eq!(
        storage_lifecycle_host_xid(&state, COMPOSITE_OVERLAY_WINDOW),
        None
    );
}

#[test]
fn map_subwindows_delta_includes_promoted_grandchildren() {
    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let [f, a, a1, b, b1, _c] = subwindows_delta_fixture(&mut state);

    let (_, delta) = map_subwindows_with_delta(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        &f.0.to_le_bytes(),
    )
    .expect("MapSubwindows");
    // C was already viewable; A and B each bring their grandchild.
    assert_eq!(delta.became_viewable, vec![a, a1, b, b1]);
    assert!(delta.became_unviewable.is_empty());
}

#[test]
fn unmap_subwindows_delta_is_union_in_post_order() {
    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let [f, a, a1, b, b1, c] = subwindows_delta_fixture(&mut state);
    let _ = state.resources.map_window(a);
    let _ = state.resources.map_window(b);

    let (_, delta) = unmap_subwindows_with_delta(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        &f.0.to_le_bytes(),
    )
    .expect("UnmapSubwindows");
    assert!(delta.became_viewable.is_empty());
    assert_eq!(delta.became_unviewable, vec![a1, a, b1, b, c]);
}

#[test]
fn query_tree_of_the_root_hides_the_overlay_window() {
    // Xorg CompositeRealChildHead (composite/compwindow.c:762), measured
    // on Xvfb 21.1 (tools/vng-scenarios/composite-reredirect).
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let child = ResourceId(0x0010_0001);
    create_root_child(&mut state, child.0);
    state
        .resources
        .materialize_cow_resource(crate::backend::WindowHandle::from_raw_for_test(0xC0C0));
    handle_query_tree(
        &mut state,
        ClientId(1),
        SequenceNumber(1),
        &ROOT_WINDOW.0.to_le_bytes(),
    )
    .expect("QueryTree");
    let reply = read_all_available(&mut peer);
    let listed: Vec<u32> = reply[32..]
        .chunks_exact(4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();
    assert_eq!(u16::from_le_bytes([reply[16], reply[17]]), 1);
    assert_eq!(listed, vec![child.0]);
}

#[test]
fn unmap_subwindows_of_the_root_leaves_the_overlay_window_mapped() {
    // dix/window.c:2897 stops at RealChildHead; measured on Xvfb 21.1.
    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let child = ResourceId(0x0010_0001);
    create_root_child(&mut state, child.0);
    let _ = state.resources.map_window(child);
    state
        .resources
        .materialize_cow_resource(crate::backend::WindowHandle::from_raw_for_test(0xC0C0));
    let (_, delta) = unmap_subwindows_with_delta(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        &ROOT_WINDOW.0.to_le_bytes(),
    )
    .expect("UnmapSubwindows");
    assert_eq!(delta.became_unviewable, vec![child]);
    assert_eq!(
        state
            .resources
            .window(COMPOSITE_OVERLAY_WINDOW)
            .map(|w| w.map_state),
        Some(MapState::Viewable)
    );
}

#[test]
fn map_and_unmap_under_unmapped_parent_still_send_notify() {
    const STRUCTURE_NOTIFY: u32 = 0x0002_0000;
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let [_f, _a, a1, ..] = subwindows_delta_fixture(&mut state);
    let _ = state.resources.unmap_window(a1);
    state
        .clients
        .get_mut(&1)
        .expect("client")
        .event_masks
        .insert(a1, STRUCTURE_NOTIFY);
    let notified = |bytes: &[u8], code: u8| {
        bytes.chunks_exact(32).any(|evt| {
            evt[0] == code && u32::from_le_bytes([evt[8], evt[9], evt[10], evt[11]]) == a1.0
        })
    };

    // A1 under unmapped A: Unmapped -> Unviewable, still MapNotify.
    handle_map_window(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        &a1.0.to_le_bytes(),
    )
    .expect("MapWindow");
    assert_eq!(
        state.resources.window(a1).map(|w| w.map_state),
        Some(MapState::Unviewable)
    );
    assert!(notified(&read_all_available(&mut peer), 19), "MapNotify");

    // Unviewable -> Unmapped, still UnmapNotify.
    handle_unmap_window(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(2),
        &a1.0.to_le_bytes(),
    )
    .expect("UnmapWindow");
    assert_eq!(
        state.resources.window(a1).map(|w| w.map_state),
        Some(MapState::Unmapped)
    );
    assert!(notified(&read_all_available(&mut peer), 18), "UnmapNotify");
}

#[test]
fn map_subwindows_exposes_grandchild_promoted_by_viewability_cascade() {
    // MapSubwindows(parent) maps parent's direct children. When a
    // child transitions Unmapped -> Viewable, the viewability cascade
    // (map_window) also promotes any of the
    // child's descendants that were sitting Unviewable (mapped while
    // their ancestor was unmapped) to Viewable. Xorg fires Expose on
    // every newly-viewable window, not just the directly-mapped child.
    // handle_map_subwindows previously emitted Expose only for the
    // direct children, so a promoted grandchild never got its Expose
    // and never painted its first frame (docs/known-issues.md).
    const CLIENT_ID: u32 = 1;
    const PARENT_XID: u32 = 0x0010_0032;
    const CHILD_XID: u32 = 0x0010_0033;
    const CHILD_HOST_XID: u32 = 0x0040_0033;
    const GRANDCHILD_XID: u32 = 0x0010_0034;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, CLIENT_ID);
    let mut backend = RecordingBackend::new();

    state.resources.create_window(
        yserver_protocol::x11::ClientId(CLIENT_ID),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(PARENT_XID),
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 400,
            height: 300,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    state.resources.create_window(
        yserver_protocol::x11::ClientId(CLIENT_ID),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(CHILD_XID),
            parent: ResourceId(PARENT_XID),
            x: 20,
            y: 30,
            width: 200,
            height: 80,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    state.resources.create_window(
        yserver_protocol::x11::ClientId(CLIENT_ID),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(GRANDCHILD_XID),
            parent: ResourceId(CHILD_XID),
            x: 5,
            y: 5,
            width: 100,
            height: 40,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );

    // Parent already viewable; child unmapped so MapSubwindows maps it;
    // grandchild mapped-but-Unviewable so the cascade promotes it.
    state
        .resources
        .window_mut(ResourceId(PARENT_XID))
        .expect("parent installed")
        .map_state = crate::resources::MapState::Viewable;
    {
        let child = state
            .resources
            .window_mut(ResourceId(CHILD_XID))
            .expect("child installed");
        child.host_xid = Some(crate::backend::WindowHandle::from_raw_for_test(
            CHILD_HOST_XID,
        ));
        child.map_state = crate::resources::MapState::Unmapped;
    }
    state
        .resources
        .window_mut(ResourceId(GRANDCHILD_XID))
        .expect("grandchild installed")
        .map_state = crate::resources::MapState::Unviewable;

    // Client selects ExposureMask (0x8000) on both child and grandchild.
    let client = state.clients.get_mut(&CLIENT_ID).expect("client");
    client
        .event_masks
        .insert(ResourceId(CHILD_XID), 0x0000_8000);
    client
        .event_masks
        .insert(ResourceId(GRANDCHILD_XID), 0x0000_8000);

    let mut body = Vec::with_capacity(4);
    body.extend_from_slice(&PARENT_XID.to_le_bytes());
    handle_map_subwindows(
        &mut state,
        &mut backend,
        None,
        ClientId(CLIENT_ID),
        SequenceNumber(1),
        &body,
    )
    .expect("handle_map_subwindows");

    // Grandchild is now Viewable (cascade worked)...
    assert_eq!(
        state
            .resources
            .window(ResourceId(GRANDCHILD_XID))
            .map(|w| w.map_state),
        Some(crate::resources::MapState::Viewable),
        "the viewability cascade must promote the grandchild"
    );

    let bytes = read_all_available(&mut peer);
    let exposed = |xid: u32| {
        bytes
            .chunks_exact(32)
            .any(|evt| evt[0] == 12 && u32::from_le_bytes([evt[4], evt[5], evt[6], evt[7]]) == xid)
    };
    assert!(
        exposed(CHILD_XID),
        "the directly-mapped child must receive Expose (harness sanity)"
    );
    assert!(
        exposed(GRANDCHILD_XID),
        "a grandchild promoted Unviewable->Viewable by MapSubwindows must \
             also receive Expose, else it never paints its first frame"
    );
}

// X11 wire-event ordering on map. Verified divergence vs Xorg:
//
// - **Xorg** (mate-xorg.xtrace lines 5164→5173 around mate-panel-top
//   map): MapNotify(event=root) first, then DAMAGE-Notify.
// - **yserver pre-fix** (mate.xtrace lines 4938→4940): DAMAGE-Notify
//   first, then MapNotify(event=root).
//
// Marco's compositor relies on MapNotify(SubstructureNotify on root)
// to register a window in its compositor tree, and on the *next*
// DAMAGE-Notify on that window to trigger NameWindowPixmap. When
// damage arrives first, marco's tree doesn't know about the window
// yet and the damage is silently discarded. Marco then never sees
// the audit-#11 initial-paint damage and never Names the window;
// the panel renders blank until a *later* damage event arrives
// (typically the post-resize damage), by which point the icon-paint
// CopyAreas have already missed their compositor pickup window.
//
// The audit-#11 fix (`accumulate_damage_full_to_state` inside
// `handle_map_window`) is structurally correct but was placed
// before the MapNotify emissions — inverting the spec-correct
// order. This test pins MapNotify-before-DAMAGE on the wire.
#[test]
fn map_window_emits_map_notify_before_damage_notify() {
    use crate::server::DamageObject;

    const CLIENT_ID: u32 = 1;
    const WINDOW_XID: u32 = 0x0010_0011;
    const HOST_XID: u32 = 0x0040_0011;
    const DAMAGE_ID: u32 = 0x0080_0011;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, CLIENT_ID);
    let mut backend = RecordingBackend::new();

    state.resources.create_window(
        yserver_protocol::x11::ClientId(CLIENT_ID),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(WINDOW_XID),
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 200,
            height: 80,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    {
        let w = state
            .resources
            .window_mut(ResourceId(WINDOW_XID))
            .expect("window installed");
        w.host_xid = Some(crate::backend::WindowHandle::from_raw_for_test(HOST_XID));
    }

    // Compositor-like subscription: SubstructureNotify on root
    // (0x0008_0000) — gets MapNotify(event=root) when a child of
    // root maps. Plus an `XDamageCreate(window)` subscription —
    // gets DAMAGE-Notify on the new window.
    state
        .clients
        .get_mut(&CLIENT_ID)
        .unwrap()
        .event_masks
        .insert(ROOT_WINDOW, 0x0008_0000);
    state.damage_objects.insert(
        DAMAGE_ID,
        DamageObject {
            owner: ClientId(CLIENT_ID),
            drawable: ResourceId(WINDOW_XID),
            level: 0,
            rects: Vec::new(),
            pending_notify_fired: false,
            last_reported_geometry: None,
        },
    );

    let mut body = Vec::with_capacity(4);
    body.extend_from_slice(&WINDOW_XID.to_le_bytes());
    handle_map_window(
        &mut state,
        &mut backend,
        None,
        ClientId(CLIENT_ID),
        SequenceNumber(1),
        &body,
    )
    .expect("handle_map_window");

    // Drain everything emitted to the client. Each X11 event is
    // 32 bytes — read until WouldBlock and walk event-by-event.
    peer.set_nonblocking(true).unwrap();
    let mut wire = Vec::new();
    let mut tmp = [0u8; 1024];
    loop {
        match peer.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => wire.extend_from_slice(&tmp[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(_) => break,
        }
    }

    // First occurrence of MapNotify(opcode 19, event=ROOT) and
    // DAMAGE-Notify(opcode 91) by 32-byte event index. MapNotify's
    // `event` field is at offset 4..8 (after response_type, pad,
    // sequence_number_lo, sequence_number_hi).
    let mut map_root_idx: Option<usize> = None;
    let mut damage_idx: Option<usize> = None;
    for (i, chunk) in wire.chunks_exact(32).enumerate() {
        let opcode = chunk[0] & 0x7f; // top bit is SendEvent flag
        if opcode == 19 && map_root_idx.is_none() {
            let event_xid = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);
            if event_xid == ROOT_WINDOW.0 {
                map_root_idx = Some(i);
            }
        } else if opcode == 91 && damage_idx.is_none() {
            damage_idx = Some(i);
        }
    }

    let m = map_root_idx
        .expect("MapNotify(event=root) must be emitted to a SubstructureNotify-on-root listener");
    let d = damage_idx.expect("DAMAGE-Notify must be emitted to a subscribed damage object");
    assert!(
        m < d,
        "MapNotify(event=root, idx={m}) must precede DAMAGE-Notify(idx={d}) on the \
             wire — pre-fix the audit-#11 damage emission inside `handle_map_window` ran \
             before the MapNotify emissions, so marco's compositor saw the damage on a \
             window that hadn't yet entered its compositor tree, silently discarded it, \
             and never NameWindowPixmap'd the freshly-mapped window. \
             See mate.xtrace lines 4938→4940 vs mate-xorg.xtrace lines 5164→5173.",
    );
}

/// Mapping a viewable window emits `VisibilityNotify(Unobscured)`
/// (event type 15, state 0) to clients selecting
/// `VisibilityChangeMask` (0x10000). Without it, GTK3 suppresses
/// all frame-clock paints after the first expose-driven frame —
/// the cinnamon-settings "title updates but content never
/// repaints" freeze, confirmed by diffing the Xorg vs yserver
/// wire traces (Xorg sends Unobscured; yserver previously sent
/// nothing).
#[test]
fn map_window_emits_visibility_notify_unobscured() {
    const CLIENT_ID: u32 = 1;
    const WINDOW_XID: u32 = 0x0010_0021;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, CLIENT_ID);
    let mut backend = RecordingBackend::new();

    state.resources.create_window(
        yserver_protocol::x11::ClientId(CLIENT_ID),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(WINDOW_XID),
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 200,
            height: 80,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    // Client selects VisibilityChangeMask on its own window.
    state
        .clients
        .get_mut(&CLIENT_ID)
        .unwrap()
        .event_masks
        .insert(ResourceId(WINDOW_XID), 0x0001_0000);

    let mut body = Vec::with_capacity(4);
    body.extend_from_slice(&WINDOW_XID.to_le_bytes());
    handle_map_window(
        &mut state,
        &mut backend,
        None,
        ClientId(CLIENT_ID),
        SequenceNumber(1),
        &body,
    )
    .expect("handle_map_window");

    peer.set_nonblocking(true).unwrap();
    let mut wire = Vec::new();
    let mut tmp = [0u8; 1024];
    loop {
        match peer.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => wire.extend_from_slice(&tmp[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(_) => break,
        }
    }

    let vis = wire.chunks_exact(32).find(|chunk| chunk[0] & 0x7f == 15);
    let vis = vis.expect("VisibilityNotify (type 15) must be emitted on map");
    let window = u32::from_le_bytes([vis[4], vis[5], vis[6], vis[7]]);
    assert_eq!(window, WINDOW_XID, "VisibilityNotify targets the window");
    assert_eq!(vis[8], 0, "state must be Unobscured (0)");
}

/// xts5 Xlib4 (`EWin.mc`) creates a window, destroys it, then
/// invokes each window-manipulation request on the freed xid and
/// expects a `BadWindow` protocol error back. Before this fix,
/// every handler silently returned `RequestOutcome::Handled`,
/// causing ~24 FAILs across the Xlib4 cluster
/// (`Got Success, Expecting BadWindow`).
///
/// One test per affected handler so a regression names the
/// specific request rather than a single bundled failure.
#[test]
fn window_handlers_on_freed_xid_return_bad_window() {
    use std::io::Read;
    const APP: u32 = 200;
    const BAD: u32 = 0x0080_dead;

    // (opcode, header.data, body, label, requires_body_len)
    let cases: &[(u8, u8, Vec<u8>, &str)] = &[
        (
            1,
            24,
            create_window_body_with_parent(BAD),
            "CreateWindow(parent)",
        ),
        (
            2,
            0,
            change_window_attributes_body(BAD),
            "ChangeWindowAttributes",
        ),
        (4, 0, BAD.to_le_bytes().to_vec(), "DestroyWindow"),
        (5, 0, BAD.to_le_bytes().to_vec(), "DestroySubwindows"),
        (8, 0, BAD.to_le_bytes().to_vec(), "MapWindow"),
        (9, 0, BAD.to_le_bytes().to_vec(), "MapSubwindows"),
        (10, 0, BAD.to_le_bytes().to_vec(), "UnmapWindow"),
        (11, 0, BAD.to_le_bytes().to_vec(), "UnmapSubwindows"),
        (12, 0, configure_window_body(BAD), "ConfigureWindow"),
        (13, 0, BAD.to_le_bytes().to_vec(), "CirculateWindow"),
    ];

    for (i, (opcode, data, body, label)) in cases.iter().enumerate() {
        let mut state = ServerState::new();
        let mut peer = install_client(&mut state, APP + i as u32);
        let mut backend = RecordingBackend::new();
        let length_units = u32::try_from(1 + body.len().div_ceil(4)).expect("body length fits");
        let seq = SequenceNumber((i + 1) as u16);
        process_request(
            &mut state,
            &mut backend,
            ClientId(APP + i as u32),
            seq,
            RequestHeader {
                opcode: *opcode,
                data: *data,
                length_units,
            },
            body,
            None,
        )
        .expect("process_request");

        peer.set_nonblocking(true).unwrap();
        let mut buf = [0u8; 32];
        peer.read_exact(&mut buf).unwrap_or_else(|e| {
            panic!("{label}: expected 32-byte error reply, got {e:?}");
        });
        assert_eq!(
            buf[0], 0,
            "{label}: byte 0 = error class (0); got {}",
            buf[0]
        );
        assert_eq!(
            buf[1],
            yserver_protocol::x11::error::BAD_WINDOW,
            "{label}: expected BadWindow (3) for freed xid; got error code {}",
            buf[1],
        );
        // bytes [4..8] = error_value (the bad xid)
        let bad_value = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
        assert_eq!(
            bad_value, BAD,
            "{label}: error_value must echo the freed window xid"
        );
        // byte [10] = major_opcode (this is the request that errored)
        assert_eq!(
            buf[10], *opcode,
            "{label}: BadWindow error must carry the request's \
                 major opcode (expected {opcode}); got {}",
            buf[10],
        );
    }
}

fn create_window_body_with_parent(parent_xid: u32) -> Vec<u8> {
    // ProcCreateWindow body (header.data carries depth):
    //   wid u32, parent u32, x i16, y i16, width u16, height u16,
    //   border_width u16, class u16, visual u32, value_mask u32, …
    // Use class=CopyFromParent(0), visual=CopyFromParent(0), no
    // attributes — the parent BadWindow check fires first.
    let mut body = Vec::with_capacity(28);
    body.extend_from_slice(&0x0080_0001u32.to_le_bytes()); // new wid (will not be used)
    body.extend_from_slice(&parent_xid.to_le_bytes());
    body.extend_from_slice(&0i16.to_le_bytes()); // x
    body.extend_from_slice(&0i16.to_le_bytes()); // y
    body.extend_from_slice(&10u16.to_le_bytes()); // width
    body.extend_from_slice(&10u16.to_le_bytes()); // height
    body.extend_from_slice(&0u16.to_le_bytes()); // border_width
    body.extend_from_slice(&0u16.to_le_bytes()); // class
    body.extend_from_slice(&0u32.to_le_bytes()); // visual
    body.extend_from_slice(&0u32.to_le_bytes()); // value_mask
    body
}

fn change_window_attributes_body(window_xid: u32) -> Vec<u8> {
    // ProcChangeWindowAttributes body: window u32, value_mask u32,
    // values…  No values needed — the window BadWindow check
    // fires before attribute application.
    let mut body = Vec::with_capacity(8);
    body.extend_from_slice(&window_xid.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes()); // value_mask
    body
}

fn configure_window_body(window_xid: u32) -> Vec<u8> {
    // ProcConfigureWindow body: window u32, value_mask u16, pad
    // u16, values…  No values needed — BadWindow check fires
    // before mask is read.
    let mut body = Vec::with_capacity(8);
    body.extend_from_slice(&window_xid.to_le_bytes());
    body.extend_from_slice(&0u16.to_le_bytes()); // value_mask
    body.extend_from_slice(&0u16.to_le_bytes()); // pad
    body
}

/// CreateWindow body with an explicit class/visual and a value list.
/// `depth` travels in `header.data`, not the body.
fn border_create_body(wid: u32, parent: u32, value_mask: u32, values: &[u32]) -> Vec<u8> {
    let mut body = Vec::with_capacity(28 + values.len() * 4);
    body.extend_from_slice(&wid.to_le_bytes());
    body.extend_from_slice(&parent.to_le_bytes());
    body.extend_from_slice(&0i16.to_le_bytes()); // x
    body.extend_from_slice(&0i16.to_le_bytes()); // y
    body.extend_from_slice(&10u16.to_le_bytes()); // width
    body.extend_from_slice(&10u16.to_le_bytes()); // height
    body.extend_from_slice(&0u16.to_le_bytes()); // border_width
    body.extend_from_slice(&1u16.to_le_bytes()); // class = InputOutput
    body.extend_from_slice(&0u32.to_le_bytes()); // visual = CopyFromParent
    body.extend_from_slice(&value_mask.to_le_bytes());
    for v in values {
        body.extend_from_slice(&v.to_le_bytes());
    }
    body
}

/// Pick the border-source forward out of a recorded call list.
fn border_forwards(calls: &[RecordedCall]) -> Vec<(u32, u32, Vec<u32>)> {
    calls
        .iter()
        .filter_map(|c| match c {
            RecordedCall::ChangeSubwindowAttributes {
                host_xid,
                value_mask,
                values,
            } if value_mask & (CWA_BORDER_PIXMAP | CWA_BORDER_PIXEL) != 0 => {
                Some((*host_xid, *value_mask, values.clone()))
            }
            _ => None,
        })
        .collect()
}

/// CWCursor on CreateWindow takes effect like ChangeWindowAttributes'
/// (Xorg `CreateWindow` → `ChangeWindowAttributes`): the window keeps
/// the cursor and the backend shows it; an unknown cursor is BadCursor.
#[test]
fn create_window_cursor_is_kept_and_forwarded() {
    const CURSOR: ResourceId = ResourceId(0x0080_0010);
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    state.resources.create_glyph_cursor(ClientId(1), CURSOR);
    state.resources.set_cursor_host_xid(
        CURSOR,
        crate::backend::CursorHandle::from_raw(0x00ab_0001).unwrap(),
    );
    let body = border_create_body(0x0080_0001, ROOT_WINDOW.0, 0x4000, &[CURSOR.0]);
    let calls = run_border_request_recording(&mut state, 1, 0, &body);
    assert_no_error(&read_all_available(&mut peer), "CreateWindow with a cursor");
    let window = state.resources.window(ResourceId(0x0080_0001)).unwrap();
    assert_eq!(window.cursor, Some(CURSOR));
    let host = window.host_xid.unwrap().as_raw();
    assert!(calls.contains(&RecordedCall::DefineCursor {
        host_window_xid: host,
        cursor_host_xid: 0x00ab_0001,
    }));

    let body = border_create_body(0x0080_0002, ROOT_WINDOW.0, 0x4000, &[0x0080_0011]);
    run_border_request(&mut state, 1, 0, &body);
    assert_error_code(
        &read_all_available(&mut peer),
        x11::error::BAD_CURSOR,
        "unknown cursor",
    );
}

/// #196: a window holds its cursor (Xorg `pCurs->refcnt`,
/// `dix/window.c:1536`), so FreeCursor of a cursor set by CreateWindow
/// keeps it — for an InputOutput and an InputOnly window alike, the
/// latter having no backend window to hold it — until the window
/// changes cursor (`:1559`) or is destroyed (`:968`); then it is freed
/// once.
#[test]
fn create_window_cursor_outlives_free_cursor_until_the_window_drops_it() {
    let host_frees = |calls: &[RecordedCall]| -> Vec<u32> {
        calls
            .iter()
            .filter_map(|c| match c {
                RecordedCall::FreeCursor(h) => Some(*h),
                _ => None,
            })
            .collect()
    };
    for (class, drop_by_destroy) in [(1u16, false), (1, true), (2, false), (2, true)] {
        let mut state = ServerState::new();
        let mut peer = install_client(&mut state, 1);
        let cursor = ResourceId(0x0080_0010);
        let window = 0x0080_0001;
        state.resources.create_glyph_cursor(ClientId(1), cursor);
        state.resources.set_cursor_host_xid(
            cursor,
            crate::backend::CursorHandle::from_raw(0x00ab_0001).unwrap(),
        );
        let mut body = border_create_body(window, ROOT_WINDOW.0, 0x4000, &[cursor.0]);
        body[18..20].copy_from_slice(&class.to_le_bytes());
        run_border_request(&mut state, 1, 0, &body);
        assert_no_error(&read_all_available(&mut peer), "CreateWindow");

        let calls = run_border_request_recording(&mut state, 95, 0, &cursor.0.to_le_bytes());
        assert_eq!(host_frees(&calls), Vec::<u32>::new(), "class {class}: held");

        let calls = if drop_by_destroy {
            run_border_request_recording(&mut state, 4, 0, &window.to_le_bytes())
        } else {
            run_border_request_recording(&mut state, 2, 0, &border_cwa_body(window, 0x4000, &[0]))
        };
        assert_eq!(host_frees(&calls), vec![0x00ab_0001], "class {class}");
        assert_no_error(&read_all_available(&mut peer), "no error");
    }
}

/// The positive control: a cursor two windows hold goes with the last.
#[test]
fn freed_cursor_goes_with_the_last_window_holding_it() {
    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    let cursor = ResourceId(0x0080_0010);
    state.resources.create_glyph_cursor(ClientId(1), cursor);
    state.resources.set_cursor_host_xid(
        cursor,
        crate::backend::CursorHandle::from_raw(0x00ab_0001).unwrap(),
    );
    for w in [0x0080_0001u32, 0x0080_0002] {
        let body = border_create_body(w, ROOT_WINDOW.0, 0x4000, &[cursor.0]);
        run_border_request(&mut state, 1, 0, &body);
    }
    let mut calls = run_border_request_recording(&mut state, 95, 0, &cursor.0.to_le_bytes());
    calls.extend(run_border_request_recording(
        &mut state,
        4,
        0,
        &0x0080_0001u32.to_le_bytes(),
    ));
    assert!(!calls.contains(&RecordedCall::FreeCursor(0x00ab_0001)));
    let calls = run_border_request_recording(&mut state, 4, 0, &0x0080_0002u32.to_le_bytes());
    assert_eq!(
        calls
            .iter()
            .filter(|c| **c == RecordedCall::FreeCursor(0x00ab_0001))
            .count(),
        1
    );
}

/// Xorg `dix/window.c:818` — a depth that differs from the parent's
/// with NO border attribute is BadMatch, because the inherited border
/// (`:879`) would carry the parent's depth. This is why awesome sends
/// `border-pixel=0x00000000` on every depth-32 CreateWindow.
#[test]
fn create_window_depth_differs_without_border_attribute_is_bad_match() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let body = border_create_body(0x0080_0001, ROOT_WINDOW.0, 0, &[]);
    run_border_request(&mut state, 1, 32, &body);
    let bytes = read_all_available(&mut peer);
    assert_error_code(
        &bytes,
        x11::error::BAD_MATCH,
        "depth 32 under depth-24 parent",
    );
}

/// The same request WITH `CWBorderPixel` is legal — the awesome
/// pattern.
#[test]
fn create_window_depth_differs_with_border_pixel_is_accepted() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let body = border_create_body(0x0080_0001, ROOT_WINDOW.0, CWA_BORDER_PIXEL, &[0]);
    run_border_request(&mut state, 1, 32, &body);
    let bytes = read_all_available(&mut peer);
    assert_no_error(&bytes, "depth 32 with border-pixel");
    assert!(
        state.resources.window(ResourceId(0x0080_0001)).is_some(),
        "window should have been created"
    );
}

/// `CWBorderPixmap` = `CopyFromParent` (xid 0) requires matching
/// depths (`dix/window.c:1254`).
#[test]
fn create_window_border_pixmap_copy_from_parent_depth_mismatch_is_bad_match() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let body = border_create_body(0x0080_0001, ROOT_WINDOW.0, CWA_BORDER_PIXMAP, &[0]);
    run_border_request(&mut state, 1, 32, &body);
    let bytes = read_all_available(&mut peer);
    assert_error_code(
        &bytes,
        x11::error::BAD_MATCH,
        "CopyFromParent border across depths",
    );
}

/// A border pixmap xid that names nothing is BadPixmap.
#[test]
fn create_window_border_pixmap_unknown_xid_is_bad_pixmap() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let body = border_create_body(
        0x0080_0001,
        ROOT_WINDOW.0,
        CWA_BORDER_PIXMAP,
        &[0x0099_9999],
    );
    run_border_request(&mut state, 1, 24, &body);
    let bytes = read_all_available(&mut peer);
    assert_error_code(&bytes, x11::error::BAD_PIXMAP, "stale border pixmap");
}

/// A real border pixmap of the wrong depth is BadMatch
/// (`dix/window.c:1275`).
#[test]
fn create_window_border_pixmap_depth_mismatch_is_bad_match() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    state.resources.create_pixmap(
        ClientId(1),
        CreatePixmapRequest {
            pixmap: ResourceId(0x0080_1200),
            drawable: ROOT_WINDOW,
            width: 4,
            height: 4,
            depth: 1,
        },
    );
    let body = border_create_body(
        0x0080_0001,
        ROOT_WINDOW.0,
        CWA_BORDER_PIXMAP,
        &[0x0080_1200],
    );
    run_border_request(&mut state, 1, 24, &body);
    let bytes = read_all_available(&mut peer);
    assert_error_code(&bytes, x11::error::BAD_MATCH, "depth-1 border on depth-24");
}

/// With both bits set the pixel wins and the pixmap is never
/// resolved — Xorg clears `CWBorderPixmap` from the mask before the
/// ddx layer sees it (`dix/window.c:1298`). So a bogus pixmap xid
/// alongside a pixel must NOT error.
#[test]
fn create_window_border_pixel_overrides_pixmap_and_skips_its_validation() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let body = border_create_body(
        0x0080_0001,
        ROOT_WINDOW.0,
        CWA_BORDER_PIXMAP | CWA_BORDER_PIXEL,
        &[0x0099_9999, 0x00ff_0000],
    );
    run_border_request(&mut state, 1, 24, &body);
    let bytes = read_all_available(&mut peer);
    assert_no_error(&bytes, "pixel overrides a bogus pixmap");
    assert_eq!(
        state
            .resources
            .window(ResourceId(0x0080_0001))
            .map(|w| w.border),
        Some(crate::resources::BorderSource::Pixel(0x00ff_0000)),
    );
}

#[test]
fn change_window_attributes_border_pixmap_unknown_xid_is_bad_pixmap() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    seed_window(&mut state, ResourceId(0x0080_0001), ROOT_WINDOW, 10, 10);
    let body = border_cwa_body(0x0080_0001, CWA_BORDER_PIXMAP, &[0x0099_9999]);
    run_border_request(&mut state, 2, 0, &body);
    let bytes = read_all_available(&mut peer);
    assert_error_code(&bytes, x11::error::BAD_PIXMAP, "CWA stale border pixmap");
}

#[test]
fn change_window_attributes_border_pixmap_depth_mismatch_is_bad_match() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    seed_window(&mut state, ResourceId(0x0080_0001), ROOT_WINDOW, 10, 10);
    state.resources.create_pixmap(
        ClientId(1),
        CreatePixmapRequest {
            pixmap: ResourceId(0x0080_1200),
            drawable: ROOT_WINDOW,
            width: 4,
            height: 4,
            depth: 1,
        },
    );
    let body = border_cwa_body(0x0080_0001, CWA_BORDER_PIXMAP, &[0x0080_1200]);
    run_border_request(&mut state, 2, 0, &body);
    let bytes = read_all_available(&mut peer);
    assert_error_code(&bytes, x11::error::BAD_MATCH, "CWA border depth mismatch");
}

/// The awesome request shape: a 16-byte CWA carrying ONLY
/// `CWBorderPixel`. Before #133 this parsed into a struct where every
/// field was `None` and did nothing.
#[test]
fn change_window_attributes_border_pixel_only_installs_the_pixel() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    seed_window(&mut state, ResourceId(0x0080_0001), ROOT_WINDOW, 10, 10);
    let body = border_cwa_body(0x0080_0001, CWA_BORDER_PIXEL, &[0xffff_0000]);
    run_border_request(&mut state, 2, 0, &body);
    let bytes = read_all_available(&mut peer);
    assert_no_error(&bytes, "CWA border-pixel only");
    assert_eq!(
        state
            .resources
            .window(ResourceId(0x0080_0001))
            .map(|w| w.border),
        Some(crate::resources::BorderSource::Pixel(0xffff_0000)),
    );
}

/// #133 step 2 (P3) — THE test for the forward path. The awesome
/// request shape (a CWA carrying only `CWBorderPixel`) must reach
/// the backend as a `change_subwindow_attributes` with the real X11
/// CW bit `0x08` and the pixel value. Before step 2 a border change
/// reached the backend by no route at all: the only CWA forward
/// fired on a background change.
#[test]
fn change_window_attributes_border_pixel_forwards_to_backend() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let win = ResourceId(0x0080_0001);
    seed_window(&mut state, win, ROOT_WINDOW, 10, 10);
    let host_xid = state
        .resources
        .window(win)
        .and_then(|w| w.host_xid)
        .expect("seed_window assigns a host xid")
        .as_raw();

    let body = border_cwa_body(win.0, CWA_BORDER_PIXEL, &[0xffff_0000]);
    let calls = run_border_request_recording(&mut state, 2, 0, &body);
    assert_no_error(&read_all_available(&mut peer), "CWA border-pixel only");
    assert_eq!(
        border_forwards(&calls),
        vec![(host_xid, CWA_BORDER_PIXEL, vec![0xffff_0000])],
        "CWBorderPixel must forward as mask 0x08 with the pixel"
    );
}

/// A border TILE forwards as `CWBorderPixmap` (`0x04`) carrying the
/// pixmap's HOST xid, not the client-visible one — same primitive
/// hand-off as a background pixmap, so the `yserver` crate never
/// needs `resources::BorderSource`.
#[test]
fn change_window_attributes_border_pixmap_forwards_host_xid() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let win = ResourceId(0x0080_0001);
    seed_window(&mut state, win, ROOT_WINDOW, 10, 10);
    let host_xid = state
        .resources
        .window(win)
        .and_then(|w| w.host_xid)
        .expect("host xid")
        .as_raw();
    let tile = ResourceId(0x0080_1300);
    state.resources.create_pixmap(
        ClientId(1),
        CreatePixmapRequest {
            pixmap: tile,
            drawable: ROOT_WINDOW,
            width: 8,
            height: 8,
            depth: 24,
        },
    );
    assert!(
        state.resources.set_pixmap_host_xid(
            tile,
            crate::backend::PixmapHandle::from_raw(0x9999_0001).expect("non-zero")
        ),
        "tile pixmap must exist"
    );

    let body = border_cwa_body(win.0, CWA_BORDER_PIXMAP, &[tile.0]);
    let calls = run_border_request_recording(&mut state, 2, 0, &body);
    assert_no_error(&read_all_available(&mut peer), "CWA border-pixmap");
    assert_eq!(
        border_forwards(&calls),
        vec![(host_xid, CWA_BORDER_PIXMAP, vec![0x9999_0001])],
    );
}

/// #143 — THE regression gate for the awesome border flicker.
///
/// awesome recolours a focused/unfocused frame with a CWA carrying
/// only `CWBorderPixel`. We repainted the ring into the redirect
/// backing but reported no protocol DAMAGE for it, so picom — which
/// partial-repaints from `EXT_buffer_age` — kept one ring colour per
/// back buffer and alternated between them at frame rate. Xorg
/// reports it from `ChangeWindowAttributes` itself: `borderClip −
/// winSize` → `PaintWindow(..., PW_BORDER)` (`dix/window.c:1581-1589`)
/// → `PolyFillRect` on the window pixmap (`mi/miexpose.c:448-471`,
/// `:558`) → `damagePolyFillRect` (`miext/damage/damage.c:1194`).
///
/// Geometry is the captured one: 1276×704 at `border_width = 2`.
#[test]
fn change_window_attributes_border_pixel_damages_the_ring() {
    use crate::server::DamageObject;
    use yserver_protocol::x11::xfixes::RegionRect;
    const DAMAGE_XID: u32 = 0x0080_9143;
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let win = ResourceId(0x0080_0001);
    seed_window(&mut state, win, ROOT_WINDOW, 1276, 704);
    if let Some(w) = state.resources.window_mut(win) {
        w.border_width = 2;
    }
    assert!(
        state.resources.map_window(win).mapping_changed,
        "window must map"
    );
    // Raw level: `area` on the wire then carries the real rect
    // instead of the NonEmpty full-extent substitute, so the test
    // can prove the negative origin survives the i16 encoding.
    state.damage_objects.insert(
        DAMAGE_XID,
        DamageObject {
            owner: ClientId(1),
            drawable: win,
            level: 0,
            rects: Vec::new(),
            pending_notify_fired: false,
            last_reported_geometry: None,
        },
    );

    let body = border_cwa_body(win.0, CWA_BORDER_PIXEL, &[0x0000_ff00]);
    run_border_request(&mut state, 2, 0, &body);

    let rects = state
        .damage_objects
        .get(&DAMAGE_XID)
        .expect("damage object")
        .rects
        .clone();
    assert_eq!(
        rects,
        vec![
            RegionRect {
                x: -2,
                y: -2,
                width: 1280,
                height: 2
            },
            RegionRect {
                x: -2,
                y: 0,
                width: 2,
                height: 704
            },
            RegionRect {
                x: 1276,
                y: 0,
                width: 2,
                height: 704
            },
            RegionRect {
                x: -2,
                y: 704,
                width: 1280,
                height: 2
            },
        ],
        "a border-source change must damage the whole ring, including the \
             negative-origin top and left strips",
    );

    // And it must reach the client as real DamageNotify events,
    // with the negative origin intact on the wire (`area.x` is
    // INT16 at byte 16).
    let bytes = read_all_available(&mut peer);
    let notifies: Vec<&[u8]> = bytes
        .chunks_exact(32)
        .filter(|c| c[0] == crate::nested::DAMAGE_FIRST_EVENT)
        .collect();
    assert_eq!(
        notifies.len(),
        4,
        "one DamageNotify per ring strip at ReportLevel Raw",
    );
    let areas: Vec<(i16, i16, u16, u16)> = notifies
        .iter()
        .map(|c| {
            (
                i16::from_le_bytes([c[16], c[17]]),
                i16::from_le_bytes([c[18], c[19]]),
                u16::from_le_bytes([c[20], c[21]]),
                u16::from_le_bytes([c[22], c[23]]),
            )
        })
        .collect();
    assert!(
        areas.contains(&(-2, -2, 1280, 2)),
        "the top strip must arrive at a NEGATIVE origin, got {areas:?}",
    );
    assert!(
        areas.contains(&(-2, 0, 2, 704)),
        "the left strip must arrive at a NEGATIVE x, got {areas:?}",
    );
}

/// The ring damage follows Xorg's two gates and no others: nothing
/// for an unbordered window (`HasBorder(pWin)`, `dix/window.c:1586`)
/// and nothing for a non-viewable one (`pWin->viewable`, same line).
#[test]
fn change_window_attributes_border_pixel_damages_nothing_unbordered_or_unmapped() {
    use crate::server::DamageObject;
    const DAMAGE_XID: u32 = 0x0080_9144;
    for (bw, map) in [(0u16, true), (2, false)] {
        let mut state = ServerState::new();
        let mut peer = install_client(&mut state, 1);
        let win = ResourceId(0x0080_0001);
        seed_window(&mut state, win, ROOT_WINDOW, 64, 64);
        if let Some(w) = state.resources.window_mut(win) {
            w.border_width = bw;
        }
        if map {
            assert!(
                state.resources.map_window(win).mapping_changed,
                "window must map"
            );
        }
        state.damage_objects.insert(
            DAMAGE_XID,
            DamageObject {
                owner: ClientId(1),
                drawable: win,
                level: 0,
                rects: Vec::new(),
                pending_notify_fired: false,
                last_reported_geometry: None,
            },
        );

        let body = border_cwa_body(win.0, CWA_BORDER_PIXEL, &[0x0000_ff00]);
        run_border_request(&mut state, 2, 0, &body);
        assert_no_error(&read_all_available(&mut peer), "CWA border-pixel");

        assert!(
            state.damage_objects[&DAMAGE_XID].rects.is_empty(),
            "bw={bw} mapped={map}: no ring to report",
        );
    }
}

/// #143, the resize half. `5d7270c1` reported the ring on a
/// border-source change and on backing activation; a RESIZE
/// repaints it too — `rotate_redirected_backing_on_resize`
/// reallocates and `allocate_redirected_backing` paints the new
/// ring — and reported nothing, so a partial-repaint compositor
/// (picom on `EXT_buffer_age`) kept a ring at the OLD extent in one
/// back buffer and the new one in the other. The bottom and right
/// strips are the ones that move when a window is resized about a
/// fixed origin, which is exactly the observed fingerprint: those
/// two edges flickered at awesome's ~1 Hz clock tick and settled
/// gone.
///
/// Xorg reports it: `compReallocPixmap` reallocates whenever the
/// bordered extent differs (`composite/compalloc.c:698`) and that
/// branch alone calls `compSetPixmap` (`:700`), whose visitor
/// queues `compRepaintBorder` for `bw != 0`
/// (`composite/compwindow.c:137-139`) — a `PolyFillRect` on the
/// window pixmap (`mi/miexpose.c:558`) that `damagePolyFillRect`
/// (`miext/damage/damage.c:1194`) reports.
///
/// bw = 2 and the captured 1276x704, shrunk and grown.
#[test]
fn configure_window_resize_damages_the_ring_at_the_new_geometry() {
    use crate::{
        resources::RedirectedBacking,
        server::{CompositeRedirectMode, DamageObject, RedirectRecord},
    };
    use yserver_protocol::x11::xfixes::RegionRect;
    const DAMAGE_XID: u32 = 0x0080_9145;
    const BW: u16 = 2;
    // (old_w, old_h, new_w, new_h, label)
    for (old_w, old_h, new_w, new_h, label) in [
        (1276u16, 704u16, 636u16, 348u16, "shrink"),
        (636, 348, 1276, 704, "grow"),
    ] {
        let mut state = ServerState::new();
        let mut peer = install_client(&mut state, 1);
        let win = ResourceId(0x0080_0001);
        seed_window(&mut state, win, ROOT_WINDOW, old_w, old_h);
        if let Some(w) = state.resources.window_mut(win) {
            w.border_width = BW;
            w.redirected_backing = Some(RedirectedBacking {
                host_pixmap: crate::backend::PixmapHandle::from_raw_for_test(0x0050_0001),
                width: old_w + 2 * BW,
                height: old_h + 2 * BW,
                depth: 24,
            });
        }
        assert!(
            state.resources.map_window(win).mapping_changed,
            "window must map"
        );
        state
            .composite_redirects
            .redirect_window(
                win,
                RedirectRecord {
                    mode: CompositeRedirectMode::Manual,
                    owner: ClientId(1),
                },
            )
            .unwrap();
        // ReportLevel Raw, so `rects` keeps the real strips instead
        // of the NonEmpty full-extent substitute.
        state.damage_objects.insert(
            DAMAGE_XID,
            DamageObject {
                owner: ClientId(1),
                drawable: win,
                level: 0,
                rects: Vec::new(),
                pending_notify_fired: false,
                last_reported_geometry: None,
            },
        );

        // ConfigureWindow, CWWidth | CWHeight.
        let mut body = Vec::with_capacity(16);
        body.extend_from_slice(&win.0.to_le_bytes());
        body.extend_from_slice(&0x000cu16.to_le_bytes());
        body.extend_from_slice(&[0u8; 2]);
        body.extend_from_slice(&u32::from(new_w).to_le_bytes());
        body.extend_from_slice(&u32::from(new_h).to_le_bytes());
        run_border_request(&mut state, 12, 0, &body);

        let rects = state.damage_objects[&DAMAGE_XID].rects.clone();
        let bw = i16::try_from(BW).unwrap();
        let ring = [
            RegionRect {
                x: -bw,
                y: -bw,
                width: new_w + 2 * BW,
                height: BW,
            },
            RegionRect {
                x: -bw,
                y: 0,
                width: BW,
                height: new_h,
            },
            RegionRect {
                x: i16::try_from(new_w).unwrap(),
                y: 0,
                width: BW,
                height: new_h,
            },
            RegionRect {
                x: -bw,
                y: i16::try_from(new_h).unwrap(),
                width: new_w + 2 * BW,
                height: BW,
            },
        ];
        assert_eq!(
            rects.get(..4),
            Some(&ring[..]),
            "{label}: a resize must damage the whole ring at the NEW \
                 geometry, before the full-extent configure damage, and the \
                 top/left strips carry NEGATIVE origins; got {rects:?}",
        );
        // The bottom and right strips are the ones whose position
        // MOVED — the #143 fingerprint. Spell them out so a
        // regression that reports the ring at the OLD extent fails
        // here and not only on the ordering assert above.
        assert!(
            rects.contains(&ring[2]) && rects.contains(&ring[3]),
            "{label}: the right and bottom strips must sit at the NEW \
                 extent ({new_w}x{new_h}), not the old one ({old_w}x{old_h})",
        );
        assert!(
            rects.contains(&RegionRect {
                x: 0,
                y: 0,
                width: new_w,
                height: new_h,
            }),
            "{label}: the pre-existing full-extent configure damage must \
                 still fire alongside the ring",
        );

        // And it reaches the client, negative origins intact on the
        // wire (`area.x`/`area.y` are INT16 at bytes 16..20).
        let bytes = read_all_available(&mut peer);
        let areas: Vec<(i16, i16, u16, u16)> = bytes
            .chunks_exact(32)
            .filter(|c| c[0] == crate::nested::DAMAGE_FIRST_EVENT)
            .map(|c| {
                (
                    i16::from_le_bytes([c[16], c[17]]),
                    i16::from_le_bytes([c[18], c[19]]),
                    u16::from_le_bytes([c[20], c[21]]),
                    u16::from_le_bytes([c[22], c[23]]),
                )
            })
            .collect();
        for r in &ring {
            assert!(
                areas.contains(&(r.x, r.y, r.width, r.height)),
                "{label}: ring strip {r:?} must arrive as a DamageNotify, got {areas:?}",
            );
        }
    }
}

/// The resize ring damage follows the same two Xorg gates as the
/// border-source one — nothing at `bw == 0` (`HasBorder(pWin)`) and
/// nothing for a non-viewable window (`pWin->viewable`,
/// `dix/window.c:1586`) — so an unbordered resize keeps reporting
/// exactly the one full-extent rect it always did.
#[test]
fn configure_window_resize_damages_no_ring_unbordered_or_unmapped() {
    use crate::{
        resources::RedirectedBacking,
        server::{CompositeRedirectMode, DamageObject, RedirectRecord},
    };
    use yserver_protocol::x11::xfixes::RegionRect;
    const DAMAGE_XID: u32 = 0x0080_9146;
    for (bw, map) in [(0u16, true), (2, false)] {
        let mut state = ServerState::new();
        let mut peer = install_client(&mut state, 1);
        let win = ResourceId(0x0080_0001);
        seed_window(&mut state, win, ROOT_WINDOW, 128, 64);
        if let Some(w) = state.resources.window_mut(win) {
            w.border_width = bw;
            w.redirected_backing = Some(RedirectedBacking {
                host_pixmap: crate::backend::PixmapHandle::from_raw_for_test(0x0050_0001),
                width: 128 + 2 * bw,
                height: 64 + 2 * bw,
                depth: 24,
            });
        }
        if map {
            assert!(
                state.resources.map_window(win).mapping_changed,
                "window must map"
            );
        }
        state
            .composite_redirects
            .redirect_window(
                win,
                RedirectRecord {
                    mode: CompositeRedirectMode::Manual,
                    owner: ClientId(1),
                },
            )
            .unwrap();
        state.damage_objects.insert(
            DAMAGE_XID,
            DamageObject {
                owner: ClientId(1),
                drawable: win,
                level: 0,
                rects: Vec::new(),
                pending_notify_fired: false,
                last_reported_geometry: None,
            },
        );

        let mut body = Vec::with_capacity(16);
        body.extend_from_slice(&win.0.to_le_bytes());
        body.extend_from_slice(&0x000cu16.to_le_bytes());
        body.extend_from_slice(&[0u8; 2]);
        body.extend_from_slice(&64u32.to_le_bytes());
        body.extend_from_slice(&32u32.to_le_bytes());
        run_border_request(&mut state, 12, 0, &body);
        assert_no_error(&read_all_available(&mut peer), "ConfigureWindow resize");

        let expected: Vec<RegionRect> = if map {
            vec![RegionRect {
                x: 0,
                y: 0,
                width: 64,
                height: 32,
            }]
        } else {
            Vec::new()
        };
        assert_eq!(
            state.damage_objects[&DAMAGE_XID].rects, expected,
            "bw={bw} mapped={map}: no ring to report on a resize",
        );
    }
}

/// A tile with no host storage degrades to the pixel bit with a
/// defined value, mirroring the background block: a backend that
/// cannot sample the tile is better off with a solid colour than
/// with a dangling xid.
#[test]
fn change_window_attributes_border_pixmap_without_host_storage_degrades_to_pixel() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let win = ResourceId(0x0080_0001);
    seed_window(&mut state, win, ROOT_WINDOW, 10, 10);
    let host_xid = state
        .resources
        .window(win)
        .and_then(|w| w.host_xid)
        .expect("host xid")
        .as_raw();
    let tile = ResourceId(0x0080_1301);
    state.resources.create_pixmap(
        ClientId(1),
        CreatePixmapRequest {
            pixmap: tile,
            drawable: ROOT_WINDOW,
            width: 8,
            height: 8,
            depth: 24,
        },
    );

    let body = border_cwa_body(win.0, CWA_BORDER_PIXMAP, &[tile.0]);
    let calls = run_border_request_recording(&mut state, 2, 0, &body);
    assert_no_error(&read_all_available(&mut peer), "hostless border tile");
    assert_eq!(
        border_forwards(&calls),
        vec![(host_xid, CWA_BORDER_PIXEL, vec![0])],
    );
}

/// A CWA changing background AND border produces BOTH forwards —
/// the border block is a sibling of the background block, not a
/// replacement, and each carries only its own bits.
#[test]
fn change_window_attributes_background_and_border_forward_separately() {
    const CWA_BACK_PIXEL: u32 = 0x0002;
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let win = ResourceId(0x0080_0001);
    seed_window(&mut state, win, ROOT_WINDOW, 10, 10);
    let host_xid = state
        .resources
        .window(win)
        .and_then(|w| w.host_xid)
        .expect("host xid")
        .as_raw();

    // Value list in ascending mask-bit order: back-pixel, then
    // border-pixel.
    let body = border_cwa_body(
        win.0,
        CWA_BACK_PIXEL | CWA_BORDER_PIXEL,
        &[0x0000_00ff, 0x00ff_0000],
    );
    let calls = run_border_request_recording(&mut state, 2, 0, &body);
    assert_no_error(&read_all_available(&mut peer), "bg + border in one CWA");
    let cwa: Vec<_> = calls
        .iter()
        .filter_map(|c| match c {
            RecordedCall::ChangeSubwindowAttributes {
                host_xid,
                value_mask,
                values,
            } => Some((*host_xid, *value_mask, values.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        cwa,
        vec![
            (host_xid, CWA_BACK_PIXEL, vec![0x0000_00ff]),
            (host_xid, CWA_BORDER_PIXEL, vec![0x00ff_0000]),
        ],
        "background forward first, then the border forward"
    );
}

/// CreateWindow forwards the INITIAL border source too. Without it
/// a window created with a border pixel that is never changed again
/// would leave the backend mirror with no source at all — and
/// CreateWindow inherits the parent's border (`dix/window.c:879`),
/// so even a client supplying no border attribute can start out
/// with a non-default one.
#[test]
fn create_window_forwards_the_initial_border_source() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let body = border_create_body(0x0080_0001, ROOT_WINDOW.0, CWA_BORDER_PIXEL, &[0x00ff_0000]);
    let calls = run_border_request_recording(&mut state, 1, 24, &body);
    assert_no_error(&read_all_available(&mut peer), "create with border pixel");
    let host_xid = state
        .resources
        .window(ResourceId(0x0080_0001))
        .and_then(|w| w.host_xid)
        .expect("created window has a host xid")
        .as_raw();
    assert_eq!(
        border_forwards(&calls),
        vec![(host_xid, CWA_BORDER_PIXEL, vec![0x00ff_0000])],
    );
}

/// xts5 Xlib4 `XChangeWindowAttributes` 22-25 / Xlib4
/// `XCreateWindow` 20-22 set a CWBackPixmap / CWColormap /
/// CWCursor value to a freed xid and expect BadPixmap /
/// BadColormap / BadCursor. Mirrors Xorg's per-attribute
/// `dixLookupResource` order in `dix/window.c::CreateWindow` and
/// `ChangeWindowAttributes`.
#[test]
fn change_window_attributes_with_stale_value_returns_specific_bad_error() {
    use std::io::Read;
    const APP: u32 = 300;
    const WIN: u32 = 0x0090_0001;
    const BAD_PIX: u32 = 0x0090_dead;
    const BAD_CMAP: u32 = 0x0090_beef;
    const BAD_CURS: u32 = 0x0090_face;

    // (mask, value, expected_error, label)
    let cases: &[(u32, u32, u8, &str)] = &[
        (
            0x0001,
            BAD_PIX,
            yserver_protocol::x11::error::BAD_PIXMAP,
            "CWBackPixmap",
        ),
        (
            0x2000,
            BAD_CMAP,
            yserver_protocol::x11::error::BAD_COLORMAP,
            "CWColormap",
        ),
        (
            0x4000,
            BAD_CURS,
            yserver_protocol::x11::error::BAD_CURSOR,
            "CWCursor",
        ),
    ];

    for (i, (mask, value, expected_err, label)) in cases.iter().enumerate() {
        let mut state = ServerState::new();
        let app_id = APP + i as u32;
        let mut peer = install_client(&mut state, app_id);
        let mut backend = RecordingBackend::new();

        // Create a live target window owned by this client so the
        // BadWindow gate doesn't fire ahead of the value check.
        let win = WIN + i as u32;
        state.resources.create_window(
            ClientId(app_id),
            yserver_protocol::x11::CreateWindowRequest {
                depth: 24,
                window: ResourceId(win),
                parent: crate::resources::ROOT_WINDOW,
                x: 0,
                y: 0,
                width: 10,
                height: 10,
                border_width: 0,
                class: 1,
                visual: crate::resources::ROOT_VISUAL,
                ..Default::default()
            },
        );

        let mut body = Vec::with_capacity(12);
        body.extend_from_slice(&win.to_le_bytes());
        body.extend_from_slice(&mask.to_le_bytes());
        body.extend_from_slice(&value.to_le_bytes());
        let length_units = u32::try_from(1 + body.len().div_ceil(4)).expect("body length fits");

        process_request(
            &mut state,
            &mut backend,
            ClientId(app_id),
            SequenceNumber(1),
            RequestHeader {
                opcode: 2,
                data: 0,
                length_units,
            },
            &body,
            None,
        )
        .expect("process_request");

        peer.set_nonblocking(true).unwrap();
        let mut buf = [0u8; 32];
        peer.read_exact(&mut buf).unwrap_or_else(|e| {
            panic!("{label}: expected 32-byte error reply, got {e:?}");
        });
        assert_eq!(
            buf[0], 0,
            "{label}: byte 0 = error class (0); got {}",
            buf[0]
        );
        assert_eq!(
            buf[1], *expected_err,
            "{label}: expected error code {expected_err} for stale xid; got {}",
            buf[1],
        );
        let bad_value = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
        assert_eq!(
            bad_value, *value,
            "{label}: error_value must echo the stale resource xid"
        );
        assert_eq!(
            buf[10], 2,
            "{label}: error must carry the ChangeWindowAttributes \
                 major opcode (2); got {}",
            buf[10],
        );
    }
}

/// xts5 Xlib4 ConfigureWindow validations beyond BadWindow.
/// Mirrors Xorg `dix/window.c::ConfigureWindow:2203-2266` — every
/// check fires *before* any state mutation.
#[test]
fn configure_window_validates_inputonly_sibling_and_size() {
    use std::io::Read;
    const APP: u32 = 400;
    const PARENT: u32 = 0x00a0_0001;
    const INPUT_ONLY: u32 = 0x00a0_0002;
    const SIB_A: u32 = 0x00a0_0003;
    const SIB_B: u32 = 0x00a0_0004;
    const ORPHAN: u32 = 0x00a0_0005;
    const BAD_SIB: u32 = 0x00a0_dead;
    const CW_X: u16 = 0x0001;
    const CW_WIDTH: u16 = 0x0004;
    const CW_HEIGHT: u16 = 0x0008;
    const CW_BORDER_WIDTH: u16 = 0x0010;
    const CW_SIBLING: u16 = 0x0020;
    const CW_STACK_MODE: u16 = 0x0040;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, APP);
    let mut backend = RecordingBackend::new();

    // Two siblings under PARENT and one orphan that's not their sibling.
    for (id, parent, class) in [
        (PARENT, crate::resources::ROOT_WINDOW.0, 1u16),
        (INPUT_ONLY, PARENT, 2u16),
        (SIB_A, PARENT, 1u16),
        (SIB_B, PARENT, 1u16),
        (ORPHAN, crate::resources::ROOT_WINDOW.0, 1u16),
    ] {
        state.resources.create_window(
            ClientId(APP),
            yserver_protocol::x11::CreateWindowRequest {
                depth: 24,
                window: ResourceId(id),
                parent: ResourceId(parent),
                x: 0,
                y: 0,
                width: 50,
                height: 50,
                border_width: 0,
                class,
                visual: crate::resources::ROOT_VISUAL,
                ..Default::default()
            },
        );
    }

    // (label, body, expected_error)
    let cases: Vec<(&str, Vec<u8>, u8)> = vec![
        (
            "InputOnly + CWBorderWidth → BadMatch",
            build_configure_body(INPUT_ONLY, CW_BORDER_WIDTH, &[1u16.to_le_bytes()]),
            yserver_protocol::x11::error::BAD_MATCH,
        ),
        (
            "CWSibling without CWStackMode → BadMatch",
            build_configure_body(
                SIB_A,
                CW_SIBLING,
                [SIB_B.to_le_bytes(); 1]
                    .iter()
                    .map(|b| {
                        let mut buf = [0u8; 2];
                        buf.copy_from_slice(&b[..2]);
                        buf
                    })
                    .collect::<Vec<_>>()
                    .as_slice(),
            ),
            yserver_protocol::x11::error::BAD_MATCH,
        ),
        (
            "CWSibling with non-sibling → BadMatch",
            build_configure_body_u32(SIB_A, CW_SIBLING | CW_STACK_MODE, ORPHAN, 0),
            yserver_protocol::x11::error::BAD_MATCH,
        ),
        (
            "CWSibling with unknown xid → BadWindow",
            build_configure_body_u32(SIB_A, CW_SIBLING | CW_STACK_MODE, BAD_SIB, 0),
            yserver_protocol::x11::error::BAD_WINDOW,
        ),
        (
            "CWWidth=0 → BadValue",
            build_configure_body(SIB_A, CW_WIDTH, &[0u16.to_le_bytes()]),
            yserver_protocol::x11::error::BAD_VALUE,
        ),
        (
            "CWHeight=0 → BadValue",
            build_configure_body(SIB_A, CW_HEIGHT, &[0u16.to_le_bytes()]),
            yserver_protocol::x11::error::BAD_VALUE,
        ),
        (
            "CWX alone → no error (MOVE_WIN path)",
            build_configure_body(SIB_A, CW_X, &[5u16.to_le_bytes()]),
            0, // 0 means: expect NO error reply
        ),
    ];

    for (i, (label, body, expected_err)) in cases.iter().enumerate() {
        let seq = SequenceNumber((i + 1) as u16);
        let length_units = u32::try_from(1 + body.len().div_ceil(4)).expect("body length fits");
        process_request(
            &mut state,
            &mut backend,
            ClientId(APP),
            seq,
            RequestHeader {
                opcode: 12,
                data: 0,
                length_units,
            },
            body,
            None,
        )
        .expect("process_request");

        peer.set_nonblocking(true).unwrap();
        let mut buf = [0u8; 32];
        match peer.read_exact(&mut buf) {
            Ok(()) => {
                if *expected_err == 0 {
                    panic!("{label}: did not expect an error reply, got {buf:?}");
                }
                assert_eq!(buf[0], 0, "{label}: byte 0 = error class (0)");
                assert_eq!(
                    buf[1], *expected_err,
                    "{label}: error code mismatch — got {}",
                    buf[1]
                );
            }
            Err(e) if *expected_err == 0 => {
                // Expected — no error reply.
                assert_eq!(e.kind(), std::io::ErrorKind::WouldBlock);
            }
            Err(e) => {
                panic!("{label}: expected error reply with code {expected_err}, got {e:?}")
            }
        }
    }
}

fn build_configure_body(window_xid: u32, value_mask: u16, values: &[[u8; 2]]) -> Vec<u8> {
    let mut body = Vec::with_capacity(8 + values.len() * 4);
    body.extend_from_slice(&window_xid.to_le_bytes());
    body.extend_from_slice(&value_mask.to_le_bytes());
    body.extend_from_slice(&0u16.to_le_bytes()); // pad
    for v in values {
        body.extend_from_slice(v);
        body.extend_from_slice(&0u16.to_le_bytes()); // each value is u32 on the wire
    }
    body
}

fn build_configure_body_u32(window_xid: u32, value_mask: u16, v0: u32, v1: u32) -> Vec<u8> {
    let mut body = Vec::with_capacity(16);
    body.extend_from_slice(&window_xid.to_le_bytes());
    body.extend_from_slice(&value_mask.to_le_bytes());
    body.extend_from_slice(&0u16.to_le_bytes()); // pad
    body.extend_from_slice(&v0.to_le_bytes());
    body.extend_from_slice(&v1.to_le_bytes());
    body
}

/// xts5 Xlib4 XCreateWindow CreateWindow class/border/depth rules:
/// parent InputOnly + child InputOutput → BadMatch (test 43),
/// InputOnly + non-zero border_width → BadMatch (test 31),
/// InputOnly + non-zero depth → BadMatch (test 44).
#[test]
fn create_window_validates_class_constraints() {
    use std::io::Read;
    const APP: u32 = 500;
    const PARENT_IPO: u32 = 0x00b0_0001;
    const PARENT_IPI: u32 = 0x00b0_0002;
    const CHILD: u32 = 0x00b0_0003;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, APP);
    let mut backend = RecordingBackend::new();

    // One InputOutput parent and one InputOnly parent.
    for (id, parent, class) in [
        (PARENT_IPO, crate::resources::ROOT_WINDOW.0, 1u16),
        (PARENT_IPI, crate::resources::ROOT_WINDOW.0, 2u16),
    ] {
        state.resources.create_window(
            ClientId(APP),
            yserver_protocol::x11::CreateWindowRequest {
                depth: 24,
                window: ResourceId(id),
                parent: ResourceId(parent),
                x: 0,
                y: 0,
                width: 50,
                height: 50,
                border_width: 0,
                class,
                visual: crate::resources::ROOT_VISUAL,
                ..Default::default()
            },
        );
    }

    // (label, depth, parent, class, border_width, expected_err)
    let cases: &[(&str, u8, u32, u16, u16, u8)] = &[
        (
            "parent InputOnly + class InputOutput",
            24,
            PARENT_IPI,
            1,
            0,
            yserver_protocol::x11::error::BAD_MATCH,
        ),
        (
            "class InputOnly + non-zero border_width",
            0,
            PARENT_IPO,
            2,
            1,
            yserver_protocol::x11::error::BAD_MATCH,
        ),
        (
            "class InputOnly + non-zero depth",
            24,
            PARENT_IPO,
            2,
            0,
            yserver_protocol::x11::error::BAD_MATCH,
        ),
    ];

    for (i, (label, depth, parent, class, bw, expected_err)) in cases.iter().enumerate() {
        let mut body = Vec::with_capacity(32);
        body.extend_from_slice(&CHILD.to_le_bytes());
        body.extend_from_slice(&parent.to_le_bytes());
        body.extend_from_slice(&0i16.to_le_bytes()); // x
        body.extend_from_slice(&0i16.to_le_bytes()); // y
        body.extend_from_slice(&10u16.to_le_bytes()); // width
        body.extend_from_slice(&10u16.to_le_bytes()); // height
        body.extend_from_slice(&bw.to_le_bytes()); // border_width
        body.extend_from_slice(&class.to_le_bytes()); // class
        body.extend_from_slice(&0u32.to_le_bytes()); // visual = CopyFromParent
        body.extend_from_slice(&0u32.to_le_bytes()); // value_mask
        let length_units = u32::try_from(1 + body.len().div_ceil(4)).expect("body fits");
        let seq = SequenceNumber((i + 1) as u16);
        process_request(
            &mut state,
            &mut backend,
            ClientId(APP),
            seq,
            RequestHeader {
                opcode: 1,
                data: *depth,
                length_units,
            },
            &body,
            None,
        )
        .expect("process_request");

        peer.set_nonblocking(true).unwrap();
        let mut buf = [0u8; 32];
        peer.read_exact(&mut buf)
            .unwrap_or_else(|e| panic!("{label}: expected error reply, got {e:?}"));
        assert_eq!(buf[0], 0, "{label}: byte 0 = error class");
        assert_eq!(
            buf[1], *expected_err,
            "{label}: expected error code {expected_err}, got {}",
            buf[1]
        );
        assert_eq!(
            buf[10], 1,
            "{label}: BadMatch must carry CreateWindow opcode"
        );
    }
}
