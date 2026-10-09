use super::*;

fn render_damage_fixture(
    render_return_region: Vec<yserver_protocol::x11::xfixes::RegionRect>,
) -> (ServerState, RecordingBackend) {
    use crate::{
        backend::PictureHandle,
        resources::{PictureKind, PictureState},
        server::DamageObject,
    };
    use yserver_protocol::x11::CreateWindowRequest;

    const COMPOSITOR: u32 = 7;
    const WIN_XID: u32 = 0x0020_0001;
    const SRC_PIC_XID: u32 = 0x0020_0010;
    const DST_PIC_XID: u32 = 0x0020_0011;
    const DAMAGE_XID: u32 = 0x0020_0020;
    const HOST_SRC: u32 = 0xAA01;
    const HOST_DST: u32 = 0xAA02;

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    backend.render_return_region = render_return_region;
    let _peer = install_client(&mut state, COMPOSITOR);

    state.resources.create_window(
        ClientId(COMPOSITOR),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(WIN_XID),
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 800,
            height: 600,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );

    state.resources.create_picture(
        ResourceId(SRC_PIC_XID),
        PictureState {
            client: ClientId(COMPOSITOR),
            host_picture_xid: Some(PictureHandle::from_raw_for_test(HOST_SRC)),
            host_owned_pixmap: None,
            kind: PictureKind::Drawable,
            drawable: Some(ResourceId(WIN_XID)),
            window: None,
        },
    );
    state.resources.create_picture(
        ResourceId(DST_PIC_XID),
        PictureState {
            client: ClientId(COMPOSITOR),
            host_picture_xid: Some(PictureHandle::from_raw_for_test(HOST_DST)),
            host_owned_pixmap: None,
            kind: PictureKind::Drawable,
            drawable: Some(ResourceId(WIN_XID)),
            window: None,
        },
    );
    // The dst window must be MAPPED for these tests to describe a real
    // situation. Damage is gated on viewability (issue #97): Xorg's
    // `checkPictureDamage` requires `RegionNotEmpty(pCompositeClip)`
    // (miext/damage/damage.c:474) and an unmapped window's clip is empty,
    // so a RENDER op into it damages nothing there either. Leaving the
    // window unmapped made these tests assert damage that Xorg would not
    // produce; mapping it keeps each test's real subject — "the render op
    // damages exactly the backend-returned region" — intact.
    let _ = state.resources.map_window(ResourceId(WIN_XID));

    state.damage_objects.insert(
        DAMAGE_XID,
        DamageObject {
            owner: ClientId(COMPOSITOR),
            drawable: ResourceId(WIN_XID),
            level: 3,
            rects: Vec::new(),
            pending_notify_fired: false,
            last_reported_geometry: None,
        },
    );

    (state, backend)
}

fn assert_damage_rects_exact(
    state: &ServerState,
    expected: &[(i16, i16, u16, u16)],
    message: &str,
) {
    const DAMAGE_XID: u32 = 0x0020_0020;
    let damage = state.damage_objects.get(&DAMAGE_XID).unwrap();
    let got: Vec<(i16, i16, u16, u16)> = damage
        .rects
        .iter()
        .map(|r| (r.x, r.y, r.width, r.height))
        .collect();
    assert_eq!(got, expected, "{message}");
}

/// RENDER Composite must damage exactly the backend-returned region on
/// the dst picture's underlying drawable.
#[test]
fn render_composite_emits_damage_on_dst_drawable() {
    const COMPOSITOR: u32 = 7;
    const SRC_PIC_XID: u32 = 0x0020_0010;
    const DST_PIC_XID: u32 = 0x0020_0011;
    let (mut state, mut backend) =
        render_damage_fixture(vec![yserver_protocol::x11::xfixes::RegionRect {
            x: 0,
            y: 0,
            width: 100,
            height: 80,
        }]);

    // RENDER Composite body (minor=8, total 32 bytes):
    // op(1) + pad(3) + src(4) + mask(4) + dst(4) + src_xy(4) +
    // mask_xy(4) + dst_xy(4) + size(4)
    let mut body = vec![0u8; 32];
    body[0] = 3; // PictOpOver
    body[4..8].copy_from_slice(&SRC_PIC_XID.to_le_bytes());
    body[8..12].copy_from_slice(&0u32.to_le_bytes()); // no mask
    body[12..16].copy_from_slice(&DST_PIC_XID.to_le_bytes());
    body[28..30].copy_from_slice(&100u16.to_le_bytes()); // width
    body[30..32].copy_from_slice(&80u16.to_le_bytes()); // height

    process_request(
        &mut state,
        &mut backend,
        ClientId(COMPOSITOR),
        SequenceNumber(1),
        RequestHeader {
            opcode: 133, // RENDER major opcode
            data: 8,     // minor: Composite
            length_units: u32::try_from(1 + 32 / 4).unwrap(),
        },
        &body,
        None,
    )
    .unwrap();

    assert_damage_rects_exact(
        &state,
        &[(0, 0, 100, 80)],
        "render_composite must damage exactly the backend-returned region",
    );
}

#[test]
fn render_trapezoids_damages_returned_region() {
    const COMPOSITOR: u32 = 7;
    const DST_PIC_XID: u32 = 0x0020_0011;
    const SRC_PIC_XID: u32 = 0x0020_0010;
    let (mut state, mut backend) =
        render_damage_fixture(vec![yserver_protocol::x11::xfixes::RegionRect {
            x: 3,
            y: 4,
            width: 20,
            height: 30,
        }]);

    let mut body = vec![0u8; 60];
    body[0] = 3;
    body[4..8].copy_from_slice(&SRC_PIC_XID.to_le_bytes());
    body[8..12].copy_from_slice(&DST_PIC_XID.to_le_bytes());
    process_request(
        &mut state,
        &mut backend,
        ClientId(COMPOSITOR),
        SequenceNumber(1),
        RequestHeader {
            opcode: 133,
            data: 10,
            length_units: u32::try_from(1 + body.len() / 4).unwrap(),
        },
        &body,
        None,
    )
    .unwrap();
    assert_damage_rects_exact(
        &state,
        &[(3, 4, 20, 30)],
        "render_trapezoids must damage exactly the backend-returned region",
    );

    let (mut state, mut backend) = render_damage_fixture(Vec::new());
    process_request(
        &mut state,
        &mut backend,
        ClientId(COMPOSITOR),
        SequenceNumber(1),
        RequestHeader {
            opcode: 133,
            data: 10,
            length_units: u32::try_from(1 + body.len() / 4).unwrap(),
        },
        &body,
        None,
    )
    .unwrap();
    assert_damage_rects_exact(&state, &[], "empty trapezoid region must not damage");
}

#[test]
fn render_triangles_damages_returned_region() {
    const COMPOSITOR: u32 = 7;
    const DST_PIC_XID: u32 = 0x0020_0011;
    const SRC_PIC_XID: u32 = 0x0020_0010;
    let (mut state, mut backend) =
        render_damage_fixture(vec![yserver_protocol::x11::xfixes::RegionRect {
            x: 7,
            y: 8,
            width: 11,
            height: 12,
        }]);

    let mut body = vec![0u8; 44];
    body[0] = 3;
    body[4..8].copy_from_slice(&SRC_PIC_XID.to_le_bytes());
    body[8..12].copy_from_slice(&DST_PIC_XID.to_le_bytes());
    process_request(
        &mut state,
        &mut backend,
        ClientId(COMPOSITOR),
        SequenceNumber(1),
        RequestHeader {
            opcode: 133,
            data: 11,
            length_units: u32::try_from(1 + body.len() / 4).unwrap(),
        },
        &body,
        None,
    )
    .unwrap();
    assert_damage_rects_exact(
        &state,
        &[(7, 8, 11, 12)],
        "render_triangles must damage exactly the backend-returned region",
    );

    let (mut state, mut backend) = render_damage_fixture(Vec::new());
    process_request(
        &mut state,
        &mut backend,
        ClientId(COMPOSITOR),
        SequenceNumber(1),
        RequestHeader {
            opcode: 133,
            data: 11,
            length_units: u32::try_from(1 + body.len() / 4).unwrap(),
        },
        &body,
        None,
    )
    .unwrap();
    assert_damage_rects_exact(&state, &[], "empty triangle region must not damage");
}

#[test]
fn render_composite_glyphs_damages_returned_region() {
    const COMPOSITOR: u32 = 7;
    const DST_PIC_XID: u32 = 0x0020_0011;
    const SRC_PIC_XID: u32 = 0x0020_0010;
    const GLYPHSET_XID: u32 = 0x0020_0030;
    const HOST_GS: u32 = 0xAA03;
    let (mut state, mut backend) =
        render_damage_fixture(vec![yserver_protocol::x11::xfixes::RegionRect {
            x: 9,
            y: 10,
            width: 13,
            height: 14,
        }]);
    state.resources.create_glyphset(
        ResourceId(GLYPHSET_XID),
        crate::resources::GlyphSetState {
            client: ClientId(COMPOSITOR),
            host_glyphset_xid: crate::backend::GlyphSetHandle::from_raw_for_test(HOST_GS),
        },
    );

    let mut body = vec![0u8; 28];
    body[0] = 3;
    body[4..8].copy_from_slice(&SRC_PIC_XID.to_le_bytes());
    body[8..12].copy_from_slice(&DST_PIC_XID.to_le_bytes());
    body[16..20].copy_from_slice(&GLYPHSET_XID.to_le_bytes());
    body[20..28].copy_from_slice(&[1, 0, 0, 0, 0, 0, 0, 0]);
    process_request(
        &mut state,
        &mut backend,
        ClientId(COMPOSITOR),
        SequenceNumber(1),
        RequestHeader {
            opcode: 133,
            data: 23,
            length_units: u32::try_from(1 + body.len() / 4).unwrap(),
        },
        &body,
        None,
    )
    .unwrap();
    assert_damage_rects_exact(
        &state,
        &[(9, 10, 13, 14)],
        "render_composite_glyphs must damage exactly the backend-returned region",
    );

    let (mut state, mut backend) = render_damage_fixture(Vec::new());
    state.resources.create_glyphset(
        ResourceId(GLYPHSET_XID),
        crate::resources::GlyphSetState {
            client: ClientId(COMPOSITOR),
            host_glyphset_xid: crate::backend::GlyphSetHandle::from_raw_for_test(HOST_GS),
        },
    );
    process_request(
        &mut state,
        &mut backend,
        ClientId(COMPOSITOR),
        SequenceNumber(1),
        RequestHeader {
            opcode: 133,
            data: 23,
            length_units: u32::try_from(1 + body.len() / 4).unwrap(),
        },
        &body,
        None,
    )
    .unwrap();
    assert_damage_rects_exact(&state, &[], "empty glyph region must not damage");
}

#[test]
fn render_picture_damage_drawable_prefers_named_window_pixmap_owner() {
    use crate::{backend::PixmapHandle, resources::NamedCompositePixmap};
    use yserver_protocol::x11::CreateWindowRequest;

    const CLIENT: u32 = 7;
    const WINDOW_XID: u32 = 0x0030_0001;
    const ALIAS_PIXMAP_XID: u32 = 0x0030_0002;
    const ALIAS_PIXMAP_HOST: u32 = 0x0050_0102;
    const ORDINARY_PIXMAP_XID: u32 = 0x0030_0003;

    let mut state = ServerState::new();
    state.resources.create_window(
        ClientId(CLIENT),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(WINDOW_XID),
            parent: ROOT_WINDOW,
            x: 100,
            y: 200,
            width: 800,
            height: 600,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    {
        let window = state
            .resources
            .window_mut(ResourceId(WINDOW_XID))
            .expect("window");
        window.composite_named_pixmaps.push(NamedCompositePixmap {
            client_pixmap: ResourceId(ALIAS_PIXMAP_XID),
            host_pixmap: PixmapHandle::from_raw_for_test(ALIAS_PIXMAP_HOST),
            width: 800,
            height: 600,
        });
    }
    assert!(
        render_picture_damage_drawable(&state, ResourceId(ALIAS_PIXMAP_XID))
            == ResourceId(WINDOW_XID),
        "RENDER pictures created on a NameWindowPixmap alias must route later damage to the \
             owning window XID so XDamageCreate(window) subscriptions wake",
    );
    assert!(
        render_picture_damage_drawable(&state, ResourceId(ORDINARY_PIXMAP_XID))
            == ResourceId(ORDINARY_PIXMAP_XID),
        "ordinary pixmaps must keep their own drawable identity; only Composite aliases \
             remap to the owner window",
    );
}

// ── RENDER::CreateAnimCursor (opcode 133 / minor 31) helpers ─────────────

fn anim_cursor_body(cid: u32, elts: &[(u32, u32)]) -> Vec<u8> {
    let mut b = cid.to_le_bytes().to_vec();
    for (cur, delay) in elts {
        b.extend_from_slice(&cur.to_le_bytes());
        b.extend_from_slice(&delay.to_le_bytes());
    }
    b
}

fn seed_cursor(state: &mut ServerState, raw: u32, host: u32) {
    state.resources.create_cursor(ClientId(1), ResourceId(raw));
    state.resources.set_cursor_host_xid(
        ResourceId(raw),
        crate::backend::CursorHandle::from_raw(host).unwrap(),
    );
}

fn send_anim_cursor(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    seq: u16,
    body: &[u8],
) {
    process_request(
        state,
        backend,
        ClientId(1),
        SequenceNumber(seq),
        RequestHeader {
            opcode: 133,
            data: 31,
            length_units: u32::try_from(1 + body.len().div_ceil(4)).unwrap(),
        },
        body,
        None,
    )
    .expect("process_request");
}

#[test]
fn create_anim_cursor_empty_list_returns_bad_value() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let body = anim_cursor_body(0x4000, &[]);
    send_anim_cursor(&mut state, &mut backend, 1, &body);
    let bytes = read_all_available(&mut peer);
    assert!(bytes.len() >= 32, "expected error reply, got {bytes:02x?}");
    assert_eq!(bytes[1], x11::error::BAD_VALUE);
    assert_eq!(&bytes[8..10], &31u16.to_le_bytes());
    assert_eq!(bytes[10], 133);
    assert!(!state.resources.cursor_exists(ResourceId(0x4000)));
}

#[test]
fn create_anim_cursor_odd_pairs_returns_bad_length() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    seed_cursor(&mut state, 0x3000, 0x77);
    // One full pair + 4 trailing bytes = not a multiple of 8.
    let mut body = anim_cursor_body(0x4000, &[(0x3000, 50)]);
    body.extend_from_slice(&0x3000u32.to_le_bytes());
    send_anim_cursor(&mut state, &mut backend, 1, &body);
    let bytes = read_all_available(&mut peer);
    assert!(bytes.len() >= 32);
    assert_eq!(bytes[1], x11::error::BAD_LENGTH);
    assert_eq!(&bytes[8..10], &31u16.to_le_bytes());
    assert_eq!(bytes[10], 133);
    assert!(!state.resources.cursor_exists(ResourceId(0x4000)));
}

#[test]
fn create_anim_cursor_nested_anim_returns_bad_match() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    seed_cursor(&mut state, 0x3000, 0x77);
    // First anim cursor (fallback path on RecordingBackend).
    let body = anim_cursor_body(0x4000, &[(0x3000, 50)]);
    send_anim_cursor(&mut state, &mut backend, 1, &body);
    let _ = read_all_available(&mut peer); // no error expected
    assert!(state.resources.cursor_is_anim(ResourceId(0x4000)));
    // Second anim cursor referencing the first → BadMatch.
    let body2 = anim_cursor_body(0x4001, &[(0x4000, 50)]);
    send_anim_cursor(&mut state, &mut backend, 2, &body2);
    let bytes = read_all_available(&mut peer);
    assert!(bytes.len() >= 32);
    assert_eq!(bytes[1], x11::error::BAD_MATCH);
    // Xorg's AnimCursorCreate returns BadMatch without setting
    // client->errorValue → value field must be zero.
    assert_eq!(&bytes[4..8], &0u32.to_le_bytes());
    assert_eq!(&bytes[8..10], &31u16.to_le_bytes());
    assert_eq!(bytes[10], 133);
    assert!(!state.resources.cursor_exists(ResourceId(0x4001)));
}

#[test]
fn create_anim_cursor_fallback_aliases_first_frame_and_sets_anim() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    seed_cursor(&mut state, 0x3000, 0x77);
    seed_cursor(&mut state, 0x3001, 0x78);
    let body = anim_cursor_body(0x4000, &[(0x3000, 50), (0x3001, 75)]);
    send_anim_cursor(&mut state, &mut backend, 1, &body);
    let bytes = read_all_available(&mut peer);
    assert!(bytes.is_empty(), "no error expected, got {bytes:02x?}");
    assert_eq!(
        state.resources.cursor_host_xid(ResourceId(0x4000)),
        Some(0x77)
    );
    assert!(state.resources.cursor_is_anim(ResourceId(0x4000)));
}

// ── Step 3 (window-storage lifecycle): Pictures on windows ──────────

const PIC_A: u32 = 0x0030_0000;
const PIC_B: u32 = 0x0040_0000;
const PIC_WIN: u32 = PIC_A | 1;
const PIC_WIN_HOST: u32 = 0x00E0_0001;
const PIC_ON_WIN: u32 = PIC_B | 1;
const PIC_PIXMAP: u32 = PIC_B | 2;
const PIC_ON_PIXMAP: u32 = PIC_B | 3;

fn picture_request(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    client: u32,
    opcode: u8,
    data: u8,
    body: &[u8],
) {
    process_request(
        state,
        backend,
        ClientId(client),
        SequenceNumber(1),
        RequestHeader {
            opcode,
            data,
            length_units: u32::try_from(1 + body.len().div_ceil(4)).unwrap(),
        },
        body,
        None,
    )
    .unwrap();
}

/// `(error code, bad value)` of every error queued on `peer`.
fn drain_errors(peer: &mut UnixStream) -> Vec<(u8, u32)> {
    peer.set_nonblocking(true).unwrap();
    let mut out = Vec::new();
    let mut buf = [0u8; 32];
    while peer.read_exact(&mut buf).is_ok() {
        if buf[0] == 0 {
            out.push((buf[1], u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]])));
        }
    }
    out
}

fn create_render_picture(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    client: u32,
    pic: u32,
    drawable: u32,
) {
    let mut body = Vec::new();
    for v in [pic, drawable, 0, 0] {
        body.extend_from_slice(&v.to_le_bytes());
    }
    picture_request(state, backend, client, 133, 4, &body);
}

fn render_fill(state: &mut ServerState, backend: &mut RecordingBackend, client: u32, dst: u32) {
    let mut body = vec![1u8, 0, 0, 0];
    body.extend_from_slice(&dst.to_le_bytes());
    body.extend_from_slice(&[0xff; 8]);
    for v in [0i16, 0] {
        body.extend_from_slice(&v.to_le_bytes());
    }
    for v in [8u16, 8] {
        body.extend_from_slice(&v.to_le_bytes());
    }
    picture_request(state, backend, client, 133, 26, &body);
}

fn render_composite_pics(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    client: u32,
    src: u32,
    dst: u32,
) {
    let mut body = vec![0u8; 32];
    body[0] = 3;
    body[4..8].copy_from_slice(&src.to_le_bytes());
    body[12..16].copy_from_slice(&dst.to_le_bytes());
    body[28..30].copy_from_slice(&8u16.to_le_bytes());
    body[30..32].copy_from_slice(&8u16.to_le_bytes());
    picture_request(state, backend, client, 133, 8, &body);
}

fn render_free(state: &mut ServerState, backend: &mut RecordingBackend, client: u32, pic: u32) {
    picture_request(state, backend, client, 133, 7, &pic.to_le_bytes());
}

fn host_pic_of(state: &ServerState, pic: u32) -> u32 {
    state
        .resources
        .picture(ResourceId(pic))
        .expect("picture exists")
        .host_picture_xid
        .expect("backed picture")
        .as_raw()
}

fn render_paints(backend: &RecordingBackend) -> Vec<RecordedCall> {
    backend
        .calls()
        .into_iter()
        .filter(|c| {
            matches!(
                c,
                RecordedCall::RenderComposite { .. } | RecordedCall::RenderFillRectangles { .. }
            )
        })
        .collect()
}

/// Client A owns a mapped top-level `PIC_WIN`; client B holds a Picture on it and
/// one on its own pixmap.
fn cross_client_picture_fixture() -> (ServerState, RecordingBackend, UnixStream, UnixStream) {
    let mut state = ServerState::new();
    let peer_a = install_client(&mut state, 1);
    let peer_b = install_client(&mut state, 2);
    let mut backend = RecordingBackend::new();
    create_pic_test_window(&mut state, PIC_WIN, PIC_WIN_HOST);
    state.resources.create_pixmap(
        ClientId(2),
        CreatePixmapRequest {
            pixmap: ResourceId(PIC_PIXMAP),
            drawable: ROOT_WINDOW,
            width: 16,
            height: 16,
            depth: 24,
        },
    );
    assert!(state.resources.set_pixmap_host_xid(
        ResourceId(PIC_PIXMAP),
        crate::backend::PixmapHandle::from_raw_for_test(0x00E0_0100),
    ));
    create_render_picture(&mut state, &mut backend, 2, PIC_ON_WIN, PIC_WIN);
    create_render_picture(&mut state, &mut backend, 2, PIC_ON_PIXMAP, PIC_PIXMAP);
    (state, backend, peer_a, peer_b)
}

fn create_pic_test_window(state: &mut ServerState, xid: u32, host: u32) {
    state.resources.create_window(
        ClientId(1),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(xid),
            parent: ROOT_WINDOW,
            width: 64,
            height: 64,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    state
        .resources
        .window_mut(ResourceId(xid))
        .unwrap()
        .host_xid = Some(crate::backend::WindowHandle::from_raw_for_test(host));
    let _ = state.resources.map_window(ResourceId(xid));
}

/// B's Picture on A's window is dead once the window is gone: every use and
/// its FreePicture are BadPicture, its pixmap Picture is untouched, and a new
/// window on the same xid and host storage does not revive it.
fn assert_window_picture_dead_after(destroy: impl FnOnce(&mut ServerState, &mut RecordingBackend)) {
    let bad_picture = crate::nested::RENDER_FIRST_ERROR + 1;
    let (mut state, mut backend, _peer_a, mut peer_b) = cross_client_picture_fixture();
    let host_on_win = host_pic_of(&state, PIC_ON_WIN);
    let host_on_pixmap = host_pic_of(&state, PIC_ON_PIXMAP);

    destroy(&mut state, &mut backend);

    assert!(state.resources.picture(ResourceId(PIC_ON_WIN)).is_none());
    let freed: Vec<u32> = backend
        .calls()
        .into_iter()
        .filter_map(|c| match c {
            RecordedCall::RenderFreePicture { host_pic } => Some(host_pic),
            _ => None,
        })
        .collect();
    assert_eq!(
        freed,
        vec![host_on_win],
        "only the window Picture's backend record is freed"
    );
    assert_eq!(host_pic_of(&state, PIC_ON_PIXMAP), host_on_pixmap);
    backend.calls.lock().unwrap().clear();

    create_pic_test_window(&mut state, PIC_WIN, PIC_WIN_HOST);
    render_fill(&mut state, &mut backend, 2, PIC_ON_WIN);
    render_composite_pics(&mut state, &mut backend, 2, PIC_ON_WIN, PIC_ON_PIXMAP);
    render_free(&mut state, &mut backend, 2, PIC_ON_WIN);
    assert_eq!(
        drain_errors(&mut peer_b),
        vec![(bad_picture, PIC_ON_WIN); 3],
        "FillRectangles, Composite and FreePicture on the dead Picture",
    );
    assert!(render_paints(&backend).is_empty());
    assert!(
        !backend
            .calls()
            .iter()
            .any(|c| matches!(c, RecordedCall::RenderFreePicture { .. }))
    );

    render_fill(&mut state, &mut backend, 2, PIC_ON_PIXMAP);
    assert_eq!(
        render_paints(&backend),
        vec![RecordedCall::RenderFillRectangles {
            host_dst: host_on_pixmap
        }],
    );
    assert!(drain_errors(&mut peer_b).is_empty());
}

#[test]
fn destroy_window_frees_other_clients_pictures_on_it() {
    assert_window_picture_dead_after(|state, backend| {
        picture_request(state, backend, 1, 4, 0, &PIC_WIN.to_le_bytes());
    });
}

#[test]
fn owner_disconnect_frees_other_clients_pictures_on_its_windows() {
    assert_window_picture_dead_after(|state, backend| {
        crate::core_loop::process_disconnect::process_disconnect(state, backend, ClientId(1));
        let _peer = install_client(state, 1);
    });
}

#[test]
fn destroying_a_parent_frees_pictures_on_its_descendants() {
    let (mut state, mut backend, _peer_a, mut peer_b) = cross_client_picture_fixture();
    const CHILD: u32 = PIC_A | 2;
    const PIC_ON_CHILD: u32 = PIC_B | 4;
    state.resources.create_window(
        ClientId(1),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(CHILD),
            parent: ResourceId(PIC_WIN),
            width: 8,
            height: 8,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    state
        .resources
        .window_mut(ResourceId(CHILD))
        .unwrap()
        .host_xid = Some(crate::backend::WindowHandle::from_raw_for_test(0x00E0_0002));
    create_render_picture(&mut state, &mut backend, 2, PIC_ON_CHILD, CHILD);
    assert!(state.resources.picture(ResourceId(PIC_ON_CHILD)).is_some());

    picture_request(&mut state, &mut backend, 1, 4, 0, &PIC_WIN.to_le_bytes());

    assert!(state.resources.picture(ResourceId(PIC_ON_CHILD)).is_none());
    assert!(state.resources.picture(ResourceId(PIC_ON_WIN)).is_none());
    assert!(state.resources.picture(ResourceId(PIC_ON_PIXMAP)).is_some());
    assert!(drain_errors(&mut peer_b).is_empty());
}

/// Xorg: an unviewable window's clipList is empty, so a Picture on it as the
/// destination draws nothing; the same Picture draws again after remap.
#[test]
fn window_picture_draws_nothing_while_unmapped_and_again_after_remap() {
    let (mut state, mut backend, _peer_a, mut peer_b) = cross_client_picture_fixture();
    let host_on_win = host_pic_of(&state, PIC_ON_WIN);
    let host_on_pixmap = host_pic_of(&state, PIC_ON_PIXMAP);
    let fill = RecordedCall::RenderFillRectangles {
        host_dst: host_on_win,
    };

    render_fill(&mut state, &mut backend, 2, PIC_ON_WIN);
    assert_eq!(render_paints(&backend), vec![fill.clone()]);
    backend.calls.lock().unwrap().clear();

    picture_request(&mut state, &mut backend, 1, 10, 0, &PIC_WIN.to_le_bytes());
    render_fill(&mut state, &mut backend, 2, PIC_ON_WIN);
    render_composite_pics(&mut state, &mut backend, 2, PIC_ON_PIXMAP, PIC_ON_WIN);
    assert!(
        render_paints(&backend).is_empty(),
        "unmapped: nothing drawn"
    );
    // As a source the hidden window's contents are undefined, not an error.
    render_composite_pics(&mut state, &mut backend, 2, PIC_ON_WIN, PIC_ON_PIXMAP);
    assert_eq!(
        render_paints(&backend),
        vec![RecordedCall::RenderComposite {
            host_src: host_on_win,
            host_dst: host_on_pixmap,
        }],
    );
    backend.calls.lock().unwrap().clear();

    picture_request(&mut state, &mut backend, 1, 8, 0, &PIC_WIN.to_le_bytes());
    render_fill(&mut state, &mut backend, 2, PIC_ON_WIN);
    assert_eq!(render_paints(&backend), vec![fill]);
    assert!(drain_errors(&mut peer_b).is_empty());
}

/// A Picture on a redirected window names the window, not the backing it
/// has at creation time, so it follows backing rotations.
#[test]
fn create_picture_on_redirected_window_names_the_window() {
    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    create_pic_test_window(&mut state, PIC_WIN, PIC_WIN_HOST);
    state
        .resources
        .window_mut(ResourceId(PIC_WIN))
        .unwrap()
        .redirected_backing = Some(crate::resources::RedirectedBacking {
        host_pixmap: crate::backend::PixmapHandle::from_raw_for_test(0x00E0_0200),
        width: 64,
        height: 64,
        depth: 24,
    });
    create_render_picture(&mut state, &mut backend, 1, PIC_A | 5, PIC_WIN);
    let host_drawables: Vec<crate::backend::AnyHandle> = backend
        .calls()
        .into_iter()
        .filter_map(|c| match c {
            RecordedCall::RenderCreatePicture { host_drawable, .. } => Some(host_drawable),
            _ => None,
        })
        .collect();
    assert_eq!(
        host_drawables,
        vec![crate::backend::AnyHandle::Window(
            crate::backend::WindowHandle::from_raw_for_test(PIC_WIN_HOST)
        )],
    );
    assert_eq!(
        state
            .resources
            .picture(ResourceId(PIC_A | 5))
            .unwrap()
            .window,
        Some(ResourceId(PIC_WIN)),
    );
}

/// Xorg `compDestroyOverlayWindow` frees the overlay through DeleteWindow, so
/// Pictures on it are freed with it (`composite/compoverlay.c:167-172`).
#[test]
fn overlay_release_frees_pictures_on_the_overlay() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let cow = crate::resources::COMPOSITE_OVERLAY_WINDOW;
    let mut body = vec![0u8; 4];
    body[0..4].copy_from_slice(&ROOT_WINDOW.0.to_le_bytes());
    dispatch_composite_minor(
        &mut state,
        &mut backend,
        ClientId(1),
        1,
        yserver_protocol::x11::composite::GET_OVERLAY_WINDOW,
        &body,
    );
    drain_errors(&mut peer);
    create_render_picture(&mut state, &mut backend, 1, PIC_A | 6, cow.0);
    assert!(state.resources.picture(ResourceId(PIC_A | 6)).is_some());
    body[0..4].copy_from_slice(&cow.0.to_le_bytes());
    dispatch_composite_minor(
        &mut state,
        &mut backend,
        ClientId(1),
        2,
        yserver_protocol::x11::composite::RELEASE_OVERLAY_WINDOW,
        &body,
    );
    assert!(state.resources.window(cow).is_none());
    assert!(state.resources.picture(ResourceId(PIC_A | 6)).is_none());
}

/// CreateConicalGradient registers a Picture: usable as a source without an
/// error (drawing with it stays unimplemented) and freeable.
#[test]
fn conical_gradient_picture_is_usable_as_source_and_freeable() {
    let (mut state, mut backend, _peer_a, mut peer_b) = cross_client_picture_fixture();
    const CONICAL: u32 = PIC_B | 9;
    let mut body = Vec::new();
    body.extend_from_slice(&CONICAL.to_le_bytes());
    body.extend_from_slice(&[0u8; 16]); // center, angle, nstops = 0
    picture_request(&mut state, &mut backend, 2, 133, 36, &body);
    assert!(state.resources.picture(ResourceId(CONICAL)).is_some());
    backend.calls.lock().unwrap().clear();
    render_composite_pics(&mut state, &mut backend, 2, CONICAL, PIC_ON_PIXMAP);
    assert!(
        render_paints(&backend).is_empty(),
        "unbacked source draws nothing"
    );
    render_free(&mut state, &mut backend, 2, CONICAL);
    assert!(state.resources.picture(ResourceId(CONICAL)).is_none());
    assert!(drain_errors(&mut peer_b).is_empty());
}

/// A CreatePicture the backend could not back still succeeds at protocol level:
/// ops on the Picture are silent no-ops and FreePicture is valid. An id that was
/// never created is still BadPicture.
#[test]
fn unbacked_picture_ops_are_silent_no_ops() {
    let (mut state, mut backend, _peer_a, mut peer_b) = cross_client_picture_fixture();
    const UNBACKED: u32 = PIC_B | 10;
    backend.render_create_picture_fails = true;
    create_render_picture(&mut state, &mut backend, 2, UNBACKED, PIC_PIXMAP);
    backend.render_create_picture_fails = false;
    let st = state
        .resources
        .picture(ResourceId(UNBACKED))
        .expect("registered");
    assert!(st.host_picture_xid.is_none());
    backend.calls.lock().unwrap().clear();

    render_fill(&mut state, &mut backend, 2, UNBACKED);
    render_composite_pics(&mut state, &mut backend, 2, UNBACKED, PIC_ON_PIXMAP);
    render_composite_pics(&mut state, &mut backend, 2, PIC_ON_PIXMAP, UNBACKED);
    render_free(&mut state, &mut backend, 2, UNBACKED);
    assert!(drain_errors(&mut peer_b).is_empty());
    assert!(state.resources.picture(ResourceId(UNBACKED)).is_none());
    assert!(backend.calls().iter().all(|c| !matches!(
        c,
        RecordedCall::RenderComposite { .. }
            | RecordedCall::RenderFillRectangles { .. }
            | RecordedCall::RenderFreePicture { .. }
    )));

    const NEVER: u32 = PIC_B | 11;
    render_fill(&mut state, &mut backend, 2, NEVER);
    assert_eq!(
        drain_errors(&mut peer_b),
        vec![(crate::nested::RENDER_FIRST_ERROR + 1, NEVER)],
    );
}

/// Xorg dixLookupDrawable: CreatePicture on an unknown drawable is BadDrawable
/// and registers nothing.
#[test]
fn create_picture_on_unknown_drawable_is_bad_drawable() {
    let (mut state, mut backend, _peer_a, mut peer_b) = cross_client_picture_fixture();
    const PIC: u32 = PIC_B | 12;
    create_render_picture(&mut state, &mut backend, 2, PIC, 0x00DE_AD00);
    assert_eq!(
        drain_errors(&mut peer_b),
        vec![(x11::error::BAD_DRAWABLE, 0x00DE_AD00)],
    );
    assert!(state.resources.picture(ResourceId(PIC)).is_none());
}
