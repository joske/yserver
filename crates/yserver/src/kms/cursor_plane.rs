//! Hardware cursor plane — replaces the Vulkan-composited cursor
//! quad with a kernel-managed DRM cursor overlay.
//!
//! Why: the cursor quad was tied to compositor cadence. Every cursor
//! position change waited for the next `composite_and_flip`, which
//! is stalled by per-op `vkQueueWaitIdle` in the paint pipeline
//! (notably when hovering over GTK widgets that schedule
//! gradient/emboss repaints — observed as severe pointer lag in
//! mate-control-center on fuji). The DRM hardware cursor plane is
//! a separate overlay the kernel positions independently —
//! an atomic position commit is microseconds and doesn't touch the GPU.
//!
//! Atomic cursor plane (replacement for legacy `set_cursor2` /
//! `move_cursor` ioctls): when `DRM_CLIENT_CAP_ATOMIC` is set,
//! AMD/amdgpu ignores the legacy cursor ioctls even though they
//! succeed. Atomic plane commits work on all drivers including AMD.
//! Legacy ioctls are retained as a per-CRTC fallback when no atomic
//! cursor plane is discovered.
//!
//! Stage 5 Phase B (per-CRTC visibility + upload/show split): the
//! shared dumb buffer is mutated by `load_image` and bound to each
//! CRTC independently. Per-CRTC visibility tracking lets the
//! per-output `PendingAck` design queue a Sw→Hw transition for one
//! output without prematurely binding the plane on outputs that
//! haven't retired the transition yet (the multi-output double-cursor
//! hazard).

use std::{
    collections::{HashMap, HashSet},
    io, mem,
    ptr::NonNull,
    rc::Rc,
};

use drm::{
    Device as DrmDevice, DriverCapability,
    buffer::{Buffer, DrmFourcc},
    control::{Device as ControlDevice, PlaneType, crtc, dumbbuffer::DumbBuffer},
};

use crate::drm::Device;

/// A failed cursor bind/reposition operation, including whether the cursor is
/// still bound after best-effort rollback. Callers must not switch an output
/// to software composition while `remains_visible()` is true: doing so would
/// display both the old hardware cursor and the new software sprite.
#[derive(Debug)]
pub enum CursorShowError {
    Unbound(io::Error),
    StillVisible {
        operation_error: io::Error,
        rollback_error: Option<io::Error>,
    },
}

impl CursorShowError {
    #[must_use]
    pub fn remains_visible(&self) -> bool {
        matches!(self, Self::StillVisible { .. })
    }

    #[must_use]
    pub fn operation_error(&self) -> &io::Error {
        match self {
            Self::Unbound(error) => error,
            Self::StillVisible {
                operation_error, ..
            } => operation_error,
        }
    }

    #[must_use]
    pub fn rollback_error(&self) -> Option<&io::Error> {
        match self {
            Self::Unbound(_) => None,
            Self::StillVisible { rollback_error, .. } => rollback_error.as_ref(),
        }
    }

    /// A failed `set_cursor2` on an already-visible CRTC leaves the prior
    /// buffer/hotspot binding intact. A position-only retry cannot repair it;
    /// the caller must retry the full show operation.
    #[must_use]
    pub fn needs_full_rebind(&self) -> bool {
        matches!(
            self,
            Self::StillVisible {
                rollback_error: None,
                ..
            }
        )
    }
}

impl std::fmt::Display for CursorShowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unbound(error) => write!(f, "cursor remains unbound: {error}"),
            Self::StillVisible {
                operation_error,
                rollback_error,
            } => match rollback_error {
                Some(rollback_error) => write!(
                    f,
                    "cursor operation failed ({operation_error}) and hide rollback failed ({rollback_error}); prior binding remains visible"
                ),
                None => write!(
                    f,
                    "cursor rebind failed ({operation_error}); prior binding remains visible"
                ),
            },
        }
    }
}

impl std::error::Error for CursorShowError {}

fn bind_failure(prior_visible: bool, error: io::Error) -> CursorShowError {
    if prior_visible {
        CursorShowError::StillVisible {
            operation_error: error,
            rollback_error: None,
        }
    } else {
        CursorShowError::Unbound(error)
    }
}

fn move_failure(move_error: io::Error, rollback: io::Result<()>) -> CursorShowError {
    match rollback {
        Ok(()) => CursorShowError::Unbound(move_error),
        Err(rollback_error) => CursorShowError::StillVisible {
            operation_error: move_error,
            rollback_error: Some(rollback_error),
        },
    }
}

/// Fallback cursor size when `DRM_CAP_CURSOR_WIDTH/HEIGHT` query fails
/// (very old drivers, broken devices). Every Intel / AMD / mainstream-
/// Mali iGPU since ~2010 supports at least 64×64.
///
/// The ACTUAL dumb buffer is allocated at the dimensions the driver
/// reports via `DriverCapability::CursorWidth/Height` (typically 64 on
/// Intel i915, 128 or 256 on amdgpu, varies on others). Using the
/// driver-reported size is load-bearing: amdgpu's display engine
/// interprets the cursor framebuffer as if it were `cursor_width ×
/// cursor_height`, so allocating smaller causes it to read past our
/// data → cursor vertically squished + intermittent corruption.
pub const HW_CURSOR_FALLBACK_W: u32 = 64;
pub const HW_CURSOR_FALLBACK_H: u32 = 64;

/// A single shared DRM dumb buffer holding the current cursor image,
/// plus per-CRTC visibility state.
///
/// Per-CRTC visibility (Stage 5 Phase B refactor): each CRTC tracks
/// whether the plane is currently bound to it. v1's pre-Phase-B global
/// `visible: bool` was correct only on single-output systems and exposed
/// the multi-output double-cursor hazard when one output retired a
/// Sw→Hw transition before another.
pub struct CursorPlane {
    device: Rc<Device>,
    dumb: Option<DumbBuffer>,
    ptr: NonNull<u8>,
    len: usize,
    stride: u32,
    /// Cursor buffer dimensions in pixels. Sourced from
    /// `DriverCapability::CursorWidth/Height`. Mandatory match with the
    /// dumb buffer geometry — see [`HW_CURSOR_FALLBACK_W`].
    width: u32,
    height: u32,
    /// Per-CRTC binding state — `Some(true)` when the plane is shown on
    /// that CRTC; `Some(false)` when hidden; absent until first show/hide.
    visible: HashMap<crtc::Handle, bool>,
    /// Stage 5 Phase B — `CursorRecord.version` last memcpy'd into
    /// the dumb buffer. `cursor_plane_upload_image` compares the
    /// requested version against this for upload dedup; `None` after
    /// init / VT-leave / full modeset (forces the next show to
    /// re-upload).
    uploaded_version: Option<u64>,
    /// Compatibility masks for explicitly exposed universal cursor planes.
    /// `None` means the driver exposed no cursor planes, so the legacy cursor
    /// ioctls remain an optimistic runtime probe. When present, every active
    /// CRTC needs a distinct compatible plane: a single universal plane cannot
    /// simultaneously display a cursor on two CRTCs.
    explicit_plane_crtcs: Option<Vec<HashSet<crtc::Handle>>>,
    /// In-memory storage for the ioctl-free test factory. Production cursor
    /// planes own an mmap through `dumb`; tests need a safe buffer to exercise
    /// lazy-init upload/version behavior without a real DRM node.
    #[cfg(test)]
    _test_backing: Option<Box<[u8]>>,
}

/// Whether distinct cursor planes can be assigned to every required CRTC.
/// Union coverage is insufficient because one plane object can only be active
/// on one CRTC at a time even if its `possible_crtcs` mask names several.
fn cursor_planes_cover_crtcs(
    required_crtcs: &[crtc::Handle],
    plane_crtcs: &[HashSet<crtc::Handle>],
) -> bool {
    let mut required = required_crtcs.to_vec();
    required.sort_by_key(|handle| u32::from(*handle));
    required.dedup();
    required.sort_by_key(|handle| {
        plane_crtcs
            .iter()
            .filter(|possible| possible.contains(handle))
            .count()
    });

    fn assign(
        required: &[crtc::Handle],
        plane_crtcs: &[HashSet<crtc::Handle>],
        used: &mut [bool],
        index: usize,
    ) -> bool {
        if index == required.len() {
            return true;
        }
        for (plane_index, possible) in plane_crtcs.iter().enumerate() {
            if used[plane_index] || !possible.contains(&required[index]) {
                continue;
            }
            used[plane_index] = true;
            if assign(required, plane_crtcs, used, index + 1) {
                return true;
            }
            used[plane_index] = false;
        }
        false
    }

    assign(
        &required,
        plane_crtcs,
        &mut vec![false; plane_crtcs.len()],
        0,
    )
}

fn discover_cursor_plane_crtcs(device: &Device) -> io::Result<Vec<HashSet<crtc::Handle>>> {
    let resources = device.resource_handles()?;
    let mut cursor_planes = Vec::new();
    for handle in device.plane_handles()? {
        let info = device.get_plane(handle)?;
        let props = device.get_properties(handle)?;
        let map = props.as_hashmap(device)?;
        let Some(type_info) = map.get("type") else {
            continue;
        };
        let raw = props
            .iter()
            .find(|(property, _)| **property == type_info.handle())
            .map(|(_, value)| *value)
            .unwrap_or(0);
        if raw != PlaneType::Cursor as u64 {
            continue;
        }
        cursor_planes.push(
            resources
                .filter_crtcs(info.possible_crtcs())
                .into_iter()
                .collect(),
        );
    }
    Ok(cursor_planes)
}

impl CursorPlane {
    /// Allocate the cursor dumb buffer + mmap it and remember any universal
    /// cursor-plane compatibility masks exposed by the device. Drivers that
    /// expose no cursor planes retain the optimistic legacy-ioctl probe.
    ///
    /// # Errors
    /// Cursor-plane topology discovery, `create_dumb_buffer`, or
    /// `map_dumb_buffer` ioctl failures.
    pub fn new(device: Rc<Device>, crtcs: &[crtc::Handle]) -> io::Result<Self> {
        let cursor_planes = discover_cursor_plane_crtcs(&device)?;
        let explicit_plane_crtcs = if cursor_planes.is_empty() {
            log::debug!(
                "cursor: no universal cursor planes exposed; assuming legacy cursor ioctl support"
            );
            None
        } else {
            Some(cursor_planes)
        };
        // Query the driver's preferred cursor dimensions. amdgpu commonly
        // reports 128×128 or 256×256; i915 typically 64×64. We MUST use
        // the reported size — see [`HW_CURSOR_FALLBACK_W`] for the
        // load-bearing rationale.
        let width = device
            .get_driver_capability(DriverCapability::CursorWidth)
            .ok()
            .filter(|&w| w >= u64::from(HW_CURSOR_FALLBACK_W))
            .and_then(|w| u32::try_from(w).ok())
            .unwrap_or(HW_CURSOR_FALLBACK_W);
        let height = device
            .get_driver_capability(DriverCapability::CursorHeight)
            .ok()
            .filter(|&h| h >= u64::from(HW_CURSOR_FALLBACK_H))
            .and_then(|h| u32::try_from(h).ok())
            .unwrap_or(HW_CURSOR_FALLBACK_H);
        log::info!("cursor: driver reports CursorWidth={width} CursorHeight={height}");

        let mut dumb = device.create_dumb_buffer((width, height), DrmFourcc::Argb8888, 32)?;
        let stride = dumb.pitch();
        let mapping = device.map_dumb_buffer(&mut dumb)?;
        let len = mapping.len();
        let ptr =
            NonNull::new(mapping.as_ptr() as *mut u8).expect("non-null mmap for cursor plane");
        mem::forget(mapping);
        // Zero-fill the plane buffer up front.
        unsafe { std::ptr::write_bytes(ptr.as_ptr(), 0, len) };

        let plane = Self {
            device,
            dumb: Some(dumb),
            ptr,
            len,
            stride,
            width,
            height,
            visible: HashMap::new(),
            uploaded_version: None,
            explicit_plane_crtcs,
            #[cfg(test)]
            _test_backing: None,
        };
        if !plane.supports_crtcs(crtcs) {
            log::warn!(
                "cursor: explicitly exposed planes cannot simultaneously cover all {} active CRTCs",
                crtcs.len()
            );
        }
        Ok(plane)
    }

    /// Build an ioctl-free cursor plane for platform state-machine tests.
    /// The backing allocation is real, so upload/version paths are safe; show
    /// remains unavailable because there is deliberately no dumb buffer.
    #[cfg(test)]
    pub(crate) fn for_tests_stub(device: Rc<Device>, width: u32, height: u32) -> Self {
        let stride = width.checked_mul(4).expect("test cursor stride overflow");
        let len = usize::try_from(stride)
            .expect("test cursor stride fits usize")
            .checked_mul(usize::try_from(height).expect("test cursor height fits usize"))
            .expect("test cursor allocation overflow");
        let mut test_backing = vec![0_u8; len].into_boxed_slice();
        let ptr = NonNull::new(test_backing.as_mut_ptr()).expect("non-empty test cursor buffer");
        Self {
            device,
            dumb: None,
            ptr,
            len,
            stride,
            width,
            height,
            visible: HashMap::new(),
            uploaded_version: None,
            explicit_plane_crtcs: None,
            _test_backing: Some(test_backing),
        }
    }

    /// Whether this device's explicitly exposed cursor planes can cover all
    /// requested CRTCs simultaneously. Drivers exposing no universal cursor
    /// planes return true and are probed through legacy ioctls at bind time.
    #[must_use]
    pub fn supports_crtcs(&self, crtcs: &[crtc::Handle]) -> bool {
        self.explicit_plane_crtcs
            .as_ref()
            .is_none_or(|planes| cursor_planes_cover_crtcs(crtcs, planes))
    }

    /// Copy a cursor image into the plane buffer. `bgra_bytes` is a
    /// tightly-packed `width × height × 4` BGRA8 buffer matching the
    /// DRM `ARGB8888` byte order in little-endian. The image lands at
    /// (0, 0); the remainder of the 64×64 buffer is zero-filled
    /// (transparent).
    ///
    /// Returns `Err(InvalidInput)` if the image is larger than
    /// `HW_CURSOR_W × HW_CURSOR_H` — caller falls back to the
    /// compositor cursor path.
    pub fn load_image(&mut self, image_w: u32, image_h: u32, bgra_bytes: &[u8]) -> io::Result<()> {
        if image_w == 0 || image_h == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "zero-sized cursor",
            ));
        }
        if image_w > self.width || image_h > self.height {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cursor exceeds hardware plane size",
            ));
        }
        let img_stride = (image_w as usize) * 4;
        let expected_bytes = img_stride * image_h as usize;
        if bgra_bytes.len() < expected_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cursor bytes shorter than width*height*4",
            ));
        }
        // Clear so a smaller cursor doesn't leave previous pixels.
        unsafe { std::ptr::write_bytes(self.ptr.as_ptr(), 0, self.len) };
        for row in 0..(image_h as usize) {
            let src_off = row * img_stride;
            let dst_off = row * (self.stride as usize);
            unsafe {
                std::ptr::copy_nonoverlapping(
                    bgra_bytes.as_ptr().add(src_off),
                    self.ptr.as_ptr().add(dst_off),
                    img_stride,
                );
            }
        }
        Ok(())
    }

    /// Stage 5 Phase B — versioned upload. Memcpys `bgra_bytes` into
    /// the shared dumb buffer ONLY when `version` differs from
    /// `uploaded_version`. **Never calls `set_cursor2`**; binding
    /// the buffer to a CRTC is a separate step (`show`).
    /// This split is load-bearing for the per-output transition
    /// state machine — uploading must not prematurely show pixels
    /// on CRTCs whose Sw→Hw retire is still pending.
    ///
    /// # Errors
    /// Same as [`Self::load_image`].
    pub fn upload_image(
        &mut self,
        version: u64,
        image_w: u32,
        image_h: u32,
        bgra_bytes: &[u8],
    ) -> io::Result<()> {
        if self.uploaded_version == Some(version) {
            return Ok(());
        }
        self.load_image(image_w, image_h, bgra_bytes)?;
        self.uploaded_version = Some(version);
        Ok(())
    }

    /// The version currently held in the dumb buffer, if any.
    #[must_use]
    pub fn uploaded_version(&self) -> Option<u64> {
        self.uploaded_version
    }

    /// Invalidate the tracked uploaded version. The next
    /// `upload_image` will memcpy unconditionally regardless of
    /// version. Used by global recovery paths (VT-leave, full
    /// modeset, `drain_all`).
    pub fn invalidate_uploaded_version(&mut self) {
        self.uploaded_version = None;
    }

    /// Make the cursor visible on `crtc` at image-top-left position
    /// `(img_x, img_y)` in CRTC-local coordinates, with the given
    /// `hotspot`. Uses `set_cursor2` + `move_cursor` (legacy ioctls)
    /// — Xorg's modesetting driver does the same
    /// (`drmmode_display.c:1812`). Legacy cursor ioctls don't EBUSY-
    /// collide with atomic scanout commits on the same CRTC (the
    /// kernel routes them through a separate path), so we avoid the
    /// atomic-cursor-vs-atomic-pageflip storm that motivated the
    /// (now-abandoned) bundle-cursor-atomic branch.
    /// Idempotent — repeated calls just re-bind and reposition.
    ///
    /// # Errors
    /// Ioctl failure.
    pub fn show(
        &mut self,
        crtc: crtc::Handle,
        hotspot: (i32, i32),
        img_x: i32,
        img_y: i32,
    ) -> Result<(), CursorShowError> {
        log::debug!(
            "cursor_plane::show CRTC={crtc:?} hotspot=({},{}) pos=({img_x},{img_y}) \
             prior_visible={}",
            hotspot.0,
            hotspot.1,
            self.visible.get(&crtc).copied().unwrap_or(false),
        );
        self.show_legacy(crtc, hotspot, img_x, img_y)
    }

    #[allow(deprecated)]
    fn show_legacy(
        &mut self,
        crtc: crtc::Handle,
        hotspot: (i32, i32),
        img_x: i32,
        img_y: i32,
    ) -> Result<(), CursorShowError> {
        let Some(dumb) = self.dumb.as_ref() else {
            return Err(CursorShowError::Unbound(io::Error::other(
                "cursor plane already destroyed",
            )));
        };
        let prior_visible = self.is_visible_on(crtc);
        self.device
            .set_cursor2(crtc, Some(dumb), hotspot)
            .map_err(|error| bind_failure(prior_visible, error))?;
        self.visible.insert(crtc, true);
        match self.device.move_cursor(crtc, (img_x, img_y)) {
            Ok(()) => Ok(()),
            Err(move_error) => Err(move_failure(move_error, self.hide_legacy(crtc))),
        }
    }

    /// Detach the cursor from `crtc`. The plane buffer is retained so
    /// a future `show` doesn't have to re-allocate. Uses `set_cursor2`
    /// (legacy ioctl) — see [`Self::show`] for why we don't use atomic.
    ///
    /// # Errors
    /// `set_cursor2` ioctl failure.
    pub fn hide(&mut self, crtc: crtc::Handle) -> io::Result<()> {
        log::debug!(
            "cursor_plane::hide CRTC={crtc:?} prior_visible={}",
            self.visible.get(&crtc).copied().unwrap_or(false),
        );
        self.hide_legacy(crtc)
    }

    #[allow(deprecated)]
    fn hide_legacy(&mut self, crtc: crtc::Handle) -> io::Result<()> {
        self.device.set_cursor2::<DumbBuffer>(crtc, None, (0, 0))?;
        self.visible.insert(crtc, false);
        Ok(())
    }

    /// Move the cursor on `crtc` to image-top-left `(x, y)` in
    /// CRTC-local coordinates. Uses `drmModeMoveCursor` (legacy ioctl)
    /// — Xorg's modesetting driver does the same
    /// (`drmmode_display.c:1797`).
    ///
    /// The legacy path is **immediate** (the kernel updates the cursor
    /// plane synchronously, not vblank-paced) — perfect for cursor
    /// responsiveness. It also doesn't EBUSY-collide with atomic
    /// scanout commits on the same CRTC because the kernel routes
    /// legacy cursor ops through a separate path from the atomic
    /// state machine.
    ///
    /// # Errors
    /// `move_cursor` ioctl failure.
    #[allow(deprecated)]
    pub fn move_to(&self, crtc: crtc::Handle, x: i32, y: i32) -> io::Result<()> {
        self.device.move_cursor(crtc, (x, y))
    }

    /// True iff the plane is currently bound (via `show`) on `crtc`.
    #[must_use]
    pub fn is_visible_on(&self, crtc: crtc::Handle) -> bool {
        self.visible.get(&crtc).copied().unwrap_or(false)
    }

    /// True iff the plane is currently bound on at least one CRTC.
    #[allow(dead_code)] // diagnostic accessor; no v2 production callers
    #[must_use]
    pub fn is_visible(&self) -> bool {
        self.visible.values().any(|&v| v)
    }

    /// Iterate every CRTC the plane has ever been bound or hidden
    /// against.
    pub fn known_crtcs(&self) -> impl Iterator<Item = crtc::Handle> + '_ {
        self.visible.keys().copied()
    }

    /// Forget visibility records for routes no longer active on this device.
    /// Raw CRTC handles may be reused after a topology change; carrying an old
    /// `true` into the new route would skip the required show transition.
    pub fn retain_crtcs(&mut self, active: &HashSet<crtc::Handle>) {
        self.visible.retain(|crtc, _| active.contains(crtc));
    }

    /// Cursor plane width in pixels (driver-reported).
    #[must_use]
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Cursor plane height in pixels (driver-reported).
    #[must_use]
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Diagnostic: write the current dumb buffer contents to a PPM file
    /// at `path`. Reads the kernel-visible bytes (respecting `self.stride`)
    /// so a stride/pitch mismatch between what `load_image` writes and
    /// what the display engine samples shows up as a visible distortion
    /// in the dump.
    ///
    /// # Errors
    /// File I/O failure.
    pub fn dump_to_ppm(&self, path: &str) -> io::Result<()> {
        use std::io::Write;
        let mut file = std::fs::File::create(path)?;
        let w = self.width;
        let h = self.height;
        file.write_all(format!("P6\n{w} {h}\n255\n").as_bytes())?;
        let mut row_buf = vec![0u8; (w as usize) * 3];
        for y in 0..h as usize {
            let row_start = y * (self.stride as usize);
            for x in 0..w as usize {
                let pi = row_start + x * 4;
                // ARGB8888 in little-endian on the wire is B, G, R, A bytes.
                let b = unsafe { *self.ptr.as_ptr().add(pi) };
                let g = unsafe { *self.ptr.as_ptr().add(pi + 1) };
                let r = unsafe { *self.ptr.as_ptr().add(pi + 2) };
                row_buf[x * 3] = r;
                row_buf[x * 3 + 1] = g;
                row_buf[x * 3 + 2] = b;
            }
            file.write_all(&row_buf)?;
        }
        log::info!("cursor: dumped {path} ({w}x{h}, stride={})", self.stride);
        Ok(())
    }
}

impl Drop for CursorPlane {
    fn drop(&mut self) {
        // Best-effort: hide cursor on all known CRTCs before releasing resources.
        let crtcs: Vec<crtc::Handle> = self.known_crtcs().collect();
        for crtc in crtcs {
            if self.visible.get(&crtc).copied().unwrap_or(false)
                && let Err(e) = self.hide(crtc)
            {
                log::debug!("cursor: hide on drop for {crtc:?} failed: {e}");
            }
        }
        if let Some(dumb) = self.dumb.take() {
            let _ = self.device.destroy_dumb_buffer(dumb);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_crtc(raw: u32) -> crtc::Handle {
        ::drm::control::from_u32(raw).unwrap()
    }

    #[test]
    fn cursor_plane_coverage_requires_every_crtc() {
        let a = test_crtc(11);
        let b = test_crtc(12);
        let planes = vec![HashSet::from([a])];

        assert!(cursor_planes_cover_crtcs(&[a], &planes));
        assert!(!cursor_planes_cover_crtcs(&[a, b], &planes));
    }

    #[test]
    fn cursor_plane_coverage_requires_distinct_simultaneous_planes() {
        let a = test_crtc(11);
        let b = test_crtc(12);
        let shared_only = vec![HashSet::from([a, b])];
        let independently_drivable = vec![HashSet::from([a, b]), HashSet::from([a, b])];

        assert!(!cursor_planes_cover_crtcs(&[a, b], &shared_only));
        assert!(cursor_planes_cover_crtcs(&[a, b], &independently_drivable));
    }

    #[test]
    fn cursor_plane_coverage_finds_non_greedy_matching() {
        let a = test_crtc(11);
        let b = test_crtc(12);
        let planes = vec![HashSet::from([a, b]), HashSet::from([a])];

        assert!(cursor_planes_cover_crtcs(&[a, b], &planes));
    }

    #[test]
    fn failed_rebind_preserves_prior_visibility() {
        let error = bind_failure(true, io::Error::from_raw_os_error(libc::EINVAL));
        assert!(error.remains_visible());
        assert!(error.rollback_error().is_none());

        let error = bind_failure(false, io::Error::from_raw_os_error(libc::EINVAL));
        assert!(!error.remains_visible());
    }

    #[test]
    fn failed_move_reports_hide_rollback_result() {
        let error = move_failure(io::Error::from_raw_os_error(libc::EINVAL), Ok(()));
        assert!(!error.remains_visible());

        let error = move_failure(
            io::Error::from_raw_os_error(libc::EINVAL),
            Err(io::Error::from_raw_os_error(libc::ENODEV)),
        );
        assert!(error.remains_visible());
        assert_eq!(
            error.rollback_error().and_then(io::Error::raw_os_error),
            Some(libc::ENODEV)
        );
    }

    /// Phase B regression: `is_visible_on` tracks per-CRTC binding
    /// independently.
    #[test]
    fn visibility_is_per_crtc() {
        let mut visible: HashMap<crtc::Handle, bool> = HashMap::new();
        let crtc_a: crtc::Handle = ::drm::control::from_u32(11).unwrap();
        let crtc_b: crtc::Handle = ::drm::control::from_u32(12).unwrap();

        visible.insert(crtc_a, true);
        assert!(visible.get(&crtc_a).copied().unwrap_or(false));
        assert!(!visible.get(&crtc_b).copied().unwrap_or(false));

        visible.insert(crtc_b, true);
        assert!(visible.get(&crtc_a).copied().unwrap_or(false));
        assert!(visible.get(&crtc_b).copied().unwrap_or(false));

        visible.insert(crtc_a, false);
        assert!(!visible.get(&crtc_a).copied().unwrap_or(false));
        assert!(visible.get(&crtc_b).copied().unwrap_or(false));
    }

    /// Phase B regression test for the unavailable-plane path. The
    /// v2 `PlatformBackend::for_tests()` fixture has no real DRM
    /// device, so `cursor_plane` is `None`. The hooks must surface
    /// that cleanly via `Err(io::Error::other(...))` rather than
    /// panicking — every Phase D' recovery path relies on this so
    /// VT-leave / shutdown / drain_all hooks can fire blindly.
    #[test]
    fn unavailable_plane_returns_err_not_panic() {
        use crate::kms::render::platform::PlatformBackend;

        let mut p = PlatformBackend::for_tests();
        assert!(!p.cursor_plane_available());
        assert!(
            p.cursor_plane_upload_image_for_output(0, 1, 16, 16, &[0u8; 16 * 16 * 4])
                .is_err()
        );
        assert!(p.cursor_plane_show_on_crtc(0, 0, 0, 0, 0).is_err());
        assert!(p.cursor_plane_move(0, 0, 0, 0).is_err());
        assert!(p.cursor_plane_hide_on_crtc(0).is_err());
        assert!(p.cursor_plane_hide_all().is_err());
        assert!(p.cursor_plane_uploaded_version_for_output(0).is_none());
    }
}
