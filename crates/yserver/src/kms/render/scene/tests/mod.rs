mod borders;
mod build_scene;
mod cursor;
mod partial_compose;
mod projection;
mod tick;
mod walk;
use super::*;

fn audit_rect(x: i32, y: i32, width: u32, height: u32) -> vk::Rect2D {
    vk::Rect2D {
        offset: vk::Offset2D { x, y },
        extent: vk::Extent2D { width, height },
    }
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

fn extent(w: u32, h: u32) -> vk::Extent2D {
    vk::Extent2D {
        width: w,
        height: h,
    }
}

// ── Step 4: the gates that make clipping safe ─────────────────

fn draw_at(x: f32, y: f32, w: f32, h: f32, alpha_passthrough: bool) -> CompositeDraw {
    CompositeDraw {
        image_view: vk::ImageView::null(),
        dst_origin: [x, y],
        dst_size: [w, h],
        src_origin: [0.0, 0.0],
        src_size: [1.0, 1.0],
        alpha_passthrough,
    }
}

fn region_of(rects: &[vk::Rect2D]) -> Region {
    Region::from_rects(rects.iter().copied())
}

// ── Stage 3f.6: subwindow scene traversal ─────────────────────

fn alloc_stub_window(
    store: &mut DrawableStore,
    windows: &mut crate::kms::render::backend::WindowsMap,
    xid: u32,
    x: i16,
    y: i16,
    w: u16,
    h: u16,
    parent: Option<u32>,
    mapped: bool,
) {
    // for_tests_null gives null image handles; build_scene
    // rejects null views. Use a non-zero sentinel handle so the
    // traversal test exercises the recurse logic. The handle
    // never gets passed to Vk because the test never composes.
    let mut storage = crate::kms::render::store::Storage::for_tests_null(
        extent(u32::from(w), u32::from(h)),
        vk::Format::B8G8R8A8_UNORM,
    );
    // SAFETY: Vk handle types are opaque u64s; constructing a
    // sentinel doesn't touch the driver. The `is_test_stub`
    // flag on Storage means Drop won't try to destroy these.
    // Stamp both views to the same sentinel so build_scene's
    // sample-side bind (`storage.sample_view`) sees the same
    // handle the legacy tests asserted against — these stubs
    // don't exercise α swizzle, just storage-routing.
    let sentinel: ash::vk::ImageView = ash::vk::Handle::from_raw(u64::from(xid) | 0xFF00_0000);
    storage.image_view = sentinel;
    storage.sample_view = sentinel;
    store
        .allocate(xid, DrawableKind::Window, 32, mapped, storage)
        .expect("stub allocate");
    windows.insert(
        xid,
        crate::kms::render::backend::WindowGeometry {
            border_width: 0,
            border_pixel: None,
            border_pixmap: None,
            x,
            y,
            width: w,
            height: h,
            depth: 32,
            mapped,
            viewable: true,
            parent,
            stack_rank: 0,
            bg_pixel: None,
            bg_pixmap: None,
            cursor: None,
        },
    );
}

// ── Step 1 stage A: the refactored emitter is a no-op ─────────────
//
// `legacy_emit_window_subtree` is the emitter as it stood before the
// per-node decision was factored out (`decide_node`) and the children
// index replaced the per-node `WindowsMap` scan. It is kept verbatim so the
// refactor can be checked against it on trees the WM-shaped tests do not
// build: deep nesting, overlapping siblings, shaped nodes, children that
// extend beyond their parent, manual/automatic redirect chains, a COW
// subtree, a non-zero layout origin, a window straddling the output edge.
// Delete it together with this test once stage B changes what is emitted.
/// Verbatim pre-refactor emitter (2026-09-03), test twin only.
fn legacy_emit_window_subtree(
    host_xid: u32,
    parent_abs_x: i32,
    parent_abs_y: i32,
    store: &mut DrawableStore,
    windows: &crate::kms::render::backend::WindowsMap,
    // Per-window SHAPE bounding regions (`KmsCore::shape_bounding`).
    // When a host xid has an entry the window's scene draw is
    // clipped to those rects — marco's rounded-corner frame masks
    // depend on this. Empty / missing entry → unshaped, single
    // full-window draw.
    shape_bounding: &HashMap<u32, Vec<xfixes::RegionRect>>,
    layout_x0: i32,
    layout_y0: i32,
    layout_w: u32,
    layout_h: u32,
    draws: &mut Vec<CompositeDraw>,
    snapshots: &mut Vec<DamageSnapshot>,
    sampled_ids: &mut Vec<crate::kms::render::store::DrawableId>,
    projected: &mut RegionSet,
    // Step 2 — one presence per participant that emits, region derived from the
    // draws it pushed. Threaded rather than returned so the recursion can append
    // in emission order.
    participants: &mut Vec<ScenePresence>,
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
    let abs_x = parent_abs_x + i32::from(geom.x);
    let abs_y = parent_abs_y + i32::from(geom.y);

    // X11 parent-clipping. This window's visible box in its OWN local
    // coords = its rect [0,own_w)×[0,own_h) intersected with the
    // accumulated ancestor clip (translated into local coords). Draws
    // are restricted to this box; descendants inherit the intersection
    // (in absolute coords) as their clip. `vis_*` empty ⇒ nothing of
    // this window is visible (fully clipped by an ancestor).
    let own_w = i32::from(geom.width);
    let own_h = i32::from(geom.height);
    let vis_lx0 = (clip_x0 - abs_x).max(0);
    let vis_ly0 = (clip_y0 - abs_y).max(0);
    let vis_lx1 = (clip_x1 - abs_x).min(own_w);
    let vis_ly1 = (clip_y1 - abs_y).min(own_h);
    // Absolute clip passed down to children = ancestor clip ∩ own rect.
    let child_clip_x0 = clip_x0.max(abs_x);
    let child_clip_y0 = clip_y0.max(abs_y);
    let child_clip_x1 = clip_x1.min(abs_x + own_w);
    let child_clip_y1 = clip_y1.min(abs_y + own_h);

    // Manual-redirect subtree boundary. When a window is
    // `scene_participating=false` here, the compositor owns the
    // entire subtree's presentation (X11 Composite §285+360 —
    // Manual-mode redirect removes the window AND its descendants
    // from normal scene-out; the compositor reads the redirected
    // backing instead). Set after the per-node decision so we
    // can return *after* the SKIP trace fires (preserves the
    // existing trace shape for live debugging) and before the
    // child-recurse below.
    //
    // Audit #3 (2026-05-19): the old `prune_subtree=true` for
    // `scene_participating=false` is gone — Automatic descendants of
    // Manual ancestors need to recurse so they can emit their own
    // backing. Per-window emit-vs-skip is decided by
    // `paint_target_is_self` below; the recurse always runs and the
    // `under_redirected_ancestor` flag carries the chain context.

    // Emit a draw entry for this window if it has live storage that
    // participates in the scene.
    let lookup_id = store.lookup(host_xid);
    if lookup_id.is_none() {
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
    if let Some(id) = lookup_id {
        // Pull diagnostic fields up front (cheap copies) so we can
        // emit a single SKIP/WILL_EMIT trace line per gate failure
        // without re-borrowing the store across log call sites.
        let drawable_snap = store.get(id).map(|d| {
            (
                d.id,
                d.kind,
                d.depth,
                d.refcount,
                d.scene_participating,
                d.storage.extent,
                d.storage.image_view == vk::ImageView::null(),
            )
        });
        if let Some((d_id, d_kind, d_depth, d_refcount, d_part, d_extent, d_view_null)) =
            drawable_snap
        {
            // Stage 4c.3 — route source-storage through `redirected_target`.
            // Both modes blit FROM B; W's geometry (dst_origin, dst_size,
            // intersect test) stays driven by W's own state in
            // `windows`. Only the sampled storage handle reroutes.
            let source_id = store.redirected_target(id).unwrap_or(id);
            let source_view_null = store
                .get(source_id)
                .is_none_or(|s| s.storage.image_view == vk::ImageView::null());

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
            //
            // Pre-fix the rule was `d_part || manual_backing_visible`
            // plus an unconditional `prune_subtree` on
            // `scene_participating=false`. That dropped Automatic-
            // redirected descendants of Manual-redirected ancestors —
            // GTK/marco CSD frames lose their inner widgets (per audit
            // #3 / Control Center missing-widget reports).
            let has_own_redirected_target = source_id != id;
            // Phase 3.1 — Manual-redirected windows (own a
            // `redirected_target` AND `scene_participating=false`)
            // must NEVER emit to scanout. They go offscreen for the
            // compositor to read via NameWindowPixmap; the X server
            // must not also blit the backing in. Mirrors Xorg's
            // structural guarantee from `compCheckRedirect`.
            let is_manual_redirected = has_own_redirected_target && !d_part;
            let paint_target_is_self = !is_manual_redirected
                && (has_own_redirected_target || (d_part && !under_redirected_ancestor));

            // Project onto output-local coords (computed once here so
            // both the SKIP=no_intersect and WILL_EMIT trace lines can
            // include the dst rect).
            let dx = abs_x - layout_x0;
            let dy = abs_y - layout_y0;
            let win_w = i32::from(geom.width);
            let win_h = i32::from(geom.height);
            let intersects = !(dx + win_w <= 0
                || dy + win_h <= 0
                || dx >= i32::try_from(layout_w).unwrap_or(i32::MAX)
                || dy >= i32::try_from(layout_h).unwrap_or(i32::MAX));

            // Pick the first failing gate and emit a single SKIP line;
            // otherwise emit WILL_EMIT. Order matches the production
            // gate ordering below so the trace mirrors the live path.
            let skip_reason: Option<&'static str> = if is_manual_redirected {
                // Phase 3.1 — first reason in the cascade. A
                // Manual-redirected window (own redirected_target +
                // scene_participating=false) is unconditionally
                // skipped; the compositor reads its backing via
                // NameWindowPixmap and re-emits it on the COW.
                Some("manual_redirect_unconditional_skip")
            } else if !paint_target_is_self {
                if has_own_redirected_target {
                    // Defensive — `paint_target_is_self` is true when
                    // `has_own_redirected_target` AND not
                    // Manual-redirected (the Manual case is handled
                    // by the branch above), so this branch is
                    // unreachable. Kept so the match stays exhaustive
                    // if the rule ever evolves.
                    Some("paint_target_not_self")
                } else if under_redirected_ancestor {
                    Some("paint_target_is_redirected_ancestor")
                } else {
                    Some("scene_participating=false")
                }
            } else if !matches!(d_kind, DrawableKind::Window) {
                Some("kind!=Window")
            } else if source_view_null {
                Some("source_image_view_null")
            } else if !intersects {
                Some("no_intersect_with_output")
            } else {
                None
            };

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

            if matches!(d_kind, DrawableKind::Window)
                && let Some(source) = store.get(source_id)
                && source.storage.image_view != vk::ImageView::null()
                && intersects
                && paint_target_is_self
            {
                // Window scene draw — bind the sample-side view
                // (format/depth-aware swizzle) instead of the
                // raw IDENTITY-swizzle attachment view. This is
                // the load-bearing fix for the "depth-24 windows
                // / COW α leak" bug: the BgraNoAlpha swizzle
                // forced α=ONE for depth-24 used to live ONLY in
                // the engine's RENDER view-cache, never on the
                // scene path. Combined with `alpha_passthrough=true`
                // below, the prior IDENTITY view leaked the
                // BGRA8 padding byte (typically 0) into the
                // shader's `src.a`, blending depth-24 windows
                // with α=0 — invisible against root, which
                // matched the post-4d.7 mate-with-compositing
                // and xfce-with-compositing hardware-smoke
                // failure shape.
                //
                // SHAPE bounding handling: when the window has a
                // bounding region (marco's rounded-corner mask,
                // panel-applet transparency cutouts, etc.) emit
                // one clipped draw per rect intersected with the
                // window's storage extent. Without bounding (the
                // common case), emit a single full-window draw —
                // preserving the alpha-passthrough invariants
                // documented above for the depth-32 / depth-24
                // distinction. Pixels outside the bounding region
                // are intentionally NOT drawn so the layer below
                // (parent / wallpaper / root) shows through.
                let image_view = source.storage.sample_view;
                #[allow(clippy::cast_precision_loss)]
                let win_w_f = win_w as f32;
                #[allow(clippy::cast_precision_loss)]
                let win_h_f = win_h as f32;
                let mut emitted_any = false;
                let draw_start = draws.len();
                if let Some(rects) = shape_bounding.get(&host_xid) {
                    for rect in rects {
                        let rx = i32::from(rect.x);
                        let ry = i32::from(rect.y);
                        let rw = i32::from(rect.width);
                        let rh = i32::from(rect.height);
                        // Clamp to the window extent AND the ancestor
                        // visible box (parent-clipping).
                        let cx = rx.max(0).max(vis_lx0);
                        let cy = ry.max(0).max(vis_ly0);
                        let cw = (rx + rw).min(win_w).min(vis_lx1) - cx;
                        let ch = (ry + rh).min(win_h).min(vis_ly1) - cy;
                        if cw <= 0 || ch <= 0 {
                            continue;
                        }
                        #[allow(clippy::cast_precision_loss)]
                        let cw_f = cw as f32;
                        #[allow(clippy::cast_precision_loss)]
                        let ch_f = ch as f32;
                        #[allow(clippy::cast_precision_loss)]
                        let cx_f = cx as f32;
                        #[allow(clippy::cast_precision_loss)]
                        let cy_f = cy as f32;
                        draws.push(CompositeDraw {
                            image_view,
                            #[allow(clippy::cast_precision_loss)]
                            dst_origin: [(dx + cx) as f32, (dy + cy) as f32],
                            dst_size: [cw_f, ch_f],
                            src_origin: [cx_f / win_w_f, cy_f / win_h_f],
                            src_size: [cw_f / win_w_f, ch_f / win_h_f],
                            // Phase 2.6 — alpha-passthrough is inherited
                            // from the COW subtree flag (set on the COW
                            // top-level + descendants). Outside the COW
                            // subtree, draws stay opaque.
                            alpha_passthrough: under_cow_subtree,
                        });
                        emitted_any = true;
                    }
                } else if vis_lx1 > vis_lx0 && vis_ly1 > vis_ly0 {
                    // Unshaped: emit the window rect clipped to the
                    // ancestor visible box. Common case (child fits
                    // inside its parent) → box == full window, so this
                    // is the full-window draw with src [0,0]-[1,1].
                    let cw = vis_lx1 - vis_lx0;
                    let ch = vis_ly1 - vis_ly0;
                    #[allow(clippy::cast_precision_loss)]
                    draws.push(CompositeDraw {
                        image_view,
                        dst_origin: [(dx + vis_lx0) as f32, (dy + vis_ly0) as f32],
                        dst_size: [cw as f32, ch as f32],
                        src_origin: [vis_lx0 as f32 / win_w_f, vis_ly0 as f32 / win_h_f],
                        src_size: [cw as f32 / win_w_f, ch as f32 / win_h_f],
                        // Phase 2.6 — alpha-passthrough is inherited
                        // from the COW subtree flag (set on the COW
                        // top-level + descendants). Outside the COW
                        // subtree, draws stay opaque (no compositor
                        // path); inside the COW subtree, the
                        // compositor's stage paints with alpha and we
                        // blend over whatever lies below.
                        alpha_passthrough: under_cow_subtree,
                    });
                    emitted_any = true;
                }
                if emitted_any {
                    sampled_ids.push(source_id);
                    // Region unioned across every draw this window pushed, so a
                    // shaped window emitting one quad per shape rect is ONE
                    // participant. Identity is the host drawable, so a redirect
                    // swap is a resample rather than a replacement.
                    if let Some(p) = legacy_presence_from_draws(
                        draws,
                        draw_start,
                        ParticipantId {
                            role: SceneRole::Window,
                            xid: host_xid,
                            generation: d_id.as_u64(),
                        },
                    ) {
                        participants.push(p);
                    }
                    if let Some(snap) = store.peek_presentation_damage(source_id) {
                        for r in snap.region.rects() {
                            add_projected_damage(projected, *r, dx, dy, layout_w, layout_h);
                        }
                        snapshots.push(snap);
                    }
                }
            }
        } else {
            log::trace!(
                "render scene_walk xid={host_xid:#x}: SKIP reason=store_get_returned_none \
                     store_id={lookup_id:?} geom=({x},{y} {w}x{h}) mapped=true depth={depth}",
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
                    x = geom.x,
                    y = geom.y,
                    w = geom.width,
                    h = geom.height,
                    depth = geom.depth,
                );
            }
        }
    }

    // Audit #3 (2026-05-19) — descendants need to know whether THEY
    // sit under a redirected ancestor. The chain is "this window
    // counts as a redirected ancestor iff it owns its own
    // `redirected_target`" — that's exactly where
    // `resolve_paint_target` stops climbing the parent chain. A
    // recursion under a Manual-redirected ancestor without own
    // backing flips the flag on; an Automatic-redirected descendant
    // beneath that resets the flag for its own descendants (because
    // its paint stops at its own B).
    let self_owns_redirected_target = store
        .lookup(host_xid)
        .and_then(|id| store.redirected_target(id))
        .is_some();
    let child_under_redirected_ancestor = under_redirected_ancestor || self_owns_redirected_target;

    // Recurse into mapped descendants in stable sibling stack order.
    let mut children: Vec<(u32, u64)> = windows
        .iter()
        .filter_map(|(xid, g)| {
            if g.parent == Some(host_xid) {
                Some((*xid, g.stack_rank))
            } else {
                None
            }
        })
        .collect();
    children.sort_by_key(|(_, rank)| *rank);
    for (child_xid, _) in children {
        legacy_emit_window_subtree(
            child_xid,
            abs_x,
            abs_y,
            store,
            windows,
            shape_bounding,
            layout_x0,
            layout_y0,
            layout_w,
            layout_h,
            draws,
            snapshots,
            sampled_ids,
            projected,
            participants,
            child_under_redirected_ancestor,
            // Phase 2.6 — COW subtree flag is inherited unchanged.
            // Once we entered the COW top-level, every descendant
            // emits with alpha_passthrough=true.
            under_cow_subtree,
            // Parent-clipping: children are clipped to this window's
            // rect intersected with the inherited ancestor clip.
            child_clip_x0,
            child_clip_y0,
            child_clip_x1,
            child_clip_y1,
        );
    }
}

#[derive(Debug, PartialEq, Eq)]
struct DrawKey {
    view: u64,
    dst_origin: [u32; 2],
    dst_size: [u32; 2],
    src_origin: [u32; 2],
    src_size: [u32; 2],
    alpha_passthrough: bool,
}

fn draw_key(d: &CompositeDraw) -> DrawKey {
    DrawKey {
        view: ash::vk::Handle::as_raw(d.image_view),
        dst_origin: d.dst_origin.map(f32::to_bits),
        dst_size: d.dst_size.map(f32::to_bits),
        src_origin: d.src_origin.map(f32::to_bits),
        src_size: d.src_size.map(f32::to_bits),
        alpha_passthrough: d.alpha_passthrough,
    }
}

#[derive(Debug, PartialEq, Eq)]
struct WalkOut {
    draws: Vec<DrawKey>,
    participants: Vec<ScenePresence>,
    sampled: Vec<crate::kms::render::store::DrawableId>,
    snapshots: Vec<(crate::kms::render::store::DrawableId, u64)>,
    projected: Vec<vk::Rect2D>,
}

/// The pre-step-1 presence constructor, kept verbatim for the legacy
/// emitter: region = union of the emitted dst rects (outward-rounded),
/// signature from the first draw. `visible` did not exist; it equals the
/// region, which is what `Visibility::Off` produces too.
fn legacy_presence_from_draws(
    draws: &[CompositeDraw],
    from: usize,
    id: ParticipantId,
) -> Option<ScenePresence> {
    let emitted = draws.get(from..)?;
    let first = emitted.first()?;
    let mut region = Region::new();
    let mut place = Vec::new();
    for d in emitted {
        let x0 = d.dst_origin[0].floor();
        let y0 = d.dst_origin[1].floor();
        let x1 = (d.dst_origin[0] + d.dst_size[0]).ceil();
        let y1 = (d.dst_origin[1] + d.dst_size[1]).ceil();
        if x1 > x0 && y1 > y0 {
            #[allow(clippy::cast_possible_truncation)]
            let r = vk::Rect2D {
                offset: vk::Offset2D {
                    x: x0 as i32,
                    y: y0 as i32,
                },
                extent: vk::Extent2D {
                    width: (x1 - x0) as u32,
                    height: (y1 - y0) as u32,
                },
            };
            region.add_rect(r);
            place.push(r);
        }
    }
    if region.is_empty() {
        return None;
    }
    Some(ScenePresence {
        id,
        visible: region.clone(),
        region,
        place,
        signature: PresenceSignature::new(
            first.image_view,
            first.src_origin,
            first.src_size,
            first.alpha_passthrough,
        ),
    })
}

fn platform_with_layout(layout: (i32, i32, u32, u32)) -> PlatformBackend {
    let (lx, ly, lw, lh) = layout;
    let mut platform = PlatformBackend::for_tests();
    let out = &mut platform.outputs[0];
    out.x = lx;
    out.y = ly;
    out.width = u16::try_from(lw).expect("test layout width");
    out.height = u16::try_from(lh).expect("test layout height");
    platform
}

/// `build_scene` on a single output at `layout`, no cursor.
fn build_with(
    mode: Visibility,
    core: &KmsCore,
    store: &mut DrawableStore,
    windows: &crate::kms::render::backend::WindowsMap,
    layout: (i32, i32, u32, u32),
    cow_host_xid: Option<u32>,
) -> SceneBuild {
    let platform = platform_with_layout(layout);
    build_scene(
        core,
        store,
        windows,
        0,
        &platform,
        None,
        None,
        cow_host_xid,
        false,
        mode,
    )
}

fn sorted_rects(mut rects: Vec<vk::Rect2D>) -> Vec<vk::Rect2D> {
    rects.sort_by_key(|r| (r.offset.y, r.offset.x, r.extent.height, r.extent.width));
    rects
}

fn walk_out_of(built: &SceneBuild) -> WalkOut {
    WalkOut {
        draws: built.scene.draws.iter().map(draw_key).collect(),
        participants: built.participants.clone(),
        sampled: built.sampled_ids.clone(),
        snapshots: built.snapshots.iter().map(|s| (s.id, s.epoch)).collect(),
        projected: sorted_rects(built.projected_damage.rects().to_vec()),
    }
}

/// Run the top-level walk with the LEGACY emitter (`legacy == true`) or the
/// real `build_scene` under `Visibility::Off`, and normalise the output.
/// The fixture has no root drawable, so the two lists line up one to one.
fn walk_with(
    legacy: bool,
    core: &KmsCore,
    store: &mut DrawableStore,
    windows: &crate::kms::render::backend::WindowsMap,
    layout: (i32, i32, u32, u32),
    cow_host_xid: Option<u32>,
) -> WalkOut {
    if !legacy {
        let built = build_with(Visibility::Off, core, store, windows, layout, cow_host_xid);
        return walk_out_of(&built);
    }
    let (lx, ly, lw, lh) = layout;
    let mut draws = Vec::new();
    let mut snapshots = Vec::new();
    let mut sampled = Vec::new();
    let mut projected = RegionSet::new();
    let mut participants = Vec::new();
    for &top in &core.top_level_order {
        let under_cow = Some(top) == cow_host_xid;
        legacy_emit_window_subtree(
            top,
            0,
            0,
            store,
            windows,
            &core.shape_bounding,
            lx,
            ly,
            lw,
            lh,
            &mut draws,
            &mut snapshots,
            &mut sampled,
            &mut projected,
            &mut participants,
            false,
            under_cow,
            i32::MIN / 2,
            i32::MIN / 2,
            i32::MAX / 2,
            i32::MAX / 2,
        );
    }
    WalkOut {
        draws: draws.iter().map(draw_key).collect(),
        participants,
        sampled,
        snapshots: snapshots.iter().map(|s| (s.id, s.epoch)).collect(),
        projected: sorted_rects(projected.rects().to_vec()),
    }
}

fn set_rank(windows: &mut crate::kms::render::backend::WindowsMap, xid: u32, rank: u64) {
    windows.get_mut(&xid).expect("window present").stack_rank = rank;
}

fn alloc_backing(
    store: &mut DrawableStore,
    xid: u32,
    w: u32,
    h: u32,
) -> crate::kms::render::store::DrawableId {
    let mut storage = crate::kms::render::store::Storage::for_tests_null(
        extent(w, h),
        vk::Format::B8G8R8A8_UNORM,
    );
    let view: vk::ImageView = ash::vk::Handle::from_raw(u64::from(xid) | 0xB000_0000);
    storage.image_view = view;
    storage.sample_view = view;
    store
        .allocate(xid, DrawableKind::Pixmap, 32, true, storage)
        .expect("alloc backing stub")
}

/// The tree every differential case runs on. Ranks are all distinct so
/// sibling order does not depend on `HashMap` iteration.
fn differential_fixture() -> (
    KmsCore,
    DrawableStore,
    crate::kms::render::backend::WindowsMap,
) {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let mut windows = crate::kms::render::backend::WindowsMap::new();
    let mut rank = 1u64;
    let mut add = |store: &mut DrawableStore,
                   windows: &mut crate::kms::render::backend::WindowsMap,
                   xid: u32,
                   x: i16,
                   y: i16,
                   w: u16,
                   h: u16,
                   parent: Option<u32>,
                   mapped: bool| {
        alloc_stub_window(store, windows, xid, x, y, w, h, parent, mapped);
        set_rank(windows, xid, rank);
        rank += 1;
    };

    // Nesting three deep.
    add(
        &mut store,
        &mut windows,
        0x100,
        10,
        10,
        300,
        200,
        None,
        true,
    );
    add(
        &mut store,
        &mut windows,
        0x101,
        20,
        20,
        200,
        100,
        Some(0x100),
        true,
    );
    add(
        &mut store,
        &mut windows,
        0x102,
        30,
        30,
        50,
        40,
        Some(0x101),
        true,
    );
    // Unmapped child with a mapped grandchild: whole subtree hidden.
    add(
        &mut store,
        &mut windows,
        0x103,
        5,
        5,
        50,
        50,
        Some(0x100),
        false,
    );
    add(
        &mut store,
        &mut windows,
        0x104,
        1,
        1,
        10,
        10,
        Some(0x103),
        true,
    );
    // Overlapping siblings.
    add(
        &mut store,
        &mut windows,
        0x200,
        100,
        100,
        150,
        150,
        None,
        true,
    );
    add(
        &mut store,
        &mut windows,
        0x201,
        200,
        150,
        150,
        150,
        None,
        true,
    );
    // Shaped node with five rects, one of them outside the window.
    add(
        &mut store,
        &mut windows,
        0x300,
        400,
        40,
        120,
        90,
        None,
        true,
    );
    core.shape_bounding.insert(
        0x300,
        vec![
            xfixes::RegionRect {
                x: 0,
                y: 0,
                width: 120,
                height: 10,
            },
            xfixes::RegionRect {
                x: 0,
                y: 10,
                width: 10,
                height: 70,
            },
            xfixes::RegionRect {
                x: 110,
                y: 10,
                width: 10,
                height: 70,
            },
            xfixes::RegionRect {
                x: 0,
                y: 80,
                width: 120,
                height: 10,
            },
            xfixes::RegionRect {
                x: 100,
                y: 85,
                width: 60,
                height: 30,
            },
        ],
    );
    // Child extending beyond a tiny parent (the fvwm holding-window case),
    // plus a grandchild that is clipped away entirely.
    add(
        &mut store,
        &mut windows,
        0x400,
        600,
        300,
        10,
        10,
        None,
        true,
    );
    add(
        &mut store,
        &mut windows,
        0x401,
        -5,
        -5,
        100,
        100,
        Some(0x400),
        true,
    );
    add(
        &mut store,
        &mut windows,
        0x402,
        50,
        50,
        20,
        20,
        Some(0x401),
        true,
    );
    // Straddling the output's top-left corner, and fully off-output.
    add(
        &mut store,
        &mut windows,
        0x500,
        -50,
        -50,
        100,
        100,
        None,
        true,
    );
    add(
        &mut store,
        &mut windows,
        0x501,
        5000,
        5000,
        10,
        10,
        None,
        true,
    );
    // Manual-redirected top-level with (a) an automatic-redirected child
    // owning its own backing and (b) a plain child whose paint lands in
    // the manual ancestor's backing.
    add(
        &mut store,
        &mut windows,
        0x600,
        50,
        400,
        200,
        100,
        None,
        true,
    );
    add(
        &mut store,
        &mut windows,
        0x601,
        10,
        10,
        60,
        40,
        Some(0x600),
        true,
    );
    add(
        &mut store,
        &mut windows,
        0x602,
        100,
        10,
        60,
        40,
        Some(0x600),
        true,
    );
    let m_id = store.lookup(0x600).expect("manual present");
    let m_backing = alloc_backing(&mut store, 0xB600, 200, 100);
    store.set_redirected_target(m_id, Some(m_backing));
    store.set_scene_participating(m_id, false);
    let a_id = store.lookup(0x601).expect("automatic present");
    let a_backing = alloc_backing(&mut store, 0xB601, 60, 40);
    store.set_redirected_target(a_id, Some(a_backing));
    // Automatic-redirected top-level (sampled through its backing).
    add(
        &mut store,
        &mut windows,
        0x700,
        300,
        400,
        80,
        60,
        None,
        true,
    );
    let r_id = store.lookup(0x700).expect("automatic top present");
    let r_backing = alloc_backing(&mut store, 0xB700, 80, 60);
    store.set_redirected_target(r_id, Some(r_backing));
    // COW top-level with a stage child — alpha_passthrough subtree.
    add(&mut store, &mut windows, 0x800, 0, 0, 800, 600, None, true);
    add(
        &mut store,
        &mut windows,
        0x801,
        0,
        0,
        800,
        600,
        Some(0x800),
        true,
    );
    // A window with geometry but no storage at all.
    windows.insert(
        0x900,
        crate::kms::render::backend::WindowGeometry {
            border_width: 0,
            border_pixel: None,
            border_pixmap: None,
            x: 700,
            y: 500,
            width: 40,
            height: 40,
            depth: 24,
            mapped: true,
            viewable: true,
            parent: None,
            stack_rank: rank,
            bg_pixel: None,
            bg_pixmap: None,
            cursor: None,
        },
    );

    core.top_level_order = vec![
        0x100, 0x200, 0x201, 0x300, 0x400, 0x500, 0x501, 0x600, 0x700, 0x900, 0x800,
    ];
    (core, store, windows)
}

// ── Step 1 stage B: the visibility walk ──────────────────────────────

/// One rasterised pixel: what the compose would show there, as the stack
/// of (view, source u, source v) samples an alpha draw leaves and an
/// opaque draw resets. Comparing stacks pixel for pixel between the
/// `On` and `Off` scenes is the invariant step 1 must keep: clipping
/// changes what is *drawn*, never what is *shown*.
type PixelStack = Vec<(u64, f64, f64)>;

fn rasterise(draws: &[CompositeDraw], w: u32, h: u32) -> Vec<PixelStack> {
    let (wi, hi) = (w as usize, h as usize);
    let mut grid: Vec<PixelStack> = vec![Vec::new(); wi * hi];
    for d in draws {
        let x0 = d.dst_origin[0].floor().max(0.0) as usize;
        let y0 = d.dst_origin[1].floor().max(0.0) as usize;
        let x1 = ((d.dst_origin[0] + d.dst_size[0]).ceil().max(0.0) as usize).min(wi);
        let y1 = ((d.dst_origin[1] + d.dst_size[1]).ceil().max(0.0) as usize).min(hi);
        let view = ash::vk::Handle::as_raw(d.image_view);
        for py in y0..y1 {
            for px in x0..x1 {
                let fx = (px as f64 + 0.5 - f64::from(d.dst_origin[0])) / f64::from(d.dst_size[0]);
                let fy = (py as f64 + 0.5 - f64::from(d.dst_origin[1])) / f64::from(d.dst_size[1]);
                let u = f64::from(d.src_origin[0]) + fx * f64::from(d.src_size[0]);
                let v = f64::from(d.src_origin[1]) + fy * f64::from(d.src_size[1]);
                let cell = &mut grid[py * wi + px];
                if d.alpha_passthrough {
                    cell.push((view, u, v));
                } else {
                    cell.clear();
                    cell.push((view, u, v));
                }
            }
        }
    }
    grid
}

fn stacks_equal(a: &PixelStack, b: &PixelStack) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(x, y)| x.0 == y.0 && (x.1 - y.1).abs() < 1e-4 && (x.2 - y.2).abs() < 1e-4)
}

/// Assert the `On` scene shows the same pixels as the `Off` scene of the
/// same fixture, on every pixel of the output. Returns the `On` build for
/// further assertions.
fn assert_oracle(
    core: &KmsCore,
    store: &mut DrawableStore,
    windows: &crate::kms::render::backend::WindowsMap,
    layout: (i32, i32, u32, u32),
    cow: Option<u32>,
    label: &str,
) -> SceneBuild {
    let off = build_with(Visibility::Off, core, store, windows, layout, cow);
    let on = build_with(Visibility::On, core, store, windows, layout, cow);
    let (w, h) = (layout.2, layout.3);
    let a = rasterise(&off.scene.draws, w, h);
    let b = rasterise(&on.scene.draws, w, h);
    for (i, (sa, sb)) in a.iter().zip(&b).enumerate() {
        assert!(
            stacks_equal(sa, sb),
            "{label}: pixel ({},{}) differs: off={sa:?} on={sb:?} (layout {layout:?}, cow {cow:?})",
            i % w as usize,
            i / w as usize,
        );
    }
    assert_eq!(
        on.stats.draws_emitted,
        u64::try_from(on.scene.draws.len()).unwrap(),
        "{label}: the stats count what was emitted"
    );
    on
}

/// Root drawable at the logical screen size, sampled through a sentinel view.
fn alloc_root(core: &KmsCore, store: &mut DrawableStore, w: u32, h: u32) {
    let mut storage = crate::kms::render::store::Storage::for_tests_null(
        extent(w, h),
        vk::Format::B8G8R8A8_UNORM,
    );
    let view: ash::vk::ImageView = ash::vk::Handle::from_raw(0x00A0_7000);
    storage.image_view = view;
    storage.sample_view = view;
    store
        .allocate(core.window_id, DrawableKind::Root, 24, true, storage)
        .expect("alloc root stub");
}

fn area_of(r: vk::Rect2D) -> u64 {
    u64::from(r.extent.width) * u64::from(r.extent.height)
}

fn draws_of(built: &SceneBuild, view_raw: u64) -> Vec<vk::Rect2D> {
    built
        .scene
        .draws
        .iter()
        .filter(|d| ash::vk::Handle::as_raw(d.image_view) == view_raw)
        .filter_map(draw_dst_rect_inward)
        .collect()
}

fn win_view(xid: u32) -> u64 {
    u64::from(xid) | 0xFF00_0000
}

impl WalkOut {
    fn stats_free_snapshots(&self) -> usize {
        self.snapshots.len()
    }
}

fn two_windows(
    lower: (i16, i16, u16, u16),
    upper: (i16, i16, u16, u16),
) -> (
    KmsCore,
    DrawableStore,
    crate::kms::render::backend::WindowsMap,
) {
    let mut core = KmsCore::for_tests();
    let mut store = DrawableStore::new();
    let mut windows = crate::kms::render::backend::WindowsMap::new();
    alloc_root(&core, &mut store, 800, 600);
    alloc_stub_window(
        &mut store,
        &mut windows,
        0x100,
        lower.0,
        lower.1,
        lower.2,
        lower.3,
        None,
        true,
    );
    alloc_stub_window(
        &mut store,
        &mut windows,
        0x200,
        upper.0,
        upper.1,
        upper.2,
        upper.3,
        None,
        true,
    );
    set_rank(&mut windows, 0x100, 1);
    set_rank(&mut windows, 0x200, 2);
    core.top_level_order = vec![0x100, 0x200];
    (core, store, windows)
}

// ── dormancy across outputs that did not walk ────────────────────────

fn set(ids: &[u64]) -> std::collections::HashSet<crate::kms::render::store::DrawableId> {
    ids.iter()
        .map(|i| crate::kms::render::store::DrawableId::for_tests(*i))
        .collect()
}

fn r(x: i32, y: i32, w: u32, h: u32) -> vk::Rect2D {
    vk::Rect2D {
        offset: vk::Offset2D { x, y },
        extent: vk::Extent2D {
            width: w,
            height: h,
        },
    }
}
