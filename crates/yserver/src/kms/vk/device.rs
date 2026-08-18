//! VkContext: instance + physical/logical device + queues + debug messenger.
//!
//! Lifetime is the full backend lifetime. Drop order matters:
//! device-level handles before device, device before instance,
//! instance-level loaders before instance.

use ash::vk;
use std::{
    ffi::{CStr, c_char, c_void},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use crate::platform::drm::DrmDeviceKey;

/// DRM node identities advertised by one Vulkan physical device through
/// `VK_EXT_physical_device_drm`.
///
/// A primary node is identity metadata only: it does not imply KMS support.
/// In particular, split display/render systems such as Asahi legitimately
/// advertise a renderer primary node that differs from the KMS controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct VulkanDrmIdentity {
    pub(crate) primary: Option<DrmDeviceKey>,
    pub(crate) render: Option<DrmDeviceKey>,
}

/// Non-owning renderer identity whose physical-device handle belongs to this
/// `VkContext`'s instance. Consumers must not retain it past the context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct VulkanDrmPhysicalDevice {
    pub(crate) physical_device: vk::PhysicalDevice,
    /// Stable identity usable from a different Vulkan instance. Platform
    /// inventory records this alongside the DRM endpoint so copied scanout can
    /// open the exact sink GPU without re-running generic device scoring.
    pub(crate) selector: VulkanDeviceSelector,
    pub(crate) identity: VulkanDrmIdentity,
}

/// Cross-instance identity for one Vulkan physical device.
///
/// DRM render-node identity remains the platform's endpoint identity.  This
/// selector serves a narrower purpose: a disposable PRIME route probe creates
/// a fresh `VkInstance`, so it cannot reuse the live instance's opaque
/// `VkPhysicalDevice` handle.  Matching both UUIDs selects the same physical
/// device *and* ICD without falling back to device-type scoring.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct VulkanDeviceSelector {
    device_uuid: [u8; vk::UUID_SIZE],
    driver_uuid: [u8; vk::UUID_SIZE],
}

impl VulkanDeviceSelector {
    /// Reconstruct the stable cross-instance selector carried over the
    /// private helper protocol. No opaque Vulkan handle crosses the process
    /// boundary; both UUIDs are required to select the same GPU and ICD.
    pub(crate) const fn from_uuid_pair(
        device_uuid: [u8; vk::UUID_SIZE],
        driver_uuid: [u8; vk::UUID_SIZE],
    ) -> Self {
        Self {
            device_uuid,
            driver_uuid,
        }
    }

    /// Return the scalar UUID pair suitable for the private helper protocol.
    #[must_use]
    pub(crate) const fn uuid_pair(self) -> ([u8; vk::UUID_SIZE], [u8; vk::UUID_SIZE]) {
        (self.device_uuid, self.driver_uuid)
    }

    #[cfg(test)]
    pub(crate) const fn for_tests(seed: u8) -> Self {
        Self::from_uuid_pair(
            [seed; vk::UUID_SIZE],
            [seed.wrapping_add(0x40); vk::UUID_SIZE],
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeviceProfile {
    Compositor,
    Transfer,
}

/// Controls the defensive device-wide wait in [`VkContext::drop`].
///
/// Live contexts always retain the wait. Disposable probe contexts may skip
/// it only after their owner has observed every submitted fence and declared
/// the complete queue graph quiescent. Uncertain attempts are retained instead
/// of being marked, so an unwind still takes the conservative path.
#[derive(Debug)]
struct DropWaitPolicy {
    disposable_probe: bool,
    probe_quiescent: AtomicBool,
}

impl DropWaitPolicy {
    const fn live() -> Self {
        Self {
            disposable_probe: false,
            probe_quiescent: AtomicBool::new(false),
        }
    }

    const fn disposable_probe() -> Self {
        Self {
            disposable_probe: true,
            probe_quiescent: AtomicBool::new(false),
        }
    }

    fn mark_probe_quiescent(&self) {
        assert!(
            self.disposable_probe,
            "only disposable probe contexts may bypass the drop idle"
        );
        self.probe_quiescent.store(true, Ordering::Release);
    }

    fn requires_device_idle(&self) -> bool {
        !self.disposable_probe || !self.probe_quiescent.load(Ordering::Acquire)
    }
}

/// DRM endpoint the compositor wants its Vulkan renderer to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RequestedRenderDevice {
    render: Option<DrmDeviceKey>,
    display_primary: DrmDeviceKey,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PhysicalDeviceSelection {
    Automatic,
    RenderEndpoint(RequestedRenderDevice),
    Exact(VulkanDeviceSelector),
}

/// Lives for the entire backend lifetime. Drop order matters: device
/// before instance; instance-level loaders before instance.
///
/// Extension loaders (`debug_utils_instance`, `external_semaphore_fd`)
/// must be stored, not reconstructed per call: the underlying ash
/// loader resolves function pointers via `vkGetInstanceProcAddr` /
/// `vkGetDeviceProcAddr` once and caches them. Drop also goes through
/// the loader (`destroy_debug_utils_messenger`).
#[allow(dead_code)] // fields populated incrementally across sub-phase 4.1.1.
pub struct VkContext {
    pub entry: ash::Entry,
    pub instance: ash::Instance,
    pub debug_utils_instance: ash::ext::debug_utils::Instance,
    pub physical_device: vk::PhysicalDevice,
    /// Stable cross-instance selector for the exact physical device and ICD
    /// backing `physical_device`.
    selected_device_selector: VulkanDeviceSelector,
    /// DRM identity advertised by the selected physical device. This is
    /// copied into the platform's separate `RenderDevice` record; it is not
    /// treated as ownership of a KMS device.
    pub(crate) selected_drm_identity: Option<VulkanDrmIdentity>,
    /// Same-instance inventory of graphics+transfer queue-capable physical
    /// devices that advertise a DRM render node. The platform turns these into distinct
    /// `RenderDevice` endpoints for later PRIME routing.
    pub(crate) drm_physical_devices: Vec<VulkanDrmPhysicalDevice>,
    pub device: ash::Device,
    pub external_semaphore_fd: ash::khr::external_semaphore_fd::Device,
    pub external_memory_fd: Option<ash::khr::external_memory_fd::Device>,
    pub image_drm_format_modifier_ext: Option<ash::ext::image_drm_format_modifier::Device>,
    /// True when `VK_EXT_image_drm_format_modifier` is enabled on the
    /// device. Phase 4.2 DRI3 import needs this for non-LINEAR tilings;
    /// when false, `kms::vk::dri3::supported_modifiers` returns
    /// `[DRM_FORMAT_MOD_LINEAR]` per design §4 fallback matrix.
    pub image_drm_format_modifier: bool,
    /// Whether `VK_EXT_queue_family_foreign` is enabled. Copied scanout moves
    /// exclusive dma-buf images between distinct Vulkan physical devices (and
    /// KMS), which requires `VK_QUEUE_FAMILY_FOREIGN_EXT`; the core
    /// `VK_QUEUE_FAMILY_EXTERNAL` sentinel is restricted to the same physical
    /// device and driver UUID.
    pub(crate) queue_family_foreign: bool,
    /// GLX-TFP: per-driver tiling strategy for the exported image,
    /// cached on first successful allocation. LINEAR is preferred —
    /// Turnip / Adreno same-GPU dma-buf sharing only delivers live
    /// pixels through LINEAR (its modifier-tiled UBWC keeps
    /// compression metadata in driver caches that don't reach the
    /// dma-buf-backed memory, so the GL importer samples a frozen
    /// snapshot). RADV rejects LINEAR + COLOR_ATTACHMENT + dma-buf
    /// with `VK_ERROR_FORMAT_NOT_SUPPORTED`, in which case
    /// [`super::target::allocate_exportable`] falls back to the
    /// modifier path and caches that. Empty until the first
    /// allocation attempt.
    pub tfp_tiling_strategy: std::sync::OnceLock<super::target::TilingStrategy>,
    pub graphics_queue_family: u32,
    pub graphics_queue: vk::Queue,
    /// Whether the selected graphics+transfer queue family can also execute
    /// compute commands. Device selection prefers such a family, but retains
    /// the graphics+transfer-only fallback for devices whose compositor path
    /// does not need compute.
    graphics_queue_supports_compute: bool,
    /// Maximum descriptor range, in bytes, for one storage buffer.
    max_storage_buffer_range: u64,
    pub debug_messenger: Option<vk::DebugUtilsMessengerEXT>,
    /// Cached `VkPhysicalDeviceDriverProperties::driverID` for the
    /// picked device. Kept as a diagnostic for log lines / future
    /// driver-specific quirks; the scanout path itself no longer
    /// branches on it (the GBM-first cross-driver Venus problem
    /// went away with the Vulkan-first pivot).
    #[allow(dead_code)]
    pub driver_id: vk::DriverId,
    /// `VkPhysicalDeviceProperties::deviceType` of the picked device.
    /// `CPU` means a software rasterizer (llvmpipe/lavapipe) — usable
    /// for headless tests but NOT for real KMS scanout (see
    /// [`Self::is_software_rasterizer`]).
    pub device_type: vk::PhysicalDeviceType,
    /// Nanoseconds per timestamp-query tick (`limits.timestampPeriod`). `0.0`
    /// means the selected queue family exposes no timestamp bits, so the
    /// compose GPU-render timer (`gpu_render_ns` telemetry) is skipped.
    pub timestamp_period: f32,
    /// Whether the device supports the optional `dualSrcBlend` feature,
    /// required by the RENDER `component_alpha` (subpixel/LCD text AA)
    /// path's SRC1_* blend factors. `false` on Broadcom V3D (RPi 4/400,
    /// v3dv); component-alpha masks then fall back to grayscale AA
    /// instead of requesting an unavailable feature.
    pub component_alpha_supported: bool,
    drop_wait_policy: DropWaitPolicy,
}

impl VkContext {
    pub fn new() -> Result<Arc<Self>, VkInitError> {
        Self::new_with_selection(
            PhysicalDeviceSelection::Automatic,
            DeviceProfile::Compositor,
            DropWaitPolicy::live(),
        )
    }

    /// Build the context on the renderer associated with `render`.
    ///
    /// The render-node identity is authoritative. `display_primary` is kept
    /// only for conflict diagnostics and same-device logging; equality is not
    /// required for split render/display hardware and never selects a device.
    pub(crate) fn new_for_render_device(
        render: Option<DrmDeviceKey>,
        display_primary: DrmDeviceKey,
    ) -> Result<Arc<Self>, VkInitError> {
        Self::new_with_selection(
            PhysicalDeviceSelection::RenderEndpoint(RequestedRenderDevice {
                render,
                display_primary,
            }),
            DeviceProfile::Compositor,
            DropWaitPolicy::live(),
        )
    }

    /// Create a disposable logical device on the exact physical device and
    /// ICD used by `live`.
    ///
    /// This never re-runs generic GPU scoring.  PRIME probing must not render
    /// on a merely similar GPU, and an instance-local `VkPhysicalDevice`
    /// handle cannot be carried into the fresh instance.
    pub(crate) fn new_disposable_for_same_physical_device(
        live: &Self,
    ) -> Result<Arc<Self>, VkInitError> {
        Self::new_with_selection(
            PhysicalDeviceSelection::Exact(live.selected_device_selector),
            DeviceProfile::Compositor,
            DropWaitPolicy::disposable_probe(),
        )
    }

    /// Create the minimal sink-side context used by copied scanout on the
    /// exact physical device and ICD named by `selector`.
    ///
    /// The sink only imports external memory and semaphores, records transfer
    /// commands, and exports a completion semaphore. It deliberately does not
    /// request compositor-only dynamic rendering, logic operations, or
    /// dual-source blending.
    pub(crate) fn new_transfer_for_device(
        selector: VulkanDeviceSelector,
    ) -> Result<Arc<Self>, VkInitError> {
        Self::new_with_selection(
            PhysicalDeviceSelection::Exact(selector),
            DeviceProfile::Transfer,
            DropWaitPolicy::live(),
        )
    }

    /// Create a sink-side transfer context owned only by one disposable route
    /// probe. Its Drop remains conservative until the probe explicitly proves
    /// every submitted fence complete.
    pub(crate) fn new_disposable_transfer_for_device(
        selector: VulkanDeviceSelector,
    ) -> Result<Arc<Self>, VkInitError> {
        Self::new_with_selection(
            PhysicalDeviceSelection::Exact(selector),
            DeviceProfile::Transfer,
            DropWaitPolicy::disposable_probe(),
        )
    }

    /// Stable UUID pair for the selected physical device and ICD.
    #[must_use]
    pub(crate) const fn device_selector(&self) -> VulkanDeviceSelector {
        self.selected_device_selector
    }

    fn new_with_selection(
        selection: PhysicalDeviceSelection,
        profile: DeviceProfile,
        drop_wait_policy: DropWaitPolicy,
    ) -> Result<Arc<Self>, VkInitError> {
        let entry = unsafe { ash::Entry::load()? };
        let app_info = vk::ApplicationInfo::default()
            .application_name(c"yserver")
            .application_version(vk::make_api_version(0, 0, 1, 0))
            .engine_name(c"yserver-kms")
            .api_version(vk::API_VERSION_1_3);

        let ext_cstrs = super::instance::required_instance_extensions();
        let ext_ptrs: Vec<_> = ext_cstrs.iter().map(|c| c.as_ptr()).collect();

        // Validation layer in debug builds only, and only if the
        // installed Vulkan loader actually has it. Some environments
        // (e.g. the vng guest with no `vulkan-validation-layers`
        // installed) don't ship it; without this guard,
        // `vkCreateInstance` returns `VK_ERROR_LAYER_NOT_PRESENT` and
        // the whole backend falls back to pixman.
        //
        // Enable cases:
        //   - debug build: always try (validation-layer cost is fine).
        //   - release build with `YSERVER_VK_VALIDATION` set: opt-in
        //     for diagnosing release-mode-only bugs (e.g. perf-branch
        //     timeline-semaphore races) without rebuilding debug.
        let validation_layer_name = c"VK_LAYER_KHRONOS_validation";
        let validation_requested =
            cfg!(debug_assertions) || std::env::var_os("YSERVER_VK_VALIDATION").is_some();
        let validation_available =
            validation_requested && validation_layer_present(&entry, validation_layer_name);
        let layer_ptrs: Vec<*const c_char> = if validation_available {
            vec![validation_layer_name.as_ptr()]
        } else {
            Vec::new()
        };
        if validation_requested && !validation_available {
            log::warn!(
                "vulkan: validation layer requested but not present (install \
                 `vulkan-validation-layers` package); continuing without"
            );
        } else if validation_available {
            log::info!("vulkan: validation layer enabled");
        }

        let create_info = vk::InstanceCreateInfo::default()
            .application_info(&app_info)
            .enabled_extension_names(&ext_ptrs)
            .enabled_layer_names(&layer_ptrs);

        let instance = unsafe { entry.create_instance(&create_info, None)? };

        // Build the rest with manual error-cleanup. If any step after
        // create_instance fails, we must destroy the instance; same
        // applies to debug messenger / device once they exist.
        let debug_utils_instance = ash::ext::debug_utils::Instance::new(&entry, &instance);

        let debug_messenger = match create_debug_messenger(&debug_utils_instance) {
            Ok(m) => m,
            Err(e) => {
                unsafe { instance.destroy_instance(None) };
                return Err(e);
            }
        };

        let PickedPhysicalDevice {
            physical_device,
            graphics_queue_family,
            selected_device_selector,
            selected_drm_identity,
            drm_physical_devices,
        } = match pick_physical_device(&instance, selection) {
            Ok(t) => t,
            Err(e) => {
                unsafe {
                    if let Some(m) = debug_messenger {
                        debug_utils_instance.destroy_debug_utils_messenger(m, None);
                    }
                    instance.destroy_instance(None);
                }
                return Err(e);
            }
        };

        // Device extensions actually used by Phase 4.1.2's
        // Vulkan-first scanout path:
        //
        // - VK_KHR_external_memory_fd: vkGetMemoryFdKHR (export the
        //   bound image memory as a dma-buf).
        // - VK_EXT_external_memory_dma_buf: handle type `DMA_BUF` for
        //   the export (`ExternalMemoryImageCreateInfo` + the alloc).
        // - VK_KHR_external_semaphore_fd: vkGetSemaphoreFdKHR(SYNC_FD)
        //   for the IN_FENCE_FD handoff to KMS.
        //
        // Phase 4.2 reintroduction: VK_EXT_image_drm_format_modifier
        // is now requested for DRI3 tiled-image import. Drivers that
        // lack it (notably lavapipe at the time of writing) will still
        // function — `supported_modifiers` returns `[LINEAR]` per the
        // design §4 fallback matrix.
        //
        // Intentionally NOT requested:
        // - VK_KHR_swapchain — WSI is out of scope (design §1); KMS
        //   pageflip is our presentation path.
        // - VK_KHR_dynamic_rendering_local_read — only Phase 4.1.4.6
        //   ShaderRMW PictOps need this; deferred until that lands.
        //
        // The filter still drops anything the picked device doesn't
        // expose; on a healthy device every wanted extension makes
        // it through. The warning path remains as an early-fail signal
        // for misconfigured environments.
        let wanted: &[&CStr] = &[
            ash::khr::external_memory_fd::NAME,
            ash::ext::external_memory_dma_buf::NAME,
            ash::khr::external_semaphore_fd::NAME,
            ash::ext::image_drm_format_modifier::NAME,
            ash::ext::queue_family_foreign::NAME,
        ];
        let supported_device_exts =
            match unsafe { instance.enumerate_device_extension_properties(physical_device) } {
                Ok(v) => v,
                Err(e) => {
                    unsafe {
                        if let Some(m) = debug_messenger {
                            debug_utils_instance.destroy_debug_utils_messenger(m, None);
                        }
                        instance.destroy_instance(None);
                    }
                    return Err(VkInitError::Vk(e));
                }
            };
        let device_extension_names: Vec<&'static CStr> = wanted
            .iter()
            .copied()
            .filter(|ext| {
                let ok = supported_device_exts.iter().any(|p| {
                    p.extension_name_as_c_str()
                        .map(|s| s == *ext)
                        .unwrap_or(false)
                });
                if !ok {
                    log::warn!(
                        "vulkan: physical device lacks {} — Vulkan-fed scanout will not work",
                        ext.to_string_lossy()
                    );
                }
                ok
            })
            .collect();
        let device_extensions: Vec<*const c_char> =
            device_extension_names.iter().map(|c| c.as_ptr()).collect();

        let priorities = [1.0_f32];
        let queue_info = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(graphics_queue_family)
            .queue_priorities(&priorities)];

        let compositor_features = profile == DeviceProfile::Compositor;
        let mut features13 = vk::PhysicalDeviceVulkan13Features::default()
            .dynamic_rendering(compositor_features)
            .synchronization2(true);

        // `logicOp` (the X11 GcFunction Xor/And/Or/Invert fill path —
        // all 16 variants map 1:1 to `VkLogicOp`) and `dualSrcBlend`
        // (the SRC1_* blend factors used by the RENDER `component_alpha`
        // subpixel-text path) are *optional* Vulkan 1.0 features, NOT
        // universally supported: Broadcom V3D (RPi 4/400, v3dv) ships
        // without `dualSrcBlend`, so requesting it unconditionally made
        // `vkCreateDevice` fail with `ERROR_FEATURE_NOT_PRESENT`. Query
        // what the device supports and enable only that; a missing
        // `dualSrcBlend` degrades component-alpha to grayscale AA
        // (`component_alpha_supported`), a missing `logicOp` (no
        // fallback yet) is a hard, named error.
        let supported_features = if compositor_features {
            unsafe { instance.get_physical_device_features(physical_device) }
        } else {
            vk::PhysicalDeviceFeatures::default()
        };
        let selected = match select_device_features_for_profile(&supported_features, profile) {
            Ok(selected) => selected,
            Err(error) => {
                unsafe {
                    if let Some(m) = debug_messenger {
                        debug_utils_instance.destroy_debug_utils_messenger(m, None);
                    }
                    instance.destroy_instance(None);
                }
                return Err(error);
            }
        };
        let enabled_features = selected.enabled;
        let component_alpha_supported = selected.component_alpha;
        if compositor_features && !component_alpha_supported {
            log::warn!(
                "vulkan: device lacks dualSrcBlend — RENDER component-alpha \
                 (subpixel text AA) falls back to grayscale AA"
            );
        }

        let device_info = vk::DeviceCreateInfo::default()
            .queue_create_infos(&queue_info)
            .enabled_extension_names(&device_extensions)
            .enabled_features(&enabled_features)
            .push_next(&mut features13);

        let device = match unsafe { instance.create_device(physical_device, &device_info, None) } {
            Ok(d) => d,
            Err(e) => {
                unsafe {
                    if let Some(m) = debug_messenger {
                        debug_utils_instance.destroy_debug_utils_messenger(m, None);
                    }
                    instance.destroy_instance(None);
                }
                return Err(VkInitError::Vk(e));
            }
        };
        let graphics_queue = unsafe { device.get_device_queue(graphics_queue_family, 0) };
        let selected_queue_family =
            unsafe { instance.get_physical_device_queue_family_properties(physical_device) }
                .get(graphics_queue_family as usize)
                .copied()
                .expect("selected Vulkan queue family remains present");
        let graphics_queue_supports_compute = selected_queue_family
            .queue_flags
            .contains(vk::QueueFlags::COMPUTE);
        let external_semaphore_fd =
            ash::khr::external_semaphore_fd::Device::new(&instance, &device);
        let external_memory_fd_supported =
            device_extension_names.contains(&ash::khr::external_memory_fd::NAME);
        let external_memory_fd = if external_memory_fd_supported {
            Some(ash::khr::external_memory_fd::Device::new(
                &instance, &device,
            ))
        } else {
            None
        };
        let image_drm_format_modifier =
            device_extension_names.contains(&ash::ext::image_drm_format_modifier::NAME);
        let image_drm_format_modifier_ext = if image_drm_format_modifier {
            Some(ash::ext::image_drm_format_modifier::Device::new(
                &instance, &device,
            ))
        } else {
            None
        };
        let queue_family_foreign =
            device_extension_names.contains(&ash::ext::queue_family_foreign::NAME);

        // Driver-id query. Diagnostic-only after the Vulkan-first
        // pivot — no path branches on it. Kept so future quirks can
        // re-introduce branches without re-querying.
        let mut driver_props = vk::PhysicalDeviceDriverProperties::default();
        let mut props2 = vk::PhysicalDeviceProperties2::default().push_next(&mut driver_props);
        unsafe {
            instance.get_physical_device_properties2(physical_device, &mut props2);
        }
        // Read from props2 (which mutably borrows driver_props) before
        // reading driver_props directly, so props2's borrow ends first.
        let device_type = props2.properties.device_type;
        // ns per timestamp-query tick (0.0 ⇒ device has no usable timestamp
        // support → gpu_render_ns telemetry stays 0). See `record_compose`.
        let timestamp_period = effective_timestamp_period(
            props2.properties.limits.timestamp_period,
            selected_queue_family.timestamp_valid_bits,
        );
        let max_storage_buffer_range = u64::from(props2.properties.limits.max_storage_buffer_range);
        let driver_id = driver_props.driver_id;

        Ok(Arc::new(VkContext {
            entry,
            instance,
            debug_utils_instance,
            physical_device,
            selected_device_selector,
            selected_drm_identity,
            drm_physical_devices,
            device,
            external_semaphore_fd,
            external_memory_fd,
            image_drm_format_modifier_ext,
            image_drm_format_modifier,
            queue_family_foreign,
            tfp_tiling_strategy: std::sync::OnceLock::new(),
            graphics_queue_family,
            graphics_queue,
            graphics_queue_supports_compute,
            max_storage_buffer_range,
            debug_messenger,
            driver_id,
            device_type,
            timestamp_period,
            component_alpha_supported,
            drop_wait_policy,
        }))
    }

    /// True when the picked Vulkan device is a software rasterizer
    /// (`VK_PHYSICAL_DEVICE_TYPE_CPU`, i.e. llvmpipe/lavapipe).
    ///
    /// Fine for headless rendering/tests, but driving **real KMS
    /// scanout** off a software device hard-hangs the machine on
    /// hardware that can't scan out the CPU/host-memory buffer
    /// (observed: nouveau on Pascal — the GPU's atomic commit wedges).
    /// The scanout bring-up refuses by default when this is true;
    /// see `PlatformBackend::from_platform_init`. Venus (virtio-gpu
    /// passthrough) reports `VIRTUAL_GPU`, not `CPU`, so it is not
    /// caught here.
    #[must_use]
    pub fn is_software_rasterizer(&self) -> bool {
        self.device_type == vk::PhysicalDeviceType::CPU
    }

    /// Mark a disposable context as fence-proven quiescent.
    ///
    /// After this call no queue submission may be made through this context.
    /// Its remaining child resources may be destroyed directly, and the final
    /// context Drop skips the otherwise unbounded `vkDeviceWaitIdle` fallback.
    pub(crate) fn mark_disposable_probe_quiescent(&self) {
        self.drop_wait_policy.mark_probe_quiescent();
    }

    pub(crate) fn requires_drop_device_idle(&self) -> bool {
        self.drop_wait_policy.requires_device_idle()
    }

    /// Whether the queue used by this context can record compute dispatches.
    #[must_use]
    pub(crate) const fn graphics_queue_supports_compute(&self) -> bool {
        self.graphics_queue_supports_compute
    }

    /// Vulkan `maxStorageBufferRange`, widened for checked byte-size math.
    #[must_use]
    pub(crate) const fn max_storage_buffer_range(&self) -> u64 {
        self.max_storage_buffer_range
    }
}

/// Pre-flight check run before KMS buffer allocation / modeset: enumerate the
/// Vulkan physical devices (instance-level only — no VkDevice and no screen
/// blank) and refuse when every available device is a software rasterizer
/// (`CPU` type — llvmpipe/lavapipe). The caller may already have opened a DRM
/// card so it can distinguish real scanout from a zero-output headless start.
///
/// Rationale: driving real KMS scanout off software Vulkan hard-hangs
/// the machine (observed on two KMS drivers: simpledrm and nvidia-drm —
/// no ping, no journal, power-cycle required). The in-bring-up guard in
/// `PlatformBackend::from_platform_init` exists too, but it runs after the
/// initial modeset; this preflight refuses before yserver allocates or commits
/// a scanout buffer.
///
/// Conservative on probe errors: if the loader / instance / enumeration
/// itself fails, this returns `Ok(())` and lets the real `VkContext::new`
/// produce its proper error — the preflight only blocks the one case it
/// can positively identify (all-software device list).
///
/// `YSERVER_ALLOW_SOFTWARE_VULKAN=1` skips the check (deliberate
/// software-scanout setups, e.g. lavapipe under vng). Venus reports
/// `VIRTUAL_GPU`, not `CPU`, and passes.
pub fn ensure_hardware_vulkan_for_scanout() -> Result<(), String> {
    if std::env::var_os("YSERVER_ALLOW_SOFTWARE_VULKAN").is_some() {
        log::warn!(
            "YSERVER_ALLOW_SOFTWARE_VULKAN set — skipping the hardware-Vulkan \
             preflight; a software rasterizer driving real KMS scanout can \
             hard-hang the machine"
        );
        return Ok(());
    }

    let entry = match unsafe { ash::Entry::load() } {
        Ok(e) => e,
        Err(e) => {
            log::warn!("hw-Vulkan preflight: loader unavailable ({e}); deferring to full init");
            return Ok(());
        }
    };
    let create_info = vk::InstanceCreateInfo::default();
    let instance = match unsafe { entry.create_instance(&create_info, None) } {
        Ok(i) => i,
        Err(e) => {
            log::warn!(
                "hw-Vulkan preflight: instance creation failed ({e}); deferring to full init"
            );
            return Ok(());
        }
    };

    // Collect (name, type) for every device, then destroy the instance
    // before deciding — no resource outlives the probe.
    let devices: Vec<(String, vk::PhysicalDeviceType)> =
        match unsafe { instance.enumerate_physical_devices() } {
            Ok(pds) => pds
                .into_iter()
                .map(|pd| {
                    let props = unsafe { instance.get_physical_device_properties(pd) };
                    let name = props
                        .device_name_as_c_str()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|_| "<unnamed>".into());
                    (name, props.device_type)
                })
                .collect(),
            Err(e) => {
                unsafe { instance.destroy_instance(None) };
                log::warn!("hw-Vulkan preflight: enumeration failed ({e}); deferring to full init");
                return Ok(());
            }
        };
    unsafe { instance.destroy_instance(None) };

    if devices.is_empty() {
        log::warn!("hw-Vulkan preflight: no Vulkan devices found; deferring to full init");
        return Ok(());
    }
    if devices
        .iter()
        .any(|(_, ty)| *ty != vk::PhysicalDeviceType::CPU)
    {
        return Ok(());
    }

    let listing = devices
        .iter()
        .map(|(name, ty)| format!("{name} ({ty:?})"))
        .collect::<Vec<_>>()
        .join(", ");
    let msg = format!(
        "every available Vulkan device is a software rasterizer: [{listing}]. \
         Driving real KMS scanout off software Vulkan (llvmpipe/lavapipe) \
         hard-hangs the machine. Refusing to start before committing scanout. \
         Install a hardware Vulkan driver for the scanout GPU (radv / anv / nvk), \
         or check the GPU driver setup (e.g. proprietary driver removed but \
         nouveau not loaded leaves only llvmpipe). To override deliberately, \
         set YSERVER_ALLOW_SOFTWARE_VULKAN=1."
    );
    log::error!("hw-Vulkan preflight: {msg}");
    Err(msg)
}

fn validation_layer_present(entry: &ash::Entry, name: &CStr) -> bool {
    match unsafe { entry.enumerate_instance_layer_properties() } {
        Ok(layers) => layers
            .iter()
            .any(|l| l.layer_name_as_c_str().map(|s| s == name).unwrap_or(false)),
        Err(_) => false,
    }
}

fn create_debug_messenger(
    debug_utils_instance: &ash::ext::debug_utils::Instance,
) -> Result<Option<vk::DebugUtilsMessengerEXT>, VkInitError> {
    // Match the validation-layer enable rule from `VkContext::new`:
    // debug builds always install the messenger; release builds only
    // when `YSERVER_VK_VALIDATION` is set. Without the messenger the
    // validation layer has nowhere to report VUIDs and the layer is
    // effectively silent.
    if !cfg!(debug_assertions) && std::env::var_os("YSERVER_VK_VALIDATION").is_none() {
        return Ok(None);
    }
    let info = vk::DebugUtilsMessengerCreateInfoEXT::default()
        .message_severity(
            vk::DebugUtilsMessageSeverityFlagsEXT::WARNING
                | vk::DebugUtilsMessageSeverityFlagsEXT::ERROR,
        )
        .message_type(
            vk::DebugUtilsMessageTypeFlagsEXT::GENERAL
                | vk::DebugUtilsMessageTypeFlagsEXT::VALIDATION
                | vk::DebugUtilsMessageTypeFlagsEXT::PERFORMANCE,
        )
        .pfn_user_callback(Some(vk_debug_callback));
    Ok(Some(unsafe {
        debug_utils_instance.create_debug_utils_messenger(&info, None)?
    }))
}

impl Drop for VkContext {
    fn drop(&mut self) {
        unsafe {
            // Wait for all queue work; tearing down with in-flight CBs
            // is undefined behaviour. Disposable probe owners may establish
            // the same fact from their complete submitted-fence set and then
            // deliberately skip this unbounded defensive fallback.
            if self.requires_drop_device_idle() {
                let _ = self.device.device_wait_idle();
            }
            self.device.destroy_device(None);
            if let Some(m) = self.debug_messenger.take() {
                self.debug_utils_instance
                    .destroy_debug_utils_messenger(m, None);
            }
            self.instance.destroy_instance(None);
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum VkInitError {
    #[error("vulkan loader: {0}")]
    Loader(#[from] ash::LoadingError),
    #[error("vulkan: {0}")]
    Vk(vk::Result),
    #[error("no suitable physical device (need a graphics + transfer queue)")]
    NoSuitableDevice,
    #[error("no Vulkan physical device matches render endpoint {0}")]
    NoMatchingRenderDevice(String),
    #[error("multiple Vulkan physical devices match render endpoint {0}")]
    AmbiguousRenderDevice(String),
    #[error("no Vulkan physical device matches disposable selector {0}")]
    NoMatchingDeviceUuid(String),
    #[error("multiple Vulkan physical devices match disposable selector {0}")]
    AmbiguousDeviceUuid(String),
    #[error("Vulkan DRM identity conflicts with render endpoint {0}")]
    ConflictingRenderDevice(String),
    #[error("multiple Vulkan physical devices advertise DRM render node {0}")]
    DuplicateRenderNodeIdentity(String),
    #[error("physical device lacks required Vulkan feature `{0}` (no fallback implemented for it)")]
    MissingRequiredFeature(&'static str),
}

impl From<vk::Result> for VkInitError {
    fn from(r: vk::Result) -> Self {
        VkInitError::Vk(r)
    }
}

/// Outcome of matching yserver's wanted `VkPhysicalDeviceFeatures`
/// against what the picked device actually reports.
pub(crate) struct SelectedFeatures {
    /// The `enabledFeatures` to hand to `vkCreateDevice` — a subset of
    /// the wanted set, pruned to what the device supports.
    pub enabled: vk::PhysicalDeviceFeatures,
    /// Whether the RENDER `component_alpha` (subpixel/LCD text AA) path
    /// may use dual-source blending. `false` on devices without the
    /// optional `dualSrcBlend` feature (e.g. Broadcom V3D / v3dv on the
    /// Raspberry Pi 4/400); those masks fall back to grayscale AA.
    pub component_alpha: bool,
}

/// Choose the `VkPhysicalDeviceFeatures` to enable, given what the
/// device reports as supported.
///
/// `logicOp` and `dualSrcBlend` are *optional* Vulkan 1.0 features, not
/// "universally supported" — the Broadcom V3D tiler (RPi 4/400, v3dv)
/// ships without `dualSrcBlend` (and `scalarBlockLayout`), so requesting
/// them unconditionally made `vkCreateDevice` fail with
/// `ERROR_FEATURE_NOT_PRESENT`.
///
/// `dualSrcBlend` has a fallback (grayscale component-alpha), so it is
/// enabled only when present. `logicOp` (the X11 `GcFunction`
/// Xor/And/Or/Invert fill path) has no fallback yet, so its absence is a
/// hard error with a message that names the feature rather than the
/// opaque `FEATURE_NOT_PRESENT`.
fn select_device_features(
    supported: &vk::PhysicalDeviceFeatures,
) -> Result<SelectedFeatures, VkInitError> {
    if supported.logic_op == vk::FALSE {
        return Err(VkInitError::MissingRequiredFeature("logicOp"));
    }
    let component_alpha = supported.dual_src_blend == vk::TRUE;
    let enabled = vk::PhysicalDeviceFeatures::default()
        .logic_op(true)
        .dual_src_blend(component_alpha);
    Ok(SelectedFeatures {
        enabled,
        component_alpha,
    })
}

fn select_device_features_for_profile(
    supported: &vk::PhysicalDeviceFeatures,
    profile: DeviceProfile,
) -> Result<SelectedFeatures, VkInitError> {
    match profile {
        DeviceProfile::Compositor => select_device_features(supported),
        DeviceProfile::Transfer => Ok(SelectedFeatures {
            enabled: vk::PhysicalDeviceFeatures::default(),
            component_alpha: false,
        }),
    }
}

#[derive(Debug, Clone, Copy)]
struct PhysicalDeviceCandidate {
    physical_device: vk::PhysicalDevice,
    graphics_queue_family: Option<u32>,
    score: u32,
    selector: VulkanDeviceSelector,
    drm_identity: Option<VulkanDrmIdentity>,
}

struct PickedPhysicalDevice {
    physical_device: vk::PhysicalDevice,
    graphics_queue_family: u32,
    selected_device_selector: VulkanDeviceSelector,
    selected_drm_identity: Option<VulkanDrmIdentity>,
    drm_physical_devices: Vec<VulkanDrmPhysicalDevice>,
}

fn pick_physical_device(
    instance: &ash::Instance,
    selection: PhysicalDeviceSelection,
) -> Result<PickedPhysicalDevice, VkInitError> {
    let devices = unsafe { instance.enumerate_physical_devices() }?;

    let candidates: Vec<PhysicalDeviceCandidate> = devices
        .into_iter()
        .map(|physical_device| {
            let props = unsafe { instance.get_physical_device_properties(physical_device) };
            let score = match props.device_type {
                vk::PhysicalDeviceType::DISCRETE_GPU => 3,
                vk::PhysicalDeviceType::INTEGRATED_GPU => 2,
                vk::PhysicalDeviceType::VIRTUAL_GPU => 1,
                _ => 0,
            };
            Ok(PhysicalDeviceCandidate {
                physical_device,
                graphics_queue_family: pick_graphics_queue_family(instance, physical_device),
                score,
                selector: physical_device_selector(instance, physical_device),
                drm_identity: physical_device_drm_identity(instance, physical_device)?,
            })
        })
        .collect::<Result<_, VkInitError>>()?;

    validate_render_device_inventory(&candidates)?;
    let selected = match selection {
        PhysicalDeviceSelection::Automatic => select_physical_device_candidate(&candidates, None),
        PhysicalDeviceSelection::RenderEndpoint(requested) => {
            select_physical_device_candidate(&candidates, Some(requested))
        }
        PhysicalDeviceSelection::Exact(selector) => {
            select_exact_physical_device_candidate(&candidates, selector)
        }
    }?;
    let queue_family = selected
        .graphics_queue_family
        .ok_or(VkInitError::NoSuitableDevice)?;
    let drm_physical_devices = drm_physical_device_inventory(&candidates);
    Ok(PickedPhysicalDevice {
        physical_device: selected.physical_device,
        graphics_queue_family: queue_family,
        selected_device_selector: selected.selector,
        selected_drm_identity: selected.drm_identity,
        drm_physical_devices,
    })
}

fn drm_physical_device_inventory(
    candidates: &[PhysicalDeviceCandidate],
) -> Vec<VulkanDrmPhysicalDevice> {
    candidates
        .iter()
        .filter(|candidate| candidate.graphics_queue_family.is_some())
        .filter_map(|candidate| {
            let identity = candidate.drm_identity?;
            identity.render.map(|_| VulkanDrmPhysicalDevice {
                physical_device: candidate.physical_device,
                selector: candidate.selector,
                identity,
            })
        })
        .collect()
}

fn select_exact_physical_device_candidate(
    candidates: &[PhysicalDeviceCandidate],
    selector: VulkanDeviceSelector,
) -> Result<&PhysicalDeviceCandidate, VkInitError> {
    let matches = candidates
        .iter()
        .filter(|candidate| candidate.graphics_queue_family.is_some())
        .filter(|candidate| candidate.selector == selector)
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [selected] => {
            log::info!(
                "vulkan: selected disposable physical device by exact UUID pair {}",
                format_device_selector(selector),
            );
            Ok(selected)
        }
        [] => Err(VkInitError::NoMatchingDeviceUuid(format_device_selector(
            selector,
        ))),
        [_, _, ..] => Err(VkInitError::AmbiguousDeviceUuid(format_device_selector(
            selector,
        ))),
    }
}

fn select_physical_device_candidate(
    candidates: &[PhysicalDeviceCandidate],
    requested: Option<RequestedRenderDevice>,
) -> Result<&PhysicalDeviceCandidate, VkInitError> {
    let suitable = || {
        candidates
            .iter()
            .filter(|candidate| candidate.graphics_queue_family.is_some())
    };

    let Some(requested) = requested else {
        // A genuinely headless start has no DRM endpoint to match. Preserve
        // the existing scored choice even when the ICD advertises identities.
        return highest_scored_candidate(suitable()).ok_or(VkInitError::NoSuitableDevice);
    };

    if let Some(render) = requested.render {
        let render_matches: Vec<_> = suitable()
            .filter(|candidate| {
                candidate
                    .drm_identity
                    .is_some_and(|identity| identity.render == Some(render))
            })
            .collect();
        match render_matches.as_slice() {
            [selected] => {
                log::info!(
                    "vulkan: selected physical device by DRM render node {render} (advertised primary {:?}, display primary {})",
                    selected.drm_identity.and_then(|identity| identity.primary),
                    requested.display_primary,
                );
                return Ok(selected);
            }
            [_, _, ..] => {
                return Err(VkInitError::AmbiguousRenderDevice(
                    format_requested_render_device(requested),
                ));
            }
            [] => {}
        }

        // A physical device claiming the display primary but a different
        // render node is a contradictory same-device fallback. Never ignore
        // its render identity and guess by primary alone.
        if suitable().any(|candidate| {
            candidate.drm_identity.is_some_and(|identity| {
                identity.primary == Some(requested.display_primary)
                    && identity.render.is_some()
                    && identity.render != Some(render)
            })
        }) {
            return Err(VkInitError::ConflictingRenderDevice(
                format_requested_render_device(requested),
            ));
        }
    }

    // Extension presence is authoritative even when the driver reports
    // `has_render = false`: primary is metadata only, not a selection key.
    // Generic scoring is reserved for ICDs where no suitable candidate
    // exposes VK_EXT_physical_device_drm at all.
    if suitable().any(|candidate| candidate.drm_identity.is_some()) {
        return Err(VkInitError::NoMatchingRenderDevice(
            format_requested_render_device(requested),
        ));
    }

    log::warn!(
        "vulkan: no suitable physical device exposes VK_EXT_physical_device_drm; using an unverified generic fallback for {}",
        format_requested_render_device(requested),
    );
    highest_scored_candidate(suitable()).ok_or(VkInitError::NoSuitableDevice)
}

fn highest_scored_candidate<'a>(
    candidates: impl Iterator<Item = &'a PhysicalDeviceCandidate>,
) -> Option<&'a PhysicalDeviceCandidate> {
    candidates.reduce(|best, candidate| {
        if candidate.score > best.score {
            candidate
        } else {
            best
        }
    })
}

fn validate_render_device_inventory(
    candidates: &[PhysicalDeviceCandidate],
) -> Result<(), VkInitError> {
    let mut seen = std::collections::HashSet::new();
    for render in candidates
        .iter()
        .filter(|candidate| candidate.graphics_queue_family.is_some())
        .filter_map(|candidate| candidate.drm_identity?.render)
    {
        if !seen.insert(render) {
            return Err(VkInitError::DuplicateRenderNodeIdentity(render.to_string()));
        }
    }
    Ok(())
}

fn physical_device_selector(
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
) -> VulkanDeviceSelector {
    let mut id = vk::PhysicalDeviceIDProperties::default();
    let mut properties = vk::PhysicalDeviceProperties2::default().push_next(&mut id);
    unsafe {
        instance.get_physical_device_properties2(physical_device, &mut properties);
    }
    VulkanDeviceSelector {
        device_uuid: id.device_uuid,
        driver_uuid: id.driver_uuid,
    }
}

fn format_device_selector(selector: VulkanDeviceSelector) -> String {
    fn uuid(bytes: [u8; vk::UUID_SIZE]) -> String {
        bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    }
    format!(
        "deviceUUID={} driverUUID={}",
        uuid(selector.device_uuid),
        uuid(selector.driver_uuid),
    )
}

fn physical_device_drm_identity(
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
) -> Result<Option<VulkanDrmIdentity>, VkInitError> {
    let extensions = unsafe { instance.enumerate_device_extension_properties(physical_device) }?;
    let supported = extensions.iter().any(|property| {
        property
            .extension_name_as_c_str()
            .map(|name| name == ash::ext::physical_device_drm::NAME)
            .unwrap_or(false)
    });
    if !supported {
        return Ok(None);
    }

    let mut drm = vk::PhysicalDeviceDrmPropertiesEXT::default();
    let mut properties = vk::PhysicalDeviceProperties2::default().push_next(&mut drm);
    unsafe {
        instance.get_physical_device_properties2(physical_device, &mut properties);
    }
    Ok(Some(drm_identity_from_properties(&drm)))
}

fn drm_identity_from_properties(
    properties: &vk::PhysicalDeviceDrmPropertiesEXT<'_>,
) -> VulkanDrmIdentity {
    VulkanDrmIdentity {
        primary: drm_key(
            properties.has_primary,
            properties.primary_major,
            properties.primary_minor,
        ),
        render: drm_key(
            properties.has_render,
            properties.render_major,
            properties.render_minor,
        ),
    }
}

fn drm_key(has_node: vk::Bool32, major: i64, minor: i64) -> Option<DrmDeviceKey> {
    if has_node == vk::FALSE {
        return None;
    }
    Some(DrmDeviceKey {
        major: u32::try_from(major).ok()?,
        minor: u32::try_from(minor).ok()?,
    })
}

fn format_requested_render_device(requested: RequestedRenderDevice) -> String {
    requested.render.map_or_else(
        || {
            format!(
                "display primary {} (no render node)",
                requested.display_primary
            )
        },
        |render| {
            format!(
                "render {render}, display primary {}",
                requested.display_primary
            )
        },
    )
}

fn pick_graphics_queue_family(instance: &ash::Instance, pd: vk::PhysicalDevice) -> Option<u32> {
    let qfp = unsafe { instance.get_physical_device_queue_family_properties(pd) };
    pick_graphics_queue_family_from_properties(&qfp)
}

fn pick_graphics_queue_family_from_properties(
    properties: &[vk::QueueFamilyProperties],
) -> Option<u32> {
    let graphics_transfer = vk::QueueFlags::GRAPHICS | vk::QueueFlags::TRANSFER;
    let preferred = graphics_transfer | vk::QueueFlags::COMPUTE;
    let find = |required| {
        properties.iter().enumerate().find_map(|(index, family)| {
            (family.queue_count > 0 && family.queue_flags.contains(required))
                .then(|| u32::try_from(index).expect("queue family index fits in u32"))
        })
    };

    find(preferred).or_else(|| find(graphics_transfer))
}

fn effective_timestamp_period(device_period: f32, queue_timestamp_valid_bits: u32) -> f32 {
    if queue_timestamp_valid_bits == 0 {
        0.0
    } else {
        device_period
    }
}

unsafe extern "system" fn vk_debug_callback(
    severity: vk::DebugUtilsMessageSeverityFlagsEXT,
    _ty: vk::DebugUtilsMessageTypeFlagsEXT,
    callback_data: *const vk::DebugUtilsMessengerCallbackDataEXT<'_>,
    _user_data: *mut c_void,
) -> vk::Bool32 {
    // Validation can call this with a null callback_data on some
    // drivers; defend against that.
    if callback_data.is_null() {
        return vk::FALSE;
    }
    let data = unsafe { &*callback_data };
    let msg = if data.p_message.is_null() {
        "<no message>"
    } else {
        unsafe { CStr::from_ptr(data.p_message) }
            .to_str()
            .unwrap_or("<non-utf8 message>")
    };
    if severity.contains(vk::DebugUtilsMessageSeverityFlagsEXT::ERROR) {
        log::error!("vk: {msg}");
    } else if severity.contains(vk::DebugUtilsMessageSeverityFlagsEXT::WARNING) {
        log::warn!("vk: {msg}");
    }
    // INFO/VERBOSE intentionally suppressed — too noisy.
    vk::FALSE
}

#[cfg(test)]
mod tests {
    use ash::vk::Handle as _;

    use super::*;

    fn key(minor: u32) -> DrmDeviceKey {
        DrmDeviceKey { major: 226, minor }
    }

    fn identity(primary: Option<u32>, render: Option<u32>) -> VulkanDrmIdentity {
        VulkanDrmIdentity {
            primary: primary.map(key),
            render: render.map(key),
        }
    }

    fn selector(seed: u8) -> VulkanDeviceSelector {
        VulkanDeviceSelector {
            device_uuid: [seed; vk::UUID_SIZE],
            driver_uuid: [seed.wrapping_add(0x40); vk::UUID_SIZE],
        }
    }

    fn candidate(
        handle: u64,
        score: u32,
        drm_identity: Option<VulkanDrmIdentity>,
    ) -> PhysicalDeviceCandidate {
        PhysicalDeviceCandidate {
            physical_device: vk::PhysicalDevice::from_raw(handle),
            graphics_queue_family: Some(0),
            score,
            selector: selector(u8::try_from(handle).expect("test handle fits in u8")),
            drm_identity,
        }
    }

    fn requested(primary: u32, render: Option<u32>) -> RequestedRenderDevice {
        RequestedRenderDevice {
            display_primary: key(primary),
            render: render.map(key),
        }
    }

    #[test]
    fn selector_uuid_pair_round_trips_without_exposing_vulkan_handles() {
        let original = selector(0x21);
        let (device_uuid, driver_uuid) = original.uuid_pair();
        assert_eq!(
            VulkanDeviceSelector::from_uuid_pair(device_uuid, driver_uuid),
            original
        );
        assert_eq!(device_uuid, [0x21; vk::UUID_SIZE]);
        assert_eq!(driver_uuid, [0x61; vk::UUID_SIZE]);
    }

    #[test]
    fn graphics_queue_selection_prefers_compute_capability() {
        let families = [
            vk::QueueFamilyProperties {
                queue_flags: vk::QueueFlags::GRAPHICS | vk::QueueFlags::TRANSFER,
                queue_count: 1,
                ..Default::default()
            },
            vk::QueueFamilyProperties {
                queue_flags: vk::QueueFlags::GRAPHICS
                    | vk::QueueFlags::TRANSFER
                    | vk::QueueFlags::COMPUTE,
                queue_count: 1,
                ..Default::default()
            },
        ];

        assert_eq!(
            pick_graphics_queue_family_from_properties(&families),
            Some(1)
        );
    }

    #[test]
    fn graphics_queue_selection_retains_transfer_only_fallback() {
        let families = [
            vk::QueueFamilyProperties {
                queue_flags: vk::QueueFlags::COMPUTE | vk::QueueFlags::TRANSFER,
                queue_count: 1,
                ..Default::default()
            },
            vk::QueueFamilyProperties {
                queue_flags: vk::QueueFlags::GRAPHICS | vk::QueueFlags::TRANSFER,
                queue_count: 1,
                ..Default::default()
            },
        ];

        assert_eq!(
            pick_graphics_queue_family_from_properties(&families),
            Some(1)
        );
    }

    #[test]
    fn graphics_queue_selection_rejects_incomplete_or_empty_families() {
        let families = [
            vk::QueueFamilyProperties {
                queue_flags: vk::QueueFlags::GRAPHICS | vk::QueueFlags::TRANSFER,
                queue_count: 0,
                ..Default::default()
            },
            vk::QueueFamilyProperties {
                queue_flags: vk::QueueFlags::GRAPHICS,
                queue_count: 1,
                ..Default::default()
            },
        ];

        assert_eq!(pick_graphics_queue_family_from_properties(&families), None);
    }

    #[test]
    fn timestamp_telemetry_is_disabled_for_a_queue_without_timestamp_bits() {
        assert_eq!(effective_timestamp_period(1.25, 0), 0.0);
        assert_eq!(effective_timestamp_period(1.25, 36), 1.25);
    }

    #[test]
    fn drm_properties_preserve_primary_and_render_identity() {
        let properties = vk::PhysicalDeviceDrmPropertiesEXT {
            has_primary: vk::TRUE,
            has_render: vk::TRUE,
            primary_major: 226,
            primary_minor: 1,
            render_major: 226,
            render_minor: 129,
            ..Default::default()
        };
        assert_eq!(
            drm_identity_from_properties(&properties),
            identity(Some(1), Some(129))
        );
    }

    #[test]
    fn render_identity_is_authoritative_across_split_primary_nodes() {
        let candidates = [
            candidate(1, 3, Some(identity(Some(1), Some(129)))),
            candidate(2, 2, Some(identity(Some(0), Some(128)))),
        ];
        let selected = select_physical_device_candidate(&candidates, Some(requested(2, Some(128))))
            .expect("Asahi-style renderer primary mismatch is valid");
        assert_eq!(selected.physical_device.as_raw(), 2);
    }

    #[test]
    fn primary_only_identity_is_metadata_not_a_selection_fallback() {
        let candidates = [
            candidate(1, 3, Some(identity(Some(0), Some(128)))),
            candidate(2, 2, Some(identity(Some(2), None))),
        ];
        assert!(matches!(
            select_physical_device_candidate(&candidates, Some(requested(2, Some(129)))),
            Err(VkInitError::NoMatchingRenderDevice(_))
        ));
    }

    #[test]
    fn conflicting_same_primary_render_identity_is_rejected() {
        let candidates = [candidate(1, 3, Some(identity(Some(2), Some(130))))];
        assert!(matches!(
            select_physical_device_candidate(&candidates, Some(requested(2, Some(129)))),
            Err(VkInitError::ConflictingRenderDevice(_))
        ));
    }

    #[test]
    fn duplicate_render_identity_is_ambiguous() {
        let candidates = [
            candidate(1, 3, Some(identity(Some(0), Some(128)))),
            candidate(2, 2, Some(identity(Some(1), Some(128)))),
        ];
        assert!(matches!(
            select_physical_device_candidate(&candidates, Some(requested(2, Some(128)))),
            Err(VkInitError::AmbiguousRenderDevice(_))
        ));
    }

    #[test]
    fn generic_score_fallback_requires_extension_absent_everywhere() {
        let unidentified = [candidate(1, 1, None), candidate(2, 3, None)];
        let selected =
            select_physical_device_candidate(&unidentified, Some(requested(2, Some(129))))
                .expect("identity-free ICDs retain the portable fallback");
        assert_eq!(selected.physical_device.as_raw(), 2);

        let identified = [
            candidate(1, 3, Some(identity(Some(0), Some(128)))),
            candidate(2, 2, None),
        ];
        assert!(matches!(
            select_physical_device_candidate(&identified, Some(requested(2, Some(129)))),
            Err(VkInitError::NoMatchingRenderDevice(_))
        ));

        let extension_present_without_nodes = [
            candidate(1, 3, Some(identity(None, None))),
            candidate(2, 2, None),
        ];
        assert!(matches!(
            select_physical_device_candidate(
                &extension_present_without_nodes,
                Some(requested(2, Some(129)))
            ),
            Err(VkInitError::NoMatchingRenderDevice(_))
        ));
    }

    #[test]
    fn headless_selection_scores_even_when_drm_identities_are_advertised() {
        let candidates = [
            candidate(1, 1, Some(identity(Some(0), Some(128)))),
            candidate(2, 3, Some(identity(Some(1), Some(129)))),
        ];
        let selected = select_physical_device_candidate(&candidates, None)
            .expect("headless selection has no DRM endpoint to match");
        assert_eq!(selected.physical_device.as_raw(), 2);
    }

    #[test]
    fn disposable_selection_uses_exact_device_and_driver_uuid_pair() {
        let candidates = [
            candidate(1, 3, Some(identity(Some(0), Some(128)))),
            candidate(2, 1, None),
        ];

        let selected = select_exact_physical_device_candidate(&candidates, selector(2))
            .expect("UUID selection ignores generic score and DRM metadata");

        assert_eq!(selected.physical_device.as_raw(), 2);
    }

    #[test]
    fn disposable_selection_never_falls_back_to_generic_scoring() {
        let candidates = [candidate(1, 3, None), candidate(2, 2, None)];

        assert!(matches!(
            select_exact_physical_device_candidate(&candidates, selector(3)),
            Err(VkInitError::NoMatchingDeviceUuid(_))
        ));
    }

    #[test]
    fn disposable_selection_rejects_duplicate_uuid_pairs() {
        let mut candidates = [candidate(1, 3, None), candidate(2, 2, None)];
        candidates[1].selector = candidates[0].selector;

        assert!(matches!(
            select_exact_physical_device_candidate(&candidates, selector(1)),
            Err(VkInitError::AmbiguousDeviceUuid(_))
        ));
    }

    #[test]
    fn generic_score_ties_preserve_vulkan_enumeration_order() {
        let candidates = [candidate(1, 3, None), candidate(2, 3, None)];

        let headless = select_physical_device_candidate(&candidates, None)
            .expect("headless selection has a suitable device");
        assert_eq!(headless.physical_device.as_raw(), 1);

        let targeted = select_physical_device_candidate(&candidates, Some(requested(2, Some(129))))
            .expect("identity-free targeted selection retains generic scoring");
        assert_eq!(targeted.physical_device.as_raw(), 1);
    }

    #[test]
    fn duplicate_render_claims_in_full_inventory_are_rejected() {
        let candidates = [
            candidate(1, 3, Some(identity(Some(0), Some(128)))),
            candidate(2, 2, Some(identity(Some(1), Some(128)))),
        ];
        assert!(matches!(
            validate_render_device_inventory(&candidates),
            Err(VkInitError::DuplicateRenderNodeIdentity(key)) if key == "226:128"
        ));
    }

    #[test]
    fn render_inventory_preserves_every_exact_uuid_selector() {
        let candidates = [
            candidate(1, 3, Some(identity(Some(0), Some(128)))),
            candidate(2, 2, Some(identity(Some(1), Some(129)))),
            candidate(3, 1, None),
        ];

        let inventory = drm_physical_device_inventory(&candidates);

        assert_eq!(inventory.len(), 2);
        assert_eq!(inventory[0].selector, selector(1));
        assert_eq!(inventory[0].identity, identity(Some(0), Some(128)));
        assert_eq!(inventory[1].selector, selector(2));
        assert_eq!(inventory[1].identity, identity(Some(1), Some(129)));
    }

    #[test]
    fn transfer_profile_does_not_require_compositor_features() {
        let unsupported = vk::PhysicalDeviceFeatures::default()
            .logic_op(false)
            .dual_src_blend(false);

        let selected = select_device_features_for_profile(&unsupported, DeviceProfile::Transfer)
            .expect("sink transfer context needs no compositor-only features");

        assert_eq!(selected.enabled.logic_op, vk::FALSE);
        assert_eq!(selected.enabled.dual_src_blend, vk::FALSE);
        assert!(!selected.component_alpha);
    }

    #[test]
    fn disposable_drop_wait_is_removed_only_after_quiescence_proof() {
        assert!(DropWaitPolicy::live().requires_device_idle());

        let policy = DropWaitPolicy::disposable_probe();
        assert!(policy.requires_device_idle());

        policy.mark_probe_quiescent();
        // A full pool owns several Arcs to the same context and marks each one;
        // proving the same disposable device quiescent again is harmless.
        policy.mark_probe_quiescent();

        assert!(!policy.requires_device_idle());
    }

    #[test]
    #[should_panic(expected = "only disposable probe contexts")]
    fn live_context_cannot_bypass_drop_wait() {
        DropWaitPolicy::live().mark_probe_quiescent();
    }

    /// Real V3D 4.2 (v3dv) feature mask observed via `vulkaninfo` on an
    /// RPi 400: `logicOp` supported, `dualSrcBlend` NOT supported. This
    /// is the mask that made `vkCreateDevice` fail with
    /// `ERROR_FEATURE_NOT_PRESENT`. Ground truth, not inferred.
    #[test]
    fn v3d_drops_unsupported_dual_src_blend() {
        let supported = vk::PhysicalDeviceFeatures::default()
            .logic_op(true)
            .dual_src_blend(false);
        let sel = select_device_features(&supported).expect("logicOp present ⇒ Ok");
        assert_eq!(
            sel.enabled.dual_src_blend,
            vk::FALSE,
            "must NOT request dualSrcBlend on a device that lacks it (V3D ⇒ FEATURE_NOT_PRESENT)"
        );
        assert_eq!(
            sel.enabled.logic_op,
            vk::TRUE,
            "logicOp is supported here and is required"
        );
        assert!(
            !sel.component_alpha,
            "component_alpha capability must be off without dualSrcBlend"
        );
    }

    /// A conformant desktop GPU (RADV / NVIDIA / lavapipe) supports both
    /// optional features; nothing is pruned and component-alpha is on.
    #[test]
    fn full_device_enables_both() {
        let supported = vk::PhysicalDeviceFeatures::default()
            .logic_op(true)
            .dual_src_blend(true);
        let sel = select_device_features(&supported).expect("both present ⇒ Ok");
        assert_eq!(sel.enabled.dual_src_blend, vk::TRUE);
        assert_eq!(sel.enabled.logic_op, vk::TRUE);
        assert!(sel.component_alpha);
    }

    /// `logicOp` has no fallback path, so its absence is a hard error
    /// that names the feature (not the opaque FEATURE_NOT_PRESENT).
    #[test]
    fn missing_logic_op_is_fatal() {
        let supported = vk::PhysicalDeviceFeatures::default()
            .logic_op(false)
            .dual_src_blend(true);
        assert!(matches!(
            select_device_features(&supported),
            Err(VkInitError::MissingRequiredFeature("logicOp"))
        ));
    }
}
