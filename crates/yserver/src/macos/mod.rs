//! The macOS shell: one AppKit window whose CAMetalLayer shows the X screen,
//! with the core loop on its own thread. `--headless` runs without it.

mod app;
mod display_link;
mod input;
mod run;
mod view;
mod window;

pub(crate) use run::run;
