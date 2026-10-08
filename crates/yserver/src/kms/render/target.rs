//! #133 step 3 (P4) — the drawable-target API: the only route from a
//! client request to a drawable's storage.
//!
//! Borders live *inside* the window's own storage, mirroring Xorg's
//! `compAllocPixmap` (`composite/compalloc.c:610`): the storage is
//! `(w + 2bw) x (h + 2bw)`, placed at the window's OUTER origin, with
//! the client-visible content starting at `(bw, bw)` inside it. Storage
//! bounds are therefore no longer the same thing as the drawable's
//! bounds, and "clip to the storage extent" — which is what every
//! engine op did — would let a client `PutImage` at a negative
//! coordinate, an oversized fill, or a `CopyArea` source scribble on or
//! read back the border ring.
//!
//! The enforcement is structural rather than advisory: every
//! destination op on [`RenderEngine`](super::engine::RenderEngine) takes
//! a [`Dst`] and every drawable read takes a [`Src`] instead of a bare
//! `DrawableId`. A `DrawableId` on its own can no longer paint. There
//! are exactly two ways to obtain one of those handles:
//!
//! 1. [`PaintTarget::dst`] / [`PaintTarget::src`] — the client-facing
//!    route. The handle carries the resolved *content* bounds, and the
//!    engine clamps and scissors against those bounds everywhere it
//!    used to use `drawable.storage.extent`. A drawing op added later
//!    inherits the clip for free; it cannot forget to consult a field.
//! 2. [`PaintTarget::server_backing_dst`] /
//!    [`PaintTarget::server_backing_src`] / [`Dst::server_internal`] /
//!    [`Src::server_internal`] — the deliberate privileged exception,
//!    in backing space, for server-internal painting only (storage
//!    initialisation, the CPU read-modify-write fallbacks' whole-storage
//!    readback and write-back, the compositor/COW/cursor paths, and —
//!    from step 4 — the border ring fill, which by construction *cannot*
//!    go through the clipped route). Deliberately named so the whole
//!    privileged surface is auditable with
//!    `grep -n 'server_backing_dst\|server_backing_src\|server_internal'`.
//!
//! `bounds == None` means "the full storage extent", which is exactly
//! what every op did before this module existed. Every pixmap, and every
//! window whose resolved chain has `border_width == 0` throughout,
//! resolves to `None`, so the `bw == 0` path — i.e. every WM we
//! currently support — runs the identical arithmetic through the
//! identical branches.

use ash::vk;

use super::store::DrawableId;
use crate::kms::cpu_types::Rectangle16;

/// A destination handle. Holding one is the only way to name the
/// storage an engine destination op writes to; `bounds` travels with it
/// and the engine applies it where it used to apply the raw storage
/// extent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Dst {
    id: DrawableId,
    bounds: Option<vk::Rect2D>,
}

impl Dst {
    /// PRIVILEGED: unclipped backing-space access to `id`.
    ///
    /// For server-internal paint that legitimately owns the whole
    /// storage — storage init, COW/scanout, cursor rasterisation, the
    /// CPU RMW write-back of pixels it just read — and for pixmaps
    /// reached without a `PaintTarget`. Never for a client request's
    /// destination: those resolve a [`PaintTarget`] and use
    /// [`PaintTarget::dst`].
    pub(crate) fn server_internal(id: DrawableId) -> Self {
        Self { id, bounds: None }
    }

    pub(crate) fn id(self) -> DrawableId {
        self.id
    }

    /// The clip the engine must honour, in storage coordinates.
    /// `None` = the full storage extent (pre-#133 behaviour).
    pub(crate) fn bounds(self) -> Option<vk::Rect2D> {
        self.bounds
    }

    /// The clip resolved against a concrete storage extent, for the
    /// engine's clamp/scissor sites.
    pub(crate) fn bounds_in(self, extent: vk::Extent2D) -> vk::Rect2D {
        resolve_bounds(self.bounds, extent)
    }

    /// Read this destination back, under the SAME bounds, for a
    /// read-modify-write op (the `*_rop_cpu` fallbacks read the
    /// destination pixels, combine them with the source through the GC
    /// function, and write them back).
    pub(crate) fn read_back(self) -> Src {
        Src {
            id: self.id,
            bounds: self.bounds,
        }
    }
}

/// A source handle — the read counterpart of [`Dst`]. `GetImage` and a
/// `CopyArea` source must not read the ring back as window content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Src {
    id: DrawableId,
    bounds: Option<vk::Rect2D>,
}

impl Src {
    /// PRIVILEGED: unclipped backing-space read of `id`. Same rules as
    /// [`Dst::server_internal`].
    pub(crate) fn server_internal(id: DrawableId) -> Self {
        Self { id, bounds: None }
    }

    pub(crate) fn id(self) -> DrawableId {
        self.id
    }

    /// The clip the engine must honour, in storage coordinates.
    /// `None` = the full storage extent (pre-#133 behaviour).
    #[cfg(test)]
    pub(crate) fn bounds(self) -> Option<vk::Rect2D> {
        self.bounds
    }

    pub(crate) fn bounds_in(self, extent: vk::Extent2D) -> vk::Rect2D {
        resolve_bounds(self.bounds, extent)
    }
}

fn resolve_bounds(bounds: Option<vk::Rect2D>, extent: vk::Extent2D) -> vk::Rect2D {
    super::engine::resolve_recorded_bounds(bounds, extent)
}

/// Stage 4a — resolution result for a paint operation against a host
/// xid. Names the drawable that actually receives the paint, the
/// translation from the drawable's own content coordinates into that
/// storage, and (#133 step 3) the content clip inside it.
///
/// The offset is non-zero when the target is a descendant of a
/// redirected ancestor — paint against descendant `C` of redirected
/// `W`, with `C` at `(cx, cy)` relative to `W`, lands at
/// `(cx + x, cy + y, w, h)` in `W`'s backing — and, since #133, when
/// any window in that chain has a border: content sits `bw` inside its
/// own storage (`compAllocPixmap`, `composite/compalloc.c:610`).
///
/// The four cases, all produced by
/// `KmsBackend::resolve_window_paint_target`'s single walk (horizontal
/// axis shown; the vertical is the same with `y`/`height`):
///
/// | case | translation | content clip |
/// |---|---|---|
/// | leaf storage (no redirected ancestor) | `bw(V)` | `(bw, bw, w, h)` |
/// | ancestor backing (`C` under redirected `W`) | `bw(W) + C.x + bw(C)` | `W`'s content ∩ `C`'s content |
/// | root redirection (top-level `T`, root redirected) | `T.x + bw(T)` | `T`'s content (root has no border) |
/// | nested descendants (`C` under `P` under redirected `W`) | `bw(W) + P.x + bw(P) + C.x + bw(C)` | every bordered level's content, intersected |
///
/// A redirected window itself is the *first* case as seen from its own
/// backing: its content clip derives from the window alone with no
/// ancestor term, for BOTH redirect modes, because Xorg keys this off
/// `redirectDraw != RedirectDrawNone` with no manual/automatic
/// distinction (`SetWinSize`, `dix/window.c:1720`; `SetBorderSize`,
/// `:1747`) — spec §The redirection exception, rule 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PaintTarget {
    id: DrawableId,
    offset: (i32, i32),
    /// The content clip in STORAGE coordinates, or `None` when the
    /// content is the whole storage (every pixmap; every window whose
    /// resolved chain is `bw == 0` throughout). `None` is not merely an
    /// optimisation: it keeps the `bw == 0` path on the identical
    /// arithmetic it had before #133.
    content: Option<vk::Rect2D>,
    /// Whether some window in the chain has a border, i.e. `content`
    /// came from a border term. A window painting into an ancestor's
    /// backing also carries its bounds in `content`
    /// ([`Self::within_window_bounds`]) without being bordered.
    bordered: bool,
    /// The logical X11 drawable depth of the ORIGINAL draw target. This
    /// can differ from the backing storage depth when a depth-24 child
    /// paints into a depth-32 redirected frame backing.
    x11_depth: u8,
}

impl PaintTarget {
    pub(crate) fn new(
        id: DrawableId,
        offset: (i32, i32),
        content: Option<vk::Rect2D>,
        x11_depth: u8,
    ) -> Self {
        Self {
            id,
            offset,
            content,
            bordered: content.is_some(),
            x11_depth,
        }
    }

    /// Narrow the clip to `bounds` (storage coordinates): the drawn
    /// window's own rect intersected with every ancestor's, for a window
    /// that paints into an ANCESTOR's backing. Xorg clips such a window
    /// to its `clipList`, which never leaves its parent's
    /// (`mi/mivaltree.c:390` and `:426-427`, `RegionIntersect(&childUniverse,
    /// universe, &pChild->borderSize)`), so the window cannot paint its parent's
    /// or its parent's siblings' pixels in the shared pixmap. Its own
    /// storage would clip it by its extent; the shared one does not.
    pub(crate) fn within_window_bounds(self, bounds: Option<vk::Rect2D>) -> Self {
        let Some(bounds) = bounds else {
            return self;
        };
        let content = match self.content {
            None => bounds,
            Some(c) => intersect_vk(c, bounds).unwrap_or(vk::Rect2D {
                offset: c.offset,
                extent: vk::Extent2D {
                    width: 0,
                    height: 0,
                },
            }),
        };
        Self {
            content: Some(content),
            ..self
        }
    }

    /// The same target, painted as an X drawable of `depth`.
    pub(crate) fn with_x11_depth(self, depth: u8) -> Self {
        Self {
            x11_depth: depth,
            ..self
        }
    }

    /// The translation from the drawable's own content coordinates into
    /// storage coordinates. Callers add this to every local rect origin.
    pub(crate) fn offset(self) -> (i32, i32) {
        self.offset
    }

    /// The logical X11 depth of the drawable the client named.
    pub(crate) fn x11_depth(self) -> u8 {
        self.x11_depth
    }

    /// The backing drawable id, for NON-DRAWING uses only: store
    /// metadata lookups, damage bookkeeping, identity comparisons,
    /// telemetry and submit tracing. It cannot paint — every engine
    /// destination op takes a [`Dst`] and every read a [`Src`], so an
    /// id on its own reaches no storage.
    pub(crate) fn backing_id(self) -> DrawableId {
        self.id
    }

    /// True when some window in the resolved chain has a border, i.e.
    /// the content clip actually restricts the storage. Used by the
    /// direct-scanout gate (#133 step 3.5) and by tests.
    pub(crate) fn has_border_clip(self) -> bool {
        self.bordered
    }

    /// The content clip in storage coordinates, or `None` for "the whole
    /// storage". `get_image` needs it to mirror the engine's clamp when
    /// computing the reply's row geometry.
    pub(crate) fn content_bounds(self) -> Option<vk::Rect2D> {
        self.content
    }

    /// The client-facing destination: content-clipped.
    pub(crate) fn dst(self) -> Dst {
        Dst {
            id: self.id,
            bounds: self.content,
        }
    }

    /// The client-facing source: content-clipped, so a `CopyArea`
    /// source or a `GetImage` cannot read ring pixels back as window
    /// content.
    pub(crate) fn src(self) -> Src {
        Src {
            id: self.id,
            bounds: self.content,
        }
    }

    /// A CLIENT read that legitimately includes the border ring.
    ///
    /// `GetImage` on a window is border-inclusive in X11: the request's
    /// rectangle may extend to `±border_width` and the read is bounded
    /// by the *containing* pixmap, not by the window's content. Xorg's
    /// `DoGetImage` checks exactly
    /// `x >= -wBorderWidth(pWin) && x + width <= wBorderWidth(pWin) +
    /// pDraw->width` (`dix/dispatch.c:2373-2377`), converts to
    /// bounding-pixmap coordinates with
    /// `relx = x + pDraw->x - pPix->screen_x` (`:2382-2390`) — which is
    /// this handle's content offset — and reads the bounding drawable
    /// (`:2405-2419`). yserver's own request handler already mirrors the
    /// `±bw` rule ("the rect may include the BORDER (xts XGetImage-7
    /// reads (-1,-1))", `process_request.rs:25566`).
    ///
    /// So the CONTENT OFFSET is what keeps `xSrc = 0` off the ring; the
    /// bound must stay the storage. Clamping a read to the content rect
    /// instead makes the reply shorter than the rectangle the client
    /// asked for, and libX11 sizes the `XImage` buffer from the reply
    /// length while indexing it with the requested width/height — an
    /// out-of-bounds read inside the client. That is what crashed xts
    /// `Xlib4/XSetWindowBackgroundPixmap` purpose 2.
    pub(crate) fn src_including_border(self) -> Src {
        Src::server_internal(self.id)
    }

    /// PRIVILEGED: the whole backing, ring included. Server-internal
    /// paint only — see the module docs. Step 4's ring fill is the
    /// motivating caller; it *cannot* use [`PaintTarget::dst`] by
    /// construction.
    pub(crate) fn server_backing_dst(self) -> Dst {
        Dst::server_internal(self.id)
    }

    /// PRIVILEGED: read the whole backing, ring included. The CPU
    /// read-modify-write fallbacks read the full storage and write it
    /// back unchanged outside the (separately clipped) paint rects.
    pub(crate) fn server_backing_src(self) -> Src {
        Src::server_internal(self.id)
    }

    /// Clip drawable-local rects to the content extent, dropping the
    /// empties. Identity when there is no border clip.
    ///
    /// For the CPU paths that read the whole storage and modify it in
    /// place: the *rects they touch* must be content-clipped even
    /// though the readback and write-back span the storage.
    pub(crate) fn clip_local_rects(self, rects: &[Rectangle16]) -> Vec<Rectangle16> {
        let Some(local) = self.content_local() else {
            return rects.to_vec();
        };
        rects
            .iter()
            .filter_map(|r| intersect_local(*r, local))
            .collect()
    }

    /// [`Self::clip_local_rects`] for a single `vk::Rect2D` in
    /// drawable-local coordinates. `None` = fully clipped away.
    pub(crate) fn clip_local_vk_rect(self, rect: vk::Rect2D) -> Option<vk::Rect2D> {
        let Some(local) = self.content_local() else {
            return Some(rect);
        };
        intersect_vk(rect, local)
    }

    /// The content clip translated back into drawable-local
    /// coordinates: `(0, 0, w, h)` for a bordered window, `None` when
    /// the content is the whole storage.
    fn content_local(self) -> Option<vk::Rect2D> {
        self.content.map(|c| vk::Rect2D {
            offset: vk::Offset2D {
                x: c.offset.x - self.offset.0,
                y: c.offset.y - self.offset.1,
            },
            extent: c.extent,
        })
    }
}

fn intersect_vk(a: vk::Rect2D, b: vk::Rect2D) -> Option<vk::Rect2D> {
    let x0 = a.offset.x.max(b.offset.x);
    let y0 = a.offset.y.max(b.offset.y);
    let x1 = a
        .offset
        .x
        .saturating_add_unsigned(a.extent.width)
        .min(b.offset.x.saturating_add_unsigned(b.extent.width));
    let y1 = a
        .offset
        .y
        .saturating_add_unsigned(a.extent.height)
        .min(b.offset.y.saturating_add_unsigned(b.extent.height));
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some(vk::Rect2D {
        offset: vk::Offset2D { x: x0, y: y0 },
        extent: vk::Extent2D {
            width: u32::try_from(x1 - x0).unwrap_or(0),
            height: u32::try_from(y1 - y0).unwrap_or(0),
        },
    })
}

fn intersect_local(r: Rectangle16, bounds: vk::Rect2D) -> Option<Rectangle16> {
    if r.width == 0 || r.height == 0 {
        return None;
    }
    let vk_r = vk::Rect2D {
        offset: vk::Offset2D {
            x: i32::from(r.x),
            y: i32::from(r.y),
        },
        extent: vk::Extent2D {
            width: u32::from(r.width),
            height: u32::from(r.height),
        },
    };
    let i = intersect_vk(vk_r, bounds)?;
    Some(Rectangle16 {
        x: i16::try_from(i.offset.x).unwrap_or(i16::MAX),
        y: i16::try_from(i.offset.y).unwrap_or(i16::MAX),
        width: u16::try_from(i.extent.width).unwrap_or(u16::MAX),
        height: u16::try_from(i.extent.height).unwrap_or(u16::MAX),
    })
}

/// Intersect a rect with the content clip of a window whose border
/// width is `bw`, in that window's own storage frame, and translate the
/// storage-frame rect. Used while resolving a paint target.
pub(crate) fn content_rect(offset: (i32, i32), width: u16, height: u16) -> vk::Rect2D {
    vk::Rect2D {
        offset: vk::Offset2D {
            x: offset.0,
            y: offset.1,
        },
        extent: vk::Extent2D {
            width: u32::from(width),
            height: u32::from(height),
        },
    }
}

/// Accumulator for the content clip while walking up the parent chain
/// to a redirected ancestor. Rects are held in the frame the walk has
/// reached; `shift` moves the whole accumulator up one frame, exactly
/// as the paint offset is shifted.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ContentClipAccum {
    clip: Option<vk::Rect2D>,
}

impl ContentClipAccum {
    /// Intersect in one level's content rect. Levels with
    /// `border_width == 0` must NOT be intersected in: their content
    /// rect is not a border constraint, and adding it would narrow the
    /// `bw == 0` path (where the storage extent is the only clip today)
    /// and so change behaviour on every desktop we support.
    pub(crate) fn intersect(&mut self, rect: vk::Rect2D) {
        self.clip = match self.clip {
            None => Some(rect),
            Some(cur) => Some(intersect_vk(cur, rect).unwrap_or(vk::Rect2D {
                offset: cur.offset,
                extent: vk::Extent2D {
                    width: 0,
                    height: 0,
                },
            })),
        };
    }

    pub(crate) fn shift(&mut self, dx: i32, dy: i32) {
        if let Some(c) = &mut self.clip {
            c.offset.x += dx;
            c.offset.y += dy;
        }
    }

    /// The accumulated clip, resolved against the storage extent it
    /// will be applied in. `None` when no bordered level contributed —
    /// the `bw == 0` identity case.
    pub(crate) fn finish(self, storage_extent: vk::Extent2D) -> Option<vk::Rect2D> {
        self.clip
            .map(|c| super::engine::clamp_rect(c, storage_extent))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn no_border_clip_is_identity() {
        let t = PaintTarget::new(DrawableId::for_tests(1), (0, 0), None, 24);
        assert!(!t.has_border_clip());
        assert_eq!(t.dst().bounds(), None);
        assert_eq!(t.src().bounds(), None);
        assert_eq!(
            t.dst().bounds_in(vk::Extent2D {
                width: 10,
                height: 4
            }),
            rect(0, 0, 10, 4)
        );
        let rects = [Rectangle16 {
            x: -5,
            y: -5,
            width: 100,
            height: 100,
        }];
        assert_eq!(t.clip_local_rects(&rects), rects.to_vec());
    }

    #[test]
    fn border_clip_confines_local_rects_to_content() {
        // 10x4 content inside 16x10 storage at (3, 3): bw = 3.
        let t = PaintTarget::new(
            DrawableId::for_tests(1),
            (3, 3),
            Some(rect(3, 3, 10, 4)),
            24,
        );
        assert!(t.has_border_clip());
        // Drawable-local (-4, -4, 20, 20) survives only as (0, 0, 10, 4).
        assert_eq!(
            t.clip_local_rects(&[Rectangle16 {
                x: -4,
                y: -4,
                width: 20,
                height: 20,
            }]),
            vec![Rectangle16 {
                x: 0,
                y: 0,
                width: 10,
                height: 4,
            }]
        );
        // A rect entirely in the ring is dropped.
        assert!(
            t.clip_local_rects(&[Rectangle16 {
                x: -3,
                y: -3,
                width: 3,
                height: 3,
            }])
            .is_empty()
        );
    }

    #[test]
    fn privileged_route_keeps_the_whole_backing() {
        let t = PaintTarget::new(
            DrawableId::for_tests(7),
            (3, 3),
            Some(rect(3, 3, 10, 4)),
            24,
        );
        assert_eq!(t.dst().bounds(), Some(rect(3, 3, 10, 4)));
        assert_eq!(t.server_backing_dst().bounds(), None);
        assert_eq!(t.server_backing_src().bounds(), None);
        // …and the privileged bounds resolve to the FULL storage.
        assert_eq!(
            t.server_backing_dst().bounds_in(vk::Extent2D {
                width: 16,
                height: 10
            }),
            rect(0, 0, 16, 10)
        );
    }

    #[test]
    fn accumulator_skips_unbordered_levels_and_shifts_like_the_offset() {
        let mut acc = ContentClipAccum::default();
        // No level contributed → identity, whatever the extent.
        assert_eq!(
            acc.finish(vk::Extent2D {
                width: 8,
                height: 8
            }),
            None
        );
        // One bordered level, then a shift up two frames.
        acc.intersect(rect(2, 2, 4, 4));
        acc.shift(10, 0);
        acc.shift(0, 5);
        assert_eq!(
            acc.finish(vk::Extent2D {
                width: 100,
                height: 100
            }),
            Some(rect(12, 7, 4, 4))
        );
    }

    #[test]
    fn accumulator_intersection_can_empty() {
        let mut acc = ContentClipAccum::default();
        acc.intersect(rect(0, 0, 4, 4));
        acc.intersect(rect(10, 10, 4, 4));
        let out = acc
            .finish(vk::Extent2D {
                width: 100,
                height: 100,
            })
            .expect("bordered");
        assert_eq!(out.extent.width, 0);
        assert_eq!(out.extent.height, 0);
    }
}
