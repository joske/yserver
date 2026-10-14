//! Vulkan instance creation helpers (extension list, debug messenger plumbing).

use std::ffi::CStr;

use super::device::VkContextOptions;

/// Instance extensions a context with `options` asks for. External memory +
/// external semaphore capabilities serve the KMS pageflip handoff; debug
/// utils the validation messenger; the surface pair a presenting consumer.
/// Absent ones are dropped by [`filter_available`]; only the surface pair is
/// mandatory (see [`required_instance_extensions`]).
#[must_use]
pub fn wanted_instance_extensions(options: &VkContextOptions) -> Vec<&'static CStr> {
    let mut wanted = vec![
        ash::khr::external_memory_capabilities::NAME,
        ash::khr::external_semaphore_capabilities::NAME,
        ash::ext::debug_utils::NAME,
    ];
    wanted.extend(required_instance_extensions(options));
    if !options.wants_kms_interop {
        // Lets the loader list portability ICDs; harmless where none exist.
        wanted.push(ash::khr::portability_enumeration::NAME);
    }
    wanted
}

/// Instance extensions without which context creation fails.
#[must_use]
pub fn required_instance_extensions(options: &VkContextOptions) -> Vec<&'static CStr> {
    if !options.wants_present {
        return Vec::new();
    }
    let mut required = vec![ash::khr::surface::NAME];
    required.extend(options.surface_extension);
    required
}

/// `wanted` in order, minus every extension `is_available` rejects.
#[must_use]
pub fn filter_available(
    wanted: &[&'static CStr],
    is_available: impl Fn(&CStr) -> bool,
) -> Vec<&'static CStr> {
    wanted
        .iter()
        .copied()
        .filter(|name| is_available(name))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kms_options_want_the_pageflip_and_debug_extensions_only() {
        let ext = wanted_instance_extensions(&VkContextOptions::KMS);
        assert_eq!(
            ext,
            [
                ash::khr::external_memory_capabilities::NAME,
                ash::khr::external_semaphore_capabilities::NAME,
                ash::ext::debug_utils::NAME,
            ]
        );
        assert!(required_instance_extensions(&VkContextOptions::KMS).is_empty());
    }

    #[test]
    fn presenting_options_require_the_surface_pair() {
        let options = VkContextOptions {
            wants_present: true,
            surface_extension: Some(ash::ext::headless_surface::NAME),
            wants_kms_interop: false,
        };
        assert_eq!(
            required_instance_extensions(&options),
            [ash::khr::surface::NAME, ash::ext::headless_surface::NAME]
        );
        let wanted = wanted_instance_extensions(&options);
        assert!(wanted.contains(&ash::khr::surface::NAME));
        assert!(wanted.contains(&ash::ext::headless_surface::NAME));
        assert!(wanted.contains(&ash::khr::portability_enumeration::NAME));
    }

    #[test]
    fn filter_drops_an_absent_extension_and_keeps_order() {
        let wanted = wanted_instance_extensions(&VkContextOptions::KMS);
        let filtered = filter_available(&wanted, |name| name != ash::ext::debug_utils::NAME);
        assert_eq!(
            filtered,
            [
                ash::khr::external_memory_capabilities::NAME,
                ash::khr::external_semaphore_capabilities::NAME,
            ]
        );
    }
}
