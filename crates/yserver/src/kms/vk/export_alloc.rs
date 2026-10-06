//! GBM allocation of dma-buf-exported backings (#100).
//!
//! A backing exported to another process (GLX texture-from-pixmap over DRI3
//! `BufferFromPixmap(s)`) must be a buffer object the kernel synchronises
//! implicitly. RADV creates every Vulkan-allocated BO with
//! `AMDGPU_GEM_CREATE_EXPLICIT_SYNC`; the importer gets that same GEM object,
//! so its command submissions neither wait for nor publish dma-buf
//! reservation fences, and the write fences yserver imports with
//! `DMA_BUF_IOCTL_IMPORT_SYNC_FILE` are ignored. KWin then samples a window
//! backing before yserver's writes land. GBM's BOs come from the GL driver
//! and carry implicit synchronisation, which is also how the scanout pool
//! allocates (`scanout::allocate_gbm_scanout_image`).
//!
//! The route is chosen per allocation:
//! - [`ExportRoute::GbmModifier`]: a single-plane modifier that Vulkan can
//!   import with [`EXPORT_IMAGE_USAGE`] and GBM can allocate; LINEAR is
//!   preferred when it qualifies (same preference as the Vulkan route; Turnip
//!   needs it for same-GPU coherence).
//! - [`ExportRoute::GbmImplicitLinear`]: no modifier extension, but Vulkan
//!   advertises a `TILING_LINEAR` dma-buf import with that usage; the GBM
//!   linear layout must equal the layout Vulkan computes for the image.
//! - [`ExportRoute::Vulkan`]: everything else (no render node — lavapipe —,
//!   no GBM, or no advertised import combination, e.g. RADV on GFX8). The
//!   historical Vulkan allocation is kept; on amdgpu it stays explicit-sync.

use std::{
    os::fd::{AsFd, IntoRawFd, OwnedFd},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use ash::vk;

use super::{
    device::VkContext,
    dri3::{DRM_FORMAT_MOD_INVALID, DRM_FORMAT_MOD_LINEAR},
    target::{EXPORT_IMAGE_USAGE, ExportableImage},
};

/// How an exportable backing's memory is obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExportRoute {
    GbmModifier,
    GbmImplicitLinear,
    Vulkan,
}

/// Pick the export route from what the renderer offers. Pure so the
/// selection is unit-testable without a device.
#[must_use]
pub(crate) fn select_export_route(
    gbm_available: bool,
    image_drm_format_modifier: bool,
    has_modifier_candidates: bool,
    implicit_linear_importable: bool,
) -> ExportRoute {
    if !gbm_available {
        return ExportRoute::Vulkan;
    }
    if image_drm_format_modifier {
        // With the extension present an implicit-layout import is never
        // needed: if no modifier qualifies, Vulkan cannot express GBM's
        // layout either.
        return if has_modifier_candidates {
            ExportRoute::GbmModifier
        } else {
            ExportRoute::Vulkan
        };
    }
    if implicit_linear_importable {
        ExportRoute::GbmImplicitLinear
    } else {
        ExportRoute::Vulkan
    }
}

/// Modifiers both sides accept for an exported backing: single-plane on the
/// Vulkan side (`vk_importable` = `(modifier, plane_count)` already filtered
/// to IMPORTABLE with [`EXPORT_IMAGE_USAGE`]) and single-plane in GBM
/// (`gbm_planes`). The export reply carries one plane. LINEAR alone when it
/// qualifies, otherwise every common modifier in Vulkan's order (GBM picks).
#[must_use]
pub(crate) fn negotiate_export_modifiers(
    vk_importable: &[(u64, u32)],
    gbm_planes: impl Fn(u64) -> Option<u32>,
) -> Vec<u64> {
    let common: Vec<u64> = vk_importable
        .iter()
        .filter(|&&(modifier, planes)| {
            planes == 1 && modifier != DRM_FORMAT_MOD_INVALID && gbm_planes(modifier) == Some(1)
        })
        .map(|&(modifier, _)| modifier)
        .collect();
    if common.contains(&DRM_FORMAT_MOD_LINEAR) {
        vec![DRM_FORMAT_MOD_LINEAR]
    } else {
        common
    }
}

/// GBM fourcc with the same memory layout as a yserver backing format.
#[must_use]
pub(crate) fn gbm_format_for(format: vk::Format) -> Option<gbm::Format> {
    match format {
        // B8G8R8A8_UNORM bytes in memory are B,G,R,A = DRM ARGB8888 (LE).
        vk::Format::B8G8R8A8_UNORM => Some(gbm::Format::Argb8888),
        vk::Format::R8_UNORM => Some(gbm::Format::R8),
        _ => None,
    }
}

static GBM_FAILURE_LOGGED: AtomicBool = AtomicBool::new(false);
static ROUTE_LOGGED: AtomicBool = AtomicBool::new(false);

/// Allocate an exportable backing through GBM, or `None` to make the caller
/// use the Vulkan allocation. A failed GBM attempt is logged once.
pub(crate) fn allocate_gbm_exportable(
    vk: &Arc<VkContext>,
    width: u32,
    height: u32,
    format: vk::Format,
) -> Option<ExportableImage> {
    let fourcc = gbm_format_for(format)?;
    vk.external_memory_fd.as_ref()?;
    let gbm = vk.export_gbm_device();
    let candidates = match gbm {
        Some(gbm) if vk.image_drm_format_modifier => {
            let vk_side =
                super::dri3::supported_modifiers_with_planes(vk, format, EXPORT_IMAGE_USAGE);
            negotiate_export_modifiers(&vk_side, |modifier| {
                gbm.format_modifier_plane_count(fourcc, gbm::Modifier::from(modifier))
            })
        }
        _ => Vec::new(),
    };
    let implicit_linear =
        gbm.is_some() && !vk.image_drm_format_modifier && linear_tiling_importable(vk, format);
    let route = select_export_route(
        gbm.is_some(),
        vk.image_drm_format_modifier,
        !candidates.is_empty(),
        implicit_linear,
    );
    if !ROUTE_LOGGED.swap(true, Ordering::Relaxed) {
        log::info!(
            "export allocator: route={route:?} format={format:?} candidates={candidates:x?}"
        );
    }
    let gbm = gbm?;
    let result = match route {
        ExportRoute::Vulkan => return None,
        ExportRoute::GbmModifier => allocate_gbm_bo(gbm, width, height, fourcc, Some(&candidates))
            .and_then(|bo| import_gbm_bo(vk, &bo, width, height, format, ImportLayout::Explicit)),
        ExportRoute::GbmImplicitLinear => allocate_gbm_bo(gbm, width, height, fourcc, None)
            .and_then(|bo| {
                import_gbm_bo(vk, &bo, width, height, format, ImportLayout::ImplicitLinear)
            }),
    };
    match result {
        Ok(img) => Some(img),
        Err(e) => {
            if !GBM_FAILURE_LOGGED.swap(true, Ordering::Relaxed) {
                log::warn!(
                    "export allocator: GBM {route:?} allocation {width}x{height} {format:?} \
                     failed ({e}); falling back to a Vulkan-allocated export, which is an \
                     explicit-sync buffer on amdgpu"
                );
            }
            None
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImportLayout {
    Explicit,
    ImplicitLinear,
}

fn allocate_gbm_bo(
    gbm: &gbm::Device<OwnedFd>,
    width: u32,
    height: u32,
    fourcc: gbm::Format,
    modifiers: Option<&[u64]>,
) -> Result<gbm::BufferObject<()>, String> {
    let bo = match modifiers {
        Some(list) => gbm
            .create_buffer_object_with_modifiers2::<()>(
                width,
                height,
                fourcc,
                list.iter().copied().map(gbm::Modifier::from),
                gbm::BufferObjectFlags::RENDERING,
            )
            .map_err(|e| format!("gbm_bo_create_with_modifiers2: {e}"))?,
        None => gbm
            .create_buffer_object::<()>(
                width,
                height,
                fourcc,
                gbm::BufferObjectFlags::RENDERING | gbm::BufferObjectFlags::LINEAR,
            )
            .map_err(|e| format!("gbm_bo_create(LINEAR): {e}"))?,
    };
    let planes = bo.plane_count();
    if planes != 1 {
        return Err(format!("GBM returned {planes} planes"));
    }
    let modifier: u64 = bo.modifier().into();
    match modifiers {
        Some(list) if !list.contains(&modifier) => Err(format!(
            "GBM chose modifier 0x{modifier:x} outside {list:x?}"
        )),
        None if modifier != DRM_FORMAT_MOD_LINEAR && modifier != DRM_FORMAT_MOD_INVALID => Err(
            format!("GBM LINEAR request returned modifier 0x{modifier:x}"),
        ),
        _ => Ok(bo),
    }
}

fn import_gbm_bo(
    vk: &Arc<VkContext>,
    bo: &gbm::BufferObject<()>,
    width: u32,
    height: u32,
    format: vk::Format,
    layout: ImportLayout,
) -> Result<ExportableImage, String> {
    let ext_memory_fd = vk
        .external_memory_fd
        .as_ref()
        .ok_or("VK_KHR_external_memory_fd missing")?;
    let modifier: u64 = match layout {
        ImportLayout::Explicit => bo.modifier().into(),
        ImportLayout::ImplicitLinear => DRM_FORMAT_MOD_LINEAR,
    };
    let stride = bo.stride_for_plane(0);
    let offset = u64::from(bo.offset(0));
    let dmabuf = bo.fd().map_err(|e| format!("gbm_bo_get_fd: {e}"))?;

    let plane_layouts = [vk::SubresourceLayout {
        offset,
        size: 0,
        row_pitch: u64::from(stride),
        array_pitch: 0,
        depth_pitch: 0,
    }];
    let mut explicit = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
        .drm_format_modifier(modifier)
        .plane_layouts(&plane_layouts);
    let mut external = vk::ExternalMemoryImageCreateInfo::default()
        .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let tiling = match layout {
        ImportLayout::Explicit => vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT,
        ImportLayout::ImplicitLinear => vk::ImageTiling::LINEAR,
    };
    let mut image_info = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(format)
        .extent(vk::Extent3D {
            width,
            height,
            depth: 1,
        })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(tiling)
        .usage(EXPORT_IMAGE_USAGE)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .push_next(&mut external);
    if layout == ImportLayout::Explicit {
        image_info = image_info.push_next(&mut explicit);
    }
    let image = unsafe { vk.device.create_image(&image_info, None) }
        .map_err(|e| format!("vkCreateImage(import): {e:?}"))?;
    let destroy_image = |vk: &VkContext| unsafe { vk.device.destroy_image(image, None) };

    let aspect = match layout {
        ImportLayout::Explicit => vk::ImageAspectFlags::MEMORY_PLANE_0_EXT,
        ImportLayout::ImplicitLinear => vk::ImageAspectFlags::COLOR,
    };
    let vk_layout = unsafe {
        vk.device.get_image_subresource_layout(
            image,
            vk::ImageSubresource {
                aspect_mask: aspect,
                mip_level: 0,
                array_layer: 0,
            },
        )
    };
    if !gbm_layout_matches(offset, stride, vk_layout) {
        destroy_image(vk);
        return Err(format!(
            "layout mismatch: gbm(offset={offset}, stride={stride}) vk(offset={}, \
             row_pitch={})",
            vk_layout.offset, vk_layout.row_pitch
        ));
    }

    let vk_fd = match dmabuf.try_clone() {
        Ok(fd) => fd.into_raw_fd(),
        Err(e) => {
            destroy_image(vk);
            return Err(format!("dup dma-buf: {e}"));
        }
    };
    let close_vk_fd = || unsafe {
        libc::close(vk_fd);
    };
    let mem_reqs = unsafe { vk.device.get_image_memory_requirements(image) };
    let mut fd_props = vk::MemoryFdPropertiesKHR::default();
    if let Err(e) = unsafe {
        ext_memory_fd.get_memory_fd_properties(
            vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
            vk_fd,
            &mut fd_props,
        )
    } {
        destroy_image(vk);
        close_vk_fd();
        return Err(format!("vkGetMemoryFdPropertiesKHR: {e:?}"));
    }
    let mem_props = unsafe {
        vk.instance
            .get_physical_device_memory_properties(vk.physical_device)
    };
    let type_bits = mem_reqs.memory_type_bits & fd_props.memory_type_bits;
    let pick = |required: vk::MemoryPropertyFlags| {
        (0..mem_props.memory_type_count).find(|&i| {
            type_bits & (1 << i) != 0
                && mem_props.memory_types[i as usize]
                    .property_flags
                    .contains(required)
        })
    };
    let Some(memory_type_index) = pick(vk::MemoryPropertyFlags::DEVICE_LOCAL)
        .or_else(|| pick(vk::MemoryPropertyFlags::empty()))
    else {
        destroy_image(vk);
        close_vk_fd();
        return Err("no memory type shared by the image and the dma-buf".to_string());
    };
    let mut import_info = vk::ImportMemoryFdInfoKHR::default()
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
        .fd(vk_fd);
    let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
    let alloc_info = vk::MemoryAllocateInfo::default()
        .allocation_size(mem_reqs.size)
        .memory_type_index(memory_type_index)
        .push_next(&mut import_info)
        .push_next(&mut dedicated);
    let memory = match super::mem_accounting::allocate_memory(
        &vk.device,
        &alloc_info,
        super::mem_accounting::MemCategory::TfpExport,
        &mem_props,
    ) {
        Ok(memory) => memory,
        Err(e) => {
            // vkAllocateMemory consumes the fd only on success.
            destroy_image(vk);
            close_vk_fd();
            return Err(format!("vkAllocateMemory(import): {e:?}"));
        }
    };
    if let Err(e) = unsafe { vk.device.bind_image_memory(image, memory, 0) } {
        unsafe { super::mem_accounting::free_memory(&vk.device, memory) };
        destroy_image(vk);
        return Err(format!("vkBindImageMemory: {e:?}"));
    }

    log_amdgpu_bo_sync_once(vk, bo);
    // The dma-buf's size, not the image requirement: the export reply
    // describes the buffer the client imports.
    let size = dmabuf_size(&dmabuf).unwrap_or(mem_reqs.size);
    Ok(ExportableImage::from_imported_parts(
        vk,
        image,
        memory,
        vk::Extent2D { width, height },
        format,
        stride,
        offset,
        size,
        modifier,
        dmabuf,
    ))
}

/// The dma-buf's byte size (`lseek(SEEK_END)`), which dma-bufs support.
fn dmabuf_size(fd: &OwnedFd) -> Option<u64> {
    use std::os::fd::AsRawFd as _;
    let raw = fd.as_fd().as_raw_fd();
    let end = unsafe { libc::lseek(raw, 0, libc::SEEK_END) };
    unsafe { libc::lseek(raw, 0, libc::SEEK_SET) };
    u64::try_from(end).ok().filter(|&n| n > 0)
}

/// Whether Vulkan's layout for the imported image equals the BO's. An
/// explicit-modifier import is created against GBM's numbers, so a mismatch
/// there is a driver disagreement; an implicit linear import must match
/// exactly, since nothing tells Vulkan GBM's pitch.
fn gbm_layout_matches(offset: u64, stride: u32, vk_layout: vk::SubresourceLayout) -> bool {
    vk_layout.offset == offset && vk_layout.row_pitch == u64::from(stride)
}

/// `TILING_LINEAR` counterpart of a modifier import check: does Vulkan
/// advertise importing a linear dma-buf with [`EXPORT_IMAGE_USAGE`]?
fn linear_tiling_importable(vk: &VkContext, format: vk::Format) -> bool {
    let mut external_info = vk::PhysicalDeviceExternalImageFormatInfo::default()
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let format_info = vk::PhysicalDeviceImageFormatInfo2::default()
        .format(format)
        .ty(vk::ImageType::TYPE_2D)
        .tiling(vk::ImageTiling::LINEAR)
        .usage(EXPORT_IMAGE_USAGE)
        .push_next(&mut external_info);
    let mut external_props = vk::ExternalImageFormatProperties::default();
    let mut props2 = vk::ImageFormatProperties2::default().push_next(&mut external_props);
    if super::image_format_properties2(
        vk,
        "export_alloc::linear_tiling_importable",
        None,
        &format_info,
        &mut props2,
    )
    .is_err()
    {
        return false;
    }
    let props = external_props.external_memory_properties;
    props
        .external_memory_features
        .contains(vk::ExternalMemoryFeatureFlags::IMPORTABLE)
        && props
            .compatible_handle_types
            .contains(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
}

/// `AMDGPU_GEM_CREATE_EXPLICIT_SYNC` (`amdgpu_drm.h`).
pub(crate) const AMDGPU_GEM_CREATE_EXPLICIT_SYNC: u64 = 1 << 7;

static AMDGPU_FLAGS_LOGGED: AtomicBool = AtomicBool::new(false);

/// Log, once, the kernel creation flags of the first GBM export BO on
/// amdgpu, so a run shows whether exports are implicitly synchronised
/// without reading debugfs.
fn log_amdgpu_bo_sync_once(vk: &VkContext, bo: &gbm::BufferObject<()>) {
    if vk.driver_id != vk::DriverId::MESA_RADV || AMDGPU_FLAGS_LOGGED.swap(true, Ordering::Relaxed)
    {
        return;
    }
    match amdgpu_bo_create_flags(bo.device_fd(), unsafe { bo.handle().u32_ }) {
        Ok(flags) => log::info!(
            "export allocator: amdgpu export BO flags=0x{flags:x} explicit_sync={}",
            flags & AMDGPU_GEM_CREATE_EXPLICIT_SYNC != 0
        ),
        Err(e) => log::info!("export allocator: amdgpu GEM_OP query failed: {e}"),
    }
}

/// `DRM_IOCTL_AMDGPU_GEM_OP` with `AMDGPU_GEM_OP_GET_GEM_CREATE_INFO`: the
/// `domain_flags` the BO was created with.
#[cfg(target_os = "linux")]
fn amdgpu_bo_create_flags(fd: std::os::fd::BorrowedFd<'_>, handle: u32) -> std::io::Result<u64> {
    use std::os::fd::AsRawFd as _;
    #[repr(C)]
    struct GemOp {
        handle: u32,
        op: u32,
        value: u64,
    }
    #[repr(C)]
    #[derive(Default)]
    struct GemCreateIn {
        bo_size: u64,
        alignment: u64,
        domains: u64,
        domain_flags: u64,
    }
    // _IOWR('d', DRM_COMMAND_BASE + DRM_AMDGPU_GEM_OP = 0x50, 16 bytes).
    const DRM_IOCTL_AMDGPU_GEM_OP: libc::Ioctl = 0xC010_6450_u32 as libc::Ioctl;
    let mut info = GemCreateIn::default();
    let mut op = GemOp {
        handle,
        op: 0, // AMDGPU_GEM_OP_GET_GEM_CREATE_INFO
        value: std::ptr::addr_of_mut!(info) as u64,
    };
    // SAFETY: valid DRM fd, correctly sized request struct; the kernel
    // writes `info` through `value` before returning.
    let rc = unsafe {
        libc::ioctl(
            fd.as_raw_fd(),
            DRM_IOCTL_AMDGPU_GEM_OP,
            std::ptr::addr_of_mut!(op),
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(info.domain_flags)
}

#[cfg(not(target_os = "linux"))]
fn amdgpu_bo_create_flags(_fd: std::os::fd::BorrowedFd<'_>, _handle: u32) -> std::io::Result<u64> {
    Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_without_gbm_is_vulkan() {
        assert_eq!(
            select_export_route(false, true, true, true),
            ExportRoute::Vulkan
        );
    }

    #[test]
    fn route_prefers_gbm_modifier_when_both_sides_agree() {
        assert_eq!(
            select_export_route(true, true, true, false),
            ExportRoute::GbmModifier
        );
    }

    #[test]
    fn route_with_modifier_ext_but_no_common_modifier_is_vulkan() {
        // An implicit linear import is not an option when the extension
        // exists: no common modifier means Vulkan cannot express GBM's layout.
        assert_eq!(
            select_export_route(true, true, false, true),
            ExportRoute::Vulkan
        );
    }

    #[test]
    fn route_without_modifier_ext_needs_an_advertised_linear_import() {
        assert_eq!(
            select_export_route(true, false, false, true),
            ExportRoute::GbmImplicitLinear
        );
        // RADV on GFX8: no modifier extension and no advertised TILING_LINEAR
        // dma-buf import — never import an unsupported combination.
        assert_eq!(
            select_export_route(true, false, false, false),
            ExportRoute::Vulkan
        );
    }

    #[test]
    fn negotiation_prefers_linear_when_both_sides_take_it() {
        let vk_side = [(0x0200_0000_0000_0001, 1), (DRM_FORMAT_MOD_LINEAR, 1)];
        assert_eq!(
            negotiate_export_modifiers(&vk_side, |_| Some(1)),
            vec![DRM_FORMAT_MOD_LINEAR]
        );
    }

    #[test]
    fn negotiation_intersects_single_plane_modifiers_in_vulkan_order() {
        let tiled_a = 0x0200_0000_0000_0001;
        let tiled_b = 0x0200_0000_0000_0002;
        let dcc = 0x0200_0000_0000_0003;
        let gbm_only_two_planes = 0x0200_0000_0000_0004;
        let vk_side = [
            (tiled_b, 1),
            (dcc, 2),
            (tiled_a, 1),
            (gbm_only_two_planes, 1),
            (DRM_FORMAT_MOD_LINEAR, 1),
        ];
        let gbm = |m: u64| match m {
            x if x == gbm_only_two_planes => Some(2),
            x if x == DRM_FORMAT_MOD_LINEAR => None,
            _ => Some(1),
        };
        assert_eq!(
            negotiate_export_modifiers(&vk_side, gbm),
            vec![tiled_b, tiled_a]
        );
    }

    #[test]
    fn negotiation_rejects_invalid_modifier_and_empty_sides() {
        assert!(negotiate_export_modifiers(&[(DRM_FORMAT_MOD_INVALID, 1)], |_| Some(1)).is_empty());
        assert!(negotiate_export_modifiers(&[], |_| Some(1)).is_empty());
        assert!(negotiate_export_modifiers(&[(DRM_FORMAT_MOD_LINEAR, 1)], |_| None).is_empty());
    }

    #[test]
    fn gbm_formats_match_backing_memory_layout() {
        assert_eq!(
            gbm_format_for(vk::Format::B8G8R8A8_UNORM),
            Some(gbm::Format::Argb8888)
        );
        assert_eq!(gbm_format_for(vk::Format::R8_UNORM), Some(gbm::Format::R8));
        assert_eq!(gbm_format_for(vk::Format::R16G16B16A16_SFLOAT), None);
    }

    #[test]
    fn imported_layout_must_equal_gbm_layout() {
        let vk_layout = vk::SubresourceLayout {
            offset: 0,
            size: 0,
            row_pitch: 1280,
            array_pitch: 0,
            depth_pitch: 0,
        };
        assert!(gbm_layout_matches(0, 1280, vk_layout));
        assert!(!gbm_layout_matches(0, 1536, vk_layout));
        assert!(!gbm_layout_matches(64, 1280, vk_layout));
    }

    /// Live check of the #100 fix on whatever GPU the host has: an exported
    /// backing comes from GBM when the renderer has a render node, and on
    /// RADV the kernel BO behind its dma-buf is not explicit-sync. Skips on
    /// renderers without GBM (lavapipe).
    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn live_export_is_gbm_allocated_and_implicit_sync_on_amdgpu() {
        use std::os::fd::AsFd as _;
        let Ok(vk) = VkContext::new() else {
            eprintln!("skip: no Vulkan device");
            return;
        };
        let Some(gbm) = vk.export_gbm_device() else {
            eprintln!("skip: renderer has no GBM render node");
            return;
        };
        let img =
            super::super::target::allocate_exportable(&vk, 64, 32, vk::Format::B8G8R8A8_UNORM)
                .expect("allocate_exportable");
        assert!(img.is_gbm_allocated(), "GBM route available but not taken");
        let export = super::super::dri3::export_backing(&vk, &img).expect("export_backing");
        assert_eq!(export.modifier, img.modifier);
        assert!(export.stride >= 64 * 4);
        if vk.driver_id != vk::DriverId::MESA_RADV {
            return;
        }
        let flags_of = |export: &super::super::dri3::DmabufExport| {
            let bo = gbm
                .import_buffer_object_from_dma_buf::<()>(
                    export.fd.as_fd(),
                    64,
                    32,
                    export.stride,
                    gbm::Format::Argb8888,
                    gbm::BufferObjectFlags::empty(),
                )
                .expect("gbm import of our export");
            amdgpu_bo_create_flags(bo.device_fd(), unsafe { bo.handle().u32_ }).expect("GEM_OP")
        };
        let flags = flags_of(&export);
        assert_eq!(
            flags & AMDGPU_GEM_CREATE_EXPLICIT_SYNC,
            0,
            "exported BO is explicit-sync (flags 0x{flags:x})"
        );
        // The oracle sees the bug: RADV's own export is explicit-sync.
        let old = super::super::target::allocate_exportable_vulkan(
            &vk,
            64,
            32,
            vk::Format::B8G8R8A8_UNORM,
        )
        .expect("allocate_exportable_vulkan");
        let old_export = super::super::dri3::export_backing(&vk, &old).expect("export_backing");
        let old_flags = flags_of(&old_export);
        assert_ne!(
            old_flags & AMDGPU_GEM_CREATE_EXPLICIT_SYNC,
            0,
            "Vulkan-allocated export expected explicit-sync (flags 0x{old_flags:x})"
        );
    }

    #[test]
    fn amdgpu_explicit_sync_flag_matches_uapi() {
        assert_eq!(AMDGPU_GEM_CREATE_EXPLICIT_SYNC, 0x80);
    }
}
