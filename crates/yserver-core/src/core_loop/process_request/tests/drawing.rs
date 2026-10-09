use super::*;

#[test]
fn xybitmap_to_zpixmap_preserves_lsb_first_bits_without_left_pad() {
    let src = [
        0b0000_1101u8,
        0,
        0,
        0, // row 0: 1 0 1 1
        0b0000_0010u8,
        0,
        0,
        0, // row 1: 0 1 0 0
    ];
    let out = xybitmap_to_zpixmap(&src, 4, 2, 0).expect("convert");
    assert_eq!(out, src);
}

#[test]
fn xybitmap_to_zpixmap_strips_left_pad_bits() {
    let src = [
        0b0011_0100u8,
        0,
        0,
        0, // left_pad=2, payload bits => 1 0 1 1
    ];
    let out = xybitmap_to_zpixmap(&src, 4, 1, 2).expect("convert");
    assert_eq!(out, vec![0b0000_1101u8, 0, 0, 0]);
}

#[test]
fn xybitmap_to_target_zpixmap_expands_bits_with_fg_bg_at_depth_24() {
    let src = [
        0b0000_0101u8,
        0,
        0,
        0, // row: 1 0 1
    ];
    let out =
        xybitmap_to_target_zpixmap(&src, 3, 1, 0, 24, 0x0000_00ff, 0x0000_ff00).expect("convert");
    assert_eq!(out.len(), 12);
    assert_eq!(&out[0..4], &0x0000_00ffu32.to_le_bytes());
    assert_eq!(&out[4..8], &0x0000_ff00u32.to_le_bytes());
    assert_eq!(&out[8..12], &0x0000_00ffu32.to_le_bytes());
}

#[test]
fn xybitmap_to_target_zpixmap_expands_bits_with_fg_bg_at_depth_4() {
    let src = [
        0b0000_0110u8,
        0,
        0,
        0, // row: 0 1 1
    ];
    let out = xybitmap_to_target_zpixmap(&src, 3, 1, 0, 4, 0x0d, 0x02).expect("convert");
    assert_eq!(out, vec![0xd2, 0x0d, 0, 0]);
}

#[test]
fn poly_fill_rectangle_unknown_drawable_returns_bad_drawable() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();

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
            clip_mask: None,
            dash_offset: None,
            dashes: None,
            arc_mode: None,
        },
    );

    let body = poly_fill_rectangle_body(0xdead_beef, 0x2000);
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
    assert_eq!(buf[1], x11::error::BAD_DRAWABLE);
    assert_eq!(buf[10], 70);
}

#[test]
fn query_font_invalid_fontable_returns_bad_font() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    // QueryFont (op 47) body = FONTABLE(4). 0xdeadbeef is unregistered.
    let body = 0xdead_beefu32.to_le_bytes();
    process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        RequestHeader {
            opcode: 47,
            data: 0,
            length_units: 2,
        },
        &body,
        None,
    )
    .expect("process_request");
    let bytes = read_all_available(&mut peer);
    assert!(bytes.len() >= 32, "expected error reply, got {bytes:02x?}");
    assert_eq!(bytes[1], x11::error::BAD_FONT, "code");
    assert_eq!(bytes[10], 47, "major opcode");
    assert_eq!(&bytes[4..8], &0xdead_beefu32.to_le_bytes(), "bad font id");
}

#[test]
fn query_text_extents_invalid_fontable_returns_bad_font() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    // QueryTextExtents (op 48) body = FONTABLE(4) + STRING16 (zero chars).
    let body = 0xdead_beefu32.to_le_bytes();
    process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        RequestHeader {
            opcode: 48,
            data: 0,
            length_units: 2,
        },
        &body,
        None,
    )
    .expect("process_request");
    let bytes = read_all_available(&mut peer);
    assert!(bytes.len() >= 32, "expected error reply, got {bytes:02x?}");
    assert_eq!(bytes[1], x11::error::BAD_FONT, "code");
    assert_eq!(bytes[10], 48, "major opcode");
    assert_eq!(
        &bytes[4..8],
        &0xdead_beefu32.to_le_bytes(),
        "bad fontable id"
    );
}

#[test]
fn poly_fill_rectangle_input_only_returns_bad_match() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();

    state.resources.create_window(
        yserver_protocol::x11::ClientId(1),
        CreateWindowRequest {
            depth: 0,
            window: ResourceId(0x1100),
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 10,
            height: 10,
            border_width: 0,
            class: 2,
            visual: ResourceId(0),
            ..Default::default()
        },
    );
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
            clip_mask: None,
            dash_offset: None,
            dashes: None,
            arc_mode: None,
        },
    );

    let body = poly_fill_rectangle_body(0x1100, 0x2000);
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
    assert_eq!(buf[1], x11::error::BAD_MATCH);
    assert_eq!(buf[10], 70);
}

#[test]
fn poly_fill_rectangle_depth_mismatch_returns_bad_match() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();

    state.resources.create_pixmap(
        ClientId(1),
        CreatePixmapRequest {
            pixmap: ResourceId(0x1200),
            drawable: ROOT_WINDOW,
            width: 4,
            height: 4,
            depth: 1,
        },
    );
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
            clip_mask: None,
            dash_offset: None,
            dashes: None,
            arc_mode: None,
        },
    );

    let body = poly_fill_rectangle_body(0x1200, 0x2000);
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
    assert_eq!(buf[1], x11::error::BAD_MATCH);
    assert_eq!(buf[10], 70);
}

/// X11 §CopyArea source-availability split: requested source
/// rect vs source drawable bounds → (clamped copy, missing
/// dst-coord rects for GraphicsExpose).
#[test]
fn copy_area_source_split_right_half_missing() {
    let mut state = ServerState::new();
    // 100×100 pixmap as the source.
    state.resources.create_pixmap(
        ClientId(1),
        yserver_protocol::x11::CreatePixmapRequest {
            depth: 24,
            pixmap: ResourceId(0x700),
            drawable: crate::resources::ROOT_WINDOW,
            width: 100,
            height: 100,
        },
    );
    // Copy 100 wide from x=50 → right 50 missing.
    let (avail, missing) = copy_area_source_split(&state, ResourceId(0x700), 50, 0, 0, 0, 100, 100);
    assert_eq!(avail, Some((50, 0, 0, 0, 50, 100)), "clamped copy");
    assert_eq!(
        missing,
        vec![(50, 0, 50, 100)],
        "missing right half in DST coords (xts XCopyArea-4 expects x=50 w=50)"
    );
    // Fully in-bounds → NoExposure (empty missing list).
    let (avail, missing) = copy_area_source_split(&state, ResourceId(0x700), 0, 0, 10, 10, 80, 80);
    assert_eq!(avail, Some((0, 0, 10, 10, 80, 80)));
    assert!(missing.is_empty(), "fully available → NoExposure");
    // Fully out of bounds → no copy, whole rect exposed.
    let (avail, missing) = copy_area_source_split(&state, ResourceId(0x700), 200, 0, 5, 5, 30, 30);
    assert!(avail.is_none());
    assert_eq!(missing, vec![(5, 5, 30, 30)]);
}

/// Stage 4d follow-up (codex review 2026-05-18): when ClipByChildren
/// (or any other clipping) fully covers the destination so no pixels
/// are actually copied, the X11 spec still requires the server to
/// emit a GraphicsExpose / NoExposure event for clients with
/// `graphics-exposures=True`. Pre-fix the early-return-on-empty
/// path silently swallowed this event; clients waiting on it
/// (xterm, gtk2 backing-store users) would hang.
#[test]
fn copy_area_fully_clipped_still_emits_graphics_expose_event() {
    use std::io::Read;
    use yserver_protocol::x11::{CreateGcRequest, CreateWindowRequest};

    const FRAME_XID: u32 = 0x0010_0001;
    const FRAME_HOST: u32 = 0x0040_0001;
    const FRAME_BACKING_HOST: u32 = 0x0050_0001;
    const CHILD_XID: u32 = 0x0010_0002;
    const CHILD_HOST: u32 = 0x0040_0002;
    const SRC_PIXMAP_XID: u32 = 0x0010_0010;
    const SRC_PIXMAP_HOST: u32 = 0x0040_0010;
    const GC_XID: u32 = 0x0010_0020;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    // Make the peer-read non-blocking so an absent event doesn't
    // hang the test.
    peer.set_nonblocking(true).expect("nonblocking");
    let mut backend = RecordingBackend::new();

    // Frame 100×100 entirely covered by child window 100×100 at
    // (0, 0). Any CopyArea(dst=frame, 0,0 100x100) gets fully
    // clipped by the child.
    state.resources.create_window(
        yserver_protocol::x11::ClientId(1),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(FRAME_XID),
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
    {
        let w = state
            .resources
            .window_mut(ResourceId(FRAME_XID))
            .expect("frame");
        w.host_xid = Some(crate::backend::WindowHandle::from_raw_for_test(FRAME_HOST));
        w.redirected_backing = Some(crate::resources::RedirectedBacking {
            host_pixmap: crate::backend::PixmapHandle::from_raw_for_test(FRAME_BACKING_HOST),
            width: 100,
            height: 100,
            depth: 24,
        });
        w.map_state = crate::resources::MapState::Viewable;
    }
    state.resources.create_window(
        yserver_protocol::x11::ClientId(1),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(CHILD_XID),
            parent: ResourceId(FRAME_XID),
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
    {
        let w = state
            .resources
            .window_mut(ResourceId(CHILD_XID))
            .expect("child");
        w.host_xid = Some(crate::backend::WindowHandle::from_raw_for_test(CHILD_HOST));
        w.map_state = crate::resources::MapState::Viewable;
    }
    state.resources.create_pixmap(
        ClientId(1),
        yserver_protocol::x11::CreatePixmapRequest {
            pixmap: ResourceId(SRC_PIXMAP_XID),
            drawable: ROOT_WINDOW,
            width: 100,
            height: 100,
            depth: 24,
        },
    );
    let _ = state.resources.set_pixmap_host_xid(
        ResourceId(SRC_PIXMAP_XID),
        crate::backend::PixmapHandle::from_raw_for_test(SRC_PIXMAP_HOST),
    );
    // GC with graphics_exposures = True (the spec default; without
    // explicit override the GC inherits that default).
    state.resources.create_gc(
        ClientId(1),
        CreateGcRequest {
            gc: ResourceId(GC_XID),
            drawable: ResourceId(FRAME_XID),
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
            graphics_exposures: Some(true),
            clip_x_origin: None,
            clip_y_origin: None,
            clip_mask: None,
            dash_offset: None,
            dashes: None,
            arc_mode: None,
        },
    );

    let mut body = Vec::with_capacity(24);
    body.extend_from_slice(&SRC_PIXMAP_XID.to_le_bytes());
    body.extend_from_slice(&FRAME_XID.to_le_bytes());
    body.extend_from_slice(&GC_XID.to_le_bytes());
    body.extend_from_slice(&0_i16.to_le_bytes());
    body.extend_from_slice(&0_i16.to_le_bytes());
    body.extend_from_slice(&0_i16.to_le_bytes());
    body.extend_from_slice(&0_i16.to_le_bytes());
    body.extend_from_slice(&100_u16.to_le_bytes());
    body.extend_from_slice(&100_u16.to_le_bytes());

    process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        RequestHeader {
            opcode: 62,
            data: 0,
            length_units: 7,
        },
        &body,
        None,
    )
    .expect("dispatch CopyArea");

    // No backend.copy_area should have fired (fully clipped).
    let copy_calls = backend
        .calls()
        .into_iter()
        .filter(|c| matches!(c, crate::backend::recording::RecordedCall::CopyArea { .. }))
        .count();
    assert_eq!(
        copy_calls, 0,
        "fully-clipped CopyArea must NOT call backend.copy_area",
    );

    // The graphics-exposures contract still fires for a fully
    // dst-clipped copy — but as NoExposure: the SOURCE was fully
    // available (GraphicsExpose only describes missing source
    // regions; dst-side child clipping doesn't expose anything).
    let mut buf = [0u8; 64];
    let n = peer.read(&mut buf).unwrap_or(0);
    assert!(
        n >= 32,
        "expected a 32-byte NoExposure event even for a \
             fully-clipped CopyArea (codex review 2026-05-18); got \
             {n} bytes",
    );
    assert_eq!(
        buf[0], 14,
        "first byte must be NoExposure (event type 14) — source \
             fully available",
    );
}

// ---- Unviewable CopyArea/CopyPlane sources (Xorg micopy.c / miexpose.c) ----

const UV_FRAME: u32 = 0x0010_0101;
const UV_CHILD: u32 = 0x0010_0102;
const UV_LONE: u32 = 0x0010_0103;
const UV_SHOWN: u32 = 0x0010_0104;
const UV_PIXMAP: u32 = 0x0010_0110;
const UV_DST_PIXMAP: u32 = 0x0010_0111;
const UV_GC: u32 = 0x0010_0120;
const UV_GC_NOEXP: u32 = 0x0010_0121;

fn uv_gc_request(gc: u32, graphics_exposures: bool) -> yserver_protocol::x11::CreateGcRequest {
    yserver_protocol::x11::CreateGcRequest {
        gc: ResourceId(gc),
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
        graphics_exposures: Some(graphics_exposures),
        clip_x_origin: None,
        clip_y_origin: None,
        clip_mask: None,
        dash_offset: None,
        dashes: None,
        arc_mode: None,
    }
}

fn uv_window(
    state: &mut ServerState,
    xid: u32,
    parent: ResourceId,
    map_state: crate::resources::MapState,
) {
    state.resources.create_window(
        ClientId(1),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(xid),
            parent,
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
    let w = state.resources.window_mut(ResourceId(xid)).expect("window");
    w.host_xid = Some(crate::backend::WindowHandle::from_raw_for_test(
        xid | 0x0040_0000,
    ));
    w.map_state = map_state;
}

fn uv_pixmap(state: &mut ServerState, xid: u32) {
    state.resources.create_pixmap(
        ClientId(1),
        yserver_protocol::x11::CreatePixmapRequest {
            pixmap: ResourceId(xid),
            drawable: ROOT_WINDOW,
            width: 100,
            height: 100,
            depth: 24,
        },
    );
    let _ = state.resources.set_pixmap_host_xid(
        ResourceId(xid),
        crate::backend::PixmapHandle::from_raw_for_test(xid | 0x0040_0000),
    );
}

/// Unmapped frame with a mapped (so Unviewable) child, a lone unmapped
/// window, a viewable window, two pixmaps and GCs with exposures on/off.
fn uv_fixture() -> (ServerState, UnixStream) {
    use crate::resources::MapState;
    let mut state = ServerState::new();
    let peer = install_client(&mut state, 1);
    uv_window(&mut state, UV_FRAME, ROOT_WINDOW, MapState::Unmapped);
    uv_window(
        &mut state,
        UV_CHILD,
        ResourceId(UV_FRAME),
        MapState::Unviewable,
    );
    uv_window(&mut state, UV_LONE, ROOT_WINDOW, MapState::Unmapped);
    uv_window(&mut state, UV_SHOWN, ROOT_WINDOW, MapState::Viewable);
    uv_pixmap(&mut state, UV_PIXMAP);
    uv_pixmap(&mut state, UV_DST_PIXMAP);
    state
        .resources
        .create_gc(ClientId(1), uv_gc_request(UV_GC, true));
    state
        .resources
        .create_gc(ClientId(1), uv_gc_request(UV_GC_NOEXP, false));
    (state, peer)
}

#[allow(clippy::too_many_arguments)]
fn uv_copy(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    opcode: u8,
    src: u32,
    dst: u32,
    gc: u32,
    src_xy: (i16, i16),
    dst_xy: (i16, i16),
    size: (u16, u16),
) {
    let mut body = Vec::with_capacity(28);
    body.extend_from_slice(&src.to_le_bytes());
    body.extend_from_slice(&dst.to_le_bytes());
    body.extend_from_slice(&gc.to_le_bytes());
    body.extend_from_slice(&src_xy.0.to_le_bytes());
    body.extend_from_slice(&src_xy.1.to_le_bytes());
    body.extend_from_slice(&dst_xy.0.to_le_bytes());
    body.extend_from_slice(&dst_xy.1.to_le_bytes());
    body.extend_from_slice(&size.0.to_le_bytes());
    body.extend_from_slice(&size.1.to_le_bytes());
    if opcode == 63 {
        body.extend_from_slice(&1u32.to_le_bytes());
    }
    process_request(
        state,
        backend,
        ClientId(1),
        SequenceNumber(1),
        RequestHeader {
            opcode,
            data: 0,
            length_units: u32::try_from(1 + body.len() / 4).unwrap(),
        },
        &body,
        None,
    )
    .expect("dispatch copy");
}

fn uv_copy_calls(backend: &RecordingBackend) -> usize {
    use crate::backend::recording::RecordedCall;
    backend
        .calls()
        .into_iter()
        .filter(|c| {
            matches!(
                c,
                RecordedCall::CopyArea { .. } | RecordedCall::CopyPlane { .. }
            )
        })
        .count()
}

/// Decoded (type, drawable, x, y, w, h, count, major) per 32-byte event.
type UvEvent = (u8, u32, u16, u16, u16, u16, u16, u8);

fn uv_events(bytes: &[u8]) -> Vec<UvEvent> {
    bytes
        .chunks_exact(32)
        .map(|e| {
            let u16_at = |i: usize| u16::from_le_bytes([e[i], e[i + 1]]);
            let drawable = u32::from_le_bytes([e[4], e[5], e[6], e[7]]);
            if e[0] == 13 {
                (
                    13,
                    drawable,
                    u16_at(8),
                    u16_at(10),
                    u16_at(12),
                    u16_at(14),
                    u16_at(18),
                    e[20],
                )
            } else {
                (e[0], drawable, 0, 0, 0, 0, 0, e[10])
            }
        })
        .collect()
}

/// `miHandleExposures` (`mi/miexpose.c:120-305`) as measured by
/// tools/vng-scenarios/expose-probe.c: a client window C (190x100)
/// with a child V (100,10 80x120). A scroll whose source runs past
/// C's bottom lands its missing rows outside C: NoExpose. A source
/// under V is hidden under ClipByChildren, not under IncludeInferiors.
#[test]
fn copy_area_exposes_what_the_source_hides_inside_the_destination_clip() {
    use crate::resources::MapState;
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let (c, v) = (0x0010_0130u32, 0x0010_0131u32);
    uv_window(&mut state, c, ROOT_WINDOW, MapState::Viewable);
    uv_window(&mut state, v, ResourceId(c), MapState::Viewable);
    for (id, x, y, w, h) in [(c, 5, 20, 190, 100), (v, 100, 10, 80, 120)] {
        let win = state.resources.window_mut(ResourceId(id)).unwrap();
        (win.x, win.y, win.width, win.height) = (x, y, w, h);
    }
    state
        .resources
        .create_gc(ClientId(1), uv_gc_request(UV_GC, true));
    let mut inferiors = uv_gc_request(UV_GC_NOEXP, true);
    inferiors.subwindow_mode = Some(1);
    state.resources.create_gc(ClientId(1), inferiors);
    let mut copy = |state: &mut ServerState, gc, src: (i16, i16), dst: (i16, i16), size| {
        uv_copy(state, &mut backend, 62, c, c, gc, src, dst, size);
        uv_events(&read_all_available(&mut peer))
    };
    assert_eq!(
        copy(&mut state, UV_GC, (0, 0), (0, 40), (100, 200)),
        vec![(14, c, 0, 0, 0, 0, 0, 62)],
        "NoExpose"
    );
    assert_eq!(
        copy(&mut state, UV_GC, (90, 20), (10, 20), (40, 40)),
        vec![(13, c, 20, 20, 30, 40, 0, 62)]
    );
    assert_eq!(
        copy(&mut state, UV_GC_NOEXP, (90, 20), (10, 20), (40, 40)),
        vec![(14, c, 0, 0, 0, 0, 0, 62)],
        "IncludeInferiors: V's pixels are the source's"
    );
}

/// Xorg: an unrealized source window has an empty clipList
/// (mivaltree.c:691-696, miwindow.c:741-744, window.c:892), so miDoCopy
/// copies nothing and miHandleExposures exposes the whole rect.
#[test]
fn copy_area_from_unviewable_window_exposes_whole_dest_rect() {
    for (opcode, src) in [
        (62u8, UV_LONE),
        (62, UV_CHILD),
        (63, UV_LONE),
        (63, UV_CHILD),
    ] {
        let (mut state, mut peer) = uv_fixture();
        let mut backend = RecordingBackend::new();
        uv_copy(
            &mut state,
            &mut backend,
            opcode,
            src,
            UV_DST_PIXMAP,
            UV_GC,
            (10, 20),
            (5, 7),
            (30, 40),
        );
        assert_eq!(
            uv_copy_calls(&backend),
            0,
            "op {opcode} src 0x{src:x}: nothing copied"
        );
        assert_eq!(
            uv_events(&read_all_available(&mut peer)),
            vec![(13, UV_DST_PIXMAP, 5, 7, 30, 40, 0, opcode)],
            "op {opcode} src 0x{src:x}: one GraphicsExpose for the dest rect, no NoExpose",
        );
    }
}

#[test]
fn copy_area_from_unviewable_window_without_exposures_sends_nothing() {
    for opcode in [62u8, 63] {
        let (mut state, mut peer) = uv_fixture();
        let mut backend = RecordingBackend::new();
        uv_copy(
            &mut state,
            &mut backend,
            opcode,
            UV_CHILD,
            UV_DST_PIXMAP,
            UV_GC_NOEXP,
            (0, 0),
            (0, 0),
            (30, 40),
        );
        assert_eq!(uv_copy_calls(&backend), 0);
        assert!(
            read_all_available(&mut peer).is_empty(),
            "op {opcode}: no events"
        );
    }
}

/// A pixmap or viewable window source is unchanged: copied, NoExpose.
#[test]
fn copy_area_from_pixmap_or_viewable_window_still_copies_with_no_expose() {
    for opcode in [62u8, 63] {
        for src in [UV_PIXMAP, UV_SHOWN] {
            let (mut state, mut peer) = uv_fixture();
            let mut backend = RecordingBackend::new();
            uv_copy(
                &mut state,
                &mut backend,
                opcode,
                src,
                UV_DST_PIXMAP,
                UV_GC,
                (10, 20),
                (5, 7),
                (30, 40),
            );
            assert_eq!(uv_copy_calls(&backend), 1, "op {opcode} src 0x{src:x}");
            assert_eq!(
                uv_events(&read_all_available(&mut peer)),
                vec![(14, UV_DST_PIXMAP, 0, 0, 0, 0, 0, opcode)],
                "op {opcode} src 0x{src:x}: NoExpose",
            );
        }
    }
}

/// Xorg miDoCopy returns NULL for an unrealized destination before any
/// copy or exposure (micopy.c:157-160), so the requestor gets NoExpose
/// even when the source is unavailable.
#[test]
fn copy_area_to_unviewable_window_copies_nothing_and_sends_no_expose() {
    for opcode in [62u8, 63] {
        for (src, src_xy) in [(UV_PIXMAP, (0, 0)), (UV_PIXMAP, (90, 0)), (UV_LONE, (0, 0))] {
            let (mut state, mut peer) = uv_fixture();
            let mut backend = RecordingBackend::new();
            uv_copy(
                &mut state,
                &mut backend,
                opcode,
                src,
                UV_CHILD,
                UV_GC,
                src_xy,
                (0, 0),
                (30, 40),
            );
            assert_eq!(uv_copy_calls(&backend), 0, "op {opcode} src 0x{src:x}");
            assert!(
                !backend.calls().iter().any(|c| matches!(
                    c,
                    crate::backend::recording::RecordedCall::PaintWindowBackgroundRect { .. }
                )),
                "op {opcode} src 0x{src:x}: no background paint",
            );
            assert_eq!(
                uv_events(&read_all_available(&mut peer)),
                vec![(14, UV_CHILD, 0, 0, 0, 0, 0, opcode)],
                "op {opcode} src 0x{src:x} at {src_xy:?}: NoExpose",
            );
        }
    }
}

/// Drawing to an unmapped window records no damage (Xorg damage.c:197).
#[test]
fn poly_fill_on_unmapped_window_records_no_damage() {
    use crate::server::DamageObject;
    const DAMAGE_XID: u32 = 0x0010_0130;
    for (window, expect_damage) in [(UV_LONE, false), (UV_CHILD, false), (UV_SHOWN, true)] {
        let (mut state, _peer) = uv_fixture();
        let mut backend = RecordingBackend::new();
        state.damage_objects.insert(
            DAMAGE_XID,
            DamageObject {
                owner: ClientId(1),
                drawable: ResourceId(window),
                level: 3,
                rects: Vec::new(),
                pending_notify_fired: false,
                last_reported_geometry: None,
            },
        );
        let body = poly_fill_rectangle_body(window, UV_GC);
        process_request(
            &mut state,
            &mut backend,
            ClientId(1),
            SequenceNumber(1),
            RequestHeader {
                opcode: 70,
                data: 0,
                length_units: u32::try_from(1 + body.len() / 4).unwrap(),
            },
            &body,
            None,
        )
        .expect("PolyFillRectangle");
        let dmg = state
            .damage_objects
            .get(&DAMAGE_XID)
            .expect("damage object");
        assert_eq!(
            !dmg.rects.is_empty() || dmg.pending_notify_fired,
            expect_damage,
            "window 0x{window:x}: rects={:?} fired={}",
            dmg.rects,
            dmg.pending_notify_fired,
        );
    }
}

/// Stage 4d Manual-redirect CopyArea ClipByChildren fix.
///
/// Scenario (matches marco's failing #6886 / #7450 CopyArea on
/// the MATE Control Center frame):
/// - Frame window 997×652 at root, mapped, with a redirected
///   backing pixmap allocated.
/// - CC client window 975×600 reparented under the frame at
///   (11, 41), mapped.
/// - X11 CopyArea(src=decoration_pixmap, dst=frame_window,
///   gc=default) 997×652.
///
/// Pre-fix: dispatch resolves `dst` to the backing pixmap and
/// hands the backend a single full-extent copy, which clobbers
/// the area where CC's content lives in the backing.
///
/// Spec-correct (Xorg `mi/midispcur.c` + the X11 GC
/// `subwindow-mode` default): under `ClipByChildren`, subtract
/// every mapped child's geometry before copying. Result is the
/// four border strips (top/bottom/left/right of CC).
#[test]
fn copy_area_into_window_with_mapped_child_excludes_child_area() {
    use crate::backend::recording::RecordedCall;
    use yserver_protocol::x11::{CreateGcRequest, CreateWindowRequest};

    const FRAME_XID: u32 = 0x0010_0001;
    const FRAME_HOST: u32 = 0x0040_0001;
    const FRAME_BACKING_HOST: u32 = 0x0050_0001;
    const CHILD_XID: u32 = 0x0010_0002;
    const CHILD_HOST: u32 = 0x0040_0002;
    const SRC_PIXMAP_XID: u32 = 0x0010_0010;
    const SRC_PIXMAP_HOST: u32 = 0x0040_0010;
    const GC_XID: u32 = 0x0010_0020;

    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();

    // Frame: top-level under root, viewable, redirected (backing
    // present). Mirrors the post-Reparent + post-Map + post-
    // activate_redirect_backing state.
    state.resources.create_window(
        yserver_protocol::x11::ClientId(1),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(FRAME_XID),
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 997,
            height: 652,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    {
        let w = state
            .resources
            .window_mut(ResourceId(FRAME_XID))
            .expect("frame");
        w.host_xid = Some(crate::backend::WindowHandle::from_raw_for_test(FRAME_HOST));
        w.redirected_backing = Some(crate::resources::RedirectedBacking {
            host_pixmap: crate::backend::PixmapHandle::from_raw_for_test(FRAME_BACKING_HOST),
            width: 997,
            height: 652,
            depth: 24,
        });
        w.map_state = crate::resources::MapState::Viewable;
    }
    // CC: child of frame, viewable, at (11, 41) sized 975×600.
    state.resources.create_window(
        yserver_protocol::x11::ClientId(1),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(CHILD_XID),
            parent: ResourceId(FRAME_XID),
            x: 11,
            y: 41,
            width: 975,
            height: 600,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    {
        let w = state
            .resources
            .window_mut(ResourceId(CHILD_XID))
            .expect("child");
        w.host_xid = Some(crate::backend::WindowHandle::from_raw_for_test(CHILD_HOST));
        w.map_state = crate::resources::MapState::Viewable;
    }
    // Source pixmap (depth-24, same depth as frame).
    state.resources.create_pixmap(
        ClientId(1),
        yserver_protocol::x11::CreatePixmapRequest {
            pixmap: ResourceId(SRC_PIXMAP_XID),
            drawable: ROOT_WINDOW,
            width: 997,
            height: 652,
            depth: 24,
        },
    );
    let _ = state.resources.set_pixmap_host_xid(
        ResourceId(SRC_PIXMAP_XID),
        crate::backend::PixmapHandle::from_raw_for_test(SRC_PIXMAP_HOST),
    );
    // GC for client 1. Every value-mask field is None → defaults
    // apply, including subwindow-mode = ClipByChildren.
    state.resources.create_gc(
        ClientId(1),
        CreateGcRequest {
            gc: ResourceId(GC_XID),
            drawable: ResourceId(FRAME_XID),
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

    // Build CopyArea body: src(4) dst(4) gc(4) src_x(2) src_y(2)
    //                      dst_x(2) dst_y(2) width(2) height(2).
    let mut body = Vec::with_capacity(24);
    body.extend_from_slice(&SRC_PIXMAP_XID.to_le_bytes());
    body.extend_from_slice(&FRAME_XID.to_le_bytes());
    body.extend_from_slice(&GC_XID.to_le_bytes());
    body.extend_from_slice(&0_i16.to_le_bytes()); // src_x
    body.extend_from_slice(&0_i16.to_le_bytes()); // src_y
    body.extend_from_slice(&0_i16.to_le_bytes()); // dst_x
    body.extend_from_slice(&0_i16.to_le_bytes()); // dst_y
    body.extend_from_slice(&997_u16.to_le_bytes()); // width
    body.extend_from_slice(&652_u16.to_le_bytes()); // height

    process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        RequestHeader {
            opcode: 62, // CopyArea major opcode
            data: 0,
            length_units: 7, // 1 header + 6 body words = 7 (4-byte units)
        },
        &body,
        None,
    )
    .expect("dispatch CopyArea");

    let copy_calls: Vec<_> = backend
        .calls()
        .into_iter()
        .filter_map(|c| match c {
            RecordedCall::CopyArea {
                src_host_xid,
                dst_host_xid,
                src_x,
                src_y,
                dst_x,
                dst_y,
                width,
                height,
            } => Some((
                src_host_xid,
                dst_host_xid,
                src_x,
                src_y,
                dst_x,
                dst_y,
                width,
                height,
            )),
            _ => None,
        })
        .collect();

    // Pre-fix: a single 997×652 CopyArea covering CC's area.
    // Post-fix: four border strips that miss the (11, 41, 975, 600)
    // child rect entirely.
    assert_eq!(
        copy_calls.len(),
        4,
        "expected 4 border strips for ClipByChildren around the \
             mapped child; pre-fix this is 1 (the full-extent copy \
             that clobbers CC's area in the backing); got {copy_calls:?}",
    );

    // No copy may overlap the child rect (11, 41, 975, 600).
    let child = (11_i32, 41_i32, 11_i32 + 975, 41_i32 + 600);
    for (_, dst_host, _, _, dx, dy, w, h) in &copy_calls {
        let r = (
            i32::from(*dx),
            i32::from(*dy),
            i32::from(*dx) + i32::from(*w),
            i32::from(*dy) + i32::from(*h),
        );
        let overlaps = r.0 < child.2 && r.2 > child.0 && r.1 < child.3 && r.3 > child.1;
        assert!(
            !overlaps,
            "copy strip ({dx},{dy} {w}x{h}) overlaps the child rect (11,41 975x600); \
                 ClipByChildren must exclude mapped children",
        );
        // Preserve window identity for backend border translation.
        assert_eq!(
            *dst_host, FRAME_HOST,
            "ClipByChildren strips stay in window-local coordinates",
        );
    }
}

/// InputOnly children do not contribute visible pixels and must
/// not be subtracted by ClipByChildren.
#[test]
fn copy_area_clip_by_children_ignores_input_only_child() {
    use crate::resources::{MapState, WindowClass};
    use yserver_protocol::x11::{CreateWindowRequest, ResourceId};

    let mut state = ServerState::new();
    state.resources.create_window(
        yserver_protocol::x11::ClientId(1),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(0x0020_0001),
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 100,
            height: 80,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    state.resources.create_window(
        yserver_protocol::x11::ClientId(1),
        CreateWindowRequest {
            depth: 0,
            window: ResourceId(0x0020_0002),
            parent: ResourceId(0x0020_0001),
            x: 10,
            y: 10,
            width: 20,
            height: 20,
            border_width: 0,
            class: 2,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    {
        let child = state
            .resources
            .window_mut(ResourceId(0x0020_0002))
            .expect("child");
        child.map_state = MapState::Viewable;
        assert_eq!(child.class, WindowClass::InputOnly);
    }

    let rect = yserver_protocol::x11::CopyAreaRequest {
        src: ResourceId(0x1),
        dst: ResourceId(0x0020_0001),
        gc: ResourceId(0x1),
        src_x: 0,
        src_y: 0,
        dst_x: 0,
        dst_y: 0,
        width: 100,
        height: 80,
    };
    let draw_state = crate::backend::DrawState {
        subwindow_mode: crate::backend::SubwindowMode::ClipByChildren,
        ..Default::default()
    };

    let got = copy_area_effective_dst_rects(&state, ResourceId(0x0020_0001), &draw_state, &rect);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].x, 0);
    assert_eq!(got[0].y, 0);
    assert_eq!(got[0].width, 100);
    assert_eq!(got[0].height, 80);
}

/// A bounding-shaped child takes only its shape out of its parent
/// under ClipByChildren (Xorg subtracts its `borderSize`,
/// `dix/window.c:1747-1770`). xfce4-settings-manager's socket S at
/// (8,8) 730x531 is shaped to the 450 rows of its viewport, and the
/// manager repaints its button bar under S's rect but outside its
/// shape with `CopyArea(8,464 730x36)`; measured on Xorg 21.1 by
/// tools/vng-scenarios/xembed-scroll-probe.c, the bar is drawn.
#[test]
fn copy_area_clip_by_children_takes_out_only_a_shaped_childs_shape() {
    use crate::resources::MapState;
    use yserver_protocol::x11::{CreateWindowRequest, ResourceId, shape as x11shape};

    let mut state = ServerState::new();
    let (c, s) = (ResourceId(0x0020_0101), ResourceId(0x0020_0102));
    for (window, parent, x, y, width, height) in
        [(c, ROOT_WINDOW, 0, 0, 746, 500), (s, c, 8, 8, 730, 531)]
    {
        state.resources.create_window(
            yserver_protocol::x11::ClientId(1),
            CreateWindowRequest {
                depth: 24,
                window,
                parent,
                x,
                y,
                width,
                height,
                border_width: 0,
                class: 1,
                visual: crate::resources::ROOT_VISUAL,
                ..Default::default()
            },
        );
    }
    state.resources.window_mut(s).expect("socket").map_state = MapState::Viewable;
    crate::nested::set_shape_rects(
        &mut state,
        s,
        x11shape::KIND_BOUNDING,
        vec![yserver_protocol::x11::xfixes::RegionRect {
            x: 0,
            y: 0,
            width: 730,
            height: 450,
        }],
    );
    let request = yserver_protocol::x11::CopyAreaRequest {
        src: ResourceId(0x1),
        dst: c,
        gc: ResourceId(0x1),
        src_x: 0,
        src_y: 0,
        dst_x: 8,
        dst_y: 464,
        width: 730,
        height: 36,
    };
    let draw_state = crate::backend::DrawState {
        subwindow_mode: crate::backend::SubwindowMode::ClipByChildren,
        ..Default::default()
    };
    let got = copy_area_effective_dst_rects(&state, c, &draw_state, &request);
    assert_eq!(got.len(), 1);
    assert_eq!(
        (got[0].x, got[0].y, got[0].width, got[0].height),
        (8, 464, 730, 36)
    );
}

/// MANUALLY-redirected children must not be subtracted by
/// ClipByChildren. They don't claim the parent's pixmap real
/// estate — the redirecting compositor (which may be the
/// parent's own client) puts the children's pixels there
/// itself via subsequent ops. Subtracting them strips the
/// compositor's own composite-target rect to empty.
///
/// Live trigger: mate-panel notification-area-applet, which
/// calls `RedirectWindow(socket, Manual)` on each tray slot and
/// then `CopyArea` from each embedded applet's redirected
/// pixmap into its own visible top-level. Pre-fix every such
/// CopyArea is "fully clipped, no backend call" because the
/// manually-redirected socket children fully overlap the
/// destination — so the tray icons never make it into the
/// notification-area-applet's own backing, and mate-compositor
/// (which reads that backing via NameWindowPixmap) shows the
/// panel without icons.
#[test]
fn copy_area_clip_by_children_ignores_manually_redirected_child() {
    use crate::{
        resources::{MapState, WindowClass},
        server::{CompositeRedirectMode, RedirectRecord},
    };
    use yserver_protocol::x11::{ClientId, CreateWindowRequest, ResourceId};

    let mut state = ServerState::new();
    let parent_xid = ResourceId(0x0021_0003);
    let child_xid = ResourceId(0x0021_0013);

    state.resources.create_window(
        ClientId(1),
        CreateWindowRequest {
            depth: 24,
            window: parent_xid,
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 100,
            height: 80,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    state.resources.create_window(
        ClientId(1),
        CreateWindowRequest {
            depth: 24,
            window: child_xid,
            parent: parent_xid,
            x: 0,
            y: 0,
            width: 100,
            height: 80,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    {
        let child = state.resources.window_mut(child_xid).expect("child");
        child.map_state = MapState::Viewable;
        assert_eq!(child.class, WindowClass::InputOutput);
    }
    // RedirectWindow(child, Manual). Key shape is
    // `(window, subwindows=false)` for per-window
    // RedirectWindow (vs `(parent, true)` for the inherited
    // RedirectSubwindows form).
    state
        .composite_redirects
        .redirect_window(
            child_xid,
            RedirectRecord {
                mode: CompositeRedirectMode::Manual,
                owner: ClientId(1),
            },
        )
        .unwrap();

    let rect = yserver_protocol::x11::CopyAreaRequest {
        src: ResourceId(0x1),
        dst: parent_xid,
        gc: ResourceId(0x1),
        src_x: 0,
        src_y: 0,
        dst_x: 0,
        dst_y: 0,
        width: 100,
        height: 80,
    };
    let draw_state = crate::backend::DrawState {
        subwindow_mode: crate::backend::SubwindowMode::ClipByChildren,
        ..Default::default()
    };

    let got = copy_area_effective_dst_rects(&state, parent_xid, &draw_state, &rect);
    assert_eq!(
        got.len(),
        1,
        "manual-redirected child must not be subtracted; got {got:?}"
    );
    assert_eq!(got[0].x, 0);
    assert_eq!(got[0].y, 0);
    assert_eq!(got[0].width, 100);
    assert_eq!(got[0].height, 80);
}

/// Regression guard: an AUTOMATIC-redirected child *must*
/// still be subtracted by ClipByChildren. With Automatic
/// redirect the X server auto-composites the child's backing
/// into the parent's pixmap; the parent's own paint then
/// clipping out the child's area is the correct shape (the
/// child's pixels arrive via the auto-composite, not via the
/// parent's paint). The manual-only exception above must not
/// loosen this case.
#[test]
fn copy_area_clip_by_children_still_subtracts_automatic_redirected_child() {
    use crate::{
        resources::{MapState, WindowClass},
        server::{CompositeRedirectMode, RedirectRecord},
    };
    use yserver_protocol::x11::{ClientId, CreateWindowRequest, ResourceId};

    let mut state = ServerState::new();
    let parent_xid = ResourceId(0x0022_0001);
    let child_xid = ResourceId(0x0022_0002);

    state.resources.create_window(
        ClientId(1),
        CreateWindowRequest {
            depth: 24,
            window: parent_xid,
            parent: ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 100,
            height: 80,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    state.resources.create_window(
        ClientId(1),
        CreateWindowRequest {
            depth: 24,
            window: child_xid,
            parent: parent_xid,
            x: 10,
            y: 10,
            width: 50,
            height: 50,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    {
        let child = state.resources.window_mut(child_xid).expect("child");
        child.map_state = MapState::Viewable;
        assert_eq!(child.class, WindowClass::InputOutput);
    }
    state
        .composite_redirects
        .redirect_window(
            child_xid,
            RedirectRecord {
                mode: CompositeRedirectMode::Automatic,
                owner: ClientId(1),
            },
        )
        .unwrap();

    let rect = yserver_protocol::x11::CopyAreaRequest {
        src: ResourceId(0x1),
        dst: parent_xid,
        gc: ResourceId(0x1),
        src_x: 0,
        src_y: 0,
        dst_x: 0,
        dst_y: 0,
        width: 100,
        height: 80,
    };
    let draw_state = crate::backend::DrawState {
        subwindow_mode: crate::backend::SubwindowMode::ClipByChildren,
        ..Default::default()
    };

    let got = copy_area_effective_dst_rects(&state, parent_xid, &draw_state, &rect);
    // Automatic-redirected child is still subtracted, so we
    // expect the original 100×80 rect to be split into four
    // border strips around the (10,10 50x50) child rect.
    assert!(
        got.len() >= 2,
        "automatic-redirected child must still be subtracted; expected > 1 \
             strip, got {got:?}"
    );
    let child = (10_i32, 10_i32, 60_i32, 60_i32);
    for r in &got {
        let rr = (
            i32::from(r.x),
            i32::from(r.y),
            i32::from(r.x) + i32::from(r.width),
            i32::from(r.y) + i32::from(r.height),
        );
        let overlaps = rr.0 < child.2 && rr.2 > child.0 && rr.1 < child.3 && rr.3 > child.1;
        assert!(
            !overlaps,
            "strip ({},{} {}x{}) overlaps automatic-redirected child (10,10 50x50)",
            r.x, r.y, r.width, r.height,
        );
    }
}
