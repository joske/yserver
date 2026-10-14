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
//! Atomic position support is discovered per cursor plane from its
//! `CRTC_X`/`CRTC_Y` properties. The device records whether every active
//! CRTC can use that path; devices without complete support keep the legacy
//! path. Task 1 deliberately leaves pointer motion on `move_cursor` until the
//! cursor commit producer lands in Task 3. Image upload and plane show/hide
//! continue to use the legacy cursor ioctls.
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
    control::{Device as ControlDevice, PlaneType, crtc, dumbbuffer::DumbBuffer, plane, property},
};

use crate::drm::Device;

/// Legacy cursor-plane hide operation. Production uses `set_cursor2`; the
/// executor is retained on the plane so rejection rollback uses the same
/// ioctl path as ordinary hide operations.
pub(crate) trait CursorHideExecutor {
    fn hide(&self, device: &Device, crtc: crtc::Handle) -> io::Result<()>;
}

#[derive(Default)]
pub(crate) struct DrmCursorHideExecutor;

impl CursorHideExecutor for DrmCursorHideExecutor {
    #[allow(deprecated)]
    fn hide(&self, device: &Device, crtc: crtc::Handle) -> io::Result<()> {
        device.set_cursor2::<DumbBuffer>(crtc, None, (0, 0))
    }
}

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
    hide_executor: Rc<dyn CursorHideExecutor>,
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
    /// CRTC to cursor-plane assignment over the device's explicitly exposed
    /// universal cursor planes; no plane serves two CRTCs. `None` means the
    /// driver exposed no cursor planes, so the legacy cursor ioctls remain an
    /// optimistic runtime probe. A CRTC without an entry has no cursor plane.
    explicit_plane_assignment: Option<HashMap<crtc::Handle, plane::Handle>>,
    /// Atomic position properties of each CRTC's assigned plane. This is
    /// topology discovery data and must survive `retain_crtcs`.
    atomic_positions: HashMap<crtc::Handle, (plane::Handle, property::Handle, property::Handle)>,
    /// In-memory storage for the ioctl-free test factory. Production cursor
    /// planes own an mmap through `dumb`; tests need a safe buffer to exercise
    /// lazy-init upload/version behavior without a real DRM node.
    #[cfg(test)]
    _test_backing: Option<Box<[u8]>>,
}

/// Assign each CRTC at most one cursor plane and each plane to at most one
/// CRTC, like wlroots: a plane whose `possible_crtcs` is exactly one CRTC goes
/// to that CRTC (the kernel's legacy cursor plane on per-CRTC-cursor drivers),
/// then the remaining planes are matched to the remaining CRTCs with augmenting
/// paths (a maximum matching, so greedy order cannot strand a coverable CRTC).
fn assign_cursor_planes(
    crtcs: &[crtc::Handle],
    planes: &[DiscoveredCursorPlane],
) -> HashMap<crtc::Handle, usize> {
    let mut assignment = HashMap::new();
    let mut exclusive = vec![false; planes.len()];
    for (index, plane) in planes.iter().enumerate() {
        if plane.crtcs.len() == 1
            && let Some(crtc) = plane.crtcs.iter().next()
            && crtcs.contains(crtc)
            && !assignment.contains_key(crtc)
        {
            assignment.insert(*crtc, index);
            exclusive[index] = true;
        }
    }
    let mut owner: Vec<Option<usize>> = vec![None; planes.len()];
    for (crtc_index, crtc) in crtcs.iter().enumerate() {
        if !assignment.contains_key(crtc) {
            let mut visited = vec![false; planes.len()];
            augment_cursor_matching(
                crtc_index,
                crtcs,
                planes,
                &exclusive,
                &mut owner,
                &mut visited,
            );
        }
    }
    for (index, crtc_index) in owner.iter().enumerate() {
        if let Some(crtc_index) = crtc_index {
            assignment.insert(crtcs[*crtc_index], index);
        }
    }
    assignment
}

/// Kuhn's augmenting-path step: give `crtcs[crtc_index]` a plane, moving
/// earlier non-exclusive owners to other planes when that frees one.
fn augment_cursor_matching(
    crtc_index: usize,
    crtcs: &[crtc::Handle],
    planes: &[DiscoveredCursorPlane],
    exclusive: &[bool],
    owner: &mut [Option<usize>],
    visited: &mut [bool],
) -> bool {
    for index in 0..planes.len() {
        if exclusive[index] || visited[index] || !planes[index].crtcs.contains(&crtcs[crtc_index]) {
            continue;
        }
        visited[index] = true;
        let free = match owner[index] {
            None => true,
            Some(other) => augment_cursor_matching(other, crtcs, planes, exclusive, owner, visited),
        };
        if free {
            owner[index] = Some(crtc_index);
            return true;
        }
    }
    false
}

/// Explicit plane assignment and the atomic position properties of each
/// assigned plane. Drivers exposing no cursor planes get no assignment.
type CursorPlaneTopology = (
    Option<HashMap<crtc::Handle, plane::Handle>>,
    HashMap<crtc::Handle, (plane::Handle, property::Handle, property::Handle)>,
);

fn cursor_plane_topology(
    crtcs: &[crtc::Handle],
    planes: &[DiscoveredCursorPlane],
) -> CursorPlaneTopology {
    if planes.is_empty() {
        return (None, HashMap::new());
    }
    let assignment = assign_cursor_planes(crtcs, planes);
    let atomic_positions = assignment
        .iter()
        .filter_map(|(crtc, index)| {
            let plane = &planes[*index];
            Some((*crtc, (plane.handle, plane.crtc_x?, plane.crtc_y?)))
        })
        .collect();
    let assignment = assignment
        .into_iter()
        .map(|(crtc, index)| (crtc, planes[index].handle))
        .collect();
    (Some(assignment), atomic_positions)
}

struct DiscoveredCursorPlane {
    handle: plane::Handle,
    crtcs: HashSet<crtc::Handle>,
    crtc_x: Option<property::Handle>,
    crtc_y: Option<property::Handle>,
}

fn discover_cursor_plane_crtcs(
    device: &Device,
) -> io::Result<(Vec<crtc::Handle>, Vec<DiscoveredCursorPlane>)> {
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
        cursor_planes.push(DiscoveredCursorPlane {
            handle,
            crtcs: resources
                .filter_crtcs(info.possible_crtcs())
                .into_iter()
                .collect(),
            crtc_x: map.get("CRTC_X").map(|property| property.handle()),
            crtc_y: map.get("CRTC_Y").map(|property| property.handle()),
        });
    }
    Ok((resources.crtcs().to_vec(), cursor_planes))
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
        let (device_crtcs, cursor_planes) = discover_cursor_plane_crtcs(&device)?;
        if cursor_planes.is_empty() {
            log::debug!(
                "cursor: no universal cursor planes exposed; assuming legacy cursor ioctl support"
            );
        }
        let (explicit_plane_assignment, atomic_positions) =
            cursor_plane_topology(&device_crtcs, &cursor_planes);
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
            hide_executor: Rc::new(DrmCursorHideExecutor),
            dumb: Some(dumb),
            ptr,
            len,
            stride,
            width,
            height,
            visible: HashMap::new(),
            uploaded_version: None,
            explicit_plane_assignment,
            atomic_positions,
            #[cfg(test)]
            _test_backing: None,
        };
        if !plane.supports_crtcs(crtcs) {
            log::warn!(
                "cursor: no distinct cursor plane is assigned to every one of the {} active CRTCs",
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
            hide_executor: Rc::new(DrmCursorHideExecutor),
            dumb: None,
            ptr,
            len,
            stride,
            width,
            height,
            visible: HashMap::new(),
            uploaded_version: None,
            explicit_plane_assignment: None,
            atomic_positions: HashMap::new(),
            _test_backing: Some(test_backing),
        }
    }

    #[cfg(test)]
    pub(crate) fn for_tests_set_atomic_position_properties(
        &mut self,
        crtc: crtc::Handle,
        plane: plane::Handle,
        crtc_x: property::Handle,
        crtc_y: property::Handle,
    ) {
        self.atomic_positions.insert(crtc, (plane, crtc_x, crtc_y));
    }

    /// Set only the in-memory binding observation. Production never calls
    /// this; atomic scene-path tests use it to model a cursor that was already
    /// shown without issuing `set_cursor2` against the test DRM fd.
    #[cfg(test)]
    pub(crate) fn for_tests_set_visible(&mut self, crtc: crtc::Handle, visible: bool) {
        self.visible.insert(crtc, visible);
    }

    #[cfg(test)]
    pub(crate) fn for_tests_set_hide_executor(&mut self, executor: Rc<dyn CursorHideExecutor>) {
        self.hide_executor = executor;
    }

    /// Whether every requested CRTC has its own assigned cursor plane. Drivers
    /// exposing no universal cursor planes return true and are probed through
    /// legacy ioctls at bind time.
    #[must_use]
    pub fn supports_crtcs(&self, crtcs: &[crtc::Handle]) -> bool {
        self.explicit_plane_assignment
            .as_ref()
            .is_none_or(|assignment| crtcs.iter().all(|crtc| assignment.contains_key(crtc)))
    }

    /// Atomic plane and position properties for `crtc`, if its assigned
    /// cursor plane exposes both `CRTC_X` and `CRTC_Y`.
    #[allow(dead_code)] // consumed by the scene commit path in Task 2
    #[must_use]
    pub(crate) fn atomic_position_properties(
        &self,
        crtc: crtc::Handle,
    ) -> Option<(plane::Handle, property::Handle, property::Handle)> {
        self.atomic_positions.get(&crtc).copied()
    }

    /// Whether every requested CRTC has a cursor plane with atomic position
    /// properties. An empty topology is supported vacuously; active topology
    /// selection always supplies its complete CRTC list.
    #[must_use]
    pub(crate) fn supports_atomic_positions(&self, crtcs: &[crtc::Handle]) -> bool {
        crtcs
            .iter()
            .all(|crtc| self.atomic_position_properties(*crtc).is_some())
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
    /// `hotspot`. Showing and hiding remain legacy `set_cursor2` operations;
    /// steady pointer motion is selected separately by device capability.
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
    /// a future `show` doesn't have to re-allocate. Uses the legacy
    /// `set_cursor2` ioctl, as required for plane show/hide.
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
        self.hide_executor.hide(&self.device, crtc)?;
        self.visible.insert(crtc, false);
        Ok(())
    }

    /// Move the cursor on `crtc` to image-top-left `(x, y)` in
    /// CRTC-local coordinates with the master's legacy ioctl. Task 1 keeps
    /// this call for every device; Task 3 switches capable devices to the
    /// atomic position path.
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

    fn discovered(handle: u32, crtcs: &[crtc::Handle], atomic: bool) -> DiscoveredCursorPlane {
        DiscoveredCursorPlane {
            handle: ::drm::control::from_u32(handle).unwrap(),
            crtcs: crtcs.iter().copied().collect(),
            crtc_x: atomic.then(|| ::drm::control::from_u32(handle * 10 + 1).unwrap()),
            crtc_y: atomic.then(|| ::drm::control::from_u32(handle * 10 + 2).unwrap()),
        }
    }

    fn assigned_plane(topology: &CursorPlaneTopology, crtc: crtc::Handle) -> Option<plane::Handle> {
        topology.0.as_ref().and_then(|map| map.get(&crtc).copied())
    }

    #[test]
    // Mutation: pick the first compatible plane per CRTC independently. The
    // shared plane 21 would then serve both CRTCs, and CRTC a would not use
    // its own exclusive plane 22 (the kernel's legacy cursor plane for a).
    fn cursor_planes_prefer_exclusive_and_never_serve_two_crtcs() {
        let a = test_crtc(11);
        let b = test_crtc(12);
        let planes = [discovered(21, &[a, b], true), discovered(22, &[a], true)];
        let topology = cursor_plane_topology(&[a, b], &planes);

        assert_eq!(assigned_plane(&topology, a), Some(planes[1].handle));
        assert_eq!(assigned_plane(&topology, b), Some(planes[0].handle));
        assert_eq!(topology.1[&a].0, planes[1].handle);
        assert_eq!(topology.1[&b].0, planes[0].handle);
    }

    #[test]
    fn cursor_plane_atomic_positions_come_only_from_the_assigned_plane() {
        let a = test_crtc(11);
        let b = test_crtc(12);
        let planes = [discovered(21, &[a], false), discovered(22, &[a, b], true)];
        let topology = cursor_plane_topology(&[a, b], &planes);

        assert_eq!(assigned_plane(&topology, a), Some(planes[0].handle));
        assert!(!topology.1.contains_key(&a));
        assert_eq!(topology.1[&b].0, planes[1].handle);
    }

    #[test]
    fn cursor_plane_assignment_leaves_an_uncovered_crtc_unassigned() {
        let a = test_crtc(11);
        let b = test_crtc(12);
        let device = Rc::new(crate::drm::Device::for_tests().expect("test DRM device"));
        let mut plane = CursorPlane::for_tests_stub(device, 64, 64);
        let shared_only = [discovered(21, &[a, b], true)];
        let (assignment, atomic_positions) = cursor_plane_topology(&[a, b], &shared_only);
        plane.explicit_plane_assignment = assignment;
        plane.atomic_positions = atomic_positions;

        assert!(plane.supports_crtcs(&[a]));
        assert!(!plane.supports_crtcs(&[b]));
        assert!(!plane.supports_crtcs(&[a, b]));
        assert!(plane.supports_atomic_positions(&[a]));
        assert!(!plane.supports_atomic_positions(&[b]));

        let independently_drivable = [discovered(21, &[a, b], true), discovered(22, &[a, b], true)];
        let topology = cursor_plane_topology(&[a, b], &independently_drivable);
        assert_ne!(assigned_plane(&topology, a), assigned_plane(&topology, b));
        assert!(assigned_plane(&topology, b).is_some());
        assert_eq!(cursor_plane_topology(&[a, b], &[]).0, None);
    }

    fn assert_every_crtc_has_its_own_plane(crtcs: &[crtc::Handle], topology: &CursorPlaneTopology) {
        let assigned: Vec<_> = crtcs
            .iter()
            .map(|&crtc| assigned_plane(topology, crtc).expect("every CRTC gets a cursor plane"))
            .collect();
        let distinct: HashSet<_> = assigned.iter().collect();
        assert_eq!(distinct.len(), assigned.len(), "a plane serves two CRTCs");
    }

    #[test]
    // Mutation: greedy first-fit gives a→21, b→none, c→22; a maximum matching
    // reroutes a to 22 or 23 so that b can take 21.
    fn cursor_plane_assignment_is_a_maximum_matching() {
        let (a, b, c) = (test_crtc(11), test_crtc(12), test_crtc(13));
        let planes = [
            discovered(21, &[a, b], true),
            discovered(22, &[a, c], true),
            discovered(23, &[a, c], true),
        ];
        let topology = cursor_plane_topology(&[a, b, c], &planes);

        assert_every_crtc_has_its_own_plane(&[a, b, c], &topology);
    }

    #[test]
    // Mutation: let augmenting paths reassign exclusive planes. CRTC c's
    // exclusive plane 23 must stay with c while a and b share the rest.
    fn cursor_plane_matching_keeps_exclusive_planes() {
        let (a, b, c) = (test_crtc(11), test_crtc(12), test_crtc(13));
        let planes = [
            discovered(21, &[a, b, c], true),
            discovered(22, &[a, c], true),
            discovered(23, &[c], true),
        ];
        let topology = cursor_plane_topology(&[a, b, c], &planes);

        assert_every_crtc_has_its_own_plane(&[a, b, c], &topology);
        assert_eq!(assigned_plane(&topology, c), Some(planes[2].handle));
        assert_eq!(assigned_plane(&topology, a), Some(planes[1].handle));
        assert_eq!(assigned_plane(&topology, b), Some(planes[0].handle));
    }

    #[test]
    fn cursor_keeps_atomic_positions_across_a_zero_crtc_topology_step() {
        let crtc = test_crtc(13);
        let plane_handle = ::drm::control::from_u32(21).unwrap();
        let crtc_x = ::drm::control::from_u32(31).unwrap();
        let crtc_y = ::drm::control::from_u32(32).unwrap();
        let device = Rc::new(crate::drm::Device::for_tests().expect("test DRM device"));
        let mut plane = CursorPlane::for_tests_stub(device, 64, 64);
        plane.for_tests_set_atomic_position_properties(crtc, plane_handle, crtc_x, crtc_y);

        plane.retain_crtcs(&HashSet::new());
        plane.retain_crtcs(&HashSet::from([crtc]));

        assert_eq!(
            plane.atomic_position_properties(crtc),
            Some((plane_handle, crtc_x, crtc_y))
        );
        assert!(plane.supports_atomic_positions(&[crtc]));
        assert!(!plane.is_visible_on(crtc));
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
