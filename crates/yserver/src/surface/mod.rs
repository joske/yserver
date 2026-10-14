//! A platform whose outputs are window-system surfaces, each presented
//! through its own Vulkan swapchain. A shell (macOS: `crate::macos`) creates
//! the surfaces; without one the server is headless.

pub(crate) mod clipboard;
pub(crate) mod clock;
pub(crate) mod input;
pub(crate) mod link;
mod platform;
mod present;
#[cfg_attr(all(kms, not(test)), allow(dead_code))] // A window shell's.
pub(crate) mod resize;
#[cfg(not(kms))]
pub(crate) mod server;
mod slots;
pub(crate) mod swapchain;
#[cfg(test)]
mod tests;
pub(crate) mod ui;

pub(crate) use platform::{SurfacePlatform, headless_backend, surface_backend};
