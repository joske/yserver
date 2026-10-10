use super::*;

pub(super) fn drain_deferred_scene_resources<W, R>(
    pending_pool_releases: &mut VecDeque<(usize, FenceTicket)>,
    failed_submit_bos: &mut VecDeque<FailedSubmitBo>,
    mut wait: W,
    mut release: R,
) where
    W: FnMut(&FenceTicket) -> bool,
    R: FnMut(DeferredSceneRelease) -> bool,
{
    let mut retained_pool_releases = VecDeque::with_capacity(pending_pool_releases.len());
    while let Some((slot, ticket)) = pending_pool_releases.pop_front() {
        if wait(&ticket) && release(DeferredSceneRelease::PoolSlot(slot)) {
            continue;
        }
        retained_pool_releases.push_back((slot, ticket));
    }
    *pending_pool_releases = retained_pool_releases;

    let mut retained_failed_submits = VecDeque::with_capacity(failed_submit_bos.len());
    while let Some(failed) = failed_submit_bos.pop_front() {
        if wait(&failed.ticket)
            && release(DeferredSceneRelease::FailedSubmit {
                bo_idx: failed.bo_idx,
                pool_slot: failed.pool_slot,
            })
        {
            continue;
        }
        retained_failed_submits.push_back(failed);
    }
    *failed_submit_bos = retained_failed_submits;
}

impl BufferAgeRing {
    pub(super) fn new(depth: usize) -> Self {
        Self {
            entries: VecDeque::with_capacity(depth + 1),
            depth,
        }
    }

    /// Push `(gen, region)`. Trims to `depth` entries.
    pub(super) fn push(&mut self, generation: u64, region: RegionSet) {
        self.entries.push_back((generation, region));
        while self.entries.len() > self.depth {
            self.entries.pop_front();
        }
    }

    /// Check whether every generation in `(last_gen+1, frame_gen)`
    /// (exclusive on both sides — those are the intervening
    /// generations between the BO's last present and the
    /// frame we're about to render) is in the ring.
    pub(super) fn contains_all(&self, last_gen: u64, frame_gen: u64) -> bool {
        if frame_gen <= last_gen {
            return true; // shouldn't happen but bail safe
        }
        let want_count = (frame_gen - last_gen - 1) as usize;
        if want_count == 0 {
            // No intervening frames; the BO's content + current
            // damage covers it.
            return true;
        }
        let mut found = 0usize;
        for &(g, _) in &self.entries {
            if g > last_gen && g < frame_gen {
                found += 1;
            }
        }
        found >= want_count
    }

    /// Union all damage regions in `(last_gen+1, frame_gen)` into
    /// `dst`.
    fn union_history_into(&self, last_gen: u64, frame_gen: u64, dst: &mut RegionSet) {
        for (g, r) in &self.entries {
            if *g > last_gen && *g < frame_gen {
                dst.union_with(r);
            }
        }
    }
}

impl SceneCompositor {
    /// Production constructor. Builds the blit pipeline (reuses
    /// v1's CompositorPipeline — same shaders, same descriptor
    /// layout) and one descriptor-pool ring per output.
    ///
    /// # Errors
    ///
    /// `PipelineInit` on shader / pipeline build failure;
    /// `Vk(...)` on descriptor-pool init.
    pub(crate) fn new(platform: &PlatformBackend) -> Result<Self, SceneError> {
        let vk = platform.vk().ok_or(SceneError::NoVk)?.clone();
        let pipeline = CompositorPipeline::new(Arc::clone(&vk), vk::Format::B8G8R8A8_UNORM)
            .map_err(SceneError::PipelineInit)?;
        // Root-overlay XOR pass pipeline cache — built for the same
        // color format the compose color attachment / scanout BO uses
        // (`B8G8R8A8_UNORM`, see `kms::vk::scanout` image-view creation)
        // so the XOR draws are format-compatible with the active compose
        // rendering instance they are recorded into.
        let overlay_xor_cache = crate::kms::vk::logic_fill_pipeline::LogicFillPipelineCache::new(
            Arc::clone(&vk),
            vk::Format::B8G8R8A8_UNORM,
        )?;
        let mut outputs = Vec::with_capacity(platform.outputs.len());
        for i in 0..platform.outputs.len() {
            outputs.push(Self::build_output_state(&vk, platform, i)?);
        }
        let mut inner = SceneCompositorInner {
            vk,
            pipeline,
            scale_pipeline: None,
            overlay_xor_cache,
            outputs,
            damage_audit_ledger: VecDeque::new(),
            damage_audit_next_event_id: 0,
            cursor: None,
            root_readbacks: Vec::new(),
        };
        ensure_intermediates(&mut inner, platform)?;
        Ok(Self {
            inner: Some(inner),
            root_overlay: crate::kms::render::root_overlay::RootOverlay::default(),
            scene_structure_dirty: true,
            structure_generation: 0,
            #[cfg(test)]
            test_flip_in_flight_override: None,
            #[cfg(test)]
            test_prime_descriptor_sets: None,
        })
    }

    fn build_output_state(
        vk: &Arc<crate::kms::vk::device::VkContext>,
        platform: &PlatformBackend,
        i: usize,
    ) -> Result<OutputSceneState, SceneError> {
        let layout = &platform.outputs[i];
        // What the scene walk covers: the footprint of a transformed output.
        let (root_x, root_y, root_w, root_h) = platform.output_root_rect(i);
        let ring = CompositePoolRing::new(Arc::clone(vk), MAX_DESCRIPTOR_SETS_PER_FRAME)
            .map_err(SceneError::Vk)?;
        let bo_depth = platform
            .scanout_pools
            .get(i)
            .and_then(|p| p.as_ref().map(|pool| pool.display_pool().bos.len()))
            .unwrap_or(3);
        Ok(OutputSceneState {
            output_idx: i,
            damage_audit: build_output_damage_audit(
                vk,
                vk::Extent2D {
                    width: root_w,
                    height: root_h,
                },
            )?,
            pool_ring: ring,
            pool_slots: VecDeque::with_capacity(4),
            pending_pool_releases: VecDeque::with_capacity(4),
            pending_acks: VecDeque::with_capacity(4),
            failed_submit_bos: VecDeque::with_capacity(4),
            damage_history: BufferAgeRing::new(bo_depth + 1),
            current_generation: 0,
            scene_structure_damage: RegionSet::new(),
            pending_repaint_after_failed_submit: RegionSet::new(),
            output_extent: vk::Extent2D {
                width: root_w,
                height: root_h,
            },
            output_origin: (root_x, root_y),
            next_submit_retry_at: None,
            last_frame_cursor_mode: OutputCursorMode::Hidden,
            cursor_prev_pos: None,
            last_present_cursor_rect: None,
            last_present_cursor_version: None,
            force_show_retry_version: None,
            last_skip_reason: None,
            // Sized from the *current* pool, exactly as `bo_depth` above is.
            // `rebuild_outputs` replaces every `OutputSceneState`, so this is
            // also how a pool that changed length or identity gets a correctly
            // shaped `missing` vector — see the plan's 3.4.
            prev_presented: Vec::new(),
            last_pieces: std::collections::HashSet::new(),
            presented_epochs: std::collections::HashMap::new(),
            intermediate: None,
            transform: platform.output_transform(i).cloned(),
            cursor_saves: CursorSaves::default(),
            // Per scanout BO, so in mode (BO) pixels even when transformed.
            damage: ScanoutDamage::new(
                bo_depth,
                vk::Extent2D {
                    width: u32::from(layout.width),
                    height: u32::from(layout.height),
                },
            ),
        })
    }

    /// Step 3 — mark every output's scanout BOs wholly stale.
    ///
    /// The safe fallback for lifecycle transitions the per-BO damage model
    /// cannot reason about: it costs one full repaint per output and can never
    /// show a stale pixel. Used by the two backend-side sites that change what
    /// is on screen without going through a compose — `set_logical_screen_size`
    /// (which reallocates root/COW storage but deliberately avoids
    /// `drain_all` + `rebuild_outputs`) and the return from direct scanout
    /// (during which the composed BOs are not painted at all).
    pub(crate) fn invalidate_all_scanout_damage(&mut self) {
        if let Some(inner) = self.inner.as_mut() {
            for o in &mut inner.outputs {
                o.damage.invalidate();
            }
        }
    }

    pub(crate) fn rebuild_outputs(&mut self, platform: &PlatformBackend) -> Result<(), SceneError> {
        let Some(inner) = self.inner.as_mut() else {
            return Ok(());
        };
        let vk = inner.vk.clone();
        let mut outputs = Vec::with_capacity(platform.outputs.len());
        for i in 0..platform.outputs.len() {
            outputs.push(Self::build_output_state(&vk, platform, i)?);
        }
        for old in &mut inner.outputs {
            release_intermediate(old, &vk);
        }
        inner.outputs = outputs;
        // Intermediates come with the next tick or `sync_output_layouts`: a
        // CRTC set rebuilds here before the core applies its new transform.
        self.note_structure_change();
        // root-overlay is root-absolute + layout-dependent; drop it on
        // topology change. Covers both connector hotplug
        // (`fire_randr_changes`) and per-CRTC reconfiguration
        // (`apply_crtc_config`) — the two callers of `rebuild_outputs`.
        self.root_overlay_clear();
        Ok(())
    }

    /// Follow the RANDR transforms in `platform` without a topology rebuild:
    /// an output whose root rect changed gets its scene extent, BO damage and
    /// intermediate redone, one whose matrix alone changed its BO damage; an
    /// identity output keeps today's state untouched.
    pub(crate) fn sync_output_layouts(
        &mut self,
        platform: &PlatformBackend,
    ) -> Result<(), SceneError> {
        let Some(inner) = self.inner.as_mut() else {
            return Ok(());
        };
        let mut changed = false;
        for (i, o) in inner.outputs.iter_mut().enumerate() {
            let (x, y, width, height) = platform.output_root_rect(i);
            let extent = vk::Extent2D { width, height };
            let transform = platform.output_transform(i);
            let wanted = transform.map(|_| extent);
            let held = o.intermediate.as_ref().map(|im| im.extent);
            let same_layout = o.output_origin == (x, y) && o.output_extent == extent;
            if same_layout && held == wanted && o.transform.as_ref() == transform {
                continue;
            }
            changed = true;
            o.transform = transform.cloned();
            if !same_layout || held != wanted {
                // The intermediate holds root pixels: only a new footprint
                // or origin redoes it.
                release_intermediate(o, &inner.vk);
                o.output_origin = (x, y);
                o.output_extent = extent;
                o.damage_audit = build_output_damage_audit(&inner.vk, extent)?;
            }
            // The BOs hold the previous transform's pixels.
            o.damage.invalidate();
            o.prev_presented.clear();
            o.last_pieces.clear();
        }
        ensure_intermediates(inner, platform)?;
        if changed {
            self.note_structure_change();
        }
        Ok(())
    }

    /// Test fixture / Stage-1b-era stub. Construct via
    /// `SceneCompositor::stub()` so the `KmsBackend::for_tests`
    /// path doesn't need Vk.
    pub(crate) fn stub() -> Self {
        Self {
            inner: None,
            root_overlay: crate::kms::render::root_overlay::RootOverlay::default(),
            scene_structure_dirty: false,
            structure_generation: 0,
            #[cfg(test)]
            test_flip_in_flight_override: None,
            #[cfg(test)]
            test_prime_descriptor_sets: None,
        }
    }

    /// Whether the scene has a live blit pipeline. Tests use
    /// this to skip Vk-only assertions.
    pub(crate) fn is_live(&self) -> bool {
        self.inner.is_some()
    }

    /// Drain in-flight compose work before tear-down. Best-effort
    /// — `device_wait_idle` is the safe fallback the platform
    /// uses anyway. Releases descriptor-pool slots so the
    /// pool-ring's Drop doesn't fire while slots are still in use.
    pub(crate) fn drain_all(&mut self, platform: &mut PlatformBackend) {
        let Some(inner) = self.inner.as_mut() else {
            return;
        };
        // Stop readiness delivery before discarding the ledger that owns each
        // job id. The source-completion fd is only a notification handle; the
        // fence ticket below still proves A's submitted command buffer is done
        // before descriptor slots are reset, and the platform subsequently
        // drains both devices before any copied pool is reset or dropped.
        platform.clear_scanout_render_completions();
        let vk = inner.vk.clone();
        for (output_idx, o) in inner.outputs.iter_mut().enumerate() {
            // B.2-context fix (codex audit followup): wait for any
            // in-flight compose fences before resetting their
            // descriptor-pool slots. `disable_output` runs
            // device_wait_idle later, but we hit
            // vkResetDescriptorPool BEFORE that wait. Wait on each
            // ack's ticket here to keep VUID-vkResetDescriptorPool-
            // descriptorPool-00313 satisfied during teardown too.
            let mut retained_acks = VecDeque::with_capacity(o.pending_acks.len());
            let mut retained_slots = VecDeque::with_capacity(o.pool_slots.len());
            while let Some(ack) = o.pending_acks.pop_front() {
                let slot = o.pool_slots.pop_front();
                let wait_ok = ack
                    .ticket
                    .as_ref()
                    .is_none_or(|ticket| match ticket.wait(&vk) {
                        Ok(()) => true,
                        Err(error) => {
                            log::error!(
                                "render scene drain: output {output_idx} compose fence wait \
                                 failed: {error:?}; retaining resources for quarantine"
                            );
                            platform.renderer_failed = true;
                            false
                        }
                    });
                if wait_ok {
                    if let Some(slot) = slot {
                        o.pool_ring.release(slot);
                    }
                } else {
                    retained_acks.push_back(ack);
                    if let Some(slot) = slot {
                        retained_slots.push_back(slot);
                    }
                }
            }
            retained_slots.append(&mut o.pool_slots);
            o.pending_acks = retained_acks;
            o.pool_slots = retained_slots;
            // Step 3 — this pops every ack, and retains any whose fence wait
            // failed, so a staged frame can be discarded or left half-retired.
            // Invalidating is consistent with both, and it is also how suspend,
            // DPMS off/on and the topology quiesce get covered: all three run
            // `drain_all` first.
            o.damage.invalidate();
            let pending_pool_releases = &mut o.pending_pool_releases;
            let failed_submit_bos = &mut o.failed_submit_bos;
            let pool_ring = &mut o.pool_ring;
            let mut wait_failed = false;
            let mut recovery_failed = false;
            drain_deferred_scene_resources(
                pending_pool_releases,
                failed_submit_bos,
                |ticket| match ticket.wait(&vk) {
                    Ok(()) => true,
                    Err(error) => {
                        log::error!(
                            "render scene drain: deferred compose fence wait failed: \
                             {error:?}; retaining resources for quarantine"
                        );
                        wait_failed = true;
                        false
                    }
                },
                |release| match release {
                    DeferredSceneRelease::PoolSlot(slot) => {
                        pool_ring.release(slot);
                        true
                    }
                    DeferredSceneRelease::FailedSubmit { bo_idx, pool_slot } => {
                        match platform.recycle_failed_submit_bo(output_idx, bo_idx) {
                            Ok(()) => {
                                pool_ring.release(pool_slot);
                                true
                            }
                            Err(error) => {
                                log::error!(
                                    "render scene drain: failed to recover output {output_idx} \
                                     bo {bo_idx}: {error}"
                                );
                                recovery_failed = true;
                                false
                            }
                        }
                    }
                },
            );
            if wait_failed || recovery_failed {
                platform.renderer_failed = true;
            }
            // Stage 5 Phase D' — global recovery: reset every
            // output's cursor mode to Hidden. The post-recovery
            // first compose re-decides via build_scene's
            // strategy. cursor_prev_pos is also cleared so the
            // next frame doesn't damage a stale trail rect.
            reset_cursor_mode_for_lifecycle(&mut o.last_frame_cursor_mode);
            o.cursor_prev_pos = None;
            o.last_present_cursor_rect = None;
            o.last_present_cursor_version = None;
            reset_cursor_retry_for_lifecycle(&mut o.force_show_retry_version);
        }
        // Hide the plane everywhere + invalidate uploaded_version.
        // Best-effort; the platform hook logs per-CRTC failures.
        let _ = platform.cursor_plane_hide_all();
    }
}
