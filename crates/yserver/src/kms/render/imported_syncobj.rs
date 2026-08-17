//! A DRI3 1.4 syncobj imported from a client fd, held as a process-local
//! DRM syncobj handle.
//!
//! This deliberately has no Vulkan in it. A DRM syncobj is a kernel object
//! and every operation the server needs — signal, query, eventfd, and
//! sync-file fence publication — has a DRM ioctl. Importing it into a
//! `VkSemaphore` instead only works where the
//! driver's `OPAQUE_FD` payload happens to be a DRM syncobj, which is true on
//! Mesa and false on NVIDIA proprietary
//! (`vkImportSemaphoreFdKHR` → `VK_ERROR_INITIALIZATION_FAILED`). See
//! docs/superpowers/specs/2026-08-08-dri3-syncobj-drm-signal-design.md.
//!
//! The sibling `OwnedSemaphore` keeps the Vulkan path for XSync `Fence`
//! resources, which need a real `VkSemaphore` for `FDFromFence`'s sync_file
//! export.
//!
//! The `Arc<crate::drm::Device>` here MUST be the render node — the device
//! DRI3 hands the client (`RenderDevice::render_node_device`), never the
//! KMS node. See the spec's "Which fd to ask" section.

use std::{
    os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd},
    sync::Arc,
};

use ::drm::control::{Device as DrmControlDevice, syncobj};

#[cfg(all(target_os = "linux", target_env = "musl"))]
type SyncobjIoctlReq = libc::c_int;
#[cfg(any(
    all(target_os = "linux", not(target_env = "musl")),
    target_os = "freebsd"
))]
type SyncobjIoctlReq = libc::c_ulong;

/// `<drm/drm.h>`'s `struct drm_syncobj_handle`.
#[repr(C)]
struct DrmSyncobjHandleArgs {
    handle: u32,
    flags: u32,
    fd: i32,
    pad: u32,
    point: u64,
}

const DRM_SYNCOBJ_FD_TO_HANDLE_FLAGS_IMPORT_SYNC_FILE: u32 = 1 << 0;

// `_IOWR('d', 0xC2, struct drm_syncobj_handle)`:
// dir=READ|WRITE(3), size=24, type='d'(0x64), nr=0xC2.
//
// Linux musl's ioctl request is signed 32-bit while glibc and FreeBSD use
// unsigned long. Build the bit pattern as u32 before casting so the high RW
// bits survive on every supported target.
const DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE: SyncobjIoctlReq = ((3u32 << 30)
    | ((std::mem::size_of::<DrmSyncobjHandleArgs>() as u32) << 16)
    | (0x64u32 << 8)
    | 0xC2) as SyncobjIoctlReq;

fn import_sync_file_into_handle(
    drm: &crate::drm::Device,
    handle: syncobj::Handle,
    fd: BorrowedFd<'_>,
) -> std::io::Result<()> {
    let mut args = DrmSyncobjHandleArgs {
        handle: handle.into(),
        flags: DRM_SYNCOBJ_FD_TO_HANDLE_FLAGS_IMPORT_SYNC_FILE,
        fd: fd.as_raw_fd(),
        pad: 0,
        point: 0,
    };
    // SAFETY: `drm` and `fd` are valid borrowed descriptors; `args` exactly
    // matches the kernel's 24-byte struct and remains live for the duration
    // of the ioctl. The kernel takes its own fence reference.
    let rc = unsafe {
        libc::ioctl(
            drm.as_fd().as_raw_fd(),
            DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE,
            std::ptr::addr_of_mut!(args),
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

pub(crate) struct ImportedSyncobj {
    drm: Arc<crate::drm::Device>,
    handle: syncobj::Handle,
}

impl ImportedSyncobj {
    /// Import a client's `DRM_SYNCOBJ` fd as a process-local handle. The fd is
    /// only borrowed — `DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE` does not consume it —
    /// so the caller keeps ownership and drops it normally. Importing a
    /// syncobj fd creates a NEW handle (with its own reference) for every
    /// import; the underlying `struct drm_syncobj` is shared, which is what
    /// lets a server-side signal reach the client's handle.
    pub(crate) fn import(
        drm: Arc<crate::drm::Device>,
        fd: BorrowedFd<'_>,
    ) -> std::io::Result<Self> {
        let handle = drm.fd_to_syncobj(fd, false)?;
        Ok(Self { drm, handle })
    }

    /// Current timeline value. Replaces `vkGetSemaphoreCounterValue` in the
    /// deferred-acquire polling fallback.
    pub(crate) fn timeline_value(&self) -> std::io::Result<u64> {
        let mut points = [0u64; 1];
        self.drm
            .syncobj_timeline_query(&[self.handle], &mut points, false)?;
        Ok(points[0])
    }

    /// Register a non-blocking kernel notification for a timeline point.
    /// Unchanged in behaviour from the previous `OwnedSemaphore` version —
    /// that method already went through DRM.
    pub(crate) fn signaled_eventfd(&self, value: u64) -> std::io::Result<OwnedFd> {
        use nix::sys::eventfd::{EfdFlags, EventFd};

        let event =
            EventFd::from_value_and_flags(0, EfdFlags::EFD_NONBLOCK | EfdFlags::EFD_CLOEXEC)
                .map_err(|e| std::io::Error::other(format!("eventfd: {e}")))?;
        self.drm
            .syncobj_eventfd(self.handle, value, event.as_fd(), false)?;
        Ok(event.into())
    }
}

/// Probe once whether `DRM_IOCTL_SYNCOBJ_EVENTFD` actually works on `drm`.
///
/// Classify by probing, not by errno. FreeBSD's drm-kmod reports
/// `DRM_CAP_SYNCOBJ_TIMELINE` — which is what `Dri3Caps::syncobj` derives from
/// — but returns **EINVAL** for this ioctl, and EINVAL is indistinguishable
/// from a genuinely bad argument on a supported kernel. Latching on EINVAL
/// would disable kernel notification for a whole session on one malformed
/// Present; not latching leaves a warning per frame (measured: 754 in one
/// FreeBSD/MATE session). Probing with arguments we know are valid answers the
/// question once and unambiguously.
///
/// Mirrors the shape of `eventfd_fires_on_the_registered_point` below, which
/// passes on Linux: a fresh syncobj, a future timeline point, no wait.
pub(crate) fn eventfd_supported(drm: &Arc<crate::drm::Device>) -> bool {
    use nix::sys::eventfd::{EfdFlags, EventFd};

    let Ok(handle) = drm.create_syncobj(false) else {
        return false;
    };
    let supported =
        EventFd::from_value_and_flags(0, EfdFlags::EFD_NONBLOCK | EfdFlags::EFD_CLOEXEC)
            .is_ok_and(|event| drm.syncobj_eventfd(handle, 1, event.as_fd(), false).is_ok());
    if let Err(e) = drm.destroy_syncobj(handle) {
        log::warn!("destroy probe syncobj failed: {e}");
    }
    supported
}

impl std::fmt::Debug for ImportedSyncobj {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImportedSyncobj")
            .field("handle", &self.handle)
            .finish_non_exhaustive()
    }
}

impl Drop for ImportedSyncobj {
    fn drop(&mut self) {
        if let Err(e) = self.drm.destroy_syncobj(self.handle) {
            log::warn!("destroy imported DRM syncobj handle failed: {e}");
        }
    }
}

impl yserver_core::backend::SyncobjHandle for ImportedSyncobj {
    /// Host-signal a timeline point. Replaces `vkSignalSemaphore`, which was
    /// also a host operation.
    ///
    /// Note the kernel CLAMPS: signalling a point at or below the current
    /// value succeeds silently and leaves the timeline where it was. Callers
    /// cannot use the return value to detect an out-of-order release.
    fn signal(&self, value: u64) -> std::io::Result<()> {
        self.drm.syncobj_timeline_signal(&[self.handle], &[value])
    }

    /// Publish a pending GPU fence at a timeline point, matching
    /// Xwayland's `xwl_dri3_syncobj_import_fence`:
    ///
    /// 1. import the sync_file as a temporary binary syncobj;
    /// 2. transfer binary point 0 to the destination timeline point;
    /// 3. destroy the temporary handle.
    ///
    /// The transfer installs its own reference to the fence, so destroying
    /// the temporary handle does not make the destination point disappear.
    fn import_sync_file(&self, value: u64, fd: BorrowedFd<'_>) -> std::io::Result<()> {
        let temporary = self.drm.create_syncobj(false)?;
        let import = import_sync_file_into_handle(&self.drm, temporary, fd);
        let transfer = import.and_then(|()| {
            self.drm
                .syncobj_timeline_transfer(temporary, self.handle, 0, value)
        });
        let destroy = self.drm.destroy_syncobj(temporary);
        match (transfer, destroy) {
            (Err(e), _) => Err(e),
            (Ok(()), Err(e)) => Err(e),
            (Ok(()), Ok(())) => Ok(()),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::{os::fd::AsFd, sync::Arc};

    use ::drm::control::Device as DrmControlDevice;

    use super::*;

    #[test]
    fn syncobj_fd_to_handle_ioctl_layout_matches_drm_header() {
        assert_eq!(std::mem::size_of::<DrmSyncobjHandleArgs>(), 24);
        assert_eq!(DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE as u32, 0xC018_64C2);
    }

    /// Open a render node, or skip. Every test here needs real DRM ioctls;
    /// they are `#[ignore]` so CI never runs them, but a machine without a
    /// node should skip rather than fail.
    ///
    /// Do NOT hardcode `/dev/dri/renderD128`. `kms/render_node.rs:1-8` states
    /// the rule outright — "we deliberately do **not** hardcode
    /// `/dev/dri/renderD128` — on multi-GPU hosts that selects the wrong
    /// device" — and the nvidia box became exactly such a host on 2026-08-08:
    /// `renderD128` is nvidia-drm and `renderD129` is the Raphael iGPU. A
    /// hardcoded 128 would make a run intended to validate Mesa silently
    /// exercise nvidia-drm and report green.
    ///
    /// Honour `YSERVER_TEST_RENDER_NODE` so a Mesa run can be directed at the
    /// amdgpu node, and enumerate otherwise rather than guessing.
    pub(crate) fn render_node() -> Option<Arc<crate::drm::Device>> {
        if let Ok(path) = std::env::var("YSERVER_TEST_RENDER_NODE") {
            return crate::drm::Device::open_render_node(&path)
                .ok()
                .map(Arc::new);
        }
        let mut paths: Vec<_> = std::fs::read_dir("/dev/dri")
            .ok()?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("renderD"))
            })
            .collect();
        paths.sort();
        paths
            .iter()
            .find_map(|p| crate::drm::Device::open_render_node(p.to_str()?).ok())
            .map(Arc::new)
    }

    /// Full round trip mirroring the server's sequence: the client exports a
    /// syncobj fd, the server imports it, signals a release point, and the
    /// client's own handle observes it through its own separate handle.
    /// Run with `cargo test -p yserver --lib imported_syncobj -- --ignored`.
    #[test]
    #[ignore = "needs a DRM render node"]
    fn signal_reaches_the_clients_handle() {
        let Some(drm) = render_node() else {
            eprintln!("skipping: no render node");
            return;
        };

        let client_handle = drm.create_syncobj(false).expect("create syncobj");
        let fd = drm.syncobj_to_fd(client_handle, false).expect("export fd");

        let imported = ImportedSyncobj::import(drm.clone(), fd.as_fd()).expect("import");
        assert_eq!(imported.timeline_value().expect("query"), 0);

        yserver_core::backend::SyncobjHandle::signal(&imported, 7).expect("signal");

        // The client must observe the release through ITS handle, not the
        // server's, or the two are not aliasing one payload and the client
        // would wait forever.
        let mut points = [0u64; 1];
        drm.syncobj_timeline_query(&[client_handle], &mut points, false)
            .expect("client query");
        assert_eq!(
            points[0], 7,
            "server signal did not reach the client handle"
        );

        drm.destroy_syncobj(client_handle).expect("destroy");
    }

    /// The synced Present Copy path publishes a completion fence rather than
    /// waiting for it and host-signalling the release point afterwards.
    #[test]
    #[ignore = "needs a DRM render node"]
    fn sync_file_fence_reaches_the_clients_timeline_point() {
        let Some(drm) = render_node() else {
            eprintln!("skipping: no render node");
            return;
        };

        let client_handle = drm.create_syncobj(false).expect("create client syncobj");
        let client_fd = drm
            .syncobj_to_fd(client_handle, false)
            .expect("export client syncobj");
        let imported =
            ImportedSyncobj::import(drm.clone(), client_fd.as_fd()).expect("import client");

        let completed = drm
            .create_syncobj(true)
            .expect("create completed binary syncobj");
        let fence = drm
            .syncobj_to_fd(completed, true)
            .expect("export sync_file");
        yserver_core::backend::SyncobjHandle::import_sync_file(&imported, 11, fence.as_fd())
            .expect("publish fence");

        let mut points = [0u64; 1];
        drm.syncobj_timeline_query(&[client_handle], &mut points, false)
            .expect("client query");
        assert_eq!(points[0], 11, "published fence did not reach client");

        drm.destroy_syncobj(completed).expect("destroy completed");
        drm.destroy_syncobj(client_handle).expect("destroy client");
    }

    /// The acquire path's kernel notification.
    #[test]
    #[ignore = "needs a DRM render node"]
    fn eventfd_fires_on_the_registered_point() {
        let Some(drm) = render_node() else {
            eprintln!("skipping: no render node");
            return;
        };
        let client_handle = drm.create_syncobj(false).expect("create syncobj");
        let fd = drm.syncobj_to_fd(client_handle, false).expect("export fd");
        let imported = ImportedSyncobj::import(drm.clone(), fd.as_fd()).expect("import");

        let event = imported.signaled_eventfd(9).expect("register eventfd");
        let mut buf = [0u8; 8];
        assert!(
            nix::unistd::read(event.as_fd(), &mut buf).is_err(),
            "eventfd readable before the point was signalled",
        );

        yserver_core::backend::SyncobjHandle::signal(&imported, 9).expect("signal");
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(
            nix::unistd::read(event.as_fd(), &mut buf).is_ok(),
            "eventfd never fired after the point was signalled",
        );

        drm.destroy_syncobj(client_handle).expect("destroy");
    }

    /// Documents measured kernel behaviour the spec depends on: a stale or
    /// duplicate timeline point is CLAMPED and returns success, it is not
    /// rejected. Release replay after teardown therefore cannot be detected
    /// by checking the signal's return value.
    #[test]
    #[ignore = "needs a DRM render node"]
    fn a_stale_point_is_clamped_not_rejected() {
        use yserver_core::backend::SyncobjHandle as _;

        let Some(drm) = render_node() else {
            eprintln!("skipping: no render node");
            return;
        };
        let client_handle = drm.create_syncobj(false).expect("create syncobj");
        let fd = drm.syncobj_to_fd(client_handle, false).expect("export fd");
        let imported = ImportedSyncobj::import(drm.clone(), fd.as_fd()).expect("import");

        imported.signal(10).expect("signal 10");
        imported
            .signal(5)
            .expect("a stale point must still return Ok");
        assert_eq!(
            imported.timeline_value().expect("query"),
            10,
            "the kernel must clamp to the max, not regress the timeline",
        );

        drm.destroy_syncobj(client_handle).expect("destroy");
    }
}
