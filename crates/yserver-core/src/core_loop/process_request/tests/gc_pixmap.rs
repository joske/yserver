use super::*;

#[test]
fn poly_fill_rectangle_unknown_gc_returns_bad_gc() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();

    let body = poly_fill_rectangle_body(ROOT_WINDOW.0, 0xdead_beef);
    process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        RequestHeader {
            opcode: 70,
            data: 0,
            length_units: u32::try_from(1 + body.len().div_ceil(4)).unwrap(),
        },
        &body,
        None,
    )
    .expect("process_request");

    let bytes = read_all_available(&mut peer);
    assert!(
        bytes.len() >= 32,
        "expected 32-byte error reply, got {} bytes: {:02x?}",
        bytes.len(),
        bytes
    );
    let buf = &bytes[..32];
    assert_eq!(buf[1], x11::error::BAD_GC);
    assert_eq!(buf[10], 70);
}

/// Decode a 32-byte X11 error at the wire offsets fixed by the core
/// protocol encoding (`Errors`): 0 = 0, 1 = code, 2..4 = sequence,
/// 4..8 = bad resource id / value, 8..10 = minor opcode, 10 = major
/// opcode. Asserted as raw bytes on purpose — running the reply back
/// through our own decoder would pass even if encoder and decoder
/// were wrong together.
fn assert_x11_error_bytes(
    bytes: &[u8],
    code: u8,
    bad_value: u32,
    minor: u16,
    major: u8,
    sequence: u16,
    what: &str,
) {
    assert_eq!(
        bytes.len(),
        32,
        "{what}: expected exactly one 32-byte error, got {:02x?}",
        bytes
    );
    assert_eq!(bytes[0], 0, "{what}: byte 0 marks a packet as an error");
    assert_eq!(bytes[1], code, "{what}: error code");
    assert_eq!(
        u16::from_le_bytes([bytes[2], bytes[3]]),
        sequence,
        "{what}: sequence number"
    );
    assert_eq!(
        u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
        bad_value,
        "{what}: bad resource id must be the XID exactly as sent"
    );
    assert_eq!(
        u16::from_le_bytes([bytes[8], bytes[9]]),
        minor,
        "{what}: minor opcode"
    );
    assert_eq!(bytes[10], major, "{what}: major opcode");
}

/// Drive one four-byte resource-release request (FreePixmap / FreeGC /
/// FreeCursor: `xResourceReq`, 2 units total) through the real
/// dispatch table and return whatever the client was sent.
fn run_free_resource_request(
    state: &mut ServerState,
    peer: &mut UnixStream,
    opcode: u8,
    xid: u32,
    sequence: u16,
) -> Vec<u8> {
    let mut backend = RecordingBackend::new();
    let body = xid.to_le_bytes().to_vec();
    process_request(
        state,
        &mut backend,
        ClientId(1),
        SequenceNumber(sequence),
        RequestHeader {
            opcode,
            data: 0,
            length_units: 2,
        },
        &body,
        None,
    )
    .expect("process_request");
    read_all_available(peer)
}

/// #143. picom's `x_prepare_for_sleep` (picom `src/x.c:1194`) fires a
/// deliberately invalid `FreePixmap(drawable=None)` as a sync barrier
/// and *expects* `BadPixmap`: XCB only completes a checked **void**
/// request once it reads a packet with a higher sequence number, and
/// for a void request that packet is the error. Xorg answers every one
/// of these (measured: 419 requests -> 419 `BadPixmap`); we answered
/// none of 538, so picom's checked `ChangeWindowAttributes` batch never
/// completed and its window import stalled ~10s instead of ~120ms.
#[test]
fn free_pixmap_none_returns_bad_pixmap() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);

    let bytes = run_free_resource_request(&mut state, &mut peer, 54, 0, 0x0067);

    assert_x11_error_bytes(
        &bytes,
        x11::error::BAD_PIXMAP,
        0,
        0,
        54,
        0x0067,
        "FreePixmap(None)",
    );
}

#[test]
fn free_pixmap_unknown_xid_returns_bad_pixmap() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);

    let bytes = run_free_resource_request(&mut state, &mut peer, 54, 0x00de_ad00, 7);

    assert_x11_error_bytes(
        &bytes,
        x11::error::BAD_PIXMAP,
        0x00de_ad00,
        0,
        54,
        7,
        "FreePixmap(unknown)",
    );
}

/// The error path must not swallow the ordinary one: a pixmap the
/// server knows is freed silently, as Xorg's `Success` return does.
#[test]
fn free_pixmap_known_xid_reports_no_error() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    state.resources.create_pixmap(
        ClientId(1),
        CreatePixmapRequest {
            depth: 24,
            pixmap: ResourceId(0x3100),
            drawable: ROOT_WINDOW,
            width: 8,
            height: 8,
        },
    );

    let bytes = run_free_resource_request(&mut state, &mut peer, 54, 0x3100, 8);

    assert!(
        bytes.is_empty(),
        "FreePixmap(known) must be silent, got {bytes:02x?}"
    );
    assert!(state.resources.pixmap(ResourceId(0x3100)).is_none());
}

/// Xorg `ProcFreeGC` -> `dixLookupGC` -> `X11_RESTYPE_GC.errorValue`
/// = `BadGC` (`../xserver/dix/resource.c:454`).
#[test]
fn free_gc_none_returns_bad_gc() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);

    let bytes = run_free_resource_request(&mut state, &mut peer, 60, 0, 0x0068);

    assert_x11_error_bytes(&bytes, x11::error::BAD_GC, 0, 0, 60, 0x0068, "FreeGC(None)");
}

#[test]
fn free_gc_unknown_xid_returns_bad_gc() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);

    let bytes = run_free_resource_request(&mut state, &mut peer, 60, 0x00be_ef00, 9);

    assert_x11_error_bytes(
        &bytes,
        x11::error::BAD_GC,
        0x00be_ef00,
        0,
        60,
        9,
        "FreeGC(unknown)",
    );
}

#[test]
fn free_gc_known_xid_reports_no_error() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    state
        .resources
        .seed_gc_for_test(ClientId(1), ResourceId(0x2100));

    let bytes = run_free_resource_request(&mut state, &mut peer, 60, 0x2100, 10);

    assert!(
        bytes.is_empty(),
        "FreeGC(known) must be silent, got {bytes:02x?}"
    );
    assert!(state.resources.gc(ResourceId(0x2100)).is_none());
}

/// Xorg `ProcFreeCursor` -> `X11_RESTYPE_CURSOR.errorValue` =
/// `BadCursor` (`../xserver/dix/resource.c:466`).
#[test]
fn free_cursor_none_returns_bad_cursor() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);

    let bytes = run_free_resource_request(&mut state, &mut peer, 95, 0, 0x0069);

    assert_x11_error_bytes(
        &bytes,
        x11::error::BAD_CURSOR,
        0,
        0,
        95,
        0x0069,
        "FreeCursor(None)",
    );
}

#[test]
fn free_cursor_unknown_xid_returns_bad_cursor() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);

    let bytes = run_free_resource_request(&mut state, &mut peer, 95, 0x00c0_ff00, 11);

    assert_x11_error_bytes(
        &bytes,
        x11::error::BAD_CURSOR,
        0x00c0_ff00,
        0,
        95,
        11,
        "FreeCursor(unknown)",
    );
}

#[test]
fn free_cursor_known_xid_reports_no_error() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    state
        .resources
        .create_cursor(ClientId(1), ResourceId(0x4100));

    let bytes = run_free_resource_request(&mut state, &mut peer, 95, 0x4100, 12);

    assert!(
        bytes.is_empty(),
        "FreeCursor(known) must be silent, got {bytes:02x?}"
    );
    assert!(!state.resources.cursor_exists(ResourceId(0x4100)));
}

#[test]
fn free_pixmap_retains_host_pixmap_while_gc_clip_mask_still_references_it() {
    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();

    state.resources.create_pixmap(
        ClientId(1),
        CreatePixmapRequest {
            depth: 1,
            pixmap: ResourceId(0x3000),
            drawable: ROOT_WINDOW,
            width: 8,
            height: 8,
        },
    );
    assert!(state.resources.set_pixmap_host_xid(
        ResourceId(0x3000),
        crate::backend::PixmapHandle::from_raw(0xcafe).unwrap(),
    ));
    state.resources.create_gc(
        ClientId(1),
        CreateGcRequest {
            gc: ResourceId(0x2000),
            drawable: ROOT_WINDOW,
            function: None,
            plane_mask: None,
            foreground: None,
            background: None,
            line_width: None,
            line_style: None,
            cap_style: None,
            join_style: None,
            fill_style: None,
            fill_rule: None,
            tile: None,
            stipple: None,
            tile_x_origin: None,
            tile_y_origin: None,
            font: None,
            subwindow_mode: None,
            graphics_exposures: None,
            clip_x_origin: None,
            clip_y_origin: None,
            clip_mask: Some(Some(ResourceId(0x3000))),
            dash_offset: None,
            dashes: None,
            arc_mode: None,
        },
    );

    handle_free_pixmap(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        &free_pixmap_body(0x3000),
    )
    .expect("free pixmap");

    let calls = backend.calls.lock().expect("calls");
    assert!(
        !calls
            .iter()
            .any(|call| matches!(call, RecordedCall::FreePixmap(0xcafe))),
        "host pixmap must stay alive while retained by GC clip mask"
    );
}

/// #196 fixture: client 1 owns GC 0x2100 and depth-24 tile / depth-1
/// stipple / depth-1 clip pixmaps, host xids 0xd001..=0xd003.
fn gc_with_freed_pixmaps(state: &mut ServerState, mask: u32, values: &[u32]) {
    for (i, depth) in [24u8, 1, 1].into_iter().enumerate() {
        let pixmap = ResourceId(0x3100 + i as u32);
        state.resources.create_pixmap(
            ClientId(1),
            CreatePixmapRequest {
                depth,
                pixmap,
                drawable: ROOT_WINDOW,
                width: 8,
                height: 8,
            },
        );
        assert!(state.resources.set_pixmap_host_xid(
            pixmap,
            crate::backend::PixmapHandle::from_raw(0xd001 + i as u32).unwrap(),
        ));
    }
    create_plain_gc(state, ResourceId(0x2100));
    change_gc(state, &mut RecordingBackend::new(), 0x2100, mask, values);
}

fn create_plain_gc(state: &mut ServerState, gc: ResourceId) {
    state.resources.create_gc(
        ClientId(1),
        CreateGcRequest {
            gc,
            drawable: ROOT_WINDOW,
            function: None,
            plane_mask: None,
            foreground: None,
            background: None,
            line_width: None,
            line_style: None,
            cap_style: None,
            join_style: None,
            fill_style: None,
            fill_rule: None,
            tile: None,
            stipple: None,
            tile_x_origin: None,
            tile_y_origin: None,
            font: None,
            subwindow_mode: None,
            graphics_exposures: None,
            clip_x_origin: None,
            clip_y_origin: None,
            clip_mask: None,
            dash_offset: None,
            dashes: None,
            arc_mode: None,
        },
    );
}

fn change_gc(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    gc: u32,
    mask: u32,
    values: &[u32],
) {
    let mut body = gc.to_le_bytes().to_vec();
    body.extend(mask.to_le_bytes());
    for v in values {
        body.extend(v.to_le_bytes());
    }
    handle_change_gc(state, backend, None, ClientId(1), SequenceNumber(1), &body)
        .expect("change gc");
}

fn free_pixmaps(state: &mut ServerState, backend: &mut RecordingBackend, ids: &[u32]) {
    for &id in ids {
        handle_free_pixmap(
            state,
            backend,
            None,
            ClientId(1),
            SequenceNumber(1),
            &free_pixmap_body(id),
        )
        .expect("free pixmap");
    }
}

const GC_TILE: u32 = 0x0000_0400;
const GC_STIPPLE: u32 = 0x0000_0800;
const GC_CLIP_MASK: u32 = 0x0008_0000;

/// #196: a GC's tile / stipple / clip-mask ref dies when ChangeGC replaces
/// it (Xorg `dix/gc.c:256`/`:273` DestroyPixmap the old tile / stipple;
/// `mi/migc.c:68` drops a clip mask as soon as it is a region). Once the
/// client has freed the pixmap, that was the last reference: the host
/// storage must go, exactly once. It leaked for the session.
#[test]
fn change_gc_releases_a_freed_pixmap_it_replaces() {
    for (mask, host) in [
        (GC_TILE, 0xd001),
        (GC_STIPPLE, 0xd002),
        (GC_CLIP_MASK, 0xd003),
    ] {
        let mut state = ServerState::new();
        let mut backend = RecordingBackend::new();
        let pixmap = 0x3100 + (host - 0xd001);
        gc_with_freed_pixmaps(&mut state, mask, &[pixmap]);
        free_pixmaps(&mut state, &mut backend, &[pixmap]);
        assert_eq!(
            host_frees(&backend),
            Vec::<u32>::new(),
            "mask {mask:#x}: GC still holds it"
        );
        // Tile can't be None: replace it with another pixmap; the rest clear.
        let replacement = if mask == GC_TILE { 0x3102 } else { 0 };
        change_gc(&mut state, &mut backend, 0x2100, mask, &[replacement]);
        assert_eq!(host_frees(&backend), vec![host], "mask {mask:#x}");
        change_gc(&mut state, &mut backend, 0x2100, mask, &[replacement]);
        assert_eq!(
            host_frees(&backend),
            vec![host],
            "mask {mask:#x}: exactly once"
        );
    }
}

/// The positive control: a displaced pixmap the client still owns stays.
#[test]
fn change_gc_keeps_a_replaced_pixmap_the_client_still_owns() {
    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    gc_with_freed_pixmaps(&mut state, GC_TILE | GC_STIPPLE, &[0x3100, 0x3101]);
    change_gc(
        &mut state,
        &mut backend,
        0x2100,
        GC_TILE | GC_STIPPLE,
        &[0x3102, 0x3102],
    );
    assert_eq!(host_frees(&backend), Vec::<u32>::new());
    free_pixmaps(&mut state, &mut backend, &[0x3100, 0x3101]);
    assert_eq!(
        host_frees(&backend),
        vec![0xd001, 0xd002],
        "unreferenced: freed at FreePixmap"
    );
}

#[test]
fn set_clip_rectangles_releases_a_freed_clip_mask() {
    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    gc_with_freed_pixmaps(&mut state, GC_CLIP_MASK, &[0x3102]);
    free_pixmaps(&mut state, &mut backend, &[0x3102]);
    let mut body = 0x2100u32.to_le_bytes().to_vec();
    body.extend([0u8; 4]); // clip x/y origin
    body.extend([0u8, 0, 0, 0, 8, 0, 8, 0]); // one rectangle
    handle_set_clip_rectangles(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        RequestHeader {
            opcode: 59,
            data: 0,
            length_units: 5,
        },
        &body,
    )
    .expect("set clip rectangles");
    assert_eq!(host_frees(&backend), vec![0xd003]);
}

/// XFixesSetGCClipRegion replaces the clip mask like SetClipRectangles
/// (`xfixes/region.c` `ProcXFixesSetGCClipRegion` -> `ChangeClip`), for
/// region None and for a real region alike: a clip mask the client freed
/// is released then, exactly once.
#[test]
fn xfixes_set_gc_clip_region_releases_a_freed_clip_mask() {
    use yserver_protocol::x11::xfixes as x11xfixes;
    for region in [0u32, 0x4100] {
        let mut state = ServerState::new();
        let mut backend = RecordingBackend::new();
        gc_with_freed_pixmaps(&mut state, GC_CLIP_MASK, &[0x3102]);
        free_pixmaps(&mut state, &mut backend, &[0x3102]);
        assert_eq!(host_frees(&backend), Vec::<u32>::new());
        let mut xfixes = |state: &mut ServerState, minor: u8, body: Vec<u8>| {
            let header = RequestHeader {
                opcode: XFIXES_MAJOR_OPCODE,
                data: minor,
                length_units: u32::try_from(1 + body.len() / 4).unwrap(),
            };
            handle_xfixes_request(
                state,
                &mut backend,
                None,
                ClientId(1),
                SequenceNumber(1),
                header,
                &body,
            )
            .expect("XFIXES request");
        };
        if region != 0 {
            let mut body = region.to_le_bytes().to_vec();
            body.extend([0u8, 0, 0, 0, 8, 0, 8, 0]);
            xfixes(&mut state, x11xfixes::CREATE_REGION, body);
        }
        let mut body = 0x2100u32.to_le_bytes().to_vec();
        body.extend(region.to_le_bytes());
        body.extend([0u8; 4]); // clip x/y origin
        xfixes(&mut state, x11xfixes::SET_GC_CLIP_REGION, body.clone());
        xfixes(&mut state, x11xfixes::SET_GC_CLIP_REGION, body);
        assert_eq!(
            host_frees(&backend),
            vec![0xd003],
            "region {region:#x}: released once"
        );
    }
}

/// Xorg `CopyGC` drops dst's old tile / stipple (`dix/gc.c:673`/`:685`);
/// one the source GC also holds survives.
#[test]
fn copy_gc_releases_freed_pixmaps_it_overwrites() {
    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    gc_with_freed_pixmaps(&mut state, GC_TILE | GC_STIPPLE, &[0x3100, 0x3101]);
    create_plain_gc(&mut state, ResourceId(0x2101));
    change_gc(
        &mut state,
        &mut backend,
        0x2101,
        GC_TILE | GC_STIPPLE,
        &[0x3100, 0x3102],
    );
    free_pixmaps(&mut state, &mut backend, &[0x3100, 0x3101, 0x3102]);
    assert_eq!(
        host_frees(&backend),
        Vec::<u32>::new(),
        "both GCs hold them"
    );
    let mut body = 0x2101u32.to_le_bytes().to_vec();
    body.extend(0x2100u32.to_le_bytes());
    body.extend((GC_TILE | GC_STIPPLE).to_le_bytes());
    handle_copy_gc(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        &body,
    )
    .expect("copy gc");
    // 0x2100 now holds tile 0xd001 + stipple 0xd003; its old stipple 0xd002 went.
    assert_eq!(host_frees(&backend), vec![0xd002]);
}

/// Xorg `FreeGC` drops the tile / stipple / clip (`dix/gc.c:776-781`).
#[test]
fn free_gc_releases_the_freed_pixmaps_it_held() {
    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    gc_with_freed_pixmaps(
        &mut state,
        GC_TILE | GC_STIPPLE | GC_CLIP_MASK,
        &[0x3100, 0x3101, 0x3102],
    );
    free_pixmaps(&mut state, &mut backend, &[0x3100, 0x3101]);
    handle_free_gc(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        &0x2100u32.to_le_bytes(),
    )
    .expect("free gc");
    assert_eq!(
        host_frees(&backend),
        vec![0xd001, 0xd002],
        "0x3102 is still the client's"
    );
}

/// #133: `FreePixmap` has to respect a window's BORDER reference the same
/// way it respects a background one. `XCreatePixmap` →
/// `XSetWindowBorderPixmap` → `XFreePixmap` is ordinary client code, and
/// X11 keeps the storage alive because the window still names it (Xorg
/// refcounts `pWin->border.pixmap`). `handle_free_pixmap` consulted only
/// the background and GC references, so it freed the host handle
/// underneath a ring that was still sampling it — found by auditing this
/// case against `change_window_attributes`'s release path, which does gate
/// on all three.
#[test]
fn free_pixmap_retains_host_pixmap_while_a_window_border_still_references_it() {
    const HOST_TILE: u32 = 0x9999_0007;
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let win = ResourceId(0x0080_0011);
    seed_window(&mut state, win, ROOT_WINDOW, 10, 10);
    let tile = ResourceId(0x0080_1307);
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
    assert!(state.resources.set_pixmap_host_xid(
        tile,
        crate::backend::PixmapHandle::from_raw(HOST_TILE).expect("non-zero"),
    ));

    run_border_request(
        &mut state,
        2,
        0,
        &border_cwa_body(win.0, CWA_BORDER_PIXMAP, &[tile.0]),
    );
    assert_no_error(&read_all_available(&mut peer), "CWA border-pixmap");
    // Not vacuous: the reference must actually be recorded, or the
    // retention below would hold for the wrong reason.
    assert!(
        state.resources.host_xid_referenced_by_window_border(
            crate::backend::PixmapHandle::from_raw(HOST_TILE).expect("non-zero")
        ),
        "the window must hold the tile as its border before FreePixmap"
    );

    let mut backend = RecordingBackend::new();
    handle_free_pixmap(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(2),
        &free_pixmap_body(tile.0),
    )
    .expect("free pixmap");

    assert!(
        !backend
            .calls()
            .iter()
            .any(|call| matches!(call, RecordedCall::FreePixmap(HOST_TILE))),
        "host pixmap must stay alive while a window still borders with it"
    );
}

/// The cross-reference case the CWA release path calls out: nothing stops
/// a client naming one pixmap as a background on window A and a border on
/// window B. The order here is deliberate — the background reference is
/// dropped FIRST, so the `FreePixmap` in the middle is suppressed by the
/// BORDER check alone and the step is not vacuous. The final release is
/// the positive control: once nothing holds the tile the host handle must
/// actually be freed, so a test that can never observe a free would fail.
#[test]
fn a_host_pixmap_shared_as_background_and_border_frees_only_when_both_let_go() {
    const HOST_TILE: u32 = 0x9999_0008;
    let host = crate::backend::PixmapHandle::from_raw(HOST_TILE).expect("non-zero");
    const CWA_BACK_PIXMAP: u32 = 0x0001;
    let freed = |calls: &[RecordedCall]| {
        calls
            .iter()
            .any(|call| matches!(call, RecordedCall::FreePixmap(HOST_TILE)))
    };

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let bg_win = ResourceId(0x0080_0021);
    let border_win = ResourceId(0x0080_0022);
    seed_window(&mut state, bg_win, ROOT_WINDOW, 10, 10);
    seed_window(&mut state, border_win, ROOT_WINDOW, 10, 10);
    let tile = ResourceId(0x0080_1308);
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
    assert!(state.resources.set_pixmap_host_xid(tile, host));

    run_border_request(
        &mut state,
        2,
        0,
        &border_cwa_body(bg_win.0, CWA_BACK_PIXMAP, &[tile.0]),
    );
    run_border_request(
        &mut state,
        2,
        0,
        &border_cwa_body(border_win.0, CWA_BORDER_PIXMAP, &[tile.0]),
    );
    assert_no_error(&read_all_available(&mut peer), "CWA shared tile");
    assert!(
        state.resources.host_xid_referenced_by_window_bg(host),
        "window A must hold the tile as its background"
    );
    assert!(
        state.resources.host_xid_referenced_by_window_border(host),
        "window B must hold the tile as its border"
    );

    // 1. Drop A's background. B's border and the client's own ownership
    //    both still hold the tile.
    let calls = run_border_request_recording(
        &mut state,
        2,
        0,
        &border_cwa_body(bg_win.0, CWA_BACK_PIXMAP, &[0]),
    );
    assert!(
        !state.resources.host_xid_referenced_by_window_bg(host),
        "window A must no longer hold the tile as its background"
    );
    assert!(
        !freed(&calls),
        "replacing the background must not free storage the other window's \
             border still samples"
    );

    // 2. The client drops the resource id. Only B's border holds the tile
    //    now, so this step exercises the border check on its own.
    let mut backend = RecordingBackend::new();
    handle_free_pixmap(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(3),
        &free_pixmap_body(tile.0),
    )
    .expect("free pixmap");
    assert!(
        !freed(&backend.calls()),
        "FreePixmap must respect a border-only reference"
    );

    // 3. Positive control: B swaps to a solid border, nothing holds the
    //    tile and the client no longer owns it, so it must be released.
    let calls = run_border_request_recording(
        &mut state,
        2,
        0,
        &border_cwa_body(border_win.0, CWA_BORDER_PIXEL, &[0x0000_00ff]),
    );
    assert!(
        freed(&calls),
        "the fully orphaned host tile must be released, or this test could \
             never observe a free at all"
    );
}

/// #133: the mirror image of the retention above. A tile kept alive past
/// its `FreePixmap` by a window's border must be RELEASED when that window
/// dies — otherwise nothing is left to notice it: no resource owns it and
/// no window references it. `destroy_window_subtree` collected only
/// background pixmaps, so this leaked the host storage for the rest of the
/// session.
#[test]
fn destroying_a_window_releases_the_border_tile_it_was_keeping_alive() {
    const HOST_TILE: u32 = 0x9999_0009;
    let host = crate::backend::PixmapHandle::from_raw(HOST_TILE).expect("non-zero");
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let win = ResourceId(0x0080_0031);
    seed_window(&mut state, win, ROOT_WINDOW, 10, 10);
    let tile = ResourceId(0x0080_1309);
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
    assert!(state.resources.set_pixmap_host_xid(tile, host));
    run_border_request(
        &mut state,
        2,
        0,
        &border_cwa_body(win.0, CWA_BORDER_PIXMAP, &[tile.0]),
    );
    assert_no_error(&read_all_available(&mut peer), "CWA border-pixmap");

    // The client drops the id; the border reference retains the storage.
    let mut backend = RecordingBackend::new();
    handle_free_pixmap(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(2),
        &free_pixmap_body(tile.0),
    )
    .expect("free pixmap");
    assert!(
        !backend
            .calls()
            .iter()
            .any(|call| matches!(call, RecordedCall::FreePixmap(HOST_TILE))),
        "retained while the border holds it"
    );
    assert!(
        state.resources.host_xid_still_referenced(host),
        "the border must be the only thing keeping it alive now"
    );

    // Destroying the window removes that last reference, so the host
    // handle has to go with it.
    let mut backend = RecordingBackend::new();
    destroy_window_subtree(&mut state, &mut backend, None, win);
    assert!(
        !state.resources.host_xid_still_referenced(host),
        "nothing may reference the tile after the window is destroyed"
    );
    assert!(
        backend
            .calls()
            .iter()
            .any(|call| matches!(call, RecordedCall::FreePixmap(HOST_TILE))),
        "destroying the window must release the border tile it was keeping alive"
    );
}

/// #133 minor: one request replacing BOTH attributes, where both named the
/// same tile, hands the same host handle back twice. Freeing it twice is
/// inert on KMS — the store entry is gone by the second call — but it is a
/// broken backend contract, and a recording or host-X11 backend sees the
/// duplicate.
#[test]
fn replacing_a_shared_background_and_border_frees_the_host_tile_once() {
    const HOST_OLD: u32 = 0x9999_000a;
    const HOST_NEW: u32 = 0x9999_000b;
    const CWA_BACK_PIXMAP: u32 = 0x0001;
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let win = ResourceId(0x0080_0041);
    seed_window(&mut state, win, ROOT_WINDOW, 10, 10);

    let make_tile = |state: &mut ServerState, id: u32, host: u32| {
        let pix = ResourceId(id);
        state.resources.create_pixmap(
            ClientId(1),
            CreatePixmapRequest {
                pixmap: pix,
                drawable: ROOT_WINDOW,
                width: 8,
                height: 8,
                depth: 24,
            },
        );
        assert!(state.resources.set_pixmap_host_xid(
            pix,
            crate::backend::PixmapHandle::from_raw(host).expect("non-zero")
        ));
        pix
    };
    let old = make_tile(&mut state, 0x0080_1310, HOST_OLD);
    let new = make_tile(&mut state, 0x0080_1311, HOST_NEW);

    // One tile as BOTH the background and the border.
    run_border_request(
        &mut state,
        2,
        0,
        &border_cwa_body(win.0, CWA_BACK_PIXMAP | CWA_BORDER_PIXMAP, &[old.0, old.0]),
    );
    assert_no_error(&read_all_available(&mut peer), "CWA shared tile");

    // The client drops the id, so only the two attributes hold it and the
    // replacement below fully orphans it.
    let mut backend = RecordingBackend::new();
    handle_free_pixmap(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(2),
        &free_pixmap_body(old.0),
    )
    .expect("free pixmap");

    // Replace both in ONE request: `released.background` and
    // `released.border` are the same handle.
    let calls = run_border_request_recording(
        &mut state,
        2,
        0,
        &border_cwa_body(win.0, CWA_BACK_PIXMAP | CWA_BORDER_PIXMAP, &[new.0, new.0]),
    );
    let frees = calls
        .iter()
        .filter(|call| matches!(call, RecordedCall::FreePixmap(HOST_OLD)))
        .count();
    assert_eq!(
        frees, 1,
        "the shared host tile must be freed exactly once, got {frees}"
    );
}

#[test]
fn free_pixmap_retains_host_pixmap_while_gc_tile_still_references_it() {
    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();

    state.resources.create_pixmap(
        ClientId(1),
        CreatePixmapRequest {
            depth: 1,
            pixmap: ResourceId(0x3001),
            drawable: ROOT_WINDOW,
            width: 8,
            height: 8,
        },
    );
    assert!(state.resources.set_pixmap_host_xid(
        ResourceId(0x3001),
        crate::backend::PixmapHandle::from_raw(0xbeef).unwrap(),
    ));
    state.resources.create_gc(
        ClientId(1),
        CreateGcRequest {
            gc: ResourceId(0x2001),
            drawable: ROOT_WINDOW,
            function: None,
            plane_mask: None,
            foreground: None,
            background: None,
            line_width: None,
            line_style: None,
            cap_style: None,
            join_style: None,
            fill_style: Some(FillStyle::Tiled.protocol_value()),
            fill_rule: None,
            tile: Some(ResourceId(0x3001)),
            stipple: None,
            tile_x_origin: None,
            tile_y_origin: None,
            font: None,
            subwindow_mode: None,
            graphics_exposures: None,
            clip_x_origin: None,
            clip_y_origin: None,
            clip_mask: None,
            dash_offset: None,
            dashes: None,
            arc_mode: None,
        },
    );

    handle_free_pixmap(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        &free_pixmap_body(0x3001),
    )
    .expect("free pixmap");

    let calls = backend.calls.lock().expect("calls");
    assert!(
        !calls
            .iter()
            .any(|call| matches!(call, RecordedCall::FreePixmap(0xbeef))),
        "host pixmap must stay alive while retained by GC tile"
    );
}
