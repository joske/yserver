use super::*;

pub(super) const DRM_PRIME_CAP_IMPORT: u64 = 1 << 0;
pub(super) const DRM_PRIME_CAP_EXPORT: u64 = 1 << 1;

pub(super) fn support_from_prime_bits(bits: u64, required: u64) -> ScanoutMetadataSupport {
    if bits & required != 0 {
        ScanoutMetadataSupport::Supported
    } else {
        ScanoutMetadataSupport::Unsupported
    }
}

pub(super) fn combine_required_metadata(
    first: ScanoutMetadataSupport,
    second: ScanoutMetadataSupport,
) -> ScanoutMetadataSupport {
    use ScanoutMetadataSupport::{Supported, Unknown, Unsupported};

    match (first, second) {
        (Unsupported, _) | (_, Unsupported) => Unsupported,
        (Supported, Supported) => Supported,
        (Supported | Unknown, Supported | Unknown) => Unknown,
    }
}

pub(super) fn kms_linear_layout(kms_scanout_modifiers: &[u64]) -> KmsLinearLayout {
    if kms_scanout_modifiers.is_empty() {
        KmsLinearLayout::LegacyAddfb
    } else if kms_scanout_modifiers.contains(&crate::kms::vk::dri3::DRM_FORMAT_MOD_LINEAR) {
        KmsLinearLayout::ExplicitModifier
    } else {
        KmsLinearLayout::NotAdvertised
    }
}

pub(super) fn kms_linear_layout_support(layout: KmsLinearLayout) -> ScanoutMetadataSupport {
    match layout {
        KmsLinearLayout::ExplicitModifier => ScanoutMetadataSupport::Supported,
        KmsLinearLayout::LegacyAddfb => ScanoutMetadataSupport::Unknown,
        KmsLinearLayout::NotAdvertised => ScanoutMetadataSupport::Unsupported,
    }
}

pub(super) fn build_linear_metadata(
    prime: ScanoutMetadataSupport,
    vulkan: ScanoutMetadataSupport,
    kms_layout: KmsLinearLayout,
) -> DmabufLinearMetadata {
    let path = combine_required_metadata(
        prime,
        combine_required_metadata(vulkan, kms_linear_layout_support(kms_layout)),
    );
    DmabufLinearMetadata {
        vulkan,
        kms_layout,
        path,
    }
}

pub(super) fn classify_modifier_observations(
    kms_advertised_modifiers: bool,
    observations: &[(u64, ScanoutMetadataSupport)],
) -> (Vec<u64>, ScanoutMetadataSupport) {
    use ScanoutMetadataSupport::{Supported, Unknown, Unsupported};

    if !kms_advertised_modifiers {
        return (Vec::new(), Unknown);
    }

    let mut modifiers = Vec::new();
    let mut saw_unknown = false;
    for &(modifier, support) in observations {
        match support {
            Supported if !modifiers.contains(&modifier) => modifiers.push(modifier),
            Supported | Unsupported => {}
            Unknown => saw_unknown = true,
        }
    }

    let support = if modifiers.is_empty() {
        if saw_unknown { Unknown } else { Unsupported }
    } else {
        Supported
    };
    (modifiers, support)
}

fn probe_kms_prime_metadata(
    drm: &crate::drm::Device,
    route: ScanoutRoute,
) -> (ScanoutMetadataSupport, ScanoutMetadataSupport) {
    match drm.get_driver_capability(DriverCapability::Prime) {
        Ok(bits) => (
            support_from_prime_bits(bits, DRM_PRIME_CAP_IMPORT),
            support_from_prime_bits(bits, DRM_PRIME_CAP_EXPORT),
        ),
        Err(error) => {
            log::warn!(
                "dma-buf metadata for {route:?}: DRM_CAP_PRIME query failed: {error}; \
                 import/export support remains unknown"
            );
            (
                ScanoutMetadataSupport::Unknown,
                ScanoutMetadataSupport::Unknown,
            )
        }
    }
}

fn probe_directional_modifiers(
    vk: &VkContext,
    kms_scanout_modifiers: &[u64],
    feature: vk::ExternalMemoryFeatureFlags,
) -> (Vec<u64>, ScanoutMetadataSupport) {
    if kms_scanout_modifiers.is_empty() {
        return classify_modifier_observations(false, &[]);
    }

    let observations = kms_scanout_modifiers
        .iter()
        .copied()
        .map(|modifier| {
            (
                modifier,
                probe_scanout_modifier_single_plane_feature(vk, modifier, feature),
            )
        })
        .collect::<Vec<_>>();
    classify_modifier_observations(true, &observations)
}

pub(super) fn build_dmabuf_scanout_metadata(
    external_memory_fd: ScanoutMetadataSupport,
    prime_import: ScanoutMetadataSupport,
    prime_export: ScanoutMetadataSupport,
    output_owned_modifiers: (Vec<u64>, ScanoutMetadataSupport),
    renderer_owned_modifiers: (Vec<u64>, ScanoutMetadataSupport),
    output_owned_linear: (ScanoutMetadataSupport, KmsLinearLayout),
    renderer_owned_linear: (ScanoutMetadataSupport, KmsLinearLayout),
) -> DmabufScanoutMetadata {
    let (output_owned_modifiers, output_owned_modifier_support) = output_owned_modifiers;
    let (renderer_owned_modifiers, renderer_owned_modifier_support) = renderer_owned_modifiers;
    let output_owned_modifier_path =
        combine_required_metadata(prime_export, output_owned_modifier_support);
    let renderer_owned_modifier_path =
        combine_required_metadata(prime_import, renderer_owned_modifier_support);
    let output_owned_linear =
        build_linear_metadata(prime_export, output_owned_linear.0, output_owned_linear.1);
    let renderer_owned_linear = build_linear_metadata(
        prime_import,
        renderer_owned_linear.0,
        renderer_owned_linear.1,
    );
    DmabufScanoutMetadata {
        vulkan_external_memory_fd: external_memory_fd,
        output_owned: DmabufDirectionMetadata {
            kms_prime: prime_export,
            vulkan_modifiers: output_owned_modifier_support,
            modifiers: output_owned_modifiers,
            modifier_path: output_owned_modifier_path,
            linear: output_owned_linear,
        },
        renderer_owned: DmabufDirectionMetadata {
            kms_prime: prime_import,
            vulkan_modifiers: renderer_owned_modifier_support,
            modifiers: renderer_owned_modifiers,
            modifier_path: renderer_owned_modifier_path,
            linear: renderer_owned_linear,
        },
    }
}

fn classify_direction_metadata(
    direction: DmabufAllocationDirection,
    metadata: &DmabufDirectionMetadata,
    output_owned_gbm: ScanoutMetadataSupport,
) -> DmabufDirectionVerdict {
    use ScanoutMetadataSupport::{Supported, Unknown, Unsupported};

    let (prime_unsupported, prime_unknown, layout_incomplete, no_shared_layout) = match direction {
        DmabufAllocationDirection::OutputOwned => (
            DmabufDirectionIncompatibility::OutputOwnedKmsPrimeExportUnsupported,
            DmabufScanoutUncertainty::OutputOwnedKmsPrimeExportUnknown,
            DmabufScanoutUncertainty::OutputOwnedLayoutMetadataIncomplete,
            DmabufScanoutUncertainty::OutputOwnedNoAdvertisedSharedLayout,
        ),
        DmabufAllocationDirection::RendererOwned => (
            DmabufDirectionIncompatibility::RendererOwnedKmsPrimeImportUnsupported,
            DmabufScanoutUncertainty::RendererOwnedKmsPrimeImportUnknown,
            DmabufScanoutUncertainty::RendererOwnedLayoutMetadataIncomplete,
            DmabufScanoutUncertainty::RendererOwnedNoAdvertisedSharedLayout,
        ),
    };

    match metadata.kms_prime {
        Unsupported => return DmabufDirectionVerdict::Unsupported(prime_unsupported),
        Unknown => return DmabufDirectionVerdict::Unknown(prime_unknown),
        Supported => {}
    }

    if direction == DmabufAllocationDirection::OutputOwned
        && output_owned_gbm != ScanoutMetadataSupport::Supported
    {
        return DmabufDirectionVerdict::Unknown(
            DmabufScanoutUncertainty::OutputOwnedGbmUnavailable,
        );
    }

    if metadata.modifier_path == Supported || metadata.linear.path == Supported {
        DmabufDirectionVerdict::Supported
    } else if metadata.modifier_path == Unknown || metadata.linear.path == Unknown {
        DmabufDirectionVerdict::Unknown(layout_incomplete)
    } else {
        // Even complete advertised metadata cannot prove that the historical
        // runtime fallback will fail. Preserve original 07's broad attempt.
        DmabufDirectionVerdict::Unknown(no_shared_layout)
    }
}

pub(super) fn classify_route_from_direction_verdicts(
    relationship: RenderKmsRelationship,
    external_memory_fd: ScanoutMetadataSupport,
    output_owned: DmabufDirectionVerdict,
    renderer_owned: DmabufDirectionVerdict,
) -> DmabufScanoutVerdict {
    use DmabufDirectionVerdict::{Supported, Unknown, Unsupported};

    match relationship {
        RenderKmsRelationship::Same => return DmabufScanoutVerdict::Compatible,
        RenderKmsRelationship::Unknown => {
            return DmabufScanoutVerdict::Unknown(vec![
                DmabufScanoutUncertainty::RenderKmsRelationshipUnknown,
            ]);
        }
        RenderKmsRelationship::Different => {}
    }

    match external_memory_fd {
        ScanoutMetadataSupport::Unsupported => {
            return DmabufScanoutVerdict::Incompatible(
                DmabufScanoutIncompatibility::VulkanExternalMemoryFdUnavailable,
            );
        }
        ScanoutMetadataSupport::Unknown => {
            return DmabufScanoutVerdict::Unknown(vec![
                DmabufScanoutUncertainty::VulkanExternalMemoryFdUnknown,
            ]);
        }
        ScanoutMetadataSupport::Supported => {}
    }

    match (output_owned, renderer_owned) {
        (Supported, _) | (_, Supported) => DmabufScanoutVerdict::Compatible,
        (Unsupported(output_owned), Unsupported(renderer_owned)) => {
            DmabufScanoutVerdict::Incompatible(
                DmabufScanoutIncompatibility::BothAllocationDirectionsUnavailable {
                    output_owned,
                    renderer_owned,
                },
            )
        }
        (output_owned, renderer_owned) => {
            let mut uncertainty = Vec::with_capacity(2);
            if let Unknown(reason) = output_owned {
                uncertainty.push(reason);
            }
            if let Unknown(reason) = renderer_owned {
                uncertainty.push(reason);
            }
            debug_assert!(
                !uncertainty.is_empty(),
                "all non-unknown direction pairs were handled above"
            );
            DmabufScanoutVerdict::Unknown(uncertainty)
        }
    }
}

pub(super) fn classify_dmabuf_scanout_route(
    route: ScanoutRoute,
    metadata: &DmabufScanoutMetadata,
    output_owned_gbm: ScanoutMetadataSupport,
) -> DmabufScanoutVerdict {
    classify_route_from_direction_verdicts(
        route.relationship,
        metadata.vulkan_external_memory_fd,
        classify_direction_metadata(
            DmabufAllocationDirection::OutputOwned,
            &metadata.output_owned,
            output_owned_gbm,
        ),
        classify_direction_metadata(
            DmabufAllocationDirection::RendererOwned,
            &metadata.renderer_owned,
            output_owned_gbm,
        ),
    )
}

pub(super) fn probe_dmabuf_scanout_metadata(
    vk: &VkContext,
    drm: &crate::drm::Device,
    route: ScanoutRoute,
    kms_scanout_modifiers: &[u64],
) -> DmabufScanoutMetadata {
    let external_memory_fd = if vk.external_memory_fd.is_some() {
        ScanoutMetadataSupport::Supported
    } else {
        ScanoutMetadataSupport::Unsupported
    };
    let (prime_import, prime_export) = probe_kms_prime_metadata(drm, route);
    let (output_owned_modifiers, output_owned_modifier_support) = probe_directional_modifiers(
        vk,
        kms_scanout_modifiers,
        vk::ExternalMemoryFeatureFlags::IMPORTABLE,
    );
    let (renderer_owned_modifiers, renderer_owned_modifier_support) = probe_directional_modifiers(
        vk,
        kms_scanout_modifiers,
        vk::ExternalMemoryFeatureFlags::EXPORTABLE,
    );
    let linear_layout = kms_linear_layout(kms_scanout_modifiers);
    let output_owned_linear = probe_scanout_modifier_single_plane_feature(
        vk,
        crate::kms::vk::dri3::DRM_FORMAT_MOD_LINEAR,
        vk::ExternalMemoryFeatureFlags::IMPORTABLE,
    );
    let renderer_owned_linear =
        probe_scanout_linear_feature(vk, vk::ExternalMemoryFeatureFlags::EXPORTABLE);
    let metadata = build_dmabuf_scanout_metadata(
        external_memory_fd,
        prime_import,
        prime_export,
        (output_owned_modifiers, output_owned_modifier_support),
        (renderer_owned_modifiers, renderer_owned_modifier_support),
        (output_owned_linear, linear_layout),
        (renderer_owned_linear, linear_layout),
    );
    log::info!(
        "dma-buf metadata for {route:?}: output-owned KMS-export={:?} \
         Vulkan-import={:?} modifiers={} linear={:?}; renderer-owned \
         Vulkan-export={:?} KMS-import={:?} modifiers={} linear={:?} \
         (observation only)",
        metadata.output_owned.kms_prime,
        metadata.output_owned.vulkan_modifiers,
        format_modifiers(&metadata.output_owned.modifiers),
        metadata.output_owned.linear,
        metadata.renderer_owned.vulkan_modifiers,
        metadata.renderer_owned.kms_prime,
        format_modifiers(&metadata.renderer_owned.modifiers),
        metadata.renderer_owned.linear,
    );
    metadata
}
