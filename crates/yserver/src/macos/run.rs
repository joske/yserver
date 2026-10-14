//! Launch on macOS.

use std::io;

use crate::launch::LaunchOptions;

/// Run the server: headless only (`--headless`) until the window shell lands.
///
/// # Errors
///
/// See [`crate::surface::headless::run`].
pub(crate) fn run(opts: LaunchOptions) -> io::Result<()> {
    crate::surface::headless::run(opts)
}
