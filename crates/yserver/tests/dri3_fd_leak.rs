//! DRI3 fd-leak harness — Phase 4.2 design §5.4.
//!
//! 10k iterations of (allocate dma-buf → import as DrawableImage →
//! drop). Asserts `/proc/self/fd` count returns to baseline after
//! the loop. Catches any slip in the §3.2 fd-ownership rule (close
//! on every error path between `dup` and successful `vkAllocateMemory`).
//!
//! Marked `#[ignore]` because it needs a working Vulkan ICD (lavapipe
//! suffices). Run with `cargo test -p yserver --test dri3_fd_leak --
//! --ignored` under vng or on bare metal.

#![cfg(target_os = "linux")]
// Test-side client buffers, outside the server's memory ledger.
#![allow(clippy::disallowed_methods)]

use std::fs;

fn fd_count() -> usize {
    fs::read_dir("/proc/self/fd")
        .map(|d| d.count())
        .unwrap_or(0)
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn dri3_import_loop_does_not_leak_fds() {
    use ash::vk;
    use yserver::kms::vk::{device::VkContext, dri3::DRM_FORMAT_MOD_LINEAR, target::DrawableImage};

    let vk = VkContext::new().expect("VkContext init failed — install lavapipe or run under vng");

    // Some lavapipe builds (e.g. GitHub's Ubuntu runner) advertise dma-buf
    // export but cannot back it; skip there instead of failing.
    match create_dmabuf_export(&vk, 64, 64) {
        Ok(probe) => drop(probe),
        Err(e) => {
            eprintln!("skipping: this Vulkan ICD cannot export dma-bufs ({e})");
            return;
        }
    }

    let baseline = fd_count();
    let iterations = 10_000usize;
    let mut peak = baseline;

    for _ in 0..iterations {
        // Self-allocate a dma-buf-exportable VkImage and re-import it
        // via DrawableImage::from_dmabuf. Both legs run on the same
        // VkContext (Venus would normally split them across two
        // contexts but the leak harness only cares about fd accounting
        // on the import side).
        let exporter = create_dmabuf_export(&vk, 64, 64).expect("export image");
        let drawable = DrawableImage::from_dmabuf(
            vk.clone(),
            exporter.fd,
            64,
            64,
            vk::Format::B8G8R8A8_UNORM,
            DRM_FORMAT_MOD_LINEAR,
            &[exporter.offset],
            &[exporter.pitch],
        )
        .expect("import");
        drop(drawable);

        let now = fd_count();
        if now > peak {
            peak = now;
        }
    }

    let after = fd_count();
    assert!(
        after <= baseline.saturating_add(8),
        "fd count grew: baseline={baseline}, after={after}, peak={peak}"
    );

    // Ignore peak in the assertion — the test is about leak,
    // not about transient growth during a loop iteration.
    let _ = peak;
}

struct ExportImage {
    fd: std::os::fd::OwnedFd,
    offset: u64,
    pitch: u32,
}

/// Allocate a `TILING_LINEAR`, dma-buf-exportable `VkImage`, export its
/// memory as a dma-buf fd, and return the fd + plane layout. Mirrors the
/// export half of `scanout::allocate_vk_scanout_image` but stays in the
/// test (the production allocator is private and returns a scanout type).
///
/// The exported fd holds an independent reference to the underlying DRM
/// buffer object, so the exporter's `VkImage` + `VkDeviceMemory` are
/// released immediately — the importer reads through the fd. That keeps
/// the 10k-iteration loop from leaking GPU objects while still exercising
/// the import-side fd accounting this harness exists to guard.
fn create_dmabuf_export(
    vk: &std::sync::Arc<yserver::kms::vk::device::VkContext>,
    width: u32,
    height: u32,
) -> Result<ExportImage, String> {
    use ash::vk;
    use std::os::fd::FromRawFd;

    let ext_memory_fd = vk
        .external_memory_fd
        .as_ref()
        .ok_or("VK_KHR_external_memory_fd unavailable")?;

    // 1. Exportable LINEAR image — the DRI3 import contract (dri3.rs §3.2)
    //    is TILING_LINEAR + a plain ExternalMemoryImageCreateInfo chain
    //    (no explicit-modifier struct).
    let mut external_info = vk::ExternalMemoryImageCreateInfo::default()
        .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let image_info = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(vk::Format::B8G8R8A8_UNORM)
        .extent(vk::Extent3D {
            width,
            height,
            depth: 1,
        })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::LINEAR)
        .usage(vk::ImageUsageFlags::TRANSFER_SRC | vk::ImageUsageFlags::SAMPLED)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .push_next(&mut external_info);
    let image = unsafe { vk.device.create_image(&image_info, None) }
        .map_err(|e| format!("create_image: {e:?}"))?;

    // 2. Dedicated, dma-buf-exportable memory.
    let mem_reqs = unsafe { vk.device.get_image_memory_requirements(image) };
    let mem_props = unsafe {
        vk.instance
            .get_physical_device_memory_properties(vk.physical_device)
    };
    let memory_type_index = (0..mem_props.memory_type_count)
        .find(|&i| {
            (mem_reqs.memory_type_bits & (1 << i)) != 0
                && mem_props.memory_types[i as usize]
                    .property_flags
                    .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
        })
        .or_else(|| {
            (0..mem_props.memory_type_count).find(|&i| (mem_reqs.memory_type_bits & (1 << i)) != 0)
        });
    let Some(memory_type_index) = memory_type_index else {
        unsafe { vk.device.destroy_image(image, None) };
        return Err("no compatible memory type".to_string());
    };

    let mut export_info = vk::ExportMemoryAllocateInfo::default()
        .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
    let alloc_info = vk::MemoryAllocateInfo::default()
        .allocation_size(mem_reqs.size)
        .memory_type_index(memory_type_index)
        .push_next(&mut export_info)
        .push_next(&mut dedicated);
    let memory = match unsafe { vk.device.allocate_memory(&alloc_info, None) } {
        Ok(m) => m,
        Err(e) => {
            unsafe { vk.device.destroy_image(image, None) };
            return Err(format!("allocate_memory: {e:?}"));
        }
    };
    if let Err(e) = unsafe { vk.device.bind_image_memory(image, memory, 0) } {
        unsafe {
            vk.device.free_memory(memory, None);
            vk.device.destroy_image(image, None);
        }
        return Err(format!("bind_image_memory: {e:?}"));
    }

    // 3. Plane layout (offset + row pitch) for the import.
    let layout = unsafe {
        vk.device.get_image_subresource_layout(
            image,
            vk::ImageSubresource {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                mip_level: 0,
                array_layer: 0,
            },
        )
    };

    // 4. Export the bound memory as a dma-buf fd.
    let get_fd_info = vk::MemoryGetFdInfoKHR::default()
        .memory(memory)
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let raw_fd = match unsafe { ext_memory_fd.get_memory_fd(&get_fd_info) } {
        Ok(fd) => fd,
        Err(e) => {
            unsafe {
                vk.device.free_memory(memory, None);
                vk.device.destroy_image(image, None);
            }
            return Err(format!("get_memory_fd: {e:?}"));
        }
    };
    // SAFETY: vkGetMemoryFdKHR creates a fresh fd and transfers ownership
    // to us, so wrapping it in an OwnedFd is sound.
    let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw_fd) };

    // The fd keeps the DRM buffer alive; release the Vulkan objects now.
    unsafe {
        vk.device.free_memory(memory, None);
        vk.device.destroy_image(image, None);
    }

    Ok(ExportImage {
        fd,
        offset: layout.offset,
        pitch: u32::try_from(layout.row_pitch).unwrap_or(u32::MAX),
    })
}
