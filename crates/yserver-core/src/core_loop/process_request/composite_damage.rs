use super::*;

#[allow(clippy::too_many_arguments)]
pub(super) fn handle_composite_request(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    use yserver_protocol::x11::{ClientByteOrder, composite as x11composite};
    const COMPOSITE_MAJOR_OPCODE: u8 = 144;
    let byte_order = state
        .clients
        .get(&client_id.0)
        .map_or(ClientByteOrder::LittleEndian, |c| c.byte_order);
    let minor = header.data;
    match minor {
        x11composite::QUERY_VERSION => {
            let _ = x11composite::parse_query_version(body);
            let major = x11composite::MAJOR_VERSION;
            let minor_ver = x11composite::MINOR_VERSION;
            debug!(
                "client {} #{} COMPOSITE::QueryVersion -> {}.{}",
                client_id.0, sequence.0, major, minor_ver
            );
            let reply =
                x11composite::encode_query_version_reply(byte_order, sequence, major, minor_ver);
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            let _byte_order = client.byte_order;
            return Ok(write_to_client(client, client_id, &reply));
        }
        x11composite::REDIRECT_WINDOW
        | x11composite::REDIRECT_SUBWINDOWS
        | x11composite::UNREDIRECT_WINDOW
        | x11composite::UNREDIRECT_SUBWINDOWS => {
            let Some((window_raw, update)) = x11composite::parse_window_update(body) else {
                return Ok(RequestOutcome::Handled);
            };
            let window = ResourceId(window_raw);
            let subwindows = matches!(
                minor,
                x11composite::REDIRECT_SUBWINDOWS | x11composite::UNREDIRECT_SUBWINDOWS
            );
            let redirect = matches!(
                minor,
                x11composite::REDIRECT_WINDOW | x11composite::REDIRECT_SUBWINDOWS
            );
            let error = |state: &mut ServerState, code: u8| {
                emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    code,
                    window_raw,
                    u16::from(minor),
                    COMPOSITE_MAJOR_OPCODE,
                )
            };
            let Some(parent) = state.resources.window(window).map(|w| w.parent) else {
                return error(state, x11::error::BAD_WINDOW);
            };
            // compositeproto: update=0 → Automatic, update=1 → Manual. An
            // unredirect with any other value matches no record (BadValue
            // below, as Xorg).
            let mode = match update {
                0 => crate::server::CompositeRedirectMode::Automatic,
                1 => crate::server::CompositeRedirectMode::Manual,
                _ => return error(state, x11::error::BAD_VALUE),
            };
            // Xorg compRedirectWindow (composite/compalloc.c:145-150): the
            // overlay window is never redirected and asking succeeds; the
            // root has no parent to redirect into (BadMatch). Unredirect has
            // no such checks: neither ever holds a record, so it BadValues.
            if redirect && !subwindows {
                if window == COMPOSITE_OVERLAY_WINDOW {
                    return Ok(RequestOutcome::Handled);
                }
                if parent == window {
                    return error(state, x11::error::BAD_MATCH);
                }
            }
            // Xorg's order is lastChild first, ours bottom first. The COW
            // gets no record from a subwindows redirect (compRedirectWindow
            // returns Success for it).
            let targets: Vec<ResourceId> = if subwindows {
                state
                    .resources
                    .children(window)
                    .iter()
                    .rev()
                    .copied()
                    .filter(|child| *child != COMPOSITE_OVERLAY_WINDOW)
                    .collect()
            } else {
                vec![window]
            };
            let before: Vec<_> = targets
                .iter()
                .map(|t| state.composite_redirects.window_mode(*t))
                .collect();
            let record = crate::server::RedirectRecord {
                mode,
                owner: client_id,
            };
            let redirects = &mut state.composite_redirects;
            let ok = match (redirect, subwindows) {
                // Only one Manual redirect per window or subwindows list,
                // whichever client holds it (compalloc.c:155-158, :336-339):
                // muffin probes RedirectWindow(frame, Manual) after its own
                // RedirectSubwindows(root, Manual) and expects BadAccess.
                (true, false) => redirects.redirect_window(window, record).is_ok(),
                (true, true) => redirects
                    .redirect_subwindows(window, &targets, record)
                    .is_ok(),
                (false, false) => redirects.unredirect_window(window, client_id, mode),
                (false, true) => redirects.unredirect_subwindows(window, &targets, client_id, mode),
            };
            debug!(
                "client {} #{} COMPOSITE::{}Redirect{}(0x{:x}, mode={:?}) -> {} targets={}",
                client_id.0,
                sequence.0,
                if redirect { "" } else { "Un" },
                if subwindows { "Subwindows" } else { "Window" },
                window_raw,
                mode,
                if ok { "ok" } else { "refused" },
                targets.len(),
            );
            if !ok {
                return error(
                    state,
                    if redirect {
                        x11::error::BAD_ACCESS
                    } else {
                        x11::error::BAD_VALUE
                    },
                );
            }
            for (target, before) in targets.into_iter().zip(before) {
                sync_redirect_backing(state, backend, origin, target, before);
            }
        }
        x11composite::CREATE_REGION_FROM_BORDER_CLIP => {
            if let Some((region, window)) = x11composite::parse_u32_pair(body) {
                let rects = vec![drawable_full_rect_xfixes(state, ResourceId(window))];
                state.xfixes_regions.insert(
                    region,
                    crate::server::XFixesRegion {
                        owner: client_id,
                        rects: normalize_region_rects(rects),
                    },
                );
            }
        }
        x11composite::NAME_WINDOW_PIXMAP => {
            let Some((window_raw, pixmap_raw)) = x11composite::parse_u32_pair(body) else {
                return Ok(RequestOutcome::Handled);
            };
            let window = ResourceId(window_raw);
            let pixmap = ResourceId(pixmap_raw);
            debug!(
                "client {} #{} COMPOSITE::NameWindowPixmap(window=0x{:x}, pixmap=0x{:x})",
                client_id.0, sequence.0, window_raw, pixmap_raw,
            );
            let snapshot = state.resources.window(window).map(|w| {
                // Xorg compext.c:246 `if (!cw) return BadMatch;`.
                let redirected = state.composite_redirects.window_mode(window).is_some();
                let (pixmap_width, pixmap_height) = w
                    .redirected_backing
                    .as_ref()
                    .map_or((w.width, w.height), |backing| {
                        (backing.width, backing.height)
                    });
                (
                    w.host_xid,
                    pixmap_width,
                    pixmap_height,
                    w.depth,
                    redirected,
                    w.map_state,
                )
            });
            let Some((host_xid, w_width, w_height, w_depth, redirected, map_state)) = snapshot
            else {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_WINDOW,
                    window_raw,
                    u16::from(minor),
                    COMPOSITE_MAJOR_OPCODE,
                );
            };
            if !redirected {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_MATCH,
                    window_raw,
                    u16::from(minor),
                    COMPOSITE_MAJOR_OPCODE,
                );
            }
            // Xorg's `ProcCompositeNameWindowPixmap` refuses a non-viewable
            // window (composite/compext.c:241 `if (!pWin->viewable) return
            // BadMatch;`) — a non-viewable window has no off-screen backing to
            // name. We honoured it instead, so in issue #97 fastcompmgr got a
            // valid pixmap for i3's unmapped frame and kept compositing a
            // window that had left the workspace.
            if map_state != crate::resources::MapState::Viewable {
                debug!(
                    "client {} #{} COMPOSITE::NameWindowPixmap(window=0x{:x}) -> BadMatch \
                     (map_state={map_state:?}, Xorg requires Viewable)",
                    client_id.0, sequence.0, window_raw,
                );
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_MATCH,
                    window_raw,
                    u16::from(minor),
                    COMPOSITE_MAJOR_OPCODE,
                );
            }
            let Some(host_window_xid) = host_xid else {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_ALLOC,
                    pixmap_raw,
                    u16::from(minor),
                    COMPOSITE_MAJOR_OPCODE,
                );
            };
            if backend.composite_opcode().is_none() {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_ALLOC,
                    pixmap_raw,
                    u16::from(minor),
                    COMPOSITE_MAJOR_OPCODE,
                );
            }
            let host_pixmap_xid = match backend.name_window_pixmap(origin, host_window_xid) {
                Ok(handle) => handle,
                Err(err) => {
                    debug!(
                        "client {} #{} COMPOSITE::NameWindowPixmap(window=0x{:x}) -> Err: {err}",
                        client_id.0, sequence.0, window_raw,
                    );
                    return emit_x11_error_with_minor(
                        state,
                        client_id,
                        sequence,
                        x11::error::BAD_ALLOC,
                        pixmap_raw,
                        u16::from(minor),
                        COMPOSITE_MAJOR_OPCODE,
                    );
                }
            };
            state.resources.create_pixmap(
                client_id,
                x11::CreatePixmapRequest {
                    pixmap,
                    drawable: window,
                    width: w_width,
                    height: w_height,
                    depth: w_depth,
                },
            );
            let _ = state.resources.set_pixmap_host_xid(pixmap, host_pixmap_xid);
            state.resources.mark_pixmap_composite_name(pixmap);
            if let Some(w) = state.resources.window_mut(window) {
                w.composite_named_pixmaps
                    .push(crate::resources::NamedCompositePixmap {
                        client_pixmap: pixmap,
                        host_pixmap: host_pixmap_xid,
                        width: w_width,
                        height: w_height,
                    });
            }
        }
        x11composite::GET_OVERLAY_WINDOW => {
            // Checked first, before anything else: a previous session's
            // overlay could not be torn down and is still materialized
            // with nobody owning it. Whatever we would hand this
            // compositor is inherited state, so refuse. Sticky for the
            // life of the process.
            if state.cow_teardown_failed {
                log::warn!(
                    "client {} #{} COMPOSITE::GetOverlayWindow refused: the \
                     overlay is orphaned by a failed teardown",
                    client_id.0,
                    sequence.0,
                );
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_ALLOC,
                    0,
                    u16::from(minor),
                    COMPOSITE_MAJOR_OPCODE,
                );
            }
            let _window = x11composite::parse_window(body).unwrap_or(ROOT_WINDOW.0);
            // Must return a distinct XID, not root. See COMPOSITE_OVERLAY_WINDOW
            // for why — marco's compositor immediately calls XSelectInput on
            // this XID and would otherwise wipe its own WM event mask on root.
            let overlay = COMPOSITE_OVERLAY_WINDOW.0;
            // Core owns the claim list; the backend counts nothing. Each
            // Get records one claim owned by the calling client (Xorg's
            // per-Get `CompOverlayClientRec`), and only the 0 → 1 edge
            // reaches the backend.
            //
            // Transactional: record the claim, materialize, and roll the
            // claim back if materialization fails. A claim recorded
            // against an overlay that does not exist is the
            // desynchronisation this design exists to remove.
            let first_claim = state.cow_claims.is_empty();
            state.cow_claims.push(client_id);
            if first_claim
                && let Err(err) =
                    crate::core_loop::composite_overlay::materialize_overlay(state, backend, origin)
            {
                state.cow_claims.pop();
                log::warn!(
                    "client {} #{} COMPOSITE::GetOverlayWindow materialization failed: {err}",
                    client_id.0,
                    sequence.0,
                );
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_ALLOC,
                    0,
                    u16::from(minor),
                    COMPOSITE_MAJOR_OPCODE,
                );
            }
            debug!(
                "client {} #{} COMPOSITE::GetOverlayWindow -> 0x{:x}",
                client_id.0, sequence.0, overlay
            );
            let reply =
                x11composite::encode_get_overlay_window_reply(byte_order, sequence, overlay);
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            let _byte_order = client.byte_order;
            return Ok(write_to_client(client, client_id, &reply));
        }
        x11composite::RELEASE_OVERLAY_WINDOW => {
            debug!(
                "client {} #{} COMPOSITE::ReleaseOverlayWindow",
                client_id.0, sequence.0
            );
            // Ownership check, as Xorg's `ProcCompositeReleaseOverlayWindow`
            // does via `compFindOverlayClient`: a client holding no claim
            // gets BadMatch and nothing changes. Get and Release pair 1:1,
            // so this drops exactly ONE of the caller's claims — never all
            // of them, and never somebody else's.
            let Some(claim_index) = state
                .cow_claims
                .iter()
                .rposition(|owner| *owner == client_id)
            else {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_MATCH,
                    0,
                    u16::from(minor),
                    COMPOSITE_MAJOR_OPCODE,
                );
            };
            if state.cow_claims.len() == 1 {
                // Final release: tear down FIRST and keep the claim unless
                // it succeeds. The claim is what keeps the overlay alive,
                // so dropping it before the overlay is gone is precisely
                // the leak.
                if let Err(err) =
                    crate::core_loop::composite_overlay::teardown_overlay(state, backend, origin)
                {
                    log::warn!(
                        "client {} #{} COMPOSITE::ReleaseOverlayWindow teardown failed: {err}",
                        client_id.0,
                        sequence.0,
                    );
                    return emit_x11_error_with_minor(
                        state,
                        client_id,
                        sequence,
                        x11::error::BAD_ALLOC,
                        0,
                        u16::from(minor),
                        COMPOSITE_MAJOR_OPCODE,
                    );
                }
            }
            state.cow_claims.remove(claim_index);
        }
        other => {
            return emit_x11_error_with_minor(
                state,
                client_id,
                sequence,
                x11::error::BAD_REQUEST,
                0,
                u16::from(other),
                COMPOSITE_MAJOR_OPCODE,
            );
        }
    }
    Ok(RequestOutcome::Handled)
}

pub(super) fn handle_damage_request(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    use yserver_protocol::x11::damage as x11damage;
    let minor = header.data;
    match minor {
        x11damage::QUERY_VERSION => {
            let (client_major, client_minor) = x11damage::parse_query_version(body)
                .unwrap_or((x11damage::MAJOR_VERSION, x11damage::MINOR_VERSION));
            let major = x11damage::MAJOR_VERSION.min(client_major);
            let minor_ver = if major < x11damage::MAJOR_VERSION {
                client_minor
            } else {
                x11damage::MINOR_VERSION.min(client_minor)
            };
            debug!(
                "client {} #{} DAMAGE::QueryVersion -> {}.{}",
                client_id.0, sequence.0, major, minor_ver
            );
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            let byte_order = client.byte_order;
            let reply =
                x11damage::encode_query_version_reply(byte_order, sequence, major, minor_ver);
            return Ok(write_to_client(client, client_id, &reply));
        }
        x11damage::CREATE => {
            if let Some((damage, drawable, level)) = x11damage::parse_create(body) {
                let drawable_id = ResourceId(drawable);
                state.damage_objects.insert(
                    damage,
                    crate::server::DamageObject {
                        owner: client_id,
                        drawable: drawable_id,
                        level,
                        rects: Vec::new(),
                        pending_notify_fired: false,
                        last_reported_geometry: None,
                    },
                );
                debug!(
                    "client {} #{} DAMAGE::Create damage=0x{damage:x} drawable=0x{drawable:x} level={level}",
                    client_id.0, sequence.0,
                );
                let seed_initial_damage = state
                    .resources
                    .window(drawable_id)
                    .is_some_and(|w| w.map_state == crate::resources::MapState::Viewable)
                    || state
                        .resources
                        .composite_named_pixmap_owner_window(drawable_id)
                        .and_then(|window_id| state.resources.window(window_id))
                        .is_some_and(|w| w.map_state == crate::resources::MapState::Viewable);
                if seed_initial_damage {
                    log::trace!(
                        target: "yserver_core::core_loop::damage_fanout",
                        "damage_create_seed_full: damage=0x{:x} drawable=0x{:x} level={}",
                        damage,
                        drawable,
                        level,
                    );
                    let _dropped = accumulate_damage_full_to_state(state, drawable_id);
                }
            } else {
                debug!(
                    "client {} #{} DAMAGE::Create parse_failed",
                    client_id.0, sequence.0
                );
            }
        }
        x11damage::DESTROY => {
            if let Some(damage) = x11damage::parse_resource(body) {
                let drawable = state.damage_objects.remove(&damage).map(|d| d.drawable.0);
                debug!(
                    "client {} #{} DAMAGE::Destroy damage=0x{damage:x} drawable=0x{:x}",
                    client_id.0,
                    sequence.0,
                    drawable.unwrap_or(0),
                );
            } else {
                debug!(
                    "client {} #{} DAMAGE::Destroy parse_failed",
                    client_id.0, sequence.0
                );
            }
        }
        x11damage::ADD => {
            if let Some((drawable, region)) = x11damage::parse_add(body) {
                let rects = if region == 0 {
                    vec![drawable_full_rect_xfixes(state, ResourceId(drawable))]
                } else {
                    state
                        .xfixes_regions
                        .get(&region)
                        .map(|r| r.rects.clone())
                        .unwrap_or_default()
                };
                for damage in state.damage_objects.values_mut() {
                    if damage.drawable == ResourceId(drawable) {
                        damage.rects.extend(rects.clone());
                        damage.rects = normalize_region_rects(std::mem::take(&mut damage.rects));
                    }
                }
            }
            debug!("client {} #{} DAMAGE::Add", client_id.0, sequence.0);
        }
        x11damage::SUBTRACT => {
            if let Some((damage_id, repair, parts)) = x11damage::parse_subtract(body) {
                // NonEmpty damage coalesces later paints without notifying
                // again. Submit those writes before acknowledging/consuming
                // the damage, not only before the initial DamageNotify.
                backend.flush_before_damage_notify();
                // Per X11 DAMAGE spec (cf. Xorg damageext.c:419 +
                // miext/damage/damage.c:1854):
                //   if repair == None: parts ← old damage; damage ← empty
                //   else:              parts ← old ∩ repair; damage ← old − repair
                // The repair region is a read-only input filter — NEVER
                // overwrite it. Compositors (marco, picom, ...) feed
                // `parts` into `SetPictureClipRectangles`; an empty
                // parts collapses every subsequent composite to a no-op
                // and the screen freezes.
                let old_damage = state
                    .damage_objects
                    .get(&damage_id)
                    .map(|d| normalize_region_rects(d.rects.clone()))
                    .unwrap_or_default();
                let drawable = state
                    .damage_objects
                    .get(&damage_id)
                    .map(|d| d.drawable.0)
                    .unwrap_or(0);
                let level = state
                    .damage_objects
                    .get(&damage_id)
                    .map(|d| d.level)
                    .unwrap_or(0);
                let owner = state
                    .damage_objects
                    .get(&damage_id)
                    .map(|d| d.owner.0)
                    .unwrap_or(0);
                let (parts_rects, new_damage) = if repair == 0 {
                    (old_damage.clone(), Vec::new())
                } else {
                    let repair_rects = state
                        .xfixes_regions
                        .get(&repair)
                        .map(|r| r.rects.clone())
                        .unwrap_or_default();
                    (
                        crate::nested::intersect_regions(&old_damage, &repair_rects),
                        crate::nested::subtract_regions(&old_damage, &repair_rects),
                    )
                };
                let repair_rects = if repair == 0 {
                    Vec::new()
                } else {
                    state
                        .xfixes_regions
                        .get(&repair)
                        .map(|r| r.rects.clone())
                        .unwrap_or_default()
                };
                if parts != 0 {
                    state.xfixes_regions.insert(
                        parts,
                        crate::server::XFixesRegion {
                            owner: client_id,
                            rects: parts_rects,
                        },
                    );
                }
                if let Some(damage) = state.damage_objects.get_mut(&damage_id) {
                    damage.rects = new_damage;
                    damage.pending_notify_fired = false;
                }
                if repair != 0
                    && level != x11damage::report_level::RAW_RECTANGLES
                    && state
                        .damage_objects
                        .get(&damage_id)
                        .is_some_and(|d| !d.rects.is_empty())
                {
                    let _dropped = report_existing_damage_to_state(state, damage_id);
                }
                let parts_owner = if parts == 0 {
                    0
                } else {
                    state.xfixes_regions.get(&parts).map_or(0, |r| r.owner.0)
                };
                trace!(
                    "client {} #{} DAMAGE::Subtract damage=0x{damage_id:x} drawable=0x{drawable:x} \
                     owner={} level={} repair=0x{repair:x} parts=0x{parts:x} parts_owner={} \
                     old_n={} old={} repair_n={} repair_rects={} parts_n={} parts_rects={} \
                     new_n={} new_rects={}",
                    client_id.0,
                    sequence.0,
                    owner,
                    level,
                    parts_owner,
                    old_damage.len(),
                    format_region_rects(&old_damage),
                    repair_rects.len(),
                    format_region_rects(&repair_rects),
                    if parts == 0 {
                        0
                    } else {
                        state
                            .xfixes_regions
                            .get(&parts)
                            .map_or(0, |r| r.rects.len())
                    },
                    format_region_rects(
                        &state
                            .xfixes_regions
                            .get(&parts)
                            .map(|r| r.rects.clone())
                            .unwrap_or_default()
                    ),
                    state
                        .damage_objects
                        .get(&damage_id)
                        .map_or(0, |d| d.rects.len()),
                    format_region_rects(
                        &state
                            .damage_objects
                            .get(&damage_id)
                            .map(|d| d.rects.clone())
                            .unwrap_or_default()
                    ),
                );
            }
        }
        other => {
            return emit_x11_error_with_minor(
                state,
                client_id,
                sequence,
                x11::error::BAD_REQUEST,
                0,
                u16::from(other),
                header.opcode,
            );
        }
    }
    Ok(RequestOutcome::Handled)
}
