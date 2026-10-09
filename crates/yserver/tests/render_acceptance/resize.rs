use super::*;

#[test]
#[ignore = "needs live Vulkan ICD"]
fn a_grow_never_samples_more_than_the_storage_holds() {
    const FRAME: u32 = 0x0064_0001;
    const CLIENT: u32 = 0x0064_0002;
    const GLCHILD: u32 = 0x0064_0003;
    const BW: u16 = 16;
    // Two sizes, in the trace's order: the tile, then the maximise.
    const SMALL: (u16, u16) = (200, 150);
    const BIG: (u16, u16) = (400, 300);
    // The client sits 17 px down and is 17 px shorter, as awesome does it.
    const TITLE: i16 = 17;

    let Some(mut f) = ProtoFixture::new() else {
        eprintln!("skipping: no Vk");
        return;
    };
    let root_res = yserver_core::resources::ROOT_WINDOW.0;
    let visual = yserver_core::resources::ARGB_VISUAL.0;
    let cmap = yserver_core::resources::ARGB_COLORMAP.0;
    // CWBackPixel | CWBorderPixel | CWColormap, as wezterm and awesome
    // send them (`awesome.xtrace:1869`, `:1873`, `:1944`).
    const CW_BACK_PIXEL: u32 = 0x0000_0002;
    const CW_BORDER_PIXEL: u32 = 0x0000_0008;
    const CW_COLORMAP: u32 = 0x0000_2000;

    // The frame: created with bw = 0, like awesome does, then given its
    // border width by a separate ConfigureWindow.
    or_create_window(
        &mut f,
        FRAME,
        root_res,
        32,
        20,
        20,
        SMALL.0,
        SMALL.1,
        0,
        visual,
        CW_BORDER_PIXEL | CW_COLORMAP,
        &[0x00FF_0000, cmap],
    );
    or_create_window(
        &mut f,
        CLIENT,
        FRAME,
        32,
        0,
        TITLE,
        SMALL.0,
        SMALL.1 - TITLE as u16,
        0,
        visual,
        CW_BACK_PIXEL | CW_BORDER_PIXEL | CW_COLORMAP,
        &[0x0000_0000, 0x0000_0000, cmap],
    );
    or_create_window(
        &mut f,
        GLCHILD,
        CLIENT,
        32,
        0,
        0,
        SMALL.0,
        SMALL.1 - TITLE as u16,
        0,
        visual,
        CW_BACK_PIXEL | CW_BORDER_PIXEL | CW_COLORMAP,
        &[0x0000_0000, 0x0000_0000, cmap],
    );
    wz_map(&mut f, FRAME);
    wz_map(&mut f, CLIENT);
    wz_map(&mut f, GLCHILD);
    // The border width arrives on its own, after creation.
    wz_configure(&mut f, FRAME, 0x10, &[i32::from(BW)]);

    // Now GROW all three, frame first, exactly as the trace does.
    wz_configure(
        &mut f,
        FRAME,
        0x4 | 0x8,
        &[i32::from(BIG.0), i32::from(BIG.1)],
    );
    wz_configure(
        &mut f,
        CLIENT,
        0x4 | 0x8,
        &[i32::from(BIG.0), i32::from(BIG.1) - i32::from(TITLE)],
    );
    wz_configure(
        &mut f,
        GLCHILD,
        0x4 | 0x8,
        &[i32::from(BIG.0), i32::from(BIG.1) - i32::from(TITLE)],
    );

    let places = f.backend.scene_participant_places_for_tests();
    let place_of = |res: u32| -> Vec<(i32, i32, u32, u32)> {
        let host = f.host_xid(res);
        places
            .iter()
            .find(|(xid, _, _)| *xid == host)
            .map(|(_, r, _)| r.clone())
            .unwrap_or_default()
    };
    for (name, res, expect_extent) in [
        (
            "frame",
            FRAME,
            (
                u32::from(BIG.0) + 2 * u32::from(BW),
                u32::from(BIG.1) + 2 * u32::from(BW),
            ),
        ),
        (
            "client",
            CLIENT,
            (u32::from(BIG.0), u32::from(BIG.1) - u32::from(TITLE as u16)),
        ),
        (
            "glchild",
            GLCHILD,
            (u32::from(BIG.0), u32::from(BIG.1) - u32::from(TITLE as u16)),
        ),
    ] {
        let host = f.host_xid(res);
        let storage = f
            .backend
            .storage_extent_for_tests(host)
            .expect("storage present");
        // Fact 1: the allocation actually GREW with the geometry.
        assert_eq!(
            storage, expect_extent,
            "{name}: storage must follow a grow (geometry says {expect_extent:?})",
        );
        // Fact 2: the walk never places more than the sampled storage
        // holds. `piece_draw` derives `src` as `piece / sampled_extent`,
        // so a placement wider than the storage samples past the end of
        // the texture — which is what shows as white on RADV.
        for r in place_of(res) {
            assert!(
                r.2 <= storage.0 && r.3 <= storage.1,
                "{name}: placed {}x{} but storage is only {}x{} — the walk samples \
                 past the end of the texture",
                r.2,
                r.3,
                storage.0,
                storage.1,
            );
        }
    }
    for (name, res) in [("frame", FRAME), ("client", CLIENT), ("glchild", GLCHILD)] {
        let host = f.host_xid(res);
        eprintln!(
            "{name}: host={host:#x} storage={:?} place={:?}",
            f.backend.storage_extent_for_tests(host),
            place_of(res),
        );
    }
    // Fact 3: the child lands at the parent's CONTENT origin plus its own
    // position, and the frame's ring is still all four sides of the grown
    // window — the step 5 geometry, restated after a resize.
    assert_eq!(
        place_of(GLCHILD),
        vec![(
            20 + i32::from(BW),
            20 + i32::from(BW) + i32::from(TITLE),
            u32::from(BIG.0),
            u32::from(BIG.1) - u32::from(TITLE as u16),
        )],
        "the GL child sits at the frame's content origin + (0, 17) after the grow",
    );
}

/// #133 — the other half of the wezterm white-block question: after a
/// GROW, is every byte of the window's new storage actually written?
///
/// The background is a DISTINCTIVE colour, not black, so the assertion
/// cannot pass by luck on a zeroed allocation — the failure the
/// coordinator warned about. Anything that is neither the background nor
/// a client paint was never written, which on RADV reads `0xFF` and is
/// the reported white.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn a_grow_writes_every_byte_of_the_new_window_storage() {
    const FRAME: u32 = 0x0065_0001;
    const CLIENT: u32 = 0x0065_0002;
    const GLCHILD: u32 = 0x0065_0003;
    const BW: u16 = 16;
    const SMALL: (u16, u16) = (200, 150);
    const BIG: (u16, u16) = (400, 300);
    const TITLE: i16 = 17;
    // Distinctive, so an unwritten byte cannot masquerade as the fill.
    const BG: u32 = 0x0011_2233;
    const BORDER: u32 = 0x00FF_0000;

    let Some(mut f) = ProtoFixture::new() else {
        eprintln!("skipping: no Vk");
        return;
    };
    let root_res = yserver_core::resources::ROOT_WINDOW.0;
    let visual = yserver_core::resources::ARGB_VISUAL.0;
    let cmap = yserver_core::resources::ARGB_COLORMAP.0;
    const CW_BACK_PIXEL: u32 = 0x0000_0002;
    const CW_BORDER_PIXEL: u32 = 0x0000_0008;
    const CW_COLORMAP: u32 = 0x0000_2000;

    or_create_window(
        &mut f,
        FRAME,
        root_res,
        32,
        20,
        20,
        SMALL.0,
        SMALL.1,
        0,
        visual,
        CW_BACK_PIXEL | CW_BORDER_PIXEL | CW_COLORMAP,
        &[BG, BORDER, cmap],
    );
    for (wid, parent, y) in [(CLIENT, FRAME, TITLE), (GLCHILD, CLIENT, 0)] {
        or_create_window(
            &mut f,
            wid,
            parent,
            32,
            0,
            y,
            SMALL.0,
            SMALL.1 - TITLE as u16,
            0,
            visual,
            CW_BACK_PIXEL | CW_BORDER_PIXEL | CW_COLORMAP,
            &[BG, BORDER, cmap],
        );
    }
    wz_map(&mut f, FRAME);
    wz_map(&mut f, CLIENT);
    wz_map(&mut f, GLCHILD);
    wz_configure(&mut f, FRAME, 0x10, &[i32::from(BW)]);

    // GROW, frame first, as awesome does.
    wz_configure(
        &mut f,
        FRAME,
        0x4 | 0x8,
        &[i32::from(BIG.0), i32::from(BIG.1)],
    );
    for wid in [CLIENT, GLCHILD] {
        wz_configure(
            &mut f,
            wid,
            0x4 | 0x8,
            &[i32::from(BIG.0), i32::from(BIG.1) - i32::from(TITLE)],
        );
    }

    let bg = or_bgr(BG);
    let border = or_bgr(BORDER);
    for (name, res, bw) in [
        ("frame", FRAME, BW),
        ("client", CLIENT, 0),
        ("glchild", GLCHILD, 0),
    ] {
        let (sw, sh, bytes) = f.backing(res);
        let b = i32::from(bw);
        let (cw, ch) = (
            i32::try_from(sw).unwrap() - 2 * b,
            i32::try_from(sh).unwrap() - 2 * b,
        );
        let mut bad: Vec<(i32, i32, [u8; 3], bool)> = Vec::new();
        for y in 0..i32::try_from(sh).unwrap() {
            for x in 0..i32::try_from(sw).unwrap() {
                let off = ((y * i32::try_from(sw).unwrap() + x) * 4) as usize;
                let px = [bytes[off], bytes[off + 1], bytes[off + 2]];
                let inside = x >= b && y >= b && x < b + cw && y < b + ch;
                let want = if inside { bg } else { border };
                if px != want {
                    bad.push((x, y, px, inside));
                }
            }
        }
        assert!(
            bad.is_empty(),
            "{name}: {} of {}x{} bytes are neither the background nor the border after a grow \
             — first offenders {:?}",
            bad.len(),
            sw,
            sh,
            &bad[..bad.len().min(6)],
        );
    }
}

/// ReparentWindow.
fn wz_reparent(f: &mut ProtoFixture, wid: u32, parent: u32, x: i16, y: i16) {
    let mut body = Vec::new();
    body.extend_from_slice(&wid.to_le_bytes());
    body.extend_from_slice(&parent.to_le_bytes());
    body.extend_from_slice(&x.to_le_bytes());
    body.extend_from_slice(&y.to_le_bytes());
    f.req(7, 0, &body);
}

/// #133 — the wezterm white-band regression, replayed in the exact
/// request ORDER awesome and wezterm use (`awesome.xtrace:1869`-`4078`),
/// not the abbreviated shape:
///
/// 1. wezterm creates its client as a ROOT child and its GL child under
///    it, both at `(0, 0)`, and maps them.
/// 2. awesome creates the frame (`border-width = 0`), REPARENTS the
///    client into it at `(0, 0)`, then sizes the frame and moves the
///    client to `(0, TITLE)` — the decoration is at the TOP.
/// 3. `border-width = 16` arrives on the frame afterwards, on its own,
///    and is then re-sent unchanged several times.
/// 4. The maximise is a simultaneous move-AND-resize on the frame and
///    the client, and wezterm resizes its GL child separately.
///
/// The band is ~17 px — the titlebar height — at the BOTTOM, and stays
/// ~17 px when `bw` doubles, so the quantity in play is the client's own
/// `y`, not the border. If that `y` is lost, the client covers the
/// titlebar at the top and leaves exactly a titlebar-high strip
/// unwritten at the bottom.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn the_awesome_wezterm_grow_keeps_the_client_below_the_titlebar() {
    const FRAME: u32 = 0x0066_0001;
    const CLIENT: u32 = 0x0066_0002;
    const GLCHILD: u32 = 0x0066_0003;
    const GC: u32 = 0x0066_0010;
    const BW: u16 = 16;
    const TITLE: u16 = 17;
    // Client sizes; the frame is TITLE taller than its client.
    const SMALL: (u16, u16) = (200, 120);
    const BIG: (u16, u16) = (400, 260);
    const FX: i16 = 20;
    const FY: i16 = 30;

    let Some(mut f) = ProtoFixture::new() else {
        eprintln!("skipping: no Vk");
        return;
    };
    let root_res = yserver_core::resources::ROOT_WINDOW.0;
    let visual = yserver_core::resources::ARGB_VISUAL.0;
    let cmap = yserver_core::resources::ARGB_COLORMAP.0;
    const CW_BACK_PIXEL: u32 = 0x0000_0002;
    const CW_BORDER_PIXEL: u32 = 0x0000_0008;
    const CW_COLORMAP: u32 = 0x0000_2000;

    // 1 — wezterm's windows, root children first, at the pre-tile size.
    or_create_window(
        &mut f,
        CLIENT,
        root_res,
        32,
        0,
        0,
        SMALL.0,
        SMALL.1,
        0,
        visual,
        CW_BACK_PIXEL | CW_BORDER_PIXEL | CW_COLORMAP,
        &[0x0000_0000, 0x0000_0000, cmap],
    );
    or_create_window(
        &mut f,
        GLCHILD,
        CLIENT,
        32,
        0,
        0,
        SMALL.0,
        SMALL.1,
        0,
        visual,
        CW_BACK_PIXEL | CW_BORDER_PIXEL | CW_COLORMAP,
        &[0x0000_0000, 0x0000_0000, cmap],
    );
    wz_map(&mut f, GLCHILD);
    wz_map(&mut f, CLIENT);

    // 2 — awesome's frame, created with NO border width.
    or_create_window(
        &mut f,
        FRAME,
        root_res,
        32,
        0,
        0,
        SMALL.0,
        SMALL.1,
        0,
        visual,
        CW_BORDER_PIXEL | CW_COLORMAP,
        &[0x00FF_0000, cmap],
    );
    wz_reparent(&mut f, CLIENT, FRAME, 0, 0);
    wz_map(&mut f, CLIENT);
    wz_configure(&mut f, CLIENT, 0x10, &[0]);
    // Frame sized to client + titlebar; client moved BELOW the titlebar.
    wz_configure(
        &mut f,
        FRAME,
        0x1 | 0x2 | 0x4 | 0x8,
        &[
            i32::from(FX),
            i32::from(FY),
            i32::from(SMALL.0),
            i32::from(SMALL.1 + TITLE),
        ],
    );
    wz_configure(
        &mut f,
        CLIENT,
        0x1 | 0x2 | 0x4 | 0x8,
        &[0, i32::from(TITLE), i32::from(SMALL.0), i32::from(SMALL.1)],
    );
    // 3 — the border width, on its own, and re-sent unchanged.
    wz_configure(&mut f, FRAME, 0x10, &[i32::from(BW)]);
    wz_map(&mut f, FRAME);
    wz_configure(&mut f, FRAME, 0x10, &[i32::from(BW)]);
    wz_configure(&mut f, FRAME, 0x10, &[i32::from(BW)]);

    // A full-size client paint, so the GL child has real pixels.
    or_create_gc(&mut f, GC, GLCHILD, 0x0000_00FF);
    or_fill(&mut f, GLCHILD, GC, 0, 0, SMALL.0, SMALL.1);

    let snapshot = |f: &mut ProtoFixture, label: &str| {
        let places = f.backend.scene_participant_places_for_tests();
        for (name, res) in [("frame", FRAME), ("client", CLIENT), ("glchild", GLCHILD)] {
            let host = f.host_xid(res);
            eprintln!(
                "{label} {name}: host={host:#x} storage={:?} place={:?}",
                f.backend.storage_extent_for_tests(host),
                places
                    .iter()
                    .find(|(x, _, _)| *x == host)
                    .map(|(_, r, _)| r.clone())
                    .unwrap_or_default(),
            );
        }
        places
    };
    snapshot(&mut f, "pre-grow");

    // 4 — the maximise: simultaneous move-and-resize, frame then client,
    // then wezterm resizes its GL child.
    wz_configure(
        &mut f,
        FRAME,
        0x1 | 0x2 | 0x4 | 0x8,
        &[
            i32::from(FX),
            i32::from(FY),
            i32::from(BIG.0),
            i32::from(BIG.1 + TITLE),
        ],
    );
    wz_configure(
        &mut f,
        CLIENT,
        0x1 | 0x2 | 0x4 | 0x8,
        &[0, i32::from(TITLE), i32::from(BIG.0), i32::from(BIG.1)],
    );
    wz_configure(
        &mut f,
        GLCHILD,
        0x4 | 0x8,
        &[i32::from(BIG.0), i32::from(BIG.1)],
    );
    or_fill(&mut f, GLCHILD, GC, 0, 0, BIG.0, BIG.1);

    let places = snapshot(&mut f, "post-grow");
    let place_of = |host: u32| -> Vec<(i32, i32, u32, u32)> {
        places
            .iter()
            .find(|(x, _, _)| *x == host)
            .map(|(_, r, _)| r.clone())
            .unwrap_or_default()
    };
    // What actually reaches the screen (`place ∩ mine`). Asserted
    // separately from `place`: the parent's inner region is its own
    // computation (`inner_place_rects`, done in the parent's STORAGE
    // space and mapped out by `dx`/`dy`), so a space mix-up there would
    // shorten the visible extent while leaving the placement correct.
    let visible_of = |host: u32| -> Vec<(i32, i32, u32, u32)> {
        places
            .iter()
            .find(|(x, _, _)| *x == host)
            .map(|(_, _, v)| v.clone())
            .unwrap_or_default()
    };
    let (frame_host, client_host, glchild_host) =
        (f.host_xid(FRAME), f.host_xid(CLIENT), f.host_xid(GLCHILD));

    // The frame's OUTER rect: its own origin, its bordered extent.
    assert_eq!(
        place_of(frame_host),
        vec![(
            i32::from(FX),
            i32::from(FY),
            u32::from(BIG.0) + 2 * u32::from(BW),
            u32::from(BIG.1 + TITLE) + 2 * u32::from(BW),
        )],
        "frame outer rect after the grow",
    );
    // The client sits at the frame's CONTENT origin PLUS its own (0, TITLE),
    // and is TITLE shorter than the frame's content — so the titlebar strip
    // at the TOP stays the frame's, and nothing is left over at the bottom.
    let want_client = vec![(
        i32::from(FX) + i32::from(BW),
        i32::from(FY) + i32::from(BW) + i32::from(TITLE),
        u32::from(BIG.0),
        u32::from(BIG.1),
    )];
    assert_eq!(place_of(client_host), want_client, "client after the grow");
    assert_eq!(
        place_of(glchild_host),
        want_client,
        "GL child after the grow"
    );
    // THE TIGHT FIT. `client.y + client.h == frame content height`
    // exactly (`TITLE + BIG.1 == BIG.1 + TITLE`), which is what awesome
    // always produces: it sets `frame_content_h = client_h + titlebar`
    // and `client.y = titlebar`, so the child exactly fills the
    // remaining parent content and the `min(outer_h)` term can no longer
    // mask a wrong child-clip bound. The frame is itself at a non-zero
    // origin, so the parent's outer absolute is a live term.
    assert_eq!(
        i32::from(TITLE) + i32::from(BIG.1),
        i32::from(BIG.1 + TITLE),
        "fixture sanity: the child must EXACTLY fill the parent's remaining content",
    );
    // The GL child is topmost, so its VISIBLE extent is what reaches the
    // screen; the client beneath it is fully covered and legitimately
    // shows nothing (a hidden participant, which is still a participant).
    assert_eq!(
        visible_of(glchild_host),
        want_client,
        "GL child VISIBLE extent after the grow (tight fit)",
    );
    assert!(
        visible_of(client_host).is_empty(),
        "the client is entirely covered by its GL child: {:?}",
        visible_of(client_host),
    );
    // `visible` is only the BOUNDING BOX of the emitted pieces
    // (`emit_node` folds them with `union_bbox`), so it cannot see a GAP
    // that leaves the bbox intact — a dropped tail beside a surviving
    // last row would pass every assertion above. The draw list is the
    // ground truth: an unbroken node contributes exactly ONE rect equal
    // to its placement.
    let draws = f.backend.scene_draw_rects_for_tests();
    assert!(
        draws.contains(&want_client[0]),
        "the GL child must contribute ONE unbroken draw equal to its placement \
         {want_client:?}; draw list was {draws:?}",
    );
    // Nothing may be placed beyond the storage it samples: `piece_draw`
    // divides by the sampled extent, so a wider placement reads past the
    // end of the texture.
    for (name, host) in [
        ("frame", frame_host),
        ("client", client_host),
        ("glchild", glchild_host),
    ] {
        let storage = f
            .backend
            .storage_extent_for_tests(host)
            .expect("storage present");
        for r in place_of(host) {
            assert!(
                r.2 <= storage.0 && r.3 <= storage.1,
                "{name}: placed {}x{} but storage is {}x{}",
                r.2,
                r.3,
                storage.0,
                storage.1,
            );
        }
    }
}

/// #133 investigation — the initial clear of window storage created with
/// NO background attribute.
///
/// Wrote the defect down as a failing test on 2026-09-11 and fixed it
/// the same day; it is a regression guard now. Kept `#[ignore]`d with
/// the rest of the Vk-gated suite, so `cargo test` stays green.
///
/// Measured, on this box, for a fresh window that is mapped and never
/// painted:
///
/// | background attribute | depth | init colour | storage reads |
/// |---|---|---|---|
/// | none                 | 24 | `[0,0,0,1]` → `000000ff` | **`ffffffff`** |
/// | none                 | 32 | `[0,0,0,0]` → `00000000` | **`ffffff00`** |
/// | `background-pixel = 0`          | 32 | `00000000` | `00000000` ✓ |
/// | `background-pixel = 0x00ff0000` | 32 | `0000ff00` | `0000ff00` ✓ |
/// | `background-pixel = 0xff00ff00` | 32 | `00ff00ff` | `00ff00ff` ✓ |
///
/// The CAUSE, measured by logging the colour that actually reached the
/// fill: `bg_pixel` arrived as `Some(0x00ff_ffff)` — WHITE — for both
/// failing rows, and the fill landed all four channels faithfully.
/// `create_window` stores `0x00ff_ffff` as a PLACEHOLDER when a request
/// carries no background attribute (`resources.rs`), records the real
/// state separately in `background_none`, and
/// `window_resolved_background` honours it by returning `None` — but
/// the CreateWindow path then defeated that with
/// `.or_else(|| local.map(|w| w.background_pixel))`, resurrecting the
/// placeholder. `default_window_init_color`'s `None` branch was
/// therefore dead for every client window.
///
/// Two earlier readings of the same numbers were WRONG, and the trap is
/// worth keeping on file. Alpha looked meaningful — exactly the init
/// colour's alpha in both rows, suggesting "something writes alpha and
/// leaves RGB untouched", and a write-mask hunt followed. It is an
/// artifact of the placeholder: `decode_x11_pixel_for_storage` takes
/// alpha from the pixel's top byte, so `0x00ff_ffff` gives `ffffffff` at
/// depth 24 and `ffffff00` at depth 32 in one pass. And `bg_pixel =
/// Some(0)` vs `None` at depth 32 do NOT hand `fill_rect` the same
/// colour, as this comment used to claim: `Some(0)` gives
/// `[0.0, 0.0, 0.0, 0.0]`, `None` gave white.
///
/// Why it shows as WHITE rather than as transparency: the scene draws
/// windows with `alpha_passthrough = false`, whose shader forces
/// `src.a = 1`, so both `ffffff00` and `ffffffff` composite as OPAQUE
/// WHITE. Any region of such a window that nothing paints afterwards is
/// white on screen.
///
/// This is the byte pattern in the reporter's own dumps: awesome's frame
/// (`border-pixel` but **no background-pixel**, `awesome.xtrace:1944`)
/// reads `ffffff00` across 808 600 px — 90.66% of its storage, exactly
/// its 1244x650 content region — with the ring (`00ff00ff`, 62 176 px =
/// exactly the annulus) and the titlebar (`535d6cff`) both landed, and
/// **zero** `00000000` pixels anywhere.
///
/// `PixmapPool` recycles image/view/memory triples (3f.10), so a marker
/// predecessor is filled and destroyed first: without it, a driver that
/// happens to zero fresh memory would let the correct-behaviour
/// assertion pass by luck.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn window_storage_init_covers_the_whole_allocation() {
    const MARKER_WIN: u32 = 0x0067_0001;
    const GC: u32 = 0x0067_0010;
    const W: u16 = 160;
    const H: u16 = 120;
    const MARKER: u32 = 0x00AB_CDEF;

    let Some(mut f) = ProtoFixture::new() else {
        eprintln!("skipping: no Vk");
        return;
    };
    let root_res = yserver_core::resources::ROOT_WINDOW.0;
    let visual = yserver_core::resources::ARGB_VISUAL.0;
    let cmap = yserver_core::resources::ARGB_COLORMAP.0;
    const CW_BACK_PIXEL: u32 = 0x0000_0002;
    const CW_BORDER_PIXEL: u32 = 0x0000_0008;
    const CW_COLORMAP: u32 = 0x0000_2000;
    // A depth-32 child of the depth-24 root MUST supply a border
    // pixel/pixmap or CreateWindow is BadMatch (`dix/window.c:818`,
    // implemented in #133 step 1) — which is why awesome sends
    // `border-pixel=0x00000000` on every depth-32 CreateWindow.

    // Dirty the pool: a same-size window painted a marker, then destroyed.
    or_create_window(
        &mut f,
        MARKER_WIN,
        root_res,
        32,
        0,
        0,
        W,
        H,
        0,
        visual,
        CW_BACK_PIXEL | CW_BORDER_PIXEL | CW_COLORMAP,
        &[MARKER, 0, cmap],
    );
    wz_map(&mut f, MARKER_WIN);
    or_create_gc(&mut f, GC, MARKER_WIN, MARKER);
    or_fill(&mut f, MARKER_WIN, GC, 0, 0, W, H);
    let (_, _, marker_bytes) = f.backing(MARKER_WIN);
    // Alpha comes from the X11 pixel's top byte, so `0x00ABCDEF` at
    // depth 32 stores alpha 0 — the same reason awesome's frame reads
    // `ffffff00` and not `ffffffff`.
    let marker_bgr = or_bgr(MARKER);
    assert_eq!(
        &marker_bytes[..4],
        &[marker_bgr[0], marker_bgr[1], marker_bgr[2], 0],
        "the marker window really holds the marker",
    );
    f.req(4, 0, &MARKER_WIN.to_le_bytes()); // DestroyWindow

    let mut failures: Vec<String> = Vec::new();
    let mut case = 0x0067_0100u32;
    for (name, depth, vis, mask, values, want) in [
        (
            "no background attribute, depth 24",
            24u8,
            yserver_core::resources::ROOT_VISUAL.0,
            0u32,
            vec![],
            // Background None: realize seeds the leaf from the parent, here the root's 0x505050.
            [80u8, 80, 80, 255],
        ),
        (
            "no background attribute, depth 32",
            32,
            visual,
            CW_BORDER_PIXEL | CW_COLORMAP,
            vec![0u32, cmap],
            [80, 80, 80, 255],
        ),
        (
            "background-pixel = 0, depth 32",
            32,
            visual,
            CW_BACK_PIXEL | CW_BORDER_PIXEL | CW_COLORMAP,
            vec![0u32, 0u32, cmap],
            [0, 0, 0, 0],
        ),
        (
            "background-pixel = 0x00ff0000, depth 32",
            32,
            visual,
            CW_BACK_PIXEL | CW_BORDER_PIXEL | CW_COLORMAP,
            vec![0x00FF_0000u32, 0u32, cmap],
            [0, 0, 255, 0],
        ),
        (
            "background-pixel = 0xff00ff00, depth 32",
            32,
            visual,
            CW_BACK_PIXEL | CW_BORDER_PIXEL | CW_COLORMAP,
            vec![0xFF00_FF00u32, 0u32, cmap],
            [0, 255, 0, 255],
        ),
    ] {
        case += 1;
        or_create_window(
            &mut f, case, root_res, depth, 0, 0, W, H, 0, vis, mask, &values,
        );
        wz_map(&mut f, case);
        let (sw, sh, bytes) = f.backing(case);
        assert_eq!((sw, sh), (u32::from(W), u32::from(H)));
        let mut distinct = std::collections::BTreeMap::<[u8; 4], usize>::new();
        for px in bytes.chunks_exact(4) {
            *distinct.entry([px[0], px[1], px[2], px[3]]).or_default() += 1;
        }
        let got: Vec<[u8; 4]> = distinct.keys().copied().collect();
        if got != vec![want] {
            failures.push(format!(
                "{name}: expected the whole allocation to be {want:?}, got {distinct:?}"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "window storage init does not cover the whole allocation:\n  {}",
        failures.join("\n  "),
    );
}

/// #133 step 5 — a child at a NON-ZERO offset inside a bordered parent
/// keeps its full width and height, in both `place` and what is
/// actually VISIBLE.
///
/// The discriminating shape for the wezterm white-band report: the loss
/// there was the child's own offset within its parent, in each axis, and
/// `child.x = 0` in that tree so only `y` lost anything. Every test that
/// puts the child at `(0, 0)` — and every test that asserts `place`
/// without asserting `place ∩ mine` — is blind to it. So this one uses a
/// non-zero offset in BOTH axes and a parent whose content is strictly
/// larger than `child.offset + child.size`, which separates a lost
/// offset from a tight fit, and it asserts the visible rects.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn a_child_at_a_nonzero_offset_in_a_bordered_parent_keeps_its_full_extent() {
    const PARENT: u32 = 0x0068_0001;
    const CHILD: u32 = 0x0068_0002;
    const GRANDCHILD: u32 = 0x0068_0003;
    const BW: u16 = 16;
    const PX: i16 = 40;
    const PY: i16 = 30;
    const PW: u16 = 400;
    const PH: u16 = 300;
    // Non-zero in BOTH axes, and strictly inside: 30 + 200 < 400 and
    // 17 + 100 < 300, so a lost offset cannot hide behind a tight fit.
    const CX: i16 = 30;
    const CY: i16 = 17;
    const CW_: u16 = 200;
    const CH_: u16 = 100;

    let Some(mut f) = ProtoFixture::new() else {
        eprintln!("skipping: no Vk");
        return;
    };
    let root_res = yserver_core::resources::ROOT_WINDOW.0;
    let visual = yserver_core::resources::ARGB_VISUAL.0;
    let cmap = yserver_core::resources::ARGB_COLORMAP.0;
    const CW_BACK_PIXEL: u32 = 0x0000_0002;
    const CW_BORDER_PIXEL: u32 = 0x0000_0008;
    const CW_COLORMAP: u32 = 0x0000_2000;

    or_create_window(
        &mut f,
        PARENT,
        root_res,
        32,
        PX,
        PY,
        PW,
        PH,
        BW,
        visual,
        CW_BACK_PIXEL | CW_BORDER_PIXEL | CW_COLORMAP,
        &[0x0011_2233, 0x00FF_0000, cmap],
    );
    or_create_window(
        &mut f,
        CHILD,
        PARENT,
        32,
        CX,
        CY,
        CW_,
        CH_,
        0,
        visual,
        CW_BACK_PIXEL | CW_BORDER_PIXEL | CW_COLORMAP,
        &[0x0000_0000, 0, cmap],
    );
    // A grandchild at its own non-zero offset, so the recurrence is
    // exercised two levels deep under the border.
    or_create_window(
        &mut f,
        GRANDCHILD,
        CHILD,
        32,
        11,
        7,
        50,
        40,
        0,
        visual,
        CW_BACK_PIXEL | CW_BORDER_PIXEL | CW_COLORMAP,
        &[0x0000_0000, 0, cmap],
    );
    wz_map(&mut f, PARENT);
    wz_map(&mut f, CHILD);
    wz_map(&mut f, GRANDCHILD);

    let places = f.backend.scene_participant_places_for_tests();
    type Rects = Vec<(i32, i32, u32, u32)>;
    let of = |host: u32| -> (Rects, Rects) {
        places
            .iter()
            .find(|(x, _, _)| *x == host)
            .map(|(_, p, v)| (p.clone(), v.clone()))
            .unwrap_or_default()
    };
    // Parent CONTENT origin, per `dix/window.c:888`.
    let (pcx, pcy) = (i32::from(PX) + i32::from(BW), i32::from(PY) + i32::from(BW));
    let child_rect = (
        pcx + i32::from(CX),
        pcy + i32::from(CY),
        u32::from(CW_),
        u32::from(CH_),
    );
    let grand_rect = (child_rect.0 + 11, child_rect.1 + 7, 50, 40);

    for (name, host, want) in [
        ("child", f.host_xid(CHILD), child_rect),
        ("grandchild", f.host_xid(GRANDCHILD), grand_rect),
    ] {
        let (place, visible) = of(host);
        assert_eq!(vec![want], place, "{name}: placement");
        // The visible extent is what reaches the screen. A loss of the
        // node's own offset shows up HERE even when `place` is right,
        // because `mine` — the parent's inner region handed to children
        // — is a separate computation.
        assert_eq!(vec![want], visible, "{name}: visible extent");
    }
    // And the parent keeps its full outer rect, ring included.
    let (pplace, _) = of(f.host_xid(PARENT));
    assert_eq!(
        vec![(
            i32::from(PX),
            i32::from(PY),
            u32::from(PW) + 2 * u32::from(BW),
            u32::from(PH) + 2 * u32::from(BW),
        )],
        pplace,
        "parent outer rect",
    );
}

// ───── #143 — a resize must not silently destroy a window's content ──
//
// Issue #143's reporter: "already open windows get broken rendering"
// when a compositor starts. Measured on HW (awesome + picom, 2026-09-16)
// as an A/B on nothing but the launch order: windows spawned BEFORE
// picom lost their content, the same windows spawned after it kept it.
//
// The redirect seed is not what loses it — `overlay_backing_inferiors`
// copies whatever the window's leaf holds, and these tests show it
// arriving intact. What loses it is the WM retile that happens while
// the window is still unredirected: `configure_subwindow` reallocated
// the leaf and discarded the pixels, and on a SHRINK the client was
// never told, so an idle client never repainted and the window stayed
// broken through the redirect and forever after. Both halves are fixed
// now: a background-None window keeps its pixels, and every resize
// reports the whole window exposed as Xorg's `miResizeWindow` does
// (`mi/miwindow.c:466-472`).

/// A shrink keeps a background-None window's pixels AND still tells the
/// client what was exposed. Xorg does both, in that order: the paint
/// returns without touching a pixel when the window has no background
/// (`mi/miexpose.c:438-440`) but `miWindowExposures` sends the
/// exposures regardless (`mi/miexpose.c:387-389`).
///
/// This test asserted ZERO Exposes until the shrink-Expose fix — that
/// was our behaviour, not Xorg's, and it is what left #143's xterm
/// black. The pixel half is unchanged.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn a_shrink_exposes_and_keeps_the_content_of_a_background_none_window() {
    use std::io::Read;
    const W: u32 = 0x1430;
    const GC: u32 = 0x1431;
    let Some(mut f) = ProtoFixture::new() else {
        eprintln!("skipping: no Vk");
        return;
    };
    let root = yserver_core::resources::ROOT_WINDOW.0;
    let visual = yserver_core::resources::ROOT_VISUAL.0;
    // CWEventMask only: background None, the shape X11 says must be
    // left alone on a size change (wezterm's, and 37 of KWin's).
    or_create_window(
        &mut f,
        W,
        root,
        24,
        0,
        0,
        200,
        100,
        0,
        visual,
        0x0000_0800,
        &[0x0000_8000],
    );
    wz_map(&mut f, W);
    or_create_gc(&mut f, GC, W, 0x0000_FF00);
    or_fill(&mut f, W, GC, 0, 0, 200, 100);
    f._peer
        .set_nonblocking(true)
        .expect("nonblocking socketpair");
    let mut sink = [0u8; 65536];
    while f._peer.read(&mut sink).map(|n| n > 0).unwrap_or(false) {}

    // The WM retiles when the next window opens.
    wz_configure(&mut f, W, 0x0C, &[100, 100]);

    let mut buf = [0u8; 65536];
    let n = f._peer.read(&mut buf).unwrap_or(0);
    let exposes: Vec<(u16, u16, u16, u16)> = (0..n / 32)
        .map(|k| &buf[k * 32..k * 32 + 32])
        .filter(|e| e[0] & 0x7f == 12 && u32::from_le_bytes([e[4], e[5], e[6], e[7]]) == W)
        .map(|e| {
            (
                u16::from_le_bytes([e[8], e[9]]),
                u16::from_le_bytes([e[10], e[11]]),
                u16::from_le_bytes([e[12], e[13]]),
                u16::from_le_bytes([e[14], e[15]]),
            )
        })
        .collect();
    assert_eq!(
        exposes,
        vec![(0, 0, 100, 100)],
        "a shrink must report the whole new window exposed, exactly once — \
         the client cannot recover a discarded window otherwise (#143)",
    );

    let (sw, sh, px) = f.backing(W);
    assert_eq!((sw, sh), (100, 100), "storage follows the new geometry");
    let mut distinct = std::collections::BTreeMap::<[u8; 4], usize>::new();
    for p in px.chunks_exact(4) {
        *distinct.entry([p[0], p[1], p[2], p[3]]).or_default() += 1;
    }
    assert_eq!(
        distinct.keys().copied().collect::<Vec<_>>(),
        vec![[0x00, 0xFF, 0x00, 0xFF]],
        "every retained pixel must still be the client's green (BGRA); X11 \
         leaves a background-None window's contents alone across a resize, \
         so a background fill here would be a wipe Xorg never does: \
         {distinct:?}",
    );
}

/// A grow keeps the pixels it retains. What lands in the strip the grow
/// added is a separate question (the wezterm ctrl-+ report) and this
/// test deliberately does not pin it — only that the client's own
/// content below it survived.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn a_grow_keeps_the_old_content_of_a_background_none_window() {
    const W: u32 = 0x1440;
    const GC: u32 = 0x1441;
    let Some(mut f) = ProtoFixture::new() else {
        eprintln!("skipping: no Vk");
        return;
    };
    let root = yserver_core::resources::ROOT_WINDOW.0;
    let visual = yserver_core::resources::ROOT_VISUAL.0;
    or_create_window(&mut f, W, root, 24, 0, 0, 100, 100, 0, visual, 0, &[]);
    wz_map(&mut f, W);
    or_create_gc(&mut f, GC, W, 0x0000_FF00);
    or_fill(&mut f, W, GC, 0, 0, 100, 100);
    wz_configure(&mut f, W, 0x0C, &[100, 160]);

    let (sw, sh, px) = f.backing(W);
    assert_eq!((sw, sh), (100, 160));
    let at = |x: usize, y: usize| {
        let o = (y * sw as usize + x) * 4;
        [px[o], px[o + 1], px[o + 2], px[o + 3]]
    };
    assert_eq!(
        at(50, 50),
        [0x00, 0xFF, 0x00, 0xFF],
        "inside the old footprint the client's own pixels survive a grow",
    );
}

/// The other half of the ForgetGravity rule, and the reason #143's fix
/// is gated on the background: a window that HAS one is discarded and
/// re-tiled, exactly as X11 specifies and as the xeyes-resize
/// regression needs.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn a_resize_still_retiles_a_window_that_has_a_background() {
    const W: u32 = 0x1448;
    const GC: u32 = 0x1449;
    let Some(mut f) = ProtoFixture::new() else {
        eprintln!("skipping: no Vk");
        return;
    };
    let root = yserver_core::resources::ROOT_WINDOW.0;
    let visual = yserver_core::resources::ROOT_VISUAL.0;
    // CWBackPixel = blue.
    or_create_window(
        &mut f,
        W,
        root,
        24,
        0,
        0,
        100,
        100,
        0,
        visual,
        0x0000_0002,
        &[0x0000_00FF],
    );
    wz_map(&mut f, W);
    or_create_gc(&mut f, GC, W, 0x0000_FF00);
    or_fill(&mut f, W, GC, 0, 0, 100, 100);
    wz_configure(&mut f, W, 0x0C, &[60, 60]);

    let (sw, sh, px) = f.backing(W);
    assert_eq!((sw, sh), (60, 60));
    let mut distinct = std::collections::BTreeMap::<[u8; 4], usize>::new();
    for p in px.chunks_exact(4) {
        *distinct.entry([p[0], p[1], p[2], p[3]]).or_default() += 1;
    }
    assert_eq!(
        distinct.keys().copied().collect::<Vec<_>>(),
        vec![[0xFF, 0x00, 0x00, 0xFF]],
        "a window with a background comes back tiled with it, not holding \
         the client's old pixels: {distinct:?}",
    );
}

/// The reporter's scenario end to end, through the protocol: a window
/// paints, the WM retiles it, and only THEN does a compositor call
/// `CompositeRedirectSubwindows(root, Manual)`. The redirect backing has
/// to come up holding what was on screen — which is what Xorg's
/// `compNewPixmap` guarantees by copying the parent with
/// `IncludeInferiors` (`composite/compalloc.c:562-571`), and what our
/// `seed_backing_from_parent` + `overlay_backing_inferiors` pair
/// reproduces. It can only carry what the leaf still holds, so this is
/// the test that fails if the retile wipes it.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn an_already_open_window_keeps_its_content_when_a_compositor_starts() {
    const FRAME: u32 = 0x1450;
    const CLIENT: u32 = 0x1451;
    const GC: u32 = 0x1452;
    for retile_first in [false, true] {
        let Some(mut f) = ProtoFixture::new() else {
            eprintln!("skipping: no Vk");
            return;
        };
        let root = yserver_core::resources::ROOT_WINDOW.0;
        let visual = yserver_core::resources::ROOT_VISUAL.0;
        // A reparenting WM's frame under root, with the client inside:
        // `RedirectSubwindows(root)` redirects the FRAME, and the
        // client's pixels have to reach the frame's backing from a
        // level down.
        or_create_window(&mut f, FRAME, root, 24, 10, 20, 200, 100, 2, visual, 0, &[]);
        or_create_window(&mut f, CLIENT, FRAME, 24, 0, 0, 200, 100, 0, visual, 0, &[]);
        wz_map(&mut f, CLIENT);
        wz_map(&mut f, FRAME);
        or_create_gc(&mut f, GC, CLIENT, 0x0000_FF00);
        or_fill(&mut f, CLIENT, GC, 0, 0, 200, 100);

        let retile = |f: &mut ProtoFixture| {
            wz_configure(f, FRAME, 0x0C, &[100, 100]);
            wz_configure(f, CLIENT, 0x0C, &[100, 100]);
        };
        let start_compositor = |f: &mut ProtoFixture| {
            let mut body = Vec::new();
            body.extend_from_slice(&root.to_le_bytes());
            body.extend_from_slice(&[1u8, 0, 0, 0]); // CompositeRedirectManual
            f.req(144, 2, &body);
        };
        if retile_first {
            retile(&mut f);
            start_compositor(&mut f);
        } else {
            start_compositor(&mut f);
            retile(&mut f);
        }

        // The compositor reads the frame's backing. Content sits `bw`
        // inside it (`compSetPixmap(pWin, pPixmap, bw)`,
        // `composite/compalloc.c:620`), so sample well inside.
        let (sw, sh, px) = f.backing(FRAME);
        assert_eq!((sw, sh), (104, 104), "bordered backing extent");
        let at = |x: usize, y: usize| {
            let o = (y * sw as usize + x) * 4;
            [px[o], px[o + 1], px[o + 2], px[o + 3]]
        };
        for (x, y) in [(10usize, 10usize), (50, 50), (90, 90)] {
            assert_eq!(
                at(x, y),
                [0x00, 0xFF, 0x00, 0xFF],
                "retile_first={retile_first}: the compositor must be handed \
                 the pixels the client painted, at ({x},{y})",
            );
        }
    }
}

/// The migrated content must land at the border inset, never at the
/// storage origin: a bordered window's storage starts at its OUTER
/// origin with the content `bw` inside (`compAllocPixmap`,
/// `composite/compalloc.c:610`), so a copy that forgot the inset would
/// both shift the image and eat the ring. Guards the #143 resize path
/// specifically — the ring is the only thing that can tell the two
/// apart once the content is uniform.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn a_shrink_keeps_the_content_at_the_border_inset() {
    const W: u32 = 0x1460;
    const GC: u32 = 0x1461;
    const BW: usize = 3;
    let Some(mut f) = ProtoFixture::new() else {
        eprintln!("skipping: no Vk");
        return;
    };
    let root = yserver_core::resources::ROOT_WINDOW.0;
    let visual = yserver_core::resources::ROOT_VISUAL.0;
    // CWBorderPixel (0x08) = red; background stays None.
    or_create_window(
        &mut f,
        W,
        root,
        24,
        0,
        0,
        100,
        100,
        BW as u16,
        visual,
        0x0000_0008,
        &[0x00FF_0000],
    );
    wz_map(&mut f, W);
    or_create_gc(&mut f, GC, W, 0x0000_FF00);
    or_fill(&mut f, W, GC, 0, 0, 100, 100);
    wz_configure(&mut f, W, 0x0C, &[60, 60]);

    let (sw, sh, px) = f.backing(W);
    assert_eq!((sw, sh), (66, 66), "bordered storage extent");
    let at = |x: usize, y: usize| {
        let o = (y * sw as usize + x) * 4;
        [px[o], px[o + 1], px[o + 2], px[o + 3]]
    };
    assert_eq!(
        at(0, 0),
        [0x00, 0x00, 0xFF, 0xFF],
        "the ring keeps the border pixel; content at (0,0) would mean the \
         migrate copy dropped the inset",
    );
    assert_eq!(
        at(BW, BW),
        [0x00, 0xFF, 0x00, 0xFF],
        "the content starts at (bw, bw)",
    );
    assert_eq!(
        at(sw as usize - BW - 1, sh as usize - BW - 1),
        [0x00, 0xFF, 0x00, 0xFF],
        "and runs to the far content corner",
    );
}

/// Window-storage step 5, through core: storage exists only while viewable, subtree-wide.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn window_storage_exists_only_while_viewable() {
    const FRAME: u32 = 0x0069_0001;
    const CLIENT: u32 = 0x0069_0002;
    const INNER: u32 = 0x0069_0003;
    let Some(mut f) = ProtoFixture::new() else {
        eprintln!("skipping: no Vk");
        return;
    };
    let root_res = yserver_core::resources::ROOT_WINDOW.0;
    let vis = yserver_core::resources::ROOT_VISUAL.0;
    or_create_window(&mut f, FRAME, root_res, 24, 10, 10, 100, 80, 0, vis, 0, &[]);
    or_create_window(&mut f, CLIENT, FRAME, 24, 5, 5, 60, 50, 0, vis, 0, &[]);
    or_create_window(&mut f, INNER, CLIENT, 24, 2, 2, 20, 10, 0, vis, 0, &[]);
    let storage = |f: &ProtoFixture| {
        [FRAME, CLIENT, INNER].map(|w| f.backend.storage_extent_for_tests(f.host_xid(w)).is_some())
    };
    assert_eq!(storage(&f), [false; 3], "created unmapped: no storage");
    wz_map(&mut f, CLIENT);
    wz_map(&mut f, INNER);
    assert_eq!(
        storage(&f),
        [false; 3],
        "mapped under an unmapped frame: still none"
    );
    wz_map(&mut f, FRAME);
    assert_eq!(
        storage(&f),
        [true; 3],
        "the frame's map realizes the subtree"
    );
    f.req(10, 0, &FRAME.to_le_bytes()); // UnmapWindow
    assert_eq!(
        storage(&f),
        [false; 3],
        "the frame's unmap releases the subtree"
    );
    wz_map(&mut f, FRAME);
    assert_eq!(storage(&f), [true; 3], "the remap realizes it again");
}

/// Window-storage step 5, through core: a resize while unmapped is allocated at map time.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn resize_while_unmapped_allocates_at_the_new_size_on_map() {
    const W: u32 = 0x0069_0010;
    let Some(mut f) = ProtoFixture::new() else {
        eprintln!("skipping: no Vk");
        return;
    };
    let root_res = yserver_core::resources::ROOT_WINDOW.0;
    let vis = yserver_core::resources::ROOT_VISUAL.0;
    or_create_window(&mut f, W, root_res, 24, 0, 0, 40, 30, 2, vis, 0, &[]);
    wz_configure(&mut f, W, 0x4 | 0x8, &[64, 48]);
    let host = f.host_xid(W);
    assert_eq!(
        f.backend.storage_extent_for_tests(host),
        None,
        "no storage while unmapped"
    );
    wz_map(&mut f, W);
    assert_eq!(
        f.backend.storage_extent_for_tests(host),
        Some((68, 52)),
        "allocated at the new size plus the border",
    );
}

/// Window-storage step 5, through core: a bg-None window's remap storage is seeded from its parent.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn bg_none_window_is_seeded_from_its_parent_on_remap() {
    const PARENT: u32 = 0x0069_0020;
    const CHILD: u32 = 0x0069_0021;
    const GC: u32 = 0x0069_0022;
    const CW_BACK_PIXEL: u32 = 0x0000_0002;
    const RED: u32 = 0x00FF_0000;
    const BLUE: u32 = 0x0000_00FF;
    let Some(mut f) = ProtoFixture::new() else {
        eprintln!("skipping: no Vk");
        return;
    };
    let root_res = yserver_core::resources::ROOT_WINDOW.0;
    let vis = yserver_core::resources::ROOT_VISUAL.0;
    or_create_window(
        &mut f,
        PARENT,
        root_res,
        24,
        0,
        0,
        60,
        60,
        0,
        vis,
        CW_BACK_PIXEL,
        &[RED],
    );
    or_create_window(&mut f, CHILD, PARENT, 24, 10, 10, 20, 20, 0, vis, 0, &[]);
    wz_map(&mut f, PARENT);
    wz_map(&mut f, CHILD);
    or_create_gc(&mut f, GC, CHILD, BLUE);
    or_fill(&mut f, CHILD, GC, 0, 0, 20, 20);
    let uniform = |f: &mut ProtoFixture| {
        let (_, _, px) = f.backing(CHILD);
        let first: [u8; 3] = px[..3].try_into().expect("pixel");
        assert!(
            px.chunks_exact(4).all(|p| p[..3] == first),
            "child not uniform"
        );
        first
    };
    assert_eq!(
        uniform(&mut f),
        or_bgr(BLUE),
        "the child holds its own paint"
    );
    f.req(10, 0, &CHILD.to_le_bytes()); // UnmapWindow
    wz_map(&mut f, CHILD);
    assert_eq!(
        uniform(&mut f),
        or_bgr(RED),
        "the remap seeds from the parent"
    );
}
