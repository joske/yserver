//! The macOS shell: one AppKit window whose CAMetalLayer shows the X screen,
//! with the core loop on its own thread. `--headless` runs without it.

mod app;
mod cursor;
mod defaults;
mod display;
mod display_link;
mod input;
mod pasteboard;
mod resize;
mod rootless;
mod run;
mod scale;
mod settings_window;
mod view;
mod window;

pub(crate) use defaults::stored_settings;
pub(crate) use run::run;
