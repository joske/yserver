use super::*;

/// Allocate the intermediate of every transformed output that lacks one of
/// its footprint's size (spec D4, Q5: only transformed outputs pay).
/// The space a root read of `output_idx` uses: the root footprint of a
/// transformed output (D6), else the mode.
fn root_readback_extent(platform: &PlatformBackend, output_idx: usize) -> vk::Extent2D {
    if platform.output_transform(output_idx).is_some() {
        let (_, _, w, h) = platform.output_root_rect(output_idx);
        return vk::Extent2D {
            width: w,
            height: h,
        };
    }
    platform
        .outputs
        .get(output_idx)
        .map_or(vk::Extent2D::default(), |layout| vk::Extent2D {
            width: u32::from(layout.width),
            height: u32::from(layout.height),
        })
}

/// Whether one of `valid` holds all of `rect`.
fn root_readback_covers(valid: &[vk::Rect2D], rect: vk::Rect2D) -> bool {
    valid.iter().any(|v| {
        v.offset.x <= rect.offset.x
            && v.offset.y <= rect.offset.y
            && i64::from(v.offset.x) + i64::from(v.extent.width)
                >= i64::from(rect.offset.x) + i64::from(rect.extent.width)
            && i64::from(v.offset.y) + i64::from(v.extent.height)
                >= i64::from(rect.offset.y) + i64::from(rect.extent.height)
    })
}

impl SceneCompositor {
    /// What output `output_idx` shows under `local` now, for a root read:
    /// Xorg's GetImage reads the screen pixmap, which every earlier
    /// request has painted (`DoGetImage`, `dix/dispatch.c:2176`), while a
    /// scanout BO holds the last composed frame. Composes the scene into
    /// a private image — just `local` for a lone read after a change once
    /// it holds a whole frame, else all of it — when anything changed
    /// since, and waits. `local` and the image are in the
    /// space a root read of this output uses: the mode for an identity
    /// output, the root footprint for a transformed one (D6). No software
    /// cursor: Xorg lifts the sprite off a GetImage (`miSpriteGetImage`).
    /// `Ok(None)` without a live scene. The caller has flushed pending
    /// paint when [`Self::root_readback_is_current`] said it was not.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn root_readback(
        &mut self,
        core: &KmsCore,
        store: &mut DrawableStore,
        windows: &crate::kms::render::backend::WindowsMap,
        platform: &PlatformBackend,
        cow_host_xid: Option<u32>,
        output_idx: usize,
        local: vk::Rect2D,
    ) -> Result<Option<vk::Image>, SceneError> {
        let generation = (self.structure_generation, store.scene_damage_generation());
        let overlay_ops = self
            .root_overlay
            .apply_list_for_output(platform.output_root_rect(output_idx));
        let Some(inner) = self.inner.as_mut() else {
            return Ok(None);
        };
        let extent = root_readback_extent(platform, output_idx);
        if extent.width == 0 || extent.height == 0 {
            return Ok(None);
        }
        if inner.root_readbacks.len() <= output_idx {
            inner.root_readbacks.resize_with(output_idx + 1, || None);
        }
        if inner.root_readbacks[output_idx]
            .as_ref()
            .is_none_or(|rb| rb.target.extent != extent)
        {
            inner.root_readbacks[output_idx] = Some(RootReadback {
                target: DamageAuditTarget::new(Arc::clone(&inner.vk), extent)
                    .map_err(SceneError::Vk)?,
                generation,
                valid: Vec::new(),
                whole: false,
                reads: 0,
                prev_reads: 0,
            });
        }
        let rb = inner.root_readbacks[output_idx]
            .as_mut()
            .expect("allocated above");
        if rb.generation != generation {
            rb.generation = generation;
            rb.valid.clear();
            rb.prev_reads = rb.reads;
            rb.reads = 0;
        }
        rb.reads = rb.reads.saturating_add(1);
        if root_readback_covers(&rb.valid, local) {
            return Ok(Some(rb.target.image));
        }
        let built = build_scene(
            core,
            store,
            windows,
            output_idx,
            platform,
            None,
            None,
            cow_host_xid,
            false,
            Visibility::On,
        );
        // The overlay XOR is not idempotent: only a full compose applies it
        // exactly once (see `record_command_buffer`).
        let one_read = rb.reads == 1 && rb.prev_reads <= 1;
        let (repaint, scissors) = if rb.whole && overlay_ops.is_empty() && one_read {
            (Repaint::Clipped(local), vec![local])
        } else {
            (Repaint::Full(extent), Vec::new())
        };
        let (xor_pipeline, xor_layout) = if overlay_ops.is_empty() {
            (vk::Pipeline::null(), vk::PipelineLayout::null())
        } else {
            let pl = inner.overlay_xor_cache.get(
                yserver_core::backend::GcFunction::Xor,
                crate::kms::vk::logic_fill_pipeline::LogicFillChannels::Color,
            )?;
            (pl, inner.overlay_xor_cache.pipeline_layout())
        };
        let vk = Arc::clone(&inner.vk);
        let rb = inner.root_readbacks[output_idx]
            .as_mut()
            .expect("allocated above");
        let draws = built.scene.draws.len();
        let pool = create_audit_descriptor_pool(&vk, draws)?;
        let ticket = platform.acquire_fence_ticket().map_err(SceneError::Vk)?;
        let mut submitted = false;
        let result = record_and_submit_render(
            &vk,
            &mut rb.target,
            &inner.pipeline,
            pool,
            &built.scene,
            repaint,
            &scissors,
            ticket.fence(),
            &mut submitted,
            &overlay_ops,
            xor_pipeline,
            xor_layout,
            None,
            None,
        );
        if submitted {
            ticket.wait(&vk).map_err(SceneError::Vk)?;
        }
        unsafe { vk.device.destroy_descriptor_pool(pool, None) };
        let recorded = result?.descriptor_count;
        // Drivers may over-allocate a pool, so the test caps the count.
        #[cfg(test)]
        let recorded = self
            .test_prime_descriptor_sets
            .map_or(recorded, |n| recorded.min(n));
        for id in &built.sampled_ids {
            store.touch_render_fence(*id, ticket.clone());
        }
        // A truncated compose painted less than the scene: not read, as a
        // truncated priming compose is not.
        if recorded == draws {
            match repaint {
                Repaint::Full(_) => {
                    rb.whole = true;
                    rb.valid = vec![vk::Rect2D {
                        offset: vk::Offset2D::default(),
                        extent,
                    }];
                }
                _ => rb.valid.push(local),
            }
            Ok(Some(rb.target.image))
        } else {
            log::warn!(
                "render root read: output {output_idx} readback composed {recorded} of {draws} \
                 draws (descriptor pool exhausted); not read"
            );
            Ok(None)
        }
    }

    /// Whether [`Self::root_readback`] of `local` would read without
    /// composing, so the caller need not flush pending paint first.
    pub(crate) fn root_readback_is_current(
        &self,
        store: &DrawableStore,
        platform: &PlatformBackend,
        output_idx: usize,
        local: vk::Rect2D,
    ) -> bool {
        let generation = (self.structure_generation, store.scene_damage_generation());
        self.inner
            .as_ref()
            .and_then(|inner| inner.root_readbacks.get(output_idx))
            .and_then(Option::as_ref)
            .is_some_and(|rb| {
                rb.generation == generation
                    && rb.target.extent == root_readback_extent(platform, output_idx)
                    && root_readback_covers(&rb.valid, local)
            })
    }
}
