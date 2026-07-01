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
        .map(|w| xfixes::RegionRect {
            x: w.x,
            y: w.y,
            width: w.width,
            height: w.height,
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

    let mut dropped: Vec<ClientId> = Vec::new();
    for rect in visible {
        for c in
            accumulate_damage_to_state(state, drawable, rect.x, rect.y, rect.width, rect.height)
        {
            if !dropped.contains(&c) {
                dropped.push(c);
            }
        }
    }
    dropped
}

pub fn accumulate_damage_to_state(
    state: &mut ServerState,
    drawable: ResourceId,
    x: i16,
    y: i16,
    width: u16,
    height: u16,
) -> Vec<ClientId> {
    if width == 0 || height == 0 {
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
            "DIAG accumulate_damage: drawable=0x{:x} rect=({},{} {}x{})\n{}",
            drawable.0,
            x,
            y,
            width,
            height,
            bt,
        );
    }
    let timestamp = state.timestamp_now();
    let mut pending: Vec<PendingNotify> = Vec::new();

    // Initial rect in the leaf drawable's coordinate space. Carry as
    // i32 through the ancestor walk so translation + clip don't
    // overflow when a child sits well past its parent's edge;
    // saturate-cast back to i16/u16 only at emit time.
    let mut rx = i32::from(x);
    let mut ry = i32::from(y);
    let mut rw = i32::from(width);
    let mut rh = i32::from(height);

    // Process the leaf level — pixmaps and windows alike. Pixmaps
    // have no parent so processing stops here; windows fall through
    // to the ancestor walk below.
    accumulate_at_level(state, drawable, rx, ry, rw, rh, &mut pending);

    // Ancestor walk — only meaningful for window drawables. Pixmaps
    // exist outside the window tree per X11 spec (Composite spec's
    // NameWindowPixmap aliases sit beside the window tree, not in
    // it), so a `state.resources.window(drawable)` miss at the top
    // of the loop terminates the walk before the first step.
    let mut current = drawable;
    for _ in 0..ANCESTOR_WALK_MAX_DEPTH {
        // Look up the current window's own offset + parent. A
        // missing window is the loop termination for pixmaps and
        // for any path that strays off the tree.
        let Some(current_win) = state.resources.window(current) else {
            break;
        };
        let cur_off_x = i32::from(current_win.x);
        let cur_off_y = i32::from(current_win.y);
        let parent = current_win.parent;
        // Root self-parents (`parent == self`, see resources.rs
        // ROOT_WINDOW initialiser). Other walks in the tree use
        // the same sentinel for termination — propagating beyond
        // root makes no sense.
        if parent == current {
            break;
        }
        // Translate the rect from `current`'s coord space into
        // `parent`'s by adding `current`'s own (x, y).
        rx += cur_off_x;
        ry += cur_off_y;
        // Clip the translated rect to `parent`'s extent. If the
        // rect doesn't intersect this parent at all, it certainly
        // won't intersect any higher ancestor either — terminate.
        let Some(parent_win) = state.resources.window(parent) else {
            // Dangling parent xid (resources tree should keep this
            // consistent; stay defensive — stop walking).
            break;
        };
        let parent_w = i32::from(parent_win.width);
        let parent_h = i32::from(parent_win.height);
        let x0 = rx.max(0);
        let y0 = ry.max(0);
        let x1 = (rx + rw).min(parent_w);
        let y1 = (ry + rh).min(parent_h);
        if x1 <= x0 || y1 <= y0 {
            break;
        }
        // Re-anchor the rect to its visible portion within
        // `parent`. The clipped rect (in parent's coord space) is
        // what higher ancestors will see, exactly mirroring the
        // X11 spec rule "damage propagates the visible region
        // through ancestors."
        rx = x0;
        ry = y0;
        rw = x1 - x0;
        rh = y1 - y0;
        current = parent;
        accumulate_at_level(state, parent, rx, ry, rw, rh, &mut pending);
    }

    if pending.is_empty() {
        return Vec::new();
    }

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

/// Per-level helper: match damage objects keyed on `level_drawable`,
/// push the (clipped, i32 → i16/u16-saturated) rect into each, and
/// add a fire-once `PendingNotify` for any object that hasn't fired
/// in this cycle yet. Called once per level by the ancestor walk
/// (leaf + each ancestor up to root).
///
/// `rx/ry/rw/rh` are in `level_drawable`'s coordinate space and
/// already clipped to its extent by the caller. They arrive as i32
/// for safe walk-time arithmetic; the saturate-cast here is the
/// single conversion point.
fn accumulate_at_level(
    state: &mut ServerState,
    level_drawable: ResourceId,
    rx: i32,
    ry: i32,
    rw: i32,
    rh: i32,
    pending: &mut Vec<PendingNotify>,
) {
    // Resolve `level_drawable` through the NameWindowPixmap alias
    // indirection. Compositors such as picom subscribe DAMAGE on the
    // pixmap returned by `NameWindowPixmap`, not on the window itself.
    // When damage is accumulated for a redirected window (either a
    // ConfigureWindow-move emits `accumulate_damage_full(state, W)` or
    // a descendant paint walks up to W as a redirected ancestor), the
    // damage objects subscribed on W's `composite_named_pixmaps[*]`
    // alias pixmaps MUST also fire here — otherwise the compositor
    // never receives `DamageNotify` for redirected-window content
    // (move: stale composite; child repaint: blank in the composited
    // frame).
    //
    // Xorg's `getDrawableDamageRef(&pWin->drawable)` resolves this
    // automatically via `GetWindowPixmap(pWin)` → backing pixmap →
    // that pixmap's damage list (miext/damage/damage.c:90-110). The
    // alias pixmap and the window share the backing pixmap's damage
    // list, so a single `DamageDamageRegion(&pWin->drawable, ...)` hits
    // every alias subscription. yserver's per-xid `damage_objects` map
    // has no such indirection, so we mirror it inline here.
    let mut drawables = vec![level_drawable];
    if let Some(window) = state.resources.window(level_drawable)
        && !window.composite_named_pixmaps.is_empty()
    {
        for alias in &window.composite_named_pixmaps {
            drawables.push(alias.client_pixmap);
        }
    }
    let damage_ids: Vec<u32> = state
        .damage_objects
        .iter()
        .filter(|(_, dmg)| drawables.contains(&dmg.drawable))
        .map(|(id, _)| *id)
        .collect();
    // Stage 4d shadow-hunt diagnostic: surface every accumulate call
    // with its target drawable + how many DAMAGE subscriptions matched
    // + how many of those have `pending_notify_fired=true` (i.e. would
    // skip the notify). Grep this in the broken-state run to find:
    // - level_drawable=0x<W> match_ids=0 (compositor never subscribed
    //   to W, or subscribed to a different xid — hypothesis 2);
    // - level_drawable=0x<W> match_ids>0 fired_count=match_ids (notify
    //   would fire but compositor isn't subtracting fast enough);
    // - "no entry for W" (paint handler isn't calling accumulate at
    //   all for that path — hypothesis 1).
    // Logged unconditionally at trace; enable with
    // `yserver_core::core_loop::damage_fanout=trace` in `RUST_LOG`.
    let fired_count = damage_ids
        .iter()
        .filter(|id| {
            state
                .damage_objects
                .get(id)
                .is_some_and(|d| d.pending_notify_fired)
        })
        .count();
    log::trace!(
        "damage_fanout: level_drawable=0x{:x} rect=({},{} {}x{}) match_ids={} fired_count={}",
        level_drawable.0,
        rx,
        ry,
        rw,
        rh,
        damage_ids.len(),
        fired_count,
    );

    if damage_ids.is_empty() {
        return;
    }

    // Saturate-cast i32 walk coords to the wire i16/u16 shape.
    // Real window dimensions stay well under i16::MAX (32767); the
    // saturate is defensive against runaway parent chains or
    // corrupt tree state.
    let rect_i16 = xfixes::RegionRect {
        x: i16::try_from(rx).unwrap_or(i16::MAX),
        y: i16::try_from(ry).unwrap_or(i16::MAX),
        width: u16::try_from(rw).unwrap_or(u16::MAX),
        height: u16::try_from(rh).unwrap_or(u16::MAX),
    };
    let area = x11damage::Rectangle {
        x: rect_i16.x,
        y: rect_i16.y,
        width: rect_i16.width,
        height: rect_i16.height,
    };

    // Per-level geometry — the damaged drawable's CURRENT geometry
    // in its parent's coordinate space (X11 DAMAGE proto + Xorg
    // damageext.c: fills from pDrawable->{x, y, width, height};
    // window drawables carry root-relative x/y, pixmaps are
    // always (0, 0)). marco/picom/etc. use this geometry to map
    // the damage rect back into screen space — if x/y stay
    // hardcoded at (0, 0), the compositor recomposites at the
    // wrong screen position after every move, observable as
    // "top-left bits of CC stay rendered" after dragging.
    let geom_full = drawable_full_rect(state, level_drawable);
    let (geom_x, geom_y) = if state.resources.window(level_drawable).is_some() {
        let (ax, ay) = state.resources.window_absolute_position(level_drawable);
        (
            i16::try_from(ax).unwrap_or(i16::MAX),
            i16::try_from(ay).unwrap_or(i16::MAX),
        )
    } else {
        (0, 0)
    };
    let geom_rect = x11damage::Rectangle {
        x: geom_x,
        y: geom_y,
        width: geom_full.width,
        height: geom_full.height,
    };

    for damage_id in damage_ids {
        let (level, fired, owner, last_reported_geometry) = {
            let dmg = state
                .damage_objects
                .get(&damage_id)
                .expect("just enumerated");
            (
                dmg.level,
                dmg.pending_notify_fired,
                dmg.owner,
                dmg.last_reported_geometry,
            )
        };
        if let Some(d) = state.damage_objects.get_mut(&damage_id) {
            d.rects.push(rect_i16);
        }
        let geometry_changed = last_reported_geometry.is_some_and(|prev| prev != geom_rect);
        // Per X11 DAMAGE proto + Xorg `damageext/damageext.c:136-153`
        // (`DamageExtReport`): Raw / Delta / BoundingBox levels emit
        // a `DamageNotify` on EVERY damage event; only NonEmpty has
        // the one-shot-per-Subtract semantic (emit on empty→non-empty
        // transition, silent until the client Subtracts). The
        // `geometry_changed` re-report fallback survives for NonEmpty
        // because the spec requires the geometry payload to be
        // current.
        let level_allows_emit = match level {
            x11damage::report_level::RAW_RECTANGLES
            | x11damage::report_level::DELTA_RECTANGLES
            | x11damage::report_level::BOUNDING_BOX => true,
            x11damage::report_level::NON_EMPTY => !fired || geometry_changed,
            _ => false,
        };
        if state.clients.contains_key(&owner.0) && level_allows_emit {
            // Per X11 DAMAGE spec + Xorg `damageext/damageext.c:117-126`
            // (`DamageExtNotify` with `pBoxes == NULL`): NonEmpty events
            // carry the full drawable extent in `area`, not the actual
            // damaged sub-rect. Compositors (marco, picom, …) treat
            // `area` as the region to recomposite — passing the small
            // rect makes them clip the next composite to it, leaving
            // the rest of the offscreen stale (observed as shrinking /
            // top-left-only CC content after small client updates on
            // marco-with-compositing). Raw / Delta / BoundingBox keep
            // the current rect.
            let area_for_level = if level == x11damage::report_level::NON_EMPTY {
                x11damage::Rectangle {
                    x: 0,
                    y: 0,
                    width: geom_full.width,
                    height: geom_full.height,
                }
            } else {
                area
            };
            pending.push(PendingNotify {
                owner,
                damage_id,
                level,
                drawable: level_drawable.0,
                geometry: geom_rect,
                area: area_for_level,
            });
            if let Some(d) = state.damage_objects.get(&damage_id) {
                log::trace!(
                    "damage_notify_queue: damage=0x{:x} owner={} drawable=0x{:x} level={} \
                     area=({},{} {}x{}) geom=({},{} {}x{}) rects_n={} rects={}",
                    damage_id,
                    owner.0,
                    level_drawable.0,
                    level,
                    area_for_level.x,
                    area_for_level.y,
                    area_for_level.width,
                    area_for_level.height,
                    geom_rect.x,
                    geom_rect.y,
                    geom_rect.width,
                    geom_rect.height,
                    d.rects.len(),
                    format_damage_rects(&d.rects),
                );
            }
            if let Some(d) = state.damage_objects.get_mut(&damage_id) {
                d.pending_notify_fired = true;
                d.last_reported_geometry = Some(geom_rect);
            }
        } else {
            let owner_alive = state.clients.contains_key(&owner.0);
            if let Some(d) = state.damage_objects.get(&damage_id) {
                log::trace!(
                    "damage_notify_skip: damage=0x{:x} owner={} drawable=0x{:x} level={} \
                     fired={} owner_alive={} geometry_changed={} rects_n={} rects={}",
                    damage_id,
                    owner.0,
                    level_drawable.0,
                    level,
                    fired,
                    owner_alive,
                    geometry_changed,
                    d.rects.len(),
                    format_damage_rects(&d.rects),
                );
            }
        }
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

    fn make_test_writer() -> Arc<Mutex<UnixStream>> {
        // Pair of sockets; we keep the read end alive in the same
        // Arc so writes don't EPIPE during tests. We never inspect
        // the data sent — assertions are against `damage_objects`.
        let (a, _b) = UnixStream::pair().expect("UnixStream::pair");
        Arc::new(Mutex::new(a))
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
                focused_window: ROOT_WINDOW,
                reader_control: None,
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
                writer: Arc::new(Mutex::new(a)),
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
                focused_window: ROOT_WINDOW,
                reader_control: None,
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
        assert_eq!(
            count_damage_notify_events(&mut reader, &state, 1),
            1,
            "first paint must emit a DamageNotify on BoundingBox level",
        );

        let _ = accumulate_damage_to_state(&mut state, w_id, 50, 50, 30, 40);
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
                writer: Arc::new(Mutex::new(writer_end)),
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
                focused_window: ROOT_WINDOW,
                reader_control: None,
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
