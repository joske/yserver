//! The macOS shell: one AppKit window whose CAMetalLayer shows the X screen,
//! with the core loop on its own thread. `--headless` runs without it.

mod app;
mod cursor;
mod display_link;
mod input;
mod pasteboard;
mod resize;
mod run;
mod scale;
mod view;
mod window;

pub(crate) use run::run;
