//! State-borrowing replacement for `nested::accumulate_damage`.
//!
//! Walks `state.damage_objects` for damage tied to `drawable`, ORs the
//! incoming rect into each, and (for objects that haven't yet fired
//! this cycle) emits a `DamageNotify` event to the owning client. The
//! old version snapshotted writer Arcs and ran the writer pass after
//! dropping the server lock; the lifted version interleaves the state
//! mutation (pending_notify_fired toggle, rect push) with target
//! collection in a single `&mut ServerState` borrow scope, then
//! encodes + writes via `client_io::write_or_buffer`.

use yserver_protocol::x11::{ClientId, ResourceId, SequenceNumber, damage as x11damage, xfixes};

use crate::{core_loop::fanout::fanout_event_to_clients, server::ServerState};

use crate::nested::DAMAGE_FIRST_EVENT;

/// One DamageNotify worth of identity that has already been
/// "committed" against `state.damage_objects` (rect pushed, fired
/// flag set). The caller drains this list to actually transmit.
#[derive(Debug, Clone)]
struct PendingNotify {
    owner: ClientId,
    damage_id: u32,
    level: u8,
    drawable: u32,
    /// Damaged area in this level's coordinate space (already
    /// translated + clipped through the ancestor walk).
    area: x11damage::Rectangle,
    /// Full extent of the level's drawable (in its own coord space).
    /// For ancestor matches this is the ancestor's extent, not the
    /// originating leaf's — per X11 DAMAGE spec.
    geometry: x11damage::Rectangle,
}

/// Maximum tree depth to walk when propagating damage to ancestors.
/// Real-world toolkits keep widget trees to a handful of levels;
/// the cap is purely a safety net against cycle / corruption in the
/// `Window.parent` chain, not a meaningful product limit.
const ANCESTOR_WALK_MAX_DEPTH: u8 = 32;

fn format_damage_rects(rects: &[xfixes::RegionRect]) -> String {
    use std::fmt::Write as _;

    let mut out = String::from("[");
    for (i, rect) in rects.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        let _ = write!(
            out,
            "({},{} {}x{})",
            rect.x, rect.y, rect.width, rect.height
        );
    }
    out.push(']');
    out
}

/// Convenience: accumulate damage over the full extent of `drawable`
/// (its width × height). Mirrors `nested::accumulate_damage_full`.
pub fn accumulate_damage_full_to_state(
    state: &mut ServerState,
    drawable: ResourceId,
) -> Vec<ClientId> {
    let r = drawable_full_rect(state, drawable);
    accumulate_damage_to_state(state, drawable, 0, 0, r.width, r.height)
}

/// The border annulus of `window`, in the window's OWN coordinate
/// space — the region Xorg calls `borderClip − winSize`
/// (`dix/window.c:1585-1587`, `composite/compwindow.c:114`). The ring
/// lies OUTSIDE the content origin, so it spans `(-bw, -bw)` to
/// `(width + bw, height + bw)` and its top/left strips carry NEGATIVE
/// rect origins. That is the same convention as the SHAPE bounding
/// default (`nested::default_shape_rect`) and as Xorg's own damage
/// report: `damageDamageBox` translates by the window's INNER origin
/// (`miext/damage/damage.c:1194` → `damageRegionAppend`), so the
/// border pixels arrive at negative window coordinates on the wire.
///
/// Returned as the actual region difference (four strips: top, bottom,
/// left, right) rather than one bounding rect, so a compositor
/// repainting exactly what we report does not also repaint the
/// content it already has.
///
/// Empty for the root, for an unknown xid and at `bw == 0` — the
/// latter being Xorg's `HasBorder(pWin)` early-out.
pub fn border_annulus_rects(state: &ServerState, window: ResourceId) -> Vec<xfixes::RegionRect> {
    if window == crate::resources::ROOT_WINDOW {
        return Vec::new();
    }
    let Some(w) = state.resources.window(window) else {
        return Vec::new();
    };
    if w.border_width == 0 {
        return Vec::new();
    }
    let bw = i32::from(w.border_width);
    let origin = i16::try_from(-bw).unwrap_or(i16::MIN);
    let outer = xfixes::RegionRect {
        x: origin,
        y: origin,
        width: u16::try_from(i32::from(w.width) + 2 * bw).unwrap_or(u16::MAX),
        height: u16::try_from(i32::from(w.height) + 2 * bw).unwrap_or(u16::MAX),
    };
    let inner = xfixes::RegionRect {
        x: 0,
        y: 0,
        width: w.width,
        height: w.height,
    };
    crate::nested::subtract_regions(&[outer], &[inner])
}

/// Report protocol DAMAGE over `window`'s border ring (#143).
///
/// Xorg reaches this for free: a border repaint is an ordinary
/// `PolyFillRect` GC op on `GetWindowPixmap(pWin)`
/// (`mi/miexpose.c:448-471`, `:558`), which the DAMAGE wrapper
/// intercepts at `damagePolyFillRect` (`miext/damage/damage.c:1194`).
/// yserver paints its ring inside the render backend
/// (`paint_window_border`), which wakes our own compositor loop but
/// never enters the DAMAGE fanout — so a compositor doing partial
/// repaint from `EXT_buffer_age` (picom) keeps a different stale ring
/// in each back buffer and alternates between them at frame rate.
/// That is the awesome focused/unfocused border flicker.
///
/// Non-viewable windows drop out inside
/// [`accumulate_damage_to_state`], matching Xorg's `pWin->viewable`
/// gate at `dix/window.c:1581`.
pub fn accumulate_damage_border_to_state(
    state: &mut ServerState,
    window: ResourceId,
) -> Vec<ClientId> {
    let ring = border_annulus_rects(state, window);
    accumulate_damage_region_to_state(state, window, &ring)
}

/// Mapped `InputOutput` children of `drawable`, as rects in the
/// drawable's local coords (a child's x/y are already relative to its
/// parent). These are the regions `ClipByChildren` subtracts from a
/// window's effective paint/damage clip; `InputOnly` and unmapped
/// children never clip, matching Xorg's `clipList` computation.
/// Empty for pixmaps (no children) and for childless windows.
pub fn mapped_child_clip_rects(
    state: &ServerState,
    drawable: ResourceId,
) -> Vec<xfixes::RegionRect> {
    state
        .resources
        .children(drawable)
        .iter()
        .filter_map(|child| state.resources.window(*child))
        .filter(|w| {
            w.class == crate::resources::WindowClass::InputOutput
                && w.map_state != crate::resources::MapState::Unmapped
        })
        .flat_map(|w| {
            let rect = xfixes::RegionRect {
                x: w.x,
                y: w.y,
                width: w.width,
                height: w.height,
            };
            // A shaped child covers only its bounding shape (Xorg's
            // `borderSize`, `dix/window.c:1747-1770`); the shape is
            // relative to its content origin.
            match state.shape_windows.get(&w.id).and_then(|s| s.bounding()) {
                Some(shape) => {
                    let bw = i16::try_from(w.border_width).unwrap_or(i16::MAX);
                    let shape = crate::nested::offset_rects(
                        shape.clone(),
                        w.x.saturating_add(bw),
                        w.y.saturating_add(bw),
                    );
                    crate::nested::intersect_regions(&[rect], &shape)
                }
                None => vec![rect],
            }
        })
        .collect()
}

/// Env-gated (`YSERVER_TRAY_DEBUG=1`) diagnostic for the intermittent
/// systray sliver/blank bug — a timing race in COMPOSITE manual-redirect
/// backing capture vs the notification-area's recomposite read.
///
/// Logs, for each ClipByChildren paint/damage on a *window with mapped
/// children* (i.e. a tray socket holding an XEMBED plug), the socket's
/// geometry + map-state, its `redirected_backing` state, each child's
/// xid/geometry/class/map-state, and the requested-vs-clipped rects. A
/// blank/sliver run can then be correlated frame-by-frame: a socket whose
/// plug is at a stale/partial geometry at Clear time, or whose backing is
/// absent/mismatched, is the dropped frame. `op` distinguishes the paint
/// (`fill`) from the damage (`damage`) decision in the same Clear cycle.
///
/// Cheap no-op when the env var is unset. Pixmaps and childless windows
/// are skipped (not sockets). REMOVE once the redirect-backing race is
/// fixed.
pub fn log_clip_by_children_debug(
    state: &ServerState,
    drawable: ResourceId,
    op: &str,
    requested: &[xfixes::RegionRect],
    clipped: &[xfixes::RegionRect],
) {
    use std::fmt::Write as _;

    if std::env::var_os("YSERVER_TRAY_DEBUG").is_none() {
        return;
    }
    let Some(w) = state.resources.window(drawable) else {
        return;
    };
    let children = state.resources.children(drawable);
    if children.is_empty() {
        return;
    }

    let redirect = w.redirected_backing.as_ref().map_or_else(
        || "none".to_string(),
        |b| format!("{}x{}d{}", b.width, b.height, b.depth),
    );
    let mut kids = String::new();
    for child in children {
        if let Some(cw) = state.resources.window(*child) {
            let _ = write!(
                kids,
                " 0x{:x}[{}x{}@{},{} {:?} {:?}]",
                child.0, cw.width, cw.height, cw.x, cw.y, cw.class, cw.map_state
            );
        }
    }
    log::info!(
        target: "yserver_core::core_loop::tray",
        "TRAY {op} win=0x{:x} geom={}x{}@{},{} {:?} redirect={} requested={} clipped={} children={{{}}}",
        drawable.0,
        w.width,
        w.height,
        w.x,
        w.y,
        w.map_state,
        redirect,
        format_damage_rects(requested),
        format_damage_rects(clipped),
        kids,
    );
}

/// ClipByChildren-aware variant of [`accumulate_damage_full_to_state`].
///
/// X.Org clips RENDER paint — and the damage it generates — to the
/// destination window's `clipList` = its geometry MINUS its mapped
/// children. That is the default `ClipByChildren` picture mode
/// (`render/picture.c:719`, `render/mipict.c:117`); the damage
/// extension then gates on a non-empty composite clip
/// (`miext/damage/damage.c:462,474`). When a window is fully covered
/// by a mapped child — e.g. a mate-panel systray socket fully covered
/// by its embedded XEMBED icon — the clip is empty and Xorg emits NO
/// damage. yserver historically damaged the full drawable regardless,
/// which fed a self-sustaining damage → recomposite → clear loop in
/// the notification area (~485 FillRectangles/socket per frame).
///
/// This emits damage only on the portion of `drawable` NOT covered by
/// mapped `InputOutput` children. For pixmaps (no children) it is
/// identical to [`accumulate_damage_full_to_state`].
pub fn accumulate_damage_clip_by_children_to_state(
    state: &mut ServerState,
    drawable: ResourceId,
) -> Vec<ClientId> {
    let full = drawable_full_rect(state, drawable);
    if full.width == 0 || full.height == 0 {
        return Vec::new();
    }

    let child_rects = mapped_child_clip_rects(state, drawable);

    let visible = if child_rects.is_empty() {
        vec![full]
    } else {
        crate::nested::subtract_regions(&[full], &child_rects)
    };

    log_clip_by_children_debug(state, drawable, "damage", &[full], &visible);

    accumulate_damage_region_to_state(state, drawable, &visible)
}

pub fn accumulate_damage_to_state(
    state: &mut ServerState,
    drawable: ResourceId,
    x: i16,
    y: i16,
    width: u16,
    height: u16,
) -> Vec<ClientId> {
    accumulate_damage_region_to_state(
        state,
        drawable,
        &[xfixes::RegionRect {
            x,
            y,
            width,
            height,
        }],
    )
}

/// DAMAGE for one drawing op over `region` of `drawable` (its own
/// coordinates), as Xorg's `damageRegionAppend` +
/// `damageRegionProcessPending` (`miext/damage/damage.c:125-300`): every
/// damage object the op reaches gets the region clipped to its drawable,
/// and reports it whole, in damage-list order.
pub fn accumulate_damage_region_to_state(
    state: &mut ServerState,
    drawable: ResourceId,
    region: &[xfixes::RegionRect],
) -> Vec<ClientId> {
    let region: Vec<xfixes::RegionRect> = region
        .iter()
        .filter(|r| r.width != 0 && r.height != 0)
        .copied()
        .collect();
    if region.is_empty() {
        return Vec::new();
    }
    // Nothing on screen ⇒ nothing damaged (issue #97). Xorg keeps
    // off-screen backing IFF `pWin->viewable` (asserted in
    // composite/compwindow.c:240), so drawing into a non-viewable window
    // produces no screen change and no DamageNotify.
    //
    // Gating the LEAF alone is sufficient: viewability is inherited, so a
    // Viewable leaf guarantees every ancestor is Viewable too. Conversely a
    // non-viewable leaf must not propagate up — that is exactly the #97 bug,
    // where i3's unmapped frame kept receiving damage from the still-mapped
    // terminal inside it and fastcompmgr faithfully re-composited a window
    // that had left the workspace.
    //
    // Pixmaps are unaffected: they sit outside the window tree, so the
    // lookup misses and the gate does not apply.
    if state
        .resources
        .window(drawable)
        .is_some_and(|w| w.map_state != crate::resources::MapState::Viewable)
    {
        return Vec::new();
    }
    // TEMP DIAG (round 3): gated on `YSERVER_DAMAGE_BACKTRACE=1`.
    // After removing the configure-damage emit, the
    // notification-area-applet loop is only 15% quieter — there's
    // a second emitter on the same tray-socket xids. REMOVE after
    // the second emitter is identified.
    if std::env::var_os("YSERVER_DAMAGE_BACKTRACE").is_some() {
        let bt = std::backtrace::Backtrace::force_capture();
        log::info!(
            "DIAG accumulate_damage: drawable=0x{:x} region={}\n{}",
            drawable.0,
            format_damage_rects(&region),
            bt,
        );
    }
    let timestamp = state.timestamp_now();
    let mut hits: Vec<Hit> = Vec::new();

    // The leaf level — pixmaps and windows alike. Pixmaps have no parent,
    // so a `state.resources.window(drawable)` miss at the top of the loop
    // ends the walk before the first step.
    let mut level_region = crate::core_loop::clip_list::canonical(region);
    collect_level(state, drawable, &level_region, &mut hits);

    let mut current = drawable;
    for _ in 0..ANCESTOR_WALK_MAX_DEPTH {
        // A redirected window draws into its own pixmap, whose damage list
        // its ancestors are not on (`getDrawableDamageRef`,
        // `miext/damage/damage.c:86-110`).
        if state.composite_redirects.window_mode(current).is_some() {
            break;
        }
        let Some(current_win) = state.resources.window(current) else {
            break;
        };
        // The child's content origin in the parent: `x + bw`
        // (`dix/window.c`, `drawable.x = parent.x + x + bw`).
        let bw = i32::from(current_win.border_width);
        let (off_x, off_y, parent) = (
            i32::from(current_win.x) + bw,
            i32::from(current_win.y) + bw,
            current_win.parent,
        );
        // Root self-parents (`parent == self`, see resources.rs
        // ROOT_WINDOW initialiser).
        if parent == current {
            break;
        }
        let Some(parent_win) = state.resources.window(parent) else {
            break;
        };
        // Into the parent's coordinates, clipped to its extent: an ancestor
        // sees the part of the paint that lies inside it.
        let bounds = xfixes::RegionRect {
            x: 0,
            y: 0,
            width: parent_win.width,
            height: parent_win.height,
        };
        level_region = crate::core_loop::clip_list::intersect(
            &crate::core_loop::clip_list::translate(level_region, off_x, off_y),
            &[bounds],
        );
        if level_region.is_empty() {
            break;
        }
        current = parent;
        collect_level(state, parent, &level_region, &mut hits);
    }

    if hits.is_empty() {
        return Vec::new();
    }
    // `damageRegionProcessPending` walks the list from its head.
    hits.sort_by_key(|h| {
        std::cmp::Reverse(
            state
                .damage_objects
                .get(&h.damage_id)
                .map_or((0, 0), |d| (d.list_pos, h.damage_id)),
        )
    });
    let mut pending: Vec<PendingNotify> = Vec::new();
    for hit in hits {
        commit_hit(state, hit, &mut pending);
    }
    if pending.is_empty() {
        return Vec::new();
    }

    // The notification makes freshly-rendered redirected content externally
    // observable. A deferred renderer must submit its writes before the
    // compositor can receive this wake and sample the backing pixmap.
    state.damage_notify_flush_pending = true;

    let mut dropped: Vec<ClientId> = Vec::new();
    for n in pending {
        let geometry = n.geometry;
        let area = n.area;
        let extras = fanout_event_to_clients(state, &[n.owner], |buf, seq, order| {
            encode_damage_notify(buf, order, seq, &n, timestamp, area, geometry);
        });
        for cid in extras {
            if !dropped.contains(&cid) {
                dropped.push(cid);
            }
        }
    }
    dropped
}

/// Xorg's `damageSetWindowPixmap` (`miext/damage/damage.c:1547-1575`) for
/// each window `compSetPixmap` visits from `window`
/// (`composite/compwindow.c:118-148`): a window's damage leaves the old
/// pixmap's list and is prepended to the new one's, newest first, so the
/// oldest ends at the head. Descendants with their own backing are skipped.
pub fn move_window_damage_to_list_head(state: &mut ServerState, window: ResourceId) {
    let mut stack = vec![window];
    while let Some(w) = stack.pop() {
        if w != window
            && state
                .resources
                .window(w)
                .is_some_and(|win| win.redirected_backing.is_some())
        {
            continue;
        }
        let mut ids: Vec<(u64, u32)> = state
            .damage_objects
            .iter()
            .filter(|(_, d)| d.drawable == w)
            .map(|(id, d)| (d.created, *id))
            .collect();
        ids.sort_unstable_by(|a, b| b.cmp(a));
        for (_, id) in ids {
            state.damage_list_clock += 1;
            let tick = state.damage_list_clock;
            if let Some(d) = state.damage_objects.get_mut(&id) {
                d.list_pos = tick;
            }
        }
        // `TraverseTree` visits firstChild first; children are stored
        // bottom-most first.
        stack.extend(state.resources.children(w).iter().copied());
    }
}

/// Re-report an already-accumulated damage object without appending a
/// new rect. This mirrors Xorg's `ProcDamageSubtract` follow-up path:
/// after subtracting a non-None repair region, any remaining damage is
/// reported immediately for coalesced levels (Delta / BoundingBox /
/// NonEmpty) instead of waiting for a future drawing op.
pub fn report_existing_damage_to_state(state: &mut ServerState, damage_id: u32) -> Vec<ClientId> {
    let Some((owner, drawable, level, fired, rects)) =
        state.damage_objects.get(&damage_id).map(|d| {
            (
                d.owner,
                d.drawable,
                d.level,
                d.pending_notify_fired,
                d.rects.clone(),
            )
        })
    else {
        return Vec::new();
    };
    if fired || rects.is_empty() || !state.clients.contains_key(&owner.0) {
        return Vec::new();
    }

    let geom_full = drawable_full_rect(state, drawable);
    let (geom_x, geom_y) = if state.resources.window(drawable).is_some() {
        let (ax, ay) = state.resources.window_absolute_position(drawable);
        (
            i16::try_from(ax).unwrap_or(i16::MAX),
            i16::try_from(ay).unwrap_or(i16::MAX),
        )
    } else {
        (0, 0)
    };
    let geometry = x11damage::Rectangle {
        x: geom_x,
        y: geom_y,
        width: geom_full.width,
        height: geom_full.height,
    };

    let mut pending: Vec<PendingNotify> = Vec::new();
    match level {
        x11damage::report_level::RAW_RECTANGLES | x11damage::report_level::DELTA_RECTANGLES => {
            let more_base = level;
            for (idx, rect) in rects.iter().enumerate() {
                let level = if idx + 1 < rects.len() {
                    more_base | x11damage::MORE_FLAG
                } else {
                    more_base
                };
                pending.push(PendingNotify {
                    owner,
                    damage_id,
                    level,
                    drawable: drawable.0,
                    area: x11damage::Rectangle {
                        x: rect.x,
                        y: rect.y,
                        width: rect.width,
                        height: rect.height,
                    },
                    geometry,
                });
            }
        }
        x11damage::report_level::BOUNDING_BOX => {
            let extents = crate::nested::region_extents(&rects);
            pending.push(PendingNotify {
                owner,
                damage_id,
                level,
                drawable: drawable.0,
                area: x11damage::Rectangle {
                    x: extents.x,
                    y: extents.y,
                    width: extents.width,
                    height: extents.height,
                },
                geometry,
            });
        }
        x11damage::report_level::NON_EMPTY => {
            pending.push(PendingNotify {
                owner,
                damage_id,
                level,
                drawable: drawable.0,
                area: x11damage::Rectangle {
                    x: 0,
                    y: 0,
                    width: geom_full.width,
                    height: geom_full.height,
                },
                geometry,
            });
        }
        _ => return Vec::new(),
    }

    if let Some(d) = state.damage_objects.get_mut(&damage_id) {
        d.pending_notify_fired = true;
        d.last_reported_geometry = Some(geometry);
    }
    if let Some(d) = state.damage_objects.get(&damage_id) {
        log::trace!(
            "damage_notify_rereport: damage=0x{:x} owner={} drawable=0x{:x} level={} rects_n={} rects={}",
            damage_id,
            owner.0,
            drawable.0,
            level,
            d.rects.len(),
            format_damage_rects(&d.rects),
        );
    }

    let timestamp = state.timestamp_now();
    let mut dropped: Vec<ClientId> = Vec::new();
    for n in pending {
        let geometry = n.geometry;
        let area = n.area;
        let extras = fanout_event_to_clients(state, &[n.owner], |buf, seq, order| {
            encode_damage_notify(buf, order, seq, &n, timestamp, area, geometry);
        });
        for cid in extras {
            if !dropped.contains(&cid) {
                dropped.push(cid);
            }
        }
    }
    dropped
}

/// One damage object reached by a drawing op: its drawable's part of the
/// op's region, in that drawable's coordinates.
struct Hit {
    damage_id: u32,
    drawable: ResourceId,
    rects: Vec<xfixes::RegionRect>,
    /// The drawable's current geometry (`damageGetGeometry`).
    geometry: x11damage::Rectangle,
}

/// The damage objects keyed on `level_drawable` or one of its
/// NameWindowPixmap aliases, each given `rects` (already clipped to it).
fn collect_level(
    state: &ServerState,
    level_drawable: ResourceId,
    rects: &[xfixes::RegionRect],
    hits: &mut Vec<Hit>,
) {
    // Resolve `level_drawable` through the NameWindowPixmap alias
    // indirection. Compositors such as picom subscribe DAMAGE on the
    // pixmap returned by `NameWindowPixmap`, not on the window itself.
    // Xorg's `getDrawableDamageRef(&pWin->drawable)` resolves this
    // automatically via `GetWindowPixmap(pWin)` → backing pixmap →
    // that pixmap's damage list (miext/damage/damage.c:90-110). The
    // alias pixmap and the window share the backing pixmap's damage
    // list, so a single `DamageDamageRegion(&pWin->drawable, ...)` hits
    // every alias subscription.
    let mut drawables = vec![level_drawable];
    if let Some(window) = state.resources.window(level_drawable) {
        drawables.extend(
            window
                .composite_named_pixmaps
                .iter()
                .map(|a| a.client_pixmap),
        );
    }
    let damage_ids: Vec<u32> = state
        .damage_objects
        .iter()
        .filter(|(_, dmg)| drawables.contains(&dmg.drawable))
        .map(|(id, _)| *id)
        .collect();
    // Logged at trace; enable with
    // `yserver_core::core_loop::damage_fanout=trace` in `RUST_LOG`.
    log::trace!(
        "damage_fanout: level_drawable=0x{:x} region={} match_ids={}",
        level_drawable.0,
        format_damage_rects(rects),
        damage_ids.len(),
    );
    if damage_ids.is_empty() {
        return;
    }
    // Per-level geometry — the damaged drawable's CURRENT geometry
    // (X11 DAMAGE proto + Xorg damageext.c: fills from
    // pDrawable->{x, y, width, height}; window drawables carry
    // root-relative x/y, pixmaps are always (0, 0)). Compositors use it
    // to map the damage rect back into screen space.
    let full = drawable_full_rect(state, level_drawable);
    let (geom_x, geom_y) = if state.resources.window(level_drawable).is_some() {
        let (ax, ay) = state.resources.window_absolute_position(level_drawable);
        (
            i16::try_from(ax).unwrap_or(i16::MAX),
            i16::try_from(ay).unwrap_or(i16::MAX),
        )
    } else {
        (0, 0)
    };
    let geometry = x11damage::Rectangle {
        x: geom_x,
        y: geom_y,
        width: full.width,
        height: full.height,
    };
    for damage_id in damage_ids {
        hits.push(Hit {
            damage_id,
            drawable: level_drawable,
            rects: rects.to_vec(),
            geometry,
        });
    }
}

/// Add `hit` to its damage object and queue the notifies its level asks
/// for (`DamageReportDamage` + `DamageExtReport`,
/// `miext/damage/damage.c:1935-1975`, `damageext/damageext.c:86-153`).
fn commit_hit(state: &mut ServerState, hit: Hit, pending: &mut Vec<PendingNotify>) {
    let Some(dmg) = state.damage_objects.get_mut(&hit.damage_id) else {
        return;
    };
    dmg.rects.extend(hit.rects.iter().copied());
    let (level, fired, owner, last_reported_geometry) = (
        dmg.level,
        dmg.pending_notify_fired,
        dmg.owner,
        dmg.last_reported_geometry,
    );
    let geometry_changed = last_reported_geometry.is_some_and(|prev| prev != hit.geometry);
    let owner_alive = state.clients.contains_key(&owner.0);
    let notify = |level: u8, area: x11damage::Rectangle| PendingNotify {
        owner,
        damage_id: hit.damage_id,
        level,
        drawable: hit.drawable.0,
        area,
        geometry: hit.geometry,
    };
    // Raw / Delta / BoundingBox report every op; only NonEmpty has the
    // one-shot-per-Subtract semantic, re-reported when the geometry the
    // last notify carried is stale.
    let queued = match level {
        x11damage::report_level::RAW_RECTANGLES
        | x11damage::report_level::DELTA_RECTANGLES
        | x11damage::report_level::BOUNDING_BOX
            if owner_alive =>
        {
            // `DamageExtNotify` flags every rect but the last of a report
            // with DamageNotifyMore (`damageext/damageext.c:103-115`).
            let more = level != x11damage::report_level::BOUNDING_BOX;
            let last = hit.rects.len() - 1;
            for (i, r) in hit.rects.iter().enumerate() {
                let level = if more && i < last {
                    level | x11damage::MORE_FLAG
                } else {
                    level
                };
                pending.push(notify(
                    level,
                    x11damage::Rectangle {
                        x: r.x,
                        y: r.y,
                        width: r.width,
                        height: r.height,
                    },
                ));
            }
            true
        }
        // NonEmpty events carry the drawable's full extent in `area`
        // (`DamageExtNotify` with `pBoxes == NULL`,
        // `damageext/damageext.c:117-126`).
        x11damage::report_level::NON_EMPTY if owner_alive && (!fired || geometry_changed) => {
            pending.push(notify(
                level,
                x11damage::Rectangle {
                    x: 0,
                    y: 0,
                    width: hit.geometry.width,
                    height: hit.geometry.height,
                },
            ));
            true
        }
        _ => false,
    };
    log::trace!(
        "damage_notify_{}: damage=0x{:x} owner={} drawable=0x{:x} level={} fired={} \
         owner_alive={} rects={}",
        if queued { "queue" } else { "skip" },
        hit.damage_id,
        owner.0,
        hit.drawable.0,
        level,
        fired,
        owner_alive,
        format_damage_rects(&hit.rects),
    );
    if queued && let Some(d) = state.damage_objects.get_mut(&hit.damage_id) {
        d.pending_notify_fired = true;
        d.last_reported_geometry = Some(hit.geometry);
    }
}

fn encode_damage_notify(
    buf: &mut Vec<u8>,
    byte_order: yserver_protocol::x11::ClientByteOrder,
    seq: SequenceNumber,
    n: &PendingNotify,
    timestamp: u32,
    area: x11damage::Rectangle,
    geometry: x11damage::Rectangle,
) {
    let evt = x11damage::encode_damage_notify_event(
        byte_order,
        DAMAGE_FIRST_EVENT,
        seq,
        n.level,
        n.drawable,
        n.damage_id,
        timestamp,
        area,
        geometry,
    );
    buf.extend_from_slice(&evt);
}

fn drawable_full_rect(state: &ServerState, drawable: ResourceId) -> xfixes::RegionRect {
    if let Some(window) = state.resources.window(drawable) {
        return xfixes::RegionRect {
            x: 0,
            y: 0,
            width: window.width,
            height: window.height,
        };
    }
    state.resources.pixmap(drawable).map_or(
        xfixes::RegionRect {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        },
        |pixmap| xfixes::RegionRect {
            x: 0,
            y: 0,
            width: pixmap.width,
            height: pixmap.height,
        },
    )
}

#[cfg(test)]
mod tests {
    //! Damage fanout — ancestor-walk + clipping coverage.
    //!
    //! Regression context: marco-with-compositing on v2 shows
    //! "windows render shadow only / hover flicker" because paint
    //! into a CHILD of a redirected top-level fires damage on the
    //! child's xid alone, never on the top-level marco subscribed
    //! to. X11 DAMAGE spec says ancestor windows receive damage
    //! from descendant paint (translated to ancestor coords +
    //! clipped to ancestor extent). These tests pin the fix so the
    //! regression can't return silently.
    use super::*;
    use crate::{
        resources::ROOT_WINDOW,
        server::{ClientState, DamageObject, ServerState},
    };
    use std::{
        collections::{HashMap, HashSet, VecDeque},
        os::unix::net::UnixStream,
        sync::{
            Arc, Mutex,
            atomic::{AtomicU16, Ordering},
        },
    };
    use yserver_protocol::x11::{ClientByteOrder, CreatePixmapRequest, CreateWindowRequest};

    fn make_test_writer() -> Arc<Mutex<crate::transport::Transport>> {
        // Pair of sockets; we keep the read end alive in the same
        // Arc so writes don't EPIPE during tests. We never inspect
        // the data sent — assertions are against `damage_objects`.
        let (a, _b) = UnixStream::pair().expect("UnixStream::pair");
        Arc::new(Mutex::new(crate::transport::Transport::Unix(a)))
    }

    fn add_client(state: &mut ServerState, client_id: u32, base: u32) {
        state.clients.insert(
            client_id,
            ClientState {
                writer: make_test_writer(),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0)),
                resource_id_base: base,
                resource_id_mask: 0x000F_FFFF,
                event_masks: HashMap::new(),
                save_set: HashSet::new(),
                big_requests_enabled: false,
                xi2_masks: HashMap::new(),
                xi1_event_classes: HashSet::new(),
                xi1_window_event_classes: HashMap::new(),
                outbound: VecDeque::new(),
                watching_writable: false,
                write_failed: false,
                gone: false,
                focused_window: ROOT_WINDOW,
                reader_control: None,
                is_local: true,
                fd_passing: true,
                internal: false,
            },
        );
    }

    fn add_window(
        state: &mut ServerState,
        owner_client: u32,
        wid: u32,
        parent: ResourceId,
        x: i16,
        y: i16,
        w: u16,
        h: u16,
    ) -> ResourceId {
        let id = ResourceId(wid);
        state.resources.create_window(
            ClientId(owner_client),
            CreateWindowRequest {
                depth: 24,
                window: id,
                parent,
                x,
                y,
                width: w,
                height: h,
                border_width: 0,
                class: 1,
                visual: crate::resources::ROOT_VISUAL,
                ..Default::default()
            },
        );
        let _ = state.resources.map_window(id);
        id
    }

    fn add_damage_on(state: &mut ServerState, owner: u32, damage_id: u32, drawable: ResourceId) {
        add_damage_on_with_level(state, owner, damage_id, drawable, 3);
    }

    fn add_damage_on_with_level(
        state: &mut ServerState,
        owner: u32,
        damage_id: u32,
        drawable: ResourceId,
        level: u8,
    ) {
        state.damage_objects.insert(
            damage_id,
            DamageObject {
                owner: ClientId(owner),
                drawable,
                level,
                rects: Vec::new(),
                pending_notify_fired: false,
                last_reported_geometry: None,
                list_pos: 0,
                created: 0,
            },
        );
    }

    /// Pair-stream test client: keeps the reader half of the
    /// socketpair alive so emitted events can be drained and
    /// inspected. Wraps `add_client` + returns the reader.
    fn add_client_with_reader(state: &mut ServerState, client_id: u32, base: u32) -> UnixStream {
        let (a, b) = UnixStream::pair().expect("UnixStream::pair");
        b.set_nonblocking(true)
            .expect("set test reader nonblocking");
        state.clients.insert(
            client_id,
            ClientState {
                writer: Arc::new(Mutex::new(crate::transport::Transport::Unix(a))),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0)),
                resource_id_base: base,
                resource_id_mask: 0x000F_FFFF,
                event_masks: HashMap::new(),
                save_set: HashSet::new(),
                big_requests_enabled: false,
                xi2_masks: HashMap::new(),
                xi1_event_classes: HashSet::new(),
                xi1_window_event_classes: HashMap::new(),
                outbound: VecDeque::new(),
                watching_writable: false,
                write_failed: false,
                gone: false,
                focused_window: ROOT_WINDOW,
                reader_control: None,
                is_local: true,
                fd_passing: true,
                internal: false,
            },
        );
        b
    }

    /// Drain the test reader plus any buffered outbound and count
    /// `DamageNotify` events (event code 91). Each X11 event is 32
    /// bytes; first byte at each 32-byte chunk is the event code.
    fn count_damage_notify_events(
        reader: &mut UnixStream,
        state: &ServerState,
        client_id: u32,
    ) -> usize {
        use std::io::Read as _;
        let mut buf = Vec::new();
        if let Some(client) = state.clients.get(&client_id) {
            buf.extend(client.outbound.iter().copied());
        }
        let mut chunk = [0u8; 4096];
        while let Ok(n) = reader.read(&mut chunk) {
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        buf.chunks_exact(32)
            .filter(|c| c[0] == DAMAGE_FIRST_EVENT)
            .count()
    }

    /// #143 — the border annulus is Xorg's `borderClip − winSize`
    /// (`dix/window.c:1585-1587`): four strips around the content,
    /// with the top and left ones at NEGATIVE window-local origins.
    /// Geometry is the awesome window from the capture: 1276×704 with
    /// `border_width = 2`.
    #[test]
    fn the_border_annulus_is_the_region_difference_at_negative_origins() {
        let mut state = ServerState::new();
        add_client(&mut state, 1, 0x0030_0000);
        let w = add_window(&mut state, 1, 0x0030_0001, ROOT_WINDOW, 100, 50, 1276, 704);
        state
            .resources
            .window_mut(w)
            .expect("seeded window")
            .border_width = 2;

        assert_eq!(
            border_annulus_rects(&state, w),
            vec![
                xfixes::RegionRect {
                    x: -2,
                    y: -2,
                    width: 1280,
                    height: 2
                },
                xfixes::RegionRect {
                    x: -2,
                    y: 0,
                    width: 2,
                    height: 704
                },
                xfixes::RegionRect {
                    x: 1276,
                    y: 0,
                    width: 2,
                    height: 704
                },
                xfixes::RegionRect {
                    x: -2,
                    y: 704,
                    width: 1280,
                    height: 2
                },
            ],
            "the ring must be the four strips of `borderClip − winSize`, \
             not one bounding rect over the content",
        );
        // The strips tile the annulus exactly: 2 * (1280 * 2) + 2 * (2 * 704).
        let area: u32 = border_annulus_rects(&state, w)
            .iter()
            .map(|r| u32::from(r.width) * u32::from(r.height))
            .sum();
        assert_eq!(area, 1280 * 708 - 1276 * 704, "outer extent minus winSize");
    }

    /// `HasBorder(pWin)` (`dix/window.c:1586`) is Xorg's early-out and
    /// ours: no border width, no ring, no damage. The root is excluded
    /// too — its ring is not a thing any compositor composites.
    #[test]
    fn the_border_annulus_is_empty_without_a_border_and_on_the_root() {
        let mut state = ServerState::new();
        add_client(&mut state, 1, 0x0030_0000);
        let w = add_window(&mut state, 1, 0x0030_0001, ROOT_WINDOW, 0, 0, 64, 64);
        assert!(
            border_annulus_rects(&state, w).is_empty(),
            "bw == 0 must produce no ring",
        );
        state
            .resources
            .window_mut(ROOT_WINDOW)
            .expect("root")
            .border_width = 2;
        assert!(
            border_annulus_rects(&state, ROOT_WINDOW).is_empty(),
            "the root never reports a border ring",
        );
    }

    /// #143 — the negative origins have to survive `accumulate_at_level`'s
    /// i32 → i16/u16 saturating cast and land in the damage object as
    /// negative rects. Xorg reports them the same way: `damageDamageBox`
    /// translates by the window's INNER origin
    /// (`miext/damage/damage.c:1194` → `damageRegionAppend`), so border
    /// pixels are at negative window coordinates there too.
    #[test]
    fn border_damage_reaches_the_damage_object_at_negative_origins() {
        let mut state = ServerState::new();
        add_client(&mut state, 1, 0x0030_0000);
        let w = add_window(&mut state, 1, 0x0030_0001, ROOT_WINDOW, 100, 50, 1276, 704);
        state
            .resources
            .window_mut(w)
            .expect("seeded window")
            .border_width = 2;
        // Raw level so `area` carries the real rect rather than the
        // NonEmpty full-extent substitute.
        add_damage_on_with_level(
            &mut state,
            1,
            0xd143_0001,
            w,
            x11damage::report_level::RAW_RECTANGLES,
        );

        let _dropped = accumulate_damage_border_to_state(&mut state, w);

        let dmg = state
            .damage_objects
            .get(&0xd143_0001)
            .expect("damage object");
        assert_eq!(
            dmg.rects,
            border_annulus_rects(&state, w),
            "every ring strip must be accumulated verbatim, negatives included",
        );
        assert!(
            dmg.rects.iter().any(|r| r.x < 0 || r.y < 0),
            "the ring's top/left strips are at negative origins by construction",
        );
        assert!(dmg.pending_notify_fired, "a notify must have been queued");
    }

    /// The ancestor walk translates by the child's content origin, `x + bw`
    /// (`dix/window.c:888`): a child's x/y are its OUTER corner, so a
    /// child at (100, 50) with `bw = 2` damages its parent from (100, 50)
    /// (Xorg, redirected-backing probe: `80,80 58x58` for (80,80) bw 4).
    #[test]
    fn the_ring_translates_to_the_outer_origin_in_the_parent() {
        let mut state = ServerState::new();
        add_client(&mut state, 1, 0x0030_0000);
        let parent = add_window(&mut state, 1, 0x0030_0001, ROOT_WINDOW, 0, 0, 2000, 1000);
        let child = add_window(&mut state, 1, 0x0030_0002, parent, 100, 50, 1276, 704);
        state
            .resources
            .window_mut(child)
            .expect("child")
            .border_width = 2;
        add_damage_on_with_level(
            &mut state,
            1,
            0xd143_0002,
            parent,
            x11damage::report_level::RAW_RECTANGLES,
        );

        let _dropped = accumulate_damage_border_to_state(&mut state, child);

        let rects = &state
            .damage_objects
            .get(&0xd143_0002)
            .expect("parent damage object")
            .rects;
        assert!(
            rects.contains(&xfixes::RegionRect {
                x: 100,
                y: 50,
                width: 1280,
                height: 2
            }),
            "the top strip must land at the child's OUTER origin in the \
             parent, got {rects:?}",
        );
        assert!(
            rects.contains(&xfixes::RegionRect {
                x: 100,
                y: 756,
                width: 1280,
                height: 2
            }),
            "the bottom strip must land one border width past the content, \
             got {rects:?}",
        );
    }

    /// Issue #97 (i3 + fastcompmgr): switching workspace left the
    /// previous workspace's window still painted, dimmed, on the new
    /// one. i3 unmaps only its FRAME window; the terminal reparented
    /// inside it stays mapped (correct X11 — a child of an unmapped
    /// parent becomes Unviewable and generates no UnmapNotify) and
    /// keeps drawing. We kept reporting that drawing as damage on the
    /// unmapped frame — 29 of 34 DamageNotify events in the bee
    /// trace, at the frame's old geometry — so the compositor
    /// faithfully re-composited a window that is not on screen.
    ///
    /// Xorg's rule, asserted in `composite/compwindow.c:240`: a window
    /// has off-screen backing IFF `pWin->viewable`. Nothing is drawn
    /// for a non-viewable window, so nothing is damaged.
    #[test]
    fn paint_into_child_of_unmapped_ancestor_fires_no_damage() {
        let mut state = ServerState::new();
        add_client(&mut state, 1, 0x0010_0000);
        // Frame W (i3's frame) with child C (the reparented terminal),
        // damage subscriptions on both — exactly the trace's shape.
        let w_id = add_window(&mut state, 1, 0x0010_0001, ROOT_WINDOW, 100, 200, 500, 400);
        let c_id = add_window(&mut state, 1, 0x0010_0002, w_id, 10, 20, 50, 60);
        add_damage_on(&mut state, 1, 0xe000_0001, c_id);
        add_damage_on(&mut state, 1, 0xe000_0002, w_id);

        // Workspace switch: i3 unmaps the FRAME only. C stays mapped
        // and becomes Unviewable by inheritance.
        let _ = state.resources.unmap_window(w_id);
        assert_eq!(
            state.resources.window(c_id).expect("C").map_state,
            crate::resources::MapState::Unviewable,
            "precondition: unmapping the frame must leave C Unviewable, \
             not Unmapped — that is what makes it keep drawing",
        );

        // The terminal keeps painting into C.
        let _ = accumulate_damage_to_state(&mut state, c_id, 5, 5, 30, 40);

        // Neither level may report: the subtree contributes no pixels
        // to the screen, so a compositor must not be told to repaint.
        let c_dmg = state
            .damage_objects
            .get(&0xe000_0001)
            .expect("C's damage object");
        assert!(
            c_dmg.rects.is_empty() && !c_dmg.pending_notify_fired,
            "C is Unviewable — no damage may be recorded or fired (got {} rects, fired={})",
            c_dmg.rects.len(),
            c_dmg.pending_notify_fired,
        );
        let w_dmg = state
            .damage_objects
            .get(&0xe000_0002)
            .expect("W's damage object");
        assert!(
            w_dmg.rects.is_empty() && !w_dmg.pending_notify_fired,
            "W is Unmapped — this is the #97 regression gate; the compositor \
             re-composites the stale window when this fires (got {} rects, fired={})",
            w_dmg.rects.len(),
            w_dmg.pending_notify_fired,
        );
    }

    /// Load-bearing test: paint into a child window fires damage on
    /// the child AND on every ancestor up to root, with rectangles
    /// translated through each ancestor's (x, y) offset. Mirrors
    /// the marco-with-compositing case where a redirected top-level
    /// has a DamageCreate on the top-level and a GTK widget child
    /// paints into a descendant.
    #[test]
    fn paint_into_child_fires_damage_on_ancestor() {
        let mut state = ServerState::new();
        add_client(&mut state, 1, 0x0010_0000);
        // Parent W at (100, 200) 500×400, child C at (10, 20) 50×60
        // inside W. Damage subscriptions on BOTH C and W so we can
        // verify the helper hits both levels (the bug case is the
        // ancestor; the child case is the v1 baseline).
        let w_id = add_window(&mut state, 1, 0x0010_0001, ROOT_WINDOW, 100, 200, 500, 400);
        let c_id = add_window(&mut state, 1, 0x0010_0002, w_id, 10, 20, 50, 60);
        add_damage_on(&mut state, 1, 0xe000_0001, c_id);
        add_damage_on(&mut state, 1, 0xe000_0002, w_id);

        // Paint at (5, 5) 30×40 into C.
        let _ = accumulate_damage_to_state(&mut state, c_id, 5, 5, 30, 40);

        // Child level: rect arrives in C's coords (untranslated).
        let c_dmg = state
            .damage_objects
            .get(&0xe000_0001)
            .expect("C's damage object");
        assert_eq!(c_dmg.rects.len(), 1, "C must record exactly one rect");
        let c_rect = c_dmg.rects[0];
        assert_eq!(
            (c_rect.x, c_rect.y, c_rect.width, c_rect.height),
            (5, 5, 30, 40),
            "C-level rect must be in C's coord space",
        );
        assert!(
            c_dmg.pending_notify_fired,
            "C's damage object must have fired its notify",
        );

        // Ancestor level: rect translates by C's (x, y) = (10, 20)
        // into W's coord space; W's extent (500×400) accommodates
        // the full rect so no clipping happens at this level.
        let w_dmg = state
            .damage_objects
            .get(&0xe000_0002)
            .expect("W's damage object — this is the regression gate");
        assert_eq!(
            w_dmg.rects.len(),
            1,
            "W must record exactly one rect via the ancestor walk \
             — pre-fix this is 0 (the load-bearing failure)",
        );
        let w_rect = w_dmg.rects[0];
        assert_eq!(
            (w_rect.x, w_rect.y, w_rect.width, w_rect.height),
            (15, 25, 30, 40),
            "W-level rect must be C's rect translated by C's (x, y) = (10, 20)",
        );
        assert!(
            w_dmg.pending_notify_fired,
            "W's damage object must have fired its notify too",
        );
    }

    /// X11 DAMAGE proto + Xorg `damageext/damageext.c:136-153` —
    /// `DamageExtReport` fires on every damage for Raw/Delta/
    /// BoundingBox levels (one-shot-per-Subtract semantics is
    /// exclusive to `DamageReportNonEmpty`). Cinnamon / muffin
    /// register `BoundingBox` damage; two paints between Subtracts
    /// must yield TWO `DamageNotify` events. Pre-fix the gate
    /// `(!fired || geometry_changed)` was applied uniformly to all
    /// levels, collapsing the second paint into the first cycle and
    /// halving the rate vs Xorg in the captured cinnamon repro
    /// (yserver 176 vs Xorg 315 notifies for the same workload).
    #[test]
    fn bounding_box_level_emits_on_every_paint() {
        let mut state = ServerState::new();
        let mut reader = add_client_with_reader(&mut state, 1, 0x0010_0000);
        let w_id = add_window(&mut state, 1, 0x0010_0001, ROOT_WINDOW, 0, 0, 500, 400);
        add_damage_on_with_level(
            &mut state,
            1,
            0xe000_0001,
            w_id,
            x11damage::report_level::BOUNDING_BOX,
        );

        let _ = accumulate_damage_to_state(&mut state, w_id, 5, 5, 30, 40);
        assert!(
            state.damage_notify_flush_pending,
            "queueing DamageNotify must arm the backend submission boundary"
        );
        assert_eq!(
            count_damage_notify_events(&mut reader, &state, 1),
            1,
            "first paint must emit a DamageNotify on BoundingBox level",
        );

        state.damage_notify_flush_pending = false;
        let _ = accumulate_damage_to_state(&mut state, w_id, 50, 50, 30, 40);
        assert!(
            state.damage_notify_flush_pending,
            "each BoundingBox notification must re-arm the boundary"
        );
        assert_eq!(
            count_damage_notify_events(&mut reader, &state, 1),
            1,
            "BoundingBox level must emit on EVERY paint (per X11 DAMAGE \
             spec + Xorg damageext.c:136-146); paint-without-Subtract \
             still fires. We already drained the first emit so this \
             assertion measures only the second paint's emit. Pre-fix \
             count is 0 because the gate applies NonEmpty's one-shot \
             semantics to all levels — that's the cinnamon scroll-\
             stale regression.",
        );
    }

    /// Damage family: a damage owner over `OUTBOUND_CAP` is flagged for
    /// the core loop to disconnect, and another owner's damage on the same
    /// window still reports.
    #[test]
    fn overflowing_damage_owner_is_flagged_for_disconnect() {
        let mut state = ServerState::new();
        let _slow = add_client_with_reader(&mut state, 1, 0x0010_0000);
        let mut fast = add_client_with_reader(&mut state, 2, 0x0020_0000);
        let w_id = add_window(&mut state, 1, 0x0010_0001, ROOT_WINDOW, 0, 0, 500, 400);
        for (client, damage) in [(1, 0x0010_0002), (2, 0x0020_0002)] {
            add_damage_on_with_level(
                &mut state,
                client,
                damage,
                w_id,
                x11damage::report_level::RAW_RECTANGLES,
            );
        }
        crate::core_loop::client_io::saturate_for_test(state.clients.get_mut(&1).unwrap());
        let _ = accumulate_damage_to_state(&mut state, w_id, 5, 5, 30, 40);
        assert_eq!(
            crate::core_loop::client_io::failed_writers(&state.clients),
            [ClientId(1)]
        );
        assert_eq!(count_damage_notify_events(&mut fast, &state, 2), 1);
    }

    /// Companion: NonEmpty (level 3) keeps its spec-correct one-shot
    /// semantics — emit once on empty→non-empty transition, silent
    /// until Subtract. Ensures the fix doesn't over-reach.
    #[test]
    fn non_empty_level_emits_once_per_subtract_cycle() {
        let mut state = ServerState::new();
        let mut reader = add_client_with_reader(&mut state, 1, 0x0010_0000);
        let w_id = add_window(&mut state, 1, 0x0010_0001, ROOT_WINDOW, 0, 0, 500, 400);
        add_damage_on_with_level(
            &mut state,
            1,
            0xe000_0001,
            w_id,
            x11damage::report_level::NON_EMPTY,
        );

        let _ = accumulate_damage_to_state(&mut state, w_id, 5, 5, 30, 40);
        assert_eq!(
            count_damage_notify_events(&mut reader, &state, 1),
            1,
            "first paint must emit on NonEmpty too",
        );

        let _ = accumulate_damage_to_state(&mut state, w_id, 50, 50, 30, 40);
        assert_eq!(
            count_damage_notify_events(&mut reader, &state, 1),
            0,
            "NonEmpty must NOT re-emit until the client Subtracts \
             (drained reader → no fresh events for the second paint)",
        );
    }

    #[test]
    fn geometry_change_rereports_damage_mid_cycle() {
        let mut state = ServerState::new();
        add_client(&mut state, 1, 0x0010_0000);
        let w_id = add_window(&mut state, 1, 0x0010_0001, ROOT_WINDOW, 0, -28, 2560, 28);
        add_damage_on(&mut state, 1, 0xe000_0001, w_id);

        let _ = accumulate_damage_full_to_state(&mut state, w_id);
        let first = state
            .damage_objects
            .get(&0xe000_0001)
            .and_then(|d| d.last_reported_geometry)
            .expect("first notify should capture geometry");
        assert_eq!(first.y, -28);

        let window = state
            .resources
            .window_mut(w_id)
            .expect("window must still exist");
        window.y = 0;

        let _ = accumulate_damage_full_to_state(&mut state, w_id);
        let second = state
            .damage_objects
            .get(&0xe000_0001)
            .and_then(|d| d.last_reported_geometry)
            .expect("geometry-change notify should refresh geometry");
        assert_eq!(second.y, 0);
    }

    /// Clipping at the ancestor boundary: child overhangs parent's
    /// edge, ancestor damage reports only the visible (clipped)
    /// portion in parent coords.
    #[test]
    fn paint_into_child_clips_to_ancestor_extent() {
        let mut state = ServerState::new();
        add_client(&mut state, 1, 0x0010_0000);
        // Parent W 100×100 at (0, 0). Child C at (-5, -10) sized
        // 200×200 — overhangs every edge of W.
        let w_id = add_window(&mut state, 1, 0x0010_0001, ROOT_WINDOW, 0, 0, 100, 100);
        let c_id = add_window(&mut state, 1, 0x0010_0002, w_id, -5, -10, 200, 200);
        add_damage_on(&mut state, 1, 0xe000_0001, w_id);

        // Paint the full extent of C (relative coords (0, 0, 200, 200)).
        // Translated into W's coords: (-5, -10, 200, 200). Clipped
        // to W (100×100): (0, 0, 95, 90) — left/top edges clip at
        // 0, right/bottom edges clip at 100. Width = 95 (= -5 + 200
        // - clamp-to-0 = 195, then min(195, 100 - 0) = 100? Wait —
        // recompute: x0 = max(-5, 0) = 0, x1 = min(-5+200, 100) =
        // 100; width = 100 - 0 = 100. y0 = max(-10, 0) = 0, y1 =
        // min(-10+200, 100) = 100; height = 100. So the entire
        // parent extent is covered. Asserting the full-cover case
        // first; the partial-cover case below tests the trimmed edge.
        let _ = accumulate_damage_to_state(&mut state, c_id, 0, 0, 200, 200);

        let w_dmg = state
            .damage_objects
            .get(&0xe000_0001)
            .expect("W's damage object");
        assert_eq!(w_dmg.rects.len(), 1);
        let r = w_dmg.rects[0];
        assert_eq!(
            (r.x, r.y, r.width, r.height),
            (0, 0, 100, 100),
            "overhanging child paint clips to parent's full extent",
        );
    }

    /// Partial-clip variant: child paint partly overhangs parent's
    /// right edge — the reported rect is the visible left half.
    #[test]
    fn paint_into_child_clips_partial_overhang() {
        let mut state = ServerState::new();
        add_client(&mut state, 1, 0x0010_0000);
        let w_id = add_window(&mut state, 1, 0x0010_0001, ROOT_WINDOW, 0, 0, 100, 100);
        // Child fully inside parent at (10, 10), 200×30 — extends
        // past parent's right edge (10 + 200 = 210 > 100).
        let c_id = add_window(&mut state, 1, 0x0010_0002, w_id, 10, 10, 200, 30);
        add_damage_on(&mut state, 1, 0xe000_0001, w_id);

        // Paint at (0, 0, 200, 30) inside C — translates to
        // (10, 10, 200, 30) in W's coords, clips to (10, 10, 90, 30)
        // at W's right edge.
        let _ = accumulate_damage_to_state(&mut state, c_id, 0, 0, 200, 30);

        let r = state.damage_objects[&0xe000_0001].rects[0];
        assert_eq!((r.x, r.y, r.width, r.height), (10, 10, 90, 30));
    }

    /// Paint into a child that lies entirely outside its parent
    /// (offscreen): no damage propagates to the parent.
    #[test]
    fn paint_into_offscreen_child_skips_ancestor() {
        let mut state = ServerState::new();
        add_client(&mut state, 1, 0x0010_0000);
        let w_id = add_window(&mut state, 1, 0x0010_0001, ROOT_WINDOW, 0, 0, 100, 100);
        // Child far past parent's right edge.
        let c_id = add_window(&mut state, 1, 0x0010_0002, w_id, 200, 200, 50, 50);
        add_damage_on(&mut state, 1, 0xe000_0001, w_id);

        let _ = accumulate_damage_to_state(&mut state, c_id, 0, 0, 50, 50);

        assert!(
            state.damage_objects[&0xe000_0001].rects.is_empty(),
            "offscreen child paint must not propagate damage to parent",
        );
    }

    /// Walk terminates at root (self-parent sentinel). A damage
    /// subscription on root fires from a deep paint without the
    /// walk running away.
    #[test]
    fn ancestor_walk_terminates_at_root_self_parent() {
        let mut state = ServerState::new();
        add_client(&mut state, 1, 0x0010_0000);
        let w_id = add_window(&mut state, 1, 0x0010_0001, ROOT_WINDOW, 50, 60, 400, 300);
        let c_id = add_window(&mut state, 1, 0x0010_0002, w_id, 20, 30, 100, 80);
        let gc_id = add_window(&mut state, 1, 0x0010_0003, c_id, 5, 10, 40, 30);
        add_damage_on(&mut state, 1, 0xe000_0001, ROOT_WINDOW);

        let _ = accumulate_damage_to_state(&mut state, gc_id, 1, 2, 5, 5);

        // Translation: gc (5, 10) + c (20, 30) + w (50, 60) = (75, 100).
        // Paint origin (1, 2) lands at root (75 + 1, 100 + 2) = (76, 102).
        let r = state.damage_objects[&0xe000_0001].rects[0];
        assert_eq!((r.x, r.y, r.width, r.height), (76, 102, 5, 5));
        // Verify the test didn't hang: the seqno bump on the client
        // is a cheap proxy for "the encoder ran and didn't deadlock."
        let _ = state.clients[&1].last_sequence.load(Ordering::Relaxed);
    }

    /// Pixmap drawables have no place in the window tree, so the
    /// ancestor walk doesn't run for them. A DamageCreate on a
    /// window doesn't fire when an unrelated pixmap is painted.
    #[test]
    fn pixmap_paint_does_not_walk_window_ancestors() {
        let mut state = ServerState::new();
        add_client(&mut state, 1, 0x0010_0000);
        let w_id = add_window(&mut state, 1, 0x0010_0001, ROOT_WINDOW, 0, 0, 100, 100);
        // Pixmap created against root (parent for depth/visual
        // inheritance only — pixmaps don't live in the window tree).
        let pix_id = ResourceId(0x0010_0005);
        state.resources.create_pixmap(
            ClientId(1),
            CreatePixmapRequest {
                depth: 32,
                pixmap: pix_id,
                drawable: ROOT_WINDOW,
                width: 16,
                height: 16,
            },
        );
        add_damage_on(&mut state, 1, 0xe000_0001, w_id);

        let _ = accumulate_damage_to_state(&mut state, pix_id, 0, 0, 16, 16);

        assert!(
            state.damage_objects[&0xe000_0001].rects.is_empty(),
            "pixmap paint must not fan out to window-tree damage objects",
        );
    }

    fn drawable_geometry_full_extent(state: &ServerState, drawable: ResourceId) -> (u16, u16) {
        let r = drawable_full_rect(state, drawable);
        (r.width, r.height)
    }

    #[test]
    fn drawable_full_rect_is_zero_for_unknown_drawable() {
        let state = ServerState::new();
        assert_eq!(
            drawable_geometry_full_extent(&state, ResourceId(0xdead_beef)),
            (0, 0),
        );
    }

    /// Per X11 DAMAGE spec + Xorg `damageext/damageext.c:117-126`:
    /// `DamageReportNonEmpty` (level 3) carries the drawable's FULL
    /// extent in the event `area` field, not the actually-damaged
    /// sub-rect. Compositors use `area` as the recomposite region —
    /// passing the small rect makes them clip the next composite to
    /// it, leaving the rest of the offscreen stale (observed as
    /// shrinking / top-left-only CC content after small client
    /// updates against marco-with-compositing).
    #[test]
    fn non_empty_damage_notify_area_is_full_drawable_extent() {
        use std::io::Read;
        let mut state = ServerState::new();

        // Client with a real UnixStream pair (not the throwaway
        // `make_test_writer` — we need to read the encoded event
        // back). The read end stays alive in `read_end` for the
        // duration of the test so writes don't EPIPE.
        let (writer_end, mut read_end) = UnixStream::pair().expect("UnixStream::pair");
        read_end
            .set_nonblocking(true)
            .expect("set_nonblocking on read end");
        state.clients.insert(
            1,
            ClientState {
                writer: Arc::new(Mutex::new(crate::transport::Transport::Unix(writer_end))),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0)),
                resource_id_base: 0x0010_0000,
                resource_id_mask: 0x000F_FFFF,
                event_masks: HashMap::new(),
                save_set: HashSet::new(),
                big_requests_enabled: false,
                xi2_masks: HashMap::new(),
                xi1_event_classes: HashSet::new(),
                xi1_window_event_classes: HashMap::new(),
                outbound: VecDeque::new(),
                watching_writable: false,
                write_failed: false,
                gone: false,
                focused_window: ROOT_WINDOW,
                reader_control: None,
                is_local: true,
                fd_passing: true,
                internal: false,
            },
        );

        // Window 800×600 at root origin. Damage object level=3 (NonEmpty).
        let w_id = add_window(&mut state, 1, 0x0010_0001, ROOT_WINDOW, 0, 0, 800, 600);
        add_damage_on(&mut state, 1, 0xe000_0001, w_id);

        // Small paint at (50, 100) 20×30 — the "post-first-frame
        // tooltip update" shape that exposed the bug on CC.
        let _ = accumulate_damage_to_state(&mut state, w_id, 50, 100, 20, 30);

        // Read the encoded DamageNotify off the socket. 32 bytes.
        let mut buf = [0u8; 32];
        let n = read_end
            .read(&mut buf)
            .expect("read encoded DamageNotify event");
        assert_eq!(n, 32, "DamageNotify event is exactly 32 bytes on the wire");

        assert_eq!(buf[0], DAMAGE_FIRST_EVENT, "response_type = DamageNotify");
        assert_eq!(
            buf[1],
            x11damage::report_level::NON_EMPTY,
            "level byte must be NonEmpty (3)",
        );

        // Area lives at bytes 16..24 (x:i16, y:i16, w:u16, h:u16),
        // little-endian per the client byte order set above.
        let area_x = i16::from_le_bytes([buf[16], buf[17]]);
        let area_y = i16::from_le_bytes([buf[18], buf[19]]);
        let area_w = u16::from_le_bytes([buf[20], buf[21]]);
        let area_h = u16::from_le_bytes([buf[22], buf[23]]);
        assert_eq!(
            (area_x, area_y, area_w, area_h),
            (0, 0, 800, 600),
            "NonEmpty area must be the drawable's full extent (Xorg \
             damageext.c:117-126 fills {{0,0,w,h}} when pBoxes == NULL); \
             pre-fix this would be (50, 100, 20, 30) — the small paint \
             rect — which is the bug",
        );
    }

    /// Regression gate for the mate-panel systray storm + invisible
    /// tray icons (resolved 2026-06-01). A RENDER FillRectangles Clear
    /// on a window fully covered by a mapped child — the systray socket
    /// under its XEMBED icon — must emit ZERO damage (Xorg's empty
    /// `ClipByChildren` composite clip). Pre-fix this damaged the full
    /// socket → fired the socket's own DamageCreate → per-vsync
    /// recompose loop. If this assertion ever flips to non-zero, the
    /// storm is back.
    #[test]
    fn clip_by_children_fully_covered_socket_emits_no_damage() {
        let mut state = ServerState::new();
        add_client(&mut state, 1, 0x0210_0000);
        // Socket 26×27; icon child 26×27 at (0,0) → full cover.
        let socket = add_window(&mut state, 1, 0x0210_0018, ROOT_WINDOW, 2376, 0, 26, 27);
        let _icon = add_window(&mut state, 1, 0x0210_0007, socket, 0, 0, 26, 27);
        // The applet's own DamageCreate on the socket — the loop's
        // wake source.
        add_damage_on(&mut state, 1, 0x0210_0019, socket);

        let dropped = accumulate_damage_clip_by_children_to_state(&mut state, socket);

        let dmg = state
            .damage_objects
            .get(&0x0210_0019)
            .expect("socket damage object");
        assert!(
            dmg.rects.is_empty(),
            "fully-covered socket must emit no damage (got {} rect(s)) \
             — the systray storm regression gate",
            dmg.rects.len(),
        );
        assert!(
            !dmg.pending_notify_fired,
            "no DamageNotify may fire for a fully-covered socket",
        );
        assert!(
            dropped.is_empty(),
            "no clients should be targeted when the clip is empty",
        );
    }

    /// Contrast: a partially-covered window still damages the exposed
    /// margin. Icon 16 wide inside a 26-wide socket leaves a 10×27
    /// strip; that strip is the only damage, not the full drawable.
    #[test]
    fn clip_by_children_partial_cover_damages_only_margin() {
        let mut state = ServerState::new();
        add_client(&mut state, 1, 0x0210_0000);
        let socket = add_window(&mut state, 1, 0x0210_0018, ROOT_WINDOW, 0, 0, 26, 27);
        let _icon = add_window(&mut state, 1, 0x0210_0007, socket, 0, 0, 16, 27);
        add_damage_on(&mut state, 1, 0x0210_0019, socket);

        let _ = accumulate_damage_clip_by_children_to_state(&mut state, socket);

        let dmg = state
            .damage_objects
            .get(&0x0210_0019)
            .expect("socket damage object");
        assert_eq!(dmg.rects.len(), 1, "exactly the uncovered margin");
        let r = dmg.rects[0];
        assert_eq!(
            (r.x, r.y, r.width, r.height),
            (16, 0, 10, 27),
            "damage must be the 10×27 strip the icon does not cover",
        );
    }

    /// `InputOnly` and unmapped children never contribute to the
    /// ClipByChildren region (matching Xorg's clipList). A socket
    /// "covered" only by such children still damages in full.
    /// A bounding-shaped child covers its parent with its shape only:
    /// a RENDER paint on the parent under the child's rect but outside
    /// its shape (xfce4-settings-manager's button bar below its shaped
    /// socket) is damage the compositor must hear about.
    #[test]
    fn mapped_child_clip_rects_use_a_childs_bounding_shape() {
        let mut state = ServerState::new();
        add_client(&mut state, 1, 0x0210_0000);
        let parent = add_window(&mut state, 1, 0x0210_0030, ROOT_WINDOW, 0, 0, 746, 500);
        let socket = add_window(&mut state, 1, 0x0210_0031, parent, 8, 8, 730, 531);
        let _ = state.resources.map_window(socket);
        crate::nested::set_shape_rects(
            &mut state,
            socket,
            yserver_protocol::x11::shape::KIND_BOUNDING,
            vec![xfixes::RegionRect {
                x: 0,
                y: 0,
                width: 730,
                height: 450,
            }],
        );
        assert_eq!(
            mapped_child_clip_rects(&state, parent),
            vec![xfixes::RegionRect {
                x: 8,
                y: 8,
                width: 730,
                height: 450,
            }]
        );
    }

    #[test]
    fn mapped_child_clip_rects_skips_inputonly_and_unmapped() {
        let mut state = ServerState::new();
        add_client(&mut state, 1, 0x0210_0000);
        let socket = add_window(&mut state, 1, 0x0210_0018, ROOT_WINDOW, 0, 0, 26, 27);

        // InputOnly child covering the socket — must NOT clip.
        let input_only = ResourceId(0x0210_0007);
        state.resources.create_window(
            ClientId(1),
            CreateWindowRequest {
                depth: 0,
                window: input_only,
                parent: socket,
                x: 0,
                y: 0,
                width: 26,
                height: 27,
                border_width: 0,
                class: 2, // InputOnly
                visual: crate::resources::ROOT_VISUAL,
                ..Default::default()
            },
        );
        let _ = state.resources.map_window(input_only);

        // Mapped-then-unmapped InputOutput child — must NOT clip.
        let gone = add_window(&mut state, 1, 0x0210_0008, socket, 0, 0, 26, 27);
        let _ = state.resources.unmap_window(gone);

        assert!(
            mapped_child_clip_rects(&state, socket).is_empty(),
            "neither InputOnly nor unmapped children may clip",
        );

        add_damage_on(&mut state, 1, 0x0210_0019, socket);
        let _ = accumulate_damage_clip_by_children_to_state(&mut state, socket);
        let dmg = state
            .damage_objects
            .get(&0x0210_0019)
            .expect("socket damage object");
        assert_eq!(
            dmg.rects.len(),
            1,
            "socket with no clipping children damages in full",
        );
        let r = dmg.rects[0];
        assert_eq!((r.x, r.y, r.width, r.height), (0, 0, 26, 27));
    }

    /// Issue-2 (2026-07-01): accumulating damage on a redirected window
    /// MUST fire DAMAGE objects subscribed on the window's
    /// `NameWindowPixmap` alias pixmap, not only those subscribed on
    /// the window itself. picom binds its GLX texture to the named
    /// pixmap and subscribes `XDamageCreate` on that pixmap xid; if
    /// `accumulate_at_level` matches only `dmg.drawable == window_xid`
    /// the compositor never receives `DamageNotify` for a ConfigureWindow
    /// move (or for repaints of descendants routed through the
    /// redirect), so the composited frame visually freezes at its
    /// pre-move position. Xorg resolves this indirection automatically
    /// via `getDrawableDamageRef(&pWin->drawable)` → `GetWindowPixmap`
    /// → the backing pixmap's shared damage list
    /// (miext/damage/damage.c:90-110); yserver's per-xid damage map
    /// has no such indirection, so it is mirrored inline here.
    #[test]
    fn accumulate_on_window_fires_damage_subscribed_on_named_pixmap_alias() {
        const CLIENT_ID: u32 = 1;
        const WINDOW_XID: u32 = 0x0010_0014;
        const ALIAS_PIXMAP_XID: u32 = 0x0020_0014;
        const DAMAGE_ID: u32 = 0x0080_0014;

        let mut state = ServerState::new();
        add_client(&mut state, CLIENT_ID, 0);
        let window = add_window(
            &mut state,
            CLIENT_ID,
            WINDOW_XID,
            ROOT_WINDOW,
            10,
            20,
            100,
            50,
        );

        // Install the NameWindowPixmap alias on the window. The host
        // pixmap handle is irrelevant to the damage-fanout path; only
        // the alias xid matters.
        {
            let w = state
                .resources
                .window_mut(window)
                .expect("window installed");
            w.composite_named_pixmaps
                .push(crate::resources::NamedCompositePixmap {
                    client_pixmap: ResourceId(ALIAS_PIXMAP_XID),
                    host_pixmap: crate::backend::PixmapHandle::from_raw_for_test(0xDEAD_BEEF),
                    width: 100,
                    height: 50,
                });
        }

        // picom creates the DAMAGE subscription on the alias pixmap,
        // NOT on the window.
        add_damage_on(
            &mut state,
            CLIENT_ID,
            DAMAGE_ID,
            ResourceId(ALIAS_PIXMAP_XID),
        );

        // ConfigureWindow on the redirected window fires
        // `accumulate_damage_full_to_state(state, window)` (the
        // steady-state move-damage path). Pre-fix this matched zero
        // damage objects (picom subscribed on the alias, not the
        // window), so the compositor never received a DamageNotify.
        let _ = accumulate_damage_full_to_state(&mut state, window);

        let dmg = state
            .damage_objects
            .get(&DAMAGE_ID)
            .expect("alias-pixmap damage object");
        assert!(
            !dmg.rects.is_empty(),
            "accumulating damage on a redirected window MUST also fire \
             DAMAGE objects subscribed on its NameWindowPixmap alias \
             pixmap (picom's subscription model)",
        );
        // The damage rect is in the window's coordinate space (the
        // alias pixmap has the same dimensions as the window's
        // backing), so the full-window rect is (0,0)–(100,50).
        let r = dmg.rects[0];
        assert_eq!((r.x, r.y, r.width, r.height), (0, 0, 100, 50));
    }
}
