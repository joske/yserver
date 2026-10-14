pub(crate) mod backend;
#[cfg(kms)]
pub mod console;
pub(crate) mod core;
pub use render::cpu_types;

/// The optional console (VT) guard of the KMS platform.
#[cfg(kms)]
pub(crate) type ConsoleGuardOpt = Option<console::ConsoleGuard>;
#[cfg(kms)]
pub(crate) mod cursor_plane;
pub(crate) mod font_index;
#[cfg(kms)]
pub(crate) mod hotplug;
#[cfg(kms)]
pub(crate) mod outputs;
pub mod render;
#[cfg(kms)]
pub(crate) mod render_node;
#[cfg(kms)]
pub(crate) mod scanout_route;
pub mod vk;
pub(super) mod xkb;
pub(super) mod xkb_desc;
#[cfg(kms)]
pub(crate) mod xshmfence;
