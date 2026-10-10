use super::*;

impl PlatformBackend {
    // ── Storage allocation (Stage 2c) ───────────────────────────

    /// Sample-side view swizzle for a (format, depth) pair. The
    /// attachment-side view kept by `Storage::image_view` always
    /// uses IDENTITY (VUID-VkFramebufferCreateInfo-pAttachments-00891
    /// requires that for color attachments). The sample-side view
    /// kept by `Storage::sample_view` carries the format-aware
    /// swizzle so the scene compositor + engine sampling paths see
    /// X11-correct alpha semantics:
    ///
    /// - `(R8_UNORM, _)` → `a=R, rgb=ZERO` — R8 storage sampled as
    ///   an alpha mask (glyphs, RENDER mask scratch, depth-1 / 8
    ///   bitmaps). RGB channels intentionally zeroed so the
    ///   composite shader's `src * coverage` reads zero RGB and
    ///   the dst keeps its own colour.
    /// - `(B8G8R8A8_UNORM, depth == 24)` → `a=ONE` — depth-24
    ///   pixmaps (`PictFormat.alpha_mask = 0` per X11 RENDER spec)
    ///   must read α = 1.0 regardless of the BGRA8 padding byte.
    ///   Otherwise the scene's `alpha_passthrough=true` window
    ///   draws blend with undefined α and the layer below leaks
    ///   through.
    /// - everything else → IDENTITY (depth-32 ARGB passes α
    ///   through; unknown formats default-safe).
    ///
    /// Mirrors `engine::swizzle_class_for` (the engine's RENDER
    /// view-cache classifier) — the engine cache stays for the
    /// cases where the sampler config also differs; this helper
    /// owns the storage-side view that the scene compositor
    /// binds directly.
    pub(crate) fn sample_view_components(format: vk::Format, depth: u8) -> vk::ComponentMapping {
        match (format, depth) {
            (vk::Format::R8_UNORM, _) => vk::ComponentMapping {
                r: vk::ComponentSwizzle::ZERO,
                g: vk::ComponentSwizzle::ZERO,
                b: vk::ComponentSwizzle::ZERO,
                a: vk::ComponentSwizzle::R,
            },
            (vk::Format::B8G8R8A8_UNORM, 24) => vk::ComponentMapping {
                r: vk::ComponentSwizzle::IDENTITY,
                g: vk::ComponentSwizzle::IDENTITY,
                b: vk::ComponentSwizzle::IDENTITY,
                a: vk::ComponentSwizzle::ONE,
            },
            _ => vk::ComponentMapping {
                r: vk::ComponentSwizzle::IDENTITY,
                g: vk::ComponentSwizzle::IDENTITY,
                b: vk::ComponentSwizzle::IDENTITY,
                a: vk::ComponentSwizzle::IDENTITY,
            },
        }
    }

    /// Build a fresh sample-side `vk::ImageView` over `image` with
    /// the format/depth-aware swizzle from
    /// [`Self::sample_view_components`]. Used by the fresh-alloc
    /// path, the pool-take path (where the pool only stores the
    /// attachment view), and the DRI3 import path (where the
    /// imported DrawableImage carries an identity-swizzle view we
    /// can't reuse for scene sampling).
    pub(crate) fn build_sample_view(
        vk: &crate::kms::vk::device::VkContext,
        image: vk::Image,
        format: vk::Format,
        depth: u8,
    ) -> Result<vk::ImageView, vk::Result> {
        let info = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(format)
            .components(Self::sample_view_components(format, depth))
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .level_count(1)
                    .layer_count(1),
            );
        unsafe { vk.device.create_image_view(&info, None) }
    }

    /// Build a fresh attachment-side `vk::ImageView` over `image` with
    /// an IDENTITY component swizzle. This matches what
    /// [`Self::allocate_drawable_storage`]'s fresh-alloc path builds for
    /// `Storage::image_view` (the colour-attachment view —
    /// VUID-VkFramebufferCreateInfo-pAttachments-00891 requires IDENTITY
    /// for attachment views). Used by the GLX-TFP promotion path
    /// (`RenderEngine::promote_drawable_exportable`) to rebuild the
    /// attachment view over the newly-adopted exportable image.
    pub(crate) fn build_attachment_view(
        vk: &crate::kms::vk::device::VkContext,
        image: vk::Image,
        format: vk::Format,
    ) -> Result<vk::ImageView, vk::Result> {
        let info = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(format)
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .level_count(1)
                    .layer_count(1),
            );
        unsafe { vk.device.create_image_view(&info, None) }
    }

    /// Map an X11 drawable depth to its v2 storage format. Mirrors
    /// `DrawableImage::format_for_pixmap_depth` (v1) so the two
    /// don't drift.
    #[must_use]
    pub(crate) fn format_for_depth(depth: u8) -> vk::Format {
        match depth {
            1 | 4 | 8 => vk::Format::R8_UNORM,
            24 | 32 => vk::Format::B8G8R8A8_UNORM,
            other => {
                log::warn!(
                    "render PlatformBackend::format_for_depth: unhandled depth {other} → \
                     defaulting to B8G8R8A8_UNORM",
                );
                vk::Format::B8G8R8A8_UNORM
            }
        }
    }

    /// Allocate a fresh server-owned [`Storage`] for the
    /// [`DrawableStore`]. DEVICE_LOCAL memory; tiling=OPTIMAL;
    /// usage covers Stage 2c (TRANSFER_SRC/DST, COLOR_ATTACHMENT,
    /// SAMPLED). Initial layout = `UNDEFINED`.
    ///
    /// # Errors
    ///
    /// Returns `ERROR_INITIALIZATION_FAILED` if Vk is not
    /// available (test fixture). Propagates `vkCreateImage` /
    /// `vkAllocateMemory` / `vkBindImageMemory` /
    /// `vkCreateImageView` failures.
    pub(crate) fn allocate_drawable_storage(
        &self,
        width: u16,
        height: u16,
        depth: u8,
    ) -> Result<Storage, vk::Result> {
        self.allocate_drawable_storage_as(
            width,
            height,
            depth,
            crate::kms::vk::mem_accounting::MemCategory::Pixmap,
        )
    }

    /// [`Self::allocate_drawable_storage`], accounting the memory under `category`.
    pub(crate) fn allocate_drawable_storage_as(
        &self,
        width: u16,
        height: u16,
        depth: u8,
        category: crate::kms::vk::mem_accounting::MemCategory,
    ) -> Result<Storage, vk::Result> {
        let vk = self
            .vk
            .as_ref()
            .ok_or(vk::Result::ERROR_INITIALIZATION_FAILED)?;
        let format = Self::format_for_depth(depth);
        let extent = vk::Extent2D {
            width: u32::from(width.max(1)),
            height: u32::from(height.max(1)),
        };

        // Stage 3f.10: try the recycle pool before falling through to
        // a fresh Vk allocate. v1's pool keys on
        // (width, height, format); the usage flag set is constant
        // across all server-owned pixmaps (matches v1).
        if let Some(pool) = self.pixmap_pool.as_ref() {
            let key = crate::kms::vk::pixmap_pool::PixmapPoolKey {
                width: extent.width,
                height: extent.height,
                format,
            };
            if let Some(pooled) = pool.try_take(key) {
                // The pool stores only the attachment-side
                // (IDENTITY) view; the sample-side view is
                // depth-specific (a recycled depth-32 BGRA8
                // image can serve a fresh depth-24 request and
                // vice versa, since the pool key is format only),
                // so always build a fresh sample_view for the
                // current request's depth. View creation is cheap;
                // pooling the image + memory is where the win is.
                let pooled_image = pooled.image;
                let sample_view = match Self::build_sample_view(vk, pooled_image, format, depth) {
                    Ok(v) => v,
                    Err(e) => {
                        // Couldn't build a sample_view: return the
                        // pooled triple back to the pool and fall
                        // through to fresh allocate (which also
                        // tries to build a sample_view and may also
                        // fail — but the diagnostic path is
                        // uniform that way).
                        let _ = pool.try_return(key, pooled);
                        return Err(e);
                    }
                };
                crate::kms::vk::mem_accounting::recategorise(pooled.memory, category);
                return Ok(Storage::from_pooled(
                    pooled,
                    sample_view,
                    extent,
                    format,
                    depth,
                ));
            }
        }

        let image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D {
                width: extent.width,
                height: extent.height,
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
        let memory_type_index = (0..mem_props.memory_type_count).find(|&i| {
            mem_reqs.memory_type_bits & (1 << i) != 0
                && mem_props.memory_types[i as usize]
                    .property_flags
                    .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
        });
        let Some(mt) = memory_type_index else {
            unsafe { vk.device.destroy_image(image, None) };
            return Err(vk::Result::ERROR_FEATURE_NOT_PRESENT);
        };

        let alloc_info = vk::MemoryAllocateInfo::default()
            .allocation_size(mem_reqs.size)
            .memory_type_index(mt);
        // Pool-sized storage only reaches here on a pool miss; the split
        // tells `vram churn` which side of `MAX_POOLED_DIM` allocates.
        let pool_sized = extent.width <= crate::kms::vk::pixmap_pool::MAX_POOLED_DIM
            && extent.height <= crate::kms::vk::pixmap_pool::MAX_POOLED_DIM;
        let memory = match crate::kms::vk::mem_accounting::allocate_storage_memory(
            &vk.device,
            &alloc_info,
            category,
            pool_sized,
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
                    crate::kms::vk::mem_accounting::free_memory(&vk.device, memory);
                    vk.device.destroy_image(image, None);
                }
                return Err(e);
            }
        };

        // Sample-side view with format/depth-aware swizzle. The
        // scene compositor and the engine view-cache fall back to
        // this view for sampling instead of `view` (IDENTITY) so
        // depth-24 BGRA8 storage reads α=ONE per X11 PictFormat
        // semantics. Built unconditionally — for depth-32 the
        // swizzle is identity, but a distinct VkImageView keeps
        // Storage's ownership story uniform.
        let sample_view = match Self::build_sample_view(vk, image, format, depth) {
            Ok(v) => v,
            Err(e) => {
                unsafe {
                    vk.device.destroy_image_view(view, None);
                    crate::kms::vk::mem_accounting::free_memory(&vk.device, memory);
                    vk.device.destroy_image(image, None);
                }
                return Err(e);
            }
        };

        Ok(Storage::new_server_owned(
            image,
            memory,
            view,
            sample_view,
            extent,
            format,
            depth,
        ))
    }
}
