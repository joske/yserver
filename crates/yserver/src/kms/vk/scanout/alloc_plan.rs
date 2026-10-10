use super::*;

impl ScanoutAllocationPlan {
    #[must_use]
    pub(crate) const fn ownership(self) -> ScanoutOwnership {
        match self {
            Self::GbmModifier(_) => ScanoutOwnership::Output,
            Self::DrmModifier(_)
            | Self::PaddedExplicitLinear { .. }
            | Self::ExplicitLinear
            | Self::LegacyLinear => ScanoutOwnership::Renderer,
        }
    }

    pub(crate) fn describe(self) -> String {
        match self {
            Self::GbmModifier(modifier) => format!("gbm-modifier=0x{modifier:x}"),
            Self::DrmModifier(modifier) => format!("modifier=0x{modifier:x}"),
            Self::PaddedExplicitLinear { row_pitch } => {
                format!("padded-explicit-linear(pitch={row_pitch})")
            }
            Self::ExplicitLinear => "explicit-linear".to_string(),
            Self::LegacyLinear => "legacy-linear".to_string(),
        }
    }
}

pub(super) fn scanout_allocation_plans(
    vk: &VkContext,
    output_owned_modifier_candidates: &[u64],
    renderer_owned_modifier_candidates: &[u64],
    width: u32,
    gbm_available: bool,
) -> Vec<ScanoutAllocationPlan> {
    assemble_scanout_allocation_plans(
        vk.image_drm_format_modifier,
        scanout_prefers_linear(vk.driver_id),
        output_owned_modifier_candidates,
        renderer_owned_modifier_candidates,
        width,
        gbm_available,
    )
}

pub(super) fn assemble_scanout_allocation_plans(
    image_drm_format_modifier: bool,
    prefer_linear: bool,
    output_owned_modifier_candidates: &[u64],
    renderer_owned_modifier_candidates: &[u64],
    width: u32,
    gbm_available: bool,
) -> Vec<ScanoutAllocationPlan> {
    let mut plans = Vec::new();
    // GBM-allocated modifiers go FIRST — that's the ecosystem-standard path and
    // the only one that produces correct tiled scanout on NVIDIA. Per-modifier
    // Vulkan-import gating is checked at allocation time (IMPORTABLE, not the
    // Vulkan-alloc EXPORTABLE gate) so unsupported entries fall through cleanly
    // to the next plan rather than being pruned here. LINEAR (padded/legacy)
    // remains as an automatic fallback below when GBM can't produce a scanout
    // BO (e.g. modifier-less Polaris → legacy-linear).
    if gbm_available && image_drm_format_modifier {
        plans.extend(
            output_owned_modifier_candidates
                .iter()
                .copied()
                .map(ScanoutAllocationPlan::GbmModifier),
        );
    }
    // On drivers that prefer LINEAR (NVIDIA/Intel — the tiled/block-linear
    // scanout path renders garbled there via Vulkan-alloc), a tight LINEAR
    // pitch that isn't 256-aligned is rejected by the display engine at atomic
    // commit (EINVAL → device lost). Keep LINEAR but force an aligned (padded)
    // pitch via an explicit DRM-modifier layout. Only meaningful when the
    // modifier extension is present (explicit-layout create needs it). See
    // [`SCANOUT_PITCH_ALIGN`].
    //
    // Deliberately the RAW driver policy, not [`resolve_prefer_linear`]: this
    // is a fallback plan, not a preference. Someone running
    // `YSERVER_SCANOUT_MODIFIER=tiled-first` on an unaligned-pitch NVIDIA
    // width (3440 ultrawide) must still keep the known-good padded-LINEAR
    // plan behind the tiled attempts, or a garbling tiled modifier leaves no
    // survivable path to a display.
    if image_drm_format_modifier && prefer_linear && !linear_scanout_stride_aligned(width) {
        plans.push(ScanoutAllocationPlan::PaddedExplicitLinear {
            row_pitch: padded_linear_pitch(width),
        });
    }
    if image_drm_format_modifier {
        plans.extend(
            renderer_owned_modifier_candidates
                .iter()
                .copied()
                .map(ScanoutAllocationPlan::DrmModifier),
        );
    }
    plans.push(ScanoutAllocationPlan::ExplicitLinear);
    plans.push(ScanoutAllocationPlan::LegacyLinear);
    plans
}

/// Whether scanout BO allocation should try `LINEAR` before the tiled
/// DRM modifiers (see [`order_scanout_modifier_candidates`]).
///
/// **Scope: the Vulkan-alloc plans only, in practice.** This policy predates
/// the GBM-first path (`5fdb56eb`, 2026-07-22). On NVIDIA the GBM-LINEAR plan
/// now fails with `EINVAL` before this ordering can matter, so NVIDIA runs
/// GBM block-linear tiled and reaches the plans this policy governs only when
/// `gbm_create_device` itself failed. Do NOT read a `prefer_linear=true` log
/// line as "this card is scanning out LINEAR" — check which plan *succeeded*.
/// See the module header for the measurements.
///
/// Driver-split policy, each entry HW-confirmed against a real dithered/
/// corrupted scanout **on the Vulkan-alloc path**:
/// - **NVIDIA proprietary** (GTX 1050/Pascal): a Vulkan-allocated
///   BLOCK_LINEAR_2D image produces a dithered display, for every gob-height
///   variant. The same modifier allocated through GBM is clean — the driver
///   applies a display-engine layout Vulkan-alloc doesn't reproduce.
/// - **Intel Mesa (ANV)** (Kaby Lake i5-7200U): the I915 Y_TILED modifier
///   (`0x0100000000000002`) selected first produces the same dithering.
///
/// AMD (RADV) is deliberately NOT in this set: RDNA4/gfx12 *requires* the
/// tiled modifier — a LINEAR scanout buffer corrupts there (issue #48) —
/// and RDNA2 scans out tiled fine. Other drivers default to tiled-first;
/// add them here only after a confirmed dithering report, not speculatively
/// (e.g. Asahi/M1 has not shown the problem and stays tiled-first).
pub(super) fn scanout_prefers_linear(driver_id: vk::DriverId) -> bool {
    matches!(
        driver_id,
        vk::DriverId::NVIDIA_PROPRIETARY | vk::DriverId::INTEL_OPEN_SOURCE_MESA
    )
}

/// Byte alignment the KMS scanout pitch must satisfy on the display engines
/// that otherwise prefer LINEAR (NVIDIA/Intel). NVIDIA's display controller
/// requires a 256-byte-aligned scanout stride; a Vulkan `LINEAR` image has a
/// TIGHT pitch (`width * 4` bytes for B8G8R8A8), so at widths whose byte-pitch
/// isn't 256-aligned the LINEAR framebuffer is rejected at atomic commit
/// (`EINVAL` → BO invalidated → `ERROR_DEVICE_LOST` → respawn loop).
///
/// HW-confirmed **2026-07-20, before the GBM-first path** (`5fdb56eb`,
/// 2026-07-22): GTX 1050 @ 2560 wide → pitch 10240 = 256×40 (OK, scanned out
/// LINEAR); GTX 1060 @ 3440 ultrawide → tight pitch 13760 (mod 256 = 192,
/// rejected at atomic commit → device lost). Same driver — only the stride
/// alignment differed; both 2560 and 1920 (aligned) rendered clean via LINEAR
/// on the 1060. So when the tight LINEAR pitch is unaligned we keep LINEAR but
/// allocate it with an explicit padded (aligned) pitch — see
/// [`padded_linear_pitch`] / `ScanoutAllocationPlan::PaddedExplicitLinear`.
///
/// Two corrections since those measurements, both from the module header's
/// 2026-07-30 data:
///
/// 1. The old claim here that "the tiled (block-linear) modifier is NOT a
///    usable escape — yserver's tiled scanout renders garbled on NVIDIA" was
///    true only of the *Vulkan-alloc* tiled path. GBM-allocated block-linear
///    displays correctly on NVIDIA, including on that same GTX 1050.
/// 2. Consequently the 1050 no longer "scans out LINEAR" at all: GBM-LINEAR
///    fails with `EINVAL` and it runs GBM tiled.
///
/// 3. This plan is consequently UNREACHABLE whenever GBM works. The 1060 was
///    re-measured 2026-07-26 (yserver 1.3.0 `46439bc67d89`, XFCE, 3440x1440 on
///    HDMI-1) and took `gbm-modifier=0x3000000004fe015` at **pitch 13760** —
///    the very unaligned pitch this constant exists to avoid — for a healthy
///    91-second session with zero device-lost, EINVAL or respawn signatures.
///    The unaligned-pitch rejection is specific to a *LINEAR* framebuffer; a
///    block-linear one at the same width is fine.
///
/// So the padded-pitch plan now guards only the no-GBM fallback. It stays:
/// unreachable costs nothing, and `gbm_create_device` failing on an ultrawide
/// NVIDIA box without it costs a respawn loop.
pub(super) const SCANOUT_PITCH_ALIGN: u32 = 256;
/// Scanout format is `B8G8R8A8_UNORM` → 4 bytes/pixel.
const SCANOUT_BYTES_PER_PIXEL: u32 = 4;

/// True if a tight `LINEAR` scanout buffer `width` px wide has a display-engine-
/// acceptable (256-byte-aligned) pitch. See [`SCANOUT_PITCH_ALIGN`].
pub(super) fn linear_scanout_stride_aligned(width: u32) -> bool {
    width
        .checked_mul(SCANOUT_BYTES_PER_PIXEL)
        .is_some_and(|pitch| pitch.is_multiple_of(SCANOUT_PITCH_ALIGN))
}

/// Pad a tight LINEAR scanout pitch up to [`SCANOUT_PITCH_ALIGN`]. Used to give
/// the display engine an aligned stride at widths (e.g. 3440 ultrawide) whose
/// tight `width*4` pitch it would otherwise reject at atomic commit.
pub(super) fn padded_linear_pitch(width: u32) -> u32 {
    let tight = width.saturating_mul(SCANOUT_BYTES_PER_PIXEL);
    tight
        .div_ceil(SCANOUT_PITCH_ALIGN)
        .saturating_mul(SCANOUT_PITCH_ALIGN)
}

pub(super) fn order_copied_source_plans(
    renderer_modifiers: &[u64],
    mut supports_pair: impl FnMut(u64) -> bool,
) -> Vec<CopiedSourcePlan> {
    let linear = crate::kms::vk::dri3::DRM_FORMAT_MOD_LINEAR;
    let mut plans: Vec<CopiedSourcePlan> = Vec::new();

    // Native modifiers retain renderer A's advertised order. Modifier 0 is
    // deliberately excluded from this tier even if the driver lists it amid
    // native layouts.
    for &modifier in renderer_modifiers {
        if modifier != linear
            && !plans.iter().any(|plan| plan.modifier() == modifier)
            && supports_pair(modifier)
        {
            plans.push(CopiedSourcePlan::DrmModifier(modifier));
        }
    }

    // Query explicit modifier 0 independently and append it exactly once.
    // This keeps LINEAR out of the native ordering regardless of where the
    // driver places it in the advertised list.
    if supports_pair(linear) {
        plans.push(CopiedSourcePlan::DrmModifier(linear));
    }
    plans
}

pub(super) fn exact_copied_source_plans(
    render_vk: &VkContext,
    sink_vk: &VkContext,
) -> Vec<CopiedSourcePlan> {
    let advertised = advertised_drm_modifiers(render_vk);
    let plans = order_copied_source_plans(&advertised, |modifier| {
        modifier_single_plane_supports_feature(
            render_vk,
            modifier,
            COPIED_TRANSPORT_IMAGE_USAGE,
            vk::ExternalMemoryFeatureFlags::EXPORTABLE,
        ) && modifier_single_plane_supports_feature(
            sink_vk,
            modifier,
            COPIED_SINK_IMPORT_USAGE,
            vk::ExternalMemoryFeatureFlags::IMPORTABLE,
        )
    });
    let candidates = plans.iter().map(|plan| plan.modifier()).collect::<Vec<_>>();
    log::info!(
        "copied transport modifier select: renderer_advertised={} -> native_then_linear={}",
        format_modifiers(&advertised),
        format_modifiers(&candidates),
    );
    plans
}

pub(super) fn assemble_copied_scanout_plans(
    destinations: &[ScanoutAllocationPlan],
    sources: &[CopiedSourcePlan],
) -> Vec<CopiedScanoutPlan> {
    let mut plans = Vec::new();
    for linear_tier in [false, true] {
        for &destination in destinations {
            for &source in sources
                .iter()
                .filter(|source| source.is_linear() == linear_tier)
            {
                plans.push(CopiedScanoutPlan {
                    source,
                    destination,
                });
            }
        }
    }
    plans
}

pub(super) fn validate_copied_route_pair(
    route: ScanoutRoute,
    destination_route: ScanoutRoute,
) -> io::Result<()> {
    if route.kms_device_key != destination_route.kms_device_key {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "copied outer and destination routes name different KMS devices",
        ));
    }
    if destination_route.relationship != RenderKmsRelationship::Same {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "copied destination route must be sink-local",
        ));
    }
    if route.render_device_id == destination_route.render_device_id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "copied source and destination routes name the same renderer",
        ));
    }
    Ok(())
}
