//! The macOS shell. Until it creates a window, the server only runs
//! headless.

mod run;

pub(crate) use run::run;
