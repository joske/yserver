//! `DrawableImage` — per-X-resource `VkImage` wrapper for the
//! Vulkan-side scene graph (sub-phase 4.1.3).
//!
//! Spec: docs/superpowers/specs/2026-05-07-phase4-1-vulkan-compositor-design.md
//! §"Per X resource — DrawableImage abstraction".
//!
//! Every X drawable backed by GPU memory (windows + pixmaps) holds a
//! `DrawableImage` alongside its pixman image. Drawing ops still
//! route through pixman in 4.1.3; the result uploads to the
//! corresponding `DrawableImage` on damage. The composite pass reads
//! per-window mirrors instead of a unified shadow buffer (which
//! eliminates the whole-framebuffer memcpy that PixmanShadow incurs
//! today).
//!
//! Two backing variants are defined from day one. Only `ServerOwned`
//! is exercised by 4.1.3-4.1.4 (server-allocated CreateWindow /
//! CreatePixmap mirrors). `Imported` is the slot for Phase 4.2 (DRI3
//! / Present) where clients hand us a dma-buf to scan out — having
//! the variant in the enum from the start means 4.2 doesn't need to
//! retrofit a new constructor onto a sealed type.

use std::sync::Arc;

use ash::vk;

use super::device::VkContext;

/// Per-drawable Vulkan image. Lives as long as the X drawable does;
/// dropped when the client calls `DestroyWindow` / `FreePixmap`.
pub struct DrawableImage {
    pub vk_image: vk::Image,
    /// Color image view bound by the composite pass's descriptor
    /// set. One view per drawable, lifetime matches `vk_image`.
    pub vk_image_view: vk::ImageView,
    /// Lazy view used when this drawable is bound as a *mask* in
    /// the 4.1.4.6 RENDER pipeline. For `B8G8R8A8_UNORM` mirrors
    /// the regular view's `.a` already returns alpha; we use the
    /// same handle. For `R8_UNORM` mirrors we lazily build a
    /// view with component swizzle `(R,R,R,R)` so the shader's
    /// uniform `mask.a` read still returns the alpha-bearing
    /// `R` channel. `None` until the drawable is first sampled
    /// as a mask.
    mask_view: Option<vk::ImageView>,
    /// Lazy view used when this drawable is bound as a *source*
    /// for a picture format with no alpha mask (depth-24 r8g8b8 /
    /// x8r8g8b8 pictures). Component swizzle `(R, G, B, ONE)`,
    /// matching X RENDER's "alpha defaults to 1 when missing".
    /// Built lazily on first such use; only meaningful for BGRA
    /// mirrors. `None` until requested.
    no_alpha_src_view: Option<vk::ImageView>,
    pub extent: vk::Extent2D,
    pub format: vk::Format,
    pub backing: ImageBacking,
    /// Damage accumulated since the last successful upload. Updated
    /// at every drawing-op call site (Task 3.3 marks whole-image
    /// damage; 4.1.4 family ports tighten to per-op rects).
    damage: MirrorDamage,
    /// Layout the image is currently in. Image is created `UNDEFINED`;
    /// after the first damage upload we transition through
    /// `TRANSFER_DST_OPTIMAL` and leave it in `SHADER_READ_ONLY_OPTIMAL`
    /// for the eventual composite-pass read (4.1.3.4 wires the read).
    /// The next upload then transitions back through
    /// `TRANSFER_DST_OPTIMAL`. We track the current layout so the
    /// uploader picks the right `oldLayout`.
    current_layout: vk::ImageLayout,
    /// Held for Drop so the Vulkan handles can be destroyed against
    /// a live device.
    vk: Arc<VkContext>,
}

/// Damage accumulator on the mirror. Tracks either "the whole image
/// is dirty" (set by drawing ops that don't have a tight rect — most
/// of them, in 4.1.3) or a bounding box that grows as rects come in.
/// 4.1.4's per-op family ports replace `mark_full` calls with
/// `mark_rect` calls for the cases where the op knows its damage.
#[derive(Debug, Default, Clone, Copy)]
pub struct MirrorDamage {
    /// `true` when at least one drawing op marked damage without a
    /// rect (or when `mark_full` was called explicitly). Suppresses
    /// per-rect bbox tracking for the rest of the frame; the upload
    /// will copy the whole image.
    fully_dirty: bool,
    /// Bounding box of all marked rects (only meaningful when
    /// `fully_dirty == false` and `bbox != None`).
    bbox: Option<vk::Rect2D>,
}

impl MirrorDamage {
    /// Mark every pixel in the image as needing re-upload. Coarsest
    /// granularity; the safe default for any op that mutates the
    /// pixman image without a known damage rect.
    pub fn mark_full(&mut self) {
        self.fully_dirty = true;
    }

    /// Mark a single rect as dirty, growing the bounding box. No-op
    /// when `fully_dirty` is already set.
    #[allow(dead_code)] // wired in by 4.1.4 per-op family ports.
    pub fn mark_rect(&mut self, rect: vk::Rect2D) {
        if self.fully_dirty || rect.extent.width == 0 || rect.extent.height == 0 {
            return;
        }
        self.bbox = Some(match self.bbox {
            None => rect,
            Some(prev) => union_rect(prev, rect),
        });
    }

    /// Take the accumulated damage as a single rect to upload, if
    /// any. Returns the bbox clamped to `[0..extent.width] ×
    /// [0..extent.height]`, or `None` when nothing was marked.
    /// Clears the accumulator.
    pub fn take(&mut self, extent: vk::Extent2D) -> Option<vk::Rect2D> {
        let was_full = std::mem::take(&mut self.fully_dirty);
        let bbox = self.bbox.take();
        if was_full {
            return Some(vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent,
            });
        }
        let rect = bbox?;
        Some(clamp_rect_to_extent(rect, extent))
    }

    /// Whether anything is dirty.
    #[allow(dead_code)] // exposed for callers that may want to skip uploads early.
    pub fn is_dirty(&self) -> bool {
        self.fully_dirty || self.bbox.is_some()
    }
}

fn union_rect(a: vk::Rect2D, b: vk::Rect2D) -> vk::Rect2D {
    let ax0 = a.offset.x;
    let ay0 = a.offset.y;
    let ax1 = a.offset.x.saturating_add_unsigned(a.extent.width);
    let ay1 = a.offset.y.saturating_add_unsigned(a.extent.height);
    let bx0 = b.offset.x;
    let by0 = b.offset.y;
    let bx1 = b.offset.x.saturating_add_unsigned(b.extent.width);
    let by1 = b.offset.y.saturating_add_unsigned(b.extent.height);
    let x0 = ax0.min(bx0);
    let y0 = ay0.min(by0);
    let x1 = ax1.max(bx1);
    let y1 = ay1.max(by1);
    vk::Rect2D {
        offset: vk::Offset2D { x: x0, y: y0 },
        extent: vk::Extent2D {
            width: u32::try_from((x1 - x0).max(0)).unwrap_or(0),
            height: u32::try_from((y1 - y0).max(0)).unwrap_or(0),
        },
    }
}

fn clamp_rect_to_extent(rect: vk::Rect2D, extent: vk::Extent2D) -> vk::Rect2D {
    let x0 = rect.offset.x.max(0);
    let y0 = rect.offset.y.max(0);
    let max_x = i32::try_from(extent.width).unwrap_or(i32::MAX);
    let max_y = i32::try_from(extent.height).unwrap_or(i32::MAX);
    let x1 = rect
        .offset
        .x
        .saturating_add_unsigned(rect.extent.width)
        .clamp(0, max_x);
    let y1 = rect
        .offset
        .y
        .saturating_add_unsigned(rect.extent.height)
        .clamp(0, max_y);
    vk::Rect2D {
        offset: vk::Offset2D { x: x0, y: y0 },
        extent: vk::Extent2D {
            width: u32::try_from((x1 - x0).max(0)).unwrap_or(0),
            height: u32::try_from((y1 - y0).max(0)).unwrap_or(0),
        },
    }
}

/// What's behind the `VkImage`.
#[allow(dead_code)] // Imported variant exercised by Phase 4.2 (DRI3).
pub enum ImageBacking {
    /// We allocated the memory ourselves. This is the case for every
    /// CreateWindow / CreatePixmap mirror through Phase 4.1.5.
    ServerOwned { vk_memory: vk::DeviceMemory },
    /// Memory was imported from a client-supplied dma-buf (DRI3).
    /// Phase 4.2 territory; the field shape is fixed now so 4.2's
    /// constructor can drop into the existing enum without the
    /// invasive refactor of a sealed type.
    Imported {
        dma_buf_fd: std::os::fd::OwnedFd,
        vk_memory: vk::DeviceMemory,
    },
}

/// Errors during `DrawableImage` creation.
#[derive(Debug, thiserror::Error)]
#[allow(dead_code)] // raised by Task 3.2 once mirrors are allocated per-window.
pub enum DrawableImageError {
    #[error("vulkan: {0}")]
    Vk(vk::Result),
    #[error("no memory type matches both image and DMA-BUF fd requirements")]
    NoMemoryType,
    #[error("missing required Vulkan extension `{0}`")]
    MissingExtension(&'static str),
    #[error("non-explicit linear DMA-BUF import requires exactly one plane, got {0}")]
    NonExplicitLinearPlaneCount(usize),
    #[error("a non-linear DMA-BUF requires explicit DRM modifier layout support")]
    NonLinearWithoutModifierSupport,
    #[error(
        "linear DMA-BUF layout mismatch: supplied offset={supplied_offset} pitch={supplied_pitch}, \
         Vulkan requires offset={required_offset} pitch={required_pitch}"
    )]
    LinearLayoutMismatch {
        supplied_offset: u64,
        supplied_pitch: u64,
        required_offset: u64,
        required_pitch: u64,
    },
}

impl From<vk::Result> for DrawableImageError {
    fn from(r: vk::Result) -> Self {
        DrawableImageError::Vk(r)
    }
}

fn intersect_import_memory_type_bits(image_bits: u32, fd_bits: u32) -> u32 {
    image_bits & fd_bits
}

fn validate_non_explicit_linear_layout(
    plane_offsets: &[u64],
    plane_pitches: &[u32],
    required: vk::SubresourceLayout,
) -> Result<(), DrawableImageError> {
    if plane_offsets.len() != 1 || plane_pitches.len() != 1 {
        return Err(DrawableImageError::NonExplicitLinearPlaneCount(
            plane_offsets.len(),
        ));
    }
    let supplied_offset = plane_offsets[0];
    let supplied_pitch = u64::from(plane_pitches[0]);
    if supplied_offset != required.offset || supplied_pitch != required.row_pitch {
        return Err(DrawableImageError::LinearLayoutMismatch {
            supplied_offset,
            supplied_pitch,
            required_offset: required.offset,
            required_pitch: required.row_pitch,
        });
    }
    Ok(())
}

impl DrawableImage {
    /// Allocate a server-owned mirror for an X window. Format is
    /// fixed `B8G8R8A8_UNORM` (matches the rest of the
    /// Phase 4.1 surface palette and pixman's `X8R8G8B8` shadow
    /// layout). Memory is bound directly (one device allocation per
    /// drawable for now; a pool allocator can amortize later if
    /// session window counts ever push us toward
    /// `maxMemoryAllocationCount`).
    #[allow(dead_code)] // called from Task 3.2 (per-window mirror creation).
    pub fn new_server_owned_window(
        vk: Arc<VkContext>,
        width: u32,
        height: u32,
    ) -> Result<Self, DrawableImageError> {
        Self::new_server_owned(vk, width, height, vk::Format::B8G8R8A8_UNORM)
    }

    /// Map an X11 pixmap depth to its server-owned mirror format.
    /// Used by both [`Self::new_server_owned_pixmap`] (for fresh
    /// allocation) and `KmsBackend::allocate_pixmap_mirror`'s pool-key
    /// derivation, so the two sites cannot drift.
    #[must_use]
    pub fn format_for_pixmap_depth(depth: u8) -> vk::Format {
        match depth {
            1 | 8 => vk::Format::R8_UNORM,
            24 | 32 => vk::Format::B8G8R8A8_UNORM,
            other => {
                log::warn!(
                    "DrawableImage::format_for_pixmap_depth: unhandled depth {other} → \
                     defaulting to B8G8R8A8_UNORM (4.1.4 should fix the format-mapping table)"
                );
                vk::Format::B8G8R8A8_UNORM
            }
        }
    }

    /// Allocate a server-owned mirror for an X pixmap. Format is
    /// derived from the pixmap's depth (only depths 1, 8, 24, 32 are
    /// in scope today, matching pixman's `FormatCode` set).
    #[allow(dead_code)] // called from Task 3.2 (per-pixmap mirror creation).
    pub fn new_server_owned_pixmap(
        vk: Arc<VkContext>,
        width: u32,
        height: u32,
        depth: u8,
    ) -> Result<Self, DrawableImageError> {
        let format = Self::format_for_pixmap_depth(depth);
        Self::new_server_owned(vk, width, height, format)
    }

    /// Import a client-supplied dma-buf (DRI3) as the backing of a
    /// drawable. Phase 4.2 design §3.2.
    ///
    /// **fd ownership rule.** This function takes ownership of
    /// `dma_buf_fd`, duplicates it for `vkAllocateMemory`, and retains the
    /// original in the resulting [`ImageBacking::Imported`]. Vulkan consumes
    /// the duplicate only when allocation succeeds. Every failure path closes
    /// both copies; successful drop frees the imported memory and closes the
    /// retained original.
    pub fn from_dmabuf(
        vk: Arc<VkContext>,
        dma_buf_fd: std::os::fd::OwnedFd,
        width: u32,
        height: u32,
        format: vk::Format,
        modifier: u64,
        plane_offsets: &[u64],
        plane_pitches: &[u32],
    ) -> Result<Self, DrawableImageError> {
        Self::from_dmabuf_with_usage(
            vk,
            dma_buf_fd,
            width,
            height,
            format,
            modifier,
            plane_offsets,
            plane_pitches,
            vk::ImageUsageFlags::SAMPLED
                | vk::ImageUsageFlags::TRANSFER_SRC
                | vk::ImageUsageFlags::TRANSFER_DST
                | vk::ImageUsageFlags::COLOR_ATTACHMENT,
        )
    }

    /// Import a DMA-BUF with an exact Vulkan usage contract.
    ///
    /// Copied scanout uses this narrower entry point so its sink GPU only has
    /// to accept the renderer's foreign image as a transfer source, rather
    /// than also accepting irrelevant sampling, attachment, and destination
    /// usages.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_dmabuf_with_usage(
        vk: Arc<VkContext>,
        dma_buf_fd: std::os::fd::OwnedFd,
        width: u32,
        height: u32,
        format: vk::Format,
        modifier: u64,
        plane_offsets: &[u64],
        plane_pitches: &[u32],
        usage: vk::ImageUsageFlags,
    ) -> Result<Self, DrawableImageError> {
        use std::os::fd::IntoRawFd as _;
        if plane_offsets.len() != plane_pitches.len() {
            return Err(DrawableImageError::Vk(
                vk::Result::ERROR_INITIALIZATION_FAILED,
            ));
        }
        if plane_offsets.is_empty() || plane_offsets.len() > 4 {
            return Err(DrawableImageError::Vk(
                vk::Result::ERROR_INITIALIZATION_FAILED,
            ));
        }
        let external_memory_fd =
            vk.external_memory_fd
                .as_ref()
                .ok_or(DrawableImageError::MissingExtension(
                    "VK_KHR_external_memory_fd",
                ))?;

        // Use the explicit-modifier path whenever the extension is
        // available — including LINEAR. VK_IMAGE_TILING_LINEAR alone
        // makes the driver compute its own row pitch from format/width
        // and ignores the client-supplied stride, which silently
        // corrupts imports whose stride doesn't match the driver's
        // alignment (e.g. Mesa anv allocates 300×BGRA8888 with
        // stride=1280, not the 1200 a tight LINEAR layout would use).
        let use_explicit_modifier = vk.image_drm_format_modifier;
        if !use_explicit_modifier && modifier != super::dri3::DRM_FORMAT_MOD_LINEAR {
            return Err(DrawableImageError::NonLinearWithoutModifierSupport);
        }

        // Build VkSubresourceLayout array for the explicit-modifier chain
        // (only used when use_explicit_modifier is true).
        let plane_layouts: Vec<vk::SubresourceLayout> = plane_offsets
            .iter()
            .zip(plane_pitches.iter())
            .map(|(&offset, &pitch)| vk::SubresourceLayout {
                offset,
                size: 0,
                row_pitch: u64::from(pitch),
                array_pitch: 0,
                depth_pitch: 0,
            })
            .collect();
        let mut modifier_info = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
            .drm_format_modifier(modifier)
            .plane_layouts(&plane_layouts);

        let mut external_info = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        if use_explicit_modifier {
            external_info.p_next =
                std::ptr::from_mut(&mut modifier_info).cast::<std::ffi::c_void>();
        }

        let tiling = if use_explicit_modifier {
            vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT
        } else {
            vk::ImageTiling::LINEAR
        };

        let image_info = vk::ImageCreateInfo::default()
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
            .usage(usage)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut external_info);

        let image = unsafe { vk.device.create_image(&image_info, None)? };

        // Without explicit modifier layout, Vulkan chooses the LINEAR image's
        // offset and row pitch. Importing a DMA-BUF whose supplied layout does
        // not exactly match those requirements would silently reinterpret its
        // rows. Validate the concrete image before binding memory. This is a
        // runtime compatibility check, not the later copied-sink policy gate
        // that requires explicit layout metadata up front.
        if !use_explicit_modifier {
            let required = unsafe {
                vk.device.get_image_subresource_layout(
                    image,
                    vk::ImageSubresource {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
                        mip_level: 0,
                        array_layer: 0,
                    },
                )
            };
            if let Err(error) =
                validate_non_explicit_linear_layout(plane_offsets, plane_pitches, required)
            {
                unsafe { vk.device.destroy_image(image, None) };
                return Err(error);
            }
        }

        // Per design §3.2 fd ownership rule: dup the fd before handing
        // to vkAllocateMemory so that on any vkAllocateMemory failure
        // we can close ours and let the caller's `dma_buf_fd` drop
        // separately. On vkAllocateMemory SUCCESS the dup'd fd is
        // owned by the VkDeviceMemory; `into_raw_fd` releases the
        // OwnedFd's drop responsibility.
        let server_fd_owned = dma_buf_fd.try_clone().map_err(|_| {
            unsafe { vk.device.destroy_image(image, None) };
            DrawableImageError::Vk(vk::Result::ERROR_OUT_OF_HOST_MEMORY)
        })?;
        let server_fd_raw = server_fd_owned.into_raw_fd();

        let mut import_info = vk::ImportMemoryFdInfoKHR::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
            .fd(server_fd_raw);

        let mem_reqs = unsafe { vk.device.get_image_memory_requirements(image) };
        let mut fd_properties = vk::MemoryFdPropertiesKHR::default();
        if let Err(error) = unsafe {
            external_memory_fd.get_memory_fd_properties(
                vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
                server_fd_raw,
                &mut fd_properties,
            )
        } {
            unsafe {
                vk.device.destroy_image(image, None);
                libc::close(server_fd_raw);
            }
            return Err(error.into());
        }
        let mem_props = unsafe {
            vk.instance
                .get_physical_device_memory_properties(vk.physical_device)
        };
        let compatible_type_bits = intersect_import_memory_type_bits(
            mem_reqs.memory_type_bits,
            fd_properties.memory_type_bits,
        );
        let memory_type_index = pick_memory_type(
            &mem_props,
            compatible_type_bits,
            vk::MemoryPropertyFlags::empty(),
        );
        let memory_type_index = match memory_type_index {
            Some(i) => i,
            None => {
                unsafe {
                    vk.device.destroy_image(image, None);
                    libc::close(server_fd_raw);
                }
                return Err(DrawableImageError::NoMemoryType);
            }
        };
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let alloc_info = vk::MemoryAllocateInfo::default()
            .allocation_size(mem_reqs.size)
            .memory_type_index(memory_type_index)
            .push_next(&mut import_info)
            .push_next(&mut dedicated);
        let memory = match unsafe { vk.device.allocate_memory(&alloc_info, None) } {
            Ok(m) => m,
            Err(e) => {
                unsafe {
                    vk.device.destroy_image(image, None);
                    // vkAllocateMemory consumed the fd only on SUCCESS.
                    libc::close(server_fd_raw);
                }
                return Err(e.into());
            }
        };
        // server_fd_raw is now owned by `memory`. Caller's
        // `dma_buf_fd` is still owned by the OwnedFd we received; it
        // will be re-stashed on the Imported variant so the original
        // fd survives at least until DrawableImage drop.
        if let Err(e) = unsafe { vk.device.bind_image_memory(image, memory, 0) } {
            unsafe {
                vk.device.free_memory(memory, None);
                vk.device.destroy_image(image, None);
            }
            return Err(e.into());
        }

        let view_info = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(format)
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .level_count(1)
                    .layer_count(1),
            );
        let view = match unsafe { vk.device.create_image_view(&view_info, None) } {
            Ok(v) => v,
            Err(e) => {
                unsafe {
                    vk.device.free_memory(memory, None);
                    vk.device.destroy_image(image, None);
                }
                return Err(e.into());
            }
        };

        Ok(Self {
            vk_image: image,
            vk_image_view: view,
            mask_view: None,
            no_alpha_src_view: None,
            extent: vk::Extent2D { width, height },
            format,
            backing: ImageBacking::Imported {
                dma_buf_fd,
                vk_memory: memory,
            },
            damage: MirrorDamage::default(),
            current_layout: vk::ImageLayout::UNDEFINED,
            vk,
        })
    }

    fn new_server_owned(
        vk: Arc<VkContext>,
        width: u32,
        height: u32,
        format: vk::Format,
    ) -> Result<Self, DrawableImageError> {
        let image_info = vk::ImageCreateInfo::default()
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
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(
                vk::ImageUsageFlags::COLOR_ATTACHMENT
                    | vk::ImageUsageFlags::TRANSFER_DST
                    | vk::ImageUsageFlags::TRANSFER_SRC
                    | vk::ImageUsageFlags::SAMPLED,
            )
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let image = unsafe { vk.device.create_image(&image_info, None)? };

        let mem_reqs = unsafe { vk.device.get_image_memory_requirements(image) };
        let mem_props = unsafe {
            vk.instance
                .get_physical_device_memory_properties(vk.physical_device)
        };
        let memory_type_index = pick_memory_type(
            &mem_props,
            mem_reqs.memory_type_bits,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
        )
        .ok_or(DrawableImageError::NoMemoryType);
        let memory_type_index = match memory_type_index {
            Ok(i) => i,
            Err(e) => {
                unsafe { vk.device.destroy_image(image, None) };
                return Err(e);
            }
        };
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let alloc_info = vk::MemoryAllocateInfo::default()
            .allocation_size(mem_reqs.size)
            .memory_type_index(memory_type_index)
            .push_next(&mut dedicated);
        let memory = match unsafe { vk.device.allocate_memory(&alloc_info, None) } {
            Ok(m) => m,
            Err(e) => {
                unsafe { vk.device.destroy_image(image, None) };
                return Err(e.into());
            }
        };
        if let Err(e) = unsafe { vk.device.bind_image_memory(image, memory, 0) } {
            unsafe {
                vk.device.free_memory(memory, None);
                vk.device.destroy_image(image, None);
            }
            return Err(e.into());
        }

        let view_info = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(format)
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .level_count(1)
                    .layer_count(1),
            );
        let view = match unsafe { vk.device.create_image_view(&view_info, None) } {
            Ok(v) => v,
            Err(e) => {
                unsafe {
                    vk.device.free_memory(memory, None);
                    vk.device.destroy_image(image, None);
                }
                return Err(e.into());
            }
        };

        Ok(Self {
            vk_image: image,
            vk_image_view: view,
            mask_view: None,
            no_alpha_src_view: None,
            extent: vk::Extent2D { width, height },
            format,
            backing: ImageBacking::ServerOwned { vk_memory: memory },
            damage: MirrorDamage::default(),
            current_layout: vk::ImageLayout::UNDEFINED,
            vk,
        })
    }

    /// Backing `VkDeviceMemory` handle. Used by the DRI3 export path
    /// (Task 13) to call `vkGetMemoryFdKHR` against either the
    /// server-allocated memory (ServerOwned) or the previously
    /// imported memory (Imported). Both variants carry the field;
    /// callers don't need to know which.
    pub fn backing_memory(&self) -> vk::DeviceMemory {
        match &self.backing {
            ImageBacking::ServerOwned { vk_memory } => *vk_memory,
            ImageBacking::Imported { vk_memory, .. } => *vk_memory,
        }
    }

    /// Borrow the client-supplied dma-buf fd for a DRI3-imported image,
    /// or `None` for server-owned storage. Used to synchronize a read
    /// (e.g. a PresentPixmap copy) against the producing client's
    /// outstanding GPU writes via the buffer's implicit fence —
    /// `DMA_BUF_IOCTL_EXPORT_SYNC_FILE`. Server-owned images are
    /// ordered by yserver's own queue barriers and need no such wait.
    pub fn imported_dma_buf_fd(&self) -> Option<std::os::fd::BorrowedFd<'_>> {
        use std::os::fd::AsFd;
        match &self.backing {
            ImageBacking::Imported { dma_buf_fd, .. } => Some(dma_buf_fd.as_fd()),
            ImageBacking::ServerOwned { .. } => None,
        }
    }

    /// Image view to bind when this BGRA mirror is sampled as a
    /// *source* for a picture format with no alpha mask (depth-24
    /// r8g8b8 / x8r8g8b8 pictures). The X RENDER spec says missing
    /// alpha defaults to 1; the swizzle `(R, G, B, ONE)` enforces
    /// that at sample time without per-pipeline shader changes.
    /// For `R8_UNORM` mirrors callers should use [`mask_image_view`]
    /// instead — R8 is alpha-only by definition.
    pub fn no_alpha_src_image_view(&mut self) -> Result<vk::ImageView, vk::Result> {
        if let Some(v) = self.no_alpha_src_view {
            return Ok(v);
        }
        let view_info = vk::ImageViewCreateInfo::default()
            .image(self.vk_image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(self.format)
            .components(vk::ComponentMapping {
                r: vk::ComponentSwizzle::IDENTITY,
                g: vk::ComponentSwizzle::IDENTITY,
                b: vk::ComponentSwizzle::IDENTITY,
                a: vk::ComponentSwizzle::ONE,
            })
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .level_count(1)
                    .layer_count(1),
            );
        let view = unsafe { self.vk.device.create_image_view(&view_info, None)? };
        self.no_alpha_src_view = Some(view);
        Ok(view)
    }

    /// Image view to bind when this mirror is sampled as a *mask*
    /// in the 4.1.4.6 RENDER pipeline. For `B8G8R8A8_UNORM` mirrors
    /// the regular view's `.a` is already alpha; we return the same
    /// handle. For `R8_UNORM` mirrors we lazily create a swizzled
    /// view (`a = R`) so the shader can uniformly read `mask.a`
    /// without knowing the underlying format. Built once per
    /// mirror; reused for every Composite that uses it as mask.
    pub fn mask_image_view(&mut self) -> Result<vk::ImageView, vk::Result> {
        if self.format == vk::Format::B8G8R8A8_UNORM {
            return Ok(self.vk_image_view);
        }
        if let Some(v) = self.mask_view {
            return Ok(v);
        }
        let view_info = vk::ImageViewCreateInfo::default()
            .image(self.vk_image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(self.format)
            .components(vk::ComponentMapping {
                r: vk::ComponentSwizzle::ZERO,
                g: vk::ComponentSwizzle::ZERO,
                b: vk::ComponentSwizzle::ZERO,
                a: vk::ComponentSwizzle::R,
            })
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .level_count(1)
                    .layer_count(1),
            );
        let view = unsafe { self.vk.device.create_image_view(&view_info, None)? };
        self.mask_view = Some(view);
        Ok(view)
    }

    /// Mark every pixel as dirty. Called at every pixman drawing-op
    /// site that mutates this drawable (Task 3.3 shim — 4.1.4 per-op
    /// ports replace this with rect-precise [`Self::mark_damage_rect`]
    /// where they can).
    pub fn mark_full_damage(&mut self) {
        self.damage.mark_full();
    }

    /// Mark a sub-rect as dirty.
    #[allow(dead_code)] // wired in by 4.1.4 per-op family ports.
    pub fn mark_damage_rect(&mut self, rect: vk::Rect2D) {
        self.damage.mark_rect(rect);
    }

    /// Pop the accumulated damage as a rect to upload, or `None` if
    /// the mirror is clean. Clears the accumulator.
    pub fn take_damage(&mut self) -> Option<vk::Rect2D> {
        self.damage.take(self.extent)
    }

    /// Whether the mirror has unflushed pixman-side writes.
    /// Currently only used internally; the 4.1.4.6 RENDER path
    /// no longer gates on it (see backend.rs `ensure_drawable_mirror_sampleable`).
    pub fn is_dirty(&self) -> bool {
        self.damage.is_dirty()
    }

    /// Run a one-shot CB that clears the freshly-created mirror to
    /// `(0, 0, 0, 0)` and transitions it to
    /// `SHADER_READ_ONLY_OPTIMAL`. Called by `KmsBackend` right
    /// after each mirror is constructed so the very first
    /// `RenderComposite` sample sees defined contents (zeros)
    /// instead of UB.
    ///
    /// `pool` is the backend's drawing-op command pool.
    pub fn initialize_clear(&mut self, pool: vk::CommandPool) -> Result<(), vk::Result> {
        let alloc_info = vk::CommandBufferAllocateInfo::default()
            .command_pool(pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        let cb = unsafe { self.vk.device.allocate_command_buffers(&alloc_info)?[0] };

        let result = (|| -> Result<(), vk::Result> {
            let device = &self.vk.device;
            let begin = vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
            crate::vk_count!(begin_command_buffer);
            unsafe { device.begin_command_buffer(cb, &begin)? };

            let to_dst = [vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                .src_access_mask(vk::AccessFlags2::empty())
                .dst_stage_mask(vk::PipelineStageFlags2::CLEAR)
                .dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .image(self.vk_image)
                .subresource_range(
                    vk::ImageSubresourceRange::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR)
                        .level_count(1)
                        .layer_count(1),
                )];
            let dep = vk::DependencyInfo::default().image_memory_barriers(&to_dst);
            crate::vk_count!(cmd_pipeline_barrier2);
            unsafe { device.cmd_pipeline_barrier2(cb, &dep) };

            let clear_color = vk::ClearColorValue {
                float32: [0.0, 0.0, 0.0, 0.0],
            };
            let ranges = [vk::ImageSubresourceRange::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .level_count(1)
                .layer_count(1)];
            unsafe {
                crate::vk_count!(cmd_clear_color_image);
                device.cmd_clear_color_image(
                    cb,
                    self.vk_image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &clear_color,
                    &ranges,
                );
            }

            let to_read = [vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::CLEAR)
                .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                .dst_stage_mask(vk::PipelineStageFlags2::FRAGMENT_SHADER)
                .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .image(self.vk_image)
                .subresource_range(
                    vk::ImageSubresourceRange::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR)
                        .level_count(1)
                        .layer_count(1),
                )];
            let dep = vk::DependencyInfo::default().image_memory_barriers(&to_read);
            crate::vk_count!(cmd_pipeline_barrier2);
            unsafe { device.cmd_pipeline_barrier2(cb, &dep) };

            crate::vk_count!(end_command_buffer);
            unsafe { device.end_command_buffer(cb)? };

            let cb_info = [vk::CommandBufferSubmitInfo::default().command_buffer(cb)];
            let submit = [vk::SubmitInfo2::default().command_buffer_infos(&cb_info)];
            unsafe {
                crate::vk_count!(queue_submit2);
                crate::vk_count!(submit_other);
                device.queue_submit2(self.vk.graphics_queue, &submit, vk::Fence::null())?;
                device.queue_wait_idle(self.vk.graphics_queue)?;
            }
            Ok(())
        })();

        unsafe { self.vk.device.free_command_buffers(pool, &[cb]) };
        if result.is_ok() {
            self.current_layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
        }
        result
    }

    /// Current layout of the image. Bumped by upload paths after
    /// they land a transition.
    pub fn current_layout(&self) -> vk::ImageLayout {
        self.current_layout
    }

    /// Set the tracked layout. The upload code path calls this after
    /// recording the matching `vkCmdPipelineBarrier2`.
    pub fn set_current_layout(&mut self, layout: vk::ImageLayout) {
        self.current_layout = layout;
    }

    /// Record into `cb` the barriers + copy that upload `rect` from a
    /// host-visible staging buffer into this image. Caller has
    /// already memcpy'd the rect's pixel rows into staging starting
    /// at `staging_offset_bytes`. Buffer rows in staging are tightly
    /// packed (no row padding); `rect.extent.width × bpp` bytes per
    /// row.
    ///
    /// The final layout is `SHADER_READ_ONLY_OPTIMAL` — the layout
    /// the composite pass (4.1.3.4) will sample from.
    pub fn record_upload_rect(
        &mut self,
        cb: vk::CommandBuffer,
        staging_buffer: vk::Buffer,
        staging_offset_bytes: u64,
        rect: vk::Rect2D,
    ) {
        let device = &self.vk.device;
        let old_layout = self.current_layout;

        // Transition into TRANSFER_DST_OPTIMAL.
        let to_dst = vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
            .src_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
            .dst_stage_mask(vk::PipelineStageFlags2::COPY)
            .dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
            .old_layout(old_layout)
            .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .image(self.vk_image)
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .level_count(1)
                    .layer_count(1),
            );
        let to_dst_arr = [to_dst];
        let to_dst_dep = vk::DependencyInfo::default().image_memory_barriers(&to_dst_arr);
        crate::vk_count!(cmd_pipeline_barrier2);
        unsafe { device.cmd_pipeline_barrier2(cb, &to_dst_dep) };

        // Copy staging buffer rect → image.
        let copy_region = vk::BufferImageCopy::default()
            .buffer_offset(staging_offset_bytes)
            .buffer_row_length(0) // tightly packed in staging
            .buffer_image_height(0)
            .image_subresource(
                vk::ImageSubresourceLayers::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .layer_count(1),
            )
            .image_offset(vk::Offset3D {
                x: rect.offset.x,
                y: rect.offset.y,
                z: 0,
            })
            .image_extent(vk::Extent3D {
                width: rect.extent.width,
                height: rect.extent.height,
                depth: 1,
            });
        let regions = [copy_region];
        unsafe {
            crate::vk_count!(cmd_copy_buffer_to_image);
            device.cmd_copy_buffer_to_image(
                cb,
                staging_buffer,
                self.vk_image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &regions,
            );
        }

        // Transition into SHADER_READ_ONLY_OPTIMAL — the layout the
        // 4.1.3.4 composite pass will sample.
        let to_read = vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::COPY)
            .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
            .dst_stage_mask(vk::PipelineStageFlags2::FRAGMENT_SHADER)
            .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
            .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .image(self.vk_image)
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .level_count(1)
                    .layer_count(1),
            );
        let to_read_arr = [to_read];
        let to_read_dep = vk::DependencyInfo::default().image_memory_barriers(&to_read_arr);
        crate::vk_count!(cmd_pipeline_barrier2);
        unsafe { device.cmd_pipeline_barrier2(cb, &to_read_dep) };

        self.current_layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
    }

    /// Construct a `DrawableImage` from a pooled entry. The pool
    /// entry's `current_layout` is preserved so the first upload's
    /// pre-barrier transitions correctly. Caller is responsible for
    /// clearing the recycled image — X11 clients rely on the
    /// Xorg/Xephyr convention of zero-filled new pixmaps (composite
    /// of smaller-than-tile content onto an uncleared pool entry
    /// leaks the previous tenant's pixels).
    ///
    /// Lazy `mask_view` / `no_alpha_src_view` are set to `None`;
    /// they'll be rebuilt on demand if needed.
    pub fn new_from_pool(
        vk: Arc<VkContext>,
        entry: crate::kms::vk::pixmap_pool::PooledPixmapImage,
        format: vk::Format,
        extent: vk::Extent2D,
    ) -> Self {
        Self {
            vk_image: entry.image,
            vk_image_view: entry.view,
            mask_view: None,
            no_alpha_src_view: None,
            extent,
            format,
            backing: ImageBacking::ServerOwned {
                vk_memory: entry.memory,
            },
            damage: MirrorDamage::default(),
            current_layout: entry.current_layout,
            vk,
        }
    }

    /// Decompose a `DrawableImage` into a pooled-pixmap-shape
    /// (image, view, memory, current_layout) for return-to-pool.
    /// Destroys the lazy views first (they're format-specific and
    /// not pooled). Pool-bound Vulkan handles transfer to the
    /// returned `PooledPixmapImage`; the rest of `self` (notably
    /// the `Arc<VkContext>`) drops normally.
    ///
    /// **Important — does NOT use `mem::forget`.** Codex P1 from
    /// plan review round 1: forgetting `self` would leak the
    /// `Arc<VkContext>` strong-count, eventually preventing
    /// VkContext::Drop's device wait. Instead, the pool-bound
    /// handles are swapped with `vk::*::null()` so the normal
    /// `Drop for DrawableImage` runs but sees null handles —
    /// Vulkan's spec permits destroying null handles as a no-op
    /// (every `destroy_*` and `free_memory` call), so Drop becomes
    /// a no-op on the Vulkan side. The `Arc<VkContext>` and other
    /// non-handle fields drop normally.
    ///
    /// Panics if `backing` is `Imported` (DRI3 dma-buf imports
    /// don't go through the pool; caller must check).
    #[allow(dead_code)] // wired by pixmap-pool T2 (free_pixmap).
    pub fn into_pool_entry(mut self) -> crate::kms::vk::pixmap_pool::PooledPixmapImage {
        let ImageBacking::ServerOwned { vk_memory } = self.backing else {
            panic!("DrawableImage::into_pool_entry: Imported backing cannot be pooled");
        };
        // Destroy lazy format-specific views (not pooled).
        let mask_view = self.mask_view.take();
        let no_alpha = self.no_alpha_src_view.take();
        unsafe {
            if let Some(v) = mask_view {
                self.vk.device.destroy_image_view(v, None);
            }
            if let Some(v) = no_alpha {
                self.vk.device.destroy_image_view(v, None);
            }
        }
        // Swap pool-bound handles out, leaving nulls in their
        // place so self's Drop is a no-op for these handles.
        let image = std::mem::replace(&mut self.vk_image, vk::Image::null());
        let view = std::mem::replace(&mut self.vk_image_view, vk::ImageView::null());
        // Replace backing memory with null so Drop's free_memory
        // is also a no-op.
        self.backing = ImageBacking::ServerOwned {
            vk_memory: vk::DeviceMemory::null(),
        };

        // self drops here: Vk handle destruction calls are no-ops
        // on null; vk Arc drops normally; damage / layout fields
        // drop naturally.
        crate::kms::vk::pixmap_pool::PooledPixmapImage {
            image,
            view,
            memory: vk_memory,
            current_layout: self.current_layout,
        }
    }

    /// Bytes-per-pixel for the image's format. Only the formats this
    /// module actually allocates are recognised.
    pub fn bytes_per_pixel(&self) -> u32 {
        match self.format {
            vk::Format::R8_UNORM => 1,
            vk::Format::B8G8R8A8_UNORM => 4,
            // Should never happen: every constructor maps depth →
            // format from this set. If something else slipped through,
            // upstream is broken; assume 4 so the caller's stride math
            // doesn't divide by zero.
            other => {
                log::warn!(
                    "DrawableImage::bytes_per_pixel: unhandled format {other:?} → defaulting to 4"
                );
                4
            }
        }
    }
}

impl Drop for DrawableImage {
    fn drop(&mut self) {
        unsafe {
            // Image view must be destroyed before the image it
            // references; both must drop before the underlying
            // memory is freed.
            if let Some(v) = self.mask_view.take() {
                self.vk.device.destroy_image_view(v, None);
            }
            if let Some(v) = self.no_alpha_src_view.take() {
                self.vk.device.destroy_image_view(v, None);
            }
            self.vk.device.destroy_image_view(self.vk_image_view, None);
            self.vk.device.destroy_image(self.vk_image, None);
            match &self.backing {
                ImageBacking::ServerOwned { vk_memory }
                | ImageBacking::Imported { vk_memory, .. } => {
                    self.vk.device.free_memory(*vk_memory, None);
                }
            }
        }
        // dma-buf OwnedFd in Imported variant drops automatically.
    }
}

// ---------------------------------------------------------------------------
// GLX TFP exportable image
// ---------------------------------------------------------------------------

/// BGRA8 is yserver's single-plane backing format for depth-24/32 pixmaps.
pub const EXPORT_FORMAT_BGRA8: vk::Format = vk::Format::B8G8R8A8_UNORM;

/// Usage flags for a GLX-TFP exportable image: it is rendered into as a
/// promoted window backing (`COLOR_ATTACHMENT`), copied into during
/// promotion (`TRANSFER_DST`) and out of for readback (`TRANSFER_SRC`),
/// and sampled by the composite pass (`SAMPLED`).
///
/// Must stay in lock-step with the usage passed to the modifier
/// capability probe ([`crate::kms::vk::dri3::export_capable_modifiers`]):
/// the driver may accept a modifier for one usage set and reject it for
/// another, so the probe and the create must ask the identical question.
pub(crate) const EXPORT_IMAGE_USAGE: vk::ImageUsageFlags = vk::ImageUsageFlags::from_raw(
    vk::ImageUsageFlags::TRANSFER_SRC.as_raw()
        | vk::ImageUsageFlags::TRANSFER_DST.as_raw()
        | vk::ImageUsageFlags::SAMPLED.as_raw()
        | vk::ImageUsageFlags::COLOR_ATTACHMENT.as_raw(),
);

/// Exact usage of copied scanout's renderer-side DMA-BUF transport. Renderer A
/// copies its optimal compose target into this image and renderer B copies out
/// of an imported alias. Keeping color-attachment and sampling usage out of
/// the external-memory contract supports transport modifiers which cannot be
/// rendered into or sampled directly.
pub(crate) const COPIED_TRANSPORT_IMAGE_USAGE: vk::ImageUsageFlags =
    vk::ImageUsageFlags::TRANSFER_DST;

/// The exact external transport representation persisted for copied scanout.
/// Unlike [`TilingStrategy`], this never consults or mutates the GLX-TFP
/// strategy cache. The local compose target is always an independent optimal
/// image and is not part of this external-memory plan.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub(crate) enum CopiedSourcePlan {
    DrmModifier(u64),
}

impl CopiedSourcePlan {
    #[must_use]
    pub(crate) const fn modifier(self) -> u64 {
        match self {
            Self::DrmModifier(modifier) => modifier,
        }
    }

    #[must_use]
    pub(crate) const fn is_linear(self) -> bool {
        self.modifier() == super::dri3::DRM_FORMAT_MOD_LINEAR
    }

    pub(crate) fn describe(self) -> String {
        if self.is_linear() {
            "drm-modifier=0x0-linear-transport".to_string()
        } else {
            format!("drm-modifier=0x{:x}-native-transport", self.modifier())
        }
    }
}

/// A freshly-allocated external-memory image that can be exported as a
/// dma-buf fd.
///
/// Unlike [`DrawableImage`] (which aliases imported client memory), this type
/// owns server-allocated, export-capable memory.  It is the allocation product
/// of [`allocate_exportable`]; the export itself is performed by
/// [`crate::kms::vk::dri3::export_backing`].
pub struct ExportableImage {
    pub image: vk::Image,
    pub memory: vk::DeviceMemory,
    pub extent: vk::Extent2D,
    pub format: vk::Format,
    /// Row stride in bytes, from `vkGetImageSubresourceLayout`.
    pub stride: u32,
    /// Plane-0 byte offset in the exported allocation.
    pub(crate) offset: u64,
    /// Total memory size in bytes, from `vkGetImageSubresourceLayout`.
    pub size: u64,
    /// DRM format modifier; `DRM_FORMAT_MOD_LINEAR` (0) for the LINEAR path.
    pub modifier: u64,
    /// Kept so `Drop` can call `vkFreeMemory` / `vkDestroyImage`.
    vk: Arc<VkContext>,
}

impl ExportableImage {
    /// Decompose into the raw Vk handles + export metadata WITHOUT
    /// running [`Drop`], transferring ownership of `image` / `memory`
    /// to the caller. Used by the GLX-TFP promotion path
    /// (`RenderEngine::promote_drawable_exportable`), which swaps these
    /// handles into a `Storage` whose own teardown then owns them.
    ///
    /// **Does NOT use `mem::forget`** — that would leak the
    /// `Arc<VkContext>` strong-count, eventually preventing
    /// `VkContext::Drop`'s `device_wait_idle` + `destroy_device` (the
    /// same hazard `DrawableImage::into_pool_entry` documents). Instead
    /// the struct is wrapped in `ManuallyDrop` so `Drop` never fires on
    /// the Vk handles, then the `Arc<VkContext>` is read out and
    /// dropped normally, decrementing the strong-count as it should.
    pub fn into_raw_parts(self) -> (vk::Image, vk::DeviceMemory, u32, u64, u64) {
        let mut md = std::mem::ManuallyDrop::new(self);
        let image = md.image;
        let memory = md.memory;
        let stride = md.stride;
        let size = md.size;
        let modifier = md.modifier;
        // Drop the Arc<VkContext> normally (decrement strong-count)
        // without destroying the Vk handles the caller now owns.
        unsafe { std::ptr::drop_in_place(&mut md.vk) };
        (image, memory, stride, size, modifier)
    }
}

impl Drop for ExportableImage {
    fn drop(&mut self) {
        // image must be destroyed before the memory it was bound to
        unsafe {
            self.vk.device.destroy_image(self.image, None);
            self.vk.device.free_memory(self.memory, None);
        }
    }
}

/// GLX-TFP tiling strategy for the exported image, cached on
/// [`crate::kms::vk::device::VkContext::tfp_tiling_strategy`] after
/// the first successful allocation.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TilingStrategy {
    /// `VK_IMAGE_TILING_LINEAR` + dma-buf. Preferred on Turnip / Adreno
    /// — its UBWC modifier tile keeps compression metadata in driver
    /// caches that do not reach the dma-buf-backed memory on same-GPU
    /// share, so the Mesa GL importer samples a frozen snapshot. LINEAR
    /// has no compression metadata, so live writes are immediately
    /// visible.
    Linear,
    /// `VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT`. Required on RADV /
    /// RDNA2 — that driver rejects `LINEAR + COLOR_ATTACHMENT + dma-buf`
    /// with `VK_ERROR_FORMAT_NOT_SUPPORTED`.
    Modifier,
}

/// Allocate an external-memory, dma-buf-exportable `VkImage` + `VkDeviceMemory`.
///
/// **LINEAR-preferred, modifier-fallback.** The strategy is selected
/// once per `VkContext` and cached on its `tfp_tiling_strategy`
/// `OnceLock`:
///
/// - Empty (first call): try LINEAR first. If `vkCreateImage` accepts
///   it (Turnip, Mesa software, most non-AMD drivers), cache
///   [`TilingStrategy::Linear`]. If it rejects with
///   `FORMAT_NOT_SUPPORTED` (RADV/RDNA2), try the modifier path with
///   the driver's export-capable single-plane modifiers list and cache
///   [`TilingStrategy::Modifier`] on success.
/// - Set: dispatch directly to the cached path, skipping the failing
///   probe each call.
///
/// The modifier path uses `VkImageDrmFormatModifierListCreateInfoEXT`
/// with the list from [`crate::kms::vk::dri3::export_capable_modifiers`],
/// reads back the driver-chosen modifier with
/// `vkGetImageDrmFormatModifierPropertiesEXT`, and queries the layout
/// via the `MEMORY_PLANE_0` aspect (mandatory for modifier-tiled —
/// absent or no modifier qualifies (e.g. lavapipe under `cargo test`),
/// where a linear single-plane BGRA8 dma-buf imports without trouble and
/// a `COLOR`-aspect layout query is correct.
///
/// Mirrors the modifier/linear plan dispatch in [`super::scanout`]
/// (`allocate_vk_scanout_image`).
pub fn allocate_exportable(
    vk: &Arc<VkContext>,
    width: u32,
    height: u32,
    format: vk::Format,
) -> Result<ExportableImage, vk::Result> {
    if let Some(strategy) = vk.tfp_tiling_strategy.get() {
        return allocate_with_strategy(vk, width, height, format, *strategy);
    }

    // First allocation: probe LINEAR first. Turnip / Adreno requires
    // it for same-GPU coherence (UBWC's compression metadata stays in
    // driver caches and the Mesa GL importer reads stale tiles).
    match allocate_exportable_linear(vk, width, height, format, EXPORT_IMAGE_USAGE) {
        Ok(img) => {
            let _ = vk.tfp_tiling_strategy.set(TilingStrategy::Linear);
            return Ok(img);
        }
        Err(e) => {
            // RADV / RDNA2 path: LINEAR + COLOR_ATTACHMENT + dma-buf
            // → `VK_ERROR_FORMAT_NOT_SUPPORTED`. Fall through to the
            // modifier path. Log at debug — RADV ALWAYS lands here on
            // first export; warning would be noise.
            log::debug!("allocate_exportable: LINEAR probe failed ({e:?}); trying modifier path");
        }
    }

    if !vk.image_drm_format_modifier {
        return Err(vk::Result::ERROR_FORMAT_NOT_SUPPORTED);
    }
    let modifiers = super::dri3::export_capable_modifiers(vk, format);
    if modifiers.is_empty() {
        return Err(vk::Result::ERROR_FORMAT_NOT_SUPPORTED);
    }
    match allocate_exportable_modifier(vk, width, height, format, &modifiers, EXPORT_IMAGE_USAGE) {
        Ok(img) => {
            let _ = vk.tfp_tiling_strategy.set(TilingStrategy::Modifier);
            Ok(img)
        }
        Err(e) => {
            log::warn!(
                "allocate_exportable: both LINEAR and modifier paths failed (modifier err {e:?})"
            );
            Err(e)
        }
    }
}

fn allocate_with_strategy(
    vk: &Arc<VkContext>,
    width: u32,
    height: u32,
    format: vk::Format,
    strategy: TilingStrategy,
) -> Result<ExportableImage, vk::Result> {
    match strategy {
        TilingStrategy::Linear => {
            allocate_exportable_linear(vk, width, height, format, EXPORT_IMAGE_USAGE)
        }
        TilingStrategy::Modifier => {
            if !vk.image_drm_format_modifier {
                return Err(vk::Result::ERROR_FORMAT_NOT_SUPPORTED);
            }
            let modifiers = super::dri3::export_capable_modifiers(vk, format);
            if modifiers.is_empty() {
                return Err(vk::Result::ERROR_FORMAT_NOT_SUPPORTED);
            }
            allocate_exportable_modifier(vk, width, height, format, &modifiers, EXPORT_IMAGE_USAGE)
        }
    }
}

/// Allocate one exact copied-scanout transport modifier without consulting or
/// changing [`VkContext::tfp_tiling_strategy`]. The one-element modifier list
/// makes the disposable winner exactly replayable on the live renderer.
pub(crate) fn allocate_copied_source_exact(
    vk: &Arc<VkContext>,
    width: u32,
    height: u32,
    format: vk::Format,
    plan: CopiedSourcePlan,
) -> Result<ExportableImage, vk::Result> {
    let requested_modifier = plan.modifier();
    let modifiers = [requested_modifier];
    let image = allocate_exportable_modifier(
        vk,
        width,
        height,
        format,
        &modifiers,
        COPIED_TRANSPORT_IMAGE_USAGE,
    )?;
    if let Err(error) = validate_copied_source_modifier(requested_modifier, image.modifier) {
        log::warn!(
            "copied transport exact modifier mismatch: requested=0x{requested_modifier:x} \
             actual=0x{:x}",
            image.modifier,
        );
        return Err(error);
    }
    Ok(image)
}

fn validate_copied_source_modifier(requested: u64, actual: u64) -> Result<(), vk::Result> {
    if requested == actual {
        Ok(())
    } else {
        Err(vk::Result::ERROR_FORMAT_NOT_SUPPORTED)
    }
}

/// DRM-modifier exportable allocation (including modifier-linear). The chosen
/// `modifiers` come from [`crate::kms::vk::dri3::export_capable_modifiers`];
/// the driver selects one from the list and reports it back.
fn allocate_exportable_modifier(
    vk: &Arc<VkContext>,
    width: u32,
    height: u32,
    format: vk::Format,
    modifiers: &[u64],
    usage: vk::ImageUsageFlags,
) -> Result<ExportableImage, vk::Result> {
    let modifier_ext = vk
        .image_drm_format_modifier_ext
        .as_ref()
        .ok_or(vk::Result::ERROR_EXTENSION_NOT_PRESENT)?;

    let mut ext_mem = vk::ExternalMemoryImageCreateInfo::default()
        .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let mut modifier_list =
        vk::ImageDrmFormatModifierListCreateInfoEXT::default().drm_format_modifiers(modifiers);

    let image_info = vk::ImageCreateInfo::default()
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
        .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
        .usage(usage)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .push_next(&mut ext_mem)
        .push_next(&mut modifier_list);

    let image = unsafe { vk.device.create_image(&image_info, None)? };

    let memory = match bind_exportable_memory(vk, image) {
        Ok(m) => m,
        Err(e) => {
            unsafe { vk.device.destroy_image(image, None) };
            return Err(e);
        }
    };

    // Read back the modifier the driver actually picked from the list.
    let mut mod_props = vk::ImageDrmFormatModifierPropertiesEXT::default();
    if let Err(e) =
        unsafe { modifier_ext.get_image_drm_format_modifier_properties(image, &mut mod_props) }
    {
        unsafe {
            vk.device.free_memory(memory, None);
            vk.device.destroy_image(image, None);
        }
        return Err(e);
    }

    // Modifier-tiled images MUST be queried with a MEMORY_PLANE aspect;
    // COLOR is a validation error. We only accept single-plane modifiers
    // (export reply carries one plane), so plane 0 is the whole image.
    let subresource = vk::ImageSubresource {
        aspect_mask: vk::ImageAspectFlags::MEMORY_PLANE_0_EXT,
        mip_level: 0,
        array_layer: 0,
    };
    let layout = unsafe { vk.device.get_image_subresource_layout(image, subresource) };

    Ok(ExportableImage {
        image,
        memory,
        extent: vk::Extent2D { width, height },
        format,
        stride: u32::try_from(layout.row_pitch).unwrap_or(u32::MAX),
        offset: layout.offset,
        size: layout.size,
        modifier: mod_props.drm_format_modifier,
        vk: Arc::clone(vk),
    })
}

/// Historical `VK_IMAGE_TILING_LINEAR` exportable allocation. Used on
/// drivers without `VK_EXT_image_drm_format_modifier` or with no
/// export-capable modifier for `format`.
fn allocate_exportable_linear(
    vk: &Arc<VkContext>,
    width: u32,
    height: u32,
    format: vk::Format,
    usage: vk::ImageUsageFlags,
) -> Result<ExportableImage, vk::Result> {
    let mut ext_mem = vk::ExternalMemoryImageCreateInfo::default()
        .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);

    let image_info = vk::ImageCreateInfo::default()
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
        .tiling(vk::ImageTiling::LINEAR)
        .usage(usage)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .push_next(&mut ext_mem);

    let image = unsafe { vk.device.create_image(&image_info, None)? };

    let memory = match bind_exportable_memory(vk, image) {
        Ok(m) => m,
        Err(e) => {
            unsafe { vk.device.destroy_image(image, None) };
            return Err(e);
        }
    };

    let subresource = vk::ImageSubresource {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        mip_level: 0,
        array_layer: 0,
    };
    let layout = unsafe { vk.device.get_image_subresource_layout(image, subresource) };

    Ok(ExportableImage {
        image,
        memory,
        extent: vk::Extent2D { width, height },
        format,
        stride: u32::try_from(layout.row_pitch).unwrap_or(u32::MAX),
        offset: layout.offset,
        size: layout.size,
        modifier: 0, // DRM_FORMAT_MOD_LINEAR
        vk: Arc::clone(vk),
    })
}

/// Allocate dma-buf-exportable, dedicated `VkDeviceMemory` for `image`
/// and bind it. Shared by the modifier and linear exportable paths.
/// On error the caller still owns `image` and must destroy it.
fn bind_exportable_memory(
    vk: &Arc<VkContext>,
    image: vk::Image,
) -> Result<vk::DeviceMemory, vk::Result> {
    let mem_reqs = unsafe { vk.device.get_image_memory_requirements(image) };
    let mem_props = unsafe {
        vk.instance
            .get_physical_device_memory_properties(vk.physical_device)
    };

    let memory_type_index = pick_memory_type(
        &mem_props,
        mem_reqs.memory_type_bits,
        vk::MemoryPropertyFlags::DEVICE_LOCAL,
    )
    .or_else(|| {
        // Fallback: any compatible type (e.g. on lavapipe / integrated GPU
        // all memory is HOST_VISIBLE with no DEVICE_LOCAL-only type).
        pick_memory_type(
            &mem_props,
            mem_reqs.memory_type_bits,
            vk::MemoryPropertyFlags::empty(),
        )
    })
    .ok_or(vk::Result::ERROR_OUT_OF_DEVICE_MEMORY)?;

    let mut export_alloc = vk::ExportMemoryAllocateInfo::default()
        .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
    let alloc_info = vk::MemoryAllocateInfo::default()
        .allocation_size(mem_reqs.size)
        .memory_type_index(memory_type_index)
        .push_next(&mut export_alloc)
        .push_next(&mut dedicated);

    let memory = unsafe { vk.device.allocate_memory(&alloc_info, None)? };

    if let Err(e) = unsafe { vk.device.bind_image_memory(image, memory, 0) } {
        unsafe { vk.device.free_memory(memory, None) };
        return Err(e);
    }
    Ok(memory)
}

fn pick_memory_type(
    props: &vk::PhysicalDeviceMemoryProperties,
    type_bits: u32,
    required: vk::MemoryPropertyFlags,
) -> Option<u32> {
    (0..props.memory_type_count).find(|&i| {
        type_bits & (1 << i) != 0
            && props.memory_types[i as usize]
                .property_flags
                .contains(required)
    })
}

// Compile-only check that `from_dmabuf` keeps a stable signature
// across Phase 4.2 work. References every parameter type so an
// upstream rename in `ash` or `std::os::fd` breaks the build before
// 4.2 lands rather than at runtime.
#[cfg(test)]
#[allow(dead_code)]
fn _compile_check_from_dmabuf(
    vk: Arc<VkContext>,
    dma_buf_fd: std::os::fd::OwnedFd,
    modifier: u64,
    plane_offsets: &[u64],
    plane_pitches: &[u32],
) {
    let _ = DrawableImage::from_dmabuf(
        vk,
        dma_buf_fd,
        128,
        128,
        vk::Format::B8G8R8A8_UNORM,
        modifier,
        plane_offsets,
        plane_pitches,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copied_transport_usage_is_exact_explicit_modifier_destination_contract() {
        assert!(!COPIED_TRANSPORT_IMAGE_USAGE.contains(vk::ImageUsageFlags::COLOR_ATTACHMENT));
        assert!(!COPIED_TRANSPORT_IMAGE_USAGE.contains(vk::ImageUsageFlags::TRANSFER_SRC));
        assert!(COPIED_TRANSPORT_IMAGE_USAGE.contains(vk::ImageUsageFlags::TRANSFER_DST));
        assert!(!COPIED_TRANSPORT_IMAGE_USAGE.contains(vk::ImageUsageFlags::SAMPLED));
    }

    #[test]
    fn copied_source_plan_distinguishes_native_and_explicit_linear_transport() {
        let linear = CopiedSourcePlan::DrmModifier(super::super::dri3::DRM_FORMAT_MOD_LINEAR);
        let native = CopiedSourcePlan::DrmModifier(0xabcd);
        assert_eq!(linear.describe(), "drm-modifier=0x0-linear-transport");
        assert_eq!(linear.modifier(), super::super::dri3::DRM_FORMAT_MOD_LINEAR);
        assert!(linear.is_linear());
        assert_eq!(native.describe(), "drm-modifier=0xabcd-native-transport");
        assert_eq!(native.modifier(), 0xabcd);
        assert!(!native.is_linear());
    }

    #[test]
    fn copied_source_exact_allocation_rejects_a_different_modifier() {
        assert_eq!(validate_copied_source_modifier(0xabcd, 0xabcd), Ok(()));
        assert_eq!(
            validate_copied_source_modifier(0xabcd, 0xef01),
            Err(vk::Result::ERROR_FORMAT_NOT_SUPPORTED)
        );
    }

    #[test]
    fn imported_memory_types_intersect_image_and_fd_requirements() {
        assert_eq!(intersect_import_memory_type_bits(0b1101, 0b0110), 0b0100);
        assert_eq!(intersect_import_memory_type_bits(0b1000, 0b0011), 0);
    }

    #[test]
    fn non_explicit_linear_layout_requires_exact_offset_and_pitch() {
        let required = vk::SubresourceLayout {
            offset: 256,
            row_pitch: 4096,
            ..Default::default()
        };
        validate_non_explicit_linear_layout(&[256], &[4096], required)
            .expect("identical concrete DMA-BUF layout is importable");

        assert!(matches!(
            validate_non_explicit_linear_layout(&[0], &[4096], required),
            Err(DrawableImageError::LinearLayoutMismatch {
                supplied_offset: 0,
                required_offset: 256,
                ..
            })
        ));
        assert!(matches!(
            validate_non_explicit_linear_layout(&[256], &[2048], required),
            Err(DrawableImageError::LinearLayoutMismatch {
                supplied_pitch: 2048,
                required_pitch: 4096,
                ..
            })
        ));
    }

    #[test]
    fn non_explicit_linear_layout_rejects_multiplane_shape() {
        let required = vk::SubresourceLayout {
            row_pitch: 4096,
            ..Default::default()
        };
        assert!(matches!(
            validate_non_explicit_linear_layout(&[0, 8192], &[4096, 4096], required),
            Err(DrawableImageError::NonExplicitLinearPlaneCount(2))
        ));
    }
}
