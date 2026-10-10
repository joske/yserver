use super::*;

impl<'a> WalkSink<'a> {
    fn new(
        output_idx: usize,
        origin: (i32, i32),
        elsewhere: &'a std::collections::HashSet<crate::kms::render::store::DrawableId>,
    ) -> Self {
        Self {
            output_idx,
            origin,
            elsewhere,
            draws: Vec::new(),
            snapshots: Vec::new(),
            carried: Vec::new(),
            sampled_ids: Vec::new(),
            projected: RegionSet::new(),
            participants: Vec::new(),
            stats: WalkStats::default(),
            pieces: Vec::new(),
            presented_ids: Vec::new(),
            pieces_ids: Vec::new(),
        }
    }

    /// Computation order → painter's order. See the type doc.
    fn reverse(&mut self) {
        self.draws.reverse();
        self.snapshots.reverse();
        self.sampled_ids.reverse();
        self.participants.reverse();
        self.presented_ids.reverse();
        self.pieces_ids.reverse();
    }
}

fn debug_scene_walk_xids() -> &'static HashSet<u32> {
    static XIDS: OnceLock<HashSet<u32>> = OnceLock::new();
    XIDS.get_or_init(|| {
        std::env::var("YSERVER_SCENE_WALK_XIDS")
            .ok()
            .map(|raw| {
                raw.split(',')
                    .filter_map(|part| {
                        let token = part.trim();
                        if token.is_empty() {
                            return None;
                        }
                        let hex = token
                            .strip_prefix("0x")
                            .or_else(|| token.strip_prefix("0X"))
                            .unwrap_or(token);
                        u32::from_str_radix(hex, 16)
                            .ok()
                            .or_else(|| token.parse::<u32>().ok())
                    })
                    .collect()
            })
            .unwrap_or_default()
    })
}

fn debug_scene_walk_all() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        matches!(
            std::env::var("YSERVER_SCENE_WALK_ALL").ok().as_deref(),
            Some("1") | Some("true") | Some("TRUE") | Some("yes") | Some("YES")
        )
    })
}

pub(super) fn scene_walk_debug_enabled_for(host_xid: u32) -> bool {
    debug_scene_walk_all() || debug_scene_walk_xids().contains(&host_xid)
}

/// Walk window tree, build the per-output scene + collect damage
/// snapshots.
///
/// Stage 3f.6 lifted the Stage 2d "top-level only" simplification:
/// the recurse below walks each top-level → mapped + scene-
/// participating descendants, accumulating parent offsets into
/// absolute (root-space) coords before projecting onto the output.
/// xterm / xclock / any real app that paints into a child window
/// needs this — the bare top-level traversal showed only the
/// parent's (typically unpainted) storage on scanout.
///
/// Still-deferred simplifications:
/// - Skip the root storage entirely — bg_pixel is the clear color
///   (`scene.bg_color`). `bg_pixmap` would need a sample-from-pixmap
///   that uses the same blit pipeline as windows, deferred to
///   Stage 4 alongside the rest of the root content pipeline.
/// - Sibling z-order between children of the same parent is
///   HashMap-iteration-order (windows's underlying
///   `HashMap<u32, WindowGeometry>`). Proper stack-order tracking
///   is post-3f.6. Most real apps (xterm, xclock) have one child
///   per parent so the ordering rarely matters at Stage 3.
/// - Cursor: Stage 3f.8 appends a default-arrow sprite at top of
///   z when `cursor` is `Some`. Real theme support + per-window
///   `define_cursor` wiring stays Stage 4.
#[allow(clippy::too_many_arguments)]
pub(super) fn build_scene(
    core: &KmsCore,
    store: &mut DrawableStore,
    windows: &crate::kms::render::backend::WindowsMap,
    output_idx: usize,
    platform: &PlatformBackend,
    cursor: Option<CursorEntry>,
    cursor_prev_pos: Option<(i32, i32)>,
    cow_host_xid: Option<u32>,
    hw_strategy_active: bool,
    mode: Visibility,
) -> SceneBuild {
    // No knowledge of other outputs: every off-output paint is treated as
    // stranded (forced and acked here), which is the single-output rule.
    let nowhere = std::collections::HashSet::new();
    build_scene_with(
        core,
        store,
        windows,
        output_idx,
        platform,
        cursor,
        cursor_prev_pos,
        cow_host_xid,
        hw_strategy_active,
        mode,
        &nowhere,
    )
}

/// Test-only (#133): the `Visibility::On` draw list for `output_idx` as
/// inward-rounded destination rects, in emission order.
///
/// Exists because `ScenePresence::visible` is the BOUNDING BOX of a
/// node's emitted pieces (`emit_node` folds them with `union_bbox`), not
/// their union — so an assertion on `visible` cannot see a GAP that
/// leaves the bbox intact, which is exactly the shape of a dropped tail.
/// The draw list is the ground truth: a node that emits one unbroken
/// piece contributes exactly one rect equal to its placement.
pub(crate) fn scene_draw_rects(
    core: &KmsCore,
    store: &mut DrawableStore,
    windows: &crate::kms::render::backend::WindowsMap,
    output_idx: usize,
    platform: &PlatformBackend,
) -> Vec<vk::Rect2D> {
    let built = build_scene(
        core,
        store,
        windows,
        output_idx,
        platform,
        None,
        None,
        None,
        false,
        Visibility::On,
    );
    built
        .scene
        .draws
        .iter()
        .filter_map(draw_dst_rect_inward)
        .collect()
}

/// Test-only observation of WHERE the scene walk places each participant
/// (#133), per output, as `(host xid, placement rects)` in output-local
/// coordinates, together with what of that placement is actually VISIBLE
/// (`place ∩ mine`) — i.e. what reaches the screen.
///
/// Exists because the wezterm white-block regression was a disagreement
/// between the extent the walk SAMPLES (derived from live geometry) and
/// the extent the sampled storage actually HAS. No pixel assertion can
/// localise that: a fresh allocation that happens to be zeroed passes,
/// and an out-of-bounds sample is a driver-defined read. The invariant a
/// test can state exactly is "placement never exceeds the storage being
/// sampled", and that needs the placement.
pub(crate) fn scene_participant_places(
    core: &KmsCore,
    store: &mut DrawableStore,
    windows: &crate::kms::render::backend::WindowsMap,
    output_idx: usize,
    platform: &PlatformBackend,
) -> Vec<(u32, Vec<vk::Rect2D>, Vec<vk::Rect2D>)> {
    let built = build_scene(
        core,
        store,
        windows,
        output_idx,
        platform,
        None,
        None,
        None,
        false,
        Visibility::On,
    );
    built
        .participants
        .into_iter()
        .map(|p| (p.id.xid, p.place, p.visible.rects().collect()))
        .collect()
}

/// [`build_scene`] with knowledge of what the OTHER outputs showed at their
/// last walk, so an off-output paint that another output presents is classified
/// `ContentDamage::OtherOutput` and left to that output. See [`WalkSink::elsewhere`].
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)]
pub(super) fn build_scene_with(
    core: &KmsCore,
    store: &mut DrawableStore,
    windows: &crate::kms::render::backend::WindowsMap,
    output_idx: usize,
    platform: &PlatformBackend,
    cursor: Option<CursorEntry>,
    _cursor_prev_pos: Option<(i32, i32)>,
    // Phase 2.6 — host xid of the materialized Composite Overlay
    // Window, if any. The top-level walk uses this to mark the COW
    // top-level (and its descendants by recursion) with
    // `under_cow_subtree = true`, which in turn sets
    // `alpha_passthrough = true` on every emitted `CompositeDraw`.
    // `None` when the COW is not materialized (no compositor active
    // or not yet claimed via GetOverlayWindow). Phase 2.7 replaced
    // the prior `cow: Option<DrawableId>` arg: the COW now emits
    // via the normal top_level_order walk, not via a special
    // post-walk append, so we only need the host xid to tag the
    // walk's recursion flag — no DrawableId needed.
    cow_host_xid: Option<u32>,
    // Stage 5 Phase C — when `true`, the strategy picks `Hw` for
    // cursors that fit the plane and lie on-output; otherwise `Sw`.
    // `false` collapses every assignment to the SW path (rollout
    // default).
    hw_strategy_active: bool,
    // Step 1 — clip each node to what nothing above it covers (`On`), or emit
    // every node's full placement as before step 1 (`Off`). Production passes
    // `On`; the damage audit's reference and the tests use `Off`.
    mode: Visibility,
    elsewhere: &std::collections::HashSet<crate::kms::render::store::DrawableId>,
) -> SceneBuild {
    let bg = [0.0, 0.0, 0.0, 1.0];
    // A transformed output walks its whole footprint (spec D4).
    let (layout_x0, layout_y0, layout_w, layout_h) = platform.output_root_rect(output_idx);

    let mut sink = WalkSink::new(output_idx, (layout_x0, layout_y0), elsewhere);
    // Stage 4c.3 — the root samples through `redirected_target` like any other
    // node; geometry stays the host drawable's. Decided up front, emitted last
    // (see below).
    let root = root_node(core, store, layout_x0, layout_y0, layout_w, layout_h, mode);
    // Phase 2.7 — the COW emits via the normal top_level_order walk
    // like any other root child. After Phase 2.2/2.5, the COW is a
    // first-class entry in `windows` + `top_level_order`; the
    // walk's `under_cow_subtree` flag (Task 2.6) carries the
    // alpha-passthrough semantic that the deleted post-walk append
    // used to wire up. Mirrors Xorg's compositor contract: COW is
    // a real root child stacked above the other top-levels.
    log::trace!(
        "render scene_walk begin output={output_idx} top_levels={n} order={order:?} \
         cow_host_xid={cow_host_xid:?} \
         layout=({layout_x0},{layout_y0} {layout_w}x{layout_h}) mode={mode:?}",
        n = core.top_level_order.len(),
        order = core.top_level_order,
    );
    // Fullscreen-unredirect / direct-scanout bypass. The COW is the always-on-
    // top compositor overlay carrying the composite of the REDIRECTED windows.
    // An UNREDIRECTED (scene_participating), opaque window that fully covers
    // this output is drawn directly by us and sits logically in front of that
    // composite — so the COW's content is entirely occluded by it. Emit the
    // window but SKIP the COW (and its `under_cow` subtree, e.g. the
    // compositor's desktop stage) for this output. Otherwise the always-on-top
    // COW — correctly capped on top by the stacking-projection rework
    // (a4ff9f1e) — paints the desktop composite over the directly-drawn window
    // and it vanishes (cinnamon-screensaver lock, and any fullscreen
    // override-redirect window, once muffin unredirects it: RedirectWindow ->
    // NameWindowPixmap -> UnredirectWindow). Mirrors mutter/Xorg
    // `unredirect_fullscreen`. Pre-rework this happened to work because the COW
    // wasn't reliably on top, so the window landed above it.
    //
    // Step 1 does NOT replace this. The fullscreen window is *below* the COW in
    // stacking and occludes it only because the compositor unredirected it —
    // not an occlusion the tree can express. The probe and its filter matrix
    // stay exactly as they are.
    let probe_lw = i32::try_from(layout_w).unwrap_or(i32::MAX);
    let probe_lh = i32::try_from(layout_h).unwrap_or(i32::MAX);
    let probe_x1 = layout_x0.saturating_add(probe_lw);
    let probe_y1 = layout_y0.saturating_add(probe_lh);
    // The probe walks top-down and lets the FIRST candidate decide, so it
    // must only consider top-levels that can actually occlude something on
    // this output. A window lying entirely outside the output cannot, and
    // must not end the scan: muffin parks 1x1 helper windows off-screen
    // (e.g. host 0xd0000a at (-200,-200)) and raises them ABOVE the managed
    // windows, so before this filter the very first candidate was one of
    // those, `covers` was false, and the COW was never suppressed — leaving
    // an unredirected fullscreen window hidden under the compositor's
    // desktop composite (issue #98: fullscreen games/video render as the
    // wallpaper while audio keeps playing).
    let topmost_on_output = core
        .top_level_order
        .iter()
        .rev()
        .filter(|&&x| Some(x) != cow_host_xid)
        .find_map(|&x| {
            windows
                .get(&x)
                .filter(|g| {
                    g.mapped
                        && i32::from(g.x) < probe_x1
                        && i32::from(g.y) < probe_y1
                        && i32::from(g.x) + i32::from(g.width) > layout_x0
                        && i32::from(g.y) + i32::from(g.height) > layout_y0
                })
                .map(|g| (x, *g))
        });
    let suppress_cow = cow_host_xid.is_some()
        && topmost_on_output.is_some_and(|(x, g)| {
            let covers = i32::from(g.x) <= layout_x0
                && i32::from(g.y) <= layout_y0
                && i32::from(g.x) + i32::from(g.width) >= probe_x1
                && i32::from(g.y) + i32::from(g.height) >= probe_y1;
            // Opaque (no alpha channel for the COW to show through) AND
            // drawn by us (scene_participating == not compositor-owned).
            let opaque = g.depth != 32;
            let participating = store
                .lookup(x)
                .and_then(|id| store.get(id))
                .is_some_and(|d| d.scene_participating);
            covers && opaque && participating
        });
    if suppress_cow {
        log::trace!(
            "render scene_walk output={output_idx}: COW suppressed — opaque fullscreen \
             unredirected window occludes the compositor overlay"
        );
    }
    // DIAG(#98): the suppression verdict plus the inputs it turned on.
    // Deduped, so a steady state costs one line rather than one per frame.
    // `cow_shape` tells us whether the compositor ALSO punched a hole in
    // the COW (mutter-lineage `shape_cow_for_window`) — if it did, the
    // suppression probe is not the only mechanism in play and the missing
    // parent-shape clipping of the COW's stage child matters too.
    if cow_host_xid.is_some() {
        let msg = format!(
            "cow_diag: output={output_idx} suppress_cow={suppress_cow} \
             cow_shape_rects={shape:?} picked={picked} order_top={top:?}",
            shape = cow_host_xid
                .and_then(|c| core.shape_bounding.get(&c))
                .map(Vec::len),
            picked = topmost_on_output.map_or_else(
                || "none".to_string(),
                |(x, g)| format!(
                    "0x{x:x}[({},{} {}x{}) depth={} part={}]",
                    g.x,
                    g.y,
                    g.width,
                    g.height,
                    g.depth,
                    store
                        .lookup(x)
                        .and_then(|id| store.get(id))
                        .is_some_and(|d| d.scene_participating),
                ),
            ),
            top = core
                .top_level_order
                .iter()
                .rev()
                .take(4)
                .map(|x| format!("0x{x:x}"))
                .collect::<Vec<_>>(),
        );
        static LAST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        std::hash::Hash::hash(&msg, &mut hasher);
        let sig = std::hash::Hasher::finish(&hasher);
        if LAST.swap(sig, std::sync::atomic::Ordering::Relaxed) != sig {
            log::debug!("{msg}");
        }
    }
    let children = children_index(windows);
    // Step 1 — the universe for the root's children is the output: X11 clips
    // top-levels to the screen. Under `Off` the region is never read.
    let mut universe = match mode {
        Visibility::On => Region::from_rect(vk::Rect2D {
            offset: vk::Offset2D::default(),
            extent: vk::Extent2D {
                width: layout_w,
                height: layout_h,
            },
        }),
        Visibility::Off => Region::new(),
    };
    // Computation order is top → bottom (`miComputeClips` visits the topmost
    // sibling first); the sink is reversed into painter's order below.
    for &top_xid in core.top_level_order.iter().rev() {
        if suppress_cow && Some(top_xid) == cow_host_xid {
            continue;
        }
        visit_window_subtree(
            top_xid,
            0,
            0,
            store,
            windows,
            &children,
            &core.shape_bounding,
            &core.shape_clip,
            layout_x0,
            layout_y0,
            layout_w,
            layout_h,
            mode,
            &mut universe,
            &mut sink,
            // Top-level windows start with no redirected ancestor;
            // the flag flips on inside the recursion when entering
            // a redirected window's subtree.
            false,
            // Phase 2.6 — flag the COW top-level (and its
            // descendants, propagated by recursion) so emitted
            // draws inherit `alpha_passthrough = true`.
            Some(top_xid) == cow_host_xid,
            // Parent-clipping: top-levels are clipped by the root =
            // the screen, which the output-extent gate already
            // enforces. Pass effectively-unbounded ancestor bounds so
            // this is a no-op for top-levels; descendants are still
            // clipped to their top-level via the recursion.
            //
            // #133 step 5: the `(0, 0)` above is the ROOT's CONTENT origin.
            // The root window has no border in X11 (`wBorderWidth(root)` is
            // 0 and CreateWindow cannot give it one), so its outer and
            // content origins coincide and no term is needed here.
            i32::MIN / 2,
            i32::MIN / 2,
            i32::MAX / 2,
            i32::MAX / 2,
        );
    }
    // The root is the last node in computation order — the bottom of the
    // stack — so it lands first after the reversal, exactly where the old
    // emitter pushed it. Under `On` it gets what the top-levels left.
    if let Some(root) = root {
        sink.stats.nodes_visited += 1;
        let out = emit_node(
            &mut sink,
            mode,
            &universe,
            &root.place,
            root.dx,
            root.dy,
            root.denom_w,
            root.denom_h,
            root.view,
            false,
            root.source_id,
            store,
            layout_w,
            layout_h,
        );
        log::trace!(
            "render scene_walk root output={output_idx}: place={} pieces={}",
            root.place.len(),
            out.emitted,
        );
        push_presence(
            &mut sink,
            root.place,
            out,
            ParticipantId {
                role: SceneRole::Root,
                xid: core.window_id,
                generation: root.id.as_u64(),
            },
        );
    }
    sink.reverse();
    let WalkSink {
        output_idx: _,
        origin: _,
        elsewhere: _,
        mut draws,
        snapshots,
        carried,
        mut sampled_ids,
        projected,
        participants,
        stats,
        pieces: _,
        mut presented_ids,
        mut pieces_ids,
    } = sink;
    log::trace!(
        "render scene_walk end output={output_idx} draws={n_draws} \
         sampled={n_sampled} nodes={nodes} hidden={hidden} collapses={collapses}",
        n_draws = draws.len(),
        n_sampled = sampled_ids.len(),
        nodes = stats.nodes_visited,
        hidden = stats.hidden_participants,
        collapses = stats.collapses(),
    );

    // Stage 5 Phase C: pure cursor strategy decision. `build_scene`
    // decides visibility + HW/SW assignment and reports the current
    // clipped footprint, but it does NOT emit cursor damage. The
    // tick owns that decision because it also owns the transactional
    // "last successfully presented cursor footprint/version" state.
    //
    // Appended AFTER the reversal so `software_cursor_tail` keeps pointing at
    // the last draw / sampled id.
    #[allow(clippy::cast_possible_truncation)]
    let mut software_cursor_tail = None;
    let (cursor_assignment, new_cursor_rect, cursor_record_version): (
        CursorAssignment,
        Option<vk::Rect2D>,
        Option<u64>,
    ) = if let Some(cur) = cursor
        && let Some(drawable) = store.get(cur.id)
        && drawable.storage.image_view != vk::ImageView::null()
    {
        let cw = i32::try_from(cur.extent.width).unwrap_or(i32::MAX);
        let ch = i32::try_from(cur.extent.height).unwrap_or(i32::MAX);
        let layout_w_i = i32::try_from(layout_w).unwrap_or(i32::MAX);
        let layout_h_i = i32::try_from(layout_h).unwrap_or(i32::MAX);
        let dx = (core.cursor_x as i32) - i32::from(cur.hot_x) - layout_x0;
        let dy = (core.cursor_y as i32) - i32::from(cur.hot_y) - layout_y0;
        let new_rect = cursor_footprint_rect(dx, dy, cw, ch, layout_w_i, layout_h_i);
        if new_rect.is_none() {
            // Off-output / fully-clipped — the cursor isn't on this
            // output this frame. Phase D treats this as `Hidden`.
            (CursorAssignment::Hidden, None, None)
        } else {
            // Phase C strategy gates (codex v6-pass — pure data, no
            // DRM side effects). Hand off to HW only when the
            // strategy is active AND the sprite fits this output's owning
            // device plane. Cursor dimensions differ by card (e.g. 128px
            // amdgpu beside 64px i915), so this must not be a global 64px
            // minimum.
            let hw_fits = platform.cursor_plane_fits_for_output(
                output_idx,
                cur.extent.width,
                cur.extent.height,
            );
            if hw_strategy_active && hw_fits {
                (
                    CursorAssignment::Hw {
                        x: core.cursor_x as i32,
                        y: core.cursor_y as i32,
                        record_version: cur.record_version,
                        hot_x: u16::try_from(cur.hot_x.max(0)).unwrap_or(0),
                        hot_y: u16::try_from(cur.hot_y.max(0)).unwrap_or(0),
                    },
                    new_rect,
                    Some(cur.record_version),
                )
            } else {
                let draw_index = draws.len();
                let sampled_index = sampled_ids.len();
                draws.push(CompositeDraw {
                    image_view: drawable.storage.sample_view,
                    #[allow(clippy::cast_precision_loss)]
                    dst_origin: [dx as f32, dy as f32],
                    #[allow(clippy::cast_precision_loss)]
                    dst_size: [cw as f32, ch as f32],
                    src_origin: [0.0, 0.0],
                    src_size: [1.0, 1.0],
                    alpha_passthrough: true,
                });
                sampled_ids.push(cur.id);
                presented_ids.push(cur.id);
                pieces_ids.push(cur.id);
                software_cursor_tail = Some((draw_index, sampled_index));
                (
                    CursorAssignment::Sw { pos: (dx, dy) },
                    new_rect,
                    Some(cur.record_version),
                )
            }
        }
    } else {
        (CursorAssignment::Hidden, None, None)
    };

    let scene = CompositeScene {
        bg_color: bg,
        draws,
    };
    SceneBuild {
        scene,
        snapshots,
        carried,
        sampled_ids,
        projected_damage: projected,
        cursor_assignment,
        new_cursor_rect,
        cursor_record_version,
        software_cursor_tail,
        participants,
        stats,
        presented_ids,
        pieces_ids,
    }
}

fn root_node(
    core: &KmsCore,
    store: &DrawableStore,
    layout_x0: i32,
    layout_y0: i32,
    layout_w: u32,
    layout_h: u32,
    mode: Visibility,
) -> Option<RootNode> {
    let id = store.lookup(core.window_id)?;
    let drawable = store.get(id)?;
    if !drawable.scene_participating || !matches!(drawable.kind, DrawableKind::Root) {
        return None;
    }
    // Stage 4c.3 — route source-storage through `redirected_target`.
    // For an Automatic-mode redirected drawable, the scene must
    // blit FROM the backing B (not the drawable's own storage).
    // Geometry stays driven by the host drawable; only the
    // sampled storage handle reroutes.
    let source_id = store.redirected_target(id).unwrap_or(id);
    let source = store.get(source_id)?;
    if source.storage.image_view == vk::ImageView::null() {
        return None;
    }
    let dx = -layout_x0;
    let dy = -layout_y0;
    let host = drawable.storage.extent;
    let full = vk::Rect2D {
        offset: vk::Offset2D { x: dx, y: dy },
        extent: host,
    };
    let (place, denom) = match mode {
        Visibility::Off => (vec![full], host),
        Visibility::On => (
            clip_rect_to_output_extent(
                full,
                vk::Extent2D {
                    width: layout_w,
                    height: layout_h,
                },
            )
            .into_iter()
            .collect(),
            source.storage.extent,
        ),
    };
    Some(RootNode {
        id,
        source_id,
        // Root scene draw — sample-side view carries the
        // format/depth-aware swizzle (depth-24 → α=ONE).
        // See `Storage::sample_view` for why scene draws
        // MUST NOT bind `image_view` directly.
        view: source.storage.sample_view,
        place,
        dx,
        dy,
        denom_w: i32::try_from(denom.width).unwrap_or(i32::MAX),
        denom_h: i32::try_from(denom.height).unwrap_or(i32::MAX),
    })
}

/// One quad for one output-local piece of a node.
///
/// `src` is derived by translating the piece back into the node's own pixels
/// (`piece − (dx, dy)`) and dividing by the sampled source's extent — so a
/// piece of a straddling window, or of a window on an output with a non-zero
/// layout origin, samples exactly the texels the unclipped draw would have.
/// Same arithmetic, same casts, as the pre-step-1 emitter, so `Visibility::Off`
/// reproduces it bit for bit.
fn piece_draw(
    piece: vk::Rect2D,
    dx: i32,
    dy: i32,
    denom_w: i32,
    denom_h: i32,
    view: vk::ImageView,
    alpha_passthrough: bool,
) -> CompositeDraw {
    let cx = piece.offset.x - dx;
    let cy = piece.offset.y - dy;
    let cw = i32::try_from(piece.extent.width).unwrap_or(i32::MAX);
    let ch = i32::try_from(piece.extent.height).unwrap_or(i32::MAX);
    #[allow(clippy::cast_precision_loss)]
    let (cw_f, ch_f, cx_f, cy_f, dw_f, dh_f) = (
        cw as f32,
        ch as f32,
        cx as f32,
        cy as f32,
        denom_w as f32,
        denom_h as f32,
    );
    CompositeDraw {
        image_view: view,
        #[allow(clippy::cast_precision_loss)]
        dst_origin: [(dx + cx) as f32, (dy + cy) as f32],
        dst_size: [cw_f, ch_f],
        src_origin: [cx_f / dw_f, cy_f / dh_f],
        src_size: [cw_f / dw_f, ch_f / dh_f],
        alpha_passthrough,
    }
}

fn union_bbox(a: vk::Rect2D, b: vk::Rect2D) -> vk::Rect2D {
    let x0 = a.offset.x.min(b.offset.x);
    let y0 = a.offset.y.min(b.offset.y);
    let x1 = (a.offset.x.saturating_add_unsigned(a.extent.width))
        .max(b.offset.x.saturating_add_unsigned(b.extent.width));
    let y1 = (a.offset.y.saturating_add_unsigned(a.extent.height))
        .max(b.offset.y.saturating_add_unsigned(b.extent.height));
    vk::Rect2D {
        offset: vk::Offset2D { x: x0, y: y0 },
        extent: vk::Extent2D {
            width: u32::try_from(x1 - x0).unwrap_or(0),
            height: u32::try_from(y1 - y0).unwrap_or(0),
        },
    }
}

/// Emit one node that passed every gate: its draws (clipped to `mine` under
/// `On`), its sampled id, its presence and its damage snapshot. Returns the
/// number of draws pushed — zero for a node something above covers entirely,
/// which is **still a participant** (see `ScenePresence`).
///
/// Draws for the node's place rects are pushed in reverse so that the sink's
/// final reversal restores place order, which is what makes `Off` byte-identical
/// to the old emitter.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_node(
    sink: &mut WalkSink<'_>,
    mode: Visibility,
    mine: &Region,
    place: &[vk::Rect2D],
    dx: i32,
    dy: i32,
    denom_w: i32,
    denom_h: i32,
    view: vk::ImageView,
    alpha_passthrough: bool,
    source_id: crate::kms::render::store::DrawableId,
    store: &DrawableStore,
    layout_w: u32,
    layout_h: u32,
) -> Emitted {
    let mut emitted = 0u64;
    let mut visible_bbox: Option<vk::Rect2D> = None;
    // The exact visible pieces this node emits, for clipping its content
    // damage below. Disjoint (each is `mine ∩ r` for a distinct place rect, and
    // the place rects are disjoint), so per-piece intersection sums exactly.
    sink.pieces.clear();
    for r in place.iter().rev() {
        match mode {
            Visibility::Off => {
                sink.draws.push(piece_draw(
                    *r,
                    dx,
                    dy,
                    denom_w,
                    denom_h,
                    view,
                    alpha_passthrough,
                ));
                emitted += 1;
            }
            Visibility::On => {
                // Per place rect, so a piece never leaves its own shape rect
                // even when `mine` is a capped superset.
                let vis = mine.clip_to_rect(*r);
                for piece in vis.rects() {
                    sink.draws.push(piece_draw(
                        piece,
                        dx,
                        dy,
                        denom_w,
                        denom_h,
                        view,
                        alpha_passthrough,
                    ));
                    emitted += 1;
                    sink.pieces.push(piece);
                    // `visible` on the presence is a damage-side summary where
                    // a superset is safe — the bounding box of the pieces, not
                    // their union, which would cost a `combine` per piece on
                    // the hot path. Content damage is clipped to the exact
                    // `pieces` instead.
                    visible_bbox = Some(match visible_bbox {
                        None => piece,
                        Some(acc) => union_bbox(acc, piece),
                    });
                }
            }
        }
    }
    let visible = match mode {
        Visibility::Off => Region::from_rects(place.iter().copied()),
        Visibility::On => visible_bbox.map_or_else(Region::new, Region::from_rect),
    };
    sink.stats.draws_emitted += emitted;
    if emitted == 0 {
        sink.stats.hidden_participants += 1;
    } else {
        sink.pieces_ids.push(source_id);
    }
    sink.sampled_ids.push(source_id);
    // Signature from the first PLACE rect — what the unclipped draw would
    // carry — never from an emitted piece, whose src moves whenever the cover
    // above moves and would read as a resample every frame. The presence itself
    // is pushed by the caller once the claim step is done with `place`
    // (`push_presence`), so the decision's rect list moves into it uncopied.
    let signature = place.first().map(|first| {
        let unclipped = piece_draw(*first, dx, dy, denom_w, denom_h, view, alpha_passthrough);
        PresenceSignature::new(
            unclipped.image_view,
            unclipped.src_origin,
            unclipped.src_size,
            unclipped.alpha_passthrough,
        )
    });
    if let Some(snap) = store.peek_presentation_damage(source_id) {
        // Stage C — project the captured damage onto the output and, under
        // `On`, keep only what lands on this node's visible pieces: a paint
        // into the covered part of a window cannot have changed a pixel on
        // screen. `Off` keeps the unclipped projection so the audit's reference
        // damages what it always did.
        let mut on_output = false;
        let mut added = false;
        for r in snap.region.rects() {
            let Some(proj) = project_onto_output(*r, dx, dy, layout_w, layout_h) else {
                continue;
            };
            on_output = true;
            match mode {
                Visibility::Off => {
                    sink.projected.add(proj);
                    added = true;
                }
                Visibility::On => {
                    for piece in &sink.pieces {
                        if let Some(hit) = intersect_rects(proj, *piece) {
                            sink.projected.add(hit);
                            added = true;
                        }
                    }
                }
            }
        }
        // Presented, and carried into this output's PendingAck, ONLY for
        // `Visible` — damage that reached the screen here — plus a node with no
        // damage at all, which has nothing to present. `Hidden`, `OtherOutput`
        // and `OffOutput` are none of those: the snapshot stays in the store for
        // the walk that does present it. Acking a snapshot this output did not
        // present is the multi-output ack race, and it must not depend on
        // cross-output knowledge to avoid: `OffOutput` used to be carried on the
        // theory that it is stranded everywhere, but "everywhere" was decided
        // from `elsewhere`, which is one walk stale. Measured 2026-09-04 on
        // silence/MATE: the caja desktop spans both outputs, output 1 classified
        // its rubberband-erase damage `OffOutput` 104× while output 0's set was
        // cold, acked it, and left the selection on output 0's scanout —
        // 260 audit mismatches, 247 unhealed. `OffOutput` still FORCES a compose
        // here (the xfce submenu case: a paint whose projection is empty must
        // not sit undrained), but the drain now comes from dormancy — the
        // snapshot is not presented, so reconciliation makes it dormant and it
        // stops re-forcing until its next paint. See [`ContentDamage`] and
        // `WalkSink::presented_ids`.
        let mut presented = true;
        let mut carry = true;
        if !snap.region.is_empty() {
            let class = if !on_output {
                if sink.elsewhere.contains(&source_id) {
                    ContentDamage::OtherOutput
                } else {
                    ContentDamage::OffOutput
                }
            } else if added {
                ContentDamage::Visible
            } else {
                ContentDamage::Hidden
            };
            match class {
                ContentDamage::Visible => sink.stats.content_visible += 1,
                ContentDamage::Hidden => {
                    sink.stats.content_hidden += 1;
                    presented = false;
                    carry = false;
                }
                ContentDamage::OffOutput => {
                    sink.stats.content_off_output += 1;
                    presented = false;
                    carry = false;
                }
                ContentDamage::OtherOutput => {
                    sink.stats.content_other_output += 1;
                    presented = false;
                    carry = false;
                }
            }
            if class != ContentDamage::Visible && tick_skip_log_enabled() {
                log::info!(
                    "content-diag: out{} drawable={:?} epoch={} class={class:?}",
                    sink.output_idx,
                    source_id,
                    snap.epoch,
                );
            }
        }
        if presented {
            sink.presented_ids.push(source_id);
        }
        if carry {
            if !snap.region.is_empty() {
                let (ox, oy) = (dx + sink.origin.0, dy + sink.origin.1);
                sink.carried.push(CarriedDamage {
                    id: source_id,
                    epoch: snap.epoch,
                    root: snap
                        .region
                        .rects()
                        .iter()
                        .map(|r| vk::Rect2D {
                            offset: vk::Offset2D {
                                x: r.offset.x + ox,
                                y: r.offset.y + oy,
                            },
                            extent: r.extent,
                        })
                        .collect(),
                });
            }
            sink.snapshots.push(snap);
        }
    } else {
        // Nothing pending to present; parity with the old "sampled ⇒ drawn".
        sink.presented_ids.push(source_id);
    }
    Emitted {
        emitted,
        visible,
        signature,
    }
}

/// Push the node's presence, consuming the decision's `place`. Called after the
/// claim step, which is the last reader of `place`; still within the node's own
/// step, so the participants list keeps computation order.
pub(super) fn push_presence(
    sink: &mut WalkSink<'_>,
    place: Vec<vk::Rect2D>,
    out: Emitted,
    participant: ParticipantId,
) {
    if let Some(signature) = out.signature
        && let Some(p) = presence_from_place(place, out.visible, participant, signature)
    {
        sink.participants.push(p);
    }
}

/// Intersection of two rects, `None` if they do not overlap.
pub(super) fn intersect_rects(a: vk::Rect2D, b: vk::Rect2D) -> Option<vk::Rect2D> {
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
