use super::*;

#[test]
#[ignore = "needs live Vulkan ICD"]
fn depth32_put_image_get_image_round_trip() {
    let Some(mut platform) = live_platform() else {
        eprintln!("no VkContext available — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");

    let storage = platform
        .allocate_drawable_storage(8, 8, 32)
        .expect("alloc storage");
    let id = store
        .allocate(
            0x1,
            crate::kms::render::store::DrawableKind::Pixmap,
            32,
            false,
            storage,
        )
        .expect("store.allocate");

    // 8x8 BGRA8 gradient.
    let mut src = vec![0u8; 8 * 8 * 4];
    for y in 0..8 {
        for x in 0..8 {
            let off = (y * 8 + x) * 4;
            src[off] = (x * 32) as u8; // B
            src[off + 1] = (y * 32) as u8; // G
            src[off + 2] = ((x + y) * 16) as u8; // R
            src[off + 3] = 0xFF; // A
        }
    }
    engine
        .put_image(
            &mut store,
            &mut platform,
            Dst::server_internal(id),
            vk::Offset2D { x: 0, y: 0 },
            vk::Extent2D {
                width: 8,
                height: 8,
            },
            &src,
            32,
        )
        .expect("put_image");

    let out = engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(id),
            vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent: vk::Extent2D {
                    width: 8,
                    height: 8,
                },
            },
            32,
        )
        .expect("get_image");
    assert_eq!(out, src, "depth-32 round-trip must be byte-identical");

    engine.drain_all(&mut platform);
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn fill_then_get_image_observes_clear_color() {
    let Some(mut platform) = live_platform() else {
        eprintln!("no VkContext available — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");

    let storage = platform.allocate_drawable_storage(4, 4, 32).expect("alloc");
    let id = store
        .allocate(
            0x1,
            crate::kms::render::store::DrawableKind::Pixmap,
            32,
            false,
            storage,
        )
        .unwrap();

    // Fill the whole pixmap with bright red (R=0xFF, G=0, B=0, A=0xFF).
    let color = decode_x11_pixel_bgra(0xFF_FF_00_00);
    engine
        .fill_rect(
            &mut store,
            &mut platform,
            Dst::server_internal(id),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 4,
                    height: 4,
                },
            },
            color,
        )
        .expect("fill_rect");

    let out = engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(id),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 4,
                    height: 4,
                },
            },
            32,
        )
        .expect("get_image");
    // Storage is BGRA8: every pixel should be [B=0, G=0, R=0xFF, A=0xFF].
    for px in out.chunks_exact(4) {
        assert_eq!(px[0], 0x00, "B");
        assert_eq!(px[1], 0x00, "G");
        assert_eq!(px[2], 0xFF, "R");
        assert_eq!(px[3], 0xFF, "A");
    }

    engine.drain_all(&mut platform);
}

/// `fill_rect` must write the source byte into `R8_UNORM`
/// storage, not treat it like BGRA. This locks the depth-8
/// GXcopy path that Xlib9 `XFillRectangle` exercises.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn fill_depth8_observes_r8_source_byte() {
    let Some(mut platform) = live_platform() else {
        eprintln!("no VkContext available — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");

    let storage = platform.allocate_drawable_storage(4, 4, 8).expect("alloc");
    let id = store
        .allocate(
            0x1,
            crate::kms::render::store::DrawableKind::Pixmap,
            8,
            false,
            storage,
        )
        .unwrap();

    let color = decode_x11_pixel_for_storage(0x01, 8, vk::Format::R8_UNORM);
    engine
        .fill_rect(
            &mut store,
            &mut platform,
            Dst::server_internal(id),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 4,
                    height: 4,
                },
            },
            color,
        )
        .expect("fill_rect");

    let out = engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(id),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 4,
                    height: 4,
                },
            },
            8,
        )
        .expect("get_image");
    for b in out {
        assert_eq!(b, 0x01, "R8 fill must preserve the source byte");
    }

    engine.drain_all(&mut platform);
}

/// Stage 3f.2: `engine.logic_fill` applies the per-`GcFunction`
/// `VkLogicOp` per pixel. Drives `Xor` against a pre-loaded BGRA8
/// pattern; expects each component to be the pre-load XOR'd with
/// the fg byte. Alpha is preserved via the `opaque_alpha=true`
/// pipeline (L1 server-α invariant on depth-24).
#[test]
#[ignore = "needs live Vulkan ICD"]
fn logic_fill_xor_applies_per_pixel() {
    use yserver_core::backend::GcFunction;

    let Some(mut platform) = live_platform() else {
        eprintln!("no VkContext available — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");

    // 4x4 BGRA8 pixmap. Store BGRA wire bytes B, G, R, A.
    let storage = platform.allocate_drawable_storage(4, 4, 24).expect("alloc");
    let id = store
        .allocate(
            0x1,
            crate::kms::render::store::DrawableKind::Pixmap,
            24,
            false,
            storage,
        )
        .unwrap();

    // Load every pixel with [B=0x20, G=0x40, R=0x80, A=0xFF].
    let mut pre = vec![0u8; 4 * 4 * 4];
    for px in pre.chunks_exact_mut(4) {
        px[0] = 0x20;
        px[1] = 0x40;
        px[2] = 0x80;
        px[3] = 0xFF;
    }
    engine
        .put_image(
            &mut store,
            &mut platform,
            Dst::server_internal(id),
            vk::Offset2D::default(),
            vk::Extent2D {
                width: 4,
                height: 4,
            },
            &pre,
            32,
        )
        .expect("put_image");

    // XOR with fg pixel 0x00FFFFFF (X11 wire = AARRGGBB: A=0,
    // R=0xFF, G=0xFF, B=0xFF). The recorder's `BGRA8_UNORM`
    // branch puts R/G/B into [0]/[1]/[2] of `fg_color`; the
    // logic-op output then targets the BGRA8 attachment in the
    // same channel order, so post-XOR every component reads as
    // `pre ^ 0xFF`.
    let rect = Rectangle16 {
        x: 0,
        y: 0,
        width: 4,
        height: 4,
    };
    engine
        .logic_fill(
            &mut store,
            &mut platform,
            Dst::server_internal(id),
            GcFunction::Xor,
            /* opaque_alpha */ true,
            /* fg */ 0x00FF_FFFF,
            &[rect],
        )
        .expect("logic_fill");

    let out = engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(id),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 4,
                    height: 4,
                },
            },
            32,
        )
        .expect("get_image");

    for px in out.chunks_exact(4) {
        assert_eq!(px[0], 0x20 ^ 0xFF, "B (XOR pre 0x20 with fg 0xFF)");
        assert_eq!(px[1], 0x40 ^ 0xFF, "G (XOR pre 0x40 with fg 0xFF)");
        assert_eq!(px[2], 0x80 ^ 0xFF, "R (XOR pre 0x80 with fg 0xFF)");
        // opaque_alpha=true: alpha channel mask drops alpha from
        // the LogicOp, so the destination's pre-load 0xFF is
        // preserved.
        assert_eq!(px[3], 0xFF, "A preserved by opaque_alpha mask");
    }

    engine.drain_all(&mut platform);
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn copy_area_disjoint_pixmaps_round_trip() {
    let Some(mut platform) = live_platform() else {
        eprintln!("no VkContext available — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");

    let storage_src = platform.allocate_drawable_storage(4, 4, 32).unwrap();
    let storage_dst = platform.allocate_drawable_storage(8, 4, 32).unwrap();
    let src = store
        .allocate(
            0x1,
            crate::kms::render::store::DrawableKind::Pixmap,
            32,
            false,
            storage_src,
        )
        .unwrap();
    let dst = store
        .allocate(
            0x2,
            crate::kms::render::store::DrawableKind::Pixmap,
            32,
            false,
            storage_dst,
        )
        .unwrap();

    // Fill src with red.
    let red = decode_x11_pixel_bgra(0xFF_FF_00_00);
    engine
        .fill_rect(
            &mut store,
            &mut platform,
            Dst::server_internal(src),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 4,
                    height: 4,
                },
            },
            red,
        )
        .unwrap();
    // Fill dst with blue.
    let blue = decode_x11_pixel_bgra(0xFF_00_00_FF);
    engine
        .fill_rect(
            &mut store,
            &mut platform,
            Dst::server_internal(dst),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 8,
                    height: 4,
                },
            },
            blue,
        )
        .unwrap();
    // Copy src into dst at (4, 0).
    engine
        .copy_area(
            &mut store,
            &mut platform,
            Src::server_internal(src),
            Dst::server_internal(dst),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 4,
                    height: 4,
                },
            },
            vk::Offset2D { x: 4, y: 0 },
        )
        .unwrap();

    let out = engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(dst),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 8,
                    height: 4,
                },
            },
            32,
        )
        .unwrap();
    // Left half (0..4) should be blue (B=0xFF, G=0, R=0, A=0xFF).
    for y in 0..4 {
        for x in 0..4 {
            let off = (y * 8 + x) * 4;
            assert_eq!(&out[off..off + 4], &[0xFF, 0x00, 0x00, 0xFF], "left blue");
        }
    }
    // Right half (4..8) should be red (B=0, G=0, R=0xFF, A=0xFF).
    for y in 0..4 {
        for x in 4..8 {
            let off = (y * 8 + x) * 4;
            assert_eq!(&out[off..off + 4], &[0x00, 0x00, 0xFF, 0xFF], "right red");
        }
    }

    engine.drain_all(&mut platform);
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn logic_fill_depth32_preserves_wire_alpha_when_not_opaque() {
    use yserver_core::backend::GcFunction;

    let Some(mut platform) = live_platform() else {
        eprintln!("no VkContext available — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");

    let storage = platform
        .allocate_drawable_storage(2, 2, 32)
        .expect("storage");
    let id = store
        .allocate(
            0x1,
            crate::kms::render::store::DrawableKind::Pixmap,
            32,
            false,
            storage,
        )
        .expect("alloc");

    engine
        .fill_rect(
            &mut store,
            &mut platform,
            Dst::server_internal(id),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 2,
                    height: 2,
                },
            },
            decode_x11_pixel_bgra(0),
        )
        .expect("clear");

    engine
        .logic_fill(
            &mut store,
            &mut platform,
            Dst::server_internal(id),
            GcFunction::Copy,
            /* opaque_alpha */ false,
            /* fg */ 0x0000_0001,
            &[Rectangle16 {
                x: 0,
                y: 0,
                width: 2,
                height: 2,
            }],
        )
        .expect("logic_fill");

    let out = engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(id),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 2,
                    height: 2,
                },
            },
            32,
        )
        .expect("get_image");

    for px in out.chunks_exact(4) {
        assert_eq!(px, &[0x01, 0x00, 0x00, 0x00]);
    }

    engine.drain_all(&mut platform);
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn logic_fill_r8_not_family_matches_x11_bytes() {
    use yserver_core::backend::GcFunction;

    let Some(mut platform) = live_platform() else {
        eprintln!("no VkContext available — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");

    let storage = platform
        .allocate_drawable_storage(2, 1, 8)
        .expect("storage");
    let id = store
        .allocate(
            0x1,
            crate::kms::render::store::DrawableKind::Pixmap,
            8,
            false,
            storage,
        )
        .expect("alloc");

    // Preload dst bytes [0x00, 0x03].
    let pre = vec![0x00, 0x03, 0x00, 0x00];
    engine
        .put_image(
            &mut store,
            &mut platform,
            Dst::server_internal(id),
            vk::Offset2D::default(),
            vk::Extent2D {
                width: 2,
                height: 1,
            },
            &pre,
            8,
        )
        .expect("put_image");

    let rect = Rectangle16 {
        x: 0,
        y: 0,
        width: 2,
        height: 1,
    };

    engine
        .logic_fill(
            &mut store,
            &mut platform,
            Dst::server_internal(id),
            GcFunction::Set,
            /* opaque_alpha */ true,
            /* fg */ 0,
            &[rect],
        )
        .expect("logic_fill set");
    let out = engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(id),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 2,
                    height: 1,
                },
            },
            8,
        )
        .expect("get_image set");
    assert_eq!(&out[..2], &[0xff, 0xff], "GXset must write all 1 bits");

    engine
        .put_image(
            &mut store,
            &mut platform,
            Dst::server_internal(id),
            vk::Offset2D::default(),
            vk::Extent2D {
                width: 2,
                height: 1,
            },
            &pre,
            8,
        )
        .expect("put_image reload");
    engine
        .logic_fill(
            &mut store,
            &mut platform,
            Dst::server_internal(id),
            GcFunction::Invert,
            /* opaque_alpha */ true,
            /* fg */ 0,
            &[rect],
        )
        .expect("logic_fill invert");
    let out = engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(id),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 2,
                    height: 1,
                },
            },
            8,
        )
        .expect("get_image invert");
    assert_eq!(&out[..2], &[0xff, 0xfc], "GXinvert must flip all 8 bits");

    engine.drain_all(&mut platform);
}

// GPU-level regression for the MATE compositor slow-drag-left shadow
// smear (commit fixing clamp_copy_rects). Reproduces the exact
// Present→COW shape: src_rect.offset == dst_pos == a NEGATIVE origin
// (the compositor's off-top-left damage sliver). The 2 off-screen
// columns are skipped on BOTH sides, so an 8-wide red source copied
// at x=-2 must paint dst columns 0..6 red and leave 6..8 blue. The
// old double-subtract copied only 4 columns (0..4), leaving cols 4..5
// stale blue — the trailing smear strip. Runs the real engine copy +
// GPU readback, not just the clamp arithmetic.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn copy_area_negative_offset_copies_trailing_strip() {
    let Some(mut platform) = live_platform() else {
        eprintln!("no VkContext available — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");

    let storage_src = platform.allocate_drawable_storage(8, 4, 32).unwrap();
    let storage_dst = platform.allocate_drawable_storage(8, 4, 32).unwrap();
    let src = store
        .allocate(
            0x1,
            crate::kms::render::store::DrawableKind::Pixmap,
            32,
            false,
            storage_src,
        )
        .unwrap();
    let dst = store
        .allocate(
            0x2,
            crate::kms::render::store::DrawableKind::Pixmap,
            32,
            false,
            storage_dst,
        )
        .unwrap();

    let red = decode_x11_pixel_bgra(0xFF_FF_00_00);
    let blue = decode_x11_pixel_bgra(0xFF_00_00_FF);
    let full8x4 = vk::Rect2D {
        offset: vk::Offset2D::default(),
        extent: vk::Extent2D {
            width: 8,
            height: 4,
        },
    };
    engine
        .fill_rect(
            &mut store,
            &mut platform,
            Dst::server_internal(src),
            full8x4,
            red,
        )
        .unwrap();
    engine
        .fill_rect(
            &mut store,
            &mut platform,
            Dst::server_internal(dst),
            full8x4,
            blue,
        )
        .unwrap();

    // Aligned negative origin: src sub-rect AND dst placement both at
    // x=-2 (mirrors PresentPixmap update rect with x0<0).
    engine
        .copy_area(
            &mut store,
            &mut platform,
            Src::server_internal(src),
            Dst::server_internal(dst),
            vk::Rect2D {
                offset: vk::Offset2D { x: -2, y: 0 },
                extent: vk::Extent2D {
                    width: 8,
                    height: 4,
                },
            },
            vk::Offset2D { x: -2, y: 0 },
        )
        .unwrap();

    let out = engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(dst),
            full8x4,
            32,
        )
        .unwrap();
    for y in 0..4 {
        for x in 0..8 {
            let off = (y * 8 + x) * 4;
            let px = &out[off..off + 4];
            if x < 6 {
                // The trailing strip cols 4..6 is what the bug dropped.
                assert_eq!(
                    px,
                    &[0x00, 0x00, 0xFF, 0xFF],
                    "col {x} must be red (copied)"
                );
            } else {
                assert_eq!(
                    px,
                    &[0xFF, 0x00, 0x00, 0xFF],
                    "col {x} must stay blue (off-copy)"
                );
            }
        }
    }

    engine.drain_all(&mut platform);
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn copy_area_self_overlap_scratch_path() {
    let Some(mut platform) = live_platform() else {
        eprintln!("no VkContext available — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");

    let storage = platform.allocate_drawable_storage(8, 1, 32).unwrap();
    let id = store
        .allocate(
            0x1,
            crate::kms::render::store::DrawableKind::Pixmap,
            32,
            false,
            storage,
        )
        .unwrap();

    // PutImage a horizontal gradient: 8 pixels each with a
    // distinct red value.
    let mut src = vec![0u8; 8 * 4];
    for x in 0..8 {
        let off = x * 4;
        src[off] = 0x00; // B
        src[off + 1] = 0x00; // G
        src[off + 2] = (x as u8) * 0x20; // R
        src[off + 3] = 0xFF; // A
    }
    engine
        .put_image(
            &mut store,
            &mut platform,
            Dst::server_internal(id),
            vk::Offset2D::default(),
            vk::Extent2D {
                width: 8,
                height: 1,
            },
            &src,
            32,
        )
        .unwrap();
    // Copy (0..4) → (2..6) (overlap; scratch path engages).
    engine
        .copy_area(
            &mut store,
            &mut platform,
            Src::server_internal(id),
            Dst::server_internal(id),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 4,
                    height: 1,
                },
            },
            vk::Offset2D { x: 2, y: 0 },
        )
        .unwrap();

    let out = engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(id),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 8,
                    height: 1,
                },
            },
            32,
        )
        .unwrap();
    // Expected R-channel sequence: [0, 0x20, 0, 0x20, 0x40, 0x60, 0xC0, 0xE0]
    // After copy of (0..4) → (2..6):
    //   col 0: original (R=0)
    //   col 1: original (R=0x20)
    //   col 2: src col 0 (R=0)
    //   col 3: src col 1 (R=0x20)
    //   col 4: src col 2 (R=0x40)
    //   col 5: src col 3 (R=0x60)
    //   col 6: original col 6 (R=0xC0)
    //   col 7: original col 7 (R=0xE0)
    let expected_r = [0x00, 0x20, 0x00, 0x20, 0x40, 0x60, 0xC0, 0xE0];
    for (x, &exp) in expected_r.iter().enumerate() {
        let off = x * 4 + 2;
        assert_eq!(
            out[off], exp,
            "R at col {x} (got {:#x}, want {exp:#x})",
            out[off]
        );
    }

    engine.drain_all(&mut platform);
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn put_image_then_fill_overwrites() {
    let Some(mut platform) = live_platform() else {
        eprintln!("no VkContext available — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&platform).expect("engine");

    let storage = platform.allocate_drawable_storage(4, 4, 32).expect("alloc");
    let id = store
        .allocate(
            0x1,
            crate::kms::render::store::DrawableKind::Pixmap,
            32,
            false,
            storage,
        )
        .unwrap();

    // PutImage all-blue, then fill (1,1)..(3,3) with green.
    // B=0xFF, G=0, R=0, A=0xFF
    let blue = [0xFFu8, 0x00, 0x00, 0xFF].repeat(16);
    engine
        .put_image(
            &mut store,
            &mut platform,
            Dst::server_internal(id),
            vk::Offset2D::default(),
            vk::Extent2D {
                width: 4,
                height: 4,
            },
            &blue,
            32,
        )
        .unwrap();
    let green = decode_x11_pixel_bgra(0xFF_00_FF_00);
    engine
        .fill_rect(
            &mut store,
            &mut platform,
            Dst::server_internal(id),
            vk::Rect2D {
                offset: vk::Offset2D { x: 1, y: 1 },
                extent: vk::Extent2D {
                    width: 2,
                    height: 2,
                },
            },
            green,
        )
        .unwrap();

    let out = engine
        .get_image(
            &mut store,
            &mut platform,
            Src::server_internal(id),
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D {
                    width: 4,
                    height: 4,
                },
            },
            32,
        )
        .unwrap();
    // (0,0) still blue.
    assert_eq!(&out[0..4], &[0xFF, 0x00, 0x00, 0xFF]);
    // (1,1) green: B=0, G=0xFF, R=0, A=0xFF.
    let off_1_1 = (4 + 1) * 4;
    assert_eq!(&out[off_1_1..off_1_1 + 4], &[0x00, 0xFF, 0x00, 0xFF]);
    // (3,3) still blue.
    let off_3_3 = (3 * 4 + 3) * 4;
    assert_eq!(&out[off_3_3..off_3_3 + 4], &[0xFF, 0x00, 0x00, 0xFF]);

    engine.drain_all(&mut platform);
}

#[test]
fn depth24_unpack_forces_alpha_ff() {
    // Source 1×1 with X-byte (alpha-slot) = 0x55.
    let src = vec![0x10u8, 0x20, 0x30, 0x55];
    let src_extent = vk::Extent2D {
        width: 1,
        height: 1,
    };
    let mut out = vec![0u8; 4];
    unpack_to_staging(&src, src_extent, 0, 0, 1, 1, 24, out.as_mut_ptr()).unwrap();
    assert_eq!(out, vec![0x10, 0x20, 0x30, 0xFF]);
}
