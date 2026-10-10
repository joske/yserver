use super::*;

impl DamageAuditTarget {
    pub(super) fn new(
        vk: Arc<crate::kms::vk::device::VkContext>,
        extent: vk::Extent2D,
    ) -> Result<Self, vk::Result> {
        let info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk::Format::B8G8R8A8_UNORM)
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
                    | vk::ImageUsageFlags::TRANSFER_SRC
                    | vk::ImageUsageFlags::STORAGE,
            )
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let image = unsafe { vk.device.create_image(&info, None)? };
        let requirements = unsafe { vk.device.get_image_memory_requirements(image) };
        let properties = unsafe {
            vk.instance
                .get_physical_device_memory_properties(vk.physical_device)
        };
        let memory_type_index = (0..properties.memory_type_count).find(|&index| {
            requirements.memory_type_bits & (1 << index) != 0
                && properties.memory_types[index as usize]
                    .property_flags
                    .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
        });
        let Some(memory_type_index) = memory_type_index else {
            unsafe { vk.device.destroy_image(image, None) };
            return Err(vk::Result::ERROR_FEATURE_NOT_PRESENT);
        };
        let allocation = vk::MemoryAllocateInfo::default()
            .allocation_size(requirements.size)
            .memory_type_index(memory_type_index);
        let memory = match crate::kms::vk::mem_accounting::allocate_memory(
            &vk.device,
            &allocation,
            crate::kms::vk::mem_accounting::MemCategory::Other,
            &properties,
        ) {
            Ok(memory) => memory,
            Err(error) => {
                unsafe { vk.device.destroy_image(image, None) };
                return Err(error);
            }
        };
        if let Err(error) = unsafe { vk.device.bind_image_memory(image, memory, 0) } {
            unsafe {
                vk.device.destroy_image(image, None);
                crate::kms::vk::mem_accounting::free_memory(&vk.device, memory);
            }
            return Err(error);
        }
        let view_info = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(vk::Format::B8G8R8A8_UNORM)
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .level_count(1)
                    .layer_count(1),
            );
        let view = match unsafe { vk.device.create_image_view(&view_info, None) } {
            Ok(view) => view,
            Err(error) => {
                unsafe {
                    vk.device.destroy_image(image, None);
                    crate::kms::vk::mem_accounting::free_memory(&vk.device, memory);
                }
                return Err(error);
            }
        };
        let pool_info = vk::CommandPoolCreateInfo::default()
            .queue_family_index(vk.graphics_queue_family)
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
        let command_pool = match unsafe { vk.device.create_command_pool(&pool_info, None) } {
            Ok(pool) => pool,
            Err(error) => {
                unsafe {
                    vk.device.destroy_image_view(view, None);
                    vk.device.destroy_image(image, None);
                    crate::kms::vk::mem_accounting::free_memory(&vk.device, memory);
                }
                return Err(error);
            }
        };
        let cb_info = vk::CommandBufferAllocateInfo::default()
            .command_pool(command_pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        let command_buffer = match unsafe { vk.device.allocate_command_buffers(&cb_info) } {
            Ok(buffers) => buffers[0],
            Err(error) => {
                unsafe {
                    vk.device.destroy_command_pool(command_pool, None);
                    vk.device.destroy_image_view(view, None);
                    vk.device.destroy_image(image, None);
                    crate::kms::vk::mem_accounting::free_memory(&vk.device, memory);
                }
                return Err(error);
            }
        };
        let timestamp_pool = if vk.timestamp_period > 0.0 {
            let info = vk::QueryPoolCreateInfo::default()
                .query_type(vk::QueryType::TIMESTAMP)
                .query_count(2);
            unsafe { vk.device.create_query_pool(&info, None) }.unwrap_or(vk::QueryPool::null())
        } else {
            vk::QueryPool::null()
        };
        Ok(Self {
            vk,
            image,
            view,
            memory,
            extent,
            command_pool,
            command_buffer,
            timestamp_pool,
            timestamps_written: false,
            last_gpu_render_ns: None,
        })
    }
}

impl Drop for DamageAuditTarget {
    fn drop(&mut self) {
        if self.vk.requires_drop_device_idle() {
            let wait = unsafe { self.vk.device.device_wait_idle() };
            if !matches!(wait, Ok(()) | Err(vk::Result::ERROR_DEVICE_LOST)) {
                log::warn!(
                    "damage audit target: vkDeviceWaitIdle failed during teardown: {wait:?}; \
                     leaking uncertain target resources"
                );
                std::mem::forget(Arc::clone(&self.vk));
                return;
            }
        }
        unsafe {
            if self.timestamp_pool != vk::QueryPool::null() {
                self.vk.device.destroy_query_pool(self.timestamp_pool, None);
            }
            self.vk.device.destroy_command_pool(self.command_pool, None);
            self.vk.device.destroy_image_view(self.view, None);
            self.vk.device.destroy_image(self.image, None);
            crate::kms::vk::mem_accounting::free_memory(&self.vk.device, self.memory);
        }
    }
}

pub(super) fn build_output_damage_audit(
    vk: &Arc<crate::kms::vk::device::VkContext>,
    extent: vk::Extent2D,
) -> Result<Option<OutputDamageAudit>, SceneError> {
    if !damage_audit_enabled() {
        return Ok(None);
    }
    if !DamageAuditComparePipeline::is_supported(vk, extent.width, extent.height) {
        log::warn!(
            "damage-audit: unavailable for {}x{} on this Vulkan context",
            extent.width,
            extent.height
        );
        return Ok(None);
    }
    let candidate = DamageAuditTarget::new(Arc::clone(vk), extent).map_err(SceneError::Vk)?;
    let reference = DamageAuditTarget::new(Arc::clone(vk), extent).map_err(SceneError::Vk)?;
    let compare = DamageAuditComparePipeline::new(Arc::clone(vk), extent.width, extent.height)
        .map_err(SceneError::Vk)?;
    log::info!(
        "damage-audit: enabled output extent={}x{} grid={}x{} interval={}",
        extent.width,
        extent.height,
        compare.grid_width(),
        compare.grid_height(),
        damage_audit_interval()
    );
    Ok(Some(OutputDamageAudit {
        candidate,
        reference,
        compare,
        initialized: false,
        frame: 0,
        consumed_event_id: 0,
        active_episodes: HashMap::new(),
        episodes_opened: 0,
        episodes_healed: 0,
        reset_count: 0,
        comparisons: 0,
        seed_draws: 0,
        frame_draws: 0,
        seed_sampled: Vec::new(),
        frame_sampled: Vec::new(),
        clipped_gpu_ns: 0,
        full_gpu_ns: 0,
        gpu_samples: 0,
        comparisons_idle: 0,
        comparisons_partial: 0,
        comparisons_full: 0,
        damage_pixels: 0,
        damage_frames: 0,
        last_compare_at: None,
        last_heartbeat_at: None,
    }))
}

impl SceneCompositor {
    pub(super) fn full_output_audit_area(&self) -> Vec<vk::Rect2D> {
        self.inner
            .as_ref()
            .map(|inner| {
                inner
                    .outputs
                    .iter()
                    .map(|output| vk::Rect2D {
                        offset: vk::Offset2D::default(),
                        extent: output.output_extent,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(super) fn record_damage_audit_event(
        &mut self,
        site: &'static Location<'static>,
        expected_area: Vec<vk::Rect2D>,
    ) -> Option<u64> {
        if !self.damage_audit_active() {
            return None;
        }
        let inner = self.inner.as_mut()?;
        let id = inner.damage_audit_next_event_id;
        inner.damage_audit_next_event_id = inner.damage_audit_next_event_id.saturating_add(1);
        inner.damage_audit_ledger.push_back(DamageAuditLedgerEntry {
            id,
            site,
            expected_area,
            contributed_outputs: Vec::new(),
        });
        bound_damage_audit_ledger(inner);
        Some(id)
    }

    pub(super) fn damage_audit_active(&self) -> bool {
        damage_audit_enabled()
            && self.inner.as_ref().is_some_and(|inner| {
                inner
                    .outputs
                    .iter()
                    .any(|output| output.damage_audit.is_some())
            })
    }
}

pub(super) fn damage_audit_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        matches!(
            std::env::var("YSERVER_DAMAGE_AUDIT").ok().as_deref(),
            Some("1") | Some("true") | Some("TRUE") | Some("yes") | Some("YES")
        )
    })
}

fn damage_audit_interval() -> u64 {
    static INTERVAL: OnceLock<u64> = OnceLock::new();
    *INTERVAL.get_or_init(|| {
        std::env::var("YSERVER_DAMAGE_AUDIT_INTERVAL")
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|&n| n > 0)
            .unwrap_or(1)
    })
}

/// Seconds between idle re-comparisons. At true idle no transition is
/// recorded, so the event-gated empty-damage hook never fires and the
/// candidate is never checked. Without this a stale divergence simply
/// stops being reported the moment the desktop goes quiet, and a static
/// soak — the primary gate — cannot produce evidence either way.
fn damage_audit_idle_recompare_secs() -> u64 {
    static SECS: OnceLock<u64> = OnceLock::new();
    *SECS.get_or_init(|| {
        std::env::var("YSERVER_DAMAGE_AUDIT_IDLE_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|&n| n > 0)
            .unwrap_or(1)
    })
}

/// Frame at which the candidate is first seeded. Seeding at frame 1 means
/// the candidate samples drawable storage during startup churn, so a paint
/// whose GPU work lands after the seed — but whose damage was already
/// consumed that same frame — latches a stale read the candidate can never
/// correct. Delaying the seed separates that from a genuine damage hole:
/// if a divergence still appears at the first compared frame after a late
/// seed, the damage really is incomplete.
fn damage_audit_seed_frame() -> u64 {
    static SEED: OnceLock<u64> = OnceLock::new();
    *SEED.get_or_init(|| {
        std::env::var("YSERVER_DAMAGE_AUDIT_SEED_FRAME")
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|&n| n > 0)
            .unwrap_or(1)
    })
}

/// Whether this output's candidate is due an idle re-comparison.
pub(super) fn audit_idle_recompare_due(inner: &SceneCompositorInner, output_idx: usize) -> bool {
    let Some(audit) = inner
        .outputs
        .get(output_idx)
        .and_then(|o| o.damage_audit.as_ref())
    else {
        return false;
    };
    if !audit.initialized {
        return false;
    }
    let due = std::time::Duration::from_secs(damage_audit_idle_recompare_secs());
    audit.last_compare_at.is_none_or(|at| at.elapsed() >= due)
}

/// Mean repaint-bbox area as a fraction of the output, over non-idle
/// comparisons. Near 1.0 means the run was almost all whole-output
/// repaints and says nothing about damage completeness.
fn mean_damage_fraction(audit: &OutputDamageAudit) -> f64 {
    if audit.damage_frames == 0 {
        return 0.0;
    }
    let area = u128::from(audit.candidate.extent.width) * u128::from(audit.candidate.extent.height);
    let denom = area.saturating_mul(u128::from(audit.damage_frames));
    if denom == 0 {
        return 0.0;
    }
    #[allow(clippy::cast_precision_loss)]
    let fraction = (audit.damage_pixels as f64) / (denom as f64);
    fraction
}

#[allow(clippy::cast_precision_loss)]
fn mean_us(total_ns: u128, samples: u64) -> f64 {
    if samples == 0 {
        return 0.0;
    }
    (total_ns as f64) / (samples as f64) / 1000.0
}

/// Periodic proof-of-life. A clean run is only meaningful if the audit
/// can be shown to have actually looked; a silent log is otherwise
/// indistinguishable between "running and clean" and "not running".
pub(super) fn emit_damage_audit_heartbeat(inner: &mut SceneCompositorInner) {
    const HEARTBEAT: std::time::Duration = std::time::Duration::from_secs(5);
    for output_idx in 0..inner.outputs.len() {
        let Some(audit) = inner.outputs[output_idx].damage_audit.as_mut() else {
            continue;
        };
        if audit
            .last_heartbeat_at
            .is_some_and(|at| at.elapsed() < HEARTBEAT)
        {
            continue;
        }
        audit.last_heartbeat_at = Some(std::time::Instant::now());
        log::info!(
            "damage-audit\theartbeat\toutput={output_idx}\tframe={}\tcomparisons={}\
             \tidle={}\tpartial={}\tfull={}\tmean_damage={:.3}\
             \tclipped_us={:.1}\tfull_us={:.1}\tgpu_n={}\
             \tepisodes_open={}\tepisodes_opened={}\tepisodes_healed={}\tresets={}",
            audit.frame,
            audit.comparisons,
            audit.comparisons_idle,
            audit.comparisons_partial,
            audit.comparisons_full,
            mean_damage_fraction(audit),
            mean_us(audit.clipped_gpu_ns, audit.gpu_samples),
            mean_us(audit.full_gpu_ns, audit.gpu_samples),
            audit.gpu_samples,
            audit.active_episodes.len(),
            audit.episodes_opened,
            audit.episodes_healed,
            audit.reset_count,
        );
    }
}

pub(super) fn note_damage_audit_contributions(
    inner: &mut SceneCompositorInner,
    event_id: u64,
    output_indices: &[usize],
) {
    if !damage_audit_enabled() {
        return;
    }
    if let Some(entry) = inner
        .damage_audit_ledger
        .iter_mut()
        .find(|entry| entry.id == event_id)
    {
        for &output_idx in output_indices {
            if !entry.contributed_outputs.contains(&output_idx) {
                entry.contributed_outputs.push(output_idx);
            }
        }
    }
}

fn bound_damage_audit_ledger(inner: &mut SceneCompositorInner) {
    const MAX_LEDGER_ENTRIES: usize = 16_384;
    while inner.damage_audit_ledger.len() > MAX_LEDGER_ENTRIES {
        if let Some(entry) = inner.damage_audit_ledger.pop_front() {
            log::warn!(
                "damage-audit: ledger bound hit; dropped oldest event id={} site={}:{}; run suspect",
                entry.id,
                entry.site.file(),
                entry.site.line()
            );
        }
    }
}

fn retire_damage_audit_ledger(inner: &mut SceneCompositorInner) {
    let min_consumed = inner
        .outputs
        .iter()
        .filter_map(|output| {
            output
                .damage_audit
                .as_ref()
                .map(|audit| audit.consumed_event_id)
        })
        .min();
    let Some(min_consumed) = min_consumed else {
        return;
    };
    while inner
        .damage_audit_ledger
        .front()
        .is_some_and(|entry| entry.id < min_consumed)
    {
        inner.damage_audit_ledger.pop_front();
    }
}

pub(super) fn audit_has_unretired_event(inner: &SceneCompositorInner, output_idx: usize) -> bool {
    let consumed = inner.outputs[output_idx]
        .damage_audit
        .as_ref()
        .map(|audit| audit.consumed_event_id)
        .unwrap_or(0);
    inner
        .damage_audit_ledger
        .back()
        .is_some_and(|entry| entry.id >= consumed)
}

pub(super) fn audit_overlay_pipeline(
    inner: &mut SceneCompositorInner,
    needed: bool,
) -> Result<(vk::Pipeline, vk::PipelineLayout), SceneError> {
    if !needed {
        return Ok((vk::Pipeline::null(), vk::PipelineLayout::null()));
    }
    let pipeline = inner.overlay_xor_cache.get(
        yserver_core::backend::GcFunction::Xor,
        crate::kms::vk::logic_fill_pipeline::LogicFillChannels::Color,
    )?;
    Ok((pipeline, inner.overlay_xor_cache.pipeline_layout()))
}

/// Diagnostic: pair each sampled drawable with its xid so a damage-audit
/// mismatch can name the drawable whose contents changed.
pub(super) fn audit_sampled_pairs(
    store: &DrawableStore,
    sampled_ids: &[crate::kms::render::store::DrawableId],
) -> Vec<(u64, u32)> {
    if !damage_audit_enabled() {
        return Vec::new();
    }
    sampled_ids
        .iter()
        .map(|id| {
            let xid = store
                .xid_entries()
                .find_map(|(xid, entry)| (entry == *id).then_some(xid))
                .unwrap_or(0);
            (id.as_u64(), xid)
        })
        .collect()
}

/// Step 1 — the damage audit's reference scene: the same frame built with
/// `Visibility::Off`, so the reference paints every node's full placement while
/// the candidate paints what the visibility walk left. Only built when the audit
/// is armed (one extra walk per audited frame). Mirrors the production build's
/// software-cursor decision so the two scenes differ in visibility alone.
#[allow(clippy::too_many_arguments)]
pub(super) fn audit_reference_scene(
    production_has_sw_cursor: bool,
    core: &KmsCore,
    store: &mut DrawableStore,
    windows: &crate::kms::render::backend::WindowsMap,
    output_idx: usize,
    platform: &PlatformBackend,
    cursor: Option<CursorEntry>,
    cursor_prev_pos: Option<(i32, i32)>,
    cow_host_xid: Option<u32>,
    hw_strategy_active: bool,
) -> Option<SceneBuild> {
    if !damage_audit_enabled() {
        return None;
    }
    let mut reference = build_scene(
        core,
        store,
        windows,
        output_idx,
        platform,
        cursor,
        cursor_prev_pos,
        cow_host_xid,
        hw_strategy_active,
        Visibility::Off,
    );
    if !production_has_sw_cursor {
        // Either the production frame had no software cursor or the tick
        // stripped it for a hide frame; either way the reference must not
        // carry one.
        reference.omit_software_cursor_for_hide();
    }
    Some(reference)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn run_damage_audit(
    inner: &mut SceneCompositorInner,
    output_idx: usize,
    platform: &mut PlatformBackend,
    scene: &CompositeScene,
    // Step 1 — the scene the REFERENCE composes: built with `Visibility::Off`,
    // i.e. every node's full placement. Candidate and reference used to render
    // the same list, so a visibility bug that hides pixels would have passed
    // clean on both sides and the audit would be vacuous for step 1. With the
    // unclipped reference, a pixel the visibility walk wrongly culls shows up
    // as a mismatch. Identical to `scene` when the audit is not armed.
    reference_scene: &CompositeScene,
    sampled: &[(u64, u32)],
    output_damage: &RegionSet,
    reset_reason: Option<&str>,
    compare_after_empty_damage: bool,
    overlay_ops: &[(u32, vk::Rect2D)],
    xor_pipeline: vk::Pipeline,
    xor_layout: vk::PipelineLayout,
) -> Result<(), SceneError> {
    if !damage_audit_enabled() {
        return Ok(());
    }
    if platform.output_transform(output_idx).is_some() {
        log::debug!(
            "damage-audit: output {output_idx} skipped; transformed scanout is out of scope"
        );
        return Ok(());
    }
    if !matches!(
        platform
            .scanout_pools
            .get(output_idx)
            .and_then(Option::as_ref),
        Some(OutputScanout::Shared(_))
    ) {
        log::debug!("damage-audit: output {output_idx} skipped; copied scanout is out of scope");
        return Ok(());
    }

    let latest_event_id = inner.damage_audit_next_event_id;
    let Some(audit) = inner.outputs[output_idx].damage_audit.as_mut() else {
        return Ok(());
    };
    if let Some(reason) = reset_reason {
        audit.initialized = false;
        audit.active_episodes.clear();
        audit.reset_count = audit.reset_count.saturating_add(1);
        log::info!(
            "damage-audit\treset\toutput={output_idx}\tframe={}\treason={reason}\tresets={}",
            audit.frame,
            audit.reset_count
        );
    }

    audit.frame = audit.frame.saturating_add(1);
    let frame = audit.frame;
    let interval = damage_audit_interval();
    let compare_this_frame = interval == 1 || frame.is_multiple_of(interval);

    let mut just_seeded = false;

    // Hold off seeding until the configured frame so the candidate is not
    // captured mid-startup. Events are still consumed so the ledger does
    // not accumulate across the delay.
    if !audit.initialized && frame < damage_audit_seed_frame() {
        audit.consumed_event_id = latest_event_id;
        retire_damage_audit_ledger(inner);
        return Ok(());
    }

    if !audit.initialized {
        let candidate_extent = audit.candidate.extent;
        let complete = submit_audit_compose(
            &inner.vk,
            platform,
            &inner.pipeline,
            &mut audit.candidate,
            scene,
            Repaint::Full(candidate_extent),
            overlay_ops,
            xor_pipeline,
            xor_layout,
        )?;
        if !complete {
            audit.initialized = false;
            audit.consumed_event_id = latest_event_id;
            log::warn!(
                "damage-audit\treset\toutput={output_idx}\tframe={frame}\
                 \treason=partial-seed-compose\trun_suspect=true"
            );
            retire_damage_audit_ledger(inner);
            return Ok(());
        }
        audit.initialized = true;
        audit.seed_draws = scene.draws.len();
        audit.seed_sampled = sampled.to_vec();
        log::info!(
            "damage-audit\tseed\toutput={output_idx}\tframe={frame}\tdraws={}\tsampled={:?}",
            scene.draws.len(),
            sampled,
        );
        // Fall through to compose the reference and compare on this very
        // frame. Both images are then full composes of the same scene at
        // the same instant, so a mismatch HERE cannot be a damage hole —
        // it means the two composes disagree, i.e. the compose sampled
        // drawable storage whose paint had not landed. That is the only
        // clean way to separate a startup sampling artefact from a real
        // hole straddled by the seed.
        just_seeded = true;
    }

    if !compare_after_empty_damage && !just_seeded {
        let Some(candidate_repaint) = output_damage
            .bounding_rect()
            .map(Repaint::AuditClearClipped)
        else {
            log::warn!(
                "damage-audit\tskip\toutput={output_idx}\tframe={frame}\
                 \treason=empty-candidate-damage-on-compose-path\trun_suspect=true"
            );
            return Ok(());
        };
        let complete = submit_audit_compose(
            &inner.vk,
            platform,
            &inner.pipeline,
            &mut audit.candidate,
            scene,
            candidate_repaint,
            overlay_ops,
            xor_pipeline,
            xor_layout,
        )?;
        if !complete {
            audit.initialized = false;
            audit.active_episodes.clear();
            audit.consumed_event_id = latest_event_id;
            log::warn!(
                "damage-audit\treset\toutput={output_idx}\tframe={frame}\
                 \treason=partial-candidate-compose\trun_suspect=true"
            );
            retire_damage_audit_ledger(inner);
            return Ok(());
        }
    }

    if !compare_this_frame && !just_seeded {
        audit.consumed_event_id = latest_event_id;
        retire_damage_audit_ledger(inner);
        return Ok(());
    }

    let reference_extent = audit.reference.extent;
    let complete = submit_audit_compose(
        &inner.vk,
        platform,
        &inner.pipeline,
        &mut audit.reference,
        reference_scene,
        Repaint::Full(reference_extent),
        overlay_ops,
        xor_pipeline,
        xor_layout,
    )?;
    if !complete {
        audit.initialized = false;
        audit.active_episodes.clear();
        audit.consumed_event_id = latest_event_id;
        log::warn!(
            "damage-audit\treset\toutput={output_idx}\tframe={frame}\
             \treason=partial-reference-compose\trun_suspect=true"
        );
        retire_damage_audit_ledger(inner);
        return Ok(());
    }

    // Classify this comparison before running it — see the field docs on
    // `comparisons_full`. `compare_after_empty_damage` is the idle path,
    // where the candidate is deliberately left untouched and a match is a
    // genuine retention test.
    let output_area =
        u128::from(audit.candidate.extent.width) * u128::from(audit.candidate.extent.height);
    if compare_after_empty_damage {
        audit.comparisons_idle = audit.comparisons_idle.saturating_add(1);
    } else {
        let bbox = output_damage.bounding_rect().map_or(0u128, |r| {
            u128::from(r.extent.width) * u128::from(r.extent.height)
        });
        if bbox >= output_area {
            audit.comparisons_full = audit.comparisons_full.saturating_add(1);
        } else {
            audit.comparisons_partial = audit.comparisons_partial.saturating_add(1);
        }
        audit.damage_pixels = audit.damage_pixels.saturating_add(bbox);
        audit.damage_frames = audit.damage_frames.saturating_add(1);
    }

    if !compare_after_empty_damage
        && !just_seeded
        && let (Some(clipped), Some(full)) = (
            audit.candidate.last_gpu_render_ns,
            audit.reference.last_gpu_render_ns,
        )
    {
        audit.clipped_gpu_ns = audit.clipped_gpu_ns.saturating_add(u128::from(clipped));
        audit.full_gpu_ns = audit.full_gpu_ns.saturating_add(u128::from(full));
        audit.gpu_samples = audit.gpu_samples.saturating_add(1);
    }

    submit_damage_audit_compare(&inner.vk, platform, audit)?;
    audit.frame_draws = scene.draws.len();
    audit.frame_sampled = sampled.to_vec();
    audit.comparisons = audit.comparisons.saturating_add(1);
    audit.last_compare_at = Some(std::time::Instant::now());
    let summaries = audit.compare.read_summary().map_err(SceneError::Vk)?;
    process_damage_audit_summary(
        output_idx,
        audit,
        &inner.damage_audit_ledger,
        &summaries,
        audit.consumed_event_id,
        latest_event_id,
        interval,
        just_seeded,
    );
    audit.consumed_event_id = latest_event_id;
    retire_damage_audit_ledger(inner);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn submit_audit_compose(
    vk: &crate::kms::vk::device::VkContext,
    platform: &PlatformBackend,
    pipeline: &CompositorPipeline,
    target: &mut DamageAuditTarget,
    scene: &CompositeScene,
    repaint: Repaint,
    overlay_ops: &[(u32, vk::Rect2D)],
    xor_pipeline: vk::Pipeline,
    xor_layout: vk::PipelineLayout,
) -> Result<bool, SceneError> {
    let descriptor_pool = create_audit_descriptor_pool(vk, scene.draws.len())?;
    let ticket = platform.acquire_fence_ticket().map_err(SceneError::Vk)?;
    let mut gpu_submitted = false;
    let result = record_and_submit_render(
        vk,
        target,
        pipeline,
        descriptor_pool,
        scene,
        repaint,
        &[],
        ticket.fence(),
        &mut gpu_submitted,
        overlay_ops,
        xor_pipeline,
        xor_layout,
        None,
        None,
    );
    let wait = if result.is_ok() {
        ticket.wait(vk).map_err(SceneError::Vk)
    } else {
        Ok(())
    };
    unsafe {
        vk.device.destroy_descriptor_pool(descriptor_pool, None);
    }
    let submitted = result?;
    wait?;
    Ok(compose_submit_was_complete(submitted, scene.draws.len()))
}

pub(super) fn create_audit_descriptor_pool(
    vk: &crate::kms::vk::device::VkContext,
    draw_count: usize,
) -> Result<vk::DescriptorPool, SceneError> {
    let count = u32::try_from(draw_count.max(1)).unwrap_or(u32::MAX);
    let pool_sizes = [vk::DescriptorPoolSize {
        ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
        descriptor_count: count,
    }];
    let pool_info = vk::DescriptorPoolCreateInfo::default()
        .max_sets(count)
        .pool_sizes(&pool_sizes);
    unsafe { vk.device.create_descriptor_pool(&pool_info, None) }.map_err(SceneError::Vk)
}

fn submit_damage_audit_compare(
    vk: &crate::kms::vk::device::VkContext,
    platform: &PlatformBackend,
    audit: &mut OutputDamageAudit,
) -> Result<(), SceneError> {
    let cb = audit.candidate.command_buffer;
    let ticket = platform.acquire_fence_ticket().map_err(SceneError::Vk)?;
    unsafe {
        vk.device
            .reset_command_buffer(cb, vk::CommandBufferResetFlags::empty())
            .map_err(SceneError::Vk)?;
        let begin = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        vk.device
            .begin_command_buffer(cb, &begin)
            .map_err(SceneError::Vk)?;

        let to_transfer = [
            image_general_to_transfer_src_barrier(audit.candidate.image),
            image_general_to_transfer_src_barrier(audit.reference.image),
        ];
        vk.device.cmd_pipeline_barrier2(
            cb,
            &vk::DependencyInfo::default().image_memory_barriers(&to_transfer),
        );

        let extent = vk::Extent3D {
            width: audit.candidate.extent.width,
            height: audit.candidate.extent.height,
            depth: 1,
        };
        let copy = [vk::BufferImageCopy::default()
            .image_subresource(
                vk::ImageSubresourceLayers::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .layer_count(1),
            )
            .image_extent(extent)];
        vk.device.cmd_copy_image_to_buffer(
            cb,
            audit.candidate.image,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            audit.compare.candidate_buffer(),
            &copy,
        );
        vk.device.cmd_copy_image_to_buffer(
            cb,
            audit.reference.image,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            audit.compare.reference_buffer(),
            &copy,
        );

        audit.compare.record_after_transfers(cb);

        let to_general = [
            image_transfer_src_to_general_barrier(audit.candidate.image),
            image_transfer_src_to_general_barrier(audit.reference.image),
        ];
        vk.device.cmd_pipeline_barrier2(
            cb,
            &vk::DependencyInfo::default().image_memory_barriers(&to_general),
        );

        vk.device.end_command_buffer(cb).map_err(SceneError::Vk)?;
        let cb_info = [vk::CommandBufferSubmitInfo::default().command_buffer(cb)];
        let submit = [vk::SubmitInfo2::default().command_buffer_infos(&cb_info)];
        crate::kms::vk::submit_stats::timed(
            crate::kms::vk::submit_stats::SubmitCause::Other,
            1,
            false,
            || {
                vk.device
                    .queue_submit2(vk.graphics_queue, &submit, ticket.fence())
            },
        )
        .map_err(SceneError::Vk)?;
    }
    ticket.wait(vk).map_err(SceneError::Vk)
}

fn image_general_to_transfer_src_barrier(image: vk::Image) -> vk::ImageMemoryBarrier2<'static> {
    vk::ImageMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
        .src_access_mask(vk::AccessFlags2::MEMORY_WRITE)
        .dst_stage_mask(vk::PipelineStageFlags2::COPY)
        .dst_access_mask(vk::AccessFlags2::TRANSFER_READ)
        .old_layout(vk::ImageLayout::GENERAL)
        .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
        .image(image)
        .subresource_range(
            vk::ImageSubresourceRange::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .level_count(1)
                .layer_count(1),
        )
}

fn image_transfer_src_to_general_barrier(image: vk::Image) -> vk::ImageMemoryBarrier2<'static> {
    vk::ImageMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::COPY)
        .src_access_mask(vk::AccessFlags2::TRANSFER_READ)
        .dst_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
        .dst_access_mask(vk::AccessFlags2::MEMORY_READ | vk::AccessFlags2::MEMORY_WRITE)
        .old_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
        .new_layout(vk::ImageLayout::GENERAL)
        .image(image)
        .subresource_range(
            vk::ImageSubresourceRange::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .level_count(1)
                .layer_count(1),
        )
}

fn process_damage_audit_summary(
    output_idx: usize,
    audit: &mut OutputDamageAudit,
    ledger: &VecDeque<DamageAuditLedgerEntry>,
    summaries: &[DamageAuditTileSummary],
    first_event_id: u64,
    latest_event_id: u64,
    interval: u64,
    at_seed: bool,
) {
    let mut mismatched_tiles = HashSet::new();
    let grid_width = audit.compare.grid_width();
    let grid_height = audit.compare.grid_height();
    for summary in summaries {
        if summary.mismatch_count == 0 {
            continue;
        }
        mismatched_tiles.insert(summary.tile_id);
        if audit.active_episodes.contains_key(&summary.tile_id) {
            continue;
        }
        audit.episodes_opened = audit.episodes_opened.saturating_add(1);
        let start = DamageAuditEpisodeStart {
            frame: audit.frame,
            first_event_id,
            next_event_id: latest_event_id,
        };
        audit.active_episodes.insert(summary.tile_id, start);
        let tile_rect = tile_rect_for_id(
            audit.candidate.extent,
            grid_width,
            grid_height,
            summary.tile_id,
        );
        let candidates = ledger_candidates_for_tile(
            ledger,
            output_idx,
            tile_rect,
            first_event_id,
            latest_event_id,
        );
        let first_x = summary.first_pixel_index % audit.candidate.extent.width;
        let first_y = summary.first_pixel_index / audit.candidate.extent.width;
        log::warn!(
            "damage-audit\tmismatch\toutput={output_idx}\tframe={}\ttile={}\tpixel={},{}\
             \tcount={}\tcandidate=0x{:08x}\treference=0x{:08x}\tseed_draws={}\tdraws={}\
             \tseed_sampled={:?}\tsampled={:?}\
             \tledger={}\tinterval={}\tqualifies={}\tat_seed={}",
            audit.frame,
            summary.tile_id,
            first_x,
            first_y,
            summary.mismatch_count,
            summary.candidate,
            summary.reference,
            audit.seed_draws,
            audit.frame_draws,
            audit.seed_sampled,
            audit.frame_sampled,
            candidates,
            interval,
            interval == 1,
            at_seed,
        );
    }

    let healed: Vec<u32> = audit
        .active_episodes
        .keys()
        .copied()
        .filter(|tile| !mismatched_tiles.contains(tile))
        .collect();
    for tile in healed {
        if let Some(start) = audit.active_episodes.remove(&tile) {
            audit.episodes_healed = audit.episodes_healed.saturating_add(1);
            log::info!(
                "damage-audit\thealed\toutput={output_idx}\tframe={}\ttile={tile}\
                 \tstart_frame={}\tstart_event={}\thealed={}",
                audit.frame,
                start.frame,
                start.first_event_id,
                audit.episodes_healed
            );
        }
    }
}

pub(super) fn ledger_candidates_for_tile(
    ledger: &VecDeque<DamageAuditLedgerEntry>,
    output_idx: usize,
    tile: vk::Rect2D,
    first_event_id: u64,
    next_event_id: u64,
) -> String {
    let mut out = String::new();
    for entry in ledger {
        if entry.id < first_event_id || entry.id >= next_event_id {
            continue;
        }
        if !entry
            .expected_area
            .iter()
            .any(|expected| rects_intersect(*expected, tile))
        {
            continue;
        }
        if !out.is_empty() {
            out.push(',');
        }
        let contributed = entry.contributed_outputs.contains(&output_idx);
        let _ = std::fmt::Write::write_fmt(
            &mut out,
            format_args!(
                "{}@{}:{}:{}",
                entry.id,
                entry.site.file(),
                entry.site.line(),
                if contributed { "contrib" } else { "missing" }
            ),
        );
    }
    if out.is_empty() {
        "none".to_string()
    } else {
        out
    }
}

pub(super) fn rects_intersect(a: vk::Rect2D, b: vk::Rect2D) -> bool {
    let ax1 = a.offset.x.saturating_add_unsigned(a.extent.width);
    let ay1 = a.offset.y.saturating_add_unsigned(a.extent.height);
    let bx1 = b.offset.x.saturating_add_unsigned(b.extent.width);
    let by1 = b.offset.y.saturating_add_unsigned(b.extent.height);
    a.offset.x < bx1 && b.offset.x < ax1 && a.offset.y < by1 && b.offset.y < ay1
}

fn tile_rect_for_id(
    extent: vk::Extent2D,
    grid_width: u32,
    grid_height: u32,
    tile_id: u32,
) -> vk::Rect2D {
    let tile_x = tile_id % grid_width;
    let tile_y = tile_id / grid_width;
    let (x0, x1) = partition_bounds(extent.width, grid_width, tile_x);
    let (y0, y1) = partition_bounds(extent.height, grid_height, tile_y);
    vk::Rect2D {
        offset: vk::Offset2D {
            x: i32::try_from(x0).unwrap_or(i32::MAX),
            y: i32::try_from(y0).unwrap_or(i32::MAX),
        },
        extent: vk::Extent2D {
            width: x1.saturating_sub(x0),
            height: y1.saturating_sub(y0),
        },
    }
}

fn partition_bounds(extent: u32, grid: u32, block: u32) -> (u32, u32) {
    let base = extent / grid;
    let extra = extent % grid;
    let start = block * base + block.min(extra);
    let end = start + base + u32::from(block < extra);
    (start, end)
}

impl ComposeRenderTarget for DamageAuditTarget {
    fn image(&self) -> vk::Image {
        self.image
    }

    fn image_view(&self) -> vk::ImageView {
        self.view
    }

    fn command_buffer(&self) -> vk::CommandBuffer {
        self.command_buffer
    }

    fn completion_semaphore(&self) -> vk::Semaphore {
        vk::Semaphore::null()
    }

    fn width(&self) -> u32 {
        self.extent.width
    }

    fn height(&self) -> u32 {
        self.extent.height
    }

    fn timestamp_pool(&self) -> vk::QueryPool {
        self.timestamp_pool
    }

    fn timestamps_written(&self) -> bool {
        self.timestamps_written
    }

    fn mark_timestamps_written(&mut self) {
        self.timestamps_written = true;
    }

    fn set_last_gpu_render_ns(&mut self, value: Option<u64>) {
        if value.is_some() {
            self.last_gpu_render_ns = value;
        }
    }

    fn post_compose_preparation(&self) -> Result<PostComposePreparation, PresentError> {
        Ok(PostComposePreparation::Shared)
    }

    fn record_post_compose(
        &self,
        vk: &crate::kms::vk::device::VkContext,
        command_buffer: vk::CommandBuffer,
        preparation: PostComposePreparation,
    ) {
        debug_assert!(matches!(preparation, PostComposePreparation::Shared));
        let to_general = [vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
            .src_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
            .dst_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
            .dst_access_mask(vk::AccessFlags2::MEMORY_READ | vk::AccessFlags2::MEMORY_WRITE)
            .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .new_layout(vk::ImageLayout::GENERAL)
            .image(self.image)
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .level_count(1)
                    .layer_count(1),
            )];
        crate::vk_count!(cmd_pipeline_barrier2);
        unsafe {
            vk.device.cmd_pipeline_barrier2(
                command_buffer,
                &vk::DependencyInfo::default().image_memory_barriers(&to_general),
            );
        }
    }
}
