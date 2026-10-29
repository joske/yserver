use super::*;
use crate::kms::{
    render::store::DrawableStore,
    vk::{device::VkContext, dri3, target},
};
use std::sync::Arc;

/// A live GPU whose ICD can allocate and export a dma-buf, or `None`.
fn export_capable_gpu() -> Option<(GpuCore, Arc<VkContext>)> {
    let Some(gpu) = live_gpu() else {
        eprintln!("no VkContext available — skipping");
        return None;
    };
    let vk = Arc::clone(gpu.vk()?);
    let probe = target::allocate_exportable(&vk, 16, 16, target::EXPORT_FORMAT_BGRA8)
        .and_then(|img| dri3::export_backing(&vk, &img).map(drop));
    if let Err(e) = probe {
        eprintln!("skipping: this Vulkan ICD cannot export dma-bufs ({e:?})");
        return None;
    }
    Some((gpu, vk))
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn promoted_export_reports_driver_plane_offset() {
    let Some((mut gpu, vk)) = export_capable_gpu() else {
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&gpu).expect("engine");
    let id = alloc_drawable_3a(&gpu, &mut store, 0x10, 64, 32);
    engine
        .promote_drawable_exportable(&mut gpu, &mut store, id)
        .expect("promote");

    let s = &store.get(id).expect("drawable").storage;
    // Same aspect the allocator queried with.
    let aspect_mask = match vk.tfp_tiling_strategy.get() {
        Some(target::TilingStrategy::Linear) => ash::vk::ImageAspectFlags::COLOR,
        Some(_) => ash::vk::ImageAspectFlags::MEMORY_PLANE_0_EXT,
        None => panic!("promotion set no tiling strategy"),
    };
    let layout = unsafe {
        vk.device.get_image_subresource_layout(
            s.image,
            ash::vk::ImageSubresource {
                aspect_mask,
                mip_level: 0,
                array_layer: 0,
            },
        )
    };
    let export = dri3::export_promoted(
        &vk,
        s.memory,
        s.export_stride,
        s.export_size,
        s.export_offset,
        s.export_modifier,
    )
    .expect("export_promoted");
    assert_eq!(u64::from(export.offset), layout.offset);
    assert_eq!(u64::from(export.stride), layout.row_pitch);
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn promoting_a_promoted_drawable_keeps_its_storage() {
    let Some((mut gpu, _vk)) = export_capable_gpu() else {
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&gpu).expect("engine");
    let id = alloc_drawable_3a(&gpu, &mut store, 0x11, 64, 32);
    engine
        .promote_drawable_exportable(&mut gpu, &mut store, id)
        .expect("first promote");
    let before = {
        let s = &store.get(id).expect("drawable").storage;
        (s.image, s.memory)
    };
    engine
        .promote_drawable_exportable(&mut gpu, &mut store, id)
        .expect("second promote");
    let s = &store.get(id).expect("drawable").storage;
    assert_eq!((s.image, s.memory), before);
}

// ── Scanout promotion ───────────────────────────────────────

use crate::kms::render::{
    engine::{ScanoutCandidate, ScanoutPromoteError},
    target::{Dst, Src},
};
use ash::vk;

/// A live GPU that can allocate linear scanout pixmaps. Checked with
/// `allocate_scanout_pixmap` itself, so the TFP tiling strategy stays unset.
fn scanout_capable_gpu() -> Option<(GpuCore, Arc<VkContext>)> {
    let Some(gpu) = live_gpu() else {
        eprintln!("no VkContext available — skipping");
        return None;
    };
    let vk = Arc::clone(gpu.vk()?);
    let probe = target::allocate_scanout_pixmap(&vk, 16, 16, target::EXPORT_FORMAT_BGRA8, &[])
        .and_then(|img| dri3::export_backing(&vk, &img).map(drop));
    if let Err(e) = probe {
        eprintln!("skipping: this Vulkan ICD cannot allocate linear scanout pixmaps ({e:?})");
        return None;
    }
    Some((gpu, vk))
}

const W: u16 = 32;
const H: u16 = 16;

fn full_rect() -> vk::Rect2D {
    vk::Rect2D {
        offset: vk::Offset2D::default(),
        extent: vk::Extent2D {
            width: u32::from(W),
            height: u32::from(H),
        },
    }
}

/// Opaque BGRA gradient, distinct per pixel.
fn gradient(seed: u8) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(usize::from(W) * usize::from(H) * 4);
    for y in 0..usize::from(H) {
        for x in 0..usize::from(W) {
            bytes.extend_from_slice(&[
                (x * 7) as u8 ^ seed,
                (y * 13) as u8,
                (x + y) as u8 ^ seed,
                0xFF,
            ]);
        }
    }
    bytes
}

fn put(
    engine: &mut RenderEngine,
    store: &mut DrawableStore,
    gpu: &mut GpuCore,
    id: DrawableId,
    bytes: &[u8],
) {
    engine
        .put_image(
            store,
            gpu,
            Dst::server_internal(id),
            vk::Offset2D::default(),
            full_rect().extent,
            bytes,
            32,
        )
        .expect("put_image");
}

fn get(
    engine: &mut RenderEngine,
    store: &mut DrawableStore,
    gpu: &mut GpuCore,
    id: DrawableId,
) -> Vec<u8> {
    engine
        .get_image(store, gpu, Src::server_internal(id), full_rect(), 32)
        .expect("get_image")
}

/// Accepts every candidate, recording its layout.
fn accept_probe(
    seen: &mut Option<(u64, u32, u64)>,
) -> impl FnOnce(&ScanoutCandidate<'_>) -> Result<(), ()> + '_ {
    move |candidate| {
        *seen = Some((candidate.offset, candidate.pitch, candidate.modifier));
        Ok(())
    }
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn scanout_promotion_keeps_pixels() {
    let Some((mut gpu, _vk)) = scanout_capable_gpu() else {
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&gpu).expect("engine");
    let id = alloc_drawable_3a(&gpu, &mut store, 0x20, W, H);
    let written = gradient(0x5A);
    put(&mut engine, &mut store, &mut gpu, id, &written);

    let mut seen = None;
    let promotion = engine
        .promote_drawable_for_scanout(&mut gpu, &mut store, id, &[&[0]], accept_probe(&mut seen))
        .expect("promote");
    let s = &store.get(id).expect("drawable").storage;
    assert!(s.is_exportable());
    assert_eq!(s.export_modifier, 0, "scanout promotion is linear");
    assert_eq!(seen, Some((s.export_offset, s.export_stride, 0)));
    assert_eq!(promotion.export.stride, s.export_stride);
    assert_eq!(
        crate::kms::vk::mem_accounting::category_of(s.memory),
        Some(crate::kms::vk::mem_accounting::MemCategory::Scanout),
        "scanout promotion is not an external export"
    );
    assert_eq!(get(&mut engine, &mut store, &mut gpu, id), written);
    engine.drain_all(&mut gpu);
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn scanout_promotion_leaves_tfp_strategy_unset() {
    let Some((mut gpu, vk)) = scanout_capable_gpu() else {
        return;
    };
    assert_eq!(vk.tfp_tiling_strategy.get(), None);
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&gpu).expect("engine");
    let id = alloc_drawable_3a(&gpu, &mut store, 0x21, W, H);
    let mut seen = None;
    engine
        .promote_drawable_for_scanout(&mut gpu, &mut store, id, &[], accept_probe(&mut seen))
        .expect("promote");
    assert_eq!(vk.tfp_tiling_strategy.get(), None);
    engine.drain_all(&mut gpu);
}

/// CopyArea from `src` into the left half, then a translucent OVER
/// Composite from `overlay` across the whole of `dst`.
fn draw_into(
    engine: &mut RenderEngine,
    store: &mut DrawableStore,
    gpu: &mut GpuCore,
    dst: DrawableId,
    src: DrawableId,
    overlay: DrawableId,
) {
    engine
        .copy_area(
            store,
            gpu,
            Src::server_internal(src),
            Dst::server_internal(dst),
            vk::Rect2D {
                offset: vk::Offset2D { x: 3, y: 2 },
                extent: vk::Extent2D {
                    width: u32::from(W) / 2,
                    height: u32::from(H) - 4,
                },
            },
            vk::Offset2D { x: 1, y: 1 },
        )
        .expect("copy_area");
    drive_render_composite_same_key_for_tests(
        engine,
        store,
        gpu,
        dst,
        overlay,
        &[(0, 0, u32::from(W), u32::from(H))],
    );
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn rendering_into_a_scanout_promoted_pixmap_matches_unpromoted() {
    let Some((mut gpu, _vk)) = scanout_capable_gpu() else {
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&gpu).expect("engine");
    let promoted = alloc_drawable_3a(&gpu, &mut store, 0x22, W, H);
    let plain = alloc_drawable_3a(&gpu, &mut store, 0x23, W, H);
    let src = alloc_drawable_3a(&gpu, &mut store, 0x24, W, H);
    let overlay = alloc_drawable_3a(&gpu, &mut store, 0x25, W, H);
    let base = gradient(0x11);
    put(&mut engine, &mut store, &mut gpu, promoted, &base);
    put(&mut engine, &mut store, &mut gpu, plain, &base);
    put(&mut engine, &mut store, &mut gpu, src, &gradient(0x77));
    let mut translucent = gradient(0x33);
    for (i, px) in translucent.chunks_exact_mut(4).enumerate() {
        let alpha = (i * 5 % 256) as u8;
        for c in &mut px[..3] {
            *c = (u16::from(*c) * u16::from(alpha) / 255) as u8;
        }
        px[3] = alpha;
    }
    put(&mut engine, &mut store, &mut gpu, overlay, &translucent);

    let mut seen = None;
    engine
        .promote_drawable_for_scanout(&mut gpu, &mut store, promoted, &[], accept_probe(&mut seen))
        .expect("promote");
    draw_into(&mut engine, &mut store, &mut gpu, promoted, src, overlay);
    draw_into(&mut engine, &mut store, &mut gpu, plain, src, overlay);
    let expected = get(&mut engine, &mut store, &mut gpu, plain);
    assert_ne!(expected, base, "the ops changed the unpromoted pixmap");
    assert_eq!(get(&mut engine, &mut store, &mut gpu, promoted), expected);
    engine.drain_all(&mut gpu);
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn failed_scanout_probe_leaves_the_drawable_untouched() {
    let Some((mut gpu, _vk)) = scanout_capable_gpu() else {
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&gpu).expect("engine");
    let id = alloc_drawable_3a(&gpu, &mut store, 0x26, W, H);
    let written = gradient(0x42);
    put(&mut engine, &mut store, &mut gpu, id, &written);
    let storage_of = |store: &DrawableStore| {
        let s = &store.get(id).expect("drawable").storage;
        (s.image, s.memory, s.current_layout, s.is_exportable())
    };
    let before = storage_of(&store);
    engine.drain_flush_outcomes();

    let mut probed = false;
    let result = engine.promote_drawable_for_scanout(&mut gpu, &mut store, id, &[], |_| {
        probed = true;
        Err::<(), _>("rejected")
    });
    assert!(probed);
    assert!(matches!(
        result,
        Err(ScanoutPromoteError::Probe("rejected"))
    ));
    assert_eq!(storage_of(&store), before);
    assert!(!before.3);
    assert!(
        engine.drain_flush_outcomes().is_empty(),
        "no submit boundary was taken"
    );
    assert_eq!(get(&mut engine, &mut store, &mut gpu, id), written);
    engine.drain_all(&mut gpu);
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn scanout_promotion_without_linear_plane_support_never_probes() {
    let Some((mut gpu, _vk)) = scanout_capable_gpu() else {
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&gpu).expect("engine");
    let id = alloc_drawable_3a(&gpu, &mut store, 0x27, W, H);
    let before = {
        let s = &store.get(id).expect("drawable").storage;
        (s.image, s.memory)
    };
    let x_tiled = 0x0100_0000_0000_0001_u64;
    let mut probed = false;
    let result =
        engine.promote_drawable_for_scanout(&mut gpu, &mut store, id, &[&[0], &[x_tiled]], |_| {
            probed = true;
            Ok::<(), ()>(())
        });
    assert!(matches!(
        result,
        Err(ScanoutPromoteError::Allocate(
            vk::Result::ERROR_FORMAT_NOT_SUPPORTED
        ))
    ));
    assert!(!probed);
    let s = &store.get(id).expect("drawable").storage;
    assert_eq!((s.image, s.memory), before);
    assert!(!s.is_exportable());
    engine.drain_all(&mut gpu);
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn scanout_promotion_refuses_an_exportable_drawable() {
    let Some((mut gpu, _vk)) = scanout_capable_gpu() else {
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&gpu).expect("engine");
    let id = alloc_drawable_3a(&gpu, &mut store, 0x28, W, H);
    let mut seen = None;
    engine
        .promote_drawable_for_scanout(&mut gpu, &mut store, id, &[], accept_probe(&mut seen))
        .expect("first promote");
    let before = {
        let s = &store.get(id).expect("drawable").storage;
        (s.image, s.memory)
    };
    let mut probed = false;
    let result = engine.promote_drawable_for_scanout(&mut gpu, &mut store, id, &[], |_| {
        probed = true;
        Ok::<(), ()>(())
    });
    assert!(matches!(
        result,
        Err(ScanoutPromoteError::AlreadyExportable)
    ));
    assert!(!probed);
    let s = &store.get(id).expect("drawable").storage;
    assert_eq!((s.image, s.memory), before);
    engine.drain_all(&mut gpu);
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn scanout_promoted_storage_is_never_pooled() {
    let Some((mut gpu, vk)) = scanout_capable_gpu() else {
        return;
    };
    let pool = Arc::new(crate::kms::vk::pixmap_pool::PixmapPool::new(vk));
    gpu.pixmap_pool = Some(Arc::clone(&pool));
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&gpu).expect("engine");
    let plain = alloc_drawable_3a(&gpu, &mut store, 0x29, W, H);
    let promoted = alloc_drawable_3a(&gpu, &mut store, 0x2A, W, H);
    let mut seen = None;
    engine
        .promote_drawable_for_scanout(&mut gpu, &mut store, promoted, &[], accept_probe(&mut seen))
        .expect("promote");
    engine.drain_all(&mut gpu);
    let accepted = || pool.stats().total_returns_accepted;

    let before = accepted();
    assert_eq!(
        store.decref(&mut gpu, plain, |_| {}),
        crate::kms::render::store::RetireDecision::Destroyed
    );
    assert_eq!(accepted(), before + 1, "control: plain storage is pooled");
    assert_eq!(
        store.decref(&mut gpu, promoted, |_| {}),
        crate::kms::render::store::RetireDecision::Destroyed
    );
    assert_eq!(accepted(), before + 1, "promoted storage is not pooled");
    engine.drain_all(&mut gpu);
}

/// Submit everything recorded so far.
fn submit_all(engine: &mut RenderEngine, store: &mut DrawableStore, gpu: &mut GpuCore) {
    engine
        .close_open_frame(
            store,
            gpu,
            crate::kms::render::frame_builder::CloseReason::SyncWait,
        )
        .expect("close frame");
    engine
        .flush_render_batch(
            store,
            gpu,
            crate::kms::render::engine::RenderFlushReason::Present,
        )
        .expect("flush batch");
    engine
        .flush_submit_group(
            store,
            gpu,
            crate::kms::render::submit_group::FlushReason::SyncBoundary,
        )
        .expect("flush group");
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn only_the_destination_of_a_read_gets_a_scanout_write() {
    use crate::kms::render::store::ScanoutWriteFence;

    let Some((mut gpu, _vk)) = scanout_capable_gpu() else {
        return;
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&gpu).expect("engine");
    let dst = alloc_drawable_3a(&gpu, &mut store, 0x40, W, H);
    let src = alloc_drawable_3a(&gpu, &mut store, 0x41, W, H);
    let mask = alloc_drawable_3a(&gpu, &mut store, 0x42, W, H);
    put(&mut engine, &mut store, &mut gpu, src, &gradient(1));
    put(&mut engine, &mut store, &mut gpu, mask, &gradient(2));
    submit_all(&mut engine, &mut store, &mut gpu);
    for id in [dst, src, mask] {
        store.track_scanout_writes(id);
    }

    engine
        .copy_area(
            &mut store,
            &mut gpu,
            Src::server_internal(src),
            Dst::server_internal(dst),
            full_rect(),
            vk::Offset2D::default(),
        )
        .expect("copy_area");
    let rect = [crate::kms::vk::ops::render::CompositeRect {
        src_x: 0,
        src_y: 0,
        mask_x: 0,
        mask_y: 0,
        dst_x: 0,
        dst_y: 0,
        width: u32::from(W),
        height: u32::from(H),
    }];
    engine
        .render_composite(
            &mut store,
            &mut gpu,
            3,
            ResolvedSource::Drawable(SourceDrawable::whole(src)),
            ResolvedSource::Drawable(SourceDrawable::whole(mask)),
            Dst::server_internal(dst),
            &rect,
            None,
            Repeat::None,
            Repeat::None,
            None,
            None,
            false,
            0,
            0,
            0,
        )
        .expect("render_composite");
    assert!(store.has_unsubmitted_scanout_write(dst));
    assert!(
        !store.has_unsubmitted_scanout_write(src),
        "a CopyArea and Composite source"
    );
    assert!(
        !store.has_unsubmitted_scanout_write(mask),
        "a Composite mask"
    );

    submit_all(&mut engine, &mut store, &mut gpu);
    assert!(!store.has_unsubmitted_scanout_write(dst));
    assert!(matches!(
        store.scanout_write_fence(dst),
        Some(ScanoutWriteFence::Fence(_) | ScanoutWriteFence::ExportFailed)
    ));
    for read in [src, mask] {
        assert!(matches!(
            store.scanout_write_fence(read),
            Some(ScanoutWriteFence::NoPendingWrite)
        ));
    }
    engine.drain_all(&mut gpu);
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn one_submit_group_fences_every_scanout_drawable_it_wrote() {
    use crate::kms::render::store::ScanoutWriteFence;
    use std::os::fd::AsRawFd as _;

    let Some((mut gpu, vk)) = scanout_capable_gpu() else {
        return;
    };
    if !vk.supports_sync_fd() {
        eprintln!("skipping: this Vulkan ICD exports no SYNC_FD");
        return;
    }
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&gpu).expect("engine");
    let a = alloc_drawable_3a(&gpu, &mut store, 0x50, W, H);
    let b = alloc_drawable_3a(&gpu, &mut store, 0x51, W, H);
    store.track_scanout_writes(a);
    store.track_scanout_writes(b);
    put(&mut engine, &mut store, &mut gpu, a, &gradient(3));
    put(&mut engine, &mut store, &mut gpu, b, &gradient(4));
    submit_all(&mut engine, &mut store, &mut gpu);

    let (Some(ScanoutWriteFence::Fence(fa)), Some(ScanoutWriteFence::Fence(fb))) =
        (store.scanout_write_fence(a), store.scanout_write_fence(b))
    else {
        panic!("both written drawables carry the submit's sync_file");
    };
    assert!(Arc::ptr_eq(fa, fb), "one sync_file serves the whole group");
    let fd = Arc::clone(fa);
    engine.drain_all(&mut gpu);
    let mut poll = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one valid pollfd for the duration of the call.
    let ready = unsafe { libc::poll(&raw mut poll, 1, 1000) };
    assert_eq!(ready, 1, "the sync_file signals once the submit completes");
    assert_ne!(poll.revents & libc::POLLIN, 0);
}
