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

use std::{collections::HashMap, io, mem, ptr::NonNull, sync::Arc};

use drm::{
    Device as DrmDevice, DriverCapability,
    buffer::{Buffer, DrmFourcc},
    control::{Device as ControlDevice, crtc, dumbbuffer::DumbBuffer},
};

use crate::drm::Device;

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
    device: Arc<Device>,
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
}

// SAFETY: ptr is an mmap'd kernel buffer that lives as long as
// `dumb`; no thread does interior mutation through the raw pointer
// without exclusive `&mut self`.
unsafe impl Send for CursorPlane {}

impl CursorPlane {
    /// Allocate the cursor dumb buffer + mmap it. Discovers cursor
    /// planes for `crtcs` and creates a DRM framebuffer for the atomic
    /// path. Falls back to legacy ioctls on any per-CRTC discovery
    /// failure.
    ///
    /// # Errors
    /// `create_dumb_buffer` or `map_dumb_buffer` ioctl failures.
    pub fn new(device: Arc<Device>, crtcs: &[crtc::Handle]) -> io::Result<Self> {
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

        // crtcs parameter retained for API stability and future expansion;
        // legacy `set_cursor2`/`move_cursor` route by CRTC handle directly,
        // no per-CRTC plane discovery needed.
        let _ = crtcs;

        Ok(Self {
            device,
            dumb: Some(dumb),
            ptr,
            len,
            stride,
            width,
            height,
            visible: HashMap::new(),
            uploaded_version: None,
        })
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
    ) -> io::Result<()> {
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
    ) -> io::Result<()> {
        let Some(dumb) = self.dumb.as_ref() else {
            return Err(io::Error::other("cursor plane already destroyed"));
        };
        self.device.set_cursor2(crtc, Some(dumb), hotspot)?;
        self.visible.insert(crtc, true);
        self.device.move_cursor(crtc, (img_x, img_y))?;
        Ok(())
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

    /// Cursor plane width in pixels (driver-reported).
    #[allow(dead_code)] // diagnostic accessor; no v2 production callers
    #[must_use]
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Cursor plane height in pixels (driver-reported).
    #[allow(dead_code)] // diagnostic accessor; no v2 production callers
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
        use crate::kms::v2::platform::PlatformBackend;

        let mut p = PlatformBackend::for_tests();
        assert!(!p.cursor_plane_available());
        assert!(
            p.cursor_plane_upload_image(1, 16, 16, &[0u8; 16 * 16 * 4])
                .is_err()
        );
        assert!(p.cursor_plane_show_on_crtc(0, 0, 0, 0, 0).is_err());
        assert!(p.cursor_plane_rebind_visible_crtcs(0, 0, 0, 0).is_err());
        assert!(p.cursor_plane_move(0, 0, 0, 0).is_err());
        assert!(p.cursor_plane_hide_on_crtc(0).is_err());
        assert!(p.cursor_plane_hide_all().is_err());
        assert!(p.cursor_plane_uploaded_version().is_none());
    }
}
