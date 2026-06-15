pub(crate) mod backend;
#[cfg(target_os = "linux")]
pub mod console;
pub(crate) mod core;
pub mod cpu_types;

/// Cross-platform type alias for the optional console guard.
/// On Linux this is the real VT guard; elsewhere it's a unit placeholder.
#[cfg(target_os = "linux")]
pub(crate) type ConsoleGuardOpt = Option<console::ConsoleGuard>;
#[cfg(not(target_os = "linux"))]
pub(crate) type ConsoleGuardOpt = Option<()>;
pub(crate) mod cursor_plane;
pub(crate) mod render_node;
pub mod v2;
pub mod vk;
pub(super) mod xkb;
pub(crate) mod xshmfence;
