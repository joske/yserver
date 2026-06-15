//! Build version info surfaced by `--version`.
//!
//! [`VERSION`] is the workspace crate version (`Cargo.toml`); [`GIT_COMMIT`]
//! is the `HEAD` hash captured at build time by `build.rs` (`"unknown"`
//! outside a git checkout, e.g. a tarball build).

/// Crate version from `Cargo.toml` (`CARGO_PKG_VERSION`).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Git commit the binary was built from — 12-char `HEAD` hash, with a
/// `-dirty` suffix when the source tree had uncommitted tracked changes.
/// `"unknown"` when built outside a git checkout. Set by `build.rs`.
pub const GIT_COMMIT: &str = env!("YSERVER_GIT_COMMIT");

/// One-line version string, e.g. `yserver 1.1.1 (fd289a835226)`.
#[must_use]
pub fn line() -> String {
    format!("yserver {VERSION} ({GIT_COMMIT})")
}
