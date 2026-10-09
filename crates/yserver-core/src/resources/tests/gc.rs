use super::*;

fn empty_create_gc_request(gc: ResourceId, drawable: ResourceId) -> CreateGcRequest {
    CreateGcRequest {
        gc,
        drawable,
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
    }
}

fn empty_change_gc(gc: ResourceId) -> GcChange {
    GcChange {
        gc,
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
        clip_mask: None,
        clip_x_origin: None,
        clip_y_origin: None,
        dash_offset: None,
        dashes: None,
        arc_mode: None,
    }
}

fn install_pixmap_with_host_xid(table: &mut ResourceTable, id: u32, host_xid: u32) {
    table.create_pixmap(
        ClientId(1),
        CreatePixmapRequest {
            depth: 24,
            pixmap: ResourceId(id),
            drawable: ROOT_WINDOW,
            width: 16,
            height: 16,
        },
    );
    assert!(table.set_pixmap_host_xid(
        ResourceId(id),
        crate::backend::PixmapHandle::from_raw_for_test(host_xid),
    ));
}

#[test]
fn change_gc_clip_mask_none_clears_clip_rectangles() {
    let mut t = ResourceTable::new();
    t.create_gc(
        ClientId(1),
        empty_create_gc_request(ResourceId(0x500), ROOT_WINDOW),
    );
    t.set_clip_rectangles(
        yserver_protocol::x11::ClientId(1),
        SetClipRectanglesRequest {
            gc: ResourceId(0x500),
            clip: ClipRectangles {
                ordering: 0,
                x_origin: 0,
                y_origin: 0,
                rectangles: vec![0, 0, 0, 0, 10, 0, 10, 0],
            },
        },
    );
    assert!(t.gc_clip_rectangles(ResourceId(0x500)).is_some());

    let mut clear = empty_change_gc(ResourceId(0x500));
    clear.clip_mask = Some(None);
    t.change_gc(yserver_protocol::x11::ClientId(1), clear);

    assert!(t.gc_clip_rectangles(ResourceId(0x500)).is_none());
}

fn install_dummy_gc(table: &mut ResourceTable, id: u32) {
    table.create_gc(
        ClientId(1),
        empty_create_gc_request(ResourceId(id), ROOT_WINDOW),
    );
}

fn install_font_with_host_xid(table: &mut ResourceTable, id: u32, host_xid: u32) {
    table.install_font(
        ClientId(1),
        ResourceId(id),
        "fixed".to_string(),
        crate::backend::FontHandle::from_raw_for_test(host_xid),
        FontMetrics::default(),
    );
}

// ---- Phase 6.2 Step 3: copy_gc unit tests for the new fields. ----

#[test]
fn copy_gc_function_and_plane_mask() {
    let mut t = ResourceTable::new();
    install_dummy_gc(&mut t, 0x500);
    install_dummy_gc(&mut t, 0x501);
    // Mutate src.
    let mut chg = empty_change_gc(ResourceId(0x500));
    chg.function = Some(GcFunction::Xor.protocol_value());
    chg.plane_mask = Some(0x00ff_00ff);
    t.change_gc(yserver_protocol::x11::ClientId(1), chg);
    // Copy GCFunction (1<<0) + GCPlaneMask (1<<1).
    t.copy_gc(ResourceId(0x500), ResourceId(0x501), 0x0000_0003);
    let dst = t.gc(ResourceId(0x501)).unwrap();
    assert_eq!(dst.function, GcFunction::Xor);
    assert_eq!(dst.plane_mask, 0x00ff_00ff);
}

#[test]
fn copy_gc_line_join_cap_styles() {
    let mut t = ResourceTable::new();
    install_dummy_gc(&mut t, 0x500);
    install_dummy_gc(&mut t, 0x501);
    let mut chg = empty_change_gc(ResourceId(0x500));
    chg.line_style = Some(LineStyle::OnOffDash.protocol_value());
    chg.cap_style = Some(CapStyle::Round.protocol_value());
    chg.join_style = Some(JoinStyle::Bevel.protocol_value());
    t.change_gc(yserver_protocol::x11::ClientId(1), chg);
    // line_style (1<<5) | cap_style (1<<6) | join_style (1<<7) = 0xE0
    t.copy_gc(ResourceId(0x500), ResourceId(0x501), 0x0000_00E0);
    let dst = t.gc(ResourceId(0x501)).unwrap();
    assert_eq!(dst.line_style, LineStyle::OnOffDash);
    assert_eq!(dst.cap_style, CapStyle::Round);
    assert_eq!(dst.join_style, JoinStyle::Bevel);
}

#[test]
fn copy_gc_fill_rule_and_subwindow_mode() {
    let mut t = ResourceTable::new();
    install_dummy_gc(&mut t, 0x500);
    install_dummy_gc(&mut t, 0x501);
    let mut chg = empty_change_gc(ResourceId(0x500));
    chg.fill_rule = Some(FillRule::Winding.protocol_value());
    chg.subwindow_mode = Some(SubwindowMode::IncludeInferiors.protocol_value());
    t.change_gc(yserver_protocol::x11::ClientId(1), chg);
    // fill_rule (1<<9) | subwindow_mode (1<<15) = 0x8200
    t.copy_gc(ResourceId(0x500), ResourceId(0x501), 0x0000_8200);
    let dst = t.gc(ResourceId(0x501)).unwrap();
    assert_eq!(dst.fill_rule, FillRule::Winding);
    assert_eq!(dst.subwindow_mode, SubwindowMode::IncludeInferiors);
}

#[test]
fn copy_gc_graphics_exposures_and_dash_offset() {
    let mut t = ResourceTable::new();
    install_dummy_gc(&mut t, 0x500);
    install_dummy_gc(&mut t, 0x501);
    let mut chg = empty_change_gc(ResourceId(0x500));
    chg.graphics_exposures = Some(false);
    chg.dash_offset = Some(7);
    t.change_gc(yserver_protocol::x11::ClientId(1), chg);
    // graphics_exposures (1<<16) | dash_offset (1<<20) = 0x0011_0000
    t.copy_gc(ResourceId(0x500), ResourceId(0x501), 0x0011_0000);
    let dst = t.gc(ResourceId(0x501)).unwrap();
    assert!(!dst.graphics_exposures);
    assert_eq!(dst.dash_offset, 7);
}

#[test]
fn copy_gc_dashes_and_arc_mode() {
    let mut t = ResourceTable::new();
    install_dummy_gc(&mut t, 0x500);
    install_dummy_gc(&mut t, 0x501);
    let mut chg = empty_change_gc(ResourceId(0x500));
    chg.dashes = Some(9);
    chg.arc_mode = Some(ArcMode::Chord.protocol_value());
    t.change_gc(yserver_protocol::x11::ClientId(1), chg);
    // dashes (1<<21) | arc_mode (1<<22) = 0x00600000
    t.copy_gc(ResourceId(0x500), ResourceId(0x501), 0x0060_0000);
    let dst = t.gc(ResourceId(0x501)).unwrap();
    assert_eq!(dst.dashes, vec![9, 9]);
    assert_eq!(dst.arc_mode, ArcMode::Chord);
}

#[test]
fn copy_gc_zero_mask_copies_nothing() {
    let mut t = ResourceTable::new();
    install_dummy_gc(&mut t, 0x500);
    install_dummy_gc(&mut t, 0x501);
    let mut chg = empty_change_gc(ResourceId(0x500));
    chg.function = Some(GcFunction::Xor.protocol_value());
    t.change_gc(yserver_protocol::x11::ClientId(1), chg);
    t.copy_gc(ResourceId(0x500), ResourceId(0x501), 0);
    let dst = t.gc(ResourceId(0x501)).unwrap();
    assert_eq!(dst.function, GcFunction::Copy);
}

// ---- Phase 6.2 Step 3: resolve_draw_state unit tests. ----

#[test]
fn resolve_draw_state_default_gc() {
    let mut t = ResourceTable::new();
    install_dummy_gc(&mut t, 0x500);
    let state = t.resolve_draw_state(ResourceId(0x500)).expect("known gc");
    // A fresh GC should match the DrawState::default() for the
    // attribute-only fields.
    let d = DrawState::default();
    assert_eq!(state.foreground, d.foreground);
    assert_eq!(state.background, d.background);
    assert_eq!(state.line_width, d.line_width);
    assert_eq!(state.line_style, d.line_style);
    assert_eq!(state.cap_style, d.cap_style);
    assert_eq!(state.join_style, d.join_style);
    assert_eq!(state.fill_style, d.fill_style);
    assert_eq!(state.fill_rule, d.fill_rule);
    assert_eq!(state.function, d.function);
    assert_eq!(state.plane_mask, d.plane_mask);
    assert_eq!(state.font, None);
    assert_eq!(state.clip, ClipState::None);
    assert_eq!(state.fill, FillState::Solid);
    assert_eq!(state.subwindow_mode, d.subwindow_mode);
    assert!(state.graphics_exposures);
    assert_eq!(state.dashes, d.dashes);
    assert_eq!(state.dash_offset, d.dash_offset);
    assert_eq!(state.arc_mode, d.arc_mode);
}

#[test]
fn resolve_draw_state_tiled_fill_resolves_pixmap_handle() {
    let mut t = ResourceTable::new();
    install_dummy_gc(&mut t, 0x500);
    install_pixmap_with_host_xid(&mut t, 0x600, 0x12345);
    let mut chg = empty_change_gc(ResourceId(0x500));
    chg.fill_style = Some(FillStyle::Tiled.protocol_value());
    chg.tile = Some(ResourceId(0x600));
    chg.tile_x_origin = Some(3);
    chg.tile_y_origin = Some(5);
    t.change_gc(yserver_protocol::x11::ClientId(1), chg);
    let state = t.resolve_draw_state(ResourceId(0x500)).unwrap();
    match state.fill {
        FillState::Tiled { pixmap, origin } => {
            assert_eq!(pixmap.as_raw(), 0x12345);
            assert_eq!(origin, (3, 5));
        }
        other => panic!("expected Tiled, got {other:?}"),
    }
}

#[test]
fn resolve_draw_state_stippled_fill_resolves_pixmap_handle() {
    let mut t = ResourceTable::new();
    install_dummy_gc(&mut t, 0x500);
    install_pixmap_with_host_xid(&mut t, 0x600, 0xabcde);
    let mut chg = empty_change_gc(ResourceId(0x500));
    chg.fill_style = Some(FillStyle::Stippled.protocol_value());
    chg.stipple = Some(ResourceId(0x600));
    t.change_gc(yserver_protocol::x11::ClientId(1), chg);
    let state = t.resolve_draw_state(ResourceId(0x500)).unwrap();
    match state.fill {
        FillState::Stippled { pixmap, origin } => {
            assert_eq!(pixmap.as_raw(), 0xabcde);
            assert_eq!(origin, (0, 0));
        }
        other => panic!("expected Stippled, got {other:?}"),
    }
}

#[test]
fn resolve_draw_state_clip_rectangles_with_origin() {
    let mut t = ResourceTable::new();
    install_dummy_gc(&mut t, 0x500);
    let rect_bytes = vec![0u8, 0, 0, 0, 10, 0, 10, 0];
    t.set_clip_rectangles(
        yserver_protocol::x11::ClientId(1),
        SetClipRectanglesRequest {
            gc: ResourceId(0x500),
            clip: ClipRectangles {
                ordering: 0,
                x_origin: 4,
                y_origin: 7,
                rectangles: rect_bytes.clone(),
            },
        },
    );
    let mut chg = empty_change_gc(ResourceId(0x500));
    chg.clip_x_origin = Some(11);
    chg.clip_y_origin = Some(13);
    t.change_gc(yserver_protocol::x11::ClientId(1), chg);
    let state = t.resolve_draw_state(ResourceId(0x500)).unwrap();
    match state.clip {
        ClipState::Rectangles { origin, rects } => {
            // origin comes from the GC's clip-x/y-origin (set above),
            // not from the SetClipRectangles x/y_origin (which lives
            // inside the rectangles payload itself).
            assert_eq!(origin, (11, 13));
            assert_eq!(rects.x_origin, 4);
            assert_eq!(rects.y_origin, 7);
            assert_eq!(rects.rectangles, rect_bytes);
        }
        other => panic!("expected Rectangles, got {other:?}"),
    }
}

/// SetClipRectangles' origin is the GC's clip origin: Xorg's
/// `SetClipRects` stores it in `clipOrg` (`dix/gc.c:1021-1024`), and
/// tools/vng-scenarios/draw-clip-probe.c measures a 40x40 clip at
/// origin (20,50) filling C's x 20-59, y 50-89. CopyGC carries it.
#[test]
fn set_clip_rectangles_origin_is_the_gc_clip_origin() {
    let mut t = ResourceTable::new();
    install_dummy_gc(&mut t, 0x500);
    install_dummy_gc(&mut t, 0x501);
    t.set_clip_rectangles(
        yserver_protocol::x11::ClientId(1),
        SetClipRectanglesRequest {
            gc: ResourceId(0x500),
            clip: ClipRectangles {
                ordering: 0,
                x_origin: 20,
                y_origin: 50,
                rectangles: vec![0, 0, 0, 0, 40, 0, 40, 0],
            },
        },
    );
    let origin = |t: &ResourceTable, gc| match t.resolve_draw_state(ResourceId(gc)).unwrap().clip {
        ClipState::Rectangles { origin, .. } => origin,
        other => panic!("expected Rectangles, got {other:?}"),
    };
    assert_eq!(origin(&t, 0x500), (20, 50));
    t.copy_gc(
        ResourceId(0x500),
        ResourceId(0x501),
        0x0002_0000 | 0x0004_0000 | 0x0008_0000,
    );
    assert_eq!(origin(&t, 0x501), (20, 50));
    t.set_gc_clip_origin(ResourceId(0x500), 30, 10);
    assert_eq!(origin(&t, 0x500), (30, 10));
}

#[test]
fn resolve_draw_state_pixmap_clip_with_origin() {
    let mut t = ResourceTable::new();
    install_dummy_gc(&mut t, 0x500);
    install_pixmap_with_host_xid(&mut t, 0x600, 0xdead);
    let mut chg = empty_change_gc(ResourceId(0x500));
    chg.clip_mask = Some(Some(ResourceId(0x600)));
    chg.clip_x_origin = Some(2);
    chg.clip_y_origin = Some(3);
    t.change_gc(yserver_protocol::x11::ClientId(1), chg);
    let state = t.resolve_draw_state(ResourceId(0x500)).unwrap();
    match state.clip {
        ClipState::Pixmap { origin, pixmap } => {
            assert_eq!(origin, (2, 3));
            assert_eq!(pixmap.as_raw(), 0xdead);
        }
        other => panic!("expected Pixmap, got {other:?}"),
    }
}

#[test]
fn resolve_draw_state_unknown_gc_returns_none() {
    let t = ResourceTable::new();
    assert!(t.resolve_draw_state(ResourceId(0x999)).is_none());
}

#[test]
fn resolve_draw_state_tiled_with_freed_tile_pixmap_preserves_fill() {
    let mut t = ResourceTable::new();
    install_dummy_gc(&mut t, 0x500);
    install_pixmap_with_host_xid(&mut t, 0x600, 0xbeef);
    let mut chg = empty_change_gc(ResourceId(0x500));
    chg.fill_style = Some(FillStyle::Tiled.protocol_value());
    chg.tile = Some(ResourceId(0x600));
    t.change_gc(yserver_protocol::x11::ClientId(1), chg);
    // Freeing the source pixmap must not clear the GC's retained tile.
    let _ = t.free_pixmap(ResourceId(0x600));
    let state = t.resolve_draw_state(ResourceId(0x500)).unwrap();
    assert_eq!(
        state.fill,
        FillState::Tiled {
            pixmap: crate::backend::PixmapHandle::from_raw(0xbeef).unwrap(),
            origin: (0, 0),
        }
    );
}

#[test]
fn resolve_draw_state_stippled_with_freed_pixmap_preserves_fill() {
    let mut t = ResourceTable::new();
    install_dummy_gc(&mut t, 0x500);
    install_pixmap_with_host_xid(&mut t, 0x600, 0xabcd);
    let mut chg = empty_change_gc(ResourceId(0x500));
    chg.fill_style = Some(FillStyle::Stippled.protocol_value());
    chg.stipple = Some(ResourceId(0x600));
    chg.tile_x_origin = Some(9);
    chg.tile_y_origin = Some(17);
    t.change_gc(yserver_protocol::x11::ClientId(1), chg);
    let _ = t.free_pixmap(ResourceId(0x600));
    let state = t.resolve_draw_state(ResourceId(0x500)).unwrap();
    assert_eq!(
        state.fill,
        FillState::Stippled {
            pixmap: crate::backend::PixmapHandle::from_raw(0xabcd).unwrap(),
            origin: (9, 17),
        }
    );
}

#[test]
fn resolve_draw_state_clip_pixmap_freed_preserves_clip() {
    let mut t = ResourceTable::new();
    install_dummy_gc(&mut t, 0x500);
    install_pixmap_with_host_xid(&mut t, 0x600, 0xcafe);
    let mut chg = empty_change_gc(ResourceId(0x500));
    chg.clip_mask = Some(Some(ResourceId(0x600)));
    t.change_gc(yserver_protocol::x11::ClientId(1), chg);
    let _ = t.free_pixmap(ResourceId(0x600));
    let state = t.resolve_draw_state(ResourceId(0x500)).unwrap();
    assert_eq!(
        state.clip,
        ClipState::Pixmap {
            origin: (0, 0),
            pixmap: crate::backend::PixmapHandle::from_raw(0xcafe).unwrap(),
        }
    );
}

#[test]
fn host_xid_referenced_by_gc_reports_retained_clip_tile_and_stipple() {
    let mut t = ResourceTable::new();
    install_dummy_gc(&mut t, 0x500);
    install_pixmap_with_host_xid(&mut t, 0x601, 0xaaa1);
    install_pixmap_with_host_xid(&mut t, 0x602, 0xaaa2);
    install_pixmap_with_host_xid(&mut t, 0x603, 0xaaa3);

    let mut chg = empty_change_gc(ResourceId(0x500));
    chg.clip_mask = Some(Some(ResourceId(0x601)));
    chg.tile = Some(ResourceId(0x602));
    chg.stipple = Some(ResourceId(0x603));
    t.change_gc(yserver_protocol::x11::ClientId(1), chg);

    let _ = t.free_pixmap(ResourceId(0x601));
    let _ = t.free_pixmap(ResourceId(0x602));
    let _ = t.free_pixmap(ResourceId(0x603));

    assert!(t.host_xid_referenced_by_gc(crate::backend::PixmapHandle::from_raw(0xaaa1).unwrap()));
    assert!(t.host_xid_referenced_by_gc(crate::backend::PixmapHandle::from_raw(0xaaa2).unwrap()));
    assert!(t.host_xid_referenced_by_gc(crate::backend::PixmapHandle::from_raw(0xaaa3).unwrap()));
}

#[test]
fn resolve_draw_state_font_resolves_handle() {
    let mut t = ResourceTable::new();
    install_dummy_gc(&mut t, 0x500);
    install_font_with_host_xid(&mut t, 0x700, 0x4242);
    let mut chg = empty_change_gc(ResourceId(0x500));
    chg.font = Some(ResourceId(0x700));
    t.change_gc(yserver_protocol::x11::ClientId(1), chg);
    let state = t.resolve_draw_state(ResourceId(0x500)).unwrap();
    let f = state.font.expect("font handle");
    assert_eq!(f.as_raw(), 0x4242);
}
