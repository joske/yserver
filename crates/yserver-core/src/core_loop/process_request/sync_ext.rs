use super::*;

/// A SYNC protocol error: `code` is either a core error or
/// `SYNC_FIRST_ERROR + x11sync::BAD_*`.
fn sync_error(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    code: u8,
    value: u32,
) -> io::Result<RequestOutcome> {
    emit_x11_error_with_minor(
        state,
        client_id,
        sequence,
        code,
        value,
        u16::from(header.data),
        header.opcode,
    )
}

/// Xorg's lookup of a client-writable counter for SetCounter /
/// ChangeCounter / DestroyCounter: an unknown XID is BadCounter, a system
/// counter BadAccess (both naming the counter; captured on Xvfb).
fn sync_writable_counter(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    counter: u32,
) -> Result<i64, io::Result<RequestOutcome>> {
    use yserver_protocol::x11::sync as x11sync;
    if crate::core_loop::sync_await::is_system_counter(counter) {
        return Err(sync_error(
            state,
            client_id,
            sequence,
            header,
            x11::error::BAD_ACCESS,
            counter,
        ));
    }
    match state.sync_counters.get(&counter) {
        Some(c) => Ok(c.value),
        None => Err(sync_error(
            state,
            client_id,
            sequence,
            header,
            crate::nested::SYNC_FIRST_ERROR + x11sync::BAD_COUNTER,
            counter,
        )),
    }
}

/// Xorg `RTFence` lookup: an unknown fence is BadFence naming it.
fn sync_known_fence(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    fence: u32,
) -> Result<(), io::Result<RequestOutcome>> {
    use yserver_protocol::x11::sync as x11sync;
    if state.sync_fences.contains_key(&fence) {
        return Ok(());
    }
    Err(sync_error(
        state,
        client_id,
        sequence,
        header,
        crate::nested::SYNC_FIRST_ERROR + x11sync::BAD_FENCE,
        fence,
    ))
}

/// Xorg's `RTAlarm` lookup: an alarm that does not exist (never created,
/// None, or destroyed) is BadAlarm naming it.
fn sync_known_alarm(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    alarm: u32,
) -> Result<(), io::Result<RequestOutcome>> {
    use yserver_protocol::x11::sync as x11sync;
    if state.sync_alarms.contains_key(&alarm) {
        return Ok(());
    }
    Err(sync_error(
        state,
        client_id,
        sequence,
        header,
        crate::nested::SYNC_FIRST_ERROR + x11sync::BAD_ALARM,
        alarm,
    ))
}

pub(super) fn handle_sync_request(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    use yserver_protocol::x11::{ClientByteOrder, sync as x11sync};
    let byte_order = state
        .clients
        .get(&client_id.0)
        .map_or(ClientByteOrder::LittleEndian, |c| c.byte_order);
    let minor = header.data;
    match minor {
        x11sync::INITIALIZE => {
            // Xorg `ProcSyncInitialize` always answers its own version
            // (Xvfb: client 3.1 / 3.0 / 2.0 / 4.0 all get 3.1).
            let reply = x11sync::encode_initialize_reply(
                byte_order,
                sequence,
                x11sync::MAJOR_VERSION,
                x11sync::MINOR_VERSION,
            );
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            return Ok(write_to_client(client, client_id, &reply));
        }
        x11sync::LIST_SYSTEM_COUNTERS => {
            let reply = x11sync::encode_list_system_counters_reply(byte_order, sequence);
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            let _byte_order = client.byte_order;
            return Ok(write_to_client(client, client_id, &reply));
        }
        x11sync::CREATE_COUNTER => {
            if let Some((counter, value)) = x11sync::parse_counter_value(body) {
                state.sync_counters.insert(
                    counter,
                    crate::server::SyncCounter {
                        owner: client_id,
                        value,
                    },
                );
            }
        }
        x11sync::SET_COUNTER => {
            if let Some((counter, value)) = x11sync::parse_counter_value(body) {
                let old = match sync_writable_counter(state, client_id, sequence, header, counter) {
                    Ok(old) => old,
                    Err(outcome) => return outcome,
                };
                if let Some(c) = state.sync_counters.get_mut(&counter) {
                    c.value = value;
                }
                crate::core_loop::sync_await::counter_changed(state, counter, old, value);
            }
        }
        x11sync::CHANGE_COUNTER => {
            if let Some((counter, delta)) = x11sync::parse_counter_value(body) {
                let old = match sync_writable_counter(state, client_id, sequence, header, counter) {
                    Ok(old) => old,
                    Err(outcome) => return outcome,
                };
                // Xorg: an INT64 overflow is BadValue naming the high half
                // of the delta (Xvfb: value 0x7fffffff for i64::MAX).
                let Some(new) = old.checked_add(delta) else {
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    let value_hi = (delta >> 32) as u32;
                    return sync_error(
                        state,
                        client_id,
                        sequence,
                        header,
                        x11::error::BAD_VALUE,
                        value_hi,
                    );
                };
                if let Some(c) = state.sync_counters.get_mut(&counter) {
                    c.value = new;
                }
                crate::core_loop::sync_await::counter_changed(state, counter, old, new);
            }
        }
        x11sync::QUERY_COUNTER => {
            let counter = x11sync::parse_resource(body).unwrap_or(0);
            // IDLETIME saturates at u32::MAX ms (~49 days) inside
            // `idletime_current_idle`. An unknown counter is BadCounter.
            let Some(value) = crate::core_loop::sync_await::counter_value(state, counter) else {
                return sync_error(
                    state,
                    client_id,
                    sequence,
                    header,
                    crate::nested::SYNC_FIRST_ERROR + x11sync::BAD_COUNTER,
                    counter,
                );
            };
            let reply = x11sync::encode_query_counter_reply(byte_order, sequence, value);
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            let _byte_order = client.byte_order;
            return Ok(write_to_client(client, client_id, &reply));
        }
        x11sync::DESTROY_COUNTER => {
            if let Some(counter) = x11sync::parse_resource(body) {
                let last = match sync_writable_counter(state, client_id, sequence, header, counter)
                {
                    Ok(last) => last,
                    Err(outcome) => return outcome,
                };
                state.sync_counters.remove(&counter);
                crate::core_loop::sync_await::counter_destroyed(state, counter, last);
            }
        }
        x11sync::AWAIT => {
            // Xorg `ProcSyncAwait`: validate every wait condition, then
            // suspend the client until one of the triggers fires. Its later
            // requests stay queued behind this one (see `sync_await`).
            let Some(conditions) = x11sync::parse_await(byte_order, body) else {
                return sync_error(
                    state,
                    client_id,
                    sequence,
                    header,
                    x11::error::BAD_LENGTH,
                    0,
                );
            };
            if conditions.is_empty() {
                return sync_error(state, client_id, sequence, header, x11::error::BAD_VALUE, 0);
            }
            let mut waits = Vec::with_capacity(conditions.len());
            for c in conditions {
                // `SyncInitTrigger` order: counter, value type, value, test
                // type. Captured on Xvfb: None / unknown → BadCounter,
                // bad value type / test type → BadValue naming it, relative
                // overflow → BadValue naming the high half of the wait.
                let current = if c.counter == 0 {
                    None
                } else {
                    crate::core_loop::sync_await::counter_value(state, c.counter)
                };
                let Some(current) = current else {
                    return sync_error(
                        state,
                        client_id,
                        sequence,
                        header,
                        crate::nested::SYNC_FIRST_ERROR + x11sync::BAD_COUNTER,
                        c.counter,
                    );
                };
                let test_value = match c.value_type {
                    x11sync::VALUE_TYPE_ABSOLUTE => c.wait_value,
                    x11sync::VALUE_TYPE_RELATIVE => {
                        let Some(v) = current.checked_add(c.wait_value) else {
                            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                            let value_hi = (c.wait_value >> 32) as u32;
                            return sync_error(
                                state,
                                client_id,
                                sequence,
                                header,
                                x11::error::BAD_VALUE,
                                value_hi,
                            );
                        };
                        v
                    }
                    other => {
                        return sync_error(
                            state,
                            client_id,
                            sequence,
                            header,
                            x11::error::BAD_VALUE,
                            other,
                        );
                    }
                };
                if c.test_type > x11sync::TEST_NEGATIVE_COMPARISON {
                    return sync_error(
                        state,
                        client_id,
                        sequence,
                        header,
                        x11::error::BAD_VALUE,
                        c.test_type,
                    );
                }
                waits.push(crate::server::SyncAwaitCondition::Counter {
                    counter: c.counter,
                    test_type: c.test_type,
                    test_value,
                    event_threshold: c.event_threshold,
                });
            }
            debug!(
                "client {} #{} SYNC::Await {} condition(s)",
                client_id.0,
                sequence.0,
                waits.len()
            );
            crate::core_loop::sync_await::begin_await(state, &*backend, client_id, waits);
        }
        x11sync::CREATE_ALARM => {
            // Xorg `ProcSyncCreateAlarm`: the value list must match the mask.
            let Some((alarm, mask)) = x11sync::parse_alarm_with_mask(body) else {
                return sync_error(
                    state,
                    client_id,
                    sequence,
                    header,
                    x11::error::BAD_LENGTH,
                    0,
                );
            };
            if body.len() != x11sync::alarm_request_len(mask) {
                return sync_error(
                    state,
                    client_id,
                    sequence,
                    header,
                    x11::error::BAD_LENGTH,
                    alarm,
                );
            }
            // Xorg's defaults (`ProcSyncCreateAlarm` + `SyncInitTrigger` on
            // None): no counter, Absolute 0, PositiveComparison, delta 1,
            // the owner selected for events. An error discards the alarm.
            let mut a = crate::server::SyncAlarm {
                owner: client_id,
                state: x11sync::ALARM_STATE_INACTIVE,
                events: true,
                test_type: x11sync::TEST_POSITIVE_COMPARISON,
                check_type: x11sync::TEST_POSITIVE_COMPARISON,
                delta: 1,
                ..crate::server::SyncAlarm::default()
            };
            let values = alarm_value_words(body);
            if let Err((code, value)) =
                change_alarm_attributes(state, alarm, &mut a, client_id, mask, &values)
            {
                return sync_error(state, client_id, sequence, header, code, value);
            }
            let counter = a.counter;
            let class = state
                .client_wm_class
                .get(&client_id.0)
                .map(String::as_str)
                .unwrap_or("<unknown>");
            log::trace!(
                "sync: client {}/{class:?} CreateAlarm 0x{alarm:x} \
                 counter={cname}(0x{counter:x}) test={test} \
                 wait_value={wait} delta={delta} events={events}",
                client_id.0,
                cname = sync_counter_name(counter),
                test = sync_test_type_name(a.test_type),
                wait = a.wait_value,
                delta = a.delta,
                events = a.events,
            );
            if counter == 0 {
                // "NULL counter will not trigger in CreateAlarm and sets
                // alarm state to Inactive" (Xorg).
                a.state = x11sync::ALARM_STATE_INACTIVE;
                state.sync_alarms.insert(alarm, a);
                return Ok(RequestOutcome::Handled);
            }
            state.sync_alarms.insert(alarm, a);
            // The trigger is tested at creation: a comparison whose
            // condition already holds fires at once. System counters
            // (SERVERTIME, IDLETIME) read the clock.
            check_new_alarm_trigger(state, alarm, counter);
        }
        x11sync::CHANGE_ALARM => {
            // Xorg `ProcSyncChangeAlarm`: minimum size, then the alarm
            // lookup (BadAlarm), then the value list against the mask
            // (BadLength naming the alarm, captured on Xvfb).
            let Some((alarm, mask)) = x11sync::parse_alarm_with_mask(body) else {
                return sync_error(
                    state,
                    client_id,
                    sequence,
                    header,
                    x11::error::BAD_LENGTH,
                    0,
                );
            };
            if let Err(outcome) = sync_known_alarm(state, client_id, sequence, header, alarm) {
                return outcome;
            }
            if body.len() != x11sync::alarm_request_len(mask) {
                return sync_error(
                    state,
                    client_id,
                    sequence,
                    header,
                    x11::error::BAD_LENGTH,
                    alarm,
                );
            }
            // Xorg `SyncChangeAlarmAttributes`: any client may change an
            // alarm; `events` selects AlarmNotify for the requesting client
            // (the owner's own flag, or the event-client list). A failure
            // part-way keeps what Xorg keeps (see change_alarm_attributes),
            // so the copy is written back either way.
            let Some(mut a) = state.sync_alarms.get(&alarm).cloned() else {
                return Ok(RequestOutcome::Handled);
            };
            let values = alarm_value_words(body);
            let outcome = change_alarm_attributes(state, alarm, &mut a, client_id, mask, &values);
            if let Err((code, value)) = outcome {
                state.sync_alarms.insert(alarm, a);
                return sync_error(state, client_id, sequence, header, code, value);
            }
            let counter = a.counter;
            let class = state
                .client_wm_class
                .get(&client_id.0)
                .map(String::as_str)
                .unwrap_or("<unknown>");
            log::trace!(
                "sync: client {}/{class:?} ChangeAlarm 0x{alarm:x} \
                 counter={cname}(0x{counter:x}) test={test} \
                 wait_value={wait} delta={delta} events={events}",
                client_id.0,
                cname = sync_counter_name(counter),
                test = sync_test_type_name(a.test_type),
                wait = a.wait_value,
                delta = a.delta,
                events = a.events,
            );
            state.sync_alarms.insert(alarm, a);
            if counter == 0 {
                // "NULL counter WILL trigger in ChangeAlarm" (Xorg): the
                // alarm goes Inactive with an AlarmNotify.
                alarm_trigger_fired(state, alarm, 0);
            } else {
                check_new_alarm_trigger(state, alarm, counter);
            }
        }
        x11sync::QUERY_ALARM => {
            // Xorg `ProcSyncQueryAlarm`: exact size, then the lookup.
            if body.len() != 4 {
                return sync_error(
                    state,
                    client_id,
                    sequence,
                    header,
                    x11::error::BAD_LENGTH,
                    0,
                );
            }
            let alarm_id = x11sync::parse_resource(body).unwrap_or(0);
            if let Err(outcome) = sync_known_alarm(state, client_id, sequence, header, alarm_id) {
                return outcome;
            }
            let alarm = state
                .sync_alarms
                .get(&alarm_id)
                .cloned()
                .unwrap_or_default();
            let reply = x11sync::encode_query_alarm_reply(
                byte_order,
                sequence,
                alarm.counter,
                alarm.wait_value,
                alarm.test_type,
                alarm.delta,
                alarm.events,
                alarm.state,
            );
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            let _byte_order = client.byte_order;
            return Ok(write_to_client(client, client_id, &reply));
        }
        x11sync::DESTROY_ALARM => {
            // Xorg `ProcSyncDestroyAlarm`: exact size, then the lookup.
            if body.len() != 4 {
                return sync_error(
                    state,
                    client_id,
                    sequence,
                    header,
                    x11::error::BAD_LENGTH,
                    0,
                );
            }
            let alarm = x11sync::parse_resource(body).unwrap_or(0);
            if let Err(outcome) = sync_known_alarm(state, client_id, sequence, header, alarm) {
                return outcome;
            }
            // Xorg `FreeAlarm`: any client may destroy it; a Destroyed
            // AlarmNotify goes to the owner and the selecting clients.
            crate::core_loop::sync_await::destroy_alarm(state, alarm);
        }
        x11sync::CREATE_FENCE => {
            if let Some(req) = x11sync::parse_create_fence(body) {
                state.sync_fences.insert(
                    req.fence,
                    crate::server::SyncFence {
                        owner: client_id,
                        triggered: req.initially_triggered,
                    },
                );
                debug!(
                    "client {} #{} SYNC::CreateFence fence=0x{:x} initially_triggered={}",
                    client_id.0, sequence.0, req.fence, req.initially_triggered
                );
            }
        }
        x11sync::DESTROY_FENCE => {
            if let Some(fence) = x11sync::parse_resource(body) {
                if let Err(outcome) = sync_known_fence(state, client_id, sequence, header, fence) {
                    return outcome;
                }
                crate::core_loop::sync_await::fence_destroyed(state, fence);
                state.sync_fences.remove(&fence);
                backend.dri3_destroy_fence(fence);
                debug!(
                    "client {} #{} SYNC::DestroyFence fence=0x{:x}",
                    client_id.0, sequence.0, fence
                );
            }
        }
        x11sync::TRIGGER_FENCE => {
            if let Some(fence) = x11sync::parse_resource(body) {
                if let Err(outcome) = sync_known_fence(state, client_id, sequence, header, fence) {
                    return outcome;
                }
                // For DRI3-imported xshmfence-backed fences, the
                // server-side triggered bit is invisible to Mesa's local
                // `xshmfence_await`. Forward the trigger to the backend so
                // it writes the shared 4-byte counter + futex-wakes any
                // local waiter — Mesa's `loader_dri3_copy_drawable` blocks
                // on exactly that.
                if let Err(e) = backend.dri3_trigger_fence(fence) {
                    log::warn!("SYNC::TriggerFence 0x{fence:x}: backend trigger failed: {e}");
                }
                debug!(
                    "client {} #{} SYNC::TriggerFence fence=0x{:x}",
                    client_id.0, sequence.0, fence
                );
                // Xorg `miSyncTriggerFence`: wakes every AwaitFence on it.
                crate::core_loop::sync_await::fence_triggered(state, fence);
            }
        }
        x11sync::RESET_FENCE => {
            if let Some(fence) = x11sync::parse_resource(body) {
                if let Err(outcome) = sync_known_fence(state, client_id, sequence, header, fence) {
                    return outcome;
                }
                // Xorg `ProcSyncResetFence`: only a triggered fence can be
                // reset (Xvfb: BadMatch naming the fence); a shared-memory
                // fence is tested, and reset, in that memory.
                if !crate::core_loop::sync_await::fence_is_triggered(state, &*backend, fence) {
                    return sync_error(
                        state,
                        client_id,
                        sequence,
                        header,
                        x11::error::BAD_MATCH,
                        fence,
                    );
                }
                backend.dri3_reset_fence(fence);
                if let Some(f) = state.sync_fences.get_mut(&fence) {
                    f.triggered = false;
                }
            }
        }
        x11sync::QUERY_FENCE => {
            let fence = x11sync::parse_resource(body).unwrap_or(0);
            if let Err(outcome) = sync_known_fence(state, client_id, sequence, header, fence) {
                return outcome;
            }
            // Xorg `ProcSyncQueryFence` → `CheckTriggered`: shared memory
            // for a DRI3 xshmfence, the server's bit otherwise.
            let triggered =
                crate::core_loop::sync_await::fence_is_triggered(state, &*backend, fence);
            let reply = x11sync::encode_query_fence_reply(byte_order, sequence, triggered);
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            return Ok(write_to_client(client, client_id, &reply));
        }
        x11sync::AWAIT_FENCE => {
            // Xorg `ProcSyncAwaitFence`: suspend the client until any of
            // the fences triggers (or is destroyed). Captured on Xvfb:
            // empty list → BadValue, None / unknown fence → BadFence.
            let Some(fences) = x11sync::parse_await_fence(byte_order, body) else {
                return sync_error(
                    state,
                    client_id,
                    sequence,
                    header,
                    x11::error::BAD_LENGTH,
                    0,
                );
            };
            if fences.is_empty() {
                return sync_error(state, client_id, sequence, header, x11::error::BAD_VALUE, 0);
            }
            for &fence in &fences {
                if !state.sync_fences.contains_key(&fence) {
                    return sync_error(
                        state,
                        client_id,
                        sequence,
                        header,
                        crate::nested::SYNC_FIRST_ERROR + x11sync::BAD_FENCE,
                        fence,
                    );
                }
            }
            debug!(
                "client {} #{} SYNC::AwaitFence n={}",
                client_id.0,
                sequence.0,
                fences.len()
            );
            let waits = fences
                .into_iter()
                .map(|fence| crate::server::SyncAwaitCondition::Fence { fence })
                .collect();
            crate::core_loop::sync_await::begin_await(state, &*backend, client_id, waits);
        }
        x11sync::SET_PRIORITY => {
            // Stub.
        }
        x11sync::GET_PRIORITY => {
            let reply = x11sync::encode_get_priority_reply(byte_order, sequence, 0);
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            let _byte_order = client.byte_order;
            return Ok(write_to_client(client, client_id, &reply));
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

/// Xorg `SyncChangeAlarmAttributes` + `SyncInitTrigger` for CreateAlarm
/// (on a fresh alarm) and ChangeAlarm (on a copy the caller writes back
/// whatever the outcome). `values` is the value list, one word each and
/// two for VALUE and DELTA, already length-checked against `mask`.
///
/// Order and partial effects, as Xorg (captured on Xvfb):
/// 1. The list is walked in mask-bit order; an `events` word other than
///    True/False is BadValue naming it, an unknown bit BadValue naming the
///    mask bits above it. Nothing is changed yet.
/// 2. `events` selects AlarmNotify for `client` — it stays selected even
///    if the request then fails.
/// 3. With DELTA or TEST_TYPE in the mask, a positive test with a
///    negative delta or a negative test with a positive delta is
///    BadMatch; nothing else is changed.
/// 4. Delta, value type, value and test type are stored.
/// 5. The trigger is initialised: an unknown counter is BadCounter, an
///    invalid value type BadValue naming it; the wait value resolves
///    (Relative without a counter is BadMatch; an INT64 overflow is
///    BadValue naming the value's high word, with the wrapped sum stored);
///    an invalid test type is BadValue naming it. Only then do the counter
///    and the test the trigger runs change, and the alarm goes Active.
///
/// So a failing ChangeAlarm can leave step 4 behind: a stored bad test
/// type is what QueryAlarm reports, while the alarm keeps running its old
/// test.
fn change_alarm_attributes(
    state: &ServerState,
    alarm_id: u32,
    alarm: &mut crate::server::SyncAlarm,
    client: ClientId,
    mask: u32,
    values: &[u32],
) -> Result<(), (u8, u32)> {
    use yserver_protocol::x11::sync as x11sync;
    let int64 = |hi: u32, lo: u32| (i64::from(hi.cast_signed()) << 32) | i64::from(lo);
    let mut words = values.iter().copied();
    let mut next = || words.next().unwrap_or(0);
    let (mut counter, mut value_type, mut raw_wait, mut test_type, mut delta) = (
        alarm.counter,
        alarm.value_type,
        alarm.raw_wait,
        alarm.test_type,
        alarm.delta,
    );
    let mut select = None;
    let mut remaining = mask;
    while remaining != 0 {
        let bit = 1u32 << remaining.trailing_zeros();
        remaining &= !bit;
        match bit {
            x11sync::CA_COUNTER => counter = next(),
            x11sync::CA_VALUE_TYPE => value_type = next(),
            x11sync::CA_VALUE => raw_wait = int64(next(), next()),
            x11sync::CA_TEST_TYPE => test_type = next(),
            x11sync::CA_DELTA => delta = int64(next(), next()),
            x11sync::CA_EVENTS => {
                let events = next();
                if events > 1 {
                    return Err((x11::error::BAD_VALUE, events));
                }
                select = Some(events == 1);
            }
            _ => return Err((x11::error::BAD_VALUE, remaining)),
        }
    }
    if let Some(want) = select {
        crate::core_loop::sync_await::select_alarm_events(alarm, client, want);
    }
    if mask & (x11sync::CA_DELTA | x11sync::CA_TEST_TYPE) != 0 {
        let positive = matches!(
            test_type,
            x11sync::TEST_POSITIVE_COMPARISON | x11sync::TEST_POSITIVE_TRANSITION
        );
        let negative = matches!(
            test_type,
            x11sync::TEST_NEGATIVE_COMPARISON | x11sync::TEST_NEGATIVE_TRANSITION
        );
        if (positive && delta < 0) || (negative && delta > 0) {
            return Err((x11::error::BAD_MATCH, alarm_id));
        }
    }
    alarm.delta = delta;
    alarm.value_type = value_type;
    alarm.raw_wait = raw_wait;
    alarm.test_type = test_type;

    let mut new_counter = alarm.counter;
    if mask & x11sync::CA_COUNTER != 0 {
        if counter != 0
            && !crate::core_loop::sync_await::is_system_counter(counter)
            && !state.sync_counters.contains_key(&counter)
        {
            return Err((
                crate::nested::SYNC_FIRST_ERROR + x11sync::BAD_COUNTER,
                counter,
            ));
        }
        new_counter = counter;
    }
    let current = if new_counter == 0 {
        None
    } else {
        crate::core_loop::sync_await::counter_value(state, new_counter)
    };
    if mask & x11sync::CA_VALUE_TYPE != 0
        && value_type != x11sync::VALUE_TYPE_ABSOLUTE
        && value_type != x11sync::VALUE_TYPE_RELATIVE
    {
        return Err((x11::error::BAD_VALUE, value_type));
    }
    if mask & (x11sync::CA_VALUE_TYPE | x11sync::CA_VALUE) != 0 {
        if value_type == x11sync::VALUE_TYPE_ABSOLUTE {
            alarm.wait_value = raw_wait;
        } else {
            let Some(value) = current else {
                return Err((x11::error::BAD_MATCH, alarm_id));
            };
            let (sum, overflow) = value.overflowing_add(raw_wait);
            alarm.wait_value = sum;
            if overflow {
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                return Err((x11::error::BAD_VALUE, (raw_wait >> 32) as u32));
            }
        }
    }
    if mask & x11sync::CA_TEST_TYPE != 0 {
        if test_type > x11sync::TEST_NEGATIVE_COMPARISON {
            return Err((x11::error::BAD_VALUE, test_type));
        }
        alarm.check_type = test_type;
    }
    alarm.counter = new_counter;
    alarm.state = x11sync::ALARM_STATE_ACTIVE;
    Ok(())
}

/// The value list of a length-checked CreateAlarm / ChangeAlarm body.
fn alarm_value_words(body: &[u8]) -> Vec<u32> {
    body.get(8..)
        .unwrap_or_default()
        .chunks_exact(4)
        .map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
        .collect()
}

/// Evaluate every active alarm watching `counter` after it moved from
/// `old` to `new`, firing `AlarmNotify` to each alarm's owner when its
/// trigger test crosses.
///
/// Re-arm behaviour (Xorg sync.c:548-555, 589-597):
/// - `delta != 0`: advance `wait_value` past the current value so the
///   next crossing re-fires; on `i64` overflow transition to Inactive.
/// - `delta == 0` + Comparison test type: transition to Inactive.
/// - `delta == 0` + Transition test type: stay Active with `wait_value`
///   unchanged — the alarm quiesces until the next edge crossing.
///
/// Human-friendly name for a SYNC counter id (system counters land
/// here; client-allocated counters fall through to `<client>`).
fn sync_counter_name(counter: u32) -> &'static str {
    use yserver_protocol::x11::sync as x11sync;
    match counter {
        x11sync::SERVERTIME_COUNTER => "SERVERTIME",
        x11sync::IDLETIME_COUNTER => "IDLETIME",
        x11sync::IDLETIME_DEVICE_VCP => "IDLETIME-VCP",
        x11sync::IDLETIME_DEVICE_VCK => "IDLETIME-VCK",
        _ => "<client>",
    }
}

/// Human-friendly name for a SYNC alarm test type.
fn sync_test_type_name(test_type: u32) -> &'static str {
    use yserver_protocol::x11::sync as x11sync;
    match test_type {
        x11sync::TEST_POSITIVE_TRANSITION => "PosTransition",
        x11sync::TEST_NEGATIVE_TRANSITION => "NegTransition",
        x11sync::TEST_POSITIVE_COMPARISON => "PosComparison",
        x11sync::TEST_NEGATIVE_COMPARISON => "NegComparison",
        _ => "?",
    }
}

/// Human-friendly name for a SYNC alarm state.
fn sync_alarm_state_name(state: u8) -> &'static str {
    use yserver_protocol::x11::sync as x11sync;
    match state {
        x11sync::ALARM_STATE_ACTIVE => "Active",
        x11sync::ALARM_STATE_INACTIVE => "Inactive",
        x11sync::ALARM_STATE_DESTROYED => "Destroyed",
        _ => "?",
    }
}

/// Run the alarms watching `counter` for a change from `old` to `new`
/// (Xorg `SyncChangeCounter` → each alarm trigger's `CheckTrigger`). This
/// is the frame-timing signal mutter/muffin waits on before compositing a
/// client frame and emitting `_NET_WM_FRAME_DRAWN`, and also the
/// idle/wake pair used by mate-power-manager.
pub(crate) fn evaluate_alarms_for_counter(
    state: &mut ServerState,
    counter: u32,
    old: i64,
    new: i64,
) {
    use yserver_protocol::x11::sync as x11sync;
    let mut candidates: Vec<u32> = state
        .sync_alarms
        .iter()
        .filter(|(_, a)| {
            a.counter == counter
                && a.state == x11sync::ALARM_STATE_ACTIVE
                && x11sync::trigger_fires(a.check_type, old, new, a.wait_value)
        })
        .map(|(id, _)| *id)
        .collect();
    candidates.sort_unstable();
    for alarm_id in candidates {
        alarm_trigger_fired(state, alarm_id, new);
    }
}

/// The trigger test CreateAlarm / ChangeAlarm run on the one alarm they
/// set up: its counter's current value, as both old and new (so only a
/// comparison can already hold).
fn check_new_alarm_trigger(state: &mut ServerState, alarm_id: u32, counter: u32) {
    use yserver_protocol::x11::sync as x11sync;
    let now_value = crate::core_loop::sync_await::counter_value(state, counter).unwrap_or(0);
    if counter == x11sync::SERVERTIME_COUNTER {
        state.sync_servertime_last = Some(now_value);
    }
    let holds = state
        .sync_alarms
        .get(&alarm_id)
        .is_some_and(|a| x11sync::trigger_fires(a.check_type, now_value, now_value, a.wait_value));
    if holds {
        alarm_trigger_fired(state, alarm_id, now_value);
    }
}

/// Xorg `SyncAlarmTriggerFired`: alarm `alarm_id` went off with its
/// counter at `value` (0 and no counter for a counterless alarm). A
/// counterless alarm, or a comparison with delta 0, goes Inactive;
/// otherwise the wait value advances by delta until the test no longer
/// holds (Inactive, value kept, on INT64 overflow). The `AlarmNotify`
/// carries the new state and the old wait value; the new wait value is
/// stored after it is sent.
pub(crate) fn alarm_trigger_fired(state: &mut ServerState, alarm_id: u32, value: i64) {
    use yserver_protocol::x11::sync as x11sync;
    let Some(a) = state.sync_alarms.get(&alarm_id) else {
        return;
    };
    if a.state != x11sync::ALARM_STATE_ACTIVE {
        return;
    }
    // Xorg reads the stored test type for the "delta 0 on a comparison"
    // rule but re-arms with the trigger's check function; the two differ
    // only after a ChangeAlarm that failed on an invalid test type.
    let (test_type, check_type) = (a.test_type, a.check_type);
    let (owner, counter, fired_wait, delta) = (a.owner, a.counter, a.wait_value, a.delta);
    let is_comparison = matches!(
        test_type,
        x11sync::TEST_POSITIVE_COMPARISON | x11sync::TEST_NEGATIVE_COMPARISON
    );
    let (new_wait, new_state) = if counter == 0 || (delta == 0 && is_comparison) {
        (fired_wait, x11sync::ALARM_STATE_INACTIVE)
    } else if delta == 0 {
        // Transition + delta 0: stays Active with the same wait value;
        // it fires again on the next crossing.
        (fired_wait, x11sync::ALARM_STATE_ACTIVE)
    } else {
        let mut w = fired_wait;
        let mut guard = 0u32;
        let mut overflowed = false;
        // The guard bounds what Xorg does not: after that failed
        // ChangeAlarm a delta of the wrong sign passes the (unmatched)
        // sign check, and Xorg's loop then steps toward INT64 overflow
        // one delta at a time — a hung server (seen on Xvfb).
        while x11sync::comparison_satisfied(check_type, value, w) && guard < 1_000_000 {
            match w.checked_add(delta) {
                Some(next) => {
                    w = next;
                    guard += 1;
                }
                None => {
                    overflowed = true;
                    break;
                }
            }
        }
        if overflowed {
            (fired_wait, x11sync::ALARM_STATE_INACTIVE)
        } else {
            (w, x11sync::ALARM_STATE_ACTIVE)
        }
    };
    let owner_class = state
        .client_wm_class
        .get(&owner.0)
        .map(String::as_str)
        .unwrap_or("<unknown>");
    log::debug!(
        "sync: alarm 0x{alarm_id:x} fired client {owner_id}/{owner_class:?} \
         counter={counter_name}(0x{counter:x}) test={test} \
         value={value} wait={fired_wait} → state={state_name}",
        owner_id = owner.0,
        counter_name = sync_counter_name(counter),
        test = sync_test_type_name(test_type),
        state_name = sync_alarm_state_name(new_state),
    );
    if let Some(a) = state.sync_alarms.get_mut(&alarm_id) {
        a.state = new_state;
    }
    crate::core_loop::sync_await::send_alarm_notify(state, alarm_id, value);
    if let Some(a) = state.sync_alarms.get_mut(&alarm_id) {
        a.wait_value = new_wait;
    }
}
