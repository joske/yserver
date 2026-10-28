//! Requests wake the scene only through the changes they make: a request that
//! changes nothing on screen leaves the compose gate quiet.

use super::*;
use yserver_core::{backend::Backend, resources::ROOT_WINDOW, server::ServerState};
use yserver_protocol::x11::ResourceId;

const WINDOW: ResourceId = ResourceId(0x0140_0101);
const GC: ResourceId = ResourceId(0x0140_0102);
const CURSOR: ResourceId = ResourceId(0x0140_0103);

fn live_fixture() -> Option<(ServerState, KmsBackend)> {
    let mut backend = match KmsBackend::for_tests_with_vk() {
        Ok(backend) => backend,
        Err(error) => {
            eprintln!("skipping: no Vulkan test device: {error}");
            return None;
        }
    };
    let layouts = backend.platform.scene_output_layouts();
    backend.scene = crate::kms::render::scene::SceneCompositor::new(&backend.platform, &layouts)
        .expect("construct hardware-free live scene");
    let mut state = ServerState::new();
    install_client_for_render(&mut state, 14);
    state
        .resources
        .window_mut(ROOT_WINDOW)
        .expect("root")
        .host_xid = yserver_core::backend::WindowHandle::from_raw(backend.core.window_id);
    Some((state, backend))
}

/// Forget every wake so far, as a tick that composed them would.
fn settle(b: &mut KmsBackend) {
    b.scene.scene_structure_dirty = false;
    for drawable in b.store.armed_damaged_ids() {
        if let Some(snapshot) = b.store.peek_presentation_damage(drawable) {
            b.store.ack_presentation_damage(snapshot);
        }
    }
    assert!(!b.scene_wants_scene(), "settled scene must be quiet");
}

fn u32s(values: &[u32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

/// A mapped 32x32 InputOutput child of the root at (10,10), background pixel
/// `bg`, with the pointer inside it.
fn mapped_window(state: &mut ServerState, b: &mut KmsBackend, bg: u32) {
    let mut body = u32s(&[WINDOW.0, ROOT_WINDOW.0]);
    for v in [10i16, 10] {
        body.extend(v.to_le_bytes());
    }
    for v in [32u16, 32, 0, 1] {
        body.extend(v.to_le_bytes()); // width, height, border, InputOutput
    }
    body.extend(u32s(&[0, 0x2, bg])); // CopyFromParent visual, CWBackPixel
    dispatch_raw(state, b, 1, 0, &body); // CreateWindow
    b.core.cursor_x = 20.0;
    b.core.cursor_y = 20.0;
    dispatch_raw(state, b, 8, 0, &WINDOW.0.to_le_bytes()); // MapWindow
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn requests_that_change_nothing_on_screen_leave_the_scene_quiet() {
    let Some((mut state, mut b)) = live_fixture() else {
        return;
    };
    mapped_window(&mut state, &mut b, 0x0000_ff00);
    settle(&mut b);

    dispatch_raw(&mut state, &mut b, 127, 0, &[]); // NoOperation
    dispatch_raw(&mut state, &mut b, 43, 0, &[]); // GetInputFocus
    dispatch_raw(
        &mut state,
        &mut b,
        16,
        0,
        &[4, 0, 0, 0, b'W', b'A', b'K', b'E'],
    ); // InternAtom
    dispatch_raw(&mut state, &mut b, 3, 0, &WINDOW.0.to_le_bytes()); // GetWindowAttributes
    dispatch_raw(&mut state, &mut b, 14, 0, &WINDOW.0.to_le_bytes()); // GetGeometry
    dispatch_raw(
        &mut state,
        &mut b,
        55,
        0,
        &u32s(&[GC.0, WINDOW.0, 0x4, 0x00ff_0000]),
    ); // CreateGC
    assert!(
        !b.scene_wants_scene(),
        "requests that change no pixel must not wake the scene"
    );
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn map_wakes_the_scene() {
    let Some((mut state, mut b)) = live_fixture() else {
        return;
    };
    let mut body = u32s(&[WINDOW.0, ROOT_WINDOW.0]);
    for v in [10i16, 10] {
        body.extend(v.to_le_bytes());
    }
    for v in [32u16, 32, 0, 1] {
        body.extend(v.to_le_bytes());
    }
    body.extend(u32s(&[0, 0x2, 0x0000_ff00]));
    dispatch_raw(&mut state, &mut b, 1, 0, &body); // CreateWindow
    settle(&mut b);

    dispatch_raw(&mut state, &mut b, 8, 0, &WINDOW.0.to_le_bytes()); // MapWindow
    assert!(
        b.scene.scene_structure_dirty,
        "MapWindow must wake the scene"
    );
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn drawing_into_a_mapped_window_wakes_the_scene() {
    let Some((mut state, mut b)) = live_fixture() else {
        return;
    };
    mapped_window(&mut state, &mut b, 0x0000_ff00);
    dispatch_raw(
        &mut state,
        &mut b,
        55,
        0,
        &u32s(&[GC.0, WINDOW.0, 0x4, 0x00ff_0000]),
    ); // CreateGC
    settle(&mut b);

    let mut body = u32s(&[WINDOW.0, GC.0]);
    for v in [2i16, 2] {
        body.extend(v.to_le_bytes());
    }
    for v in [8u16, 8] {
        body.extend(v.to_le_bytes());
    }
    dispatch_raw(&mut state, &mut b, 70, 0, &body); // PolyFillRectangle
    assert!(
        b.store.has_pending_presentation_damage(),
        "a fill into a mapped window must arm its presentation damage"
    );
    assert!(b.scene_wants_scene());
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn a_background_change_made_visible_by_clear_area_wakes_the_scene() {
    let Some((mut state, mut b)) = live_fixture() else {
        return;
    };
    mapped_window(&mut state, &mut b, 0x0000_ff00);
    let mut body = u32s(&[WINDOW.0, 0x2, 0x00ff_00ff]);
    dispatch_raw(&mut state, &mut b, 2, 0, &body); // ChangeWindowAttributes(CWBackPixel)
    settle(&mut b);

    body = WINDOW.0.to_le_bytes().to_vec();
    body.extend([0u8; 8]); // x, y, width, height = 0: the whole window
    dispatch_raw(&mut state, &mut b, 61, 0, &body); // ClearArea, exposures=False
    assert!(
        b.store.has_pending_presentation_damage(),
        "ClearArea must paint the new background"
    );
    assert!(b.scene_wants_scene());
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn defining_a_cursor_under_the_pointer_wakes_the_scene() {
    let Some((mut state, mut b)) = live_fixture() else {
        return;
    };
    mapped_window(&mut state, &mut b, 0x0000_ff00);
    let source = Backend::create_pixmap(&mut b, None, 1, 1, 1).expect("cursor source pixmap");
    Backend::put_image(&mut b, None, source.as_raw(), 1, 1, 1, 0, 0, &[1, 0, 0, 0])
        .expect("paint visible cursor pixel");
    let host = Backend::create_cursor(
        &mut b,
        None,
        source,
        None,
        (u16::MAX, u16::MAX, u16::MAX),
        (0, 0, 0),
        0,
        0,
    )
    .expect("create visible cursor");
    state
        .resources
        .create_glyph_cursor(yserver_protocol::x11::ClientId(14), CURSOR);
    state.resources.set_cursor_host_xid(CURSOR, host);
    settle(&mut b);

    dispatch_raw(
        &mut state,
        &mut b,
        2,
        0,
        &u32s(&[WINDOW.0, 0x4000, CURSOR.0]),
    ); // CWCursor
    assert_eq!(
        b.effective_cursor_xid,
        Some(host.as_raw()),
        "the pointer is inside the window, so its cursor is shown"
    );
    assert!(
        b.scene.scene_structure_dirty,
        "a displayed cursor change must wake the scene"
    );
}
