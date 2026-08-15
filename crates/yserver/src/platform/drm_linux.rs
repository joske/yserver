//! Linux-specific DRM node relationship discovery.
//!
//! DRM/KMS ioctls and `/dev/dri` node enumeration are shared with FreeBSD.
//! Only Linux sysfs topology belongs in this module.

use std::{
    fs,
    io::{self, ErrorKind},
    path::PathBuf,
};

use super::drm::DrmDeviceKey;

/// Fast path for finding a render-node sibling through the primary node's
/// Linux device directory.
pub(super) fn render_node_path_for_primary(primary: DrmDeviceKey) -> io::Result<Option<PathBuf>> {
    let dir = PathBuf::from(format!("/sys/dev/char/{primary}/device/drm"));
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        if name.to_string_lossy().starts_with("renderD") {
            return Ok(Some(PathBuf::from("/dev/dri").join(name)));
        }
    }
    Ok(None)
}

pub(super) fn device_parent_for(key: DrmDeviceKey) -> io::Result<PathBuf> {
    fs::canonicalize(format!("/sys/dev/char/{key}/device"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_node_path_returns_none_for_absurd_device() {
        let result = render_node_path_for_primary(DrmDeviceKey {
            major: 9999,
            minor: 9999,
        });
        assert!(matches!(result, Ok(None)));
    }
}
