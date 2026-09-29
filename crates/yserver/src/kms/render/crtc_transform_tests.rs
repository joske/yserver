//! RANDR CRTC transforms on the KMS backend (spec D4–D6): intermediate
//! lifecycle, the scale pass, cursor, direct scanout, input and root reads.
//!
//! The pixel tests run on lavapipe, which cannot allocate scanout BOs; they
//! compose through `SceneCompositor::compose_transformed_for_tests`, the
//! production record path with an offscreen image standing in for the BO.

use ash::vk;
use yserver_core::{
    backend::Backend,
    randr::{CrtcTransform, FIXED_ONE, Filter, RandrOutput},
    server::ServerState,
};

use super::{KmsBackend, tests::push_test_output};
use crate::kms::render::scene::{CursorAssignment, CursorEntry, SceneCompositor};

/// The mode of both test outputs: small enough for lavapipe, one pattern
/// byte per root column.
const MODE: (u16, u16) = (64, 48);

fn scale(word: i32, filter: Option<Filter>) -> CrtcTransform {
    CrtcTransform::new(
        [word, 0, 0, 0, word, 0, 0, 0, FIXED_ONE],
        filter,
        Vec::new(),
    )
    .unwrap()
}

/// Root pixel `(x, y)` of the test pattern, BGRA; unique per pixel while the
/// root stays under 256×256.
fn pattern(x: u32, y: u32) -> [u8; 4] {
    #[allow(clippy::cast_possible_truncation)]
    [x as u8, y as u8, (x * 7 + y * 13) as u8, 0xff]
}

/// Identity at 0,0 and `transform` on the right-hand output at 64,0 (the
/// Cinnamon layout, scaled down), over `root`, painted with [`pattern`].
fn transformed_pair(transform: CrtcTransform, root: (u16, u16)) -> Option<KmsBackend> {
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return None;
        }
    };
    push_test_output(&mut b, 2);
    for (i, output) in b.platform.outputs.iter_mut().enumerate() {
        output.x = i32::from(MODE.0) * i32::try_from(i).unwrap();
        (output.width, output.height) = MODE;
    }
    b.platform.scanout_pools.push(None);
    b.platform.bo_generations.push(Vec::new());
    b.platform.first_pageflip_logged.push(false);
    b.set_logical_screen_size(root.0, root.1)
        .expect("root size");
    let key = b.platform.outputs[1].key.clone();
    b.platform.output_transforms.insert(key, transform);
    b.scene = SceneCompositor::new(&b.platform).expect("live scene");
    let (w, h) = (u32::from(root.0), u32::from(root.1));
    let bytes: Vec<u8> = (0..h)
        .flat_map(|y| (0..w).flat_map(move |x| pattern(x, y)))
        .collect();
    let root_xid = b.core.window_id;
    b.put_image(None, root_xid, 24, root.0, root.1, 0, 0, &bytes)
        .expect("pattern");
    Some(b)
}

/// Flush the paint, then compose output 1 through its transform.
fn compose_right(b: &mut KmsBackend) -> Vec<u8> {
    use crate::kms::render::{frame_builder::CloseReason, submit_group::FlushReason};
    b.engine
        .close_open_frame(&mut b.store, &mut b.platform, CloseReason::LegacyScCompose)
        .expect("close frame");
    b.engine
        .flush_submit_group(&mut b.store, &mut b.platform, FlushReason::SceneCompose)
        .expect("flush");
    b.scene
        .compose_transformed_for_tests(&b.core, &mut b.store, &b.windows, &b.platform, 1)
}

fn scanout_px(bytes: &[u8], x: u32, y: u32) -> [u8; 4] {
    let i = ((y * u32::from(MODE.0) + x) * 4) as usize;
    bytes[i..i + 4].try_into().unwrap()
}

/// pixman's nearest sample for pure scale `m` (16.16): the destination
/// centre `d + ½` through the matrix, less `pixman_fixed_e`, truncated.
fn pixman_nearest(m: i32, d: u32) -> i64 {
    let p = (i64::from(d) << 16) + 0x8000;
    let v = i64::from(m) * (p >> 16) + ((i64::from(m) * (p & 0xffff) + 0x8000) >> 16);
    (v - 1) >> 16
}

/// Bilinear reference: texel centres at `i + ½`, clamp-to-edge inside the
/// footprint, transparent black outside the root (spec D4).
fn bilinear_reference(m: i32, footprint: (u32, u32), root: (u32, u32), d: (u32, u32)) -> [f64; 3] {
    let s = f64::from(m) / 65536.0;
    let texel = |tx: i64, ty: i64| -> [f64; 3] {
        let tx = tx.clamp(0, i64::from(footprint.0) - 1);
        let ty = ty.clamp(0, i64::from(footprint.1) - 1);
        let (rx, ry) = (tx + i64::from(MODE.0), ty);
        if rx >= i64::from(root.0) || ry >= i64::from(root.1) {
            return [0.0; 3];
        }
        let p = pattern(u32::try_from(rx).unwrap(), u32::try_from(ry).unwrap());
        [f64::from(p[0]), f64::from(p[1]), f64::from(p[2])]
    };
    let u = (f64::from(d.0) + 0.5) * s - 0.5;
    let v = (f64::from(d.1) + 0.5) * s - 0.5;
    #[allow(clippy::cast_possible_truncation)]
    let (x0, y0) = (u.floor() as i64, v.floor() as i64);
    let (fx, fy) = (u - u.floor(), v - v.floor());
    let mut out = [0.0; 3];
    for (c, o) in out.iter_mut().enumerate() {
        let top = texel(x0, y0)[c] * (1.0 - fx) + texel(x0 + 1, y0)[c] * fx;
        let bottom = texel(x0, y0 + 1)[c] * (1.0 - fx) + texel(x0 + 1, y0 + 1)[c] * fx;
        *o = top * (1.0 - fy) + bottom * fy;
    }
    out
}

fn assert_nearest_exact(m: i32, root: (u16, u16)) {
    let Some(mut b) = transformed_pair(scale(m, Some(Filter::Nearest)), root) else {
        return;
    };
    let out = compose_right(&mut b);
    let (fw, fh) = b
        .platform
        .output_transform(1)
        .unwrap()
        .footprint(MODE.0, MODE.1);
    for dy in 0..u32::from(MODE.1) {
        for dx in 0..u32::from(MODE.0) {
            let ix = pixman_nearest(m, dx).clamp(0, i64::from(fw) - 1);
            let iy = pixman_nearest(m, dy).clamp(0, i64::from(fh) - 1);
            let (rx, ry) = (ix + i64::from(MODE.0), iy);
            let want = if rx < i64::from(root.0) && ry < i64::from(root.1) {
                pattern(u32::try_from(rx).unwrap(), u32::try_from(ry).unwrap())
            } else {
                [0; 4]
            };
            assert_eq!(
                scanout_px(&out, dx, dy)[..3],
                want[..3],
                "m={m:#x} scanout ({dx},{dy}) should show root ({rx},{ry})"
            );
        }
    }
}

fn assert_bilinear_close(m: i32, root: (u16, u16)) {
    let Some(mut b) = transformed_pair(scale(m, Some(Filter::Bilinear)), root) else {
        return;
    };
    let out = compose_right(&mut b);
    let (fw, fh) = b
        .platform
        .output_transform(1)
        .unwrap()
        .footprint(MODE.0, MODE.1);
    let root = (u32::from(root.0), u32::from(root.1));
    for dy in 0..u32::from(MODE.1) {
        for dx in 0..u32::from(MODE.0) {
            let want = bilinear_reference(m, (u32::from(fw), u32::from(fh)), root, (dx, dy));
            let got = scanout_px(&out, dx, dy);
            for c in 0..3 {
                assert!(
                    (f64::from(got[c]) - want[c]).abs() <= 2.0,
                    "m={m:#x} scanout ({dx},{dy}) channel {c}: got {} want {:.1}",
                    got[c],
                    want[c]
                );
            }
        }
    }
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn nearest_2x_and_half_scale_match_pixman_exactly() {
    // Scale-down 100%'s 2.0 and scale-up's 0.5 `nearest` (spec, muffin table).
    assert_nearest_exact(0x20000, (64 + 128, 96));
    assert_nearest_exact(0x8000, (64 + 32, 48));
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn bilinear_1_6_and_0_8_match_within_tolerance() {
    // xrandr's 1.599991 and 0.799988 (spec, Xorg table): footprints 103×77
    // and 52×39 of a 64×48 mode.
    assert_bilinear_close(0x19999, (64 + 103, 77));
    assert_bilinear_close(0xcccc, (64 + 52, 48));
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn a_root_smaller_than_the_footprint_leaves_the_crop_black() {
    // Footprint 128×96 at 64,0; the root stops at 164×80.
    assert_nearest_exact(0x20000, (164, 80));
    // Bilinear blends toward that black at the root edge.
    assert_bilinear_close(0x19999, (64 + 82, 60));
    let Some(mut b) = transformed_pair(scale(0x19999, Some(Filter::Bilinear)), (64 + 82, 60))
    else {
        return;
    };
    let out = compose_right(&mut b);
    // Mode pixel 51 samples intermediate columns 81 | 82 at 0.1 | 0.9: a
    // tenth root, the rest crop, so darker than its neighbour, not black.
    let edge = scanout_px(&out, 51, 10);
    let inside = scanout_px(&out, 50, 10);
    assert!(edge[2] > 0 && edge[2] < inside[2], "{edge:?} vs {inside:?}");
    assert_eq!(scanout_px(&out, 60, 10)[..3], [0, 0, 0]);
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn the_intermediate_follows_the_transform_lifecycle() {
    use crate::kms::vk::mem_accounting::{MemCategory, category_of};
    let Some(mut b) = transformed_pair(scale(0x20000, None), (192, 96)) else {
        return;
    };
    assert!(
        b.scene.intermediate_for_tests(0).is_none(),
        "identity pays nothing"
    );
    let (memory, extent) = b.scene.intermediate_for_tests(1).expect("allocated");
    assert_eq!((extent.width, extent.height), (128, 96), "footprint-sized");
    assert_eq!(category_of(memory), Some(MemCategory::Transform));

    let key = b.platform.outputs[1].key.clone();
    b.platform
        .output_transforms
        .insert(key.clone(), scale(0x8000, None));
    b.scene.sync_output_layouts(&b.platform).unwrap();
    let (resized, extent) = b.scene.intermediate_for_tests(1).expect("reallocated");
    assert_eq!(
        (extent.width, extent.height),
        (32, 24),
        "follows the footprint"
    );
    assert_eq!(category_of(resized), Some(MemCategory::Transform));

    b.platform.output_transforms.remove(&key);
    b.scene.sync_output_layouts(&b.platform).unwrap();
    assert!(
        b.scene.intermediate_for_tests(1).is_none(),
        "freed at identity"
    );
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn root_get_image_reads_the_transformed_output_in_root_space() {
    let root = (192u16, 96u16);
    let Some(mut b) = transformed_pair(scale(0x20000, Some(Filter::Nearest)), root) else {
        return;
    };
    let _ = compose_right(&mut b);
    // A request crossing both outputs splits at the CRTC edge; the right
    // piece reads the intermediate at `requested ∩ footprint − origin`.
    let request = vk::Rect2D {
        offset: vk::Offset2D::default(),
        extent: vk::Extent2D {
            width: 100,
            height: 70,
        },
    };
    let pieces = super::split_root_scanout_reads(request, 40, 8, 0, 0, &b.crtc_root_rects());
    assert_eq!(pieces.len(), 2, "{pieces:?}");
    let route = super::select_scanout_read_route(
        &b,
        pieces[1].read,
        super::ScanoutReadSelection::OnScreenOnly,
    )
    .expect("route");
    assert_eq!(
        route,
        super::ScanoutReadRoute::Intermediate {
            output_idx: 1,
            local: vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 8 },
                extent: vk::Extent2D {
                    width: 76,
                    height: 70,
                },
            },
        }
    );
    // Root space, not scaled scanout pixels. (The identity half has no
    // scanout BO on lavapipe and zero-fills.)
    let root_xid = b.core.window_id;
    let got = b
        .get_image_pixels_for_tests(root_xid, 2, 40, 8, 100, 70, !0)
        .expect("get_image")
        .expect("bytes");
    for y in 0..70u32 {
        for x in 24..100u32 {
            let i = ((y * 100 + x) * 4) as usize;
            assert_eq!(
                got[i..i + 3],
                pattern(40 + x, 8 + y)[..3],
                "root ({}, {})",
                40 + x,
                8 + y
            );
        }
    }
}

/// The opaque colour of the 4×4 test cursor.
const SPRITE: [u8; 3] = [0x11, 0x22, 0xee];

/// Register an opaque 4×4 [`SPRITE`] cursor with its hotspot at the top left.
fn register_test_cursor(b: &mut KmsBackend) {
    let sprite = b.create_pixmap(None, 32, 4, 4).expect("sprite").as_raw();
    b.put_image(
        None,
        sprite,
        32,
        4,
        4,
        0,
        0,
        &[0x11, 0x22, 0xee, 0xff].repeat(16),
    )
    .expect("sprite pixels");
    let id = b.store.lookup(sprite).expect("sprite drawable");
    b.scene.register_cursor(CursorEntry {
        id,
        extent: vk::Extent2D {
            width: 4,
            height: 4,
        },
        hot_x: 0,
        hot_y: 0,
        record_version: 1,
        bgra_bytes: None,
    });
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn the_cursor_is_software_on_every_output_and_scales_with_the_content() {
    let Some(mut b) = transformed_pair(scale(0x20000, Some(Filter::Nearest)), (192, 96)) else {
        return;
    };
    register_test_cursor(&mut b);
    for (output_idx, at) in [(0, (10.0, 10.0)), (1, (72.0, 8.0))] {
        (b.core.cursor_x, b.core.cursor_y) = at;
        let assignment = b.scene.cursor_assignment_for_tests(
            &b.core,
            &mut b.store,
            &b.windows,
            &b.platform,
            output_idx,
        );
        assert!(
            matches!(assignment, CursorAssignment::Sw { .. }),
            "output {output_idx}: {assignment:?}"
        );
    }
    // Drawn into the intermediate at root (72, 8), so scanout (4, 4) shows
    // it and scanout (6, 6) — root (76, 12) — does not.
    let out = compose_right(&mut b);
    assert_eq!(scanout_px(&out, 4, 4)[..3], SPRITE);
    assert_eq!(scanout_px(&out, 6, 6)[..3], pattern(76, 12)[..3]);
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn root_reads_never_see_the_software_cursor() {
    // Xorg's misprite takes a software cursor off the screen before any
    // window read that overlaps it (`miSpriteSourceValidate`).
    let Some(mut b) = transformed_pair(scale(0x20000, Some(Filter::Nearest)), (192, 96)) else {
        return;
    };
    register_test_cursor(&mut b);
    let root_xid = b.core.window_id;

    // Transformed: the sprite at root (72, 8) is on the scanout, scaled, but
    // a root GetImage over and around it returns the pattern.
    (b.core.cursor_x, b.core.cursor_y) = (72.0, 8.0);
    let out = compose_right(&mut b);
    assert_eq!(scanout_px(&out, 4, 4)[..3], SPRITE, "scanout keeps it");
    assert_eq!(scanout_px(&out, 5, 5)[..3], SPRITE);
    let got = b
        .get_image_pixels_for_tests(root_xid, 2, 70, 6, 8, 8, !0)
        .expect("get_image")
        .expect("bytes");
    for y in 0..8u32 {
        for x in 0..8u32 {
            let i = ((y * 8 + x) * 4) as usize;
            assert_eq!(
                got[i..i + 3],
                pattern(70 + x, 6 + y)[..3],
                "root ({}, {})",
                70 + x,
                6 + y
            );
        }
    }
    // Moved off: the next compose's save replaces the old one.
    (b.core.cursor_x, b.core.cursor_y) = (100.0, 40.0);
    let _ = compose_right(&mut b);
    let got = b
        .get_image_pixels_for_tests(root_xid, 2, 100, 40, 4, 4, !0)
        .expect("get_image")
        .expect("bytes");
    assert_eq!(got[..3], pattern(100, 40)[..3]);

    // Identity: the BO shows it at (10, 10); a read of the BO does not.
    (b.core.cursor_x, b.core.cursor_y) = (10.0, 10.0);
    let (bo, read) =
        b.scene
            .compose_identity_for_tests(&b.core, &mut b.store, &b.windows, &b.platform, 0);
    for y in 8..16u32 {
        for x in 8..16u32 {
            let under = (10..14).contains(&x) && (10..14).contains(&y);
            let want = if under {
                SPRITE
            } else {
                pattern(x, y)[..3].try_into().unwrap()
            };
            assert_eq!(scanout_px(&bo, x, y)[..3], want, "BO ({x}, {y})");
            assert_eq!(
                scanout_px(&read, x, y)[..3],
                pattern(x, y)[..3],
                "read ({x}, {y})"
            );
        }
    }
}

/// Two 2560×1440 outputs side by side with their RANDR rows, as muffin
/// configures them on silence (spec, "What muffin sends").
fn silence_pair(transforms: [CrtcTransform; 2], root: (u16, u16)) -> (KmsBackend, ServerState) {
    let mut b = KmsBackend::for_tests();
    push_test_output(&mut b, 2);
    for (i, output) in b.platform.outputs.iter_mut().enumerate() {
        output.x = 2560 * i32::try_from(i).unwrap();
        (output.width, output.height) = (2560, 1440);
    }
    let mut state = ServerState::new();
    state.randr.outputs = (0..2u32)
        .zip(transforms)
        .map(|(i, current_transform)| RandrOutput {
            name: format!("DP-{i}"),
            output_id: 10 + i,
            crtc_id: 20 + i,
            mode_id: 0x13,
            connected: true,
            x: 2560 * i16::try_from(i).unwrap(),
            y: 0,
            width: 2560,
            height: 1440,
            vrefresh: 60,
            timing: None,
            mm_width: 0,
            mm_height: 0,
            mode_ids: vec![0x13],
            num_preferred: 1,
            pending_transform: current_transform.clone(),
            current_transform,
        })
        .collect();
    (state.randr.screen_width, state.randr.screen_height) = root;
    for i in 0..2 {
        let key = b.platform.outputs[i].key.clone();
        b.output_key_by_id
            .insert(10 + u32::try_from(i).unwrap(), key);
    }
    (b, state)
}

/// A relative device move, as the input thread delivers it: already
/// accumulated into a root position.
fn motion(b: &mut KmsBackend, state: &mut ServerState, to: (i32, i32), delta: (i32, i32)) {
    b.on_host_input(
        state,
        yserver_core::core_loop::HostInputEvent::PointerMotion {
            x: to.0,
            y: to.1,
            time: 0,
            relative: true,
            dx: delta.0,
            dy: delta.1,
        },
    );
}

#[test]
fn a_layout_change_moves_a_pointer_in_the_scale_up_hole_to_the_nearest_crtc() {
    // Scale-up 125%: 0.5 `nearest` at 0,0 and 0.799988 `good` at 2560,0.
    let (mut b, mut state) = silence_pair(
        [
            scale(0x8000, Some(Filter::Nearest)),
            scale(0xcccc, Some(Filter::Bilinear)),
        ],
        (4608, 1152),
    );
    (b.core.cursor_x, b.core.cursor_y) = (1500.0, 300.0);
    Backend::randr_layout_changed(&mut b, &mut state);
    assert_eq!(
        (b.platform.fb_w, b.platform.fb_h),
        (4608, 1152),
        "the root extent"
    );
    assert_eq!(
        b.crtc_root_rects(),
        vec![(0, 0, 1280, 720), (2560, 0, 2048, 1152)],
        "footprints at the CRTC origins"
    );
    assert_eq!((b.core.cursor_x, b.core.cursor_y), (1279.0, 300.0));
    assert_eq!(state.pointer_root, (1279, 300), "delivered as a motion");
}

#[test]
fn moves_and_warps_leaving_every_crtc_clamp_to_the_one_they_left() {
    // Scale-down 100%: identity at 0,0, 2.0 at 2560,0; below the identity
    // output lies a hole of the 7680×2880 root.
    let (mut b, mut state) = silence_pair(
        [
            CrtcTransform::identity(),
            scale(0x20000, Some(Filter::Bilinear)),
        ],
        (7680, 2880),
    );
    Backend::randr_layout_changed(&mut b, &mut state);
    Backend::warp_pointer_root(&mut b, &mut state, 100, 100);
    Backend::warp_pointer_root(&mut b, &mut state, 100, 2000);
    assert_eq!(state.pointer_root, (100, 1439), "a warp into the hole");
    motion(&mut b, &mut state, (1000, 2000), (900, 561));
    assert_eq!(state.pointer_root, (1000, 1439), "a move into the hole");
    motion(&mut b, &mut state, (3000, 2000), (2000, 561));
    assert_eq!(state.pointer_root, (3000, 2000), "onto the scaled CRTC");
    motion(&mut b, &mut state, (3025, 2010), (25, 10));
    motion(&mut b, &mut state, (3050, 2020), (25, 10));
    assert_eq!(
        state.pointer_root,
        (3050, 2020),
        "relative motion is root space, unscaled by the 2.0 output (Q6)"
    );
    motion(&mut b, &mut state, (2500, 2020), (-550, 0));
    assert_eq!(state.pointer_root, (2560, 2020), "leaving it into the hole");
}

#[test]
fn direct_scanout_unflips_and_stays_off_while_another_output_is_transformed() {
    let (mut b, mut state) = silence_pair(
        [CrtcTransform::identity(), CrtcTransform::identity()],
        (5120, 1440),
    );
    b.scanout_m2.cursor_bound_all = true;
    let fallback = super::tests::seed_window(&mut b, 0x6a00, None, 0, 0);
    super::tests::install_direct_frame_for_target_test(&mut b, 0x6b00, fallback, true);
    b.scanout_m2.hold_direct = true;
    Backend::randr_layout_changed(&mut b, &mut state);
    assert!(
        !b.scanout_m2.unflip_requested,
        "identity keeps direct scanout"
    );

    state.randr.outputs[1].current_transform = scale(0x20000, Some(Filter::Bilinear));
    Backend::randr_layout_changed(&mut b, &mut state);
    assert!(b.platform.any_output_transformed());
    assert!(b.scanout_m2.unflip_requested);
    assert_eq!(b.scanout_m2.unflip_reason, Some("crtc_transform_changed"));
}

/// One RANDR request from client 5, through the core dispatcher.
fn randr_request(b: &mut KmsBackend, state: &mut ServerState, minor: u8, body: &[u8]) {
    use yserver_core::core_loop::process_request;
    process_request::process_request(
        state,
        b as &mut dyn Backend,
        yserver_protocol::x11::ClientId(5),
        yserver_protocol::x11::SequenceNumber(1),
        yserver_protocol::x11::RequestHeader {
            opcode: 128,
            data: minor,
            length_units: u32::try_from(1 + body.len().div_ceil(4)).unwrap(),
        },
        body,
        None,
    )
    .expect("process_request");
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn a_root_read_right_after_a_transform_becomes_current_sees_the_root() {
    use yserver_protocol::x11::randr as x11randr;
    let root = (192u16, 96u16);
    let Some(mut b) = transformed_pair(scale(0x20000, Some(Filter::Nearest)), root) else {
        return;
    };
    // Start at identity: the transform arrives through the protocol.
    b.platform.output_transforms.clear();
    b.scene.sync_output_layouts(&b.platform).unwrap();
    for output in &mut b.platform.outputs {
        output.output.picked.width = MODE.0;
        output.output.picked.height = MODE.1;
        output.output.modes = vec![output.output.picked.clone()];
    }
    // The fixture's KMS device takes this renderer's output, as startup's
    // automatic PRIME Output Source does.
    let sink = b.platform.outputs[1].key.device_key;
    let source = b.selected_render_provider_endpoint().expect("renderer");
    b.provider_output_sources.insert(sink, source);
    let mut state = ServerState::new();
    super::tests::install_client_for_render(&mut state, 5);
    b.rebuild_randr_state(&mut state, None, false);
    (state.randr.screen_width, state.randr.screen_height) = root;
    let right = state
        .randr
        .outputs
        .iter()
        .find(|o| o.x == i16::try_from(MODE.0).unwrap())
        .expect("right-hand output")
        .clone();

    let mut transform = right.crtc_id.to_le_bytes().to_vec();
    for cell in [0x20000i32, 0, 0, 0, 0x20000, 0, 0, 0, FIXED_ONE] {
        transform.extend_from_slice(&cell.to_le_bytes());
    }
    transform.extend_from_slice(&7u16.to_le_bytes());
    transform.extend_from_slice(&[0; 2]);
    transform.extend_from_slice(b"nearest\0");
    randr_request(
        &mut b,
        &mut state,
        x11randr::RR_SET_CRTC_TRANSFORM,
        &transform,
    );
    let mut config = right.crtc_id.to_le_bytes().to_vec();
    config.extend_from_slice(&[0; 4]);
    config.extend_from_slice(&state.randr.config_timestamp.to_le_bytes());
    config.extend_from_slice(&right.x.to_le_bytes());
    config.extend_from_slice(&right.y.to_le_bytes());
    config.extend_from_slice(&right.mode_id.to_le_bytes());
    config.extend_from_slice(&1u16.to_le_bytes());
    config.extend_from_slice(&[0; 2]);
    config.extend_from_slice(&right.output_id.to_le_bytes());
    randr_request(&mut b, &mut state, x11randr::RR_SET_CRTC_CONFIG, &config);
    assert!(
        b.platform.output_transform(1).is_some(),
        "transform current"
    );

    // No scene tick in between: the read itself composes the output.
    let root_xid = b.core.window_id;
    let got = b
        .get_image_pixels_for_tests(root_xid, 2, 64, 0, 128, 96, !0)
        .expect("get_image")
        .expect("bytes");
    for y in 0..96u32 {
        for x in 0..128u32 {
            let i = ((y * 128 + x) * 4) as usize;
            assert_eq!(
                got[i..i + 3],
                pattern(64 + x, y)[..3],
                "root ({}, {y})",
                64 + x
            );
        }
    }
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn a_truncated_priming_compose_is_not_read() {
    let root = (192u16, 96u16);
    let Some(mut b) = transformed_pair(scale(0x20000, Some(Filter::Nearest)), root) else {
        return;
    };
    // Root + cursor is two draws; an exhausted pool records only the root.
    register_test_cursor(&mut b);
    (b.core.cursor_x, b.core.cursor_y) = (80.0, 10.0);
    b.scene.test_prime_descriptor_sets = Some(1);
    let root_xid = b.core.window_id;
    let got = b
        .get_image_pixels_for_tests(root_xid, 2, 64, 0, 128, 96, !0)
        .expect("get_image")
        .expect("bytes");
    assert!(
        got.iter().all(|&byte| byte == 0),
        "zero-filled, not a partial frame"
    );
    assert!(
        b.scene.transform_intermediate(1).is_none(),
        "not marked composed"
    );

    b.scene.test_prime_descriptor_sets = None;
    let got = b
        .get_image_pixels_for_tests(root_xid, 2, 64, 0, 128, 96, !0)
        .expect("get_image")
        .expect("bytes");
    assert_eq!(
        got[..3],
        pattern(64, 0)[..3],
        "a complete priming compose is read"
    );
    let under = ((10 * 128 + 16) * 4) as usize;
    assert_eq!(
        got[under..under + 3],
        pattern(80, 10)[..3],
        "without the sprite"
    );
}
