//! Vulkan instance creation helpers (extension list, debug messenger plumbing).

use std::ffi::CStr;

/// Instance extensions we always request. Selection rationale: external
/// memory + external semaphore for KMS pageflip handoff (sub-phase
/// 4.1.2); debug utils for the validation/messenger pass.
pub fn required_instance_extensions() -> Vec<&'static CStr> {
    vec![
        ash::khr::external_memory_capabilities::NAME,
        ash::khr::external_semaphore_capabilities::NAME,
        ash::ext::debug_utils::NAME,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_instance_extensions_includes_external_memory_fd() {
        let ext = required_instance_extensions();
        assert!(ext.contains(&ash::khr::external_memory_capabilities::NAME));
        assert!(ext.contains(&ash::khr::external_semaphore_capabilities::NAME));
        assert!(ext.contains(&ash::ext::debug_utils::NAME));
    }
}
