use super::*;

impl ScratchImage {
    pub(super) fn size_bytes(&self) -> u64 {
        self.size_bytes
    }
}

impl Drop for ScratchImage {
    fn drop(&mut self) {
        unsafe {
            self.vk.device.destroy_image(self.image, None);
            crate::kms::vk::mem_accounting::free_memory(&self.vk.device, self.memory);
        }
    }
}

impl Drop for ClipSnapshot {
    fn drop(&mut self) {
        unsafe {
            self.vk.device.destroy_image_view(self.view, None);
            self.vk.device.destroy_image(self.image, None);
            crate::kms::vk::mem_accounting::free_memory(&self.vk.device, self.memory);
        }
    }
}

impl Drop for SampledScratchImage {
    fn drop(&mut self) {
        unsafe {
            self.vk.device.destroy_image_view(self.view, None);
            self.vk.device.destroy_image(self.image, None);
            crate::kms::vk::mem_accounting::free_memory(&self.vk.device, self.memory);
        }
    }
}

impl RenderEngine {
    /// Create a new pinned R8 snapshot image (TRANSFER_DST | SAMPLED), UNDEFINED
    /// layout, `snapshotted_version = u64::MAX` (forces the first refresh).
    /// Allocation only — the caller (Task 14, at clip-mask install while the
    /// source pixmap is guaranteed live) MUST call `refresh_clip_snapshot`
    /// (Task 13) to populate it BEFORE the first masked use: retain-after-free
    /// requires the snapshot hold real bytes before any later free (finding 5).
    #[allow(
        dead_code,
        reason = "used by refresh_clip_snapshot (Task 13) and backend routing (Task 14)"
    )]
    pub(crate) fn create_clip_snapshot(
        &mut self,
        width: u32,
        height: u32,
    ) -> Result<SnapshotId, RenderError> {
        let inner = self.inner.as_mut().ok_or(RenderError::NoVk)?;
        // Reuse allocate_sampled_scratch_image's body but with R8_UNORM and a
        // persistent (non-Drop-on-scope) image. Inline the alloc here so the
        // image/memory/view live in ClipSnapshot, not SampledScratchImage.
        let format = vk::Format::R8_UNORM;
        let snap = alloc_clip_snapshot(&inner.vk.clone(), width, height, format)?;
        let id = SnapshotId(inner.next_snapshot_id);
        inner.next_snapshot_id = inner.next_snapshot_id.wrapping_add(1);
        inner.clip_snapshots.insert(id, snap);
        Ok(id)
    }

    pub(crate) fn clip_snapshot_extent(&self, id: SnapshotId) -> Option<vk::Extent2D> {
        self.inner
            .as_ref()?
            .clip_snapshots
            .get(&id)
            .map(|s| s.extent)
    }

    pub(crate) fn clip_snapshot_image(&self, id: SnapshotId) -> Option<vk::Image> {
        self.inner
            .as_ref()?
            .clip_snapshots
            .get(&id)
            .map(|s| s.image)
    }

    pub(crate) fn clip_snapshot_view(&self, id: SnapshotId) -> Option<vk::ImageView> {
        self.inner.as_ref()?.clip_snapshots.get(&id).map(|s| s.view)
    }

    pub(crate) fn clip_snapshot_layout(&self, id: SnapshotId) -> Option<vk::ImageLayout> {
        self.inner
            .as_ref()?
            .clip_snapshots
            .get(&id)
            .map(|s| s.current_layout)
    }

    pub(crate) fn clip_snapshot_version(&self, id: SnapshotId) -> Option<u64> {
        self.inner
            .as_ref()?
            .clip_snapshots
            .get(&id)
            .map(|s| s.snapshotted_version)
    }

    /// Retire a snapshot (GC freed / re-allocated at new size). Deferred behind
    /// the snapshot's last_render_ticket so no in-flight frame samples a freed image.
    pub(crate) fn retire_clip_snapshot(&mut self, id: SnapshotId) {
        let Some(inner) = self.inner.as_mut() else {
            return;
        };
        if let Some(snap) = inner.clip_snapshots.remove(&id) {
            let guard = snap.last_render_ticket.clone();
            inner.retired_snapshots.push((snap, guard));
        }
    }

    /// Phase 2 clip Task 13: (re)populate a GC-owned clip `ClipSnapshot` from the
    /// live clip pixmap by appending a standalone `ClipSnapshotRefresh` op.
    ///
    /// Called at clip-mask install (while the live pixmap is guaranteed present →
    /// retain-after-free) and before any masked copy whose snapshot version is
    /// stale (same-frame mask writes). The live clip pixmap is a first-class frame
    /// participant (READ → terminal SHADER_READ): it gets first-touch / ticket /
    /// old-layout registration just like `masked_copy_area`'s src. Both the live
    /// mask and the snapshot end at `SHADER_READ_ONLY_OPTIMAL`.
    ///
    /// This is the WRITE path that advances `snapshotted_version` (deferred from
    /// Task 12's SAMPLE path): the commit sets the snapshot's terminal layout,
    /// binds it to this frame's ticket, AND records the new version. A close-time
    /// failure rolls all three back via `rollback_snapshots` (from the
    /// `snapshot_touch` overlay seeded by `snapshot_first_touch`).
    ///
    /// Mirrors `masked_copy_area`'s entry prelude + frame-open/ticket acquisition
    /// verbatim.
    ///
    /// # Errors
    /// `RendererFailed` if the renderer already failed; `NoVk` if there is no Vk
    /// inner; `UnknownDrawable` if the live mask is absent; any flush error.
    pub(crate) fn refresh_clip_snapshot(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
        id: SnapshotId,
        live_mask_id: DrawableId,
        version: u64,
    ) -> Result<(), RenderError> {
        // No-op if already current (read BEFORE any mutation).
        if self
            .inner
            .as_ref()
            .and_then(|i| i.clip_snapshots.get(&id))
            .map(|s| s.snapshotted_version)
            == Some(version)
        {
            return Ok(());
        }

        // ENTRY PRELUDE — same as copy_area/masked_copy_area: renderer guard +
        // flush_render_batch BEFORE any open-frame mutation, so the refresh op is
        // chronologically ordered after any pending render batch.
        if platform.renderer_failed {
            return Err(RenderError::RendererFailed);
        }
        self.flush_render_batch(store, platform, RenderFlushReason::Other)?;
        let inner = self.inner.as_mut().ok_or(RenderError::NoVk)?;

        // Preflight reads (borrow-split: locals first, before the open-frame
        // mutable borrow). Live mask image; snapshot image / extent / layout.
        let live_image = {
            let d = store
                .get(live_mask_id)
                .ok_or(RenderError::UnknownDrawable(live_mask_id))?;
            d.storage.image
        };
        let copy_extent = inner.clip_snapshots.get(&id).expect("snapshot").extent;
        let snap_image = inner.clip_snapshots.get(&id).expect("snapshot").image;
        let snap_old = inner
            .clip_snapshots
            .get(&id)
            .expect("snapshot")
            .current_layout;

        // Open the frame if not already open. Mirror masked_copy_area: bump
        // acquire_generation at open + capture on OpenFrame.
        if !inner.frame_builder.is_open() {
            let _ = inner;
            let ticket = platform.submit_group_ticket_or_open()?;
            let inner = self.inner.as_mut().expect("inner");
            inner.acquire_generation = inner.acquire_generation.saturating_add(1);
            let frame_generation = inner.acquire_generation;
            inner.frame_builder.open_for_paint(ticket, frame_generation);
        }
        let inner = self.inner.as_mut().expect("inner");
        let frame_ticket = inner
            .frame_builder
            .open
            .as_ref()
            .expect("just opened")
            .ticket
            .clone();

        // Live-mask drawable participation (first-touch / ticket / old-layout); it
        // is a READ → terminal SHADER_READ. Mirrors masked_copy_area's src.
        let lm_pre = inner.current_layout_for_drawable(store, live_mask_id);
        let prior_lm = store
            .get(live_mask_id)
            .and_then(|d| d.last_render_ticket.clone());
        {
            let open = inner.frame_builder.open.as_mut().expect("open");
            open.touched.first_touch(live_mask_id, prior_lm);
            open.layouts.first_touch_drawable(live_mask_id, lm_pre);
        }
        store.touch_render_fence(live_mask_id, frame_ticket.clone());

        // Snapshot first-touch for rollback (Task 12 helper).
        snapshot_first_touch(inner, id);

        // Append the standalone refresh op + set the live-mask terminal overlay.
        let payload = Box::new(
            crate::kms::render::frame_builder::RecordedClipSnapshotRefresh {
                snapshot_id: id,
                snapshot_image: snap_image,
                snapshot_old_layout: snap_old,
                live_mask_id,
                live_mask_image: live_image,
                live_mask_old_layout: lm_pre,
                copy_extent,
            },
        );
        {
            let open = inner.frame_builder.open.as_mut().expect("open");
            open.push_op_and_set_layouts(
                crate::kms::render::frame_builder::RecordedOp::ClipSnapshotRefresh(payload),
                &[(live_mask_id, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)],
            );
        }

        // WRITE-path commit: terminal layout + ticket + version. Unlike the
        // SAMPLE path (masked_copy_area), this path ADVANCES the version — the
        // refresh (re)populates the snapshot to `version`. On close-failure
        // `rollback_snapshots` restores all three fields from `snapshot_touch`.
        if let Some(snap) = inner.clip_snapshots.get_mut(&id) {
            snap.current_layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
            snap.last_render_ticket = Some(frame_ticket.clone());
            snap.snapshotted_version = version;
        }
        Ok(())
    }
}

/// Allocate a scratch image for `copy_area`'s overlap path.
/// Device-local, OPTIMAL tiling, TRANSFER_SRC|TRANSFER_DST usage.
/// Caller is responsible for adopting it into the op's
/// `SubmittedOp.scratch` so it retires on the fence.
pub(super) fn allocate_scratch_image(
    vk: &Arc<VkContext>,
    _platform: &PlatformBackend,
    width: u32,
    height: u32,
    format: vk::Format,
) -> Result<ScratchImage, RenderError> {
    let info = vk::ImageCreateInfo::default()
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
        .usage(vk::ImageUsageFlags::TRANSFER_SRC | vk::ImageUsageFlags::TRANSFER_DST)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED);
    let image = unsafe { vk.device.create_image(&info, None)? };
    let mem_reqs = unsafe { vk.device.get_image_memory_requirements(image) };
    let mem_props = unsafe {
        vk.instance
            .get_physical_device_memory_properties(vk.physical_device)
    };
    let Some(mt) = (0..mem_props.memory_type_count).find(|&i| {
        mem_reqs.memory_type_bits & (1 << i) != 0
            && mem_props.memory_types[i as usize]
                .property_flags
                .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
    }) else {
        unsafe { vk.device.destroy_image(image, None) };
        return Err(RenderError::Vk(vk::Result::ERROR_FEATURE_NOT_PRESENT));
    };
    let alloc_info = vk::MemoryAllocateInfo::default()
        .allocation_size(mem_reqs.size)
        .memory_type_index(mt);
    let memory = match crate::kms::vk::mem_accounting::allocate_memory(
        &vk.device,
        &alloc_info,
        crate::kms::vk::mem_accounting::MemCategory::Scratch,
        &mem_props,
    ) {
        Ok(m) => m,
        Err(e) => {
            unsafe { vk.device.destroy_image(image, None) };
            return Err(RenderError::Vk(e));
        }
    };
    if let Err(e) = unsafe { vk.device.bind_image_memory(image, memory, 0) } {
        unsafe {
            crate::kms::vk::mem_accounting::free_memory(&vk.device, memory);
            vk.device.destroy_image(image, None);
        }
        return Err(RenderError::Vk(e));
    }
    Ok(ScratchImage {
        vk: Arc::clone(vk),
        image,
        memory,
        size_bytes: mem_reqs.size,
    })
}

/// Allocate the backing image/memory/view for a [`ClipSnapshot`]. Mirrors
/// [`allocate_sampled_scratch_image`]'s image/memory/view creation verbatim,
/// but wraps the result in `ClipSnapshot` with `current_layout = UNDEFINED`,
/// `last_render_ticket = None`, and `snapshotted_version = u64::MAX` (force the
/// first refresh). Format is `R8_UNORM` (depth-1 coverage mask).
fn alloc_clip_snapshot(
    vk: &Arc<VkContext>,
    width: u32,
    height: u32,
    format: vk::Format,
) -> Result<ClipSnapshot, RenderError> {
    let info = vk::ImageCreateInfo::default()
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
        .usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED);
    let image = unsafe { vk.device.create_image(&info, None)? };
    let mem_reqs = unsafe { vk.device.get_image_memory_requirements(image) };
    let mem_props = unsafe {
        vk.instance
            .get_physical_device_memory_properties(vk.physical_device)
    };
    let Some(mt) = (0..mem_props.memory_type_count).find(|&i| {
        mem_reqs.memory_type_bits & (1 << i) != 0
            && mem_props.memory_types[i as usize]
                .property_flags
                .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
    }) else {
        unsafe { vk.device.destroy_image(image, None) };
        return Err(RenderError::Vk(vk::Result::ERROR_FEATURE_NOT_PRESENT));
    };
    let alloc_info = vk::MemoryAllocateInfo::default()
        .allocation_size(mem_reqs.size)
        .memory_type_index(mt);
    let memory = match crate::kms::vk::mem_accounting::allocate_memory(
        &vk.device,
        &alloc_info,
        crate::kms::vk::mem_accounting::MemCategory::Scratch,
        &mem_props,
    ) {
        Ok(m) => m,
        Err(e) => {
            unsafe { vk.device.destroy_image(image, None) };
            return Err(RenderError::Vk(e));
        }
    };
    if let Err(e) = unsafe { vk.device.bind_image_memory(image, memory, 0) } {
        unsafe {
            crate::kms::vk::mem_accounting::free_memory(&vk.device, memory);
            vk.device.destroy_image(image, None);
        }
        return Err(RenderError::Vk(e));
    }
    // IDENTITY view (no .components()) — matches Storage::image_view semantics.
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
            return Err(RenderError::Vk(e));
        }
    };
    Ok(ClipSnapshot {
        vk: Arc::clone(vk),
        image,
        view,
        memory,
        extent: vk::Extent2D { width, height },
        current_layout: vk::ImageLayout::UNDEFINED,
        last_render_ticket: None,
        snapshotted_version: u64::MAX,
        size_bytes: mem_reqs.size,
    })
}

pub(super) fn allocate_sampled_scratch_image(
    vk: &Arc<VkContext>,
    width: u32,
    height: u32,
    format: vk::Format,
) -> Result<SampledScratchImage, RenderError> {
    let info = vk::ImageCreateInfo::default()
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
        .usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED);
    let image = unsafe { vk.device.create_image(&info, None)? };
    let mem_reqs = unsafe { vk.device.get_image_memory_requirements(image) };
    let mem_props = unsafe {
        vk.instance
            .get_physical_device_memory_properties(vk.physical_device)
    };
    let Some(mt) = (0..mem_props.memory_type_count).find(|&i| {
        mem_reqs.memory_type_bits & (1 << i) != 0
            && mem_props.memory_types[i as usize]
                .property_flags
                .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
    }) else {
        unsafe { vk.device.destroy_image(image, None) };
        return Err(RenderError::Vk(vk::Result::ERROR_FEATURE_NOT_PRESENT));
    };
    let alloc_info = vk::MemoryAllocateInfo::default()
        .allocation_size(mem_reqs.size)
        .memory_type_index(mt);
    let memory = match crate::kms::vk::mem_accounting::allocate_memory(
        &vk.device,
        &alloc_info,
        crate::kms::vk::mem_accounting::MemCategory::Scratch,
        &mem_props,
    ) {
        Ok(m) => m,
        Err(e) => {
            unsafe { vk.device.destroy_image(image, None) };
            return Err(RenderError::Vk(e));
        }
    };
    if let Err(e) = unsafe { vk.device.bind_image_memory(image, memory, 0) } {
        unsafe {
            crate::kms::vk::mem_accounting::free_memory(&vk.device, memory);
            vk.device.destroy_image(image, None);
        }
        return Err(RenderError::Vk(e));
    }
    // IDENTITY view (no .components()) — matches Storage::image_view semantics.
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
            return Err(RenderError::Vk(e));
        }
    };
    Ok(SampledScratchImage {
        vk: Arc::clone(vk),
        image,
        view,
        memory,
        size_bytes: mem_reqs.size,
    })
}
