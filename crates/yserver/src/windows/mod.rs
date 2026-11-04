//! The Windows shell: one Win32 window whose Vulkan surface shows the X
//! screen, or one per top-level with `--rootless` ([`rootless`]), with the
//! core loop on its own thread. `--headless` runs without it.

mod app;
mod clipboard;
mod cursor;
mod edid;
mod frame_clock;
mod icon;
mod input;
mod resize;
mod rootless;
mod scale;
mod settings_dialog;
mod tray;
mod vulkan;
mod window;
pub(crate) mod xkb;

use std::{
    io,
    sync::atomic::{AtomicBool, Ordering},
};

use crate::{
    launch::{LaunchOptions, ShellMode},
    settings::Settings,
};

/// The Settings dialog's Restart shut the server down: start it again.
static RESTART: AtomicBool = AtomicBool::new(false);

/// The settings in `%APPDATA%\yserver\settings.conf`.
pub(crate) fn stored_settings() -> Settings {
    Settings::load(&settings_dialog::store())
}

/// Run the server in a window, in a window per top-level (rootless), or
/// headless with `--headless`.
///
/// # Errors
///
/// See [`crate::surface::server::run_headless`], [`app::run`] and
/// [`rootless::run`].
pub(crate) fn run(opts: LaunchOptions, mode: ShellMode) -> io::Result<()> {
    let result = if opts.headless {
        crate::surface::server::run_headless(opts)
    } else {
        scale::declare_dpi_awareness();
        scale::set_option(opts.scale);
        match mode {
            ShellMode::Rooted => app::run(opts),
            ShellMode::Rootless => rootless::run(opts),
        }
    };
    if RESTART.load(Ordering::Acquire) {
        relaunch();
    }
    result.map_err(vulkan::explain)
}

/// Start this program again with this run's arguments: the server has shut
/// down and closed its listener, so the new one can take the display.
fn relaunch() {
    let spawned = std::env::current_exe().and_then(|exe| {
        std::process::Command::new(exe)
            .args(std::env::args_os().skip(1))
            .spawn()
    });
    match spawned {
        Ok(child) => log::info!("windows: restart: started process {}", child.id()),
        Err(e) => log::error!("windows: restart: cannot start yserver again: {e}"),
    }
}
