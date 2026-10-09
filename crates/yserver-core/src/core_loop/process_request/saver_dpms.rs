use super::*;

/// Apply a DPMS level transition. Updates `state.dpms.power_level`
/// **first** (so the page-flip gate trips immediately and an
/// interleaved composite tick can't submit a flip into a CRTC the
/// kernel is about to disable), then calls the backend, then emits
/// a notify iff the level actually changed. Backend errors are
/// logged but do not propagate — matches Xorg's "set it anyway"
/// posture (`Xext/dpms.c:262-293`).
pub(crate) fn apply_dpms_transition(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    new_level: u8,
) {
    let old = state.dpms.power_level;
    // ALWAYS call the backend, even when new_level == old. Matches Xorg
    // DPMSSet (dpms.c:262-293), which gates only the notify on level
    // change but always runs the per-screen DPMS hook. Two reasons this
    // matters:
    //   (1) Recovery via reissue. If a previous transition advanced
    //       state.dpms.power_level but the backend errored (per the
    //       error contract, state advances anyway), a client can send
    //       DPMSForceLevel(same_level) to retry. The KMS backend's
    //       kms_outputs_active guard short-circuits idempotently when
    //       state IS consistent, and re-attempts when desynced.
    //   (2) Spam suppression is already handled at the backend level:
    //       KmsBackend::set_dpms_power early-returns when want_active
    //       equals self.kms_outputs_active.
    if new_level != old {
        log::info!(
            "dpms: apply_dpms_transition {old} → {new_level} (enabled={}, kms_capable={})",
            state.dpms.enabled,
            state.dpms.kms_capable,
        );
    }
    state.dpms.power_level = new_level;

    // SS coupling — fires BEFORE backend hook and BEFORE DPMS notify
    // (Xorg dpms.c:262-279 ordering).
    //   non-On + SS Off → SCREEN_SAVER_FORCER + Active → forced=true
    //   On     + SS On  → SCREEN_SAVER_OFF    + Reset  → forced=false
    // Neither path resets last_activity (Xorg's NoticeTime only runs
    // for the FORCER+Reset combination, window.c:3187-3193).
    let dpms_on = new_level == 0;
    match (dpms_on, state.screensaver.active) {
        (false, ScreenSaverActive::Off) => {
            apply_screen_saver_transition(
                state,
                backend,
                ScreenSaverActive::On,
                /*forced=*/ true,
            );
        }
        (true, ScreenSaverActive::On) => {
            apply_screen_saver_transition(
                state,
                backend,
                ScreenSaverActive::Off,
                /*forced=*/ false,
            );
        }
        _ => {}
    }

    if let Err(e) = backend.set_dpms_power(new_level) {
        log::error!("set_dpms_power({new_level}) failed: {e}");
        // Intentionally swallowed: state still advances. See spec
        // "Backend hook / Error contract".
    }
    if new_level != old {
        emit_dpms_notify(state);
    }
}

/// Fan a `DPMSInfoNotify` GenericEvent out to every client
/// currently subscribed via `DPMSSelectInput(DPMS_INFO_NOTIFY_MASK)`.
/// Uses the existing `fanout_event_to_clients` helper for sequence
/// + byte-order + per-client write_or_buffer handling.
pub(crate) fn emit_dpms_notify(state: &mut ServerState) {
    use crate::nested::DPMS_MAJOR_OPCODE;
    use yserver_protocol::x11::dpms as x11dpms;
    if state.dpms.selected_by.is_empty() {
        return;
    }
    let ts = state.timestamp_now();
    let level = u16::from(state.dpms.power_level);
    let enabled = state.dpms.enabled;
    let subscribers: Vec<ClientId> = state.dpms.selected_by.iter().copied().collect();
    let dropped = crate::core_loop::fanout::fanout_event_to_clients(
        state,
        &subscribers,
        |buf, seq, order| {
            x11dpms::encode_dpms_info_notify_event(
                buf,
                order,
                seq,
                DPMS_MAJOR_OPCODE,
                ts,
                level,
                enabled,
            );
        },
    );
    // Reap subscribers whose outbound buffer overflowed — they're
    // already on a path to disconnect via run.rs::reconcile_client_
    // writable_interest, and keeping them in selected_by would burn
    // CPU on every subsequent notify until that finishes.
    for cid in dropped {
        state.dpms.selected_by.remove(&cid);
    }
}

/// `ScreenSaverSetAttributes` (`Xext/saver.c:734-1073`): CreateWindow's
/// checks against the root, then BadAccess when another client holds
/// the attributes; else they are this client's, replacing its own.
fn screen_saver_set_attributes(
    state: &mut ServerState,
    client_id: ClientId,
    body: &[u8],
) -> Result<(), (u8, u32)> {
    use crate::server::SaverAttributes;
    if body.len() < 24 {
        return Err((x11::error::BAD_LENGTH, 0));
    }
    let u16_at = |i: usize| u16::from_le_bytes([body[i], body[i + 1]]);
    let u32_at = |i: usize| u32::from_le_bytes([body[i], body[i + 1], body[i + 2], body[i + 3]]);
    let drawable = u32_at(0);
    let (x, y) = (u16_at(4) as i16, u16_at(6) as i16);
    let (width, height, border_width) = (u16_at(8), u16_at(10), u16_at(12));
    let (class, depth, visual, value_mask) = (body[14], body[15], u32_at(16), u32_at(20));
    if !drawable_exists(state, ResourceId(drawable)) {
        return Err((x11::error::BAD_DRAWABLE, drawable));
    }
    let values: Vec<u32> = body[24..].chunks_exact(4).map(u32_at_slice).collect();
    if usize::try_from(value_mask.count_ones()).ok() != Some(values.len()) {
        return Err((x11::error::BAD_LENGTH, 0));
    }
    if width == 0 || height == 0 {
        return Err((x11::error::BAD_VALUE, 0));
    }
    // The root is InputOutput, of the root depth and visual.
    let effective_class = match class {
        0 | 1 => 1,
        2 => 2,
        other => return Err((x11::error::BAD_VALUE, u32::from(other))),
    };
    if effective_class == 2 && (border_width != 0 || depth != 0) {
        return Err((x11::error::BAD_MATCH, 0));
    }
    let depth = if effective_class == 1 && depth == 0 {
        crate::resources::ROOT_DEPTH
    } else {
        depth
    };
    let visual = if visual == 0 {
        crate::resources::ROOT_VISUAL.0
    } else {
        visual
    };
    if (visual != crate::resources::ROOT_VISUAL.0 || depth != crate::resources::ROOT_DEPTH)
        && !state.resources.is_known_visual(ResourceId(visual))
    {
        return Err((x11::error::BAD_MATCH, 0));
    }
    const CW_BORDER: u32 = 0x0004 | 0x0008;
    const CW_COLORMAP: u32 = 0x2000;
    if value_mask & CW_BORDER == 0 && effective_class != 2 && depth != crate::resources::ROOT_DEPTH
    {
        return Err((x11::error::BAD_MATCH, 0));
    }
    if value_mask & CW_COLORMAP == 0
        && effective_class != 2
        && visual != crate::resources::ROOT_VISUAL.0
    {
        return Err((x11::error::BAD_MATCH, 0));
    }
    if state
        .screensaver
        .attributes
        .as_ref()
        .is_some_and(|a| a.client != client_id)
    {
        return Err((x11::error::BAD_ACCESS, 0));
    }
    state.screensaver.attributes = Some(SaverAttributes {
        client: client_id,
        x,
        y,
        width,
        height,
        border_width,
        class,
        depth,
        visual,
        value_mask,
        values,
    });
    Ok(())
}

fn u32_at_slice(c: &[u8]) -> u32 {
    u32::from_le_bytes([c[0], c[1], c[2], c[3]])
}

/// `ScreenSaverUnsetAttributes` (`Xext/saver.c:1075-1096`), and a client
/// going away (`ScreenSaverFreeAttr`): its attributes are dropped, and
/// the saver window with them.
pub(crate) fn unset_screen_saver_attributes(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    client_id: ClientId,
) {
    if state
        .screensaver
        .attributes
        .as_ref()
        .is_some_and(|a| a.client == client_id)
    {
        state.screensaver.attributes = None;
        // `ScreenSaverFreeAttr` (`Xext/saver.c:333-355`): a shown window
        // goes by resetting the saver and starting it again.
        if state.screensaver.window_shown {
            apply_screen_saver_transition(state, backend, ScreenSaverActive::Off, true);
            apply_screen_saver_transition(state, backend, ScreenSaverActive::On, true);
        }
    }
}

/// `CreateSaverWindow` (`Xext/saver.c:466-564`): the attributes as a
/// server-owned, override-redirect child of the root, mapped.
fn create_screen_saver_window(state: &mut ServerState, backend: &mut dyn Backend) {
    destroy_screen_saver_window(state, backend);
    let Some(attrs) = state.screensaver.attributes.clone() else {
        return;
    };
    const CW_OVERRIDE_REDIRECT: u32 = 0x0200;
    let mask = attrs.value_mask | CW_OVERRIDE_REDIRECT;
    let mut values = Vec::with_capacity(attrs.values.len() + 1);
    let mut given = attrs.values.iter();
    for bit in 0..15u32 {
        let b = 1 << bit;
        if mask & b == 0 {
            continue;
        }
        if b == CW_OVERRIDE_REDIRECT {
            if attrs.value_mask & b != 0 {
                given.next();
            }
            values.push(1);
        } else if let Some(v) = given.next() {
            values.push(*v);
        }
    }
    // Xorg keeps the values as `unsigned long` and hands them to
    // CreateWindow as an `XID *` (`Xext/saver.c:494`): on a 64-bit server
    // every other XID it reads is the high half of the one before, 0.
    // dtsession's mask-0 window (override-redirect alone) is unaffected;
    // a mask with background pixel loses its override-redirect.
    let values: Vec<u32> = values
        .iter()
        .flat_map(|v| [*v, 0])
        .take(values.len())
        .collect();
    let window = crate::resources::SCREEN_SAVER_WINDOW;
    let mut body = Vec::with_capacity(28 + 4 * values.len());
    body.extend_from_slice(&window.0.to_le_bytes());
    body.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
    for v in [
        attrs.x as u16,
        attrs.y as u16,
        attrs.width,
        attrs.height,
        attrs.border_width,
    ] {
        body.extend_from_slice(&v.to_le_bytes());
    }
    body.extend_from_slice(&u16::from(attrs.class).to_le_bytes());
    body.extend_from_slice(&attrs.visual.to_le_bytes());
    body.extend_from_slice(&mask.to_le_bytes());
    for v in &values {
        body.extend_from_slice(&v.to_le_bytes());
    }
    let header = RequestHeader {
        opcode: 1,
        data: attrs.depth,
        length_units: u32::try_from(1 + body.len() / 4).unwrap_or(u32::MAX),
    };
    let server = crate::resources::SERVER_OWNER;
    let _ = handle_create_window(
        state,
        backend,
        None,
        server,
        SequenceNumber(0),
        header,
        &body,
    );
    if state.resources.window(window).is_none() {
        return;
    }
    let _ = handle_map_window(
        state,
        backend,
        None,
        server,
        SequenceNumber(0),
        &window.0.to_le_bytes(),
    );
    state.screensaver.window_shown = true;
}

/// `DestroySaverWindow` (`Xext/saver.c:566-584`).
fn destroy_screen_saver_window(state: &mut ServerState, backend: &mut dyn Backend) {
    if !state.screensaver.window_shown {
        return;
    }
    state.screensaver.window_shown = false;
    if state
        .resources
        .window(crate::resources::SCREEN_SAVER_WINDOW)
        .is_some()
    {
        destroy_window_subtree(state, backend, None, crate::resources::SCREEN_SAVER_WINDOW);
    }
}

/// Transition the screensaver to `new` (must be `Off` or `On`).
/// `Cycle` is an event-only value — it never appears in
/// `screensaver.active`. Passing it here is a programmer error; the
/// helper debug-asserts and no-ops in release builds. Idempotent on
/// same-state. On every Off↔On transition updates
/// `screensaver.active`, `screensaver.forced`, and
/// `screensaver.next_cycle`, then fires `ScreenSaverNotify` to
/// `SCREEN_SAVER_NOTIFY_MASK` subscribers. The `backend` parameter
/// is reserved for signature parity with `apply_dpms_transition`
/// and is currently unused (SS is purely server-side bookkeeping).
pub(crate) fn apply_screen_saver_transition(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    new: ScreenSaverActive,
    forced: bool,
) {
    debug_assert!(
        !matches!(new, ScreenSaverActive::Cycle),
        "Cycle is event-only and must never be written into screensaver.active — \
         route it through emit_screen_saver_notify instead"
    );
    if matches!(new, ScreenSaverActive::Cycle) {
        return; // release-build safety net
    }
    if state.screensaver.active == new {
        return;
    }
    state.screensaver.active = new;
    state.screensaver.forced = forced;
    state.screensaver.next_cycle = match new {
        ScreenSaverActive::On if state.screensaver.interval_ms > 0 => Some(
            std::time::Instant::now()
                + std::time::Duration::from_millis(u64::from(state.screensaver.interval_ms)),
        ),
        _ => None,
    };
    // `ScreenSaverHandle` (`Xext/saver.c:586-614`): the window first,
    // then the notify that names it.
    match new {
        ScreenSaverActive::On => create_screen_saver_window(state, backend),
        _ => destroy_screen_saver_window(state, backend),
    }
    emit_screen_saver_notify(state, new, forced);
}

/// `kind` of QueryInfo and ScreenSaverNotify: External while a client's
/// attributes are set (`Xext/saver.c:411-416`).
fn screen_saver_kind(state: &ServerState) -> u8 {
    use yserver_protocol::x11::screensaver as x11ss;
    if state.screensaver.attributes.is_some() {
        x11ss::SCREEN_SAVER_EXTERNAL
    } else if state.screensaver.prefer_blanking {
        x11ss::SCREEN_SAVER_BLANKED
    } else {
        x11ss::SCREEN_SAVER_INTERNAL
    }
}

/// Fan a `ScreenSaverNotify` event out to subscribers. `notify_state`
/// and `forced` are passed explicitly so the cycle-timer path can
/// fire `Cycle` events without mutating `screensaver.active`. Cycle
/// events deliver to `CYCLE_MASK` subscribers; Off/On events deliver
/// to `NOTIFY_MASK` subscribers (Xorg `saver.c:389-391`).
pub(crate) fn emit_screen_saver_notify(
    state: &mut ServerState,
    notify_state: ScreenSaverActive,
    forced: bool,
) {
    use crate::nested::MIT_SCREEN_SAVER_FIRST_EVENT;
    use yserver_protocol::x11::screensaver as x11ss;
    let (active_state, deliver_mask) = match notify_state {
        ScreenSaverActive::Off => (x11ss::SCREEN_SAVER_OFF, x11ss::SCREEN_SAVER_NOTIFY_MASK),
        ScreenSaverActive::On => (x11ss::SCREEN_SAVER_ON, x11ss::SCREEN_SAVER_NOTIFY_MASK),
        ScreenSaverActive::Cycle => (x11ss::SCREEN_SAVER_CYCLE, x11ss::SCREEN_SAVER_CYCLE_MASK),
    };
    let subs: Vec<ClientId> = state
        .screensaver
        .selected_by
        .iter()
        .filter(|(_, mask)| **mask & deliver_mask != 0)
        .map(|(c, _)| *c)
        .collect();
    if subs.is_empty() {
        return;
    }
    let ts = state.timestamp_now();
    let root = crate::resources::ROOT_WINDOW.0;
    let kind = screen_saver_kind(state);
    let dropped =
        crate::core_loop::fanout::fanout_event_to_clients(state, &subs, |buf, seq, order| {
            x11ss::encode_screen_saver_notify_event(
                buf,
                order,
                seq,
                MIT_SCREEN_SAVER_FIRST_EVENT,
                active_state,
                ts,
                root,
                crate::resources::SCREEN_SAVER_WINDOW.0,
                kind,
                forced,
            );
        });
    for cid in dropped {
        state.screensaver.selected_by.remove(&cid);
    }
}

/// Current idle value for an IDLETIME-family counter, expressed as
/// milliseconds since `state.idletime_baseline(counter)`. Used by
/// CREATE_ALARM / CHANGE_ALARM (Relative values, trigger checks) for
/// counter values that aren't stored in `state.sync_counters`.
pub(crate) fn idletime_current_idle(state: &ServerState, counter: u32) -> i64 {
    let baseline = state.idletime_baseline(counter);
    #[allow(clippy::cast_possible_truncation)]
    let v = std::time::Instant::now()
        .duration_since(baseline)
        .as_millis()
        .min(u128::from(u32::MAX)) as i64;
    v
}

/// Evaluate Negative-* alarms on IDLETIME-family counters after input
/// wake. Called from the key + pointer fanout prologues immediately
/// after `last_activity` is updated to "now". The semantic is:
/// `old_idle = (now - prior_last_activity)`, `new_idle = 0`. Fires any
/// alarms whose trigger crosses on this transition.
///
/// `device_id` identifies which per-device counter to evaluate; pass
/// 2 for pointer events (drives IDLETIME_DEVICE_VCP), 3 for key events
/// (drives IDLETIME_DEVICE_VCK). The global IDLETIME counter is
/// evaluated unconditionally because any input resets the global
/// last_activity.
pub(crate) fn evaluate_idletime_negative_alarms_on_input_wake(
    state: &mut ServerState,
    device_id: u8,
    prior_global_idle_ms: i64,
    prior_device_idle_ms: i64,
) {
    use yserver_protocol::x11::sync as x11sync;
    // Suspend gate (Xorg WaitFor.c:519). When XScreenSaverSuspend is
    // held, the unified timer is not armed in either direction —
    // including the input-wake firing of Negative-* alarms. Without
    // this gate, an input event during fullscreen video would still
    // fire MATE's wake alarms and could prompt mate-power-manager to
    // re-arm screen-blanking too aggressively.
    if !state.screensaver.suspend_counts.is_empty() {
        return;
    }
    // Global IDLETIME: always reset on any input.
    crate::core_loop::sync_await::counter_changed(
        state,
        x11sync::IDLETIME_COUNTER,
        prior_global_idle_ms,
        0,
    );
    state
        .idletime_last_evaluated
        .insert(x11sync::IDLETIME_COUNTER, 0);

    // Per-device IDLETIME: only the affected device resets.
    let device_counter = match device_id {
        2 => x11sync::IDLETIME_DEVICE_VCP,
        3 => x11sync::IDLETIME_DEVICE_VCK,
        _ => return,
    };
    crate::core_loop::sync_await::counter_changed(state, device_counter, prior_device_idle_ms, 0);
    state.idletime_last_evaluated.insert(device_counter, 0);
}

/// Reset IDLETIME bookkeeping when the suspend-counts table drains to
/// empty. Must be called at the SAME spot the existing MIT-SCREEN-SAVER
/// code resets `state.dpms.last_activity` (SS Suspend handler +
/// process_disconnect's last-suspender cleanup).
///
/// Without this, IDLETIME alarms can be skipped post-suspend because
/// `idletime_last_evaluated` holds stale-high values from before the
/// suspend, causing `evaluate_alarms_for_counter`'s
/// `old < wait <= new` Transition check to never hold.
pub(crate) fn reset_idletime_state_after_suspend_release(state: &mut ServerState) {
    let now = std::time::Instant::now();
    // dpms.last_activity is already reset by the caller. Reset the
    // per-device baselines so post-resume per-device IDLETIME
    // computation is consistent.
    for entry in state.per_device_last_activity.values_mut() {
        *entry = now;
    }
    // Clear the evaluator's last-seen cache so the next post-poll pass
    // computes `(old=0, new=current)` from a clean slate, preserving
    // the Transition `old < wait <= new` invariant.
    state.idletime_last_evaluated.clear();
}

pub(super) fn handle_dpms_request(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    use crate::nested::DPMS_MAJOR_OPCODE;
    use yserver_protocol::x11::{ClientByteOrder, dpms as x11dpms};
    let byte_order = state
        .clients
        .get(&client_id.0)
        .map_or(ClientByteOrder::LittleEndian, |c| c.byte_order);
    let minor = header.data;
    debug!(
        "client {} #{} DPMS::minor={} body_len={}",
        client_id.0,
        sequence.0,
        minor,
        body.len()
    );
    match minor {
        x11dpms::GET_VERSION => {
            let reply = x11dpms::encode_get_version_reply(
                byte_order,
                sequence,
                x11dpms::MAJOR_VERSION,
                x11dpms::MINOR_VERSION,
            );
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            return Ok(write_to_client(client, client_id, &reply));
        }
        x11dpms::CAPABLE => {
            let reply = x11dpms::encode_capable_reply(byte_order, sequence, state.dpms.kms_capable);
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            return Ok(write_to_client(client, client_id, &reply));
        }
        x11dpms::GET_TIMEOUTS => {
            #[allow(clippy::cast_possible_truncation)]
            let s = (state.dpms.standby_ms / 1000) as u16;
            #[allow(clippy::cast_possible_truncation)]
            let su = (state.dpms.suspend_ms / 1000) as u16;
            #[allow(clippy::cast_possible_truncation)]
            let o = (state.dpms.off_ms / 1000) as u16;
            let reply = x11dpms::encode_get_timeouts_reply(byte_order, sequence, s, su, o);
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            return Ok(write_to_client(client, client_id, &reply));
        }
        x11dpms::SET_TIMEOUTS => {
            let Some((standby, suspend, off)) = x11dpms::parse_set_timeouts_request(body) else {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_LENGTH,
                    0,
                    u16::from(header.data),
                    DPMS_MAJOR_OPCODE,
                );
            };
            // Zero = "this level disabled". Among non-zero values
            // require off >= suspend >= standby. Xorg `:370-376`.
            let nonzero_violates = |a: u16, b: u16| a != 0 && b != 0 && a > b;
            // Report the value being compared against (the one that
            // "should have been bigger"), matching Xorg dpms.c:373.
            if nonzero_violates(suspend, off) {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_VALUE,
                    u32::from(off),
                    u16::from(header.data),
                    DPMS_MAJOR_OPCODE,
                );
            }
            if nonzero_violates(standby, suspend) {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_VALUE,
                    u32::from(suspend),
                    u16::from(header.data),
                    DPMS_MAJOR_OPCODE,
                );
            }
            state.dpms.standby_ms = u32::from(standby) * 1000;
            state.dpms.suspend_ms = u32::from(suspend) * 1000;
            state.dpms.off_ms = u32::from(off) * 1000;
        }
        x11dpms::ENABLE => {
            if !state.dpms.enabled {
                state.dpms.enabled = true;
                emit_dpms_notify(state);
            }
        }
        x11dpms::DISABLE => {
            let was_enabled = state.dpms.enabled;
            // Force level back to On (calls notify iff level changed).
            apply_dpms_transition(state, backend, 0);
            if was_enabled {
                state.dpms.enabled = false;
                emit_dpms_notify(state);
            }
        }
        x11dpms::FORCE_LEVEL => {
            if !state.dpms.enabled {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_MATCH,
                    0,
                    u16::from(header.data),
                    DPMS_MAJOR_OPCODE,
                );
            }
            let Some(level) = x11dpms::parse_force_level_request(body) else {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_LENGTH,
                    0,
                    u16::from(header.data),
                    DPMS_MAJOR_OPCODE,
                );
            };
            if level > 3 {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_VALUE,
                    u32::from(level),
                    u16::from(header.data),
                    DPMS_MAJOR_OPCODE,
                );
            }
            #[allow(clippy::cast_possible_truncation)]
            apply_dpms_transition(state, backend, level as u8);
        }
        x11dpms::INFO => {
            let reply = x11dpms::encode_info_reply(
                byte_order,
                sequence,
                u16::from(state.dpms.power_level),
                state.dpms.enabled,
            );
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            return Ok(write_to_client(client, client_id, &reply));
        }
        x11dpms::SELECT_INPUT => {
            let Some(mask) = x11dpms::parse_select_input_request(body) else {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_LENGTH,
                    0,
                    u16::from(header.data),
                    DPMS_MAJOR_OPCODE,
                );
            };
            if mask & !x11dpms::DPMS_INFO_NOTIFY_MASK != 0 {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_VALUE,
                    mask,
                    u16::from(header.data),
                    DPMS_MAJOR_OPCODE,
                );
            }
            if mask & x11dpms::DPMS_INFO_NOTIFY_MASK != 0 {
                state.dpms.selected_by.insert(client_id);
            } else {
                state.dpms.selected_by.remove(&client_id);
            }
        }
        _ => {
            return emit_x11_error_with_minor(
                state,
                client_id,
                sequence,
                x11::error::BAD_REQUEST,
                0,
                u16::from(header.data),
                DPMS_MAJOR_OPCODE,
            );
        }
    }
    Ok(RequestOutcome::Handled)
}

pub(super) fn handle_screen_saver_request(
    state: &mut ServerState,
    _backend: &mut dyn Backend,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    use crate::nested::MIT_SCREEN_SAVER_MAJOR_OPCODE;
    use yserver_protocol::x11::{ClientByteOrder, screensaver as x11ss};
    let byte_order = state
        .clients
        .get(&client_id.0)
        .map_or(ClientByteOrder::LittleEndian, |c| c.byte_order);
    let minor = header.data;
    let minor_u16 = u16::from(minor);
    debug!(
        "client {} #{} ScreenSaver::minor={} body_len={}",
        client_id.0,
        sequence.0,
        minor,
        body.len()
    );

    match minor {
        x11ss::QUERY_VERSION => {
            let reply = x11ss::encode_query_version_reply(
                byte_order,
                sequence,
                x11ss::SERVER_MAJOR_VERSION,
                x11ss::SERVER_MINOR_VERSION,
            );
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            return Ok(write_to_client(client, client_id, &reply));
        }
        x11ss::QUERY_INFO => {
            if x11ss::parse_query_info_request(body).is_none() {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_LENGTH,
                    0,
                    minor_u16,
                    MIT_SCREEN_SAVER_MAJOR_OPCODE,
                );
            }
            debug_assert!(
                !matches!(state.screensaver.active, ScreenSaverActive::Cycle),
                "Cycle must never appear in screensaver.active — \
                 see apply_screen_saver_transition guard"
            );
            // X11 timestamps are 32-bit ms; wraps at ~49 days per X11 spec.
            #[allow(clippy::cast_possible_truncation)]
            let last_input = state.dpms.last_activity.elapsed().as_millis() as u32;
            let timeout = state.screensaver.timeout_ms;
            let (reply_state, til_or_since) = match state.screensaver.active {
                ScreenSaverActive::On => {
                    let ts = last_input.wrapping_sub(timeout);
                    (x11ss::SCREEN_SAVER_ON, if timeout > 0 { ts } else { 0 })
                }
                ScreenSaverActive::Off | ScreenSaverActive::Cycle => {
                    if timeout == 0 {
                        (x11ss::SCREEN_SAVER_DISABLED, 0)
                    } else if timeout < last_input {
                        (x11ss::SCREEN_SAVER_OFF, 0)
                    } else {
                        (x11ss::SCREEN_SAVER_OFF, timeout - last_input)
                    }
                }
            };
            let event_mask = state
                .screensaver
                .selected_by
                .get(&client_id)
                .copied()
                .unwrap_or(0);
            let kind = screen_saver_kind(state);
            let reply = x11ss::encode_query_info_reply(
                byte_order,
                sequence,
                reply_state,
                crate::resources::SCREEN_SAVER_WINDOW.0,
                til_or_since,
                last_input,
                event_mask,
                kind,
            );
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            return Ok(write_to_client(client, client_id, &reply));
        }
        x11ss::SELECT_INPUT => {
            let Some((_drawable, mask)) = x11ss::parse_select_input_request(body) else {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_LENGTH,
                    0,
                    minor_u16,
                    MIT_SCREEN_SAVER_MAJOR_OPCODE,
                );
            };
            if mask == 0 {
                state.screensaver.selected_by.remove(&client_id);
            } else {
                state.screensaver.selected_by.insert(client_id, mask);
            }
        }
        x11ss::SET_ATTRIBUTES => {
            return match screen_saver_set_attributes(state, client_id, body) {
                Ok(()) => Ok(RequestOutcome::Handled),
                Err((code, value)) => emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    code,
                    value,
                    minor_u16,
                    MIT_SCREEN_SAVER_MAJOR_OPCODE,
                ),
            };
        }
        x11ss::UNSET_ATTRIBUTES => {
            let Some(drawable) = x11ss::parse_unset_attributes_request(body) else {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_LENGTH,
                    0,
                    minor_u16,
                    MIT_SCREEN_SAVER_MAJOR_OPCODE,
                );
            };
            if !drawable_exists(state, ResourceId(drawable)) {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_DRAWABLE,
                    drawable,
                    minor_u16,
                    MIT_SCREEN_SAVER_MAJOR_OPCODE,
                );
            }
            unset_screen_saver_attributes(state, _backend, client_id);
        }
        x11ss::SUSPEND => {
            let Some(suspend) = x11ss::parse_suspend_request(body) else {
                return emit_x11_error_with_minor(
                    state,
                    client_id,
                    sequence,
                    x11::error::BAD_LENGTH,
                    0,
                    minor_u16,
                    MIT_SCREEN_SAVER_MAJOR_OPCODE,
                );
            };
            if suspend {
                *state
                    .screensaver
                    .suspend_counts
                    .entry(client_id)
                    .or_insert(0) += 1;
            } else {
                let drained = match state.screensaver.suspend_counts.get_mut(&client_id) {
                    Some(c) if *c > 1 => {
                        *c -= 1;
                        false
                    }
                    Some(_) => {
                        state.screensaver.suspend_counts.remove(&client_id);
                        true
                    }
                    None => false,
                };
                if drained
                    && state.screensaver.suspend_counts.is_empty()
                    && matches!(state.screensaver.active, ScreenSaverActive::Off)
                    && state.dpms.power_level == 0
                {
                    // Mirrors ScreenSaverFreeSuspend (saver.c:343-378):
                    // restart the idle clock from now. No notify fires.
                    state.dpms.last_activity = std::time::Instant::now();
                    reset_idletime_state_after_suspend_release(state);
                }
            }
        }
        _ => {
            return emit_x11_error_with_minor(
                state,
                client_id,
                sequence,
                x11::error::BAD_REQUEST,
                0,
                minor_u16,
                MIT_SCREEN_SAVER_MAJOR_OPCODE,
            );
        }
    }
    Ok(RequestOutcome::Handled)
}

/// GetScreenSaver (108): reflects ScreenSaverState back to the client.
pub(super) fn handle_get_screen_saver(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
) -> io::Result<RequestOutcome> {
    debug!("client {} #{} GetScreenSaver", client_id.0, sequence.0);
    let Some(client) = state.clients.get_mut(&client_id.0) else {
        return Ok(RequestOutcome::Handled);
    };
    let byte_order = client.byte_order;
    let mut buf = x11::fixed_reply(byte_order, sequence, 0, 0);
    #[allow(clippy::cast_possible_truncation)]
    let timeout = (state.screensaver.timeout_ms / 1000) as u16;
    #[allow(clippy::cast_possible_truncation)]
    let interval = (state.screensaver.interval_ms / 1000) as u16;
    x11::write_u16(byte_order, &mut buf, timeout);
    x11::write_u16(byte_order, &mut buf, interval);
    buf.push(u8::from(state.screensaver.prefer_blanking));
    buf.push(u8::from(state.screensaver.allow_exposures));
    buf.resize(32, 0);
    Ok(write_to_client(client, client_id, &buf))
}

/// SetScreenSaver (107): body is `timeout:i16 interval:i16
/// prefer_blanking:u8 allow_exposures:u8 pad:u16` (8 bytes).
///
/// Sentinel resolution matches Xorg `dix/globals.c:96-99`:
///   -1 (timeout/interval)            → restore 600s default
///    2 (prefer_blanking/allow_expo)  → restore the default value
pub(super) fn handle_set_screen_saver(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    // Body layout: timeout:i16 interval:i16 prefer_blanking:u8
    // allow_exposures:u8 pad:u16 = 8 bytes.
    if body.len() < 8 {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_LENGTH,
            0,
            header.opcode,
        );
    }
    let timeout = i16::from_le_bytes([body[0], body[1]]);
    let interval = i16::from_le_bytes([body[2], body[3]]);
    let prefer_blanking = body[4];
    let allow_exposures = body[5];

    if !(-1..=0x7fff).contains(&i32::from(timeout)) {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_VALUE,
            u32::from(timeout as u16),
            header.opcode,
        );
    }
    if !(-1..=0x7fff).contains(&i32::from(interval)) {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_VALUE,
            u32::from(interval as u16),
            header.opcode,
        );
    }
    if prefer_blanking > 2 {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_VALUE,
            u32::from(prefer_blanking),
            header.opcode,
        );
    }
    if allow_exposures > 2 {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_VALUE,
            u32::from(allow_exposures),
            header.opcode,
        );
    }

    state.screensaver.timeout_ms = match timeout {
        -1 => 600_000,
        n => u32::from(n as u16) * 1000, // n is already ≥ 0 (validated above)
    };
    state.screensaver.interval_ms = match interval {
        -1 => 600_000,
        n => u32::from(n as u16) * 1000, // n is already ≥ 0 (validated above)
    };
    state.screensaver.prefer_blanking = match prefer_blanking {
        0 => false,
        1 => true,
        _ => true, // 2 = Default; Xorg defaultScreenSaverBlanking = PreferBlanking
    };
    state.screensaver.allow_exposures = match allow_exposures {
        0 => false,
        1 => true,
        _ => true, // 2 = Default; Xorg defaultScreenSaverAllowExposures = AllowExposures
    };

    // No last_activity reset — Xorg's ProcSetScreenSaver only calls
    // SetScreenSaverTimer(), which recomputes the deadline from the
    // unchanged LastEventTime. yserver computes the deadline lazily
    // at every poll iteration; the next iteration sees the new
    // timeout_ms against the unchanged last_activity.

    Ok(RequestOutcome::Handled)
}

/// ForceScreenSaver (115): `mode` lives in the request header's
/// `data` byte (Fixed(1) in the core length table — no body bytes).
/// mode=0 (Reset) → force Off + bump last_activity.
/// mode=1 (Activate) → force On.
pub(super) fn handle_force_screen_saver(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
) -> io::Result<RequestOutcome> {
    let mode = header.data; // u8 stashed in the request header's data byte
    if mode > 1 {
        return emit_x11_error(
            state,
            client_id,
            sequence,
            x11::error::BAD_VALUE,
            u32::from(mode),
            header.opcode,
        );
    }
    if mode == 0 {
        apply_screen_saver_transition(
            state,
            backend,
            ScreenSaverActive::Off,
            /*forced=*/ true,
        );
        state.dpms.last_activity = std::time::Instant::now();
    } else {
        apply_screen_saver_transition(state, backend, ScreenSaverActive::On, /*forced=*/ true);
    }
    Ok(RequestOutcome::Handled)
}
