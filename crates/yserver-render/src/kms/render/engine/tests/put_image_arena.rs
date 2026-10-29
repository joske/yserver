//! PutImage source placement: images up to `PUT_IMAGE_ARENA_MAX_BYTES`
//! are copied into the open frame's upload arena, larger ones into a
//! pooled staging buffer. Every test reads the pixels back through
//! `get_image` and compares them with a CPU model of the puts.

use super::*;
use crate::kms::render::{
    engine::put_get::PUT_IMAGE_ARENA_MAX_BYTES,
    frame_builder::{PutImageSrc, RecordedOp},
    store::DrawableStore,
    target::{Dst, Src},
};

/// A drawable plus a CPU copy of what its pixels must be, in the
/// `get_image` wire layout (rows padded to 4 bytes).
struct Model {
    id: DrawableId,
    width: u32,
    height: u32,
    depth: u8,
    bytes: Vec<u8>,
}

impl Model {
    fn new(
        gpu: &mut GpuCore,
        store: &mut DrawableStore,
        width: u32,
        height: u32,
        depth: u8,
    ) -> Self {
        let storage = gpu
            .allocate_drawable_storage(
                u16::try_from(width).expect("width"),
                u16::try_from(height).expect("height"),
                depth,
            )
            .expect("alloc storage");
        let id = store
            .allocate(
                0x1,
                crate::kms::render::store::DrawableKind::Pixmap,
                depth,
                false,
                storage,
            )
            .expect("store.allocate");
        let stride = row_stride(depth, width);
        Self {
            id,
            width,
            height,
            depth,
            bytes: vec![0; stride * height as usize],
        }
    }

    /// Put a `w`x`h` wire image generated from `seed` at (`x`, `y`) and
    /// apply the same put, clipped to the drawable, to the model.
    #[allow(clippy::too_many_arguments)]
    fn put(
        &mut self,
        engine: &mut RenderEngine,
        store: &mut DrawableStore,
        gpu: &mut GpuCore,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
        seed: u32,
    ) {
        let bpp = bytes_per_pixel(self.depth);
        let src_stride = row_stride(self.depth, w);
        let mut src = vec![0u8; src_stride * h as usize];
        for row in 0..h as usize {
            for col in 0..w as usize {
                for c in 0..bpp {
                    src[row * src_stride + col * bpp + c] = pattern(seed, col, row, c, bpp);
                }
            }
        }
        engine
            .put_image(
                store,
                gpu,
                Dst::server_internal(self.id),
                vk::Offset2D { x, y },
                vk::Extent2D {
                    width: w,
                    height: h,
                },
                &src,
                self.depth,
            )
            .expect("put_image");

        let dst_stride = row_stride(self.depth, self.width);
        for row in 0..h as i32 {
            for col in 0..w as i32 {
                let (dx, dy) = (x + col, y + row);
                if dx < 0 || dy < 0 || dx >= self.width as i32 || dy >= self.height as i32 {
                    continue;
                }
                let s = row as usize * src_stride + col as usize * bpp;
                let d = dy as usize * dst_stride + dx as usize * bpp;
                self.bytes[d..d + bpp].copy_from_slice(&src[s..s + bpp]);
            }
        }
    }

    fn assert_read_back(
        &self,
        engine: &mut RenderEngine,
        store: &mut DrawableStore,
        gpu: &mut GpuCore,
    ) {
        let out = engine
            .get_image(
                store,
                gpu,
                Src::server_internal(self.id),
                vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: vk::Extent2D {
                        width: self.width,
                        height: self.height,
                    },
                },
                self.depth,
            )
            .expect("get_image");
        let stride = row_stride(self.depth, self.width);
        let used = self.width as usize * bytes_per_pixel(self.depth);
        assert_eq!(out.len(), self.bytes.len());
        for row in 0..self.height as usize {
            let range = row * stride..row * stride + used;
            assert_eq!(
                &out[range.clone()],
                &self.bytes[range],
                "depth-{} row {row} differs from the model",
                self.depth,
            );
        }
    }
}

fn bytes_per_pixel(depth: u8) -> usize {
    match depth {
        8 => 1,
        24 | 32 => 4,
        d => panic!("model covers depths 8/24/32, not {d}"),
    }
}

fn row_stride(depth: u8, width: u32) -> usize {
    (width as usize * bytes_per_pixel(depth)).div_ceil(4) * 4
}

fn pattern(seed: u32, x: usize, y: usize, channel: usize, bpp: usize) -> u8 {
    if bpp == 4 && channel == 3 {
        return 0xFF;
    }
    (x * 7 + y * 13 + channel * 61 + seed as usize * 31) as u8
}

/// The sources of the PutImage ops recorded in the open frame, in order.
fn open_put_sources(engine: &RenderEngine) -> Vec<PutImageSrc> {
    let open = engine
        .inner
        .as_ref()
        .and_then(|inner| inner.frame_builder.open.as_ref())
        .expect("frame open");
    open.ops
        .iter()
        .filter_map(|op| match op {
            RecordedOp::PutImage(pi) => Some(pi.src),
            _ => None,
        })
        .collect()
}

/// The (buffer, offset) each arena-backed source names in the open frame.
fn open_upload_slices(engine: &RenderEngine, sources: &[PutImageSrc]) -> Vec<(vk::Buffer, u64)> {
    let open = engine
        .inner
        .as_ref()
        .and_then(|inner| inner.frame_builder.open.as_ref())
        .expect("frame open");
    sources
        .iter()
        .filter_map(|src| match src {
            PutImageSrc::Upload(idx) => {
                let slice = open.pins.upload_slices[idx.0 as usize];
                Some((slice.buffer, slice.offset))
            }
            PutImageSrc::Staging(_) => None,
        })
        .collect()
}

#[test]
#[ignore = "needs live Vulkan ICD"]
// Mutation: emit copies every arena-backed put from offset 0.
fn small_puts_in_one_frame_share_the_arena_and_read_back_exactly() {
    let Some(mut gpu) = live_gpu() else {
        eprintln!("no VkContext available — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&gpu).expect("engine");
    let mut model = Model::new(&mut gpu, &mut store, 32, 32, 32);

    model.put(&mut engine, &mut store, &mut gpu, 0, 0, 32, 32, 1);
    model.put(&mut engine, &mut store, &mut gpu, 0, 0, 8, 8, 2);
    model.put(&mut engine, &mut store, &mut gpu, 10, 4, 7, 5, 3);
    // Overlaps both previous puts: the later put wins.
    model.put(&mut engine, &mut store, &mut gpu, 4, 4, 8, 8, 4);
    // Clipped at the top-left: only the trailing 3x3 lands.
    model.put(&mut engine, &mut store, &mut gpu, -2, -3, 5, 6, 5);

    let sources = open_put_sources(&engine);
    assert_eq!(sources.len(), 5, "all puts recorded in one open frame");
    assert!(
        sources
            .iter()
            .all(|src| matches!(src, PutImageSrc::Upload(_))),
        "small puts copy from the upload arena: {sources:?}"
    );
    let slices = open_upload_slices(&engine, &sources);
    for (i, a) in slices.iter().enumerate() {
        for b in &slices[i + 1..] {
            assert_ne!(a, b, "each put has its own arena slice");
        }
    }

    model.assert_read_back(&mut engine, &mut store, &mut gpu);
    engine.drain_all(&mut gpu);
}

#[test]
#[ignore = "needs live Vulkan ICD"]
// Mutation: place arena slices with byte alignment.
fn odd_width_depth8_puts_through_the_arena_read_back_exactly() {
    let Some(mut gpu) = live_gpu() else {
        eprintln!("no VkContext available — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&gpu).expect("engine");
    let mut model = Model::new(&mut gpu, &mut store, 13, 9, 8);

    model.put(&mut engine, &mut store, &mut gpu, 0, 0, 13, 9, 1);
    // 5x3 = 15 bytes: the next slice must not start right after it.
    model.put(&mut engine, &mut store, &mut gpu, 1, 1, 5, 3, 2);
    model.put(&mut engine, &mut store, &mut gpu, 6, 2, 3, 7, 3);
    model.put(&mut engine, &mut store, &mut gpu, 11, 7, 5, 5, 4);

    let sources = open_put_sources(&engine);
    assert_eq!(sources.len(), 4);
    assert!(
        sources
            .iter()
            .all(|src| matches!(src, PutImageSrc::Upload(_)))
    );
    // vkCmdCopyBufferToImage needs bufferOffset to be a multiple of 4.
    for (_, offset) in open_upload_slices(&engine, &sources) {
        assert_eq!(
            offset % 4,
            0,
            "arena slice offset {offset} is not 4-aligned"
        );
    }

    model.assert_read_back(&mut engine, &mut store, &mut gpu);
    engine.drain_all(&mut gpu);
}

#[test]
#[ignore = "needs live Vulkan ICD"]
// Mutation: route every put through the arena, or none.
fn puts_above_the_threshold_use_pooled_staging_alongside_arena_puts() {
    let Some(mut gpu) = live_gpu() else {
        eprintln!("no VkContext available — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&gpu).expect("engine");
    let mut model = Model::new(&mut gpu, &mut store, 160, 130, 32);

    // 128x128x4 is exactly the threshold: arena.
    assert_eq!(128 * 128 * 4, PUT_IMAGE_ARENA_MAX_BYTES);
    // 160x130x4 and 128x129x4 are above it: pooled staging.
    model.put(&mut engine, &mut store, &mut gpu, 0, 0, 160, 130, 1);
    model.put(&mut engine, &mut store, &mut gpu, 3, 0, 128, 128, 2);
    model.put(&mut engine, &mut store, &mut gpu, 20, 1, 128, 129, 3);
    model.put(&mut engine, &mut store, &mut gpu, 150, 120, 3, 3, 4);

    let sources = open_put_sources(&engine);
    assert!(
        matches!(
            sources.as_slice(),
            [
                PutImageSrc::Staging(_),
                PutImageSrc::Upload(_),
                PutImageSrc::Staging(_),
                PutImageSrc::Upload(_),
            ]
        ),
        "large puts take pooled staging, small ones the arena: {sources:?}"
    );

    model.assert_read_back(&mut engine, &mut store, &mut gpu);
    engine.drain_all(&mut gpu);
}

#[test]
#[ignore = "needs live Vulkan ICD"]
// Mutation: small puts acquire a pooled staging buffer each.
fn many_small_puts_in_one_frame_pin_no_staging_buffers() {
    let Some(mut gpu) = live_gpu() else {
        eprintln!("no VkContext available — skipping");
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&gpu).expect("engine");
    let mut model = Model::new(&mut gpu, &mut store, 80, 80, 24);

    // More same-size puts than the staging pool keeps per size.
    let mut seed = 0;
    for y in (0..80).step_by(10) {
        for x in (0..80).step_by(10) {
            model.put(&mut engine, &mut store, &mut gpu, x, y, 10, 10, seed);
            seed += 1;
        }
    }

    let open = engine
        .inner
        .as_ref()
        .and_then(|inner| inner.frame_builder.open.as_ref())
        .expect("frame open");
    assert_eq!(open_put_sources(&engine).len(), 64);
    assert!(
        open.pins.staging_buffers.is_empty(),
        "small puts pin no pooled staging buffers"
    );

    model.assert_read_back(&mut engine, &mut store, &mut gpu);
    engine.drain_all(&mut gpu);
}
