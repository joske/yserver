//! KMS-facing render-node helpers.
//!
//! Portable node discovery and selection live in `platform::drm`; this module
//! retains the DRI3-specific open semantics and explicit override.

use std::{
    env, io,
    os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd},
    path::{Path, PathBuf},
};

use crate::platform::drm::{self, DrmDeviceKey, DrmNode, RENDER_NODE_ENV};

/// One opened DRM render node kept together with the stable identity and path
/// that were verified at open time.
pub(crate) struct OpenedRenderNode {
    fd: OwnedFd,
    node: DrmNode,
}

impl OpenedRenderNode {
    #[must_use]
    pub(crate) fn raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    #[must_use]
    pub(crate) fn path(&self) -> &Path {
        &self.node.path
    }

    #[must_use]
    pub(crate) fn key(&self) -> DrmDeviceKey {
        self.node.key
    }

    /// Open a fresh kernel struct file for one DRI3 client while retaining
    /// the renderer identity selected at startup. A disappearing/recreated
    /// path must not silently redirect the client to another DRM device.
    pub(crate) fn open_fresh(&self) -> io::Result<OwnedFd> {
        drm::open_node(&self.node)
    }

    /// Verify another long-lived wrapper opened from the retained path before
    /// it is used for renderer-owned syncobj ioctls.
    pub(crate) fn verify_fd(&self, fd: BorrowedFd<'_>) -> io::Result<()> {
        let opened = drm::primary_device_key_from_fd(fd)?;
        if opened != self.node.key {
            return Err(io::Error::other(format!(
                "DRM render node {} changed identity (selected {}, reopened {opened})",
                self.node.path.display(),
                self.node.key,
            )));
        }
        Ok(())
    }
}

impl AsFd for OpenedRenderNode {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

/// Resolve and open the render node associated with `card_fd`.
///
/// The returned path is retained so DRI3 can open a fresh kernel struct file
/// for each client rather than `dup()`-ing a shared one.
pub fn open_for_card<F: AsFd>(card_fd: F) -> io::Result<OpenedRenderNode> {
    if let Some(path) = explicit_render_node_path() {
        let node = drm::render_node_from_path(path)?;
        let fd = drm::open_node(&node)?;
        return Ok(OpenedRenderNode { fd, node });
    }

    let primary = drm::primary_device_key_from_fd(card_fd.as_fd())?;
    let render = drm::render_node_for_primary(primary)?.ok_or_else(|| {
        io::Error::other(format!(
            "no DRM render node found for card with rdev {primary}. \
             Override with {RENDER_NODE_ENV}=/dev/dri/renderDN if needed."
        ))
    })?;
    let fd = drm::open_node(&render)?;
    Ok(OpenedRenderNode { fd, node: render })
}

fn explicit_render_node_path() -> Option<PathBuf> {
    let path = PathBuf::from(env::var_os(RENDER_NODE_ENV)?);
    (!path.as_os_str().is_empty()).then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_fresh_rechecks_the_retained_node_identity() {
        let path = PathBuf::from("/dev/null");
        let fd = drm::open_node_path(&path).expect("open /dev/null");
        let key = drm::primary_device_key_from_fd(fd.as_fd()).expect("identify /dev/null");
        let node = OpenedRenderNode {
            fd,
            node: DrmNode {
                path,
                key,
                kind: crate::platform::drm::DrmNodeKind::Render,
            },
        };
        assert!(node.open_fresh().is_ok());

        let mismatched = OpenedRenderNode {
            fd: drm::open_node_path(Path::new("/dev/null")).expect("open /dev/null"),
            node: DrmNode {
                path: PathBuf::from("/dev/null"),
                key: DrmDeviceKey {
                    major: key.major,
                    minor: key.minor.wrapping_add(1),
                },
                kind: crate::platform::drm::DrmNodeKind::Render,
            },
        };
        assert!(mismatched.open_fresh().is_err());
    }
}
