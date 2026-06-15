//! State-borrowing replacement for `server::pointer_event_fanout`.
//!
//! Mirrors the pre-lift logic from `server.rs`:
//!   * translate root_x/root_y from host-screen coords to ynest-root,
//!   * honour any active or passive pointer grab,
//!   * walk the propagation chain for core device events,
//!   * fan out parallel XI2 device + raw events to clients selecting
//!     XI2 masks on the target / top-level / root.
//!
//! All work happens inside a single `&mut ServerState` borrow scope —
//! per-target writers go through `client_io::write_or_buffer` so the
//! D3 lift can wire the disconnect list back into the core loop.
//!
//! The xid_map is still passed as `Arc<Mutex<HostXidMap>>`. Phase F1
//! demotes it to a plain field on `HostX11Backend` and at that point
//! the helper takes `&HostXidMap`.

use yserver_protocol::x11::{self, ClientId, ResourceId, SequenceNumber};

use crate::{
    core_loop::fanout::{
        client_target_id, fanout_event_to_clients, pointer_propagation_target_by_id,
    },
    host_x11::{HostPointerEvent, HostXidMap, PointerEventKind},
    resources::ROOT_WINDOW,
    server::{ServerState, xi2_mask_for_client},
};

const XI2_MAJOR_OPCODE: u8 = 137;
const XI2_MASTER_POINTER_DEVICE_ID: u16 = 2;
const XI2_SLAVE_POINTER_DEVICE_ID: u16 = 4;

/// Fan a host pointer event out to nested clients.
///
/// `handle_grabs` toggles passive-grab matching and active-grab
/// redirection. Pass `false` from `AllowEvents ReplayPointer` to avoid
/// re-checking the same passive grab that was just released.
///
/// `is_replay` is set when the call comes from the core
/// `AllowEvents(ReplayPointer)` re-delivery path. That path keeps XI2
/// fanout suppressed because the original physical event has already
/// been delivered to XI2 listeners. XI2 replay after `XIAllowEvents`
/// uses `is_replay=false` because synchronous XI2 passive grabs must
/// re-deliver the device event to the natural target.
pub fn pointer_event_fanout_to_state(
    state: &mut ServerState,
    backend: &mut dyn crate::backend::Backend,
    xid_map: &HostXidMap,
    event: HostPointerEvent,
    handle_grabs: bool,
    is_replay: bool,
) -> Vec<ClientId> {
    // SetPointerMapping: physical button → logical button before any
    // routing (Xorg UpdateDeviceState applies b->map at event
    // generation). A 0 entry disables the button — the event vanishes.
    let mut event = event;
    if matches!(
        event.kind,
        PointerEventKind::ButtonPress | PointerEventKind::ButtonRelease
    ) && let Some(map) = &state.pointer_mapping_override
        && let Some(&mapped) = map.get(usize::from(event.detail).wrapping_sub(1))
    {
        if mapped == 0 {
            return Vec::new();
        }
        event.detail = mapped;
    }
    // Track logical buttons-down for the passive-grab activation
    // predicate (`find_passive_grab` rejects a grab when another
    // button is already down — XGrabButton-1). This is NOT stamped
    // onto delivered events: the backend cooks the authoritative
    // modifier/button state into `event.state` (see the modifier
    // source-of-truth note in `key_fanout`).
    if matches!(
        event.kind,
        PointerEventKind::ButtonPress | PointerEventKind::ButtonRelease
    ) && (1..=16).contains(&event.detail)
    {
        let bit = 1u16 << (event.detail - 1);
        if event.kind == PointerEventKind::ButtonPress {
            state.buttons_down |= bit;
        } else {
            state.buttons_down &= !bit;
        }
    }
    // Pointer confinement (Xorg CheckPhysLimits): while a confined
    // grab is active, motion outside the confine rectangle is
    // replaced by a warp to the nearest inside point; press/release
    // coordinates clamp in place.
    if !is_replay
        && state.pointer_confine_to.0 != 0
        && let Some(w) = state.resources.window(state.pointer_confine_to)
        && w.map_state == crate::resources::MapState::Viewable
    {
        let (x0, y0) = state
            .resources
            .window_absolute_position(state.pointer_confine_to);
        let (x1, y1) = (x0 + i32::from(w.width), y0 + i32::from(w.height));
        let cx = i32::from(event.root_x).clamp(x0, (x1 - 1).max(x0));
        let cy = i32::from(event.root_y).clamp(y0, (y1 - 1).max(y0));
        if cx != i32::from(event.root_x) || cy != i32::from(event.root_y) {
            // Clamp in place — the event delivers at the nearest
            // inside point — and pull the physical cursor along.
            // `warp_pointer_root` re-enters this fanout with the
            // generated motion; the guard stops a second warp if the
            // re-derived coordinates still disagree (recursing here
            // overflowed the stack — 2026-06-07 round-5 crash).
            #[allow(clippy::cast_possible_truncation)]
            {
                event.root_x = cx as i16;
                event.root_y = cy as i16;
            }
            if !state.confine_warp_active {
                state.confine_warp_active = true;
                backend.warp_pointer_root(state, cx, cy);
                state.confine_warp_active = false;
            }
        }
    }
    let now = std::time::Instant::now();
    // Capture priors BEFORE mutating; needed by the IDLETIME wake handler.
    #[allow(clippy::cast_possible_truncation)]
    let prior_global = now
        .duration_since(state.dpms.last_activity)
        .as_millis()
        .min(u128::from(u32::MAX)) as i64;
    // XI2 master device IDs are always small (2 here); cast u16 → u8 is safe.
    // Per-device prior: fall back to global if no per-device entry yet.
    // Matches `idletime_baseline`'s fallback (server.rs Task 1) — without
    // this, the very first input event for a device whose baseline isn't
    // recorded would compute prior_device=0 and a per-device Negative
    // alarm (whose wait_value > 0) would not see the `old > wait` half of
    // its trigger.
    let prior_device = state
        .per_device_last_activity
        .get(&(XI2_MASTER_POINTER_DEVICE_ID as u8))
        .copied()
        .map(|t| {
            #[allow(clippy::cast_possible_truncation)]
            let v = now.duration_since(t).as_millis().min(u128::from(u32::MAX)) as i64;
            v
        })
        .unwrap_or(prior_global);

    state.dpms.last_activity = now;
    state
        .per_device_last_activity
        .insert(XI2_MASTER_POINTER_DEVICE_ID as u8, now);

    // IDLETIME wake: fires Negative-* alarms before the input event itself
    // reaches clients (predictable ordering).
    crate::core_loop::process_request::evaluate_idletime_negative_alarms_on_input_wake(
        state,
        XI2_MASTER_POINTER_DEVICE_ID as u8,
        prior_global,
        prior_device,
    );

    if state.dpms.enabled && state.dpms.power_level != 0 {
        crate::core_loop::process_request::apply_dpms_transition(state, backend, 0);
        // DPMS coupling tail already flipped SS Off if it was On.
    }
    if matches!(
        state.screensaver.active,
        crate::server::ScreenSaverActive::On
    ) {
        // Standalone SS activation (DPMS was On already; SS came up
        // via idle timer or ForceScreenSaver) — input wakes it.
        crate::core_loop::process_request::apply_screen_saver_transition(
            state,
            backend,
            crate::server::ScreenSaverActive::Off,
            /*forced=*/ false,
        );
    }

    let mut dropped = Vec::new();

    // Step 1 — translate host-screen coords to ynest-root coords.
    let event = translate_host_event(state, xid_map, event);

    // Cache the pointer position so server-generated events that must
    // carry it (XI2 focus events) don't ship (0,0). Mirrors Xorg keeping
    // the sprite position in device state.
    state.pointer_root = (event.root_x, event.root_y);

    // Sync-passive-grab freeze queue (Xorg `dix/events.c:1320`
    // ComputeFreezes + PlayReleasedEvents). While a sync passive
    // grab is frozen — between the activating press and
    // AllowEvents — subsequent pointer events MUST NOT leak to
    // the natural target. Marco does ~10 round-trips of focus and
    // property work between the press and AllowEvents(ReplayPointer);
    // a fast user release in that window would otherwise reach the
    // app before the replayed press, malforming the gesture
    // (menus + titlebar drags break). Queue them here for replay.
    //
    // Crossings (Enter/Leave) and the replay path itself bypass —
    // crossings are pointer-tracking notifications Xorg doesn't
    // queue, and the replay re-entry mustn't recursively re-queue.
    // The device is frozen either by a sync passive grab holding the
    // activating press, or by the unified per-device sync state (an
    // explicit GrabPointer(GrabModeSync), an AllowEvents(SyncPointer)
    // re-arm, or a hold on behalf of the other device's grab — Xorg
    // ComputeFreezes switches the whole device to the enqueue proc).
    let pointer_frozen_unified = state
        .xi1_frozen
        .get(&crate::xinput::DEVICEID_SLAVE_POINTER)
        .is_some_and(crate::server::Xi1Freeze::frozen);
    if !is_replay
        && handle_grabs
        && (pointer_frozen_unified
            || (state.pointer_grab_is_passive && state.frozen_pointer_event.is_some()))
        && !matches!(
            event.kind,
            PointerEventKind::EnterNotify | PointerEventKind::LeaveNotify,
        )
    {
        log::trace!(
            "pointer_fanout: QUEUE-WHILE-FROZEN kind={:?} button={} root=({},{}) queue_len={}",
            event.kind,
            event.detail,
            event.root_x,
            event.root_y,
            state.frozen_pointer_queue.len() + 1,
        );
        state.frozen_pointer_queue.push_back(event);
        return dropped;
    }

    if matches!(
        event.kind,
        PointerEventKind::ButtonPress | PointerEventKind::ButtonRelease
    ) {
        log::trace!(
            "pointer_fanout entry: kind={:?} button={} host_xid=0x{:x} root=({},{}) event_xy=({},{})",
            event.kind,
            event.detail,
            event.host_xid,
            event.root_x,
            event.root_y,
            event.event_x,
            event.event_y,
        );
    }

    // Resolve the actual hit window (deepest mapped child under cursor)
    // up front. We need it for both the core-event paths below (passive
    // grab matching, normal propagation) and for the XI2 fanout.
    let natural_hit = natural_pointer_target(state, xid_map, event);
    let top_level_id_opt = natural_hit
        .map(|(target, _, _)| state.top_level_for_target(target))
        .or_else(|| xid_map.get(&event.host_xid).copied());
    let top_level_id = top_level_id_opt.unwrap_or(ROOT_WINDOW);
    let (target, target_x, target_y) =
        natural_hit.unwrap_or((ROOT_WINDOW, event.event_x, event.event_y));

    // ── Core fanout ─────────────────────────────────────────────────
    let mut handled_core_via_grab = false;

    // Step 2 — active-grab redirection (core events only).
    if handle_grabs
        && let Some((grab_window, grab_client, gx, gy, owner_events, via_xi2)) =
            active_grab_target(state)
    {
        // With `owner_events=true`, pointer events on windows owned
        // by the grab client are reported normally (to the deepest
        // natural window) rather than redirected to `grab_window`.
        // The grab itself is just an exclusivity mechanism — other
        // clients can't see the events. GTK3 menus rely on this:
        // motion fires on the panel button until the user actually
        // crosses into the popup, at which point natural
        // Enter/Leave fire and GTK3 transitions menu state. With
        // `owner_events=false`, all events report against grab_window.
        // Step-2 active-grab redirect — `owner_events=true` semantics
        // per X11 spec: deliver normally if the natural target's
        // window is OWNED BY the grab client, in addition to the
        // topological "within grab subtree" case. The earlier check
        // used pure topology, which worked for Cinnamon's muffin
        // (grab window = the same window the menu sub-widgets are
        // descendants of) but failed on MATE's mate-panel pattern
        // where menu items are SEPARATE TOP-LEVEL override-redirect
        // windows OWNED BY mate-panel — siblings of the panel main
        // window, not descendants. Pre-fix this redirected hover
        // motion to the panel main window with grab-relative coords,
        // GTK couldn't localise the hover, submenus stopped opening.
        // No-op when owner_events=false (Cinnamon's pattern).
        let target_qualifies_for_natural = target == grab_window
            || state.resources.is_descendant_of(target, grab_window)
            || state.resources.window_owner(target) == Some(grab_client);
        let redirect_to_grab = !owner_events || !target_qualifies_for_natural;
        if !matches!(
            event.kind,
            PointerEventKind::EnterNotify | PointerEventKind::LeaveNotify
        ) && redirect_to_grab
        {
            let event_x = clamp_grab_coord(event.root_x, gx);
            let event_y = clamp_grab_coord(event.root_y, gy);
            log::trace!(
                "pointer_fanout: ACTIVE-GRAB redirect kind={:?} button={} grab_window=0x{:x} grab_client={:?} owner_events={} via_xi2={}",
                event.kind,
                event.detail,
                grab_window.0,
                grab_client,
                owner_events,
                via_xi2,
            );
            // Deliver the CORE form only for a CORE grab. An XI2 grab
            // (via_xi2) delivers its XI2 form in the XI2 redirect below;
            // sending the core form here too double-delivers every
            // button event to the pure-XI2 grab owner (muffin/cinnamon),
            // corrupting its button state — observed as the stuck
            // mouse-button / rubber-band on the Cinnamon desktop
            // (x11trace: the click delivered as both core ButtonPress
            // and XI2 ButtonPress to the same window). Mirrors Xorg
            // DeliverGrabbedEvent delivering one form per grab protocol.
            // handled_core_via_grab is set regardless so the core event
            // is never ALSO leaked to the natural target in step 4.
            if !via_xi2 {
                let extras = fanout_event_to_clients(state, &[grab_client], |buf, seq, order| {
                    encode_pointer_event(
                        buf,
                        order,
                        event.kind,
                        seq,
                        event.detail,
                        event.time,
                        grab_window,
                        ResourceId(0), // active-grab redirect: no propagation child
                        event,
                        event_x,
                        event_y,
                    );
                });
                merge_dropped(&mut dropped, extras);
            }
            handled_core_via_grab = true;
            // Else (owner_events=true and target owned by grab client):
            // fall through to normal propagation so the event fires on
            // the natural window via the usual subscriber-walk path.
        }
        // For Enter/Leave (natural pointer crossings between windows),
        // never mark handled_core_via_grab — let them fall through to
        // the normal core propagation in step 4. Pre-fix the existing
        // code set the flag unconditionally and dropped natural
        // crossings entirely while a grab was active, so GTK3 menus
        // (xfce4-panel's main menu, marco's title-bar popup) never
        // received the "pointer entered me" notification needed to
        // engage hover/click tracking. Matches Xorg
        // `dix/events.c::DeliverGrabbedEvent` which only re-routes
        // pointer events explicitly listed in the grab's event mask.
    }

    // Passive button grabs must end on the matching release even when
    // `owner_events=true` keeps the press/release on the owned window.
    // Releasing here keeps the grab lifecycle aligned with Xorg and
    // avoids pinning the dialog in a grabbed state until its own
    // timeout path gives up.
    release_passive_grab_on_button_release(state, event.kind);

    // Step 3 — passive button-grab matching for ButtonPress.
    //
    // Delivery mirrors Xorg `DeliverGrabbedEvent` (dix/events.c:4361):
    //
    // 1. With `owner_events=true`, run the natural propagation walk
    //    FILTERED TO THE GRAB CLIENT — `TryClientEvents`
    //    (dix/events.c:2069) returns -1 for any other client ("not
    //    delivered due to grab"), which ABORTS the walk at that
    //    window. Topology (hit being a descendant of the grab
    //    window) is irrelevant; only the grab client's own event
    //    masks qualify.
    // 2. No natural delivery → report to the grab client on the
    //    grab window, filtered by the grab's event_mask.
    // 3. `GrabModeSync` freezes the pointer queue only when a
    //    delivery happened (`FreezeThisEventIfNeededForSyncGrab`
    //    runs under `if (deliveries)`), so the AllowEvents that
    //    thaws it always has a recipient.
    //
    // Pre-fix the qualification accepted any descendant of the grab
    // window — wmaker's click-to-focus sync grab on a CLIENT's
    // window leaked the activating press to the app client while
    // the queue froze; wmaker never saw the press, never called
    // AllowEvents, and the pointer stream wedged (cursor moves,
    // clicks dead — silence HW 2026-06-04).
    // Xorg `Xi/exevents.c:1925`: `CheckDeviceGrabs` (passive-grab
    // matching) only runs on a ButtonPress when there is no active
    // grab — `if (!grab && CheckDeviceGrabs(...))`. With an active
    // grab in place the press goes through `DeliverGrabbedEvent`
    // instead. Gating the activation on `!handled_core_via_grab`
    // alone is wrong because step 2 above intentionally falls through
    // (does NOT set `handled_core_via_grab`) for `owner_events=true`
    // when the natural target is owned by the grab client (GTK3 menu
    // pattern). That fall-through ran passive-grab matching while an
    // active grab was already in place, activated a SYNC passive grab
    // on the same press, set `frozen_pointer_event = Some(event)`,
    // and the unified-freeze QUEUE-WHILE-FROZEN check then swallowed
    // every subsequent press/release into a growing queue with no
    // path to thaw — the XFCE/MATE click-lockup that appeared after
    // the unified-freeze work landed on master.
    let active_grab_present = state.pointer_grab.is_some();
    if !handled_core_via_grab
        && !active_grab_present
        && handle_grabs
        && event.kind == PointerEventKind::ButtonPress
        && let Some((grab, _hit_window)) = try_match_passive_grab(state, xid_map, event)
    {
        log::debug!(
            "pointer_fanout: PASSIVE-GRAB match button={} grab_owner={:?} grab_window=0x{:x} mode={} owner_events={}",
            event.detail,
            grab.owner,
            grab.grab_window.0,
            grab.pointer_mode,
            grab.owner_events,
        );
        // Activate the passive grab atomically with the dispatch.
        state.pointer_grab = Some((grab.owner, grab.grab_window));
        state.pointer_grab_is_passive = true;

        let mask_bit = pointer_mask_bit(event.kind, event.state);
        let natural = if grab.owner_events {
            grabbed_natural_target(state, target, target_x, target_y, mask_bit, grab.owner)
        } else {
            None
        };
        let mut delivered = false;
        if let Some((natural_window, event_x, event_y, child)) = natural {
            let extras = fanout_event_to_clients(state, &[grab.owner], |buf, seq, order| {
                encode_pointer_event(
                    buf,
                    order,
                    PointerEventKind::ButtonPress,
                    seq,
                    event.detail,
                    event.time,
                    natural_window,
                    child,
                    event,
                    event_x,
                    event_y,
                );
            });
            merge_dropped(&mut dropped, extras);
            delivered = true;
        } else if grab.event_mask & mask_bit != 0
            && let Some(grab_target) = client_target_id(state, grab.owner)
        {
            let extras = fanout_event_to_clients(state, &[grab_target], |buf, seq, order| {
                encode_pointer_event(
                    buf,
                    order,
                    PointerEventKind::ButtonPress,
                    seq,
                    event.detail,
                    event.time,
                    grab.grab_window,
                    ResourceId(0), // passive grab activation: no propagation child
                    event,
                    event.event_x,
                    event.event_y,
                );
            });
            merge_dropped(&mut dropped, extras);
            delivered = true;
        }
        if delivered && grab.pointer_mode == 0 {
            state.frozen_pointer_event = Some(event);
        }
        // Xorg ActivatePointerGrab → CheckGrabForSyncs: a sync
        // pointer_mode freezes the pointer's device stream; a sync
        // keyboard_mode holds the KEYBOARD on this grab's behalf
        // (XGrabButton-18/19/20 freeze the keyboard via a button
        // grab and thaw it with AllowEvents).
        xi1_check_grab_for_syncs(
            state,
            crate::xinput::DEVICEID_SLAVE_POINTER,
            grab.owner,
            grab.pointer_mode == 0,
            grab.keyboard_mode == 0,
        );
        // ConfineCursorToWindow — record the confinement and pull
        // the pointer inside the confine window (XGrabButton-23/24).
        state.pointer_confine_to = grab.confine_to;
        if grab.confine_to.0 != 0 {
            crate::core_loop::process_request::confine_pointer_now(state, backend);
        }
        // During a grab, core pointer events never reach other
        // clients — both branches above are the only deliveries.
        handled_core_via_grab = true;
    }

    // Step 4 — normal core propagation, only when no grab took ownership.
    //
    // For Crossing events we also run when top_level_id is None: the
    // producer (`update_pointer_window`) emits Leave/Enter chain events
    // with host_xid pointing at the KMS root container for the
    // ROOT_WINDOW endpoint, and that host_xid isn't in xid_map.
    let is_crossing = matches!(
        event.kind,
        PointerEventKind::EnterNotify | PointerEventKind::LeaveNotify
    );
    if !handled_core_via_grab && (top_level_id_opt.is_some() || is_crossing) {
        let mask_bit = pointer_mask_bit(event.kind, event.state);
        let (nested_id, event_x, event_y, mut core_targets, propagation_child) =
            pointer_propagation_target_by_id(state, target, target_x, target_y, mask_bit)
                .unwrap_or((target, target_x, target_y, Vec::new(), ResourceId(0)));

        // XI2 shadows core per client (Xorg behaviour, mirrors
        // `deliver_key_to_window`): a client that receives the XI2
        // form of this event must NOT also receive the core form, or
        // it processes every click/motion twice. Chromium's Ozone X11
        // layer selects both core and XI2 on its windows; without this
        // dedup its 3-dots / extensions menu got two ButtonPress
        // events and opened-then-closed. Skip on replay — the XI2
        // fanout below is skipped on replay, so there is no XI2 form
        // to dedup against.
        if !is_replay {
            let xi2_evt = xi2_evtype(event.kind);
            if xi2_evt != 0 {
                let (xi2_dedup, _) =
                    compute_xi2_targets(state, target, top_level_id, xi2_evt, None);
                core_targets.retain(|c| !xi2_dedup.contains(c));
            }
        }

        // Log button delivery (always) and crossing delivery (low-frequency,
        // only on window change — safe at debug). Crossing delivery is the
        // boundary that distinguishes "Enter never reached the WM client"
        // from "Enter delivered fine → focus bug is WM-side / a wire field
        // muffin rejects". `detail` carries the button number for
        // ButtonPress/Release and the NotifyDetail (Ancestor/Virtual/
        // Nonlinear/Inferior) for crossings.
        if matches!(
            event.kind,
            PointerEventKind::ButtonPress
                | PointerEventKind::ButtonRelease
                | PointerEventKind::EnterNotify
                | PointerEventKind::LeaveNotify
        ) {
            log::debug!(
                "pointer_fanout: kind={:?} detail={} mode={} host_xid=0x{:x} top_level=0x{:x} target=0x{:x} \
                 propagation_window=0x{:x} child=0x{:x} core_targets={:?} root=({},{}) event_xy=({},{})",
                event.kind,
                event.detail,
                event.crossing_mode,
                event.host_xid,
                top_level_id.0,
                target.0,
                nested_id.0,
                propagation_child.0,
                core_targets.iter().map(|c| c.0).collect::<Vec<_>>(),
                event.root_x,
                event.root_y,
                event_x,
                event_y,
            );
        }

        let extras = fanout_event_to_clients(state, &core_targets, |buf, seq, order| {
            encode_pointer_event(
                buf,
                order,
                event.kind,
                seq,
                event.detail,
                event.time,
                nested_id,
                propagation_child,
                event,
                event_x,
                event_y,
            );
        });
        merge_dropped(&mut dropped, extras);
    }

    // ── XI2 fanout ──────────────────────────────────────────────────
    //
    // Skip on replay: XI2 was already fanned out on the original
    // libinput-driven invocation. Re-running here would deliver a second
    // XI2 ButtonPress for the same physical click and confuse GTK gesture
    // controllers (see is_replay rationale on the public fn).
    if is_replay {
        return dropped;
    }

    // ── XI 1.x device-event fanout ──────────────────────────────────
    //
    // Legacy XInput events (DeviceButtonPress/Release,
    // DeviceMotionNotify) for clients that selected the matching
    // `XEventClass` via SelectExtensionEvent. XI1 events propagate up
    // the ancestor chain like core events (Xorg dix
    // DeliverDeviceEvents) and report the slave/extension pointer (4),
    // matching the device XTS opens (masters are not XI1-openable).
    //
    // Deliberately BEFORE the `top_level_id_opt` gate below: XI1
    // routing resolves its own target from `natural_target` and its
    // own grab state, so a cursor over the bare root (host_xid not in
    // xid_map — no top-level under it) must still route. The XTS
    // AllowDeviceEvents probes are exactly that shape: the fake device
    // motion parks the cursor at x=0 off every window, and the grab
    // owner still expects its DeviceMotionNotify.
    let xi1_offset = match event.kind {
        PointerEventKind::ButtonPress => Some(crate::xinput::XI_DEVICE_BUTTON_PRESS_OFFSET),
        PointerEventKind::ButtonRelease => Some(crate::xinput::XI_DEVICE_BUTTON_RELEASE_OFFSET),
        PointerEventKind::MotionNotify => Some(crate::xinput::XI_DEVICE_MOTION_NOTIFY_OFFSET),
        _ => None,
    };
    if let Some(offset) = xi1_offset {
        let evcode = crate::server::XI_FIRST_EVENT + offset;
        let detail = if event.kind == PointerEventKind::MotionNotify {
            0
        } else {
            event.detail
        };
        let extras = xi1_route_device_event(
            state,
            crate::server::Xi1QueuedEvent {
                deviceid: crate::xinput::DEVICEID_SLAVE_POINTER,
                evcode,
                detail,
                time: event.time,
                root_x: event.root_x,
                root_y: event.root_y,
                event_x: target_x,
                event_y: target_y,
                state_mask: event.state,
                natural_target: target,
                // Pointer devices have no focus class — always the
                // plain selection walk (Xorg ProcessOtherEvent only
                // routes keyboard events through DeliverFocusedEvent).
                focus_route: crate::server::Xi1FocusRoute::Walk,
                axes: None,
                replay_floor: None,
            },
            true,
        );
        merge_dropped(&mut dropped, extras);
    }

    let Some(top_level_id) = top_level_id_opt else {
        log::debug!(
            "pointer_fanout: kind={:?} host_xid=0x{:x} not in xid_map — XI2 fanout skipped",
            event.kind,
            event.host_xid,
        );
        return dropped;
    };
    let xi2_evtype = xi2_evtype(event.kind);
    let xi2_raw_evtype = xi2_raw_evtype(event.kind);

    // For XI2 the "event window" is the hit target — XI2 doesn't
    // propagate up through the event mask the way core does. Use the
    // original (untranslated) coords relative to the hit target.
    let (mut event_x, mut event_y) = (target_x, target_y);
    let mut nested_id = target;
    let (mut xi2_targets, xi2_raw_targets) =
        compute_xi2_targets(state, target, top_level_id, xi2_evtype, xi2_raw_evtype);

    // Synchronous passive XI2 button grabs freeze the device event at
    // the grab owner until XIAllowEvents(ReplayDevice) replays it.
    // Without this filter GTK sees the press on the unfocused target
    // before muffin finishes focus activation, then never receives the
    // replay it expects.
    if handle_grabs
        && event.kind == PointerEventKind::ButtonPress
        && state.pointer_grab_is_passive
        && state.frozen_pointer_event.is_some()
        && let Some((grab_owner, _)) = state.pointer_grab
    {
        xi2_targets.retain(|cid| *cid == grab_owner);
    }

    // Active device-grab redirection for XI2 device events. When a client
    // holds an active pointer grab (XIGrabDevice — or an activated passive
    // grab), the grabbed device's XI2 button/motion events must funnel to
    // the grab owner, reported against the grab window, even when the
    // pointer has moved onto another client's window. This mirrors the
    // core Step-2 redirect; without it a window-move grab (muffin) stops
    // receiving XI_Motion / XI_ButtonRelease the moment the drag pulls the
    // pointer off the grab window, so the move never ends and the button
    // stays "held". Crossings keep natural delivery (same as the core
    // path); raw events bypass grabs and are left untouched.
    if handle_grabs
        && !matches!(
            event.kind,
            PointerEventKind::EnterNotify | PointerEventKind::LeaveNotify
        )
        && let Some((grab_window, grab_client, gx, gy, owner_events, via_xi2)) =
            active_grab_target(state)
    {
        // Same ownership-aware natural-delivery test as the Step-2
        // core path: `owner_events=true` keeps motion on whichever
        // GTK sub-window the cursor is over when that sub-window is
        // OWNED BY the grab client, even if it's a sibling top-level
        // (mate-panel menu items) rather than a descendant of the
        // grab window. No-op when owner_events=false.
        let target_qualifies_for_natural = target == grab_window
            || state.resources.is_descendant_of(target, grab_window)
            || state.resources.window_owner(target) == Some(grab_client);
        if !owner_events || !target_qualifies_for_natural {
            // The grab is exclusive either way; whether the OWNER gets
            // an XI2 copy depends on the protocol the grab was
            // established with. A core GrabPointer owner receives core
            // events only (Step-2 above) — pushing it here sent XI2
            // XGE events to plain-Xlib clients, and libXi's wire
            // handler NULL-derefs when the client linked libXi without
            // ever calling XIQueryVersion (xts5 Xlib11/ButtonPress
            // TP10 crashed in exactly that state, poisoning the
            // display mutex and hanging the rest of the TCM).
            xi2_targets.clear();
            if via_xi2 {
                xi2_targets.push(grab_client);
                nested_id = grab_window;
                event_x = clamp_grab_coord(event.root_x, gx);
                event_y = clamp_grab_coord(event.root_y, gy);
            }
        }
    }

    if matches!(
        event.kind,
        PointerEventKind::ButtonPress
            | PointerEventKind::ButtonRelease
            | PointerEventKind::EnterNotify
            | PointerEventKind::LeaveNotify
    ) {
        // `event_window` (nested_id) is what the XI2 crossing carries as
        // its `event` field — i.e. the window muffin sees the pointer
        // enter. It is resolved from cursor COORDINATES (root_pointer_
        // target_at), NOT from this crossing's intrinsic host_xid, so it
        // can diverge from the producer's window_under_cursor decision.
        // Compare against host_xid + the `upw:` producer trace to spot
        // the dual-hit-test divergence behind flaky focus-follows-mouse.
        log::debug!(
            "pointer_fanout XI2: kind={:?} detail={} time={} host_xid=0x{:x} event_window=0x{:x} target=0x{:x} top_level=0x{:x} \
             xi2_targets={:?} xi2_raw_targets={:?} root=({},{}) event_xy=({},{}) state=0x{:x}",
            event.kind,
            event.detail,
            event.time,
            event.host_xid,
            nested_id.0,
            target.0,
            top_level_id.0,
            xi2_targets.iter().map(|c| c.0).collect::<Vec<_>>(),
            xi2_raw_targets.iter().map(|c| c.0).collect::<Vec<_>>(),
            event.root_x,
            event.root_y,
            event_x,
            event_y,
            event.state,
        );
    }

    // XI2 raw events.
    if let Some(raw_evtype) = xi2_raw_evtype {
        let extras = fanout_event_to_clients(state, &xi2_raw_targets, |buf, seq, order| {
            x11::encode_xi2_raw_event(
                buf,
                order,
                seq,
                XI2_MAJOR_OPCODE,
                raw_evtype,
                XI2_MASTER_POINTER_DEVICE_ID,
                event.time,
                u32::from(event.detail),
                XI2_SLAVE_POINTER_DEVICE_ID,
                i32::from(event.root_x),
                i32::from(event.root_y),
            );
        });
        merge_dropped(&mut dropped, extras);
    }

    // If this is a wheel button press (4 = up, 5 = down, 6 = left,
    // 7 = right), prepend an XI_Motion event carrying the scroll-axis
    // valuator update before the XI_ButtonPress. The Motion's axis
    // value is the **CUMULATIVE** scroll counter, not the per-event
    // delta — XI2 §11.5: clients compute the delta as
    // `(current - previous) / increment`. The previous-value
    // baseline comes from `XIQueryDevice` (the valuator class
    // declares the current position) and `DeviceChanged` events as
    // clients connect mid-session.
    //
    // A pre-2026-05-29 yserver sent `delta` (±1) here. GDK reads the
    // axisvalue, subtracts its cached previous (which after the first
    // scroll is also 1), and gets 0 — no scroll. The first scroll
    // worked (1 - 0 = 1), every subsequent scroll on the same client
    // got stuck. This bug went unnoticed because GDK was falling back
    // to the legacy XI_ButtonPress(4..7) emulation (XIPointerEmulated
    // flag wasn't being set, so GDK accepted those buttons as scroll).
    // The XI_POINTER_EMULATED fix (Chrome scroll-crash repair) made
    // GDK correctly skip the emulated buttons, exposing the latent
    // cumulative-vs-delta bug.
    //
    // ButtonRelease doesn't carry an axis update; only ButtonPress does.
    let scroll_axis_info: Option<(u8, i32)> = if event.kind == PointerEventKind::ButtonPress
        && (event.detail >= 4 && event.detail <= 7)
    {
        let (axis_idx, delta): (usize, i32) = match event.detail {
            4 => (0, -1),
            5 => (0, 1),
            6 => (1, -1),
            7 => (1, 1),
            _ => unreachable!(),
        };
        state.scroll_axis_value[axis_idx] = state.scroll_axis_value[axis_idx].wrapping_add(delta);
        let scroll_axis_num: u8 = if axis_idx == 0 { 2 } else { 3 };
        Some((scroll_axis_num, state.scroll_axis_value[axis_idx]))
    } else {
        None
    };

    // XI2 device events (crossing or non-crossing).
    let extras = fanout_event_to_clients(state, &xi2_targets, |buf, seq, order| {
        if matches!(
            event.kind,
            PointerEventKind::EnterNotify | PointerEventKind::LeaveNotify
        ) {
            x11::encode_xi2_crossing_event(
                buf,
                order,
                seq,
                XI2_MAJOR_OPCODE,
                xi2_evtype,
                XI2_MASTER_POINTER_DEVICE_ID,
                event.time,
                ROOT_WINDOW,
                nested_id,
                event.root_x,
                event.root_y,
                event_x,
                event_y,
                event.state,
                0,
                0,
                XI2_SLAVE_POINTER_DEVICE_ID,
            );
        } else {
            if let Some((axis, value)) = scroll_axis_info {
                x11::encode_xi2_motion_with_scroll(
                    buf,
                    order,
                    seq,
                    XI2_MAJOR_OPCODE,
                    XI2_MASTER_POINTER_DEVICE_ID,
                    event.time,
                    ROOT_WINDOW,
                    nested_id,
                    event.root_x,
                    event.root_y,
                    event_x,
                    event_y,
                    event.state,
                    XI2_SLAVE_POINTER_DEVICE_ID,
                    axis,
                    value,
                );
            }
            // Mark scroll-emulated XI_ButtonPress/Release(4..7) with
            // XIPointerEmulated so XI2-aware clients discard the legacy
            // button event after consuming the matching XI_Motion
            // scroll-axis update. Skipping this flag double-dispatches
            // wheel input — release Chrome stack-smashed on rapid
            // scroll into yserver from this exact gap (see
            // `yserver-protocol::x11::XI_POINTER_EMULATED` for the full
            // rationale).
            let xi2_flags: u32 = if matches!(
                event.kind,
                PointerEventKind::ButtonPress | PointerEventKind::ButtonRelease
            ) && (4..=7).contains(&event.detail)
            {
                x11::XI_POINTER_EMULATED
            } else {
                0
            };
            x11::encode_xi2_device_event(
                buf,
                order,
                seq,
                XI2_MAJOR_OPCODE,
                xi2_evtype,
                XI2_MASTER_POINTER_DEVICE_ID,
                event.time,
                ROOT_WINDOW,
                nested_id,
                ResourceId(0), // XI2 doesn't propagate; event=hit-target, so child=None
                event.root_x,
                event.root_y,
                event_x,
                event_y,
                event.state,
                u32::from(event.detail),
                XI2_SLAVE_POINTER_DEVICE_ID,
                xi2_flags,
            );
        }
    });
    merge_dropped(&mut dropped, extras);

    dropped
}

/// Route one XI 1.x device input event through grab + freeze + selection
/// semantics. The single entry point shared by the pointer/key fanouts
/// and the AllowDeviceEvents thaw path:
///
/// 1. Frozen device → queue the event, deliver nothing.
/// 2. Active device grab → deliver to the grab owner addressed to the
///    grab window (owner_events keeps natural delivery when the natural
///    target would already report to the owner); a synchronous grab
///    re-freezes after each key/button event (`allow_freeze`).
///    A passive-activated grab auto-releases on its matching release.
/// 3. Otherwise a press may activate a matching passive grab
///    (GrabDeviceKey / GrabDeviceButton): sets the active grab, updates
///    last-device-grab time (XTS XGrabDeviceKey-3), delivers to the
///    owner, and freezes when synchronous.
/// 4. Otherwise: SelectExtensionEvent selection walk.
///
/// NOTE: deliberately NO implicit grab from a plain DeviceButtonPress
/// selection — per the XInput 1.x spec (XTS XSelectExtensionEvent-5)
/// automatic grabs are opt-in via the DeviceButtonPressGrab class.
pub(crate) fn xi1_route_device_event(
    state: &mut ServerState,
    q: crate::server::Xi1QueuedEvent,
    allow_freeze: bool,
) -> Vec<ClientId> {
    use crate::xinput::{
        XI_DEVICE_BUTTON_PRESS_OFFSET, XI_DEVICE_BUTTON_RELEASE_OFFSET, XI_DEVICE_KEY_PRESS_OFFSET,
        XI_DEVICE_KEY_RELEASE_OFFSET,
    };
    let first = crate::server::XI_FIRST_EVENT;
    state.xi1_last_input_time = state.xi1_last_input_time.max(q.time);
    let is_press = q.evcode == first + XI_DEVICE_KEY_PRESS_OFFSET
        || q.evcode == first + XI_DEVICE_BUTTON_PRESS_OFFSET;
    let is_release = q.evcode == first + XI_DEVICE_KEY_RELEASE_OFFSET
        || q.evcode == first + XI_DEVICE_BUTTON_RELEASE_OFFSET;

    // 1. Frozen → queue (Xorg FreezeThaw switching processInputProc
    // to the enqueue proc: NOTHING is delivered while frozen, not
    // even to the grab owner).
    if state
        .xi1_frozen
        .get(&q.deviceid)
        .is_some_and(crate::server::Xi1Freeze::frozen)
    {
        state
            .xi1_frozen
            .entry(q.deviceid)
            .or_default()
            .queue
            .push_back(q);
        log::debug!(
            "xi1_route: device {} frozen — queued evcode={} detail={}",
            q.deviceid,
            q.evcode,
            q.detail,
        );
        return Vec::new();
    }

    // Maintain the per-device axis values (Xorg `axisVal`): real
    // motion writes the sprite position into axes 0/1; faked device
    // motion writes its explicit payload. After the frozen check, like
    // the bitmask updates below.
    if q.evcode == first + crate::xinput::XI_DEVICE_MOTION_NOTIFY_OFFSET {
        let entry = state.xi1_device_input_state.entry(q.deviceid).or_default();
        if let Some(axes) = q.axes {
            for i in 0..usize::from(axes.count.min(6)) {
                if let Some(slot) = entry.valuators.get_mut(usize::from(axes.first) + i) {
                    *slot = axes.values[i];
                }
            }
        } else {
            entry.valuators[0] = i32::from(q.root_x);
            entry.valuators[1] = i32::from(q.root_y);
        }
    }

    // Maintain the per-device key/button-down bitmasks consumed by
    // DeviceStateNotify (Xorg `dev->key->down` / `dev->button->down`).
    // After the frozen check: queued events come back through here on
    // thaw, so updating at queue time would double-count them.
    if is_press || is_release {
        let is_key = q.evcode == first + XI_DEVICE_KEY_PRESS_OFFSET
            || q.evcode == first + XI_DEVICE_KEY_RELEASE_OFFSET;
        let entry = state.xi1_device_input_state.entry(q.deviceid).or_default();
        let bits = if is_key {
            &mut entry.keys_down
        } else {
            &mut entry.buttons_down
        };
        let (byte, bit) = (usize::from(q.detail) / 8, q.detail % 8);
        if is_press {
            bits[byte] |= 1 << bit;
        } else {
            bits[byte] &= !(1 << bit);
        }
    }

    // 2. Active grab.
    if let Some(grab) = state.xi1_active_grabs.get(&q.deviceid).copied() {
        // owner_events: natural delivery when the natural target's
        // selection walk would report to the grab owner anyway.
        let natural = compute_xi1_route_targets(state, &q);
        let (targets, event_window) = match natural {
            Some((clients, w)) if grab.owner_events && clients.contains(&grab.owner) => {
                (vec![grab.owner], w)
            }
            _ => (vec![grab.owner], grab.grab_window),
        };
        log::debug!(
            "xi1_route: evcode={} GRAB owner={} window=0x{:x} detail={}",
            q.evcode,
            grab.owner.0,
            event_window.0,
            q.detail,
        );
        let dropped = xi1_fan_device_event(state, &targets, event_window, &q);
        // Sync-state transition on a delivered key/button event (Xorg
        // FreezeThisEventIfNeededForSyncGrab) — the FreezeNextEvent /
        // FreezeBothNextEvent armed states trip to FrozenWithEvent
        // here. A plain sync grab does NOT re-freeze on every press:
        // CheckGrabForSyncs froze it once at activation.
        if allow_freeze && (is_press || is_release) {
            xi1_freeze_this_event_if_needed(state, q.deviceid, grab.owner, &q);
        }
        if is_release && grab.passive_detail == Some(q.detail) {
            xi1_deactivate_device_grab(state, q.deviceid);
        }
        return dropped;
    }

    // 3. Passive grab activation on press. A ReplayThisDevice
    // reprocessing pass skips grabs at or above the released grab's
    // window — "as though they were not present" (the replay floor).
    if is_press {
        let matched = state
            .xi1_passive_grabs
            .iter()
            .find(|g| {
                g.deviceid == q.deviceid
                    && (g.detail == 0 || g.detail == q.detail)
                    && (g.modifiers == 0x8000 || g.modifiers == q.state_mask & 0x00ff)
                    && xi1_window_in_chain(state, q.natural_target, g.grab_window)
                    && !q
                        .replay_floor
                        .is_some_and(|floor| xi1_window_in_chain(state, floor, g.grab_window))
            })
            .copied();
        if let Some(g) = matched {
            state.xi1_active_grabs.insert(
                q.deviceid,
                crate::server::Xi1ActiveGrab {
                    owner: g.owner,
                    deviceid: q.deviceid,
                    grab_window: g.grab_window,
                    owner_events: g.owner_events,
                    this_mode: g.this_mode,
                    other_mode: g.other_mode,
                    passive_detail: Some(q.detail),
                },
            );
            state.xi1_last_grab_time = q.time;
            // CheckGrabForSyncs at activation, then deliver the
            // activating press; a sync grab stores it for Replay
            // (FROZEN_NO_EVENT → FROZEN_WITH_EVENT, Xorg
            // DeliverGrabbedEvent / ActivateGrabNoDelivery tail).
            if allow_freeze {
                xi1_check_grab_for_syncs(
                    state,
                    q.deviceid,
                    g.owner,
                    g.this_mode == 0,
                    g.other_mode == 0,
                );
            }
            let dropped = xi1_fan_device_event(state, &[g.owner], g.grab_window, &q);
            if allow_freeze {
                let sync = state.xi1_frozen.entry(q.deviceid).or_default();
                if sync.state == crate::server::Xi1SyncState::FrozenNoEvent {
                    sync.state = crate::server::Xi1SyncState::FrozenWithEvent;
                    sync.stored = Some(q);
                }
            }
            return dropped;
        }
    }

    // 4. Selection delivery, gated by the device-focus route
    // (DeliverFocusedEvent for keyboard devices; plain walk for
    // pointer devices).
    let hit = compute_xi1_route_targets(state, &q);
    log::debug!(
        "xi1_route: evcode={} target=0x{:x} route={:?} hit={:?}",
        q.evcode,
        q.natural_target.0,
        q.focus_route,
        hit.as_ref()
            .map(|(t, w)| (t.iter().map(|c| c.0).collect::<Vec<_>>(), w.0)),
    );
    let dropped = match hit {
        Some((targets, w)) => xi1_fan_device_event(state, &targets, w, &q),
        None => Vec::new(),
    };
    // A BRIDGED core grab (no XI1 grab, but the core pointer/keyboard
    // grab controls this device) must still trip an armed
    // FreezeNextEvent / FreezeBothNextEvent on key/button events —
    // Xorg delivers through that one grab slot and trips there.
    if allow_freeze
        && (is_press || is_release)
        && let Some(owner) = xi1_device_grab_owner(state, q.deviceid)
    {
        xi1_freeze_this_event_if_needed(state, q.deviceid, owner, &q);
    }
    dropped
}

/// Apply the event's [`Xi1FocusRoute`] to find selection-delivery
/// targets — the XI1 analogue of Xorg `DeliverFocusedEvent`
/// (dix/events.c:4202): unbounded walk, walk bounded at the focus
/// window, focus-window-only, or none (focus = None).
fn compute_xi1_route_targets(
    state: &ServerState,
    q: &crate::server::Xi1QueuedEvent,
) -> Option<(Vec<ClientId>, ResourceId)> {
    match q.focus_route {
        crate::server::Xi1FocusRoute::Walk => {
            compute_xi1_targets_bounded(state, q.natural_target, q.evcode, q.deviceid, None)
        }
        crate::server::Xi1FocusRoute::WalkUpTo(stop) => {
            compute_xi1_targets_bounded(state, q.natural_target, q.evcode, q.deviceid, Some(stop))
        }
        crate::server::Xi1FocusRoute::WindowOnly(w) => {
            let targets = xi1_window_selectors(state, w, q.evcode, q.deviceid);
            if targets.is_empty() {
                None
            } else {
                Some((targets, w))
            }
        }
        crate::server::Xi1FocusRoute::Drop => None,
    }
}

/// Clients that selected `(deviceid << 8) | evcode` on exactly `window`.
fn xi1_window_selectors(
    state: &ServerState,
    window: ResourceId,
    evcode: u8,
    deviceid: u16,
) -> Vec<ClientId> {
    let class = (u32::from(deviceid) << 8) | u32::from(evcode);
    state
        .clients
        .iter()
        .filter(|(_, c)| {
            c.xi1_window_event_classes
                .get(&window)
                .is_some_and(|set| set.contains(&class))
        })
        .map(|(id, _)| ClientId(*id))
        .collect()
}

/// True when `grab_window` is `target` or one of its ancestors.
fn xi1_window_in_chain(state: &ServerState, target: ResourceId, grab_window: ResourceId) -> bool {
    let mut window = target;
    loop {
        if window == grab_window {
            return true;
        }
        if window == ROOT_WINDOW {
            return false;
        }
        match state.resources.window(window).map(|w| w.parent) {
            Some(parent) if parent != window => window = parent,
            _ => return false,
        }
    }
}

fn xi1_fan_device_event(
    state: &mut ServerState,
    targets: &[ClientId],
    event_window: ResourceId,
    q: &crate::server::Xi1QueuedEvent,
) -> Vec<ClientId> {
    // DeviceMotionNotify MUST be a MORE_EVENTS chain with a trailing
    // deviceValuator: libXi's XInputWireToEvent returns DONT_ENQUEUE
    // unconditionally for the leading motion event and only enqueues
    // when the valuator continuation lands (XExtInt.c) — a bare motion
    // event silently vanishes inside every libXi client. Key/button
    // events enqueue standalone. The valuator payload mirrors the
    // device's X/Y axes (the sprite position), matching Xorg's
    // getValuatorEvents for a 2-axis motion.
    let is_motion =
        q.evcode == crate::server::XI_FIRST_EVENT + crate::xinput::XI_DEVICE_MOTION_NOTIFY_OFFSET;
    fanout_event_to_clients(state, targets, |buf, seq, order| {
        crate::xinput::encode_xi1_device_input_event(
            buf,
            order,
            q.evcode,
            q.detail,
            seq,
            q.time,
            ROOT_WINDOW.0,
            event_window.0,
            0,
            q.root_x,
            q.root_y,
            q.event_x,
            q.event_y,
            q.state_mask,
            if is_motion {
                q.deviceid | u16::from(crate::xinput::XI1_MORE_EVENTS)
            } else {
                q.deviceid
            },
        );
        if is_motion {
            // Faked motion carries its explicit axis payload; real
            // motion reports the X/Y axes (= sprite position).
            let (num, first_v, values) = match q.axes {
                Some(a) => (a.count.min(6), a.first, a.values),
                None => (2, 0, [i32::from(q.root_x), i32::from(q.root_y), 0, 0, 0, 0]),
            };
            #[allow(clippy::cast_possible_truncation)]
            crate::xinput::encode_xi1_device_valuator(
                buf,
                order,
                crate::server::XI_FIRST_EVENT + crate::xinput::XI_DEVICE_VALUATOR_OFFSET,
                q.deviceid as u8,
                seq,
                q.state_mask,
                num,
                first_v,
                values,
            );
        }
    })
}

/// The paired input device: pointer (4) <-> keyboard (5). Used for
/// other_devices_mode freeze bookkeeping.
pub(crate) fn xi1_other_input_device(deviceid: u16) -> u16 {
    if deviceid == crate::xinput::DEVICEID_SLAVE_POINTER {
        crate::xinput::DEVICEID_SLAVE_KEYBOARD
    } else {
        crate::xinput::DEVICEID_SLAVE_POINTER
    }
}

/// Force-thaw a device: reset its sync state outright and flush. Used
/// by teardown paths (client disconnect with no grabs left) where no
/// grab semantics apply — NOT by AllowDeviceEvents, which manipulates
/// the sync state per Xorg `AllowSome` and then calls
/// [`xi1_compute_freezes`].
pub(crate) fn xi1_thaw_device(state: &mut ServerState, deviceid: u16) {
    if let Some(freeze) = state.xi1_frozen.get_mut(&deviceid) {
        freeze.state = crate::server::Xi1SyncState::Thawed;
        freeze.other = None;
    }
    xi1_compute_freezes(state);
}

/// Port of Xorg `ComputeFreezes` (dix/events.c:1320) over the
/// two-device model: re-derive each device's frozen flag from its sync
/// state and flush the queued events of every no-longer-frozen device.
/// Re-freezing mid-flush (a queued press activating a sync passive
/// grab) leaves the remainder queued, exactly like Xorg's restart loop.
pub(crate) fn xi1_compute_freezes(state: &mut ServerState) {
    for dev in [
        crate::xinput::DEVICEID_SLAVE_POINTER,
        crate::xinput::DEVICEID_SLAVE_KEYBOARD,
    ] {
        while let Some(freeze) = state.xi1_frozen.get_mut(&dev) {
            if freeze.frozen() {
                break;
            }
            // Withheld CORE keys replay first (they were withheld at
            // fanout entry, before the XI1 form was queued).
            if let Some(ev) = freeze.core_key_queue.pop_front() {
                log::debug!("xi1_compute_freezes: device {dev} replaying core key");
                let _ = crate::core_loop::key_fanout::deliver_routed_key(state, ev);
                continue;
            }
            let Some(q) = freeze.queue.pop_front() else {
                break;
            };
            log::debug!(
                "xi1_compute_freezes: device {dev} replaying evcode={}",
                q.evcode
            );
            let _ = xi1_route_device_event(state, q, true);
        }
    }
    // Core pointer events withheld under the unified freeze: when the
    // pointer thaws outside the AllowEvents pointer-release path
    // (which drains the queue itself before thawing), drop the stale
    // backlog — holding them until the next freeze would replay
    // ancient events.
    let ptr_frozen = state
        .xi1_frozen
        .get(&crate::xinput::DEVICEID_SLAVE_POINTER)
        .is_some_and(crate::server::Xi1Freeze::frozen);
    if !ptr_frozen && state.frozen_pointer_event.is_none() && !state.frozen_pointer_queue.is_empty()
    {
        log::debug!(
            "xi1_compute_freezes: dropping {} stale withheld core pointer events",
            state.frozen_pointer_queue.len()
        );
        state.frozen_pointer_queue.clear();
    }
}

/// Port of Xorg `CheckGrabForSyncs` (dix/events.c:1424-1450): set the
/// sync state at grab activation. A sync `this_mode` freezes the
/// grabbed device ONCE (FrozenNoEvent); a sync `other_mode` holds the
/// paired device on behalf of this grab (`sync.other`). Async modes
/// release the same-client holds.
pub(crate) fn xi1_check_grab_for_syncs(
    state: &mut ServerState,
    deviceid: u16,
    owner: ClientId,
    this_sync: bool,
    other_sync: bool,
) {
    {
        let sync = state.xi1_frozen.entry(deviceid).or_default();
        if this_sync {
            sync.state = crate::server::Xi1SyncState::FrozenNoEvent;
        } else {
            sync.state = crate::server::Xi1SyncState::Thawed;
            if sync.other == Some(owner) {
                sync.other = None;
            }
        }
    }
    let other_dev = xi1_other_input_device(deviceid);
    let other = state.xi1_frozen.entry(other_dev).or_default();
    if other_sync {
        other.other = Some(owner);
    } else if other.other == Some(owner) {
        other.other = None;
    }
    xi1_compute_freezes(state);
}

/// Port of Xorg `FreezeThisEventIfNeededForSyncGrab`
/// (dix/events.c:4420-4447): after a key/button event is delivered
/// through the active grab, an armed FreezeNextEvent /
/// FreezeBothNextEvent state trips to FrozenWithEvent (storing the
/// event for Replay); FreezeBothNextEvent also re-holds the paired
/// device.
pub(crate) fn xi1_freeze_this_event_if_needed(
    state: &mut ServerState,
    deviceid: u16,
    owner: ClientId,
    q: &crate::server::Xi1QueuedEvent,
) {
    use crate::server::Xi1SyncState;
    let st = state
        .xi1_frozen
        .get(&deviceid)
        .map_or(Xi1SyncState::Thawed, |f| f.state);
    match st {
        Xi1SyncState::FreezeBothNextEvent => {
            let other_dev = xi1_other_input_device(deviceid);
            let other_owner = xi1_device_grab_owner(state, other_dev);
            let other = state.xi1_frozen.entry(other_dev).or_default();
            if other.state == Xi1SyncState::FreezeBothNextEvent && other_owner == Some(owner) {
                other.state = Xi1SyncState::FrozenNoEvent;
            } else {
                other.other = Some(owner);
            }
            let sync = state.xi1_frozen.entry(deviceid).or_default();
            sync.state = Xi1SyncState::FrozenWithEvent;
            sync.stored = Some(*q);
        }
        Xi1SyncState::FreezeNextEvent => {
            let sync = state.xi1_frozen.entry(deviceid).or_default();
            sync.state = Xi1SyncState::FrozenWithEvent;
            sync.stored = Some(*q);
        }
        _ => {}
    }
}

/// Deactivate the active XI1 grab on `deviceid` (UngrabDevice, passive
/// release auto-end, disconnect teardown): reset its sync state,
/// release the paired device if it was held on this grab's behalf, and
/// flush (Xorg DeactivateKeyboard/PointerGrab tail).
pub(crate) fn xi1_deactivate_device_grab(state: &mut ServerState, deviceid: u16) {
    let Some(grab) = state.xi1_active_grabs.remove(&deviceid) else {
        return;
    };
    if let Some(sync) = state.xi1_frozen.get_mut(&deviceid) {
        sync.state = crate::server::Xi1SyncState::Thawed;
        sync.stored = None;
    }
    let other_dev = xi1_other_input_device(deviceid);
    if let Some(other) = state.xi1_frozen.get_mut(&other_dev)
        && other.other == Some(grab.owner)
    {
        other.other = None;
    }
    xi1_compute_freezes(state);
}

/// Release the core-grab bridge hold on `deviceid` at GRAB
/// DEACTIVATION (core UngrabPointer / UngrabKeyboard, passive key-grab
/// auto-release): thaw the device's sync state when no XI1 grab
/// controls it and release the paired device if held on `owner`'s
/// behalf — Xorg DeactivateKeyboard/PointerGrab clears every
/// `sync.other` pointing at the dying grab.
pub(crate) fn xi1_core_grab_bridge_release(
    state: &mut ServerState,
    deviceid: u16,
    owner: ClientId,
) {
    if !state.xi1_active_grabs.contains_key(&deviceid)
        && let Some(sync) = state.xi1_frozen.get_mut(&deviceid)
    {
        sync.state = crate::server::Xi1SyncState::Thawed;
        sync.stored = None;
    }
    let other_dev = xi1_other_input_device(deviceid);
    if let Some(other) = state.xi1_frozen.get_mut(&other_dev)
        && other.other == Some(owner)
    {
        other.other = None;
    }
    xi1_compute_freezes(state);
}

/// The client owning the grab that controls `deviceid`'s sync state:
/// the XI1 device grab, or the bridged core grab (core pointer grab ↔
/// slave pointer, core keyboard grab ↔ slave keyboard) — in Xorg these
/// are one `deviceGrab.grab` slot.
pub(crate) fn xi1_device_grab_owner(state: &ServerState, deviceid: u16) -> Option<ClientId> {
    if let Some(g) = state.xi1_active_grabs.get(&deviceid) {
        return Some(g.owner);
    }
    if deviceid == crate::xinput::DEVICEID_SLAVE_POINTER {
        state.active_pointer_grab.as_ref().map(|g| g.owner)
    } else {
        state.active_keyboard_grab.as_ref().map(|g| g.owner)
    }
}

/// Find the clients receiving an XI 1.x device input event: starting
/// at the hit target, walk up the ancestor chain; the first window
/// where at least one client selected `(deviceid << 8) | evcode`
/// becomes the event window, and all clients selecting there receive
/// it. Mirrors core propagation (Xorg dix DeliverDeviceEvents); the
/// XI1 dont-propagate list is not honoured yet.
///
/// `stop_at` is the last window checked (inclusive) — Xorg
/// `DeliverDeviceEvents`'s `stopAt` argument, used when a focus window
/// caps the walk (dix/events.c:4220).
fn compute_xi1_targets_bounded(
    state: &ServerState,
    target: ResourceId,
    evcode: u8,
    deviceid: u16,
    stop_at: Option<ResourceId>,
) -> Option<(Vec<ClientId>, ResourceId)> {
    let class = (u32::from(deviceid) << 8) | u32::from(evcode);
    let mut window = target;
    loop {
        let targets: Vec<ClientId> = state
            .clients
            .iter()
            .filter(|(_, c)| {
                c.xi1_window_event_classes
                    .get(&window)
                    .is_some_and(|set| set.contains(&class))
            })
            .map(|(id, _)| ClientId(*id))
            .collect();
        if !targets.is_empty() {
            return Some((targets, window));
        }
        if window == ROOT_WINDOW || stop_at == Some(window) {
            return None;
        }
        let parent = state.resources.window(window).map(|w| w.parent)?;
        if parent == window {
            return None;
        }
        window = parent;
    }
}

fn translate_host_event(
    state: &ServerState,
    xid_map: &HostXidMap,
    event: HostPointerEvent,
) -> HostPointerEvent {
    let Some(top_level_id) = xid_map.get(&event.host_xid).copied() else {
        return event;
    };
    let Some((rx, ry)) = state
        .resources
        .window(top_level_id)
        .map(|w| (w.x + event.event_x, w.y + event.event_y))
    else {
        return event;
    };
    HostPointerEvent {
        root_x: rx,
        root_y: ry,
        ..event
    }
}

fn host_pointer_target(
    state: &ServerState,
    xid_map: &HostXidMap,
    event: HostPointerEvent,
) -> Option<(ResourceId, i16, i16)> {
    let mapped_id = xid_map.get(&event.host_xid).copied()?;
    state
        .pointer_target_at(mapped_id, event.event_x, event.event_y)
        .or(Some((mapped_id, event.event_x, event.event_y)))
}

fn natural_pointer_target(
    state: &ServerState,
    xid_map: &HostXidMap,
    event: HostPointerEvent,
) -> Option<(ResourceId, i16, i16)> {
    // Prefer the backend's host-xid routing. When the protocol tree
    // temporarily lags behind the real host window stack or shape state,
    // a pure root hit-test can fall through to an unrelated sibling
    // (e.g. Cinnamon's full-screen InputOnly guard window) even though
    // the backend is already dispatching against the correct host child.
    host_pointer_target(state, xid_map, event)
        .or_else(|| state.root_pointer_target_at(event.root_x, event.root_y))
}

fn active_grab_target(
    state: &ServerState,
) -> Option<(
    yserver_protocol::x11::ResourceId,
    ClientId,
    i32,
    i32,
    bool,
    bool,
)> {
    let (client_id, grab_window) = state.pointer_grab?;
    let target = client_target_id(state, client_id)?;
    let (gx, gy) = state.resources.window_absolute_position(grab_window);
    // `owner_events` / `via_xi2` from the active grab record. Passive
    // button-grabs (activated via try_match_passive_grab) do not
    // populate `active_pointer_grab`, so look up the matching passive
    // grab and preserve its flags; otherwise default to false (X11
    // implicit grab semantics — events report against the grab
    // window, core protocol).
    let (owner_events, via_xi2) = if state.pointer_grab_is_passive {
        state
            .button_grabs
            .iter()
            .rev()
            .find(|g| g.owner == client_id && g.grab_window == grab_window)
            .map_or((false, false), |g| (g.owner_events, g.via_xi2))
    } else {
        state
            .active_pointer_grab
            .filter(|g| g.owner == client_id)
            .map_or((false, false), |g| (g.owner_events, g.via_xi2))
    };
    Some((grab_window, target, gx, gy, owner_events, via_xi2))
}

/// Xorg `DeliverGrabbedEvent`'s `owner_events=true` natural-delivery
/// walk (dix/events.c:4361 → `DeliverDeviceEvents` with the grab as
/// client filter). Walk up from `start`; at the FIRST window where any
/// client selected `mask_bits`:
///
/// - grab client among the subscribers → natural delivery there
///   (returns the window, translated coords, and the X11 `child`);
/// - only foreign subscribers → the walk ABORTS with no delivery
///   (`TryClientEvents` dix/events.c:2069 returns -1, "not delivered
///   due to grab"; `DeliverDeviceEvents` breaks on `deliveries < 0`).
///   The caller then falls back to grab-window delivery.
fn grabbed_natural_target(
    state: &ServerState,
    start: ResourceId,
    start_x: i16,
    start_y: i16,
    mask_bits: u32,
    grab_client: ClientId,
) -> Option<(ResourceId, i16, i16, ResourceId)> {
    let mut current = start;
    let mut x = start_x;
    let mut y = start_y;
    let mut child: Option<ResourceId> = None;
    for _ in 0..256 {
        let subs = crate::core_loop::fanout::subscribers_by_id(state, current, mask_bits);
        if !subs.is_empty() {
            return subs.contains(&grab_client).then_some((
                current,
                x,
                y,
                child.unwrap_or(ResourceId(0)),
            ));
        }
        let window = state.resources.window(current)?;
        if window.parent == current {
            return None;
        }
        x = x.wrapping_add(window.x);
        y = y.wrapping_add(window.y);
        child = Some(current);
        current = window.parent;
    }
    None
}

fn release_passive_grab_on_button_release(state: &mut ServerState, kind: PointerEventKind) {
    if kind == PointerEventKind::ButtonRelease && state.pointer_grab_is_passive {
        let owner = state.pointer_grab.map(|(c, _)| c);
        state.pointer_grab = None;
        state.pointer_grab_is_passive = false;
        state.frozen_pointer_event = None;
        state.pointer_confine_to = yserver_protocol::x11::ResourceId(0);
        // Xorg DeactivatePointerGrab: releasing the grab also releases
        // the sync holds it placed (a sync keyboard_mode froze the
        // keyboard on this grab's behalf — XGrabButton-19).
        if let Some(owner) = owner {
            xi1_core_grab_bridge_release(state, crate::xinput::DEVICEID_SLAVE_POINTER, owner);
        }
    }
}

fn clamp_grab_coord(root_coord: i16, grab_origin: i32) -> i16 {
    i32::from(root_coord)
        .saturating_sub(grab_origin)
        .clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
}

fn try_match_passive_grab(
    state: &ServerState,
    xid_map: &HostXidMap,
    event: HostPointerEvent,
) -> Option<(
    crate::server::PassiveButtonGrab,
    yserver_protocol::x11::ResourceId,
)> {
    let (hit_window, _, _) = natural_pointer_target(state, xid_map, event)?;
    let grab = state.find_passive_grab(hit_window, event.detail, event.state)?;
    Some((grab, hit_window))
}

fn pointer_mask_bit(kind: PointerEventKind, state_mask: u16) -> u32 {
    match kind {
        PointerEventKind::ButtonPress => 0x0000_0004,
        PointerEventKind::ButtonRelease => 0x0000_0008,
        PointerEventKind::MotionNotify => {
            let mut bits: u32 = 0x0000_0040;
            let buttons_held = (state_mask >> 8) & 0x1f;
            if buttons_held != 0 {
                bits |= 0x0000_2000;
                for n in 0..5 {
                    if buttons_held & (1 << n) != 0 {
                        bits |= 0x0000_0100 << n;
                    }
                }
            }
            bits
        }
        PointerEventKind::EnterNotify => 0x0000_0010,
        PointerEventKind::LeaveNotify => 0x0000_0020,
    }
}

fn xi2_evtype(kind: PointerEventKind) -> u16 {
    match kind {
        PointerEventKind::ButtonPress => 4,
        PointerEventKind::ButtonRelease => 5,
        PointerEventKind::MotionNotify => 6,
        PointerEventKind::EnterNotify => 7,
        PointerEventKind::LeaveNotify => 8,
    }
}

fn xi2_raw_evtype(kind: PointerEventKind) -> Option<u16> {
    match kind {
        PointerEventKind::ButtonPress => Some(15),
        PointerEventKind::ButtonRelease => Some(16),
        PointerEventKind::MotionNotify => Some(17),
        PointerEventKind::EnterNotify | PointerEventKind::LeaveNotify => None,
    }
}

fn compute_xi2_targets(
    state: &ServerState,
    target: yserver_protocol::x11::ResourceId,
    top_level_id: yserver_protocol::x11::ResourceId,
    xi2_evtype: u16,
    xi2_raw_evtype: Option<u16>,
) -> (Vec<ClientId>, Vec<ClientId>) {
    let mut xi2_targets: Vec<ClientId> = Vec::new();
    let mut xi2_raw_targets: Vec<ClientId> = Vec::new();
    if xi2_evtype == 0 {
        return (xi2_targets, xi2_raw_targets);
    }
    for (cid_u32, c) in state.clients.iter() {
        let cid = ClientId(*cid_u32);
        let mask = xi2_mask_for_client(
            c,
            target,
            top_level_id,
            &[
                XI2_SLAVE_POINTER_DEVICE_ID,
                XI2_MASTER_POINTER_DEVICE_ID,
                1,
                0,
            ],
        );
        if mask & (1 << xi2_evtype) != 0 {
            xi2_targets.push(cid);
        }
        if let Some(raw_evtype) = xi2_raw_evtype {
            if mask & (1 << raw_evtype) != 0 {
                xi2_raw_targets.push(cid);
            }
            let root_mask = xi2_mask_for_client(
                c,
                ROOT_WINDOW,
                ROOT_WINDOW,
                &[
                    1,
                    0,
                    XI2_SLAVE_POINTER_DEVICE_ID,
                    XI2_MASTER_POINTER_DEVICE_ID,
                ],
            );
            if root_mask & (1 << raw_evtype) != 0 && !xi2_raw_targets.contains(&cid) {
                xi2_raw_targets.push(cid);
            }
        }
    }
    (xi2_targets, xi2_raw_targets)
}

#[allow(clippy::too_many_arguments)]
fn encode_pointer_event(
    buf: &mut Vec<u8>,
    order: yserver_protocol::x11::ClientByteOrder,
    kind: PointerEventKind,
    seq: SequenceNumber,
    detail: u8,
    time: u32,
    target_window: yserver_protocol::x11::ResourceId,
    child: yserver_protocol::x11::ResourceId,
    event: HostPointerEvent,
    event_x: i16,
    event_y: i16,
) {
    let pointer = x11::PointerEvent {
        sequence: seq,
        detail,
        time,
        root: ROOT_WINDOW,
        event: target_window,
        child,
        root_x: event.root_x,
        root_y: event.root_y,
        event_x,
        event_y,
        state: event.state,
    };
    match kind {
        PointerEventKind::ButtonPress => x11::encode_button_press_event(buf, order, pointer),
        PointerEventKind::ButtonRelease => x11::encode_button_release_event(buf, order, pointer),
        PointerEventKind::MotionNotify => x11::encode_motion_notify_event(
            buf,
            order,
            x11::PointerEvent {
                detail: 0,
                ..pointer
            },
        ),
        // For Crossing events, `child` and `detail` come from the
        // producer (HostPointerEvent), which has the spec-correct
        // values computed by `crossings::normal_mode_crossings` /
        // `implicit_grab_crossings`. The fanout-walk's
        // `propagation_child` is the right value for Button/Motion
        // (where it identifies the immediate descendant of the
        // propagation target on the path to the source), but NOT for
        // crossings — crossing `child` per X11 spec is per-event in
        // the chain (None on endpoints, the next inferior on virtual
        // intermediates) and the propagation walk can't know which is
        // which.
        PointerEventKind::EnterNotify => x11::encode_enter_notify_event(
            buf,
            order,
            x11::CrossingEvent {
                sequence: seq,
                time,
                root: ROOT_WINDOW,
                event: target_window,
                child: yserver_protocol::x11::ResourceId(event.child),
                root_x: event.root_x,
                root_y: event.root_y,
                event_x,
                event_y,
                state: event.state,
                detail: event.detail,
                mode: event.crossing_mode,
            },
        ),
        PointerEventKind::LeaveNotify => x11::encode_leave_notify_event(
            buf,
            order,
            x11::CrossingEvent {
                sequence: seq,
                time,
                root: ROOT_WINDOW,
                event: target_window,
                child: yserver_protocol::x11::ResourceId(event.child),
                root_x: event.root_x,
                root_y: event.root_y,
                event_x,
                event_y,
                state: event.state,
                detail: event.detail,
                mode: event.crossing_mode,
            },
        ),
    }
}

fn merge_dropped(into: &mut Vec<ClientId>, more: Vec<ClientId>) {
    for cid in more {
        if !into.contains(&cid) {
            into.push(cid);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::{ScreenSaverActive, ServerState};
    use yserver_protocol::x11::ClientId;

    /// AllowSome state machine pins (Xorg dix/events.c semantics):
    /// grab activation freezes ONCE; FreezeNextEvent re-arms; trips to
    /// FrozenWithEvent on a delivered key/button; deactivation clears
    /// the paired device's on-behalf hold.
    #[test]
    fn xi1_sync_state_machine_pins() {
        use crate::{
            server::{Xi1ActiveGrab, Xi1SyncState},
            xinput::{DEVICEID_SLAVE_KEYBOARD as KBD, DEVICEID_SLAVE_POINTER as PTR},
        };
        let mut state = ServerState::new();
        let owner = ClientId(7);
        state.xi1_active_grabs.insert(
            PTR,
            Xi1ActiveGrab {
                owner,
                deviceid: PTR,
                grab_window: crate::resources::ROOT_WINDOW,
                owner_events: false,
                this_mode: 0,
                other_mode: 0,
                passive_detail: None,
            },
        );
        // Sync grab activation: this device FrozenNoEvent, paired
        // device held on the grab's behalf.
        xi1_check_grab_for_syncs(&mut state, PTR, owner, true, true);
        assert_eq!(
            state.xi1_frozen[&PTR].state,
            Xi1SyncState::FrozenNoEvent,
            "sync this_mode freezes once at activation"
        );
        assert_eq!(state.xi1_frozen[&KBD].other, Some(owner));
        assert!(state.xi1_frozen[&PTR].frozen());
        assert!(state.xi1_frozen[&KBD].frozen(), "held via sync.other");

        // FreezeNextEvent arming + trip on a delivered button event.
        state.xi1_frozen.get_mut(&PTR).unwrap().state = Xi1SyncState::FreezeNextEvent;
        assert!(!state.xi1_frozen[&PTR].frozen(), "armed ≠ frozen");
        let q = crate::server::Xi1QueuedEvent {
            deviceid: PTR,
            evcode: crate::server::XI_FIRST_EVENT + crate::xinput::XI_DEVICE_BUTTON_PRESS_OFFSET,
            detail: 1,
            time: 1,
            root_x: 0,
            root_y: 0,
            event_x: 0,
            event_y: 0,
            state_mask: 0,
            natural_target: crate::resources::ROOT_WINDOW,
            focus_route: crate::server::Xi1FocusRoute::Walk,
            axes: None,
            replay_floor: None,
        };
        xi1_freeze_this_event_if_needed(&mut state, PTR, owner, &q);
        assert_eq!(state.xi1_frozen[&PTR].state, Xi1SyncState::FrozenWithEvent);
        assert!(state.xi1_frozen[&PTR].stored.is_some(), "Replay material");

        // Deactivation thaws this device AND releases the paired hold.
        xi1_deactivate_device_grab(&mut state, PTR);
        assert!(!state.xi1_frozen[&PTR].frozen());
        assert_eq!(state.xi1_frozen[&KBD].other, None);
        assert!(!state.xi1_frozen[&KBD].frozen());
    }

    use crate::server::ClientState;
    use std::{
        collections::{HashMap, HashSet, VecDeque},
        io::Read,
        os::unix::net::UnixStream,
        sync::{Arc, Mutex, atomic::AtomicU16},
    };

    // Duplicated from process_request.rs::tests. If you change one,
    // change both. A shared test_fixtures module is the right home
    // long-term; tracked as a follow-up.
    fn install_client(state: &mut ServerState, id: u32) -> UnixStream {
        use crate::resources::ROOT_WINDOW;
        use yserver_protocol::x11::ClientByteOrder;
        let (a, b) = UnixStream::pair().unwrap();
        state.clients.insert(
            id,
            ClientState {
                writer: Arc::new(Mutex::new(a)),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0)),
                resource_id_base: 0,
                resource_id_mask: u32::MAX,
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

    fn read_all_available(peer: &mut UnixStream) -> Vec<u8> {
        peer.set_nonblocking(true).expect("set_nonblocking");
        let mut out = Vec::new();
        let mut buf = [0u8; 512];
        loop {
            match peer.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(err) => panic!("read failed: {err}"),
            }
        }
        peer.set_nonblocking(false).expect("unset_nonblocking");
        out
    }

    fn motion_event() -> HostPointerEvent {
        HostPointerEvent {
            kind: PointerEventKind::MotionNotify,
            host_xid: 0,
            detail: 0,
            time: 1,
            root_x: 10,
            root_y: 20,
            event_x: 10,
            event_y: 20,
            state: 0,
            crossing_mode: 0,
            child: 0,
        }
    }

    #[test]
    fn pointer_fanout_prefers_host_target_over_guard_window_root_hit() {
        use crate::resources::{COMPOSITE_OVERLAY_WINDOW, ROOT_VISUAL, ROOT_WINDOW};
        use yserver_protocol::x11::{CreateWindowRequest, ResourceId, xfixes};

        let mut state = ServerState::new();
        let mut peer = install_client(&mut state, 1);

        let cow_host_xid = crate::backend::WindowHandle::from_raw_panicking(0x4000_0103);
        state.resources.materialize_cow_resource(cow_host_xid);

        let stage = ResourceId(0x0010_0050);
        state.resources.create_window(
            ClientId(1),
            CreateWindowRequest {
                depth: 24,
                window: stage,
                parent: COMPOSITE_OVERLAY_WINDOW,
                x: 0,
                y: 0,
                width: 100,
                height: 100,
                border_width: 0,
                class: 1,
                visual: ROOT_VISUAL,
                ..Default::default()
            },
        );
        let _ = state.resources.map_window(stage);

        let guard = ResourceId(0x0010_0060);
        state.resources.create_window(
            ClientId(1),
            CreateWindowRequest {
                depth: 24,
                window: guard,
                parent: ROOT_WINDOW,
                x: 0,
                y: 0,
                width: 100,
                height: 100,
                border_width: 0,
                class: 2,
                visual: ROOT_VISUAL,
                override_redirect: Some(true),
                ..Default::default()
            },
        );
        let _ = state.resources.map_window(guard);

        state
            .shape_windows
            .entry(COMPOSITE_OVERLAY_WINDOW)
            .or_default()
            .input = Some(Vec::<xfixes::RegionRect>::new());
        state.shape_windows.entry(stage).or_default().input =
            Some(Vec::<xfixes::RegionRect>::new());

        state
            .clients
            .get_mut(&1)
            .unwrap()
            .event_masks
            .insert(stage, 0x0000_0040);

        let event = HostPointerEvent {
            host_xid: 0x4000_0009,
            root_x: 50,
            root_y: 50,
            event_x: 50,
            event_y: 50,
            ..motion_event()
        };

        let (protocol_hit, _, _) = state.root_pointer_target_at(50, 50).expect("root hit");
        assert_eq!(
            protocol_hit, guard,
            "empty-input COW should let the protocol root hit-test fall through to the guard",
        );

        let mut xid_map = HostXidMap::new();
        xid_map.insert(0x4000_0009, stage);
        let (natural_hit, _, _) =
            natural_pointer_target(&state, &xid_map, event).expect("natural hit");
        assert_eq!(
            natural_hit, stage,
            "host-backed pointer targeting must stay on the stage even while the protocol root hit-test sees the guard",
        );

        let mut backend = crate::backend::recording::RecordingBackend::default();
        let _ =
            pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, event, true, false);

        let bytes = read_all_available(&mut peer);
        assert!(
            bytes.len() >= 32,
            "expected a MotionNotify event for the stage subscriber; got {} bytes",
            bytes.len(),
        );
        assert_eq!(bytes[0], 6, "core delivery should be MotionNotify");
        assert_eq!(
            &bytes[12..16],
            &stage.0.to_le_bytes(),
            "MotionNotify must report the stage window, not the guard window",
        );
    }

    /// wmaker wedge regression (2026-06-04, silence HW): a WM places a
    /// SYNCHRONOUS `owner_events=true` button grab on a CLIENT's window
    /// (click-to-focus). A press on that window's subtree must be
    /// reported to the GRAB CLIENT on the grab window — per Xorg
    /// `DeliverGrabbedEvent` (dix/events.c:4361), the `owner_events`
    /// natural walk is filtered to the grab client: `TryClientEvents`
    /// (dix/events.c:2069) returns -1 for any other client ("not
    /// delivered due to grab"), aborting propagation, and the event
    /// falls back to the grab window. Pre-fix, the descendant arm of
    /// `target_qualifies_for_natural` leaked the press to the app
    /// client while the sync grab froze the queue — the WM never saw
    /// the press, never called AllowEvents, and the pointer stream
    /// stayed frozen forever (cursor moves, clicks dead).
    #[test]
    fn passive_sync_grab_on_foreign_window_delivers_to_grab_client_and_freezes() {
        use yserver_protocol::x11::ResourceId;

        let mut state = ServerState::new();
        let grab_window = ResourceId(0x0020_0001); // app client's top-level
        let child_window = ResourceId(0x0020_0002); // app client's child

        let mut wm_peer = install_client(&mut state, 1);
        let mut app_peer = install_client(&mut state, 2);

        state.resources.create_window(
            ClientId(2),
            yserver_protocol::x11::CreateWindowRequest {
                depth: 24,
                window: grab_window,
                parent: crate::resources::ROOT_WINDOW,
                x: 0,
                y: 0,
                width: 100,
                height: 100,
                border_width: 0,
                class: 1,
                visual: crate::resources::ROOT_VISUAL,
                ..Default::default()
            },
        );
        state.resources.create_window(
            ClientId(2),
            yserver_protocol::x11::CreateWindowRequest {
                depth: 24,
                window: child_window,
                parent: grab_window,
                x: 10,
                y: 10,
                width: 40,
                height: 40,
                border_width: 0,
                class: 1,
                visual: crate::resources::ROOT_VISUAL,
                ..Default::default()
            },
        );
        let _ = state.resources.map_window(grab_window);
        let _ = state.resources.map_window(child_window);

        // The WM has NO event mask anywhere on the chain — its
        // interest is expressed solely via the grab. The app client
        // selects ButtonPress on its child (the leak target pre-fix).
        state
            .clients
            .get_mut(&2)
            .unwrap()
            .event_masks
            .insert(child_window, 0x0000_0004);

        // wmaker idiom: XGrabButton(AnyButton, AnyModifier, client_win,
        // owner_events=True, ButtonPressMask, GrabModeSync,
        // GrabModeAsync).
        state.button_grabs.push(crate::server::PassiveButtonGrab {
            owner: ClientId(1),
            grab_window,
            button: 0,         // AnyButton
            modifiers: 0x8000, // AnyModifier
            owner_events: true,
            event_mask: 0x0000_0004, // ButtonPressMask
            pointer_mode: 0,         // GrabModeSync
            keyboard_mode: 1,
            confine_to: ResourceId(0),
            via_xi2: false,
        });

        let mut xid_map = HostXidMap::new();
        xid_map.insert(0xCAFE_u32, grab_window);
        let mut backend = crate::backend::recording::RecordingBackend::default();

        let _ = pointer_event_fanout_to_state(
            &mut state,
            &mut backend,
            &xid_map,
            HostPointerEvent {
                kind: PointerEventKind::ButtonPress,
                host_xid: 0xCAFE,
                detail: 1,
                time: 0,
                root_x: 20,
                root_y: 20,
                event_x: 20,
                event_y: 20,
                state: 0,
                crossing_mode: 0,
                child: 0,
            },
            true,
            false,
        );

        let wm_bytes = read_all_available(&mut wm_peer);
        assert!(
            wm_bytes.len() >= 32,
            "sync passive grab must deliver the activating press to the \
             grab client (the WM) — otherwise nobody ever AllowEvents and \
             the frozen pointer queue wedges; got {} bytes",
            wm_bytes.len(),
        );
        assert_eq!(wm_bytes[0], 4, "event type should be ButtonPress");
        assert_eq!(
            &wm_bytes[12..16],
            &grab_window.0.to_le_bytes(),
            "press must be reported on the grab window (Xorg grab-window \
             fallback — the grab client has no mask on the natural chain)",
        );

        let app_bytes = read_all_available(&mut app_peer);
        let app_core: Vec<&[u8]> = app_bytes.chunks(32).filter(|c| c[0] == 4).collect();
        assert!(
            app_core.is_empty(),
            "the app client must NOT see the core press while the grab \
             holds (Xorg TryClientEvents: 'not delivered due to grab'); \
             got {} core ButtonPress event(s)",
            app_core.len(),
        );

        assert!(
            state.frozen_pointer_event.is_some(),
            "GrabModeSync activation must freeze the pointer queue",
        );
        assert_eq!(
            state.pointer_grab,
            Some((ClientId(1), grab_window)),
            "passive grab must be active for client 1",
        );
    }

    #[test]
    fn pointer_event_resets_dpms_last_activity() {
        use std::time::{Duration, Instant};
        let mut state = ServerState::new();
        state.dpms.kms_capable = true;
        state.dpms.enabled = true;
        state.dpms.last_activity = Instant::now() - Duration::from_secs(10);
        let stale = state.dpms.last_activity;
        let xid_map = HostXidMap::new();
        let mut backend = crate::backend::recording::RecordingBackend::default();

        let _ = pointer_event_fanout_to_state(
            &mut state,
            &mut backend,
            &xid_map,
            motion_event(),
            true,
            false,
        );

        let elapsed = state.dpms.last_activity.duration_since(stale);
        assert!(
            elapsed > Duration::from_secs(9),
            "last_activity should be ≈now, not stale"
        );
    }

    #[test]
    fn pointer_event_during_off_wakes_via_set_dpms_power_on() {
        let mut state = ServerState::new();
        state.dpms.kms_capable = true;
        state.dpms.enabled = true;
        state.dpms.power_level = 3; // Off
        let xid_map = HostXidMap::new();
        let mut backend = crate::backend::recording::RecordingBackend::default();

        let _ = pointer_event_fanout_to_state(
            &mut state,
            &mut backend,
            &xid_map,
            motion_event(),
            true,
            false,
        );

        let calls = backend.calls.lock().unwrap().clone();
        assert!(
            calls
                .iter()
                .any(|c| matches!(c, crate::backend::recording::RecordedCall::SetDpmsPower(0))),
            "wake must call set_dpms_power(0); got {calls:?}"
        );
        assert_eq!(
            state.dpms.power_level, 0,
            "in-memory level should be On after wake"
        );
    }

    #[test]
    fn pointer_event_during_off_with_backend_error_still_advances_state() {
        let mut state = ServerState::new();
        state.dpms.kms_capable = true;
        state.dpms.enabled = true;
        state.dpms.power_level = 3;
        let xid_map = HostXidMap::new();
        let mut backend = crate::backend::recording::RecordingBackend::default();
        backend.dpms_set_returns_err = true;

        let _ = pointer_event_fanout_to_state(
            &mut state,
            &mut backend,
            &xid_map,
            motion_event(),
            true,
            false,
        );

        assert_eq!(
            state.dpms.power_level, 0,
            "state must advance on backend error"
        );
    }

    #[test]
    fn pointer_event_during_screen_saver_on_flips_off_via_independent_path() {
        // Standalone SS-On (DPMS still On). Motion event must flip
        // SS Off with forced=0.
        let mut state = ServerState::new();
        state.dpms.kms_capable = true;
        state.dpms.enabled = true;
        // dpms.power_level already 0 from new()
        state.screensaver.active = ScreenSaverActive::On;
        state.screensaver.selected_by.insert(ClientId(1), 0x01);
        let xid_map = HostXidMap::new();
        let mut backend = crate::backend::recording::RecordingBackend::default();

        let _ = pointer_event_fanout_to_state(
            &mut state,
            &mut backend,
            &xid_map,
            motion_event(),
            true,
            false,
        );

        assert_eq!(state.screensaver.active, ScreenSaverActive::Off);
        assert!(!state.screensaver.forced, "input-driven Off is non-forced");
    }

    #[test]
    fn pointer_event_updates_global_and_per_device_vcp_last_activity() {
        use std::time::Duration;
        let mut state = ServerState::new();
        state.dpms.last_activity = std::time::Instant::now() - Duration::from_secs(30);
        let stale = state.dpms.last_activity;
        let xid_map = HostXidMap::new();
        let mut backend = crate::backend::recording::RecordingBackend::default();

        let _ = pointer_event_fanout_to_state(
            &mut state,
            &mut backend,
            &xid_map,
            motion_event(),
            true,
            false,
        );

        assert!(
            state.dpms.last_activity > stale,
            "global last_activity advanced"
        );
        let vcp = state
            .per_device_last_activity
            .get(&2)
            .copied()
            .expect("VCP per-device entry inserted");
        assert!(vcp > stale, "VCP per-device last_activity advanced");
    }

    #[test]
    fn pointer_event_fires_neg_transition_alarm_when_prior_idle_crosses_threshold() {
        use std::time::Duration;
        use yserver_protocol::x11::sync as x11sync;
        let mut state = ServerState::new();
        // User idle for 90s, NegativeTransition alarm at 60s.
        state.dpms.last_activity = std::time::Instant::now() - Duration::from_secs(90);
        state
            .per_device_last_activity
            .insert(2, std::time::Instant::now() - Duration::from_secs(90));
        let alarm_id = 0x4000;
        state.sync_alarms.insert(
            alarm_id,
            crate::server::SyncAlarm {
                owner: ClientId(1),
                counter: x11sync::IDLETIME_DEVICE_VCP,
                wait_value: 60_000,
                delta: 0,
                test_type: x11sync::TEST_NEGATIVE_TRANSITION as u8,
                events: false,
                state: x11sync::ALARM_STATE_ACTIVE,
            },
        );
        let xid_map = HostXidMap::new();
        let mut backend = crate::backend::recording::RecordingBackend::default();

        let _ = pointer_event_fanout_to_state(
            &mut state,
            &mut backend,
            &xid_map,
            motion_event(),
            true,
            false,
        );

        // Alarm stays Active (Transition + delta=0 — Task 2 fix); cache reflects post-wake idle=0.
        assert_eq!(
            state.sync_alarms[&alarm_id].state,
            x11sync::ALARM_STATE_ACTIVE
        );
        assert_eq!(
            state
                .idletime_last_evaluated
                .get(&x11sync::IDLETIME_DEVICE_VCP)
                .copied(),
            Some(0),
            "post-wake last_evaluated should be 0"
        );
    }

    #[test]
    fn pointer_event_fires_neg_transition_alarm_on_per_device_idletime_vcp() {
        use std::time::Duration;
        use yserver_protocol::x11::sync as x11sync;
        let mut state = ServerState::new();
        let mut peer = install_client(&mut state, 1);
        state.dpms.last_activity = std::time::Instant::now() - Duration::from_secs(90);
        state
            .per_device_last_activity
            .insert(2, std::time::Instant::now() - Duration::from_secs(90));
        let alarm_id = 0x5000;
        state.sync_alarms.insert(
            alarm_id,
            crate::server::SyncAlarm {
                owner: ClientId(1),
                counter: x11sync::IDLETIME_DEVICE_VCP,
                wait_value: 60_000,
                delta: 0,
                test_type: x11sync::TEST_NEGATIVE_TRANSITION as u8,
                events: true, // load-bearing
                state: x11sync::ALARM_STATE_ACTIVE,
            },
        );
        let xid_map = HostXidMap::new();
        let mut backend = crate::backend::recording::RecordingBackend::default();

        let _ = pointer_event_fanout_to_state(
            &mut state,
            &mut backend,
            &xid_map,
            motion_event(),
            true,
            false,
        );

        // PRIMARY: AlarmNotify event type 84.
        let bytes = read_all_available(&mut peer);
        // AlarmNotify is a 32-byte sequential event; type byte at offset 0.
        assert!(
            bytes.len() >= 32,
            "expected AlarmNotify event (32B); got {} bytes",
            bytes.len()
        );
        assert_eq!(
            bytes[0], 84,
            "AlarmNotify event type (SYNC_FIRST_EVENT + 1)"
        );
        assert_eq!(bytes[1], 1, "AlarmNotify kind = AlarmNotify (1)");
        assert_eq!(
            state.sync_alarms[&alarm_id].state,
            x11sync::ALARM_STATE_ACTIVE
        );
        assert_eq!(
            state
                .idletime_last_evaluated
                .get(&x11sync::IDLETIME_DEVICE_VCP)
                .copied(),
            Some(0)
        );
    }
}
