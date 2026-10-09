use super::*;

/// Task 13 (spec §"Amendment 2026-08-01 — successor-gate relaxation"):
/// single source of truth for whether `successor` provably presents its
/// full source extent, and therefore may act as a scrap successor at
/// all. MANDATORY shared helper — used by BOTH
/// `present_supersession_covers` (the coverage gate below) and
/// `supersede_covered_pending_presents`'s decline-telemetry guard; they
/// must agree on what "cleared the gate" means, or the telemetry
/// mislabels every full-extent-region decline as a coverage failure.
///
/// `None` (unchanged Xorg gate, `present_scmd.c:802`) always passes.
/// `Some(rects)` passes iff at least one single rect `r` contains the
/// full source extent, every term explicitly widened to `i32` (as
/// literally written in `i16`/`u16` this would not compile, and `u16`
/// arithmetic would wrap on negative `r.x`):
///
/// ```text
/// i32::from(r.x) <= 0
///     && i32::from(r.y) <= 0
///     && i32::from(r.x) + i32::from(r.width) >= i32::from(src_width)
///     && i32::from(r.y) + i32::from(r.height) >= i32::from(src_height)
/// ```
///
/// (pixmap coordinates, before `x_off`/`y_off` translation — Xorg
/// installs the update region as a `CT_REGION` clip with
/// `GCClipXOrigin/GCClipYOrigin = x_off/y_off`, `present.c:76-92`.) A
/// zero-area rect can never satisfy both `>=` conditions, so
/// `Some([zero-area rect])` and `Some(empty)` both decline via `.any()`
/// on an iterator that is either empty or has no satisfying element.
/// Union coverage across multiple rects is deliberately NOT computed (no
/// observed client needs it — see the spec amendment's rejected-
/// alternative note): a multi-rect region whose union but no single rect
/// covers the extent declines, conservatively.
fn successor_presents_full_extent(successor: &PendingPresentPixmap) -> bool {
    match &successor.update_rects {
        None => true,
        Some(rects) => rects.iter().any(|r| {
            i32::from(r.x) <= 0
                && i32::from(r.y) <= 0
                && i32::from(r.x) + i32::from(r.width) >= i32::from(successor.src_width)
                && i32::from(r.y) + i32::from(r.height) >= i32::from(successor.src_height)
        }),
    }
}

/// Task 8 §Supersession coverage predicate (spec §Supersession; Xorg
/// verification item (d), `present_scmd.c:802`). `successor` is the
/// newly-arriving present `B`; `predecessor` is a candidate victim `A`
/// already sitting unexecuted in `present_pending_exec`. Pure — no state
/// mutation, so the arrival-time coverage scan
/// (`supersede_covered_pending_presents`) can call it as a `.filter()`
/// predicate.
///
/// Destination-coordinate arithmetic in `i32`: offsets (`x_off`/`y_off`)
/// are `i16` and may be negative (a present whose window is partially
/// off-screen), and must NOT saturate the way `execute_present_pixmap_copy`'s
/// copy-time math does (`saturating_add`) — a saturated coordinate could
/// make an uncovered predecessor look covered.
///
/// The **successor gate comes first**, via `successor_presents_full_extent`
/// (spec §"Amendment 2026-08-01 — successor-gate relaxation"): Xorg's
/// scrap loop (`present_scmd.c:802`, `if (!update && pixmap)`) attempts
/// scrap only when the successor carries no update region; yserver
/// additionally accepts a `Some(rects)` successor when one single rect
/// provably covers the full source extent (the NVIDIA-WSI shape — a
/// full-extent single-rect update region where Mesa attaches none). A
/// successor whose update region does not clear that gate — including
/// the documented zero-pixel `Some(empty)` present and any partial or
/// multi-rect-union-only region — never scraps anything, regardless of
/// predecessor geometry; this still dissolves the marco/picom
/// drag-sliver risk (a sliver successor's region never clears the gate).
///
/// Given the gate passes, coverage is yserver's strictly conservative
/// addition on top of Xorg (which is geometry-blind): the predecessor's
/// footprint, in destination coordinates, must fit entirely inside the
/// successor's full-extent rect (top-left `(x_off, y_off)`, bottom-right
/// `(x_off plus src_width, y_off plus src_height)`) — source-pixmap-
/// bounded, not window-bounded, which matters mid-resize when Mesa
/// reallocates swapchain pixmaps on `PresentConfigureNotify`. A
/// predecessor with `update_rects == None` is covered iff its own
/// full-extent rect fits; `Some(rects)` iff every destination rect fits;
/// `Some(empty)` is trivially covered (no content, so nothing can fail
/// to fit — falls out of `.all()` on an empty iterator).
pub(super) fn present_supersession_covers(
    successor: &PendingPresentPixmap,
    predecessor: &PendingPresentPixmap,
) -> bool {
    if !successor_presents_full_extent(successor) {
        return false;
    }

    let (succ_x, succ_y) = successor.request.offsets();
    let (succ_x, succ_y) = (i32::from(succ_x), i32::from(succ_y));
    let succ_right = succ_x + i32::from(successor.src_width);
    let succ_bottom = succ_y + i32::from(successor.src_height);

    let fits = |x: i32, y: i32, w: i32, h: i32| -> bool {
        x >= succ_x && y >= succ_y && x + w <= succ_right && y + h <= succ_bottom
    };

    let (pred_x_i16, pred_y_i16) = predecessor.request.offsets();
    let (pred_x, pred_y) = (i32::from(pred_x_i16), i32::from(pred_y_i16));
    match &predecessor.update_rects {
        None => fits(
            pred_x,
            pred_y,
            i32::from(predecessor.src_width),
            i32::from(predecessor.src_height),
        ),
        Some(rects) => rects.iter().all(|r| {
            // Evaluate exactly where the real copy lands
            // (`execute_present_pixmap_copy`'s `x_off.saturating_add(rect.x)`,
            // computed in `i16` — not unsaturated `i32` addition). Near
            // `i16::MAX` the two disagree: an unsaturated `pred_x + r.x`
            // can land past where the copy's saturated dest actually
            // draws, so the predicate would judge "covered" at a
            // position the copy never used (sliver loss). Saturate in
            // `i16` first, exactly like the copy, then widen to `i32`
            // only for the extent comparison.
            fits(
                i32::from(pred_x_i16.saturating_add(r.x)),
                i32::from(pred_y_i16.saturating_add(r.y)),
                i32::from(r.width),
                i32::from(r.height),
            )
        }),
    }
}

/// Task 8 §Supersession: build the `CompletedPresentEvent` a scrapped or
/// copy-failed entry would have produced had it reached
/// `execute_present_pixmap_copy`'s `enqueue_present_completion` call —
/// fields exactly as that function builds them (`host_xid` is the
/// client-visible pixmap XID, not the backend host xid, matching the
/// wire-event identity Mesa expects).
pub(super) fn completed_event_for_pending(
    pending: &PendingPresentPixmap,
) -> crate::backend::CompletedPresentEvent {
    crate::backend::CompletedPresentEvent {
        client_id: pending.client_id,
        serial: pending.request.serial(),
        host_xid: pending.request.pixmap(),
        dst_host_xid: pending.request.window(),
        options: pending.masked_options,
        present_id: pending.present_id,
        window_generation: pending.window_generation,
        crtc_id: pending.crtc_id,
        crtc_epoch: pending.crtc_epoch,
        msc_offset: pending.msc_offset,
        completion_clock: None,
        wake: pending.wake.clone(),
        completion_mode: yserver_protocol::x11::present::COMPLETE_MODE_COPY,
        emit_idle: true,
    }
}

/// IdleNotify-only half of `fire_present_completion_events_at`, fired
/// immediately at supersession scrap and at the copy-failure reroute
/// (Task 8) — deliberately NOT reusing `fire_present_completion_events_at`
/// itself, whose `CompleteNotify` half must NOT fire at this point (the
/// completion is parked for ordered delivery at the target MSC instead).
/// Modeled on the `IdleNotify` branch of that function: same selection
/// lookup, same `encode_idle_notify` call, gated on `IDLE_NOTIFY_MASK`.
fn fire_present_idle_notify_now(
    state: &mut ServerState,
    event: &crate::backend::CompletedPresentEvent,
) {
    use crate::backend::PresentWake;
    use yserver_protocol::x11::present as x11present;

    /// PRESENT major opcode.
    const PRESENT_MAJOR_OPCODE: u8 = 145;
    /// `PresentEventMaskIdleNotify`.
    const IDLE_NOTIFY_MASK: u32 = 0x4;

    let window = ResourceId(event.dst_host_xid);
    let pixmap_xid = event.host_xid;
    let idle_fence = match &event.wake {
        PresentWake::Pixmap { idle_fence_xid } => *idle_fence_xid,
        PresentWake::PixmapSynced {
            release_syncobj, ..
        } => *release_syncobj,
    };

    let mut targets: Vec<(u32, ClientId, u32)> = Vec::new();
    for (eid, sel) in &state.present_event_selections {
        if sel.window == window {
            targets.push((*eid, sel.owner, sel.event_mask));
        }
    }

    for (eid, owner, mask) in targets {
        if mask & IDLE_NOTIFY_MASK == 0 {
            continue;
        }
        let Some(client) = state.clients.get_mut(&owner.0) else {
            continue;
        };
        let byte_order = client.byte_order;
        let seq = SequenceNumber(
            client
                .last_sequence
                .load(std::sync::atomic::Ordering::Relaxed),
        );
        let ev = x11present::encode_idle_notify(
            byte_order,
            seq,
            PRESENT_MAJOR_OPCODE,
            eid,
            window.0,
            event.serial,
            pixmap_xid,
            idle_fence,
        );
        let _ = write_to_client(client, owner, &ev);
    }
}

/// Task 8 §Supersession: scan `present_pending_exec` for entries covered
/// by the newly-arriving successor `successor` and scrap them. Called in
/// BOTH Present::Pixmap and Present::PixmapSynced handlers right after
/// `pending` is fully built and after the successor's own
/// `arm_present_source_wait` / `arm_present_syncobj_wait` has already
/// succeeded (arm-before-scrap: see the comment at each call site) —
/// scrap happens at arrival regardless of whether the successor's own
/// source was ready or deferred, and regardless of its own park/execute
/// msc-due classification.
///
/// Gated on the successor having a known effective target. A successor
/// with `effective_target_msc = None` never scraps because target
/// equivalence is not known. Victims are entries in
/// `present_pending_exec` for the same window, CRTC domain, and effective
/// target whose coverage
/// (`present_supersession_covers`) passes — everything in the store is
/// unexecuted by definition, so no additional "not yet executed" check
/// is needed.
pub(super) fn supersede_covered_pending_presents(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    successor: &PendingPresentPixmap,
) {
    let Some(target) = successor.effective_target_msc else {
        return;
    };
    let window = successor.request.window();

    let mut victim_ids: Vec<u64> = Vec::new();
    for (&pid, entry) in &state.present_pending_exec {
        if entry.pending.request.window() != window
            || entry.pending.crtc_id != successor.crtc_id
            || entry.pending.crtc_epoch != successor.crtc_epoch
            || entry.pending.effective_target_msc != Some(target)
        {
            continue;
        }
        if present_supersession_covers(successor, &entry.pending) {
            victim_ids.push(pid);
        } else if successor_presents_full_extent(successor) {
            // Task 10 telemetry (spec §Telemetry / verification item (d)):
            // the successor cleared the amended successor gate
            // (`successor_presents_full_extent` — no update region, or a
            // single rect covering the full source extent) but yserver's
            // own extent-coverage check declined this candidate anyway —
            // the empirical input for the future coverage-relaxation
            // decision. Excluded here: successors whose update region
            // does NOT clear the gate, which never even attempt scrap
            // (the gate itself, `present_supersession_covers`'s first
            // check) and would flood this log on every marco-style
            // partial update.
            log::debug!(
                target: "present_pace",
                "PACE-INSTR t={} pid={} stage=supersede_declined by={} window=0x{:x} eff={}",
                pace_instr_ms(), pid, successor.present_id, window, target
            );
        }
    }

    for pid in victim_ids {
        let Some(entry) = state.present_pending_exec.remove(&pid) else {
            continue;
        };
        // Task 10 telemetry: one `note_present_skip` per scrapped victim
        // — the copy that did NOT happen. KMS surfaces this as
        // `present_skips/s` in `render_telemetry`.
        backend.note_present_skip();
        // 1. Cancel the source/acquire wait if armed, and release the
        // entry's own source pin.
        if let Some(wid) = entry.wait_id {
            backend.finish_present_source_wait(wid);
            state.present_wait_to_id.remove(&wid);
        }
        if let Some(pin) = entry.pin {
            backend.release_present_source(pin);
        }
        // 2. By-XID buffer release, immediately — the victim never
        // reached `enqueue_present_completion`, so this is its sole
        // release path (no backend `PinnedWake` / gate entry exists for
        // it). The X11 fence mirror write is REQUIRED here, as it is in
        // the window-destroy purge: without it a client's own fence query
        // could disagree with its unblocked wait for up to a period.
        match &entry.pending.wake {
            crate::backend::PresentWake::Pixmap { idle_fence_xid } if *idle_fence_xid != 0 => {
                if let Err(e) = backend.dri3_trigger_fence(*idle_fence_xid) {
                    log::warn!(
                        "PRESENT supersede: trigger idle fence 0x{idle_fence_xid:x} failed: {e}"
                    );
                }
                crate::core_loop::sync_await::fence_triggered(state, *idle_fence_xid);
            }
            crate::backend::PresentWake::PixmapSynced {
                release,
                release_syncobj,
                release_value,
            } => {
                if let Err(e) = release.signal(*release_value) {
                    log::warn!(
                        "PRESENT supersede: signal release syncobj 0x{release_syncobj:x}@\
                         {release_value} failed: {e}"
                    );
                }
            }
            _ => {}
        }
        let event = completed_event_for_pending(&entry.pending);
        // 3. Fire IdleNotify only, now — CompleteNotify is parked below.
        fire_present_idle_notify_now(state, &event);
        log::debug!(
            target: "present_pace",
            "PACE-INSTR t={} pid={} stage=superseded by={} window=0x{:x} eff={}",
            pace_instr_ms(), pid, successor.present_id, window, target
        );
        // 4. Park the Skip for ordered delivery at the target clock.
        // 5. The entry + side-map row are already gone (removed above).
        state
            .present_pending_complete
            .push(crate::server::PendingPresentComplete {
                event,
                effective_target_msc: target,
                mode: yserver_protocol::x11::present::COMPLETE_MODE_SKIP,
                emit_idle: false,
            });
    }
}

/// Arm a `Present::Pixmap` successor's own source-pixmap wait, then scrap
/// covered pending entries — **in this order, never reversed**. The arm
/// is client-reachably fallible; scrap is not reversible (it releases
/// victims' buffers and fires their IdleNotify/Skip immediately). Running
/// scrap first and then having the arm fail would unwind via `?` with the
/// victims already destroyed and nothing committed to replace them,
/// leaving the window with no frame at the target MSC where Xorg would
/// still have shown the victim. Factored out (rather than inlined at the
/// call site) so a regression test can exercise the exact production
/// ordering instead of calling the two steps itself.
fn arm_present_pixmap_source_then_supersede(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    pending: &PendingPresentPixmap,
) -> std::io::Result<crate::backend::PresentSourceWait> {
    let armed =
        backend.arm_present_source_wait(pending.src_host_xid, pending.paint_dst_host_xid)?;
    supersede_covered_pending_presents(state, backend, pending);
    Ok(armed)
}

/// `Present::PixmapSynced` counterpart of
/// `arm_present_pixmap_source_then_supersede`: arms the explicit acquire
/// syncobj wait before scrapping covered pending entries, for the same
/// reason (an unknown/invalid acquire syncobj makes this arm fail in a
/// way clients can trigger, and scrap must not run ahead of a still-
/// fallible arm).
pub(super) fn arm_present_pixmap_synced_source_then_supersede(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    acquire_syncobj: u32,
    acquire_value: u64,
    pending: &PendingPresentPixmap,
) -> std::io::Result<crate::backend::PresentSourceWait> {
    let armed = backend.arm_present_syncobj_wait(
        pending.src_host_xid,
        pending.paint_dst_host_xid,
        acquire_syncobj,
        acquire_value,
    )?;
    supersede_covered_pending_presents(state, backend, pending);
    Ok(armed)
}

fn present_crtc_is_enabled(state: &ServerState, crtc_id: u32) -> bool {
    crtc_id != 0
        && state
            .randr
            .enabled_outputs()
            .any(|output| output.crtc_id == crtc_id)
}

fn present_crtc_exists(state: &ServerState, crtc_id: u32) -> bool {
    state
        .randr
        .outputs
        .iter()
        .any(|output| output.crtc_id == crtc_id)
}

/// Select an enabled output by greatest intersection with `window`.
/// RANDR primary wins equal-area ties (including the all-zero fallback),
/// then the stable `randr.outputs` order wins. With no enabled outputs the
/// synthetic headless domain 0 preserves zero-device operation.
pub(super) fn default_present_crtc_for_window(state: &ServerState, window: ResourceId) -> u32 {
    let Some(window_record) = state.resources.window(window) else {
        return 0;
    };
    let (window_x, window_y) = state.resources.window_absolute_position(window);
    let window_right = window_x.saturating_add(i32::from(window_record.width));
    let window_bottom = window_y.saturating_add(i32::from(window_record.height));
    let primary = state.randr.primary_output;

    let mut best: Option<(u64, bool, u32)> = None;
    for output in state.randr.enabled_outputs() {
        let output_x = i32::from(output.x);
        let output_y = i32::from(output.y);
        let (footprint_w, footprint_h) = output.footprint();
        let output_right = output_x.saturating_add(i32::from(footprint_w));
        let output_bottom = output_y.saturating_add(i32::from(footprint_h));
        let width = window_right.min(output_right) - window_x.max(output_x);
        let height = window_bottom.min(output_bottom) - window_y.max(output_y);
        let area = if width > 0 && height > 0 {
            u64::try_from(width).unwrap_or(0) * u64::try_from(height).unwrap_or(0)
        } else {
            0
        };
        let is_primary = output.output_id == primary;
        let replace = best.is_none_or(|(best_area, best_primary, _)| {
            area > best_area || (area == best_area && is_primary && !best_primary)
        });
        if replace {
            best = Some((area, is_primary, output.crtc_id));
        }
    }
    best.map_or(0, |(_, _, crtc_id)| crtc_id)
}

pub(crate) fn cached_present_crtc_clock(
    state: &ServerState,
    crtc_id: u32,
    crtc_epoch: u64,
) -> crate::server::PresentCrtcClock {
    state
        .present_crtc_clocks
        .get(&(crtc_id, crtc_epoch))
        .copied()
        .unwrap_or_else(|| crate::server::PresentCrtcClock {
            epoch: crtc_epoch,
            ..crate::server::PresentCrtcClock::default()
        })
}

/// Refresh one current physical clock epoch. A valid-but-Off CRTC and the
/// headless domain stay at zero; they are accepted but deliberately unpaced.
pub(crate) fn refresh_present_crtc_general_clock(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    crtc_id: u32,
    crtc_epoch: u64,
) -> crate::server::PresentCrtcClock {
    if crtc_id == 0
        || !present_crtc_is_enabled(state, crtc_id)
        || backend.present_crtc_clock_epoch(crtc_id) != crtc_epoch
    {
        return cached_present_crtc_clock(state, crtc_id, crtc_epoch);
    }
    let (msc, ust) = backend.present_get_ust_msc(crtc_id);
    let clock = state
        .present_crtc_clocks
        .entry((crtc_id, crtc_epoch))
        .or_insert_with(|| crate::server::PresentCrtcClock {
            epoch: crtc_epoch,
            ..crate::server::PresentCrtcClock::default()
        });
    if msc > 0 {
        clock.msc = msc;
        clock.ust = ust;
    }
    *clock
}

pub(crate) fn refresh_present_crtc_completion_clock(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    crtc_id: u32,
    crtc_epoch: u64,
    exact: Option<crate::backend::PresentClockSample>,
) -> crate::backend::PresentClockSample {
    let has_exact = exact.is_some();
    let sample = if let Some(exact) = exact {
        exact
    } else if crtc_id != 0
        && present_crtc_is_enabled(state, crtc_id)
        && backend.present_crtc_clock_epoch(crtc_id) == crtc_epoch
    {
        backend.present_get_completion_clock(crtc_id)
    } else {
        return cached_present_crtc_clock(state, crtc_id, crtc_epoch).completion;
    };
    advance_present_crtc_completion_clock(state, crtc_id, crtc_epoch, sample);
    let cached = state
        .present_crtc_clocks
        .get(&(crtc_id, crtc_epoch))
        .copied()
        .unwrap_or_else(|| crate::server::PresentCrtcClock {
            epoch: crtc_epoch,
            ..crate::server::PresentCrtcClock::default()
        });
    if has_exact { sample } else { cached.completion }
}

fn advance_present_crtc_completion_clock(
    state: &mut ServerState,
    crtc_id: u32,
    crtc_epoch: u64,
    sample: crate::backend::PresentClockSample,
) {
    let clock = state
        .present_crtc_clocks
        .entry((crtc_id, crtc_epoch))
        .or_insert_with(|| crate::server::PresentCrtcClock {
            epoch: crtc_epoch,
            ..crate::server::PresentCrtcClock::default()
        });
    let advances_cache = sample.msc > 0
        && (clock.completion.msc == 0
            || sample.msc == clock.completion.msc
            || crate::present_scheduler::msc_is_after(sample.msc, clock.completion.msc));
    if advances_cache {
        clock.completion = sample;
    }
}

pub(super) fn present_wire_clock(
    raw: crate::backend::PresentClockSample,
    msc_offset: u64,
) -> crate::backend::PresentClockSample {
    crate::backend::PresentClockSample {
        msc: raw.msc.wrapping_sub(msc_offset),
        ..raw
    }
}

/// Bind a Present window to a CRTC clock while preserving Xorg's continuous
/// window MSC. `msc_offset` is defined by `wire_msc = raw_msc - offset`;
/// therefore a clock-domain switch applies `offset += new_raw - old_raw`.
fn bind_present_window_domain(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    window: u32,
    crtc_id: u32,
) -> PresentDomainSelection {
    let crtc_epoch = backend.present_crtc_clock_epoch(crtc_id);
    let new_clock = refresh_present_crtc_general_clock(state, backend, crtc_id, crtc_epoch);
    let previous = state.present_window_msc.get(&window).copied();

    let next = match previous {
        None => crate::server::PresentWindowMsc {
            last_crtc: crtc_id,
            last_crtc_epoch: crtc_epoch,
            msc_offset: 0,
            last_raw_msc: new_clock.msc,
        },
        Some(mut window_clock)
            if window_clock.last_crtc == crtc_id && window_clock.last_crtc_epoch == crtc_epoch =>
        {
            if new_clock.msc > 0 {
                window_clock.last_raw_msc = new_clock.msc;
            }
            window_clock
        }
        Some(mut window_clock) => {
            let current_old_epoch = backend.present_crtc_clock_epoch(window_clock.last_crtc);
            let old_raw = if current_old_epoch == window_clock.last_crtc_epoch {
                let old_clock = refresh_present_crtc_general_clock(
                    state,
                    backend,
                    window_clock.last_crtc,
                    window_clock.last_crtc_epoch,
                );
                if old_clock.msc > 0 {
                    old_clock.msc
                } else {
                    window_clock.last_raw_msc
                }
            } else {
                // The stable RANDR XID now names another physical counter.
                // Never sample that replacement as the old side of the rebase.
                let cached_old = cached_present_crtc_clock(
                    state,
                    window_clock.last_crtc,
                    window_clock.last_crtc_epoch,
                );
                if cached_old.msc > 0 {
                    cached_old.msc
                } else {
                    window_clock.last_raw_msc
                }
            };
            window_clock.msc_offset = window_clock
                .msc_offset
                .wrapping_add(new_clock.msc.wrapping_sub(old_raw));
            window_clock.last_crtc = crtc_id;
            window_clock.last_crtc_epoch = crtc_epoch;
            window_clock.last_raw_msc = new_clock.msc;
            window_clock
        }
    };
    state.present_window_msc.insert(window, next);
    PresentDomainSelection {
        crtc_id,
        crtc_epoch,
        msc_offset: next.msc_offset,
        raw_msc: new_clock.msc,
        raw_ust: new_clock.ust,
    }
}

/// Resolve and bind a request domain. Explicit nonzero XIDs validate only
/// resource existence (Xorg accepts a valid-but-Off CRTC); implicit Pixmap
/// requests select by coverage every time, while NotifyMSC can request reuse
/// of the window's previous Present domain.
pub(super) fn select_present_domain(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    window: u32,
    target_crtc: u32,
    reuse_previous: bool,
) -> Result<PresentDomainSelection, u32> {
    let crtc_id = if target_crtc != 0 {
        if !present_crtc_exists(state, target_crtc) {
            return Err(target_crtc);
        }
        target_crtc
    } else if reuse_previous {
        state.present_window_msc.get(&window).map_or_else(
            || default_present_crtc_for_window(state, ResourceId(window)),
            |c| c.last_crtc,
        )
    } else {
        default_present_crtc_for_window(state, ResourceId(window))
    };
    Ok(bind_present_window_domain(state, backend, window, crtc_id))
}

pub(super) fn effective_present_target_raw(
    domain: PresentDomainSelection,
    target_msc: u64,
    divisor: u64,
    remainder: u64,
    options: u32,
) -> Option<u64> {
    if domain.raw_msc == 0 {
        return None;
    }
    // Xorg intentionally shifts only target_msc; divisor/remainder stay in
    // the raw CRTC clock's residue class.
    let effective = crate::present_scheduler::effective_target_msc(
        target_msc.wrapping_add(domain.msc_offset),
        domain.raw_msc,
        divisor,
        remainder,
        options,
    );
    // Xorg retains the target MSC even when an async request resolves to the
    // current field. Besides being the protocol completion identity, that
    // value groups same-target requests for supersession. `None` is reserved
    // for a domain with no usable clock; collapsing current/past async targets
    // to `None` makes the core park them behind an in-flight flip and prevents
    // both core supersession and the backend's bounded successor queue.
    Some(effective)
}

/// Execute a batch of specific `present_pending_exec` entries by id —
/// shared by every msc-due execution site (the due-pass's normal
/// execute-due step, the idle-display fallback, the absolute-arm
/// Ok(0)/Err fallback, and the blackout flush). Missing ids (already
/// resolved by a concurrent path) are silently skipped. Mirrors
/// `drain_ready_present_pixmaps`'s execute/release/mark_dirty sequence:
/// the entry pin is released after `execute_present_pixmap_copy`
/// regardless of success or failure, and `mark_dirty` fires only on
/// success.
pub(crate) fn execute_parked_present_ids(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    ids: &[u64],
    trigger: &str,
) {
    for &pid in ids {
        let Some(entry) = state.present_pending_exec.remove(&pid) else {
            continue;
        };
        log::debug!(
            target: "present_pace",
            "PACE-INSTR t={} pid={} stage=exec_due trigger={trigger} eff={:?}",
            pace_instr_ms(),
            pid,
            entry.pending.effective_target_msc
        );
        let ok = execute_present_pixmap_copy_or_reroute(state, backend, entry.pending);
        if let Some(pin) = entry.pin {
            backend.release_present_source(pin);
        }
        if ok {
            backend.mark_dirty();
        }
    }
}

/// msc-due-pass (spec §msc-due; Task 7): re-classify every msc-parked
/// source-ready entry against the fresh general clock and execute
/// whatever just became due, then run the two fallback-ladder rungs that
/// are execution decisions rather than arming (the third rung, the
/// absolute-vblank arm, is an arming call site and lives in
/// `run::arm_present_idle_vblanks`, post-compose, alongside the other two
/// arms). Called at the top of `drain_present_completions`, which Task 4
/// hoisted above `maybe_composite`, so an entry executed here is visible
/// to THIS iteration's compose.
pub(crate) fn drain_due_present_pending_exec(state: &mut ServerState, backend: &mut dyn Backend) {
    // Sampled up front (not just inside the blackout branch below): an
    // empty `present_pending_exec` must NOT short-circuit past the
    // blackout flush, or DPMS-off with a display that never accumulates
    // a msc-parked entry (flips keep retiring normally, so every arrival
    // classifies ExecuteNow and the store stays empty) would leave
    // `present_pending_complete` entries gated against a frozen
    // completion clock with nothing left to ever call
    // `fire_all_present_completions_now` — they'd park forever. The
    // sweep inside that call still early-returns on an empty queue, so
    // this costs nothing on the common (non-blackout) empty-store path.
    let blackout = backend.present_scanout_blackout();
    if state.present_pending_exec.is_empty() && !blackout {
        return;
    }
    let mut domains: Vec<(u32, u64)> = state
        .present_pending_exec
        .values()
        .map(|entry| (entry.pending.crtc_id, entry.pending.crtc_epoch))
        .collect();
    domains.sort_unstable();
    domains.dedup();
    for &(crtc_id, crtc_epoch) in &domains {
        if backend.present_crtc_clock_epoch(crtc_id) == crtc_epoch {
            refresh_present_crtc_general_clock(state, backend, crtc_id, crtc_epoch);
        }
    }

    let due: Vec<u64> = state
        .present_pending_exec
        .iter()
        .filter(|(_, e)| {
            if !e.source_ready {
                return false;
            }
            let epoch_current =
                backend.present_crtc_clock_epoch(e.pending.crtc_id) == e.pending.crtc_epoch;
            if !epoch_current {
                // A stable RANDR XID now names a replacement raw counter.
                // The saved raw target is meaningless there; fail open
                // unpaced instead of arming/comparing against the new epoch.
                return true;
            }
            let clock_msc =
                cached_present_crtc_clock(state, e.pending.crtc_id, e.pending.crtc_epoch).msc;
            matches!(
                crate::present_scheduler::classify_msc_due(
                    e.pending.effective_target_msc,
                    clock_msc,
                    backend.present_flip_in_flight(e.pending.crtc_id),
                ),
                crate::present_scheduler::MscDue::ExecuteNow
            )
        })
        .map(|(&pid, _)| pid)
        .collect();
    execute_parked_present_ids(state, backend, &due, "drain");

    // Idle-display fallback (spec §msc-due, flip-driven drivers only): a
    // parked entry normally becomes due as flips advance the clock — the
    // next flip retirement wakes this same due-pass and the `due` filter
    // above catches it. But if the display is fully idle (no flip in
    // flight AND nothing composing), the clock can never advance at all,
    // so waiting would deadlock forever; execute immediately instead.
    // Gated to `!present_absolute_vblank_arm_supported()` drivers only —
    // on sequence-capable drivers an idle display still ticks via the
    // absolute arm, and applying this fallback there would reintroduce
    // the early-frame bug for mpv (spec §msc-due).
    let idle_fallback: Vec<u64> = state
        .present_pending_exec
        .iter()
        .filter(|(_, e)| {
            e.source_ready
                && e.pending.effective_target_msc.is_some()
                && !backend.present_absolute_vblank_arm_supported(e.pending.crtc_id)
                && backend.present_display_idle(e.pending.crtc_id)
        })
        .map(|(&pid, _)| pid)
        .collect();
    execute_parked_present_ids(state, backend, &idle_fallback, "idle_fallback");

    // Blackout flush (spec Lifecycle §"DPMS-off / VT-away blackout"):
    // checked unconditionally (not an `else` of the arm/idle-display
    // rungs above) so it fires even while the clock is frozen — no flips,
    // no sequence samples, `next_wakeup`'s scene deadline gated off, so
    // nothing else here would ever unblock these entries. Only
    // `source_ready` (msc-parked) entries are force-executed: a
    // `source_ready == false` entry is still waiting on its OWN producer
    // fence, a condition blackout does nothing to resolve, and forcing
    // that copy would read whatever partial content the producer has
    // written so far.
    if blackout {
        let blackout_ids: Vec<u64> = state
            .present_pending_exec
            .iter()
            .filter(|(_, e)| e.source_ready)
            .map(|(&pid, _)| pid)
            .collect();
        execute_parked_present_ids(state, backend, &blackout_ids, "blackout");

        // Both halves flush together: parked completions deliver too,
        // ignoring the due check entirely (a frozen clock would
        // otherwise never satisfy it — round-4 F1c). Stamped with the
        // COMPLETION clock, not the general clock used for scheduling
        // above — stamping/gate-release keeps its existing
        // completion-clock provenance (spec "Loop-order and clock
        // contract" item 2: scheduling and stamping are different
        // concerns).
        fire_all_present_completions_now(state, backend);
    }
}

fn accumulate_present_execution_damage(
    state: &mut ServerState,
    window: u32,
    x_off: i16,
    y_off: i16,
    src_width: u16,
    src_height: u16,
    update_rects: Option<&[yserver_protocol::x11::xfixes::RegionRect]>,
) {
    if let Some(rects) = update_rects {
        for rect in rects {
            let _dropped = accumulate_damage_to_state(
                state,
                ResourceId(window),
                x_off.saturating_add(rect.x),
                y_off.saturating_add(rect.y),
                rect.width,
                rect.height,
            );
        }
    } else {
        let _dropped = accumulate_damage_to_state(
            state,
            ResourceId(window),
            x_off,
            y_off,
            src_width,
            src_height,
        );
    }
}

pub(super) fn execute_present_pixmap_copy(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    pending: PendingPresentPixmap,
) -> io::Result<()> {
    let PendingPresentPixmap {
        origin,
        client_id,
        request: req,
        wake,
        masked_options,
        src_host_xid,
        paint_dst_host_xid,
        completion_dst_host_xid,
        src_width,
        src_height,
        update_rects,
        present_id,
        window_generation,
        crtc_id,
        crtc_epoch,
        msc_offset,
        effective_target_msc,
    } = pending;
    let (serial, pixmap, window, x_off, y_off, valid, update) = match &req {
        PendingPresentRequest::Pixmap(req) => (
            req.serial, req.pixmap, req.window, req.x_off, req.y_off, req.valid, req.update,
        ),
        PendingPresentRequest::PixmapSynced(req) => (
            req.serial, req.pixmap, req.window, req.x_off, req.y_off, req.valid, req.update,
        ),
    };

    let candidate = crate::backend::PresentScanoutCandidate {
        client_id: client_id.0,
        present_id,
        crtc_id,
        crtc_epoch,
        src_pixmap_xid: pixmap,
        dst_window_xid: window,
        src_host_xid,
        paint_dst_host_xid,
        completion_dst_host_xid,
        src_width,
        src_height,
        x_off,
        y_off,
        valid_region_xid: valid,
        update_region_xid: update,
        update_is_full: update_rects.is_none(),
        explicit_sync: matches!(req, PendingPresentRequest::PixmapSynced(_)),
        options: masked_options,
    };
    let completion = crate::backend::CompletedPresentEvent {
        client_id,
        serial,
        host_xid: pixmap,
        dst_host_xid: window,
        options: masked_options,
        present_id,
        window_generation,
        crtc_id,
        crtc_epoch,
        msc_offset,
        completion_clock: None,
        wake,
        completion_mode: yserver_protocol::x11::present::COMPLETE_MODE_COPY,
        emit_idle: true,
    };

    if window_unviewable(state, ResourceId(window)) {
        // Xorg: flip check fails on the empty clipList and the Copy clips to nothing,
        // but idle and CompleteModeCopy are still delivered (present_execute.c:119-156).
        if let Some(eff) = effective_target_msc {
            state.present_complete_gate.insert(
                present_id,
                crate::server::PresentCompleteGate {
                    crtc_id,
                    crtc_epoch,
                    msc_offset,
                    effective_target_msc: eff,
                    owner: client_id,
                    dst_window_xid: window,
                },
            );
        }
        backend.enqueue_present_completion(completion, completion_dst_host_xid);
        return Ok(());
    }
    backend.note_present_scanout_candidate(candidate);

    // M2b attempts direct ownership before recording the fallback Copy. The
    // completion gate must exist before the backend can own the page flip;
    // on decline/error remove the provisional row so the unchanged Copy path
    // below retains its historical "install only after successful Copy"
    // failure semantics.
    if let Some(eff) = effective_target_msc {
        state.present_complete_gate.insert(
            present_id,
            crate::server::PresentCompleteGate {
                crtc_id,
                crtc_epoch,
                msc_offset,
                effective_target_msc: eff,
                owner: client_id,
                dst_window_xid: window,
            },
        );
    }
    match backend.try_present_direct(candidate, completion.clone()) {
        Ok(true) => {
            backend.note_present_pixmap(src_host_xid, paint_dst_host_xid);
            accumulate_present_execution_damage(
                state,
                window,
                x_off,
                y_off,
                src_width,
                src_height,
                update_rects.as_deref(),
            );
            return Ok(());
        }
        Ok(false) => {}
        Err(error) => log::warn!(
            "present direct submit failed, retaining Copy fallback (pid={present_id}): {error}"
        ),
    }
    state.present_complete_gate.remove(&present_id);

    // PresentPixmap has no client GC. Clear every piece of draw state that a
    // preceding request may have left bound before recording the copy.
    let present_gc = crate::backend::DrawState::default();
    backend.apply_clip_state(origin, &present_gc.clip)?;
    backend.apply_draw_state(origin, &present_gc)?;

    if let Some(rects) = &update_rects {
        for rect in rects {
            backend.copy_area(
                origin,
                src_host_xid,
                paint_dst_host_xid,
                rect.x,
                rect.y,
                x_off.saturating_add(rect.x),
                y_off.saturating_add(rect.y),
                rect.width,
                rect.height,
            )?;
        }
    } else {
        backend.copy_area(
            origin,
            src_host_xid,
            paint_dst_host_xid,
            0,
            0,
            x_off,
            y_off,
            src_width,
            src_height,
        )?;
    }

    backend.note_present_pixmap(src_host_xid, paint_dst_host_xid);
    // Task 13 (spec §"Amendment 2026-08-01" — damage-arm dependency):
    // re-keyed off `update_rects.is_none()`, not the raw `update` xid —
    // an unresolvable region (`update != 0` but `update_rects == None`)
    // must still damage full-extent, not nothing. Per-rect damage is
    // translated by `x_off`/`y_off` exactly like the copy arm above
    // (`x_off.saturating_add(rect.x)`), matching Xorg's `CT_REGION` clip-
    // origin semantics (`present.c:76-92`).
    accumulate_present_execution_damage(
        state,
        window,
        x_off,
        y_off,
        src_width,
        src_height,
        update_rects.as_deref(),
    );

    // The direct attempt above removed its provisional gate when it declined
    // ownership. Reinstall that gate only after the fallback Copy succeeded,
    // preserving the historical copy-failure semantics.
    if let Some(eff) = effective_target_msc {
        state.present_complete_gate.insert(
            present_id,
            crate::server::PresentCompleteGate {
                crtc_id,
                crtc_epoch,
                msc_offset,
                effective_target_msc: eff,
                owner: client_id,
                dst_window_xid: window,
            },
        );
    }
    backend.enqueue_present_completion(completion, completion_dst_host_xid);
    Ok(())
}

/// Task 8 §Supersession copy-failure reroute (round-4 F5). Wrapper
/// around `execute_present_pixmap_copy` used by ALL THREE execute sites
/// (`execute_parked_present_ids`, `arrival_execute_or_park_present_pixmap`'s
/// `ExecuteNow` arm, `drain_ready_present_pixmaps`'s `ExecuteNow` arm) —
/// pin release stays at the call sites, after this call, as today.
///
/// Today a failing `execute_present_pixmap_copy` can only fail in
/// `apply_clip_state`/`apply_draw_state`/`copy_area` — all BEFORE the
/// completion gate is inserted and `enqueue_present_completion` runs, so
/// a failed copy leaves no gate row and no retained backend wake. Left
/// alone, that strands the client's buffer (never idled) and its
/// completion (never delivered) forever. On failure this wrapper:
/// releases the buffer by XID immediately (fence/syncobj + fence mirror,
/// mirroring scrap), fires `IdleNotify` now, and either parks a `Copy`
/// completion (`emit_idle: false`) in the ordered delivery queue when
/// `effective_target_msc` is `Some` (rides the same per-window hold-back
/// as everything else), or — when `None` (no clock, including
/// nested backends whose sweep never runs because `clock.msc == 0`
/// guards it) — delivers it inline, mirroring `run.rs`'s no-clock arm: a
/// `fire_due_present_completions` flush first (so this inline delivery
/// cannot overtake an already-due sibling), then `complete_present_with_clock`
/// with an Immediate clock sample built like `complete_present_now` does.
///
/// Returns `true` iff the copy succeeded.
pub(super) fn execute_present_pixmap_copy_or_reroute(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    pending: PendingPresentPixmap,
) -> bool {
    // Captured before the call below consumes `pending`.
    let event = completed_event_for_pending(&pending);
    let wake = pending.wake.clone();
    let effective_target_msc = pending.effective_target_msc;
    let present_id = pending.present_id;

    match execute_present_pixmap_copy(state, backend, pending) {
        Ok(()) => true,
        Err(e) => {
            log::warn!("present copy failed, rerouting completion (pid={present_id}): {e}");
            // Accepted trade (Task 0 item (a) has the same shape for
            // Xorg-parity scrap): on a multi-rect `update_rects` copy, a
            // failure partway through may leave earlier rects' GPU reads
            // of the source already recorded before the failing rect. By-
            // XID buffer release below happens immediately regardless, so
            // this is a WAR hazard against those already-recorded reads —
            // a concurrent client write to the released buffer could race
            // GPU work still reading it. Accepted because the alternative
            // (holding the buffer/completion open indefinitely, hoping a
            // retry never comes) strands the client's buffer and
            // completion forever, which is strictly worse.
            match wake {
                crate::backend::PresentWake::Pixmap { idle_fence_xid } if idle_fence_xid != 0 => {
                    if let Err(e) = backend.dri3_trigger_fence(idle_fence_xid) {
                        log::warn!(
                            "PRESENT copy-failure: trigger idle fence 0x{idle_fence_xid:x} \
                             failed: {e}"
                        );
                    }
                    crate::core_loop::sync_await::fence_triggered(state, idle_fence_xid);
                }
                crate::backend::PresentWake::PixmapSynced {
                    release,
                    release_syncobj,
                    release_value,
                } => {
                    if let Err(e) = release.signal(release_value) {
                        log::warn!(
                            "PRESENT copy-failure: signal release syncobj \
                             0x{release_syncobj:x}@{release_value} failed: {e}"
                        );
                    }
                }
                _ => {}
            }
            fire_present_idle_notify_now(state, &event);
            log::debug!(
                target: "present_pace",
                "PACE-INSTR t={} pid={} stage=copy_failed eff={:?}",
                pace_instr_ms(), present_id, effective_target_msc
            );
            if let Some(target) = effective_target_msc {
                state
                    .present_pending_complete
                    .push(crate::server::PendingPresentComplete {
                        event,
                        effective_target_msc: target,
                        mode: yserver_protocol::x11::present::COMPLETE_MODE_COPY,
                        emit_idle: false,
                    });
            } else {
                // No clock: no due-pass will ever drain a parked
                // queue entry for this present, so deliver inline —
                // mirroring run.rs's no-clock completion arm. Flush
                // due-and-unblocked siblings first so this inline
                // delivery cannot overtake an already-due one.
                let clock = refresh_present_crtc_completion_clock(
                    state,
                    backend,
                    event.crtc_id,
                    event.crtc_epoch,
                    event.completion_clock,
                );
                fire_due_present_completions_for_domain(
                    state,
                    backend,
                    event.crtc_id,
                    event.crtc_epoch,
                    clock,
                );
                let cached = cached_present_crtc_clock(state, event.crtc_id, event.crtc_epoch);
                let immediate = crate::backend::PresentClockSample {
                    msc: cached.msc,
                    ust: cached.ust,
                    source: crate::backend::PresentClockSource::Immediate,
                };
                complete_present_with_clock(
                    state,
                    backend,
                    &event,
                    immediate,
                    yserver_protocol::x11::present::COMPLETE_MODE_COPY,
                    false,
                );
            }
            false
        }
    }
}

/// Arrival-time msc-due evaluation (spec §msc-due; Task 7 Step 2) for a
/// present whose source is READY at request time — i.e. this runs at the
/// point today's code executed the copy unconditionally. Request drain
/// runs before the run-loop tail, so a lone present on an idle display
/// still executes right here at arrival; it never waits for the next
/// due-pass. `ExecuteNow` keeps today's immediate-copy path. `Park` inserts
/// a `source_ready: true, wait_id: None` entry (taking the entry pin) —
/// the due-pass (`drain_due_present_pending_exec`) picks it up later.
pub(super) fn arrival_execute_or_park_present_pixmap(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    present_id: u64,
    pending: PendingPresentPixmap,
) {
    let epoch_current = backend.present_crtc_clock_epoch(pending.crtc_id) == pending.crtc_epoch;
    let clock_msc = if epoch_current {
        refresh_present_crtc_general_clock(state, backend, pending.crtc_id, pending.crtc_epoch).msc
    } else {
        cached_present_crtc_clock(state, pending.crtc_id, pending.crtc_epoch).msc
    };
    let flip_in_flight = epoch_current && backend.present_flip_in_flight(pending.crtc_id);
    let due = if epoch_current {
        crate::present_scheduler::classify_msc_due(
            pending.effective_target_msc,
            clock_msc,
            flip_in_flight,
        )
    } else {
        crate::present_scheduler::MscDue::ExecuteNow
    };
    match due {
        crate::present_scheduler::MscDue::ExecuteNow => {
            let stage = match &pending.request {
                PendingPresentRequest::Pixmap(_) => "source_ready",
                PendingPresentRequest::PixmapSynced(_) => "acquire_ready",
            };
            log::debug!(
                target: "present_pace",
                "PACE-INSTR t={} pid={} stage={stage}",
                pace_instr_ms(), pending.present_id
            );
            execute_present_pixmap_copy_or_reroute(state, backend, pending);
        }
        crate::present_scheduler::MscDue::Park => {
            let reason = if flip_in_flight {
                "flip_in_flight"
            } else {
                "future"
            };
            log::debug!(
                target: "present_pace",
                "PACE-INSTR t={} pid={} stage=parked_msc reason={reason} eff={:?} clock_msc={clock_msc}",
                pace_instr_ms(), pending.present_id, pending.effective_target_msc
            );
            let pin = backend.pin_present_source(pending.src_host_xid);
            state.present_pending_exec.insert(
                present_id,
                crate::server::PendingPresentEntry {
                    pending,
                    source_ready: true,
                    wait_id: None,
                    pin,
                },
            );
        }
    }
}

pub(crate) fn drain_ready_present_pixmaps(state: &mut ServerState, backend: &mut dyn Backend) {
    for wait_id in backend.drain_ready_present_source_waits() {
        let present_id = state.present_wait_to_id.remove(&wait_id);
        let entry = present_id.and_then(|pid| state.present_pending_exec.remove(&pid));
        let outcome = entry.map(|mut entry| {
            let stage = match &entry.pending.request {
                PendingPresentRequest::Pixmap(_) => "source_signaled",
                PendingPresentRequest::PixmapSynced(_) => "acquire_signaled",
            };
            let epoch_current = backend.present_crtc_clock_epoch(entry.pending.crtc_id)
                == entry.pending.crtc_epoch;
            let clock_msc = if epoch_current {
                refresh_present_crtc_general_clock(
                    state,
                    backend,
                    entry.pending.crtc_id,
                    entry.pending.crtc_epoch,
                )
                .msc
            } else {
                cached_present_crtc_clock(
                    state,
                    entry.pending.crtc_id,
                    entry.pending.crtc_epoch,
                )
                .msc
            };
            let flip_in_flight =
                epoch_current && backend.present_flip_in_flight(entry.pending.crtc_id);
            let due = if epoch_current {
                crate::present_scheduler::classify_msc_due(
                    entry.pending.effective_target_msc,
                    clock_msc,
                    flip_in_flight,
                )
            } else {
                crate::present_scheduler::MscDue::ExecuteNow
            };
            log::debug!(
                target: "present_pace",
                "PACE-INSTR t={} pid={} stage={} wait_id={}",
                pace_instr_ms(), entry.pending.present_id, stage, wait_id
            );
            match due {
                // The entry became source-ready AND is msc-due: execute now
                // — releasing the ENTRY pin on both the success and
                // failure path so a failing copy never leaks the pinned
                // drawable.
                crate::present_scheduler::MscDue::ExecuteNow => {
                    backend.begin_ready_present_destination_write(wait_id);
                    let ok =
                        execute_present_pixmap_copy_or_reroute(state, backend, entry.pending);
                    if let Some(pin) = entry.pin {
                        backend.release_present_source(pin);
                    }
                    Some(ok)
                }
                // Source-ready but not yet msc-due (Task 7 Step 2 combined
                // vector): stays parked, now purely on msc-due — the WAIT
                // pin still drops below (unconditional, as always), but
                // the ENTRY pin stays held until the due-pass executes it.
                crate::present_scheduler::MscDue::Park => {
                    let reason = if flip_in_flight { "flip_in_flight" } else { "future" };
                    log::debug!(
                        target: "present_pace",
                        "PACE-INSTR t={} pid={} stage=parked_msc reason={reason} eff={:?} clock_msc={clock_msc}",
                        pace_instr_ms(), entry.pending.present_id, entry.pending.effective_target_msc
                    );
                    let pid = entry.pending.present_id;
                    entry.source_ready = true;
                    entry.wait_id = None;
                    state.present_pending_exec.insert(pid, entry);
                    None
                }
            }
        });
        // The WAIT pin is distinct from the entry pin above and is
        // released exactly here, as today — unconditionally, whether or
        // not the entry was still present (an unknown-id backend report
        // is a no-op guard).
        backend.finish_present_source_wait(wait_id);
        match outcome {
            Some(Some(true)) => backend.mark_dirty(),
            // Failure already logged + rerouted (by-XID release, IdleNotify,
            // parked/inline completion) inside
            // `execute_present_pixmap_copy_or_reroute`.
            Some(Some(false)) => {}
            Some(None) => {}
            None => log::warn!("backend reported unknown Present source wait id {wait_id}"),
        }
    }
}

/// Full-server shutdown: release every still-parked (pre-copy) entry in
/// the unified pending-present store. Mirrors the window-destroy teardown
/// loop's by-XID release exactly (same wake mechanism, same two distinct
/// pin releases) but with no window filter — every entry is stale once
/// there is no more request/vblank drain left to resolve it. Called once,
/// core-side, before the listening socket is torn down (yserver's
/// `lib.rs`, next to `signal_all_retained_present_wakes` which does the
/// analogous flush for the *post*-copy retained-wake population).
pub fn shutdown_drain_present_pending_exec(state: &mut ServerState, backend: &mut dyn Backend) {
    for (_, entry) in std::mem::take(&mut state.present_pending_exec) {
        match &entry.pending.wake {
            crate::backend::PresentWake::Pixmap { idle_fence_xid } if *idle_fence_xid != 0 => {
                if let Err(e) = backend.dri3_trigger_fence(*idle_fence_xid) {
                    log::warn!(
                        "PRESENT shutdown: trigger idle fence 0x{idle_fence_xid:x} failed: {e}"
                    );
                }
            }
            crate::backend::PresentWake::PixmapSynced {
                release,
                release_syncobj,
                release_value,
            } => {
                if let Err(e) = release.signal(*release_value) {
                    log::warn!(
                        "PRESENT shutdown: signal release syncobj 0x{release_syncobj:x}@\
                         {release_value} failed: {e}"
                    );
                }
            }
            _ => {}
        }
        if let Some(wid) = entry.wait_id {
            backend.finish_present_source_wait(wid);
        }
        if let Some(pin) = entry.pin {
            backend.release_present_source(pin);
        }
    }
    state.present_wait_to_id.clear();
}

#[allow(clippy::too_many_arguments)]
fn present_remainder_invalid(divisor: u64, remainder: u64) -> bool {
    (divisor == 0 && remainder != 0) || (divisor != 0 && remainder >= divisor)
}

fn present_remainder_error_value(remainder: u64) -> u32 {
    u32::try_from(remainder & u64::from(u32::MAX)).expect("masked to CARD32")
}

/// Validate the resource/option/pacing tail shared by PresentPixmap and
/// PresentPixmapSynced, in Xorg `proc_present_pixmap_common` order. Drawable
/// and depth checks precede this helper; PixmapSynced's syncobj/point checks
/// precede the entire common path. `None` fence arguments model Synced's two
/// protocol-level `None` fences, while `Some(0)` is PresentPixmap's explicit
/// `None` XID.
#[allow(clippy::too_many_arguments)]
fn present_pixmap_common_validation_error(
    state: &ServerState,
    valid_region: u32,
    update_region: u32,
    target_crtc: u32,
    wait_fence: Option<u32>,
    idle_fence: Option<u32>,
    options: u32,
    divisor: u64,
    remainder: u64,
) -> Option<(u8, u32)> {
    if valid_region != 0 && !state.xfixes_regions.contains_key(&valid_region) {
        return Some((XFIXES_BAD_REGION, valid_region));
    }
    if update_region != 0 && !state.xfixes_regions.contains_key(&update_region) {
        return Some((XFIXES_BAD_REGION, update_region));
    }
    if target_crtc != 0 && !present_crtc_exists(state, target_crtc) {
        return Some((RANDR_BAD_CRTC, target_crtc));
    }
    if let Some(wait_fence) = wait_fence
        && wait_fence != 0
        && !state.sync_fences.contains_key(&wait_fence)
    {
        return Some((SYNC_BAD_FENCE, wait_fence));
    }
    if let Some(idle_fence) = idle_fence
        && idle_fence != 0
        && !state.sync_fences.contains_key(&idle_fence)
    {
        return Some((SYNC_BAD_FENCE, idle_fence));
    }
    if options & !PRESENT_ALL_OPTIONS != 0 {
        return Some((x11::error::BAD_VALUE, options));
    }
    if present_remainder_invalid(divisor, remainder) {
        return Some((
            x11::error::BAD_VALUE,
            present_remainder_error_value(remainder),
        ));
    }
    None
}

pub(super) fn handle_present_request(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    use yserver_protocol::x11::{ClientByteOrder, present as x11present};
    const PRESENT_MAJOR_OPCODE: u8 = 145;
    let byte_order = state
        .clients
        .get(&client_id.0)
        .map_or(ClientByteOrder::LittleEndian, |c| c.byte_order);
    let minor = header.data;
    debug!(
        "client {} #{} PRESENT dispatch minor={} body.len()={}",
        client_id.0,
        sequence.0,
        minor,
        body.len()
    );
    match minor {
        x11present::QUERY_VERSION => {
            let _ = x11present::parse_query_version(body);
            debug!(
                "client {} #{} PRESENT::QueryVersion -> {}.{}",
                client_id.0,
                sequence.0,
                x11present::MAJOR_VERSION,
                x11present::MINOR_VERSION
            );
            let reply = x11present::encode_query_version_reply(
                byte_order,
                sequence,
                x11present::MAJOR_VERSION,
                x11present::MINOR_VERSION,
            );
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            let _byte_order = client.byte_order;
            return Ok(write_to_client(client, client_id, &reply));
        }
        x11present::QUERY_CAPABILITIES => {
            let target = x11present::parse_query_capabilities(body).unwrap_or(0);
            let caps = backend.present_capabilities(target).encode();
            debug!(
                "client {} #{} PRESENT::QueryCapabilities target=0x{target:x} -> 0x{caps:x}",
                client_id.0, sequence.0
            );
            let reply = x11present::encode_query_capabilities_reply(byte_order, sequence, caps);
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            let _byte_order = client.byte_order;
            return Ok(write_to_client(client, client_id, &reply));
        }
        x11present::SELECT_INPUT => {
            if let Some(req) = x11present::parse_select_input(body) {
                debug!(
                    "client {} #{} PRESENT::SelectInput eid=0x{:x} window=0x{:x} mask=0x{:x}",
                    client_id.0, sequence.0, req.eid, req.window, req.event_mask
                );
                state.present_event_selections.insert(
                    req.eid,
                    crate::server::PresentEventSelection {
                        owner: client_id,
                        window: ResourceId(req.window),
                        event_mask: req.event_mask,
                    },
                );
            } else {
                debug!(
                    "client {} #{} PRESENT::SelectInput parse failed body_len={}",
                    client_id.0,
                    sequence.0,
                    body.len()
                );
            }
        }
        x11present::PIXMAP => {
            let Some(req) = x11present::parse_pixmap(body) else {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_LENGTH,
                    0,
                    u16::from(header.data),
                    PRESENT_MAJOR_OPCODE,
                );
            };
            // Phase 4.2.3: wait_fence / idle_fence are accepted. The
            // dispatcher mirrors the result onto state.sync_fences via
            // the XSync resource table for QueryFence / TriggerFence.
            let window_exists = state.resources.window(ResourceId(req.window)).is_some();
            let pixmap_exists = state.resources.pixmap(ResourceId(req.pixmap)).is_some();
            let src = state.resources.host_drawable_target(ResourceId(req.pixmap));
            let dst = state.resources.host_drawable_target(ResourceId(req.window));
            let dst_window_host_xid = state
                .resources
                .window(ResourceId(req.window))
                .and_then(|window| window.host_xid)
                .map(|host_xid| host_xid.as_raw());
            if !window_exists {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_WINDOW,
                    req.window,
                    u16::from(header.data),
                    PRESENT_MAJOR_OPCODE,
                );
            }
            if !pixmap_exists {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_DRAWABLE,
                    req.pixmap,
                    u16::from(header.data),
                    PRESENT_MAJOR_OPCODE,
                );
            }
            if let (
                Some(crate::resources::HostDrawableTarget::Pixmap {
                    depth: src_depth, ..
                }),
                Some(dst),
            ) = (&src, &dst)
                && *src_depth != dst.depth()
            {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_MATCH,
                    req.pixmap,
                    u16::from(header.data),
                    PRESENT_MAJOR_OPCODE,
                );
            }
            // Keep the full Xorg common validation path read-only. A rejected
            // request must neither bind this window's Present domain nor
            // allocate a new lifetime generation.
            if let Some((error_code, error_value)) = present_pixmap_common_validation_error(
                state,
                req.valid,
                req.update,
                req.target_crtc,
                Some(req.wait_fence),
                Some(req.idle_fence),
                req.options,
                req.divisor,
                req.remainder,
            ) {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    error_code,
                    error_value,
                    u16::from(header.data),
                    PRESENT_MAJOR_OPCODE,
                );
            }
            let domain =
                match select_present_domain(state, backend, req.window, req.target_crtc, false) {
                    Ok(domain) => domain,
                    Err(crtc) => {
                        return emit_x11_error_with_minor(
                            state,
                            client_id,
                            sequence,
                            RANDR_BAD_CRTC,
                            crtc,
                            u16::from(header.data),
                            PRESENT_MAJOR_OPCODE,
                        );
                    }
                };
            // Per design §4 AsyncMayTear silent-clear: mask the bit
            // off here when the cap isn't advertised. Computed up
            // front so the deferred-completion enqueue (inside the
            // `if let` below) uses the masked options.
            let caps = backend.present_capabilities(req.window);
            let masked_options = if caps.async_may_tear {
                req.options
            } else {
                const PRESENT_OPTION_ASYNC_MAY_TEAR: u32 = 0x10;
                req.options & !PRESENT_OPTION_ASYNC_MAY_TEAR
            };
            // INSTRUMENTATION (2026-05-29, wait_fence hypothesis check): on a
            // BLANK Firefox run, do content-sized presents even arrive, and is
            // the wait_fence un-triggered when we copy? Logs src kind+SIZE,
            // wait_fence + our server-side triggered bit (caveat: that bit is
            // set by SYNC TriggerFence; Mesa may trigger the xshmfence
            // out-of-band, so `wait_triggered` reflects only what the server
            // observed, not the true GPU/xshmfence state). Grep "PRESENT-INSTR".
            {
                let (src_kind, sw, sh) = match &src {
                    Some(crate::resources::HostDrawableTarget::Pixmap {
                        width, height, ..
                    }) => ("pixmap", *width, *height),
                    Some(_) => ("non-pixmap", 0, 0),
                    None => ("unresolved", 0, 0),
                };
                let wait_triggered = if req.wait_fence == 0 {
                    "no-fence".to_string()
                } else {
                    match state.sync_fences.get(&req.wait_fence) {
                        Some(f) => format!("{}", f.triggered),
                        None => "unknown-fence".to_string(),
                    }
                };
                log::debug!(
                    "PRESENT-INSTR client {} #{} Pixmap serial={} src={} {}x{} \
                     window=0x{:x} wait_fence=0x{:x} wait_triggered={} idle_fence=0x{:x}",
                    client_id.0,
                    sequence.0,
                    req.serial,
                    src_kind,
                    sw,
                    sh,
                    req.window,
                    req.wait_fence,
                    wait_triggered,
                    req.idle_fence,
                );
            }
            // Diagnostic: the copy below is gated on the source resolving to
            // a usable host pixmap and the window resolving to a host target.
            // The pixmap/window resource-existence checks already ran above
            // (BadDrawable/BadWindow), so reaching here unresolved means the
            // resource exists but has no backend host xid — the classic
            // failed-DRI3-dmabuf-import shape (e.g. PixmapFromBuffers rejected
            // a multi-plane/modifier buffer). Without this warn the present is
            // silently dropped and the window shows stale content.
            if !matches!(
                src,
                Some(crate::resources::HostDrawableTarget::Pixmap { .. })
            ) || dst.is_none()
            {
                log::warn!(
                    "client {} #{} Present::Pixmap pixmap=0x{:x} window=0x{:x}: dropping present \
                     copy — src_resolved_as_host_pixmap={} dst_resolved={} (window keeps stale \
                     content; a preceding DRI3 dmabuf import likely failed)",
                    client_id.0,
                    sequence.0,
                    req.pixmap,
                    req.window,
                    matches!(
                        src,
                        Some(crate::resources::HostDrawableTarget::Pixmap { .. })
                    ),
                    dst.is_some(),
                );
            }
            if let (
                Some(crate::resources::HostDrawableTarget::Pixmap {
                    host_xid,
                    width,
                    height,
                    depth: _src_depth,
                    ..
                }),
                Some(dst),
            ) = (src, dst)
            {
                // Keep the destination's window identity for the paint. A
                // redirected window's HostDrawableTarget names its backing
                // pixmap, which is useful for completion fencing but loses
                // ClipByChildren and hierarchy stacking. The backend resolves
                // this window xid to that same backing after computing the
                // correct window clip.
                let paint_dst_host_xid = dst_window_host_xid.unwrap_or_else(|| dst.host_xid());
                // DIAG (2026-07-08, MATE compositor slow-drag smear): log what the
                // compositor actually presents into the COW so we can tell whether
                // the `update` region is a full frame or thin drag slivers, and
                // whether the slivers tile the swept region. Env-gated
                // (YSERVER_PRESENT_TRACE=1) so it is zero-cost / no timing
                // perturbation when off. Grep "PRESENT-UPDATE". See
                // docs/superpowers/findings/2026-07-08-mate-compositor-drag-smear-diagnosis.md
                {
                    use std::sync::OnceLock;
                    static TRACE: OnceLock<bool> = OnceLock::new();
                    if *TRACE.get_or_init(|| std::env::var_os("YSERVER_PRESENT_TRACE").is_some()) {
                        let (nrects, bbox): (isize, Option<(i32, i32, i32, i32)>) =
                            if req.update == 0 {
                                (-1, None) // full copy: no update region
                            } else if let Some(region) = state.xfixes_regions.get(&req.update) {
                                let bbox = region.rects.iter().fold(
                                    None,
                                    |acc: Option<(i32, i32, i32, i32)>, r| {
                                        let x0 = i32::from(r.x);
                                        let y0 = i32::from(r.y);
                                        let x1 = x0 + i32::from(r.width);
                                        let y1 = y0 + i32::from(r.height);
                                        Some(match acc {
                                            None => (x0, y0, x1, y1),
                                            Some((ax0, ay0, ax1, ay1)) => {
                                                (ax0.min(x0), ay0.min(y0), ax1.max(x1), ay1.max(y1))
                                            }
                                        })
                                    },
                                );
                                (region.rects.len() as isize, bbox)
                            } else {
                                (-2, None) // update id set but region not found → full copy
                            };
                        log::info!(
                            "PRESENT-UPDATE window=0x{:x} x_off={} y_off={} pixmap={}x{} \
                             update_id=0x{:x} nrects={} bbox={:?}",
                            req.window,
                            req.x_off,
                            req.y_off,
                            width,
                            height,
                            req.update,
                            nrects,
                            bbox,
                        );
                    }
                }
                // Snapshot the region now: XFixes regions are mutable resources,
                // while an imported producer may keep this request parked.
                let update_rects = if req.update == 0 {
                    None
                } else {
                    state
                        .xfixes_regions
                        .get(&req.update)
                        .map(|region| region.rects.clone())
                };
                let present_id = state.next_present_id();
                let window_generation = state.present_window_generation(req.window);
                let effective_target_msc = effective_present_target_raw(
                    domain,
                    req.target_msc,
                    req.divisor,
                    req.remainder,
                    masked_options,
                );
                log::debug!(
                    target: "present_pace",
                    "PACE-INSTR t={} pid={} client={} serial={} stage=request kind=pixmap target_msc={} div={} rem={} kernel_msc={} eff={:?}",
                    pace_instr_ms(), present_id, client_id.0, req.serial, req.target_msc, req.divisor, req.remainder, domain.raw_msc, effective_target_msc
                );
                let pending = PendingPresentPixmap {
                    origin,
                    client_id,
                    request: PendingPresentRequest::Pixmap(req.clone()),
                    wake: crate::backend::PresentWake::Pixmap {
                        idle_fence_xid: req.idle_fence,
                    },
                    masked_options,
                    src_host_xid: host_xid.as_raw(),
                    paint_dst_host_xid,
                    completion_dst_host_xid: dst.host_xid(),
                    src_width: width,
                    src_height: height,
                    update_rects,
                    present_id,
                    window_generation,
                    crtc_id: domain.crtc_id,
                    crtc_epoch: domain.crtc_epoch,
                    msc_offset: domain.msc_offset,
                    effective_target_msc,
                };
                // Arm before scrap — see
                // `arm_present_pixmap_source_then_supersede` doc comment
                // for why the order matters (a fallible arm after scrap
                // would destroy a frame nothing replaces).
                let armed = arm_present_pixmap_source_then_supersede(state, backend, &pending)?;
                match armed {
                    crate::backend::PresentSourceWait::Ready => {
                        arrival_execute_or_park_present_pixmap(state, backend, present_id, pending);
                    }
                    crate::backend::PresentSourceWait::Deferred(wait_id) => {
                        log::debug!(target: "present_pace", "PACE-INSTR t={} pid={} stage=source_deferred wait_id={}", pace_instr_ms(), pending.present_id, wait_id);
                        let pin = backend.pin_present_source(pending.src_host_xid);
                        if state
                            .present_wait_to_id
                            .insert(wait_id, present_id)
                            .is_some()
                        {
                            log::warn!("backend reused live Present source wait id {wait_id}");
                        }
                        state.present_pending_exec.insert(
                            present_id,
                            crate::server::PendingPresentEntry {
                                pending,
                                source_ready: false,
                                wait_id: Some(wait_id),
                                pin,
                            },
                        );
                    }
                }
            }
            debug!(
                "client {} #{} PRESENT::Pixmap serial={} notifies={}",
                client_id.0,
                sequence.0,
                req.serial,
                req.notifies.len()
            );
        }
        x11present::NOTIFY_MSC => {
            if let Some(req) = x11present::parse_notify_msc(body) {
                if state.resources.window(ResourceId(req.window)).is_none() {
                    return emit_x11_error_with_minor(
                        state,
                        client_id,
                        sequence,
                        x11::error::BAD_WINDOW,
                        req.window,
                        u16::from(header.data),
                        PRESENT_MAJOR_OPCODE,
                    );
                }
                // Xorg resolves the target window first. Invalid residue is
                // BadValue only once BadWindow has been ruled out.
                if present_remainder_invalid(req.divisor, req.remainder) {
                    return emit_x11_error_with_minor(
                        state,
                        client_id,
                        sequence,
                        x11::error::BAD_VALUE,
                        present_remainder_error_value(req.remainder),
                        u16::from(header.data),
                        PRESENT_MAJOR_OPCODE,
                    );
                }
                let domain = select_present_domain(state, backend, req.window, 0, true)
                    .expect("implicit Present CRTC selection cannot fail");
                let raw_target_msc = req.target_msc.wrapping_add(domain.msc_offset);
                // Vblank-paced clock: the current MSC is the real kernel
                // value from the last pageflip (mirrored into ServerState).
                // If already satisfied (and we have a real flip to time
                // against), complete now; otherwise PARK the request and let
                // a future pageflip fire it (drain_present_completions). On
                // master these were dropped when unsatisfied, which froze a
                // compositor's `present` frame clock after one frame.
                let current_msc = domain.raw_msc;
                let clockless =
                    domain.crtc_id == 0 || !present_crtc_is_enabled(state, domain.crtc_id);
                let satisfied = clockless
                    || (current_msc > 0
                        && notify_msc_satisfied(
                            current_msc,
                            raw_target_msc,
                            req.divisor,
                            req.remainder,
                        ));
                if satisfied {
                    fire_present_notify_msc_complete_events(
                        state,
                        byte_order,
                        PRESENT_MAJOR_OPCODE,
                        req.window,
                        req.serial,
                        current_msc.wrapping_sub(domain.msc_offset),
                        domain.raw_ust,
                    );
                } else {
                    state
                        .present_pending_msc
                        .push(crate::server::PendingNotifyMsc {
                            owner: client_id,
                            window: req.window,
                            crtc_id: domain.crtc_id,
                            crtc_epoch: domain.crtc_epoch,
                            msc_offset: domain.msc_offset,
                            serial: req.serial,
                            target_msc: raw_target_msc,
                            divisor: req.divisor,
                            remainder: req.remainder,
                            byte_order,
                        });
                }
            }
            debug!("client {} #{} PRESENT::NotifyMSC", client_id.0, sequence.0);
        }
        x11present::PIXMAP_SYNCED => {
            let Some(req) = x11present::parse_pixmap_synced(body) else {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_LENGTH,
                    0,
                    u16::from(header.data),
                    PRESENT_MAJOR_OPCODE,
                );
            };
            let acquire_known = req.acquire_syncobj != 0
                && backend.dri3_syncobj_owned(client_id, req.acquire_syncobj);
            if !acquire_known {
                // Mirror Xorg's bad-value choice: VERIFY_DRI3_SYNCOBJ
                // (dri3/dri3.h:51-56) sets client->errorValue = <xid> when the
                // syncobj lookup fails. This is the error ARGUMENT, not the
                // error CODE — it is NOT part of the declared divergence
                // (which covers the code only).
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_VALUE,
                    req.acquire_syncobj,
                    u16::from(header.data),
                    PRESENT_MAJOR_OPCODE,
                );
            }
            let release_handle = if req.release_syncobj != 0
                && backend.dri3_syncobj_owned(client_id, req.release_syncobj)
            {
                backend.dri3_syncobj_handle(req.release_syncobj)
            } else {
                None
            };
            let Some(release_handle) = release_handle else {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_VALUE,
                    req.release_syncobj,
                    u16::from(header.data),
                    PRESENT_MAJOR_OPCODE,
                );
            };
            if req.acquire_value == 0
                || req.release_value == 0
                || (req.acquire_syncobj == req.release_syncobj
                    && req.acquire_value >= req.release_value)
            {
                // Point/ordering failures in Xorg (present_request.c:299-301)
                // leave errorValue at zero.
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_VALUE,
                    0,
                    u16::from(header.data),
                    PRESENT_MAJOR_OPCODE,
                );
            }
            let window_exists = state.resources.window(ResourceId(req.window)).is_some();
            let pixmap_exists = state.resources.pixmap(ResourceId(req.pixmap)).is_some();
            let src = state.resources.host_drawable_target(ResourceId(req.pixmap));
            let dst = state.resources.host_drawable_target(ResourceId(req.window));
            let dst_window_host_xid = state
                .resources
                .window(ResourceId(req.window))
                .and_then(|window| window.host_xid)
                .map(|host_xid| host_xid.as_raw());
            if !window_exists {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_WINDOW,
                    req.window,
                    u16::from(header.data),
                    PRESENT_MAJOR_OPCODE,
                );
            }
            if !pixmap_exists {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_DRAWABLE,
                    req.pixmap,
                    u16::from(header.data),
                    PRESENT_MAJOR_OPCODE,
                );
            }
            // Keep domain binding after every fallible drawable validation:
            // a rejected request must not change the CRTC that a later
            // NotifyMSC remembers for this window.
            if let (
                Some(crate::resources::HostDrawableTarget::Pixmap {
                    depth: src_depth, ..
                }),
                Some(dst),
            ) = (&src, &dst)
                && *src_depth != dst.depth()
            {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_MATCH,
                    req.pixmap,
                    u16::from(header.data),
                    PRESENT_MAJOR_OPCODE,
                );
            }
            if let Some((error_code, error_value)) = present_pixmap_common_validation_error(
                state,
                req.valid,
                req.update,
                req.target_crtc,
                None,
                None,
                req.options,
                req.divisor,
                req.remainder,
            ) {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    error_code,
                    error_value,
                    u16::from(header.data),
                    PRESENT_MAJOR_OPCODE,
                );
            }
            let caps = backend.present_capabilities(req.window);
            let masked_options = if caps.async_may_tear {
                req.options
            } else {
                const PRESENT_OPTION_ASYNC_MAY_TEAR: u32 = 0x10;
                req.options & !PRESENT_OPTION_ASYNC_MAY_TEAR
            };
            let domain =
                match select_present_domain(state, backend, req.window, req.target_crtc, false) {
                    Ok(domain) => domain,
                    Err(crtc) => {
                        return emit_x11_error_with_minor(
                            state,
                            client_id,
                            sequence,
                            RANDR_BAD_CRTC,
                            crtc,
                            u16::from(header.data),
                            PRESENT_MAJOR_OPCODE,
                        );
                    }
                };
            // INSTRUMENTATION (2026-05-29): synced variant uses acquire_syncobj
            // (explicit-sync timeline) as its wait. Log src kind+SIZE + the
            // acquire/release syncobj so a blank run shows whether content-sized
            // synced presents arrive and what sync points they carry.
            // Grep "PRESENT-INSTR".
            {
                let (src_kind, sw, sh) = match &src {
                    Some(crate::resources::HostDrawableTarget::Pixmap {
                        width, height, ..
                    }) => ("pixmap", *width, *height),
                    Some(_) => ("non-pixmap", 0, 0),
                    None => ("unresolved", 0, 0),
                };
                log::debug!(
                    "PRESENT-INSTR client {} #{} PixmapSynced serial={} src={} {}x{} \
                     window=0x{:x} acquire_syncobj=0x{:x} acquire_value={} \
                     release_syncobj=0x{:x} release_value={}",
                    client_id.0,
                    sequence.0,
                    req.serial,
                    src_kind,
                    sw,
                    sh,
                    req.window,
                    req.acquire_syncobj,
                    req.acquire_value,
                    req.release_syncobj,
                    req.release_value,
                );
            }
            // Same silent-drop diagnostic as the Present::Pixmap path: if the
            // source pixmap didn't resolve to a backend host pixmap (failed
            // DRI3 dmabuf import) or the window didn't resolve, the copy below
            // is skipped and the window keeps stale content.
            if !matches!(
                src,
                Some(crate::resources::HostDrawableTarget::Pixmap { .. })
            ) || dst.is_none()
            {
                log::warn!(
                    "client {} #{} Present::PixmapSynced pixmap=0x{:x} window=0x{:x}: dropping \
                     present copy — src_resolved_as_host_pixmap={} dst_resolved={} (window keeps \
                     stale content; a preceding DRI3 dmabuf import likely failed)",
                    client_id.0,
                    sequence.0,
                    req.pixmap,
                    req.window,
                    matches!(
                        src,
                        Some(crate::resources::HostDrawableTarget::Pixmap { .. })
                    ),
                    dst.is_some(),
                );
            }
            let queued_copy = if let (
                Some(crate::resources::HostDrawableTarget::Pixmap {
                    host_xid,
                    width,
                    height,
                    depth: src_depth,
                    ..
                }),
                Some(dst),
            ) = (src, dst)
            {
                let paint_dst_host_xid = dst_window_host_xid.unwrap_or_else(|| dst.host_xid());
                if src_depth != dst.depth() {
                    false
                } else {
                    let update_rects = if req.update == 0 {
                        None
                    } else {
                        state
                            .xfixes_regions
                            .get(&req.update)
                            .map(|region| region.rects.clone())
                    };
                    let present_id = state.next_present_id();
                    let window_generation = state.present_window_generation(req.window);
                    let effective_target_msc = effective_present_target_raw(
                        domain,
                        req.target_msc,
                        req.divisor,
                        req.remainder,
                        masked_options,
                    );
                    log::debug!(
                        target: "present_pace",
                        "PACE-INSTR t={} pid={} client={} serial={} stage=request kind=synced target_msc={} div={} rem={} kernel_msc={} eff={:?} acquire=0x{:x}@{} release=0x{:x}@{}",
                        pace_instr_ms(), present_id, client_id.0, req.serial, req.target_msc, req.divisor, req.remainder, domain.raw_msc, effective_target_msc,
                        req.acquire_syncobj, req.acquire_value, req.release_syncobj, req.release_value,
                    );
                    let pending = PendingPresentPixmap {
                        origin,
                        client_id,
                        request: PendingPresentRequest::PixmapSynced(req.clone()),
                        wake: crate::backend::PresentWake::PixmapSynced {
                            release: release_handle.clone(),
                            release_syncobj: req.release_syncobj,
                            release_value: req.release_value,
                        },
                        masked_options,
                        src_host_xid: host_xid.as_raw(),
                        paint_dst_host_xid,
                        completion_dst_host_xid: dst.host_xid(),
                        src_width: width,
                        src_height: height,
                        update_rects,
                        present_id,
                        window_generation,
                        crtc_id: domain.crtc_id,
                        crtc_epoch: domain.crtc_epoch,
                        msc_offset: domain.msc_offset,
                        effective_target_msc,
                    };
                    // Arm before scrap — see
                    // `arm_present_pixmap_synced_source_then_supersede`
                    // doc comment for why the order matters (a fallible
                    // arm after scrap would destroy a frame nothing
                    // replaces).
                    let armed = arm_present_pixmap_synced_source_then_supersede(
                        state,
                        backend,
                        req.acquire_syncobj,
                        req.acquire_value,
                        &pending,
                    )?;
                    match armed {
                        crate::backend::PresentSourceWait::Ready => {
                            arrival_execute_or_park_present_pixmap(
                                state, backend, present_id, pending,
                            );
                        }
                        crate::backend::PresentSourceWait::Deferred(wait_id) => {
                            log::debug!(target: "present_pace", "PACE-INSTR t={} pid={} stage=acquire_deferred wait_id={} syncobj=0x{:x} value={}", pace_instr_ms(), pending.present_id, wait_id, req.acquire_syncobj, req.acquire_value);
                            let pin = backend.pin_present_source(pending.src_host_xid);
                            if state
                                .present_wait_to_id
                                .insert(wait_id, present_id)
                                .is_some()
                            {
                                log::warn!("backend reused live Present syncobj wait id {wait_id}");
                            }
                            state.present_pending_exec.insert(
                                present_id,
                                crate::server::PendingPresentEntry {
                                    pending,
                                    source_ready: false,
                                    wait_id: Some(wait_id),
                                    pin,
                                },
                            );
                        }
                    }
                    true
                }
            } else {
                false
            };
            if !queued_copy {
                log::debug!(
                    "PRESENT::PixmapSynced copy path was not queued \
                     (window=0x{:x} pixmap=0x{:x})",
                    req.window,
                    req.pixmap
                );
            }
            debug!(
                "client {} #{} PRESENT::PixmapSynced serial={} acquire=0x{:x}@{} release=0x{:x}@{}",
                client_id.0,
                sequence.0,
                req.serial,
                req.acquire_syncobj,
                req.acquire_value,
                req.release_syncobj,
                req.release_value
            );
        }
        _ => {
            return emit_x11_error_with_minor(
                state,
                client_id,
                sequence,
                x11::error::BAD_REQUEST,
                0,
                u16::from(minor),
                header.opcode,
            );
        }
    }
    Ok(RequestOutcome::Handled)
}

fn notify_msc_satisfied(current_msc: u64, target_msc: u64, divisor: u64, remainder: u64) -> bool {
    if crate::present_scheduler::msc_is_after(target_msc, current_msc) {
        return false;
    }
    if divisor == 0 {
        return true;
    }
    current_msc % divisor == remainder
}

/// Fan out `CompleteNotify { mode: Copy }` and `IdleNotify` to every
/// `present_event_selections` entry that subscribed to the window
/// with the matching event-mask bit. Phase 4.2.3 design §3.3.2.
pub fn fire_present_completion_events(
    state: &mut ServerState,
    event: &crate::backend::CompletedPresentEvent,
) {
    let cached = cached_present_crtc_clock(state, event.crtc_id, event.crtc_epoch);
    let raw = event.completion_clock.unwrap_or({
        if cached.completion.msc > 0 {
            cached.completion
        } else {
            crate::backend::PresentClockSample {
                msc: cached.msc,
                ust: cached.ust,
                source: crate::backend::PresentClockSource::Immediate,
            }
        }
    });
    let clock = present_wire_clock(raw, event.msc_offset);
    fire_present_completion_events_at(
        state,
        event,
        clock,
        yserver_protocol::x11::present::COMPLETE_MODE_COPY,
        true,
        true,
    );
}

pub(crate) fn fire_present_completion_events_at(
    state: &mut ServerState,
    event: &crate::backend::CompletedPresentEvent,
    clock: crate::backend::PresentClockSample,
    mode: u8,
    emit_idle: bool,
    emit_complete: bool,
) {
    use crate::backend::PresentWake;
    use yserver_protocol::x11::present as x11present;

    /// PRESENT major opcode.
    const PRESENT_MAJOR_OPCODE: u8 = 145;
    /// `PresentEventMaskCompleteNotify` per `presentproto`.
    const COMPLETE_NOTIFY_MASK: u32 = 0x2;
    /// `PresentEventMaskIdleNotify`.
    const IDLE_NOTIFY_MASK: u32 = 0x4;

    let window = ResourceId(event.dst_host_xid);
    // Report the provenance-aware display sample selected by the caller.
    // Pixmap completion deliberately excludes standalone sequence events
    // observed while scanout is active, while NotifyMSC remains driven by
    // the general vblank clock. (Was: a per-window software counter + ust=0,
    // which picom rejects as "Invalid PresentCompleteNotify event".)
    let current_msc = clock.msc;
    let pixmap_xid = event.host_xid;
    let idle_fence = match &event.wake {
        PresentWake::Pixmap { idle_fence_xid } => *idle_fence_xid,
        PresentWake::PixmapSynced {
            release_syncobj, ..
        } => *release_syncobj,
    };

    // Gather the matching (eid, owning client, mask) tuples up front
    // so we can iterate state.clients without holding a borrow on
    // present_event_selections.
    let mut targets: Vec<(u32, ClientId, u32)> = Vec::new();
    for (eid, sel) in &state.present_event_selections {
        if sel.window == window {
            targets.push((*eid, sel.owner, sel.event_mask));
        }
    }
    debug!(
        "PRESENT events: window=0x{:x} serial={} pixmap=0x{:x} targets={:?}",
        window.0,
        event.serial,
        pixmap_xid,
        targets
            .iter()
            .map(|(e, c, m)| format!("eid=0x{e:x}/client={}/mask=0x{m:x}", c.0))
            .collect::<Vec<_>>()
    );

    for (eid, owner, mask) in targets {
        let Some(client) = state.clients.get_mut(&owner.0) else {
            continue;
        };
        // byte_order is sourced from the OWNER client (the one
        // receiving the event), not the caller. Fixes a latent bug
        // where events to owners of different endianness than the
        // submitting client were mis-encoded.
        let byte_order = client.byte_order;
        let seq = SequenceNumber(
            client
                .last_sequence
                .load(std::sync::atomic::Ordering::Relaxed),
        );
        // Per Xorg's present_execute_copy: IdleNotify fires *first*
        // (the pixmap is idle as soon as the GPU finishes reading,
        // which on our synchronous CopyArea path is immediately),
        // CompleteNotify fires on vblank afterwards. Mesa's
        // loader_dri3 expects this order — flipping it makes
        // vkAcquireNextImage hang on the second frame.
        if emit_idle && mask & IDLE_NOTIFY_MASK != 0 {
            let ev = x11present::encode_idle_notify(
                byte_order,
                seq,
                PRESENT_MAJOR_OPCODE,
                eid,
                window.0,
                event.serial,
                pixmap_xid,
                idle_fence,
            );
            let outcome = write_to_client(client, owner, &ev);
            debug!(
                "PRESENT IdleNotify -> client {} eid=0x{eid:x} pixmap=0x{:x} fence=0x{idle_fence:x} ({} bytes) outcome={:?} outbound_len={}",
                owner.0,
                pixmap_xid,
                ev.len(),
                outcome,
                client.outbound.len(),
            );
        }
        if emit_complete && mask & COMPLETE_NOTIFY_MASK != 0 {
            let ev = x11present::encode_complete_notify(
                byte_order,
                seq,
                PRESENT_MAJOR_OPCODE,
                eid,
                window.0,
                event.serial,
                x11present::COMPLETE_KIND_PIXMAP,
                mode,
                clock.ust,
                current_msc,
            );
            debug!(
                "PRESENT CompleteNotify -> client {} eid=0x{eid:x} ({} bytes)",
                owner.0,
                ev.len()
            );
            let _ = write_to_client(client, owner, &ev);
        }
    }
}

/// Emit `Present::ConfigureNotify` to every client that selected
/// `EVENT_MASK_CONFIGURE_NOTIFY` on `window`. Triggered from
/// `handle_configure_window` whenever the window's geometry changes
/// (size primarily — Mesa keys buffer reallocation on
/// `pixmap_{width,height}` here). Mirrors the iteration shape of
/// `fire_present_completion_events`.
pub(crate) fn fire_present_configure_notify_for_window(
    state: &mut ServerState,
    window_id: ResourceId,
    geometry: yserver_protocol::x11::Geometry,
) {
    use yserver_protocol::x11::present as x11present;
    const PRESENT_MAJOR_OPCODE: u8 = 145;

    let mut targets: Vec<(u32, ClientId, u32)> = Vec::new();
    for (eid, sel) in &state.present_event_selections {
        if sel.window == window_id {
            targets.push((*eid, sel.owner, sel.event_mask));
        }
    }
    for (eid, owner, mask) in targets {
        if mask & x11present::EVENT_MASK_CONFIGURE_NOTIFY == 0 {
            continue;
        }
        let Some(client) = state.clients.get_mut(&owner.0) else {
            continue;
        };
        let byte_order = client.byte_order;
        let seq = SequenceNumber(
            client
                .last_sequence
                .load(std::sync::atomic::Ordering::Relaxed),
        );
        let ev = x11present::encode_configure_notify(
            byte_order,
            seq,
            PRESENT_MAJOR_OPCODE,
            eid,
            window_id.0,
            geometry.x,
            geometry.y,
            geometry.width,
            geometry.height,
            0,
            0,
            geometry.width,
            geometry.height,
            0,
        );
        let _ = write_to_client(client, owner, &ev);
        debug!(
            "PRESENT ConfigureNotify -> client {} eid=0x{eid:x} window=0x{:x} \
             geom=({},{} {}x{})",
            owner.0, window_id.0, geometry.x, geometry.y, geometry.width, geometry.height,
        );
    }
}

fn fire_present_notify_msc_complete_events(
    state: &mut ServerState,
    byte_order: yserver_protocol::x11::ClientByteOrder,
    extension_major: u8,
    window: u32,
    serial: u32,
    current_msc: u64,
    ust: u64,
) {
    use yserver_protocol::x11::present as x11present;
    const COMPLETE_NOTIFY_MASK: u32 = 0x2;

    let window = ResourceId(window);
    let mut targets: Vec<(u32, ClientId, u32)> = Vec::new();
    for (eid, sel) in &state.present_event_selections {
        if sel.window == window {
            targets.push((*eid, sel.owner, sel.event_mask));
        }
    }

    for (eid, owner, mask) in targets {
        if mask & COMPLETE_NOTIFY_MASK == 0 {
            continue;
        }
        let Some(client) = state.clients.get_mut(&owner.0) else {
            continue;
        };
        let seq = SequenceNumber(
            client
                .last_sequence
                .load(std::sync::atomic::Ordering::Relaxed),
        );
        let ev = x11present::encode_complete_notify(
            byte_order,
            seq,
            extension_major,
            eid,
            window.0,
            serial,
            x11present::COMPLETE_KIND_NOTIFY_MSC,
            x11present::COMPLETE_MODE_COPY,
            ust,
            current_msc,
        );
        debug!(
            "PRESENT NotifyMSC CompleteNotify -> client {} eid=0x{eid:x} msc={current_msc} ust={ust} ({} bytes)",
            owner.0,
            ev.len()
        );
        let _ = write_to_client(client, owner, &ev);
    }
}

/// Fire every parked `NotifyMSC` (NOTIFY_MSC handler) whose target MSC is now
/// satisfied by the real kernel `(msc, ust)` from the latest pageflip.
/// Called from `drain_present_completions` after the backend advances the
/// vblank clock — this is what keeps a compositor's `present` frame clock
/// running at the display refresh rate.
#[cfg(test)]
pub(crate) fn fire_due_present_notify_msc(state: &mut ServerState, msc: u64, ust: u64) {
    let mut domains: Vec<(u32, u64)> = state
        .present_pending_msc
        .iter()
        .map(|pending| (pending.crtc_id, pending.crtc_epoch))
        .collect();
    domains.sort_unstable();
    domains.dedup();
    for (crtc_id, crtc_epoch) in domains {
        fire_due_present_notify_msc_for_domain(state, crtc_id, crtc_epoch, msc, ust, false);
    }
}

pub(crate) fn fire_due_present_notify_msc_for_domain(
    state: &mut ServerState,
    crtc_id: u32,
    crtc_epoch: u64,
    msc: u64,
    ust: u64,
    force: bool,
) {
    if (!force && msc == 0) || state.present_pending_msc.is_empty() {
        return;
    }
    const PRESENT_MAJOR_OPCODE: u8 = 145;
    let mut still_pending = Vec::new();
    for p in std::mem::take(&mut state.present_pending_msc) {
        if p.crtc_id != crtc_id || p.crtc_epoch != crtc_epoch {
            still_pending.push(p);
        } else if force || notify_msc_satisfied(msc, p.target_msc, p.divisor, p.remainder) {
            fire_present_notify_msc_complete_events(
                state,
                p.byte_order,
                PRESENT_MAJOR_OPCODE,
                p.window,
                p.serial,
                msc.wrapping_sub(p.msc_offset),
                ust,
            );
        } else {
            still_pending.push(p);
        }
    }
    state.present_pending_msc = still_pending;
}

/// Milliseconds since first call, for correlating per-present pipeline stages
/// under `target: "present_pace"`.
///
/// Kept deliberately (was introduced as TEMP scaffolding): the
/// `PresentPixmapSynced` acquire-wait work reports its ready/deferred/signalled
/// stages through this target, and the numbers that validated that fix — 473 of
/// 2,221 synced requests arriving before their acquire point, 0.87 ms mean
/// wait — came from it. Emission is `log::debug!(target: "present_pace", ...)`,
/// so it is silent unless that target is explicitly enabled and costs nothing
/// at the default log level.
pub(crate) fn pace_instr_ms() -> u128 {
    use std::{sync::OnceLock, time::Instant};
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_millis()
}

/// Signal the client's retained wake (real xshmfence/syncobj) via the
/// backend, update X11 fence bookkeeping, then emit IdleNotify+CompleteNotify
/// (idle-before-complete, per Xorg). Stamps the complete event with the
/// latest cached clock for this request's exact CRTC epoch. Used for ungated
/// completions and teardown; paced completion passes an explicit raw sample.
pub(crate) fn complete_present_now(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    event: &crate::backend::CompletedPresentEvent,
) {
    let cached = cached_present_crtc_clock(state, event.crtc_id, event.crtc_epoch);
    let clock = event.completion_clock.unwrap_or({
        if cached.completion.msc > 0 {
            cached.completion
        } else {
            crate::backend::PresentClockSample {
                msc: cached.msc,
                ust: cached.ust,
                source: crate::backend::PresentClockSource::Immediate,
            }
        }
    });
    complete_present_with_clock(
        state,
        backend,
        event,
        clock,
        event.completion_mode,
        event.emit_idle,
    );
}

/// Whether a backend completion still belongs to the live destination window
/// that accepted it. Numeric XIDs may be reused after destruction, so resource
/// existence alone is not sufficient.
pub(crate) fn present_event_window_is_current(
    state: &ServerState,
    event: &crate::backend::CompletedPresentEvent,
) -> bool {
    // Legacy unit fixtures predate window generations and use zero. Real
    // accepted Presents always allocate a nonzero generation.
    #[cfg(test)]
    if event.window_generation == 0 {
        return true;
    }
    state
        .resources
        .window(ResourceId(event.dst_host_xid))
        .is_some()
        && state
            .present_window_generations
            .get(&event.dst_host_xid)
            .is_some_and(|&generation| generation == event.window_generation)
}

/// Drop a backend completion for a destroyed/reused window without emitting
/// wire events. Copy completions may release once their GPU read retired;
/// direct completions must wait for the distinct retired-idle event because
/// their source can still be scanned out.
pub(crate) fn discard_stale_present_event(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    event: &crate::backend::CompletedPresentEvent,
    retired_idle: bool,
) {
    let release = retired_idle || event.emit_idle;
    if !release {
        return;
    }
    backend.signal_present_wake(event.present_id);
    if let crate::backend::PresentWake::Pixmap { idle_fence_xid } = &event.wake
        && *idle_fence_xid != 0
    {
        crate::core_loop::sync_await::fence_triggered(state, *idle_fence_xid);
    }
}

/// `mode`/`emit_idle` are the wire mode byte and whether to also release
/// the retained wake / emit IdleNotify (spec §Ordered completion delivery
/// item 1): a parked Skip (Task 8 supersession) already released its idle
/// fence/syncobj and set the `sync_fences` mirror at scrap time, so its
/// delivery here must not touch idle machinery a second time — no
/// `signal_present_wake`, no IdleNotify, no fence-mirror write.
/// `signal_present_wake` is gated too (not just the fence mirror /
/// IdleNotify): both `emit_idle == false` populations — scrap (Task 8
/// §Supersession) and the copy-failure reroute (Task 8 round-4 F5) — are
/// never-executed: the failing copy precedes `enqueue_present_completion`
/// just as scrap precedes it, so neither ever retains a real backend
/// wake, and the by-XID release (fence/syncobj + mirror + IdleNotify) at
/// scrap/failure time is each entry's sole release path. Gating this
/// signal is therefore a no-op today given how the two populations are
/// built — kept explicit (rather than relying on the backend's
/// unknown-id no-op guard) so a future `emit_idle: false` population
/// that DOES retain a real wake cannot silently double-fire it.
pub(crate) fn complete_present_with_clock(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    event: &crate::backend::CompletedPresentEvent,
    clock: crate::backend::PresentClockSample,
    mode: u8,
    emit_idle: bool,
) {
    use crate::backend::PresentWake;
    let clock = present_wire_clock(clock, event.msc_offset);
    match &event.wake {
        PresentWake::Pixmap { idle_fence_xid } => log::debug!(
            target: "present_pace",
            "PACE-INSTR t={} pid={} stage=signal_wake msc={} source={:?} idle_fence=0x{:x}",
            pace_instr_ms(), event.present_id, clock.msc, clock.source, idle_fence_xid,
        ),
        PresentWake::PixmapSynced {
            release_syncobj,
            release_value,
            ..
        } => log::debug!(
            target: "present_pace",
            "PACE-INSTR t={} pid={} stage=signal_wake msc={} source={:?} release=0x{:x}@{}",
            pace_instr_ms(), event.present_id, clock.msc, clock.source,
            release_syncobj, release_value,
        ),
    }
    if emit_idle {
        backend.signal_present_wake(event.present_id);
    }
    if emit_idle
        && let PresentWake::Pixmap { idle_fence_xid } = &event.wake
        && *idle_fence_xid != 0
    {
        crate::core_loop::sync_await::fence_triggered(state, *idle_fence_xid);
    }
    fire_present_completion_events_at(state, event, clock, mode, emit_idle, true);
}

/// Release a previously completed direct-scanout source after its replacement
/// has retired. This emits IdleNotify only; CompleteNotify was emitted when
/// the source first reached every CRTC.
pub(crate) fn retire_present_idle(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    event: &crate::backend::CompletedPresentEvent,
) {
    use crate::backend::PresentWake;

    backend.signal_present_wake(event.present_id);
    if let PresentWake::Pixmap { idle_fence_xid } = &event.wake
        && *idle_fence_xid != 0
    {
        crate::core_loop::sync_await::fence_triggered(state, *idle_fence_xid);
    }
    let clock = refresh_present_crtc_completion_clock(
        state,
        backend,
        event.crtc_id,
        event.crtc_epoch,
        event.completion_clock,
    );
    let clock = present_wire_clock(clock, event.msc_offset);
    fire_present_completion_events_at(state, event, clock, event.completion_mode, true, false);
}

/// Fire parked completions whose target MSC has been reached. Called from the
/// MSC-advance drain, alongside `fire_due_present_notify_msc`.
///
/// Delivery is per-window `present_id` order, not raw queue order (spec
/// §"Ordered completion delivery (per-window `present_id` order)"):
/// `present_id` is allocated monotonically at request time, so it IS
/// per-window `CompleteNotify` serial order, and Mesa's `loader_dri3`
/// regenerates its swap accounting from the latest event's serial — a
/// backward serial is a real client hazard. A Copy enters this queue at
/// GPU-fence-retirement time while a Skip (Task 8 supersession) enters at
/// scrap (request-arrival) time, so raw insertion order is not serial
/// order: a due entry for window `W` must be held back while any SMALLER
/// `present_id` for `W` is still unresolved in any of three places —
/// msc-parked-unexecuted (`present_pending_exec`), executed-but-undrained
/// (`present_complete_gate`, which carries `dst_window_xid`), or
/// parked-not-yet-due (this same queue). The hold-back is per-window so a
/// stalled window's GPU copy cannot head-of-line block another window's
/// completions.
#[cfg(test)]
pub(crate) fn fire_due_present_completions(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    clock: crate::backend::PresentClockSample,
) {
    let mut domains: Vec<(u32, u64)> = state
        .present_pending_complete
        .iter()
        .map(|pending| (pending.event.crtc_id, pending.event.crtc_epoch))
        .collect();
    domains.sort_unstable();
    domains.dedup();
    for (crtc_id, crtc_epoch) in domains {
        advance_present_crtc_completion_clock(state, crtc_id, crtc_epoch, clock);
    }
    fire_present_completions_sweep(state, backend, false);
}

pub(crate) fn fire_due_present_completions_for_domain(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    crtc_id: u32,
    crtc_epoch: u64,
    clock: crate::backend::PresentClockSample,
) {
    advance_present_crtc_completion_clock(state, crtc_id, crtc_epoch, clock);
    fire_present_completions_sweep(state, backend, false);
}

/// Blackout flush (spec Lifecycle §"DPMS-off / VT-away blackout"; Task 7):
/// deliver every parked completion NOW, ignoring the msc-due check —
/// `present_scanout_blackout()` means no flips and no sequence samples
/// will ever arrive, so the clock is frozen and would never otherwise
/// satisfy `fire_due_present_completions`'s due test (round-4 F1c). The
/// per-window hold-back (`blocked`) still applies unmodified: an
/// outstanding GPU fence in `present_complete_gate` is not something a
/// dark display can force to retire early.
///
/// The msc-parked half of the hold-back is only PARTIALLY cleared by
/// `drain_due_present_pending_exec`'s own blackout branch, which runs
/// first in the same due-pass: that branch force-executes `source_ready`
/// entries only (deliberately — a `source_ready == false` entry is still
/// waiting on its own producer fence, and blackout does nothing to
/// resolve that). So an uncovered `source_ready:false` entry that the
/// successor gate declines to scrap (spec §Supersession) keeps its
/// window's parked completions held back through this flush too, even
/// while blackout is on. DECISION (accepted, do not change): holding the
/// order here is correct — the entry's own producer fence can still
/// signal during blackout (GPU work is display-independent), after which
/// the *next* blackout pass force-executes it and the queue drains. The
/// truly-stuck case (the producer fence never signals) is the same
/// pre-existing exposure that entry always had regardless of blackout,
/// and the purge paths (destroy/disconnect/shutdown) clear it.
pub(crate) fn fire_all_present_completions_now(state: &mut ServerState, backend: &mut dyn Backend) {
    fire_present_completions_sweep(state, backend, true);
}

fn fire_present_completions_sweep(state: &mut ServerState, backend: &mut dyn Backend, force: bool) {
    if state.present_pending_complete.is_empty() {
        return;
    }

    // Smallest still-unresolved present_id per window in the other two
    // hold-back states (BTreeMap iteration order doesn't matter here —
    // every entry is visited to find the per-window min).
    let mut exec_min_by_window: HashMap<u32, u64> = HashMap::new();
    for (&pid, entry) in &state.present_pending_exec {
        exec_min_by_window
            .entry(entry.pending.request.window())
            .and_modify(|m| *m = (*m).min(pid))
            .or_insert(pid);
    }
    let mut gate_min_by_window: HashMap<u32, u64> = HashMap::new();
    for (&pid, gate) in &state.present_complete_gate {
        gate_min_by_window
            .entry(gate.dst_window_xid)
            .and_modify(|m| *m = (*m).min(pid))
            .or_insert(pid);
    }

    // BTreeMap (not HashMap): the rebuild below walks this in window-XID
    // order, which makes the surviving `still_pending` queue's
    // cross-window order deterministic run-to-run. Nothing currently
    // depends on that order, but a HashMap here would be a standing
    // test-flakiness trap for whatever eventually does.
    let mut by_window: std::collections::BTreeMap<u32, Vec<crate::server::PendingPresentComplete>> =
        std::collections::BTreeMap::new();
    for p in std::mem::take(&mut state.present_pending_complete) {
        by_window.entry(p.event.dst_host_xid).or_default().push(p);
    }

    let mut still_pending = Vec::new();
    for (window, mut entries) in by_window {
        // Ascending present_id: the third hold-back state (parked-not-yet-
        // due, i.e. an earlier entry in this very group) falls out of
        // walking in this order and stopping at the first entry that is
        // not both due and externally unblocked — everything after it is
        // held back too, by construction.
        entries.sort_by_key(|p| p.event.present_id);
        let ext_min = match (
            exec_min_by_window.get(&window),
            gate_min_by_window.get(&window),
        ) {
            (Some(&a), Some(&b)) => Some(a.min(b)),
            (Some(&a), None) | (None, Some(&a)) => Some(a),
            (None, None) => None,
        };
        let mut iter = entries.into_iter();
        for p in iter.by_ref() {
            let blocked = ext_min.is_some_and(|m| m < p.event.present_id);
            let cached = cached_present_crtc_clock(state, p.event.crtc_id, p.event.crtc_epoch);
            let due_clock = if cached.completion.msc > 0 {
                cached.completion
            } else if let Some(exact) = p.event.completion_clock {
                exact
            } else {
                crate::backend::PresentClockSample {
                    msc: cached.msc,
                    ust: cached.ust,
                    source: crate::backend::PresentClockSource::Immediate,
                }
            };
            // The domain cache decides when a paced event is due. An exact
            // grouped-direct reference sample only stamps that event; using an
            // older exact retirement as the due clock would park it forever
            // after the domain itself advanced past the target.
            let stamp_clock = p.event.completion_clock.unwrap_or(due_clock);
            let epoch_current =
                backend.present_crtc_clock_epoch(p.event.crtc_id) == p.event.crtc_epoch;
            // Due when msc has reached/passed the target (wrap-safe): NOT
            // (target after msc) — or unconditionally due when `force`
            // (blackout flush) bypasses the clock test entirely. An epoch
            // mismatch also fails open: its raw target belongs to a counter
            // that no longer backs this RANDR XID and must never be compared
            // against the replacement epoch.
            let due = force
                || !epoch_current
                || (due_clock.msc > 0
                    && !crate::present_scheduler::msc_is_after(
                        p.effective_target_msc,
                        due_clock.msc,
                    ));
            if blocked || !due {
                still_pending.push(p);
                break;
            }
            log::debug!(
                target: "present_pace",
                "PACE-INSTR t={} pid={} stage=fired msc={} eff={} source={:?}",
                pace_instr_ms(), p.event.present_id, stamp_clock.msc, p.effective_target_msc,
                stamp_clock.source
            );
            complete_present_with_clock(state, backend, &p.event, stamp_clock, p.mode, p.emit_idle);
        }
        still_pending.extend(iter);
    }
    state.present_pending_complete = still_pending;
}
