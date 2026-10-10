use super::*;

pub(super) fn scanout_image_usage() -> vk::ImageUsageFlags {
    // The scanout image is only ever a compose render target (color
    // attachment), a transfer destination (initial clear / upload), and a
    // transfer source (root screenshots and diagnostic scanout dumps).
    // It is NEVER sampled — RENDER PictOps target pixmap/window mirrors,
    // not the scanout BO, and the compose pass samples those mirrors and
    // blends into this attachment.
    vk::ImageUsageFlags::COLOR_ATTACHMENT
        | vk::ImageUsageFlags::TRANSFER_SRC
        | vk::ImageUsageFlags::TRANSFER_DST
}

pub(super) fn addfb_flags_for_modifier(modifier: Option<u64>) -> FbCmd2Flags {
    if modifier.is_some() {
        FbCmd2Flags::MODIFIERS
    } else {
        FbCmd2Flags::empty()
    }
}

pub(super) fn destroy_scanout_image(vk: &VkContext, image: vk::Image, memory: vk::DeviceMemory) {
    unsafe {
        vk.device.destroy_image(image, None);
        crate::kms::vk::mem_accounting::free_memory(&vk.device, memory);
    }
}

/// Allocate a scanout `VkImage` whose memory is dma-buf-exportable;
/// bind memory; export the dma-buf; query the row pitch the driver
/// picked.
pub(super) fn allocate_vk_scanout_image(
    vk: &VkContext,
    width: u32,
    height: u32,
    plan: ScanoutAllocationPlan,
) -> Result<VkScanoutImage, vk::Result> {
    // GbmModifier is routed via allocate_gbm_scanout_image; the
    // Vulkan-alloc path never sees it.
    debug_assert!(
        !matches!(plan, ScanoutAllocationPlan::GbmModifier(_)),
        "GbmModifier plans must be dispatched via allocate_gbm_scanout_image"
    );
    let ext_memory_fd = vk
        .external_memory_fd
        .as_ref()
        .ok_or(vk::Result::ERROR_EXTENSION_NOT_PRESENT)?;

    let drm_modifier = match plan {
        ScanoutAllocationPlan::DrmModifier(modifier) => Some(modifier),
        ScanoutAllocationPlan::PaddedExplicitLinear { .. }
        | ScanoutAllocationPlan::ExplicitLinear
        | ScanoutAllocationPlan::LegacyLinear => None,
        ScanoutAllocationPlan::GbmModifier(_) => unreachable!(),
    };
    let padded_pitch = match plan {
        ScanoutAllocationPlan::PaddedExplicitLinear { row_pitch } => Some(row_pitch),
        _ => None,
    };

    let mut external_info = vk::ExternalMemoryImageCreateInfo::default()
        .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let modifier_storage = [drm_modifier.unwrap_or(crate::kms::vk::dri3::DRM_FORMAT_MOD_LINEAR)];
    let mut modifier_list = vk::ImageDrmFormatModifierListCreateInfoEXT::default()
        .drm_format_modifiers(if drm_modifier.is_some() {
            &modifier_storage
        } else {
            &[]
        });
    // Explicit single-plane LINEAR layout carrying the padded (aligned) stride.
    // `size = 0` lets the implementation compute the plane size for the pitch.
    let explicit_plane_layouts = [vk::SubresourceLayout {
        offset: 0,
        size: 0,
        row_pitch: u64::from(padded_pitch.unwrap_or(0)),
        array_pitch: 0,
        depth_pitch: 0,
    }];
    let mut explicit_modifier_info = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
        .drm_format_modifier(crate::kms::vk::dri3::DRM_FORMAT_MOD_LINEAR)
        .plane_layouts(&explicit_plane_layouts);

    let tiling = match plan {
        ScanoutAllocationPlan::DrmModifier(_)
        | ScanoutAllocationPlan::PaddedExplicitLinear { .. } => {
            vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT
        }
        ScanoutAllocationPlan::ExplicitLinear | ScanoutAllocationPlan::LegacyLinear => {
            vk::ImageTiling::LINEAR
        }
        ScanoutAllocationPlan::GbmModifier(_) => unreachable!(),
    };

    let image_info_base = vk::ImageCreateInfo::default()
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
        .tiling(tiling)
        .usage(scanout_image_usage())
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED);

    let image_info = match plan {
        ScanoutAllocationPlan::DrmModifier(_) => image_info_base
            .push_next(&mut external_info)
            .push_next(&mut modifier_list),
        ScanoutAllocationPlan::PaddedExplicitLinear { .. } => image_info_base
            .push_next(&mut external_info)
            .push_next(&mut explicit_modifier_info),
        ScanoutAllocationPlan::ExplicitLinear | ScanoutAllocationPlan::LegacyLinear => {
            image_info_base.push_next(&mut external_info)
        }
        ScanoutAllocationPlan::GbmModifier(_) => unreachable!(),
    };

    let image = unsafe { vk.device.create_image(&image_info, None)? };

    // 3. Memory: dma-buf-exportable + dedicated to this image.
    let mem_reqs = unsafe { vk.device.get_image_memory_requirements(image) };
    let mem_props = unsafe {
        vk.instance
            .get_physical_device_memory_properties(vk.physical_device)
    };
    let memory_type_index = match pick_memory_type(
        &mem_props,
        mem_reqs.memory_type_bits,
        vk::MemoryPropertyFlags::DEVICE_LOCAL,
    )
    .or_else(|| {
        pick_memory_type(
            &mem_props,
            mem_reqs.memory_type_bits,
            vk::MemoryPropertyFlags::empty(),
        )
    }) {
        Some(i) => i,
        None => {
            unsafe { vk.device.destroy_image(image, None) };
            return Err(vk::Result::ERROR_OUT_OF_DEVICE_MEMORY);
        }
    };

    let mut export_info = vk::ExportMemoryAllocateInfo::default()
        .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
    let alloc_info = vk::MemoryAllocateInfo::default()
        .allocation_size(mem_reqs.size)
        .memory_type_index(memory_type_index)
        .push_next(&mut export_info)
        .push_next(&mut dedicated);

    let memory = match crate::kms::vk::mem_accounting::allocate_memory(
        &vk.device,
        &alloc_info,
        crate::kms::vk::mem_accounting::MemCategory::Scanout,
        &mem_props,
    ) {
        Ok(m) => m,
        Err(e) => {
            unsafe { vk.device.destroy_image(image, None) };
            return Err(e);
        }
    };

    if let Err(e) = unsafe { vk.device.bind_image_memory(image, memory, 0) } {
        unsafe {
            crate::kms::vk::mem_accounting::free_memory(&vk.device, memory);
            vk.device.destroy_image(image, None);
        }
        return Err(e);
    }

    let selected_modifier = match plan {
        ScanoutAllocationPlan::DrmModifier(_) => {
            let Some(ext) = vk.image_drm_format_modifier_ext.as_ref() else {
                unsafe {
                    crate::kms::vk::mem_accounting::free_memory(&vk.device, memory);
                    vk.device.destroy_image(image, None);
                }
                return Err(vk::Result::ERROR_EXTENSION_NOT_PRESENT);
            };
            let mut props = vk::ImageDrmFormatModifierPropertiesEXT::default();
            if let Err(e) =
                unsafe { ext.get_image_drm_format_modifier_properties(image, &mut props) }
            {
                unsafe {
                    crate::kms::vk::mem_accounting::free_memory(&vk.device, memory);
                    vk.device.destroy_image(image, None);
                }
                return Err(e);
            }
            Some(props.drm_format_modifier)
        }
        // Created with an explicit LINEAR modifier — no need to re-query it.
        ScanoutAllocationPlan::PaddedExplicitLinear { .. } => {
            Some(crate::kms::vk::dri3::DRM_FORMAT_MOD_LINEAR)
        }
        ScanoutAllocationPlan::ExplicitLinear => Some(crate::kms::vk::dri3::DRM_FORMAT_MOD_LINEAR),
        ScanoutAllocationPlan::LegacyLinear => None,
        ScanoutAllocationPlan::GbmModifier(_) => unreachable!(),
    };

    // Row pitch from the driver. We need this for KMS addfb2.
    // Modifier-tiled images MUST be queried with a MEMORY_PLANE aspect;
    // COLOR is a validation error (the single-plane scanout buffer is
    // plane 0). LINEAR-tiled fallbacks keep the COLOR aspect. The
    // padded-explicit-LINEAR image is a DRM-modifier image too → MEMORY_PLANE_0.
    let layout_aspect = match plan {
        ScanoutAllocationPlan::DrmModifier(_)
        | ScanoutAllocationPlan::PaddedExplicitLinear { .. } => {
            vk::ImageAspectFlags::MEMORY_PLANE_0_EXT
        }
        ScanoutAllocationPlan::ExplicitLinear | ScanoutAllocationPlan::LegacyLinear => {
            vk::ImageAspectFlags::COLOR
        }
        ScanoutAllocationPlan::GbmModifier(_) => unreachable!(),
    };
    let layout = unsafe {
        vk.device.get_image_subresource_layout(
            image,
            vk::ImageSubresource {
                aspect_mask: layout_aspect,
                mip_level: 0,
                array_layer: 0,
            },
        )
    };
    let pitch = u32::try_from(layout.row_pitch).unwrap_or(u32::MAX);

    // 4. Export the bound memory as a dma-buf fd.
    let get_fd_info = vk::MemoryGetFdInfoKHR::default()
        .memory(memory)
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let raw_fd = match unsafe { ext_memory_fd.get_memory_fd(&get_fd_info) } {
        Ok(fd) => fd,
        Err(e) => {
            unsafe {
                crate::kms::vk::mem_accounting::free_memory(&vk.device, memory);
                vk.device.destroy_image(image, None);
            }
            return Err(e);
        }
    };
    let dmabuf = crate::kms::vk::owned_fd_from_vk(raw_fd, "vkGetMemoryFdKHR(DMA_BUF)")?;

    let offset = u32::try_from(layout.offset).unwrap_or(0);
    Ok(VkScanoutImage {
        image,
        memory,
        dmabuf,
        pitch,
        offset,
        modifier: selected_modifier,
        gbm_bo: None,
    })
}

/// GBM-allocate a single-plane scanout BO with the given DRM
/// modifier, then import its dma-buf into Vulkan as the compose
/// render target. This is the preferred allocation path — see the
/// module doc-comment for why.
///
/// The returned `VkScanoutImage.gbm_bo` MUST be kept alive at least
/// until the returned VkImage/VkDeviceMemory/GEM/framebuffer have all
/// been torn down. `ScanoutBo` handles that by declaring `gbm_bo`
/// last in its field list so Rust drops it after the explicit `Drop`
/// impl has released the derived resources.
pub(super) fn allocate_gbm_scanout_image(
    vk: &VkContext,
    gbm: &Rc<GbmDevice>,
    width: u32,
    height: u32,
    modifier: u64,
) -> Result<VkScanoutImage, GbmScanoutError> {
    let ext_memory_fd = vk
        .external_memory_fd
        .as_ref()
        .ok_or(GbmScanoutError::MissingExtension(
            "VK_KHR_external_memory_fd",
        ))?;
    if vk.image_drm_format_modifier_ext.is_none() {
        return Err(GbmScanoutError::MissingExtension(
            "VK_EXT_image_drm_format_modifier",
        ));
    }

    // Codex gate: verify Vulkan can IMPORT (not just export) a
    // COLOR_ATTACHMENT image with this exact modifier as DMA_BUF.
    if !scanout_modifier_is_single_plane_importable(vk, modifier) {
        return Err(GbmScanoutError::NotImportable(modifier));
    }

    // 1. GBM allocation — driver-side scanout-layout buffer.
    let modifier_iter = std::iter::once(gbm::Modifier::from(modifier));
    let bo = gbm
        .create_buffer_object_with_modifiers2::<()>(
            width,
            height,
            gbm::Format::Xrgb8888,
            modifier_iter,
            gbm::BufferObjectFlags::RENDERING | gbm::BufferObjectFlags::SCANOUT,
        )
        .map_err(GbmScanoutError::GbmCreate)?;

    // Multi-plane modifiers (e.g. AMD DCC compression) are out of
    // scope for the first cut — see codex correction in the spec.
    let plane_count = bo.plane_count();
    if plane_count != 1 {
        return Err(GbmScanoutError::MultiPlane(plane_count));
    }
    let gbm_modifier: u64 = bo.modifier().into();
    if gbm_modifier != modifier {
        return Err(GbmScanoutError::UnexpectedModifier {
            requested: modifier,
            actual: gbm_modifier,
        });
    }
    let stride = bo.stride_for_plane(0);
    let offset = bo.offset(0);

    // 2. Bo dma-buf fd. Vulkan takes ownership of the fd we hand to
    //    ImportMemoryFdInfoKHR ONLY on vkAllocateMemory success —
    //    dup so we retain a copy for PRIME_FD_TO_HANDLE afterwards
    //    (matches the DRI3 importer's ownership rule at
    //    target.rs:355).
    let bo_fd = bo.fd().map_err(|_| GbmScanoutError::InvalidBoFd)?;
    let vk_fd_owned = bo_fd.try_clone().map_err(GbmScanoutError::FdDup)?;
    let vk_fd_raw = vk_fd_owned.into_raw_fd();

    // 3. Create the VkImage against GBM's stride/offset via the
    //    explicit-modifier layout struct. Same usage tuple as the
    //    IMPORTABLE gate above (`scanout_image_usage()` + DMA_BUF external
    //    memory + DRM_FORMAT_MODIFIER_EXT tiling).
    let plane_layouts = [vk::SubresourceLayout {
        offset: u64::from(offset),
        size: 0,
        row_pitch: u64::from(stride),
        array_pitch: 0,
        depth_pitch: 0,
    }];
    let mut explicit_modifier_info = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
        .drm_format_modifier(gbm_modifier)
        .plane_layouts(&plane_layouts);
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
        .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
        .usage(scanout_image_usage())
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .push_next(&mut external_info)
        .push_next(&mut explicit_modifier_info);
    let image = match unsafe { vk.device.create_image(&image_info, None) } {
        Ok(i) => i,
        Err(e) => {
            unsafe { libc::close(vk_fd_raw) };
            return Err(GbmScanoutError::Vk(e));
        }
    };

    // 4. Memory-type selection intersects image requirements with
    //    the dma-buf's own compatible memory types
    //    (vkGetMemoryFdPropertiesKHR). Codex flagged that the DRI3
    //    importer at target.rs:371 skips this — it's mandated for
    //    robust external import and NVIDIA proprietary in particular
    //    exposes distinct memory types for imported vs local BOs.
    let mem_reqs = unsafe { vk.device.get_image_memory_requirements(image) };
    let mut fd_props = vk::MemoryFdPropertiesKHR::default();
    if let Err(e) = unsafe {
        ext_memory_fd.get_memory_fd_properties(
            vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
            vk_fd_raw,
            &mut fd_props,
        )
    } {
        unsafe {
            vk.device.destroy_image(image, None);
            libc::close(vk_fd_raw);
        }
        return Err(GbmScanoutError::Vk(e));
    }
    let mem_props = unsafe {
        vk.instance
            .get_physical_device_memory_properties(vk.physical_device)
    };
    let effective_type_bits = mem_reqs.memory_type_bits & fd_props.memory_type_bits;
    if effective_type_bits == 0 {
        unsafe {
            vk.device.destroy_image(image, None);
            libc::close(vk_fd_raw);
        }
        return Err(GbmScanoutError::NoImportableMemoryType);
    }
    let memory_type_index = pick_memory_type(
        &mem_props,
        effective_type_bits,
        vk::MemoryPropertyFlags::DEVICE_LOCAL,
    )
    .or_else(|| {
        pick_memory_type(
            &mem_props,
            effective_type_bits,
            vk::MemoryPropertyFlags::empty(),
        )
    });
    let Some(memory_type_index) = memory_type_index else {
        unsafe {
            vk.device.destroy_image(image, None);
            libc::close(vk_fd_raw);
        }
        return Err(GbmScanoutError::NoImportableMemoryType);
    };

    let mut import_info = vk::ImportMemoryFdInfoKHR::default()
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
        .fd(vk_fd_raw);
    let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
    let alloc_info = vk::MemoryAllocateInfo::default()
        .allocation_size(mem_reqs.size)
        .memory_type_index(memory_type_index)
        .push_next(&mut import_info)
        .push_next(&mut dedicated);
    let memory = match crate::kms::vk::mem_accounting::allocate_memory(
        &vk.device,
        &alloc_info,
        crate::kms::vk::mem_accounting::MemCategory::Scanout,
        &mem_props,
    ) {
        Ok(m) => m,
        Err(e) => {
            unsafe {
                vk.device.destroy_image(image, None);
                // vkAllocateMemory consumes vk_fd_raw only on success.
                libc::close(vk_fd_raw);
            }
            return Err(GbmScanoutError::Vk(e));
        }
    };
    // On success `memory` owns `vk_fd_raw`; do NOT close it here.
    if let Err(e) = unsafe { vk.device.bind_image_memory(image, memory, 0) } {
        unsafe {
            crate::kms::vk::mem_accounting::free_memory(&vk.device, memory);
            vk.device.destroy_image(image, None);
        }
        return Err(GbmScanoutError::Vk(e));
    }

    // Sanity-check what the GBM-imported layout looks like from
    // Vulkan's side. If GBM's stride/offset disagrees with what
    // Vulkan reports back for the same modifier, that's a driver
    // bug worth surfacing; the AddFB2 side always gets GBM's
    // numbers regardless (they came from the same driver that
    // laid out the BO).
    let layout = unsafe {
        vk.device.get_image_subresource_layout(
            image,
            vk::ImageSubresource {
                aspect_mask: vk::ImageAspectFlags::MEMORY_PLANE_0_EXT,
                mip_level: 0,
                array_layer: 0,
            },
        )
    };
    if layout.row_pitch != u64::from(stride) || layout.offset != u64::from(offset) {
        log::warn!(
            "scanout gbm import: layout mismatch — gbm(stride={stride},offset={offset}) \
             vk(row_pitch={},offset={}); using gbm values",
            layout.row_pitch,
            layout.offset,
        );
    }

    // We still need a dma-buf fd for PRIME_FD_TO_HANDLE. Vulkan
    // owns the dup we handed it; reuse the original bo_fd we kept
    // around.
    Ok(VkScanoutImage {
        image,
        memory,
        dmabuf: bo_fd,
        pitch: stride,
        offset,
        modifier: Some(gbm_modifier),
        gbm_bo: Some(bo),
    })
}

impl std::fmt::Display for GbmScanoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingExtension(name) => write!(f, "missing Vulkan extension: {name}"),
            Self::NotImportable(m) => write!(
                f,
                "modifier 0x{m:x} is not IMPORTABLE + DMA_BUF for COLOR_ATTACHMENT B8G8R8A8_UNORM"
            ),
            Self::GbmCreate(e) => write!(f, "gbm_bo_create_with_modifiers: {e}"),
            Self::MultiPlane(n) => write!(
                f,
                "multi-plane modifier not supported (plane_count={n}); first cut is \
                 single-plane only"
            ),
            Self::UnexpectedModifier { requested, actual } => write!(
                f,
                "GBM returned modifier 0x{actual:x} for exact requested modifier 0x{requested:x}"
            ),
            Self::InvalidBoFd => write!(f, "gbm_bo_get_fd returned an invalid fd"),
            Self::FdDup(e) => write!(f, "dup(gbm_bo_fd) failed: {e}"),
            Self::NoImportableMemoryType => write!(
                f,
                "no memory type satisfies image requirements ∩ dma-buf-import requirements"
            ),
            Self::Vk(r) => write!(f, "vk error: {r:?}"),
        }
    }
}

impl DrmPlanarBuffer for VkScanoutFb {
    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
    fn format(&self) -> DrmFourcc {
        DrmFourcc::Xrgb8888
    }
    fn modifier(&self) -> Option<DrmModifier> {
        self.modifier.map(DrmModifier::from)
    }
    fn pitches(&self) -> [u32; 4] {
        [self.pitch, 0, 0, 0]
    }
    fn handles(&self) -> [Option<DrmBufferHandle>; 4] {
        [Some(self.gem_handle), None, None, None]
    }
    fn offsets(&self) -> [u32; 4] {
        [self.offset, 0, 0, 0]
    }
}

/// Create a binary `VkSemaphore` whose payload can be exported as a
/// SYNC_FD via `vkGetSemaphoreFdKHR`. Reused for the bo's full
/// lifetime; the fd payload churns per submit.
pub(super) fn create_export_semaphore(vk: &VkContext) -> Result<vk::Semaphore, vk::Result> {
    let mut export_info = vk::ExportSemaphoreCreateInfo::default()
        .handle_types(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);
    let create_info = vk::SemaphoreCreateInfo::default().push_next(&mut export_info);
    unsafe { vk.device.create_semaphore(&create_info, None) }
}

/// Allocate per-bo transfer resources: command pool + 1 command
/// buffer; staging buffer + host-mapped device memory sized for one
/// XRGB8888 frame at (width × height).
pub(super) fn allocate_transfer_resources(
    vk: &VkContext,
    width: u32,
    height: u32,
) -> Result<TransferResources, vk::Result> {
    let pool_info = vk::CommandPoolCreateInfo::default()
        .queue_family_index(vk.graphics_queue_family)
        .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
    let command_pool = unsafe { vk.device.create_command_pool(&pool_info, None)? };

    let cb_info = vk::CommandBufferAllocateInfo::default()
        .command_pool(command_pool)
        .level(vk::CommandBufferLevel::PRIMARY)
        .command_buffer_count(1);
    let command_buffers = match unsafe { vk.device.allocate_command_buffers(&cb_info) } {
        Ok(cbs) => cbs,
        Err(e) => {
            unsafe { vk.device.destroy_command_pool(command_pool, None) };
            return Err(e);
        }
    };
    let command_buffer = command_buffers[0];

    let staging_size: u64 = u64::from(width) * u64::from(height) * 4;
    let buf_info = vk::BufferCreateInfo::default()
        .size(staging_size)
        .usage(transfer_staging_buffer_usage())
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    let staging_buffer = match unsafe { vk.device.create_buffer(&buf_info, None) } {
        Ok(b) => b,
        Err(e) => {
            unsafe { vk.device.destroy_command_pool(command_pool, None) };
            return Err(e);
        }
    };

    let mem_reqs = unsafe { vk.device.get_buffer_memory_requirements(staging_buffer) };
    let mem_props = unsafe {
        vk.instance
            .get_physical_device_memory_properties(vk.physical_device)
    };
    let want_strict = vk::MemoryPropertyFlags::HOST_VISIBLE
        | vk::MemoryPropertyFlags::HOST_COHERENT
        | vk::MemoryPropertyFlags::DEVICE_LOCAL;
    let want_loose = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
    let memory_type_index = pick_memory_type(&mem_props, mem_reqs.memory_type_bits, want_strict)
        .or_else(|| pick_memory_type(&mem_props, mem_reqs.memory_type_bits, want_loose))
        .ok_or(vk::Result::ERROR_OUT_OF_DEVICE_MEMORY);
    let memory_type_index = match memory_type_index {
        Ok(i) => i,
        Err(e) => {
            unsafe {
                vk.device.destroy_buffer(staging_buffer, None);
                vk.device.destroy_command_pool(command_pool, None);
            }
            return Err(e);
        }
    };

    let alloc_info = vk::MemoryAllocateInfo::default()
        .allocation_size(mem_reqs.size)
        .memory_type_index(memory_type_index);
    let staging_memory = match crate::kms::vk::mem_accounting::allocate_memory(
        &vk.device,
        &alloc_info,
        crate::kms::vk::mem_accounting::MemCategory::Staging,
        &mem_props,
    ) {
        Ok(m) => m,
        Err(e) => {
            unsafe {
                vk.device.destroy_buffer(staging_buffer, None);
                vk.device.destroy_command_pool(command_pool, None);
            }
            return Err(e);
        }
    };
    if let Err(e) = unsafe {
        vk.device
            .bind_buffer_memory(staging_buffer, staging_memory, 0)
    } {
        unsafe {
            crate::kms::vk::mem_accounting::free_memory(&vk.device, staging_memory);
            vk.device.destroy_buffer(staging_buffer, None);
            vk.device.destroy_command_pool(command_pool, None);
        }
        return Err(e);
    }

    let mapped_ptr = match unsafe {
        vk.device
            .map_memory(staging_memory, 0, staging_size, vk::MemoryMapFlags::empty())
    } {
        Ok(p) => p,
        Err(e) => {
            unsafe {
                crate::kms::vk::mem_accounting::free_memory(&vk.device, staging_memory);
                vk.device.destroy_buffer(staging_buffer, None);
                vk.device.destroy_command_pool(command_pool, None);
            }
            return Err(e);
        }
    };
    let staging_mapped =
        std::ptr::NonNull::new(mapped_ptr.cast::<u8>()).expect("vkMapMemory returned non-null");

    // 2-query TIMESTAMP pool for the compose GPU-render timer. Created
    // even if the device lacks timestamp support (creation succeeds
    // regardless); `record_composite_command_buffer` gates use on
    // `vk.timestamp_period > 0.0`. Created last so no earlier error path
    // needs to reap it.
    let timestamp_pool = {
        let info = vk::QueryPoolCreateInfo::default()
            .query_type(vk::QueryType::TIMESTAMP)
            .query_count(2);
        match unsafe { vk.device.create_query_pool(&info, None) } {
            Ok(pool) => pool,
            Err(error) => {
                unsafe {
                    vk.device.unmap_memory(staging_memory);
                    vk.device.destroy_buffer(staging_buffer, None);
                    crate::kms::vk::mem_accounting::free_memory(&vk.device, staging_memory);
                    vk.device.destroy_command_pool(command_pool, None);
                }
                return Err(error);
            }
        }
    };

    Ok(TransferResources {
        command_pool,
        command_buffer,
        staging_buffer,
        staging_memory,
        staging_mapped,
        staging_size,
        timestamp_pool,
        timestamps_written: false,
    })
}

pub(super) fn transfer_staging_buffer_usage() -> vk::BufferUsageFlags {
    // Normal upload/readback code shares this per-BO allocation. Copied
    // compatibility probing additionally writes complete A/B images into the
    // mapping before the CPU validates their content.
    vk::BufferUsageFlags::TRANSFER_SRC | vk::BufferUsageFlags::TRANSFER_DST
}

pub(super) fn destroy_transfer_resources(vk: &VkContext, transfer: &mut TransferResources) {
    unsafe {
        vk.device.unmap_memory(transfer.staging_memory);
        vk.device.destroy_buffer(transfer.staging_buffer, None);
        crate::kms::vk::mem_accounting::free_memory(&vk.device, transfer.staging_memory);
        if transfer.timestamp_pool != vk::QueryPool::null() {
            vk.device.destroy_query_pool(transfer.timestamp_pool, None);
        }
        vk.device.destroy_command_pool(transfer.command_pool, None);
    }
}

fn pick_memory_type(
    props: &vk::PhysicalDeviceMemoryProperties,
    type_bits: u32,
    required: vk::MemoryPropertyFlags,
) -> Option<u32> {
    (0..props.memory_type_count).find(|&i| {
        let candidate = type_bits & (1 << i) != 0;
        candidate
            && props.memory_types[i as usize]
                .property_flags
                .contains(required)
    })
}
