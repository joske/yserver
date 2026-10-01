//! RECORD extension 1.13 (Xorg `record/record.c`, `record/set.c`).
//!
//! A context is a server-wide XID resource owned by its creator; any client
//! may register on, query, enable, disable or free it. `EnableContext`
//! turns the enabling connection into the context's data connection: it
//! stops dispatching that client's requests (Xorg `IgnoreClient`, here via
//! `run.rs` `client_runnable`) and streams every recorded element to it as
//! replies carrying the Enable request's sequence number, until another
//! client disables or frees the context.
//!
//! Phase 1 records core device events (KeyPress..MotionNotify), ClientStarted
//! and ClientDied. Request, reply, error and delivered-event ranges are
//! validated, stored and reported by `GetContext`, but not recorded yet.
//! Each element goes out as its own reply; Xorg batches elements of one
//! category and client per flush, and clients must accept either.

use std::{collections::BTreeMap, io};

use yserver_protocol::x11::{
    self, ClientByteOrder, ClientId, RequestHeader, SequenceNumber,
    record::{self as x11rec, Range},
};

use crate::{
    core_loop::{
        client_io::{self, WriteOutcome},
        process_request::{RequestOutcome, emit_x11_error_with_minor, write_to_client},
    },
    nested::{RECORD_FIRST_ERROR, RECORD_MAJOR_OPCODE},
    resources::ROOT_WINDOW,
    server::ServerState,
};

/// Xorg `LimitClients` default: the ceiling on a request's client count.
const LIMIT_CLIENTS: u32 = 256;

/// A set of opcodes / event types as sorted, merged, non-abutting
/// intervals — the members Xorg's bit-vector or interval-list set holds.
type Set = Vec<(u16, u16)>;

fn make_set(mut intervals: Vec<(u16, u16)>) -> Set {
    intervals.sort_by_key(|interval| interval.0);
    let mut set: Set = Vec::with_capacity(intervals.len());
    for (first, last) in intervals {
        match set.last_mut() {
            Some(prev) if u32::from(prev.1) + 1 >= u32::from(first) => prev.1 = prev.1.max(last),
            _ => set.push((first, last)),
        }
    }
    set
}

fn set_contains(set: &Set, member: u16) -> bool {
    set.iter()
        .any(|&(first, last)| (first..=last).contains(&member))
}

/// The set's intervals with members above `max` dropped (Xorg
/// `RecordConvertSetToRanges`' `imax`).
fn capped(set: &Set, max: u16) -> impl Iterator<Item = (u16, u16)> + '_ {
    set.iter()
        .take_while(move |interval| interval.0 <= max)
        .map(move |&(first, last)| (first, last.min(max)))
}

/// Minor-opcode set for one extension major-opcode interval.
#[derive(Debug)]
struct MinorSets {
    major: (u8, u8),
    minors: Set,
}

/// Xorg `RecordClientsAndProtocolRec`: the clients and protocol of one
/// CreateContext / RegisterClients.
#[derive(Debug, Default)]
struct Rcap {
    /// Client resource-id bases, or `FUTURE_CLIENTS`.
    clients: Vec<u32>,
    /// Core and extension major request opcodes.
    requests: Set,
    request_minors: Vec<MinorSets>,
    replies: Set,
    reply_minors: Vec<MinorSets>,
    delivered_events: Set,
    device_events: Set,
    errors: Set,
    client_started: bool,
    client_died: bool,
}

impl Rcap {
    /// Xorg `RecordRegisterClients` / `RecordConvertRangesToIntervals`.
    fn from_ranges(clients: Vec<u32>, ranges: &[Range]) -> Self {
        fn core(ranges: &[Range], pick: impl Fn(&Range) -> (u8, u8)) -> Vec<(u16, u16)> {
            ranges
                .iter()
                .map(pick)
                .filter(|&(first, last)| first != 0 || last != 0)
                .map(|(first, last)| (u16::from(first), u16::from(last)))
                .collect()
        }
        fn ext(
            ranges: &[Range],
            majors: &mut Vec<(u16, u16)>,
            pick: impl Fn(&Range) -> ((u8, u8), (u16, u16)),
        ) -> Vec<MinorSets> {
            let mut groups: Vec<MinorSets> = Vec::new();
            for (major, minor) in ranges.iter().map(pick) {
                if major == (0, 0) {
                    continue;
                }
                majors.push((u16::from(major.0), u16::from(major.1)));
                match groups.iter_mut().find(|g| g.major == major) {
                    Some(group) => group.minors.push(minor),
                    None => groups.push(MinorSets {
                        major,
                        minors: vec![minor],
                    }),
                }
            }
            for group in &mut groups {
                group.minors = make_set(std::mem::take(&mut group.minors));
            }
            groups
        }
        let mut requests = core(ranges, |r| r.core_requests);
        let request_minors = ext(ranges, &mut requests, |r| {
            (r.ext_requests_major, r.ext_requests_minor)
        });
        let mut replies = core(ranges, |r| r.core_replies);
        let reply_minors = ext(ranges, &mut replies, |r| {
            (r.ext_replies_major, r.ext_replies_minor)
        });
        Self {
            clients,
            requests: make_set(requests),
            request_minors,
            replies: make_set(replies),
            reply_minors,
            delivered_events: make_set(core(ranges, |r| r.delivered_events)),
            device_events: make_set(core(ranges, |r| r.device_events)),
            errors: make_set(core(ranges, |r| r.errors)),
            client_started: ranges.iter().any(|r| r.client_started != 0),
            client_died: ranges.iter().any(|r| r.client_died != 0),
        }
    }

    /// The ranges GetContext reports (Xorg `ProcRecordGetContext`): each
    /// kind fills slots from 0 independently, so kinds share ranges.
    fn to_ranges(&self) -> Vec<Range> {
        fn slot(ranges: &mut Vec<Range>, index: usize) -> &mut Range {
            if ranges.len() <= index {
                ranges.resize(index + 1, Range::default());
            }
            &mut ranges[index]
        }
        #[allow(clippy::cast_possible_truncation)]
        fn fill(ranges: &mut Vec<Range>, set: &Set, max: u16, put: fn(&mut Range, (u8, u8))) {
            for (index, (first, last)) in capped(set, max).enumerate() {
                put(slot(ranges, index), (first as u8, last as u8));
            }
        }
        fn fill_ext(
            ranges: &mut Vec<Range>,
            groups: &[MinorSets],
            put: fn(&mut Range, (u8, u8), (u16, u16)),
        ) {
            let mut index = 0;
            for group in groups {
                for minor in capped(&group.minors, u16::MAX) {
                    put(slot(ranges, index), group.major, minor);
                    index += 1;
                }
            }
        }
        let mut ranges = Vec::new();
        fill(&mut ranges, &self.requests, 127, |r, v| r.core_requests = v);
        fill(&mut ranges, &self.replies, 127, |r, v| r.core_replies = v);
        fill(&mut ranges, &self.delivered_events, 255, |r, v| {
            r.delivered_events = v;
        });
        fill(&mut ranges, &self.device_events, 255, |r, v| {
            r.device_events = v;
        });
        fill(&mut ranges, &self.errors, 255, |r, v| r.errors = v);
        fill_ext(&mut ranges, &self.request_minors, |r, major, minor| {
            r.ext_requests_major = major;
            r.ext_requests_minor = minor;
        });
        fill_ext(&mut ranges, &self.reply_minors, |r, major, minor| {
            r.ext_replies_major = major;
            r.ext_replies_minor = minor;
        });
        if self.client_started || self.client_died {
            let first = slot(&mut ranges, 0);
            first.client_started = u8::from(self.client_started);
            first.client_died = u8::from(self.client_died);
        }
        ranges
    }
}

#[derive(Clone, Copy, Debug)]
struct Recorder {
    client: ClientId,
    /// The EnableContext request's sequence number; every reply of the
    /// stream carries it.
    sequence: SequenceNumber,
}

#[derive(Debug)]
struct RecordContext {
    owner: ClientId,
    recorder: Option<Recorder>,
    element_header: u8,
    /// Index 0 is the head of Xorg's `pListOfRCAP` (newest first).
    rcaps: Vec<Rcap>,
}

impl RecordContext {
    /// Xorg `RecordFindClientOnContext`: (rcap index, position).
    fn find_client(&self, spec: u32) -> Option<(usize, usize)> {
        self.rcaps.iter().enumerate().find_map(|(index, rcap)| {
            rcap.clients
                .iter()
                .position(|c| *c == spec)
                .map(|pos| (index, pos))
        })
    }

    /// Xorg `RecordDeleteClientFromContext`: the last client moves into the
    /// vacated slot, and an RCAP left with no clients goes away.
    fn delete_client(&mut self, spec: u32) {
        if let Some((index, pos)) = self.find_client(spec) {
            let rcap = &mut self.rcaps[index];
            rcap.clients.swap_remove(pos);
            if rcap.clients.is_empty() {
                self.rcaps.remove(index);
            }
        }
    }
}

/// Every RECORD context, keyed by XID.
#[derive(Debug, Default)]
pub struct RecordState {
    contexts: BTreeMap<u32, RecordContext>,
    /// Data connections whose stream write failed, awaiting disconnect.
    failed: Vec<ClientId>,
}

impl RecordState {
    #[must_use]
    pub fn contains(&self, id: u32) -> bool {
        self.contexts.contains_key(&id)
    }

    pub fn ids(&self) -> impl Iterator<Item = u32> + '_ {
        self.contexts.keys().copied()
    }

    #[cfg(test)]
    pub(crate) fn fail_recorder_for_test(&mut self, client: ClientId) {
        self.failed.push(client);
    }

    #[cfg(test)]
    pub(crate) fn insert_for_test(&mut self, id: u32, owner: ClientId) {
        self.contexts.insert(
            id,
            RecordContext {
                owner,
                recorder: None,
                element_header: 0,
                rcaps: Vec::new(),
            },
        );
    }
}

/// Whether `client` is the data connection of an enabled context, whose
/// requests are not dispatched until the context is disabled.
#[must_use]
pub fn client_is_recording(state: &ServerState, client: ClientId) -> bool {
    state
        .record
        .contexts
        .values()
        .any(|ctx| ctx.recorder.is_some_and(|r| r.client == client))
}

/// Whether `client`'s queued requests must wait: it records an enabled
/// context, or its stream write failed and it awaits disconnect (its queued
/// requests must never run).
#[must_use]
pub fn client_blocks_requests(state: &ServerState, client: ClientId) -> bool {
    client_is_recording(state, client) || state.record.failed.contains(&client)
}

/// The running client whose XID range holds `xid`: (id, base, mask).
fn owning_client(state: &ServerState, xid: u32) -> Option<(ClientId, u32, u32)> {
    state
        .clients
        .iter()
        .find(|(_, c)| (xid & !c.resource_id_mask) == c.resource_id_base)
        .map(|(id, c)| (ClientId(*id), c.resource_id_base, c.resource_id_mask))
}

fn client_base(state: &ServerState, client: ClientId) -> Option<u32> {
    state.clients.get(&client.0).map(|c| c.resource_id_base)
}

/// An X error to send back: (code, value).
type Error = (u8, u32);

/// Xorg `RecordSanityCheckClientSpecifiers`. `recorder` is the enabled
/// context's data connection, which may not be named.
fn check_client_specs(
    state: &ServerState,
    specs: &[u32],
    recorder: Option<ClientId>,
) -> Result<(), Error> {
    let recorder = recorder.and_then(|r| state.clients.get(&r.0));
    for &spec in specs {
        if matches!(
            spec,
            x11rec::CURRENT_CLIENTS | x11rec::FUTURE_CLIENTS | x11rec::ALL_CLIENTS
        ) {
            continue;
        }
        if recorder.is_some_and(|r| (spec & !r.resource_id_mask) == r.resource_id_base) {
            return Err((x11::error::BAD_MATCH, 0));
        }
        match owning_client(state, spec) {
            Some((_, base, _)) if spec == base => {}
            Some(_) if state.xid_occupied(spec) => {}
            Some(_) => return Err((x11::error::BAD_VALUE, spec)),
            None => return Err((x11::error::BAD_MATCH, 0)),
        }
    }
    Ok(())
}

/// Xorg `RecordCanonicalizeClientSpecifiers`: XIDs become their client's
/// base, duplicates go, and the first All/Current expands to every running
/// client but `exclude` (All adds FutureClients) and replaces the list.
fn canonicalize(state: &ServerState, specs: &[u32], exclude: Option<u32>) -> Vec<u32> {
    let mut specs: Vec<u32> = specs
        .iter()
        .map(|&spec| {
            if spec > x11rec::ALL_CLIENTS {
                owning_client(state, spec).map_or(spec, |(_, base, _)| base)
            } else {
                spec
            }
        })
        .collect();
    let mut n = specs.len();
    let mut i = 0;
    while i < n {
        if matches!(specs[i], x11rec::ALL_CLIENTS | x11rec::CURRENT_CLIENTS) {
            let mut bases: Vec<u32> = state
                .clients
                .values()
                .map(|c| c.resource_id_base)
                .filter(|base| Some(*base) != exclude)
                .collect();
            // Xorg walks clients by index, i.e. by base.
            bases.sort_unstable();
            if specs[i] == x11rec::ALL_CLIENTS {
                bases.push(x11rec::FUTURE_CLIENTS);
            }
            return bases;
        }
        let mut j = i + 1;
        while j < n {
            if specs[i] == specs[j] {
                specs[j] = specs[n - 1];
                n -= 1;
            } else {
                j += 1;
            }
        }
        i += 1;
    }
    specs.truncate(n);
    specs
}

/// Xorg `RecordSanityCheckRegisterClients` for a CreateContext /
/// RegisterClients body: the client specs and ranges, or the error.
fn check_register(
    state: &ServerState,
    byte_order: ClientByteOrder,
    body: &[u8],
    head: &x11rec::RegisterHeader,
    recorder: Option<ClientId>,
) -> Result<(Vec<u32>, Vec<Range>), Error> {
    if head.n_clients > LIMIT_CLIENTS {
        return Err((x11::error::BAD_VALUE, 0));
    }
    let max_ranges = (i64::from(i32::MAX) - 4 * i64::from(head.n_clients)) / 24;
    if i64::from(head.n_ranges) > max_ranges {
        return Err((x11::error::BAD_VALUE, 0));
    }
    let n_clients = head.n_clients as usize;
    let n_ranges = head.n_ranges as usize;
    if body.len() - x11rec::REGISTER_FIXED_BODY != 4 * n_clients + x11rec::RANGE_SIZE * n_ranges {
        return Err((x11::error::BAD_LENGTH, 0));
    }
    if head.element_header
        & !(x11rec::FROM_SERVER_TIME | x11rec::FROM_CLIENT_TIME | x11rec::FROM_CLIENT_SEQUENCE)
        != 0
    {
        return Err((x11::error::BAD_VALUE, u32::from(head.element_header)));
    }
    let specs: Vec<u32> = (0..n_clients)
        .map(|i| x11rec::read_list_u32(byte_order, body, x11rec::REGISTER_FIXED_BODY, i))
        .collect();
    check_client_specs(state, &specs, recorder)?;
    let ranges_at = x11rec::REGISTER_FIXED_BODY + 4 * n_clients;
    let ranges: Vec<Range> = (0..n_ranges)
        .map(|i| Range::parse(byte_order, &body[ranges_at + x11rec::RANGE_SIZE * i..]))
        .collect();
    for r in &ranges {
        check_range(r)?;
    }
    Ok((specs, ranges))
}

/// Xorg's per-range checks, in its order; the value is the offending
/// interval's first field.
fn check_range(r: &Range) -> Result<(), Error> {
    let bad = |v: u32| Err((x11::error::BAD_VALUE, v));
    let bad_major = |(first, last): (u8, u8)| {
        (first != 0 || last != 0) && (first < 128 || last < 128 || first > last)
    };
    let bad_event = |(first, last): (u8, u8)| {
        (first != 0 || last != 0) && (first < 2 || last < 2 || first > last)
    };
    if r.core_requests.0 > r.core_requests.1 {
        return bad(r.core_requests.0.into());
    }
    if r.core_replies.0 > r.core_replies.1 {
        return bad(r.core_replies.0.into());
    }
    if bad_major(r.ext_requests_major) {
        return bad(r.ext_requests_major.0.into());
    }
    if r.ext_requests_minor.0 > r.ext_requests_minor.1 {
        return bad(r.ext_requests_minor.0.into());
    }
    if bad_major(r.ext_replies_major) {
        return bad(r.ext_replies_major.0.into());
    }
    if r.ext_replies_minor.0 > r.ext_replies_minor.1 {
        return bad(r.ext_replies_minor.0.into());
    }
    if bad_event(r.delivered_events) {
        return bad(r.delivered_events.0.into());
    }
    if bad_event(r.device_events) {
        return bad(r.device_events.0.into());
    }
    if r.errors.0 > r.errors.1 {
        return bad(r.errors.0.into());
    }
    if r.client_started > 1 {
        return bad(r.client_started.into());
    }
    if r.client_died > 1 {
        return bad(r.client_died.into());
    }
    Ok(())
}

/// Xorg `SwapCreateRegister`'s length checks, which a byte-swapped client
/// hits before anything else in the request.
fn check_swapped_register_length(body: &[u8], head: &x11rec::RegisterHeader) -> Result<(), Error> {
    let words = ((body.len() + 4) / 4) as u64;
    let avail = words.saturating_sub(5);
    if u64::from(head.n_clients) > avail
        || u64::from(head.n_ranges) > (avail - u64::from(head.n_clients)) / 6
    {
        return Err((x11::error::BAD_LENGTH, 0));
    }
    Ok(())
}

/// Xorg `RecordRegisterClients` after its sanity checks.
fn register_clients(
    state: &mut ServerState,
    context: u32,
    element_header: u8,
    specs: &[u32],
    ranges: &[Range],
) {
    let exclude = state.record.contexts[&context]
        .recorder
        .and_then(|r| client_base(state, r.client));
    let canon = if specs.is_empty() {
        Vec::new()
    } else {
        canonicalize(state, specs, exclude)
    };
    let ctx = state
        .record
        .contexts
        .get_mut(&context)
        .expect("context exists");
    ctx.element_header = element_header;
    if specs.is_empty() {
        return;
    }
    for &spec in &canon {
        ctx.delete_client(spec);
    }
    ctx.rcaps.insert(0, Rcap::from_ranges(canon, ranges));
}

/// Send one reply to a context's data connection. A recorder whose write
/// fails (peer gone, or `OUTBOUND_CAP` reached) stops recording at once, as
/// its stream can no longer be whole, and is queued for the core loop to
/// disconnect (`take_failed_recorders`): this can run inside another
/// client's disconnect, so it never disconnects inline.
fn send_to_recorder(state: &mut ServerState, recorder: ClientId, bytes: &[u8]) {
    if state.record.failed.contains(&recorder) {
        return;
    }
    let Some(client) = state.clients.get_mut(&recorder.0) else {
        return;
    };
    crate::core_loop::fanout::record_outbound_telemetry(recorder, client.byte_order, bytes);
    if matches!(
        client_io::write_or_buffer(client, bytes),
        Ok(WriteOutcome::Done | WriteOutcome::WouldBlock)
    ) {
        return;
    }
    log::warn!(
        "record: data connection {} cannot take its stream; disconnecting it",
        recorder.0
    );
    for ctx in state.record.contexts.values_mut() {
        if ctx.recorder.is_some_and(|r| r.client == recorder) {
            ctx.recorder = None;
        }
    }
    state.record.failed.push(recorder);
}

/// RECORD data connections whose stream write failed, for the core loop to
/// disconnect.
pub(crate) fn take_failed_recorders(state: &mut ServerState) -> Vec<ClientId> {
    std::mem::take(&mut state.record.failed)
}

/// Recording client's byte order, if it is still connected.
fn recorder_order(state: &ServerState, recorder: Recorder) -> Option<ClientByteOrder> {
    state.clients.get(&recorder.client.0).map(|c| c.byte_order)
}

/// A StartOfData / EndOfData reply (no client, no data).
fn send_marker(state: &mut ServerState, recorder: Recorder, element_header: u8, category: u8) {
    let Some(order) = recorder_order(state, recorder) else {
        return;
    };
    let reply = x11rec::encode_enable_context_header(
        order,
        recorder.sequence,
        category,
        0,
        element_header,
        order == ClientByteOrder::BigEndian,
        0,
        state.timestamp_now(),
        0,
    );
    send_to_recorder(state, recorder.client, &reply);
}

/// Xorg `RecordDisableContext`: EndOfData unless `recorder_gone`, and the
/// data connection's requests run again.
fn disable_context(state: &mut ServerState, context: u32, recorder_gone: bool) {
    let Some(ctx) = state.record.contexts.get_mut(&context) else {
        return;
    };
    let Some(recorder) = ctx.recorder.take() else {
        return;
    };
    let element_header = ctx.element_header;
    log::debug!(
        "record: context 0x{context:x} disabled, client {} resumes",
        recorder.client.0
    );
    if !recorder_gone {
        send_marker(state, recorder, element_header, x11rec::END_OF_DATA);
    }
}

/// Xorg `RecordDeleteContext`.
fn free_context(state: &mut ServerState, context: u32) {
    disable_context(state, context, false);
    state.record.contexts.remove(&context);
}

pub(crate) fn handle_record_request(
    state: &mut ServerState,
    client_id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<RequestOutcome> {
    let minor = header.data;
    let Some(byte_order) = state.clients.get(&client_id.0).map(|c| c.byte_order) else {
        return Ok(RequestOutcome::Handled);
    };
    let error = |state: &mut ServerState, (code, value): Error| {
        emit_x11_error_with_minor(
            state,
            client_id,
            sequence,
            code,
            value,
            u16::from(minor),
            RECORD_MAJOR_OPCODE,
        )
    };
    let bad_context = |state: &mut ServerState, id: u32| {
        error(state, (RECORD_FIRST_ERROR + x11rec::BAD_CONTEXT, id))
    };
    let exact = |len: usize| body.len() == len;
    match minor {
        x11rec::QUERY_VERSION => {
            if !exact(4) {
                return error(state, (x11::error::BAD_LENGTH, 0));
            }
            let reply = x11rec::encode_query_version_reply(byte_order, sequence);
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            Ok(write_to_client(client, client_id, &reply))
        }
        x11rec::CREATE_CONTEXT | x11rec::REGISTER_CLIENTS => {
            let Some(head) = x11rec::parse_register_header(byte_order, body) else {
                return error(state, (x11::error::BAD_LENGTH, 0));
            };
            if byte_order == ClientByteOrder::BigEndian
                && let Err(err) = check_swapped_register_length(body, &head)
            {
                return error(state, err);
            }
            let recorder = if minor == x11rec::CREATE_CONTEXT {
                let base = client_base(state, client_id).unwrap_or(0);
                let mask = state
                    .clients
                    .get(&client_id.0)
                    .map_or(0, |c| c.resource_id_mask);
                if (head.context & !mask) != base || state.xid_occupied(head.context) {
                    return error(state, (x11::error::BAD_ID_CHOICE, head.context));
                }
                None
            } else {
                let Some(ctx) = state.record.contexts.get(&head.context) else {
                    return bad_context(state, head.context);
                };
                ctx.recorder.map(|r| r.client)
            };
            let (specs, ranges) = match check_register(state, byte_order, body, &head, recorder) {
                Ok(parsed) => parsed,
                Err(err) => return error(state, err),
            };
            if minor == x11rec::CREATE_CONTEXT {
                state.record.contexts.insert(
                    head.context,
                    RecordContext {
                        owner: client_id,
                        recorder: None,
                        element_header: 0,
                        rcaps: Vec::new(),
                    },
                );
            }
            register_clients(state, head.context, head.element_header, &specs, &ranges);
            log::debug!(
                "client {} #{} RECORD {} context 0x{:x} clients={specs:x?} ranges={}",
                client_id.0,
                sequence.0,
                if minor == x11rec::CREATE_CONTEXT {
                    "CreateContext"
                } else {
                    "RegisterClients"
                },
                head.context,
                ranges.len(),
            );
            Ok(RequestOutcome::Handled)
        }
        x11rec::UNREGISTER_CLIENTS => {
            if body.len() < x11rec::UNREGISTER_FIXED_BODY {
                return error(state, (x11::error::BAD_LENGTH, 0));
            }
            let context = x11rec::read_body_u32(byte_order, body, 0);
            let n_clients = x11rec::read_body_u32(byte_order, body, 4);
            if u64::from(n_clients) * 4 != (body.len() - x11rec::UNREGISTER_FIXED_BODY) as u64 {
                return error(state, (x11::error::BAD_LENGTH, 0));
            }
            if !state.record.contains(context) {
                return bad_context(state, context);
            }
            let specs: Vec<u32> = (0..n_clients as usize)
                .map(|i| x11rec::read_list_u32(byte_order, body, x11rec::UNREGISTER_FIXED_BODY, i))
                .collect();
            if let Err(err) = check_client_specs(state, &specs, None) {
                return error(state, err);
            }
            let canon = canonicalize(state, &specs, None);
            let ctx = state
                .record
                .contexts
                .get_mut(&context)
                .expect("context exists");
            for spec in canon {
                ctx.delete_client(spec);
            }
            Ok(RequestOutcome::Handled)
        }
        x11rec::GET_CONTEXT => {
            if !exact(4) {
                return error(state, (x11::error::BAD_LENGTH, 0));
            }
            let context = x11rec::read_body_u32(byte_order, body, 0);
            let Some(ctx) = state.record.contexts.get(&context) else {
                return bad_context(state, context);
            };
            let per_rcap: Vec<(&Rcap, Vec<Range>)> =
                ctx.rcaps.iter().map(|r| (r, r.to_ranges())).collect();
            let infos: Vec<(u32, &[Range])> = per_rcap
                .iter()
                .flat_map(|(rcap, ranges)| {
                    rcap.clients
                        .iter()
                        .map(move |client| (*client, ranges.as_slice()))
                })
                .collect();
            let reply = x11rec::encode_get_context_reply(
                byte_order,
                sequence,
                ctx.recorder.is_some(),
                ctx.element_header,
                &infos,
            );
            let Some(client) = state.clients.get_mut(&client_id.0) else {
                return Ok(RequestOutcome::Handled);
            };
            Ok(write_to_client(client, client_id, &reply))
        }
        x11rec::ENABLE_CONTEXT => {
            if !exact(4) {
                return error(state, (x11::error::BAD_LENGTH, 0));
            }
            let context = x11rec::read_body_u32(byte_order, body, 0);
            let Some(ctx) = state.record.contexts.get(&context) else {
                return bad_context(state, context);
            };
            if ctx.recorder.is_some() {
                return error(state, (x11::error::BAD_MATCH, 0));
            }
            let base = client_base(state, client_id).unwrap_or(0);
            let recorder = Recorder {
                client: client_id,
                sequence,
            };
            let ctx = state
                .record
                .contexts
                .get_mut(&context)
                .expect("context exists");
            ctx.recorder = Some(recorder);
            // The data connection never records itself.
            ctx.delete_client(base);
            let element_header = ctx.element_header;
            log::debug!(
                "client {} #{} RECORD EnableContext 0x{context:x}: requests suspended",
                client_id.0,
                sequence.0,
            );
            send_marker(state, recorder, element_header, x11rec::START_OF_DATA);
            Ok(RequestOutcome::Handled)
        }
        x11rec::DISABLE_CONTEXT | x11rec::FREE_CONTEXT => {
            if !exact(4) {
                return error(state, (x11::error::BAD_LENGTH, 0));
            }
            let context = x11rec::read_body_u32(byte_order, body, 0);
            if !state.record.contains(context) {
                return bad_context(state, context);
            }
            if minor == x11rec::DISABLE_CONTEXT {
                disable_context(state, context, false);
            } else {
                free_context(state, context);
            }
            Ok(RequestOutcome::Handled)
        }
        other => {
            log::debug!("client {} RECORD minor {other} -> BadRequest", client_id.0);
            error(state, (x11::error::BAD_REQUEST, 0))
        }
    }
}

/// A core device event as the RECORD device-event hook sees it: the event
/// as clients get it, before grabs and delivery.
#[derive(Clone, Copy, Debug)]
pub struct RecordedDeviceEvent {
    /// Core event code, KeyPress (2) through MotionNotify (6).
    pub event_type: u8,
    pub detail: u8,
    /// A server-generated autorepeat press.
    pub repeat: bool,
    pub time: u32,
    pub root_x: i16,
    pub root_y: i16,
    /// Key/button state before the event.
    pub state: u16,
}

/// Xorg `RecordADeviceEvent` for a master-device core event: every enabled
/// context's every RCAP whose device-event set holds the type records it,
/// whichever client caused it.
pub fn record_device_event(state: &mut ServerState, event: RecordedDeviceEvent) {
    let mut targets: Vec<(Recorder, u8)> = Vec::new();
    for ctx in state.record.contexts.values() {
        let Some(recorder) = ctx.recorder else {
            continue;
        };
        for rcap in &ctx.rcaps {
            if set_contains(&rcap.device_events, u16::from(event.event_type)) {
                targets.push((recorder, ctx.element_header));
            }
        }
    }
    for (recorder, element_header) in targets {
        let Some(order) = recorder_order(state, recorder) else {
            continue;
        };
        let now = state.timestamp_now();
        let mut data = Vec::with_capacity(36);
        if element_header & x11rec::FROM_SERVER_TIME != 0 {
            x11::write_u32(order, &mut data, now);
        }
        // Xorg records a root window only for motion (exevents.c).
        let root = if event.event_type == 6 {
            ROOT_WINDOW.0
        } else {
            0
        };
        data.extend_from_slice(&x11rec::encode_core_device_event(
            order,
            event.event_type,
            event.detail,
            event.repeat,
            event.time,
            root,
            event.root_x,
            event.root_y,
            event.state,
        ));
        let mut reply = x11rec::encode_enable_context_header(
            order,
            recorder.sequence,
            x11rec::FROM_SERVER,
            u32::try_from(data.len() / 4).unwrap_or(0),
            element_header,
            false,
            0,
            now,
            0,
        );
        reply.extend_from_slice(&data);
        send_to_recorder(state, recorder.client, &reply);
    }
}

/// Xorg's ClientStateRunning callback: every context holding FutureClients
/// registers the new client on that RCAP; enabled ones whose RCAP selects
/// clientStarted record the setup reply the client received.
pub(crate) fn client_started(state: &mut ServerState, client: ClientId, setup_reply: &[u8]) {
    let Some((base, order)) = state
        .clients
        .get(&client.0)
        .map(|c| (c.resource_id_base, c.byte_order))
    else {
        return;
    };
    let mut announce: Vec<(Recorder, u8)> = Vec::new();
    for ctx in state.record.contexts.values_mut() {
        let Some((index, _)) = ctx.find_client(x11rec::FUTURE_CLIENTS) else {
            continue;
        };
        let rcap = &mut ctx.rcaps[index];
        rcap.clients.push(base);
        if let Some(recorder) = ctx.recorder
            && rcap.client_started
        {
            announce.push((recorder, ctx.element_header));
        }
    }
    for (recorder, element_header) in announce {
        let Some(recorder_order) = recorder_order(state, recorder) else {
            continue;
        };
        let mut reply = x11rec::encode_enable_context_header(
            recorder_order,
            recorder.sequence,
            x11rec::CLIENT_STARTED,
            u32::try_from(setup_reply.len() / 4).unwrap_or(0),
            element_header,
            order != recorder_order,
            base,
            state.timestamp_now(),
            0,
        );
        reply.extend_from_slice(setup_reply);
        send_to_recorder(state, recorder.client, &reply);
    }
}

/// Xorg's ClientStateGone callback followed by the client's resource
/// teardown: contexts it was recording stop without EndOfData, it leaves
/// every context (ClientDied where selected), and contexts it created are
/// freed. Runs while the client is still in `state.clients`.
pub(crate) fn client_disconnected(state: &mut ServerState, client: ClientId) {
    state.record.failed.retain(|c| *c != client);
    if state.record.contexts.is_empty() {
        return;
    }
    let Some((base, order, last_sequence)) = state.clients.get(&client.0).map(|c| {
        (
            c.resource_id_base,
            c.byte_order,
            c.last_sequence.load(std::sync::atomic::Ordering::Relaxed),
        )
    }) else {
        return;
    };
    let ids: Vec<u32> = state.record.ids().collect();
    for &id in &ids {
        if state.record.contexts[&id]
            .recorder
            .is_some_and(|r| r.client == client)
        {
            disable_context(state, id, true);
        }
        let ctx = state.record.contexts.get_mut(&id).expect("context exists");
        let Some((index, _)) = ctx.find_client(base) else {
            continue;
        };
        let died = ctx.recorder.filter(|_| ctx.rcaps[index].client_died);
        let element_header = ctx.element_header;
        ctx.delete_client(base);
        if let Some(recorder) = died
            && let Some(recorder_order) = recorder_order(state, recorder)
        {
            let mut data = Vec::new();
            if element_header & x11rec::FROM_CLIENT_SEQUENCE != 0 {
                x11::write_u32(recorder_order, &mut data, u32::from(last_sequence));
            }
            let mut reply = x11rec::encode_enable_context_header(
                recorder_order,
                recorder.sequence,
                x11rec::CLIENT_DIED,
                u32::try_from(data.len() / 4).unwrap_or(0),
                element_header,
                order != recorder_order,
                base,
                state.timestamp_now(),
                u32::from(last_sequence),
            );
            reply.extend_from_slice(&data);
            send_to_recorder(state, recorder.client, &reply);
        }
    }
    for id in ids {
        if state
            .record
            .contexts
            .get(&id)
            .is_some_and(|ctx| ctx.owner == client)
        {
            free_context(state, id);
        }
    }
}

/// Goldens are Xvfb 21.1.24 captures from `tools/record-probe.c`, run with
/// `RECORD_PROBE_HEX=1` (raw bytes) or decoded; the scenario is named on
/// each test. Clients get Xvfb's resource bases (0x200000, 0x400000,
/// 0x600000, mask 0x1fffff) so ids compare byte for byte. Differences that
/// are not ours to match: Xvfb's major opcode 146 and first error 154
/// (yserver 154 / 189), its root window 0x39f, the uninitialised pad bytes
/// of its EnableContext replies (zero here), and its batching of several
/// FromServer elements into one reply (one reply per element here).
#[cfg(test)]
mod tests {
    use std::{
        collections::{HashMap, HashSet, VecDeque},
        io::Read,
        os::unix::net::UnixStream,
        sync::{
            Arc, Mutex,
            atomic::{AtomicU16, Ordering},
        },
    };

    use yserver_protocol::x11::ClientByteOrder::{BigEndian, LittleEndian};

    use super::*;
    use crate::{
        backend::recording::RecordingBackend, core_loop::process_request::process_request,
        server::ClientState,
    };

    const REC: u32 = 1;
    const CTL: u32 = 2;
    const C3: u32 = 3;
    const REC_BASE: u32 = 0x0020_0000;
    const CTL_BASE: u32 = 0x0040_0000;
    const C3_BASE: u32 = 0x0060_0000;
    const MASK: u32 = 0x001F_FFFF;
    const BAD_CONTEXT: u8 = RECORD_FIRST_ERROR;

    fn install(state: &mut ServerState, id: u32, base: u32, order: ClientByteOrder) -> UnixStream {
        let (a, b) = UnixStream::pair().unwrap();
        b.set_nonblocking(true).unwrap();
        state.clients.insert(
            id,
            ClientState {
                writer: Arc::new(Mutex::new(crate::transport::Transport::Unix(a))),
                byte_order: order,
                last_sequence: Arc::new(AtomicU16::new(0)),
                resource_id_base: base,
                resource_id_mask: MASK,
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
                is_local: true,
                fd_passing: true,
            },
        );
        b
    }

    fn read_all(peer: &mut UnixStream) -> Vec<u8> {
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        while let Ok(n) = peer.read(&mut buf) {
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        out
    }

    fn send(state: &mut ServerState, client: u32, seq: u16, minor: u8, body: &[u8]) {
        process_request(
            state,
            &mut RecordingBackend::new(),
            ClientId(client),
            SequenceNumber(seq),
            RequestHeader {
                opcode: RECORD_MAJOR_OPCODE,
                data: minor,
                length_units: u32::try_from(1 + body.len() / 4).unwrap(),
            },
            body,
            None,
        )
        .expect("RECORD request");
    }

    fn order_of(state: &ServerState, client: u32) -> ClientByteOrder {
        state.clients[&client].byte_order
    }

    fn put32(order: ClientByteOrder, out: &mut Vec<u8>, v: u32) {
        x11::write_u32(order, out, v);
    }

    fn register_body(
        order: ClientByteOrder,
        ctx: u32,
        ehdr: u8,
        specs: &[u32],
        ranges: &[Range],
    ) -> Vec<u8> {
        let mut out = Vec::new();
        put32(order, &mut out, ctx);
        out.extend_from_slice(&[ehdr, 0, 0, 0]);
        put32(order, &mut out, u32::try_from(specs.len()).unwrap());
        put32(order, &mut out, u32::try_from(ranges.len()).unwrap());
        for spec in specs {
            put32(order, &mut out, *spec);
        }
        for range in ranges {
            range.encode(order, &mut out);
        }
        out
    }

    fn words(order: ClientByteOrder, values: &[u32]) -> Vec<u8> {
        let mut out = Vec::new();
        for v in values {
            put32(order, &mut out, *v);
        }
        out
    }

    fn create(
        state: &mut ServerState,
        client: u32,
        seq: u16,
        ctx: u32,
        ehdr: u8,
        specs: &[u32],
        ranges: &[Range],
    ) {
        let body = register_body(order_of(state, client), ctx, ehdr, specs, ranges);
        send(state, client, seq, x11rec::CREATE_CONTEXT, &body);
    }

    fn ctx_request(state: &mut ServerState, client: u32, seq: u16, minor: u8, ctx: u32) {
        let body = words(order_of(state, client), &[ctx]);
        send(state, client, seq, minor, &body);
    }

    /// "01000300 1c000000 …" as bytes.
    fn hex(s: &str) -> Vec<u8> {
        let digits: Vec<u8> = s.bytes().filter(u8::is_ascii_hexdigit).collect();
        digits
            .chunks(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }

    /// Split a byte stream into X replies / errors.
    fn packets(order: ClientByteOrder, bytes: &[u8]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut at = 0;
        while at < bytes.len() {
            let extra = if bytes[at] == 1 {
                4 * x11::read_u32(order, &bytes[at + 4..at + 8]) as usize
            } else {
                0
            };
            out.push(bytes[at..at + 32 + extra].to_vec());
            at += 32 + extra;
        }
        out
    }

    /// (code, value, minor) of an error packet, checking it is RECORD's.
    fn error_of(order: ClientByteOrder, packet: &[u8]) -> (u8, u32, u16) {
        assert_eq!(packet[0], 0, "expected an error, got {packet:02x?}");
        assert_eq!(packet[10], RECORD_MAJOR_OPCODE);
        let minor = match order {
            LittleEndian => u16::from_le_bytes([packet[8], packet[9]]),
            BigEndian => u16::from_be_bytes([packet[8], packet[9]]),
        };
        (packet[1], x11::read_u32(order, &packet[4..8]), minor)
    }

    type ErrorCase = (&'static str, u8, Vec<u8>, u8, Option<u32>);

    fn check_errors(
        state: &mut ServerState,
        rec: &mut UnixStream,
        order: ClientByteOrder,
        seq: &mut u16,
        cases: Vec<ErrorCase>,
    ) {
        for (name, minor, body, code, value) in cases {
            *seq += 1;
            send(state, REC, *seq, minor, &body);
            let got = read_all(rec);
            assert_eq!(got.len(), 32, "{order:?} {name}: one error");
            let (got_code, got_value, got_minor) = error_of(order, &got);
            assert_eq!(got_code, code, "{order:?} {name}");
            assert_eq!(got_minor, u16::from(minor), "{order:?} {name}");
            if let Some(value) = value {
                assert_eq!(got_value, value, "{order:?} {name}");
            }
        }
    }

    /// Client resources a GetContext reply lists, in order.
    fn get_context_clients(order: ClientByteOrder, reply: &[u8]) -> Vec<u32> {
        let n = x11::read_u32(order, &reply[12..16]) as usize;
        let mut at = 32;
        let mut out = Vec::new();
        for _ in 0..n {
            out.push(x11::read_u32(order, &reply[at..at + 4]));
            let ranges = x11::read_u32(order, &reply[at + 4..at + 8]) as usize;
            at += 8 + 24 * ranges;
        }
        out
    }

    /// Patch the time words of an expected EnableContext reply from the
    /// reply actually sent: header `server_time` and every word listed.
    fn with_times(mut expected: Vec<u8>, got: &[u8], word_offsets: &[usize]) -> Vec<u8> {
        for &at in [16].iter().chain(word_offsets) {
            expected[at..at + 4].copy_from_slice(&got[at..at + 4]);
        }
        expected
    }

    fn device_range() -> Range {
        Range {
            device_events: (2, 6),
            client_started: 1,
            client_died: 1,
            ..Range::default()
        }
    }

    /// `record-probe ranges l`: kinds share range slots, abutting intervals
    /// merge, core request/reply columns stop at 127, extension minors are
    /// grouped per major interval, and a duplicate client spec is dropped.
    #[test]
    fn get_context_merges_ranges_like_xorg() {
        let mut state = ServerState::new();
        let mut rec = install(&mut state, REC, REC_BASE, LittleEndian);
        let _ctl = install(&mut state, CTL, CTL_BASE, LittleEndian);
        let ranges = [
            Range {
                core_requests: (10, 20),
                core_replies: (5, 5),
                ext_requests_major: (150, 150),
                ext_requests_minor: (3, 7),
                device_events: (2, 3),
                errors: (1, 4),
                client_started: 1,
                ..Range::default()
            },
            Range {
                core_requests: (21, 30),
                ext_requests_major: (150, 150),
                ext_requests_minor: (8, 9),
                delivered_events: (12, 14),
                device_events: (5, 6),
                client_died: 1,
                ..Range::default()
            },
            Range {
                core_requests: (100, 200),
                core_replies: (1, 2),
                ext_requests_major: (160, 170),
                ext_replies_major: (140, 141),
                ext_replies_minor: (1, 2),
                ..Range::default()
            },
        ];
        let ctx = REC_BASE + 1;
        create(
            &mut state,
            REC,
            2,
            ctx,
            0,
            &[CTL_BASE, CTL_BASE, 2],
            &ranges,
        );
        ctx_request(&mut state, REC, 3, x11rec::GET_CONTEXT, ctx);
        let rcap1 = "0a1e0102 96960300 09008c8d 01000200 0c0e0203 01040101 \
                     647f0505 a0aa0000 00000000 00000000 00000506 00000000";
        assert_eq!(
            read_all(&mut rec),
            hex(&format!(
                "01000300 1c000000 00000000 02000000 00000000 00000000 00000000 00000000 \
                 00004000 02000000 {rcap1} 02000000 02000000 {rcap1}"
            ))
        );

        // The context's own XID names its owner; the new RCAP goes first.
        let dev4 = Range {
            device_events: (4, 4),
            ..Range::default()
        };
        let body = register_body(LittleEndian, ctx, 1, &[ctx], &[dev4]);
        send(&mut state, REC, 4, x11rec::REGISTER_CLIENTS, &body);
        ctx_request(&mut state, REC, 5, x11rec::GET_CONTEXT, ctx);
        let rcap2 = "00002000 01000000 00000000 00000000 00000000 00000000 00000404 00000000";
        assert_eq!(
            read_all(&mut rec),
            hex(&format!(
                "01000500 24000000 01000000 03000000 00000000 00000000 00000000 00000000 \
                 {rcap2} 00004000 02000000 {rcap1} 02000000 02000000 {rcap1}"
            ))
        );

        // No clients: only the element header changes.
        let body = register_body(LittleEndian, ctx, 5, &[], &[dev4]);
        send(&mut state, REC, 6, x11rec::REGISTER_CLIENTS, &body);
        ctx_request(&mut state, REC, 7, x11rec::GET_CONTEXT, ctx);
        assert_eq!(
            read_all(&mut rec),
            hex(&format!(
                "01000700 24000000 05000000 03000000 00000000 00000000 00000000 00000000 \
                 {rcap2} 00004000 02000000 {rcap1} 02000000 02000000 {rcap1}"
            ))
        );

        let unregister = |state: &mut ServerState, seq, spec| {
            send(
                state,
                REC,
                seq,
                x11rec::UNREGISTER_CLIENTS,
                &words(LittleEndian, &[ctx, 1, spec]),
            );
        };
        unregister(&mut state, 8, x11rec::FUTURE_CLIENTS);
        ctx_request(&mut state, REC, 9, x11rec::GET_CONTEXT, ctx);
        let reply = read_all(&mut rec);
        assert_eq!(
            get_context_clients(LittleEndian, &reply),
            [REC_BASE, CTL_BASE]
        );
        // CurrentClients expands to every running client; emptied RCAPs go.
        unregister(&mut state, 10, x11rec::CURRENT_CLIENTS);
        ctx_request(&mut state, REC, 11, x11rec::GET_CONTEXT, ctx);
        assert_eq!(
            read_all(&mut rec),
            hex("01000b00 00000000 05000000 00000000 00000000 00000000 00000000 00000000")
        );

        // AllClients replaces the whole spec list with its expansion.
        let body = register_body(
            LittleEndian,
            ctx,
            0,
            &[CTL_BASE, x11rec::ALL_CLIENTS, REC_BASE],
            &[device_range()],
        );
        send(&mut state, REC, 12, x11rec::REGISTER_CLIENTS, &body);
        ctx_request(&mut state, REC, 13, x11rec::GET_CONTEXT, ctx);
        let reply = read_all(&mut rec);
        assert_eq!(
            get_context_clients(LittleEndian, &reply),
            [REC_BASE, CTL_BASE, x11rec::FUTURE_CLIENTS]
        );

        let ctx2 = REC_BASE + 2;
        create(
            &mut state,
            REC,
            14,
            ctx2,
            0,
            &[x11rec::CURRENT_CLIENTS],
            &[dev4],
        );
        ctx_request(&mut state, REC, 15, x11rec::GET_CONTEXT, ctx2);
        let reply = read_all(&mut rec);
        assert_eq!(
            get_context_clients(LittleEndian, &reply),
            [REC_BASE, CTL_BASE]
        );
        crate::core_loop::process_disconnect::process_disconnect(
            &mut state,
            &mut RecordingBackend::new(),
            ClientId(CTL),
        );
        ctx_request(&mut state, REC, 16, x11rec::GET_CONTEXT, ctx2);
        let reply = read_all(&mut rec);
        assert_eq!(get_context_clients(LittleEndian, &reply), [REC_BASE]);
    }

    /// `record-probe errors l` and `errors B`. `None` marks an error value
    /// Xorg leaves stale (not set by the failing check).
    #[test]
    fn request_errors_match_xvfb() {
        for order in [LittleEndian, BigEndian] {
            let mut state = ServerState::new();
            let mut rec = install(&mut state, REC, REC_BASE, order);
            let _ctl = install(&mut state, CTL, CTL_BASE, LittleEndian);
            state.resources.create_cursor(
                ClientId(CTL),
                yserver_protocol::x11::ResourceId(CTL_BASE + 1),
            );
            let ctx = REC_BASE + 1;
            let bogus = REC_BASE + 0x32;
            let all = [x11rec::ALL_CLIENTS];
            let reg = |ctx: u32, ehdr: u8, specs: &[u32], ranges: &[Range]| {
                register_body(order, ctx, ehdr, specs, ranges)
            };
            let raw = |n_clients: u32, extra: usize| {
                let mut body = words(order, &[ctx, 0, n_clients, 0]);
                body.resize(16 + extra, 0);
                body
            };
            let big = order == BigEndian;
            let range = |r: Range| reg(ctx, 0, &all, &[r]);
            let cases: Vec<ErrorCase> = vec![
                ("short", 1, vec![0; 12], x11::error::BAD_LENGTH, None),
                (
                    "foreign id",
                    1,
                    reg(CTL_BASE + 1, 0, &all, &[device_range()]),
                    x11::error::BAD_ID_CHOICE,
                    Some(CTL_BASE + 1),
                ),
                (
                    "foreign id + ehdr 8",
                    1,
                    reg(CTL_BASE + 1, 8, &all, &[device_range()]),
                    x11::error::BAD_ID_CHOICE,
                    Some(CTL_BASE + 1),
                ),
                // Swapped clients fail SwapCreateRegister's length check first.
                (
                    "nClients=1000",
                    1,
                    raw(1000, 0),
                    if big {
                        x11::error::BAD_LENGTH
                    } else {
                        x11::error::BAD_VALUE
                    },
                    None,
                ),
                (
                    "nClients=300",
                    1,
                    raw(300, 4),
                    if big {
                        x11::error::BAD_LENGTH
                    } else {
                        x11::error::BAD_VALUE
                    },
                    None,
                ),
                (
                    "length mismatch",
                    1,
                    {
                        let mut b = raw(1, 4);
                        b.extend_from_slice(&[0; 4]);
                        b
                    },
                    x11::error::BAD_LENGTH,
                    None,
                ),
                (
                    "ehdr 8",
                    1,
                    reg(ctx, 8, &all, &[device_range()]),
                    x11::error::BAD_VALUE,
                    Some(8),
                ),
                (
                    "spec 4",
                    1,
                    reg(ctx, 0, &[4], &[device_range()]),
                    x11::error::BAD_MATCH,
                    None,
                ),
                (
                    "spec root",
                    1,
                    reg(ctx, 0, &[ROOT_WINDOW.0], &[device_range()]),
                    x11::error::BAD_MATCH,
                    None,
                ),
                (
                    "spec ctl+77",
                    1,
                    reg(ctx, 0, &[CTL_BASE + 0x77], &[device_range()]),
                    x11::error::BAD_VALUE,
                    Some(CTL_BASE + 0x77),
                ),
                (
                    "spec 0x1ff00000",
                    1,
                    reg(ctx, 0, &[0x1ff0_0000], &[device_range()]),
                    x11::error::BAD_MATCH,
                    None,
                ),
                (
                    "spec rec+5",
                    1,
                    reg(ctx, 0, &[REC_BASE + 5], &[device_range()]),
                    x11::error::BAD_VALUE,
                    Some(REC_BASE + 5),
                ),
                (
                    "core req 5-4",
                    1,
                    range(Range {
                        core_requests: (5, 4),
                        ..Range::default()
                    }),
                    x11::error::BAD_VALUE,
                    Some(5),
                ),
                (
                    "core rep 9-8",
                    1,
                    range(Range {
                        core_replies: (9, 8),
                        ..Range::default()
                    }),
                    x11::error::BAD_VALUE,
                    Some(9),
                ),
                (
                    "ext req 100-100",
                    1,
                    range(Range {
                        ext_requests_major: (100, 100),
                        ..Range::default()
                    }),
                    x11::error::BAD_VALUE,
                    Some(100),
                ),
                (
                    "ext req 200-150",
                    1,
                    range(Range {
                        ext_requests_major: (200, 150),
                        ..Range::default()
                    }),
                    x11::error::BAD_VALUE,
                    Some(200),
                ),
                (
                    "ext req minor 3-2",
                    1,
                    range(Range {
                        ext_requests_minor: (3, 2),
                        ..Range::default()
                    }),
                    x11::error::BAD_VALUE,
                    Some(3),
                ),
                (
                    "ext rep 128-127",
                    1,
                    range(Range {
                        ext_replies_major: (128, 127),
                        ..Range::default()
                    }),
                    x11::error::BAD_VALUE,
                    Some(128),
                ),
                (
                    "ext rep minor 7-1",
                    1,
                    range(Range {
                        ext_replies_minor: (7, 1),
                        ..Range::default()
                    }),
                    x11::error::BAD_VALUE,
                    Some(7),
                ),
                (
                    "delivered 2-1",
                    1,
                    range(Range {
                        delivered_events: (2, 1),
                        ..Range::default()
                    }),
                    x11::error::BAD_VALUE,
                    Some(2),
                ),
                (
                    "delivered 0-5",
                    1,
                    range(Range {
                        delivered_events: (0, 5),
                        ..Range::default()
                    }),
                    x11::error::BAD_VALUE,
                    Some(0),
                ),
                (
                    "device 1-6",
                    1,
                    range(Range {
                        device_events: (1, 6),
                        ..Range::default()
                    }),
                    x11::error::BAD_VALUE,
                    Some(1),
                ),
                (
                    "errors 9-3",
                    1,
                    range(Range {
                        errors: (9, 3),
                        ..Range::default()
                    }),
                    x11::error::BAD_VALUE,
                    Some(9),
                ),
                (
                    "clientStarted 2",
                    1,
                    range(Range {
                        client_started: 2,
                        ..Range::default()
                    }),
                    x11::error::BAD_VALUE,
                    Some(2),
                ),
                (
                    "clientDied 7",
                    1,
                    range(Range {
                        client_died: 7,
                        ..Range::default()
                    }),
                    x11::error::BAD_VALUE,
                    Some(7),
                ),
                (
                    "register bogus",
                    2,
                    reg(bogus, 0, &all, &[device_range()]),
                    BAD_CONTEXT,
                    Some(bogus),
                ),
                (
                    "unregister bogus",
                    3,
                    words(order, &[bogus, 1, 3]),
                    BAD_CONTEXT,
                    Some(bogus),
                ),
                (
                    "get length 3",
                    4,
                    words(order, &[ctx, 0]),
                    x11::error::BAD_LENGTH,
                    None,
                ),
                (
                    "query length 1",
                    0,
                    Vec::new(),
                    x11::error::BAD_LENGTH,
                    None,
                ),
                (
                    "enable length 1",
                    5,
                    Vec::new(),
                    x11::error::BAD_LENGTH,
                    None,
                ),
                (
                    "enable bogus",
                    5,
                    words(order, &[bogus]),
                    BAD_CONTEXT,
                    Some(bogus),
                ),
                (
                    "disable bogus",
                    6,
                    words(order, &[bogus]),
                    BAD_CONTEXT,
                    Some(bogus),
                ),
                (
                    "free bogus",
                    7,
                    words(order, &[bogus]),
                    BAD_CONTEXT,
                    Some(bogus),
                ),
                ("minor 9", 9, Vec::new(), x11::error::BAD_REQUEST, None),
            ];
            // After a successful create: a duplicate id and Unregister checks.
            let after: Vec<ErrorCase> = vec![
                (
                    "dup id",
                    1,
                    reg(ctx, 0, &all, &[device_range()]),
                    x11::error::BAD_ID_CHOICE,
                    Some(ctx),
                ),
                (
                    "unregister length",
                    3,
                    words(order, &[ctx, 2, 3]),
                    x11::error::BAD_LENGTH,
                    None,
                ),
                (
                    "unregister spec 4",
                    3,
                    words(order, &[ctx, 1, 4]),
                    x11::error::BAD_MATCH,
                    None,
                ),
                (
                    "unregister ctl+77",
                    3,
                    words(order, &[ctx, 1, CTL_BASE + 0x77]),
                    x11::error::BAD_VALUE,
                    Some(CTL_BASE + 0x77),
                ),
            ];
            let mut seq = 1;
            check_errors(&mut state, &mut rec, order, &mut seq, cases);
            // A spec naming an existing resource of a running client is fine.
            send(
                &mut state,
                REC,
                100,
                1,
                &reg(ctx, 0, &[CTL_BASE + 1], &[device_range()]),
            );
            send(
                &mut state,
                REC,
                101,
                x11rec::FREE_CONTEXT,
                &words(order, &[ctx]),
            );
            send(
                &mut state,
                REC,
                102,
                1,
                &reg(ctx, 0, &all, &[device_range()]),
            );
            assert!(read_all(&mut rec).is_empty(), "{order:?}: valid requests");
            let mut seq = 102;
            check_errors(&mut state, &mut rec, order, &mut seq, after);
        }
    }

    /// `record-probe basic B 7`: a big-endian recorder with every element
    /// header, a little-endian control client and a little-endian client
    /// that connects and leaves while recording.
    #[test]
    fn stream_to_byte_swapped_recorder_matches_xvfb() {
        let mut state = ServerState::new();
        let mut rec = install(&mut state, REC, REC_BASE, BigEndian);
        let ctx = REC_BASE + 1;
        create(
            &mut state,
            REC,
            3,
            ctx,
            7,
            &[x11rec::ALL_CLIENTS],
            &[device_range()],
        );
        let mut ctl = install(&mut state, CTL, CTL_BASE, LittleEndian);
        client_started(&mut state, ClientId(CTL), &[]);
        ctx_request(&mut state, REC, 6, x11rec::ENABLE_CONTEXT, ctx);
        assert!(client_is_recording(&state, ClientId(REC)));
        let got = read_all(&mut rec);
        assert_eq!(
            got,
            with_times(
                hex("01040006 00000000 07010000 00000000 00000000 00000000 00000000 00000000"),
                &got,
                &[]
            )
        );

        // ctl sees the recorder gone from the context, FutureClients moved
        // into its slot (swap-remove).
        let range = "00000000 00000000 00000000 00000000 00000206 00000101";
        ctx_request(&mut state, CTL, 3, x11rec::GET_CONTEXT, ctx);
        assert_eq!(
            read_all(&mut ctl),
            hex(&format!(
                "01010300 10000000 07000000 02000000 00000000 00000000 00000000 00000000 \
                 00004000 01000000 {range} 02000000 01000000 {range}"
            ))
        );
        ctx_request(&mut state, CTL, 4, x11rec::ENABLE_CONTEXT, ctx);
        let body = register_body(LittleEndian, ctx, 0, &[REC_BASE], &[Range::default()]);
        send(&mut state, CTL, 5, x11rec::REGISTER_CLIENTS, &body);
        let errors = packets(LittleEndian, &read_all(&mut ctl));
        assert_eq!(errors.len(), 2);
        assert_eq!(error_of(LittleEndian, &errors[0]).0, x11::error::BAD_MATCH);
        assert_eq!(error_of(LittleEndian, &errors[1]).0, x11::error::BAD_MATCH);

        // Xvfb sent these five elements in one reply (length 0x2d); each
        // element is a time header then the event:
        //   014cccda 06000000 014cccda 0000039f 00000000 00000000 00640032 00000000 00000000
        //   014cccda 02260000 014cccda 00000000 00000000 00000000 00640032 00000000 00000000
        //   … 03260000 … 04010000 … 05010000 … 00640032 00000000 01000000
        let events = [
            (
                6,
                0,
                0,
                "06000000 00000064 00000000 00000000 00000000 00640032 00000000 00000000",
            ),
            (
                2,
                38,
                0,
                "02260000 00000064 00000000 00000000 00000000 00640032 00000000 00000000",
            ),
            (
                3,
                38,
                0,
                "03260000 00000064 00000000 00000000 00000000 00640032 00000000 00000000",
            ),
            (
                4,
                1,
                0,
                "04010000 00000064 00000000 00000000 00000000 00640032 00000000 00000000",
            ),
            (
                5,
                1,
                0x100,
                "05010000 00000064 00000000 00000000 00000000 00640032 00000000 01000000",
            ),
        ];
        for (event_type, detail, key_state, bytes) in events {
            record_device_event(
                &mut state,
                RecordedDeviceEvent {
                    event_type,
                    detail,
                    repeat: false,
                    time: 0x64,
                    root_x: 100,
                    root_y: 50,
                    state: key_state,
                },
            );
            let got = read_all(&mut rec);
            let mut expected = hex(&format!(
                "01000006 00000009 07000000 00000000 00000000 00000000 00000000 00000000 \
                 00000000 {bytes}"
            ));
            if event_type == 6 {
                expected[44..48].copy_from_slice(&ROOT_WINDOW.0.to_be_bytes());
            }
            assert_eq!(got, with_times(expected, &got, &[32]), "event {event_type}");
            assert_eq!(got[16..20], got[32..36], "element time is the reply's");
        }

        // c3 connects: ClientStarted carries its setup bytes verbatim.
        let _c3 = install(&mut state, C3, C3_BASE, LittleEndian);
        let setup = hex("01000b00 00000100 a0a5b800");
        client_started(&mut state, ClientId(C3), &setup);
        let got = read_all(&mut rec);
        assert_eq!(
            got,
            with_times(
                hex(
                    "01020006 00000003 07010000 00600000 00000000 00000000 00000000 00000000 \
                     01000b00 00000100 a0a5b800"
                ),
                &got,
                &[]
            )
        );
        // …and leaves after one request: `01030006 00000001 0701…
        // 00600000 <time> 00000001 … 00000001`.
        state.clients[&C3].last_sequence.store(1, Ordering::Relaxed);
        crate::core_loop::process_disconnect::process_disconnect(
            &mut state,
            &mut RecordingBackend::new(),
            ClientId(C3),
        );
        let got = read_all(&mut rec);
        assert_eq!(
            got,
            with_times(
                hex(
                    "01030006 00000001 07010000 00600000 00000000 00000001 00000000 00000000 \
                     00000001"
                ),
                &got,
                &[]
            )
        );

        ctx_request(&mut state, CTL, 6, x11rec::DISABLE_CONTEXT, ctx);
        assert!(!client_is_recording(&state, ClientId(REC)));
        let got = read_all(&mut rec);
        assert_eq!(
            got,
            with_times(
                hex("01050006 00000000 07010000 00000000 00000000 00000000 00000000 00000000"),
                &got,
                &[]
            )
        );

        ctx_request(&mut state, REC, 7, x11rec::GET_CONTEXT, ctx);
        let be_range = "00000000 00000000 00000000 00000000 00000206 00000101";
        assert_eq!(
            read_all(&mut rec),
            hex(&format!(
                "01000007 00000010 07000000 00000002 00000000 00000000 00000000 00000000 \
                 00400000 00000001 {be_range} 00000002 00000001 {be_range}"
            ))
        );
        ctx_request(&mut state, REC, 8, x11rec::DISABLE_CONTEXT, ctx);
        ctx_request(&mut state, REC, 9, x11rec::FREE_CONTEXT, ctx);
        ctx_request(&mut state, REC, 10, x11rec::GET_CONTEXT, ctx);
        // Xvfb: 009a000a 00200001 00049200 (its BadContext 154, major 146).
        assert_eq!(
            read_all(&mut rec),
            hex("00bd000a 00200001 00049a00 00000000 00000000 00000000 00000000 00000000")
        );
    }

    /// `record-probe basic l 0`: AllClients at create holds the creator and
    /// FutureClients; a client connecting later joins that RCAP.
    #[test]
    fn future_clients_join_the_context() {
        let mut state = ServerState::new();
        let mut rec = install(&mut state, REC, REC_BASE, LittleEndian);
        let ctx = REC_BASE + 1;
        create(
            &mut state,
            REC,
            3,
            ctx,
            0,
            &[x11rec::ALL_CLIENTS],
            &[device_range()],
        );
        ctx_request(&mut state, REC, 4, x11rec::GET_CONTEXT, ctx);
        let range = "00000000 00000000 00000000 00000000 00000206 00000101";
        assert_eq!(
            read_all(&mut rec),
            hex(&format!(
                "01000400 10000000 00000000 02000000 00000000 00000000 00000000 00000000 \
                 00002000 01000000 {range} 02000000 01000000 {range}"
            ))
        );
        let _ctl = install(&mut state, CTL, CTL_BASE, LittleEndian);
        client_started(&mut state, ClientId(CTL), &[]);
        ctx_request(&mut state, REC, 5, x11rec::GET_CONTEXT, ctx);
        let reply = read_all(&mut rec);
        assert_eq!(
            get_context_clients(LittleEndian, &reply),
            [REC_BASE, x11rec::FUTURE_CLIENTS, CTL_BASE]
        );
        // Disabled: nothing is recorded, and no ClientStarted was sent.
        assert_eq!(reply.len(), 32 + 3 * 32);
    }

    /// `record-probe free l`: FreeContext of an enabled context from another
    /// client ends the stream; the recorder's queued request then runs.
    #[test]
    fn free_context_of_an_enabled_context_ends_the_stream() {
        let mut state = ServerState::new();
        let mut rec = install(&mut state, REC, REC_BASE, LittleEndian);
        let ctx = REC_BASE + 1;
        create(
            &mut state,
            REC,
            2,
            ctx,
            0,
            &[x11rec::ALL_CLIENTS],
            &[device_range()],
        );
        let _ctl = install(&mut state, CTL, CTL_BASE, LittleEndian);
        client_started(&mut state, ClientId(CTL), &[]);
        ctx_request(&mut state, REC, 3, x11rec::ENABLE_CONTEXT, ctx);
        let _start = read_all(&mut rec);
        assert!(client_is_recording(&state, ClientId(REC)));
        ctx_request(&mut state, CTL, 4, x11rec::FREE_CONTEXT, ctx);
        let got = read_all(&mut rec);
        // EndOfData seq=3: 01050300 00000000 0000…
        assert_eq!(
            got,
            with_times(
                hex("01050300 00000000 00000000 00000000 00000000 00000000 00000000 00000000"),
                &got,
                &[]
            )
        );
        assert!(!client_is_recording(&state, ClientId(REC)));
        assert!(!state.record.contains(ctx));
    }

    /// `record-probe recdie l`: the recorder leaving disables the context
    /// (no EndOfData to a gone client); ctl's context survives.
    #[test]
    fn recorder_disconnect_disables_the_context() {
        let mut state = ServerState::new();
        let _rec = install(&mut state, REC, REC_BASE, LittleEndian);
        let mut ctl = install(&mut state, CTL, CTL_BASE, LittleEndian);
        let ctx = CTL_BASE + 1;
        let died = Range {
            device_events: (2, 6),
            client_died: 1,
            ..Range::default()
        };
        create(&mut state, CTL, 2, ctx, 0, &[x11rec::ALL_CLIENTS], &[died]);
        ctx_request(&mut state, REC, 2, x11rec::ENABLE_CONTEXT, ctx);
        let enabled_clients = [x11rec::FUTURE_CLIENTS, CTL_BASE];
        ctx_request(&mut state, CTL, 5, x11rec::GET_CONTEXT, ctx);
        let reply = read_all(&mut ctl);
        assert_eq!(reply[1], 1);
        assert_eq!(get_context_clients(LittleEndian, &reply), enabled_clients);
        crate::core_loop::process_disconnect::process_disconnect(
            &mut state,
            &mut RecordingBackend::new(),
            ClientId(REC),
        );
        ctx_request(&mut state, CTL, 6, x11rec::GET_CONTEXT, ctx);
        let reply = read_all(&mut ctl);
        assert_eq!(reply[1], 0, "disabled");
        assert_eq!(get_context_clients(LittleEndian, &reply), enabled_clients);
    }

    /// `record-probe ownerdie l`: the creator leaving records its ClientDied
    /// (with the FromClientSequence header), then frees the context, which
    /// ends the stream.
    #[test]
    fn owner_disconnect_records_client_died_then_ends_the_stream() {
        let mut state = ServerState::new();
        let mut rec = install(&mut state, REC, REC_BASE, LittleEndian);
        let _ctl = install(&mut state, CTL, CTL_BASE, LittleEndian);
        let ctx = CTL_BASE + 1;
        let died = Range {
            device_events: (2, 6),
            client_died: 1,
            ..Range::default()
        };
        create(&mut state, CTL, 2, ctx, 4, &[x11rec::ALL_CLIENTS], &[died]);
        ctx_request(&mut state, REC, 2, x11rec::ENABLE_CONTEXT, ctx);
        let _start = read_all(&mut rec);
        state.clients[&CTL]
            .last_sequence
            .store(3, Ordering::Relaxed);
        crate::core_loop::process_disconnect::process_disconnect(
            &mut state,
            &mut RecordingBackend::new(),
            ClientId(CTL),
        );
        let got = packets(LittleEndian, &read_all(&mut rec));
        assert_eq!(got.len(), 2);
        // ClientDied seq=2 ehdr=4 idbase=ctl recseq=3 len=1 seqheader=3
        assert_eq!(
            got[0],
            with_times(
                hex(
                    "01030200 01000000 04000000 00004000 00000000 03000000 00000000 00000000 \
                     03000000"
                ),
                &got[0],
                &[]
            )
        );
        assert_eq!(
            got[1],
            with_times(
                hex("01050200 00000000 04000000 00000000 00000000 00000000 00000000 00000000"),
                &got[1],
                &[]
            )
        );
        assert!(!client_is_recording(&state, ClientId(REC)));
        assert!(!state.record.contains(ctx));
    }

    /// Fill `client`'s socket and outbound buffer to `OUTBOUND_CAP`, so its
    /// next write fails.
    fn saturate(state: &mut ServerState, client: u32) {
        let c = state.clients.get_mut(&client).unwrap();
        c.writer.lock().unwrap().set_nonblocking(true).unwrap();
        c.outbound
            .extend(std::iter::repeat_n(0u8, client_io::OUTBOUND_CAP));
        let _ = client_io::drain_outbound(c);
        let missing = client_io::OUTBOUND_CAP - c.outbound.len();
        c.outbound.extend(std::iter::repeat_n(0u8, missing));
    }

    /// A data connection that cannot take its stream (here: over
    /// `OUTBOUND_CAP`) stops recording and is queued for disconnect, also
    /// when the failing write happens inside another client's disconnect;
    /// the context's owner is unaffected.
    #[test]
    fn overflowing_recorder_is_queued_for_disconnect() {
        let mut state = ServerState::new();
        let _rec = install(&mut state, REC, REC_BASE, LittleEndian);
        let mut ctl = install(&mut state, CTL, CTL_BASE, LittleEndian);
        let _c3 = install(&mut state, C3, C3_BASE, LittleEndian);
        let ctx = CTL_BASE + 1;
        create(
            &mut state,
            CTL,
            2,
            ctx,
            0,
            &[x11rec::ALL_CLIENTS],
            &[device_range()],
        );
        ctx_request(&mut state, REC, 2, x11rec::ENABLE_CONTEXT, ctx);
        assert!(client_is_recording(&state, ClientId(REC)));
        saturate(&mut state, REC);

        // ClientDied written during c3's disconnect: queued, not inline.
        crate::core_loop::process_disconnect::process_disconnect(
            &mut state,
            &mut RecordingBackend::new(),
            ClientId(C3),
        );
        assert!(state.clients.contains_key(&REC));
        assert!(!client_is_recording(&state, ClientId(REC)));
        // Its pipelined requests stay queued until the disconnect.
        assert!(client_blocks_requests(&state, ClientId(REC)));
        // Later events do not touch it again.
        record_device_event(
            &mut state,
            RecordedDeviceEvent {
                event_type: 2,
                detail: 38,
                repeat: false,
                time: 0,
                root_x: 0,
                root_y: 0,
                state: 0,
            },
        );
        assert_eq!(take_failed_recorders(&mut state), [ClientId(REC)]);
        assert!(take_failed_recorders(&mut state).is_empty());

        // What the core loop then does with it.
        crate::core_loop::process_disconnect::process_disconnect(
            &mut state,
            &mut RecordingBackend::new(),
            ClientId(REC),
        );
        assert!(!state.clients.contains_key(&REC));
        assert!(!client_blocks_requests(&state, ClientId(REC)));
        ctx_request(&mut state, CTL, 3, x11rec::GET_CONTEXT, ctx);
        let reply = read_all(&mut ctl);
        assert_eq!(reply[1], 0, "disabled");
        assert_eq!(
            get_context_clients(LittleEndian, &reply),
            [x11rec::FUTURE_CLIENTS, CTL_BASE]
        );
    }

    /// The pointer fan-out records motion and buttons once, from the
    /// physical event: not crossings, not an AllowEvents replay, not a
    /// frozen-queue replay (Xorg skips the callback while playingEvents).
    #[test]
    fn pointer_fanout_records_physical_events_only() {
        use crate::{
            core_loop::pointer_fanout::pointer_event_fanout_to_state,
            host_x11::{HostPointerEvent, HostXidMap, PointerEventKind},
        };
        let mut state = ServerState::new();
        let mut rec = install(&mut state, REC, REC_BASE, LittleEndian);
        let ctx = REC_BASE + 1;
        create(
            &mut state,
            REC,
            2,
            ctx,
            0,
            &[x11rec::FUTURE_CLIENTS],
            &[device_range()],
        );
        ctx_request(&mut state, REC, 3, x11rec::ENABLE_CONTEXT, ctx);
        let _start = read_all(&mut rec);
        let ev = |kind, detail, state| HostPointerEvent {
            kind,
            host_xid: 0,
            detail,
            time: 0x0138_482a,
            root_x: 100,
            root_y: 50,
            event_x: 100,
            event_y: 50,
            state,
            crossing_mode: 0,
            child: 0,
            raw_dx: 0,
            raw_dy: 0,
            tree_change: false,
        };
        let xid_map = HostXidMap::default();
        let mut backend = RecordingBackend::new();
        for event in [
            ev(PointerEventKind::MotionNotify, 0, 0),
            ev(PointerEventKind::EnterNotify, 0, 0),
            ev(PointerEventKind::ButtonPress, 1, 0),
        ] {
            let _ = pointer_event_fanout_to_state(
                &mut state,
                &mut backend,
                &xid_map,
                event,
                true,
                false,
            );
        }
        let _ = pointer_event_fanout_to_state(
            &mut state,
            &mut backend,
            &xid_map,
            ev(PointerEventKind::ButtonPress, 1, 0),
            false,
            true,
        );
        state.playing_sync_events = true;
        let _ = pointer_event_fanout_to_state(
            &mut state,
            &mut backend,
            &xid_map,
            ev(PointerEventKind::ButtonRelease, 1, 0x100),
            true,
            false,
        );
        state.playing_sync_events = false;
        let _ = pointer_event_fanout_to_state(
            &mut state,
            &mut backend,
            &xid_map,
            ev(PointerEventKind::ButtonRelease, 1, 0x100),
            true,
            false,
        );
        let got = packets(LittleEndian, &read_all(&mut rec));
        let elements: Vec<Vec<u8>> = got.iter().map(|p| p[32..].to_vec()).collect();
        // `record-probe basic l 0` (the event time is that run's):
        //   06000000 2a483801 9f030000 00000000 00000000 64003200 00000000 00000000
        //   04010000 2a483801 00000000 00000000 00000000 64003200 00000000 00000000
        //   05010000 2a483801 00000000 00000000 00000000 64003200 00000000 00010000
        let mut motion =
            hex("06000000 2a483801 9f030000 00000000 00000000 64003200 00000000 00000000");
        motion[8..12].copy_from_slice(&ROOT_WINDOW.0.to_le_bytes());
        assert_eq!(
            elements,
            [
                motion,
                hex("04010000 2a483801 00000000 00000000 00000000 64003200 00000000 00000000"),
                hex("05010000 2a483801 00000000 00000000 00000000 64003200 00000000 00010000"),
            ]
        );
    }
}
