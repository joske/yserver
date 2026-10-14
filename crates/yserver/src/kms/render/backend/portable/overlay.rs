//! The Composite Overlay Window: its storage and window projection.

use crate::kms::render::{
    backend::{WindowGeometry, XBackend, portable::default_window_init_color, seam::Platform},
    store::DrawableKind,
    target::Dst,
};
use std::io;
use yserver_core::backend::OriginContext;

impl<P: Platform> XBackend<P> {
    pub(in crate::kms::render::backend) fn finish_cow_release(&mut self) {
        self.deferred_cow_release = false;

        // Keep the backend COW projection and storage together until the
        // last owner is safe to drop. For a direct frame, this helper runs
        // only after the composed replacement retired and its pins released.
        let cow_host_xid = yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0;
        self.windows.remove(&cow_host_xid);
        self.scene.mark_scene_structure_dirty();

        self.drain_engine_present_batches();
        if let Err(e) = self.engine.flush_render_batch(
            &mut self.store,
            self.platform.gpu_mut(),
            crate::kms::render::engine::RenderFlushReason::Other,
        ) {
            log::warn!("render release_overlay_window: flush_render_batch failed: {e:?}");
        }
        self.drain_render_telemetry();
        if let Some(id) = self.cow_id.take() {
            self.store_decref_with_invalidate(id);
        }
    }

    pub(in crate::kms::render::backend) fn finish_deferred_cow_release(&mut self) {
        if self.deferred_cow_release {
            self.finish_cow_release();
        }
    }

    /// Stage 4d — Composite Overlay Window allocation.
    ///
    /// The **0 → 1 claim edge only**: core owns the claim list and this
    /// backend counts nothing, so every call here is a first claim.
    /// Allocates screen-extent depth-24 storage at xid
    /// `COMPOSITE_OVERLAY_WINDOW` (0x103) and stores the resulting
    /// `DrawableId` on `self.cow_id`. The drawable stays off the normal
    /// scene path; xfwm4 paints its composited desktop into its own child
    /// window, so adding the COW as a topmost scene layer would cover the
    /// real output with a stale black surface.
    ///
    /// Initial fill: storage from `allocate_drawable_storage`
    /// is uninitialised Vk-DEVICE_LOCAL memory (same problem
    /// Stage 3f.14 fixed for `create_pixmap`). We do an explicit
    /// OPAQUE-black fill via `engine.fill_rect` so the
    /// compositor's first paint composites over a known value
    /// rather than recycled GPU garbage. Opaque, not transparent:
    /// the COW is depth-24, and a depth-24 drawable has no alpha
    /// channel on X11 — see `default_window_init_color`. The fill is
    /// best-effort — on the stub fixture (no Vk) `engine.fill_rect`
    /// errors; log + continue (storage already exists at xid level).
    pub(in crate::kms::render::backend) fn backend_overlay_get_overlay_window(
        &mut self,
        _origin: Option<OriginContext>,
    ) -> io::Result<bool> {
        if self.cow_id.is_some() {
            // Core only calls this on the 0 → 1 claim edge, so a live
            // `cow_id` here can only be a physical teardown the previous
            // final release deferred behind direct scanout: the protocol
            // resource was logically destroyed but the backend identity
            // and storage stayed alive for their safe replacement. Reuse
            // that identity and ask core to materialize the protocol
            // resource again.
            debug_assert!(
                self.deferred_cow_release,
                "get_overlay_window is the 0 → 1 edge; a live cow_id here \
                 without a deferred release means core and the backend have \
                 drifted",
            );
            self.deferred_cow_release = false;
            return Ok(true);
        }
        let fb_w = self.platform.fb_size().0.max(1);
        let fb_h = self.platform.fb_size().1.max(1);
        let storage = match self.platform.gpu().allocate_drawable_storage_as(
            fb_w,
            fb_h,
            24,
            crate::kms::vk::mem_accounting::MemCategory::WindowStorage,
        ) {
            Ok(storage) => {
                self.telemetry.record_storage_allocation();
                self.telemetry.record_image_view_create();
                storage
            }
            Err(e) => {
                // Test-fixture / no-Vk path: same shape as
                // `init_root_storage` — fall back to a null-view
                // stub so unit tests can exercise refcount /
                // scene-registration without a live Vk ICD.
                log::debug!("render get_overlay_window: no Vk, using stub COW storage: {e:?}");
                crate::kms::render::store::Storage::for_tests_null(
                    ash::vk::Extent2D {
                        width: u32::from(fb_w),
                        height: u32::from(fb_h),
                    },
                    crate::kms::render::gpu::GpuCore::format_for_depth(24),
                )
            }
        };
        let xid = yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0;
        // Defensive: if a stale mapping somehow survives a prior
        // teardown (decref's PendingFence path detaches xid for us,
        // but a synchronous-destroy path could race), detach first
        // so the allocate doesn't trip XidInUse.
        self.store.detach_xid(xid);
        let id = self
            .store_alloc(xid, DrawableKind::Window, 24, true, storage)
            .map_err(|e| {
                io::Error::other(format!("render get_overlay_window: store alloc: {e:?}"))
            })?;
        // Stage 3f.14 follow-on — zero-fill the fresh storage so
        // the compositor doesn't composite over recycled GPU
        // garbage on its first paint. Best-effort on stub paths.
        let rect = ash::vk::Rect2D {
            offset: ash::vk::Offset2D::default(),
            extent: ash::vk::Extent2D {
                width: u32::from(fb_w),
                height: u32::from(fb_h),
            },
        };
        if let Err(e) = self.engine.fill_rect(
            &mut self.store,
            self.platform.gpu_mut(),
            Dst::server_internal(id),
            rect,
            default_window_init_color(24),
        ) && self.platform.gpu().vk.is_some()
        {
            log::warn!("render get_overlay_window: initial fill failed: {e:?}");
        }
        self.cow_id = Some(id);

        // Phase 2 Task 2.2 — also materialize the backend's window-
        // tree projection so the COW participates in build_scene /
        // hit-testing / paint resolution the same way any top-level
        // window does. The xid is the well-known protocol xid; v2
        // keys windows on host xid directly.
        let cow_host_xid = yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0;
        let rank = self.alloc_window_stack_rank();
        let geom = WindowGeometry {
            border_width: 0,
            border_pixel: None,
            border_pixmap: None,
            x: 0,
            y: 0,
            width: fb_w,
            height: fb_h,
            depth: 24,
            mapped: true,
            viewable: true,
            // `parent: None` matches windows's convention for a
            // direct child of the root (root is not itself tracked
            // in windows — see register_top_level).
            parent: None,
            stack_rank: rank,
            bg_pixel: None,
            bg_pixmap: None,
            cursor: None,
        };
        self.windows.insert(cow_host_xid, geom);
        // The COW takes the pointer until its input region is emptied, so
        // crossings resolve it like any window (Nonlinear to a sibling).
        self.core.xid_map.insert(
            cow_host_xid,
            yserver_core::resources::COMPOSITE_OVERLAY_WINDOW,
        );
        self.deferred_cow_release = false;
        // Step 2 (DRIFT 2): the COW's place in top_level_order is no longer
        // set here — the GetOverlayWindow core handler reprojects from core
        // children via `sync_top_level_order` AFTER materialize_cow_resource
        // (which inserts the COW as a root child capped on top).
        self.scene.mark_scene_structure_dirty();
        Ok(true)
    }

    /// Stage 4d — Composite Overlay Window release.
    ///
    /// The **1 → 0 claim edge only**: core owns the claim list, so every
    /// call here is the final release. Decrefs the store storage and
    /// clears `self.cow_id`. If direct scanout is active, the logical
    /// release succeeds immediately but physical teardown is deferred
    /// until the composed replacement retires.
    /// `DrawableStore::decref` removes the xid mapping (immediately on
    /// synchronous-destroy, deferred on `PendingFence`) so the next
    /// `GetOverlayWindow` reallocates fresh storage at the same xid.
    ///
    /// Returns `Ok(false)` when nothing was materialized — defensive
    /// only; core does not call this without a claim.
    pub(in crate::kms::render::backend) fn backend_overlay_release_overlay_window(
        &mut self,
        _origin: Option<OriginContext>,
    ) -> io::Result<bool> {
        if self.cow_id.is_none() {
            return Ok(false);
        }
        if P::defer_cow_release(self)? {
            return Ok(true);
        }
        self.finish_cow_release();
        Ok(true)
    }

    pub(in crate::kms::render::backend) fn backend_overlay_cow_host_xid(&self) -> Option<u32> {
        // The COW's host xid is the well-known protocol xid once
        // get_overlay_window has materialized; None otherwise.
        if self.cow_id.is_some() {
            Some(yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0)
        } else {
            None
        }
    }
}
