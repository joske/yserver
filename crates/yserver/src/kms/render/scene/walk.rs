use super::*;

impl WalkStats {
    /// True if some snapshot's captured damage projected entirely off this
    /// output — the only classification that may force a compose on the
    /// empty-damage path. See [`ContentDamage`].
    pub(super) fn off_output_damage_forces_compose(&self) -> bool {
        self.content_off_output > 0
    }

    pub(super) fn collapses(&self) -> u64 {
        self.collapses_mine
            + self.collapses_claim
            + self.collapses_taken
            + self.collapses_taken_skipped
    }
}

pub(super) fn children_index(windows: &crate::kms::render::backend::WindowsMap) -> ChildrenIndex {
    let mut by_parent: HashMap<u32, Vec<(u32, u64)>> = HashMap::new();
    for (xid, g) in windows {
        if let Some(parent) = g.parent {
            by_parent
                .entry(parent)
                .or_default()
                .push((*xid, g.stack_rank));
        }
    }
    by_parent
        .into_iter()
        .map(|(parent, mut children)| {
            children.sort_by_key(|(_, rank)| *rank);
            (parent, children.into_iter().map(|(xid, _)| xid).collect())
        })
        .collect()
}

/// #133 step 5 (5.2 / 5.3) — a node's INNER region as output-local rects:
/// Xorg's `winSize` (`SetWinSize`, `dix/window.c:1713`).
///
/// `content ∩ bounding-shape ∩ clip-shape`, with both shapes taken in
/// WINDOW-LOCAL coordinates and translated into the node's storage-local
/// space by `co`. `SetWinSize` does the same thing the other way round:
/// it translates `winSize` by `-drawable.x/y` into window-local coordinates,
/// intersects with `wBoundingShape` and then `wClipShape`, and translates
/// back (`:1735`-`:1742`). Also clamped to the ancestor visible box, so it
/// composes with parent-clipping exactly as `place` does.
///
/// The shape rect lists are XFixes regions — already validated as disjoint —
/// so the pairwise products stay disjoint and the result needs no union. An
/// explicitly EMPTY list (`Some(&[])`, the "draw nothing" shape) collapses
/// the result to empty, which is the DRIFT-1 distinction `set_shape_rectangles`
/// preserves.
#[allow(clippy::too_many_arguments)]
fn inner_place_rects(
    co: i32,
    own_w: i32,
    own_h: i32,
    vis: (i32, i32, i32, i32),
    bounding: Option<&[xfixes::RegionRect]>,
    clip: Option<&[xfixes::RegionRect]>,
    dx: i32,
    dy: i32,
    mode: Visibility,
    layout_w: u32,
    layout_h: u32,
) -> Vec<vk::Rect2D> {
    let (vis_lx0, vis_ly0, vis_lx1, vis_ly1) = vis;
    // The content box in storage-local coords, clamped to the ancestor
    // visible box. At `co == 0` this is the window rect — the same box
    // `place` starts from.
    let mut boxes: Vec<(i32, i32, i32, i32)> = Vec::new();
    let bx0 = co.max(vis_lx0);
    let by0 = co.max(vis_ly0);
    let bx1 = (co + own_w).min(vis_lx1);
    let by1 = (co + own_h).min(vis_ly1);
    if bx1 > bx0 && by1 > by0 {
        boxes.push((bx0, by0, bx1, by1));
    }
    for list in [bounding, clip].into_iter().flatten() {
        let mut next: Vec<(i32, i32, i32, i32)> = Vec::new();
        for &(ax0, ay0, ax1, ay1) in &boxes {
            for r in list {
                let sx0 = i32::from(r.x) + co;
                let sy0 = i32::from(r.y) + co;
                let sx1 = sx0 + i32::from(r.width);
                let sy1 = sy0 + i32::from(r.height);
                let ix0 = ax0.max(sx0);
                let iy0 = ay0.max(sy0);
                let ix1 = ax1.min(sx1);
                let iy1 = ay1.min(sy1);
                if ix1 > ix0 && iy1 > iy0 {
                    next.push((ix0, iy0, ix1, iy1));
                }
            }
        }
        boxes = next;
        if boxes.is_empty() {
            break;
        }
    }
    let out = boxes.into_iter().map(|(x0, y0, x1, y1)| vk::Rect2D {
        offset: vk::Offset2D {
            x: dx + x0,
            y: dy + y0,
        },
        extent: vk::Extent2D {
            width: u32::try_from(x1 - x0).unwrap_or(0),
            height: u32::try_from(y1 - y0).unwrap_or(0),
        },
    });
    match mode {
        // Same rule as `place`: under `On` a rect must lie on the output,
        // because the region algebra and the `src` back-translation are both
        // output-local.
        Visibility::On => {
            let output = vk::Extent2D {
                width: layout_w,
                height: layout_h,
            };
            out.filter_map(|r| clip_rect_to_output_extent(r, output))
                .collect()
        }
        Visibility::Off => out.collect(),
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn decide_node(
    host_xid: u32,
    geom: &crate::kms::render::backend::WindowGeometry,
    parent_content_abs_x: i32,
    parent_content_abs_y: i32,
    store: &DrawableStore,
    shape_bounding: &HashMap<u32, Vec<xfixes::RegionRect>>,
    shape_clip: &HashMap<u32, Vec<xfixes::RegionRect>>,
    layout_x0: i32,
    layout_y0: i32,
    layout_w: u32,
    layout_h: u32,
    mode: Visibility,
    under_redirected_ancestor: bool,
    under_cow_subtree: bool,
    clip_x0: i32,
    clip_y0: i32,
    clip_x1: i32,
    clip_y1: i32,
) -> NodeDecision {
    // #133 step 5 (P6) — the CONTENT OFFSET of the storage this node samples.
    //
    // Read from the ALLOCATION (`Drawable::content_offset`), never derived
    // from `geom.border_width`: step 6 re-bases the two TOGETHER (it moves
    // the pixels when it moves the offset), so the allocation is always the
    // truth about where the content is, and deriving the layout from live
    // geometry would move the coordinate system without moving the pixels
    // (that is the xts5 `Xlib9` `IncludeInferiors` regression documented on
    // `KmsBackend::storage_content_offset`).
    //
    // It is read from the SAMPLED drawable — `redirected_target` when the
    // window owns one — because that is where the pixels are: Xorg's
    // `compAllocPixmap` allocates the backing at the bordered extent, places
    // it at the OUTER origin, and records `bw` as its content offset
    // (`composite/compalloc.c:610`, then `compSetPixmap(pWin, pPixmap, bw)`
    // at `:620`).
    //
    // No allocation at all ⇒ fall back to `geom.border_width`. A window with
    // no storage has no pixels to re-base, so the invariant is not in play,
    // and its descendants must still be positioned against the content origin
    // the client was told about. `0` for every pre-#133 window either way.
    let lookup_id = store.lookup(host_xid);
    let layout_source_id = lookup_id.map(|id| store.redirected_target(id).unwrap_or(id));
    let co = layout_source_id
        .and_then(|id| store.get(id))
        .map_or_else(|| i32::from(geom.border_width), |d| d.content_offset);
    // Audit #3 — "this window counts as a redirected ancestor iff it owns its
    // own `redirected_target`", i.e. iff the drawable it samples is not
    // itself. Derived from `layout_source_id` rather than a second
    // `redirected_target` call, so the hoist adds no map lookup to the hot
    // path: the one `store.get` for `content_offset` above replaces the
    // `redirected_target` call this predicate used to make at the bottom of
    // the function, and `store_info` below now reuses `layout_source_id`.
    let self_owns_redirected_target = layout_source_id != lookup_id;

    // #133 step 5 (P6) — TWO absolutes per node. `geom.x` / `geom.y` are
    // parent-relative and, per X11, relative to the parent's CONTENT origin,
    // so the recurrence is:
    //
    //     child_outer   = parent_content + child.x/y
    //     child_content = child_outer    + child's own border width
    //
    // The OUTER absolute is where the node's storage origin lands (the border
    // ring lives at the storage origin) and how it occludes siblings
    // (`borderSize`); the CONTENT absolute is what descends as the child-clip
    // origin (`winSize`). Keeping only one of them is what left awesome's
    // inner child displaced by every ancestor's border width. Identical, term
    // for term, at `co == 0`.
    let abs_x = parent_content_abs_x + i32::from(geom.x);
    let abs_y = parent_content_abs_y + i32::from(geom.y);
    let content_abs_x = abs_x + co;
    let content_abs_y = abs_y + co;

    // The window's own rect in both spaces. `own_w` / `own_h` is the CONTENT
    // extent (what the client asked for and what GetGeometry reports);
    // `outer_w` / `outer_h` is `w + (bw << 1)` — Xorg's `borderSize` extent
    // (`dix/window.c:1770`) and exactly the storage this node samples.
    let own_w = i32::from(geom.width);
    let own_h = i32::from(geom.height);
    let outer_w = own_w + 2 * co;
    let outer_h = own_h + 2 * co;

    // #133 step 5 (5.4) — the redirection exception, and it is a RESET, not
    // an intersect. `mi/mivaltree.c:233` — "In redirected drawing case, reset
    // universe to borderSize" — and `RegionCopy(universe,
    // &pParent->borderSize)` at `:239` REPLACES the accumulated ancestor
    // region rather than narrowing it. `SetWinSize` and `SetBorderSize` agree
    // ("Redirected clients get clip list equal to their own geometry, not
    // clipped to their parent", `dix/window.c:1719` and `:1753`). The reset
    // happens BEFORE the `RegionIntersect(universe, universe,
    // &pParent->winSize)` at `:390`, so descendants inherit the
    // un-parent-clipped region too — that is the point of redirection: the
    // window owns a full-size pixmap.
    //
    // Keyed off owning a `redirected_target`, i.e. the walk's existing
    // `has_own_redirected_target`, and deliberately NOT split by mode: Xorg
    // keys it off `redirectDraw != RedirectDrawNone` with no
    // manual/automatic distinction. Manual redirect's extra behaviour
    // (`TreatAsTransparent`, `mivaltree.c:171` — it does not subtract from
    // its siblings' universe) is a separate rule and is already what the
    // walk's `manual_redirect_unconditional_skip` plus `opaque == false`
    // give.
    //
    // This resets the ancestor-clip RECTS only. The `Region` universe the
    // walk threads is the parent's own `mine`, mutated in place by the
    // recursion, so resetting it here would corrupt the parent's claim
    // accounting; leaving it narrowing is the conservative direction — it can
    // only clip a redirected window's draws further, never reveal a pixel
    // Xorg would hide.
    let (clip_x0, clip_y0, clip_x1, clip_y1) = if self_owns_redirected_target {
        (abs_x, abs_y, abs_x + outer_w, abs_y + outer_h)
    } else {
        (clip_x0, clip_y0, clip_x1, clip_y1)
    };

    // X11 parent-clipping. This window's visible box in its OWN
    // STORAGE-local coords = its OUTER rect [0,outer_w)×[0,outer_h)
    // intersected with the accumulated ancestor clip (translated into local
    // coords). Draws are restricted to this box; `vis_*` empty ⇒ nothing of
    // this window is visible (fully clipped by an ancestor). Storage-local
    // rather than content-local, because the pixels this node samples start
    // at the outer origin.
    let vis_lx0 = (clip_x0 - abs_x).max(0);
    let vis_ly0 = (clip_y0 - abs_y).max(0);
    let vis_lx1 = (clip_x1 - abs_x).min(outer_w);
    let vis_ly1 = (clip_y1 - abs_y).min(outer_h);
    // #133 step 5 (5.2) — the absolute clip passed down to children is the
    // ancestor clip ∩ this window's INNER (`winSize`) rect, never its outer
    // one: `mi/mivaltree.c:386` — "to make doubly sure that no child overlaps
    // the parent's border, we remove the parent's border from the universe
    // before proceeding" — followed by `RegionIntersect(universe, universe,
    // &pParent->winSize)` at `:390`. Simply widening the single rect that
    // used to serve both roles would have let children cover their parent's
    // border.
    let child_clip_x0 = clip_x0.max(content_abs_x);
    let child_clip_y0 = clip_y0.max(content_abs_y);
    let child_clip_x1 = clip_x1.min(content_abs_x + own_w);
    let child_clip_y1 = clip_y1.min(content_abs_y + own_h);

    // Project onto output-local coords. `dx` / `dy` is where the node's
    // STORAGE ORIGIN lands, i.e. the outer origin: `piece_draw` derives `src`
    // by translating a piece back by `(dx, dy)`, so it has to be the origin
    // of the texture being sampled.
    let dx = abs_x - layout_x0;
    let dy = abs_y - layout_y0;
    let win_w = outer_w;
    let win_h = outer_h;
    let intersects = !(dx + win_w <= 0
        || dy + win_h <= 0
        || dx >= i32::try_from(layout_w).unwrap_or(i32::MAX)
        || dy >= i32::try_from(layout_h).unwrap_or(i32::MAX));

    // Place: where this window's pixels land on the output, using the same
    // clamps the emitter always applied. Independent of the store, so a
    // non-emitting node still has a place for its descendants.
    let mut place: Vec<vk::Rect2D> = Vec::new();
    if let Some(rects) = shape_bounding.get(&host_xid) {
        for rect in rects {
            // #133 step 5 (5.3) — SHAPE regions are WINDOW-LOCAL, i.e.
            // relative to the CONTENT origin: `SetWinSize` translates
            // `winSize` by `-pWin->drawable.x/y` before intersecting and back
            // after (`dix/window.c:1736`), and `drawable.x` IS the content
            // origin. `place` is storage-local, whose origin is the OUTER
            // one, hence the `+ co`. Without it every shaped window's mask
            // would sit `bw` pixels off.
            let rx = i32::from(rect.x) + co;
            let ry = i32::from(rect.y) + co;
            let rw = i32::from(rect.width);
            let rh = i32::from(rect.height);
            // Clamp to the window's OUTER extent AND the ancestor visible box
            // (parent-clipping). This is `SetBorderSize`'s rule
            // (`dix/window.c:1747`): the expanded box ∩ the bounding shape —
            // the CLIP shape is deliberately absent here, it belongs to
            // `winSize` only. `SetBorderSize`'s trailing
            // `RegionUnion(borderSize, borderSize, winSize)` (`:1776`) is
            // provably a no-op, since `winSize = content ∩ bounding ∩ clip ⊆
            // outer ∩ bounding`, so it is not reproduced. Note the
            // consequence: a bounding shape that stops at the content edge
            // removes the border — correct, because the bounding region
            // defines the window's border-INCLUSIVE extent.
            let cx = rx.max(0).max(vis_lx0);
            let cy = ry.max(0).max(vis_ly0);
            let cw = (rx + rw).min(win_w).min(vis_lx1) - cx;
            let ch = (ry + rh).min(win_h).min(vis_ly1) - cy;
            if cw <= 0 || ch <= 0 {
                continue;
            }
            place.push(vk::Rect2D {
                offset: vk::Offset2D {
                    x: dx + cx,
                    y: dy + cy,
                },
                extent: vk::Extent2D {
                    width: u32::try_from(cw).unwrap_or(0),
                    height: u32::try_from(ch).unwrap_or(0),
                },
            });
        }
    } else if vis_lx1 > vis_lx0 && vis_ly1 > vis_ly0 {
        // Unshaped: the window rect clipped to the ancestor visible box.
        // Common case (child fits inside its parent) → box == full window.
        place.push(vk::Rect2D {
            offset: vk::Offset2D {
                x: dx + vis_lx0,
                y: dy + vis_ly0,
            },
            extent: vk::Extent2D {
                width: u32::try_from(vis_lx1 - vis_lx0).unwrap_or(0),
                height: u32::try_from(vis_ly1 - vis_ly0).unwrap_or(0),
            },
        });
    }

    // Step 1 — under `On`, place is output-clipped as well: the universe is the
    // output, and a piece must be translated back to window pixels for its
    // `src`, which only works if it lies on the output. Under `Off` the
    // pre-step-1 clamps stand and the rasteriser clips a straddling window.
    if mode == Visibility::On {
        let output = vk::Extent2D {
            width: layout_w,
            height: layout_h,
        };
        place = place
            .into_iter()
            .filter_map(|r| clip_rect_to_output_extent(r, output))
            .collect();
    }

    // #133 step 5 (5.2 / 5.3) — the INNER region (`winSize`) descendants are
    // clipped to: content ∩ bounding ∩ clip shape, every term in window-local
    // coordinates (`SetWinSize`, `dix/window.c:1713`). `SetWinSize`
    // intersects with BOTH shapes; bounding and clip are not interchangeable,
    // and neither is the input shape, which belongs to hit-testing (step 8)
    // and appears nowhere here.
    //
    // `None` = "identical to `place`", which holds exactly when the window
    // has no border AND no clip shape: `outer == content`, and the bounding
    // term is already folded into `place`. That is every window before #133,
    // so the `bw == 0` path costs one integer test plus one map lookup and
    // allocates nothing.
    let clip_shape = shape_clip.get(&host_xid);
    let child_place = if co == 0 && clip_shape.is_none() {
        None
    } else {
        Some(inner_place_rects(
            co,
            own_w,
            own_h,
            (vis_lx0, vis_ly0, vis_lx1, vis_ly1),
            shape_bounding.get(&host_xid).map(Vec::as_slice),
            clip_shape.map(Vec::as_slice),
            dx,
            dy,
            mode,
            layout_w,
            layout_h,
        ))
    };

    let store_info = lookup_id.and_then(|id| {
        let d = store.get(id)?;
        // Stage 4c.3 — route source-storage through `redirected_target`.
        // Both modes blit FROM B; W's geometry (dst_origin, dst_size,
        // intersect test) stays driven by W's own state in `windows`.
        // Only the sampled storage handle reroutes.
        let source_id = layout_source_id.unwrap_or(id);
        let source = store.get(source_id);
        let source_view_null = source.is_none_or(|s| s.storage.image_view == vk::ImageView::null());
        let source_view = source.map_or(vk::ImageView::null(), |s| s.storage.sample_view);
        let source_extent = source.map_or(vk::Extent2D::default(), |s| s.storage.extent);

        // Audit #3 (2026-05-19) — emit-or-skip is governed by
        // "is this window's storage where paint actually lands?"
        //
        //   has_own_redirected_target   self owns a `redirected_target`
        //                               → paint lands in its B, emit B.
        //   under_redirected_ancestor   some ancestor owns one
        //                               → paint lands in ancestor's B,
        //                                 ancestor emits it, we skip.
        //   d_part                      `scene_participating=true` —
        //                                 ordinary non-redirected window
        //                                 with its own storage as the
        //                                 paint target. Emit own storage.
        let has_own_redirected_target = source_id != id;
        // Phase 3.1 — Manual-redirected windows (own a
        // `redirected_target` AND `scene_participating=false`)
        // must NEVER emit to scanout. They go offscreen for the
        // compositor to read via NameWindowPixmap; the X server
        // must not also blit the backing in. Mirrors Xorg's
        // structural guarantee from `compCheckRedirect`.
        let is_manual_redirected = has_own_redirected_target && !d.scene_participating;
        let paint_target_is_self = !is_manual_redirected
            && (has_own_redirected_target || (d.scene_participating && !under_redirected_ancestor));

        // First failing gate, in production order.
        let skip_reason: Option<&'static str> = if is_manual_redirected {
            Some("manual_redirect_unconditional_skip")
        } else if !paint_target_is_self {
            if has_own_redirected_target {
                // Unreachable by construction (see `paint_target_is_self`);
                // kept so the cascade stays exhaustive if the rule evolves.
                Some("paint_target_not_self")
            } else if under_redirected_ancestor {
                Some("paint_target_is_redirected_ancestor")
            } else {
                Some("scene_participating=false")
            }
        } else if !matches!(d.kind, DrawableKind::Window) {
            Some("kind!=Window")
        } else if source_view_null {
            Some("source_image_view_null")
        } else if !intersects {
            Some("no_intersect_with_output")
        } else {
            None
        };

        Some(NodeStoreInfo {
            d_id: d.id,
            d_kind: d.kind,
            d_depth: d.depth,
            d_refcount: d.refcount,
            d_part: d.scene_participating,
            d_extent: d.storage.extent,
            d_view_null: d.storage.image_view == vk::ImageView::null(),
            source_id,
            source_view_null,
            source_view,
            source_extent,
            has_own_redirected_target,
            paint_target_is_self,
            skip_reason,
        })
    });

    let emits = store_info.is_some_and(|s| s.skip_reason.is_none()) && !place.is_empty();
    // Audit #3 — descendants need to know whether THEY sit under a
    // redirected ancestor. The chain is "this window counts as a redirected
    // ancestor iff it owns its own `redirected_target`" — exactly where
    // `resolve_paint_target` stops climbing the parent chain. Hoisted to the
    // top of this function by #133 step 5, which needs the same predicate for
    // the redirection reset.

    NodeDecision {
        abs_x,
        abs_y,
        content_abs_x,
        content_abs_y,
        dx,
        dy,
        win_w,
        win_h,
        vis_lx0,
        vis_ly0,
        vis_lx1,
        vis_ly1,
        child_clip_x0,
        child_clip_y0,
        child_clip_x1,
        child_clip_y1,
        intersects,
        lookup_id,
        store: store_info,
        place,
        child_place,
        emits,
        opaque: emits && !under_cow_subtree,
        child_under_redirected_ancestor: under_redirected_ancestor || self_owns_redirected_target,
    }
}

/// One node of the scene walk, mirroring Xorg's `miComputeClips`
/// (`mi/mivaltree.c:197`): decide, clip the children to this node's place,
/// visit them top → bottom, emit what they left of this node, then claim this
/// node's pixels from the caller's universe.
///
/// Coordinates: `parent_content_abs_x` / `parent_content_abs_y` are the
/// absolute (root-space) origin of this window's parent's CONTENT — the
/// parent's outer origin plus the parent's border width. The window's own
/// position (`geom.x`, `geom.y`) is parent-relative per X11 and measured from
/// exactly that origin, so this window's OUTER absolute origin is
/// `parent_content + geom`, and its own CONTENT origin is that plus its own
/// border width (#133 step 5, P6). We project onto the output by subtracting
/// the output's layout origin. The two collapse into one at `bw == 0`, which
/// is the pre-#133 single-absolute recurrence.
///
/// A child is only visible if every ancestor in the chain is mapped;
/// `unmapped` short-circuits the entire subtree (matches X11
/// MapWindow semantics — an unmapped parent hides all descendants).
///
/// The per-node decision lives in [`decide_node`]; this function logs it,
/// runs the visibility algebra, and recurses. Under `Visibility::Off` the
/// universe is never read and every place rect is emitted, which — with the
/// sink's final reversal — reproduces the pre-step-1 emitter byte for byte.
///
/// # The cap, and the one safe direction
///
/// `universe` and `mine` are only ever intersected and subtracted, so when the
/// 32-box cap collapses one to its bounding box the result is a **superset** of
/// the truly unclaimed area: nodes below emit more than needed (harmless under
/// painter's order), never less. `mine` is the one union in the walk (a union
/// of `universe ∩ r` over the place rects); when it collapses, children are
/// clipped to the parent's bounding box — what the emitter did before step 1.
/// `place` itself is never a `Region`: a collapsed place would claim shape
/// holes, and that is a hole nothing fills.
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)]
pub(super) fn visit_window_subtree(
    host_xid: u32,
    parent_content_abs_x: i32,
    parent_content_abs_y: i32,
    store: &mut DrawableStore,
    windows: &crate::kms::render::backend::WindowsMap,
    children: &ChildrenIndex,
    // Per-window SHAPE bounding regions (`KmsCore::shape_bounding`).
    // When a host xid has an entry the window's scene draw is
    // clipped to those rects — marco's rounded-corner frame masks
    // depend on this. Empty / missing entry → unshaped, single
    // full-window draw.
    shape_bounding: &HashMap<u32, Vec<xfixes::RegionRect>>,
    // Per-window SHAPE CLIP regions (`KmsCore::shape_clip`). #133 step 5
    // (5.3): the clip shape narrows `winSize` — the region DESCENDANTS are
    // clipped to — and nothing else. `SetWinSize` intersects with both the
    // bounding and the clip shape (`dix/window.c:1735`); `SetBorderSize`
    // intersects with the bounding shape only (`:1772`), so the clip shape
    // never touches the node's own `place`.
    shape_clip: &HashMap<u32, Vec<xfixes::RegionRect>>,
    layout_x0: i32,
    layout_y0: i32,
    layout_w: u32,
    layout_h: u32,
    mode: Visibility,
    // What nothing above this node has claimed yet, output-local. This node's
    // subtree subtracts what it opaquely covers on the way out.
    universe: &mut Region,
    sink: &mut WalkSink<'_>,
    // Audit #3 (2026-05-19): true iff some ancestor on the recursion
    // path owns a `redirected_target`. When set, this window's paint
    // landed in that ancestor's backing (via `resolve_paint_target`'s
    // ancestor walk), so emitting this window's own storage would
    // show stale/empty pixels — the ancestor's emit already shows
    // the content. A descendant that owns ITS OWN `redirected_target`
    // breaks this chain (its paint stops at itself), so it still
    // emits its own backing regardless of the inherited flag.
    under_redirected_ancestor: bool,
    // Phase 2.6 — true iff the current recursion path entered the
    // COW top-level (or one of its descendants). When set, emitted
    // `CompositeDraw` entries take `alpha_passthrough = true` so the
    // compositor's composited result blends over the layer below;
    // outside the COW subtree (no compositor active) draws stay
    // opaque (`alpha_passthrough = false`). Mirrors the threading of
    // `under_redirected_ancestor` above.
    under_cow_subtree: bool,
    // X11 parent-clipping: a window's visible region is the
    // intersection of its own rectangle with EVERY ancestor's
    // rectangle. These are the accumulated ancestor bounds in absolute
    // screen coords (half-open [x0,x1) × [y0,y1)); this window's draw
    // and its descendants' clips are intersected against them. The
    // top-level call passes effectively-unbounded bounds (top-levels
    // are screen-clipped by the output-extent gate), so this is a
    // no-op for the common case where children fit inside their
    // parents — it only bites a child that extends beyond its parent,
    // e.g. an fvwm frame decoration parked in a tiny holding window.
    // Under `On` the universe clips children to the parent's place as well,
    // which is what also clips them to the parent's SHAPE.
    clip_x0: i32,
    clip_y0: i32,
    clip_x1: i32,
    clip_y1: i32,
) {
    let debug_focus = scene_walk_debug_enabled_for(host_xid);
    // Stage 4 diagnostic: trace-level scene-walk decision per window.
    // Enable with `RUST_LOG=yserver::kms::render::scene=trace`. The
    // top-level and descendant paths share this function so the
    // single trace site covers both. Format is greppable —
    // `render scene_walk xid=...: ...` — for `grep "render scene_walk"`
    // over yserver-hw.log to extract just these lines.
    let Some(geom) = windows.get(&host_xid) else {
        log::trace!("render scene_walk xid={host_xid:#x}: SKIP reason=geom_not_in_windows");
        if debug_focus {
            log::debug!("render scene_walk xid={host_xid:#x}: SKIP reason=geom_not_in_windows");
        }
        return;
    };
    if !geom.mapped {
        // X11: an unmapped window (and entire subtree) is invisible.
        log::trace!(
            "render scene_walk xid={host_xid:#x}: SKIP reason=geom_unmapped \
             geom=({x},{y} {w}x{h}) depth={depth} parent={parent:?}",
            x = geom.x,
            y = geom.y,
            w = geom.width,
            h = geom.height,
            depth = geom.depth,
            parent = geom.parent,
        );
        if debug_focus {
            log::debug!(
                "render scene_walk xid={host_xid:#x}: SKIP reason=geom_unmapped \
                 geom=({x},{y} {w}x{h}) depth={depth} parent={parent:?}",
                x = geom.x,
                y = geom.y,
                w = geom.width,
                h = geom.height,
                depth = geom.depth,
                parent = geom.parent,
            );
        }
        return;
    }

    let node = decide_node(
        host_xid,
        geom,
        parent_content_abs_x,
        parent_content_abs_y,
        store,
        shape_bounding,
        shape_clip,
        layout_x0,
        layout_y0,
        layout_w,
        layout_h,
        mode,
        under_redirected_ancestor,
        under_cow_subtree,
        clip_x0,
        clip_y0,
        clip_x1,
        clip_y1,
    );
    sink.stats.nodes_visited += 1;
    // OUTER absolute — what the node samples and occludes with.
    let abs_x = node.abs_x;
    let abs_y = node.abs_y;

    // Manual-redirect subtree boundary. When a window is
    // `scene_participating=false` here, the compositor owns the
    // entire subtree's presentation (X11 Composite §285+360 —
    // Manual-mode redirect removes the window AND its descendants
    // from normal scene-out; the compositor reads the redirected
    // backing instead).
    //
    // Audit #3 (2026-05-19): the old `prune_subtree=true` for
    // `scene_participating=false` is gone — Automatic descendants of
    // Manual ancestors need to recurse so they can emit their own
    // backing. Per-window emit-vs-skip is decided by
    // `paint_target_is_self` in `decide_node`; the recurse always
    // runs and the `under_redirected_ancestor` flag carries the
    // chain context.
    if node.lookup_id.is_none() {
        log::trace!(
            "render scene_walk xid={host_xid:#x}: SKIP reason=no_store_lookup \
             geom=({x},{y} {w}x{h}) mapped=true depth={depth}",
            x = geom.x,
            y = geom.y,
            w = geom.width,
            h = geom.height,
            depth = geom.depth,
        );
        if debug_focus {
            log::debug!(
                "render scene_walk xid={host_xid:#x}: SKIP reason=no_store_lookup \
                 geom=({x},{y} {w}x{h}) mapped=true depth={depth}",
                x = geom.x,
                y = geom.y,
                w = geom.width,
                h = geom.height,
                depth = geom.depth,
            );
        }
    }
    if node.lookup_id.is_some() {
        if let Some(s) = node.store {
            let NodeStoreInfo {
                d_id,
                d_kind,
                d_depth,
                d_refcount,
                d_part,
                d_extent,
                d_view_null,
                source_id,
                source_view_null,
                has_own_redirected_target,
                paint_target_is_self,
                skip_reason,
                ..
            } = s;
            let dx = node.dx;
            let dy = node.dy;
            let win_w = node.win_w;
            let win_h = node.win_h;
            let intersects = node.intersects;

            if debug_focus {
                log::debug!(
                    "render scene_walk focus xid={host_xid:#x} source_id={source_id:?} \
                     has_own_redirected_target={has_own_redirected_target} \
                     under_redirected_ancestor={under_redirected_ancestor} \
                     paint_target_is_self={paint_target_is_self} \
                     intersects={intersects} skip_reason={skip_reason:?}",
                );
            }

            if let Some(reason) = skip_reason {
                log::trace!(
                    "render scene_walk xid={host_xid:#x}: SKIP reason={reason} \
                     geom=({gx},{gy} {gw}x{gh}) mapped=true \
                     store_id={d_id:?} kind={d_kind:?} depth={d_depth} \
                     refcount={d_refcount} scene_participating={d_part} \
                     storage_extent={dew}x{deh} image_view_null={d_view_null} \
                     source_id={source_id:?} source_view_null={source_view_null}",
                    gx = geom.x,
                    gy = geom.y,
                    gw = geom.width,
                    gh = geom.height,
                    dew = d_extent.width,
                    deh = d_extent.height,
                );
                if debug_focus {
                    log::debug!(
                        "render scene_walk xid={host_xid:#x}: SKIP reason={reason} \
                         geom=({gx},{gy} {gw}x{gh}) mapped=true \
                         store_id={d_id:?} kind={d_kind:?} depth={d_depth} \
                         refcount={d_refcount} scene_participating={d_part} \
                         storage_extent={dew}x{deh} image_view_null={d_view_null} \
                         source_id={source_id:?} source_view_null={source_view_null}",
                        gx = geom.x,
                        gy = geom.y,
                        gw = geom.width,
                        gh = geom.height,
                        dew = d_extent.width,
                        deh = d_extent.height,
                    );
                }
            } else {
                log::trace!(
                    "render scene_walk xid={host_xid:#x}: WILL_EMIT \
                     geom=({gx},{gy} {gw}x{gh}) abs=({abs_x},{abs_y}) \
                     output=({dx},{dy} {win_w}x{win_h}) \
                     store_id={d_id:?} kind={d_kind:?} depth={d_depth} \
                     refcount={d_refcount} scene_participating={d_part} \
                     storage_extent={dew}x{deh} image_view_null={d_view_null} \
                     source_id={source_id:?}",
                    gx = geom.x,
                    gy = geom.y,
                    gw = geom.width,
                    gh = geom.height,
                    dew = d_extent.width,
                    deh = d_extent.height,
                );
                if debug_focus {
                    log::debug!(
                        "render scene_walk xid={host_xid:#x}: WILL_EMIT \
                         geom=({gx},{gy} {gw}x{gh}) abs=({abs_x},{abs_y}) \
                         output=({dx},{dy} {win_w}x{win_h}) \
                         store_id={d_id:?} kind={d_kind:?} depth={d_depth} \
                         refcount={d_refcount} scene_participating={d_part} \
                         storage_extent={dew}x{deh} image_view_null={d_view_null} \
                         source_id={source_id:?}",
                        gx = geom.x,
                        gy = geom.y,
                        gw = geom.width,
                        gh = geom.height,
                        dew = d_extent.width,
                        deh = d_extent.height,
                    );
                }
            }
        } else {
            log::trace!(
                "render scene_walk xid={host_xid:#x}: SKIP reason=store_get_returned_none \
                 store_id={lookup_id:?} geom=({x},{y} {w}x{h}) mapped=true depth={depth}",
                lookup_id = node.lookup_id,
                x = geom.x,
                y = geom.y,
                w = geom.width,
                h = geom.height,
                depth = geom.depth,
            );
            if debug_focus {
                log::debug!(
                    "render scene_walk xid={host_xid:#x}: SKIP reason=store_get_returned_none \
                     store_id={lookup_id:?} geom=({x},{y} {w}x{h}) mapped=true depth={depth}",
                    lookup_id = node.lookup_id,
                    x = geom.x,
                    y = geom.y,
                    w = geom.width,
                    h = geom.height,
                    depth = geom.depth,
                );
            }
        }
    }

    // Step 2 of the walk: `mine = ⋃ᵣ (universe ∩ r)` over the exact place
    // rects — what this node and its descendants may still paint. Clipping the
    // children to the parent's PLACE (rect ∩ ancestors ∩ shape), not its
    // bounding box, is the parent-bounding-shape fix: Xorg's child universe is
    // `∩ borderSize`, and `borderSize` is shape-clipped. This is the one union
    // in the walk; a collapse degrades to "children clipped to the parent's
    // bbox", which is exactly the pre-step-1 behaviour.
    //
    // Leaf fast path: a node with no children needs no `mine` at all. Its
    // pieces are `universe ∩ r` per place rect (which is exactly `mine ∩ r`,
    // since `mine ∩ rⱼ = ⋃ᵢ (universe ∩ rᵢ ∩ rⱼ) = universe ∩ rⱼ`), and a
    // non-opaque leaf has no descendants to claim for. e16's shaped
    // decorations are almost all leaves with many rects, so this removes both
    // the union and the collapses it caused — a leaf's pieces are exact where
    // a collapsed `mine` would have made them a superset.
    let kids = children.get(&host_xid).filter(|k| !k.is_empty());
    let is_leaf = kids.is_none();
    // #133 step 5 (5.2) — `mine` is built from the node's INNER region
    // (`winSize`), not from its `place` (`borderSize`): `mi/mivaltree.c:390`
    // intersects the child universe with `winSize` precisely so that "no
    // child overlaps the parent's border" (`:386`). `child_place` is `None`
    // for every borderless, clip-shape-less window, where the two regions
    // ARE the same rect list — the `bw == 0` path reads exactly the vector it
    // read before, through the same code below.
    let child_region_rects: &[vk::Rect2D] = node.child_place.as_deref().unwrap_or(&node.place);
    let mut mine = Region::new();
    // #133 step 5 (5.2) — this node's OWN region that lies outside its inner
    // region: the border ring. Xorg keeps the same two things apart —
    // `borderClip = universe` is taken BEFORE `universe ∩= winSize`
    // (`mi/mivaltree.c:373` and `:390`), and the ring it paints is
    // `borderClip − winSize` (`dix/window.c:1586`). No child can occupy it,
    // so it is held aside while the children claim from `mine` and unioned
    // back before this node emits. Empty — and not even computed — whenever
    // `child_place` is `None`, i.e. for every `bw == 0` window without a clip
    // shape.
    let mut ring = Region::new();
    if mode == Visibility::On && !is_leaf {
        match node.place.as_slice() {
            [] => {}
            [only] => mine = universe.clip_to_rect(*only),
            many => {
                for r in many {
                    let piece = universe.clip_to_rect(*r);
                    if mine.union_with_reporting(&piece) {
                        sink.stats.collapses_mine += 1;
                    }
                }
            }
        }
        if node.child_place.is_some() {
            // Split `mine` into the part descendants may claim (`∩ winSize`)
            // and the part they may not (the ring).
            let mut inner = Region::new();
            for r in child_region_rects {
                let piece = mine.clip_to_rect(*r);
                if inner.union_with_reporting(&piece) {
                    sink.stats.collapses_mine += 1;
                }
            }
            ring = mine;
            if ring.subtract_reporting(&inner) {
                sink.stats.collapses_mine += 1;
            }
            mine = inner;
        }
    }

    // Step 3: children TOP → BOTTOM, each claiming from `mine` on the way out.
    // (Computation order; the sink is reversed into painter's order at the
    // end of the walk.)
    if let Some(kids) = kids {
        for &child_xid in kids.iter().rev() {
            visit_window_subtree(
                child_xid,
                // #133 step 5 (P6) — children are positioned against this
                // window's CONTENT origin, not its outer one. This is the
                // second half of the two-absolute recurrence; passing the
                // outer origin here is what displaced awesome's inner child
                // by `bw` at every level.
                node.content_abs_x,
                node.content_abs_y,
                store,
                windows,
                children,
                shape_bounding,
                shape_clip,
                layout_x0,
                layout_y0,
                layout_w,
                layout_h,
                mode,
                &mut mine,
                sink,
                node.child_under_redirected_ancestor,
                // Phase 2.6 — COW subtree flag is inherited unchanged.
                // Once we entered the COW top-level, every descendant
                // emits with alpha_passthrough=true.
                under_cow_subtree,
                // Parent-clipping: children are clipped to this window's
                // rect intersected with the inherited ancestor clip.
                node.child_clip_x0,
                node.child_clip_y0,
                node.child_clip_x1,
                node.child_clip_y1,
            );
        }
    }

    // #133 step 5 (5.2) — the ring rejoins this node's region now that the
    // children are done: they claimed only from `mine ∩ winSize`, and the
    // ring is what `borderClip − winSize` leaves. A no-op (an empty region,
    // never even built) at `bw == 0` without a clip shape.
    if !ring.is_empty() && mine.union_with_reporting(&ring) {
        sink.stats.collapses_mine += 1;
    }

    // Step 4: emit what the children left of this node. The presence is pushed
    // in step 6, after the claim step has finished reading `place`.
    let mut emitted_presence: Option<(Emitted, ParticipantId)> = None;
    if node.emits
        && let Some(s) = node.store
    {
        // Window scene draw — bind the sample-side view
        // (format/depth-aware swizzle) instead of the raw
        // IDENTITY-swizzle attachment view. This is the
        // load-bearing fix for the "depth-24 windows / COW α
        // leak" bug: the BgraNoAlpha swizzle forced α=ONE for
        // depth-24 used to live ONLY in the engine's RENDER
        // view-cache, never on the scene path. Combined with
        // `alpha_passthrough=true` in the COW subtree, the
        // prior IDENTITY view leaked the BGRA8 padding byte
        // (typically 0) into the shader's `src.a`, blending
        // depth-24 windows with α=0 — invisible against root.
        //
        // One draw per visible piece of each `place` rect: a SHAPE
        // bounding region (marco's rounded-corner mask, panel-applet
        // cutouts) yields one clipped draw per rect; the unshaped,
        // uncovered common case yields the single full-window draw
        // with src [0,0]-[1,1]. Pixels outside the bounding region
        // are intentionally NOT drawn so the layer below (parent /
        // wallpaper / root) shows through.
        //
        // Phase 2.6 — alpha-passthrough is inherited from the
        // COW subtree flag (set on the COW top-level +
        // descendants). Outside the COW subtree, draws stay
        // opaque (no compositor path); inside it, the
        // compositor's stage paints with alpha and we blend
        // over whatever lies below.
        //
        // `src` denominators: under `Off` the host window size, which is
        // what the pre-step-1 emitter divided by; under `On` the SAMPLED
        // source's extent. They differ only for a redirected window whose
        // backing is larger than its host geometry (the bordered extent
        // carries the ring; #143 made `redirected_backing_can_fit` exact,
        // so a resize no longer leaves an oversized backing behind),
        // where the window's content sits at the backing's origin
        // (`resolve_paint_target` routes with offset (0,0)) and dividing by
        // the host size stretches it — the pre-step-1 behaviour, kept under
        // `Off` only so the audit's reference stays comparable frame for
        // frame.
        let (denom_w, denom_h) = match mode {
            Visibility::Off => (node.win_w, node.win_h),
            Visibility::On => (
                i32::try_from(s.source_extent.width).unwrap_or(i32::MAX),
                i32::try_from(s.source_extent.height).unwrap_or(i32::MAX),
            ),
        };
        let out = emit_node(
            sink,
            mode,
            if is_leaf { universe } else { &mine },
            &node.place,
            node.dx,
            node.dy,
            denom_w,
            denom_h,
            s.source_view,
            under_cow_subtree,
            s.source_id,
            store,
            layout_w,
            layout_h,
        );
        // Identity is the host drawable, so a redirect swap is a resample
        // rather than a replacement.
        emitted_presence = Some((
            out,
            ParticipantId {
                role: SceneRole::Window,
                xid: host_xid,
                generation: s.d_id.as_u64(),
            },
        ));
    }

    // Step 5: claim from the caller's universe. An opaque node takes its whole
    // place (its visible pieces plus everything its descendants took, which
    // lie inside it — X11 clips them to the parent). A non-opaque node (COW
    // subtree, manual-redirected, no storage, off-output) takes only what its
    // descendants took: per place rect, `r − mine_after_children`, never
    // through a union of the place rects. Subtraction of exact rects from a
    // capped region can only leave a superset — the safe direction. The one
    // way to over-claim here is `taken` itself collapsing to its bounding box,
    // so a collapsed `taken` is not claimed at all.
    if mode == Visibility::On {
        // The full invariant check (`universe_after ⊆ universe_before`) clones
        // and subtracts a region per node — real work under the
        // `-C debug-assertions=yes` builds every HW recipe and the reporter
        // use — so it is test-only; the pixel-oracle tests exercise every
        // branch of this step. Only the allocation-free check stays a
        // `debug_assert!`.
        #[cfg(test)]
        let before = universe.clone();
        let mut collapsed_here = false;
        if node.opaque {
            for r in &node.place {
                // Nothing to subtract if nothing above left this rect in the
                // universe — the common case for a covered window.
                if !universe.intersects_rect(*r) {
                    continue;
                }
                if universe.subtract_reporting(&Region::from_rect(*r)) {
                    sink.stats.collapses_claim += 1;
                    collapsed_here = true;
                }
            }
        } else if !is_leaf {
            // A non-opaque leaf has no descendants, so it claims nothing; a
            // non-opaque parent claims what its descendants took.
            //
            // #133 step 5 (5.2) — from the INNER rects, because that is where
            // the descendants were confined. A non-opaque node does not draw
            // its own ring either (it is manual-redirected, in the COW
            // subtree, storage-less or off-output), so claiming the OUTER
            // rect would punch a hole in the parent exactly the width of a
            // border nobody painted. Identical to `place` at `bw == 0`.
            for r in child_region_rects {
                if !universe.intersects_rect(*r) {
                    continue;
                }
                let mut taken = Region::from_rect(*r);
                if taken.subtract_reporting(&mine) {
                    // Superset: claiming it could hide something visible.
                    sink.stats.collapses_taken_skipped += 1;
                    collapsed_here = true;
                    continue;
                }
                if universe.subtract_reporting(&taken) {
                    sink.stats.collapses_taken += 1;
                    collapsed_here = true;
                }
            }
        }
        // The invariants hold exactly unless the cap collapsed the universe to
        // its bounding box — which is a superset by design and may well
        // exceed `before` (it fills the holes higher siblings left). That is
        // the documented safe direction, not a bug, so it is not asserted.
        #[cfg(test)]
        if !collapsed_here {
            assert!(
                before.contains(universe),
                "visibility walk: universe grew at xid={host_xid:#x}"
            );
        }
        if !collapsed_here && node.opaque {
            for r in &node.place {
                debug_assert!(
                    !universe.intersects_rect(*r),
                    "visibility walk: opaque place still in universe at xid={host_xid:#x}"
                );
            }
        }
    }

    // Step 6: the presence, consuming the decision's `place` — nothing reads it
    // after the claim step, so it moves into the presence uncopied.
    if let Some((out, participant)) = emitted_presence {
        push_presence(sink, node.place, out, participant);
    }
}
