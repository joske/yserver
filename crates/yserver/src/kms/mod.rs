pub(crate) mod backend;
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub mod console;
pub(crate) mod core;
pub mod cpu_types;

/// Cross-platform type alias for the optional console guard.
/// On Linux/FreeBSD this is the real VT guard; elsewhere it's a unit placeholder.
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub(crate) type ConsoleGuardOpt = Option<console::ConsoleGuard>;
#[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
pub(crate) type ConsoleGuardOpt = Option<()>;
pub(crate) mod cursor_plane;
pub(crate) mod hotplug;
pub mod render;
pub(crate) mod render_node;
pub(crate) mod scanout_route;
pub mod vk;
pub(super) mod xkb;
pub(crate) mod xshmfence;
