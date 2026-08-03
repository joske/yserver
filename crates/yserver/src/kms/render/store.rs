//! `DrawableStore` — drawable storage + lifetime + damage.
//!
//! Per rendering-model-v2 spec § "DrawableStore — drawable storage +
//! lifetime" and Stage 2 plan substage 2b. Owns every drawable's
//! storage handle, refcount, retirement-generation against I6a
//! [`FenceTicket`]s, image-layout state, and the **two damage lists**
//! per I5 (presentation damage with snapshot/ack semantics + protocol
//! damage for the DAMAGE extension).
//!
//! Stage 2b lands the structure + tests. KmsBackend wires a
//! handful of allocation paths through; full wiring (every
//! allocation method on the Backend trait) arrives across
//! Stages 2c–2d as those substages need the metadata side of
//! the drawables they paint into.
//!
//! Storage allocation is **split** from the metadata layer:
//! `PlatformBackend` creates the Vk handles ([`Storage`]) and
//! hands them to `DrawableStore::allocate`. This keeps the
//! store's allocation path uniform across production (real
//! VkContext) and tests (synthesised null handles) — both go
//! through the same metadata bookkeeping.

#![allow(
    dead_code,
    reason = "DrawableStore primitives are consumed by Stages 2c–2e"
)]

use std::{
    collections::{HashMap, HashSet},
    os::fd::OwnedFd,
    sync::Arc,
};

use ash::vk;

use super::platform::{FenceTicket, PlatformBackend};

// ────────────────────────────────────────────────────────────────
// Identity + classification
// ────────────────────────────────────────────────────────────────

/// Opaque per-drawable handle. Stable across resize (the
/// `vk::Image` may change but the id doesn't).
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub(crate) struct DrawableId(u64);

impl DrawableId {
    /// Raw value for diagnostic logging (`YSERVER_SUBMIT_TRACE`).
    /// Do not use to build new ids — that's [`DrawableStore`]'s
    /// job.
    #[must_use]
    pub(crate) fn as_u64(self) -> u64 {
        self.0
    }

    /// Test-only constructor. Production callers must allocate via
    /// `DrawableStore::allocate(...)` so the store's bookkeeping stays
    /// consistent.
    #[cfg(test)]
    pub(crate) fn for_tests(raw: u64) -> Self {
        Self(raw)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DrawableKind {
    /// Single root storage covering the full virtual-screen
    /// extent. Always scene-participating.
    Root,
    /// One per InputOutput window (after `map_subwindow`).
    /// Scene-participation toggles with map state.
    Window,
    /// One per X11 Pixmap. Never scene-participating.
    Pixmap,
    /// One per server-allocated cursor (Cursor / GlyphCursor /
    /// RenderCreateCursor). Scene-participating when active.
    Cursor,
    /// COMPOSITE `NameWindowPixmap` / `AllocateRedirectedBacking`
    /// target. Per I4, never scene-participating in v2 Stage 2
    /// (Stage 4 connects them to the visible scene path).
    RedirectedBacking,
    // COW deferred to Stage 4.
}

/// Immutable dma-buf layout supplied by the DRI3 client at import time.
/// Vulkan consumes this information while importing the image, but retaining
/// it here lets later Present/scanout policy prove KMS compatibility without
/// querying or mutating the buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ImportedDmabufMetadata {
    pub(crate) fourcc: u32,
    pub(crate) vk_format: vk::Format,
    pub(crate) modifier: u64,
    pub(crate) planes: Vec<ImportedDmabufPlane>,
    pub(crate) width: u16,
    pub(crate) height: u16,
    pub(crate) depth: u8,
    pub(crate) bpp: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ImportedDmabufPlane {
    pub(crate) offset: u64,
    pub(crate) pitch: u32,
}

// ────────────────────────────────────────────────────────────────
// Storage handles — the Vk side of a drawable.
// PlatformBackend creates these; DrawableStore borrows them.
// ────────────────────────────────────────────────────────────────

/// The Vk resources backing one drawable. Created via
/// [`PlatformBackend::allocate_drawable_storage`] (production)
/// or [`Storage::for_tests_null`] (unit tests that exercise
/// metadata bookkeeping without touching Vk).
pub(crate) struct Storage {
    pub(crate) image: vk::Image,
    pub(crate) memory: vk::DeviceMemory,
    /// IDENTITY-swizzle view. Used as a colour attachment
    /// (VUID-VkFramebufferCreateInfo-pAttachments-00891 requires
    /// IDENTITY on attachment views) and as the default sample
    /// source inside the engine's view cache for cases where the
    /// format-aware swizzle would be IDENTITY anyway. The scene
    /// compositor MUST NOT bind this view as a sampler input — see
    /// `sample_view`.
    pub(crate) image_view: vk::ImageView,
    /// Format-and-depth-aware view used by the scene compositor for
    /// sampling. For BGRA8 storage of an X11 depth-24 drawable the
    /// swizzle pins `α=ONE`, matching the X11 RENDER `PictFormat`
    /// `alpha_mask=0` invariant (depth-24 / xRGB samples must read
    /// α=1.0). For BGRA8 depth-32 the swizzle is identity. For R8
    /// storage the swizzle reads as alpha-only (R→a, rest=0). Always
    /// a distinct `VkImageView` from `image_view`; both views alias
    /// the same `image` so paint writes through `image_view` are
    /// readable through `sample_view`.
    pub(crate) sample_view: vk::ImageView,
    pub(crate) extent: vk::Extent2D,
    pub(crate) format: vk::Format,
    /// X11 drawable depth (1/8/24/32). Recorded so `Storage::destroy`
    /// can reason about format/depth-specific cleanup and so test
    /// helpers can introspect without re-deriving from format alone
    /// (BGRA8 covers both 24 and 32).
    pub(crate) depth: u8,
    /// Current layout tracked outside the Vk driver — see
    /// [`Drawable::record_layout_transition`] for the central
    /// mutation point. Single source of truth for what the
    /// next op's barrier `srcLayout` should be.
    pub(crate) current_layout: vk::ImageLayout,
    /// Set when the storage was made via `for_tests_null`. The
    /// `Drop` path skips destroying null handles.
    pub(crate) is_test_stub: bool,
    /// When `Some`, the Vk handles in `image`/`memory`/`image_view`
    /// above are **borrowed from** this `DrawableImage` (the DRI3
    /// import path — Stage 4d / Phase 4.2 §3.2). `Storage::destroy`
    /// skips its own destroy path in this case; the inner
    /// `DrawableImage`'s own `Drop` releases the handles + the
    /// imported dma-buf fd. Pool-return is also skipped because
    /// imported memory isn't pool-eligible. `sample_view` is still
    /// owned by `Storage` (we build it fresh against the borrowed
    /// image) and is destroyed explicitly in `Storage::destroy`.
    pub(crate) imported_drawable: Option<crate::kms::vk::target::DrawableImage>,
    pub(crate) imported_dmabuf: Option<ImportedDmabufMetadata>,
    /// GLX-TFP (Task 1.2): set once this server-owned storage has been
    /// migrated onto dma-buf-exportable memory via
    /// [`Self::adopt_exportable`]. Distinct from
    /// `imported_drawable.is_some()` (which is the DRI3-import case):
    /// a promoted image owns server-allocated, export-capable memory
    /// outright. A promoted image MUST NOT be returned to the
    /// `PixmapPool` (pool images are OPTIMAL/non-exportable) — see the
    /// `!self.promoted_exportable` guard in [`Self::destroy`].
    pub(crate) promoted_exportable: bool,
    /// GLX-TFP (Task 1.2): row stride (bytes) of the exportable image,
    /// captured from `vkGetImageSubresourceLayout` at allocation time
    /// so the DRI3 export reply (Task 1.3) never has to re-query it.
    /// `0` for non-promoted storage.
    pub(crate) export_stride: u32,
    /// GLX-TFP (Task 1.2): total memory size (bytes) of the exportable
    /// image, captured at allocation time. `0` for non-promoted
    /// storage.
    pub(crate) export_size: u64,
    /// GLX-TFP (Phase 1): DRM format modifier of the exportable image,
    /// read back from `vkGetImageDrmFormatModifierPropertiesEXT` at
    /// promotion time. `DRM_FORMAT_MOD_LINEAR` (0) for the LINEAR path or
    /// non-promoted storage. Carried into the `BuffersFromPixmap` (op 8)
    /// reply — a modifier-tiled buffer is unusable by the client without it.
    pub(crate) export_modifier: u64,
}

/// Old Vk handles displaced by promotion ([`Storage::adopt_exportable`]).
/// Destroyed by the engine only once the fence guarding the old image's
/// last render has signaled.
pub(crate) struct RetiredImage {
    pub image: vk::Image,
    pub memory: vk::DeviceMemory,
    pub image_view: vk::ImageView,
    pub sample_view: vk::ImageView,
}

impl Storage {
    /// Production constructor — Vk handles owned by `PlatformBackend::
    /// allocate_drawable_storage`. Initial layout is `UNDEFINED`;
    /// transitions tracked thereafter via
    /// [`Drawable::record_layout_transition`].
    pub(crate) fn new_server_owned(
        image: vk::Image,
        memory: vk::DeviceMemory,
        image_view: vk::ImageView,
        sample_view: vk::ImageView,
        extent: vk::Extent2D,
        format: vk::Format,
        depth: u8,
    ) -> Self {
        Self {
            image,
            memory,
            image_view,
            sample_view,
            extent,
            format,
            depth,
            current_layout: vk::ImageLayout::UNDEFINED,
            is_test_stub: false,
            imported_drawable: None,
            imported_dmabuf: None,
            promoted_exportable: false,
            export_stride: 0,
            export_size: 0,
            export_modifier: 0,
        }
    }

    /// Wrap a DRI3-imported [`DrawableImage`](crate::kms::vk::target::DrawableImage)
    /// as a v2 `Storage`. The handles in `image`/`memory`/`image_view`
    /// alias the inner `DrawableImage`; the inner `Drop` owns the
    /// release of those handles + the imported dma-buf fd.
    /// `current_layout` starts at `UNDEFINED` (`DrawableImage`'s
    /// `from_dmabuf` likewise leaves the image undefined until the
    /// first paint barrier). Used by `dri3_import_pixmap`.
    pub(crate) fn from_imported_drawable_image(
        drawable: crate::kms::vk::target::DrawableImage,
        sample_view: vk::ImageView,
        depth: u8,
        imported_dmabuf: ImportedDmabufMetadata,
    ) -> Self {
        let image = drawable.vk_image;
        let image_view = drawable.vk_image_view;
        let memory = drawable.backing_memory();
        let extent = drawable.extent;
        let format = drawable.format;
        Self {
            image,
            memory,
            image_view,
            sample_view,
            extent,
            format,
            depth,
            current_layout: vk::ImageLayout::UNDEFINED,
            is_test_stub: false,
            imported_drawable: Some(drawable),
            imported_dmabuf: Some(imported_dmabuf),
            promoted_exportable: false,
            export_stride: 0,
            export_size: 0,
            export_modifier: 0,
        }
    }

    /// Stage 3f.10: pool-take constructor. Reuses a recycled
    /// `PooledPixmapImage` triple (image + memory + view) +
    /// inherits the pool entry's tracked layout so subsequent
    /// ops transition from the right source state.
    pub(crate) fn from_pooled(
        pooled: crate::kms::vk::pixmap_pool::PooledPixmapImage,
        sample_view: vk::ImageView,
        extent: vk::Extent2D,
        format: vk::Format,
        depth: u8,
    ) -> Self {
        Self {
            image: pooled.image,
            memory: pooled.memory,
            image_view: pooled.view,
            sample_view,
            extent,
            format,
            depth,
            current_layout: pooled.current_layout,
            is_test_stub: false,
            imported_drawable: None,
            imported_dmabuf: None,
            promoted_exportable: false,
            export_stride: 0,
            export_size: 0,
            export_modifier: 0,
        }
    }

    /// Test-only constructor with null Vk handles. Used by
    /// unit tests that exercise refcount / damage / snapshot
    /// logic without needing a live VkContext.
    #[doc(hidden)]
    pub(crate) fn for_tests_null(extent: vk::Extent2D, format: vk::Format) -> Self {
        // Match the production depth→format pairing so tests that
        // inspect `Storage::depth` see a sensible default — but
        // never build real Vk views (this is the null-stub path).
        let depth = match format {
            vk::Format::R8_UNORM => 8,
            _ => 32,
        };
        Self {
            image: vk::Image::null(),
            memory: vk::DeviceMemory::null(),
            image_view: vk::ImageView::null(),
            sample_view: vk::ImageView::null(),
            depth,
            extent,
            format,
            current_layout: vk::ImageLayout::UNDEFINED,
            is_test_stub: true,
            imported_drawable: None,
            imported_dmabuf: None,
            promoted_exportable: false,
            export_stride: 0,
            export_size: 0,
            export_modifier: 0,
        }
    }

    /// True when this storage's memory is already dma-buf-exportable
    /// (DRI3-imported, or previously promoted). Used to avoid
    /// re-promoting an already-exportable drawable.
    pub(crate) fn is_exportable(&self) -> bool {
        self.imported_drawable.is_some() || self.promoted_exportable
    }

    /// Adopt a freshly-allocated exportable image as this drawable's
    /// permanent backing. Caller MUST have copied old→new content and
    /// invalidated the view cache, and MUST destroy the returned old
    /// handles only after the old image's last render fence has
    /// signaled (see `RenderEngine::retire_image_after`).
    ///
    /// `export_stride`/`export_size` come from the exportable image's
    /// `vkGetImageSubresourceLayout` at allocation time and are stored
    /// so the DRI3 export reply (Task 1.3) never has to re-query them.
    pub(crate) fn adopt_exportable(
        &mut self,
        new_image: vk::Image,
        new_memory: vk::DeviceMemory,
        new_sample_view: vk::ImageView,
        new_image_view: vk::ImageView,
        new_layout: vk::ImageLayout,
        export_stride: u32,
        export_size: u64,
        export_modifier: u64,
    ) -> RetiredImage {
        let retired = RetiredImage {
            image: self.image,
            memory: self.memory,
            image_view: self.image_view,
            sample_view: self.sample_view,
        };
        self.image = new_image;
        self.memory = new_memory;
        self.image_view = new_image_view;
        self.sample_view = new_sample_view;
        self.current_layout = new_layout;
        self.promoted_exportable = true;
        self.export_stride = export_stride;
        self.export_size = export_size;
        self.export_modifier = export_modifier;
        retired
    }

    /// Destroy the underlying Vk handles. Called by
    /// `DrawableStore` after the last consumer has released
    /// the drawable AND its `FenceTicket` is signaled. No-op
    /// for test stubs.
    ///
    /// Stage 3f.10: try the pixmap pool first. Eligible-size
    /// entries (`PixmapPool::eligible(key)` — typically ≤128×128)
    /// are returned for recycle; ineligible or pool-full entries
    /// fall through to synchronous destroy.
    fn destroy(&mut self, platform: &PlatformBackend) {
        if self.is_test_stub {
            return;
        }
        // DRI3-imported storage: the DrawableImage owns its own Vk
        // handles + dma-buf fd; its Drop releases the
        // image/view/memory borrowed below. But `sample_view` was
        // built by us against the borrowed image — we own it and
        // must destroy it explicitly before the inner Drop fires.
        if self.imported_drawable.is_some() {
            if let Some(vk) = platform.vk.as_ref()
                && self.sample_view != vk::ImageView::null()
            {
                unsafe { vk.device.destroy_image_view(self.sample_view, None) };
            }
            self.sample_view = vk::ImageView::null();
            // Null out the aliasing handles before the inner Drop
            // runs to avoid any chance of pool-return or other
            // observers seeing live handles after the underlying
            // memory has been freed.
            self.image = vk::Image::null();
            self.image_view = vk::ImageView::null();
            self.memory = vk::DeviceMemory::null();
            // Dropping `self.imported_drawable` triggers
            // DrawableImage::drop which destroys the real handles.
            self.imported_drawable = None;
            return;
        }
        let Some(vk) = platform.vk.as_ref() else {
            // No VkContext — happens only in malformed test
            // fixtures. Log + leak.
            log::error!(
                "Storage::destroy: no VkContext available for image {:?}",
                self.image,
            );
            return;
        };
        // Destroy sample_view first. It's always owned by Storage
        // (the pool stores only the attachment-side view) and its
        // swizzle is depth-specific, so we never recycle it — a
        // pool-take rebuilds a fresh sample_view for whatever depth
        // the new allocation requests.
        if self.sample_view != vk::ImageView::null() {
            unsafe { vk.device.destroy_image_view(self.sample_view, None) };
            self.sample_view = vk::ImageView::null();
        }
        // Pool-return path: only attempt for non-null handles and
        // when the platform has a pool wired (production path).
        // Promoted (exportable) images are NEVER pool-eligible — the
        // pool only holds OPTIMAL-tiled, non-exportable images; a
        // LINEAR exportable image returned here would corrupt the
        // pool's size/format buckets and silently hand out
        // export-capable memory to ordinary pixmaps.
        if !self.promoted_exportable
            && self.image != vk::Image::null()
            && self.image_view != vk::ImageView::null()
            && self.memory != vk::DeviceMemory::null()
            && let Some(pool) = platform.pixmap_pool.as_ref()
        {
            let key = crate::kms::vk::pixmap_pool::PixmapPoolKey {
                width: self.extent.width,
                height: self.extent.height,
                format: self.format,
            };
            let entry = crate::kms::vk::pixmap_pool::PooledPixmapImage {
                image: self.image,
                view: self.image_view,
                memory: self.memory,
                current_layout: self.current_layout,
            };
            match pool.try_return(key, entry) {
                Ok(()) => {
                    // Pool adopted the handles. Null them so the
                    // fallthrough destroy below skips this entry.
                    self.image = vk::Image::null();
                    self.image_view = vk::ImageView::null();
                    self.memory = vk::DeviceMemory::null();
                    return;
                }
                Err(returned) => {
                    // Bucket full / ineligible — restore handles
                    // from the entry that was rejected (try_return
                    // gives them back) and fall through to destroy.
                    self.image = returned.image;
                    self.image_view = returned.view;
                    self.memory = returned.memory;
                }
            }
        }
        unsafe {
            if self.image_view != vk::ImageView::null() {
                vk.device.destroy_image_view(self.image_view, None);
            }
            if self.image != vk::Image::null() {
                vk.device.destroy_image(self.image, None);
            }
            if self.memory != vk::DeviceMemory::null() {
                vk.device.free_memory(self.memory, None);
            }
        }
    }
}

// ────────────────────────────────────────────────────────────────
// RegionSet — minimal Vec<Rect2D> with union / subtract.
//
// Stage 2 regions are typically <= a handful of rects per
// drawable, so a Vec-backed set is fast enough. Full pixman-
// style region algebra arrives later if profiling shows it.
// ────────────────────────────────────────────────────────────────

#[derive(Debug, Default, Clone)]
pub(crate) struct RegionSet {
    rects: Vec<vk::Rect2D>,
}

impl RegionSet {
    pub(crate) fn new() -> Self {
        Self { rects: Vec::new() }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.rects.is_empty()
    }

    pub(crate) fn rects(&self) -> &[vk::Rect2D] {
        &self.rects
    }

    /// Cap on the number of rects a `RegionSet` retains. Past this the
    /// set collapses to its bounding rect (a safe superset — worst case
    /// is over-paint, never lost damage). Without a cap, a client that
    /// damages faster than page-flips drain the region (Window Maker's
    /// restack/draw storm) grows the set unbounded, and the O(n·m)
    /// [`RegionSet::subtract`] in the page-flip-complete handler then
    /// wedges the single-threaded core loop — the machine stops
    /// responding to input, ZAP included. Capping both operands keeps
    /// `subtract` at O(MAX_RECTS²) ≈ constant.
    const MAX_RECTS: usize = 256;

    /// Add a rect, coalescing to the bounding rect once the set exceeds
    /// [`Self::MAX_RECTS`] so the region stays bounded (see the const's
    /// note for why this is load-bearing, not just an optimization).
    pub(crate) fn add(&mut self, rect: vk::Rect2D) {
        if rect.extent.width == 0 || rect.extent.height == 0 {
            return;
        }
        self.rects.push(rect);
        if self.rects.len() > Self::MAX_RECTS
            && let Some(bounds) = self.bounding_rect()
        {
            self.rects.clear();
            self.rects.push(bounds);
        }
    }

    /// Union with another set. O(n) — for Stage 2's small
    /// region counts this is fine.
    pub(crate) fn union_with(&mut self, other: &RegionSet) {
        for &r in &other.rects {
            self.add(r);
        }
    }

    /// Subtract `other` from `self` rect-by-rect. The
    /// implementation is conservative (treats overlap as "kept")
    /// — only exact-match rects are removed. Stage 2's
    /// snapshot/ack flows always pass back a slice of `self`
    /// so this is sufficient. Full region-difference algebra
    /// lands later.
    pub(crate) fn subtract(&mut self, other: &RegionSet) {
        if other.rects.is_empty() {
            return;
        }
        self.rects.retain(|r| {
            !other
                .rects
                .iter()
                .any(|o| o.offset == r.offset && o.extent == r.extent)
        });
    }

    pub(crate) fn clear(&mut self) {
        self.rects.clear();
    }

    /// Bounding rect over every rect in the set. Returns `None`
    /// if the set is empty. Stage 2e uses this for the
    /// buffer-age repaint scissor — we don't yet split the
    /// scissor per-rect; Stage 5 may tighten if profiling shows
    /// over-paint matters.
    pub(crate) fn bounding_rect(&self) -> Option<vk::Rect2D> {
        let mut iter = self.rects.iter().copied();
        let first = iter.next()?;
        let mut x0 = first.offset.x;
        let mut y0 = first.offset.y;
        let mut x1 = first.offset.x.saturating_add_unsigned(first.extent.width);
        let mut y1 = first.offset.y.saturating_add_unsigned(first.extent.height);
        for r in iter {
            x0 = x0.min(r.offset.x);
            y0 = y0.min(r.offset.y);
            x1 = x1.max(r.offset.x.saturating_add_unsigned(r.extent.width));
            y1 = y1.max(r.offset.y.saturating_add_unsigned(r.extent.height));
        }
        Some(vk::Rect2D {
            offset: vk::Offset2D { x: x0, y: y0 },
            extent: vk::Extent2D {
                width: u32::try_from((x1 - x0).max(0)).unwrap_or(0),
                height: u32::try_from((y1 - y0).max(0)).unwrap_or(0),
            },
        })
    }

    /// Clone — used by snapshot/ack capture paths. The
    /// `derive(Clone)` already covers this; this method is just
    /// a name for readability at call sites.
    #[must_use]
    pub(crate) fn snapshot(&self) -> RegionSet {
        self.clone()
    }
}

// ────────────────────────────────────────────────────────────────
// DamageSnapshot — peek/ack token for the I5 snapshot/ack rule.
// ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub(crate) struct DamageSnapshot {
    pub(crate) id: DrawableId,
    pub(crate) epoch: u64,
    pub(crate) region: RegionSet,
}

// ────────────────────────────────────────────────────────────────
// Drawable — one entry in DrawableStore.
// ────────────────────────────────────────────────────────────────

pub(crate) struct Drawable {
    pub(crate) id: DrawableId,
    pub(crate) xid: u32,
    pub(crate) kind: DrawableKind,
    pub(crate) depth: u8,
    pub(crate) refcount: u32,
    pub(crate) scene_participating: bool,
    pub(crate) storage: Storage,

    /// I6a: latest render-completion ticket for which this
    /// drawable was a consumer (read or written) in flight. None
    /// = no GPU work has touched it since the last retirement.
    /// Coalesces — overwritten by the newest touch. Per cross-
    /// cutting §5 the underlying Arc keeps prior consumers
    /// alive via their own clones.
    pub(crate) last_render_ticket: Option<FenceTicket>,

    /// Presentation damage — region the scene needs to re-blit.
    /// Accumulates only when `scene_participating` is true.
    /// Drained via [`peek_presentation_damage`] +
    /// [`ack_presentation_damage`].
    pub(crate) presentation_damage: RegionSet,
    pub(crate) presentation_damage_epoch: u64,

    /// Idle free-run fix (cut 2b): set when this drawable is
    /// scene-participating and holds presentation damage but was NOT
    /// drawn by `build_scene` on ANY output last tick — i.e. it clips
    /// to an empty visible box (mapped but off-screen / empty bounding
    /// shape). Such damage can never be composed/ack'd, so it must not
    /// keep arming the compose scheduler (`has_pending_presentation_
    /// damage`) — that busy-spins the core loop at idle. The damage
    /// itself is PRESERVED (not cleared): when a structural change
    /// (map/move/restack/configure/shape/RandR — all mark
    /// `scene_structure_dirty`) brings the drawable on-screen, the
    /// forced full recompose draws it from storage and the ack drains
    /// it, clearing this flag. Reconciled only when every output walked
    /// this tick (see `SceneCompositor::tick`).
    pub(crate) offscreen_no_draw: bool,

    /// Ungated monotonic content-write counter. Bumped (saturating) on EVERY
    /// write to this drawable's pixels — the eight engine paint entry points —
    /// regardless of `scene_participating`, unlike `presentation_damage_epoch`
    /// which is gated and so misses offscreen clip-mask writes. The clip-mask
    /// cache compares this to detect a genuine mask mutation vs a cheap
    /// clip-install re-toggle. `saturating_add` matters only at `u64::MAX`
    /// (unreachable in practice): it freezes rather than wrapping, which avoids
    /// aliasing a fresh version onto an old cached one.
    pub(crate) content_version: u64,

    /// Stage 4a — COMPOSITE redirect routing. When `Some(B_id)`,
    /// paint that resolves through this drawable's xid lands in
    /// `B_id` instead. Pure storage-side state; side effects on
    /// damage / refcount / `scene_participating` are the caller's
    /// responsibility (4c sets those via the dedicated Backend
    /// methods). Default `None` — no redirect.
    pub(crate) redirected_target: Option<DrawableId>,
}

impl Drawable {
    /// Record an image-layout transition on `cb` with full
    /// producer/consumer access masks. Updates
    /// `storage.current_layout` so subsequent ops see the
    /// correct old-layout in their barrier.
    ///
    /// **Single source of truth** for what the current layout
    /// is. Reading or writing `current_layout` outside this
    /// method is a layered correctness hazard — see Risk 11
    /// in the Stage 2 plan.
    pub(crate) fn record_layout_transition(
        &mut self,
        vk: &crate::kms::vk::device::VkContext,
        cb: vk::CommandBuffer,
        target_layout: vk::ImageLayout,
        src_stage: vk::PipelineStageFlags2,
        src_access: vk::AccessFlags2,
        dst_stage: vk::PipelineStageFlags2,
        dst_access: vk::AccessFlags2,
    ) {
        if self.storage.is_test_stub {
            // Tests don't issue real Vk; just update the
            // tracker so logic-side assertions can verify.
            self.storage.current_layout = target_layout;
            return;
        }
        let barrier = vk::ImageMemoryBarrier2::default()
            .src_stage_mask(src_stage)
            .src_access_mask(src_access)
            .dst_stage_mask(dst_stage)
            .dst_access_mask(dst_access)
            .old_layout(self.storage.current_layout)
            .new_layout(target_layout)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(self.storage.image)
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .level_count(1)
                    .layer_count(1),
            );
        let dep =
            vk::DependencyInfo::default().image_memory_barriers(std::slice::from_ref(&barrier));
        unsafe { vk.device.cmd_pipeline_barrier2(cb, &dep) };
        self.storage.current_layout = target_layout;
    }
}

// ────────────────────────────────────────────────────────────────
// Errors + retirement decision
// ────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub(crate) enum AllocError {
    /// Two allocations collided on the same xid; caller is
    /// responsible for picking a fresh one (`KmsCore::next_host_xid`).
    XidInUse,
    /// Caller passed an unsupported `(depth, format)` combo.
    UnsupportedFormat,
    /// VkContext / pool allocation failure.
    Vk(vk::Result),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RetireDecision {
    /// Refcount > 0 after decref; storage stays.
    StillReferenced,
    /// Refcount hit zero AND no fence ticket attached (or it
    /// was already signaled). Vk handles destroyed; entry
    /// removed from the map.
    Destroyed,
    /// Refcount hit zero but a fence ticket is still
    /// unsignaled. Entry parked in `pending_retire`; future
    /// `poll_pending_retire` calls will sweep it once the
    /// fence fires.
    PendingFence,
}

// ────────────────────────────────────────────────────────────────
// DrawableStore — the map + accessors.
// ────────────────────────────────────────────────────────────────

pub(crate) struct DrawableStore {
    next_id: u64,
    entries: HashMap<DrawableId, Drawable>,
    by_xid: HashMap<u32, DrawableId>,
    /// Drawables that hit refcount-zero but whose ticket isn't
    /// signaled yet. `poll_pending_retire` drains.
    pending_retire: Vec<DrawableId>,
    /// GLX-TFP (Task 2.3): dma-buf fds for drawables that have been
    /// exported to a GL consumer, indexed by `DrawableId`. The backend
    /// (`KmsBackend::exported_dmabufs`) owns the canonical lifetime
    /// record; this map is a parallel sync-only dup the backend keeps in
    /// lockstep so the engine's flush chokepoint (which sees `store` +
    /// `platform` but NOT the backend) can resolve fds for bidirectional
    /// implicit sync. `Arc` so the engine can cheaply collect borrows
    /// across the `&mut self` flush.
    exported_sync: HashMap<DrawableId, Arc<OwnedFd>>,
    /// GLX-TFP (Task 2.3): exported `DrawableId`s written since the last
    /// flush. `touch_render_fence` pushes here when the stamped id is in
    /// `exported_sync`; `take_exported_writes` drains it at flush.
    exported_writes: Vec<DrawableId>,
    /// Exported destination whose external-reader fence was already awaited
    /// by the deferred Present gate. Consumed by the next flush containing a
    /// write to that drawable; the flush still publishes its new WRITE fence.
    prewaited_exported_writes: std::collections::HashSet<DrawableId>,
}

impl DrawableStore {
    pub(crate) fn new() -> Self {
        Self {
            next_id: 1,
            entries: HashMap::new(),
            by_xid: HashMap::new(),
            pending_retire: Vec::new(),
            exported_sync: HashMap::new(),
            exported_writes: Vec::new(),
            prewaited_exported_writes: std::collections::HashSet::new(),
        }
    }

    /// GLX-TFP (Task 2.3): register/replace the sync-only dma-buf fd dup
    /// for an exported drawable. Called by the backend when an
    /// `ExportedBacking` first gains its fd.
    pub(crate) fn set_exported_sync_fd(&mut self, id: DrawableId, fd: Arc<OwnedFd>) {
        self.exported_sync.insert(id, fd);
    }

    /// GLX-TFP (Task 2.3): drop the sync-only fd dup for `id`. Called by
    /// the backend at export teardown. Also purges any pending
    /// `exported_writes` entry so a torn-down export can't be waited on.
    pub(crate) fn clear_exported_sync_fd(&mut self, id: DrawableId) {
        self.exported_sync.remove(&id);
        self.exported_writes.retain(|&w| w != id);
        self.prewaited_exported_writes.remove(&id);
    }

    /// GLX-TFP (Task 2.3): true iff `id` currently has a sync-only dma-buf
    /// fd registered (i.e. is live-exported to a GL consumer).
    pub(crate) fn is_exported(&self, id: DrawableId) -> bool {
        self.exported_sync.contains_key(&id)
    }

    pub(crate) fn exported_sync_fd(&self, id: DrawableId) -> Option<Arc<OwnedFd>> {
        self.exported_sync.get(&id).cloned()
    }

    /// GLX-TFP (Task 2.3): drain the exported-writes accumulator and
    /// resolve each id to its sync fd `Arc`, deduped. Returned at the
    /// flush chokepoint so the platform can wait/publish around the
    /// `vkQueueSubmit2`. Entries whose fd was cleared between stamp and
    /// flush are skipped.
    pub(crate) fn take_exported_writes(&mut self) -> Vec<(Arc<OwnedFd>, bool)> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for id in self.exported_writes.drain(..) {
            if seen.insert(id)
                && let Some(fd) = self.exported_sync.get(&id)
            {
                out.push((Arc::clone(fd), self.prewaited_exported_writes.remove(&id)));
            }
        }
        out
    }

    /// Authorize one exported write to bypass the queue-level old-reader
    /// wait. Refuse when an earlier write to the same backing is already
    /// pending in the current submit group: that older write was not covered
    /// by the Present gate's fence snapshot.
    pub(crate) fn begin_prewaited_exported_write(&mut self, id: DrawableId) {
        if self.exported_sync.contains_key(&id) && !self.exported_writes.contains(&id) {
            self.prewaited_exported_writes.insert(id);
        }
    }

    /// Revoke an unused authorization (for example, a fully clipped Present).
    pub(crate) fn end_prewaited_exported_write(&mut self, id: DrawableId) {
        if !self.exported_writes.contains(&id) {
            self.prewaited_exported_writes.remove(&id);
        }
    }

    /// Allocate a fresh drawable. The caller has already built
    /// the `Storage` (via PlatformBackend in production, or
    /// `Storage::for_tests_null` in unit tests). Refcount
    /// starts at 1; layout is `UNDEFINED`; both damage lists
    /// empty.
    ///
    /// # Errors
    ///
    /// - `XidInUse` if `xid` already maps to a drawable.
    pub(crate) fn allocate(
        &mut self,
        xid: u32,
        kind: DrawableKind,
        depth: u8,
        scene_participating: bool,
        storage: Storage,
    ) -> Result<DrawableId, AllocError> {
        if self.by_xid.contains_key(&xid) {
            return Err(AllocError::XidInUse);
        }
        let id = DrawableId(self.next_id);
        self.next_id = self.next_id.checked_add(1).expect("DrawableId overflow");
        let drawable = Drawable {
            id,
            xid,
            kind,
            depth,
            refcount: 1,
            scene_participating,
            storage,
            last_render_ticket: None,
            presentation_damage: RegionSet::new(),
            presentation_damage_epoch: 0,
            offscreen_no_draw: false,
            content_version: 0,
            redirected_target: None,
        };
        self.entries.insert(id, drawable);
        self.by_xid.insert(xid, id);
        Ok(id)
    }

    pub(crate) fn lookup(&self, xid: u32) -> Option<DrawableId> {
        self.by_xid.get(&xid).copied()
    }

    /// Diagnostic-only: iterate every `(host_xid, DrawableId)` pair
    /// currently registered. Used by the Ctrl-Alt-F12 drawables dump to
    /// sweep pixmaps that aren't reachable through `windows` /
    /// backings (e.g. e16's menu-item background pixmaps).
    pub(crate) fn xid_entries(&self) -> impl Iterator<Item = (u32, DrawableId)> + '_ {
        self.by_xid.iter().map(|(&xid, &id)| (xid, id))
    }

    /// Shutdown-only: destroy every remaining drawable's Vk
    /// storage. The runtime release path (`destroy_now`) walks
    /// from `decref` → `poll_pending_retire`, so any drawable
    /// still in `entries` at shutdown was alive at SIGTERM (e.g.
    /// MATE's resident pixmaps when the session quit). Without
    /// this call the entries are dropped silently and their
    /// `VkImage` / `VkImageView` / `VkDeviceMemory` handles leak
    /// to `vkDestroyDevice`'s `has N leaked objects` warning
    /// (948 VkDeviceMemory observed on bee/MATE 2026-05-31 after
    /// the FenceTicket fix). Idempotent: `Storage::destroy`
    /// nulls handles after pool-return and the direct-destroy
    /// path is null-guarded throughout, so re-calling is safe.
    /// Caller is `KmsBackend::shutdown_destroy_drawables` from
    /// `lib.rs`'s explicit shutdown block.
    pub(crate) fn shutdown_destroy_all(&mut self, platform: &PlatformBackend) {
        self.by_xid.clear();
        self.pending_retire.clear();
        for (_, mut drawable) in self.entries.drain() {
            drawable.storage.destroy(platform);
        }
    }

    pub(crate) fn get(&self, id: DrawableId) -> Option<&Drawable> {
        self.entries.get(&id)
    }

    pub(crate) fn get_mut(&mut self, id: DrawableId) -> Option<&mut Drawable> {
        self.entries.get_mut(&id)
    }

    pub(crate) fn get_by_xid(&self, xid: u32) -> Option<&Drawable> {
        self.lookup(xid).and_then(|id| self.get(id))
    }

    pub(crate) fn get_by_xid_mut(&mut self, xid: u32) -> Option<&mut Drawable> {
        let id = self.lookup(xid)?;
        self.entries.get_mut(&id)
    }

    pub(crate) fn incref(&mut self, id: DrawableId) {
        if let Some(d) = self.entries.get_mut(&id) {
            d.refcount = d.refcount.saturating_add(1);
        }
    }

    /// Detach the xid → DrawableId mapping for `xid`, without
    /// touching the drawable's refcount. The drawable stays alive
    /// in `entries` for any holders that captured the id (Pictures,
    /// in-flight compose ops). Used by `configure_subwindow`'s
    /// resize path: the window's storage is being replaced with a
    /// fresh allocation, so the xid map needs to retarget, but
    /// existing Picture refcounts on the old storage must not be
    /// dropped (the picture's next `store.lookup(xid)` will return
    /// the new id — which is what the caller installs next).
    ///
    /// Idempotent: missing mappings are silently ignored.
    pub(crate) fn detach_xid(&mut self, xid: u32) {
        self.by_xid.remove(&xid);
    }

    /// Drop one reference. If refcount hits zero, decide
    /// retirement: synchronous-destroy if no fence is
    /// pending; otherwise park in `pending_retire`.
    ///
    /// **xid detachment on PendingFence**: when the storage parks
    /// because of an in-flight GPU ticket, the `by_xid` mapping is
    /// removed immediately. The drawable stays alive in `entries`
    /// (and `pending_retire`) until the ticket signals, but the
    /// xid is now free for re-allocation — needed by, e.g.,
    /// `configure_subwindow`'s resize path which calls
    /// `decref` then `allocate(same_xid, …)`. Without this
    /// detachment the re-allocate would fail with `XidInUse` and
    /// the caller would silently keep the old (now-orphaned) storage.
    /// Id-based access stays valid (any in-flight op captured the id
    /// before this point), so this only affects xid-based lookups.
    pub(crate) fn decref<F>(
        &mut self,
        platform: &mut PlatformBackend,
        id: DrawableId,
        on_destroyed: F,
    ) -> RetireDecision
    where
        F: FnOnce(DrawableId),
    {
        let Some(drawable) = self.entries.get_mut(&id) else {
            return RetireDecision::Destroyed;
        };
        if drawable.refcount > 1 {
            drawable.refcount -= 1;
            return RetireDecision::StillReferenced;
        }
        drawable.refcount = 0;
        let ticket_ready = match drawable.last_render_ticket.as_ref() {
            None => true,
            Some(t) => match platform.vk.as_ref() {
                Some(vk) => t.poll_signaled(vk),
                None => true, // no Vk (tests) — treat as signaled
            },
        };
        if ticket_ready {
            // Engine-cache invalidation must fire BEFORE
            // `destroy_now` so cached `VkImageView`s are
            // destroyed while their underlying `VkImage` is still
            // alive (the Vulkan-spec-clean ordering — view
            // destruction is technically independent of the image
            // but VUID-best-practice + most validation layers
            // expect view-first). Caller threads the closure
            // through to bridge `&mut DrawableStore` with
            // `&mut RenderEngine` (disjoint sibling fields on
            // `KmsBackend`).
            on_destroyed(id);
            self.destroy_now(platform, id);
            RetireDecision::Destroyed
        } else {
            // Detach from xid map so the xid is free for re-alloc
            // (configure_subwindow resize). entries[id] persists for
            // pending_retire poll.
            let xid = drawable.xid;
            self.by_xid.remove(&xid);
            self.pending_retire.push(id);
            RetireDecision::PendingFence
        }
    }

    /// Internal: destroy storage and remove from maps.
    ///
    /// Only removes the `by_xid[xid]` mapping if it currently
    /// points to **this** DrawableId. Necessary because
    /// `decref → PendingFence` already detaches the xid map and
    /// the same xid may have been re-allocated (e.g.
    /// `configure_subwindow`'s resize: decref → alloc with same
    /// xid → new DrawableId installed). When the parked old
    /// drawable's fence eventually signals and destroy_now runs,
    /// a blanket `by_xid.remove(xid)` would nuke the NEW
    /// drawable's lookup, "orphaning" the resized window.
    fn destroy_now(&mut self, platform: &mut PlatformBackend, id: DrawableId) {
        let Some(mut drawable) = self.entries.remove(&id) else {
            return;
        };
        if self.by_xid.get(&drawable.xid).copied() == Some(id) {
            self.by_xid.remove(&drawable.xid);
        }
        drawable.storage.destroy(platform);
        // last_render_ticket drops here; its Rc inner refcount
        // ensures the underlying fence handle stays alive until
        // every consumer that cloned it has also dropped.
    }

    /// Flip scene-participation. When set to false, **clears
    /// unpresented presentation damage and bumps the epoch**
    /// (per codex round 1 point 5): an unmap means the scene
    /// shouldn't repaint from this storage; any in-flight
    /// snapshot that ack's against the new epoch will become
    /// a no-op subtract against an empty set. Protocol damage
    /// unaffected.
    pub(crate) fn set_scene_participating(&mut self, id: DrawableId, v: bool) {
        let Some(d) = self.entries.get_mut(&id) else {
            return;
        };
        let was = d.scene_participating;
        d.scene_participating = v;
        if was && !v {
            d.presentation_damage.clear();
            d.presentation_damage_epoch = d.presentation_damage_epoch.checked_add(1).unwrap_or(0);
        }
    }

    /// Accumulate presentation damage. No-op on non-scene-
    /// participating drawables (pixmaps, Manual-redirected
    /// backings). Bumps `presentation_damage_epoch` whenever
    /// damage was actually appended.
    ///
    /// Protocol-side `DamageNotify` fanout is handled by
    /// `yserver-core::core_loop::damage_fanout` at the request
    /// layer (see spec §I5 amendment), independent of this
    /// store.
    pub(crate) fn damage(&mut self, id: DrawableId, rect: vk::Rect2D) {
        let Some(d) = self.entries.get_mut(&id) else {
            return;
        };
        if d.scene_participating {
            d.presentation_damage.add(rect);
            d.presentation_damage_epoch = d.presentation_damage_epoch.checked_add(1).unwrap_or(0);
        }
    }

    /// True if any scene-participating drawable has undrained
    /// presentation damage — i.e. a client painted a visible window
    /// and the SceneCompositor has not composed it yet. `next_wakeup`
    /// / `maybe_composite` consult this so that *content* paint arms a
    /// compose, not only structural map/unmap/restack changes
    /// (`scene_structure_dirty`). Without it, a window painted after
    /// its map-compose already ran is stranded until an unrelated
    /// event pokes the loop (the xfce submenu bug). Cheap: short-
    /// circuits on the first match, no allocation.
    pub(crate) fn has_pending_presentation_damage(&self) -> bool {
        self.entries.values().any(|d| {
            d.scene_participating && !d.presentation_damage.is_empty() && !d.offscreen_no_draw
        })
    }

    /// Idle free-run fix (cut 2b): reconcile the `offscreen_no_draw`
    /// flag from the set of drawables actually drawn (sampled) by
    /// `build_scene` across ALL outputs this tick. A scene-participating
    /// drawable with undrained presentation damage that was NOT drawn on
    /// any output cannot be composed/ack'd right now (it clips to an
    /// empty visible box — off-screen / empty bounding shape), so flag
    /// it OUT of the compose scheduler; one that WAS drawn is cleared.
    /// The damage is never cleared here — see `offscreen_no_draw`. MUST
    /// be called only when every output walked (`build_scene` ran), so a
    /// drawable visible only on a mid-flip output isn't mis-flagged.
    /// `drawn` holds *sampled source* ids (a redirected Automatic
    /// window is sampled via its backing id, not its own).
    pub(crate) fn reconcile_offscreen_no_draw(&mut self, drawn: &HashSet<DrawableId>) {
        for (id, d) in &mut self.entries {
            if !d.scene_participating || d.presentation_damage.is_empty() {
                d.offscreen_no_draw = false;
                continue;
            }
            d.offscreen_no_draw = !drawn.contains(id);
        }
    }

    /// Snapshot for the SceneCompositor to ack later.
    /// Returns `None` if the drawable doesn't exist or isn't
    /// scene-participating (no presentation damage to peek).
    pub(crate) fn peek_presentation_damage(&self, id: DrawableId) -> Option<DamageSnapshot> {
        let d = self.entries.get(&id)?;
        if !d.scene_participating {
            return None;
        }
        Some(DamageSnapshot {
            id,
            epoch: d.presentation_damage_epoch,
            region: d.presentation_damage.clone(),
        })
    }

    /// Ack: if `snap.epoch == current_epoch`, clear live
    /// damage. If `snap.epoch < current_epoch`, paint arrived
    /// between peek and ack — subtract only the snapshot's
    /// region so post-peek damage survives.
    ///
    /// Per codex round 1 point 5: if scene_participating
    /// flipped to false since the snapshot, the live damage
    /// is already empty and the subtract is a no-op.
    pub(crate) fn ack_presentation_damage(&mut self, snap: DamageSnapshot) {
        let Some(d) = self.entries.get_mut(&snap.id) else {
            return;
        };
        if snap.epoch == d.presentation_damage_epoch {
            d.presentation_damage.clear();
        } else {
            d.presentation_damage.subtract(&snap.region);
        }
    }

    /// Stage 4a — set or clear a window's COMPOSITE redirect
    /// routing. `Some(backing_id)` routes future paint resolution
    /// against `window_id`'s xid (or any descendant whose nearest
    /// redirected ancestor is this drawable) into `backing_id`;
    /// `None` un-redirects. No side effects on damage / refcount /
    /// `scene_participating` — those flips belong to the protocol
    /// handler in 4c via the dedicated Backend methods.
    pub(crate) fn set_redirected_target(
        &mut self,
        window_id: DrawableId,
        backing_id: Option<DrawableId>,
    ) {
        // Diagnostic trace (TEMP — Stage 4d "opaque black backing"
        // investigation). Every redirect-route mutation matters
        // because clearing the route makes future paints land on
        // W's own storage instead of B. Volume is bounded by the
        // small number of redirected windows per session, so the
        // trace is left at log::trace! and gated by the
        // `yserver::kms::render::store` target.
        if log::log_enabled!(target: "yserver::kms::render::store", log::Level::Trace) {
            let old = self
                .entries
                .get(&window_id)
                .and_then(|d| d.redirected_target);
            log::trace!(
                target: "yserver::kms::render::store",
                "set_redirected_target window={window_id:?} old={old:?} new={backing_id:?}",
            );
        }
        if let Some(d) = self.entries.get_mut(&window_id) {
            d.redirected_target = backing_id;
        }
    }

    /// Stage 4a — leaf accessor returning the per-drawable
    /// `redirected_target`. Returns `None` if the drawable
    /// doesn't exist or isn't redirected. The full ancestor walk
    /// lives on `KmsBackend::resolve_paint_target` because it
    /// needs window-geometry metadata (`windows`) that isn't
    /// in the store.
    pub(crate) fn redirected_target(&self, id: DrawableId) -> Option<DrawableId> {
        self.entries.get(&id)?.redirected_target
    }

    /// True when some live window drawable currently routes paint into
    /// `candidate` via `set_redirected_target(..., Some(candidate))`.
    /// This captures real redirected-backing identity even when the
    /// backing was allocated through the generic pixmap path.
    pub(crate) fn is_active_redirect_target(&self, candidate: DrawableId) -> bool {
        self.entries
            .values()
            .any(|d| d.redirected_target == Some(candidate))
    }

    /// Record an in-flight ticket. Coalesces: replaces any
    /// prior ticket. The Rc inner stays alive in other
    /// holders (e.g. SceneCompositor's pending-ack list) per
    /// cross-cutting §5.
    pub(crate) fn touch_render_fence(&mut self, id: DrawableId, ticket: FenceTicket) {
        if let Some(d) = self.entries.get_mut(&id) {
            d.last_render_ticket = Some(ticket);
        }
        // GLX-TFP (Task 2.3): every write stamps its destination here, so
        // this is the single point that catches ALL mutation paths
        // (fills/clears/copies/composite/uploads). Record exported
        // destinations for the bidirectional implicit-sync wait/publish
        // at the flush chokepoint.
        if self.exported_sync.contains_key(&id) {
            self.exported_writes.push(id);
        }
    }

    /// Sweep `pending_retire`. Drawables whose ticket has
    /// signaled (or never had one) get destroyed and removed.
    /// `on_destroyed` is invoked per drawable BEFORE its storage
    /// is destroyed, so engine-side caches keyed on `DrawableId`
    /// (e.g. `RenderEngine::drawable_view_cache`) can drop their
    /// `VkImageView`s while the underlying `VkImage` is still
    /// alive.
    pub(crate) fn poll_pending_retire<F>(
        &mut self,
        platform: &mut PlatformBackend,
        mut on_destroyed: F,
    ) where
        F: FnMut(DrawableId),
    {
        let mut survivors = Vec::with_capacity(self.pending_retire.len());
        let mut to_destroy = Vec::new();
        for id in std::mem::take(&mut self.pending_retire) {
            let ready = match self.entries.get(&id) {
                None => true, // already gone
                Some(d) => match d.last_render_ticket.as_ref() {
                    None => true,
                    Some(t) => match platform.vk.as_ref() {
                        Some(vk) => t.poll_signaled(vk),
                        None => true,
                    },
                },
            };
            if ready {
                to_destroy.push(id);
            } else {
                survivors.push(id);
            }
        }
        for id in to_destroy {
            on_destroyed(id);
            self.destroy_now(platform, id);
        }
        self.pending_retire = survivors;
    }

    /// Number of live entries (test introspection).
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// Number of entries pending retirement.
    pub(crate) fn pending_retire_count(&self) -> usize {
        self.pending_retire.len()
    }

    /// Live sizes of the store's long-lived maps and lists, for the
    /// 1Hz `population[...]` telemetry gauge. All `len()`, so O(1).
    ///
    /// Returned as a tuple rather than the telemetry struct so
    /// `store` stays independent of the telemetry module:
    /// `(by_xid, exported_sync, exported_writes)`. `entries` and
    /// `pending_retire` already have their own accessors.
    pub(crate) fn population_counts(&self) -> (usize, usize, usize) {
        (
            self.by_xid.len(),
            self.exported_sync.len(),
            self.exported_writes.len(),
        )
    }

    /// Bump a drawable's `content_version` (saturating). Call on every
    /// successful pixel write. No-op for an unknown id.
    pub(crate) fn mark_contents_modified(&mut self, id: DrawableId) {
        if let Some(d) = self.get_mut(id) {
            d.content_version = d.content_version.saturating_add(1);
        }
    }

    /// Stage-1b-era compatibility constructor (was `stub()`).
    /// Kept so existing callers in `kms::render::backend` continue
    /// to compile until they're updated to call `new()`.
    pub(crate) fn stub() -> Self {
        Self::new()
    }
}

// ────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn stub_storage() -> Storage {
        Storage::for_tests_null(
            vk::Extent2D {
                width: 16,
                height: 16,
            },
            vk::Format::B8G8R8A8_UNORM,
        )
    }

    fn rect(x: i32, y: i32, w: u32, h: u32) -> vk::Rect2D {
        vk::Rect2D {
            offset: vk::Offset2D { x, y },
            extent: vk::Extent2D {
                width: w,
                height: h,
            },
        }
    }

    #[test]
    fn region_set_add_stays_bounded_and_covers_all_damage() {
        // Regression (wmaker restack/draw storm → machine lock-up, no ZAP):
        // presentation damage accumulated unbounded, and O(n·m)
        // RegionSet::subtract in the page-flip handler then wedged the
        // single-threaded core loop. `add` must keep the set bounded
        // (coalescing to the bounding rect past a cap) while never losing
        // damage — the bounding rect must still cover everything added.
        let mut rs = RegionSet::new();
        for i in 0..10_000i32 {
            rs.add(rect(i, 0, 1, 1));
        }
        assert!(
            rs.rects().len() <= 256,
            "RegionSet must stay bounded under heavy add, got {}",
            rs.rects().len(),
        );
        let b = rs.bounding_rect().expect("non-empty");
        assert_eq!(b.offset.x, 0, "bounding rect starts at first damage");
        assert!(
            b.offset.x.saturating_add_unsigned(b.extent.width) >= 10_000,
            "bounding rect must still cover ALL added damage (no lost repaint)",
        );
    }

    #[test]
    fn region_set_subtract_still_removes_exact_matches_under_cap() {
        // Guard: the common small-set path (exact-match removal) is intact.
        let mut a = RegionSet::new();
        a.add(rect(0, 0, 10, 10));
        a.add(rect(20, 0, 10, 10));
        let mut b = RegionSet::new();
        b.add(rect(0, 0, 10, 10));
        a.subtract(&b);
        assert_eq!(a.rects().len(), 1);
        assert_eq!(a.rects()[0], rect(20, 0, 10, 10));
    }

    #[test]
    fn allocate_and_lookup() {
        let mut s = DrawableStore::new();
        let id = s
            .allocate(0x1234, DrawableKind::Pixmap, 32, false, stub_storage())
            .expect("allocate");
        assert_eq!(s.lookup(0x1234), Some(id));
        let d = s.get(id).expect("get");
        assert_eq!(d.xid, 0x1234);
        assert_eq!(d.kind, DrawableKind::Pixmap);
        assert_eq!(d.depth, 32);
        assert_eq!(d.refcount, 1);
        assert!(!d.scene_participating);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn prewait_authorization_is_one_shot_and_rejects_earlier_writes() {
        use nix::sys::eventfd::{EfdFlags, EventFd};

        let mut s = DrawableStore::new();
        let id = s
            .allocate(0x1234, DrawableKind::Pixmap, 32, false, stub_storage())
            .expect("allocate");
        let fd: OwnedFd =
            EventFd::from_value_and_flags(0, EfdFlags::EFD_NONBLOCK | EfdFlags::EFD_CLOEXEC)
                .expect("eventfd")
                .into();
        s.set_exported_sync_fd(id, Arc::new(fd));

        s.begin_prewaited_exported_write(id);
        s.exported_writes.push(id);
        s.end_prewaited_exported_write(id);
        let first = s.take_exported_writes();
        assert_eq!(first.len(), 1);
        assert!(first[0].1, "authorization survives until the write flush");

        s.exported_writes.push(id);
        s.begin_prewaited_exported_write(id);
        let second = s.take_exported_writes();
        assert_eq!(second.len(), 1);
        assert!(!second[0].1, "an earlier pending write cannot be covered");
    }

    #[test]
    fn allocate_rejects_xid_collision() {
        let mut s = DrawableStore::new();
        s.allocate(0x1, DrawableKind::Pixmap, 32, false, stub_storage())
            .expect("first");
        let err = s
            .allocate(0x1, DrawableKind::Pixmap, 32, false, stub_storage())
            .expect_err("collision");
        assert!(matches!(err, AllocError::XidInUse));
    }

    #[test]
    fn decref_destroys_immediately_when_no_ticket() {
        let mut s = DrawableStore::new();
        let mut platform = PlatformBackend::for_tests();
        let id = s
            .allocate(0x1, DrawableKind::Pixmap, 32, false, stub_storage())
            .unwrap();
        assert_eq!(
            s.decref(&mut platform, id, |_| {}),
            RetireDecision::Destroyed
        );
        assert!(s.lookup(0x1).is_none());
        assert_eq!(s.len(), 0);
    }

    #[test]
    fn incref_then_decref_keeps_alive() {
        let mut s = DrawableStore::new();
        let mut platform = PlatformBackend::for_tests();
        let id = s
            .allocate(0x1, DrawableKind::Pixmap, 32, false, stub_storage())
            .unwrap();
        s.incref(id);
        assert_eq!(
            s.decref(&mut platform, id, |_| {}),
            RetireDecision::StillReferenced
        );
        assert!(s.lookup(0x1).is_some());
        assert_eq!(
            s.decref(&mut platform, id, |_| {}),
            RetireDecision::Destroyed
        );
        assert!(s.lookup(0x1).is_none());
    }

    #[test]
    fn damage_pixmap_is_no_op() {
        let mut s = DrawableStore::new();
        let id = s
            .allocate(0x1, DrawableKind::Pixmap, 32, false, stub_storage())
            .unwrap();
        s.damage(id, rect(0, 0, 4, 4));
        let d = s.get(id).unwrap();
        assert!(d.presentation_damage.is_empty());
        assert_eq!(d.presentation_damage_epoch, 0);
    }

    #[test]
    fn damage_window_accumulates_presentation_and_bumps_epoch() {
        let mut s = DrawableStore::new();
        let id = s
            .allocate(0x1, DrawableKind::Window, 24, true, stub_storage())
            .unwrap();
        s.damage(id, rect(0, 0, 4, 4));
        let d = s.get(id).unwrap();
        assert_eq!(d.presentation_damage.rects().len(), 1);
        assert_eq!(d.presentation_damage_epoch, 1);
        s.damage(id, rect(8, 8, 2, 2));
        assert_eq!(s.get(id).unwrap().presentation_damage_epoch, 2);
    }

    #[test]
    fn peek_and_ack_clears_when_epoch_matches() {
        let mut s = DrawableStore::new();
        let id = s
            .allocate(0x1, DrawableKind::Window, 24, true, stub_storage())
            .unwrap();
        s.damage(id, rect(0, 0, 4, 4));
        let snap = s.peek_presentation_damage(id).expect("snap");
        assert_eq!(snap.epoch, 1);
        s.ack_presentation_damage(snap);
        assert!(s.get(id).unwrap().presentation_damage.is_empty());
    }

    #[test]
    fn paint_between_peek_and_ack_survives() {
        let mut s = DrawableStore::new();
        let id = s
            .allocate(0x1, DrawableKind::Window, 24, true, stub_storage())
            .unwrap();
        s.damage(id, rect(0, 0, 4, 4));
        let snap = s.peek_presentation_damage(id).unwrap();
        // Paint arrives between peek and ack.
        s.damage(id, rect(8, 8, 2, 2));
        s.ack_presentation_damage(snap);
        // The post-peek paint survives.
        let live = &s.get(id).unwrap().presentation_damage;
        assert_eq!(live.rects().len(), 1);
        assert_eq!(live.rects()[0].offset, vk::Offset2D { x: 8, y: 8 });
    }

    #[test]
    fn set_scene_participating_false_clears_unpresented_damage() {
        let mut s = DrawableStore::new();
        let id = s
            .allocate(0x1, DrawableKind::Window, 24, true, stub_storage())
            .unwrap();
        s.damage(id, rect(0, 0, 4, 4));
        assert_eq!(s.get(id).unwrap().presentation_damage.rects().len(), 1);
        let epoch_before = s.get(id).unwrap().presentation_damage_epoch;
        s.set_scene_participating(id, false);
        let d = s.get(id).unwrap();
        assert!(d.presentation_damage.is_empty());
        assert!(d.presentation_damage_epoch > epoch_before);
    }

    /// Idle free-run fix (cut 2b): a scene-participating window with
    /// damage that `build_scene` did NOT draw (not in the `drawn` set)
    /// is flagged out of the compose scheduler — but its damage is
    /// PRESERVED, and being drawn again re-includes it.
    #[test]
    fn reconcile_offscreen_no_draw_gates_scheduler_but_preserves_damage() {
        let mut s = DrawableStore::new();
        let id = s
            .allocate(0x1, DrawableKind::Window, 24, true, stub_storage())
            .unwrap();
        s.damage(id, rect(0, 0, 4, 4));
        assert!(
            s.has_pending_presentation_damage(),
            "fresh damage on a scene window arms the scheduler",
        );

        // Not drawn on any output this tick → flag off-screen.
        s.reconcile_offscreen_no_draw(&HashSet::new());
        assert!(
            !s.has_pending_presentation_damage(),
            "un-drawable off-screen damage must not arm the scheduler (idle spin)",
        );
        assert_eq!(
            s.get(id).unwrap().presentation_damage.rects().len(),
            1,
            "damage must be PRESERVED, not cleared",
        );

        // Drawn this tick (e.g. it came on-screen) → flag cleared →
        // counted again so its damage composes + drains.
        let drawn: HashSet<DrawableId> = std::iter::once(id).collect();
        s.reconcile_offscreen_no_draw(&drawn);
        assert!(
            s.has_pending_presentation_damage(),
            "a drawn window's damage re-arms the scheduler",
        );
    }

    #[test]
    fn ack_after_unmap_is_noop_against_empty_live_damage() {
        // Codex round 1 point 5: peek; unmap; ack against the
        // stale snapshot. Live damage is already empty; the
        // subtract-by-snapshot is a no-op. No corruption.
        let mut s = DrawableStore::new();
        let id = s
            .allocate(0x1, DrawableKind::Window, 24, true, stub_storage())
            .unwrap();
        s.damage(id, rect(0, 0, 4, 4));
        let snap = s.peek_presentation_damage(id).unwrap();
        s.set_scene_participating(id, false);
        s.ack_presentation_damage(snap);
        let d = s.get(id).unwrap();
        assert!(d.presentation_damage.is_empty());
        assert!(!d.scene_participating);
    }

    /// `population_counts` must report the live map sizes, and
    /// `by_xid` must fall back to zero once the entries are
    /// destroyed. Issue #115: a `by_xid` that stays high while
    /// `len()` drops is the signature of orphaned xid mappings, so
    /// the two have to be independently observable.
    #[test]
    fn population_counts_track_live_map_sizes() {
        let mut s = DrawableStore::new();
        let mut platform = PlatformBackend::for_tests();
        assert_eq!(s.population_counts(), (0, 0, 0));

        let a = s
            .allocate(0x1, DrawableKind::Pixmap, 32, false, stub_storage())
            .expect("allocate a");
        s.allocate(0x2, DrawableKind::Pixmap, 32, false, stub_storage())
            .expect("allocate b");
        let (by_xid, exported_sync, exported_writes) = s.population_counts();
        assert_eq!(by_xid, 2);
        assert_eq!((exported_sync, exported_writes), (0, 0));
        assert_eq!(s.len(), 2);

        s.destroy_now(&mut platform, a);
        assert_eq!(s.population_counts().0, 1);
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn poll_pending_retire_with_no_ticket_destroys() {
        let mut s = DrawableStore::new();
        let mut platform = PlatformBackend::for_tests();
        let id = s
            .allocate(0x1, DrawableKind::Pixmap, 32, false, stub_storage())
            .unwrap();
        // Force into pending_retire by simulating a never-
        // signaled ticket (here we just push id manually since
        // we can't easily construct a FenceTicket in tests).
        s.entries.get_mut(&id).unwrap().refcount = 0;
        s.pending_retire.push(id);
        s.poll_pending_retire(&mut platform, |_| {});
        // No ticket attached → treated as signaled → destroyed.
        assert!(s.lookup(0x1).is_none());
        assert_eq!(s.pending_retire_count(), 0);
    }

    /// xeyes resize bug: decref → PendingFence + re-allocate the
    /// same xid + later destroy_now of the old drawable MUST NOT
    /// remove `by_xid[xid]` (which now maps to the NEW drawable).
    /// Pre-fix: blanket `by_xid.remove(drawable.xid)` in destroy_now
    /// orphaned the new storage when the old fence eventually
    /// signaled.
    #[test]
    fn decref_then_realloc_then_retire_keeps_new_xid_mapping() {
        let mut s = DrawableStore::new();
        let mut platform = PlatformBackend::for_tests();
        // Allocate old; force into pending_retire (simulates an
        // unsignaled ticket). We can't construct a real FenceTicket
        // in the test fixture, so we manually push.
        let old_id = s
            .allocate(0x42, DrawableKind::Window, 24, true, stub_storage())
            .unwrap();
        s.entries.get_mut(&old_id).unwrap().refcount = 0;
        // Mimic decref → PendingFence: park + detach xid.
        s.pending_retire.push(old_id);
        s.by_xid.remove(&0x42);
        // Re-allocate the SAME xid with fresh storage.
        let new_id = s
            .allocate(0x42, DrawableKind::Window, 24, true, stub_storage())
            .unwrap();
        assert_ne!(old_id, new_id, "store mints a fresh DrawableId");
        assert_eq!(
            s.lookup(0x42),
            Some(new_id),
            "by_xid now points to the new drawable",
        );
        // Now retire the old drawable. No real ticket attached, so
        // poll_pending_retire treats it as signaled → destroy_now.
        s.poll_pending_retire(&mut platform, |_| {});
        // The new drawable's xid mapping MUST survive.
        assert_eq!(
            s.lookup(0x42),
            Some(new_id),
            "destroy_now of old drawable preserves new xid mapping",
        );
        assert!(
            s.get(new_id).is_some(),
            "new drawable still alive in entries",
        );
        assert!(s.get(old_id).is_none(), "old drawable destroyed",);
    }

    /// xeyes resize regression with Picture refs: a Picture
    /// wrapping a window increfs the drawable. Pre-fix:
    /// `configure_subwindow`'s `decref(old) → StillReferenced`
    /// kept by_xid mapped to old → `allocate(xid, new)` failed
    /// `XidInUse` → window stayed at old size (heavy visible
    /// artifact). Now: `detach_xid` runs unconditionally,
    /// re-allocate succeeds, old drawable lingers for the
    /// picture's lifetime.
    #[test]
    fn detach_xid_lets_realloc_succeed_even_with_picture_refcount() {
        let mut s = DrawableStore::new();
        let mut platform = PlatformBackend::for_tests();
        let id = s
            .allocate(0x42, DrawableKind::Window, 24, true, stub_storage())
            .unwrap();
        // Simulate a Picture wrapping the window: bump refcount.
        s.incref(id);
        assert_eq!(s.get(id).unwrap().refcount, 2);
        // Simulate configure_subwindow resize sequence:
        s.detach_xid(0x42);
        let r = s.decref(&mut platform, id, |_| {});
        assert_eq!(
            r,
            RetireDecision::StillReferenced,
            "picture still references the old drawable",
        );
        // Old drawable survives in entries (picture still has it),
        // but its xid mapping is gone.
        assert!(s.get(id).is_some(), "old drawable kept alive by picture");
        assert!(s.lookup(0x42).is_none(), "xid map free for re-alloc");
        // Re-allocate the same xid — pre-fix: XidInUse.
        let new_id = s
            .allocate(0x42, DrawableKind::Window, 24, true, stub_storage())
            .expect("re-alloc must succeed after detach_xid");
        assert_ne!(new_id, id);
        assert_eq!(s.lookup(0x42), Some(new_id));
    }

    /// Stage 4a — `set_redirected_target(Some(B))` makes
    /// `redirected_target(W)` return `Some(B)`. Pure storage-side
    /// state; the full ancestor walk + paint dispatch lives on
    /// `KmsBackend::resolve_paint_target`.
    #[test]
    fn set_redirected_target_stores_backing_id() {
        let mut s = DrawableStore::new();
        let w_id = s
            .allocate(0x100, DrawableKind::Window, 24, true, stub_storage())
            .unwrap();
        let b_id = s
            .allocate(0x200, DrawableKind::Pixmap, 24, false, stub_storage())
            .unwrap();
        assert_eq!(s.redirected_target(w_id), None);
        s.set_redirected_target(w_id, Some(b_id));
        assert_eq!(s.redirected_target(w_id), Some(b_id));
    }

    /// Stage 4a — `set_redirected_target(None)` clears the route.
    #[test]
    fn set_redirected_target_none_clears_route() {
        let mut s = DrawableStore::new();
        let w_id = s
            .allocate(0x100, DrawableKind::Window, 24, true, stub_storage())
            .unwrap();
        let b_id = s
            .allocate(0x200, DrawableKind::Pixmap, 24, false, stub_storage())
            .unwrap();
        s.set_redirected_target(w_id, Some(b_id));
        s.set_redirected_target(w_id, None);
        assert_eq!(s.redirected_target(w_id), None);
    }

    /// Stage 4a — flipping `set_redirected_target` does NOT touch
    /// damage / refcount / `scene_participating`. Those flips are
    /// 4c's responsibility via the dedicated Backend methods.
    #[test]
    fn set_redirected_target_has_no_side_effects() {
        let mut s = DrawableStore::new();
        let w_id = s
            .allocate(0x100, DrawableKind::Window, 24, true, stub_storage())
            .unwrap();
        let b_id = s
            .allocate(0x200, DrawableKind::Pixmap, 24, false, stub_storage())
            .unwrap();
        s.damage(w_id, rect(0, 0, 4, 4));
        let epoch_before = s.get(w_id).unwrap().presentation_damage_epoch;
        let refcount_before = s.get(w_id).unwrap().refcount;
        let participating_before = s.get(w_id).unwrap().scene_participating;
        s.set_redirected_target(w_id, Some(b_id));
        let d = s.get(w_id).unwrap();
        assert_eq!(d.presentation_damage.rects().len(), 1);
        assert_eq!(d.presentation_damage_epoch, epoch_before);
        assert_eq!(d.refcount, refcount_before);
        assert_eq!(d.scene_participating, participating_before);
    }

    #[test]
    fn redirected_target_unknown_id_returns_none() {
        let s = DrawableStore::new();
        // Construct a fresh DrawableId that doesn't exist in the store.
        assert_eq!(s.redirected_target(DrawableId(999)), None);
    }

    #[test]
    fn region_set_subtract_exact_match_only() {
        let mut r = RegionSet::new();
        r.add(rect(0, 0, 4, 4));
        r.add(rect(8, 8, 2, 2));
        let mut sub = RegionSet::new();
        sub.add(rect(0, 0, 4, 4));
        r.subtract(&sub);
        assert_eq!(r.rects().len(), 1);
        assert_eq!(r.rects()[0].offset, vk::Offset2D { x: 8, y: 8 });
    }

    #[test]
    fn region_set_zero_extent_ignored() {
        let mut r = RegionSet::new();
        r.add(vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent: vk::Extent2D {
                width: 0,
                height: 4,
            },
        });
        r.add(vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent: vk::Extent2D {
                width: 4,
                height: 0,
            },
        });
        assert!(r.is_empty());
    }

    /// Stage 3f.10: `Storage::from_pooled` inherits the pool
    /// entry's tracked layout so the next `record_layout_transition`
    /// issues a correct `old_layout` barrier (not the
    /// `UNDEFINED` that a fresh allocate would imply). Pool entries
    /// also keep the size + format the key was built for.
    #[test]
    fn storage_from_pooled_inherits_layout_and_dims() {
        use crate::kms::vk::pixmap_pool::PooledPixmapImage;
        // Sentinel non-null handles — Storage::from_pooled just
        // copies them; nothing dereferences a real Vk image here.
        let pooled = PooledPixmapImage {
            image: ash::vk::Handle::from_raw(0x1000_0001),
            view: ash::vk::Handle::from_raw(0x1000_0002),
            memory: ash::vk::Handle::from_raw(0x1000_0003),
            current_layout: ash::vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        };
        let extent = ash::vk::Extent2D {
            width: 32,
            height: 64,
        };
        let format = ash::vk::Format::B8G8R8A8_UNORM;
        let sample_view: ash::vk::ImageView = ash::vk::Handle::from_raw(0x1000_0004);
        let s = Storage::from_pooled(pooled, sample_view, extent, format, 32);
        assert_eq!(s.extent.width, 32);
        assert_eq!(s.extent.height, 64);
        assert_eq!(s.format, format);
        assert_eq!(
            s.current_layout,
            ash::vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL
        );
        assert!(!s.is_test_stub);
    }

    /// `shutdown_destroy_all` on an empty store is a clean no-op:
    /// no entries, no by_xid lookups, no pending retires.
    #[test]
    fn shutdown_destroy_all_on_empty_store_is_noop() {
        let mut s = DrawableStore::new();
        let platform = PlatformBackend::for_tests();
        s.shutdown_destroy_all(&platform);
        assert!(s.entries.is_empty());
        assert!(s.by_xid.is_empty());
        assert!(s.pending_retire.is_empty());
    }

    /// Regression: pre-fix `DrawableStore` had no `Drop`, so
    /// stub drawables still in `entries` at SIGTERM had their
    /// `Storage` dropped silently — production drawables would
    /// leak their real Vk handles. Verify `shutdown_destroy_all`
    /// drains every entry and clears the xid lookup. Stub
    /// storage's `destroy` is a no-op (early-returns on
    /// `is_test_stub`), so this only proves the iteration +
    /// drain logic; live-Vk leak verification is via the smoke
    /// recipe with `VK_LAYER_KHRONOS_validation`.
    #[test]
    fn shutdown_destroy_all_drains_every_entry() {
        let mut s = DrawableStore::new();
        let platform = PlatformBackend::for_tests();
        let id1 = s
            .allocate(0x100_0001, DrawableKind::Pixmap, 32, false, stub_storage())
            .expect("allocate 1");
        let id2 = s
            .allocate(0x100_0002, DrawableKind::Window, 24, true, stub_storage())
            .expect("allocate 2");
        assert_eq!(s.entries.len(), 2);
        assert_eq!(s.by_xid.len(), 2);
        // Park one in pending_retire to verify it's cleared too
        // (would otherwise hold a stale DrawableId past the drain).
        s.pending_retire.push(id1);
        s.pending_retire.push(id2);

        s.shutdown_destroy_all(&platform);

        assert!(s.entries.is_empty(), "shutdown must drain entries");
        assert!(s.by_xid.is_empty(), "shutdown must clear xid lookup");
        assert!(
            s.pending_retire.is_empty(),
            "shutdown must clear pending_retire so a redrop attempt finds no DrawableId",
        );
    }

    /// `decref` invokes `on_destroyed` exactly when it actually
    /// destroys the drawable's storage (refcount→0 + ticket
    /// signaled). The callback is the bridge that lets
    /// `KmsBackend` invalidate `RenderEngine::drawable_view_cache`
    /// entries keyed on the destroyed `DrawableId` before
    /// `Storage::destroy` runs. Pre-fix the callback parameter
    /// didn't exist; the engine's cached views accumulated for
    /// every `DestroyPixmap` / `DestroyWindow` across the session
    /// and only got swept at engine `Drop`.
    #[test]
    fn decref_invokes_invalidation_callback_on_destroy() {
        let mut s = DrawableStore::new();
        let mut platform = PlatformBackend::for_tests();
        let id = s
            .allocate(0x100_0001, DrawableKind::Pixmap, 32, false, stub_storage())
            .expect("allocate");
        let mut invalidated = Vec::new();
        let decision = s.decref(&mut platform, id, |dropped| invalidated.push(dropped));
        assert_eq!(decision, RetireDecision::Destroyed);
        assert_eq!(
            invalidated,
            vec![id],
            "decref must invoke `on_destroyed(id)` BEFORE Storage::destroy \
             so engine view cache invalidates while the underlying VkImage \
             is still alive (the Vulkan view-before-image idiom)",
        );
    }

    /// Companion to the test above: when `decref` parks the
    /// drawable in `pending_retire` (ticket not yet signaled),
    /// the invalidation callback must NOT fire — the drawable's
    /// storage is still alive and its cached views are still
    /// valid. The callback fires later, when `poll_pending_retire`
    /// finally destroys it. Pre-fix this distinction was moot
    /// (no callback); post-fix it's load-bearing because firing
    /// early would invalidate views the engine may still be
    /// sampling from in-flight ops.
    #[test]
    fn decref_does_not_invalidate_when_still_referenced() {
        let mut s = DrawableStore::new();
        let mut platform = PlatformBackend::for_tests();
        let id = s
            .allocate(0x100_0002, DrawableKind::Pixmap, 32, false, stub_storage())
            .expect("allocate");
        s.incref(id);
        let mut invalidated = Vec::new();
        let decision = s.decref(&mut platform, id, |dropped| invalidated.push(dropped));
        assert_eq!(decision, RetireDecision::StillReferenced);
        assert!(
            invalidated.is_empty(),
            "decref with refcount > 1 must NOT invalidate — drawable storage \
             is still alive and engine cache entries are still valid",
        );
    }

    /// `poll_pending_retire` invokes `on_destroyed` per drawable
    /// that gets destroyed during the sweep (one callback per
    /// id, BEFORE `Storage::destroy`).
    #[test]
    fn poll_pending_retire_invokes_invalidation_callback_per_destroyed_id() {
        let mut s = DrawableStore::new();
        let mut platform = PlatformBackend::for_tests();
        // Stash two drawables in pending_retire by setting
        // refcount=1 + a (stub) ticket and decref'ing. Stub
        // storage has no real ticket so decref goes synchronous;
        // park them directly via the internal list.
        let id_a = s
            .allocate(0x100_0003, DrawableKind::Pixmap, 32, false, stub_storage())
            .expect("allocate a");
        let id_b = s
            .allocate(0x100_0004, DrawableKind::Pixmap, 32, false, stub_storage())
            .expect("allocate b");
        s.pending_retire.push(id_a);
        s.pending_retire.push(id_b);
        let mut invalidated = Vec::new();
        s.poll_pending_retire(&mut platform, |dropped| invalidated.push(dropped));
        assert_eq!(invalidated.len(), 2);
        assert!(invalidated.contains(&id_a));
        assert!(invalidated.contains(&id_b));
        assert!(s.entries.is_empty(), "both entries destroyed");
        assert!(s.pending_retire.is_empty(), "pending_retire drained");
    }

    #[test]
    fn mark_contents_modified_bumps_content_version() {
        let mut s = DrawableStore::new();
        let id = s
            .allocate(0x1, DrawableKind::Pixmap, 32, false, stub_storage())
            .unwrap();
        assert_eq!(s.get(id).unwrap().content_version, 0);
        s.mark_contents_modified(id);
        s.mark_contents_modified(id);
        assert_eq!(s.get(id).unwrap().content_version, 2);
        // Unknown id is a silent no-op (never panics).
        s.mark_contents_modified(DrawableId::for_tests(u64::MAX));
    }

    /// `mark_contents_modified` uses `saturating_add`, so at `u64::MAX` the
    /// counter freezes rather than wrapping to 0. Wrapping would alias a fresh
    /// version onto the sentinel "nothing cached yet" value and cause the
    /// clip-mask cache to treat a new mask as unchanged. Freezing is
    /// preferable: the cache conservatively re-reads while the counter stays
    /// saturated, which is correct (if extremely rare in practice).
    #[test]
    fn mark_contents_modified_saturates_at_max() {
        let mut s = DrawableStore::new();
        let id = s
            .allocate(0x1, DrawableKind::Pixmap, 32, false, stub_storage())
            .unwrap();
        s.get_mut(id).unwrap().content_version = u64::MAX;
        s.mark_contents_modified(id);
        assert_eq!(s.get(id).unwrap().content_version, u64::MAX);
    }

    #[test]
    fn imported_dmabuf_metadata_preserves_layout_exactly() {
        let metadata = ImportedDmabufMetadata {
            fourcc: u32::from_le_bytes(*b"XR24"),
            vk_format: vk::Format::B8G8R8A8_UNORM,
            modifier: 0x0100_0000_0000_0002,
            planes: vec![ImportedDmabufPlane {
                offset: 4096,
                pitch: 8192,
            }],
            width: 1920,
            height: 1080,
            depth: 24,
            bpp: 32,
        };
        assert_eq!(metadata.fourcc, u32::from_le_bytes(*b"XR24"));
        assert_eq!(metadata.vk_format, vk::Format::B8G8R8A8_UNORM);
        assert_eq!(metadata.modifier, 0x0100_0000_0000_0002);
        assert_eq!(metadata.planes[0].offset, 4096);
        assert_eq!(metadata.planes[0].pitch, 8192);
        assert_eq!((metadata.width, metadata.height), (1920, 1080));
        assert_eq!(metadata.depth, 24);
        assert_eq!(metadata.bpp, 32);
        assert!(stub_storage().imported_dmabuf.is_none());
    }
}
