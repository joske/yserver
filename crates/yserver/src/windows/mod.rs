//! The Windows shell: one Win32 window whose Vulkan surface shows the X
//! screen, or one per top-level with `--rootless` ([`rootless`]), with the
//! core loop on its own thread. `--headless` runs without it.

mod app;
mod clipboard;
mod cursor;
mod frame_clock;
mod input;
mod resize;
mod rootless;
mod window;

use std::io;

use crate::launch::{LaunchOptions, ShellMode};

/// Run the server in a window, in a window per top-level (rootless), or
/// headless with `--headless`.
///
/// # Errors
///
/// See [`crate::surface::server::run_headless`], [`app::run`] and
/// [`rootless::run`].
pub(crate) fn run(opts: LaunchOptions, mode: ShellMode) -> io::Result<()> {
    if opts.headless {
        return crate::surface::server::run_headless(opts);
    }
    match mode {
        ShellMode::Rooted => app::run(opts),
        ShellMode::Rootless => rootless::run(opts),
    }
}
