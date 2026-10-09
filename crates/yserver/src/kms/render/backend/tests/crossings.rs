use super::*;

/// Windows for the tree-change crossing tests: `A` (0,0 200x200) with
/// child `C` (50,50 100x100), and a top-level `B` (50,50 100x100) above
/// `A`, all unmapped; the pointer rests at (100,100) on the root. Client
/// 14 selects crossings and StructureNotify on each and on the root.
fn tree_crossing_fixture() -> (
    yserver_core::server::ServerState,
    KmsBackend,
    std::os::unix::net::UnixStream,
) {
    use yserver_core::{resources::ROOT_WINDOW, server::ServerState};
    let mut state = ServerState::new();
    let mut b = KmsBackend::for_tests();
    let peer = kbd_map_client_id(&mut state, 14);
    seed_state_window(&mut state, &mut b, TREE_A, ROOT_WINDOW, 0, 0, 200, 200);
    seed_state_window(&mut state, &mut b, TREE_C, TREE_A, 50, 50, 100, 100);
    seed_state_window(&mut state, &mut b, TREE_B, ROOT_WINDOW, 50, 50, 100, 100);
    let masks = &mut state.clients.get_mut(&14).unwrap().event_masks;
    masks.insert(ROOT_WINDOW, 0x30);
    for w in [TREE_A, TREE_B, TREE_C] {
        masks.insert(w, 0x0002_0030);
        b.core.xid_map.insert(synth_host_xid(w), w);
    }
    b.core.xid_map.insert(b.core.window_id, ROOT_WINDOW);
    b.core.cursor_x = 100.0;
    b.core.cursor_y = 100.0;
    b.core.prev_pointer_window = Some(b.core.window_id);
    (state, b, peer)
}

const TREE_A: yserver_protocol::x11::ResourceId = yserver_protocol::x11::ResourceId(0x0010_0a01);
const TREE_B: yserver_protocol::x11::ResourceId = yserver_protocol::x11::ResourceId(0x0010_0a02);
const TREE_C: yserver_protocol::x11::ResourceId = yserver_protocol::x11::ResourceId(0x0010_0a03);

/// The events client 14 got, one line each: Map/Unmap/Configure/Destroy
/// by window, crossings as `Enter|Leave <event> <detail> child=<child>`.
fn tree_events(peer: &mut std::os::unix::net::UnixStream) -> Vec<String> {
    let name = |xid: u32| match xid {
        0 => "None".to_string(),
        x if x == TREE_A.0 => "A".to_string(),
        x if x == TREE_B.0 => "B".to_string(),
        x if x == TREE_C.0 => "C".to_string(),
        x if x == yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0 => "COW".to_string(),
        x if x == yserver_core::resources::ROOT_WINDOW.0 => "root".to_string(),
        x => format!("{x:#x}"),
    };
    let word = |e: &[u8], at: usize| u32::from_le_bytes(e[at..at + 4].try_into().unwrap());
    let details = [
        "Ancestor",
        "Virtual",
        "Inferior",
        "Nonlinear",
        "NonlinearVirtual",
    ];
    kbd_map_drain(peer)
        .chunks(32)
        .filter_map(|e| match e[0] & 0x7f {
            7 | 8 => Some(format!(
                "{} {} {} child={} mode={}",
                if e[0] & 0x7f == 7 { "Enter" } else { "Leave" },
                name(word(e, 12)),
                details[usize::from(e[1])],
                name(word(e, 16)),
                e[30],
            )),
            17 => Some(format!("Destroy {}", name(word(e, 8)))),
            18 => Some(format!("Unmap {}", name(word(e, 8)))),
            19 => Some(format!("Map {}", name(word(e, 8)))),
            21 => Some(format!(
                "Reparent {} to {}",
                name(word(e, 8)),
                name(word(e, 12))
            )),
            22 => Some(format!("Configure {}", name(word(e, 8)))),
            26 => Some(format!("Circulate {} place={}", name(word(e, 8)), e[16])),
            _ => None,
        })
        .collect()
}

fn tree_request(
    state: &mut yserver_core::server::ServerState,
    b: &mut KmsBackend,
    opcode: u8,
    window: yserver_protocol::x11::ResourceId,
) {
    dispatch_raw(state, b, opcode, 0, &window.0.to_le_bytes());
}

/// An InputOnly child (no backend window) of `A` at (100,20) 40x40
/// with a cursor: the pointer enters it, shows its cursor and clicks
/// on it, as on Xorg (tools/vng-scenarios/goldens/cursor.txt: dtwm's
/// frame resize handles are such windows).
#[test]
fn pointer_enters_input_only_window_and_shows_its_cursor() {
    use yserver_core::{backend::Backend, core_loop::message::HostInputEvent};
    use yserver_protocol::x11::{ClientId, CreateWindowRequest, ResourceId};
    const ONLY: ResourceId = ResourceId(0x0010_0a07);
    const CURSOR: ResourceId = ResourceId(0x0010_0a08);
    const CURSOR_HOST: u32 = 0x00ab_0001;
    let (mut state, mut b, mut peer) = tree_crossing_fixture();
    tree_request(&mut state, &mut b, 8, TREE_A);
    state.resources.create_glyph_cursor(ClientId(14), CURSOR);
    state.resources.set_cursor_host_xid(
        CURSOR,
        yserver_core::backend::CursorHandle::from_raw(CURSOR_HOST).unwrap(),
    );
    state.resources.create_window(
        ClientId(14),
        CreateWindowRequest {
            window: ONLY,
            parent: TREE_A,
            x: 100,
            y: 20,
            width: 40,
            height: 40,
            class: 2,
            cursor: Some(CURSOR),
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(ONLY);
    state
        .clients
        .get_mut(&14)
        .unwrap()
        .event_masks
        .insert(ONLY, 0x0000_003c);
    b.core.cursor_x = 180.0;
    b.core.cursor_y = 150.0;
    b.windows_restructured(&mut state);
    let _ = tree_events(&mut peer);
    let root_cursor = b.effective_cursor_xid;

    let motion = |x: i32, y: i32| HostInputEvent::PointerMotion {
        origin: yserver_core::core_loop::InputOrigin::NestedHost,
        motion_delta: None,
        x,
        y,
        time: 0,
        relative: false,
        dx: 0,
        dy: 0,
    };
    b.on_host_input(&mut state, motion(120, 40));
    assert_eq!(
        tree_events(&mut peer),
        [
            "Leave A Inferior child=None mode=0",
            "Enter 0x100a07 Ancestor child=None mode=0",
        ],
    );
    assert_eq!(b.effective_cursor_xid, Some(CURSOR_HOST));

    b.on_host_input(
        &mut state,
        HostInputEvent::PointerButton {
            origin: yserver_core::core_loop::InputOrigin::NestedHost,
            button: 0x110,
            pressed: true,
            time: 0,
        },
    );
    let press = kbd_map_drain(&mut peer);
    let press = press
        .chunks(32)
        .find(|e| e[0] & 0x7f == 4)
        .expect("ButtonPress");
    assert_eq!(
        &press[12..16],
        &ONLY.0.to_le_bytes(),
        "on the InputOnly window"
    );
    assert_eq!(
        (
            i16::from_le_bytes([press[24], press[25]]),
            i16::from_le_bytes([press[26], press[27]])
        ),
        (20, 20),
        "event coordinates relative to it"
    );
    b.on_host_input(
        &mut state,
        HostInputEvent::PointerButton {
            origin: yserver_core::core_loop::InputOrigin::NestedHost,
            button: 0x110,
            pressed: false,
            time: 0,
        },
    );

    b.on_host_input(&mut state, motion(180, 150));
    assert_eq!(
        tree_events(&mut peer),
        [
            "Leave 0x100a07 Ancestor child=None mode=0",
            "Enter A Inferior child=None mode=0"
        ],
    );
    assert_eq!(b.effective_cursor_xid, root_cursor);
}

/// #196: an InputOnly window has no backend window, yet holds its
/// cursor like any window (Xorg `dix/window.c:1536`): after FreeCursor
/// the pointer entering it still shows that cursor, which is destroyed
/// only once the window changes cursor (`:1559`) or dies (`:968`).
#[test]
fn input_only_window_keeps_its_freed_cursor_until_it_drops_it() {
    use yserver_core::{backend::Backend, core_loop::message::HostInputEvent};
    use yserver_protocol::x11::{ClientId, CreateWindowRequest, ResourceId};
    const ONLY: ResourceId = ResourceId(0x0010_0a07);
    const OTHER: ResourceId = ResourceId(0x0010_0a09);
    const CURSOR: ResourceId = ResourceId(0x0010_0a08);
    const CURSOR2: ResourceId = ResourceId(0x0010_0a0a);
    let (mut state, mut b, mut peer) = tree_crossing_fixture();
    tree_request(&mut state, &mut b, 8, TREE_A);
    let (c, c2) = (test_cursor(&mut b), test_cursor(&mut b));
    for (id, host) in [(CURSOR, c), (CURSOR2, c2)] {
        state.resources.create_glyph_cursor(ClientId(14), id);
        state.resources.set_cursor_host_xid(
            id,
            yserver_core::backend::CursorHandle::from_raw(host).unwrap(),
        );
    }
    for (window, x, cursor) in [(ONLY, 100, CURSOR), (OTHER, 10, CURSOR2)] {
        state.resources.create_window(
            ClientId(14),
            CreateWindowRequest {
                window,
                parent: TREE_A,
                x,
                y: 20,
                width: 40,
                height: 40,
                class: 2,
                cursor: Some(cursor),
                ..Default::default()
            },
        );
        let _ = state.resources.map_window(window);
    }
    b.core.cursor_x = 180.0;
    b.core.cursor_y = 150.0;
    b.windows_restructured(&mut state);
    let motion = |x: i32, y: i32| HostInputEvent::PointerMotion {
        origin: yserver_core::core_loop::InputOrigin::NestedHost,
        motion_delta: None,
        x,
        y,
        time: 0,
        relative: false,
        dx: 0,
        dy: 0,
    };
    b.on_host_input(&mut state, motion(120, 40));
    assert_eq!(b.effective_cursor_xid, Some(c));
    b.on_host_input(&mut state, motion(20, 40));
    assert_eq!(b.effective_cursor_xid, Some(c2));
    b.on_host_input(&mut state, motion(180, 150));
    let root_cursor = b.effective_cursor_xid;

    dispatch_raw(&mut state, &mut b, 95, 0, &CURSOR.0.to_le_bytes());
    dispatch_raw(&mut state, &mut b, 95, 0, &CURSOR2.0.to_le_bytes());
    assert!(b.cursor_records.contains_key(&c) && b.cursor_records.contains_key(&c2));
    b.on_host_input(&mut state, motion(120, 40));
    assert_eq!(b.effective_cursor_xid, Some(c), "the window still shows it");

    // ChangeWindowAttributes(cursor = None) under the pointer.
    let mut body = ONLY.0.to_le_bytes().to_vec();
    body.extend(0x4000u32.to_le_bytes());
    body.extend(0u32.to_le_bytes());
    dispatch_raw(&mut state, &mut b, 2, 0, &body);
    assert_eq!(b.effective_cursor_xid, root_cursor, "inherits A's again");
    assert!(!b.cursor_records.contains_key(&c), "dropped: destroyed");
    assert!(b.cursor_records.contains_key(&c2), "OTHER still holds it");

    tree_request(&mut state, &mut b, 4, OTHER);
    assert!(!b.cursor_records.contains_key(&c2), "destroyed with OTHER");
    assert!(b.released_cursors.is_empty());
    assert!(
        b.default_cursor_xid
            .is_some_and(|d| b.cursor_records.contains_key(&d)),
        "the root X_cursor is never released"
    );
    let _ = tree_events(&mut peer);
}

/// Xvfb, pointer still at the centre: MapWindow of a window under it
/// sends MapNotify, then Leave(root, Inferior) / Enter(A, Ancestor), in
/// the same request (Xorg MapWindow → WindowsRestructured).
#[test]
fn map_under_a_still_pointer_crosses_within_the_request() {
    let (mut state, mut b, mut peer) = tree_crossing_fixture();
    tree_request(&mut state, &mut b, 8, TREE_A);
    assert_eq!(
        tree_events(&mut peer),
        [
            "Map A",
            "Leave root Inferior child=None mode=0",
            "Enter A Ancestor child=None mode=0",
        ],
    );
    assert_eq!(b.core.prev_pointer_window, Some(synth_host_xid(TREE_A)));
}

/// Xvfb: unmapping the window under the pointer hands it to the window
/// it revealed — UnmapNotify, then Leave(B) / Enter(A), both Nonlinear.
#[test]
fn unmap_under_a_still_pointer_enters_the_revealed_window() {
    let (mut state, mut b, mut peer) = tree_crossing_fixture();
    tree_request(&mut state, &mut b, 8, TREE_A);
    tree_request(&mut state, &mut b, 8, TREE_B);
    let _ = tree_events(&mut peer);
    tree_request(&mut state, &mut b, 10, TREE_B);
    assert_eq!(
        tree_events(&mut peer),
        [
            "Unmap B",
            "Leave B Nonlinear child=None mode=0",
            "Enter A Nonlinear child=None mode=0",
        ],
    );
}

/// Xvfb: moving a window out from under the pointer leaves its child
/// (Ancestor), the window itself (Virtual, child = C) and enters the
/// root (Inferior), after the ConfigureNotify; moving it to where it
/// already is sends no crossing.
#[test]
fn configure_under_a_still_pointer_crosses_after_configure_notify() {
    let (mut state, mut b, mut peer) = tree_crossing_fixture();
    tree_request(&mut state, &mut b, 8, TREE_A);
    dispatch_raw(&mut state, &mut b, 9, 0, &TREE_A.0.to_le_bytes());
    let _ = tree_events(&mut peer);
    dispatch_configure_window(&mut state, &mut b, TREE_A, Some(0), Some(0), None);
    assert_eq!(tree_events(&mut peer), Vec::<String>::new());
    dispatch_configure_window(&mut state, &mut b, TREE_A, Some(300), Some(300), None);
    assert_eq!(
        tree_events(&mut peer),
        [
            "Configure A",
            "Leave C Ancestor child=None mode=0",
            "Leave A Virtual child=C mode=0",
            "Enter root Inferior child=None mode=0",
        ],
    );
}

/// Xvfb: the last ReleaseOverlayWindow with the pointer on the COW
/// unmaps it, leaves it for the root while it still exists, and only
/// then destroys it (Xorg compDestroyOverlayWindow → DeleteWindow).
#[test]
fn release_overlay_under_a_still_pointer_leaves_before_destroy_notify() {
    use yserver_core::resources::{COMPOSITE_OVERLAY_WINDOW, ROOT_WINDOW};
    let (mut state, mut b, mut peer) = tree_crossing_fixture();
    let masks = &mut state.clients.get_mut(&14).unwrap().event_masks;
    masks.insert(ROOT_WINDOW, 0x0008_0030);
    masks.insert(COMPOSITE_OVERLAY_WINDOW, 0x0002_0030);
    dispatch_raw(&mut state, &mut b, 144, 7, &ROOT_WINDOW.0.to_le_bytes());
    assert_eq!(b.core.prev_pointer_window, Some(COMPOSITE_OVERLAY_WINDOW.0));
    let _ = tree_events(&mut peer);
    dispatch_raw(&mut state, &mut b, 144, 8, &ROOT_WINDOW.0.to_le_bytes());
    assert_eq!(
        tree_events(&mut peer),
        [
            "Unmap COW",
            "Unmap COW",
            "Leave COW Ancestor child=None mode=0",
            "Enter root Inferior child=None mode=0",
            "Destroy COW",
            "Destroy COW",
        ],
    );
    assert_eq!(b.core.prev_pointer_window, Some(b.core.window_id));
}

/// Xvfb: destroying a window whose child holds the pointer unmaps it
/// first — UnmapNotify(A), the crossings out of C and A while both still
/// exist, and only then the DestroyNotifys. The pointer never refers to
/// a destroyed window afterwards.
#[test]
fn destroy_under_a_still_pointer_leaves_before_destroy_notify() {
    let (mut state, mut b, mut peer) = tree_crossing_fixture();
    tree_request(&mut state, &mut b, 8, TREE_A);
    dispatch_raw(&mut state, &mut b, 9, 0, &TREE_A.0.to_le_bytes());
    let _ = tree_events(&mut peer);
    tree_request(&mut state, &mut b, 4, TREE_A);
    assert_eq!(
        tree_events(&mut peer),
        [
            "Unmap A",
            "Leave C Ancestor child=None mode=0",
            "Leave A Virtual child=C mode=0",
            "Enter root Inferior child=None mode=0",
            "Destroy C",
            "Destroy A",
        ],
    );
    assert_eq!(b.core.prev_pointer_window, Some(b.core.window_id));
}

/// Xvfb: a bounding shape that misses the pointer takes the window out
/// from under it as an input shape would (`miSpriteTrace` checks
/// `PointInBorderSize`), and resetting it brings the pointer back.
#[test]
fn bounding_shape_off_the_pointer_leaves_the_window() {
    let (mut state, mut b, mut peer) = tree_crossing_fixture();
    tree_request(&mut state, &mut b, 8, TREE_A);
    tree_request(&mut state, &mut b, 8, TREE_B);
    let _ = tree_events(&mut peer);
    // SHAPE Rectangles(Set, Bounding, B, 0,0, [0,0 10x10]).
    let mut body = vec![0u8, 0, 0, 0];
    body.extend_from_slice(&TREE_B.0.to_le_bytes());
    body.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 10, 0, 10, 0]);
    dispatch_raw(&mut state, &mut b, 141, 1, &body);
    assert_eq!(
        tree_events(&mut peer),
        [
            "Leave B Nonlinear child=None mode=0",
            "Enter A Nonlinear child=None mode=0",
        ],
    );
    // SHAPE Mask(Set, Bounding, B, None) resets it.
    let mut body = vec![0u8, 0, 0, 0];
    body.extend_from_slice(&TREE_B.0.to_le_bytes());
    body.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0]);
    dispatch_raw(&mut state, &mut b, 141, 2, &body);
    assert_eq!(
        tree_events(&mut peer),
        [
            "Leave A Nonlinear child=None mode=0",
            "Enter B Nonlinear child=None mode=0",
        ],
    );
}

/// Xvfb: ReparentWindow of a mapped window is an UnmapWindow, the
/// reparent and a MapWindow — the pointer leaves the window at its old
/// place before ReparentNotify, and enters it at its new one after
/// MapNotify.
#[test]
fn reparent_of_a_mapped_window_unmaps_and_maps_it_around_the_pointer() {
    let (mut state, mut b, mut peer) = tree_crossing_fixture();
    tree_request(&mut state, &mut b, 8, TREE_A);
    tree_request(&mut state, &mut b, 8, TREE_B);
    let _ = tree_events(&mut peer);
    dispatch_reparent_window(&mut state, &mut b, TREE_B, TREE_A, 150, 150);
    assert_eq!(
        tree_events(&mut peer),
        [
            "Unmap B",
            "Leave B Nonlinear child=None mode=0",
            "Enter A Nonlinear child=None mode=0",
            "Reparent B to A",
            "Map B",
        ],
    );
    let root = yserver_core::resources::ROOT_WINDOW;
    dispatch_reparent_window(&mut state, &mut b, TREE_B, root, 50, 50);
    assert_eq!(
        tree_events(&mut peer),
        [
            "Unmap B",
            "Reparent B to root",
            "Map B",
            "Leave A Nonlinear child=None mode=0",
            "Enter B Nonlinear child=None mode=0",
        ],
    );
}

/// Xorg `CoreEnterLeaveEvent`: a crossing goes to the selections on
/// its own window and never propagates — a parent that selected
/// crossings gets its Leave(Inferior), not its child's Enter as well.
#[test]
fn core_crossings_do_not_propagate_to_the_parent() {
    let (mut state, mut b, mut peer) = tree_crossing_fixture();
    state
        .clients
        .get_mut(&14)
        .unwrap()
        .event_masks
        .remove(&TREE_C);
    tree_request(&mut state, &mut b, 8, TREE_A);
    let _ = tree_events(&mut peer);
    dispatch_raw(&mut state, &mut b, 9, 0, &TREE_A.0.to_le_bytes());
    assert_eq!(
        tree_events(&mut peer),
        ["Leave A Inferior child=None mode=0"],
    );
}

/// Xvfb, GrabPointer(E, owner_events=false, Enter|Leave): a window
/// mapped over E sends the grab client its Leave on E only — the
/// Enter on the new window is not the grab window's, so nobody gets it.
#[test]
fn crossings_under_a_grab_reach_only_the_grab_window() {
    let (mut state, mut b, mut peer) = tree_crossing_fixture();
    tree_request(&mut state, &mut b, 8, TREE_A);
    let _ = tree_events(&mut peer);
    let mut body = TREE_A.0.to_le_bytes().to_vec();
    body.extend_from_slice(&0x30u16.to_le_bytes());
    body.extend_from_slice(&[1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    dispatch_raw(&mut state, &mut b, 26, 0, &body);
    let _ = tree_events(&mut peer);
    tree_request(&mut state, &mut b, 8, TREE_B);
    assert_eq!(
        tree_events(&mut peer),
        ["Map B", "Leave A Nonlinear child=None mode=0"],
    );
}

/// XI2 crossings of client 15, which selected XI_Enter|XI_Leave on the
/// master pointers of `windows`: (evtype, deviceid, sourceid, mode,
/// detail, event window).
fn tree_xi2_crossings(
    state: &mut yserver_core::server::ServerState,
    windows: &[yserver_protocol::x11::ResourceId],
) -> std::os::unix::net::UnixStream {
    let peer = kbd_map_client_id(state, 15);
    let masks = &mut state.clients.get_mut(&15).unwrap().xi2_masks;
    for w in windows {
        masks.insert((*w, 1), (1 << 7) | (1 << 8));
    }
    peer
}

fn drain_xi2_crossings(
    peer: &mut std::os::unix::net::UnixStream,
) -> Vec<(u16, u16, u16, u8, u8, u32)> {
    let bytes = kbd_map_drain(peer);
    let half = |at: usize| u16::from_le_bytes([bytes[at], bytes[at + 1]]);
    let mut out = Vec::new();
    let mut at = 0;
    while at + 32 <= bytes.len() {
        let len = 32 + 4 * u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
        if bytes[at] == 35 {
            out.push((
                half(at + 8),
                half(at + 10),
                half(at + 16),
                bytes[at + 18],
                bytes[at + 19],
                u32::from_le_bytes(bytes[at + 24..at + 28].try_into().unwrap()),
            ));
        }
        at += if bytes[at] == 35 { len } else { 32 };
    }
    out
}

/// Xvfb: GrabPointer's Grab-mode crossings and a tree change's Normal
/// ones reach XI2 selectors too, from the master pointer (sourceid 2:
/// Xorg passes the master's id when no device event caused them).
#[test]
fn grab_and_tree_change_crossings_have_an_xi2_form_from_the_master() {
    let (mut state, mut b, _peer) = tree_crossing_fixture();
    let mut xi2 = tree_xi2_crossings(&mut state, &[TREE_A, TREE_B]);
    let (a, bb) = (TREE_A.0, TREE_B.0);
    tree_request(&mut state, &mut b, 8, TREE_A);
    assert_eq!(drain_xi2_crossings(&mut xi2), [(7, 2, 2, 0, 0, a)]);
    tree_request(&mut state, &mut b, 8, TREE_B);
    assert_eq!(
        drain_xi2_crossings(&mut xi2),
        [(8, 2, 2, 0, 3, a), (7, 2, 2, 0, 3, bb)],
        "map B over A: Leave(A) Enter(B), Nonlinear",
    );
    // GrabPointer(A) by client 14: the Grab-mode chain B -> A.
    let mut body = TREE_A.0.to_le_bytes().to_vec();
    body.extend_from_slice(&0x30u16.to_le_bytes());
    body.extend_from_slice(&[1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    dispatch_raw(&mut state, &mut b, 26, 0, &body);
    assert_eq!(
        drain_xi2_crossings(&mut xi2),
        [(8, 2, 2, 1, 3, bb), (7, 2, 2, 1, 3, a)],
    );
}

/// Xvfb: CirculateWindow(LowerHighest) lowers the highest MAPPED child
/// that overlaps a sibling below it (an unmapped one above is skipped),
/// RaiseLowest raises the lowest one a sibling above overlaps; each sends
/// CirculateNotify, then the crossings of the restack.
#[test]
fn circulate_picks_the_overlapping_mapped_child() {
    let (mut state, mut b, mut peer) = tree_crossing_fixture();
    let unmapped = yserver_protocol::x11::ResourceId(0x0010_0a05);
    let root = yserver_core::resources::ROOT_WINDOW;
    seed_state_window(&mut state, &mut b, unmapped, root, 0, 0, 300, 300);
    tree_request(&mut state, &mut b, 8, TREE_A);
    tree_request(&mut state, &mut b, 8, TREE_B);
    let _ = tree_events(&mut peer);
    dispatch_raw(&mut state, &mut b, 13, 1, &root.0.to_le_bytes());
    assert_eq!(
        tree_events(&mut peer),
        [
            "Circulate B place=1",
            "Leave B Nonlinear child=None mode=0",
            "Enter A Nonlinear child=None mode=0",
        ],
    );
    dispatch_raw(&mut state, &mut b, 13, 0, &root.0.to_le_bytes());
    assert_eq!(
        tree_events(&mut peer),
        [
            "Circulate B place=0",
            "Leave A Nonlinear child=None mode=0",
            "Enter B Nonlinear child=None mode=0",
        ],
    );
}

/// Xorg `CoreEnterLeaveEvent`: the `focus` flag is set only on the
/// focus window and its inferiors (or everywhere under PointerRoot).
#[test]
fn crossing_focus_flag_follows_the_focus_window() {
    let (mut state, mut b, mut peer) = tree_crossing_fixture();
    state.core_focus.raw = TREE_A.0;
    tree_request(&mut state, &mut b, 8, TREE_A);
    dispatch_raw(&mut state, &mut b, 9, 0, &TREE_A.0.to_le_bytes());
    let flags = |peer: &mut std::os::unix::net::UnixStream| -> Vec<(u8, u32, u8)> {
        kbd_map_drain(peer)
            .chunks(32)
            .filter(|e| matches!(e[0] & 0x7f, 7 | 8))
            .map(|e| {
                (
                    e[0],
                    u32::from_le_bytes(e[12..16].try_into().unwrap()),
                    e[31],
                )
            })
            .collect()
    };
    let root = yserver_core::resources::ROOT_WINDOW.0;
    assert_eq!(
        flags(&mut peer),
        [
            (8, root, 2),
            (7, TREE_A.0, 3),
            (8, TREE_A.0, 3),
            (7, TREE_C.0, 3)
        ],
    );
    // Xvfb: a window over the focus window, not inside it, has no focus.
    tree_request(&mut state, &mut b, 8, TREE_B);
    assert_eq!(
        flags(&mut peer),
        [(8, TREE_C.0, 3), (8, TREE_A.0, 3), (7, TREE_B.0, 2)],
    );
}

/// Xvfb: DestroyWindow sends UnmapNotify for the destroyed window only,
/// and DestroyNotify for its inferiors topmost first (Xorg CrushTree).
#[test]
fn destroy_notifies_inferiors_topmost_first_without_unmap_notify() {
    use yserver_protocol::x11::ResourceId;
    let (mut state, mut b, mut peer) = tree_crossing_fixture();
    let top = ResourceId(0x0010_0a04);
    seed_state_window(&mut state, &mut b, top, TREE_A, 0, 0, 10, 10);
    state
        .clients
        .get_mut(&14)
        .unwrap()
        .event_masks
        .insert(top, 0x0002_0000);
    tree_request(&mut state, &mut b, 8, TREE_A);
    dispatch_raw(&mut state, &mut b, 9, 0, &TREE_A.0.to_le_bytes());
    let _ = tree_events(&mut peer);
    tree_request(&mut state, &mut b, 4, TREE_A);
    let events = tree_events(&mut peer);
    let structure: Vec<&String> = events
        .iter()
        .filter(|e| !e.starts_with("Enter") && !e.starts_with("Leave"))
        .collect();
    assert_eq!(
        structure,
        ["Unmap A", "Destroy 0x100a04", "Destroy C", "Destroy A"],
    );
}

/// A tree change is not user input: its crossings leave the idle clock
/// (and with it DPMS and the screen saver) alone.
#[test]
fn tree_change_crossings_do_not_count_as_activity() {
    let (mut state, mut b, mut peer) = tree_crossing_fixture();
    let idle_since = std::time::Instant::now() - std::time::Duration::from_secs(60);
    state.dpms.last_activity = idle_since;
    tree_request(&mut state, &mut b, 8, TREE_A);
    assert_eq!(tree_events(&mut peer).len(), 3);
    assert_eq!(state.dpms.last_activity, idle_since);
}
