//! A platform whose outputs are window-system surfaces, each presented
//! through its own Vulkan swapchain. A shell (macOS: `crate::macos`) creates
//! the surfaces; without one the server is headless.

pub(crate) mod clock;
pub(crate) mod input;
pub(crate) mod link;
mod platform;
mod present;
#[cfg(not(kms))]
pub(crate) mod server;
mod slots;
pub(crate) mod swapchain;
#[cfg(test)]
mod tests;
pub(crate) mod ui;

pub(crate) use platform::{SurfacePlatform, headless_backend, surface_backend};
