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
