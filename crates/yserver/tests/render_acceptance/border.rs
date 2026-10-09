use super::*;

// ───── #133 step 3 (P4) — the content clip, per operation family ─────
//
// A bordered window's storage is the BORDERED extent
// `(w + 2bw) x (h + 2bw)`, placed at the window's OUTER origin with the
// client-visible content at `(bw, bw)` inside it — Xorg
// `compAllocPixmap` (`composite/compalloc.c:610`). Storage bounds are
// therefore no longer the drawable's bounds, and every client route
// into that storage has to be confined to the content rect: a draw may
// not paint the ring and a read may not return it as window content.
//
// The fixture below is deliberately over-reaching in every test: each
// op names a rect that covers the WHOLE storage (content-local
// `(-bw, -bw, w + 2bw, h + 2bw)`). Pre-#133 that painted or read the
// ring, because the storage extent was the only clip.
//
// The last two tests are the complement, and they are what makes the
// rest meaningful: the PRIVILEGED backing route (`server_backing_dst`,
// which step 4's ring fill uses) CAN reach the ring, and a `bw == 0`
// window has no ring at all — so these tests cannot be satisfied by an
// implementation that has merely lost the ability to write there.

/// Border width, content width and content height of the fixture.
/// Storage is therefore 24x16 with content at (4, 4).
const BRD_BW: u16 = 4;
const BRD_CW: u16 = 16;
const BRD_CH: u16 = 8;
const BRD_SW: u32 = BRD_CW as u32 + 2 * BRD_BW as u32;
const BRD_SH: u32 = BRD_CH as u32 + 2 * BRD_BW as u32;

/// A depth-32 top-level window with `border_width = BRD_BW`, background
/// `bg`. Creation fills the WHOLE allocation (ring included) with the
/// background through the privileged route, so the ring starts at a
/// known colour without anything having painted a border yet (that is
/// step 4).
fn brd_bordered_window(b: &mut KmsBackend, bg: u32) -> (yserver_core::backend::WindowHandle, u32) {
    use yserver_core::{backend::WindowHandle, host_x11::HostSubwindowVisual};
    let root = WindowHandle::from_raw(1).expect("root");
    let w = b
        .create_subwindow(
            None,
            root,
            0,
            0,
            BRD_CW,
            BRD_CH,
            BRD_BW,
            HostSubwindowVisual::Explicit {
                depth: 32,
                visual_xid: 0,
                colormap_xid: 0,
            },
            Some(bg),
            None,
        )
        .expect("create bordered window");
    let xid = w.as_raw();
    b.map_window_for_tests(xid).expect("map bordered window");
    assert_eq!(
        b.storage_extent_for_tests(xid),
        Some((BRD_SW, BRD_SH)),
        "storage must be the bordered extent (w + 2bw) x (h + 2bw)",
    );
    (w, xid)
}

/// Assert every storage pixel OUTSIDE the content rect still reads
/// `expect`, and every pixel inside reads `content` (when given).
fn brd_assert_ring(b: &mut KmsBackend, xid: u32, expect: u32, content: Option<u32>, ctx: &str) {
    let (sw, sh, bytes) = b
        .backing_pixels_for_tests(xid)
        .expect("privileged backing read");
    assert_eq!((sw, sh), (BRD_SW, BRD_SH), "{ctx}: storage extent");
    let bw = i64::from(BRD_BW);
    for y in 0..i64::from(sh) {
        for x in 0..i64::from(sw) {
            let off = ((y * i64::from(sw) + x) * 4) as usize;
            let got = &bytes[off..off + 4];
            let inside =
                x >= bw && y >= bw && x < bw + i64::from(BRD_CW) && y < bw + i64::from(BRD_CH);
            if inside {
                if let Some(c) = content {
                    assert_eq!(got, &brd_bgra(c), "{ctx}: content pixel ({x},{y})");
                }
            } else {
                assert_eq!(
                    got,
                    &brd_bgra(expect),
                    "{ctx}: RING pixel ({x},{y}) was written by a client route",
                );
            }
        }
    }
}

/// 3.3 + the privileged half of 3.4's complement: storage is the
/// bordered extent, and the creation-time initialisation — a
/// PRIVILEGED backing write — covers the ring, so the ring is never
/// pool garbage.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn border_storage_is_bordered_extent_and_init_reaches_the_ring() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let (_, xid) = brd_bordered_window(&mut b, BRD_RED);
    // Whole allocation, ring INCLUDED, initialised to the background.
    brd_assert_ring(&mut b, xid, BRD_RED, Some(BRD_RED), "init");

    // The bw == 0 control: storage is exactly w x h, and the resolved
    // target carries no content clip at all.
    use yserver_core::{backend::WindowHandle, host_x11::HostSubwindowVisual};
    let root = WindowHandle::from_raw(1).expect("root");
    let plain = b
        .create_subwindow(
            None,
            root,
            0,
            0,
            BRD_CW,
            BRD_CH,
            0,
            HostSubwindowVisual::Explicit {
                depth: 32,
                visual_xid: 0,
                colormap_xid: 0,
            },
            Some(BRD_RED),
            None,
        )
        .expect("create bw=0 window");
    b.map_window_for_tests(plain.as_raw()).expect("map");
    assert_eq!(
        b.storage_extent_for_tests(plain.as_raw()),
        Some((u32::from(BRD_CW), u32::from(BRD_CH))),
        "bw == 0 storage must stay exactly w x h",
    );
    let (offset, clip, bordered) = b
        .paint_target_shape_for_tests(plain.as_raw())
        .expect("resolve bw=0");
    assert_eq!(offset, (0, 0), "bw == 0 content offset");
    assert_eq!(clip, None, "bw == 0 must have no content clip");
    assert!(!bordered);
}

/// Core fill destination (`PolyFillRectangle` → `fill_solid_rects`).
#[test]
#[ignore = "needs live Vulkan ICD"]
fn border_content_clip_confines_core_fill() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let (_, xid) = brd_bordered_window(&mut b, BRD_RED);
    b.fill_rectangle(
        None,
        xid,
        BRD_GREEN,
        -(BRD_BW as i16),
        -(BRD_BW as i16),
        BRD_SW as u16,
        BRD_SH as u16,
    )
    .expect("fill_rectangle over the whole storage");
    brd_assert_ring(&mut b, xid, BRD_RED, Some(BRD_GREEN), "core fill");
}

/// `PutImage` at NEGATIVE destination coordinates: the leading rows and
/// columns must be cropped off the wire image, not written into the ring.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn border_content_clip_confines_put_image_at_negative_coords() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let (_, xid) = brd_bordered_window(&mut b, BRD_RED);
    let px = brd_bgra(BRD_GREEN);
    let data: Vec<u8> = (0..(BRD_SW * BRD_SH))
        .flat_map(|_| px.into_iter())
        .collect();
    b.put_image(
        None,
        xid,
        32,
        BRD_SW as u16,
        BRD_SH as u16,
        -(BRD_BW as i16),
        -(BRD_BW as i16),
        &data,
    )
    .expect("put_image at negative coords");
    brd_assert_ring(&mut b, xid, BRD_RED, Some(BRD_GREEN), "put_image");
}

/// `CopyArea` DESTINATION.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn border_content_clip_confines_copy_area_destination() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let (_, xid) = brd_bordered_window(&mut b, BRD_RED);
    let src = b
        .create_pixmap(None, 32, BRD_SW as u16, BRD_SH as u16)
        .expect("src pixmap")
        .as_raw();
    b.fill_rectangle(None, src, BRD_GREEN, 0, 0, BRD_SW as u16, BRD_SH as u16)
        .expect("seed src green");
    b.copy_area(
        None,
        src,
        xid,
        0,
        0,
        -(BRD_BW as i16),
        -(BRD_BW as i16),
        BRD_SW as u16,
        BRD_SH as u16,
    )
    .expect("copy_area into the whole storage");
    brd_assert_ring(&mut b, xid, BRD_RED, Some(BRD_GREEN), "copy_area dst");
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn redirected_titlebar_copy_preserves_border() {
    let mut f = ProtoFixture::new().expect("live Vulkan");
    let root = yserver_core::resources::ROOT_WINDOW.0;
    const W: u32 = 0x1470;
    const SRC: u32 = 0x1471;
    const GC: u32 = 0x1472;
    or_create_window(
        &mut f,
        W,
        root,
        32,
        20,
        30,
        BRD_CW,
        BRD_CH,
        BRD_BW,
        yserver_core::resources::ARGB_VISUAL.0,
        8 | 0x2000,
        &[BRD_RED, yserver_core::resources::ARGB_COLORMAP.0],
    );
    wz_map(&mut f, W);
    let mut body = root.to_le_bytes().to_vec();
    body.extend_from_slice(&[1, 0, 0, 0]);
    f.req(144, 2, &body);
    let mut body = SRC.to_le_bytes().to_vec();
    body.extend_from_slice(&W.to_le_bytes());
    body.extend_from_slice(&BRD_CW.to_le_bytes());
    body.extend_from_slice(&BRD_CH.to_le_bytes());
    f.req(53, 32, &body);
    or_create_gc(&mut f, GC, SRC, BRD_GREEN);
    or_fill(&mut f, SRC, GC, 0, 0, BRD_CW, BRD_CH);
    let mut body = SRC.to_le_bytes().to_vec();
    body.extend_from_slice(&W.to_le_bytes());
    body.extend_from_slice(&GC.to_le_bytes());
    body.extend_from_slice(&[0; 8]);
    body.extend_from_slice(&BRD_CW.to_le_bytes());
    body.extend_from_slice(&3u16.to_le_bytes());
    f.req(62, 0, &body);
    let host = f
        .state
        .resources
        .window(yserver_protocol::x11::ResourceId(W))
        .unwrap()
        .host_xid
        .unwrap()
        .as_raw();
    brd_assert_ring(&mut f.backend, host, BRD_RED, None, "titlebar copy");
    let (sw, _, pixels) = f.backing(W);
    let offset = ((u32::from(BRD_BW) * sw + u32::from(BRD_BW)) * 4) as usize;
    assert_eq!(&pixels[offset..offset + 4], &brd_bgra(BRD_GREEN));

    // Reading the window also starts at its content origin, not its ring.
    let mut body = W.to_le_bytes().to_vec();
    body.extend_from_slice(&SRC.to_le_bytes());
    body.extend_from_slice(&GC.to_le_bytes());
    body.extend_from_slice(&[0; 8]);
    body.extend_from_slice(&BRD_CW.to_le_bytes());
    body.extend_from_slice(&3u16.to_le_bytes());
    f.req(62, 0, &body);
    let src = f
        .state
        .resources
        .pixmap(yserver_protocol::x11::ResourceId(SRC))
        .unwrap()
        .host_xid
        .unwrap()
        .as_raw();
    let copied = f
        .backend
        .get_image_pixels_for_tests(src, 2, 0, 0, BRD_CW, 3, !0)
        .unwrap()
        .unwrap();
    for pixel in copied.chunks_exact(4) {
        assert_eq!(pixel, &brd_bgra(BRD_GREEN));
    }
}

/// A client window C inside its redirected frame F (an xfwm4 frame under
/// the compositor), with a child V reaching past C's bottom as GTK's
/// scrolled bin window does. C and V paint into F's backing, which F's
/// own pixels share: Xorg confines each to its clipList, its rect inside
/// its parent's (`mi/mivaltree.c:390`) minus its children for
/// ClipByChildren, and a move copies only that (`fbCopyWindow`). Nothing
/// outside C may change, whatever C or V draws or wherever V moves.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn redirected_frame_child_paints_stay_inside_the_child() {
    const FRAME: u32 = 0xFF20_2020;
    const GRAY: u32 = 0xFF3B_3B3E;
    const BLUE: u32 = 0xFF00_00FF;
    const ORANGE: u32 = 0xFFFF_8000;
    const F: u32 = 0x1480;
    const C: u32 = 0x1481;
    const V: u32 = 0x1482;
    const GC: u32 = 0x1483;
    let mut f = ProtoFixture::new().expect("live Vulkan");
    let root = yserver_core::resources::ROOT_WINDOW.0;
    let window = |f: &mut ProtoFixture, wid, parent, x, y, w, h, bg| {
        or_create_window(
            f,
            wid,
            parent,
            32,
            x,
            y,
            w,
            h,
            0,
            yserver_core::resources::ARGB_VISUAL.0,
            2 | 8 | 0x2000,
            &[bg, 0, yserver_core::resources::ARGB_COLORMAP.0],
        );
        wz_map(f, wid);
    };
    window(&mut f, F, root, 20, 30, 200, 150, FRAME);
    let mut body = root.to_le_bytes().to_vec();
    body.extend_from_slice(&[1, 0, 0, 0]);
    f.req(144, 2, &body);
    window(&mut f, C, F, 5, 20, 190, 100, GRAY);
    window(&mut f, V, C, 100, 10, 80, 120, BLUE);
    let check = |f: &mut ProtoFixture, when: &str, expect: &[(u32, u32, u32)]| {
        let (sw, _, pixels) = f.backing(F);
        for &(x, y, pixel) in expect {
            let at = ((y * sw + x) * 4) as usize;
            assert_eq!(
                &pixels[at..at + 4],
                &brd_bgra(pixel),
                "{when}: F's backing at ({x},{y})"
            );
        }
    };
    // Below C (F's own pixels), where V's 120 rows would reach.
    check(
        &mut f,
        "V mapped",
        &[(145, 135, FRAME), (145, 60, BLUE), (50, 60, GRAY)],
    );

    or_create_gc(&mut f, GC, C, ORANGE);
    or_fill(&mut f, C, GC, -50, -50, 400, 400);
    check(
        &mut f,
        "fill over C",
        &[
            (50, 10, FRAME),
            (2, 60, FRAME),
            (50, 60, ORANGE),
            (145, 60, BLUE),
            (145, 135, FRAME),
        ],
    );

    let (w, h) = (230u16, 140u16);
    let mut body = C.to_le_bytes().to_vec();
    body.extend_from_slice(&GC.to_le_bytes());
    body.extend_from_slice(&w.to_le_bytes());
    body.extend_from_slice(&h.to_le_bytes());
    body.extend_from_slice(&(-20i16).to_le_bytes());
    body.extend_from_slice(&(-20i16).to_le_bytes());
    body.extend_from_slice(&[0, 32, 0, 0]);
    for _ in 0..u32::from(w) * u32::from(h) {
        body.extend_from_slice(&brd_bgra(GRAY));
    }
    f.req(72, 2, &body);
    check(
        &mut f,
        "PutImage over C",
        &[
            (50, 10, FRAME),
            (198, 60, FRAME),
            (50, 60, GRAY),
            (145, 60, BLUE),
            (145, 135, FRAME),
        ],
    );

    // Scrolled up by 50: V's old rows past C's bottom are not V's to
    // carry, and its new rows above C's top are not V's to write.
    wz_configure(&mut f, V, 2, &[-40]);
    check(
        &mut f,
        "V moved to y=-40",
        &[(145, 10, FRAME), (145, 25, BLUE), (145, 135, FRAME)],
    );
}

/// Lines and points into a client window C inside its redirected frame
/// F take the clip fills take: not under H, a sibling stacked above C
/// that shares F's backing, and not over C's child V under
/// ClipByChildren — Xorg strokes through the GC's composite clip,
/// C's clipList (`mi/mivaltree.c:390-437`). Measured by
/// tools/vng-scenarios/draw-clip-probe.c.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn redirected_frame_child_strokes_stay_in_its_clip() {
    const FRAME: u32 = 0xFF20_2020;
    const GRAY: u32 = 0xFF3B_3B3E;
    const BLUE: u32 = 0xFF00_00FF;
    const CYAN: u32 = 0xFF00_FFFF;
    const ORANGE: u32 = 0xFFFF_8000;
    const F: u32 = 0x14a0;
    const C: u32 = 0x14a1;
    const H: u32 = 0x14a2;
    const V: u32 = 0x14a3;
    const GC: u32 = 0x14a4;
    let mut f = ProtoFixture::new().expect("live Vulkan");
    let root = yserver_core::resources::ROOT_WINDOW.0;
    let window = |f: &mut ProtoFixture, wid, parent, x, y, w, h, bg| {
        or_create_window(
            f,
            wid,
            parent,
            32,
            x,
            y,
            w,
            h,
            0,
            yserver_core::resources::ARGB_VISUAL.0,
            2 | 8 | 0x2000,
            &[bg, 0, yserver_core::resources::ARGB_COLORMAP.0],
        );
        wz_map(f, wid);
    };
    window(&mut f, F, root, 20, 30, 200, 150, FRAME);
    let mut body = root.to_le_bytes().to_vec();
    body.extend_from_slice(&[1, 0, 0, 0]);
    f.req(144, 2, &body);
    window(&mut f, C, F, 5, 20, 190, 100, GRAY);
    window(&mut f, H, F, 130, 15, 30, 20, CYAN);
    window(&mut f, V, C, 100, 10, 80, 120, BLUE);
    or_create_gc(&mut f, GC, C, ORANGE);
    // PolySegment: C's rows 2 (through H) and 40 (through V), and a
    // column at C's x=140 from above C to below it.
    let mut body = C.to_le_bytes().to_vec();
    body.extend_from_slice(&GC.to_le_bytes());
    for (x1, y1, x2, y2) in [
        (-50i16, 2i16, 300i16, 2i16),
        (-50, 40, 300, 40),
        (140, -50, 140, 200),
    ] {
        for v in [x1, y1, x2, y2] {
            body.extend_from_slice(&v.to_le_bytes());
        }
    }
    f.req(66, 0, &body);
    // PolyPoint: a row of points through H and V.
    let mut body = C.to_le_bytes().to_vec();
    body.extend_from_slice(&GC.to_le_bytes());
    for x in (-40i16..300).step_by(2) {
        for y in [4i16, 50] {
            body.extend_from_slice(&x.to_le_bytes());
            body.extend_from_slice(&y.to_le_bytes());
        }
    }
    f.req(64, 0, &body);
    let (sw, _, pixels) = f.backing(F);
    // F coordinates: C's (x, y) is F's (x + 5, y + 20).
    for (x, y, pixel, what) in [
        (50, 22, ORANGE, "C's row 2"),
        (140, 22, CYAN, "H over C's row 2"),
        (50, 60, ORANGE, "C's row 40"),
        (150, 60, BLUE, "V under C's row 40"),
        (145, 25, CYAN, "H over C's column 140"),
        (145, 45, BLUE, "V under C's column 140"),
        (145, 10, FRAME, "F above C's column 140"),
        (61, 24, ORANGE, "a point in C's row 4"),
        (141, 24, CYAN, "H over a point in C's row 4"),
        (151, 70, BLUE, "V under a point in C's row 50"),
        (90, 61, GRAY, "C between the lines"),
    ] {
        let at = ((y * sw + x) * 4) as usize;
        assert_eq!(
            &pixels[at..at + 4],
            &brd_bgra(pixel),
            "{what}: F's backing at ({x},{y})"
        );
    }
}

/// GetImage of a window without a compositor reads what it shows,
/// its children included: Xorg reads the screen pixmap under it
/// (`DoGetImage`, `dix/dispatch.c:2176-2189`). Measured by
/// tools/vng-scenarios/draw-clip-probe.c (`GetImage C`, direct): F
/// (200x150) with C (5,20 190x100), C's child V (100,10 80x120) and
/// V's child G (10,10 20x20), all of the root's depth.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn get_image_of_an_unredirected_window_includes_its_inferiors() {
    const FRAME: u32 = 0x0020_2020;
    const GRAY: u32 = 0x003B_3B3E;
    const BLUE: u32 = 0x0000_00FF;
    const GREEN: u32 = 0x0000_FF00;
    const F: u32 = 0x14b0;
    const C: u32 = 0x14b1;
    const V: u32 = 0x14b2;
    const G: u32 = 0x14b3;
    let mut f = ProtoFixture::new().expect("live Vulkan");
    let root = yserver_core::resources::ROOT_WINDOW.0;
    let visual = yserver_core::resources::ROOT_VISUAL.0;
    let window = |f: &mut ProtoFixture, wid, parent, x, y, w, h, bg| {
        or_create_window(f, wid, parent, 24, x, y, w, h, 0, visual, 2, &[bg]);
        wz_map(f, wid);
    };
    window(&mut f, F, root, 0, 0, 200, 150, FRAME);
    window(&mut f, C, F, 5, 20, 190, 100, GRAY);
    window(&mut f, V, C, 100, 10, 80, 120, BLUE);
    window(&mut f, G, V, 10, 10, 20, 20, GREEN);
    let host = f.host_xid(F);
    let reply = f
        .backend
        .get_image(None, host, 2, 0, 0, 200, 150, !0)
        .expect("get_image")
        .expect("a reply");
    let pixels = &reply[32..];
    for (x, y, pixel, what) in [
        (2, 2, FRAME, "F"),
        (50, 60, GRAY, "C"),
        (150, 60, BLUE, "V"),
        (120, 45, GREEN, "G"),
        (150, 125, FRAME, "F under V's rows past C's bottom"),
    ] {
        let at = ((y * 200 + x) * 4) as usize;
        assert_eq!(&pixels[at..at + 3], &or_bgr(pixel), "{what} at ({x},{y})");
    }
}

/// RENDER through a Picture on a window without a compositor: as a
/// source it reads the window's children too, whatever its subwindow
/// mode (`miClipPictureSrc`, `render/mipict.c:265-284`); as an
/// IncludeInferiors destination it paints over them
/// (`render/mipict.c:114-118`). Measured by
/// tools/vng-scenarios/draw-clip-probe.c (direct): C (190x100) with
/// its child V (100,10 80x120).
#[test]
#[ignore = "needs live Vulkan ICD"]
fn render_through_an_unredirected_window_reaches_its_inferiors() {
    use yserver_core::backend::{AnyHandle, PixmapHandle, WindowHandle};
    const GRAY: u32 = 0x003B_3B3E;
    const BLUE: u32 = 0x0000_00FF;
    const C: u32 = 0x14c0;
    const V: u32 = 0x14c1;
    let mut f = ProtoFixture::new().expect("live Vulkan");
    let root = yserver_core::resources::ROOT_WINDOW.0;
    let visual = yserver_core::resources::ROOT_VISUAL.0;
    or_create_window(&mut f, C, root, 24, 0, 0, 190, 100, 0, visual, 2, &[GRAY]);
    wz_map(&mut f, C);
    or_create_window(&mut f, V, C, 24, 100, 10, 80, 120, 0, visual, 2, &[BLUE]);
    wz_map(&mut f, V);
    let (c, v) = (f.host_xid(C), f.host_xid(V));
    let window_pic = |f: &mut ProtoFixture, mode: u32| {
        f.backend
            .render_create_picture(
                None,
                AnyHandle::Window(WindowHandle::from_raw(c).unwrap()),
                0,
                0x0100, // CPSubwindowMode
                &mode.to_le_bytes(),
            )
            .unwrap()
            .unwrap()
            .as_raw()
    };
    for mode in [0, 1] {
        let pm = f.backend.create_pixmap(None, 24, 190, 100).unwrap();
        let pm_xid = pm.as_raw();
        let dst = f
            .backend
            .render_create_picture(
                None,
                AnyHandle::Pixmap(PixmapHandle::from_raw(pm_xid).unwrap()),
                0,
                0,
                &[],
            )
            .unwrap()
            .unwrap()
            .as_raw();
        let src = window_pic(&mut f, mode);
        f.backend
            .render_composite(None, 1, src, 0, dst, 0, 0, 0, 0, 0, 0, 190, 100)
            .unwrap();
        let got = f
            .backend
            .get_image_pixels_for_tests(pm_xid, 2, 0, 0, 190, 100, !0)
            .unwrap()
            .unwrap();
        for (x, y, pixel) in [(50, 50, GRAY), (150, 50, BLUE)] {
            let at = ((y * 190 + x) * 4) as usize;
            assert_eq!(
                &got[at..at + 3],
                &or_bgr(pixel),
                "source mode {mode} at ({x},{y})"
            );
        }
    }
    let dst = window_pic(&mut f, 1);
    let orange = [0xff, 0xff, 0x80, 0x80, 0, 0, 0xff, 0xff];
    let mut rect = Vec::new();
    for v in [-50i16, -50] {
        rect.extend_from_slice(&v.to_le_bytes());
    }
    for v in [400u16, 400] {
        rect.extend_from_slice(&v.to_le_bytes());
    }
    f.backend
        .render_fill_rectangles(None, dst, 1, orange, &rect, 0, 0)
        .unwrap();
    let got = f
        .backend
        .get_image_pixels_for_tests(v, 2, 0, 0, 80, 120, !0)
        .unwrap()
        .unwrap();
    assert_eq!(
        &got[(20 * 80 + 20) * 4..(20 * 80 + 20) * 4 + 3],
        &or_bgr(0x00FF_8000),
        "V under an IncludeInferiors fill of C"
    );
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn redirected_menu_border_change_resizes_backing() {
    let mut f = ProtoFixture::new().expect("live Vulkan");
    let root = yserver_core::resources::ROOT_WINDOW.0;
    const W: u32 = 0x1460;
    const GC: u32 = 0x1461;
    or_create_window(
        &mut f,
        W,
        root,
        32,
        20,
        30,
        100,
        30,
        0,
        yserver_core::resources::ARGB_VISUAL.0,
        8 | 0x2000,
        &[BRD_RED, yserver_core::resources::ARGB_COLORMAP.0],
    );
    wz_map(&mut f, W);
    let mut body = root.to_le_bytes().to_vec();
    body.extend_from_slice(&[1, 0, 0, 0]);
    f.req(144, 2, &body);
    or_create_gc(&mut f, GC, W, BRD_GREEN);
    or_fill(&mut f, W, GC, 0, 0, 100, 30);
    for (mask, values, extent, content) in [
        (0x10, vec![2], (104, 34), (2..102, 2..32)),
        // Same outer extent, different content origin.
        (0x1c, vec![98, 28, 3], (104, 34), (3..101, 3..31)),
        (0x10, vec![1], (100, 30), (1..99, 1..29)),
        (0x10, vec![0], (98, 28), (0..98, 0..28)),
    ] {
        let mut body = GC.to_le_bytes().to_vec();
        body.extend_from_slice(&3u32.to_le_bytes()); // function | plane-mask
        body.extend_from_slice(&5u32.to_le_bytes()); // GXnoop
        body.extend_from_slice(&0u32.to_le_bytes());
        f.req(56, 0, &body);
        or_fill(&mut f, W, GC, 0, 0, 1, 1);
        wz_configure(&mut f, W, mask, &values);
        let (sw, sh, pixels) = f.backing(W);
        assert_eq!((sw, sh), extent, "named backing includes the new border");
        for y in 0..sh {
            for x in 0..sw {
                let expected = if content.0.contains(&x) && content.1.contains(&y) {
                    BRD_GREEN
                } else {
                    BRD_RED
                };
                let offset = ((y * sw + x) * 4) as usize;
                assert_eq!(
                    &pixels[offset..offset + 4],
                    &brd_bgra(expected),
                    "config {values:?}: pixel ({x},{y})"
                );
            }
        }
    }
}

/// `CopyArea` SOURCE: a copy OUT of a bordered window may not carry
/// ring pixels with it. The ring is painted a colour that appears
/// nowhere else, through the privileged route, so its presence in the
/// destination would be unambiguous.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn border_content_clip_confines_copy_area_source() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let (_, xid) = brd_bordered_window(&mut b, BRD_GREEN);
    // Ring := BLUE (privileged, backing space), content stays GREEN.
    assert!(
        b.fill_backing_rect_for_tests(xid, 0, 0, BRD_SW, BRD_SH, BRD_BLUE),
        "privileged ring fill",
    );
    b.fill_rectangle(None, xid, BRD_GREEN, 0, 0, BRD_CW, BRD_CH)
        .expect("client content fill");
    brd_assert_ring(&mut b, xid, BRD_BLUE, Some(BRD_GREEN), "source setup");

    // Copy the whole storage rect OUT of the window into a RED pixmap.
    let dst = b
        .create_pixmap(None, 32, BRD_SW as u16, BRD_SH as u16)
        .expect("dst pixmap")
        .as_raw();
    b.fill_rectangle(None, dst, BRD_RED, 0, 0, BRD_SW as u16, BRD_SH as u16)
        .expect("seed dst red");
    b.copy_area(
        None,
        xid,
        dst,
        -(BRD_BW as i16),
        -(BRD_BW as i16),
        0,
        0,
        BRD_SW as u16,
        BRD_SH as u16,
    )
    .expect("copy_area out of the bordered window");

    let out = b
        .get_image_pixels_for_tests(dst, 2, 0, 0, BRD_SW as u16, BRD_SH as u16, !0)
        .expect("get_image dst")
        .expect("Some bytes");
    for y in 0..BRD_SH {
        for x in 0..BRD_SW {
            let off = ((y * BRD_SW + x) * 4) as usize;
            assert_ne!(
                &out[off..off + 4],
                &brd_bgra(BRD_BLUE),
                "CopyArea source returned a RING pixel at ({x},{y})",
            );
        }
    }
    // Source and destination stay aligned as the clip advances both
    // origins: content (0,0) lands at dst (bw, bw), and everything
    // before it is untouched RED.
    let at = |x: u32, y: u32| {
        let off = ((y * BRD_SW + x) * 4) as usize;
        out[off..off + 4].to_vec()
    };
    assert_eq!(at(0, 0), brd_bgra(BRD_RED), "dst (0,0) must be untouched");
    assert_eq!(
        at(u32::from(BRD_BW), u32::from(BRD_BW)),
        brd_bgra(BRD_GREEN),
        "content must land at dst (bw, bw)",
    );
}

/// `ClearArea` DESTINATION. ClearArea repaints a window's background,
/// which is a CLIENT route and therefore content-clipped like any other
/// — Xorg clears `winSize`-relative coordinates and never the border
/// (`dix/window.c`'s `ClearToBackground` clips to `pWin->clipList`).
/// A request spanning the whole storage rect from negative coordinates
/// must repaint the content and leave the ring alone.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn border_content_clip_confines_clear_area() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let (_, xid) = brd_bordered_window(&mut b, BRD_GREEN);
    // Ring := BLUE through the privileged route, content := RED by a
    // client fill, so both a leak and a no-op are distinguishable from
    // the expected outcome.
    assert!(
        b.fill_backing_rect_for_tests(xid, 0, 0, BRD_SW, BRD_SH, BRD_BLUE),
        "privileged ring fill",
    );
    b.fill_rectangle(None, xid, BRD_RED, 0, 0, BRD_CW, BRD_CH)
        .expect("client content fill");
    brd_assert_ring(&mut b, xid, BRD_BLUE, Some(BRD_RED), "clear_area setup");

    b.clear_area(
        None,
        xid,
        BRD_GREEN,
        None,
        -(BRD_BW as i16),
        -(BRD_BW as i16),
        BRD_SW as u16,
        BRD_SH as u16,
        (0, 0),
    )
    .expect("clear_area over the whole storage");
    // Content back to the background colour proves the clear ran at all;
    // the ring still BLUE proves it stayed inside.
    brd_assert_ring(&mut b, xid, BRD_BLUE, Some(BRD_GREEN), "clear_area");
}

/// `CopyPlane` DESTINATION. The existing CopyPlane coverage
/// (`copy_plane_source_follows_redirect_routing`) deliberately uses
/// `bw = 0` to isolate redirect routing, so nothing exercised it against
/// a border. Its destination writes decompose into `poly_fill_rectangle`
/// calls, which case (a) already proves are clipped — but "the plumbing
/// looks migrated" is not coverage.
///
/// The expected content colour comes from `core.current_foreground`,
/// which this level cannot set, so it is CALIBRATED: the same CopyPlane
/// into a plain pixmap says what the foreground resolves to, and the
/// bordered window then has to match it exactly inside the content and
/// not at all outside.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn border_content_clip_confines_copy_plane_destination() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    // Plane 0 is set in every pixel of a BLUE source, so every sampled
    // pixel classifies as foreground and the destination comes out
    // uniform — which is what makes the calibration below a single value.
    const PLANE: u32 = 0x0000_0001;
    let src = b
        .create_pixmap(None, 32, BRD_SW as u16, BRD_SH as u16)
        .expect("src pixmap")
        .as_raw();
    b.fill_rectangle(None, src, BRD_BLUE, 0, 0, BRD_SW as u16, BRD_SH as u16)
        .expect("seed src blue");

    // Calibration: an unbordered destination of the same size.
    let cal = b
        .create_pixmap(None, 32, BRD_SW as u16, BRD_SH as u16)
        .expect("cal pixmap")
        .as_raw();
    b.copy_plane(
        None,
        src,
        cal,
        0,
        0,
        0,
        0,
        BRD_SW as u16,
        BRD_SH as u16,
        PLANE,
    )
    .expect("copy_plane into a plain pixmap");
    let cal_px = b
        .get_image_pixels_for_tests(cal, 2, 0, 0, 1, 1, !0)
        .expect("get_image cal")
        .expect("Some bytes")[0..4]
        .to_vec();

    let (_, xid) = brd_bordered_window(&mut b, BRD_RED);
    b.copy_plane(
        None,
        src,
        xid,
        0,
        0,
        -(BRD_BW as i16),
        -(BRD_BW as i16),
        BRD_SW as u16,
        BRD_SH as u16,
        PLANE,
    )
    .expect("copy_plane into the whole storage");

    let (sw, sh, bytes) = b
        .backing_pixels_for_tests(xid)
        .expect("privileged backing read");
    assert_eq!((sw, sh), (BRD_SW, BRD_SH), "storage extent");
    let bw = i64::from(BRD_BW);
    let mut content_written = 0u32;
    for y in 0..i64::from(sh) {
        for x in 0..i64::from(sw) {
            let off = ((y * i64::from(sw) + x) * 4) as usize;
            let got = &bytes[off..off + 4];
            let inside =
                x >= bw && y >= bw && x < bw + i64::from(BRD_CW) && y < bw + i64::from(BRD_CH);
            if inside {
                assert_eq!(
                    got,
                    &cal_px[..],
                    "copy_plane: content pixel ({x},{y}) must match the plain-pixmap result",
                );
                content_written += 1;
            } else {
                assert_eq!(
                    got,
                    &brd_bgra(BRD_RED),
                    "copy_plane: RING pixel ({x},{y}) was written by a client route",
                );
            }
        }
    }
    assert_eq!(
        content_written,
        u32::from(BRD_CW) * u32::from(BRD_CH),
        "every content pixel must have been checked",
    );
    assert_ne!(
        cal_px,
        brd_bgra(BRD_RED),
        "the calibration colour must differ from the ring, or the ring \
         assertion above would hold vacuously",
    );
}

/// `GetImage` on a window: `xSrc = 0` must land on the window's
/// CONTENT, and an explicitly border-inclusive request must return the
/// ring — X11 permits the rectangle to reach `±border_width` and reads
/// the containing pixmap (Xorg `DoGetImage`, `dix/dispatch.c:2373-2377`
/// for the bounds rule, `:2382-2390` + `:2405-2419` for the
/// bounding-drawable read; yserver's own handler mirrors the `±bw`
/// rule at `process_request.rs:25566`, citing xts XGetImage-7 which
/// reads `(-1, -1)`).
///
/// This test replaces a round-1 assertion of mine that had it backwards
/// — it required the reply to be CLAMPED to the content rect, which is
/// both anti-Xorg and a protocol violation: the reply came back shorter
/// than the requested rectangle and libX11 indexes the XImage by the
/// REQUESTED width/height. That is what crashed xts
/// `Xlib4/XSetWindowBackgroundPixmap` purpose 2.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn border_get_image_reads_content_at_zero_and_the_ring_when_asked() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let (_, xid) = brd_bordered_window(&mut b, BRD_GREEN);
    assert!(
        b.fill_backing_rect_for_tests(xid, 0, 0, BRD_SW, BRD_SH, BRD_BLUE),
        "privileged ring fill",
    );
    b.fill_rectangle(None, xid, BRD_GREEN, 0, 0, BRD_CW, BRD_CH)
        .expect("client content fill");

    // (a) The window's own rectangle: all content, full length, no ring.
    let content = b
        .get_image_pixels_for_tests(xid, 2, 0, 0, BRD_CW, BRD_CH, !0)
        .expect("get_image content")
        .expect("Some bytes");
    assert_eq!(
        content.len(),
        usize::from(BRD_CW) * usize::from(BRD_CH) * 4,
        "the reply must cover the requested rectangle",
    );
    for (i, px) in content.chunks_exact(4).enumerate() {
        assert_eq!(
            px,
            &brd_bgra(BRD_GREEN),
            "pixel {i}: xSrc = 0 must land on the CONTENT origin, not the ring",
        );
    }

    // (b) The border-inclusive rectangle: full length, and the ring is
    //     present at the edges — this is what X11 asks for.
    let over_w = BRD_CW + 2 * BRD_BW;
    let over_h = BRD_CH + 2 * BRD_BW;
    let over = b
        .get_image_pixels_for_tests(
            xid,
            2,
            -(BRD_BW as i16),
            -(BRD_BW as i16),
            over_w,
            over_h,
            !0,
        )
        .expect("get_image bordered")
        .expect("Some bytes");
    assert_eq!(
        over.len(),
        usize::from(over_w) * usize::from(over_h) * 4,
        "a border-inclusive GetImage must return the requested \
         rectangle's worth of data, never a short reply",
    );
    let at = |x: u32, y: u32| {
        let off = ((y * u32::from(over_w) + x) * 4) as usize;
        over[off..off + 4].to_vec()
    };
    assert_eq!(at(0, 0), brd_bgra(BRD_BLUE), "the ring corner is readable");
    assert_eq!(
        at(u32::from(BRD_BW), u32::from(BRD_BW)),
        brd_bgra(BRD_GREEN),
        "…and the content starts bw inside it",
    );
}

/// RENDER destination (`Composite` onto a Picture wrapping the window).
#[test]
#[ignore = "needs live Vulkan ICD"]
fn border_content_clip_confines_render_destination() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let (w, xid) = brd_bordered_window(&mut b, BRD_RED);
    let dst_pic = b
        .render_create_picture(None, AnyHandle::Window(w), 0, 0, &[])
        .expect("render_create_picture")
        .expect("Some(PictureHandle)");
    // Opaque premultiplied green source (wire u16 per channel).
    let src_pic = b
        .render_create_solid_fill(None, [0x00, 0x00, 0xFF, 0xFF, 0x00, 0x00, 0xFF, 0xFF])
        .expect("solid_fill")
        .expect("Some(PictureHandle)");
    b.render_composite(
        None,
        1, // Src
        src_pic.as_raw(),
        0,
        dst_pic.as_raw(),
        0,
        0,
        0,
        0,
        -(BRD_BW as i16),
        -(BRD_BW as i16),
        BRD_SW as u16,
        BRD_SH as u16,
    )
    .expect("render_composite over the whole storage");
    brd_assert_ring(&mut b, xid, BRD_RED, Some(BRD_GREEN), "RENDER composite");
}

/// RENDER glyph destination (`CompositeGlyphs`): a glyph stamped in the
/// ring must be scissored away even though the picture carries no clip
/// of its own.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn border_content_clip_confines_render_glyph_destination() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let (w, xid) = brd_bordered_window(&mut b, BRD_RED);
    let dst_pic = b
        .render_create_picture(None, AnyHandle::Window(w), 0, 0, &[])
        .expect("render_create_picture")
        .expect("Some(PictureHandle)");
    let src_pic = b
        .render_create_solid_fill(None, [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF])
        .expect("solid_fill")
        .expect("Some(PictureHandle)");
    let gs = b
        .render_create_glyphset(None, yserver_protocol::x11::RENDER_FMT_A8)
        .expect("glyphset")
        .expect("Some");
    // One 4x4 fully-opaque A8 glyph, advance 4.
    let mut add_body: Vec<u8> = Vec::new();
    add_body.extend_from_slice(&1_u32.to_le_bytes());
    add_body.extend_from_slice(&1_u32.to_le_bytes());
    add_body.extend_from_slice(&u16::to_le_bytes(4));
    add_body.extend_from_slice(&u16::to_le_bytes(4));
    add_body.extend_from_slice(&i16::to_le_bytes(0));
    add_body.extend_from_slice(&i16::to_le_bytes(0));
    add_body.extend_from_slice(&i16::to_le_bytes(4));
    add_body.extend_from_slice(&i16::to_le_bytes(0));
    add_body.extend_from_slice(&[0xFFu8; 16]);
    b.render_add_glyphs(None, gs.as_raw(), &add_body)
        .expect("add_glyphs");
    // Pen starts at content-local (-4, 0): the first glyph lands
    // entirely in the LEFT ring, the second at content (0, 0).
    let mut items: Vec<u8> = Vec::new();
    items.extend_from_slice(&[2u8, 0, 0, 0]);
    items.extend_from_slice(&i16::to_le_bytes(-(BRD_BW as i16)));
    items.extend_from_slice(&i16::to_le_bytes(0));
    items.extend_from_slice(&[1u8, 1, 0, 0]);
    b.render_composite_glyphs(
        None,
        23, // CompositeGlyphs8
        1,  // Src
        src_pic.as_raw(),
        dst_pic.as_raw(),
        0,
        gs.as_raw(),
        0,
        0,
        &items,
        0,
        0,
    )
    .expect("render_composite_glyphs");

    // The ring keeps the background; the content glyph painted white.
    brd_assert_ring(&mut b, xid, BRD_RED, None, "RENDER glyphs");
    let out = b
        .get_image_pixels_for_tests(xid, 2, 0, 0, BRD_CW, BRD_CH, !0)
        .expect("get_image")
        .expect("Some bytes");
    assert_eq!(
        &out[0..4],
        &[0xFF, 0xFF, 0xFF, 0xFF],
        "the in-content glyph must still paint",
    );
}

/// Core TEXT destination (`ImageText8`): the background fill and the
/// glyph run both go through the content-clipped route. Needs the
/// "fixed" bitmap font; skips loudly if the font catalogue lacks it.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn border_content_clip_confines_core_text_destination() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let (_, xid) = brd_bordered_window(&mut b, BRD_RED);
    let Ok((font, _metrics)) = b.open_font(None, "fixed") else {
        eprintln!("skipping core-text clip check: 'fixed' font not found");
        return;
    };
    b.apply_draw_state(
        None,
        &DrawState {
            font: Some(font),
            ..DrawState::default()
        },
    )
    .expect("apply_draw_state");
    // ImageText8 body: drawable(4) + gc(4) + x(2) + y(2) + string.
    // Baseline at content-local (-8, BRD_CH): the run starts inside the
    // LEFT ring and the glyph rows sit in the content band, so both the
    // opaque text background and the glyph spans reach the ring.
    let text: &[u8] = b"WWWWWWWWWWWW";
    let mut body: Vec<u8> = vec![0; 12];
    body[8..10].copy_from_slice(&(-8i16).to_le_bytes());
    body[10..12].copy_from_slice(&(BRD_CH as i16).to_le_bytes());
    body.extend_from_slice(text);
    b.image_text8(
        None,
        xid,
        BRD_GREEN,
        BRD_BLUE,
        u8::try_from(text.len()).expect("len"),
        &body,
    )
    .expect("image_text8");
    brd_assert_ring(&mut b, xid, BRD_RED, None, "core text");
    // Non-vacuity: the run must actually have marked the CONTENT (the
    // opaque ImageText background is BLUE here, distinct from both the
    // window background and the glyph colour).
    let out = b
        .get_image_pixels_for_tests(xid, 2, 0, 0, BRD_CW, BRD_CH, !0)
        .expect("get_image")
        .expect("Some bytes");
    assert!(
        out.chunks_exact(4)
            .any(|px| px == brd_bgra(BRD_BLUE) || px == brd_bgra(BRD_GREEN)),
        "ImageText8 painted nothing in the content — the ring check above \
         would pass vacuously",
    );
}

/// The COMPLEMENT, and the point of the whole set: the privileged
/// backing route CAN write the ring (this is the shape step 4's ring
/// fill takes) while every client route cannot. Without this the suite
/// would pass just as well on an implementation that had simply lost
/// the ability to write there — and that would surface as a step-4 bug.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn border_privileged_backing_route_reaches_the_ring_client_routes_do_not() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let (_, xid) = brd_bordered_window(&mut b, BRD_RED);

    // PRIVILEGED: fill the whole backing BLUE — the ring included.
    assert!(
        b.fill_backing_rect_for_tests(xid, 0, 0, BRD_SW, BRD_SH, BRD_BLUE),
        "privileged backing fill must succeed",
    );
    brd_assert_ring(&mut b, xid, BRD_BLUE, Some(BRD_BLUE), "privileged write");

    // CLIENT: the same rect, in content-local coordinates, through
    // every destination family — none of them may touch the ring.
    b.fill_rectangle(
        None,
        xid,
        BRD_GREEN,
        -(BRD_BW as i16),
        -(BRD_BW as i16),
        BRD_SW as u16,
        BRD_SH as u16,
    )
    .expect("client fill");
    let px = brd_bgra(BRD_GREEN);
    let data: Vec<u8> = (0..(BRD_SW * BRD_SH))
        .flat_map(|_| px.into_iter())
        .collect();
    b.put_image(
        None,
        xid,
        32,
        BRD_SW as u16,
        BRD_SH as u16,
        -(BRD_BW as i16),
        -(BRD_BW as i16),
        &data,
    )
    .expect("client put_image");
    brd_assert_ring(&mut b, xid, BRD_BLUE, Some(BRD_GREEN), "client routes");
}

// ───── #133 step 3 round 2 — RENDER sources, CopyPlane, seeding ─────
//
// Three gaps the first pass left, all of the same family: a route that
// reaches storage with pre-border coordinates.
//
// 1. A RENDER source/mask picture resolved straight to a DrawableId
//    sampled storage `(0, 0)` — a bordered window's ring — instead of
//    its content origin.
// 2. `CopyPlane` resolved its source with a raw store lookup, so it
//    bypassed redirect routing entirely.
// 3. The privileged redirect seed / inferior-reconstruct paths wrote
//    `w x h` at `(0, 0)` into a backing that is now bordered.

/// A RENDER `Composite` whose SOURCE picture wraps a bordered WINDOW
/// must sample the window's CONTENT. Xorg reaches the same place from
/// the other side: `create_bits_picture` builds the pixman image over
/// the whole backing pixmap (`fb/fbpict.c:293-296`) and then adds
/// `pict->pDrawable->x/y` to the sampling offset (`:328-329`), which
/// for a bordered window is exactly `bw`.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn border_render_window_source_samples_content_not_the_ring() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let (w, xid) = brd_bordered_window(&mut b, BRD_GREEN);
    // Ring := BLUE (privileged), content := GREEN (client).
    assert!(
        b.fill_backing_rect_for_tests(xid, 0, 0, BRD_SW, BRD_SH, BRD_BLUE),
        "privileged ring fill",
    );
    b.fill_rectangle(None, xid, BRD_GREEN, 0, 0, BRD_CW, BRD_CH)
        .expect("client content fill");

    // Composite the window (as a source picture) onto a RED pixmap.
    let src_pic = b
        .render_create_picture(None, AnyHandle::Window(w), 0, 0, &[])
        .expect("src picture")
        .expect("Some");
    let dst = b
        .create_pixmap(None, 32, BRD_CW, BRD_CH)
        .expect("dst pixmap");
    let dst_xid = dst.as_raw();
    b.fill_rectangle(None, dst_xid, BRD_RED, 0, 0, BRD_CW, BRD_CH)
        .expect("seed dst red");
    let dst_pic = b
        .render_create_picture(None, AnyHandle::Pixmap(dst), 0, 0, &[])
        .expect("dst picture")
        .expect("Some");
    b.render_composite(
        None,
        1, // Src
        src_pic.as_raw(),
        0,
        dst_pic.as_raw(),
        0,
        0,
        0,
        0,
        0,
        0,
        BRD_CW,
        BRD_CH,
    )
    .expect("render_composite from the bordered window");

    let out = b
        .get_image_pixels_for_tests(dst_xid, 2, 0, 0, BRD_CW, BRD_CH, !0)
        .expect("get_image")
        .expect("Some bytes");
    for (i, px) in out.chunks_exact(4).enumerate() {
        assert_eq!(
            px,
            &brd_bgra(BRD_GREEN),
            "pixel {i}: a RENDER window source must sample content, not the ring",
        );
    }
}

/// The complement, and the distinction that must not be collapsed: a
/// picture made from a COMPOSITE-NAMED WINDOW PIXMAP legitimately
/// includes the ring, because that pixmap IS the bordered image —
/// `compAllocPixmap` allocates it at `w + 2bw` x `h + 2bw`
/// (`composite/compalloc.c:608-618`). Sampling it at `(0, 0)` returns
/// the ring, exactly as it must.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn border_render_named_window_pixmap_source_includes_the_ring() {
    use yserver_core::backend::WindowHandle;

    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let (w, xid) = brd_bordered_window(&mut b, BRD_GREEN);
    // Redirect W so it has a named pixmap. The backing is the bordered
    // extent — the same rule the core now applies when it activates a
    // redirect.
    let backing = b
        .allocate_redirected_backing(
            None,
            WindowHandle::from_raw(xid).expect("W handle"),
            u16::try_from(BRD_SW).expect("sw"),
            u16::try_from(BRD_SH).expect("sh"),
            32,
        )
        .expect("allocate_redirected_backing");
    let named = b.name_window_pixmap(None, w).expect("name_window_pixmap");
    assert_eq!(
        named.as_raw(),
        backing.as_raw(),
        "named pixmap IS the backing"
    );

    // Paint the whole backing BLUE through the privileged route, then
    // the window's CONTENT green through the client route. The named
    // pixmap's (0,0) is therefore ring-blue and its (bw,bw) is green.
    assert!(
        b.fill_backing_rect_for_tests(xid, 0, 0, BRD_SW, BRD_SH, BRD_BLUE),
        "privileged whole-backing fill",
    );
    b.fill_rectangle(None, xid, BRD_GREEN, 0, 0, BRD_CW, BRD_CH)
        .expect("client content fill");

    // A picture on the NAMED PIXMAP: sampling at (0,0) must return the
    // ring pixel, and at (bw,bw) the content.
    let src_pic = b
        .render_create_picture(None, AnyHandle::Pixmap(named), 0, 0, &[])
        .expect("src picture")
        .expect("Some");
    let dst = b.create_pixmap(None, 32, 2, 2).expect("dst pixmap");
    let dst_xid = dst.as_raw();
    let dst_pic = b
        .render_create_picture(None, AnyHandle::Pixmap(dst), 0, 0, &[])
        .expect("dst picture")
        .expect("Some");
    let sample_at = |b: &mut KmsBackend, sx: i16, sy: i16| -> Vec<u8> {
        b.fill_rectangle(None, dst_xid, BRD_RED, 0, 0, 2, 2)
            .expect("seed dst");
        b.render_composite(
            None,
            1,
            src_pic.as_raw(),
            0,
            dst_pic.as_raw(),
            sx,
            sy,
            0,
            0,
            0,
            0,
            1,
            1,
        )
        .expect("render_composite from the named pixmap");
        b.get_image_pixels_for_tests(dst_xid, 2, 0, 0, 1, 1, !0)
            .expect("get_image")
            .expect("Some bytes")
    };
    let at_origin = sample_at(&mut b, 0, 0);
    assert_eq!(
        &at_origin[..4],
        &brd_bgra(BRD_BLUE),
        "a named-window-pixmap source at (0,0) must return the RING — the \
         pixmap is the bordered image",
    );
    let at_content = sample_at(
        &mut b,
        i16::try_from(BRD_BW).expect("bw"),
        i16::try_from(BRD_BW).expect("bw"),
    );
    assert_eq!(
        &at_content[..4],
        &brd_bgra(BRD_GREEN),
        "…and at (bw, bw) the window's content",
    );
}

/// `RepeatNone` domain: a source rect reaching past the window's own
/// extent must contribute NOTHING, not a border-ring texel. Xorg's
/// shape for source-side restriction is
/// `miClipPictureSrc(pRegion, pSrc, xDst - xSrc, yDst - ySrc)`
/// (`render/mipict.c:353-356`).
#[test]
#[ignore = "needs live Vulkan ICD"]
fn border_render_repeat_none_source_domain_excludes_the_ring() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let (w, xid) = brd_bordered_window(&mut b, BRD_GREEN);
    assert!(
        b.fill_backing_rect_for_tests(xid, 0, 0, BRD_SW, BRD_SH, BRD_BLUE),
        "privileged ring fill",
    );
    b.fill_rectangle(None, xid, BRD_GREEN, 0, 0, BRD_CW, BRD_CH)
        .expect("client content fill");

    // Source picture on the window; RepeatNone is the default.
    let src_pic = b
        .render_create_picture(None, AnyHandle::Window(w), 0, 0, &[])
        .expect("src picture")
        .expect("Some");
    // Destination is as wide as the window, but we sample starting at
    // xSrc = CW - 2: only 2 source columns exist, the rest of the rect
    // is outside the window's own extent.
    let dst = b
        .create_pixmap(None, 32, BRD_CW, BRD_CH)
        .expect("dst pixmap");
    let dst_xid = dst.as_raw();
    b.fill_rectangle(None, dst_xid, BRD_RED, 0, 0, BRD_CW, BRD_CH)
        .expect("seed dst red");
    let dst_pic = b
        .render_create_picture(None, AnyHandle::Pixmap(dst), 0, 0, &[])
        .expect("dst picture")
        .expect("Some");
    b.render_composite(
        None,
        1, // Src — so anything painted REPLACES the red seed
        src_pic.as_raw(),
        0,
        dst_pic.as_raw(),
        i16::try_from(BRD_CW - 2).expect("xSrc"),
        0,
        0,
        0,
        0,
        0,
        BRD_CW,
        BRD_CH,
    )
    .expect("render_composite past the source's own extent");

    let out = b
        .get_image_pixels_for_tests(dst_xid, 2, 0, 0, BRD_CW, BRD_CH, !0)
        .expect("get_image")
        .expect("Some bytes");
    for y in 0..u32::from(BRD_CH) {
        for x in 0..u32::from(BRD_CW) {
            let off = ((y * u32::from(BRD_CW) + x) * 4) as usize;
            let px = &out[off..off + 4];
            assert_ne!(
                px,
                &brd_bgra(BRD_BLUE),
                "({x},{y}): RepeatNone sampled a RING texel outside the source's extent",
            );
            if x < 2 {
                assert_eq!(px, &brd_bgra(BRD_GREEN), "({x},{y}): in-domain content");
            } else {
                assert_eq!(
                    px,
                    &brd_bgra(BRD_RED),
                    "({x},{y}): outside the source domain must stay untouched",
                );
            }
        }
    }
}

/// `CopyPlane` must resolve its SOURCE through the paint target, like
/// every other read: a redirected window's pixels live in the backing
/// and its leaf storage is stale. Seeds the leaf with a plane-set
/// colour BEFORE the redirect and the backing with a plane-clear one
/// after, then checks which one the plane extraction saw.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn copy_plane_source_follows_redirect_routing() {
    use yserver_core::backend::{DrawState, WindowHandle};

    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    // Plain (bw = 0) window so this isolates redirect routing.
    let (w, xid) = {
        use yserver_core::host_x11::HostSubwindowVisual;
        let root = WindowHandle::from_raw(1).expect("root");
        let w = b
            .create_subwindow(
                None,
                root,
                0,
                0,
                4,
                4,
                0,
                HostSubwindowVisual::Explicit {
                    depth: 32,
                    visual_xid: 0,
                    colormap_xid: 0,
                },
                None,
                None,
            )
            .expect("create W");
        (w, w.as_raw())
    };
    b.map_window_for_tests(xid).expect("map");
    // Leaf := 0x…0001 in the low bit (plane 1 SET).
    b.fill_rectangle(None, xid, 0xFF00_0001, 0, 0, 4, 4)
        .expect("seed leaf plane-set");
    // Redirect, then paint the backing with the low bit CLEAR.
    let _backing = b
        .allocate_redirected_backing(None, w, 4, 4, 32)
        .expect("allocate_redirected_backing");
    b.fill_rectangle(None, xid, 0xFF00_0000, 0, 0, 4, 4)
        .expect("paint backing plane-clear");

    // fg / bg so the two outcomes are distinguishable.
    b.apply_draw_state(
        None,
        &DrawState {
            foreground: BRD_RED,
            background: BRD_GREEN,
            ..DrawState::default()
        },
    )
    .expect("apply_draw_state");
    let dst = b.create_pixmap(None, 32, 4, 4).expect("dst pixmap");
    let dst_xid = dst.as_raw();
    b.fill_rectangle(None, dst_xid, BRD_BLUE, 0, 0, 4, 4)
        .expect("seed dst");
    b.copy_plane(None, xid, dst_xid, 0, 0, 0, 0, 4, 4, 0x1)
        .expect("copy_plane");

    let out = b
        .get_image_pixels_for_tests(dst_xid, 2, 0, 0, 4, 4, !0)
        .expect("get_image")
        .expect("Some bytes");
    for (i, px) in out.chunks_exact(4).enumerate() {
        assert_eq!(
            px,
            &brd_bgra(BRD_GREEN),
            "pixel {i}: CopyPlane must read the REDIRECT BACKING (plane clear \
             → background), not the stale leaf (plane set → foreground)",
        );
    }
}

/// Bordered redirect seeding: activating a redirect on a bordered
/// window must place the window's inherited image at the CONTENT
/// origin `(bw, bw)` of the bordered backing — Xorg
/// `compSetPixmap(pWin, pPixmap, bw)`
/// (`composite/compalloc.c:620`) — not shifted to `(0, 0)`.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn border_redirect_seed_places_content_at_the_content_origin() {
    use yserver_core::backend::WindowHandle;

    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let (w, xid) = brd_bordered_window(&mut b, BRD_RED);
    // Distinguishable leaf state: ring BLUE (privileged), content
    // GREEN (client). Map it so the inferior walk includes it.
    assert!(
        b.fill_backing_rect_for_tests(xid, 0, 0, BRD_SW, BRD_SH, BRD_BLUE),
        "privileged ring fill",
    );
    b.fill_rectangle(None, xid, BRD_GREEN, 0, 0, BRD_CW, BRD_CH)
        .expect("client content fill");
    b.map_window_for_tests(xid).expect("map");
    // The map repaints the background over the content, so restore the
    // marker colour after mapping.
    b.fill_rectangle(None, xid, BRD_GREEN, 0, 0, BRD_CW, BRD_CH)
        .expect("client content fill (post-map)");

    // Activate the redirect at the BORDERED extent, exactly as the core
    // now does (`compAllocPixmap`, `composite/compalloc.c:608-618`).
    let backing = b
        .allocate_redirected_backing(
            None,
            WindowHandle::from_raw(xid).expect("W handle"),
            u16::try_from(BRD_SW).expect("sw"),
            u16::try_from(BRD_SH).expect("sh"),
            32,
        )
        .expect("allocate_redirected_backing");
    let _ = w;

    let out = b
        .get_image_pixels_for_tests(
            backing.as_raw(),
            2,
            0,
            0,
            u16::try_from(BRD_SW).expect("sw"),
            u16::try_from(BRD_SH).expect("sh"),
            !0,
        )
        .expect("get_image backing")
        .expect("Some bytes");
    let at = |x: u32, y: u32| {
        let off = ((y * BRD_SW + x) * 4) as usize;
        out[off..off + 4].to_vec()
    };
    // The window's own content must land at (bw, bw) …
    assert_eq!(
        at(u32::from(BRD_BW), u32::from(BRD_BW)),
        brd_bgra(BRD_GREEN),
        "the inherited image must sit at the CONTENT origin (bw, bw)",
    );
    assert_eq!(
        at(
            BRD_SW - u32::from(BRD_BW) - 1,
            BRD_SH - u32::from(BRD_BW) - 1
        ),
        brd_bgra(BRD_GREEN),
        "…and reach the far content edge — a bordered backing sized \
         w x h would have clipped it",
    );
    // … and NOT be shifted up-left into the ring band, which is what
    // seeding `w x h` at (0, 0) produced.
    assert_ne!(
        at(0, 0),
        brd_bgra(BRD_GREEN),
        "content must not be shifted to the backing origin",
    );
}

// ───── #133 step 3 round 3 — RENDER sources under redirect ─────
//
// `resolve_picture_for_render` started from a raw `store.lookup`, so a
// source picture on a window sampled that window's LEAF storage. For a
// redirected window the leaf is stale (its pixels live in the backing —
// the same premise `copy_plane_source_follows_redirect_routing` pins),
// and for a child below a redirected ancestor the sampling origin needs
// the whole `W.bw + C.x + C.bw` chain, not one level's `border_width`.
// Both now come from `resolve_paint_target`.
//
// Each test paints THREE distinguishable colours so a failure says
// which half broke: the correct sample, the wrong-offset sample, and
// the wrong-drawable (stale leaf) sample.

/// 1×1 `PictOpSrc` composite from `src_pic` at `(sx, sy)` onto a fresh
/// pixmap seeded with `BRD_RED`; returns the resulting BGRA pixel.
fn brd_sample_picture(
    b: &mut KmsBackend,
    src_pic: yserver_core::backend::PictureHandle,
    sx: i16,
    sy: i16,
) -> Vec<u8> {
    let dst = b.create_pixmap(None, 32, 1, 1).expect("dst pixmap");
    let dst_xid = dst.as_raw();
    b.fill_rectangle(None, dst_xid, BRD_RED, 0, 0, 1, 1)
        .expect("seed dst");
    let dst_pic = b
        .render_create_picture(None, AnyHandle::Pixmap(dst), 0, 0, &[])
        .expect("dst picture")
        .expect("Some");
    b.render_composite(
        None,
        1, // Src
        src_pic.as_raw(),
        0,
        dst_pic.as_raw(),
        sx,
        sy,
        0,
        0,
        0,
        0,
        1,
        1,
    )
    .expect("render_composite");
    b.get_image_pixels_for_tests(dst_xid, 2, 0, 0, 1, 1, !0)
        .expect("get_image")
        .expect("Some bytes")
}

/// A Picture wrapping a REDIRECTED window must sample the backing, at
/// the content origin. Three outcomes are distinguishable:
/// GREEN = backing content (correct), RED = backing ring (routing right,
/// offset wrong), BLUE = stale leaf (routing wrong).
#[test]
#[ignore = "needs live Vulkan ICD"]
fn border_render_source_on_redirected_window_samples_the_backing() {
    use yserver_core::backend::WindowHandle;

    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let (w, xid) = brd_bordered_window(&mut b, BRD_RED);
    // Pre-redirect: the LEAF's content is BLUE. Nothing may sample it
    // once the redirect is active.
    b.fill_rectangle(None, xid, BRD_BLUE, 0, 0, BRD_CW, BRD_CH)
        .expect("leaf content blue");

    let backing = b
        .allocate_redirected_backing(
            None,
            WindowHandle::from_raw(xid).expect("W handle"),
            u16::try_from(BRD_SW).expect("sw"),
            u16::try_from(BRD_SH).expect("sh"),
            32,
        )
        .expect("allocate_redirected_backing");
    assert_ne!(backing.as_raw(), xid, "backing is a distinct drawable");

    // Backing: whole allocation RED (privileged — ring included), then
    // the window's CONTENT green through the client route, which now
    // lands in the backing at (bw, bw).
    assert!(
        b.fill_backing_rect_for_tests(xid, 0, 0, BRD_SW, BRD_SH, BRD_RED),
        "privileged whole-backing fill",
    );
    b.fill_rectangle(None, xid, BRD_GREEN, 0, 0, BRD_CW, BRD_CH)
        .expect("backing content green");

    let src_pic = b
        .render_create_picture(None, AnyHandle::Window(w), 0, 0, &[])
        .expect("src picture")
        .expect("Some");
    let px = brd_sample_picture(&mut b, src_pic, 0, 0);
    assert_ne!(
        &px[..4],
        &brd_bgra(BRD_BLUE),
        "a source picture on a redirected window sampled the STALE LEAF",
    );
    assert_eq!(
        &px[..4],
        &brd_bgra(BRD_GREEN),
        "xSrc = 0 must sample the backing's CONTENT origin (got {:?}; \
         RED would mean the ring)",
        &px[..4],
    );
    // The far content corner too, so the offset is not merely
    // coincidentally right at the origin.
    let far = brd_sample_picture(
        &mut b,
        src_pic,
        i16::try_from(BRD_CW - 1).expect("cw"),
        i16::try_from(BRD_CH - 1).expect("ch"),
    );
    assert_eq!(
        &far[..4],
        &brd_bgra(BRD_GREEN),
        "the last content pixel must still be inside the sampled content",
    );
}

/// A Picture on a CHILD below a redirected ancestor needs the whole
/// accumulated chain — `W.bw + C.x + C.bw` — as its sampling origin,
/// not one level's `border_width`. GREEN = correct, RED = elsewhere in
/// the backing (wrong offset), BLUE = the child's stale leaf.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn border_render_source_on_child_of_redirected_ancestor_accumulates_the_offset() {
    use yserver_core::{backend::WindowHandle, host_x11::HostSubwindowVisual};

    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    // W: 32x16 content, bw 8 → backing 48x32, W content at (8, 8).
    // C: 8x4 content, bw 2, at (10, 4) inside W's content →
    //    C content origin in the backing = 8 + 10 + 2 = 20,
    //                                      8 +  4 + 2 = 14.
    const W_CW: u16 = 32;
    const W_CH: u16 = 16;
    const W_BW: u16 = 8;
    const C_CW: u16 = 8;
    const C_CH: u16 = 4;
    const C_BW: u16 = 2;
    const C_X: i16 = 10;
    const C_Y: i16 = 4;
    let w_sw = u32::from(W_CW) + 2 * u32::from(W_BW);
    let w_sh = u32::from(W_CH) + 2 * u32::from(W_BW);
    let expect_off_x = u32::from(W_BW) + u32::try_from(C_X).expect("cx") + u32::from(C_BW);
    let expect_off_y = u32::from(W_BW) + u32::try_from(C_Y).expect("cy") + u32::from(C_BW);
    assert_eq!((expect_off_x, expect_off_y), (20, 14), "worked example");

    let visual = HostSubwindowVisual::Explicit {
        depth: 32,
        visual_xid: 0,
        colormap_xid: 0,
    };
    let root = WindowHandle::from_raw(1).expect("root");
    let w = b
        .create_subwindow(
            None,
            root,
            0,
            0,
            W_CW,
            W_CH,
            W_BW,
            visual,
            Some(BRD_RED),
            None,
        )
        .expect("create W");
    let w_xid = w.as_raw();
    let c = b
        .create_subwindow(
            None,
            w,
            C_X,
            C_Y,
            C_CW,
            C_CH,
            C_BW,
            visual,
            Some(BRD_BLUE),
            None,
        )
        .expect("create C");
    let c_xid = c.as_raw();
    b.map_window_for_tests(w_xid).expect("map W");
    b.map_window_for_tests(c_xid).expect("map C");

    // Redirect W at the bordered extent.
    let _backing = b
        .allocate_redirected_backing(
            None,
            WindowHandle::from_raw(w_xid).expect("W handle"),
            u16::try_from(w_sw).expect("sw"),
            u16::try_from(w_sh).expect("sh"),
            32,
        )
        .expect("allocate_redirected_backing");

    // Whole backing RED (privileged), then C's content GREEN through the
    // client route — which lands at (20, 14) in the backing.
    assert!(
        b.fill_backing_rect_for_tests(w_xid, 0, 0, w_sw, w_sh, BRD_RED),
        "privileged whole-backing fill",
    );
    b.fill_rectangle(None, c_xid, BRD_GREEN, 0, 0, C_CW, C_CH)
        .expect("C content green");

    // Sanity on the destination side, so a failure below is attributable
    // to the SOURCE path: the client fill really did land at (20, 14).
    let (sw, sh, backing_px) = b
        .backing_pixels_for_tests(w_xid)
        .expect("privileged backing read");
    assert_eq!((sw, sh), (w_sw, w_sh));
    let at = |x: u32, y: u32| {
        let off = ((y * sw + x) * 4) as usize;
        backing_px[off..off + 4].to_vec()
    };
    assert_eq!(
        at(expect_off_x, expect_off_y),
        brd_bgra(BRD_GREEN),
        "destination side: C's content must land at (20, 14)",
    );
    assert_eq!(
        at(u32::from(C_BW), u32::from(C_BW)),
        brd_bgra(BRD_RED),
        "…and NOT at a single level's (bw, bw)",
    );

    // Now the source side.
    let src_pic = b
        .render_create_picture(None, AnyHandle::Window(c), 0, 0, &[])
        .expect("src picture")
        .expect("Some");
    let px = brd_sample_picture(&mut b, src_pic, 0, 0);
    assert_ne!(
        &px[..4],
        &brd_bgra(BRD_BLUE),
        "sampled C's STALE LEAF instead of the ancestor backing",
    );
    assert_eq!(
        &px[..4],
        &brd_bgra(BRD_GREEN),
        "xSrc = 0 on a child of a redirected ancestor must sample \
         W.bw + C.x + C.bw = (20, 14); got {:?} (RED = wrong offset)",
        &px[..4],
    );
    // RepeatNone domain: one pixel past C's own extent contributes
    // nothing, so the destination keeps its RED seed rather than
    // picking up a neighbouring backing texel.
    let past = brd_sample_picture(&mut b, src_pic, i16::try_from(C_CW).expect("cw"), 0);
    assert_eq!(
        &past[..4],
        &brd_bgra(BRD_RED),
        "a RepeatNone sample past the child's own extent must contribute nothing",
    );
}

// ───── #133 step 3 round 4 — the xts Xlib4/XSetWindowBackgroundPixmap
//       purpose-2 crash ─────
//
// That purpose is the ONLY Xlib4 purpose that sets a border width on
// the window it then clears and reads:
//
//     XSetWindowBorderWidth(display, w, 2);
//     ... XClearWindow(display, w);
//     checktile(display, w, 0, -ap.x-border_width, -ap.y-border_width, pm)
//
// and `checktile` (xts5/src/lib/checktile.c:160-183) does
// `getsize()` → `XGetImage(d, 0, 0, width, height, …)` → `XGetPixel`
// over the full `width x height`. A GetImage reply shorter than the
// requested rectangle therefore walks `XGetPixel` off the end of the
// buffer libX11 allocated from the reply length — SIGSEGV in the
// CLIENT, not in the server.
//
// Two defects produced that short reply, and both are pinned here.

/// DEFECT A (the crash itself): a GetImage reply must always describe
/// the rectangle the client asked for. Truncating it to whatever the
/// read bounds allowed is a protocol violation — libX11 sizes the
/// XImage buffer from the reply length but indexes it with the
/// REQUESTED width/height (`checktile` → `XGetPixel`), so a short
/// reply is an out-of-bounds read in the client.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn get_image_reply_always_covers_the_requested_rectangle() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let (_, xid) = brd_bordered_window(&mut b, BRD_GREEN);
    let full = usize::from(BRD_CW) * usize::from(BRD_CH) * 4;

    // The whole window: the reply must be exactly w*h pixels.
    let out = b
        .get_image_pixels_for_tests(xid, 2, 0, 0, BRD_CW, BRD_CH, !0)
        .expect("get_image")
        .expect("Some bytes");
    assert_eq!(out.len(), full, "in-bounds GetImage must be full length");

    // A rectangle reaching past the content on every side: still
    // exactly as many pixels as were asked for. The parts outside the
    // drawable are undefined per the protocol, but they must be
    // PRESENT — and they must not be ring pixels.
    let over_w = BRD_CW + 2 * BRD_BW;
    let over_h = BRD_CH + 2 * BRD_BW;
    let over = b
        .get_image_pixels_for_tests(
            xid,
            2,
            -(BRD_BW as i16),
            -(BRD_BW as i16),
            over_w,
            over_h,
            !0,
        )
        .expect("get_image")
        .expect("Some bytes");
    assert_eq!(
        over.len(),
        usize::from(over_w) * usize::from(over_h) * 4,
        "an out-of-range GetImage must still return the requested \
         rectangle's worth of data, not a short reply",
    );
    for (i, px) in over.chunks_exact(4).enumerate() {
        assert_ne!(
            px,
            &brd_bgra(BRD_BLUE),
            "pixel {i} leaked a ring texel into a GetImage reply",
        );
    }
}
