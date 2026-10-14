//! A platform whose outputs are window-system surfaces, each presented
//! through its own Vulkan swapchain. A shell (macOS: `crate::macos`) creates
//! the surfaces; until one does, the server it drives is headless.

#[cfg(not(kms))]
pub(crate) mod headless;
mod platform;
#[cfg(test)]
mod tests;

#[cfg(test)]
pub(crate) use platform::SurfacePlatform;
pub(crate) use platform::headless_backend;
