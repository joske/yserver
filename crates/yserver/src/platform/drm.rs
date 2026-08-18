//! Portable DRM node discovery and selection policy.
//!
//! Linux and FreeBSD both expose DRM device nodes under `/dev/dri` and the
//! same KMS control API. Shared node identity, enumeration, and selection live
//! here; optional OS topology mechanisms such as Linux sysfs stay in their
//! platform module. KMS connector/CRTC/plane discovery remains in
//! `crate::drm::modeset`.

use std::{
    fmt, fs,
    io::{self, ErrorKind},
    os::{
        fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd},
        unix::fs::MetadataExt,
    },
    path::{Path, PathBuf},
};

use ::drm::control::{Device as _, connector};

pub(crate) use crate::drm::modeset::{
    ConnectorProbe, ConnectorSnapshotProbe, Mode, ModeIdentity, Output,
    discover_output_for_connector, discover_outputs, probe_connector_snapshots, probe_connectors,
};

const DRM_DIR: &str = "/dev/dri";
pub(crate) const RENDER_NODE_ENV: &str = "YSERVER_DRI_RENDER_NODE";

/// Stable kernel identity of one DRM device node.
///
/// This is the node's `st_rdev` major/minor pair, matching the identity
/// exported by `VK_EXT_physical_device_drm`; unlike `cardN`, it is suitable as
/// a join key for later multi-device PRIME topology.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct DrmDeviceKey {
    pub(crate) major: u32,
    pub(crate) minor: u32,
}

impl fmt::Display for DrmDeviceKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.major, self.minor)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DrmNodeKind {
    Primary,
    Render,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DrmNode {
    pub(crate) path: PathBuf,
    pub(crate) key: DrmDeviceKey,
    pub(crate) kind: DrmNodeKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct KmsCardCandidate {
    node: DrmNode,
    has_connected_connector: bool,
}

/// Resolve the KMS cards yserver should open at startup.
///
/// `YSERVER_DRM_DEVICES` accepts a comma-separated ordered device list. The
/// existing singular `YSERVER_DRM_DEVICE` remains a one-device-only override
/// and is ignored when the plural override is present. The first device that
/// opens successfully becomes the startup KMS device; remaining devices stay
/// available as secondary KMS/provider inventory.
///
/// Node enumeration is portable between yserver's Linux and FreeBSD targets;
/// only optional relationship metadata is delegated to an OS module. The
/// selected primary scanout candidate is first and every remaining KMS-capable
/// primary node keeps its platform enumeration order. No candidates is a valid
/// headless result.
pub(crate) fn resolve_default_kms_devices() -> io::Result<Vec<PathBuf>> {
    let ordered_override = ordered_kms_override_value(std::env::var("YSERVER_DRM_DEVICES"))?;
    let singular_override = std::env::var("YSERVER_DRM_DEVICE").ok();
    if let Some(devices) =
        resolve_kms_device_override(ordered_override.as_deref(), singular_override.as_deref())?
    {
        let source = if ordered_override.is_some() {
            "YSERVER_DRM_DEVICES"
        } else {
            "YSERVER_DRM_DEVICE"
        };
        log::info!(
            "yserver: using {source} DRM device override: {}",
            devices
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
        return Ok(devices);
    }

    let (candidates, reasons) = discover_kms_candidates()?;
    let ordered = order_kms_card_candidates(candidates);
    if let Some(chosen) = ordered.first() {
        log::info!(
            "yserver: selected primary DRM device {} (connected_display={})",
            chosen.node.path.display(),
            chosen.has_connected_connector
        );
        for secondary in ordered.iter().skip(1) {
            log::info!(
                "yserver: discovered secondary DRM device {} (connected_display={})",
                secondary.node.path.display(),
                secondary.has_connected_connector
            );
        }
        return Ok(ordered
            .into_iter()
            .map(|candidate| candidate.node.path)
            .collect());
    }

    if reasons.is_empty() {
        log::info!("yserver: no KMS-capable DRM devices discovered; starting headless");
    } else {
        log::warn!(
            "yserver: no KMS-capable DRM devices could be opened; starting headless. Tried:\n  {}",
            reasons.join("\n  ")
        );
    }
    Ok(Vec::new())
}

fn ordered_kms_override_value(
    value: Result<String, std::env::VarError>,
) -> io::Result<Option<String>> {
    match value {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "YSERVER_DRM_DEVICES must be valid UTF-8",
        )),
    }
}

fn resolve_kms_device_override(
    ordered: Option<&str>,
    singular: Option<&str>,
) -> io::Result<Option<Vec<PathBuf>>> {
    if let Some(spec) = ordered {
        return parse_ordered_kms_devices(spec).map(Some);
    }
    Ok(singular.map(|path| vec![PathBuf::from(path)]))
}

fn parse_ordered_kms_devices(spec: &str) -> io::Result<Vec<PathBuf>> {
    if spec.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "YSERVER_DRM_DEVICES must contain at least one device path",
        ));
    }

    let mut devices = Vec::new();
    for (index, component) in spec.split(',').enumerate() {
        let component = component.trim();
        if component.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("YSERVER_DRM_DEVICES contains an empty path at position {index}"),
            ));
        }
        let path = PathBuf::from(component);
        if devices.contains(&path) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "YSERVER_DRM_DEVICES contains duplicate path {}",
                    path.display()
                ),
            ));
        }
        devices.push(path);
    }
    Ok(devices)
}

fn discover_kms_candidates() -> io::Result<(Vec<KmsCardCandidate>, Vec<String>)> {
    let nodes = enumerate_nodes("card", DrmNodeKind::Primary)?;
    let mut candidates = Vec::new();
    let mut reasons = Vec::new();

    for node in nodes {
        let path = node.path.to_string_lossy().into_owned();
        let device = match crate::drm::Device::open(&path) {
            Ok(device) => device,
            Err(err) => {
                log::info!("yserver: skipping {path}: open failed: {err}");
                reasons.push(format!("{path}: open failed: {err}"));
                continue;
            }
        };
        let resources = match device.resource_handles() {
            Ok(resources) => resources,
            Err(err) => {
                log::info!("yserver: skipping {path}: not KMS-capable: {err}");
                reasons.push(format!("{path}: not KMS-capable: {err}"));
                continue;
            }
        };
        let has_connected_connector = resources.connectors().iter().any(|&handle| {
            device
                .get_connector(handle, true)
                .is_ok_and(|info| info.state() == connector::State::Connected)
        });
        log::info!(
            "yserver: candidate {path}: KMS-capable, connected_display={has_connected_connector}"
        );
        candidates.push(KmsCardCandidate {
            node,
            has_connected_connector,
        });
    }

    Ok((candidates, reasons))
}

#[cfg(test)]
fn pick_kms_card_candidate(candidates: &[KmsCardCandidate]) -> Option<&KmsCardCandidate> {
    primary_kms_card_candidate_index(candidates).map(|idx| &candidates[idx])
}

fn primary_kms_card_candidate_index(candidates: &[KmsCardCandidate]) -> Option<usize> {
    candidates
        .iter()
        .position(|candidate| candidate.has_connected_connector)
        .or((!candidates.is_empty()).then_some(0))
}

/// Move the selected primary candidate to the front while preserving the
/// relative order of every secondary device.
fn order_kms_card_candidates(mut candidates: Vec<KmsCardCandidate>) -> Vec<KmsCardCandidate> {
    let Some(primary_idx) = primary_kms_card_candidate_index(&candidates) else {
        return candidates;
    };
    let primary = candidates.remove(primary_idx);
    let mut ordered = Vec::with_capacity(candidates.len() + 1);
    ordered.push(primary);
    ordered.extend(candidates);
    ordered
}

pub(crate) fn primary_device_key_from_fd(fd: BorrowedFd<'_>) -> io::Result<DrmDeviceKey> {
    device_key_from_fd(fd)
}

pub(crate) fn render_node_for_primary(primary: DrmDeviceKey) -> io::Result<Option<DrmNode>> {
    #[cfg(target_os = "linux")]
    if let Some(path) = super::drm_linux::render_node_path_for_primary(primary)? {
        return node_for_path(path, DrmNodeKind::Render).map(Some);
    }

    let candidates = enumerate_nodes("renderD", DrmNodeKind::Render)?;
    let primary_parent = device_parent_for(primary);
    let resolved: Vec<(DrmNode, Option<PathBuf>)> = candidates
        .into_iter()
        .map(|node| {
            let parent = device_parent_for(node.key);
            (node, parent)
        })
        .collect();
    select_render_node(&resolved, primary_parent.as_deref(), primary)
}

fn select_render_node(
    candidates: &[(DrmNode, Option<PathBuf>)],
    primary_parent: Option<&Path>,
    primary: DrmDeviceKey,
) -> io::Result<Option<DrmNode>> {
    if candidates.is_empty() {
        return Ok(None);
    }

    if let Some(primary_parent) = primary_parent
        && let Some((node, _)) = candidates
            .iter()
            .find(|(_, parent)| parent.as_deref() == Some(primary_parent))
    {
        return Ok(Some(node.clone()));
    }

    match candidates {
        [(only, _)] => Ok(Some(only.clone())),
        _ => {
            let why = if primary_parent.is_some() {
                "none is a sibling of"
            } else {
                "no platform parent data is available to match them to"
            };
            Err(io::Error::other(format!(
                "multiple DRM render nodes found but {why} card rdev {primary}: {}. \
                 Set {RENDER_NODE_ENV}=/dev/dri/renderDN.",
                display_node_paths(candidates)
            )))
        }
    }
}

fn enumerate_nodes(prefix: &str, kind: DrmNodeKind) -> io::Result<Vec<DrmNode>> {
    let entries = match fs::read_dir(DRM_DIR) {
        Ok(entries) => entries,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(io::Error::new(
                err.kind(),
                format!("read_dir({DRM_DIR}): {err}"),
            ));
        }
    };

    let mut nodes = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                log::warn!("yserver: failed to read a {DRM_DIR} entry: {err}");
                continue;
            }
        };
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with(prefix) {
            continue;
        }
        match node_for_path(entry.path(), kind) {
            Ok(node) => nodes.push(node),
            Err(err) if err.kind() == ErrorKind::NotFound => log::debug!(
                "yserver: skipping disappearing DRM node {}: {err}",
                entry.path().display()
            ),
            Err(err) => log::warn!(
                "yserver: cannot identify DRM node {}: {err}",
                entry.path().display()
            ),
        }
    }
    nodes.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(nodes)
}

fn node_for_path(path: PathBuf, kind: DrmNodeKind) -> io::Result<DrmNode> {
    let metadata = fs::metadata(&path)?;
    Ok(DrmNode {
        path,
        key: device_key_from_rdev(metadata.rdev()),
        kind,
    })
}

/// Identify one explicit render-node path by stable `st_rdev` identity.
pub(crate) fn render_node_from_path(path: PathBuf) -> io::Result<DrmNode> {
    node_for_path(path, DrmNodeKind::Render)
}

pub(crate) fn open_node_path(path: &Path) -> io::Result<OwnedFd> {
    let file = fs::OpenOptions::new().read(true).write(true).open(path)?;
    Ok(file.into())
}

/// Open an enumerated node and verify that the path still names the same
/// device. DRM nodes can disappear and be recreated between enumeration and
/// open; retaining a stale key beside the replacement fd would corrupt later
/// device-qualified routing.
pub(crate) fn open_node(node: &DrmNode) -> io::Result<OwnedFd> {
    let fd = open_node_path(&node.path)?;
    let opened_key = device_key_from_fd(fd.as_fd())?;
    if opened_key != node.key {
        return Err(io::Error::other(format!(
            "DRM node {} changed identity during open (enumerated {}, opened {opened_key})",
            node.path.display(),
            node.key
        )));
    }
    Ok(fd)
}

fn device_key_from_fd(fd: BorrowedFd<'_>) -> io::Result<DrmDeviceKey> {
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::fstat(fd.as_raw_fd(), &mut stat) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(device_key_from_rdev(stat.st_rdev))
}

fn device_key_from_rdev(rdev: libc::dev_t) -> DrmDeviceKey {
    DrmDeviceKey {
        major: libc_major(rdev),
        minor: libc_minor(rdev),
    }
}

#[allow(clippy::cast_possible_truncation)]
fn libc_major(rdev: libc::dev_t) -> u32 {
    libc::major(rdev) as u32
}

#[allow(clippy::cast_possible_truncation)]
fn libc_minor(rdev: libc::dev_t) -> u32 {
    libc::minor(rdev) as u32
}

#[cfg(target_os = "linux")]
fn device_parent_for(key: DrmDeviceKey) -> Option<PathBuf> {
    super::drm_linux::device_parent_for(key).ok()
}

#[cfg(not(target_os = "linux"))]
fn device_parent_for(_key: DrmDeviceKey) -> Option<PathBuf> {
    None
}

fn display_node_paths(candidates: &[(DrmNode, Option<PathBuf>)]) -> String {
    candidates
        .iter()
        .map(|(node, _)| node.path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsFd;

    use super::*;

    fn node(path: &str, minor: u32) -> DrmNode {
        DrmNode {
            path: PathBuf::from(path),
            key: DrmDeviceKey { major: 226, minor },
            kind: DrmNodeKind::Render,
        }
    }

    fn candidate(path: &str, connected: bool) -> KmsCardCandidate {
        KmsCardCandidate {
            node: DrmNode {
                path: PathBuf::from(path),
                key: DrmDeviceKey {
                    major: 226,
                    minor: 0,
                },
                kind: DrmNodeKind::Primary,
            },
            has_connected_connector: connected,
        }
    }

    #[test]
    fn ordered_kms_override_preserves_order_and_colons_in_stable_paths() {
        assert_eq!(
            parse_ordered_kms_devices(
                " /dev/dri/by-path/pci-0000:01:00.0-card,\
                 /dev/dri/by-path/pci-0000:00:02.0-card ",
            )
            .unwrap(),
            vec![
                PathBuf::from("/dev/dri/by-path/pci-0000:01:00.0-card"),
                PathBuf::from("/dev/dri/by-path/pci-0000:00:02.0-card"),
            ]
        );
    }

    #[test]
    fn ordered_kms_override_rejects_empty_and_duplicate_paths() {
        for invalid in [
            "",
            "   ",
            ",/dev/dri/card0",
            "/dev/dri/card0,",
            "/dev/dri/card1,,/dev/dri/card0",
            "/dev/dri/card1,/dev/dri/card1",
            "/dev/dri/card1, /dev/dri/card1 ",
        ] {
            let error = parse_ordered_kms_devices(invalid)
                .expect_err("empty or duplicate ordered override must fail");
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{invalid:?}");
        }
    }

    #[test]
    fn plural_kms_override_wins_and_fails_closed() {
        assert_eq!(
            resolve_kms_device_override(
                Some("/dev/dri/card1,/dev/dri/card0"),
                Some("/dev/dri/card9"),
            )
            .unwrap(),
            Some(vec![
                PathBuf::from("/dev/dri/card1"),
                PathBuf::from("/dev/dri/card0"),
            ])
        );
        assert_eq!(
            resolve_kms_device_override(None, Some("/dev/dri/card9")).unwrap(),
            Some(vec![PathBuf::from("/dev/dri/card9")])
        );
        assert_eq!(
            resolve_kms_device_override(Some("/dev/dri/card1"), Some("/dev/dri/card9")).unwrap(),
            Some(vec![PathBuf::from("/dev/dri/card1")]),
            "a single-entry plural override remains plural and wins precedence"
        );
        assert_eq!(resolve_kms_device_override(None, None).unwrap(), None);

        let error = resolve_kms_device_override(Some(""), Some("/dev/dri/card9"))
            .expect_err("an invalid plural override must not fall back to singular");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);

        assert_eq!(
            ordered_kms_override_value(Err(std::env::VarError::NotPresent)).unwrap(),
            None
        );
        let error = ordered_kms_override_value(Err(std::env::VarError::NotUnicode(
            std::ffi::OsString::from("non-Unicode sentinel"),
        )))
        .expect_err("a present non-Unicode plural override must not look absent");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn pick_kms_card_returns_none_for_empty_input() {
        assert!(pick_kms_card_candidate(&[]).is_none());
    }

    #[test]
    fn pick_kms_card_prefers_a_connected_later_device() {
        let candidates = [
            candidate("/dev/dri/card1", false),
            candidate("/dev/dri/card2", true),
        ];
        assert_eq!(
            pick_kms_card_candidate(&candidates).unwrap().node.path,
            PathBuf::from("/dev/dri/card2")
        );
    }

    #[test]
    fn pick_kms_card_keeps_order_among_connected_devices() {
        let candidates = [
            candidate("/dev/dri/card0", true),
            candidate("/dev/dri/card1", true),
        ];
        assert_eq!(
            pick_kms_card_candidate(&candidates).unwrap().node.path,
            PathBuf::from("/dev/dri/card0")
        );
    }

    #[test]
    fn pick_kms_card_falls_back_to_first_headless_device() {
        let candidates = [
            candidate("/dev/dri/card0", false),
            candidate("/dev/dri/card1", false),
        ];
        assert_eq!(
            pick_kms_card_candidate(&candidates).unwrap().node.path,
            PathBuf::from("/dev/dri/card0")
        );
    }

    #[test]
    fn pick_kms_card_accepts_one_disconnected_device() {
        let candidates = [candidate("/dev/dri/card0", false)];
        assert_eq!(
            pick_kms_card_candidate(&candidates).unwrap().node.path,
            PathBuf::from("/dev/dri/card0")
        );
    }

    #[test]
    fn ordering_moves_primary_connected_candidate_first() {
        let ordered = order_kms_card_candidates(vec![
            candidate("/dev/dri/card0", false),
            candidate("/dev/dri/card1", true),
            candidate("/dev/dri/card2", false),
        ]);
        let paths: Vec<PathBuf> = ordered
            .into_iter()
            .map(|candidate| candidate.node.path)
            .collect();
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/dev/dri/card1"),
                PathBuf::from("/dev/dri/card0"),
                PathBuf::from("/dev/dri/card2"),
            ]
        );
    }

    #[test]
    fn ordering_keeps_secondary_platform_order() {
        let ordered = order_kms_card_candidates(vec![
            candidate("/dev/dri/card0", true),
            candidate("/dev/dri/card1", true),
            candidate("/dev/dri/card2", false),
        ]);
        let paths: Vec<PathBuf> = ordered
            .into_iter()
            .map(|candidate| candidate.node.path)
            .collect();
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/dev/dri/card0"),
                PathBuf::from("/dev/dri/card1"),
                PathBuf::from("/dev/dri/card2"),
            ]
        );
    }

    #[test]
    fn render_selection_prefers_exact_sibling() {
        let candidates = [
            (
                node("/dev/dri/renderD128", 128),
                Some(PathBuf::from("/sys/devices/gpu-a")),
            ),
            (
                node("/dev/dri/renderD129", 129),
                Some(PathBuf::from("/sys/devices/gpu-b")),
            ),
        ];
        let picked = select_render_node(
            &candidates,
            Some(Path::new("/sys/devices/gpu-b")),
            DrmDeviceKey {
                major: 226,
                minor: 1,
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(picked.path, PathBuf::from("/dev/dri/renderD129"));
    }

    #[test]
    fn render_selection_accepts_lone_split_soc_node() {
        let candidates = [(node("/dev/dri/renderD128", 128), None)];
        let picked = select_render_node(
            &candidates,
            Some(Path::new("/sys/devices/display")),
            DrmDeviceKey {
                major: 226,
                minor: 2,
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(picked.path, PathBuf::from("/dev/dri/renderD128"));
    }

    #[test]
    fn render_selection_accepts_lone_node_without_platform_metadata() {
        let candidates = [(node("/dev/dri/renderD128", 128), None)];
        assert!(
            select_render_node(
                &candidates,
                None,
                DrmDeviceKey {
                    major: 226,
                    minor: 0,
                }
            )
            .unwrap()
            .is_some()
        );
    }

    #[test]
    fn render_selection_refuses_ambiguous_nodes() {
        let candidates = [
            (node("/dev/dri/renderD128", 128), None),
            (node("/dev/dri/renderD129", 129), None),
        ];
        let error = select_render_node(
            &candidates,
            None,
            DrmDeviceKey {
                major: 226,
                minor: 0,
            },
        )
        .expect_err("ambiguous render-node selection must not guess");
        let message = error.to_string();
        assert!(message.contains(RENDER_NODE_ENV));
        assert!(message.contains("renderD128"));
        assert!(message.contains("renderD129"));
    }

    #[test]
    fn render_selection_returns_none_without_candidates() {
        assert!(
            select_render_node(
                &[],
                None,
                DrmDeviceKey {
                    major: 226,
                    minor: 0,
                }
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn fd_identity_matches_metadata_rdev() {
        let file = fs::File::open("/dev/null").unwrap();
        let metadata = file.metadata().unwrap();
        assert_eq!(
            device_key_from_fd(file.as_fd()).unwrap(),
            device_key_from_rdev(metadata.rdev())
        );
    }

    #[test]
    fn open_node_path_fails_for_missing_path() {
        let path = std::env::temp_dir().join("yserver-drm-platform-test-nonexistent");
        let _ = fs::remove_file(&path);
        assert!(open_node_path(&path).is_err());
    }

    /// On a host with exactly one render node, every primary node must resolve
    /// to it. This is the split display/render shape that keeps Asahi DRI3
    /// accelerated even though the two nodes have different platform parents.
    #[test]
    fn one_render_node_resolves_for_every_primary_node() {
        let Ok(render_nodes) = enumerate_nodes("renderD", DrmNodeKind::Render) else {
            return;
        };
        if render_nodes.len() != 1 {
            return;
        }
        let Ok(primary_nodes) = enumerate_nodes("card", DrmNodeKind::Primary) else {
            return;
        };
        for primary in primary_nodes {
            let resolved = render_node_for_primary(primary.key);
            assert!(
                matches!(&resolved, Ok(Some(node)) if node.path == render_nodes[0].path),
                "primary {} resolved to {resolved:?}, want {}",
                primary.key,
                render_nodes[0].path.display()
            );
        }
    }
}
