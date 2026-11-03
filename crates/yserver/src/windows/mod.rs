//! The Windows shell: one Win32 window whose Vulkan surface shows the X
//! screen, with the core loop on its own thread. `--headless` runs without
//! it.

mod app;
mod cursor;
mod frame_clock;
mod input;
mod resize;
mod window;

use std::io;

use crate::launch::LaunchOptions;

/// Run the server in a window, or headless with `--headless`.
///
/// # Errors
///
/// See [`crate::surface::server::run_headless`] and [`app::run`].
pub(crate) fn run(opts: LaunchOptions) -> io::Result<()> {
    if opts.headless {
        return crate::surface::server::run_headless(opts);
    }
    app::run(opts)
}
