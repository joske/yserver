//! KMS-facing render-node helpers.
//!
//! Portable node discovery and selection live in `platform::drm`; this module
//! retains the DRI3-specific open semantics and explicit override.

use std::{
    env, io,
    os::fd::{AsFd, OwnedFd},
    path::{Path, PathBuf},
};

use crate::platform::drm::{self, RENDER_NODE_ENV};

/// Resolve and open the render node associated with `card_fd`.
///
/// The returned path is retained so DRI3 can open a fresh kernel struct file
/// for each client rather than `dup()`-ing a shared one.
pub fn open_for_card<F: AsFd>(card_fd: F) -> io::Result<(OwnedFd, PathBuf)> {
    if let Some(path) = explicit_render_node_path() {
        let fd = drm::open_node_path(&path)?;
        return Ok((fd, path));
    }

    let primary = drm::primary_device_key_from_fd(card_fd.as_fd())?;
    let render = drm::render_node_for_primary(primary)?.ok_or_else(|| {
        io::Error::other(format!(
            "no DRM render node found for card with rdev {primary}. \
             Override with {RENDER_NODE_ENV}=/dev/dri/renderDN if needed."
        ))
    })?;
    let fd = drm::open_node(&render)?;
    Ok((fd, render.path))
}

pub fn open_fresh(path: &Path) -> io::Result<OwnedFd> {
    drm::open_node_path(path)
}

fn explicit_render_node_path() -> Option<PathBuf> {
    let path = PathBuf::from(env::var_os(RENDER_NODE_ENV)?);
    (!path.as_os_str().is_empty()).then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_fresh_fails_for_missing_path() {
        let path = std::env::temp_dir().join("yserver-render-node-test-nonexistent");
        let _ = std::fs::remove_file(&path);
        assert!(open_fresh(&path).is_err());
    }
}
