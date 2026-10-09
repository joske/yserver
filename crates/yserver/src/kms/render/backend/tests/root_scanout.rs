use super::*;

fn dispatch_poly_fill_rectangle(
    state: &mut yserver_core::server::ServerState,
    backend: &mut KmsBackend,
    drawable: yserver_protocol::x11::ResourceId,
    gc: yserver_protocol::x11::ResourceId,
    rects: &[Rectangle16],
) {
    use yserver_core::{backend::Backend, core_loop::process_request};
    use yserver_protocol::x11::{ClientId, RequestHeader, SequenceNumber};

    let mut body = Vec::with_capacity(8 + rects.len() * 8);
    body.extend_from_slice(&drawable.0.to_le_bytes());
    body.extend_from_slice(&gc.0.to_le_bytes());
    for rect in rects {
        body.extend_from_slice(&rect.x.to_le_bytes());
        body.extend_from_slice(&rect.y.to_le_bytes());
        body.extend_from_slice(&rect.width.to_le_bytes());
        body.extend_from_slice(&rect.height.to_le_bytes());
    }

    process_request::process_request(
        state,
        backend as &mut dyn Backend,
        ClientId(14),
        SequenceNumber(1),
        RequestHeader {
            opcode: 70, // PolyFillRectangle
            data: 0,
            length_units: u32::try_from((4 + body.len()) / 4).expect("request length"),
        },
        &body,
        None,
    )
    .expect("process_request(PolyFillRectangle) must succeed");
}

/// #133 step 2 (P3): the core resource tree and the backend
/// geometry mirror must agree on `border_width` after a
/// ConfigureWindow. Divergence between the authoritative tree and
/// the render mirror is a standing bug class here (Step 2 DRIFT 2
/// exists for exactly that reason on `top_level_order`), so this
/// asserts the two side by side rather than the mirror alone.
///
/// This runs the real dispatcher, so it also covers the wire hop:
/// `handle_configure_window` → `HostSubwindowConfig::border_width`
/// → `KmsBackend::configure_subwindow`.
#[test]
fn configure_border_width_agrees_between_core_tree_and_backend_mirror() {
    use yserver_core::{resources::ROOT_WINDOW, server::ServerState};
    use yserver_protocol::x11::ResourceId;

    let mut state = ServerState::new();
    let mut backend = KmsBackend::for_tests();
    install_client_for_render(&mut state, 14);
    state
        .resources
        .window_mut(ROOT_WINDOW)
        .expect("root")
        .host_xid = yserver_core::backend::WindowHandle::from_raw(backend.core.window_id);

    let win = ResourceId(0x0133_0001);
    let host = create_live_window(&mut state, &mut backend, win, ROOT_WINDOW, 10, 20, 100, 50);
    let host_xid = host.as_raw();

    // Baseline: both trees start at the CreateWindow border width.
    assert_eq!(
        state
            .resources
            .window(win)
            .expect("core window")
            .border_width,
        0
    );
    assert_eq!(backend.windows[&host_xid].border_width, 0);

    // ConfigureWindow with CWBorderWidth (bit 4) only.
    dispatch_configure_window(&mut state, &mut backend, win, None, None, Some(16));
    let core_bw = state
        .resources
        .window(win)
        .expect("core window")
        .border_width;
    assert_eq!(core_bw, 16, "core tree took the new border width");
    assert_eq!(
        backend.windows[&host_xid].border_width, core_bw,
        "backend mirror must not drift from the core tree"
    );

    // And back down to zero — the bw == 0 path stays reachable.
    dispatch_configure_window(&mut state, &mut backend, win, None, None, Some(0));
    let core_bw = state
        .resources
        .window(win)
        .expect("core window")
        .border_width;
    assert_eq!(core_bw, 0);
    assert_eq!(backend.windows[&host_xid].border_width, core_bw);
}

/// A window background clear paints like Xorg's `miPaintWindow`
/// (GXcopy, all planes, FillSolid, children clipped out) whatever
/// state the last client GC left in the backend.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn clear_area_ignores_leftover_client_draw_state() {
    use yserver_core::{
        backend::{Backend, ClipState, GcFunction, SubwindowMode},
        resources::ROOT_WINDOW,
    };
    use yserver_protocol::x11::ResourceId;

    const OLD: u32 = 0x0012_3456;
    const NEW: u32 = 0x00ed_a870;
    const CHILD: u32 = 0x0000_00ff;
    type Leave = fn(&mut KmsBackend);
    let leftovers: [(&str, Leave); 5] = [
        ("GXxor", |b| b.core.current_function = GcFunction::Xor),
        ("plane mask", |b| b.core.current_plane_mask = 0x0000_ff00),
        ("NoOp", |b| b.core.current_function = GcFunction::NoOp),
        ("IncludeInferiors", |b| {
            b.core.current_subwindow_mode = SubwindowMode::IncludeInferiors;
        }),
        ("clip", |b| {
            b.core.current_clip = ClipState::Rectangles {
                origin: (0, 0),
                rects: yserver_protocol::x11::ClipRectangles {
                    ordering: 0,
                    x_origin: 0,
                    y_origin: 0,
                    // One 4x4 rect at the origin: x, y, w, h.
                    rectangles: [0u16, 0, 4, 4]
                        .iter()
                        .flat_map(|v| v.to_le_bytes())
                        .collect(),
                },
            };
        }),
    ];
    for (i, (name, leave)) in leftovers.into_iter().enumerate() {
        let mut state = yserver_core::server::ServerState::new();
        let mut backend = match KmsBackend::for_tests_with_vk() {
            Ok(b) => b,
            Err(e) => {
                eprintln!("skipping: no Vk: {e}");
                return;
            }
        };
        install_client_for_render(&mut state, 14);
        state
            .resources
            .window_mut(ROOT_WINDOW)
            .expect("root")
            .host_xid = yserver_core::backend::WindowHandle::from_raw(backend.core.window_id);
        let base = 0x0134_0000 + u32::try_from(i).expect("index") * 0x10;
        let top = ResourceId(base + 1);
        let child = ResourceId(base + 2);
        let top_xid =
            create_live_window(&mut state, &mut backend, top, ROOT_WINDOW, 10, 20, 64, 48).as_raw();
        let child_xid =
            create_live_window(&mut state, &mut backend, child, top, 40, 30, 16, 12).as_raw();
        backend
            .fill_rectangle(None, top_xid, OLD, 0, 0, 64, 48)
            .expect("fill top");
        backend
            .fill_rectangle(None, child_xid, CHILD, 0, 0, 16, 12)
            .expect("fill child");

        leave(&mut backend);
        backend
            .clear_area(None, top_xid, NEW, None, 0, 0, 64, 48, (0, 0))
            .expect("clear_area");

        let pixels = |backend: &mut KmsBackend, xid, w, h| -> Vec<u32> {
            backend
                .get_image_pixels_for_tests(xid, 2, 0, 0, w, h, !0)
                .expect("get_image")
                .expect("bytes")
                .chunks_exact(4)
                .map(|p| u32::from_le_bytes([p[0], p[1], p[2], p[3]]) & 0x00ff_ffff)
                .collect()
        };
        let top_px = pixels(&mut backend, top_xid, 64, 48);
        for (n, px) in top_px.iter().enumerate() {
            let (x, y) = (n % 64, n / 64);
            if (40..56).contains(&x) && (30..42).contains(&y) {
                continue;
            }
            assert_eq!(*px, NEW, "{name}: top ({x},{y}) is 0x{px:06x}");
        }
        let child_px = pixels(&mut backend, child_xid, 16, 12);
        assert!(
            child_px.iter().all(|&px| px == CHILD),
            "{name}: the clear reached the child"
        );
    }
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn process_request_root_fill_include_inferiors_matches_top_level_after_move() {
    use yserver_core::resources::ROOT_WINDOW;
    use yserver_protocol::x11::{ClientId, CreateGcRequest, ResourceId};

    let mut state = yserver_core::server::ServerState::new();
    let mut backend = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    install_client_for_render(&mut state, 14);
    state
        .resources
        .window_mut(ROOT_WINDOW)
        .expect("root")
        .host_xid = yserver_core::backend::WindowHandle::from_raw(backend.core.window_id);

    let top = ResourceId(0x2000);
    let gc = ResourceId(0x2001);
    let child_base = 0x2100u32;
    let grandchild_base = 0x2200u32;

    let top_host = create_live_window(&mut state, &mut backend, top, ROOT_WINDOW, 11, 7, 100, 90);
    let top_xid = top_host.as_raw();

    backend
        .fill_rectangle(None, top_xid, 0x0000_0000, 0, 0, 100, 90)
        .expect("clear top");
    backend
        .fill_rectangle(None, top_xid, 0x0000_00ff, 20, 30, 70, 30)
        .expect("baseline fill");
    let expected = backend
        .get_image_pixels_for_tests(top_xid, 2, 0, 0, 100, 90, !0)
        .expect("baseline get_image")
        .expect("baseline bytes");
    backend
        .fill_rectangle(None, top_xid, 0x0000_0000, 0, 0, 100, 90)
        .expect("re-clear top");

    state.resources.create_gc(
        ClientId(14),
        CreateGcRequest {
            gc,
            drawable: top,
            function: None,
            plane_mask: None,
            foreground: Some(0x0000_00ff),
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
            subwindow_mode: Some(1),
            graphics_exposures: None,
            clip_x_origin: None,
            clip_y_origin: None,
            clip_mask: None,
            dash_offset: None,
            dashes: None,
            arc_mode: None,
        },
    );

    for i in 0..4 {
        let child = ResourceId(child_base + i);
        create_live_window(
            &mut state,
            &mut backend,
            child,
            top,
            (i * 20) as i16,
            0,
            10,
            90,
        );
        for j in 0..9 {
            create_live_window(
                &mut state,
                &mut backend,
                ResourceId(grandchild_base + i * 16 + j),
                child,
                0,
                (j * 10) as i16,
                10,
                6,
            );
        }
    }

    let rect = [Rectangle16 {
        x: 20,
        y: 30,
        width: 70,
        height: 30,
    }];

    dispatch_poly_fill_rectangle(&mut state, &mut backend, top, gc, &rect);
    let top_include = backend
        .get_image_pixels_for_tests(top_xid, 2, 0, 0, 100, 90, !0)
        .expect("top include get_image")
        .expect("top include bytes");
    assert_eq!(
        top_include, expected,
        "top-level request path must match baseline"
    );

    backend
        .fill_rectangle(None, top_xid, 0x0000_0000, 0, 0, 100, 90)
        .expect("re-clear top");

    dispatch_configure_window(&mut state, &mut backend, top, Some(0), Some(0), Some(0));

    let geom = backend.windows.get(&top_xid).expect("top geom after move");
    assert_eq!(
        (geom.x, geom.y),
        (0, 0),
        "backend geometry must track ConfigureWindow"
    );

    dispatch_poly_fill_rectangle(&mut state, &mut backend, ROOT_WINDOW, gc, &rect);
    let root_out = backend
        .get_image_pixels_for_tests(top_xid, 2, 0, 0, 100, 90, !0)
        .expect("root-path get_image")
        .expect("root-path bytes");
    assert_eq!(root_out, expected);
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn root_get_image_reads_scanout_pixels_not_root_storage() {
    use ash::vk;
    use yserver_core::{resources::ROOT_WINDOW, server::ServerState};
    use yserver_protocol::x11::ResourceId;

    let mut state = ServerState::new();
    let mut backend = match KmsBackend::for_tests_with_vk_live_scene() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    install_client_for_render(&mut state, 14);
    state
        .resources
        .window_mut(ROOT_WINDOW)
        .expect("root")
        .host_xid = yserver_core::backend::WindowHandle::from_raw(backend.core.window_id);

    let (root_w, root_h) = {
        let root = state.resources.window(ROOT_WINDOW).expect("root window");
        (root.width, root.height)
    };
    let window = ResourceId(0x2002);
    let window_host = create_live_window(
        &mut state,
        &mut backend,
        window,
        ROOT_WINDOW,
        32,
        24,
        16,
        16,
    );

    let root_color = 0x0011_2233;
    let window_color = 0x00aa_55ff;
    backend
        .fill_rectangle(
            None,
            backend.core.window_id,
            root_color,
            0,
            0,
            root_w,
            root_h,
        )
        .expect("fill root");
    backend
        .fill_rectangle(None, window_host.as_raw(), window_color, 0, 0, 16, 16)
        .expect("fill window");
    backend.tick_maybe_composite_for_tests();

    let scan_rect = vk::Rect2D {
        offset: vk::Offset2D { x: 32, y: 24 },
        extent: vk::Extent2D {
            width: 16,
            height: 16,
        },
    };
    let scanout = crate::kms::render::backend::read_scanout_region(
        &mut backend,
        scan_rect,
        crate::kms::render::backend::ScanoutReadSelection::OnScreenOnly,
    )
    .expect("scanout readback");
    let root_out = backend
        .get_image_pixels_for_tests(backend.core.window_id, 2, 32, 24, 16, 16, !0)
        .expect("root get_image")
        .expect("root bytes");
    let window_out = backend
        .get_image_pixels_for_tests(window_host.as_raw(), 2, 0, 0, 16, 16, !0)
        .expect("window get_image")
        .expect("window bytes");

    assert_eq!(
        root_out, scanout,
        "root GetImage must read the on-screen scanout"
    );
    assert_eq!(
        root_out, window_out,
        "root GetImage must match the visible window pixels"
    );
}

/// Protocol-visible half of the direct-scanout stale-read bug: a
/// root-source read (here root `GetImage`, which shares
/// `read_root_scanout_assembled` with root-source `CopyArea` and the root
/// screenshot pixmap) must return the flipped client buffer.
///
/// The fixture has a live render engine but NO scanout pools
/// (`for_tests_with_vk_live_scene`, the only fixture that allocates them,
/// needs a real DRM device and skips on lavapipe), so the composed route
/// here can only fail and zero-fill. That is what makes the assertion
/// bite: before the fix this read went to the pool unconditionally and
/// came back all-black. The composed-versus-direct A/B on the same live
/// pool needs hardware.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn root_read_during_direct_scanout_returns_the_flipped_source() {
    use yserver_core::backend::Backend;

    let mut backend = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let (fb_w, fb_h) = (backend.platform.fb_w, backend.platform.fb_h);

    let read = |b: &mut KmsBackend| {
        b.get_image_pixels_for_tests(b.core.window_id, 2, 32, 24, 16, 16, !0)
            .expect("root get_image")
            .expect("root bytes")
    };
    let bgr = |bytes: &[u8]| -> Vec<[u8; 3]> {
        bytes.chunks_exact(4).map(|p| [p[0], p[1], p[2]]).collect()
    };

    // Control: with no direct frame the read takes the composed route,
    // which this fixture cannot satisfy, so every piece zero-fills.
    let composed = read(&mut backend);
    assert_eq!(composed.len(), 16 * 16 * 4);
    assert!(
        bgr(&composed).iter().all(|px| *px == [0, 0, 0]),
        "control: the composed route has no pool bo here"
    );

    // A client buffer the size of the root, flipped straight onto the
    // CRTC. The compositor never composes it.
    let direct_color = 0x00aa_55ff_u32;
    let expected = [
        u8::try_from(direct_color & 0xff).unwrap(),
        u8::try_from((direct_color >> 8) & 0xff).unwrap(),
        u8::try_from((direct_color >> 16) & 0xff).unwrap(),
    ];
    let source = backend
        .create_pixmap(None, 24, fb_w, fb_h)
        .expect("direct source pixmap");
    let source_xid = source.as_raw();
    backend
        .fill_rectangle(None, source_xid, direct_color, 0, 0, fb_w, fb_h)
        .expect("fill direct source");
    let root_xid = backend.core.window_id;
    retain_direct_frame_from_source_test(&mut backend, source_xid, root_xid, fb_w, fb_h);

    let direct = read(&mut backend);
    assert_eq!(direct.len(), 16 * 16 * 4);
    assert!(
        bgr(&direct).iter().all(|px| *px == expected),
        "a root read during direct scanout must return the flipped source's \
             pixels; got {:?}",
        bgr(&direct).first()
    );

    // Dropping the direct frame puts the read back on the composed route.
    backend.scanout_m2.current = None;
    backend.scanout_m2.hold_direct = false;
    assert!(
        bgr(&read(&mut backend)).iter().all(|px| *px == [0, 0, 0]),
        "the composed route must come back once the CRTC is unflipped"
    );
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn root_overlay_xor_pass_reaches_scanout() {
    // Task 6: the retained root-`IncludeInferiors` overlay is applied
    // as an XOR pass at the END of compose, into the freshly-
    // composited scanout BO. Drive one compose with the whole root
    // filled a uniform color and an Invert (`value = plane_mask`)
    // overlay toggled over ONE sub-region, then read the scanout back:
    // the overlaid region must differ from an identically-filled
    // control region that has no overlay (i.e. the XOR reached the
    // scanout), while the control region reflects the untouched fill.
    use ash::vk;
    use yserver_core::{resources::ROOT_WINDOW, server::ServerState};
    use yserver_protocol::x11::ClientId;

    let mut state = ServerState::new();
    let mut backend = match KmsBackend::for_tests_with_vk_live_scene() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    install_client_for_render(&mut state, 14);
    state
        .resources
        .window_mut(ROOT_WINDOW)
        .expect("root")
        .host_xid = yserver_core::backend::WindowHandle::from_raw(backend.core.window_id);

    let (root_w, root_h) = {
        let root = state.resources.window(ROOT_WINDOW).expect("root window");
        (root.width, root.height)
    };

    // Uniform root fill so the overlaid region and the control region
    // start from the same pixels.
    let root_color = 0x0033_5577;
    backend
        .fill_rectangle(
            None,
            backend.core.window_id,
            root_color,
            0,
            0,
            root_w,
            root_h,
        )
        .expect("fill root");

    // Overlay region + an identically-filled control region.
    let overlay_rect = vk::Rect2D {
        offset: vk::Offset2D { x: 20, y: 20 },
        extent: vk::Extent2D {
            width: 60,
            height: 60,
        },
    };
    let control_rect = vk::Rect2D {
        offset: vk::Offset2D { x: 200, y: 20 },
        extent: vk::Extent2D {
            width: 60,
            height: 60,
        },
    };

    // Invert: xor_value = plane_mask (24-bit). Injects damage so the
    // compose actually runs over the region.
    let xor_value = 0x00ff_ffff;
    backend
        .scene
        .root_overlay_toggle(ClientId(14), xor_value, &[overlay_rect]);
    assert!(
        !backend.scene.root_overlay.is_empty(),
        "overlay op must be retained"
    );

    backend.tick_maybe_composite_for_tests();

    let overlaid = crate::kms::render::backend::read_scanout_region(
        &mut backend,
        overlay_rect,
        crate::kms::render::backend::ScanoutReadSelection::OnScreenOnly,
    )
    .expect("scanout readback (overlay)");
    let control = crate::kms::render::backend::read_scanout_region(
        &mut backend,
        control_rect,
        crate::kms::render::backend::ScanoutReadSelection::OnScreenOnly,
    )
    .expect("scanout readback (control)");

    assert!(
        !overlaid.is_empty() && !control.is_empty(),
        "non-empty reads"
    );
    assert_eq!(
        overlaid.len(),
        control.len(),
        "same-size regions read same byte count"
    );
    assert_ne!(
        overlaid, control,
        "XOR overlay pass must have changed the overlaid scanout region \
             relative to the identically-filled control region"
    );
    // Control region is uniform (the untouched fill): every pixel equals
    // the first, confirming the compose ran and only the overlay region
    // was toggled.
    let px = &control[0..4];
    assert!(
        control.chunks_exact(4).all(|c| c == px),
        "control region must be the uniform root fill (overlay must not \
             have touched it)"
    );
}

/// Window-storage step 4: a NameWindowPixmap taken before an unmap keeps
/// the released backing alive and readable, and the remap hands the
/// window a new backing (Xorg compUnrealizeWindow / compRealizeWindow).
#[test]
#[ignore = "needs live Vulkan ICD"]
fn named_pixmap_survives_unmap_and_remap_gets_new_backing() {
    use yserver_core::{backend::Backend, resources::ROOT_WINDOW, server::ServerState};
    use yserver_protocol::x11::ResourceId;

    let mut state = ServerState::new();
    let mut backend = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    install_client_for_render(&mut state, 14);
    state
        .resources
        .window_mut(ROOT_WINDOW)
        .expect("root")
        .host_xid = yserver_core::backend::WindowHandle::from_raw(backend.core.window_id);

    let window = ResourceId(0x0140_0001);
    let named = ResourceId(0x0140_0002);
    let host = create_live_window(
        &mut state,
        &mut backend,
        window,
        ROOT_WINDOW,
        10,
        10,
        16,
        16,
    );
    let window_body = window.0.to_le_bytes();
    let mut redirect = window.0.to_le_bytes().to_vec();
    redirect.extend_from_slice(&[1, 0, 0, 0]); // Manual
    dispatch_raw(&mut state, &mut backend, 144, 1, &redirect); // RedirectWindow
    let first = state
        .resources
        .window(window)
        .unwrap()
        .redirected_backing
        .as_ref()
        .expect("viewable redirected window has a backing")
        .host_pixmap
        .as_raw();

    let color = 0x00aa_55ff;
    backend
        .fill_rectangle(None, host.as_raw(), color, 0, 0, 16, 16)
        .expect("paint window");
    let mut name = window.0.to_le_bytes().to_vec();
    name.extend_from_slice(&named.0.to_le_bytes());
    dispatch_raw(&mut state, &mut backend, 144, 6, &name); // NameWindowPixmap
    let named_host = state
        .resources
        .pixmap(named)
        .and_then(|p| p.host_xid)
        .expect("named pixmap resource")
        .as_raw();
    assert_eq!(named_host, first, "named pixmap is the backing");

    dispatch_raw(&mut state, &mut backend, 10, 0, &window_body); // UnmapWindow
    assert!(
        state
            .resources
            .window(window)
            .unwrap()
            .redirected_backing
            .is_none()
    );
    assert!(
        state.composite_redirects.window_mode(window).is_some(),
        "redirect stays"
    );
    assert_eq!(backend.test_host_window_to_backing(host.as_raw()), None);
    assert_eq!(
        backend.test_alias_registry_get(first).map(|e| e.refcount),
        Some(1),
        "only the named pixmap holds the old backing",
    );
    let read = |backend: &mut KmsBackend, xid: u32| {
        backend
            .get_image_pixels_for_tests(xid, 2, 0, 0, 16, 16, !0)
            .expect("get_image")
            .expect("bytes")
    };
    let painted = |bytes: &[u8]| {
        bytes
            .chunks_exact(4)
            .all(|px| px[..3] == [0xff, 0x55, 0xaa])
    };
    assert!(
        painted(&read(&mut backend, named_host)),
        "named pixmap readable after unmap"
    );
    let copy = backend.create_pixmap(None, 24, 16, 16).expect("copy dst");
    backend
        .copy_area(None, named_host, copy.as_raw(), 0, 0, 0, 0, 16, 16)
        .expect("copy");
    assert!(
        painted(&read(&mut backend, copy.as_raw())),
        "CopyArea from named pixmap"
    );

    dispatch_raw(&mut state, &mut backend, 8, 0, &window_body); // MapWindow
    let second = state
        .resources
        .window(window)
        .unwrap()
        .redirected_backing
        .as_ref()
        .expect("remap re-creates the backing")
        .host_pixmap
        .as_raw();
    assert_ne!(second, first, "remap gets a new backing");
    assert_eq!(
        backend.test_host_window_to_backing(host.as_raw()),
        Some(second)
    );
    assert_eq!(
        state
            .resources
            .pixmap(named)
            .and_then(|p| p.host_xid)
            .map(|h| h.as_raw()),
        Some(first),
        "named pixmap keeps the old backing",
    );
    assert!(
        painted(&read(&mut backend, named_host)),
        "named pixmap unchanged by the remap"
    );

    dispatch_raw(&mut state, &mut backend, 54, 0, &named.0.to_le_bytes()); // FreePixmap
    assert!(
        backend.test_alias_registry_get(first).is_none(),
        "FreePixmap drops the last hold"
    );
    assert_eq!(
        backend.test_alias_registry_get(second).map(|e| e.refcount),
        Some(1)
    );
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn root_copy_area_include_inferiors_captures_window_into_pixmap() {
    use yserver_core::{backend::SubwindowMode, resources::ROOT_WINDOW, server::ServerState};
    use yserver_protocol::x11::ResourceId;

    let mut state = ServerState::new();
    let mut backend = match KmsBackend::for_tests_with_vk_live_scene() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    install_client_for_render(&mut state, 14);
    state
        .resources
        .window_mut(ROOT_WINDOW)
        .expect("root")
        .host_xid = yserver_core::backend::WindowHandle::from_raw(backend.core.window_id);

    let (root_w, root_h) = {
        let root = state.resources.window(ROOT_WINDOW).expect("root window");
        (root.width, root.height)
    };
    let window = ResourceId(0x2002);
    let window_host = create_live_window(
        &mut state,
        &mut backend,
        window,
        ROOT_WINDOW,
        32,
        24,
        16,
        16,
    );

    let root_color = 0x0011_2233;
    let window_color = 0x00aa_55ff;
    backend
        .fill_rectangle(
            None,
            backend.core.window_id,
            root_color,
            0,
            0,
            root_w,
            root_h,
        )
        .expect("fill root");
    backend
        .fill_rectangle(None, window_host.as_raw(), window_color, 0, 0, 16, 16)
        .expect("fill window");
    backend.tick_maybe_composite_for_tests();

    // Screenshot path: CopyArea(src=root, dst=pixmap) with a GC whose
    // subwindow-mode is IncludeInferiors (Qt5 QScreen::grabWindow).
    let dst = backend
        .create_pixmap(None, 24, root_w, root_h)
        .expect("create destination pixmap");
    backend.core.current_subwindow_mode = SubwindowMode::IncludeInferiors;
    backend
        .copy_area(
            None,
            backend.core.window_id,
            dst.as_raw(),
            0,
            0,
            0,
            0,
            root_w,
            root_h,
        )
        .expect("copy root to pixmap");

    // The mapped window's region in the destination must carry the
    // composited WINDOW pixels, not the root background. On the buggy
    // path copy_area reads root STORAGE (background), so this region
    // comes back as root_color and the assert fails.
    let win_region = backend
        .get_image_pixels_for_tests(dst.as_raw(), 2, 32, 24, 16, 16, !0)
        .expect("dst get_image window region")
        .expect("dst window bytes");
    let scanout_win = crate::kms::render::backend::read_scanout_region(
        &mut backend,
        ash::vk::Rect2D {
            offset: ash::vk::Offset2D { x: 32, y: 24 },
            extent: ash::vk::Extent2D {
                width: 16,
                height: 16,
            },
        },
        crate::kms::render::backend::ScanoutReadSelection::OnScreenOnly,
    )
    .expect("scanout readback");
    assert_eq!(
        win_region, scanout_win,
        "root CopyArea(IncludeInferiors) must copy the composited desktop \
             (window pixels), not the root background"
    );

    // A region with no window on top must still carry the root background.
    let bg_region = backend
        .get_image_pixels_for_tests(dst.as_raw(), 2, 0, 0, 8, 8, !0)
        .expect("dst get_image bg region")
        .expect("dst bg bytes");
    let scanout_bg = crate::kms::render::backend::read_scanout_region(
        &mut backend,
        ash::vk::Rect2D {
            offset: ash::vk::Offset2D { x: 0, y: 0 },
            extent: ash::vk::Extent2D {
                width: 8,
                height: 8,
            },
        },
        crate::kms::render::backend::ScanoutReadSelection::OnScreenOnly,
    )
    .expect("scanout bg readback");
    assert_eq!(
        bg_region, scanout_bg,
        "root CopyArea(IncludeInferiors) must copy the background where no window covers"
    );
}

#[test]
fn split_root_scanout_reads_single_output_whole_rect() {
    let got = crate::kms::render::backend::split_root_scanout_reads(
        r(0, 0, 100, 50),
        0,
        0,
        0,
        0,
        &[(0, 0, 200, 200)],
    );
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].read, r(0, 0, 100, 50));
    assert_eq!(got[0].dst_local, ash::vk::Offset2D { x: 0, y: 0 });
}

#[test]
fn split_root_scanout_reads_honors_src_origin() {
    // Copy from root (32,24) into dst (0,0): read root-absolute (32,24),
    // write to dst-local (0,0).
    let got = crate::kms::render::backend::split_root_scanout_reads(
        r(0, 0, 16, 16),
        32,
        24,
        0,
        0,
        &[(0, 0, 800, 600)],
    );
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].read, r(32, 24, 16, 16));
    assert_eq!(got[0].dst_local, ash::vk::Offset2D { x: 0, y: 0 });
}

#[test]
fn split_root_scanout_reads_spanning_two_outputs() {
    // Full 200x100 grab across two 100-wide side-by-side outputs → two reads.
    let got = crate::kms::render::backend::split_root_scanout_reads(
        r(0, 0, 200, 100),
        0,
        0,
        0,
        0,
        &[(0, 0, 100, 100), (100, 0, 100, 100)],
    );
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].read, r(0, 0, 100, 100));
    assert_eq!(got[0].dst_local, ash::vk::Offset2D { x: 0, y: 0 });
    assert_eq!(got[1].read, r(100, 0, 100, 100));
    assert_eq!(got[1].dst_local, ash::vk::Offset2D { x: 100, y: 0 });
}

#[test]
fn split_root_scanout_reads_clips_partially_offscreen() {
    // Requested 200x200 but output is only 150x120: read the covered piece.
    let got = crate::kms::render::backend::split_root_scanout_reads(
        r(0, 0, 200, 200),
        0,
        0,
        0,
        0,
        &[(0, 0, 150, 120)],
    );
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].read, r(0, 0, 150, 120));
    assert_eq!(got[0].dst_local, ash::vk::Offset2D { x: 0, y: 0 });
}

#[test]
fn split_root_scanout_reads_fully_offscreen_is_empty() {
    let got = crate::kms::render::backend::split_root_scanout_reads(
        r(0, 0, 50, 50),
        500,
        500,
        0,
        0,
        &[(0, 0, 100, 100)],
    );
    assert!(got.is_empty());
}

#[test]
fn assemble_root_scanout_spans_two_outputs() {
    // Two 2x2 outputs side by side; a full 4x2 root region. Left output's
    // read returns solid 0x11, right returns 0x22. The assembled buffer must
    // carry each output's pixels in its half — the exact dual-monitor case
    // the old single-`read_scanout_region` path returned all-black for
    // (rect spanning two outputs → no matching BO → empty reply).
    let outputs = [(0i32, 0i32, 2u32, 2u32), (2, 0, 2, 2)];
    let got =
        crate::kms::render::backend::assemble_root_scanout(r(0, 0, 4, 2), &outputs, |rect, _| {
            let px = (rect.extent.width * rect.extent.height) as usize;
            let byte = if rect.offset.x == 0 { 0x11u8 } else { 0x22u8 };
            Some(vec![byte; px * 4])
        });
    assert_eq!(got.len(), 4 * 2 * 4);
    // stride = 4 px * 4 bytes = 16. Left 2 px (8 bytes) then right 2 px.
    assert_eq!(&got[0..8], &[0x11u8; 8], "row0 left half from output 0");
    assert_eq!(&got[8..16], &[0x22u8; 8], "row0 right half from output 1");
    assert_eq!(&got[16..24], &[0x11u8; 8], "row1 left half from output 0");
    assert_eq!(&got[24..32], &[0x22u8; 8], "row1 right half from output 1");
}

#[test]
fn assemble_root_scanout_reads_uncovered_area_from_the_root_background() {
    // A 4x2 output over a 6x3 root region: the column right of it and the
    // row below it are covered by no CRTC and read from the root storage
    // (Xorg answers the root background there), never from a scanout.
    let outputs = [(0i32, 0i32, 4u32, 2u32)];
    let mut background_px = 0;
    let got = crate::kms::render::backend::assemble_root_scanout(
        r(0, 0, 6, 3),
        &outputs,
        |rect, source| {
            let px = (rect.extent.width * rect.extent.height) as usize;
            match source {
                crate::kms::render::backend::RootReadSource::Scanout => {
                    assert_eq!(rect, r(0, 0, 4, 2), "scanout read stays on the output");
                    Some(vec![0x11u8; px * 4])
                }
                crate::kms::render::backend::RootReadSource::Background => {
                    background_px += px;
                    Some(vec![0x22u8; px * 4])
                }
            }
        },
    );
    assert_eq!(background_px, 6 * 3 - 4 * 2, "every uncovered pixel, once");
    let stride = 6 * 4;
    assert_eq!(&got[0..16], &[0x11u8; 16], "row0 under the output");
    assert_eq!(&got[16..24], &[0x22u8; 8], "row0 right of the output");
    assert_eq!(&got[2 * stride..3 * stride], &[0x22u8; 24], "row2 below it");
}

#[test]
fn assemble_root_scanout_failed_read_is_zero_filled() {
    // A piece whose read fails stays zero (black) instead of aborting the
    // whole capture; the covered output still lands.
    let outputs = [(0i32, 0i32, 2u32, 2u32), (2, 0, 2, 2)];
    let got =
        crate::kms::render::backend::assemble_root_scanout(r(0, 0, 4, 1), &outputs, |rect, _| {
            if rect.offset.x == 0 {
                Some(vec![
                    0x11u8;
                    (rect.extent.width * rect.extent.height) as usize * 4
                ])
            } else {
                None
            }
        });
    assert_eq!(&got[0..8], &[0x11u8; 8], "covered output landed");
    assert_eq!(&got[8..16], &[0x00u8; 8], "failed-read output stays black");
}

#[test]
fn split_root_scanout_reads_output_offset_shifts_dst_local() {
    // Output starts at x=50: the left half (root x 0..50) is off-screen and
    // dropped; the right half reads root x50.. and writes to dst-local x50.
    let got = crate::kms::render::backend::split_root_scanout_reads(
        r(0, 0, 100, 100),
        0,
        0,
        0,
        0,
        &[(50, 0, 800, 600)],
    );
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].read, r(50, 0, 50, 100));
    assert_eq!(got[0].dst_local, ash::vk::Offset2D { x: 50, y: 0 });
}

#[test]
fn split_root_scanout_reads_carries_subrect_dst_offset() {
    // A GC-clip sub-rect at dst-local (20,10): source tracks it, dst-local
    // offset is preserved.
    let got = crate::kms::render::backend::split_root_scanout_reads(
        r(20, 10, 30, 30),
        0,
        0,
        0,
        0,
        &[(0, 0, 800, 600)],
    );
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].read, r(20, 10, 30, 30));
    assert_eq!(got[0].dst_local, ash::vk::Offset2D { x: 20, y: 10 });
}

// ── Direct scanout: an on-screen read must follow the flipped source ──
//
// While a CRTC scans out a client buffer directly its pool BOs are not
// painted at all, so a pool read answers with content that is not on
// screen and can be arbitrarily old.

fn outputs_set(indices: &[usize]) -> std::collections::HashSet<usize> {
    indices.iter().copied().collect()
}

#[test]
fn direct_frame_slot_is_composed_without_any_direct_frame() {
    assert_eq!(
        crate::kms::render::backend::direct_frame_slot_on_output(0, None, false, &outputs_set(&[])),
        None
    );
}

#[test]
fn direct_frame_slot_current_covers_every_output() {
    for idx in 0..2 {
        assert_eq!(
            crate::kms::render::backend::direct_frame_slot_on_output(
                idx,
                None,
                true,
                &outputs_set(&[])
            ),
            Some(crate::kms::render::backend::DirectFrameSlot::Current),
            "a fully retired direct frame is on screen on every CRTC"
        );
    }
}

#[test]
fn direct_frame_slot_pending_only_on_outputs_where_it_retired() {
    // Successor submitted to both CRTCs, retired on output 0 only.
    let awaiting = outputs_set(&[1]);
    assert_eq!(
        crate::kms::render::backend::direct_frame_slot_on_output(
            0,
            Some(&awaiting),
            true,
            &outputs_set(&[])
        ),
        Some(crate::kms::render::backend::DirectFrameSlot::Pending)
    );
    assert_eq!(
        crate::kms::render::backend::direct_frame_slot_on_output(
            1,
            Some(&awaiting),
            true,
            &outputs_set(&[])
        ),
        Some(crate::kms::render::backend::DirectFrameSlot::Current),
        "output 1 still shows the predecessor until its flip retires"
    );
}

#[test]
fn direct_frame_slot_first_direct_frame_leaves_unretired_outputs_composed() {
    // No predecessor: an output that has not retired the first direct
    // flip is still compositing into its own pool.
    let awaiting = outputs_set(&[1]);
    assert_eq!(
        crate::kms::render::backend::direct_frame_slot_on_output(
            0,
            Some(&awaiting),
            false,
            &outputs_set(&[])
        ),
        Some(crate::kms::render::backend::DirectFrameSlot::Pending)
    );
    assert_eq!(
        crate::kms::render::backend::direct_frame_slot_on_output(
            1,
            Some(&awaiting),
            false,
            &outputs_set(&[])
        ),
        None
    );
}

#[test]
fn direct_frame_slot_composed_unflip_retires_per_output() {
    // Composed unflip submitted to both CRTCs, retired on output 0 only.
    let unflip = outputs_set(&[1]);
    assert_eq!(
        crate::kms::render::backend::direct_frame_slot_on_output(0, None, true, &unflip),
        None,
        "output 0 is back on its composited BO"
    );
    assert_eq!(
        crate::kms::render::backend::direct_frame_slot_on_output(1, None, true, &unflip),
        Some(crate::kms::render::backend::DirectFrameSlot::Current),
        "output 1 is still scanning out the client buffer"
    );
}

#[test]
fn scanout_read_route_follows_the_direct_source() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let target_xid = 0x5700;
    let fallback_id = seed_window(&mut b, target_xid, None, 0, 0);
    let (source_id, _, _, _) =
        install_direct_frame_for_target_test(&mut b, target_xid, fallback_id, true);

    let route = crate::kms::render::backend::select_scanout_read_route(
        &b,
        r(10, 20, 30, 40),
        crate::kms::render::backend::ScanoutReadSelection::OnScreenOnly,
    )
    .expect("a flipped CRTC resolves a direct route");
    match route {
        crate::kms::render::backend::ScanoutReadRoute::Direct {
            source_id: got,
            depth,
            source,
            ..
        } => {
            assert_eq!(got, source_id, "must read the flipped source drawable");
            assert_eq!(depth, 24);
            assert_eq!(
                source,
                r(10, 20, 30, 40),
                "x_off/y_off are pinned to zero, so root-absolute maps 1:1"
            );
        }
        other => panic!("expected a direct route, got {other:?}"),
    }
}

#[test]
fn scanout_read_route_is_composed_once_the_direct_frame_is_gone() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let target_xid = 0x5704;
    let fallback_id = seed_window(&mut b, target_xid, None, 0, 0);
    install_direct_frame_for_target_test(&mut b, target_xid, fallback_id, true);
    b.scanout_m2.current = None;

    // The stub fixture has no scanout pools, so the composed branch can
    // only report "not covered by any pool" — which is still proof the
    // direct branch was not taken.
    let err = crate::kms::render::backend::select_scanout_read_route(
        &b,
        r(10, 20, 30, 40),
        crate::kms::render::backend::ScanoutReadSelection::PermissiveDump,
    )
    .expect_err("stub fixture has no pool bos");
    assert!(
        err.to_string().contains("not covered by any pool"),
        "expected the composed-pool branch, got {err}"
    );
}

#[test]
fn scanout_read_route_rejects_a_rect_outside_the_direct_source() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let target_xid = 0x5708;
    let fallback_id = seed_window(&mut b, target_xid, None, 0, 0);
    install_direct_frame_for_target_test(&mut b, target_xid, fallback_id, true);

    // The fixture source is 100x100; the output is 800x600.
    let err = crate::kms::render::backend::select_scanout_read_route(
        &b,
        r(0, 0, 800, 600),
        crate::kms::render::backend::ScanoutReadSelection::PermissiveDump,
    )
    .expect_err("an unreadable direct source must not fall back to the pool");
    assert!(
        err.to_string().contains("falls outside direct source"),
        "expected the direct-source bounds error, got {err}"
    );
}

#[test]
fn scanout_read_route_rejects_a_vanished_direct_source() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let target_xid = 0x570c;
    let fallback_id = seed_window(&mut b, target_xid, None, 0, 0);
    install_direct_frame_for_target_test(&mut b, target_xid, fallback_id, true);
    b.scanout_m2
        .current
        .as_mut()
        .expect("direct frame installed")
        .source_id = crate::kms::render::store::DrawableId::for_tests(0x00ff_ffff);

    let err = crate::kms::render::backend::select_scanout_read_route(
        &b,
        r(10, 20, 30, 40),
        crate::kms::render::backend::ScanoutReadSelection::PermissiveDump,
    )
    .expect_err("an unresolvable direct source must be reported, not papered over");
    assert!(
        err.to_string().contains("no longer in the drawable store"),
        "expected the missing-source error, got {err}"
    );
}

#[test]
fn scanout_read_route_is_per_output_during_a_composed_unflip() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    push_test_output(&mut b, 2);
    // Two side-by-side outputs small enough to sit inside the fixture's
    // 100x100 direct source, which stands in for the real invariant that
    // a direct source spans the whole root framebuffer.
    for (idx, x) in [(0usize, 0i32), (1usize, 50i32)] {
        b.platform.outputs[idx].x = x;
        b.platform.outputs[idx].y = 0;
        b.platform.outputs[idx].width = 50;
        b.platform.outputs[idx].height = 50;
    }
    let target_xid = 0x5710;
    let fallback_id = seed_window(&mut b, target_xid, None, 0, 0);
    install_direct_frame_for_target_test(&mut b, target_xid, fallback_id, true);
    // Composed unflip retired on output 0, still awaited on output 1.
    b.scanout_m2.unflip_awaiting_outputs = outputs_set(&[1]);

    assert!(
        b.direct_scanout_frame_for_output(0).is_none(),
        "output 0 went back to its composited BO"
    );
    assert!(
        b.direct_scanout_frame_for_output(1).is_some(),
        "output 1 is still scanning out the client buffer"
    );

    // A rect wholly inside output 1 still resolves to the direct source,
    // rebased into that drawable's own space (the source covers the whole
    // root, so the output origin is NOT subtracted).
    let route = crate::kms::render::backend::select_scanout_read_route(
        &b,
        r(60, 10, 20, 20),
        crate::kms::render::backend::ScanoutReadSelection::PermissiveDump,
    )
    .expect("output 1 is direct");
    match route {
        crate::kms::render::backend::ScanoutReadRoute::Direct { source, .. } => assert_eq!(
            source,
            r(60, 10, 20, 20),
            "the source spans the root, so the output origin is NOT subtracted"
        ),
        other => panic!("expected a direct route on the still-flipped output, got {other:?}"),
    }
}

#[test]
fn scanout_read_origin_labels_name_the_buffer() {
    assert_eq!(
        crate::kms::render::backend::ScanoutReadOrigin::ComposedPool {
            pool_idx: 1,
            bo_idx: 2
        }
        .label(),
        "composed-pool1-bo2"
    );
    assert_eq!(
        crate::kms::render::backend::ScanoutReadOrigin::DirectSource {
            source_xid: 0x4a0_0007
        }
        .label(),
        "direct-src-0x4a00007"
    );
}

/// A hotplug that GREW the virtual extent must grow root backing
/// storage with it.
///
/// Measured chain this guards (dual-head MATE, HDMI-3 unplug/replug):
/// the desktop reacted to the unplug with `RRSetScreenSize(2560x1440)`,
/// which reallocated root storage down to the survivor; the relight then
/// grew the extent back to 5120x1440 but nothing resized root storage,
/// so x=2560..5120 -- exactly the relit monitor -- had no root pixels
/// behind it and the monitor showed no background at all.
#[test]
fn a_hotplug_that_grew_the_extent_grows_root_backing_storage() {
    use yserver_core::backend::Backend;

    let mut b = KmsBackend::for_tests();
    clear_test_outputs(&mut b);
    let survivor = push_enabled_test_output(&mut b, "A", 7, 0, 0, 1920, 1080);

    // The desktop shrank the logical screen around the survivor while the
    // second monitor was away; root storage followed it down.
    b.set_logical_screen_size(1920, 1080)
        .expect("logical resize must not fail on the test fixture");
    assert_eq!(
        b.root_storage_extent().map(|e| (e.width, e.height)),
        Some((1920, 1080)),
        "the client resize is what leaves root storage too small",
    );

    // The relight puts the second monitor back at its remembered slot and
    // the enable path recomputes the extent over both live layouts.
    let relit = push_enabled_test_output(&mut b, "B", 8, 1920, 0, 1920, 1080);
    b.platform.recompute_fb_extent_with_reservations(&[]);
    assert_eq!(b.platform.fb_dimensions(), (3840, 1080));

    let (outputs, modes) = b.randr_outputs_and_modes();
    let mut state = ServerState::with_randr_outputs_and_modes(
        1920,
        1080,
        outputs,
        modes,
        yserver_core::server::BackendCapabilities::from_backend(&b),
    );
    let rescan = crate::kms::render::platform::RescanResult {
        added_keys: vec![relit],
        dropped_keys: Vec::new(),
        dropped_old_indices: Vec::new(),
        dropped_layouts: Vec::new(),
        added_count: 1,
        connected: Vec::new(),
    };
    assert!(
        b.fire_randr_changes(&mut state, rescan, &[survivor], true, true, false),
        "publishing the relit topology must succeed",
    );

    assert_eq!(
        b.root_storage_extent().map(|e| (e.width, e.height)),
        Some((3840, 1080)),
        "root storage must cover the grown extent, or the relit output \
             has nothing to sample",
    );
}

/// The counterpart: an extent that SHRANK leaves root storage alone.
/// Oversized storage still covers every visible pixel, and reallocating
/// it would wipe root content that is still on screen.
#[test]
fn a_hotplug_that_shrank_the_extent_leaves_root_storage_alone() {
    let mut b = KmsBackend::for_tests();
    clear_test_outputs(&mut b);
    let a = push_enabled_test_output(&mut b, "A", 7, 0, 0, 1920, 1080);
    let departing = push_enabled_test_output(&mut b, "B", 8, 1920, 0, 1920, 1080);
    b.platform.recompute_fb_extent_with_reservations(&[]);
    // Start from storage that actually covers the dual-head extent.
    // `set_logical_screen_size` would early-return here: the extent
    // recompute above already moved `fb_w`/`fb_h`, so drive the shared
    // helper it delegates to.
    b.apply_virtual_screen_extent(3840, 1080)
        .expect("growing the virtual extent must not fail");
    assert_eq!(
        b.root_storage_extent().map(|e| (e.width, e.height)),
        Some((3840, 1080)),
    );

    // B departs for good: no reservation, so the extent shrinks.
    b.randr_id_alloc.entry_mut(&departing).last_enabled = None;
    b.platform.outputs.pop();
    b.platform.scanout_pools.pop();
    b.platform.bo_generations.pop();
    b.platform.first_pageflip_logged.pop();
    b.platform.recompute_fb_extent_with_reservations(&[]);
    assert_eq!(b.platform.fb_dimensions(), (1920, 1080));

    let (outputs, modes) = b.randr_outputs_and_modes();
    let mut state = ServerState::with_randr_outputs_and_modes(
        1920,
        1080,
        outputs,
        modes,
        yserver_core::server::BackendCapabilities::from_backend(&b),
    );
    let rescan = crate::kms::render::platform::RescanResult {
        added_keys: Vec::new(),
        dropped_keys: vec![departing],
        dropped_old_indices: vec![1],
        dropped_layouts: Vec::new(),
        added_count: 0,
        connected: Vec::new(),
    };
    assert!(b.fire_randr_changes(&mut state, rescan, &[a], true, true, false));

    assert_eq!(
        b.root_storage_extent().map(|e| (e.width, e.height)),
        Some((3840, 1080)),
        "a shrink must not reallocate root storage",
    );
}

/// The newly covered region carries the ROOT BACKGROUND, not recycled GPU
/// content: the shared helper fills the whole reallocated root storage
/// with `core.bg_pixel`. Needs real Vk -- the headless fixture cannot
/// allocate storage, so it stubs a null view and never fills.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn a_grown_root_storage_is_filled_with_the_root_background() {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    // A background nothing else in the fixture paints, so a wrong or
    // missing fill cannot pass by accident.
    b.core.bg_pixel = Some(0x00ff_0000);

    let old_w = b.platform.fb_w;
    let new_w = old_w.saturating_add(1280);
    b.apply_virtual_screen_extent(new_w, b.platform.fb_h)
        .expect("growing the virtual extent must not fail");
    b.engine_close_open_frame_for_timeout_for_tests()
        .expect("close open frame");
    b.engine_drain_all_for_tests();

    let root_id = b
        .store
        .lookup(b.core.window_id)
        .expect("root must be live after the grow");
    let bytes = b
        .engine
        .get_image(
            &mut b.store,
            &mut b.platform,
            crate::kms::render::target::Src::server_internal(root_id),
            ash::vk::Rect2D {
                offset: ash::vk::Offset2D {
                    x: i32::from(old_w) + 4,
                    y: 4,
                },
                extent: ash::vk::Extent2D {
                    width: 1,
                    height: 1,
                },
            },
            32,
        )
        .expect("readback of the newly covered region");
    assert_eq!(
        (bytes[0], bytes[1], bytes[2], bytes[3]),
        (0x00, 0x00, 0xff, 0xff),
        "the newly covered region must hold the opaque root background \
             (B8G8R8A8), not recycled content",
    );
}

/// A root background PIXMAP survives the reallocation: the newly covered
/// region is tiled from the root origin, not left in the pixel fill
/// (Xorg `SetRootClip` exposes the whole resized root, `miPaintWindow`
/// tiles it). Measured in the vng scenario `root-bg-resize`.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn a_resized_root_keeps_its_background_pixmap() {
    use yserver_core::backend::Backend;

    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    // A 4x4 tile: green, with a blue top-left pixel marking the phase.
    let tile = b.create_pixmap(None, 32, 4, 4).expect("tile pixmap");
    b.fill_rectangle(None, tile.as_raw(), 0xFF00_FF00, 0, 0, 4, 4)
        .expect("tile fill green");
    b.fill_rectangle(None, tile.as_raw(), 0xFF00_00FF, 0, 0, 1, 1)
        .expect("tile phase pixel blue");
    b.set_container_background_pixmap(None, tile.as_raw())
        .expect("set root bg pixmap");

    let old_w = b.platform.fb_w;
    let new_w = old_w.saturating_add(1280);
    b.apply_virtual_screen_extent(new_w, b.platform.fb_h)
        .expect("growing the virtual extent must not fail");
    b.engine_close_open_frame_for_timeout_for_tests()
        .expect("close open frame");
    b.engine_drain_all_for_tests();

    let root_id = b
        .store
        .lookup(b.core.window_id)
        .expect("root must be live after the grow");
    // A tile-aligned 2x1 read in the newly covered region: the phase
    // pixel, then plain tile.
    let x = (i32::from(old_w) + 4) & !3;
    let bytes = b
        .engine
        .get_image(
            &mut b.store,
            &mut b.platform,
            crate::kms::render::target::Src::server_internal(root_id),
            ash::vk::Rect2D {
                offset: ash::vk::Offset2D { x, y: 4 },
                extent: ash::vk::Extent2D {
                    width: 2,
                    height: 1,
                },
            },
            32,
        )
        .expect("readback of the newly covered region");
    assert_eq!(
        (&bytes[0..3], &bytes[4..7]),
        (&[0xff, 0x00, 0x00][..], &[0x00, 0xff, 0x00][..]),
        "the resized root must be tiled with its background pixmap from \
             the root origin (B8G8R8A8)",
    );
}

/// `set_logical_screen_size` updates `fb_w`/`fb_h`, reallocates root
/// storage to the new dimensions, and leaves the scene dirty without
/// calling `drain_all` or `rebuild_outputs` (which would clear
/// `pending_acks` and expose a kernel-level EBUSY if a flip was in
/// flight).  On the no-Vk test fixture, the root storage is a null-view
/// stub; the test verifies the observable contract:
///
/// - `platform.fb_w` / `fb_h` carry the new size.
/// - Root xid is still live in the store with the new extent.
/// - The scene is marked dirty (next tick will repaint).
/// - A second call to the same dimensions is idempotent.
#[test]
fn set_logical_screen_size_updates_fb_and_root_storage() {
    use yserver_core::backend::Backend;

    let mut b = KmsBackend::for_tests();
    let initial_w = b.platform.fb_w;
    let initial_h = b.platform.fb_h;
    let initial_epoch = b.crtc_config_topology_epoch;

    // Sanity: root storage allocated at boot dimensions.
    let root_xid = b.core.window_id;
    let id_before = b
        .store
        .lookup(root_xid)
        .expect("root must be live before resize");
    let extent_before = b.store.get(id_before).unwrap().storage.extent;
    assert_eq!(extent_before.width, u32::from(initial_w));
    assert_eq!(extent_before.height, u32::from(initial_h));

    // Perform resize.
    let new_w: u16 = initial_w.saturating_add(1280);
    let new_h: u16 = initial_h.saturating_add(0); // height unchanged
    b.set_logical_screen_size(new_w, new_h)
        .expect("set_logical_screen_size must not fail on test fixture");

    // Platform extent updated.
    assert_eq!(b.platform.fb_w, new_w, "fb_w must reflect new width");
    assert_eq!(b.platform.fb_h, new_h, "fb_h must reflect new height");
    assert_eq!(
        b.crtc_config_topology_epoch,
        initial_epoch.wrapping_add(1),
        "a changed logical extent invalidates pending topology snapshots",
    );

    // Root xid is live with the new extent.
    let id_after = b
        .store
        .lookup(root_xid)
        .expect("root must still be live after resize");
    let extent_after = b.store.get(id_after).unwrap().storage.extent;
    assert_eq!(
        extent_after.width,
        u32::from(new_w),
        "root extent.width must match new_w"
    );
    assert_eq!(
        extent_after.height,
        u32::from(new_h),
        "root extent.height must match new_h"
    );

    // DrawableId changes (new allocation) — old storage freed/parked.
    assert_ne!(
        id_before, id_after,
        "resize must produce a fresh DrawableId for root"
    );

    // Scene is dirty — next tick will compose with new dimensions.
    assert!(
        b.scene.scene_structure_dirty,
        "scene must be marked dirty after resize so next tick repaints"
    );

    // Idempotent: same dimensions again must succeed without panic.
    b.set_logical_screen_size(new_w, new_h)
        .expect("repeat call to same dimensions must succeed");
    assert_eq!(b.platform.fb_w, new_w);
    assert_eq!(b.platform.fb_h, new_h);
    assert_eq!(
        b.crtc_config_topology_epoch,
        initial_epoch.wrapping_add(1),
        "an idempotent logical-size request must not invalidate probes",
    );
}
