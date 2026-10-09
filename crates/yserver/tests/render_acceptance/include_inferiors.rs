use super::*;

/// Mirrors XTS XFillRectangle TP27's root-window special case:
/// drawing on the root with `IncludeInferiors` must update the
/// overlapping top-level and descendant windows exactly as if the
/// draw had targeted the top-level directly.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn root_fill_with_include_inferiors_matches_top_level_result() {
    use yserver_core::{
        backend::WindowHandle,
        host_x11::{HostSubwindowConfig, HostSubwindowVisual},
    };

    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let root = WindowHandle::from_raw(1).expect("root");
    let top = b
        .create_subwindow(
            None,
            root,
            11,
            7,
            100,
            90,
            0,
            HostSubwindowVisual::Explicit {
                depth: 32,
                visual_xid: 0,
                colormap_xid: 0,
            },
            None,
            None,
        )
        .expect("top-level");
    let top_xid = top.as_raw();
    b.map_window_for_tests(top_xid).expect("map top");

    b.fill_rectangle(None, top_xid, 0x0000_0000, 0, 0, 100, 90)
        .expect("clear top");
    b.fill_rectangle(None, top_xid, 0x0000_00ff, 20, 30, 70, 30)
        .expect("baseline fill");
    let expected = b
        .get_image_pixels_for_tests(top_xid, 2, 0, 0, 100, 90, !0)
        .expect("baseline get_image")
        .expect("baseline bytes");

    b.fill_rectangle(None, top_xid, 0x0000_0000, 0, 0, 100, 90)
        .expect("re-clear top");

    for i in 0..4 {
        let child = b
            .create_subwindow(
                None,
                top,
                (i * 20) as i16,
                0,
                10,
                90,
                0,
                HostSubwindowVisual::Explicit {
                    depth: 32,
                    visual_xid: 0,
                    colormap_xid: 0,
                },
                None,
                None,
            )
            .expect("strip child");
        b.map_window_for_tests(child.as_raw()).expect("map child");
        for j in 0..9 {
            let grandchild = b
                .create_subwindow(
                    None,
                    child,
                    0,
                    (j * 10) as i16,
                    10,
                    6,
                    0,
                    HostSubwindowVisual::Explicit {
                        depth: 32,
                        visual_xid: 0,
                        colormap_xid: 0,
                    },
                    None,
                    None,
                )
                .expect("strip grandchild");
            b.map_window_for_tests(grandchild.as_raw())
                .expect("map grandchild");
        }
    }

    b.apply_draw_state(
        None,
        &DrawState {
            subwindow_mode: SubwindowMode::IncludeInferiors,
            ..DrawState::default()
        },
    )
    .expect("apply include inferiors");

    b.fill_rectangle(None, top_xid, 0x0000_00ff, 20, 30, 70, 30)
        .expect("top fill include inferiors");
    let top_include_out = b
        .get_image_pixels_for_tests(top_xid, 2, 0, 0, 100, 90, !0)
        .expect("top include get_image")
        .expect("top include bytes");
    assert_eq!(top_include_out, expected);

    b.fill_rectangle(None, top_xid, 0x0000_0000, 0, 0, 100, 90)
        .expect("re-clear top after include inferiors");

    b.configure_subwindow(
        None,
        top_xid,
        HostSubwindowConfig {
            x: Some(0),
            y: Some(0),
            width: None,
            height: None,
            border_width: Some(0),
            sibling: None,
            stack_mode: None,
        },
    )
    .expect("move top to root origin");

    b.fill_rectangle(None, root.as_raw(), 0x0000_00ff, 20, 30, 70, 30)
        .expect("root fill include inferiors");

    let out = b
        .get_image_pixels_for_tests(top_xid, 2, 0, 0, 100, 90, !0)
        .expect("root-path get_image")
        .expect("root-path bytes");
    assert_eq!(out, expected);
}

/// Pack one `PolyRectangle` rect into X11 wire bytes:
/// x:i16, y:i16, w:u16, h:u16 — all little-endian.
fn pack_rect(x: i16, y: i16, w: u16, h: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(8);
    out.extend_from_slice(&x.to_le_bytes());
    out.extend_from_slice(&y.to_le_bytes());
    out.extend_from_slice(&w.to_le_bytes());
    out.extend_from_slice(&h.to_le_bytes());
    out
}

/// Import-selection regression (positive): a stroke drawn on the ROOT
/// with `subwindow_mode = IncludeInferiors` must also paint into a
/// redirected top-level window's own backing, so the selection
/// rectangle is visible over the composited window (not just in
/// root's occluded backing). ImageMagick `import` draws its XOR
/// selection rect this way.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn stroke_on_root_include_inferiors_reaches_redirected_toplevel_backing() {
    use yserver_core::{backend::WindowHandle, host_x11::HostSubwindowVisual};

    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let root = WindowHandle::from_raw(1).expect("root");
    // Top-level W: 200×200 at screen (100, 100), depth 24.
    let w = b
        .create_subwindow(
            None,
            root,
            100,
            100,
            200,
            200,
            0,
            HostSubwindowVisual::Explicit {
                depth: 24,
                visual_xid: 0,
                colormap_xid: 0,
            },
            None,
            None,
        )
        .expect("top-level W");
    let w_xid = w.as_raw();
    b.map_window_for_tests(w_xid).expect("map W");

    // Redirected backing for W. `get_image(w_xid)` now reads this.
    let backing = b.create_pixmap(None, 24, 200, 200).expect("W backing");
    assert!(
        b.test_set_redirected_target(w_xid, backing.as_raw()),
        "redirect route must be recorded"
    );

    // Clear W's routed backing.
    b.fill_rectangle(None, w_xid, 0x0000_0000, 0, 0, 200, 200)
        .expect("clear W backing");

    // GC: IncludeInferiors + Copy, foreground red.
    b.apply_draw_state(
        None,
        &DrawState {
            subwindow_mode: SubwindowMode::IncludeInferiors,
            function: GcFunction::Copy,
            foreground: 0x00FF_0000,
            ..DrawState::default()
        },
    )
    .expect("apply include-inferiors copy");

    // PolyRectangle on ROOT spanning W exactly: (100, 100, 200, 200).
    let rect = pack_rect(100, 100, 200, 200);
    b.poly_rectangle(None, root.as_raw(), 0x00FF_0000, &rect)
        .expect("poly_rectangle on root");

    // W's routed backing top row must carry the red top edge.
    let out = b
        .get_image_pixels_for_tests(w_xid, 2, 0, 0, 200, 1, !0)
        .expect("get_image W")
        .expect("Some bytes");
    let hit = out
        .chunks_exact(4)
        .any(|px| u32::from_le_bytes([px[0], px[1], px[2], px[3]]) & 0x00FF_FFFF == 0x00FF_0000);
    assert!(
        hit,
        "rectangle top edge (red) must land in the redirected top-level backing"
    );
}

/// Import-selection regression (guard): a SINGLE-pass XOR/invert
/// stroke on the ROOT with `IncludeInferiors` must invert each
/// resolved backing EXACTLY once, even where the stroke also crosses a
/// non-redirected child that routes into the same backing. The naive
/// recursion (emit per visited window) would invert those overlap
/// pixels twice → cancel → a gap. A draw+erase round-trip cannot catch
/// this (it is symmetric), so we assert uniform single-pass coverage.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn stroke_root_xor_include_inferiors_no_gap_over_subwindow() {
    use yserver_core::{backend::WindowHandle, host_x11::HostSubwindowVisual};

    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let root = WindowHandle::from_raw(1).expect("root");
    // Redirected top-level W: 200×200 at screen (100, 100), depth 24.
    let w = b
        .create_subwindow(
            None,
            root,
            100,
            100,
            200,
            200,
            0,
            HostSubwindowVisual::Explicit {
                depth: 24,
                visual_xid: 0,
                colormap_xid: 0,
            },
            None,
            None,
        )
        .expect("top-level W");
    let w_xid = w.as_raw();
    b.map_window_for_tests(w_xid).expect("map W");

    let backing = b.create_pixmap(None, 24, 200, 200).expect("W backing");
    assert!(
        b.test_set_redirected_target(w_xid, backing.as_raw()),
        "redirect route must be recorded"
    );

    // Clear W's routed backing to 0.
    b.fill_rectangle(None, w_xid, 0x0000_0000, 0, 0, 200, 200)
        .expect("clear W backing");

    // Non-redirected child C at W-local (0, 50), size 40×40. The
    // rectangle's LEFT edge (screen x=100 → W-local x=0) runs down
    // through C's rows, so C and W both route into `backing` there.
    let c = b
        .create_subwindow(
            None,
            w,
            0,
            50,
            40,
            40,
            0,
            HostSubwindowVisual::Explicit {
                depth: 24,
                visual_xid: 0,
                colormap_xid: 0,
            },
            None,
            None,
        )
        .expect("child C");
    b.map_window_for_tests(c.as_raw()).expect("map C");

    // GC: IncludeInferiors + Invert.
    b.apply_draw_state(
        None,
        &DrawState {
            subwindow_mode: SubwindowMode::IncludeInferiors,
            function: GcFunction::Invert,
            foreground: 0x00FF_FFFF,
            ..DrawState::default()
        },
    )
    .expect("apply include-inferiors invert");

    // ONE PolyRectangle on root spanning W.
    let rect = pack_rect(100, 100, 200, 200);
    b.poly_rectangle(None, root.as_raw(), 0x00FF_FFFF, &rect)
        .expect("poly_rectangle on root");

    // Left-edge column of W's backing: every interior pixel inverted
    // 0 → white for the full height, INCLUDING the rows overlapping C
    // (W-local y 50..90). Under the naive per-child recursion those
    // overlap pixels would be inverted twice → 0 → a gap. Under the
    // shipped dedup they are inverted exactly once → white.
    //
    // The two extreme rows (y=0 top-left, y=199 bottom-left) are the
    // rectangle-outline CORNERS: each corner pixel is emitted by two
    // adjacent edge segments, so under XOR/Invert it self-cancels to 0.
    // That is inherent to an XOR rectangle outline (same on Xorg) and
    // is orthogonal to the inferior-dedup this test guards, so the two
    // corner rows are excluded. Every non-corner row — the whole C
    // band included — must be uniformly inverted.
    let out = b
        .get_image_pixels_for_tests(w_xid, 2, 0, 0, 1, 200, !0)
        .expect("get_image W column")
        .expect("Some bytes");
    let rows: Vec<u32> = out
        .chunks_exact(4)
        .map(|px| u32::from_le_bytes([px[0], px[1], px[2], px[3]]) & 0x00FF_FFFF)
        .collect();
    assert_eq!(rows.len(), 200, "expected 200-pixel column");
    for (row, &v) in rows.iter().enumerate() {
        if row == 0 || row == 199 {
            continue; // outline corner: XOR self-cancels (see above)
        }
        assert_eq!(
            v, 0x00FF_FFFF,
            "row {row}: expected single-pass invert (no double-invert gap over sub-window)"
        );
    }
}

// ───── #133 step 3 round 5 — the IncludeInferiors regression ─────
//
// xts5 Xlib9 regressed 14 purposes, one per drawing test case, all of
// them the `subwindow_mode = IncludeInferiors` assertion (e.g.
// XFillRectangle tp27, xts5/Xlib9/XFillRectangle/XFillRectangle.c:2854).
// Their shape is identical:
//
//   1. draw on a childless window, `savimage()` it
//      (= XGetImage(w, 0, 0, width, height), xts5/src/lib/savimage.c:131)
//   2. clear, set IncludeInferiors, create strip children AND
//      grandchildren inside them
//   3. draw again
//   4. `compsavimage()` — GetImage the PARENT again and compare
//
// So the assertion is entirely about the PARENT's own pixels: they must
// come out the same whether or not children exist. The existing
// IncludeInferiors acceptance tests all assert the opposite half — that
// the fan-out REACHES the children — which is why they missed this.

/// The parent's own storage must be unaffected by the presence of
/// mapped children under `IncludeInferiors`, including grandchildren.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn include_inferiors_fill_leaves_the_parent_pixels_unchanged() {
    use yserver_core::{
        backend::{DrawState, SubwindowMode, WindowHandle},
        host_x11::HostSubwindowVisual,
    };

    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    const PW: u16 = 32;
    const PH: u16 = 16;
    let visual = HostSubwindowVisual::Explicit {
        depth: 32,
        visual_xid: 0,
        colormap_xid: 0,
    };
    let root = WindowHandle::from_raw(1).expect("root");

    // Reference: fill a childless window and save its pixels.
    // xts's `makewin` creates every test window with border_width = 1
    // (xts5/src/lib/makewin2.c:232).
    const XTS_BW: u16 = 1;
    let bare = b
        .create_subwindow(
            None,
            root,
            0,
            0,
            PW,
            PH,
            XTS_BW,
            visual,
            Some(BRD_RED),
            None,
        )
        .expect("create bare");
    b.map_window_for_tests(bare.as_raw()).expect("map bare");
    b.apply_draw_state(
        None,
        &DrawState {
            subwindow_mode: SubwindowMode::IncludeInferiors,
            ..DrawState::default()
        },
    )
    .expect("apply_draw_state");
    b.fill_rectangle(None, bare.as_raw(), BRD_GREEN, 0, 0, PW, PH)
        .expect("fill bare");
    let expected = b
        .get_image_pixels_for_tests(bare.as_raw(), 2, 0, 0, PW, PH, !0)
        .expect("get_image bare")
        .expect("Some bytes");
    assert!(
        expected.chunks_exact(4).all(|px| px == brd_bgra(BRD_GREEN)),
        "reference fill must cover the whole childless window",
    );

    // Same window shape, but with strip children and grandchildren
    // over parts of it (the purpose's exact construction).
    let parent = b
        .create_subwindow(
            None,
            root,
            0,
            0,
            PW,
            PH,
            XTS_BW,
            visual,
            Some(BRD_RED),
            None,
        )
        .expect("create parent");
    b.map_window_for_tests(parent.as_raw()).expect("map parent");
    for i in 0..2u16 {
        let strip = b
            .create_subwindow(
                None,
                parent,
                i16::try_from(i * 16).expect("x"),
                0,
                8,
                PH,
                0,
                visual,
                Some(BRD_BLUE),
                None,
            )
            .expect("create strip");
        b.map_window_for_tests(strip.as_raw()).expect("map strip");
        for j in (0..PH).step_by(6) {
            let grand = b
                .create_subwindow(
                    None,
                    strip,
                    0,
                    i16::try_from(j).expect("y"),
                    8,
                    3,
                    0,
                    visual,
                    Some(BRD_BLUE),
                    None,
                )
                .expect("create grandchild");
            b.map_window_for_tests(grand.as_raw()).expect("map grand");
        }
    }
    b.apply_draw_state(
        None,
        &DrawState {
            subwindow_mode: SubwindowMode::IncludeInferiors,
            ..DrawState::default()
        },
    )
    .expect("apply_draw_state");
    b.fill_rectangle(None, parent.as_raw(), BRD_GREEN, 0, 0, PW, PH)
        .expect("fill parent");

    let got = b
        .get_image_pixels_for_tests(parent.as_raw(), 2, 0, 0, PW, PH, !0)
        .expect("get_image parent")
        .expect("Some bytes");
    for y in 0..u32::from(PH) {
        for x in 0..u32::from(PW) {
            let off = ((y * u32::from(PW) + x) * 4) as usize;
            assert_eq!(
                &got[off..off + 4],
                &brd_bgra(BRD_GREEN),
                "({x},{y}): inferiors affected the PARENT's own pixels under \
                 IncludeInferiors",
            );
        }
    }
}

/// The other half of those purposes, and the one that actually moved:
/// a draw on the ROOT with `IncludeInferiors` must be visible in a
/// top-level window's own storage. Xorg has one framebuffer, so
/// painting the root over the window's area writes those pixels and
/// reading the window back returns them; yserver reproduces that by
/// fanning the draw out into each descendant's storage. The purpose
/// verifies it exactly that way — it drops A_DRAW to the root origin,
/// fills the ROOT, then `compsavimage()`s the WINDOW
/// (xts5/Xlib9/XFillRectangle/XFillRectangle.c:2922-2950).
#[test]
#[ignore = "needs live Vulkan ICD"]
fn include_inferiors_root_fill_reaches_a_top_level_windows_storage() {
    use yserver_core::{
        backend::{DrawState, SubwindowMode, WindowHandle},
        host_x11::HostSubwindowVisual,
    };

    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    const PW: u16 = 32;
    const PH: u16 = 16;
    let visual = HostSubwindowVisual::Explicit {
        depth: 32,
        visual_xid: 0,
        colormap_xid: 0,
    };
    let root = WindowHandle::from_raw(1).expect("root");
    // Top-level at the root origin, like the purpose's XMoveWindow(0, 0).
    // xts's `makewin` uses border_width = 1
    // (xts5/src/lib/makewin2.c:232); the purpose then drops it to 0
    // (`XSetWindowBorderWidth(A_DISPLAY, A_DRAW, 0)`) before moving the
    // window to the root origin and filling the root.
    let w = b
        .create_subwindow(None, root, 0, 0, PW, PH, 1, visual, Some(BRD_RED), None)
        .expect("create top-level");
    b.map_window_for_tests(w.as_raw()).expect("map");
    b.configure_subwindow(
        None,
        w.as_raw(),
        yserver_core::host_x11::HostSubwindowConfig {
            border_width: Some(0),
            ..yserver_core::host_x11::HostSubwindowConfig::default()
        },
    )
    .expect("border width 0");

    b.apply_draw_state(
        None,
        &DrawState {
            subwindow_mode: SubwindowMode::IncludeInferiors,
            ..DrawState::default()
        },
    )
    .expect("apply_draw_state");
    // Fill the ROOT over the window's area.
    b.fill_rectangle(None, b.window_id(), BRD_GREEN, 0, 0, PW, PH)
        .expect("fill root");

    let got = b
        .get_image_pixels_for_tests(w.as_raw(), 2, 0, 0, PW, PH, !0)
        .expect("get_image window")
        .expect("Some bytes");
    for y in 0..u32::from(PH) {
        for x in 0..u32::from(PW) {
            let off = ((y * u32::from(PW) + x) * 4) as usize;
            assert_eq!(
                &got[off..off + 4],
                &brd_bgra(BRD_GREEN),
                "({x},{y}): a root fill with IncludeInferiors did not reach \
                 the top-level window's storage",
            );
        }
    }
}

/// The exact xts sequence, with the exact numbers. `makewin` creates
/// the test window 100x90 with `border_width = 1`
/// (xts5/src/lib/makewin2.c:232, W_STDWIDTH/W_STDHEIGHT); the fill rect
/// is `(20, 30, 70, 30)` (`setargs`, XFillRectangle.c:1571-1580); and
/// the reported failure is `Pixel mismatch at (90, 31) (0 - 1)` —
/// W_BG = 0 expected, W_FG = 1 obtained. `(90, 31)` is the column
/// immediately RIGHT of the rect, one row down: exactly the union of
/// the rect and the same rect shifted by +1, i.e. a stale copy of the
/// pre-border-change fill.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn include_inferiors_root_fill_after_border_width_change_has_no_stale_copy() {
    use yserver_core::{
        backend::{DrawState, SubwindowMode, WindowHandle},
        host_x11::{HostSubwindowConfig, HostSubwindowVisual},
    };

    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    const W: u16 = 100;
    const H: u16 = 90;
    const RX: i16 = 20;
    const RY: i16 = 30;
    const RW: u16 = 70;
    const RH: u16 = 30;
    let visual = HostSubwindowVisual::Explicit {
        depth: 32,
        visual_xid: 0,
        colormap_xid: 0,
    };
    let root = WindowHandle::from_raw(1).expect("root");
    let w = b
        .create_subwindow(None, root, 10, 5, W, H, 1, visual, Some(BRD_RED), None)
        .expect("create test window");
    let xid = w.as_raw();
    b.map_window_for_tests(xid).expect("map");
    b.apply_draw_state(
        None,
        &DrawState {
            subwindow_mode: SubwindowMode::IncludeInferiors,
            ..DrawState::default()
        },
    )
    .expect("apply_draw_state");

    // (1) Fill the window itself, then save its pixels.
    b.fill_rectangle(None, xid, BRD_GREEN, RX, RY, RW, RH)
        .expect("fill window");
    let saved = b
        .get_image_pixels_for_tests(xid, 2, 0, 0, W, H, !0)
        .expect("get_image")
        .expect("Some bytes");

    // (2) dclear, then drop the border width to 0 and move to the root
    //     origin — the purpose's XSetWindowBorderWidth / XMoveWindow.
    b.clear_area(None, xid, BRD_RED, None, 0, 0, W, H, (0, 0))
        .expect("dclear");
    b.configure_subwindow(
        None,
        xid,
        HostSubwindowConfig {
            x: Some(0),
            y: Some(0),
            border_width: Some(0),
            ..HostSubwindowConfig::default()
        },
    )
    .expect("border width 0 + move");

    // (3) Fill the ROOT with IncludeInferiors over the same rect.
    b.fill_rectangle(None, b.window_id(), BRD_GREEN, RX, RY, RW, RH)
        .expect("fill root");

    // (4) Compare, the way compsavimage does.
    let got = b
        .get_image_pixels_for_tests(xid, 2, 0, 0, W, H, !0)
        .expect("get_image")
        .expect("Some bytes");
    for y in 0..u32::from(H) {
        for x in 0..u32::from(W) {
            let off = ((y * u32::from(W) + x) * 4) as usize;
            assert_eq!(
                &got[off..off + 4],
                &saved[off..off + 4],
                "pixel mismatch at ({x}, {y}) — a border-width change left a \
                 stale copy of the window's content one pixel off",
            );
        }
    }
}

/// ROOT CAUSE of the 14 Xlib9 IncludeInferiors regressions.
///
/// The fan-out translates a parent-space rect into a child's local
/// space by subtracting the child's `(x, y)` — its OUTER origin. A
/// child's local drawing coordinates are relative to its CONTENT
/// origin, which sits `bw` further in, and step 3 then adds that same
/// `bw` back as the paint offset. Net result: a fanned-out draw lands
/// `bw` px away from where the identical draw performed directly on
/// the child lands.
///
/// Before step 3 both paths ignored `bw`, so they agreed and the
/// purposes passed; step 3 made the direct path border-correct and left
/// the fan-out on outer coordinates. Every xts purpose here compares
/// exactly those two paths (draw on the window, save, clear, draw the
/// same shape on the ROOT with IncludeInferiors, compare), which is why
/// all 14 report "Drawing on root window with IncludeInferiors gave
/// incorrect results" with a 1-px mismatch — `makewin` gives every test
/// window `border_width = 1` (xts5/src/lib/makewin2.c:232).
#[test]
#[ignore = "needs live Vulkan ICD"]
fn include_inferiors_fanout_lands_where_a_direct_draw_lands() {
    use yserver_core::{
        backend::{DrawState, SubwindowMode, WindowHandle},
        host_x11::HostSubwindowVisual,
    };

    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    const W: u16 = 100;
    const H: u16 = 90;
    const BW: u16 = 1;
    // The window's OUTER origin on the root.
    const WX: i16 = 10;
    const WY: i16 = 5;
    // The draw, in the drawing drawable's own coordinates.
    const RX: i16 = 20;
    const RY: i16 = 30;
    const RW: u16 = 70;
    const RH: u16 = 30;

    let visual = HostSubwindowVisual::Explicit {
        depth: 32,
        visual_xid: 0,
        colormap_xid: 0,
    };
    let root = WindowHandle::from_raw(1).expect("root");
    let w = b
        .create_subwindow(None, root, WX, WY, W, H, BW, visual, Some(BRD_RED), None)
        .expect("create test window");
    let xid = w.as_raw();
    b.map_window_for_tests(xid).expect("map");
    b.apply_draw_state(
        None,
        &DrawState {
            subwindow_mode: SubwindowMode::IncludeInferiors,
            ..DrawState::default()
        },
    )
    .expect("apply_draw_state");

    // (a) Draw directly on the window and save the result.
    b.fill_rectangle(None, xid, BRD_GREEN, RX, RY, RW, RH)
        .expect("direct fill");
    let saved = b
        .get_image_pixels_for_tests(xid, 2, 0, 0, W, H, !0)
        .expect("get_image")
        .expect("Some bytes");

    // (b) Clear, then draw the SAME shape on the ROOT with
    //     IncludeInferiors, at the position that covers the same part
    //     of the window: the window's CONTENT origin is (WX + BW,
    //     WY + BW) in root coordinates.
    b.fill_rectangle(None, xid, BRD_RED, 0, 0, W, H)
        .expect("clear window");
    b.fill_rectangle(
        None,
        b.window_id(),
        BRD_GREEN,
        WX + BW as i16 + RX,
        WY + BW as i16 + RY,
        RW,
        RH,
    )
    .expect("root fill");

    let got = b
        .get_image_pixels_for_tests(xid, 2, 0, 0, W, H, !0)
        .expect("get_image")
        .expect("Some bytes");
    for y in 0..u32::from(H) {
        for x in 0..u32::from(W) {
            let off = ((y * u32::from(W) + x) * 4) as usize;
            assert_eq!(
                &got[off..off + 4],
                &saved[off..off + 4],
                "pixel mismatch at ({x}, {y}): a fanned-out IncludeInferiors \
                 draw must land where the same direct draw lands",
            );
        }
    }
}

/// xts5 `Xlib9/XFillRectangle` purpose 27, root section, replayed
/// request-for-request through the core dispatcher.
///
/// The purpose (XFillRectangle.c:2854-2953) does, on a window created
/// by `makewin` with `border_width = 1`
/// (xts5/src/lib/makewin2.c:232, 100x90 at (10, 5)):
///
///   PolyFillRectangle(window, (20,30,70,30))   [ClipByChildren]
///   savimage(window)                            = GetImage(0,0,100,90)
///   dclear(window)                              = fill (0,0,101,91)
///   ChangeGC(subwindow_mode = IncludeInferiors)
///   create strip children + grandchildren
///   PolyFillRectangle(window, same rect)
///   compsavimage(window)                        [first CHECK — passes]
///   dclear(window); ConfigureWindow(bw=0); ConfigureWindow(x=0,y=0)
///   PolyFillRectangle(ROOT, same rect)
///   compsavimage(window)                        [second CHECK — FAILS]
///
/// and reports `Pixel mismatch at (90, 31) (0 - 1)` — W_BG expected,
/// W_FG obtained, in the column immediately right of the rect.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn xts_xlib9_include_inferiors_root_fill_purpose27() {
    const WID: u32 = 0x0060_0001;
    const GC: u32 = 0x0060_0002;
    const STRIP0: u32 = 0x0060_0010;
    const W: u16 = 100;
    const H: u16 = 90;
    const RX: i16 = 20;
    const RY: i16 = 30;
    const RW: u16 = 70;
    const RH: u16 = 30;
    // W_FG / W_BG (xts5/include/xtest.h:168-169).
    const W_FG: u32 = 1;
    const W_BG: u32 = 0;

    let Some(mut f) = ProtoFixture::new() else {
        eprintln!("skipping: no Vk");
        return;
    };
    let root_res = yserver_core::resources::ROOT_WINDOW.0;

    // CreateWindow(depth=CopyFromParent, wid, parent=root, 10,5,
    //              100x90, border_width=1, class=InputOutput,
    //              visual=CopyFromParent, mask=CWBackPixel)
    let mut body = Vec::new();
    body.extend_from_slice(&WID.to_le_bytes());
    body.extend_from_slice(&root_res.to_le_bytes());
    body.extend_from_slice(&10i16.to_le_bytes());
    body.extend_from_slice(&5i16.to_le_bytes());
    body.extend_from_slice(&W.to_le_bytes());
    body.extend_from_slice(&H.to_le_bytes());
    body.extend_from_slice(&1u16.to_le_bytes()); // border_width = 1
    body.extend_from_slice(&1u16.to_le_bytes()); // class InputOutput
    body.extend_from_slice(&0u32.to_le_bytes()); // visual CopyFromParent
    body.extend_from_slice(&0x0000_0002u32.to_le_bytes()); // CWBackPixel
    body.extend_from_slice(&W_BG.to_le_bytes());
    f.req(1, 0, &body);
    // MapWindow
    f.req(8, 0, &WID.to_le_bytes());
    // CreateGC(gc, window, CWForeground) — foreground = W_FG
    let mut body = Vec::new();
    body.extend_from_slice(&GC.to_le_bytes());
    body.extend_from_slice(&WID.to_le_bytes());
    body.extend_from_slice(&0x0000_0004u32.to_le_bytes()); // GCForeground
    body.extend_from_slice(&W_FG.to_le_bytes());
    f.req(55, 0, &body);

    let fill = |f: &mut ProtoFixture, drawable: u32| {
        let mut body = Vec::new();
        body.extend_from_slice(&drawable.to_le_bytes());
        body.extend_from_slice(&GC.to_le_bytes());
        body.extend_from_slice(&RX.to_le_bytes());
        body.extend_from_slice(&RY.to_le_bytes());
        body.extend_from_slice(&RW.to_le_bytes());
        body.extend_from_slice(&RH.to_le_bytes());
        f.req(70, 0, &body);
    };
    // `dclear` -> `dset(d, W_BG)`: a FRESH GC (so
    // `subwindow_mode = ClipByChildren`, whatever the test's own GC is
    // set to) filling `(0, 0, width + 1, height + 1)`
    // (xts5/src/lib/dset.c:126-149). The fresh GC matters: the clear is
    // clipped by the strip children, so it does NOT erase the parent's
    // pixels underneath them.
    let mut next_gc = GC + 1;
    let dclear = |f: &mut ProtoFixture, drawable: u32, gc_id: u32| {
        let mut body = Vec::new();
        body.extend_from_slice(&gc_id.to_le_bytes());
        body.extend_from_slice(&drawable.to_le_bytes());
        body.extend_from_slice(&0x0000_0004u32.to_le_bytes()); // GCForeground
        body.extend_from_slice(&W_BG.to_le_bytes());
        f.req(55, 0, &body);
        let mut body = Vec::new();
        body.extend_from_slice(&drawable.to_le_bytes());
        body.extend_from_slice(&gc_id.to_le_bytes());
        body.extend_from_slice(&0i16.to_le_bytes());
        body.extend_from_slice(&0i16.to_le_bytes());
        body.extend_from_slice(&(W + 1).to_le_bytes());
        body.extend_from_slice(&(H + 1).to_le_bytes());
        f.req(70, 0, &body);
    };

    // (1) fill the window, then save its pixels.
    fill(&mut f, WID);
    let saved = f
        .backend
        .get_image_pixels_for_tests(
            f.state
                .resources
                .window(yserver_protocol::x11::ResourceId(WID))
                .and_then(|w| w.host_xid)
                .expect("host xid")
                .as_raw(),
            2,
            0,
            0,
            W,
            H,
            !0,
        )
        .expect("get_image")
        .expect("Some bytes");
    let host_wid = f
        .state
        .resources
        .window(yserver_protocol::x11::ResourceId(WID))
        .and_then(|w| w.host_xid)
        .expect("host xid")
        .as_raw();

    // (2) clear, switch to IncludeInferiors, create strips + grandchildren.
    dclear(&mut f, WID, next_gc);
    next_gc += 1;
    let mut body = Vec::new();
    body.extend_from_slice(&GC.to_le_bytes());
    body.extend_from_slice(&0x0000_8000u32.to_le_bytes()); // GCSubwindowMode
    body.extend_from_slice(&1u32.to_le_bytes()); // IncludeInferiors
    f.req(56, 0, &body);
    // `subwins[5]` (XFillRectangle.c:2847) → `swmwidth = 100 / 10 = 10`,
    // strips at x = 0, 20, 40, 60, 80, each 10 wide and full height,
    // with grandchildren every 10 rows inside them
    // (XFillRectangle.c:2879-2887). `crechild` uses border_width = 0.
    const SWM: u16 = 10;
    let mut child_xid = STRIP0;
    for i in 0..5u16 {
        let strip = child_xid;
        child_xid += 1;
        let mut body = Vec::new();
        body.extend_from_slice(&strip.to_le_bytes());
        body.extend_from_slice(&WID.to_le_bytes());
        body.extend_from_slice(&i16::try_from(2 * i * SWM).unwrap().to_le_bytes());
        body.extend_from_slice(&0i16.to_le_bytes());
        body.extend_from_slice(&SWM.to_le_bytes());
        body.extend_from_slice(&H.to_le_bytes());
        body.extend_from_slice(&0u16.to_le_bytes()); // crechild: bw = 0
        body.extend_from_slice(&1u16.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
        f.req(1, 0, &body);
        f.req(8, 0, &strip.to_le_bytes());
        for j in (0..H).step_by(10) {
            let grand = child_xid;
            child_xid += 1;
            let mut body = Vec::new();
            body.extend_from_slice(&grand.to_le_bytes());
            body.extend_from_slice(&strip.to_le_bytes());
            body.extend_from_slice(&0i16.to_le_bytes());
            body.extend_from_slice(&i16::try_from(j).unwrap().to_le_bytes());
            body.extend_from_slice(&SWM.to_le_bytes());
            body.extend_from_slice(&6u16.to_le_bytes());
            body.extend_from_slice(&0u16.to_le_bytes());
            body.extend_from_slice(&1u16.to_le_bytes());
            body.extend_from_slice(&0u32.to_le_bytes());
            body.extend_from_slice(&0u32.to_le_bytes());
            f.req(1, 0, &body);
            f.req(8, 0, &grand.to_le_bytes());
        }
    }
    fill(&mut f, WID);
    let with_children = f
        .backend
        .get_image_pixels_for_tests(host_wid, 2, 0, 0, W, H, !0)
        .expect("get_image")
        .expect("Some bytes");
    assert_eq!(
        with_children, saved,
        "first CHECK: inferiors must not affect the parent's own pixels",
    );

    // (3) clear, border_width -> 0, move to the root origin.
    dclear(&mut f, WID, next_gc);
    let mut body = Vec::new();
    body.extend_from_slice(&WID.to_le_bytes());
    body.extend_from_slice(&0x0010u16.to_le_bytes()); // CWBorderWidth
    body.extend_from_slice(&0u16.to_le_bytes()); // pad
    body.extend_from_slice(&0u32.to_le_bytes()); // border_width = 0
    f.req(12, 0, &body);
    let mut body = Vec::new();
    body.extend_from_slice(&WID.to_le_bytes());
    body.extend_from_slice(&0x0003u16.to_le_bytes()); // CWX | CWY
    body.extend_from_slice(&0u16.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes()); // x = 0
    body.extend_from_slice(&0u32.to_le_bytes()); // y = 0
    f.req(12, 0, &body);

    // (4) fill the ROOT with IncludeInferiors, then compare.
    fill(&mut f, root_res);
    let got = f
        .backend
        .get_image_pixels_for_tests(host_wid, 2, 0, 0, W, H, !0)
        .expect("get_image")
        .expect("Some bytes");
    for y in 0..u32::from(H) {
        for x in 0..u32::from(W) {
            let off = ((y * u32::from(W) + x) * 4) as usize;
            assert_eq!(
                &got[off..off + 4],
                &saved[off..off + 4],
                "Pixel mismatch at ({x}, {y}) — root fill with IncludeInferiors",
            );
        }
    }
}
