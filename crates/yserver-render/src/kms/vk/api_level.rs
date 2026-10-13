//! Which Vulkan API level the logical device runs at, and the device dispatch
//! that hides the difference from call sites.
//!
//! The renderer is written against Vulkan 1.3's dynamic rendering and
//! synchronization2. Vulkan 1.2 devices that expose the same commands as
//! `VK_KHR_dynamic_rendering` / `VK_KHR_synchronization2` (Ivy Bridge and
//! Haswell on hasvk, for example) run the identical code: [`load_device`]
//! points ash's core-1.3 function table at the KHR entry points, so every
//! `device.cmd_begin_rendering(..)` / `device.cmd_pipeline_barrier2(..)` call
//! site stays as it is. Core-1.3 commands without a KHR route here are left
//! unloaded on 1.2 and panic by name if called.

use ash::vk;
use std::ffi::{CStr, c_void};

/// Highest instance API version yserver asks for.
pub(crate) const MAX_API_VERSION: u32 = vk::API_VERSION_1_3;

/// Environment cap on the requested instance API version (`1.2`), for testing
/// the Vulkan 1.2 path on a 1.3 driver such as lavapipe.
pub(crate) const API_VERSION_CAP_ENV: &str = "YSERVER_VK_MAX_API_VERSION";

/// How the selected device provides dynamic rendering and synchronization2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApiPath {
    /// Vulkan 1.3 or newer: both are core features.
    Core13,
    /// Vulkan 1.2: both come from KHR extensions. `dynamic_rendering` is
    /// false for the transfer profile, which never renders.
    Khr12 { dynamic_rendering: bool },
}

impl ApiPath {
    /// Device extensions this path must enable.
    pub(crate) fn required_extensions(self) -> &'static [&'static CStr] {
        match self {
            Self::Core13 => &[],
            Self::Khr12 {
                dynamic_rendering: true,
            } => &[
                ash::khr::synchronization2::NAME,
                ash::khr::dynamic_rendering::NAME,
            ],
            Self::Khr12 {
                dynamic_rendering: false,
            } => &[ash::khr::synchronization2::NAME],
        }
    }
}

/// `major.minor` of a packed API version, comparable as a tuple.
const fn major_minor(version: u32) -> (u32, u32) {
    (
        vk::api_version_major(version),
        vk::api_version_minor(version),
    )
}

/// Instance API version to request: the lower of [`MAX_API_VERSION`], what
/// the loader supports (`None` = a Vulkan 1.0 loader) and an optional cap.
pub(crate) fn instance_api_version(loader: Option<u32>, cap: Option<u32>) -> u32 {
    let loader = loader.unwrap_or(vk::API_VERSION_1_0);
    [Some(MAX_API_VERSION), Some(loader), cap]
        .into_iter()
        .flatten()
        .map(major_minor)
        .min()
        .map_or(MAX_API_VERSION, |(major, minor)| {
            vk::make_api_version(0, major, minor, 0)
        })
}

/// Parse an [`API_VERSION_CAP_ENV`] value of the form `1.2`.
pub(crate) fn parse_api_version_cap(value: &str) -> Option<u32> {
    let (major, minor) = value.trim().split_once('.')?;
    Some(vk::make_api_version(
        0,
        major.parse().ok()?,
        minor.parse().ok()?,
        0,
    ))
}

/// Format a packed API version as `major.minor.patch`.
pub(crate) fn format_api_version(version: u32) -> String {
    format!(
        "{}.{}.{}",
        vk::api_version_major(version),
        vk::api_version_minor(version),
        vk::api_version_patch(version)
    )
}

/// Why a device cannot run yserver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ApiShortfall {
    pub(crate) requirement: &'static str,
    pub(crate) missing: Vec<&'static str>,
}

/// Choose the API path for a device whose usable API version (the lower of
/// the instance and device versions) is `usable_version`.
pub(crate) fn select_api_path(
    usable_version: u32,
    renders: bool,
    has_extension: impl Fn(&CStr) -> bool,
) -> Result<ApiPath, ApiShortfall> {
    let requirement = if renders {
        "Vulkan 1.3, or Vulkan 1.2 with VK_KHR_dynamic_rendering and VK_KHR_synchronization2"
    } else {
        "Vulkan 1.3, or Vulkan 1.2 with VK_KHR_synchronization2"
    };
    let version = major_minor(usable_version);
    if version >= (1, 3) {
        return Ok(ApiPath::Core13);
    }
    let path = ApiPath::Khr12 {
        dynamic_rendering: renders,
    };
    let mut missing: Vec<&'static str> = Vec::new();
    if version < (1, 2) {
        missing.push("Vulkan 1.2");
    }
    missing.extend(
        path.required_extensions()
            .iter()
            .filter(|ext| !has_extension(ext))
            .map(|ext| ext.to_str().expect("extension names are ASCII")),
    );
    if missing.is_empty() {
        Ok(path)
    } else {
        Err(ApiShortfall {
            requirement,
            missing,
        })
    }
}

/// On the 1.2 path, the KHR entry point that serves core-1.3 command `name`.
/// `None` leaves the command unloaded.
pub(crate) fn khr_entry_point(name: &CStr, path: ApiPath) -> Option<&'static CStr> {
    let ApiPath::Khr12 { dynamic_rendering } = path else {
        return None;
    };
    let alias = match name.to_bytes() {
        b"vkCmdPipelineBarrier2" => c"vkCmdPipelineBarrier2KHR",
        b"vkQueueSubmit2" => c"vkQueueSubmit2KHR",
        b"vkCmdWriteTimestamp2" => c"vkCmdWriteTimestamp2KHR",
        b"vkCmdSetEvent2" => c"vkCmdSetEvent2KHR",
        b"vkCmdResetEvent2" => c"vkCmdResetEvent2KHR",
        b"vkCmdWaitEvents2" => c"vkCmdWaitEvents2KHR",
        b"vkCmdBeginRendering" if dynamic_rendering => c"vkCmdBeginRenderingKHR",
        b"vkCmdEndRendering" if dynamic_rendering => c"vkCmdEndRenderingKHR",
        _ => return None,
    };
    Some(alias)
}

/// Build the ash dispatch for `device`, created on `path`.
///
/// # Safety
/// `device` must be a live device created from `instance`.
pub(crate) unsafe fn load_device(
    instance: &ash::Instance,
    device: vk::Device,
    path: ApiPath,
) -> ash::Device {
    let get_device_proc_addr = instance.fp_v1_0().get_device_proc_addr;
    let load = |name: &CStr| -> *const c_void {
        unsafe { get_device_proc_addr(device, name.as_ptr()) }
            .map_or(std::ptr::null(), |f| f as *const c_void)
    };
    match path {
        ApiPath::Core13 => unsafe { ash::Device::load(instance.fp_v1_0(), device) },
        ApiPath::Khr12 { .. } => ash::Device::from_parts_1_3(
            device,
            ash::DeviceFnV1_0::load(load),
            ash::DeviceFnV1_1::load(load),
            ash::DeviceFnV1_2::load(load),
            ash::DeviceFnV1_3::load(|name| {
                khr_entry_point(name, path).map_or(std::ptr::null(), load)
            }),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const V1_1: u32 = vk::make_api_version(0, 1, 1, 107);
    const V1_2: u32 = vk::make_api_version(0, 1, 2, 175);
    const V1_3: u32 = vk::make_api_version(0, 1, 3, 250);
    const V1_4: u32 = vk::make_api_version(0, 1, 4, 354);

    fn exts(list: &'static [&'static CStr]) -> impl Fn(&CStr) -> bool {
        move |name| list.contains(&name)
    }

    const BOTH: &[&CStr] = &[
        ash::khr::dynamic_rendering::NAME,
        ash::khr::synchronization2::NAME,
    ];
    const SYNC2: &[&CStr] = &[ash::khr::synchronization2::NAME];
    const DYNREN: &[&CStr] = &[ash::khr::dynamic_rendering::NAME];
    const NONE: &[&CStr] = &[];

    #[test]
    fn vulkan_1_3_and_newer_take_the_core_path_without_extensions() {
        for version in [V1_3, V1_4] {
            for renders in [true, false] {
                assert_eq!(
                    select_api_path(version, renders, exts(NONE)),
                    Ok(ApiPath::Core13)
                );
            }
        }
    }

    #[test]
    fn vulkan_1_2_compositor_needs_both_extensions() {
        assert_eq!(
            select_api_path(V1_2, true, exts(BOTH)),
            Ok(ApiPath::Khr12 {
                dynamic_rendering: true
            })
        );
        let missing = |list| select_api_path(V1_2, true, exts(list)).unwrap_err().missing;
        assert_eq!(missing(SYNC2), ["VK_KHR_dynamic_rendering"]);
        assert_eq!(missing(DYNREN), ["VK_KHR_synchronization2"]);
        assert_eq!(
            missing(NONE),
            ["VK_KHR_synchronization2", "VK_KHR_dynamic_rendering"]
        );
    }

    #[test]
    fn vulkan_1_2_transfer_needs_only_synchronization2() {
        assert_eq!(
            select_api_path(V1_2, false, exts(SYNC2)),
            Ok(ApiPath::Khr12 {
                dynamic_rendering: false
            })
        );
        let err = select_api_path(V1_2, false, exts(DYNREN)).unwrap_err();
        assert_eq!(err.missing, ["VK_KHR_synchronization2"]);
        assert_eq!(
            err.requirement,
            "Vulkan 1.3, or Vulkan 1.2 with VK_KHR_synchronization2"
        );
    }

    #[test]
    fn vulkan_1_1_is_refused_even_with_the_extensions() {
        let err = select_api_path(V1_1, true, exts(BOTH)).unwrap_err();
        assert_eq!(err.missing, ["Vulkan 1.2"]);
    }

    #[test]
    fn instance_version_is_the_lowest_of_ours_the_loaders_and_the_cap() {
        assert_eq!(instance_api_version(Some(V1_4), None), vk::API_VERSION_1_3);
        assert_eq!(instance_api_version(Some(V1_2), None), vk::API_VERSION_1_2);
        assert_eq!(instance_api_version(None, None), vk::API_VERSION_1_0);
        assert_eq!(
            instance_api_version(Some(V1_4), Some(vk::API_VERSION_1_2)),
            vk::API_VERSION_1_2
        );
        assert_eq!(
            instance_api_version(Some(V1_4), Some(vk::make_api_version(0, 1, 4, 0))),
            vk::API_VERSION_1_3
        );
    }

    #[test]
    fn api_version_cap_parses_major_dot_minor() {
        assert_eq!(parse_api_version_cap("1.2"), Some(vk::API_VERSION_1_2));
        assert_eq!(parse_api_version_cap(" 1.3 "), Some(vk::API_VERSION_1_3));
        assert_eq!(parse_api_version_cap("1"), None);
        assert_eq!(parse_api_version_cap("one.two"), None);
    }

    #[test]
    fn khr_entry_points_cover_only_the_enabled_extensions() {
        let compositor = ApiPath::Khr12 {
            dynamic_rendering: true,
        };
        let transfer = ApiPath::Khr12 {
            dynamic_rendering: false,
        };
        assert_eq!(
            khr_entry_point(c"vkCmdBeginRendering", compositor),
            Some(c"vkCmdBeginRenderingKHR")
        );
        assert_eq!(khr_entry_point(c"vkCmdBeginRendering", transfer), None);
        for path in [compositor, transfer] {
            assert_eq!(
                khr_entry_point(c"vkQueueSubmit2", path),
                Some(c"vkQueueSubmit2KHR")
            );
            assert_eq!(
                khr_entry_point(c"vkCmdPipelineBarrier2", path),
                Some(c"vkCmdPipelineBarrier2KHR")
            );
            // VK_KHR_copy_commands2 is not enabled: copies use the 1.0 commands.
            assert_eq!(khr_entry_point(c"vkCmdCopyImage2", path), None);
        }
        assert_eq!(
            khr_entry_point(c"vkQueueSubmit2", ApiPath::Core13),
            None,
            "the core path loads core names directly"
        );
    }
}
