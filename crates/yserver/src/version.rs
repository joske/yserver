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

/// Built feature set, alphabetical and comma-separated with no spaces, e.g.
/// `tcp-transport,xdmcp` or empty when neither is compiled in.
///
/// Ordering is by construction — each feature occupies a fixed array slot in
/// alphabetical position, present (`Some`) or absent (`None`) depending on
/// the `#[cfg]` — not by hoping the source order happens to sort correctly.
#[must_use]
fn feature_list() -> String {
    #[cfg(feature = "tcp-transport")]
    let tcp_transport = Some("tcp-transport");
    #[cfg(not(feature = "tcp-transport"))]
    let tcp_transport: Option<&'static str> = None;

    #[cfg(feature = "xdmcp")]
    let xdmcp = Some("xdmcp");
    #[cfg(not(feature = "xdmcp"))]
    let xdmcp: Option<&'static str> = None;

    [tcp_transport, xdmcp]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(",")
}

/// One-line version string, e.g.
/// `yserver 1.1.1 (fd289a835226) features=[tcp-transport,xdmcp]`.
///
/// The `features=[...]` suffix is a stable, machine-readable field — not
/// prose — read by packagers, recipes and bug reports. It is alphabetical,
/// comma-separated, has no spaces, and is always present, including when
/// empty (`features=[]`).
#[must_use]
pub fn line() -> String {
    format!(
        "yserver {VERSION} ({GIT_COMMIT}) features=[{}]",
        feature_list()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // Pins the exact, literal suffix per build configuration. Each assertion
    // only holds under the matching `--features`/`--no-default-features`
    // invocation; run all three to see all three strings.
    #[cfg(all(feature = "tcp-transport", feature = "xdmcp"))]
    #[test]
    fn feature_suffix_default_build() {
        assert_eq!(feature_list(), "tcp-transport,xdmcp");
    }

    #[cfg(all(feature = "tcp-transport", not(feature = "xdmcp")))]
    #[test]
    fn feature_suffix_tcp_transport_only() {
        assert_eq!(feature_list(), "tcp-transport");
    }

    #[cfg(not(feature = "tcp-transport"))]
    #[test]
    fn feature_suffix_no_default_features() {
        assert_eq!(feature_list(), "");
    }
}
