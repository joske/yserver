use super::*;

const LINEAR: u64 = super::super::dri3::DRM_FORMAT_MOD_LINEAR;
// Representative tiled modifiers (real AMD GFX9+ vendor-tiled values).
const TILED_A: u64 = 0x0200_0000_0000_0008;
const TILED_B: u64 = 0x0200_0000_0000_000a;

#[derive(Default)]
struct ProbeFenceSpy {
    abandoned: u8,
    destroyed_idle: u8,
    wait_error: Option<io::ErrorKind>,
    waits: Vec<(u64, &'static str)>,
}

impl ProbeFenceSpy {
    fn timing_out() -> Self {
        Self {
            wait_error: Some(io::ErrorKind::TimedOut),
            ..Self::default()
        }
    }
}

impl DisposableProbeFence for ProbeFenceSpy {
    fn abandon(&mut self) {
        self.abandoned += 1;
    }

    fn destroy_idle(&mut self) {
        self.destroyed_idle += 1;
    }

    fn wait_bounded(&mut self, timeout_ns: u64, operation: &'static str) -> io::Result<()> {
        self.waits.push((timeout_ns, operation));
        match self.wait_error {
            Some(kind) => Err(io::Error::new(kind, "scripted probe fence wait")),
            None => Ok(()),
        }
    }
}

fn test_route(relationship: RenderKmsRelationship) -> ScanoutRoute {
    ScanoutRoute::new(
        crate::kms::scanout_route::RenderDeviceId::DrmRender(crate::platform::drm::DrmDeviceKey {
            major: 226,
            minor: 128,
        }),
        crate::platform::drm::DrmDeviceKey {
            major: 226,
            minor: 1,
        },
        relationship,
    )
}

fn test_sink_route() -> ScanoutRoute {
    ScanoutRoute::new(
        crate::kms::scanout_route::RenderDeviceId::DrmRender(crate::platform::drm::DrmDeviceKey {
            major: 226,
            minor: 129,
        }),
        crate::platform::drm::DrmDeviceKey {
            major: 226,
            minor: 1,
        },
        RenderKmsRelationship::Same,
    )
}

fn test_direction_metadata(
    kms_prime: ScanoutMetadataSupport,
    modifier_path: ScanoutMetadataSupport,
    linear_path: ScanoutMetadataSupport,
) -> DmabufDirectionMetadata {
    let kms_layout = match linear_path {
        ScanoutMetadataSupport::Supported => KmsLinearLayout::ExplicitModifier,
        ScanoutMetadataSupport::Unsupported => KmsLinearLayout::NotAdvertised,
        ScanoutMetadataSupport::Unknown => KmsLinearLayout::LegacyAddfb,
    };
    DmabufDirectionMetadata {
        kms_prime,
        vulkan_modifiers: modifier_path,
        modifiers: (modifier_path == ScanoutMetadataSupport::Supported)
            .then_some(TILED_A)
            .into_iter()
            .collect(),
        modifier_path,
        linear: DmabufLinearMetadata {
            vulkan: linear_path,
            kms_layout,
            path: linear_path,
        },
    }
}

fn test_scanout_metadata(
    external_memory_fd: ScanoutMetadataSupport,
    output_owned: DmabufDirectionMetadata,
    renderer_owned: DmabufDirectionMetadata,
) -> DmabufScanoutMetadata {
    DmabufScanoutMetadata {
        vulkan_external_memory_fd: external_memory_fd,
        output_owned,
        renderer_owned,
    }
}

fn test_direction_verdict(
    status: ScanoutMetadataSupport,
    direction: DmabufAllocationDirection,
) -> DmabufDirectionVerdict {
    match (status, direction) {
        (ScanoutMetadataSupport::Supported, _) => DmabufDirectionVerdict::Supported,
        (ScanoutMetadataSupport::Unsupported, DmabufAllocationDirection::OutputOwned) => {
            DmabufDirectionVerdict::Unsupported(
                DmabufDirectionIncompatibility::OutputOwnedKmsPrimeExportUnsupported,
            )
        }
        (ScanoutMetadataSupport::Unsupported, DmabufAllocationDirection::RendererOwned) => {
            DmabufDirectionVerdict::Unsupported(
                DmabufDirectionIncompatibility::RendererOwnedKmsPrimeImportUnsupported,
            )
        }
        (ScanoutMetadataSupport::Unknown, DmabufAllocationDirection::OutputOwned) => {
            DmabufDirectionVerdict::Unknown(
                DmabufScanoutUncertainty::OutputOwnedLayoutMetadataIncomplete,
            )
        }
        (ScanoutMetadataSupport::Unknown, DmabufAllocationDirection::RendererOwned) => {
            DmabufDirectionVerdict::Unknown(
                DmabufScanoutUncertainty::RendererOwnedLayoutMetadataIncomplete,
            )
        }
    }
}

#[test]
fn copied_source_candidates_dedupe_native_and_append_linear_once() {
    let advertised = [LINEAR, TILED_A, TILED_B, TILED_A, LINEAR];
    assert_eq!(
        order_copied_source_plans(&advertised, |_| true),
        vec![
            CopiedSourcePlan::DrmModifier(TILED_A),
            CopiedSourcePlan::DrmModifier(TILED_B),
            CopiedSourcePlan::DrmModifier(LINEAR),
        ]
    );
}

#[test]
fn copied_source_candidates_filter_unsupported_pairs_and_keep_native_only() {
    let advertised = [TILED_A, TILED_B];
    assert_eq!(
        order_copied_source_plans(&advertised, |modifier| modifier == TILED_B),
        vec![CopiedSourcePlan::DrmModifier(TILED_B)]
    );
    assert_eq!(
        order_copied_source_plans(&[], |modifier| modifier == LINEAR),
        vec![CopiedSourcePlan::DrmModifier(LINEAR)]
    );
}

#[test]
fn copied_plan_order_exhausts_native_tier_before_linear() {
    let destinations = [
        ScanoutAllocationPlan::GbmModifier(TILED_A),
        ScanoutAllocationPlan::DrmModifier(TILED_B),
    ];
    // Deliberately place LINEAR first: assembly must enforce tiers rather
    // than trusting its caller's input order.
    let sources = [
        CopiedSourcePlan::DrmModifier(LINEAR),
        CopiedSourcePlan::DrmModifier(TILED_A),
        CopiedSourcePlan::DrmModifier(TILED_B),
    ];

    assert_eq!(
        assemble_copied_scanout_plans(&destinations, &sources),
        vec![
            CopiedScanoutPlan {
                source: CopiedSourcePlan::DrmModifier(TILED_A),
                destination: ScanoutAllocationPlan::GbmModifier(TILED_A),
            },
            CopiedScanoutPlan {
                source: CopiedSourcePlan::DrmModifier(TILED_B),
                destination: ScanoutAllocationPlan::GbmModifier(TILED_A),
            },
            CopiedScanoutPlan {
                source: CopiedSourcePlan::DrmModifier(TILED_A),
                destination: ScanoutAllocationPlan::DrmModifier(TILED_B),
            },
            CopiedScanoutPlan {
                source: CopiedSourcePlan::DrmModifier(TILED_B),
                destination: ScanoutAllocationPlan::DrmModifier(TILED_B),
            },
            CopiedScanoutPlan {
                source: CopiedSourcePlan::DrmModifier(LINEAR),
                destination: ScanoutAllocationPlan::GbmModifier(TILED_A),
            },
            CopiedScanoutPlan {
                source: CopiedSourcePlan::DrmModifier(LINEAR),
                destination: ScanoutAllocationPlan::DrmModifier(TILED_B),
            },
        ]
    );
}

#[test]
fn copied_plan_is_transport_not_a_third_allocation_owner() {
    let plan = CopiedScanoutPlan {
        source: CopiedSourcePlan::DrmModifier(TILED_A),
        destination: ScanoutAllocationPlan::GbmModifier(TILED_B),
    };

    assert_eq!(plan.destination.ownership(), ScanoutOwnership::Output);
    assert_eq!(
        plan.describe(),
        format!(
            "source-drm-modifier=0x{TILED_A:x}-native-transport -> destination-gbm-modifier=0x{TILED_B:x}"
        )
    );
}

#[test]
fn copied_content_probe_accepts_exact_odd_extent_pixels() {
    let width = 3;
    let height = 5;
    let pixels: Vec<u8> = (0..tight_bgra_len(width, height).unwrap())
        .map(|index| index.wrapping_mul(37) as u8)
        .collect();

    verify_copied_probe_pixels(&pixels, &pixels, width, height, 1, 0, 2)
        .expect("identical renderer and sink pixels");
}

#[test]
fn copied_content_probe_rejects_one_channel_corruption() {
    let width = 4;
    let height = 3;
    let renderer = vec![0x5a; tight_bgra_len(width, height).unwrap()];
    let mut sink = renderer.clone();
    let mismatch = ((2 * width + 1) * 4 + 2) as usize;
    sink[mismatch] ^= 0xff;

    let error = verify_copied_probe_pixels(&renderer, &sink, width, height, 0, 1, 1).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    let text = error.to_string();
    assert!(text.contains("first difference at (1,2) channel=R"));
    assert!(text.contains("renderer_hash=fnv1a64:"));
    assert!(text.contains("sink_hash=fnv1a64:"));
}

#[test]
fn copied_content_probe_rejects_equal_stale_cycles() {
    let error = validate_copied_probe_freshness(
        Some(0xfeed_face_cafe_beef),
        0xfeed_face_cafe_beef,
        2,
        1,
        5,
    )
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("stale renderer pixels"));

    validate_copied_probe_freshness(Some(0xfeed_face_cafe_beef), 0x0123_4567_89ab_cdef, 2, 1, 5)
        .expect("different tokenized renderer frames remain admissible");
}

#[test]
fn copied_content_probe_requires_rendered_corner_fiducials() {
    let pixels = [
        83, 37, 241, 255, 71, 211, 29, 255, 233, 91, 47, 255, 19, 173, 223, 255,
    ];
    validate_copied_probe_fiducials(&pixels, 2, 2, 0, 0, 0).expect("four shader fiducials");

    let token_one = [
        76, 52, 240, 255, 88, 194, 28, 255, 246, 74, 46, 255, 12, 188, 222, 255,
    ];
    validate_copied_probe_fiducials(&token_one, 2, 2, 0, 1, 1).expect("tokenized shader fiducials");

    let uniform = [0; 16];
    assert!(validate_copied_probe_fiducials(&uniform, 2, 2, 0, 0, 0).is_err());
}

#[test]
fn copied_gpu_digest_accepts_matching_blocks_and_tokenized_corners() {
    let token = 1;
    let mut renderer = vec![
        copied_probe_marker_word([241, 37, 83], token),
        copied_probe_marker_word([29, 211, 71], token),
        copied_probe_marker_word([47, 91, 233], token),
        copied_probe_marker_word([223, 173, 19], token),
    ];
    renderer.extend([
        0x1020_3040,
        0x5060_7080,
        0x90a0_b0c0,
        0xd0e0_f001,
        0x1234_5678,
        0x9abc_def0,
        0x55aa_aa55,
        0x0f0f_f0f0,
    ]);
    let sink = renderer.clone();

    validate_copied_probe_digest_fiducials(&renderer, 0, 1, token)
        .expect("digest carries the current rendered corner words");
    verify_copied_probe_digests(&renderer, &sink, 2, 1, 0, 1, token)
        .expect("matching per-block GPU digests");
}

#[test]
fn copied_gpu_digest_rejects_block_lane_corruption() {
    let token = 0;
    let mut renderer = vec![
        copied_probe_marker_word([241, 37, 83], token),
        copied_probe_marker_word([29, 211, 71], token),
        copied_probe_marker_word([47, 91, 233], token),
        copied_probe_marker_word([223, 173, 19], token),
    ];
    renderer.extend(0_u32..24);
    let mut sink = renderer.clone();
    // Header occupies four words; this is lane 2 of grid block (1, 1)
    // in a 3-wide grid.
    sink[4 + (4 * 4) + 2] ^= 1;

    let error = verify_copied_probe_digests(&renderer, &sink, 3, 2, 0, 0, token).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("block=(1, 1) lane=2"));
}

#[test]
fn copied_gpu_digest_rejects_wrong_or_stale_frame() {
    let token = 1;
    let previous = vec![1, 2, 3, 4, 5, 6, 7, 8];
    let renderer = previous.clone();
    let stale = validate_copied_probe_digest_freshness(Some(&previous), &renderer, 2, 1, token)
        .unwrap_err();
    assert_eq!(stale.kind(), io::ErrorKind::InvalidData);
    assert!(stale.to_string().contains("stale compact GPU digest"));

    let wrong_corners = vec![0; 8];
    let wrong = validate_copied_probe_digest_fiducials(&wrong_corners, 2, 1, token).unwrap_err();
    assert_eq!(wrong.kind(), io::ErrorKind::InvalidData);
    assert!(wrong.to_string().contains("corner BGRA words"));
}

#[test]
fn transfer_staging_buffer_supports_upload_and_probe_readback() {
    let usage = transfer_staging_buffer_usage();
    assert!(usage.contains(vk::BufferUsageFlags::TRANSFER_SRC));
    assert!(usage.contains(vk::BufferUsageFlags::TRANSFER_DST));
}

#[test]
fn copied_route_keeps_outer_and_sink_local_identity_distinct() {
    let outer = test_route(RenderKmsRelationship::Different);
    let sink = test_sink_route();
    validate_copied_route_pair(outer, sink).expect("truthful copied route pair");

    let wrong_kms = ScanoutRoute::new(
        sink.render_device_id,
        crate::platform::drm::DrmDeviceKey {
            major: 226,
            minor: 2,
        },
        RenderKmsRelationship::Same,
    );
    assert!(validate_copied_route_pair(outer, wrong_kms).is_err());

    let nonlocal_sink = ScanoutRoute::new(
        sink.render_device_id,
        sink.kms_device_key,
        RenderKmsRelationship::Unknown,
    );
    assert!(validate_copied_route_pair(outer, nonlocal_sink).is_err());

    let same_renderer = ScanoutRoute::new(
        outer.render_device_id,
        outer.kms_device_key,
        RenderKmsRelationship::Same,
    );
    assert!(validate_copied_route_pair(outer, same_renderer).is_err());
}

#[test]
fn copied_device_lost_error_keeps_structured_source_chain() {
    let error = scanout_io_context(
        "copied sink BO 2",
        scanout_vk_error("submit copied sink transfer", vk::Result::ERROR_DEVICE_LOST),
    );
    assert!(scanout_error_is_device_lost(&error));
    assert!(error.to_string().contains("copied sink BO 2"));
    assert!(error.to_string().contains("submit copied sink transfer"));
}

#[test]
fn copied_recovery_reuses_resources_only_after_successful_quiescence() {
    assert!(copied_quiescence_result("test", Ok(())).is_ok());

    let lost = copied_quiescence_result("test", Err(vk::Result::ERROR_DEVICE_LOST))
        .expect_err("device-lost work must remain quarantined");
    assert!(scanout_error_is_device_lost(&lost));

    assert!(copied_quiescence_result("test", Err(vk::Result::ERROR_OUT_OF_DEVICE_MEMORY)).is_err());
}

#[test]
fn copied_local_target_readback_is_independent_of_transport_ownership() {
    let mut contents = CopiedRenderTargetContents::default();
    assert!(contents.validate_readback().is_err());

    contents.note_submit_succeeded();
    assert!(contents.validate_readback().is_ok());

    contents.invalidate();
    assert!(contents.validate_readback().is_err());
}

#[test]
fn copied_transport_preparation_tracks_first_discard_and_foreign_return() {
    for state in [
        CopiedSourceOwnership::RendererFirstUse,
        CopiedSourceOwnership::RendererDiscard,
    ] {
        assert_eq!(
            state.transport_preparation().expect("local full overwrite"),
            CopiedTransportPreparation {
                foreign_acquire: false,
                local_old_layout: vk::ImageLayout::UNDEFINED,
            }
        );
    }
    assert_eq!(
        CopiedSourceOwnership::ForeignAwaitingRenderer
            .transport_preparation()
            .expect("synchronized foreign return"),
        CopiedTransportPreparation {
            foreign_acquire: true,
            local_old_layout: vk::ImageLayout::GENERAL,
        }
    );
    assert!(
        CopiedSourceOwnership::ForeignAwaitingSink
            .transport_preparation()
            .is_err()
    );
    assert!(
        CopiedSourceOwnership::ForeignReturnPending
            .transport_preparation()
            .is_err()
    );
}

#[test]
fn copied_sink_query_and_import_share_exact_transfer_source_usage() {
    assert_eq!(COPIED_SINK_IMPORT_USAGE, vk::ImageUsageFlags::TRANSFER_SRC);
}

#[test]
fn lifecycle_quiescence_normalizes_every_source_handoff_to_full_discard() {
    let states = [
        CopiedSourceOwnership::RendererFirstUse,
        CopiedSourceOwnership::ForeignAwaitingSink,
        CopiedSourceOwnership::ForeignAwaitingRenderer,
        CopiedSourceOwnership::RendererDiscard,
        CopiedSourceOwnership::ForeignReturnPending,
    ];
    for state in states {
        assert_eq!(
            state.after_lifecycle_quiescence(),
            CopiedSourceOwnership::RendererDiscard,
            "source state {state:?} must resume through a full repaint"
        );
    }
}

#[test]
fn lifecycle_quiescence_normalizes_every_destination_to_local_discard() {
    let states = [
        CopiedDestinationOwnership::LocalFirstUse,
        CopiedDestinationOwnership::ForeignImportedFirstUse,
        CopiedDestinationOwnership::ForeignPendingKmsFromSink,
        CopiedDestinationOwnership::ForeignPendingKmsUninitialized,
        CopiedDestinationOwnership::ForeignRetiredByKms,
        CopiedDestinationOwnership::ReleasedButAtomicRejected,
    ];
    for state in states {
        let resumed = state.after_lifecycle_quiescence();
        assert_eq!(
            resumed,
            CopiedDestinationOwnership::ReleasedButAtomicRejected,
            "destination state {state:?} must resume through a full copy"
        );
        assert_eq!(resumed.foreign_acquire_layouts(), None);
        assert_eq!(
            resumed
                .local_copy_old_layout()
                .expect("discard is reusable"),
            vk::ImageLayout::UNDEFINED
        );
    }
}

#[test]
fn destination_becomes_foreign_reusable_only_after_kms_retirement() {
    assert!(
        CopiedDestinationOwnership::ForeignPendingKmsFromSink
            .local_copy_old_layout()
            .is_err()
    );
    assert!(
        CopiedDestinationOwnership::ForeignPendingKmsUninitialized
            .local_copy_old_layout()
            .is_err()
    );
    assert_eq!(
        CopiedDestinationOwnership::ForeignRetiredByKms.foreign_acquire_layouts(),
        Some((vk::ImageLayout::GENERAL, vk::ImageLayout::GENERAL))
    );
}

#[test]
fn modeset_retirement_preserves_uninitialized_vs_sink_general_provenance() {
    let fresh = CopiedDestinationOwnership::LocalFirstUse.after_kms_modeset();
    assert_eq!(
        fresh,
        CopiedDestinationOwnership::ForeignPendingKmsUninitialized
    );
    let fresh_retired = fresh.after_kms_retirement(0).expect("fresh retirement");
    assert_eq!(
        fresh_retired,
        CopiedDestinationOwnership::ForeignImportedFirstUse
    );
    assert_eq!(
        fresh_retired.foreign_acquire_layouts(),
        Some((vk::ImageLayout::UNDEFINED, vk::ImageLayout::GENERAL))
    );

    let produced = CopiedDestinationOwnership::ForeignRetiredByKms.after_kms_modeset();
    assert_eq!(
        produced,
        CopiedDestinationOwnership::ForeignPendingKmsFromSink
    );
    let produced_retired = produced
        .after_kms_retirement(1)
        .expect("sink-produced retirement");
    assert_eq!(
        produced_retired,
        CopiedDestinationOwnership::ForeignRetiredByKms
    );
    assert_eq!(
        produced_retired.foreign_acquire_layouts(),
        Some((vk::ImageLayout::GENERAL, vk::ImageLayout::GENERAL))
    );
}

#[test]
fn successful_or_failed_export_controls_binary_semaphore_reuse() {
    let mut state = ExportSemaphoreReuseState::Reusable;
    state.begin_post_submit_export();
    assert!(state.needs_rearm());
    state.finish_successful_export();
    assert!(!state.needs_rearm());
}

#[test]
fn prime_capability_bits_keep_import_and_export_distinct() {
    assert_eq!(
        support_from_prime_bits(DRM_PRIME_CAP_IMPORT, DRM_PRIME_CAP_IMPORT),
        ScanoutMetadataSupport::Supported
    );
    assert_eq!(
        support_from_prime_bits(DRM_PRIME_CAP_IMPORT, DRM_PRIME_CAP_EXPORT),
        ScanoutMetadataSupport::Unsupported
    );
    assert_eq!(
        support_from_prime_bits(DRM_PRIME_CAP_EXPORT, DRM_PRIME_CAP_IMPORT),
        ScanoutMetadataSupport::Unsupported
    );
    assert_eq!(
        support_from_prime_bits(DRM_PRIME_CAP_EXPORT, DRM_PRIME_CAP_EXPORT),
        ScanoutMetadataSupport::Supported
    );
}

#[test]
fn direction_metadata_uses_kms_export_for_output_and_import_for_renderer() {
    let metadata = build_dmabuf_scanout_metadata(
        ScanoutMetadataSupport::Supported,   // VK_KHR_external_memory_fd
        ScanoutMetadataSupport::Supported,   // KMS PRIME import
        ScanoutMetadataSupport::Unsupported, // KMS PRIME export
        (
            vec![TILED_A],
            ScanoutMetadataSupport::Supported, // Vulkan import
        ),
        (
            vec![TILED_B],
            ScanoutMetadataSupport::Supported, // Vulkan export
        ),
        (
            ScanoutMetadataSupport::Supported, // Vulkan imports GBM LINEAR
            KmsLinearLayout::ExplicitModifier,
        ),
        (
            ScanoutMetadataSupport::Supported, // Vulkan exports linear image
            KmsLinearLayout::ExplicitModifier,
        ),
    );

    assert_eq!(
        metadata.vulkan_external_memory_fd,
        ScanoutMetadataSupport::Supported
    );
    assert_eq!(
        metadata.output_owned.kms_prime,
        ScanoutMetadataSupport::Unsupported
    );
    assert_eq!(metadata.output_owned.modifiers, vec![TILED_A]);
    assert_eq!(
        metadata.output_owned.linear.path,
        ScanoutMetadataSupport::Unsupported
    );
    assert_eq!(
        metadata.output_owned.modifier_path,
        ScanoutMetadataSupport::Unsupported
    );
    assert_eq!(
        metadata.renderer_owned.kms_prime,
        ScanoutMetadataSupport::Supported
    );
    assert_eq!(metadata.renderer_owned.modifiers, vec![TILED_B]);
    assert_eq!(
        metadata.renderer_owned.linear.path,
        ScanoutMetadataSupport::Supported
    );
    assert_eq!(
        metadata.renderer_owned.modifier_path,
        ScanoutMetadataSupport::Supported
    );
}

#[test]
fn linear_layout_distinguishes_legacy_unknown_from_not_advertised() {
    let legacy = kms_linear_layout(&[]);
    assert_eq!(legacy, KmsLinearLayout::LegacyAddfb);
    assert_eq!(
        kms_linear_layout_support(legacy),
        ScanoutMetadataSupport::Unknown
    );

    let explicit = kms_linear_layout(&[TILED_A, LINEAR]);
    assert_eq!(explicit, KmsLinearLayout::ExplicitModifier);
    assert_eq!(
        kms_linear_layout_support(explicit),
        ScanoutMetadataSupport::Supported
    );

    let absent_from_known_list = kms_linear_layout(&[TILED_A]);
    assert_eq!(absent_from_known_list, KmsLinearLayout::NotAdvertised);
    assert_eq!(
        kms_linear_layout_support(absent_from_known_list),
        ScanoutMetadataSupport::Unsupported
    );
}

#[test]
fn renderer_owned_legacy_linear_evidence_remains_unknown_and_attemptable() {
    let linear = build_linear_metadata(
        ScanoutMetadataSupport::Supported,
        ScanoutMetadataSupport::Supported,
        KmsLinearLayout::LegacyAddfb,
    );
    assert_eq!(linear.kms_layout, KmsLinearLayout::LegacyAddfb);
    assert_eq!(linear.path, ScanoutMetadataSupport::Unknown);
}

#[test]
fn absent_or_failed_modifier_metadata_stays_unknown() {
    let absent = classify_modifier_observations(false, &[]);
    assert_eq!(absent, (Vec::new(), ScanoutMetadataSupport::Unknown));

    let failed =
        classify_modifier_observations(true, &[(TILED_A, ScanoutMetadataSupport::Unknown)]);
    assert_eq!(failed, (Vec::new(), ScanoutMetadataSupport::Unknown));
}

#[test]
fn conclusive_modifier_observations_remain_tri_state() {
    let unsupported = classify_modifier_observations(
        true,
        &[
            (TILED_A, ScanoutMetadataSupport::Unsupported),
            (TILED_B, ScanoutMetadataSupport::Unsupported),
        ],
    );
    assert_eq!(
        unsupported,
        (Vec::new(), ScanoutMetadataSupport::Unsupported)
    );

    let partly_known = classify_modifier_observations(
        true,
        &[
            (TILED_A, ScanoutMetadataSupport::Unknown),
            (TILED_B, ScanoutMetadataSupport::Supported),
        ],
    );
    assert_eq!(
        partly_known,
        (vec![TILED_B], ScanoutMetadataSupport::Supported)
    );
}

#[test]
fn unknown_prerequisite_never_becomes_false() {
    assert_eq!(
        combine_required_metadata(
            ScanoutMetadataSupport::Supported,
            ScanoutMetadataSupport::Unknown,
        ),
        ScanoutMetadataSupport::Unknown
    );
    assert_eq!(
        combine_required_metadata(
            ScanoutMetadataSupport::Unknown,
            ScanoutMetadataSupport::Supported,
        ),
        ScanoutMetadataSupport::Unknown
    );
}

#[test]
fn different_route_blocks_only_when_both_directions_are_unsupported() {
    use ScanoutMetadataSupport::{Supported, Unknown, Unsupported};

    for output_status in [Supported, Unsupported, Unknown] {
        for renderer_status in [Supported, Unsupported, Unknown] {
            let verdict = classify_route_from_direction_verdicts(
                RenderKmsRelationship::Different,
                Supported,
                test_direction_verdict(output_status, DmabufAllocationDirection::OutputOwned),
                test_direction_verdict(renderer_status, DmabufAllocationDirection::RendererOwned),
            );
            let should_block = output_status == Unsupported && renderer_status == Unsupported;
            assert_eq!(
                matches!(verdict, DmabufScanoutVerdict::Incompatible(_)),
                should_block,
                "output={output_status:?} renderer={renderer_status:?} verdict={verdict:?}"
            );
            if output_status == Supported || renderer_status == Supported {
                assert_eq!(verdict, DmabufScanoutVerdict::Compatible);
            } else if !should_block {
                assert!(matches!(verdict, DmabufScanoutVerdict::Unknown(_)));
            }
        }
    }
}

#[test]
fn same_and_unknown_relationships_ignore_every_direction_status() {
    use ScanoutMetadataSupport::{Supported, Unknown, Unsupported};

    for external_memory_fd in [Supported, Unsupported, Unknown] {
        for output_status in [Supported, Unsupported, Unknown] {
            for renderer_status in [Supported, Unsupported, Unknown] {
                let output =
                    test_direction_verdict(output_status, DmabufAllocationDirection::OutputOwned);
                let renderer = test_direction_verdict(
                    renderer_status,
                    DmabufAllocationDirection::RendererOwned,
                );
                assert_eq!(
                    classify_route_from_direction_verdicts(
                        RenderKmsRelationship::Same,
                        external_memory_fd,
                        output,
                        renderer,
                    ),
                    DmabufScanoutVerdict::Compatible,
                );
                assert_eq!(
                    classify_route_from_direction_verdicts(
                        RenderKmsRelationship::Unknown,
                        external_memory_fd,
                        output,
                        renderer,
                    ),
                    DmabufScanoutVerdict::Unknown(vec![
                        DmabufScanoutUncertainty::RenderKmsRelationshipUnknown,
                    ]),
                );
            }
        }
    }
}

#[test]
fn asahi_shaped_export_only_or_import_only_routes_remain_attemptable() {
    use ScanoutMetadataSupport::{Supported, Unsupported};

    let output_owned_only = test_scanout_metadata(
        Supported,
        test_direction_metadata(Supported, Supported, Unsupported),
        test_direction_metadata(Unsupported, Unsupported, Unsupported),
    );
    assert_eq!(
        classify_dmabuf_scanout_route(
            test_route(RenderKmsRelationship::Different),
            &output_owned_only,
            Supported,
        ),
        DmabufScanoutVerdict::Compatible,
    );

    let renderer_owned_only = test_scanout_metadata(
        Supported,
        test_direction_metadata(Unsupported, Unsupported, Unsupported),
        test_direction_metadata(Supported, Supported, Unsupported),
    );
    assert_eq!(
        classify_dmabuf_scanout_route(
            test_route(RenderKmsRelationship::Different),
            &renderer_owned_only,
            Supported,
        ),
        DmabufScanoutVerdict::Compatible,
    );
}

#[test]
fn no_shared_layout_and_query_uncertainty_still_attempt() {
    use ScanoutMetadataSupport::{Supported, Unknown, Unsupported};

    let no_shared_layout = test_scanout_metadata(
        Supported,
        test_direction_metadata(Supported, Unsupported, Unsupported),
        test_direction_metadata(Supported, Unsupported, Unsupported),
    );
    let verdict = classify_dmabuf_scanout_route(
        test_route(RenderKmsRelationship::Different),
        &no_shared_layout,
        Supported,
    );
    assert_eq!(
        verdict,
        DmabufScanoutVerdict::Unknown(vec![
            DmabufScanoutUncertainty::OutputOwnedNoAdvertisedSharedLayout,
            DmabufScanoutUncertainty::RendererOwnedNoAdvertisedSharedLayout,
        ])
    );

    let query_unknown = test_scanout_metadata(
        Supported,
        test_direction_metadata(Unknown, Unknown, Unknown),
        test_direction_metadata(Unknown, Unknown, Unknown),
    );
    assert!(matches!(
        classify_dmabuf_scanout_route(
            test_route(RenderKmsRelationship::Different),
            &query_unknown,
            Unknown,
        ),
        DmabufScanoutVerdict::Unknown(_)
    ));
}

#[test]
fn gbm_unavailable_makes_output_owned_unknown_but_prime_negative_dominates() {
    use ScanoutMetadataSupport::{Supported, Unknown, Unsupported};

    let output_only = test_scanout_metadata(
        Supported,
        test_direction_metadata(Supported, Supported, Unsupported),
        test_direction_metadata(Unsupported, Unsupported, Unsupported),
    );
    assert_eq!(
        classify_dmabuf_scanout_route(
            test_route(RenderKmsRelationship::Different),
            &output_only,
            Unknown,
        ),
        DmabufScanoutVerdict::Unknown(vec![DmabufScanoutUncertainty::OutputOwnedGbmUnavailable,])
    );
    assert_eq!(
        classify_dmabuf_scanout_route(
            test_route(RenderKmsRelationship::Same),
            &output_only,
            Unknown,
        ),
        DmabufScanoutVerdict::Compatible,
    );
    assert_eq!(
        classify_dmabuf_scanout_route(
            test_route(RenderKmsRelationship::Unknown),
            &output_only,
            Unknown,
        ),
        DmabufScanoutVerdict::Unknown(
            vec![DmabufScanoutUncertainty::RenderKmsRelationshipUnknown,]
        ),
    );

    let neither_prime_direction = test_scanout_metadata(
        Supported,
        test_direction_metadata(Unsupported, Unsupported, Unsupported),
        test_direction_metadata(Unsupported, Unsupported, Unsupported),
    );
    assert!(matches!(
        classify_dmabuf_scanout_route(
            test_route(RenderKmsRelationship::Different),
            &neither_prime_direction,
            Unknown,
        ),
        DmabufScanoutVerdict::Incompatible(
            DmabufScanoutIncompatibility::BothAllocationDirectionsUnavailable { .. }
        )
    ));
}

#[test]
fn external_memory_fd_absence_blocks_only_known_different_routes() {
    use ScanoutMetadataSupport::{Supported, Unknown, Unsupported};

    let supported_direction =
        test_direction_verdict(Supported, DmabufAllocationDirection::OutputOwned);
    let unknown_direction =
        test_direction_verdict(Unknown, DmabufAllocationDirection::RendererOwned);
    assert_eq!(
        classify_route_from_direction_verdicts(
            RenderKmsRelationship::Different,
            Unsupported,
            supported_direction,
            unknown_direction,
        ),
        DmabufScanoutVerdict::Incompatible(
            DmabufScanoutIncompatibility::VulkanExternalMemoryFdUnavailable,
        )
    );
    assert_eq!(
        classify_route_from_direction_verdicts(
            RenderKmsRelationship::Same,
            Unsupported,
            supported_direction,
            unknown_direction,
        ),
        DmabufScanoutVerdict::Compatible,
    );
    assert_eq!(
        classify_route_from_direction_verdicts(
            RenderKmsRelationship::Different,
            Unknown,
            supported_direction,
            unknown_direction,
        ),
        DmabufScanoutVerdict::Unknown(vec![
            DmabufScanoutUncertainty::VulkanExternalMemoryFdUnknown,
        ]),
    );
    assert!(matches!(
        classify_route_from_direction_verdicts(
            RenderKmsRelationship::Unknown,
            Unsupported,
            supported_direction,
            unknown_direction,
        ),
        DmabufScanoutVerdict::Unknown(_)
    ));
}

#[test]
fn incompatible_metadata_retains_both_direction_diagnostics_without_a_gate() {
    use ScanoutMetadataSupport::{Supported, Unsupported};

    let route = test_route(RenderKmsRelationship::Different);
    let metadata = test_scanout_metadata(
        Supported,
        test_direction_metadata(Unsupported, Unsupported, Unsupported),
        test_direction_metadata(Unsupported, Unsupported, Unsupported),
    );
    let verdict = classify_dmabuf_scanout_route(route, &metadata, Supported);
    let message = format!("{verdict:?}");
    assert!(message.contains("OutputOwnedKmsPrimeExportUnsupported"));
    assert!(message.contains("RendererOwnedKmsPrimeImportUnsupported"));
    assert!(matches!(verdict, DmabufScanoutVerdict::Incompatible(_)));
}

#[test]
fn scanout_usage_matches_render_and_readback_paths() {
    let usage = scanout_image_usage();
    assert!(usage.contains(vk::ImageUsageFlags::COLOR_ATTACHMENT));
    assert!(usage.contains(vk::ImageUsageFlags::TRANSFER_SRC));
    assert!(usage.contains(vk::ImageUsageFlags::TRANSFER_DST));
    assert!(!usage.contains(vk::ImageUsageFlags::SAMPLED));
}

#[test]
fn exact_plan_order_keeps_output_imports_before_renderer_exports() {
    let plans = assemble_scanout_allocation_plans(
        true,
        true,
        &[TILED_A, LINEAR],
        &[TILED_B, LINEAR],
        3440,
        true,
    );

    assert_eq!(
        plans,
        vec![
            ScanoutAllocationPlan::GbmModifier(TILED_A),
            ScanoutAllocationPlan::GbmModifier(LINEAR),
            ScanoutAllocationPlan::PaddedExplicitLinear {
                row_pitch: padded_linear_pitch(3440),
            },
            ScanoutAllocationPlan::DrmModifier(TILED_B),
            ScanoutAllocationPlan::DrmModifier(LINEAR),
            ScanoutAllocationPlan::ExplicitLinear,
            ScanoutAllocationPlan::LegacyLinear,
        ]
    );
    assert!(
        plans[..2]
            .iter()
            .all(|plan| plan.ownership() == ScanoutOwnership::Output)
    );
    assert!(
        plans[2..]
            .iter()
            .all(|plan| plan.ownership() == ScanoutOwnership::Renderer)
    );
}

#[test]
fn unavailable_gbm_removes_only_output_owned_plans() {
    let plans = assemble_scanout_allocation_plans(true, false, &[TILED_A], &[TILED_B], 1920, false);

    assert_eq!(
        plans,
        vec![
            ScanoutAllocationPlan::DrmModifier(TILED_B),
            ScanoutAllocationPlan::ExplicitLinear,
            ScanoutAllocationPlan::LegacyLinear,
        ]
    );
}

#[test]
fn device_lost_marker_survives_pool_and_bo_context() {
    let error = scanout_io_context(
        "pool",
        scanout_io_context(
            "BO 2",
            scanout_vk_error("queue submit", vk::Result::ERROR_DEVICE_LOST),
        ),
    );
    assert!(scanout_error_is_device_lost(&error));
    assert!(!scanout_error_is_device_lost(&scanout_vk_error(
        "queue submit",
        vk::Result::ERROR_OUT_OF_DEVICE_MEMORY,
    )));
}

#[test]
fn probe_teardown_accepts_success_and_device_loss_only() {
    assert!(probe_teardown_wait_completed(Ok(())));
    assert!(probe_teardown_wait_completed(Err(
        vk::Result::ERROR_DEVICE_LOST
    )));
    assert!(!probe_teardown_wait_completed(Err(
        vk::Result::ERROR_OUT_OF_HOST_MEMORY
    )));
}

#[test]
fn copied_probe_gives_every_fence_a_fresh_timeout() {
    const TIMEOUT_NS: u64 = 200_000_000;
    let mut observed = Vec::new();

    for _bo_idx in 0..3 {
        for _cycle in 0..2 {
            let mut render = ProbeFenceSpy::default();
            let mut sink = ProbeFenceSpy::default();
            wait_copied_probe_fence_pair(&mut render, &mut sink, TIMEOUT_NS)
                .expect("both scripted fences complete");
            observed.extend(render.waits);
            observed.extend(sink.waits);
            assert_eq!(render.destroyed_idle, 1);
            assert_eq!(sink.destroyed_idle, 1);
        }
    }

    assert_eq!(observed.len(), 12);
    assert!(observed.iter().all(|(timeout, _)| *timeout == TIMEOUT_NS));
    assert_eq!(
        observed
            .iter()
            .map(|(_, operation)| *operation)
            .collect::<Vec<_>>(),
        ["copied renderer probe", "copied sink probe"].repeat(6),
    );
}

#[test]
fn copy_free_probe_gives_every_bo_a_fresh_timeout() {
    const TIMEOUT_NS: u64 = 200_000_000;

    for _bo_idx in 0..3 {
        let mut fence = ProbeFenceSpy::default();
        wait_copy_free_probe_fence(&mut fence, TIMEOUT_NS)
            .expect("scripted copy-free fence completes");
        assert_eq!(
            fence.waits,
            vec![(TIMEOUT_NS, "disposable scanout rendering probe")]
        );
        assert_eq!(fence.destroyed_idle, 1);
        assert_eq!(fence.abandoned, 0);
    }
}

#[test]
fn disposable_probe_failure_policy_distinguishes_rejection_and_quarantine() {
    let mismatch =
        DisposableProbeError::from(io::Error::new(io::ErrorKind::InvalidData, "pixel mismatch"));
    assert!(!mismatch.requires_quarantine());
    assert!(!mismatch.abort_candidate_search());
    assert!(!mismatch.bypass_normal_teardown());

    let safe_pre_submit_timeout = DisposableProbeError::from(io::Error::new(
        io::ErrorKind::TimedOut,
        "pre-submit operation timed out",
    ));
    assert!(!safe_pre_submit_timeout.requires_quarantine());
    assert!(!safe_pre_submit_timeout.abort_candidate_search());
    assert!(!safe_pre_submit_timeout.bypass_normal_teardown());

    let blob_cleanup = DisposableProbeError::terminal_cleanup(io::Error::other(
        "TEST_ONLY mode blob cleanup failed",
    ));
    assert!(!blob_cleanup.requires_quarantine());
    assert!(blob_cleanup.abort_candidate_search());
    assert!(
        !blob_cleanup.bypass_normal_teardown(),
        "blob failure is terminal but must still strictly clean the pool"
    );

    let uncertain = DisposableProbeError::quarantined(io::Error::other("submission unknown"))
        .with_context("BO 2 copied sink wait");
    assert!(uncertain.requires_quarantine());
    assert!(uncertain.abort_candidate_search());
    assert!(uncertain.bypass_normal_teardown());
    assert!(uncertain.to_string().contains("BO 2 copied sink wait"));
}

#[test]
fn probe_fence_failure_disposition_never_waits_device_wide() {
    let cases = [
        (PendingProbeSubmissions::None, (0, 1), (0, 1), false),
        (PendingProbeSubmissions::Render, (1, 0), (0, 1), true),
        (PendingProbeSubmissions::RenderAndSink, (1, 0), (1, 0), true),
    ];

    for (pending, render_expected, sink_expected, quarantine_expected) in cases {
        let mut render = ProbeFenceSpy::default();
        let mut sink = ProbeFenceSpy::default();
        let error = finish_pending_probe_failure(
            pending,
            DisposableProbeError::from(io::Error::other("probe failed")),
            &mut render,
            &mut sink,
        );

        assert_eq!(
            (render.abandoned, render.destroyed_idle),
            render_expected,
            "render fence disposition for {pending:?}"
        );
        assert_eq!(
            (sink.abandoned, sink.destroyed_idle),
            sink_expected,
            "sink fence disposition for {pending:?}"
        );
        assert_eq!(error.requires_quarantine(), quarantine_expected);
    }
}

#[test]
fn actual_probe_fence_timeouts_are_terminal_and_quarantined() {
    const TIMEOUT_NS: u64 = 200_000_000;

    let mut copy_free = ProbeFenceSpy::timing_out();
    let error = wait_copy_free_probe_fence(&mut copy_free, TIMEOUT_NS)
        .expect_err("copy-free timeout must fail");
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert!(error.requires_quarantine());
    assert!(error.abort_candidate_search());
    assert!(error.bypass_normal_teardown());
    assert_eq!((copy_free.abandoned, copy_free.destroyed_idle), (1, 0));

    let mut render = ProbeFenceSpy::timing_out();
    let mut sink = ProbeFenceSpy::default();
    let error = wait_copied_probe_fence_pair(&mut render, &mut sink, TIMEOUT_NS)
        .expect_err("renderer timeout must fail the copied pair");
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert!(error.requires_quarantine());
    assert_eq!((render.abandoned, render.destroyed_idle), (1, 0));
    assert_eq!((sink.abandoned, sink.destroyed_idle), (1, 0));

    let mut render = ProbeFenceSpy::default();
    let mut sink = ProbeFenceSpy::timing_out();
    let error = wait_copied_probe_fence_pair(&mut render, &mut sink, TIMEOUT_NS)
        .expect_err("sink timeout must fail the copied pair");
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert!(error.requires_quarantine());
    assert_eq!((render.abandoned, render.destroyed_idle), (0, 1));
    assert_eq!((sink.abandoned, sink.destroyed_idle), (1, 0));
}

#[test]
fn strict_drm_cleanup_orders_framebuffer_before_gem_and_preserves_failures() {
    use std::cell::Cell;

    let calls = Cell::new(0_u8);
    let mut framebuffer = Some(11_u8);
    let mut gem = Some(22_u8);
    release_drm_handles_strict(
        &mut framebuffer,
        &mut gem,
        |handle| {
            assert_eq!((handle, calls.get()), (11, 0));
            calls.set(1);
            Ok(())
        },
        |handle| {
            assert_eq!((handle, calls.get()), (22, 1));
            calls.set(2);
            Ok(())
        },
    )
    .expect("both strict cleanup ioctls succeed");
    assert_eq!((framebuffer, gem, calls.get()), (None, None, 2));

    let calls = Cell::new(0_u8);
    let mut framebuffer = Some(11_u8);
    let mut gem = Some(22_u8);
    release_drm_handles_strict(
        &mut framebuffer,
        &mut gem,
        |_| {
            calls.set(1);
            Err(io::Error::other("RMFB failed"))
        },
        |_| {
            calls.set(2);
            Ok(())
        },
    )
    .expect_err("framebuffer cleanup failure is terminal");
    assert_eq!(
        (framebuffer, gem, calls.get()),
        (Some(11), Some(22), 1),
        "GEM close must not run after RMFB failure"
    );

    let calls = Cell::new(0_u8);
    let mut framebuffer = Some(11_u8);
    let mut gem = Some(22_u8);
    release_drm_handles_strict(
        &mut framebuffer,
        &mut gem,
        |_| {
            calls.set(1);
            Ok(())
        },
        |_| {
            assert_eq!(calls.get(), 1);
            calls.set(2);
            Err(io::Error::other("GEM_CLOSE failed"))
        },
    )
    .expect_err("GEM cleanup failure is terminal");
    assert_eq!(
        (framebuffer, gem, calls.get()),
        (None, Some(22), 2),
        "successful RMFB stays cleared while the failed GEM handle is retained"
    );
}

#[test]
fn production_probe_finalizer_enforces_strict_cleanup_precedence() {
    use std::{cell::Cell, rc::Rc};

    #[derive(Default)]
    struct AttemptCounts {
        known_quiescent: Cell<u8>,
        strict_cleanups: Cell<u8>,
        drops: Cell<u8>,
        device_idle_waits: Cell<u8>,
    }

    struct AttemptSpy {
        counts: Rc<AttemptCounts>,
        cleanup_fails: bool,
    }

    impl DisposableProbeAttempt for AttemptSpy {
        fn mark_known_quiescent(&self) {
            self.counts
                .known_quiescent
                .set(self.counts.known_quiescent.get() + 1);
        }

        fn release_strict_drm_resources(&mut self) -> io::Result<()> {
            self.counts
                .strict_cleanups
                .set(self.counts.strict_cleanups.get() + 1);
            if self.cleanup_fails {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "strict DRM cleanup failure",
                ))
            } else {
                Ok(())
            }
        }
    }

    impl Drop for AttemptSpy {
        fn drop(&mut self) {
            self.counts.drops.set(self.counts.drops.get() + 1);
            if self.counts.known_quiescent.get() == 0 {
                self.counts
                    .device_idle_waits
                    .set(self.counts.device_idle_waits.get() + 1);
            }
        }
    }

    let cases = [
        ("success", Ok(()), false, true, false, false, (1, 1, 1, 0)),
        (
            "completed mismatch",
            completed_probe_validation(Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "pixel mismatch",
            ))),
            false,
            false,
            false,
            false,
            (1, 1, 1, 0),
        ),
        (
            "copied source allocation rejected after destination allocation",
            Err(DisposableProbeError::from(io::Error::other(
                "copied source allocation failed",
            ))),
            false,
            false,
            false,
            false,
            (1, 1, 1, 0),
        ),
        (
            "terminal blob cleanup after strict pool cleanup",
            Err(DisposableProbeError::terminal_cleanup(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "mode blob cleanup failure",
            ))),
            false,
            false,
            true,
            false,
            (1, 1, 1, 0),
        ),
        (
            "uncertain post-submit failure",
            Err(DisposableProbeError::quarantined(io::Error::new(
                io::ErrorKind::TimedOut,
                "fence timeout",
            ))),
            false,
            false,
            true,
            true,
            (0, 0, 0, 0),
        ),
        (
            "cleanup failure overrides success",
            Ok(()),
            true,
            false,
            true,
            true,
            (1, 1, 0, 0),
        ),
        (
            "cleanup failure overrides completed mismatch",
            completed_probe_validation(Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "pixel mismatch",
            ))),
            true,
            false,
            true,
            true,
            (1, 1, 0, 0),
        ),
    ];

    for (label, result, cleanup_fails, expect_ok, expect_abort, expect_quarantine, expected) in
        cases
    {
        let counts = Rc::new(AttemptCounts::default());
        let returned = finish_disposable_probe_attempt(
            AttemptSpy {
                counts: Rc::clone(&counts),
                cleanup_fails,
            },
            result,
        );

        assert_eq!(returned.is_ok(), expect_ok, "{label}");
        assert_eq!(
            returned
                .as_ref()
                .err()
                .is_some_and(DisposableProbeError::abort_candidate_search),
            expect_abort,
            "{label}",
        );
        assert_eq!(
            returned
                .as_ref()
                .err()
                .is_some_and(DisposableProbeError::requires_quarantine),
            expect_quarantine,
            "{label}",
        );
        assert_eq!(
            (
                counts.known_quiescent.get(),
                counts.strict_cleanups.get(),
                counts.drops.get(),
                counts.device_idle_waits.get(),
            ),
            expected,
            "{label}",
        );
    }
}

#[test]
fn completed_probe_validation_is_authoritative() {
    assert_eq!(
        completed_probe_validation::<u8, io::Error>(Ok(7)).expect("completed matching content"),
        7
    );

    let mismatch = completed_probe_validation::<(), io::Error>(Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "pixel mismatch",
    )))
    .expect_err("completed mismatching content remains a rejection");
    assert_eq!(mismatch.kind(), io::ErrorKind::InvalidData);
    assert!(!mismatch.requires_quarantine());
    assert!(!mismatch.abort_candidate_search());
    assert!(!mismatch.bypass_normal_teardown());
}

#[test]
fn digest_readback_infrastructure_failure_is_not_route_rejection() {
    let invalidation = copied_probe_digest_readback_error(
        "test digest invalidate",
        vk::Result::ERROR_OUT_OF_HOST_MEMORY,
    );
    assert!(!invalidation.requires_quarantine());
    assert!(invalidation.abort_candidate_search());
    assert!(!invalidation.bypass_normal_teardown());

    let device_lost =
        copied_probe_digest_readback_error("test digest invalidate", vk::Result::ERROR_DEVICE_LOST);
    assert!(!device_lost.requires_quarantine());
    assert!(!device_lost.abort_candidate_search());
    assert!(scanout_error_is_device_lost(device_lost.as_io_error()));
}

#[test]
fn disarmed_owned_backing_is_forgotten_instead_of_dropped() {
    use std::{cell::Cell, rc::Rc};

    struct DropSpy(Rc<Cell<u32>>);
    impl Drop for DropSpy {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }

    let drops = Rc::new(Cell::new(0));
    let mut backing = Some(DropSpy(Rc::clone(&drops)));
    leak_owned_backing(&mut backing);

    assert!(backing.is_none());
    assert_eq!(drops.get(), 0);
}

#[test]
fn modifier_order_prefers_tiled_over_linear() {
    // KMS advertises linear first, then a tiled modifier; both are
    // Vulkan-supported and exportable. Tiled must win — issue #48.
    let candidates =
        order_scanout_modifier_candidates(&[LINEAR, TILED_A], &[LINEAR, TILED_A], false, |_| true);
    assert_eq!(
        candidates,
        vec![TILED_A, LINEAR],
        "tiled modifier must precede LINEAR"
    );
}

#[test]
fn modifier_order_keeps_kms_order_among_tiled() {
    let candidates = order_scanout_modifier_candidates(
        &[TILED_B, TILED_A, LINEAR],
        &[TILED_A, TILED_B, LINEAR],
        false,
        |_| true,
    );
    assert_eq!(candidates, vec![TILED_B, TILED_A, LINEAR]);
}

#[test]
fn modifier_order_drops_tiled_not_supported_by_vulkan() {
    let candidates =
        order_scanout_modifier_candidates(&[TILED_A, LINEAR], &[LINEAR], false, |_| true);
    assert_eq!(candidates, vec![LINEAR], "TILED_A not in the Vulkan set");
}

#[test]
fn modifier_order_drops_non_exportable_tiled() {
    // A multi-plane (DCC) tiled modifier fails the single-plane
    // exportability check and must be skipped, leaving LINEAR.
    let candidates =
        order_scanout_modifier_candidates(&[TILED_A, LINEAR], &[TILED_A, LINEAR], false, |m| {
            m != TILED_A
        });
    assert_eq!(candidates, vec![LINEAR]);
}

#[test]
fn modifier_order_linear_only_plane_yields_linear() {
    let candidates = order_scanout_modifier_candidates(&[LINEAR], &[LINEAR], false, |_| true);
    assert_eq!(candidates, vec![LINEAR]);
}

#[test]
fn modifier_order_drops_linear_without_directional_support() {
    let candidates = order_scanout_modifier_candidates(&[LINEAR], &[LINEAR], false, |_| false);
    assert!(candidates.is_empty());
}

#[test]
fn modifier_order_empty_when_no_intersection() {
    let candidates = order_scanout_modifier_candidates(&[TILED_A], &[TILED_B], false, |_| true);
    assert!(candidates.is_empty());
}

#[test]
fn linear_scanout_stride_alignment_matches_hw_observations() {
    // GTX 1050 @ 2560 wide → pitch 10240 = 256×40 → aligned → LINEAR OK.
    assert!(linear_scanout_stride_aligned(2560));
    // GTX 1060 @ 3440 ultrawide → pitch 13760, mod 256 = 192 → unaligned.
    assert!(!linear_scanout_stride_aligned(3440));
    // Common aligned widths.
    assert!(linear_scanout_stride_aligned(1920)); // 7680 = 256×30
    assert!(linear_scanout_stride_aligned(1280)); // 5120 = 256×20
    assert!(linear_scanout_stride_aligned(3840)); // 15360 = 256×60 (4K)
    // 1366 laptop → 5464, mod 256 = 88 → unaligned.
    assert!(!linear_scanout_stride_aligned(1366));
}

#[test]
fn padded_linear_pitch_rounds_up_to_alignment() {
    // 3440 → tight 13760 → padded up to 13824 = 256×54.
    assert_eq!(padded_linear_pitch(3440), 13824);
    assert!(padded_linear_pitch(3440).is_multiple_of(SCANOUT_PITCH_ALIGN));
    // Already-aligned widths are unchanged.
    assert_eq!(padded_linear_pitch(2560), 10240); // 256×40
    assert_eq!(padded_linear_pitch(1920), 7680); // 256×30
    // 1366 → tight 5464 → padded 5632 = 256×22.
    assert_eq!(padded_linear_pitch(1366), 5632);
}

#[test]
fn scanout_linear_policy_covers_dithering_drivers_not_amd() {
    use super::vk::DriverId;
    // HW-confirmed dithering on tiled scanout → must prefer LINEAR.
    assert!(scanout_prefers_linear(DriverId::NVIDIA_PROPRIETARY));
    assert!(scanout_prefers_linear(DriverId::INTEL_OPEN_SOURCE_MESA));
    // AMD needs tiled (RDNA4 LINEAR corrupts, issue #48) — must NOT prefer.
    assert!(!scanout_prefers_linear(DriverId::MESA_RADV));
    assert!(!scanout_prefers_linear(DriverId::AMD_PROPRIETARY));
    // Unconfirmed drivers stay on the tiled-first default.
    assert!(!scanout_prefers_linear(DriverId::MESA_LLVMPIPE));
}

// NVIDIA prefer_linear path — mirrors what GBM does on Pascal (GTX 1050):
// LINEAR selected first even though tiled modifiers are also advertised.
#[test]
fn modifier_order_nvidia_prefers_linear_over_tiled() {
    // NVIDIA KMS advertises tiled first, then LINEAR — but prefer_linear=true
    // must put LINEAR at the front.
    let candidates = order_scanout_modifier_candidates(
        &[TILED_A, TILED_B, LINEAR],
        &[TILED_A, TILED_B, LINEAR],
        true,
        |_| true,
    );
    assert_eq!(candidates, vec![LINEAR, TILED_A, TILED_B]);
}

#[test]
fn modifier_order_nvidia_linear_only_plane_yields_linear() {
    let candidates = order_scanout_modifier_candidates(&[LINEAR], &[LINEAR], true, |_| true);
    assert_eq!(candidates, vec![LINEAR]);
}

#[test]
fn modifier_order_nvidia_no_linear_falls_back_to_tiled() {
    // If LINEAR is absent from the KMS plane, even prefer_linear=true should
    // yield the tiled modifiers (they're the only option).
    let candidates =
        order_scanout_modifier_candidates(&[TILED_A, TILED_B], &[TILED_A, TILED_B], true, |_| true);
    assert_eq!(candidates, vec![TILED_A, TILED_B]);
}

#[test]
fn fresh_bo_is_free() {
    let bo = BoState::default();
    assert_eq!(bo.phase, BoPhase::Free);
    assert!(bo.in_fence_fd.is_none());
    assert!(bo.release_fence_fd.is_none());
}

#[test]
fn synchronous_modeset_reserves_its_front_buffer_without_waiting_for_an_event() {
    let mut bo = BoState::default();

    bo.mark_on_screen_after_modeset();

    assert_eq!(bo.phase, BoPhase::OnScreen);
    assert!(bo.in_fence_fd.is_none());
    assert!(bo.release_fence_fd.is_none());
}

#[test]
fn record_then_submit_transitions_to_submitted() {
    let mut bo = BoState::default();
    bo.transition_to_recording();
    assert_eq!(bo.phase, BoPhase::Recording);
    bo.transition_to_submitted(/* in_fence */ 42);
    assert_eq!(bo.phase, BoPhase::Submitted);
    assert_eq!(bo.in_fence_fd, Some(42));
}

#[test]
fn submit_with_no_fence_sentinel_does_not_store_fd() {
    let mut bo = BoState::default();
    bo.transition_to_recording();
    bo.transition_to_submitted(/* no fence */ -1);
    assert_eq!(bo.phase, BoPhase::Submitted);
    assert!(bo.in_fence_fd.is_none());
}

#[test]
fn atomic_accept_returns_in_fence_for_caller_to_close_and_stores_out_fence() {
    let mut bo = BoState::default();
    bo.transition_to_recording();
    bo.transition_to_submitted(42);
    let reclaimed = bo.transition_to_pending(/* out_fence */ 99);
    assert_eq!(bo.phase, BoPhase::Pending);
    assert_eq!(
        reclaimed,
        Some(42),
        "caller closes the in-fence fd; kernel only refs the sync_file"
    );
    assert!(bo.in_fence_fd.is_none(), "moved out into reclaimed");
    assert_eq!(bo.release_fence_fd, Some(99));
}

#[test]
fn atomic_accept_with_no_out_fence_sentinel_does_not_store_release_fd() {
    let mut bo = BoState::default();
    bo.transition_to_recording();
    bo.transition_to_submitted(42);
    let reclaimed = bo.transition_to_pending(/* no out fence */ -1);
    assert_eq!(reclaimed, Some(42));
    assert!(bo.release_fence_fd.is_none());
}

#[test]
fn atomic_reject_returns_to_recording_and_we_still_own_in_fence() {
    let mut bo = BoState::default();
    bo.transition_to_recording();
    bo.transition_to_submitted(42);
    let reclaimed = bo.transition_to_recording_after_atomic_reject();
    assert_eq!(bo.phase, BoPhase::Recording);
    assert_eq!(reclaimed, Some(42), "caller closes the fd");
    assert!(bo.in_fence_fd.is_none(), "moved out into reclaimed");
}

#[test]
fn modeset_preempt_from_submitted_returns_in_fence() {
    let mut bo = BoState::default();
    bo.transition_to_recording();
    bo.transition_to_submitted(7);
    let in_fence = bo.transition_to_free_after_modeset_preempt();
    assert_eq!(bo.phase, BoPhase::Free);
    assert_eq!(in_fence, Some(7));
}

#[test]
fn pending_then_onscreen_then_retiring_then_free_releases_fence() {
    let mut bo = BoState::default();
    bo.transition_to_recording();
    bo.transition_to_submitted(11);
    let _ = bo.transition_to_pending(22);
    assert_eq!(bo.phase, BoPhase::Pending);

    bo.transition_to_on_screen();
    assert_eq!(bo.phase, BoPhase::OnScreen);
    assert_eq!(
        bo.release_fence_fd,
        Some(22),
        "release fence stays attached while on-screen"
    );

    bo.transition_to_retiring();
    assert_eq!(bo.phase, BoPhase::Retiring);

    let release = bo.transition_to_free_after_retire();
    assert_eq!(bo.phase, BoPhase::Free);
    assert_eq!(release, Some(22), "caller closes the release fence");
    assert!(bo.release_fence_fd.is_none());
}

/// 4.1.2.7 fence-cycle integration test (host-pure variant per
/// the plan: "mock the GPU side if Vulkan creation under
/// lavapipe is awkward inside `cargo test`"). Drives a 3-bo
/// pool through 6 frames in the steady-state cycle and asserts
/// every fence fd issued is closed exactly once. No real GPU.
/// The accounting catches state-machine bugs that leak fence
/// fds (which is exactly the class of bug we hit on bare metal
/// with the original IN_FENCE_FD ownership confusion).
#[test]
fn six_frames_cycle_through_pool_without_leaking_fences() {
    let mut bos: Vec<BoState> = (0..3).map(|_| BoState::default()).collect();
    let mut issued = 0u32;
    let mut closed = 0u32;
    let mut next_fd = 100i32;

    let alloc_fd = |issued: &mut u32, next_fd: &mut i32| -> i32 {
        *issued += 1;
        let fd = *next_fd;
        *next_fd += 1;
        fd
    };
    let close = |fd: Option<i32>, closed: &mut u32| {
        if fd.is_some() {
            *closed += 1;
        }
    };

    for _frame in 0..6 {
        // 1. Acquire Free bo and submit.
        let bo_idx = bos.iter().position(|b| b.phase == BoPhase::Free).expect(
            "with 3 bos and the cycle-advance below, at least one bo \
                 should be Free every frame",
        );
        let bo = &mut bos[bo_idx];
        bo.transition_to_recording();
        let in_fence = alloc_fd(&mut issued, &mut next_fd);
        bo.transition_to_submitted(in_fence);

        // 2. Atomic accept → Pending; closes the in-fence we just
        //    issued.
        let out_fence = alloc_fd(&mut issued, &mut next_fd);
        close(bo.transition_to_pending(out_fence), &mut closed);

        // 3. Pageflip-complete advance (mirrors
        //    `advance_pool_on_pageflip_complete` in backend.rs).
        let phases: Vec<BoPhase> = bos.iter().map(|b| b.phase).collect();
        for (i, phase) in phases.into_iter().enumerate() {
            match phase {
                BoPhase::Retiring => {
                    close(bos[i].transition_to_free_after_retire(), &mut closed);
                }
                BoPhase::OnScreen => bos[i].transition_to_retiring(),
                BoPhase::Pending => bos[i].transition_to_on_screen(),
                _ => {}
            }
        }
    }

    // Drain remaining bos (simulates shutdown).
    for bo in &mut bos {
        let r = bo.transition_to_free_after_modeset_reset();
        close(r.in_fence, &mut closed);
        close(r.release_fence, &mut closed);
    }

    assert_eq!(
        issued, closed,
        "every fence fd issued must be closed exactly once \
             (issued={issued}, closed={closed})"
    );
    assert_eq!(
        issued, 12,
        "6 frames × (1 in_fence + 1 release_fence) = 12 fds expected"
    );
}

#[test]
fn modeset_reset_returns_all_currently_held_fences() {
    // Pending: in-fence already returned to caller for closing,
    // live release fence still held by the bo.
    let mut bo = BoState::default();
    bo.transition_to_recording();
    bo.transition_to_submitted(5);
    let _ = bo.transition_to_pending(60);
    let released = bo.transition_to_free_after_modeset_reset();
    assert_eq!(bo.phase, BoPhase::Free);
    assert_eq!(released.in_fence, None);
    assert_eq!(released.release_fence, Some(60));

    // Submitted: still own the in-fence.
    let mut bo = BoState::default();
    bo.transition_to_recording();
    bo.transition_to_submitted(5);
    let released = bo.transition_to_free_after_modeset_reset();
    assert_eq!(released.in_fence, Some(5));
    assert_eq!(released.release_fence, None);

    // Recording: nothing held.
    let mut bo = BoState::default();
    bo.transition_to_recording();
    let released = bo.transition_to_free_after_modeset_reset();
    assert_eq!(released.in_fence, None);
    assert_eq!(released.release_fence, None);
}

#[test]
fn addfb_modifier_flag_tracks_modifier_presence() {
    assert_eq!(addfb_flags_for_modifier(None), FbCmd2Flags::empty());
    assert_eq!(
        addfb_flags_for_modifier(Some(crate::kms::vk::dri3::DRM_FORMAT_MOD_LINEAR)),
        FbCmd2Flags::MODIFIERS
    );
}

// ── YSERVER_SCANOUT_MODIFIER override ────────────────────────────

#[test]
fn modifier_override_parses_order_keywords() {
    assert_eq!(
        parse_scanout_modifier_override("tiled-first"),
        Some(ScanoutModifierOverride::TiledFirst)
    );
    assert_eq!(
        parse_scanout_modifier_override("linear-first"),
        Some(ScanoutModifierOverride::LinearFirst)
    );
    // Underscores and case are accepted — this is typed by hand on a
    // console during a hardware triage round.
    assert_eq!(
        parse_scanout_modifier_override("TILED_FIRST"),
        Some(ScanoutModifierOverride::TiledFirst)
    );
    assert_eq!(
        parse_scanout_modifier_override("  Linear-First  "),
        Some(ScanoutModifierOverride::LinearFirst)
    );
}

#[test]
fn modifier_override_parses_explicit_modifier() {
    // The block-linear modifier an RTX 3060 Ti / driver 595 actually
    // scans out with (issue #32 telemetry).
    assert_eq!(
        parse_scanout_modifier_override("0x300000000606015"),
        Some(ScanoutModifierOverride::First(0x0300_0000_0060_6015))
    );
    // Bare hex (no 0x) is accepted: the log prints values with 0x, but
    // a copy-paste that loses the prefix should still work.
    assert_eq!(
        parse_scanout_modifier_override("300000000606015"),
        Some(ScanoutModifierOverride::First(0x0300_0000_0060_6015))
    );
    assert_eq!(
        parse_scanout_modifier_override("0X0"),
        Some(ScanoutModifierOverride::First(LINEAR))
    );
}

#[test]
fn modifier_override_rejects_garbage_without_panicking() {
    // A typo must not take the display server down: unparseable values
    // are ignored (with a warning) and the driver policy stands.
    assert_eq!(parse_scanout_modifier_override(""), None);
    assert_eq!(parse_scanout_modifier_override("   "), None);
    assert_eq!(parse_scanout_modifier_override("tiled"), None);
    assert_eq!(parse_scanout_modifier_override("0xzz"), None);
    // Wider than u64.
    assert_eq!(
        parse_scanout_modifier_override("0x1_0000_0000_0000_0000"),
        None
    );
}

#[test]
fn modifier_override_forces_order_against_driver_policy() {
    use super::vk::DriverId;
    // No override → driver policy stands (NVIDIA prefers LINEAR).
    assert!(resolve_prefer_linear(DriverId::NVIDIA_PROPRIETARY, None));
    assert!(!resolve_prefer_linear(DriverId::MESA_RADV, None));
    // tiled-first overrides NVIDIA's LINEAR preference — this is the
    // knob that answers "does GBM tiled scan out clean on Pascal?".
    assert!(!resolve_prefer_linear(
        DriverId::NVIDIA_PROPRIETARY,
        Some(ScanoutModifierOverride::TiledFirst)
    ));
    // linear-first overrides AMD's tiled preference (reproduces #48).
    assert!(resolve_prefer_linear(
        DriverId::MESA_RADV,
        Some(ScanoutModifierOverride::LinearFirst)
    ));
    // An explicit modifier does not change the LINEAR-vs-tiled policy
    // for the REST of the list; hoisting handles the pinned entry.
    assert!(resolve_prefer_linear(
        DriverId::NVIDIA_PROPRIETARY,
        Some(ScanoutModifierOverride::First(TILED_A))
    ));
}

#[test]
fn modifier_override_hoists_pinned_modifier_to_front() {
    // Present in the list → moved to front, relative order preserved.
    let mut candidates = vec![LINEAR, TILED_A, TILED_B];
    hoist_modifier_first(&mut candidates, TILED_B);
    assert_eq!(candidates, vec![TILED_B, LINEAR, TILED_A]);

    // Already first → unchanged.
    let mut candidates = vec![TILED_A, LINEAR];
    hoist_modifier_first(&mut candidates, TILED_A);
    assert_eq!(candidates, vec![TILED_A, LINEAR]);

    // Absent → prepended anyway. The GBM plan checks importability at
    // allocation time and falls through cleanly, so pinning a modifier
    // the Vulkan side did not advertise is a legal experiment rather
    // than a boot failure.
    let mut candidates = vec![LINEAR];
    hoist_modifier_first(&mut candidates, TILED_A);
    assert_eq!(candidates, vec![TILED_A, LINEAR]);

    // Empty candidate list (no modifier survived the filters) still
    // yields the pinned modifier for the GBM path to try.
    let mut candidates = Vec::new();
    hoist_modifier_first(&mut candidates, TILED_A);
    assert_eq!(candidates, vec![TILED_A]);
}
