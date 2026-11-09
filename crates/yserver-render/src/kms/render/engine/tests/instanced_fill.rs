//! Recording and pixel tests for the `RecordedFillRect` instanced draw path.

use super::*;
use crate::kms::render::{
    frame_builder::{PinnedUploadIdx, RecordedOp},
    store::{DrawableKind, DrawableStore},
    target::{Dst, Src},
};
use ash::vk;

fn target(
    gpu: &mut GpuCore,
    store: &mut DrawableStore,
    xid: u32,
    width: u16,
    height: u16,
    depth: u8,
) -> DrawableId {
    let storage = gpu
        .allocate_drawable_storage(width, height, depth)
        .expect("allocate storage");
    store
        .allocate(xid, DrawableKind::Pixmap, depth, false, storage)
        .expect("allocate drawable")
}

fn rect(x: i32, y: i32, width: u32, height: u32) -> vk::Rect2D {
    vk::Rect2D {
        offset: vk::Offset2D { x, y },
        extent: vk::Extent2D { width, height },
    }
}

fn full_rect(width: u32, height: u32) -> vk::Rect2D {
    rect(0, 0, width, height)
}

fn readback(
    engine: &mut RenderEngine,
    store: &mut DrawableStore,
    gpu: &mut GpuCore,
    id: DrawableId,
    width: u32,
    height: u32,
    depth: u8,
) -> Vec<u8> {
    engine
        .get_image(
            store,
            gpu,
            Src::server_internal(id),
            full_rect(width, height),
            depth,
        )
        .expect("get_image")
}

/// The open frame's recorded fills, as `(dst, instance_count)`. Read at the
/// test boundary, before the readback submits the frame.
fn recorded_fills(engine: &RenderEngine) -> Vec<(DrawableId, u32)> {
    let inner = engine.inner.as_ref().expect("live engine");
    let open = inner
        .frame_builder
        .open
        .as_ref()
        .expect("frame remains open");
    open.ops
        .iter()
        .filter_map(|op| match op {
            RecordedOp::FillRect(fr) => Some((fr.dst_id, fr.instance_count)),
            _ => None,
        })
        .collect()
}

fn expected_bgra_pixel(bytes: &[u8], width: usize, x: usize, y: usize) -> [u8; 4] {
    let offset = (y * width + x) * 4;
    bytes[offset..offset + 4]
        .try_into()
        .expect("four BGRA bytes")
}

fn rect_contains_pixel(rect: vk::Rect2D, x: i32, y: i32) -> bool {
    let x0 = rect.offset.x.max(0);
    let y0 = rect.offset.y.max(0);
    let x1 = rect
        .offset
        .x
        .saturating_add_unsigned(rect.extent.width)
        .min(17);
    let y1 = rect
        .offset
        .y
        .saturating_add_unsigned(rect.extent.height)
        .min(13);
    x >= x0 && y >= y0 && x < x1 && y < y1
}

// Mutations that must fail this test: one recorded fill per rect; instance count 1 at emit.
#[test]
#[ignore = "needs a live lavapipe Vulkan ICD"]
fn instanced_fill_one_draw_per_fill() {
    let mut gpu = live_gpu().expect("lavapipe Vulkan device must initialize");
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&gpu).expect("engine");
    let id = target(&mut gpu, &mut store, 0x501, 64, 64, 32);
    let color = crate::kms::render::engine::pixels::decode_x11_pixel_bgra(0xFF_3A_7C_B1);
    engine
        .fill_rect_batch(
            &mut store,
            &mut gpu,
            Dst::server_internal(id),
            color,
            &[rect(4, 5, 1, 1)],
        )
        .expect("one-rect fill");
    assert_eq!(recorded_fills(&engine), [(id, 1)]);
    let bytes = readback(&mut engine, &mut store, &mut gpu, id, 64, 64, 32);
    assert_eq!(
        expected_bgra_pixel(&bytes, 64, 4, 5),
        [0xB1, 0x7C, 0x3A, 0xFF]
    );

    let rects: Vec<vk::Rect2D> = (0..1000)
        .map(|i| rect(i % 64, (i / 64) % 64, 1, 1))
        .collect();
    engine
        .fill_rect_batch(
            &mut store,
            &mut gpu,
            Dst::server_internal(id),
            color,
            &rects,
        )
        .expect("1000-rect fill");
    assert_eq!(recorded_fills(&engine), [(id, 1000)]);
    let bytes = readback(&mut engine, &mut store, &mut gpu, id, 64, 64, 32);
    for i in [0, 1, 63, 64, 500, 999] {
        let (x, y) = ((i % 64) as usize, ((i / 64) % 64) as usize);
        assert_eq!(
            expected_bgra_pixel(&bytes, 64, x, y),
            [0xB1, 0x7C, 0x3A, 0xFF],
            "instance {i} must be drawn"
        );
    }

    engine.drain_all(&mut gpu);
}

// Mutations that must fail this test: quad size - 1; premultiplied source-over blending.
#[test]
#[ignore = "needs a live lavapipe Vulkan ICD"]
fn instanced_fill_pixels_exact() {
    let mut gpu = live_gpu().expect("lavapipe Vulkan device must initialize");
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&gpu).expect("engine");
    let width = 17u32;
    let height = 13u32;
    let bg_wire = 0xFF_25_46_73;
    let fill_wire = 0x6D_82_39_C7;
    let bg_color = crate::kms::render::engine::pixels::decode_x11_pixel_bgra(bg_wire);
    let fill_color = crate::kms::render::engine::pixels::decode_x11_pixel_bgra(fill_wire);
    let rects = [
        rect(0, 0, 1, 1),   // one pixel at the upper-left edge
        rect(2, 1, 5, 1),   // adjacent horizontal span
        rect(2, 2, 1, 4),   // one-pixel-wide vertical span
        rect(4, 2, 3, 3),   // overlaps both spans
        rect(7, 2, 2, 2),   // adjacent to the horizontal span
        rect(15, 10, 5, 5), // clipped at the right and bottom edges
        rect(16, 12, 1, 1), // one pixel at the lower-right edge
        rect(8, 8, 1, 1),   // isolated pixel for the size-minus-one mutation
    ];

    let bgra = target(&mut gpu, &mut store, 0x502, width as u16, height as u16, 32);
    engine
        .fill_rect(
            &mut store,
            &mut gpu,
            Dst::server_internal(bgra),
            full_rect(width, height),
            bg_color,
        )
        .expect("initialize BGRA background");
    let _ = readback(&mut engine, &mut store, &mut gpu, bgra, width, height, 32);
    engine
        .fill_rect_batch(
            &mut store,
            &mut gpu,
            Dst::server_internal(bgra),
            fill_color,
            &rects,
        )
        .expect("BGRA exact fill");
    let bgra_bytes = readback(&mut engine, &mut store, &mut gpu, bgra, width, height, 32);
    let bg_expected = [0x73, 0x46, 0x25, 0xFF];
    let fill_expected = [0xC7, 0x39, 0x82, 0x6D];
    for y in 0..height as i32 {
        for x in 0..width as i32 {
            let expected = if rects.iter().any(|r| rect_contains_pixel(*r, x, y)) {
                fill_expected
            } else {
                bg_expected
            };
            assert_eq!(
                expected_bgra_pixel(&bgra_bytes, width as usize, x as usize, y as usize),
                expected,
                "BGRA pixel ({x},{y}) must match all four clear-equivalent bytes"
            );
        }
    }

    let r8 = target(&mut gpu, &mut store, 0x503, width as u16, height as u16, 8);
    let r8_bg = crate::kms::render::engine::pixels::decode_x11_pixel_for_storage(
        0x49,
        8,
        vk::Format::R8_UNORM,
    );
    let r8_fill = crate::kms::render::engine::pixels::decode_x11_pixel_for_storage(
        0x37,
        8,
        vk::Format::R8_UNORM,
    );
    assert_ne!(0x37, 0xFF, "R8 source byte must differ from pushed alpha");
    engine
        .fill_rect(
            &mut store,
            &mut gpu,
            Dst::server_internal(r8),
            full_rect(width, height),
            r8_bg,
        )
        .expect("initialize R8 background");
    let _ = readback(&mut engine, &mut store, &mut gpu, r8, width, height, 8);
    engine
        .fill_rect_batch(
            &mut store,
            &mut gpu,
            Dst::server_internal(r8),
            r8_fill,
            &rects,
        )
        .expect("R8 exact fill");
    let r8_bytes = readback(&mut engine, &mut store, &mut gpu, r8, width, height, 8);
    let stride = (width as usize + 3) & !3;
    for y in 0..height as i32 {
        for x in 0..width as i32 {
            let expected = if rects.iter().any(|r| rect_contains_pixel(*r, x, y)) {
                0x37
            } else {
                0x49
            };
            assert_eq!(
                r8_bytes[y as usize * stride + x as usize],
                expected,
                "R8 pixel ({x},{y}) must preserve red, not alpha"
            );
        }
    }

    engine.drain_all(&mut gpu);
}

// Mutations that must fail this test: skip pipeline bind; skip scissor; bind the vertex buffer at offset 0; skip the colour push.
#[test]
#[ignore = "needs a live lavapipe Vulkan ICD"]
fn instanced_fill_in_session() {
    use crate::kms::cpu_types::Rectangle16;
    use yserver_protocol::x11::gc::GcFunction;

    let mut gpu = live_gpu().expect("lavapipe Vulkan device must initialize");
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&gpu).expect("engine");
    let width = 17u32;
    let height = 13u32;
    let id = target(&mut gpu, &mut store, 0x504, width as u16, height as u16, 32);
    let background = [0x3A, 0x29, 0x18, 0xFF];
    let background_bytes: Vec<u8> = background
        .iter()
        .copied()
        .cycle()
        .take((width * height * 4) as usize)
        .collect();
    engine
        .put_image(
            &mut store,
            &mut gpu,
            Dst::server_internal(id),
            vk::Offset2D::default(),
            vk::Extent2D { width, height },
            &background_bytes,
            32,
        )
        .expect("initialize session background");
    let _ = readback(&mut engine, &mut store, &mut gpu, id, width, height, 32);

    // This logic fill leaves a narrow scissor and a known pipeline/colour
    // behind for the two fill ops that continue the same pass.
    engine
        .logic_fill(
            &mut store,
            &mut gpu,
            Dst::server_internal(id),
            GcFunction::Copy,
            false,
            0xFF_63_74_85,
            &[Rectangle16 {
                x: 1,
                y: 1,
                width: 2,
                height: 2,
            }],
        )
        .expect("logic-fill predecessor");
    let color_a = crate::kms::render::engine::pixels::decode_x11_pixel_bgra(0xFF_A1_2B_31);
    let color_b = crate::kms::render::engine::pixels::decode_x11_pixel_bgra(0xFF_4D_C3_17);
    engine
        .fill_rect_batch(
            &mut store,
            &mut gpu,
            Dst::server_internal(id),
            color_a,
            &[rect(6, 2, 2, 2)],
        )
        .expect("fill A");
    engine
        .fill_rect_batch(
            &mut store,
            &mut gpu,
            Dst::server_internal(id),
            color_b,
            &[rect(11, 7, 3, 2)],
        )
        .expect("fill B");

    let inner = engine.inner.as_ref().expect("live engine");
    let open = inner
        .frame_builder
        .open
        .as_ref()
        .expect("frame remains open");
    // A same-dst logic fill followed by two same-dst fills, with nothing in
    // between: replay opens the pass on the logic fill and both fills take
    // `SessionStep::Continue` (the classification is unit-tested in
    // `engine/tests/mod.rs`).
    let shape: Vec<(&str, Option<DrawableId>)> = open
        .ops
        .iter()
        .map(|op| match op {
            RecordedOp::LogicFill(_) => ("logic", op.dst_id()),
            RecordedOp::FillRect(_) => ("fill", op.dst_id()),
            _ => ("other", op.dst_id()),
        })
        .collect();
    assert_eq!(
        shape,
        [("logic", Some(id)), ("fill", Some(id)), ("fill", Some(id))],
        "both fills must continue the logic fill's pass"
    );
    let fill_pins: Vec<PinnedUploadIdx> = open
        .ops
        .iter()
        .filter_map(|op| match op {
            RecordedOp::FillRect(fr) => Some(fr.instance_pin),
            _ => None,
        })
        .collect();
    assert_eq!(fill_pins.len(), 2, "two recorded fill instance slices");
    let a = open.pins.upload_slices[fill_pins[0].0 as usize];
    let b = open.pins.upload_slices[fill_pins[1].0 as usize];
    assert_eq!(a.buffer, b.buffer, "A and B share one arena block");
    assert_ne!(b.offset, 0, "B's slice begins at a nonzero arena offset");
    let _ = inner;

    let bytes = readback(&mut engine, &mut store, &mut gpu, id, width, height, 32);

    let predecessor = [0x85, 0x74, 0x63, 0xFF];
    let expected_a = [0x31, 0x2B, 0xA1, 0xFF];
    let expected_b = [0x17, 0xC3, 0x4D, 0xFF];
    let expected_bg = [0x3A, 0x29, 0x18, 0xFF];
    for y in 0..height as usize {
        for x in 0..width as usize {
            let expected = if (1..3).contains(&x) && (1..3).contains(&y) {
                predecessor
            } else if (6..8).contains(&x) && (2..4).contains(&y) {
                expected_a
            } else if (11..14).contains(&x) && (7..9).contains(&y) {
                expected_b
            } else {
                expected_bg
            };
            assert_eq!(
                expected_bgra_pixel(&bytes, width as usize, x, y),
                expected,
                "session pixel ({x},{y}) outside the predecessor scissor must be correct"
            );
        }
    }

    engine.drain_all(&mut gpu);
}

// Mutation that must fail this test: instance count 1.
#[test]
#[ignore = "needs a live lavapipe Vulkan ICD"]
fn instanced_fill_large() {
    use crate::kms::render::upload_arena::BLOCK_BYTES;

    let mut gpu = live_gpu().expect("lavapipe Vulkan device must initialize");
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&gpu).expect("engine");
    let width = 1500u32;
    let height = 1200u32;
    let id = target(&mut gpu, &mut store, 0x505, width as u16, height as u16, 32);
    let background = crate::kms::render::engine::pixels::decode_x11_pixel_bgra(0xFF_12_23_34);
    let color = crate::kms::render::engine::pixels::decode_x11_pixel_bgra(0xFF_D5_86_27);
    engine
        .fill_rect(
            &mut store,
            &mut gpu,
            Dst::server_internal(id),
            full_rect(width, height),
            background,
        )
        .expect("initialize large-fill background");
    let _ = readback(&mut engine, &mut store, &mut gpu, id, width, height, 32);

    // Place varied, non-overlapping rectangles on a 3-pixel grid. The first
    // and last instances have unique readback witnesses; all 200,000 remain
    // inside this 1500×1200 destination and exceed the shared-block size.
    let rects: Vec<vk::Rect2D> = (0..200_000u32)
        .map(|i| {
            let x = (i % 500) * 3;
            let y = (i / 500) * 3;
            rect(x as i32, y as i32, 1 + i % 3, 1 + (i / 7) % 3)
        })
        .collect();
    engine
        .fill_rect_batch(
            &mut store,
            &mut gpu,
            Dst::server_internal(id),
            color,
            &rects,
        )
        .expect("large fill");

    let inner = engine.inner.as_ref().expect("live engine");
    let open = inner
        .frame_builder
        .open
        .as_ref()
        .expect("frame remains open");
    let fill = open
        .ops
        .iter()
        .find_map(|op| match op {
            RecordedOp::FillRect(fr) => Some(fr),
            _ => None,
        })
        .expect("recorded fill");
    let slice = open.pins.upload_slices[fill.instance_pin.0 as usize];
    assert_eq!(fill.instance_count, 200_000);
    assert_eq!(slice.offset, 0, "dedicated-block slice begins at zero");
    assert_eq!(open.pins.uploads.dedicated_len(), 1);
    assert!(
        u64::from(fill.instance_count) * 16 > BLOCK_BYTES,
        "instance payload uses the dedicated arena-block path"
    );
    let _ = inner;

    let bytes = readback(&mut engine, &mut store, &mut gpu, id, width, height, 32);
    let expected = [0x27, 0x86, 0xD5, 0xFF];
    let background = [0x34, 0x23, 0x12, 0xFF];
    assert_eq!(expected_bgra_pixel(&bytes, width as usize, 0, 0), expected);
    assert_eq!(
        expected_bgra_pixel(&bytes, width as usize, 1497, 1197),
        expected
    );
    assert_eq!(
        expected_bgra_pixel(&bytes, width as usize, 1499, 1199),
        background
    );

    engine.drain_all(&mut gpu);
}
