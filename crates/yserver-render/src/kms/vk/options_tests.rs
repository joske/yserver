//! Lavapipe checks for the `VkContextOptions` gates.

use std::sync::Arc;

use ash::vk;

use super::{
    device::{VkContext, VkContextOptions},
    target::{EXPORT_FORMAT_BGRA8, allocate_exportable},
};

const HEADLESS_PRESENT: VkContextOptions = VkContextOptions {
    wants_present: true,
    surface_extension: Some(ash::ext::headless_surface::NAME),
    wants_kms_interop: false,
};

fn vk_or_skip(options: VkContextOptions) -> Option<Arc<VkContext>> {
    match VkContext::new_with_options(options) {
        Ok(vk) => Some(vk),
        Err(e) => {
            eprintln!("skipping: no Vk: {e:?}");
            None
        }
    }
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn headless_present_context_can_present_from_its_graphics_queue() {
    let Some(vk) = vk_or_skip(HEADLESS_PRESENT) else {
        return;
    };
    assert!(vk.swapchain, "wants_present enables VK_KHR_swapchain");
    let headless = ash::ext::headless_surface::Instance::new(&vk.entry, &vk.instance);
    let surface_fns = ash::khr::surface::Instance::new(&vk.entry, &vk.instance);
    let surface = unsafe {
        headless.create_headless_surface(&vk::HeadlessSurfaceCreateInfoEXT::default(), None)
    }
    .expect("vkCreateHeadlessSurfaceEXT");
    let supported = unsafe {
        surface_fns.get_physical_device_surface_support(
            vk.physical_device,
            vk.graphics_queue_family,
            surface,
        )
    };
    unsafe { surface_fns.destroy_surface(surface, None) };
    assert_eq!(
        supported,
        Ok(true),
        "graphics queue presents to the surface"
    );
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn surface_context_picks_a_device_presenting_to_the_probed_surface() {
    let mut probes = 0;
    let mut probe = |entry: &ash::Entry, instance: &ash::Instance| {
        probes += 1;
        let headless = ash::ext::headless_surface::Instance::new(entry, instance);
        unsafe {
            headless.create_headless_surface(&vk::HeadlessSurfaceCreateInfoEXT::default(), None)
        }
    };
    let vk = match VkContext::new_with_surface(HEADLESS_PRESENT, &mut probe) {
        Ok(vk) => vk,
        Err(e) => {
            eprintln!("skipping: no Vk: {e:?}");
            return;
        }
    };
    assert_eq!(probes, 1, "the device pick probed the surface once");
    assert!(vk.swapchain);

    let mut failing = |_: &ash::Entry, _: &ash::Instance| Err(vk::Result::ERROR_SURFACE_LOST_KHR);
    assert!(
        VkContext::new_with_surface(HEADLESS_PRESENT, &mut failing).is_err(),
        "no surface, no presenting context"
    );
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn kms_context_keeps_dmabuf_export_and_no_swapchain() {
    let Some(vk) = vk_or_skip(VkContextOptions::KMS) else {
        return;
    };
    assert!(!vk.swapchain);
    // Drivers on macOS (KosmicKrisp) have no dma-buf at all.
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    assert!(vk.dmabuf_export, "lavapipe exposes dma-buf export");
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn surface_context_refuses_dmabuf_export() {
    let Some(vk) = vk_or_skip(HEADLESS_PRESENT) else {
        return;
    };
    assert!(!vk.dmabuf_export);
    assert!(!vk.supports_sync_fd());
    assert_eq!(
        allocate_exportable(&vk, 16, 16, EXPORT_FORMAT_BGRA8).err(),
        Some(vk::Result::ERROR_FORMAT_NOT_SUPPORTED),
    );
    assert!(vk.tfp_tiling_strategy.get().is_none(), "no strategy probed");
}
