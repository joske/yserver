//! DRI3 Vulkan helpers — modifier query and dma-buf import.
//!
//! Phase 4.2 design §3.2. The headline entry point is
//! [`supported_modifiers`]; [`import_dmabuf`] (Task 10) will follow.

use ash::vk;
use std::{ffi::c_void, sync::Arc};

use super::{
    device::VkContext,
    target::{DrawableImage, DrawableImageError},
};

/// Linear, untagged DRM format modifier (`DRM_FORMAT_MOD_LINEAR`).
pub const DRM_FORMAT_MOD_LINEAR: u64 = 0;
/// "Layout not named." What DRI3 1.0 `PixmapFromBuffer` conveys, and
/// what we must report back for such a pixmap rather than inventing a
/// concrete modifier -- see `Dri3ImportModifier` and #138.
pub const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

/// All DRM format modifiers the driver advertises as importable for
/// `format` as DMA_BUF (single-plane, EXCLUSIVE sharing).
///
/// Algorithm per design §3.2:
/// 1. Query `VkDrmFormatModifierPropertiesListEXT` via
///    `vkGetPhysicalDeviceFormatProperties2` to enumerate every DRM
///    modifier the driver can use with `format`.
/// 2. For each candidate, query
///    `vkGetPhysicalDeviceImageFormatProperties2` with
///    `VkPhysicalDeviceImageDrmFormatModifierInfoEXT` and
///    `VkPhysicalDeviceExternalImageFormatInfo` chained as **siblings**
///    under `VkPhysicalDeviceImageFormatInfo2`. Keep modifiers where
///    the call succeeds and `compatibleHandleTypes` includes
///    `DMA_BUF_BIT_EXT`.
///
/// When `VK_EXT_image_drm_format_modifier` is unavailable (e.g.
/// lavapipe on older Mesa), returns `[DRM_FORMAT_MOD_LINEAR]` so that
/// LINEAR-tiled imports still work. Per design §3.2 the LINEAR
/// modifier is always implicitly supported by the import path —
/// `VK_IMAGE_TILING_LINEAR` plus a plain `VkExternalMemoryImageCreateInfo`
/// chain (no explicit-modifier struct) is the fallback.
///
/// `usage` is the image usage the imported buffer must satisfy, and it
/// materially changes the result on some drivers: Broadcom V3D (v3dv)
/// grants `SAMPLED` only for its tiled (UIF) layout — its texture unit
/// cannot sample a linear 2D image — so a `SAMPLED` probe excludes
/// LINEAR, while a scanout probe (color attachment plus transfer source and
/// destination, never sampled) keeps it. Callers therefore pass the usage that matches how
/// the buffer will actually be used: [`CLIENT_IMPORT_USAGE`] for
/// DRI3 client pixmaps (composited as sampled textures), or the scanout
/// render-target usage for the display buffer.
#[must_use]
pub fn supported_modifiers(
    vk: &VkContext,
    format: vk::Format,
    usage: vk::ImageUsageFlags,
) -> Vec<u64> {
    supported_modifiers_with_planes(vk, format, usage)
        .into_iter()
        .map(|(modifier, _planes)| modifier)
        .collect()
}

/// [`supported_modifiers`], but each entry carries the number of memory
/// planes the layout needs (`drmFormatModifierPlaneCount`).
///
/// Callers that can only handle a single plane must filter on it rather
/// than guess from the modifier bits: AMD's DCC layouts need two planes
/// (three when retiled), and advertising one we then refuse makes the
/// client render nothing at all rather than fall back.
#[must_use]
pub fn supported_modifiers_with_planes(
    vk: &VkContext,
    format: vk::Format,
    usage: vk::ImageUsageFlags,
) -> Vec<(u64, u32)> {
    if !vk.image_drm_format_modifier {
        return vec![(DRM_FORMAT_MOD_LINEAR, 1)];
    }

    let modifier_count = match list_modifier_count(vk, format) {
        Ok(n) if n > 0 => n,
        _ => return Vec::new(),
    };

    let mut props_storage =
        vec![vk::DrmFormatModifierPropertiesEXT::default(); modifier_count as usize];
    let mut list = vk::DrmFormatModifierPropertiesListEXT::default()
        .drm_format_modifier_properties(&mut props_storage);
    let mut format_props = vk::FormatProperties2::default().push_next(&mut list);
    unsafe {
        vk.instance.get_physical_device_format_properties2(
            vk.physical_device,
            format,
            &mut format_props,
        );
    }
    let entries = list.drm_format_modifier_count as usize;

    let mut accepted = Vec::with_capacity(entries);
    for prop in props_storage.iter().take(entries) {
        if can_import_modifier(vk, format, prop.drm_format_modifier, usage) {
            accepted.push((
                prop.drm_format_modifier,
                prop.drm_format_modifier_plane_count,
            ));
        }
    }

    accepted
}

fn list_modifier_count(vk: &VkContext, format: vk::Format) -> Result<u32, vk::Result> {
    let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
    let mut format_props = vk::FormatProperties2::default().push_next(&mut list);
    unsafe {
        vk.instance.get_physical_device_format_properties2(
            vk.physical_device,
            format,
            &mut format_props,
        );
    }
    Ok(list.drm_format_modifier_count)
}

/// Probe whether `(format, modifier)` is importable as a DMA_BUF
/// `VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT` image with the
/// usage flags Phase 4.2 needs.
/// Usage flags a DRI3 *client* buffer import must satisfy: a client
/// pixmap (e.g. a GL/Vulkan app's dma-buf) is composited as a window
/// texture, so it must be `SAMPLED`, and may also be a render/transfer
/// target. `SAMPLED` here is load-bearing — it is what (correctly)
/// excludes LINEAR on v3dv, whose texture unit can't sample a linear
/// layout, steering clients to a tiled modifier.
pub const CLIENT_IMPORT_USAGE: vk::ImageUsageFlags = vk::ImageUsageFlags::from_raw(
    vk::ImageUsageFlags::SAMPLED.as_raw()
        | vk::ImageUsageFlags::TRANSFER_SRC.as_raw()
        | vk::ImageUsageFlags::TRANSFER_DST.as_raw()
        | vk::ImageUsageFlags::COLOR_ATTACHMENT.as_raw(),
);

fn can_import_modifier(
    vk: &VkContext,
    format: vk::Format,
    modifier: u64,
    usage: vk::ImageUsageFlags,
) -> bool {
    // Build the chain manually: format_info.pNext -> external_info,
    // external_info.pNext -> modifier_info, modifier_info.pNext -> null.
    // ash's `push_next` builder isn't implemented for
    // `PhysicalDeviceExternalImageFormatInfo` ↔
    // `PhysicalDeviceImageDrmFormatModifierInfoEXT` (the lifetime
    // generic prevents it). Per design §3.2 we're set up as siblings
    // under format_info, which the driver walks as a flat list.
    let mut modifier_info = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
        .drm_format_modifier(modifier)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);

    let mut external_info = vk::PhysicalDeviceExternalImageFormatInfo::default()
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    external_info.p_next = std::ptr::from_mut(&mut modifier_info).cast::<c_void>();

    let mut format_info = vk::PhysicalDeviceImageFormatInfo2::default()
        .format(format)
        .ty(vk::ImageType::TYPE_2D)
        .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
        .usage(usage);
    format_info.p_next = std::ptr::from_mut(&mut external_info).cast::<c_void>();

    let mut external_props = vk::ExternalImageFormatProperties::default();
    let mut props2 = vk::ImageFormatProperties2::default().push_next(&mut external_props);

    let result = unsafe {
        vk.instance.get_physical_device_image_format_properties2(
            vk.physical_device,
            &format_info,
            &mut props2,
        )
    };

    if result.is_err() {
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

/// DRM format modifiers the driver can **export** as a single-plane
/// DMA_BUF image for `format` with the GLX-TFP export usage
/// ([`super::target::EXPORT_IMAGE_USAGE`]).
///
/// Sibling of [`supported_modifiers`], but for the *export* direction:
/// it requires the `EXPORTABLE` external-memory feature (not merely a
/// compatible handle type) and a single-plane layout, because the TFP
/// export reply carries exactly one plane. Returns an empty vec when
/// `VK_EXT_image_drm_format_modifier` is unavailable or no modifier
/// qualifies — callers then fall back to the LINEAR export path.
#[must_use]
pub fn export_capable_modifiers(vk: &VkContext, format: vk::Format) -> Vec<u64> {
    if !vk.image_drm_format_modifier {
        return Vec::new();
    }

    let modifier_count = match list_modifier_count(vk, format) {
        Ok(n) if n > 0 => n,
        _ => return Vec::new(),
    };

    let mut props_storage =
        vec![vk::DrmFormatModifierPropertiesEXT::default(); modifier_count as usize];
    let mut list = vk::DrmFormatModifierPropertiesListEXT::default()
        .drm_format_modifier_properties(&mut props_storage);
    let mut format_props = vk::FormatProperties2::default().push_next(&mut list);
    unsafe {
        vk.instance.get_physical_device_format_properties2(
            vk.physical_device,
            format,
            &mut format_props,
        );
    }
    let entries = list.drm_format_modifier_count as usize;

    let mut accepted = Vec::with_capacity(entries);
    for prop in props_storage.iter().take(entries) {
        if prop.drm_format_modifier_plane_count == 1
            && can_export_modifier(vk, format, prop.drm_format_modifier)
        {
            accepted.push(prop.drm_format_modifier);
        }
    }
    accepted
}

/// Probe whether `(format, modifier)` can be allocated as a server-owned
/// `VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT` image and **exported** as a
/// DMA_BUF with the GLX-TFP export usage.
///
/// Mirrors [`can_import_modifier`]'s chain construction, but checks the
/// `EXPORTABLE` external-memory feature and uses
/// [`super::target::EXPORT_IMAGE_USAGE`] (which must match the usage the
/// allocation actually requests).
fn can_export_modifier(vk: &VkContext, format: vk::Format, modifier: u64) -> bool {
    let mut modifier_info = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
        .drm_format_modifier(modifier)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);

    let mut external_info = vk::PhysicalDeviceExternalImageFormatInfo::default()
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    external_info.p_next = std::ptr::from_mut(&mut modifier_info).cast::<c_void>();

    let mut format_info = vk::PhysicalDeviceImageFormatInfo2::default()
        .format(format)
        .ty(vk::ImageType::TYPE_2D)
        .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
        .usage(super::target::EXPORT_IMAGE_USAGE);
    format_info.p_next = std::ptr::from_mut(&mut external_info).cast::<c_void>();

    let mut external_props = vk::ExternalImageFormatProperties::default();
    let mut props2 = vk::ImageFormatProperties2::default().push_next(&mut external_props);

    let result = unsafe {
        vk.instance.get_physical_device_image_format_properties2(
            vk.physical_device,
            &format_info,
            &mut props2,
        )
    };

    if result.is_err() {
        return false;
    }
    let props = external_props.external_memory_properties;
    props
        .external_memory_features
        .contains(vk::ExternalMemoryFeatureFlags::EXPORTABLE)
        && props
            .compatible_handle_types
            .contains(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
}

/// One plane of a multi-plane DMA_BUF import. Phase 4.2 only accepts
/// `num_planes == 1` (RGB single-plane); the multi-plane shape is
/// kept on the API so future YCbCr work doesn't need to widen
/// `import_dmabuf`'s signature.
#[derive(Debug, Clone, Copy)]
pub struct DmabufPlane {
    pub offset: u64,
    pub pitch: u32,
}

/// Export a `DrawableImage`'s backing memory as a fresh dma-buf fd.
/// Phase 4.2 design §3.2 export path. Caller owns the returned fd.
///
/// Single-plane only (matches the import path's scope). Returns the
/// row pitch and total memory size needed by the
/// `BufferFromPixmap` reply.
pub fn export_dmabuf(
    vk: &VkContext,
    drawable: &super::target::DrawableImage,
) -> Result<DmabufExport, vk::Result> {
    let ext = vk
        .external_memory_fd
        .as_ref()
        .ok_or(vk::Result::ERROR_EXTENSION_NOT_PRESENT)?;
    // An imported pixmap is the CLIENT's buffer. Hand back that same
    // buffer, with the client's own plane description -- do not launder
    // it through `vkGetMemoryFdKHR` on our VkImage.
    //
    // #138: for a legacy `PixmapFromBuffer` we had to guess a modifier to
    // build that VkImage at all, so re-exporting from it describes the
    // buffer by our guess rather than by what it is. Returning the
    // original fd keeps the buffer's own layout metadata intact, which is
    // what lets the client resolve the layout itself.
    if let super::target::ImageBacking::Imported { dma_buf_fd, .. } = &drawable.backing
        && let Some((stride, offset)) = drawable.import_plane0
    {
        let fd = dma_buf_fd
            .try_clone()
            .map_err(|_| vk::Result::ERROR_OUT_OF_HOST_MEMORY)?;
        // The client's own stated size when the request carried one.
        // Never measured with lseek: this fd shares its open-file
        // description with the client's, so seeking it would move the
        // client's offset, and a failed seek would silently report 0.
        let size = drawable.import_size.unwrap_or_else(|| {
            let layout = unsafe {
                vk.device.get_image_subresource_layout(
                    drawable.vk_image,
                    vk::ImageSubresource {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
                        mip_level: 0,
                        array_layer: 0,
                    },
                )
            };
            u32::try_from(layout.size).unwrap_or(u32::MAX)
        });
        return Ok(DmabufExport {
            fd,
            size,
            stride,
            offset,
            modifier: drawable.drm_modifier.unwrap_or(DRM_FORMAT_MOD_LINEAR),
        });
    }

    let memory = drawable.backing_memory();
    let layout = unsafe {
        vk.device.get_image_subresource_layout(
            drawable.vk_image,
            vk::ImageSubresource {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                mip_level: 0,
                array_layer: 0,
            },
        )
    };
    let size = u32::try_from(layout.size).unwrap_or(u32::MAX);
    let pitch = u32::try_from(layout.row_pitch).unwrap_or(u32::MAX);
    let info = vk::MemoryGetFdInfoKHR::default()
        .memory(memory)
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let raw_fd = unsafe { ext.get_memory_fd(&info)? };
    let fd = super::owned_fd_from_vk(raw_fd, "vkGetMemoryFdKHR(DMA_BUF)")?;
    Ok(DmabufExport {
        fd,
        size,
        stride: pitch,
        offset: u32::try_from(layout.offset).unwrap_or(0),
        // Report the modifier the image was actually imported with.
        //
        // This used to hardcode LINEAR "because DrawableImage does not
        // track its modifier", which is the export half of #138: a
        // client that imported a TILED buffer through the legacy
        // PixmapFromBuffer asked for it back, was told LINEAR, and
        // sampled its own frame as linear. Server-owned images have no
        // client modifier and still answer LINEAR here; they export
        // through export_promoted, which carries the real one.
        modifier: drawable.drm_modifier.unwrap_or(DRM_FORMAT_MOD_LINEAR),
    })
}

#[derive(Debug)]
pub struct DmabufExport {
    pub fd: std::os::fd::OwnedFd,
    pub size: u32,
    pub stride: u32,
    /// Byte offset of plane 0 in the dma-buf. Always 0 for yserver's
    /// single-plane, dedicated-allocation exports; carried so the
    /// `BuffersFromPixmap` (op 8) reply can report it per-plane.
    pub offset: u32,
    /// DRM format modifier of the exported buffer. `DRM_FORMAT_MOD_LINEAR`
    /// (0) for LINEAR-tiled exports; the driver-chosen modifier for the
    /// `DRM_FORMAT_MODIFIER_EXT` path. The client MUST receive this to
    /// import a modifier-tiled buffer correctly — which is why TFP needs
    /// op 8 (op 3's reply has no modifier field).
    pub modifier: u64,
}

/// Export a server-allocated [`super::target::ExportableImage`] as a fresh
/// dma-buf fd.
///
/// Shared by the GLX-TFP promotion path (server-owned exportable images).
/// Requires `VK_KHR_external_memory_fd`; returns `Err` if the extension is
/// absent.
pub fn export_backing(
    vk: &VkContext,
    img: &super::target::ExportableImage,
) -> Result<DmabufExport, vk::Result> {
    let ext = vk
        .external_memory_fd
        .as_ref()
        .ok_or(vk::Result::ERROR_EXTENSION_NOT_PRESENT)?;
    let info = vk::MemoryGetFdInfoKHR::default()
        .memory(img.memory)
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let raw_fd = unsafe { ext.get_memory_fd(&info)? };
    let fd = super::owned_fd_from_vk(raw_fd, "vkGetMemoryFdKHR(DMA_BUF) export_backing")?;
    Ok(DmabufExport {
        fd,
        size: u32::try_from(img.size).unwrap_or(u32::MAX),
        stride: img.stride,
        offset: 0,
        modifier: img.modifier,
    })
}

/// Export a promoted pixmap's exportable memory as a fresh dma-buf fd.
///
/// GLX-TFP (Task 1.2/1.3): once a pixmap has been promoted onto
/// exportable storage, its raw `vk::DeviceMemory` handle lives on the
/// `Storage` (the `ExportableImage` wrapper was decomposed via
/// `into_raw_parts`), and the `stride`/`size` were captured at
/// allocation time. This exports that memory directly without
/// reconstructing an `ExportableImage`. Requires
/// `VK_KHR_external_memory_fd`.
pub fn export_promoted(
    vk: &VkContext,
    memory: vk::DeviceMemory,
    stride: u32,
    size: u64,
    modifier: u64,
) -> Result<DmabufExport, vk::Result> {
    let ext = vk
        .external_memory_fd
        .as_ref()
        .ok_or(vk::Result::ERROR_EXTENSION_NOT_PRESENT)?;
    let info = vk::MemoryGetFdInfoKHR::default()
        .memory(memory)
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let raw_fd = unsafe { ext.get_memory_fd(&info)? };
    let fd = super::owned_fd_from_vk(raw_fd, "vkGetMemoryFdKHR(DMA_BUF) export_promoted")?;
    Ok(DmabufExport {
        fd,
        size: u32::try_from(size).unwrap_or(u32::MAX),
        stride,
        offset: 0,
        modifier,
    })
}

/// Import a client-supplied dma-buf into a `DrawableImage` per design
/// §3.2. Takes ownership of `dma_buf_fd`. On success the fd lifetime
/// is owned by the resulting `DrawableImage`; on failure the OwnedFd
/// drops and closes the fd.
/// [`import_dmabuf`], but recording a *reported* modifier that may differ
/// from the one Vulkan was given.
///
/// They diverge for a legacy `PixmapFromBuffer`: Vulkan needs some
/// concrete modifier to build an image at all, while the honest answer to
/// give a client asking about that pixmap is `DRM_FORMAT_MOD_INVALID`.
pub fn import_dmabuf_reporting(
    vk: Arc<VkContext>,
    dma_buf_fd: std::os::fd::OwnedFd,
    width: u32,
    height: u32,
    format: vk::Format,
    modifier: u64,
    reported_modifier: Option<u64>,
    client_size: Option<u32>,
    planes: &[DmabufPlane],
) -> Result<DrawableImage, DrawableImageError> {
    let mut image = import_dmabuf(vk, dma_buf_fd, width, height, format, modifier, planes)?;
    image.drm_modifier = reported_modifier;
    image.import_size = client_size;
    Ok(image)
}

pub fn import_dmabuf(
    vk: Arc<VkContext>,
    dma_buf_fd: std::os::fd::OwnedFd,
    width: u32,
    height: u32,
    format: vk::Format,
    modifier: u64,
    planes: &[DmabufPlane],
) -> Result<DrawableImage, DrawableImageError> {
    let plane_offsets: Vec<u64> = planes.iter().map(|p| p.offset).collect();
    let plane_pitches: Vec<u32> = planes.iter().map(|p| p.pitch).collect();
    DrawableImage::from_dmabuf(
        vk,
        dma_buf_fd,
        width,
        height,
        format,
        modifier,
        &plane_offsets,
        &plane_pitches,
    )
}

/// Outcome of a dma-buf fence wait via [`wait_dmabuf_read_ready`] or
/// [`wait_dmabuf_write_ready`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DmabufWait {
    /// No fence attached (buffer already idle) — nothing to wait on.
    Idle,
    /// Outstanding fences signalled before the deadline.
    Ready,
    /// Deadline elapsed with the fence still pending — caller proceeds
    /// anyway (a possibly-incomplete frame, never a hang).
    TimedOut,
    /// `DMA_BUF_IOCTL_EXPORT_SYNC_FILE` unsupported / errored — caller
    /// falls back to the prior (no-wait) behaviour.
    Unsupported,
}

/// Backwards-compatible alias kept so external consumers that were
/// already written against `DmabufReadWait` continue to compile.
pub type DmabufReadWait = DmabufWait;

// `struct dma_buf_export_sync_file { __u32 flags; __s32 fd; }`
#[repr(C)]
struct DmaBufExportSyncFile {
    flags: u32,
    fd: i32,
}

// `_IOWR(DMA_BUF_BASE='b', 2, struct dma_buf_export_sync_file)` →
// dir=READ|WRITE(3) size=8 type='b'(0x62) nr=2.
// `libc::Ioctl` is `c_ulong` on glibc, `c_int` on musl (issue #15). This
// request has the high bit set (dir=READ|WRITE), so it overflows a signed
// 32-bit `Ioctl`; cast from the unsigned literal to preserve the bit pattern.
#[cfg(target_os = "linux")]
const DMA_BUF_IOCTL_EXPORT_SYNC_FILE: libc::Ioctl = 0xc008_6202_u32 as libc::Ioctl;
#[cfg(not(target_os = "linux"))]
const DMA_BUF_IOCTL_EXPORT_SYNC_FILE: libc::c_ulong = 0xc008_6202;
const DMA_BUF_SYNC_READ: u32 = 1 << 0;
const DMA_BUF_SYNC_WRITE: u32 = 1 << 1;

/// Outcome of exporting a dma-buf reservation-object fence as a
/// `sync_file` fd (no poll). Distinguishes "nothing to wait on" from
/// "the ioctl is unavailable" so callers can pick a fallback.
pub enum ExportedSyncFile {
    /// No fence attached in the requested scope → nothing to wait on.
    Idle,
    /// `DMA_BUF_IOCTL_EXPORT_SYNC_FILE` unsupported / errored.
    Unsupported,
    /// The exported sync_file fd; caller owns it.
    Fd(std::os::fd::OwnedFd),
}

/// Shared `DMA_BUF_IOCTL_EXPORT_SYNC_FILE` ioctl. `flags` selects the
/// reservation scope (see [`sync_file_export_and_poll`] doc). Returns the
/// owned sync_file fd, or `Idle`/`Unsupported`.
fn export_sync_file_fd(dma_buf_fd: std::os::fd::BorrowedFd<'_>, flags: u32) -> ExportedSyncFile {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    let mut export = DmaBufExportSyncFile { flags, fd: -1 };
    // SAFETY: ioctl on a valid borrowed dma-buf fd with a correctly
    // sized request struct. Returns 0 on success and fills `export.fd`.
    let rc = unsafe {
        libc::ioctl(
            dma_buf_fd.as_raw_fd(),
            DMA_BUF_IOCTL_EXPORT_SYNC_FILE,
            std::ptr::addr_of_mut!(export),
        )
    };
    if rc != 0 {
        return ExportedSyncFile::Unsupported;
    }
    if export.fd < 0 {
        // No fences in the reservation object → buffer is idle.
        return ExportedSyncFile::Idle;
    }
    // Own the returned sync_file fd so it is always closed.
    // SAFETY: the kernel just handed us an owned fd via the ioctl.
    ExportedSyncFile::Fd(unsafe { OwnedFd::from_raw_fd(export.fd) })
}

/// Shared implementation of `DMA_BUF_IOCTL_EXPORT_SYNC_FILE` + `poll`.
///
/// `flags` selects the reservation scope:
/// - [`DMA_BUF_SYNC_READ`]  — snapshot the write fence a reader must wait on.
/// - [`DMA_BUF_SYNC_WRITE`] — snapshot ALL fences (readers + writers) a writer
///   must wait on before overwriting the buffer.
fn sync_file_export_and_poll(
    dma_buf_fd: std::os::fd::BorrowedFd<'_>,
    flags: u32,
    timeout_ms: i32,
) -> DmabufWait {
    use std::os::fd::AsRawFd;

    let sync_fd = match export_sync_file_fd(dma_buf_fd, flags) {
        ExportedSyncFile::Unsupported => return DmabufWait::Unsupported,
        ExportedSyncFile::Idle => return DmabufWait::Idle,
        ExportedSyncFile::Fd(fd) => fd,
    };
    let mut pfd = libc::pollfd {
        fd: sync_fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: single valid pollfd; bounded timeout.
    let pr = unsafe { libc::poll(std::ptr::addr_of_mut!(pfd), 1, timeout_ms) };
    // `sync_fd` drops (closes) here regardless of outcome.
    if pr > 0 && (pfd.revents & libc::POLLIN) != 0 {
        DmabufWait::Ready
    } else {
        DmabufWait::TimedOut
    }
}

/// CPU-wait for a DRI3-imported dma-buf's outstanding producer writes
/// to complete before yserver reads it (e.g. a `PresentPixmap` copy).
///
/// `PresentPixmap` with `wait_fence=0` relies on implicit dma-buf sync;
/// some GPU stacks (Turnip/Adreno, Apple) don't make yserver's read
/// queue honour it, so the copy can race the client's still-pending GPU
/// render and capture a partly-rendered (transparent) frame. This
/// exports the buffer's read fence (`DMA_BUF_IOCTL_EXPORT_SYNC_FILE`,
/// `DMA_BUF_SYNC_READ` → the *write* fence a reader must wait on) and
/// `poll()`s it.
///
/// **Bounded / deadlock-safe:** on `timeout_ms` elapse it returns
/// [`DmabufWait::TimedOut`] and the caller proceeds — worst case a
/// stale frame, never a stall. This remains available for diagnostic and
/// compatibility paths; `PresentPixmap` uses
/// [`export_dmabuf_read_access_sync_file`] and resumes asynchronously from
/// the main loop instead of blocking here.
pub fn wait_dmabuf_read_ready(
    dma_buf_fd: std::os::fd::BorrowedFd<'_>,
    timeout_ms: i32,
) -> DmabufWait {
    sync_file_export_and_poll(dma_buf_fd, DMA_BUF_SYNC_READ, timeout_ms)
}

/// Export the READ-access sync-file for an imported dma-buf without waiting.
/// The returned fence represents the producer write a yserver copy must not
/// overtake. Callers may register the fd in an event loop and submit the copy
/// only after it becomes readable.
pub fn export_dmabuf_read_access_sync_file(
    dma_buf_fd: std::os::fd::BorrowedFd<'_>,
) -> ExportedSyncFile {
    export_sync_file_fd(dma_buf_fd, DMA_BUF_SYNC_READ)
}

/// Export the WRITE-access sync-file for an exported dma-buf without
/// waiting. The returned fence represents all current readers and writers
/// that a subsequent yserver write must not overtake. Callers should import
/// it as a Vulkan wait semaphore on the submission that performs the write.
pub fn export_dmabuf_write_access_sync_file(
    dma_buf_fd: std::os::fd::BorrowedFd<'_>,
) -> ExportedSyncFile {
    export_sync_file_fd(dma_buf_fd, DMA_BUF_SYNC_WRITE)
}

/// CPU-wait until ALL current users (readers AND writers) of an exported
/// dma-buf are done before yserver overwrites it.
///
/// Use this before copying new content into an exported pixmap backing —
/// it guards against overwriting a buffer that a GL consumer (e.g.
/// muffin) is still sampling. `timeout_ms = 0` polls without blocking
/// (non-zero blocks up to that many milliseconds).
///
/// **Bounded / deadlock-safe:** on `timeout_ms` elapse it returns
/// [`DmabufWait::TimedOut`] and the caller proceeds.
pub fn wait_dmabuf_write_ready(
    dma_buf_fd: std::os::fd::BorrowedFd<'_>,
    timeout_ms: i32,
) -> DmabufWait {
    sync_file_export_and_poll(dma_buf_fd, DMA_BUF_SYNC_WRITE, timeout_ms)
}

// `struct dma_buf_import_sync_file { __u32 flags; __s32 fd; }` — 8 bytes.
#[repr(C)]
struct DmaBufImportSyncFile {
    flags: u32,
    fd: i32,
}

// `_IOW(DMA_BUF_BASE='b', 3, struct dma_buf_import_sync_file)` where the struct
// is 8 bytes. dir=WRITE(1), size=8, type='b'(0x62), nr=3:
//   (1<<30) | (8<<16) | (0x62<<8) | 3 = 0x4008_6203
#[cfg(target_os = "linux")]
const DMA_BUF_IOCTL_IMPORT_SYNC_FILE: libc::Ioctl = 0x4008_6203_u32 as libc::Ioctl;
#[cfg(not(target_os = "linux"))]
const DMA_BUF_IOCTL_IMPORT_SYNC_FILE: libc::c_ulong = 0x4008_6203;

/// Attach `sync_fd` (a sync_file representing yserver's completed Vulkan write)
/// onto the dmabuf's reservation object as a WRITE fence.  Mesa's implicit-sync
/// GL read on an imported dmabuf will wait on this fence automatically before
/// sampling the texture.
///
/// The kernel dup()s the fd internally; the caller retains ownership of
/// `sync_fd` and may close it at any point after this call returns.
///
/// # Errors
///
/// Returns `Err(io::ErrorKind::Unsupported)` when the kernel or driver
/// does not support `DMA_BUF_IOCTL_IMPORT_SYNC_FILE` (`ENOTTY` or
/// `EINVAL`).  Any other non-zero ioctl return is propagated as
/// `io::Error::last_os_error()`.
pub fn import_dmabuf_write_fence(
    dmabuf: std::os::fd::BorrowedFd<'_>,
    sync_fd: std::os::fd::BorrowedFd<'_>,
) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    let mut arg = DmaBufImportSyncFile {
        flags: DMA_BUF_SYNC_WRITE,
        fd: sync_fd.as_raw_fd(),
    };
    // SAFETY: ioctl on a valid dma-buf fd with the correct request
    // constant and a properly-sized C struct pointer.  The kernel dup()s
    // `arg.fd` and does not keep a reference to `arg` after the call.
    let rc = unsafe {
        libc::ioctl(
            dmabuf.as_raw_fd(),
            DMA_BUF_IOCTL_IMPORT_SYNC_FILE,
            std::ptr::addr_of_mut!(arg),
        )
    };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        if matches!(err.raw_os_error(), Some(libc::ENOTTY) | Some(libc::EINVAL)) {
            return Err(std::io::Error::from(std::io::ErrorKind::Unsupported));
        }
        return Err(err);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drm_format_mod_linear_is_zero() {
        // Sanity: the LINEAR sentinel really is 0 — many call sites
        // bake this in.
        assert_eq!(DRM_FORMAT_MOD_LINEAR, 0);
    }

    // supported_modifiers() and import_dmabuf() are exercised only
    // against a real VkContext (lavapipe LINEAR-fallback leg + Venus
    // modifier-path leg of the §5.5 hardware coverage matrix), via
    // the vng integration smoke. No unit-test seam without faking
    // the entire instance/device, which the workspace doesn't do.
}
