use super::*;

impl ScanoutModifierOverride {
    fn describe(self) -> String {
        match self {
            Self::TiledFirst => "tiled-first".to_string(),
            Self::LinearFirst => "linear-first".to_string(),
            Self::First(modifier) => format!("0x{modifier:x}"),
        }
    }
}

/// Parse one `YSERVER_SCANOUT_MODIFIER` value. `None` for anything
/// unrecognised — a typo in a diagnostic env var must not keep the display
/// server from starting, so the caller warns and falls back to the driver
/// policy. Pure so the accepted spellings are unit-testable.
pub(super) fn parse_scanout_modifier_override(raw: &str) -> Option<ScanoutModifierOverride> {
    let token = raw.trim();
    if token.is_empty() {
        return None;
    }
    match token.to_ascii_lowercase().replace('_', "-").as_str() {
        "tiled-first" => return Some(ScanoutModifierOverride::TiledFirst),
        "linear-first" => return Some(ScanoutModifierOverride::LinearFirst),
        _ => {}
    }
    // Modifier values are logged as `0x…` by `format_modifiers`, so accept
    // that spelling verbatim; also accept bare hex and `_` digit grouping.
    let hex = token
        .strip_prefix("0x")
        .or_else(|| token.strip_prefix("0X"))
        .unwrap_or(token);
    u64::from_str_radix(&hex.replace('_', ""), 16)
        .ok()
        .map(ScanoutModifierOverride::First)
}

/// The process-wide `YSERVER_SCANOUT_MODIFIER` setting. Read once: scanout
/// pools are re-allocated on every modeset, and re-warning per BO would bury
/// the log the override exists to produce.
fn scanout_modifier_override() -> Option<ScanoutModifierOverride> {
    static OVERRIDE: OnceLock<Option<ScanoutModifierOverride>> = OnceLock::new();
    *OVERRIDE.get_or_init(|| {
        let raw = std::env::var("YSERVER_SCANOUT_MODIFIER").ok()?;
        let parsed = parse_scanout_modifier_override(&raw);
        match parsed {
            Some(over) => log::warn!(
                "YSERVER_SCANOUT_MODIFIER={raw} — overriding the per-driver scanout \
                 modifier policy with {}. This is a diagnostic knob; a wrong choice \
                 shows up as a garbled or dithered display, not as an error.",
                over.describe()
            ),
            None => log::warn!(
                "YSERVER_SCANOUT_MODIFIER={raw} is not a recognised value \
                 (expected tiled-first, linear-first, or a 0x<hex> modifier) — \
                 ignoring it and keeping the per-driver policy"
            ),
        }
        parsed
    })
}

/// Whether to order LINEAR ahead of the tiled modifiers, combining the
/// per-driver policy with any [`ScanoutModifierOverride`].
///
/// `First(_)` deliberately leaves the policy alone: it pins ONE modifier at
/// the front, and the rest of the list stays in the order the driver policy
/// asks for, so a failed pin degrades to normal behaviour.
pub(super) fn resolve_prefer_linear(
    driver_id: vk::DriverId,
    over: Option<ScanoutModifierOverride>,
) -> bool {
    match over {
        Some(ScanoutModifierOverride::TiledFirst) => false,
        Some(ScanoutModifierOverride::LinearFirst) => true,
        Some(ScanoutModifierOverride::First(_)) | None => scanout_prefers_linear(driver_id),
    }
}

/// Move `modifier` to the front of `candidates`, inserting it if absent.
///
/// Absent is legal on purpose: the GBM plan checks Vulkan importability at
/// allocation time and falls through to the next plan when it fails (see
/// [`scanout_allocation_plans`]), so pinning a modifier the Vulkan side did
/// not advertise is a survivable experiment — and one worth running, since
/// GBM can allocate layouts Vulkan declines to export.
pub(super) fn hoist_modifier_first(candidates: &mut Vec<u64>, modifier: u64) {
    candidates.retain(|&m| m != modifier);
    candidates.insert(0, modifier);
}

pub(super) fn scanout_modifier_candidates(
    vk: &VkContext,
    kms_scanout_modifiers: &[u64],
    ownership: ScanoutOwnership,
) -> Vec<u64> {
    if kms_scanout_modifiers.is_empty() {
        return Vec::new();
    }

    // Probe each KMS candidate in the allocation direction with the scanout
    // image's actual usage (color attachment and transfers, never sampled).
    // `dri3::supported_modifiers` is intentionally import-only, so using it as
    // a common pre-filter here would silently discard export-only modifiers
    // from renderer-owned plans.
    let vulkan = kms_scanout_modifiers
        .iter()
        .copied()
        .filter(|&modifier| match ownership {
            ScanoutOwnership::Output => scanout_modifier_is_single_plane_importable(vk, modifier),
            ScanoutOwnership::Renderer => scanout_modifier_is_single_plane_exportable(vk, modifier),
        })
        .collect::<Vec<_>>();
    let over = scanout_modifier_override();
    let prefer_linear = resolve_prefer_linear(vk.driver_id, over);
    let mut candidates =
        order_scanout_modifier_candidates(kms_scanout_modifiers, &vulkan, prefer_linear, |_| true);
    if let Some(ScanoutModifierOverride::First(modifier)) = over {
        if !candidates.contains(&modifier) {
            log::warn!(
                "YSERVER_SCANOUT_MODIFIER pins 0x{modifier:x}, which is not in the \
                 KMS/Vulkan intersection — trying it first anyway (GBM may still \
                 allocate it); allocation falls through to the normal order if it fails"
            );
        }
        hoist_modifier_first(&mut candidates, modifier);
    }
    // Diagnostic for scanout-corruption reports (issue #48): show what
    // the plane offered vs. what survived the Vulkan/exportable filter,
    // so a card that simply has no tiled scanout modifier on offer is
    // distinguishable from one whose tiled modifier we rejected. `override`
    // is included so a log captured during a triage round can't be mistaken
    // for the shipped policy's behaviour.
    log::info!(
        "scanout modifier select: ownership={ownership:?} kms_plane={} vulkan_supports={} \
         prefer_linear={prefer_linear} override={} -> candidates={}",
        format_modifiers(kms_scanout_modifiers),
        format_modifiers(&vulkan),
        over.map_or_else(|| "none".to_string(), ScanoutModifierOverride::describe),
        format_modifiers(&candidates),
    );
    candidates
}

pub(super) fn format_modifiers(modifiers: &[u64]) -> String {
    if modifiers.is_empty() {
        return "[]".to_string();
    }
    let joined = modifiers
        .iter()
        .map(|m| format!("0x{m:x}"))
        .collect::<Vec<_>>()
        .join(",");
    format!("[{joined}]")
}

/// Order the KMS/Vulkan modifier intersection into the sequence the
/// allocator tries.
///
/// Default (`prefer_linear = false`): **tiled modifiers first, `LINEAR` last.**
/// This fixes corruption on RDNA4/gfx12 (RX 9070 XT, issue #48) where a
/// Vulkan-rendered linear scanout buffer produces horizontal tiling
/// artifacts. RADV on gfx8/Polaris doesn't expose
/// `VK_EXT_image_drm_format_modifier`, so those cards never reach this
/// path and allocation falls through to the untagged-linear plan.
///
/// `prefer_linear = true`: **LINEAR first, tiled as fallback.**
/// Used for `NVIDIA_PROPRIETARY` where a *Vulkan-allocated* BLOCK_LINEAR_2D
/// image produces a dithered/scrambled display on Pascal hardware (GTX 1050,
/// GP107) even though allocation and KMS import succeed.
///
/// Note this does NOT mirror what GBM does — the opposite is true, and the
/// earlier claim here that "GBM implicitly selects LINEAR for scanout on
/// those cards" was wrong. NVIDIA's GBM *refuses* LINEAR for a
/// `RENDERING|SCANOUT` BO (`EINVAL`), so on the GBM path this ordering only
/// costs one guaranteed-failed attempt before a tiled variant wins. See
/// [`scanout_prefers_linear`] and the module header.
///
/// Pure (no Vulkan calls of its own) so the ordering policy is unit
/// testable; `supports_direction` is IMPORTABLE for output ownership and
/// EXPORTABLE for renderer ownership.
pub(super) fn order_scanout_modifier_candidates(
    kms_scanout_modifiers: &[u64],
    vulkan_supported: &[u64],
    prefer_linear: bool,
    supports_direction: impl Fn(u64) -> bool,
) -> Vec<u64> {
    let mut candidates = Vec::new();

    // When LINEAR is preferred (NVIDIA), add it first if both sides advertise it.
    if prefer_linear
        && kms_scanout_modifiers.contains(&crate::kms::vk::dri3::DRM_FORMAT_MOD_LINEAR)
        && vulkan_supported.contains(&crate::kms::vk::dri3::DRM_FORMAT_MOD_LINEAR)
        && supports_direction(crate::kms::vk::dri3::DRM_FORMAT_MOD_LINEAR)
    {
        candidates.push(crate::kms::vk::dri3::DRM_FORMAT_MOD_LINEAR);
    }

    // Non-LINEAR modifiers in KMS-advertised order.
    for &modifier in kms_scanout_modifiers {
        if modifier == crate::kms::vk::dri3::DRM_FORMAT_MOD_LINEAR {
            continue;
        }
        if vulkan_supported.contains(&modifier)
            && supports_direction(modifier)
            && !candidates.contains(&modifier)
        {
            candidates.push(modifier);
        }
    }

    // When tiled is preferred (default), LINEAR comes last.
    if !prefer_linear
        && kms_scanout_modifiers.contains(&crate::kms::vk::dri3::DRM_FORMAT_MOD_LINEAR)
        && vulkan_supported.contains(&crate::kms::vk::dri3::DRM_FORMAT_MOD_LINEAR)
        && supports_direction(crate::kms::vk::dri3::DRM_FORMAT_MOD_LINEAR)
        && !candidates.contains(&crate::kms::vk::dri3::DRM_FORMAT_MOD_LINEAR)
    {
        candidates.push(crate::kms::vk::dri3::DRM_FORMAT_MOD_LINEAR);
    }

    candidates
}

pub(super) fn scanout_modifier_is_single_plane_importable(vk: &VkContext, modifier: u64) -> bool {
    scanout_modifier_single_plane_supports_feature(
        vk,
        modifier,
        vk::ExternalMemoryFeatureFlags::IMPORTABLE,
    )
}

fn scanout_modifier_is_single_plane_exportable(vk: &VkContext, modifier: u64) -> bool {
    scanout_modifier_single_plane_supports_feature(
        vk,
        modifier,
        vk::ExternalMemoryFeatureFlags::EXPORTABLE,
    )
}

fn scanout_modifier_single_plane_supports_feature(
    vk: &VkContext,
    modifier: u64,
    feature: vk::ExternalMemoryFeatureFlags,
) -> bool {
    modifier_single_plane_supports_feature(vk, modifier, scanout_image_usage(), feature)
}

#[track_caller]
pub(super) fn modifier_single_plane_supports_feature(
    vk: &VkContext,
    modifier: u64,
    usage: vk::ImageUsageFlags,
    feature: vk::ExternalMemoryFeatureFlags,
) -> bool {
    use std::ffi::c_void;

    if !vk.image_drm_format_modifier || vk.external_memory_fd.is_none() {
        return false;
    }

    let mut modifier_info = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
        .drm_format_modifier(modifier)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    let mut external_info = vk::PhysicalDeviceExternalImageFormatInfo::default()
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    external_info.p_next = std::ptr::from_mut(&mut modifier_info).cast::<c_void>();

    let mut format_info = vk::PhysicalDeviceImageFormatInfo2::default()
        .format(vk::Format::B8G8R8A8_UNORM)
        .ty(vk::ImageType::TYPE_2D)
        .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
        .usage(usage);
    format_info.p_next = std::ptr::from_mut(&mut external_info).cast::<c_void>();

    let mut external_props = vk::ExternalImageFormatProperties::default();
    let mut props2 = vk::ImageFormatProperties2::default().push_next(&mut external_props);
    if crate::kms::vk::image_format_properties2(
        vk,
        "scanout::modifier_single_plane_supports_feature",
        Some(modifier),
        &format_info,
        &mut props2,
    )
    .is_err()
    {
        return false;
    }

    external_props
        .external_memory_properties
        .external_memory_features
        .contains(feature)
        && external_props
            .external_memory_properties
            .compatible_handle_types
            .contains(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
        && drm_modifier_plane_count(vk, modifier) == Some(1)
}

pub(super) fn advertised_drm_modifiers(vk: &VkContext) -> Vec<u64> {
    if !vk.image_drm_format_modifier {
        return Vec::new();
    }

    let modifier_count = {
        let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
        let mut format_props = vk::FormatProperties2::default().push_next(&mut list);
        unsafe {
            vk.instance.get_physical_device_format_properties2(
                vk.physical_device,
                vk::Format::B8G8R8A8_UNORM,
                &mut format_props,
            );
        }
        list.drm_format_modifier_count
    };
    if modifier_count == 0 {
        return Vec::new();
    }

    let mut props_storage =
        vec![vk::DrmFormatModifierPropertiesEXT::default(); modifier_count as usize];
    let mut list = vk::DrmFormatModifierPropertiesListEXT::default()
        .drm_format_modifier_properties(&mut props_storage);
    let mut format_props = vk::FormatProperties2::default().push_next(&mut list);
    unsafe {
        vk.instance.get_physical_device_format_properties2(
            vk.physical_device,
            vk::Format::B8G8R8A8_UNORM,
            &mut format_props,
        );
    }

    let entries = list.drm_format_modifier_count as usize;
    let mut modifiers = Vec::new();
    for property in props_storage.iter().take(entries) {
        if !modifiers.contains(&property.drm_format_modifier) {
            modifiers.push(property.drm_format_modifier);
        }
    }
    modifiers
}

/// Observe one explicit-modifier external-memory feature without collapsing
/// failed or missing metadata into `Unsupported`.
///
/// This intentionally does not replace
/// [`scanout_modifier_single_plane_supports_feature`]: the latter is part of
/// the established allocator's candidate construction. Keeping the runtime
/// predicate separate guarantees that adding diagnostics cannot prune or
/// reorder any allocation plan.
#[track_caller]
pub(super) fn probe_scanout_modifier_single_plane_feature(
    vk: &VkContext,
    modifier: u64,
    feature: vk::ExternalMemoryFeatureFlags,
) -> ScanoutMetadataSupport {
    use ScanoutMetadataSupport::{Supported, Unknown, Unsupported};
    use std::ffi::c_void;

    if !vk.image_drm_format_modifier || vk.external_memory_fd.is_none() {
        return Unsupported;
    }

    let mut modifier_info = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
        .drm_format_modifier(modifier)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    let mut external_info = vk::PhysicalDeviceExternalImageFormatInfo::default()
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    external_info.p_next = std::ptr::from_mut(&mut modifier_info).cast::<c_void>();

    let mut format_info = vk::PhysicalDeviceImageFormatInfo2::default()
        .format(vk::Format::B8G8R8A8_UNORM)
        .ty(vk::ImageType::TYPE_2D)
        .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
        .usage(scanout_image_usage());
    format_info.p_next = std::ptr::from_mut(&mut external_info).cast::<c_void>();

    let mut external_props = vk::ExternalImageFormatProperties::default();
    let mut props2 = vk::ImageFormatProperties2::default().push_next(&mut external_props);
    if let Err(error) = crate::kms::vk::image_format_properties2(
        vk,
        "scanout::probe_scanout_modifier_single_plane_feature",
        Some(modifier),
        &format_info,
        &mut props2,
    ) {
        return if error == vk::Result::ERROR_FORMAT_NOT_SUPPORTED {
            Unsupported
        } else {
            Unknown
        };
    }

    let external = external_props.external_memory_properties;
    if !external.external_memory_features.contains(feature)
        || !external
            .compatible_handle_types
            .contains(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
    {
        return Unsupported;
    }

    match drm_modifier_plane_count(vk, modifier) {
        Some(1) => Supported,
        Some(_) => Unsupported,
        None => Unknown,
    }
}

/// Observe Vulkan's DMA-BUF support for a plain
/// `VK_IMAGE_TILING_LINEAR` scanout image. This is the renderer-owned
/// ExplicitLinear/LegacyLinear evidence; padded explicit-linear remains part
/// of the modifier observation above.
#[track_caller]
pub(super) fn probe_scanout_linear_feature(
    vk: &VkContext,
    feature: vk::ExternalMemoryFeatureFlags,
) -> ScanoutMetadataSupport {
    use ScanoutMetadataSupport::{Supported, Unknown, Unsupported};

    if vk.external_memory_fd.is_none() {
        return Unsupported;
    }

    let mut external_info = vk::PhysicalDeviceExternalImageFormatInfo::default()
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let format_info = vk::PhysicalDeviceImageFormatInfo2::default()
        .format(vk::Format::B8G8R8A8_UNORM)
        .ty(vk::ImageType::TYPE_2D)
        .tiling(vk::ImageTiling::LINEAR)
        .usage(scanout_image_usage())
        .push_next(&mut external_info);
    let mut external_props = vk::ExternalImageFormatProperties::default();
    let mut props2 = vk::ImageFormatProperties2::default().push_next(&mut external_props);
    if let Err(error) = crate::kms::vk::image_format_properties2(
        vk,
        "scanout::probe_scanout_linear_feature",
        None,
        &format_info,
        &mut props2,
    ) {
        return if error == vk::Result::ERROR_FORMAT_NOT_SUPPORTED {
            Unsupported
        } else {
            Unknown
        };
    }

    let external = external_props.external_memory_properties;
    if external.external_memory_features.contains(feature)
        && external
            .compatible_handle_types
            .contains(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
    {
        Supported
    } else {
        Unsupported
    }
}

fn drm_modifier_plane_count(vk: &VkContext, modifier: u64) -> Option<u32> {
    let modifier_count = {
        let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
        let mut format_props = vk::FormatProperties2::default().push_next(&mut list);
        unsafe {
            vk.instance.get_physical_device_format_properties2(
                vk.physical_device,
                vk::Format::B8G8R8A8_UNORM,
                &mut format_props,
            );
        }
        list.drm_format_modifier_count
    };
    if modifier_count == 0 {
        return None;
    }

    let mut props_storage =
        vec![vk::DrmFormatModifierPropertiesEXT::default(); modifier_count as usize];
    let mut list = vk::DrmFormatModifierPropertiesListEXT::default()
        .drm_format_modifier_properties(&mut props_storage);
    let mut format_props = vk::FormatProperties2::default().push_next(&mut list);
    unsafe {
        vk.instance.get_physical_device_format_properties2(
            vk.physical_device,
            vk::Format::B8G8R8A8_UNORM,
            &mut format_props,
        );
    }
    let entries = list.drm_format_modifier_count as usize;
    props_storage
        .iter()
        .take(entries)
        .find(|p| p.drm_format_modifier == modifier)
        .map(|p| p.drm_format_modifier_plane_count)
}
