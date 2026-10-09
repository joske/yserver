use super::*;

/// #133 step 8 (P9) — protocol-observable proof that a client is told
/// the CONTENT-relative coordinate of a press on its border, negative
/// on the left and top.
///
/// Unit tests over `root_pointer_target_at` pin the hit test, but the
/// coordinate a client actually reads comes off the wire after the
/// propagation walk and the INT16 encoder. The step 3 lesson (a green
/// Backend-trait suite next to a red xts) says assert the wire.
///
/// Fixture: one root child at (100, 200), 300x400, `border_width = 16`
/// — awesome's configured width. Content origin (116, 216)
/// (`dix/window.c:888`), outer box x [100, 432) x y [200, 632).
/// Press at each of the four border midpoints and the four corners, and
/// read `event_x` / `event_y` out of the 32-byte ButtonPress
/// (bytes 24..28, signed INT16 — Xorg reports `x - pWin->drawable.x`,
/// `dix/window.c:2995`, and never clamps it at 0).
#[test]
#[ignore = "needs live Vulkan ICD"]
fn border_press_reports_negative_content_coords_on_the_wire() {
    use std::io::Read as _;
    use yserver_core::host_x11::{HostPointerEvent, PointerEventKind};

    const WID: u32 = 0x0060_0300;
    const BW: u16 = 16;
    const WX: i16 = 100;
    const WY: i16 = 200;
    const W: u16 = 300;
    const H: u16 = 400;
    // Event mask bit for ButtonPress (X11 §Events).
    const BUTTON_PRESS_MASK: u32 = 0x0000_0004;

    let Some(mut f) = ProtoFixture::new() else {
        eprintln!("skipping: no Vk");
        return;
    };
    let root_res = yserver_core::resources::ROOT_WINDOW.0;

    // CreateWindow(..., border_width = 16, CWBackPixel | CWEventMask).
    let mut body = Vec::new();
    body.extend_from_slice(&WID.to_le_bytes());
    body.extend_from_slice(&root_res.to_le_bytes());
    body.extend_from_slice(&WX.to_le_bytes());
    body.extend_from_slice(&WY.to_le_bytes());
    body.extend_from_slice(&W.to_le_bytes());
    body.extend_from_slice(&H.to_le_bytes());
    body.extend_from_slice(&BW.to_le_bytes());
    body.extend_from_slice(&1u16.to_le_bytes()); // class InputOutput
    body.extend_from_slice(&0u32.to_le_bytes()); // visual CopyFromParent
    // CWBackPixel (0x02) | CWEventMask (0x0800), value order = bit order.
    body.extend_from_slice(&0x0000_0802u32.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes()); // background pixel
    body.extend_from_slice(&BUTTON_PRESS_MASK.to_le_bytes());
    f.req(1, 0, &body);
    f.req(8, 0, &WID.to_le_bytes()); // MapWindow

    // Drain whatever the map produced so the reads below see only
    // presses. The socketpair is blocking, so switch to non-blocking
    // and read until empty.
    f._peer
        .set_nonblocking(true)
        .expect("nonblocking socketpair");
    let mut sink = [0u8; 4096];
    while f._peer.read(&mut sink).map(|n| n > 0).unwrap_or(false) {}

    // (name, root point, expected wire event_x/event_y). Every value is
    // `root - content_origin`; the left/top probes are negative.
    let probes: [(&str, i16, i16, i16, i16); 8] = [
        ("left edge", 100, 416, -16, 200),
        ("top edge", 266, 200, 150, -16),
        ("right edge", 431, 416, 315, 200),
        ("bottom edge", 266, 631, 150, 415),
        ("top-left corner", 100, 200, -16, -16),
        ("top-right corner", 431, 200, 315, -16),
        ("bottom-left corner", 100, 631, -16, 415),
        ("bottom-right corner", 431, 631, 315, 415),
    ];

    let xid_map = yserver_core::host_x11::HostXidMap::new();
    for (i, &(name, rx, ry, want_x, want_y)) in probes.iter().enumerate() {
        // `host_xid` is deliberately absent from `xid_map` so
        // `resolve_pointer_hit` takes the live-root path and the hit is
        // resolved from `root_x`/`root_y` alone.
        let press = HostPointerEvent {
            origin: yserver_core::core_loop::InputOrigin::XTest(4),
            kind: PointerEventKind::ButtonPress,
            host_xid: 0,
            detail: 1,
            time: 1000 + i as u32,
            root_x: rx,
            root_y: ry,
            event_x: rx,
            event_y: ry,
            state: 0,
            crossing_mode: 0,
            child: 0,
            raw_dx: 0,
            raw_dy: 0,
            tree_change: false,
        };
        yserver_core::core_loop::pointer_fanout::pointer_event_fanout_to_state(
            &mut f.state,
            &mut f.backend,
            &xid_map,
            press,
            /*handle_grabs=*/ false,
            /*is_replay=*/ false,
        );

        let mut buf = [0u8; 4096];
        let n = f._peer.read(&mut buf).unwrap_or(0);
        assert!(n >= 32, "{name}: no event delivered ({n} bytes)");
        // Find the ButtonPress (event code 4) among what was written.
        let press_at = (0..n / 32)
            .map(|k| k * 32)
            .find(|&off| buf[off] & 0x7f == 4)
            .unwrap_or_else(|| panic!("{name}: no ButtonPress in {n} bytes"));
        let ev = &buf[press_at..press_at + 32];
        let event_win = u32::from_le_bytes([ev[12], ev[13], ev[14], ev[15]]);
        let event_x = i16::from_le_bytes([ev[24], ev[25]]);
        let event_y = i16::from_le_bytes([ev[26], ev[27]]);
        assert_eq!(event_win, WID, "{name}: press must land on the window");
        assert_eq!(
            (event_x, event_y),
            (want_x, want_y),
            "{name}: root ({rx},{ry}) must be reported as content ({want_x},{want_y})"
        );
        // Release before the next probe: XTEST 4 keeps its own button
        // state, and a press of a button it already holds is dropped
        // (Xorg `UpdateDeviceState`, Xi/exevents.c:948).
        yserver_core::core_loop::pointer_fanout::pointer_event_fanout_to_state(
            &mut f.state,
            &mut f.backend,
            &xid_map,
            HostPointerEvent {
                kind: PointerEventKind::ButtonRelease,
                ..press
            },
            /*handle_grabs=*/ false,
            /*is_replay=*/ false,
        );
        // Drain any trailing events (crossings) before the next probe.
        while f._peer.read(&mut sink).map(|k| k > 0).unwrap_or(false) {}
    }
}

/// The mechanism behind all 14, in isolation and independent of the
/// drawing op: a `border_width` change must not move the client's
/// pixels in the client's own coordinates.
///
/// A window's storage is the bordered extent with the content `bw`
/// inside it (`compAllocPixmap`, `composite/compalloc.c:610`).
/// Re-basing that origin off the new `border_width` without moving the
/// pixels displaces everything already drawn by the delta (step 3's
/// invariant). Step 6 (P8) re-bases it and reallocates — 42x22 → 40x20
/// here — but migrates the content in the same step, so this reads the
/// same either way: the window's pixels, at the window's own
/// coordinates, must be identical across the change.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn border_width_change_does_not_displace_existing_content() {
    const WID: u32 = 0x0061_0001;
    const GC: u32 = 0x0061_0002;
    const W: u16 = 40;
    const H: u16 = 20;

    let Some(mut f) = ProtoFixture::new() else {
        eprintln!("skipping: no Vk");
        return;
    };
    let root_res = yserver_core::resources::ROOT_WINDOW.0;

    // A window with border_width = 1, like every xts test window.
    let mut body = Vec::new();
    body.extend_from_slice(&WID.to_le_bytes());
    body.extend_from_slice(&root_res.to_le_bytes());
    body.extend_from_slice(&10i16.to_le_bytes());
    body.extend_from_slice(&5i16.to_le_bytes());
    body.extend_from_slice(&W.to_le_bytes());
    body.extend_from_slice(&H.to_le_bytes());
    body.extend_from_slice(&1u16.to_le_bytes());
    body.extend_from_slice(&1u16.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&0x0000_0002u32.to_le_bytes()); // CWBackPixel
    body.extend_from_slice(&0u32.to_le_bytes());
    f.req(1, 0, &body);
    f.req(8, 0, &WID.to_le_bytes());
    let mut body = Vec::new();
    body.extend_from_slice(&GC.to_le_bytes());
    body.extend_from_slice(&WID.to_le_bytes());
    body.extend_from_slice(&0x0000_0004u32.to_le_bytes());
    body.extend_from_slice(&1u32.to_le_bytes()); // foreground = W_FG
    f.req(55, 0, &body);
    // An asymmetric mark, so any displacement shows up.
    let mut body = Vec::new();
    body.extend_from_slice(&WID.to_le_bytes());
    body.extend_from_slice(&GC.to_le_bytes());
    body.extend_from_slice(&3i16.to_le_bytes());
    body.extend_from_slice(&4i16.to_le_bytes());
    body.extend_from_slice(&5u16.to_le_bytes());
    body.extend_from_slice(&6u16.to_le_bytes());
    f.req(70, 0, &body);

    let host_wid = f
        .state
        .resources
        .window(yserver_protocol::x11::ResourceId(WID))
        .and_then(|w| w.host_xid)
        .expect("host xid")
        .as_raw();
    let before = f
        .backend
        .get_image_pixels_for_tests(host_wid, 2, 0, 0, W, H, !0)
        .expect("get_image")
        .expect("Some bytes");

    assert_eq!(
        f.backend.storage_extent_for_tests(host_wid),
        Some((u32::from(W) + 2, u32::from(H) + 2)),
        "baseline: storage is the bordered extent at bw = 1",
    );

    // XSetWindowBorderWidth(w, 0) — step 6 reallocates at the new
    // bordered extent and migrates the content from offset 1 to 0.
    let mut body = Vec::new();
    body.extend_from_slice(&WID.to_le_bytes());
    body.extend_from_slice(&0x0010u16.to_le_bytes()); // CWBorderWidth
    body.extend_from_slice(&0u16.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    f.req(12, 0, &body);

    assert_eq!(
        f.backend.storage_extent_for_tests(host_wid),
        Some((u32::from(W), u32::from(H))),
        "step 6 (6.1): the bordered extent moved, so the storage did too",
    );
    let after = f
        .backend
        .get_image_pixels_for_tests(host_wid, 2, 0, 0, W, H, !0)
        .expect("get_image")
        .expect("Some bytes");
    for y in 0..u32::from(H) {
        for x in 0..u32::from(W) {
            let off = ((y * u32::from(W) + x) * 4) as usize;
            assert_eq!(
                &after[off..off + 4],
                &before[off..off + 4],
                "pixel ({x}, {y}) moved when border_width changed — the \
                 content layout must follow the ALLOCATION, and step 6 must \
                 migrate the pixels when it moves that layout",
            );
        }
    }
}

// ───── #133 step 6 (P8) — border-width change ─────
//
// The reproduction these two model, measured on awesome at
// `border_width = 16`: every FLOATING terminal had a 1 px border and
// `content_offset = 1`, every TILED one a correct 16 px border and
// `content_offset = 16`. awesome creates the frame, sets the border
// width, and only the tiled layout then resizes it — and a resize was
// the only thing that reallocated the storage, so only the tiled frames
// picked up the new layout.
//
// Both tests read the WHOLE backing through the privileged route: the
// ring is not part of the window drawable, so no client route can
// observe it (step 3), and "the content is where it was" and "the ring
// is really painted" are two halves of the same assertion.

/// A distinctive border pixel — not `W_BG` and not `W_FG`, so a stale
/// content pixel inside the new ring is a visible mismatch rather than
/// an accidental match.
const P8_BORDER: u32 = 0x00_00_FF_00;

/// #133 step 6 (6.1 + 6.2) — `XSetWindowBorderWidth` on a window
/// created with `border_width = 0`: the awesome reproduction, and the
/// request order six xts5 `Xlib4` purposes actually use (`crechild`
/// creates with `bw = 0`, `xts5/src/lib/crechild.c:189`).
///
/// The bordered extent moves, so the storage is reallocated; the
/// content offset moves with it, so the client's pixels are copied
/// forward from the retained old storage — Xorg's `cw->pOldPixmap`
/// recovery (`composite/compalloc.c:676-702`,
/// `composite/compwindow.c:501-540`) — and the ring is painted after
/// the copy, around the content rather than over it.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn border_width_set_after_create_reallocates_migrates_and_paints_the_ring() {
    const WID: u32 = 0x0067_0001;
    const GC: u32 = 0x0067_0002;
    const W: u16 = 40;
    const H: u16 = 20;
    const BW: u16 = 5;
    // An asymmetric mark, so any displacement shows up.
    const MX: u16 = 3;
    const MY: u16 = 4;
    const MW: u16 = 5;
    const MH: u16 = 6;

    let Some(mut f) = ProtoFixture::new() else {
        eprintln!("skipping: no Vk");
        return;
    };
    let root = yserver_core::resources::ROOT_WINDOW.0;

    // CWBackPixel | CWBorderPixel, values in ascending mask-bit order.
    or_create_window(
        &mut f,
        WID,
        root,
        0,
        10,
        5,
        W,
        H,
        0,
        0,
        0x0000_000a,
        &[OR_W_BG, P8_BORDER],
    );
    f.req(8, 0, &WID.to_le_bytes());
    or_create_gc(&mut f, GC, WID, OR_W_FG);
    or_fill(&mut f, WID, GC, MX as i16, MY as i16, MW, MH);

    let host = f.host_xid(WID);
    assert_eq!(
        f.backend.storage_extent_for_tests(host),
        Some((u32::from(W), u32::from(H))),
        "baseline: an unbordered window's storage IS its content",
    );
    let before = f
        .backend
        .get_image_pixels_for_tests(host, 2, 0, 0, W, H, !0)
        .expect("get_image")
        .expect("Some bytes");

    // XSetWindowBorderWidth(w, 5).
    let mut body = Vec::new();
    body.extend_from_slice(&WID.to_le_bytes());
    body.extend_from_slice(&0x0010u16.to_le_bytes()); // CWBorderWidth
    body.extend_from_slice(&0u16.to_le_bytes());
    body.extend_from_slice(&u32::from(BW).to_le_bytes());
    f.req(12, 0, &body);

    // 6.1 — reallocated at the new bordered extent, with the content
    // offset recorded as the new border width.
    assert_eq!(
        f.backend.storage_extent_for_tests(host),
        Some((
            u32::from(W) + 2 * u32::from(BW),
            u32::from(H) + 2 * u32::from(BW)
        )),
        "the bordered extent must follow the new border width",
    );
    assert_eq!(
        f.backend.paint_target_shape_for_tests(host),
        Some((
            (i32::from(BW), i32::from(BW)),
            Some((i32::from(BW), i32::from(BW), u32::from(W), u32::from(H))),
            true
        )),
        "the content origin is (bw, bw) in the new allocation",
    );

    // 6.2 — the client's content, in the client's own coordinates, is
    // exactly what it was.
    let after = f
        .backend
        .get_image_pixels_for_tests(host, 2, 0, 0, W, H, !0)
        .expect("get_image")
        .expect("Some bytes");
    assert_eq!(
        after, before,
        "the content must be migrated, not erased and not displaced",
    );

    // …and the ring is painted around it, with no content pixel left in
    // it and no content pixel overpainted.
    or_for_each_backing_pixel(&mut f, WID, BW, W, H, |x, y, inside, bgr| {
        let cx = x - i32::from(BW);
        let cy = y - i32::from(BW);
        let marked = cx >= i32::from(MX)
            && cy >= i32::from(MY)
            && cx < i32::from(MX + MW)
            && cy < i32::from(MY + MH);
        let want = if !inside {
            P8_BORDER
        } else if marked {
            OR_W_FG
        } else {
            OR_W_BG
        };
        assert_eq!(
            bgr,
            or_bgr(want),
            "backing ({x}, {y}) (inside={inside}, marked={marked})",
        );
    });
}

/// #133 step 6 (6.2) — the case the spec says prose alone would let an
/// implementer skip: `w=100,bw=2 → w=98,bw=3` in ONE ConfigureWindow.
///
/// The outer extent is 104 either way, so nothing is reallocated —
/// Xorg's `compReallocPixmap` takes its `else` branch and keeps the
/// pixmap (`composite/compalloc.c:706`) — and the content still has to
/// move from storage offset 2 to offset 3. Reinterpreting the bytes
/// makes the new content sample the old ring along its leading edge and
/// leaves the old content's trailing pixels inside the new ring; both
/// are caught by the whole-backing sweep below.
///
/// The window keeps the default ForgetGravity, and a size change is a
/// resize: Xorg exposes the whole new clip list (`mi/miwindow.c:466-472`,
/// nothing recovered without a bit gravity) and paints it with the
/// background (`miWindowExposures`), so the marks do not survive and the
/// content reads background. The migration of surviving content is pinned
/// by the border-only change above, which Xorg handles as a move.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn border_change_with_unchanged_outer_extent_migrates_the_content() {
    const WID: u32 = 0x0068_0001;
    const GC: u32 = 0x0068_0002;
    const W0: u16 = 100;
    const H0: u16 = 60;
    const BW0: u16 = 2;
    const W1: u16 = 98;
    const H1: u16 = 58;
    const BW1: u16 = 3;
    /// Single-pixel marks in CONTENT coordinates, including both
    /// extreme corners of the SURVIVING content, which is what pins the
    /// mapping to the pixel.
    const MARKS: [(u16, u16); 4] = [(0, 0), (97, 0), (0, 57), (97, 57)];

    let Some(mut f) = ProtoFixture::new() else {
        eprintln!("skipping: no Vk");
        return;
    };
    let root = yserver_core::resources::ROOT_WINDOW.0;

    or_create_window(
        &mut f,
        WID,
        root,
        0,
        10,
        5,
        W0,
        H0,
        BW0,
        0,
        0x0000_000a,
        &[OR_W_BG, P8_BORDER],
    );
    f.req(8, 0, &WID.to_le_bytes());
    or_create_gc(&mut f, GC, WID, OR_W_FG);
    for (mx, my) in MARKS {
        or_fill(&mut f, WID, GC, mx as i16, my as i16, 1, 1);
    }

    let host = f.host_xid(WID);
    let outer = (
        u32::from(W0) + 2 * u32::from(BW0),
        u32::from(H0) + 2 * u32::from(BW0),
    );
    assert_eq!(f.backend.storage_extent_for_tests(host), Some(outer));

    // ConfigureWindow(CWWidth | CWHeight | CWBorderWidth) — one request,
    // as `configure_window` forwards it to the backend in one call.
    let mut body = Vec::new();
    body.extend_from_slice(&WID.to_le_bytes());
    body.extend_from_slice(&0x001Cu16.to_le_bytes()); // CWWidth|CWHeight|CWBorderWidth
    body.extend_from_slice(&0u16.to_le_bytes());
    body.extend_from_slice(&u32::from(W1).to_le_bytes());
    body.extend_from_slice(&u32::from(H1).to_le_bytes());
    body.extend_from_slice(&u32::from(BW1).to_le_bytes());
    f.req(12, 0, &body);

    // 6.1 — no reallocation: 98 + 6 == 100 + 4.
    assert_eq!(
        f.backend.storage_extent_for_tests(host),
        Some(outer),
        "the outer extent is unchanged, so the storage must be reused",
    );
    // 6.2 — …and the content offset moved anyway.
    assert_eq!(
        f.backend.paint_target_shape_for_tests(host),
        Some((
            (i32::from(BW1), i32::from(BW1)),
            Some((i32::from(BW1), i32::from(BW1), u32::from(W1), u32::from(H1))),
            true
        )),
        "the content origin must follow the new border width",
    );

    // Every pixel of the storage: background over the whole content,
    // marks included, and the border pixel everywhere in the new ring.
    or_for_each_backing_pixel(&mut f, WID, BW1, W1, H1, |x, y, inside, bgr| {
        let cx = x - i32::from(BW1);
        let cy = y - i32::from(BW1);
        let marked = MARKS
            .iter()
            .any(|(mx, my)| cx == i32::from(*mx) && cy == i32::from(*my));
        let want = if inside { OR_W_BG } else { P8_BORDER };
        assert_eq!(
            bgr,
            or_bgr(want),
            "backing ({x}, {y}) (inside={inside}, marked={marked}) — the resized \
             content is repainted with the background and no stale pixel is \
             left in the new ring",
        );
    });

    // …and back the other way, in one request again: the border shrinks
    // to 2 while the content grows to 100x60, outer extent still 104.
    // The relocation runs in the opposite direction (offset 3 → 2), and
    // the whole content is exposed again: it must read W_BG, including
    // the two columns and rows that held ring pixels a moment ago.
    let mut body = Vec::new();
    body.extend_from_slice(&WID.to_le_bytes());
    body.extend_from_slice(&0x001Cu16.to_le_bytes()); // CWWidth|CWHeight|CWBorderWidth
    body.extend_from_slice(&0u16.to_le_bytes());
    body.extend_from_slice(&u32::from(W0).to_le_bytes());
    body.extend_from_slice(&u32::from(H0).to_le_bytes());
    body.extend_from_slice(&u32::from(BW0).to_le_bytes());
    f.req(12, 0, &body);

    assert_eq!(
        f.backend.storage_extent_for_tests(host),
        Some(outer),
        "still no reallocation on the way back",
    );
    or_for_each_backing_pixel(&mut f, WID, BW0, W0, H0, |x, y, inside, bgr| {
        let cx = x - i32::from(BW0);
        let cy = y - i32::from(BW0);
        let marked = MARKS
            .iter()
            .any(|(mx, my)| cx == i32::from(*mx) && cy == i32::from(*my));
        let want = if inside { OR_W_BG } else { P8_BORDER };
        assert_eq!(
            bgr,
            or_bgr(want),
            "backing ({x}, {y}) (inside={inside}, marked={marked}) — the \
             round trip repaints the whole content with the background",
        );
    });
}

// ───── #133 step 4 (P5) — the ring fill, xts oracle replays ─────
//
// Step 4's oracle is seven pre-existing xts5 Xlib4 purposes that read
// painted border pixels. They are replayed here through `ProtoFixture`
// — the real dispatcher into a live `KmsBackend` — because the step-3
// lesson was that Backend-trait tests stay green while xts does not.
//
// TWO things the sources say that the plan's oracle table does not, both
// found by reading them (`/home/jos/Projects/xts/xts5/Xlib4/`):
//
// 1. Six of the seven give the window its border width AFTER creation,
//    via `XSetWindowBorderWidth` on a window `crechild` /
//    `creunmapchild` created with `border_width = 0`
//    (`xts5/src/lib/crechild.c:189`, `XCreateSimpleWindow(..., 0,
//    W_FG, W_BG)`). A border-width change that must REALLOCATE and
//    migrate the content is step 6 (plan 6.1-6.4), and until it does,
//    the allocation's content offset stays 0 — so there is no ring to
//    paint, by the step-3 invariant that the layout is a property of
//    the allocation. Only `XSetWindowBorderPixmap` purpose 3 creates
//    its window WITH a border (`mkwinchild(..., parent, 5)`).
//
//    The replays below therefore supply the border width at
//    `CreateWindow` and keep everything else from the purpose. That is
//    the whole of step 4's mechanism; the missing half is step 6's.
//
// 2. All seven then read the pixel with `getpixel(display, PARENT, ...)`
//    or `PIXCHECK(display, parent)` — an `XGetImage` on the window's
//    PARENT (`xts5/src/lib/checkpixel.c:145-159`). Xorg can answer that
//    because an unredirected window's pixels live in the screen pixmap,
//    which is also its parent's bounding drawable. yserver gives every
//    window its own storage and `get_image` on a non-root window reads
//    exactly that storage, so a parent read does not see any child
//    pixel — border or content. The ring is asserted here on the
//    window's OWN backing instead, which is the same pixel the purpose
//    is reaching for.

/// The ring geometry of the replay fixture: a 20x20 child at (50, 60)
/// inside its parent with `border_width = 5` — `ap` from
/// `XSetWindowBorder.c:284-287` and the `XSetWindowBorderWidth(w, 5)`
/// the purposes apply.
const OR_CX: i16 = 50;
const OR_CY: i16 = 60;
const OR_CW: u16 = 20;
const OR_CH: u16 = 20;
const OR_BW: u16 = 5;
/// xts `W_FG` / `W_BG` (`xts5/include/xtest.h:168-169`).
const OR_W_FG: u32 = 1;
const OR_W_BG: u32 = 0;

/// ChangeWindowAttributes.
fn or_cwa(f: &mut ProtoFixture, wid: u32, value_mask: u32, values: &[u32]) {
    let mut body = Vec::new();
    body.extend_from_slice(&wid.to_le_bytes());
    body.extend_from_slice(&value_mask.to_le_bytes());
    for v in values {
        body.extend_from_slice(&v.to_le_bytes());
    }
    f.req(2, 0, &body);
}

/// Walk a bordered window's whole backing, calling `f(x, y, inside,
/// bgr)` for every pixel — `inside` marks the client-visible content.
fn or_for_each_backing_pixel(
    f: &mut ProtoFixture,
    res: u32,
    bw: u16,
    cw: u16,
    ch: u16,
    mut visit: impl FnMut(i32, i32, bool, [u8; 3]),
) {
    let (sw, sh, bytes) = f.backing(res);
    assert_eq!(
        (sw, sh),
        (
            u32::from(cw) + 2 * u32::from(bw),
            u32::from(ch) + 2 * u32::from(bw)
        ),
        "storage must be the bordered extent",
    );
    let b = i32::from(bw);
    for y in 0..sh as i32 {
        for x in 0..sw as i32 {
            let off = ((y * sw as i32 + x) * 4) as usize;
            let inside = x >= b && y >= b && x < b + i32::from(cw) && y < b + i32::from(ch);
            visit(x, y, inside, [bytes[off], bytes[off + 1], bytes[off + 2]]);
        }
    }
}

/// xts5 `Xlib4/XSetWindowBorder` purposes 1 and 2, replayed: setting
/// the border pixel paints the ring, and changing it REPAINTS the ring
/// with no further request (purpose 2's "When the border pixel value is
/// changed, then the border is repainted", `XSetWindowBorder.c:390`).
///
/// This is the solid half of the oracle — the four purposes the plan
/// lists under "the solid fill" (`XSetWindowBorder` tp1-3,
/// `XSetWindowBorderWidth` tp1) all reduce to it.
///
/// Xorg paints here for exactly the same reason: `ChangeWindowAttributes`
/// itself calls `PaintWindow(pWin, borderClip − winSize, PW_BORDER)`
/// when `vmaskCopy & (CWBorderPixel | CWBorderPixmap)`
/// (`dix/window.c:1584-1591`).
#[test]
#[ignore = "needs live Vulkan ICD"]
fn xts_xlib4_xsetwindowborder_paints_and_repaints_the_ring() {
    const PARENT: u32 = 0x0062_0001;
    const CHILD: u32 = 0x0062_0002;

    let Some(mut f) = ProtoFixture::new() else {
        eprintln!("skipping: no Vk");
        return;
    };
    let root = yserver_core::resources::ROOT_WINDOW.0;

    // `defdraw` / `defwin`: a top-level with W_BG background.
    or_create_window(
        &mut f,
        PARENT,
        root,
        0,
        10,
        10,
        200,
        150,
        0,
        0,
        0x0000_0002,
        &[OR_W_BG],
    );
    f.req(8, 0, &PARENT.to_le_bytes());

    // `creunmapchild` + `XSetWindowBorderWidth(w, 5)`, folded into the
    // create (see the section comment: the post-create width change is
    // step 6). `XCreateSimpleWindow` supplies border = W_FG and
    // background = W_BG, i.e. CWBackPixel | CWBorderPixel = 0x0a with
    // the values in ascending mask-bit order.
    or_create_window(
        &mut f,
        CHILD,
        PARENT,
        0,
        OR_CX,
        OR_CY,
        OR_CW,
        OR_CH,
        OR_BW,
        0,
        0x0000_000a,
        &[OR_W_BG, OR_W_FG],
    );
    f.req(8, 0, &CHILD.to_le_bytes());

    // Purpose 1's first CHECK: the ring carries the border pixel, and
    // the content still carries the background.
    or_for_each_backing_pixel(&mut f, CHILD, OR_BW, OR_CW, OR_CH, |x, y, inside, got| {
        let want = if inside { OR_W_BG } else { OR_W_FG };
        assert_eq!(
            got,
            or_bgr(want),
            "after create: {} pixel ({x}, {y})",
            if inside { "content" } else { "RING" },
        );
    });

    // XSetWindowBorder(display, w, border_pixel) — CWBorderPixel only.
    // Purpose 2: no map/unmap, no ClearArea, nothing else; the ring
    // must be repainted by the attribute change alone.
    or_cwa(&mut f, CHILD, 0x0000_0008, &[OR_W_BG]);
    or_for_each_backing_pixel(&mut f, CHILD, OR_BW, OR_CW, OR_CH, |x, y, _inside, got| {
        assert_eq!(
            got,
            or_bgr(OR_W_BG),
            "after XSetWindowBorder(W_BG): pixel ({x}, {y})",
        );
    });

    // …and back, so a green→red style recolour (awesome's focus
    // change, the whole point of #133) is proven in both directions.
    or_cwa(&mut f, CHILD, 0x0000_0008, &[OR_W_FG]);
    or_for_each_backing_pixel(&mut f, CHILD, OR_BW, OR_CW, OR_CH, |x, y, inside, got| {
        let want = if inside { OR_W_BG } else { OR_W_FG };
        assert_eq!(
            got,
            or_bgr(want),
            "after XSetWindowBorder(W_FG): {} pixel ({x}, {y})",
            if inside { "content" } else { "RING" },
        );
    });
}

/// xts5 `Xlib4/XSetWindowBorderPixmap` purposes 1-3, replayed: the
/// tiled half. `maketile` builds a 7x7 pixmap
/// (`xts5/src/lib/checktile.c:195-219`) and the purposes then
/// `PIXCHECK` the parent against a reference image, so the tile's
/// PHASE is what they are really asserting.
///
/// The tile here is 8x8 with a one-pixel marker column at tile x = 0
/// and marker row at tile y = 0, which pins the phase exactly: a
/// mis-set tile origin of any amount that is not a multiple of 8 moves
/// the markers.
///
/// The phase rule is Xorg's, from `mi/miexpose.c:458-469`: walk up
/// while the background is ParentRelative, take that window's
/// `drawable.x/y`, and subtract the pixmap's `screen_x/y` — which for
/// a bordered window is its OUTER origin
/// (`composite/compalloc.c:610`). With a concrete (non-ParentRelative)
/// background that comes out as the CONTENT origin, so ring pixel
/// `(x, y)` in content-local coordinates samples tile
/// `(x mod tw, y mod th)` — negative on the top and left sides.
///
/// Tiled borders have NO WM coverage (awesome never sets
/// `border-pixmap`), so this test is the only proof for that half.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn xts_xlib4_xsetwindowborderpixmap_tiles_the_ring_at_the_content_phase() {
    const PARENT: u32 = 0x0063_0001;
    const CHILD: u32 = 0x0063_0002;
    const TILE: u32 = 0x0063_0003;
    const GC: u32 = 0x0063_0004;
    const TW: u16 = 8;
    const TH: u16 = 8;
    const T_BASE: u32 = 0x0000_1020;
    const T_COL: u32 = 0x0000_30f0;
    const T_ROW: u32 = 0x0000_f050;

    let Some(mut f) = ProtoFixture::new() else {
        eprintln!("skipping: no Vk");
        return;
    };
    let root = yserver_core::resources::ROOT_WINDOW.0;

    or_create_window(
        &mut f,
        PARENT,
        root,
        0,
        10,
        10,
        200,
        150,
        0,
        0,
        0x0000_0002,
        &[OR_W_BG],
    );
    f.req(8, 0, &PARENT.to_le_bytes());

    // `maketile(display, w)`: CreatePixmap at the window's depth, then
    // paint the pattern into it.
    let mut body = Vec::new();
    body.extend_from_slice(&TILE.to_le_bytes());
    body.extend_from_slice(&PARENT.to_le_bytes());
    body.extend_from_slice(&TW.to_le_bytes());
    body.extend_from_slice(&TH.to_le_bytes());
    f.req(53, 24, &body);
    or_create_gc(&mut f, GC, TILE, T_BASE);
    or_fill(&mut f, TILE, GC, 0, 0, TW, TH);
    let mut body = Vec::new();
    body.extend_from_slice(&GC.to_le_bytes());
    body.extend_from_slice(&0x0000_0004u32.to_le_bytes());
    body.extend_from_slice(&T_COL.to_le_bytes());
    f.req(56, 0, &body);
    or_fill(&mut f, TILE, GC, 0, 0, 1, TH);
    let mut body = Vec::new();
    body.extend_from_slice(&GC.to_le_bytes());
    body.extend_from_slice(&0x0000_0004u32.to_le_bytes());
    body.extend_from_slice(&T_ROW.to_le_bytes());
    f.req(56, 0, &body);
    or_fill(&mut f, TILE, GC, 0, 0, TW, 1);

    // `mkwinchild(display, vp, &ap, False, parent, 5)` — purpose 3's
    // create: the border width IS supplied at CreateWindow here, which
    // is why purpose 3 is the one oracle purpose step 4 can reach on
    // its own. CWBackPixel | CWBorderPixmap = 0x06.
    or_create_window(
        &mut f,
        CHILD,
        PARENT,
        0,
        OR_CX,
        OR_CY,
        OR_CW,
        OR_CH,
        OR_BW,
        0,
        0x0000_0006,
        &[OR_W_BG, TILE],
    );
    f.req(8, 0, &CHILD.to_le_bytes());

    let b = i32::from(OR_BW);
    let tile_at = |lx: i32, ly: i32| {
        let tx = lx.rem_euclid(i32::from(TW));
        let ty = ly.rem_euclid(i32::from(TH));
        if ty == 0 {
            T_ROW
        } else if tx == 0 {
            T_COL
        } else {
            T_BASE
        }
    };
    or_for_each_backing_pixel(&mut f, CHILD, OR_BW, OR_CW, OR_CH, |x, y, inside, got| {
        if inside {
            assert_eq!(
                got,
                or_bgr(OR_W_BG),
                "tiled border must not reach the content: pixel ({x}, {y})",
            );
        } else {
            assert_eq!(
                got,
                or_bgr(tile_at(x - b, y - b)),
                "RING pixel ({x}, {y}) — content-local ({}, {}) — wrong tile phase",
                x - b,
                y - b,
            );
        }
    });

    // XSetWindowBorderPixmap on an already-bordered window repaints
    // (purpose 2's assertion, in its tiled form): set a solid pixel
    // first, then the tile, and the tile must come back.
    or_cwa(&mut f, CHILD, 0x0000_0008, &[OR_W_FG]);
    or_for_each_backing_pixel(&mut f, CHILD, OR_BW, OR_CW, OR_CH, |x, y, inside, got| {
        if !inside {
            assert_eq!(got, or_bgr(OR_W_FG), "solid override: RING ({x}, {y})");
        }
    });
    or_cwa(&mut f, CHILD, 0x0000_0004, &[TILE]);
    or_for_each_backing_pixel(&mut f, CHILD, OR_BW, OR_CW, OR_CH, |x, y, inside, got| {
        // The content assertion here is load-bearing in a way the
        // first block's is not: nothing repaints the background after
        // this point (`map_subwindow` re-tiles the content at map
        // time), so a ring fill that spilled into the content would
        // survive to be seen.
        let want = if inside {
            OR_W_BG
        } else {
            tile_at(x - b, y - b)
        };
        assert_eq!(
            got,
            or_bgr(want),
            "tile restored: {} ({x}, {y})",
            if inside { "content" } else { "RING" },
        );
    });
}

/// #133 step 4 (4.3) — Xorg's depth-32 alpha rule, end to end through
/// the dispatcher. A depth-32 window under a depth-24 parent must get
/// `fill.pixel |= 0xff000000` ("Make sure alpha will sample as 1.0 for
/// opaque windows", `mi/miexpose.c:491-511`), because the ring fill
/// lands on `vkCmdClearAttachments` (`render/engine.rs:10047`), which
/// writes all four channels verbatim from the decoded pixel — nothing
/// downstream forces alpha for a depth-32 destination.
///
/// A ring left at α = 0 renders as a fully transparent border: the
/// scene compositor runs window draws in `alpha_passthrough` mode, so
/// whatever is underneath shows through where the border should be.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn border_ring_forces_opaque_alpha_for_a_depth32_window_under_depth24() {
    const PARENT: u32 = 0x0064_0001;
    const CHILD: u32 = 0x0064_0002;
    // α = 0 on the wire; the rule must make it 0xFF in storage.
    const BORDER: u32 = 0x0000_ff00;

    let Some(mut f) = ProtoFixture::new() else {
        eprintln!("skipping: no Vk");
        return;
    };
    let root = yserver_core::resources::ROOT_WINDOW.0;
    let argb_visual = yserver_core::resources::ARGB_VISUAL.0;
    let argb_colormap = yserver_core::resources::ARGB_COLORMAP.0;

    or_create_window(
        &mut f,
        PARENT,
        root,
        0,
        10,
        10,
        200,
        150,
        0,
        0,
        0x0000_0002,
        &[OR_W_BG],
    );
    f.req(8, 0, &PARENT.to_le_bytes());

    // depth 32 under a depth-24 parent. CWBackPixel | CWBorderPixel |
    // CWColormap = 0x02 | 0x08 | 0x2000, values in ascending bit order.
    or_create_window(
        &mut f,
        CHILD,
        PARENT,
        32,
        OR_CX,
        OR_CY,
        OR_CW,
        OR_CH,
        OR_BW,
        argb_visual,
        0x0000_200a,
        &[0xff00_0000, BORDER, argb_colormap],
    );
    f.req(8, 0, &CHILD.to_le_bytes());

    let (sw, sh, bytes) = f.backing(CHILD);
    let b = u32::from(OR_BW);
    assert_eq!(
        (sw, sh),
        (u32::from(OR_CW) + 2 * b, u32::from(OR_CH) + 2 * b)
    );
    // Top-left ring pixel.
    assert_eq!(
        &bytes[0..4],
        &brd_bgra(BORDER | 0xff00_0000),
        "depth-32 ring under a depth-24 ancestor must sample as α = 1.0",
    );
}

/// #133 step 4 — `bw == 0` IDENTITY, at the protocol level. Every WM in
/// the smoke set uses `bw = 0`; the ring fill must not touch that path
/// at all — no submit, no changed pixel — however many border
/// attributes a client sets.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn bw_zero_border_changes_paint_nothing() {
    const WID: u32 = 0x0065_0001;

    let Some(mut f) = ProtoFixture::new() else {
        eprintln!("skipping: no Vk");
        return;
    };
    let root = yserver_core::resources::ROOT_WINDOW.0;

    or_create_window(
        &mut f,
        WID,
        root,
        0,
        10,
        10,
        40,
        30,
        0,
        0,
        0x0000_0002,
        &[OR_W_BG],
    );
    f.req(8, 0, &WID.to_le_bytes());
    let before = f.backing(WID);
    assert_eq!(
        (before.0, before.1),
        (40, 30),
        "bw == 0 storage is exactly the content extent",
    );

    // Every border-attribute shape a client can send.
    or_cwa(&mut f, WID, 0x0000_0008, &[0x00ff_00ff]);
    or_cwa(&mut f, WID, 0x0000_0008, &[OR_W_FG]);
    // ConfigureWindow(border_width = 0) — the no-change case.
    let mut body = Vec::new();
    body.extend_from_slice(&WID.to_le_bytes());
    body.extend_from_slice(&0x0010u16.to_le_bytes()); // CWBorderWidth
    body.extend_from_slice(&0u16.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    f.req(12, 0, &body);

    let after = f.backing(WID);
    assert_eq!(
        after, before,
        "a bw == 0 window has no ring: nothing may change a pixel",
    );
}

/// The complement that keeps the two halves honest: once the ring is
/// PAINTED, a client draw still cannot reach it. Step 3 proved the clip
/// against a ring holding the background; this proves it against a ring
/// holding a different colour, which is the only version that can tell
/// a working clip from a fill that happened to write the same value.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn a_client_draw_cannot_reach_the_painted_ring() {
    const WID: u32 = 0x0066_0001;
    const GC: u32 = 0x0066_0002;
    const BORDER: u32 = 0x0000_00ff;
    const CONTENT: u32 = 0x00ff_0000;

    let Some(mut f) = ProtoFixture::new() else {
        eprintln!("skipping: no Vk");
        return;
    };
    let root = yserver_core::resources::ROOT_WINDOW.0;

    or_create_window(
        &mut f,
        WID,
        root,
        0,
        10,
        10,
        OR_CW,
        OR_CH,
        OR_BW,
        0,
        0x0000_000a,
        &[OR_W_BG, BORDER],
    );
    f.req(8, 0, &WID.to_le_bytes());
    or_create_gc(&mut f, GC, WID, CONTENT);
    // A fill that covers the WHOLE allocation in content-local
    // coordinates: `(-bw, -bw, w + 2bw, h + 2bw)`.
    or_fill(
        &mut f,
        WID,
        GC,
        -(OR_BW as i16),
        -(OR_BW as i16),
        OR_CW + 2 * OR_BW,
        OR_CH + 2 * OR_BW,
    );

    or_for_each_backing_pixel(&mut f, WID, OR_BW, OR_CW, OR_CH, |x, y, inside, got| {
        let want = if inside { CONTENT } else { BORDER };
        assert_eq!(
            got,
            or_bgr(want),
            "{} pixel ({x}, {y}) after an over-reaching client fill",
            if inside { "content" } else { "RING" },
        );
    });
}
