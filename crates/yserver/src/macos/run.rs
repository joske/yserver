//! Launch on macOS.

use std::io;

use crate::launch::LaunchOptions;

/// Run the server in a window, or headless with `--headless`.
///
/// # Errors
///
/// See [`crate::surface::server::run_headless`] and [`super::app::run`].
pub(crate) fn run(opts: LaunchOptions) -> io::Result<()> {
    if opts.headless {
        return crate::surface::server::run_headless(opts);
    }
    super::app::run(opts)
}
