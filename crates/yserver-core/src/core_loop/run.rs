//! Skeleton of the single-threaded core loop.
//!
//! B4 established the shape; D4 wired `Message::Request` against the
//! new `process_request` entry point and the lifecycle arms
//! (SetupAllocate, ClientSetupComplete, ClientDisconnected, HostInput).
//! E3/E4 (DRM + signalfd) and F2
//! (host-X11) supply the missing token arms; D5 supplies the
//! listener.

use std::{
    collections::{HashMap, VecDeque},
    io,
    os::{
        fd::{AsRawFd, OwnedFd, RawFd},
        unix::net::{UnixListener, UnixStream},
    },
    sync::Arc,
    time::{Duration, Instant},
};

use log::{error, warn};
use mio::{Events, Interest, Poll, unix::SourceFd};

use super::{
    auth::AuthState,
    client_io::{self, WriteOutcome},
    message::{HostInputEvent, Message, SetupAllocateResponse},
    poll_tokens::{
        ClientIdAllocator, LISTENER_TOKEN, NOTIFY_TOKEN, backend_token, client_token,
        token_to_backend_index, token_to_client,
    },
    process_request::{
        PendingCrtcConfig, RequestOutcome, complete_crtc_config,
        fire_present_configure_notify_for_window, process_request,
    },
    sender::{CoreReceiver, CoreSender},
    setup_thread::{self, SetupRegistry},
};
use crate::{
    backend::{Backend, BackendFdKind, CrtcConfigToken, HostSocketStatus},
    host_x11::HostEvent,
    server::{KeyRepeatState, ServerState},
};

/// Diagnostic: per-second loop telemetry emit interval. Toggle via
/// `YSERVER_LOOP_TELEMETRY=1` env var (off by default to avoid log
/// spam in normal runs). When on, every ~1s we emit a single
/// `info!` line with:
///   - iterations/sec
///   - requests/sec + max-drain-per-iter
///   - top-3 opcodes by count + total time
///   - host_input + page_flip dispatches/sec
///   - max time between subsequent HostInput dispatches (cursor-lag proxy)
///   - max single-iteration wall time
///   - total/per-client deferred depth and request age
///   - largest shared-channel request drain, including its dominant client
///   - accepted sequence 0xffff/0x0000 boundary counts
///
/// Costs are deliberately accepted only when explicitly enabled: request
/// timestamps cross the reader boundary, and the core maintains small
/// per-client maps in addition to the existing per-opcode counters.
const TELEMETRY_EMIT_INTERVAL: Duration = Duration::from_secs(1);

/// Number of opcodes to show in the per-second telemetry emit.
const TELEMETRY_TOP_N: usize = 3;

#[derive(Debug, Default)]
struct ClientLoopTelemetry {
    deferred_current: usize,
    deferred_max: usize,
    accepted: u64,
    dispatched: u64,
    request_age_max: Duration,
    sequence_ffff: u64,
    sequence_zero: u64,
    requests_by_opcode: HashMap<(u8, Option<u8>), u64>,
}

#[derive(Debug, Default)]
struct LoopTelemetry {
    enabled: bool,
    last_emit: Option<Instant>,
    iter_count: u64,
    requests_total: u64,
    requests_per_iter_max: u32,
    requests_by_opcode: HashMap<u8, (u64, Duration)>,
    request_total_time: Duration,
    longest_request: (u8, Duration),
    host_input_count: u64,
    host_input_max_gap: Duration,
    last_host_input: Option<Instant>,
    page_flip_count: u64,
    max_iter_wall: Duration,
    /// Peak depth of `deferred_requests` observed this window.
    ///
    /// Distinguishes two failure modes that look identical from the
    /// outside during a request flood. A shallow backlog (tens) means
    /// the drain keeps up and any residual stutter is
    /// request-vs-input scheduling — what `REQUEST_TIME_BUDGET`
    /// addresses. A deep, growing backlog (thousands) means requests
    /// arrive faster than they drain, so a low-rate client's request
    /// (marco's `ConfigureWindow`, which is what actually moves a
    /// dragged window) waits behind a high-rate client's flood — a
    /// per-client fairness problem the time budget does NOT fix.
    /// Added because that distinction had been argued repeatedly
    /// without ever being measured.
    deferred_current: usize,
    max_deferred_depth: usize,
    clients: HashMap<yserver_protocol::x11::ClientId, ClientLoopTelemetry>,
    channel_request_batch_max: usize,
    channel_client_batch_max: (u32, usize),
}

impl LoopTelemetry {
    fn new() -> Self {
        let enabled = std::env::var_os("YSERVER_LOOP_TELEMETRY").is_some();
        Self {
            enabled,
            last_emit: None,
            ..Default::default()
        }
    }

    fn record_request(
        &mut self,
        client: yserver_protocol::x11::ClientId,
        opcode: u8,
        data: u8,
        dur: Duration,
        age: Duration,
    ) {
        if !self.enabled {
            return;
        }
        self.requests_total += 1;
        self.request_total_time += dur;
        let entry = self.requests_by_opcode.entry(opcode).or_default();
        entry.0 += 1;
        entry.1 += dur;
        if dur > self.longest_request.1 {
            self.longest_request = (opcode, dur);
        }
        let client_stats = self.clients.entry(client).or_default();
        client_stats.dispatched += 1;
        client_stats.request_age_max = client_stats.request_age_max.max(age);
        let request_key = (opcode, (opcode >= 128).then_some(data));
        *client_stats
            .requests_by_opcode
            .entry(request_key)
            .or_default() += 1;
    }

    fn record_request_accepted(
        &mut self,
        client: yserver_protocol::x11::ClientId,
        sequence: yserver_protocol::x11::SequenceNumber,
    ) {
        if !self.enabled {
            return;
        }
        let client_stats = self.clients.entry(client).or_default();
        client_stats.accepted += 1;
        match sequence.0 {
            0xffff => client_stats.sequence_ffff += 1,
            0 => client_stats.sequence_zero += 1,
            _ => {}
        }
    }

    fn record_deferred_push(&mut self, client: yserver_protocol::x11::ClientId) {
        if !self.enabled {
            return;
        }
        self.deferred_current += 1;
        self.max_deferred_depth = self.max_deferred_depth.max(self.deferred_current);
        let client_stats = self.clients.entry(client).or_default();
        client_stats.deferred_current += 1;
        client_stats.deferred_max = client_stats.deferred_max.max(client_stats.deferred_current);
    }

    fn record_deferred_pop(&mut self, client: yserver_protocol::x11::ClientId) {
        if !self.enabled {
            return;
        }
        self.deferred_current = self.deferred_current.saturating_sub(1);
        let client_stats = self.clients.entry(client).or_default();
        client_stats.deferred_current = client_stats.deferred_current.saturating_sub(1);
    }

    fn record_channel_drain(
        &mut self,
        requests: usize,
        requests_by_client: &HashMap<yserver_protocol::x11::ClientId, usize>,
    ) {
        if !self.enabled {
            return;
        }
        self.channel_request_batch_max = self.channel_request_batch_max.max(requests);
        if let Some((&client, &count)) = requests_by_client.iter().max_by_key(|(_, count)| *count)
            && count > self.channel_client_batch_max.1
        {
            self.channel_client_batch_max = (client.0, count);
        }
    }

    fn record_host_input(&mut self, now: Instant) {
        if !self.enabled {
            return;
        }
        self.host_input_count += 1;
        if let Some(prev) = self.last_host_input {
            let gap = now.saturating_duration_since(prev);
            if gap > self.host_input_max_gap {
                self.host_input_max_gap = gap;
            }
        }
        self.last_host_input = Some(now);
    }

    fn record_iteration(&mut self, requests_this_iter: u32, iter_wall: Duration) {
        if !self.enabled {
            return;
        }
        self.iter_count += 1;
        if requests_this_iter > self.requests_per_iter_max {
            self.requests_per_iter_max = requests_this_iter;
        }
        if iter_wall > self.max_iter_wall {
            self.max_iter_wall = iter_wall;
        }
    }

    fn maybe_emit(&mut self, now: Instant) {
        if !self.enabled {
            return;
        }
        let last = match self.last_emit {
            Some(t) => t,
            None => {
                self.last_emit = Some(now);
                return;
            }
        };
        let elapsed = now.saturating_duration_since(last);
        if elapsed < TELEMETRY_EMIT_INTERVAL {
            return;
        }
        let secs = elapsed.as_secs_f64().max(1e-6);

        // Top-N opcodes by total time (the most-actionable view; opcodes
        // that fire often but cheap-each don't dominate, opcodes that
        // fire rarely but expensive-each do).
        let mut by_time: Vec<(u8, u64, Duration)> = self
            .requests_by_opcode
            .iter()
            .map(|(op, (cnt, t))| (*op, *cnt, *t))
            .collect();
        by_time.sort_by_key(|(_, _, total)| std::cmp::Reverse(*total));
        let top_time: Vec<String> = by_time
            .iter()
            .take(TELEMETRY_TOP_N)
            .map(|(op, cnt, t)| format!("op{op}:n={cnt}/t={:.1}ms", t.as_secs_f64() * 1000.0))
            .collect();

        let mut by_count = by_time.clone();
        by_count.sort_by_key(|(_, count, _)| std::cmp::Reverse(*count));
        let top_count: Vec<String> = by_count
            .iter()
            .take(TELEMETRY_TOP_N)
            .map(|(op, cnt, t)| format!("op{op}:n={cnt}/t={:.1}ms", t.as_secs_f64() * 1000.0))
            .collect();

        let mut deferred_clients: Vec<_> = self.clients.iter().collect();
        deferred_clients.sort_by_key(|(_, stats)| std::cmp::Reverse(stats.deferred_max));
        let top_deferred: Vec<String> = deferred_clients
            .iter()
            .take(TELEMETRY_TOP_N)
            .map(|(id, stats)| {
                format!(
                    "c{}:cur={}/max={}",
                    id.0, stats.deferred_current, stats.deferred_max
                )
            })
            .collect();

        let mut age_clients: Vec<_> = self.clients.iter().collect();
        age_clients.sort_by_key(|(_, stats)| std::cmp::Reverse(stats.request_age_max));
        let top_age: Vec<String> = age_clients
            .iter()
            .take(TELEMETRY_TOP_N)
            .map(|(id, stats)| {
                format!(
                    "c{}:n={}/max={:.1}ms",
                    id.0,
                    stats.dispatched,
                    stats.request_age_max.as_secs_f64() * 1000.0
                )
            })
            .collect();
        let sequence_ffff: u64 = self.clients.values().map(|stats| stats.sequence_ffff).sum();
        let sequence_zero: u64 = self.clients.values().map(|stats| stats.sequence_zero).sum();

        let mut request_clients: Vec<_> = self.clients.iter().collect();
        request_clients
            .sort_by_key(|(_, stats)| std::cmp::Reverse(stats.accepted.max(stats.dispatched)));
        let request_client_mix: Vec<String> = request_clients
            .iter()
            .filter(|(_, stats)| stats.accepted != 0 || stats.dispatched != 0)
            .take(TELEMETRY_TOP_N)
            .map(|(id, stats)| {
                let mut operations: Vec<_> = stats.requests_by_opcode.iter().collect();
                operations.sort_by_key(|(_, count)| std::cmp::Reverse(**count));
                let top: Vec<String> = operations
                    .iter()
                    .take(5)
                    .map(|((major, minor), count)| match minor {
                        Some(minor) => format!("{major}.{minor}={count}"),
                        None => format!("{major}={count}"),
                    })
                    .collect();
                format!(
                    "c{}:accepted={}/dispatched={}/top={}",
                    id.0,
                    stats.accepted,
                    stats.dispatched,
                    top.join("|")
                )
            })
            .collect();

        let outbound = crate::core_loop::fanout::take_outbound_telemetry();
        let mut outbound_by_client: HashMap<_, Vec<_>> = HashMap::new();
        for ((client, kind), count) in outbound {
            outbound_by_client
                .entry(client)
                .or_default()
                .push((kind, count));
        }
        let mut outbound_clients: Vec<_> = outbound_by_client.into_iter().collect();
        outbound_clients.sort_by_key(|(_, kinds)| {
            std::cmp::Reverse(kinds.iter().map(|(_, count)| count).sum::<u64>())
        });
        let outbound_client_mix: Vec<String> = outbound_clients
            .iter_mut()
            .take(TELEMETRY_TOP_N)
            .map(|(id, kinds)| {
                kinds.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
                let total: u64 = kinds.iter().map(|(_, count)| count).sum();
                let top: Vec<String> = kinds
                    .iter()
                    .take(5)
                    .map(|(kind, count)| {
                        use crate::core_loop::fanout::OutboundTelemetryKind;
                        let label = match kind {
                            OutboundTelemetryKind::Reply => "reply".to_string(),
                            OutboundTelemetryKind::Error(code) => format!("err{code}"),
                            OutboundTelemetryKind::Event(event) => format!("e{event}"),
                            OutboundTelemetryKind::GenericEvent {
                                extension,
                                event_type,
                            } => format!("ge{extension}.{event_type}"),
                        };
                        format!("{label}={count}")
                    })
                    .collect();
                format!("c{}:n={total}/top={}", id.0, top.join("|"))
            })
            .collect();

        log::info!(
            "loop telemetry [{:.2}s]: iter/s={:.0} req/s={:.0} drain_max={} \
             req_time={:.1}ms ({:.1}%) longest=op{}:{:.2}ms \
             host_input/s={:.1} gap_max={:.1}ms \
             page_flip/s={:.1} iter_wall_max={:.1}ms deferred={}/{} \
             channel_batch_max={} channel_client_max=c{}:{} seq_boundary[ffff={} zero={}] \
             deferred_clients=[{}] age_clients=[{}] \
             request_clients=[{}] outbound_clients=[{}] \
             top_by_time=[{}] top_by_count=[{}]",
            secs,
            self.iter_count as f64 / secs,
            self.requests_total as f64 / secs,
            self.requests_per_iter_max,
            self.request_total_time.as_secs_f64() * 1000.0,
            self.request_total_time.as_secs_f64() / secs * 100.0,
            self.longest_request.0,
            self.longest_request.1.as_secs_f64() * 1000.0,
            self.host_input_count as f64 / secs,
            self.host_input_max_gap.as_secs_f64() * 1000.0,
            self.page_flip_count as f64 / secs,
            self.max_iter_wall.as_secs_f64() * 1000.0,
            self.deferred_current,
            self.max_deferred_depth,
            self.channel_request_batch_max,
            self.channel_client_batch_max.0,
            self.channel_client_batch_max.1,
            sequence_ffff,
            sequence_zero,
            top_deferred.join(","),
            top_age.join(","),
            request_client_mix.join(","),
            outbound_client_mix.join(","),
            top_time.join(","),
            top_count.join(","),
        );

        // Reset accumulators for next window. Keep `enabled` /
        // `last_host_input` (cross-window gap measurement) /
        // `last_emit`. Everything else zeroes.
        self.last_emit = Some(now);
        self.iter_count = 0;
        self.requests_total = 0;
        self.requests_per_iter_max = 0;
        self.requests_by_opcode.clear();
        self.request_total_time = Duration::ZERO;
        self.longest_request = (0, Duration::ZERO);
        self.host_input_count = 0;
        self.host_input_max_gap = Duration::ZERO;
        self.page_flip_count = 0;
        self.max_iter_wall = Duration::ZERO;
        self.max_deferred_depth = self.deferred_current;
        self.channel_request_batch_max = 0;
        self.channel_client_batch_max = (0, 0);
        self.clients.retain(|_, stats| {
            stats.deferred_max = stats.deferred_current;
            stats.accepted = 0;
            stats.dispatched = 0;
            stats.request_age_max = Duration::ZERO;
            stats.sequence_ffff = 0;
            stats.sequence_zero = 0;
            stats.requests_by_opcode.clear();
            stats.deferred_current != 0
        });
    }
}

/// Core-loop work cap. Each main-loop iteration processes at
/// most this many X protocol requests before yielding back to the
/// outer poll / maintenance pass. Excess requests are buffered in
/// `deferred_requests` and picked up at the start of the next
/// iteration.
///
/// **Why this matters** (per the telemetry rollups from the bee /
/// adapta-nokto investigation): without a cap, `Message::Request`
/// can monopolise the thread for SECONDS at a time on a single
/// iteration when GTK fires bursts of RENDER traffic during a
/// window drag — observed iter_wall_max=6884ms with
/// drain_max=32857 in one iteration. During that window,
/// `HostInput` messages and DRM readiness sit undelivered, so the cursor
/// visibly freezes (gap_max
/// up to 8.5 seconds between consecutive cursor events).
///
/// 32 chosen as the initial cap because: typical request cost is
/// ~0.25 ms, so 32 × 0.25 ≈ 8 ms per iteration worst case — about
/// one frame at 120 Hz, well below the perceptual cursor-lag
/// threshold.
///
/// The count cap alone is NOT sufficient: it presumes the ~0.25 ms
/// figure above, and that presumption was measured false. See
/// [`REQUEST_TIME_BUDGET`], which now bounds the same iteration by
/// wall clock. The count cap is retained because for well-behaved
/// requests it binds first (32 × 0.25 ms == the 8 ms budget by
/// construction), so the fast path is unchanged.
const MAX_REQUESTS_PER_ITER: usize = 32;

/// Wall-clock ceiling on request processing per main-loop iteration,
/// enforced alongside [`MAX_REQUESTS_PER_ITER`] — whichever trips
/// first ends the drain.
///
/// **Why the count cap was not enough** (measured on silence, dual
/// 1440p, MATE + adapta-nokto, dragging the mate-control-center
/// window — `YSERVER_LOOP_TELEMETRY=1`): GTK emits ~200,000 requests
/// per second during that drag (each themed fill costs CreatePixmap +
/// CreatePicture + FillRectangles + FreePicture + FreePixmap), and
/// individual requests reach **44-50 ms** (`longest=op70:44.23ms`,
/// `op70:49.61ms`) because a request that closes the open frame
/// absorbs the whole batch flush. 32 × 44 ms is ~1.4 s inside one
/// iteration, while `HostInput` and DRM readiness still need service —
/// so the cursor and the window position stall together
/// (`gap_max` 225-360 ms between consecutive input events, against
/// `host_input/s` ≈ 128 arriving fine). The visible symptom is a drag
/// that tracks, lags, then skips.
///
/// A deadline cannot preempt a request already running, so this does
/// not make a 44 ms request cheaper — it stops that request from
/// authorising 31 more. Worst-case iteration becomes one overrunning
/// request instead of 32.
///
/// 8 ms is the figure `MAX_REQUESTS_PER_ITER` was already aiming at
/// (one frame at 120 Hz), so this restores the intended design point
/// rather than picking a new one.
const REQUEST_TIME_BUDGET: Duration = Duration::from_millis(8);

/// One backend-owned source registered with the core poller. The vector index
/// is encoded in its mio token, preserving the exact fd identity even when
/// several entries share one `BackendFdKind`.
#[derive(Debug, Clone, Copy)]
struct BackendPollSource {
    fd: RawFd,
    kind: BackendFdKind,
}

/// Whether this iteration's request drain must stop, given how many
/// requests remain in the count budget and how long the drain has been
/// running. Split out as a pure function so the count-vs-deadline
/// interaction is unit-testable without driving the whole core loop.
///
/// `elapsed` is measured from the top of the iteration, so the first
/// request always passes (`elapsed` ≈ 0) — that guarantees forward
/// progress even when every request overruns the budget.
fn budget_exhausted(remaining: usize, elapsed: Duration) -> bool {
    remaining == 0 || elapsed >= REQUEST_TIME_BUDGET
}

/// One pending X protocol request accepted by a reader but not yet dispatched.
struct DeferredRequest {
    id: yserver_protocol::x11::ClientId,
    sequence: yserver_protocol::x11::SequenceNumber,
    accepted_at: Option<Instant>,
    header: yserver_protocol::x11::RequestHeader,
    body: Vec<u8>,
    attached_fd: Option<OwnedFd>,
}

/// A request whose backend work is running asynchronously. The raw request is
/// deliberately not retained: validation and begin-side effects run exactly
/// once, and completion resumes only the protocol reply/notification tail.
struct ParkedCrtcConfig {
    client_id: yserver_protocol::x11::ClientId,
    sequence: yserver_protocol::x11::SequenceNumber,
    continuation: PendingCrtcConfig,
    request_wire_bytes: usize,
}

/// Backend waits indexed both by opaque token (completion) and by client
/// (strict same-client FIFO blocking/cancellation).
#[derive(Default)]
struct PendingBackendRequests {
    crtc_by_token: HashMap<CrtcConfigToken, ParkedCrtcConfig>,
    crtc_by_client: HashMap<yserver_protocol::x11::ClientId, CrtcConfigToken>,
}

impl PendingBackendRequests {
    fn client_is_blocked(&self, client: yserver_protocol::x11::ClientId) -> bool {
        self.crtc_by_client.contains_key(&client)
    }

    fn park_crtc(&mut self, parked: ParkedCrtcConfig) -> Result<(), &'static str> {
        let client = parked.client_id;
        let token = parked.continuation.token;
        if self.crtc_by_client.contains_key(&client) {
            return Err("client already has a pending backend request");
        }
        if self.crtc_by_token.contains_key(&token) {
            return Err("backend reused a live CRTC configuration token");
        }
        self.crtc_by_client.insert(client, token);
        self.crtc_by_token.insert(token, parked);
        Ok(())
    }

    fn take_crtc(&mut self, token: CrtcConfigToken) -> Option<ParkedCrtcConfig> {
        let parked = self.crtc_by_token.remove(&token)?;
        self.crtc_by_client.remove(&parked.client_id);
        Some(parked)
    }

    fn take_client_crtc(
        &mut self,
        client: yserver_protocol::x11::ClientId,
    ) -> Option<CrtcConfigToken> {
        let token = self.crtc_by_client.remove(&client)?;
        self.crtc_by_token.remove(&token);
        Some(token)
    }

    fn take_all_crtc_tokens(&mut self) -> Vec<CrtcConfigToken> {
        self.crtc_by_client.clear();
        self.crtc_by_token.drain().map(|(token, _)| token).collect()
    }
}

/// Per-client FIFO queues behind a round-robin ready ring.
///
/// Request order is preserved within each client, as required by X11, while a
/// continuously busy client gets at most one request before every other ready
/// client gets a turn. Cross-client request order has no protocol meaning.
#[derive(Default)]
struct FairRequestQueue {
    by_client: HashMap<yserver_protocol::x11::ClientId, VecDeque<DeferredRequest>>,
    ready: VecDeque<yserver_protocol::x11::ClientId>,
    len: usize,
}

impl FairRequestQueue {
    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn push_back(&mut self, req: DeferredRequest) {
        let client = req.id;
        let queue = self.by_client.entry(client).or_default();
        if queue.is_empty() {
            self.ready.push_back(client);
        }
        queue.push_back(req);
        self.len += 1;
    }

    /// Restore an older, temporarily parked prefix ahead of this client's
    /// remaining requests without changing the client's place in the ready
    /// ring. `requests` must contain exactly one client's requests in arrival
    /// order.
    fn prepend_client(&mut self, mut requests: VecDeque<DeferredRequest>) {
        let Some(first) = requests.front() else {
            return;
        };
        let client = first.id;
        debug_assert!(requests.iter().all(|req| req.id == client));
        let added = requests.len();

        if let Some(existing) = self.by_client.get_mut(&client) {
            requests.append(existing);
            *existing = requests;
        } else {
            self.ready.push_back(client);
            self.by_client.insert(client, requests);
        }
        self.len = self.len.saturating_add(added);
    }

    #[cfg(test)]
    fn pop_front(&mut self) -> Option<DeferredRequest> {
        self.pop_front_if(|_| true)
    }

    fn has_runnable(&self, pending: &PendingBackendRequests) -> bool {
        self.ready
            .iter()
            .any(|client| !pending.client_is_blocked(*client))
    }

    fn pop_front_unblocked(&mut self, pending: &PendingBackendRequests) -> Option<DeferredRequest> {
        self.pop_front_if(|client| !pending.client_is_blocked(client))
    }

    fn pop_front_if(
        &mut self,
        mut is_runnable: impl FnMut(yserver_protocol::x11::ClientId) -> bool,
    ) -> Option<DeferredRequest> {
        // Inspect each currently-ready client at most once. Blocked clients
        // retain their position in the ring while other clients keep moving.
        let candidates = self.ready.len();
        for _ in 0..candidates {
            let Some(client) = self.ready.pop_front() else {
                break;
            };
            if !is_runnable(client) {
                self.ready.push_back(client);
                continue;
            }
            let (request, remains_ready) = {
                let Some(queue) = self.by_client.get_mut(&client) else {
                    continue;
                };
                (queue.pop_front(), !queue.is_empty())
            };
            let Some(request) = request else {
                self.by_client.remove(&client);
                continue;
            };
            self.len = self.len.saturating_sub(1);
            if remains_ready {
                self.ready.push_back(client);
            } else {
                self.by_client.remove(&client);
            }
            return Some(request);
        }
        None
    }
}

fn blocked_by_server_grab(state: &ServerState, req: &DeferredRequest) -> bool {
    state.server_grab_owner.is_some_and(|owner| owner != req.id)
}

/// Restore parked server-grab requests to the fair queue without changing
/// their per-client arrival order.
fn release_server_grab_waiters(
    deferred_requests: &mut FairRequestQueue,
    server_grab_waiters: &mut VecDeque<DeferredRequest>,
    telemetry: &mut LoopTelemetry,
) {
    // A waiter is an older prefix temporarily removed from one client's fair
    // queue while another client owned GrabServer. Restore each prefix ahead
    // of that client's requests which remained queued. Appending here breaks
    // X11's strict per-client order (observed as #59264 dispatched before
    // #59216), causing Xlib/XCB to abort with threads_sequence_lost.
    let mut client_order = Vec::new();
    let mut by_client: HashMap<_, VecDeque<_>> = HashMap::new();
    while let Some(req) = server_grab_waiters.pop_front() {
        telemetry.record_deferred_push(req.id);
        let client = req.id;
        match by_client.entry(client) {
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                entry.get_mut().push_back(req);
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                client_order.push(client);
                entry.insert(VecDeque::from([req]));
            }
        }
    }
    for client in client_order {
        deferred_requests.prepend_client(
            by_client
                .remove(&client)
                .expect("server-grab waiter client recorded"),
        );
    }
}

fn grant_request_credit(
    state: &ServerState,
    client: yserver_protocol::x11::ClientId,
    bytes: usize,
) {
    if let Some(control) = state
        .clients
        .get(&client.0)
        .and_then(|client| client.reader_control.as_ref())
    {
        let _ = control.send(crate::server::ReaderControl::GrantRequestBytes(bytes));
    }
}

fn disconnect_with_pending_cleanup(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    pending: &mut PendingBackendRequests,
    client: yserver_protocol::x11::ClientId,
) {
    if let Some(token) = pending.take_client_crtc(client) {
        backend.cancel_crtc_config(token);
    }
    crate::core_loop::process_disconnect::process_disconnect(state, backend, client);
}

fn cancel_all_pending_backend_requests(
    backend: &mut dyn Backend,
    pending: &mut PendingBackendRequests,
) {
    for token in pending.take_all_crtc_tokens() {
        backend.cancel_crtc_config(token);
    }
}

fn process_one_request(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    telemetry: &mut LoopTelemetry,
    pending: &mut PendingBackendRequests,
    requests_this_iter: &mut u32,
    request_budget: &mut usize,
    req: DeferredRequest,
) {
    let req_opcode = req.header.opcode;
    let req_data = req.header.data;
    let req_wire_bytes = usize::try_from(req.header.length_units)
        .unwrap_or(usize::MAX)
        .saturating_mul(4)
        .max(4);
    let req_client = req.id;
    let req_age = req.accepted_at.map(|accepted_at| accepted_at.elapsed());
    let req_start = if telemetry.enabled {
        Some(Instant::now())
    } else {
        None
    };
    let outcome = process_request_inline(
        state,
        backend,
        req.id,
        req.sequence,
        req.header,
        &req.body,
        req.attached_fd,
    );
    if let Some(start) = req_start {
        telemetry.record_request(
            req_client,
            req_opcode,
            req_data,
            start.elapsed(),
            req_age.unwrap_or_default(),
        );
    }
    *requests_this_iter += 1;
    *request_budget -= 1;
    match outcome {
        RequestOutcome::Handled => grant_request_credit(state, req_client, req_wire_bytes),
        RequestOutcome::Disconnect(disc_id) => {
            disconnect_with_pending_cleanup(state, backend, pending, disc_id);
        }
        RequestOutcome::PendingCrtcConfig(continuation) => {
            let token = continuation.token;
            let parked = ParkedCrtcConfig {
                client_id: req_client,
                sequence: req.sequence,
                continuation,
                request_wire_bytes: req_wire_bytes,
            };
            if let Err(reason) = pending.park_crtc(parked) {
                log::error!(
                    "cannot park asynchronous RRSetCrtcConfig for client {} token {}: {reason}",
                    req_client.0,
                    token.0,
                );
                // If this token is not already owned by another waiter, it is
                // the just-started operation and can be cancelled safely.
                if !pending.crtc_by_token.contains_key(&token) {
                    backend.cancel_crtc_config(token);
                }
                // An ordering/token contract violation cannot be replied to
                // safely without overtaking an earlier request from this
                // client. Disconnect it and cancel any older parked work.
                disconnect_with_pending_cleanup(state, backend, pending, req_client);
            }
        }
    }
}

fn drain_pending_requests(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    telemetry: &mut LoopTelemetry,
    pending: &mut PendingBackendRequests,
    deferred_requests: &mut FairRequestQueue,
    server_grab_waiters: &mut VecDeque<DeferredRequest>,
    requests_this_iter: &mut u32,
    request_budget: &mut usize,
    drain_start: Instant,
) {
    while !budget_exhausted(*request_budget, drain_start.elapsed()) {
        let Some(req) = deferred_requests.pop_front_unblocked(pending) else {
            break;
        };
        telemetry.record_deferred_pop(req.id);
        if blocked_by_server_grab(state, &req) {
            server_grab_waiters.push_back(req);
            continue;
        }
        process_one_request(
            state,
            backend,
            telemetry,
            pending,
            requests_this_iter,
            request_budget,
            req,
        );
        if state.server_grab_owner.is_none() {
            release_server_grab_waiters(deferred_requests, server_grab_waiters, telemetry);
        }
    }
}

/// Process one X protocol request and run its post-handler bookkeeping
/// (mark_dirty + disconnect-on-error). Factored so the two drain paths
/// in `run_core` (the deferred queue at the top of each iteration and
/// the channel drain inside `NOTIFY_TOKEN`) share identical semantics.
///
fn process_request_inline(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    id: yserver_protocol::x11::ClientId,
    sequence: yserver_protocol::x11::SequenceNumber,
    header: yserver_protocol::x11::RequestHeader,
    body: &[u8],
    attached_fd: Option<OwnedFd>,
) -> RequestOutcome {
    // Half-closed-socket / post-disconnect guard. The `Message::Request`
    // The reader/channel/fair-queue path preserves per-client arrival order.
    // When a client crashes (e.g.
    // mate-appearance-properties cratering with the keyring locked) and
    // the client_reader thread enqueues a burst of bogus requests
    // before/around the EOF, the main thread can still be draining those
    // queued Requests *after* `process_disconnect` removed the client
    // from `state.clients`. Several handlers (CreatePixmap, CreateGC,
    // CreateWindow, etc. — eight sites at process_request.rs) read
    // `state.clients.get(client_id).expect("client registered")` to
    // validate the request's resource XID against the client's
    // allocation range, and panic the whole server when the lookup misses.
    //
    // Without this guard we observed a session crash on 2026-05-26 in
    // the adapta-nokto investigation: 240 BadIDChoice warnings for
    // CreatePixmap pid=0xffffffff, then panic at process_request.rs:11686
    // when state.clients.remove(client_51) finally won the race.
    //
    // Drop silently: the client is gone, no reply / error can be
    // delivered to anyone, and the work would be a no-op. Tests that
    // exercise individual handlers via `process_request` directly are
    // unaffected (they don't go through this dispatcher).
    if !state.clients.contains_key(&id.0) {
        log::debug!(
            "process_request_inline: dropping request from already-disconnected client {} \
             (opcode={}, seq={})",
            id.0,
            header.opcode,
            sequence.0,
        );
        return RequestOutcome::Handled;
    }
    let outcome = match process_request(state, backend, id, sequence, header, body, attached_fd) {
        Ok(out) => out,
        Err(err) => {
            // A request handler errored — usually a backend-side
            // limit (e.g., "too many points"). Log + continue rather
            // than killing the server. Pre-existing bug: bogus client
            // requests shouldn't be fatal.
            log::warn!(
                "request handler error (client {} opcode {}): {err}",
                id.0,
                header.opcode,
            );
            RequestOutcome::Handled
        }
    };
    // Pending work has not committed any visible result yet. Its completion
    // path performs this bookkeeping exactly once when the result is applied.
    if !matches!(&outcome, RequestOutcome::PendingCrtcConfig(_)) {
        if std::mem::take(&mut state.damage_notify_flush_pending) {
            backend.flush_before_damage_notify();
        }
        backend.mark_dirty();
    }
    outcome
}

/// Resume every asynchronous CRTC request whose backend result is ready.
/// `finish_crtc_config` is called only while the originating client is still
/// waiting, so a late worker completion can never install a cancelled mode.
fn drain_ready_crtc_configs(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    pending: &mut PendingBackendRequests,
) {
    for token in backend.drain_ready_crtc_configs() {
        let Some(parked) = pending.take_crtc(token) else {
            // Cancellation may race a worker completion. Discard the backend
            // result and keep the operation from becoming visible later.
            backend.cancel_crtc_config(token);
            continue;
        };
        if !state.clients.contains_key(&parked.client_id.0) {
            backend.cancel_crtc_config(token);
            continue;
        }

        let result = backend.finish_crtc_config(token);
        let outcome = match complete_crtc_config(
            state,
            backend,
            parked.client_id,
            parked.sequence,
            parked.continuation.completion,
            result,
        ) {
            Ok(outcome) => outcome,
            Err(err) => {
                log::warn!(
                    "RRSetCrtcConfig completion handler error (client {} token {}): {err}",
                    parked.client_id.0,
                    token.0,
                );
                RequestOutcome::Handled
            }
        };

        if std::mem::take(&mut state.damage_notify_flush_pending) {
            backend.flush_before_damage_notify();
        }
        backend.mark_dirty();
        match outcome {
            RequestOutcome::Disconnect(client) => {
                disconnect_with_pending_cleanup(state, backend, pending, client);
            }
            RequestOutcome::Handled => {
                grant_request_credit(state, parked.client_id, parked.request_wire_bytes)
            }
            RequestOutcome::PendingCrtcConfig(_) => {
                unreachable!("CRTC completion cannot start a second asynchronous request")
            }
        }
    }
}

/// X11 default auto-repeat initial delay before the first synthetic
/// KeyPress fires. Matches xset's `-r` defaults; not yet pulled from
/// the XKB Controls block.
const REPEAT_INITIAL_DELAY: Duration = Duration::from_millis(660);

/// X11 default auto-repeat period (25 Hz = 40 ms between synthetic
/// KeyPress events while a key is held).
const REPEAT_PERIOD: Duration = Duration::from_millis(40);

/// Run the core loop until `Message::Shutdown` is observed.
///
/// `poll` must already have its waker registered against `NOTIFY_TOKEN`
/// (see `core_loop::channel`). Additional fds (listener, client
/// writers, drm, libinput, signalfd, host-X11) get registered by their
/// respective phase tasks before this function takes over the thread.
///
/// `state` and `backend` are owned by the core loop for the duration
/// of the run — the whole point of the single-threaded refactor is
/// that only this thread can mutate them.
pub fn run_core(
    mut poll: Poll,
    rx: CoreReceiver,
    sender: CoreSender,
    state: &mut ServerState,
    backend: &mut dyn Backend,
    listener: Option<UnixListener>,
    client_id_allocator: &ClientIdAllocator,
    auth: Arc<AuthState>,
) -> io::Result<()> {
    let setup_registry = setup_thread::make_registry();
    let listener = if let Some(listener) = listener {
        listener.set_nonblocking(true)?;
        let raw = listener.as_raw_fd();
        poll.registry()
            .register(&mut SourceFd(&raw), LISTENER_TOKEN, Interest::READABLE)?;
        Some(listener)
    } else {
        None
    };

    // E3: register backend-owned fds with the core poller. KMS returns
    // `Drm` only after `take_input_ctx`; the libinput context, when
    // present, is owned by the dedicated libinput thread (E2/E4) so
    // the core never sees the libinput fd in production. The Libinput
    // arm is registered defensively in case a backend variant chooses
    // to skip the dedicated thread and run libinput on the core poll.
    let backend_poll_sources: Vec<_> = backend
        .poll_fds()
        .into_iter()
        .map(|(fd, kind)| BackendPollSource { fd, kind })
        .collect();
    for (index, source) in backend_poll_sources.iter().enumerate() {
        let token = backend_token(index).ok_or_else(|| {
            io::Error::other(format!(
                "backend exposes too many poll fds: {}",
                backend_poll_sources.len()
            ))
        })?;
        poll.registry()
            .register(&mut SourceFd(&source.fd), token, Interest::READABLE)?;
    }

    // Probe input devices at startup, Xorg-style: drain libinput's
    // initial device enumeration and seed `state.xi_devices` BEFORE the
    // serve loop begins, so the first client to connect sees the real
    // device model (e.g. device 4 = touchpad) immediately. Without this
    // the registry carries only the static {2,3,4,5} model until
    // libinput's first `DeviceAdded` burst is dispatched from the loop,
    // which on real hardware can land seconds after the desktop's
    // clients have already enumerated devices and cached a plain
    // pointer. No-op for backends without an on-core libinput context
    // (Direct mode, host-X11/nested) — see `Backend::probe_input_devices`.
    let seeded = backend.probe_input_devices(state);
    log::info!("xi: startup input probe — {seeded} devices seeded");
    // TODO(direct-mode startup probe): in Direct mode the libinput
    // Context lives on the dedicated input thread, so the hook above is
    // a no-op here. The input thread already dispatches the initial
    // enumeration and sends the `DeviceAdded` burst on the channel as
    // its very first action (input_thread::run, before its epoll loop),
    // which shrinks the startup probe race. Fully closing
    // it would mean draining already-queued `Message::HostInput` device
    // events from `rx` here before the serve loop — left out for now to
    // avoid reordering/duplicating the loop's own message handling for a
    // path that isn't the primary (M2/Asahi) target.

    // Xorg seeds `_XKB_RULES_NAMES` on the root at init; setxkbmap reads
    // it to learn the current rules before applying a new layout.
    crate::core_loop::xkb_layout::publish_xkb_rules_names(state, backend);

    let mut events = Events::with_capacity(64);
    let mut telemetry = LoopTelemetry::new();
    if telemetry.enabled {
        crate::core_loop::fanout::enable_outbound_telemetry();
        log::info!(
            "loop telemetry: enabled (YSERVER_LOOP_TELEMETRY set); \
             1s rollups via info!"
        );
    }
    let mut deferred_requests = FairRequestQueue::default();
    let mut server_grab_waiters: VecDeque<DeferredRequest> = VecDeque::new();
    let mut pending_backend_requests = PendingBackendRequests::default();
    loop {
        // The grab can be dropped by paths that have no release check of
        // their own — notably the two disconnect sites outside the message
        // loop (a failed outbound write, and the writable-interest
        // reconcile). Re-check once per iteration so a released grab always
        // frees its waiters no matter who released it. Without this, an
        // owner that dies via a failed write leaves waiters parked while
        // `deferred_requests` stays empty, so the timeout below blocks on
        // deadlines and those clients hang until unrelated traffic arrives.
        if state.server_grab_owner.is_none() {
            release_server_grab_waiters(
                &mut deferred_requests,
                &mut server_grab_waiters,
                &mut telemetry,
            );
        }
        // Fairness: if we already have unprocessed work queued from a
        // prior iteration, don't block on the poller — we have things
        // to do right now. Without this, an idle moment where the
        // channel is briefly empty would let `poll.poll` block until
        // a fresh fd event, leaving the backlog stranded.
        let poll_timeout = if deferred_requests.has_runnable(&pending_backend_requests) {
            Some(Duration::ZERO)
        } else {
            // Wake for the earliest deadline owned by either core
            // key-repeat or the backend (for example, a compositor
            // commit retry). `Duration::ZERO` keeps mio returning
            // immediately when a deadline is already due.
            let now = Instant::now();
            let repeat_deadline = state.repeat_state.as_ref().map(|r| r.next_fire);
            let backend_deadline = backend.next_wakeup();
            let dpms_deadline = state.dpms_transition_deadline();
            let ss_idle_deadline = state.screensaver_idle_deadline();
            let ss_cycle_deadline = state.screensaver_cycle_deadline();
            let idletime_alarm_deadline = state.idletime_alarm_deadline();
            repeat_deadline
                .into_iter()
                .chain(backend_deadline)
                .chain(dpms_deadline)
                .chain(ss_idle_deadline)
                .chain(ss_cycle_deadline)
                .chain(idletime_alarm_deadline)
                .min()
                .map(|deadline| {
                    deadline
                        .checked_duration_since(now)
                        .unwrap_or(Duration::ZERO)
                })
        };
        // BlockHandler analog (cf. Xorg glamor_block_handler → glamor_flush):
        // reap GPU render-op resources whose fences have signaled right
        // before we block. Driving this here — not from on_page_flip_ready —
        // is what keeps the KMS backend's engine `submitted` queue bounded
        // while the display is dark and clients keep drawing
        // (project_reclamation_starvation_leak). No-op for backends without
        // GPU resources to reap.
        backend.before_block();
        // Retry on EINTR. A signal delivered while we're blocked in poll()
        // surfaces as `ErrorKind::Interrupted` — notably SIGCONT and the
        // VT/seat signals on resume-from-suspend. That is NOT fatal: re-poll.
        // Propagating it `?` crashed yserver on wake from sleep (run_core
        // returned EINTR → exit → drop to the display manager). Mirrors the
        // Interrupted handling in `client_reader.rs`.
        loop {
            match poll.poll(&mut events, poll_timeout) {
                Ok(()) => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    cancel_all_pending_backend_requests(backend, &mut pending_backend_requests);
                    return Err(e);
                }
            }
        }
        let iter_start = if telemetry.enabled {
            Some(Instant::now())
        } else {
            None
        };
        let mut requests_this_iter: u32 = 0;
        let mut request_budget: usize = MAX_REQUESTS_PER_ITER;
        // Deadline for this iteration's request processing, paired with
        // `request_budget` — see `REQUEST_TIME_BUDGET`. Taken
        // unconditionally (not gated on telemetry) because the drain
        // loops below depend on it for latency bounding, and measured
        // from here so the first request of the iteration always runs.
        let drain_start = Instant::now();
        // Drain backlog from prior iterations first. The ready ring gives each
        // active client one turn while the count/time cap still guarantees
        // input and page-flip maintenance between request slices.
        drain_pending_requests(
            state,
            backend,
            &mut telemetry,
            &mut pending_backend_requests,
            &mut deferred_requests,
            &mut server_grab_waiters,
            &mut requests_this_iter,
            &mut request_budget,
            drain_start,
        );
        for ev in events.iter() {
            if let Some(index) = token_to_backend_index(ev.token()) {
                let Some(source) = backend_poll_sources.get(index).copied() else {
                    warn!(
                        "core_loop::run: backend poll token {:?} has no source",
                        ev.token()
                    );
                    continue;
                };
                match source.kind {
                    BackendFdKind::Drm => {
                        // Drain only the DRM device whose fd became readable.
                        // `receive_events()` may block on an idle device, so a
                        // multi-device backend must retain this exact identity.
                        if telemetry.enabled {
                            telemetry.page_flip_count += 1;
                        }
                        backend.on_page_flip_ready(state, source.fd);
                    }
                    BackendFdKind::DrmHotplug => {
                        backend.on_display_hotplug(state);
                    }
                    BackendFdKind::Libinput => {
                        // Optional core-owned libinput path. Direct KMS does
                        // not expose this fd because its input thread owns it.
                        backend.on_libinput_ready(state);
                    }
                    BackendFdKind::HostX11 => {
                        // Drain host frames into the backend's pending
                        // reply/event queues. Fanout remains at the outer-loop
                        // boundary to avoid recursive dispatch.
                        match backend.drain_host_socket() {
                            Ok(HostSocketStatus::WouldBlock) => {}
                            Ok(HostSocketStatus::Eof) => {
                                log::info!("host X11 connection closed; shutting down");
                                cancel_all_pending_backend_requests(
                                    backend,
                                    &mut pending_backend_requests,
                                );
                                return Ok(());
                            }
                            Err(err) => {
                                log::warn!("drain_host_socket: {err}");
                                cancel_all_pending_backend_requests(
                                    backend,
                                    &mut pending_backend_requests,
                                );
                                return Ok(());
                            }
                        }
                    }
                    BackendFdKind::PresentCompletion => {
                        drain_present_completions(state, backend);
                    }
                    BackendFdKind::ScanoutRenderCompletion => {
                        backend.on_scanout_render_completion(state);
                    }
                }
                continue;
            }
            match ev.token() {
                LISTENER_TOKEN => {
                    if let Some(listener) = listener.as_ref() {
                        accept_pending(
                            listener,
                            client_id_allocator,
                            &sender,
                            &setup_registry,
                            &auth,
                        );
                    }
                }
                NOTIFY_TOKEN => {
                    let mut channel_requests = 0_usize;
                    let mut channel_requests_by_client = HashMap::new();
                    for msg in rx.try_recv_all() {
                        match msg {
                            Message::Shutdown => {
                                setup_thread::shutdown_all(&setup_registry);
                                cancel_all_pending_backend_requests(
                                    backend,
                                    &mut pending_backend_requests,
                                );
                                return Ok(());
                            }
                            Message::Request {
                                id,
                                sequence,
                                accepted_at,
                                header,
                                body,
                                attached_fd,
                            } => {
                                if telemetry.enabled {
                                    channel_requests += 1;
                                    *channel_requests_by_client.entry(id).or_insert(0) += 1;
                                    telemetry.record_request_accepted(id, sequence);
                                }
                                let req = DeferredRequest {
                                    id,
                                    sequence,
                                    accepted_at,
                                    header,
                                    body,
                                    attached_fd,
                                };
                                // Keep one canonical per-client FIFO even
                                // while another client owns GrabServer. The
                                // drain path may temporarily park an older
                                // prefix, but newly accepted requests must
                                // remain behind the requests already queued
                                // for this client. Sending them directly to
                                // `server_grab_waiters` lets new arrivals jump
                                // ahead of that remaining suffix on release.
                                telemetry.record_deferred_push(req.id);
                                deferred_requests.push_back(req);
                            }
                            Message::SetupAllocate { id, response_tx } => {
                                handle_setup_allocate(state, id, response_tx);
                            }
                            Message::ClientSetupComplete {
                                id,
                                stream,
                                resource_id_base,
                                resource_id_mask,
                                byte_order,
                            } => {
                                if let Err(err) = handle_client_setup_complete(
                                    poll.registry(),
                                    &sender,
                                    &setup_registry,
                                    state,
                                    id,
                                    stream,
                                    resource_id_base,
                                    resource_id_mask,
                                    byte_order,
                                ) {
                                    error!("ClientSetupComplete for client {} failed: {err}", id.0);
                                    disconnect_with_pending_cleanup(
                                        state,
                                        backend,
                                        &mut pending_backend_requests,
                                        id,
                                    );
                                }
                            }
                            Message::ClientDisconnected { id, reason: _ } => {
                                disconnect_with_pending_cleanup(
                                    state,
                                    backend,
                                    &mut pending_backend_requests,
                                    id,
                                );
                            }
                            Message::HostInput(ev) => {
                                if telemetry.enabled {
                                    telemetry.record_host_input(Instant::now());
                                }
                                handle_host_input(state, backend, ev);
                                backend.mark_dirty();
                            }
                            Message::CrtcConfigReady => {
                                drain_ready_crtc_configs(
                                    state,
                                    backend,
                                    &mut pending_backend_requests,
                                );
                            }
                            Message::VtRelease => {
                                // When VT switching isn't armed there is no
                                // switch to service — ignore. (Deliberate
                                // diagnostic dumps go through DumpScanout /
                                // DumpDrawables via the Ctrl-Alt-Enter /
                                // Ctrl-Alt-F12 hotkeys, not this path.)
                                if backend.vt_switching_armed() {
                                    backend.on_vt_release(state);
                                }
                            }
                            Message::VtAcquire => {
                                if backend.vt_switching_armed() {
                                    backend.on_vt_acquire(state);
                                }
                            }
                            Message::SwitchVt(vt) => {
                                if backend.vt_switching_armed() {
                                    backend.request_vt_switch(vt);
                                }
                            }
                            Message::DumpScanout => backend.dump_scanout(),
                            Message::DumpDrawables => backend.dump_drawables(),
                        }
                        if state.server_grab_owner.is_none() {
                            release_server_grab_waiters(
                                &mut deferred_requests,
                                &mut server_grab_waiters,
                                &mut telemetry,
                            );
                        }
                    }
                    telemetry.record_channel_drain(channel_requests, &channel_requests_by_client);
                    drain_pending_requests(
                        state,
                        backend,
                        &mut telemetry,
                        &mut pending_backend_requests,
                        &mut deferred_requests,
                        &mut server_grab_waiters,
                        &mut requests_this_iter,
                        &mut request_budget,
                        drain_start,
                    );
                }
                tok => {
                    let Some(client_id) = token_to_client(tok) else {
                        warn!("core_loop::run: unhandled poll token {tok:?}");
                        continue;
                    };

                    // I3: WRITABLE-readiness on a client writer fd.
                    // Drain the outbound buffer; if it empties, the
                    // post-loop interest reconciliation drops
                    // WRITABLE. If the peer disappeared, mark the
                    // client for disconnect.
                    if !ev.is_writable() {
                        // mio always reports both READABLE+WRITABLE
                        // as readiness even when only one was asked
                        // for; the writer fd's READABLE wakeups are
                        // ignored — the reader thread owns reads.
                        continue;
                    }
                    let Some(client) = state.clients.get_mut(&client_id.0) else {
                        // Already removed by a prior disconnect; the
                        // poller will be deregistered after.
                        continue;
                    };
                    match client_io::drain_outbound(client) {
                        Ok(WriteOutcome::Done | WriteOutcome::WouldBlock) => {}
                        Ok(WriteOutcome::Disconnect) | Err(_) => {
                            disconnect_with_pending_cleanup(
                                state,
                                backend,
                                &mut pending_backend_requests,
                                client_id,
                            );
                        }
                    }
                }
            }
        }
        // F2: drain any host-X11 events the backend decoded during
        // this iteration. Fanout runs at the outermost stack frame
        // — no `wait_for_reply` is on the stack here — so handlers
        // that issue further host requests are safe.
        if dispatch_pending_host_events(state, backend) {
            // Host events (pointer, expose, configure) can change
            // visible state; mark dirty so the KMS gate re-arms. No-op
            // for backends without their own composite loop.
            backend.mark_dirty();
        }

        // Auto-repeat: if a key is held and its `next_fire` has
        // elapsed (either because the poll woke on the timeout, or
        // because an unrelated event arrived after the deadline),
        // fan out a synthetic KeyRelease+KeyPress pair.
        if state.repeat_state.is_some() {
            // Only poke the compositor when a repeat actually fired.
            // `fire_pending_repeats` returns false when the armed key
            // is merely not-yet-due (the common case every iteration
            // while a key is held) — an unconditional `mark_dirty()`
            // here re-dirtied the scene at the loop-iteration rate,
            // busy-spinning the compositor (and never letting it idle
            // when a phantom key is stuck armed).
            if fire_pending_repeats(state, backend) {
                backend.mark_dirty();
            }
        }

        // DPMS: evaluate idle-cascade transitions.
        if let Some(deadline) = state.dpms_transition_deadline() {
            let now = Instant::now();
            if now >= deadline {
                // Saturate rather than truncate — `as_millis()` returns u128
                // and idle > 49 days would silently wrap a `as u32` cast,
                // which would then fall *below* the timeout thresholds.
                let idle_ms = u32::try_from(state.dpms.last_activity.elapsed().as_millis())
                    .unwrap_or(u32::MAX);
                let target =
                    crate::server::next_dpms_level(state.dpms.power_level, idle_ms, &state.dpms);
                if target != state.dpms.power_level {
                    crate::core_loop::process_request::apply_dpms_transition(
                        state, backend, target,
                    );
                }
            }
        }

        // SS: evaluate idle activation and Cycle re-fire.
        evaluate_screen_saver_post_poll(state, backend);
        evaluate_idletime_alarms_post_poll(state, backend);

        // F2: if a `wait_for_reply` (called by `process_request`
        // mid-handler) saw the host close, propagate it as a clean
        // shutdown. The IO error already surfaced to the caller; we
        // observe the EOF flag here and stop the core loop.
        if backend.host_socket_eof() {
            log::info!("host X11 EOF observed; shutting down");
            cancel_all_pending_backend_requests(backend, &mut pending_backend_requests);
            return Ok(());
        }

        // I2: walk clients once per loop iteration and reconcile
        // poll interest against the live state of `outbound`. A
        // client whose buffer just became non-empty needs WRITABLE;
        // one that just drained back to empty drops it. Swallows
        // reregister errors that mean "fd already deregistered" so a
        // disconnect that ran during this iteration doesn't break
        // the next one.
        for disc_id in reconcile_client_writable_interest(poll.registry(), state) {
            disconnect_with_pending_cleanup(state, backend, &mut pending_backend_requests, disc_id);
        }

        run_iteration_tail(state, backend);

        // Diagnostic: per-iteration accounting + per-second telemetry
        // emit. Both are no-ops when `YSERVER_LOOP_TELEMETRY` is unset.
        if let Some(start) = iter_start {
            let now = Instant::now();
            let wall = now.saturating_duration_since(start);
            telemetry.record_iteration(requests_this_iter, wall);
            telemetry.maybe_emit(now);
        }
    }
}

/// The loop-body tail: service time-based backend work, drain due Present
/// work, then kick the compose path. Extracted so the drain-before-compose
/// ordering (see the comment on the `drain_present_completions` call below)
/// is independently testable via `RecordingBackend` without spinning up the
/// full `run` poll loop.
pub(crate) fn run_iteration_tail(state: &mut ServerState, backend: &mut dyn Backend) {
    // Damage can also originate outside a directly-dispatched request (for
    // example deferred Present execution). Preserve the same write-before-
    // observer boundary before the next poll can drain client output.
    if std::mem::take(&mut state.damage_notify_flush_pending) {
        backend.flush_before_damage_notify();
    }

    // Service time-based backend work that is not tied to an fd edge. The
    // backend reports its cadence via `next_wakeup`.
    backend.poll_deferred_input(state);

    // Drain-before-compose (spec "Loop-order and clock contract" item 1):
    // an entry executed here must be visible to THIS iteration's
    // `maybe_composite`, or it slips a full period whenever unrelated
    // damage exists.
    drain_present_completions(state, backend);

    // Wake the composite path back up if the backend went dormant
    // after the previous pageflip-complete (because nothing was
    // dirty) and fresh damage has since arrived. No-op for
    // backends that don't drive their own composite loop, and
    // no-op if a flip is still in flight on the KMS path.
    if let Err(e) = backend.maybe_composite() {
        log::warn!("core_loop::run: maybe_composite failed: {e}");
    }

    arm_present_idle_vblanks(state, backend);
}

/// Idle vblank arming for parked Present work — MUST run after
/// `maybe_composite`, not folded back into the pre-compose drain. KMS's
/// completion arm hard-gates on `present_completion_is_idle()`
/// (`!has_pending_page_flips() && !scene_wants_compose()`); `mark_dirty()`
/// alone (no output damage) makes `tick_one_output` return
/// `Skipped(EmptyDamage)`, which still clears `scene_wants_compose()`. Arm
/// before compose and that clear hasn't happened yet, so the gate sees a
/// dirty scene, arms nothing (`Ok(0)`), and a parked `CompleteNotify` can
/// starve with no fd left to wake `poll`. Running here, once per iteration,
/// also covers parks made by the epfd-driven drain (`run.rs:1027`, itself
/// pre-compose) in the same iteration — the backend dedups against its
/// per-CRTC armed-target map so a second call per iteration is safe.
pub(crate) fn arm_present_idle_vblanks(state: &mut ServerState, backend: &mut dyn Backend) {
    // Idle vblank arming: if NotifyMSC requests remain parked, ask the
    // backend to schedule a kernel vblank so the clock keeps advancing even
    // when nothing is flipping. A full-screen compositor redirects every
    // window → the scene is a static overlay → no pageflips → MSC never
    // advances → the compositor's `present` clock deadlocks. The backend
    // dedups against its per-CRTC armed-target map, so calling every
    // iteration is safe (no refire storm).
    if !state.present_pending_msc.is_empty() {
        let mut by_domain: std::collections::BTreeMap<(u32, u64), Vec<u64>> =
            std::collections::BTreeMap::new();
        for pending in &state.present_pending_msc {
            by_domain
                .entry((pending.crtc_id, pending.crtc_epoch))
                .or_default()
                .push(pending.target_msc);
        }
        for ((crtc_id, crtc_epoch), targets) in by_domain {
            if backend.present_crtc_clock_epoch(crtc_id) != crtc_epoch {
                continue;
            }
            match backend.arm_idle_vblanks(crtc_id, &targets) {
                Ok(armed) => {
                    if armed > 0 {
                        log::debug!(
                            "PRESENT-DBG: arm_idle_vblanks crtc=0x{crtc_id:x} pending={} -> armed={armed}",
                            targets.len()
                        );
                    }
                }
                Err(e) => log::warn!(
                    "PRESENT-DBG: arm_idle_vblanks crtc=0x{crtc_id:x} pending={} -> ERR {e}",
                    targets.len()
                ),
            }
        }
    }
    if !state.present_pending_complete.is_empty() {
        let mut by_domain: std::collections::BTreeMap<(u32, u64), Vec<u64>> =
            std::collections::BTreeMap::new();
        for pending in &state.present_pending_complete {
            by_domain
                .entry((pending.event.crtc_id, pending.event.crtc_epoch))
                .or_default()
                .push(pending.effective_target_msc);
        }
        for ((crtc_id, crtc_epoch), targets) in by_domain {
            if backend.present_crtc_clock_epoch(crtc_id) != crtc_epoch {
                continue;
            }
            // A page flip in flight is not sufficient as the only wake
            // source: arm the selected CRTC independently.
            let result = if backend.present_absolute_vblank_arm_supported(crtc_id) {
                backend.arm_present_absolute_vblank(crtc_id, &targets)
            } else {
                backend.arm_present_completion_idle_vblanks(crtc_id, &targets)
            };
            match result {
                Ok(armed) => {
                    if armed > 0 {
                        log::debug!(
                            "PRESENT-DBG: arm_present_completion_vblanks crtc=0x{crtc_id:x} pending={} -> armed={armed}",
                            targets.len()
                        );
                    }
                }
                Err(e) => log::warn!(
                    "PRESENT-DBG: arm_present_completion_vblanks crtc=0x{crtc_id:x} pending={} -> ERR {e}",
                    targets.len()
                ),
            }
        }
    }

    // Third arming call site (spec §msc-due, future-target fallback rung
    // 1): parked msc-due entries whose target is more than one vblank out
    // get an absolute per-target sequence arm here, alongside the other
    // two idle arms above — placement matches the spec's own wording
    // ("a third arming call site in run.rs, alongside present_pending_msc
    // ... and present_pending_complete ...", spec §msc-due future-target
    // bullet), not folded into the pre-compose due-pass
    // (`drain_due_present_pending_exec`): this call arms a kernel event,
    // it doesn't decide an execution, and every other arming call site in
    // this codebase already lives in this post-compose function. Must
    // NOT route through `arm_present_completion_idle_vblanks` — its
    // idle-only gate would suppress the arm during any activity.
    {
        // `(present_id, eff - 1)` for every still-parked, source-ready,
        // genuinely future-target entry. The `-1` is CORE-SIDE: `eff` is
        // the vblank at which the compose carrying this copy must already
        // have been submitted, so the copy itself is due one vblank
        // earlier, at `eff - 1`. `arm_present_absolute_vblank` arms
        // exactly the values it receives (Task 3) — it does not itself
        // subtract. `wrapping_sub`: `eff` is a wrapped MSC value (u64
        // wraparound is a documented, tested case throughout this
        // module), so a plain `eff - 1` would debug-panic when `eff == 0`.
        let mut by_domain: std::collections::BTreeMap<(u32, u64), Vec<(u64, u64)>> =
            std::collections::BTreeMap::new();
        for (&pid, entry) in &state.present_pending_exec {
            if !entry.source_ready {
                continue;
            }
            let crtc_id = entry.pending.crtc_id;
            let crtc_epoch = entry.pending.crtc_epoch;
            if backend.present_crtc_clock_epoch(crtc_id) != crtc_epoch
                || !backend.present_absolute_vblank_arm_supported(crtc_id)
            {
                continue;
            }
            let clock_msc = crate::core_loop::process_request::cached_present_crtc_clock(
                state, crtc_id, crtc_epoch,
            )
            .msc;
            if let Some(eff) = entry.pending.effective_target_msc
                && crate::present_scheduler::msc_is_after(eff, clock_msc.wrapping_add(1))
            {
                by_domain
                    .entry((crtc_id, crtc_epoch))
                    .or_default()
                    .push((pid, eff.wrapping_sub(1)));
            }
        }
        for ((crtc_id, _crtc_epoch), future_parked) in by_domain {
            let targets: Vec<u64> = future_parked.iter().map(|&(_, t)| t).collect();
            // Full coverage required, not just `> 0`: the trait contract
            // (`arm_present_absolute_vblank`'s doc comment) allows a
            // partial `Ok(n)` — some targets newly armed or already
            // covered, others not (e.g. a CRTC set change mid-call).
            // Treating any partial result as success would leave the
            // uncovered subset parked with no wake source at all.
            // Unreachable against today's KMS impl (Task 3): it arms
            // every target on every connected CRTC or trips the
            // EOPNOTSUPP latch and returns `Err`, so it's all-or-`Err`
            // in practice — this guard is a contract-level guarantee,
            // not a dead branch removal candidate.
            match backend.arm_present_absolute_vblank(crtc_id, &targets) {
                Ok(covered) if covered == targets.len() => {
                    log::debug!(
                        "PRESENT-DBG: arm_present_absolute_vblank crtc=0x{crtc_id:x} pending={} -> armed={covered}",
                        targets.len()
                    );
                }
                other => {
                    // `Ok(0)` (nothing covered — including the iteration
                    // where an EOPNOTSUPP latch first trips), a partial
                    // `Ok(n < targets.len())`, or `Err`: the caller must
                    // not park the uncovered entries on this mechanism.
                    // Execute ALL of them immediately in this same pass
                    // (trigger=idle_fallback) rather than leave any
                    // subset parked with no wake source. This runs
                    // post-compose (this function, per the call-site
                    // placement above), so a latch-trip execution here
                    // misses THIS iteration's compose and lands in the
                    // next one instead — `mark_dirty` still guarantees
                    // the wake for it; accepted as a rare, one-iteration-
                    // latency path.
                    match other {
                        Ok(covered) => log::debug!(
                            "PRESENT-DBG: arm_present_absolute_vblank crtc=0x{crtc_id:x} pending={} -> covered={covered}, \
                             executing immediately",
                            targets.len()
                        ),
                        Err(e) => log::warn!(
                            "PRESENT-DBG: arm_present_absolute_vblank crtc=0x{crtc_id:x} pending={} -> ERR {e}",
                            targets.len()
                        ),
                    }
                    let ids: Vec<u64> = future_parked.iter().map(|&(pid, _)| pid).collect();
                    crate::core_loop::process_request::execute_parked_present_ids(
                        state,
                        backend,
                        &ids,
                        "idle_fallback",
                    );
                }
            }
        }
    }
}

fn drain_present_completions(state: &mut ServerState, backend: &mut dyn Backend) {
    // Producer readiness precedes copy submission, which in turn precedes the
    // existing GPU-completion queue below. Keeping both on the same stable
    // backend wake fd avoids blocking request dispatch on client GPU work.
    crate::core_loop::process_request::drain_ready_present_pixmaps(state, backend);

    // msc-due-pass (spec §msc-due; Task 7): re-classify every msc-parked
    // source-ready entry against the fresh general clock and execute
    // whatever is now due, plus the idle-display and blackout fallback
    // rungs (the absolute-vblank-arm rung is a call-site match for the
    // other two arms below and lives in `arm_present_idle_vblanks`,
    // post-compose). Runs here, at the top of this pre-compose drain
    // (Task 4), so an entry executed here is visible to THIS iteration's
    // compose.
    crate::core_loop::process_request::drain_due_present_pending_exec(state, backend);

    let completed = backend.drain_completed_present_events();
    for entry in completed {
        if !crate::core_loop::process_request::present_event_window_is_current(state, &entry) {
            state.present_complete_gate.remove(&entry.present_id);
            crate::core_loop::process_request::discard_stale_present_event(
                state, backend, &entry, false,
            );
            continue;
        }
        let completion_clock =
            crate::core_loop::process_request::refresh_present_crtc_completion_clock(
                state,
                backend,
                entry.crtc_id,
                entry.crtc_epoch,
                entry.completion_clock,
            );
        // Pace: if this completion recorded a future target-msc gate, park the
        // whole thing (wake NOT signalled yet) until that vblank. Otherwise
        // (async / no clock / target already reached) complete now.
        // The epoch-qualified cache here is the previous iteration's value;
        // the refresh + per-domain sweep below release anything due now.
        match state.present_complete_gate.remove(&entry.present_id) {
            Some(gate)
                if backend.present_crtc_clock_epoch(gate.crtc_id) == gate.crtc_epoch
                    && crate::present_scheduler::msc_is_after(
                        gate.effective_target_msc,
                        completion_clock.msc,
                    ) =>
            {
                let mode = entry.completion_mode;
                let emit_idle = entry.emit_idle;
                log::debug!(
                    target: "present_pace",
                    "PACE-INSTR t={} pid={} stage=drained_parked eff={} kernel_msc={}",
                    crate::core_loop::process_request::pace_instr_ms(),
                    entry.present_id,
                    gate.effective_target_msc,
                    completion_clock.msc
                );
                state
                    .present_pending_complete
                    .push(crate::server::PendingPresentComplete {
                        event: entry,
                        effective_target_msc: gate.effective_target_msc,
                        mode,
                        emit_idle,
                    });
            }
            Some(gate) => {
                let mode = entry.completion_mode;
                let emit_idle = entry.emit_idle;
                // Due now against the completion clock, but still routed
                // through the ordered queue (spec §Ordered completion
                // delivery item 2) rather than fired here directly: a
                // Skip parked earlier at scrap (request-arrival) time can
                // have a *smaller* present_id than this entry's, and
                // firing this Copy immediately would let it overtake that
                // Skip in the client's per-window CompleteNotify stream.
                // `fire_due_present_completions`, called later in this
                // same drain pass, delivers in per-window present_id
                // order instead of raw arrival order.
                log::debug!(
                    target: "present_pace",
                    "PACE-INSTR t={} pid={} stage=drained_due completion_msc={} source={:?}",
                    crate::core_loop::process_request::pace_instr_ms(),
                    entry.present_id,
                    completion_clock.msc,
                    completion_clock.source
                );
                state
                    .present_pending_complete
                    .push(crate::server::PendingPresentComplete {
                        event: entry,
                        effective_target_msc: gate.effective_target_msc,
                        mode,
                        emit_idle,
                    });
            }
            None => {
                log::debug!(
                    target: "present_pace",
                    "PACE-INSTR t={} pid={} stage=drained_immediate kernel_msc={}",
                    crate::core_loop::process_request::pace_instr_ms(),
                    entry.present_id,
                    completion_clock.msc
                );
                // Async completions sit outside the per-window hold-back
                // by design (spec round-4 F6) and fire here immediately —
                // but flush anything already due-and-unblocked in the
                // queue FIRST, or this inline fire would itself create a
                // backward serial against a same-window gated Copy that
                // is due but hasn't been swept yet (that Copy was pushed
                // into the queue by the `Some(gate)` arm above, earlier
                // in this same `completed` loop, for exactly this
                // reason). Held-back entries are unaffected — they stay
                // held regardless of how many times the sweep runs.
                crate::core_loop::process_request::fire_due_present_completions_for_domain(
                    state,
                    backend,
                    entry.crtc_id,
                    entry.crtc_epoch,
                    completion_clock,
                );
                crate::core_loop::process_request::complete_present_now(state, backend, &entry);
            }
        }
    }

    // Direct Present completion and source-idle are different retirements.
    // A replacement frame idles the previous source without completing it a
    // second time.
    for event in backend.drain_retired_present_idle_events() {
        if crate::core_loop::process_request::present_event_window_is_current(state, &event) {
            crate::core_loop::process_request::retire_present_idle(state, backend, &event);
        } else {
            crate::core_loop::process_request::discard_stale_present_event(
                state, backend, &event, true,
            );
        }
    }

    // Refresh every domain that still owns parked work. Epoch-qualified
    // caches preserve old clocks across stable-XID remaps; stale rows fail
    // open against that old cache and are never compared/armed against the
    // replacement physical counter.
    let mut domains: Vec<(u32, u64)> = Vec::new();
    domains.extend(
        state
            .present_pending_msc
            .iter()
            .map(|p| (p.crtc_id, p.crtc_epoch)),
    );
    domains.extend(
        state
            .present_pending_complete
            .iter()
            .map(|p| (p.event.crtc_id, p.event.crtc_epoch)),
    );
    domains.extend(
        state
            .present_pending_exec
            .values()
            .map(|p| (p.pending.crtc_id, p.pending.crtc_epoch)),
    );
    domains.sort_unstable();
    domains.dedup();

    for (crtc_id, crtc_epoch) in domains {
        let epoch_current = backend.present_crtc_clock_epoch(crtc_id) == crtc_epoch;
        let general = if epoch_current {
            crate::core_loop::process_request::refresh_present_crtc_general_clock(
                state, backend, crtc_id, crtc_epoch,
            )
        } else {
            crate::core_loop::process_request::cached_present_crtc_clock(state, crtc_id, crtc_epoch)
        };
        crate::core_loop::process_request::fire_due_present_notify_msc_for_domain(
            state,
            crtc_id,
            crtc_epoch,
            general.msc,
            general.ust,
            !epoch_current,
        );
        let completion = crate::core_loop::process_request::refresh_present_crtc_completion_clock(
            state, backend, crtc_id, crtc_epoch, None,
        );
        crate::core_loop::process_request::fire_due_present_completions_for_domain(
            state, backend, crtc_id, crtc_epoch, completion,
        );
    }
}

/// F2: pop every pending host event off the backend and fan it out
/// to nested clients. Runs at the outer-loop boundary so a host
/// request issued inside fanout (CreateWindow forwarding,
/// SetClipRectangles, etc.) cannot recursively re-dispatch — the new
/// request's reply lands in `pending_replies` and the next
/// outer-loop iteration drains anything `wait_for_reply` re-enqueued.
fn dispatch_pending_host_events(state: &mut ServerState, backend: &mut dyn Backend) -> bool {
    let mut any = false;
    while let Some(event) = backend.pop_pending_host_event() {
        any = true;
        // The fanout helpers borrow `xid_map` immutably — clone the
        // map up-front so we can release the immutable borrow on
        // backend before mutating `state`'s per-client outbound
        // buffers. The map is a few hundred entries even on a busy
        // session.
        let xid_map = backend.xid_map().clone();
        match event {
            HostEvent::Pointer(ev) => {
                use crate::core_loop::pointer_fanout::pointer_event_fanout_to_state;
                let _dropped =
                    pointer_event_fanout_to_state(state, backend, &xid_map, ev, true, false);
            }
            HostEvent::Expose(ev) => {
                use crate::core_loop::fanout::expose_event_fanout_to_state;
                let _dropped = expose_event_fanout_to_state(state, &xid_map, ev);
            }
            HostEvent::Key(ev) => {
                use crate::core_loop::key_fanout::key_event_fanout_to_state;
                let _dropped = key_event_fanout_to_state(state, backend, ev);
            }
            HostEvent::Configure(ev) => {
                if backend.window_id() == ev.host_xid {
                    handle_host_container_resize(state, backend, ev);
                }
            }
            HostEvent::Closed => {
                log::info!("host container window destroyed; shutting down");
                // Triggering shutdown via a flag is awkward without
                // sender access here — return Ok from run_core via
                // host_socket_eof check on next iteration.
            }
        }
    }
    any
}

pub(crate) fn handle_host_container_resize(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    ev: crate::host_x11::HostConfigureEvent,
) {
    if ev.width == 0
        || ev.height == 0
        || (state.randr.screen_width == ev.width && state.randr.screen_height == ev.height)
    {
        return;
    }
    let timestamp = state.timestamp_now();
    state.randr.resize(timestamp, ev.width, ev.height);

    // The nested path resizes the single output, so it genuinely changes
    // CRTC geometry — pass the output as changed so CrtcChangeNotify fires.
    let changed: Vec<(u32, u32, u32)> = state
        .randr
        .outputs
        .first()
        .map(|o| (o.output_id, o.crtc_id, o.mode_id))
        .into_iter()
        .collect();
    apply_screen_size_side_effects(state, backend, ev.width, ev.height, &changed);
}

/// Tell RANDR subscribers the layout changed without the screen resizing.
///
/// Xorg treats a primary-output change as a layout change, not a geometry one:
/// `RRSetPrimaryOutput` marks the affected outputs via `RROutputChanged`, sets
/// `layoutChanged`, and calls `RRTellChanged` (randr/rroutput.c), which fans out
/// ScreenChangeNotify plus one OutputChangeNotify per changed output. No
/// CrtcChangeNotify: no CRTC moved. Without this, panels and desktop shells
/// never learn the primary moved, because polling `GetOutputPrimary` is not how
/// they are written — they wait for the notify.
///
/// Screen dimensions come straight from `state.randr`, so this stays correct if
/// it is ever called after a resize.
pub(crate) fn notify_randr_layout_changed(state: &mut ServerState, changed_outputs: &[u32]) {
    use std::sync::atomic::Ordering;
    use yserver_protocol::x11::{SequenceNumber, randr as x11randr};

    const RANDR_FIRST_EVENT: u8 = 89;

    let timestamp = state.randr.timestamp;
    let config_timestamp = state.randr.config_timestamp;
    let width = state.randr.screen_width;
    let height = state.randr.screen_height;
    let width_mm = u16::try_from(state.randr.width_mm).unwrap_or(u16::MAX);
    let height_mm = u16::try_from(state.randr.height_mm).unwrap_or(u16::MAX);
    // Resolve each changed output's current crtc/mode for the notify payload.
    let changed: Vec<(u32, u32, u32, u8)> = changed_outputs
        .iter()
        .filter_map(|id| {
            state
                .randr
                .outputs
                .iter()
                .find(|o| o.output_id == *id)
                .map(|o| {
                    (
                        o.output_id,
                        if o.mode_id != 0 { o.crtc_id } else { 0 },
                        o.mode_id,
                        if o.connected {
                            x11randr::CONNECTION_CONNECTED
                        } else {
                            x11randr::CONNECTION_DISCONNECTED
                        },
                    )
                })
        })
        .collect();

    let subscribers: Vec<(u32, yserver_protocol::x11::ResourceId, u16)> = state
        .randr_select_masks
        .iter()
        .map(|((owner, window), mask)| (*owner, *window, *mask))
        .collect();
    for (owner, request_window, mask) in subscribers {
        let Some(client) = state.clients.get_mut(&owner) else {
            continue;
        };
        let sequence = SequenceNumber(client.last_sequence.load(Ordering::Relaxed));
        if mask & x11randr::NOTIFY_MASK_SCREEN_CHANGE != 0 {
            let event = x11randr::encode_screen_change_notify_event(
                client.byte_order,
                RANDR_FIRST_EVENT,
                sequence,
                x11randr::ScreenChangeNotify {
                    timestamp,
                    config_timestamp,
                    root: crate::resources::ROOT_WINDOW.0,
                    request_window: request_window.0,
                    width,
                    height,
                    width_mm,
                    height_mm,
                },
            );
            crate::core_loop::fanout::record_outbound_telemetry(
                yserver_protocol::x11::ClientId(owner),
                client.byte_order,
                &event,
            );
            let _ = client_io::write_or_buffer(client, &event);
        }
        if mask & x11randr::NOTIFY_MASK_OUTPUT_CHANGE != 0 {
            for &(output, crtc, mode, connection) in &changed {
                let event = x11randr::encode_output_change_notify_event(
                    client.byte_order,
                    RANDR_FIRST_EVENT,
                    sequence,
                    x11randr::OutputChangeNotify {
                        timestamp,
                        config_timestamp,
                        request_window: request_window.0,
                        output,
                        crtc,
                        mode,
                        connection,
                    },
                );
                crate::core_loop::fanout::record_outbound_telemetry(
                    yserver_protocol::x11::ClientId(owner),
                    client.byte_order,
                    &event,
                );
                let _ = client_io::write_or_buffer(client, &event);
            }
        }
    }
}

/// Fan out `RRNotify_ProviderChange` after an output-source relationship
/// actually changes. Xorg marks and announces the initiating provider only;
/// the peer's reciprocal association is observable through `GetProviderInfo`
/// without a second event.
pub(crate) fn notify_randr_provider_changed(state: &mut ServerState, provider: u32) {
    use std::sync::atomic::Ordering;
    use yserver_protocol::x11::{SequenceNumber, randr as x11randr};

    const RANDR_FIRST_EVENT: u8 = 89;

    let subscribers: Vec<(u32, yserver_protocol::x11::ResourceId, u16)> = state
        .randr_select_masks
        .iter()
        .map(|((owner, window), mask)| (*owner, *window, *mask))
        .collect();
    for (owner, request_window, mask) in subscribers {
        if mask & x11randr::NOTIFY_MASK_PROVIDER_CHANGE == 0 {
            continue;
        }
        let Some(client) = state.clients.get_mut(&owner) else {
            continue;
        };
        let sequence = SequenceNumber(client.last_sequence.load(Ordering::Relaxed));
        let event = x11randr::encode_provider_change_notify_event(
            client.byte_order,
            RANDR_FIRST_EVENT,
            sequence,
            x11randr::ProviderChangeNotify {
                timestamp: state.randr.timestamp,
                request_window: request_window.0,
                provider,
            },
        );
        crate::core_loop::fanout::record_outbound_telemetry(
            yserver_protocol::x11::ClientId(owner),
            client.byte_order,
            &event,
        );
        let _ = client_io::write_or_buffer(client, &event);
    }
}

/// Fans out `RRNotify_OutputProperty` (randr/rrproperty.c
/// `RRDeliverPropertyEvent`) to every client that selected
/// `NOTIFY_MASK_OUTPUT_PROPERTY` via `RRSelectInput`. Unlike
/// `notify_randr_layout_changed`, this is not gated on
/// `NOTIFY_MASK_SCREEN_CHANGE`/`NOTIFY_MASK_OUTPUT_CHANGE` — property
/// changes are a distinct notify sub-type in the real protocol.
pub(crate) fn notify_randr_output_property_changed(
    state: &mut ServerState,
    output: u32,
    atom: yserver_protocol::x11::AtomId,
    property_state: u8,
) {
    use std::sync::atomic::Ordering;
    use yserver_protocol::x11::{SequenceNumber, randr as x11randr};

    const RANDR_FIRST_EVENT: u8 = 89;

    let timestamp = state.randr.timestamp;
    let subscribers: Vec<(u32, yserver_protocol::x11::ResourceId, u16)> = state
        .randr_select_masks
        .iter()
        .map(|((owner, window), mask)| (*owner, *window, *mask))
        .collect();
    for (owner, request_window, mask) in subscribers {
        if mask & x11randr::NOTIFY_MASK_OUTPUT_PROPERTY == 0 {
            continue;
        }
        let Some(client) = state.clients.get_mut(&owner) else {
            continue;
        };
        let sequence = SequenceNumber(client.last_sequence.load(Ordering::Relaxed));
        let event = x11randr::encode_output_property_notify_event(
            client.byte_order,
            RANDR_FIRST_EVENT,
            sequence,
            x11randr::OutputPropertyNotify {
                request_window: request_window.0,
                output,
                atom: atom.0,
                timestamp,
                state: property_state,
            },
        );
        crate::core_loop::fanout::record_outbound_telemetry(
            yserver_protocol::x11::ClientId(owner),
            client.byte_order,
            &event,
        );
        let _ = client_io::write_or_buffer(client, &event);
    }
}

/// Common side-effects of a logical-screen-size change: update root +
/// overlay window records, emit ConfigureNotify / Present ConfigureNotify,
/// fan out RANDR notifies (ScreenChange always; Crtc/Output only for entries
/// in `changed`), and re-clamp/warp the pointer. `changed` is empty for a pure
/// RRSetScreenSize (CRTCs unchanged).
pub(crate) fn apply_screen_size_side_effects(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    width: u16,
    height: u16,
    changed: &[(u32, u32, u32)],
) {
    if let Some(root) = state.resources.window_mut(crate::resources::ROOT_WINDOW) {
        root.width = width;
        root.height = height;
    }
    if let Some(overlay) = state
        .resources
        .window_mut(crate::resources::COMPOSITE_OVERLAY_WINDOW)
    {
        overlay.width = width;
        overlay.height = height;
    }

    emit_screen_resize_window_notifications(state, width, height);
    emit_randr_change_notifications(state, changed);

    // Pointer: clamp into [0,w)×[0,h); if the screen shrank below the
    // current cursor position, warp it inside (Xorg
    // RRPointerScreenConfigured / ScreenRestructured). The KMS motion
    // clamp only applies on the NEXT motion, so the explicit warp is
    // required to avoid a stranded off-screen cursor.
    let (px, py) = state.pointer_root;
    let cx = i32::from(px).clamp(0, i32::from(width.saturating_sub(1)));
    let cy = i32::from(py).clamp(0, i32::from(height.saturating_sub(1)));
    if cx != i32::from(px) || cy != i32::from(py) {
        let prev = state.barrier_bypass;
        state.barrier_bypass = true;
        backend.warp_pointer_root(state, cx, cy);
        state.barrier_bypass = prev;
    }
}

/// Emit the window-size notifications that clients normally get from
/// `ConfigureWindow`, for screen-sized server windows that RandR resizes
/// out-of-band. This intentionally does not mutate the resource geometry:
/// callers must update root/COW first, then use this to wake clients that
/// cache drawable or Present buffer sizes.
pub(crate) fn emit_screen_resize_window_notifications(
    state: &mut ServerState,
    width: u16,
    height: u16,
) {
    use yserver_protocol::x11;

    let root_geometry = x11::Geometry {
        root: crate::resources::ROOT_WINDOW,
        x: 0,
        y: 0,
        width,
        height,
        border_width: 0,
        depth: 24,
    };

    // Core ConfigureNotify on root for non-RANDR-aware clients
    // selecting StructureNotifyMask. Spec-correct ordering: emit this
    // before the RANDR fanout so non-RANDR-aware clients (panels,
    // "fill the screen" apps) reflow at the same point in the event
    // stream that RANDR-aware toolkits see screen-change.
    let _dropped = crate::core_loop::fanout::emit_window_event_to_state(
        state,
        crate::resources::ROOT_WINDOW,
        0x0002_0000, // StructureNotifyMask
        |buf, seq, order| {
            x11::encode_configure_notify_event(
                buf,
                seq,
                order,
                crate::resources::ROOT_WINDOW,
                crate::resources::ROOT_WINDOW,
                None,
                root_geometry,
                false,
            );
        },
    );
    fire_present_configure_notify_for_window(state, crate::resources::ROOT_WINDOW, root_geometry);

    if let Some((parent, geometry, override_redirect)) = state
        .resources
        .window(crate::resources::COMPOSITE_OVERLAY_WINDOW)
        .map(|overlay| {
            (
                overlay.parent,
                x11::Geometry {
                    root: crate::resources::ROOT_WINDOW,
                    x: overlay.x,
                    y: overlay.y,
                    width: overlay.width,
                    height: overlay.height,
                    border_width: overlay.border_width,
                    depth: overlay.depth,
                },
                overlay.override_redirect,
            )
        })
    {
        let above_sibling = state
            .resources
            .configure_notify_above_sibling(crate::resources::COMPOSITE_OVERLAY_WINDOW);
        let _dropped = crate::core_loop::fanout::emit_window_event_to_state(
            state,
            crate::resources::COMPOSITE_OVERLAY_WINDOW,
            0x0002_0000, // StructureNotifyMask
            |buf, seq, order| {
                x11::encode_configure_notify_event(
                    buf,
                    seq,
                    order,
                    crate::resources::COMPOSITE_OVERLAY_WINDOW,
                    crate::resources::COMPOSITE_OVERLAY_WINDOW,
                    above_sibling,
                    geometry,
                    override_redirect,
                );
            },
        );
        let _dropped = crate::core_loop::fanout::emit_window_event_to_state(
            state,
            parent,
            0x0008_0000, // SubstructureNotifyMask
            |buf, seq, order| {
                x11::encode_configure_notify_event(
                    buf,
                    seq,
                    order,
                    parent,
                    crate::resources::COMPOSITE_OVERLAY_WINDOW,
                    above_sibling,
                    geometry,
                    override_redirect,
                );
            },
        );
        fire_present_configure_notify_for_window(
            state,
            crate::resources::COMPOSITE_OVERLAY_WINDOW,
            geometry,
        );
    }
}

/// `RRSetCrtcConfig` can complete the physical modeset after a compositor
/// already issued `RRSetScreenSize` and received its immediate configure
/// notifications. When the active-output bbox then changes and catches up
/// with the logical screen, re-emit root/COW notifications so clients observe
/// the size again after the modeset. An unchanged bbox (for example, a
/// refresh-rate-only change) needs no window-size notification.
pub(crate) fn emit_screen_resize_window_notifications_if_outputs_caught_up(
    state: &mut ServerState,
    previous_bbox: Option<(u16, u16)>,
) {
    let Some((bbox_w, bbox_h)) = enabled_output_bbox(state) else {
        return;
    };
    if previous_bbox != Some((bbox_w, bbox_h))
        && bbox_w == state.randr.screen_width
        && bbox_h == state.randr.screen_height
    {
        emit_screen_resize_window_notifications(state, bbox_w, bbox_h);
    }
}

pub(crate) fn enabled_output_bbox(state: &ServerState) -> Option<(u16, u16)> {
    let mut any = false;
    let mut max_x = 0i32;
    let mut max_y = 0i32;
    for output in state.randr.outputs.iter().filter(|o| o.mode_id != 0) {
        any = true;
        max_x = max_x.max(i32::from(output.x).saturating_add(i32::from(output.width)));
        max_y = max_y.max(i32::from(output.y).saturating_add(i32::from(output.height)));
    }
    any.then(|| {
        (
            u16::try_from(max_x.max(0)).unwrap_or(u16::MAX),
            u16::try_from(max_y.max(0)).unwrap_or(u16::MAX),
        )
    })
}

/// Fan out RANDR change notifications for a topology/geometry change.
pub fn emit_randr_change_notifications(state: &mut ServerState, changed: &[(u32, u32, u32)]) {
    emit_randr_change_notifications_split(state, changed, changed);
}

/// Fan out a connector-registry change while allowing Output-only changes to
/// remain distinct from changes to current CRTC assignment or geometry.
/// The dirty sets are independent: a recompact can move a surviving CRTC
/// without changing its output association, while a mode-list or connection
/// refresh can dirty only an Output.
pub fn emit_randr_connector_change_notifications(
    state: &mut ServerState,
    crtc_changed: &[(u32, u32, u32)],
    output_changed: &[(u32, u32, u32)],
) {
    emit_randr_change_notifications_split(state, crtc_changed, output_changed);
}

fn emit_randr_change_notifications_split(
    state: &mut ServerState,
    crtc_changed: &[(u32, u32, u32)],
    output_changed: &[(u32, u32, u32)],
) {
    use std::sync::atomic::Ordering;
    use yserver_protocol::x11::{SequenceNumber, randr as x11randr};

    const RANDR_FIRST_EVENT: u8 = 89;

    let timestamp = state.randr.timestamp;
    let config_timestamp = state.randr.config_timestamp;
    let width = state.randr.screen_width;
    let height = state.randr.screen_height;
    let width_mm = u16::try_from(state.randr.width_mm).unwrap_or(u16::MAX);
    let height_mm = u16::try_from(state.randr.height_mm).unwrap_or(u16::MAX);
    // Per-CRTC geometry (position AND mode size). CrtcChangeNotify must
    // report each CRTC's own mode dimensions — NOT the logical screen
    // size — or a multi-monitor client sees every CRTC as e.g. 5120×1440
    // instead of its real 2560×1440. An off CRTC (no mode) reports 0×0.
    let crtc_geom: std::collections::HashMap<u32, (i16, i16, u16, u16)> = state
        .randr
        .outputs
        .iter()
        .map(|o| (o.crtc_id, (o.x, o.y, o.width, o.height)))
        .collect();
    let output_states: std::collections::HashMap<u32, (u8, u32)> = state
        .randr
        .outputs
        .iter()
        .map(|output| {
            (
                output.output_id,
                (
                    if output.connected {
                        x11randr::CONNECTION_CONNECTED
                    } else {
                        x11randr::CONNECTION_DISCONNECTED
                    },
                    if output.mode_id != 0 {
                        output.crtc_id
                    } else {
                        0
                    },
                ),
            )
        })
        .collect();

    let subscribers: Vec<(u32, yserver_protocol::x11::ResourceId, u16)> = state
        .randr_select_masks
        .iter()
        .map(|((owner, window), mask)| (*owner, *window, *mask))
        .collect();
    for (owner, request_window, mask) in subscribers {
        let Some(client) = state.clients.get_mut(&owner) else {
            continue;
        };
        let sequence = SequenceNumber(client.last_sequence.load(Ordering::Relaxed));
        if mask & x11randr::NOTIFY_MASK_SCREEN_CHANGE != 0 {
            let event = x11randr::encode_screen_change_notify_event(
                client.byte_order,
                RANDR_FIRST_EVENT,
                sequence,
                x11randr::ScreenChangeNotify {
                    timestamp,
                    config_timestamp,
                    root: crate::resources::ROOT_WINDOW.0,
                    request_window: request_window.0,
                    width,
                    height,
                    width_mm,
                    height_mm,
                },
            );
            crate::core_loop::fanout::record_outbound_telemetry(
                yserver_protocol::x11::ClientId(owner),
                client.byte_order,
                &event,
            );
            let _ = client_io::write_or_buffer(client, &event);
        }
        // Xorg fans out all dirty CRTCs before all dirty outputs for each
        // subscriber; do not interleave the two event classes per output.
        if mask & x11randr::NOTIFY_MASK_CRTC_CHANGE != 0 {
            for &(_output, crtc, mode) in crtc_changed {
                let (x, y, crtc_w, crtc_h) = crtc_geom.get(&crtc).copied().unwrap_or((0, 0, 0, 0));
                let event = x11randr::encode_crtc_change_notify_event(
                    client.byte_order,
                    RANDR_FIRST_EVENT,
                    sequence,
                    x11randr::CrtcChangeNotify {
                        timestamp,
                        request_window: request_window.0,
                        crtc,
                        mode,
                        x,
                        y,
                        width: crtc_w,
                        height: crtc_h,
                    },
                );
                crate::core_loop::fanout::record_outbound_telemetry(
                    yserver_protocol::x11::ClientId(owner),
                    client.byte_order,
                    &event,
                );
                let _ = client_io::write_or_buffer(client, &event);
            }
        }
        if mask & x11randr::NOTIFY_MASK_OUTPUT_CHANGE != 0 {
            for &(output, projected_crtc, projected_mode) in output_changed {
                let (connection, current_crtc) = output_states
                    .get(&output)
                    .copied()
                    .unwrap_or((x11randr::CONNECTION_CONNECTED, projected_crtc));
                let event = x11randr::encode_output_change_notify_event(
                    client.byte_order,
                    RANDR_FIRST_EVENT,
                    sequence,
                    x11randr::OutputChangeNotify {
                        timestamp,
                        config_timestamp,
                        request_window: request_window.0,
                        output,
                        crtc: current_crtc,
                        mode: projected_mode,
                        connection,
                    },
                );
                crate::core_loop::fanout::record_outbound_telemetry(
                    yserver_protocol::x11::ClientId(owner),
                    client.byte_order,
                    &event,
                );
                let _ = client_io::write_or_buffer(client, &event);
            }
        }
    }
}

/// I2: re-arm `WRITABLE` interest on each client's writer fd to track
/// `outbound` state. Called once per outer poll iteration so per-event
/// processing doesn't have to thread the registry through every
/// fanout helper.
/// Drain any buffered outbound, then reconcile each client's poller
/// interest with whether it still has bytes pending. Returns the ids of
/// clients whose drain attempts surfaced peer-gone errors so the caller
/// can run `process_disconnect`.
///
/// The proactive drain is load-bearing: mio uses edge-triggered epoll on
/// Linux, so when `write_or_buffer` partial-writes and buffers the tail,
/// the kernel can transition the fd writable *before* this function
/// re-registers WRITABLE interest. Without an immediate drain attempt
/// we'd register for an edge that has already passed and the buffered
/// tail would never go out — clients see truncated replies and stall.
fn reconcile_client_writable_interest(
    registry: &mio::Registry,
    state: &mut ServerState,
) -> Vec<yserver_protocol::x11::ClientId> {
    let mut to_disconnect = Vec::new();
    for (id, client) in state.clients.iter_mut() {
        if !client.outbound.is_empty() {
            match client_io::drain_outbound(client) {
                Ok(WriteOutcome::Done | WriteOutcome::WouldBlock) => {}
                Ok(WriteOutcome::Disconnect) | Err(_) => {
                    to_disconnect.push(yserver_protocol::x11::ClientId(*id));
                    continue;
                }
            }
        }
        let needs_writable = !client.outbound.is_empty();
        if needs_writable == client.watching_writable {
            continue;
        }
        let raw = std::os::fd::AsRawFd::as_raw_fd(&*client.writer.lock().unwrap());
        let interest = if needs_writable {
            Interest::READABLE | Interest::WRITABLE
        } else {
            Interest::READABLE
        };
        match registry.reregister(
            &mut SourceFd(&raw),
            client_token(yserver_protocol::x11::ClientId(*id)),
            interest,
        ) {
            Ok(()) => client.watching_writable = needs_writable,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                // fd already deregistered (disconnect path); nothing
                // to track.
            }
            Err(err) => {
                warn!("reregister client {} writable interest: {err}", id);
            }
        }
    }
    to_disconnect
}

fn handle_setup_allocate(
    state: &mut ServerState,
    id: yserver_protocol::x11::ClientId,
    response_tx: crossbeam_channel::Sender<SetupAllocateResponse>,
) {
    let _ = id;
    let response = match state.id_allocator.allocate() {
        Some((base, mask)) => SetupAllocateResponse {
            resource_id_base: base,
            resource_id_mask: mask,
            screen_width_px: state.randr.screen_width,
            screen_height_px: state.randr.screen_height,
            current_input_masks: state
                .clients
                .values()
                .filter_map(|c| c.event_masks.get(&crate::resources::ROOT_WINDOW).copied())
                .fold(0u32, |a, b| a | b),
        },
        None => SetupAllocateResponse {
            resource_id_base: 0,
            resource_id_mask: 0,
            screen_width_px: 0,
            screen_height_px: 0,
            current_input_masks: 0,
        },
    };
    let _ = response_tx.send(response);
}

/// Process a real host input event: arm/refresh/clear the auto-repeat
/// timer before fanning the event out via `backend.on_host_input`.
///
/// This is the single entry point for input that originated from a
/// user (libinput, host-X11 forwarded events, XTEST). Synthetic
/// release+press pairs emitted by [`fire_pending_repeats`] must NOT
/// route through here — they call `backend.on_host_input` directly so
/// the synthetic release doesn't re-enter [`update_repeat_state`] and
/// clear the armed key.
///
/// Public so backend-owned input dispatch paths can route through the
/// repeat-state wrapper instead of calling `backend.on_host_input`
/// directly.
pub fn handle_host_input(state: &mut ServerState, backend: &mut dyn Backend, ev: HostInputEvent) {
    update_repeat_state(state, &ev);
    backend.on_host_input(state, ev);
}

/// Whether a keycode currently auto-repeats per core
/// ChangeKeyboardControl state: the global flag gates everything,
/// then the per-key bitmap decides (Xorg `kbdfeed->ctrl.autoRepeat`
/// + `autoRepeats[]`).
fn key_auto_repeats(kc: &crate::server::KeyboardControlState, keycode: u8) -> bool {
    kc.global_auto_repeat && kc.auto_repeats[usize::from(keycode >> 3)] & (1 << (keycode & 7)) != 0
}

/// Arm / refresh / clear `state.repeat_state` from an incoming host
/// input event. X11 spec: only the most recently pressed key
/// repeats — pressing a different key replaces the armed key;
/// releasing the armed key clears it; releases of other keys are
/// ignored. Non-key events don't affect repeat state.
fn update_repeat_state(state: &mut ServerState, ev: &HostInputEvent) {
    use crate::core_loop::message::HostInputEvent::Key;
    let Key(key) = ev else {
        return;
    };
    if key.pressed {
        let synthetic = state
            .repeat_state
            .as_ref()
            .is_some_and(|r| r.event.keycode == key.keycode && r.event.pressed);
        if synthetic {
            // This is a repeat we just fired; don't reset the timer.
            return;
        }
        // ChangeKeyboardControl gate: global auto-repeat off disables
        // all repeat; otherwise the per-key bitmap decides. A
        // non-repeating press still replaces (disarms) the armed key —
        // only the most recently pressed key may repeat.
        if !key_auto_repeats(&state.keyboard_control, key.keycode) {
            state.repeat_state = None;
            return;
        }
        state.repeat_state = Some(KeyRepeatState {
            event: *key,
            next_fire: Instant::now() + REPEAT_INITIAL_DELAY,
        });
    } else if state
        .repeat_state
        .as_ref()
        .is_some_and(|r| r.event.keycode == key.keycode)
    {
        state.repeat_state = None;
    }
}

/// Fire any auto-repeat events whose `next_fire` has elapsed. Loops
/// in case the poll wake was delayed past more than one period
/// (under load) so we don't drop events. Each fire emits a
/// KeyRelease + KeyPress pair through the same host-input fan-out
/// path the original press took, matching classic X11 auto-repeat
/// (every client handles it without opting into XKB
/// DetectableAutoRepeat).
/// Returns `true` iff a repeat was actually fanned out this call. The
/// caller uses this to decide whether to poke the compositor: a call
/// that merely observes an armed-but-not-yet-due key (or disarms a
/// no-longer-repeating one) produces no events and must NOT re-dirty
/// the scene — doing so unconditionally every loop iteration while a
/// key is held (or while a phantom key is stuck armed) busy-spins the
/// compositor at the iteration rate instead of the repeat rate
/// (idle free-run, [[project_idle_compositor_redraw_loop]] cut 2a).
fn fire_pending_repeats(state: &mut ServerState, backend: &mut dyn Backend) -> bool {
    let Some(armed) = state.repeat_state else {
        return false;
    };
    // Repeat may have been disabled (ChangeKeyboardControl) after the
    // key was armed — disarm instead of firing.
    if !key_auto_repeats(&state.keyboard_control, armed.event.keycode) {
        state.repeat_state = None;
        return false;
    }
    let now = Instant::now();
    if now < armed.next_fire {
        return false;
    }
    let mut next_fire = armed.next_fire;
    while now >= next_fire {
        next_fire += REPEAT_PERIOD;
    }
    // Update the timer first so any reentrant arming during fan-out
    // doesn't double-fire.
    if let Some(s) = state.repeat_state.as_mut() {
        s.next_fire = next_fire;
    }
    let mut release = armed.event;
    release.pressed = false;
    let mut press = armed.event;
    press.pressed = true;
    backend.on_host_input(state, HostInputEvent::Key(release));
    backend.on_host_input(state, HostInputEvent::Key(press));
    true
}

/// Post-poll screen-saver evaluator. Drives idle activation and the
/// periodic Cycle event re-fire. Extracted from the outer loop body
/// so unit tests can drive it directly with pre-armed state.
///
/// Mirrors the DPMS cascade evaluator above it in the loop:
/// compute the deadline, check `now >= deadline`, drive the helper.
// nested-if matches the DPMS evaluator's shape for readability symmetry
#[allow(clippy::collapsible_if)]
pub(crate) fn evaluate_screen_saver_post_poll(state: &mut ServerState, backend: &mut dyn Backend) {
    // SS: idle activation. Mirrors Xorg WaitFor.c:441 timing.
    // `screensaver_idle_deadline` returns None when DPMS is blanked
    // (power_level != 0), so this branch is already suppressed under
    // DPMS blanking — Xorg WaitFor.c:457 parity.
    if let Some(deadline) = state.screensaver_idle_deadline() {
        if Instant::now() >= deadline {
            crate::core_loop::process_request::apply_screen_saver_transition(
                state,
                backend,
                crate::server::ScreenSaverActive::On,
                /*forced=*/ false,
            );
        }
    }
    // SS: cycle re-fire. Mirrors Xorg WaitFor.c:470-476.
    if let Some(deadline) = state.screensaver_cycle_deadline() {
        let now = Instant::now();
        if now >= deadline {
            crate::core_loop::process_request::emit_screen_saver_notify(
                state,
                crate::server::ScreenSaverActive::Cycle,
                /*forced=*/ false,
            );
            state.screensaver.next_cycle =
                Some(now + Duration::from_millis(u64::from(state.screensaver.interval_ms)));
        }
    }
}

/// Post-poll IDLETIME alarm evaluator. For each IDLETIME counter,
/// compute the current idle, walk Active alarms referencing the
/// counter, run the test-type check against the cached
/// `(last_evaluated, current)` pair, and fire via
/// `evaluate_alarms_for_counter` (which handles re-arm + emission).
/// Mirrors Xorg's `IdleTimeBlockHandler` + `IdleTimeWakeupHandler`
/// (sync.c:2647, :2750).
pub(crate) fn evaluate_idletime_alarms_post_poll(
    state: &mut ServerState,
    _backend: &mut dyn crate::backend::Backend,
) {
    use yserver_protocol::x11::sync as x11sync;
    // Suspend gate (Xorg WaitFor.c:519 unified-timer rule) — mirrors
    // `idletime_alarm_deadline`. Skip the whole evaluator when any
    // client holds XScreenSaverSuspend; otherwise an unrelated wake
    // could still fire Positive alarms mid-fullscreen-video.
    if !state.screensaver.suspend_counts.is_empty() {
        return;
    }
    const IDLETIME_COUNTERS: &[u32] = &[
        x11sync::IDLETIME_COUNTER,
        x11sync::IDLETIME_DEVICE_VCP,
        x11sync::IDLETIME_DEVICE_VCK,
    ];
    let now = Instant::now();
    for &counter in IDLETIME_COUNTERS {
        // Skip if no alarms reference this counter.
        let has_alarm = state
            .sync_alarms
            .values()
            .any(|a| a.counter == counter && a.state == x11sync::ALARM_STATE_ACTIVE);
        if !has_alarm {
            continue;
        }
        let baseline = state.idletime_baseline(counter);
        #[allow(clippy::cast_possible_truncation)]
        let current_idle = now
            .duration_since(baseline)
            .as_millis()
            .min(u128::from(u32::MAX)) as i64;
        let old_idle = state
            .idletime_last_evaluated
            .get(&counter)
            .copied()
            .unwrap_or(0);
        // Run the existing evaluator helper — it walks Active alarms,
        // calls trigger_fires, applies the Task 2 state-transition fix,
        // emits AlarmNotify, and updates wait_value.
        crate::core_loop::process_request::evaluate_alarms_for_counter(
            state,
            counter,
            old_idle,
            current_idle,
        );
        state.idletime_last_evaluated.insert(counter, current_idle);
    }
}

/// Wire a freshly-completed setup handshake into the core's bookkeeping:
///   - try_clone the stream for the writer (set non-blocking on the
///     core's clone)
///   - build the (`reader_control_tx`, `reader_control_rx`) channel
///   - install a `ClientState` for `id`
///   - drop the entry from the setup-thread teardown registry (the
///     setup thread is exiting)
///   - register the writer fd with the poller (no interest yet — I2
///     re-registers `WRITABLE` only when there's pending outbound)
///   - spawn the reader thread (the only path that produces
///     `Message::Request` for this client)
#[allow(clippy::too_many_arguments)]
fn handle_client_setup_complete(
    registry: &mio::Registry,
    sender: &CoreSender,
    setup_registry: &SetupRegistry,
    state: &mut ServerState,
    id: yserver_protocol::x11::ClientId,
    stream: UnixStream,
    resource_id_base: u32,
    resource_id_mask: u32,
    byte_order: yserver_protocol::x11::ClientByteOrder,
) -> io::Result<()> {
    use std::sync::{Arc, Mutex, atomic::AtomicU16};
    let writer = stream.try_clone()?;
    writer.set_nonblocking(true)?;
    let writer_fd = writer.as_raw_fd();

    let (reader_control_tx, reader_control_rx) = crossbeam_channel::unbounded();

    state.clients.insert(
        id.0,
        crate::server::ClientState {
            writer: Arc::new(Mutex::new(writer)),
            byte_order,
            last_sequence: Arc::new(AtomicU16::new(0)),
            resource_id_base,
            resource_id_mask,
            event_masks: std::collections::HashMap::new(),
            save_set: std::collections::HashSet::new(),
            big_requests_enabled: false,
            xi2_masks: std::collections::HashMap::new(),
            xi1_event_classes: std::collections::HashSet::new(),
            xi1_window_event_classes: std::collections::HashMap::new(),
            outbound: std::collections::VecDeque::new(),
            watching_writable: false,
            focused_window: crate::resources::ROOT_WINDOW,
            reader_control: Some(reader_control_tx),
        },
    );

    setup_registry
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&id);

    // Initial interest is READABLE — mio doesn't accept empty interest.
    // I2 reregisters WRITABLE-only when `client.outbound` becomes
    // non-empty and back to READABLE when it drains. The reader thread
    // already polls the peer fd directly, so this registration's only
    // wake-up role today is the eventual WRITABLE-on-drain edge.
    registry.register(
        &mut SourceFd(&writer_fd),
        crate::core_loop::poll_tokens::client_token(id),
        Interest::READABLE,
    )?;

    const BIG_REQUESTS_MAJOR_OPCODE: u8 = 135;
    crate::core_loop::client_reader::spawn(
        id,
        stream,
        byte_order,
        BIG_REQUESTS_MAJOR_OPCODE,
        reader_control_rx,
        sender.clone_handle(),
    )?;

    Ok(())
}

/// Drain pending accepts on the listener. For each, allocate a fresh
/// `ClientId` and spawn a setup thread that does the X11 handshake.
fn accept_pending(
    listener: &UnixListener,
    client_id_allocator: &ClientIdAllocator,
    sender: &CoreSender,
    registry: &SetupRegistry,
    auth: &Arc<AuthState>,
) {
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                let id = client_id_allocator.allocate();
                if let Err(err) = setup_thread::spawn(
                    id,
                    stream,
                    sender.clone_handle(),
                    registry.clone(),
                    auth.clone(),
                ) {
                    error!("setup thread spawn failed for client {}: {err}", id.0);
                }
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
            Err(err) => {
                warn!("accept failed: {err}");
                break;
            }
        }
    }
}

// Silence unused-import lints when the listener path is only exercised
// indirectly. Concrete uses below.
#[allow(dead_code)]
fn _hint(_: UnixStream) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn randr_change_fanout_orders_screen_then_all_crtcs_then_all_outputs() {
        use crate::server::ClientState;
        use std::{
            collections::{HashMap, HashSet, VecDeque},
            io::Read,
            os::unix::net::UnixStream,
            sync::{Arc, Mutex, atomic::AtomicU16},
        };
        use yserver_protocol::x11::{ClientByteOrder, randr as x11randr};

        let mut state = ServerState::new();
        let first = state.randr.outputs[0].clone();
        let mut second = first.clone();
        second.name = "HDMI-A-1".into();
        second.output_id = first.output_id + 10;
        second.crtc_id = first.crtc_id + 10;
        state.randr.outputs.push(second.clone());

        let (mut peer, writer) = UnixStream::pair().unwrap();
        writer.set_nonblocking(true).unwrap();
        state.clients.insert(
            7,
            ClientState {
                writer: Arc::new(Mutex::new(writer)),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(9)),
                resource_id_base: 0,
                resource_id_mask: 0,
                event_masks: HashMap::new(),
                save_set: HashSet::new(),
                big_requests_enabled: false,
                xi2_masks: HashMap::new(),
                xi1_event_classes: HashSet::new(),
                xi1_window_event_classes: HashMap::new(),
                outbound: VecDeque::new(),
                watching_writable: false,
                focused_window: crate::resources::ROOT_WINDOW,
                reader_control: None,
            },
        );
        state.randr_select_masks.insert(
            (7, crate::resources::ROOT_WINDOW),
            x11randr::NOTIFY_MASK_SCREEN_CHANGE
                | x11randr::NOTIFY_MASK_CRTC_CHANGE
                | x11randr::NOTIFY_MASK_OUTPUT_CHANGE,
        );

        emit_randr_connector_change_notifications(
            &mut state,
            &[(first.output_id, first.crtc_id, first.mode_id)],
            &[(second.output_id, second.crtc_id, second.mode_id)],
        );

        let mut wire = [0; 96];
        peer.read_exact(&mut wire).unwrap();
        const RANDR_FIRST_EVENT: u8 = 89;
        assert_eq!(wire[0] & 0x7f, RANDR_FIRST_EVENT);
        assert_eq!(wire[32] & 0x7f, RANDR_FIRST_EVENT + 1);
        assert_eq!(wire[33], x11randr::NOTIFY_CRTC_CHANGE);
        assert_eq!(wire[64] & 0x7f, RANDR_FIRST_EVENT + 1);
        assert_eq!(wire[65], x11randr::NOTIFY_OUTPUT_CHANGE);
        assert_eq!(
            u32::from_le_bytes(wire[44..48].try_into().unwrap()),
            first.crtc_id
        );
        assert_eq!(
            u32::from_le_bytes(wire[80..84].try_into().unwrap()),
            second.output_id
        );
    }

    /// The count cap alone was sized on an assumption of ~0.25 ms per
    /// request (`MAX_REQUESTS_PER_ITER`'s own doc comment). Measured on
    /// silence under MATE + adapta-nokto during a window drag, single
    /// requests reach 44-50 ms (`longest=op70:44.23ms`), so 32 of them
    /// is ~1.4 s in one iteration — during which host input and backend-fd
    /// readiness sit undelivered and the cursor visibly stalls (`gap_max`
    /// 225-360 ms). These pin the deadline half of the budget.
    #[test]
    fn budget_not_exhausted_when_both_count_and_time_remain() {
        assert!(!budget_exhausted(31, Duration::from_millis(1)));
    }

    #[test]
    fn budget_exhausted_when_count_runs_out() {
        assert!(budget_exhausted(0, Duration::from_millis(0)));
    }

    /// THE FIX: slow requests must stop the drain even with count left.
    #[test]
    fn budget_exhausted_when_deadline_passed_despite_count_remaining() {
        assert!(budget_exhausted(31, REQUEST_TIME_BUDGET));
        assert!(budget_exhausted(
            31,
            REQUEST_TIME_BUDGET + Duration::from_millis(40)
        ));
    }

    /// One 44 ms request must not authorise 31 more. This is the
    /// 1.4 s-iteration case the count-only cap allowed.
    #[test]
    fn budget_stops_after_a_single_overrunning_request() {
        let elapsed_after_one_slow_request = Duration::from_millis(44);
        assert!(budget_exhausted(
            MAX_REQUESTS_PER_ITER - 1,
            elapsed_after_one_slow_request
        ));
    }

    /// Forward progress: the budget is checked before each request with
    /// elapsed measured from the top of the iteration, so the first
    /// request of an iteration always runs. Without this the loop could
    /// livelock without draining anything.
    #[test]
    fn budget_permits_the_first_request_of_an_iteration() {
        assert!(!budget_exhausted(MAX_REQUESTS_PER_ITER, Duration::ZERO));
    }

    /// The fast path must be unchanged: 32 × 0.25 ms = 8 ms, so a
    /// well-behaved burst still exhausts on count, not on time.
    #[test]
    fn typical_fast_requests_still_exhaust_on_count_first() {
        let typical = Duration::from_micros(250);
        let elapsed_at_cap = typical * u32::try_from(MAX_REQUESTS_PER_ITER).unwrap();
        assert!(
            elapsed_at_cap <= REQUEST_TIME_BUDGET,
            "time budget must not bind before the count cap for ~0.25ms requests \
             (elapsed_at_cap={elapsed_at_cap:?}, budget={REQUEST_TIME_BUDGET:?})"
        );
    }

    #[test]
    fn loop_telemetry_attributes_burst_depth_age_and_sequence_boundary() {
        let client = yserver_protocol::x11::ClientId(17);
        let other = yserver_protocol::x11::ClientId(23);
        let mut telemetry = LoopTelemetry {
            enabled: true,
            ..LoopTelemetry::default()
        };

        telemetry.record_channel_drain(65_541, &HashMap::from([(client, 65_536), (other, 5)]));
        telemetry.record_request_accepted(client, yserver_protocol::x11::SequenceNumber(0xffff));
        telemetry.record_request_accepted(client, yserver_protocol::x11::SequenceNumber(0x0000));
        telemetry.record_deferred_push(client);
        telemetry.record_deferred_push(client);
        telemetry.record_deferred_push(other);
        telemetry.record_deferred_pop(client);
        telemetry.record_request(
            client,
            133,
            26,
            Duration::from_micros(20),
            Duration::from_millis(1_750),
        );

        assert_eq!(telemetry.channel_request_batch_max, 65_541);
        assert_eq!(telemetry.channel_client_batch_max, (17, 65_536));
        assert_eq!(telemetry.deferred_current, 2);
        assert_eq!(telemetry.max_deferred_depth, 3);
        let client_stats = &telemetry.clients[&client];
        assert_eq!(client_stats.deferred_current, 1);
        assert_eq!(client_stats.deferred_max, 2);
        assert_eq!(client_stats.accepted, 2);
        assert_eq!(client_stats.request_age_max, Duration::from_millis(1_750));
        assert_eq!(client_stats.requests_by_opcode[&(133, Some(26))], 1);
        assert_eq!(client_stats.sequence_ffff, 1);
        assert_eq!(client_stats.sequence_zero, 1);
    }

    fn deferred_request(id: u32, opcode: u8) -> DeferredRequest {
        DeferredRequest {
            id: yserver_protocol::x11::ClientId(id),
            sequence: yserver_protocol::x11::SequenceNumber(1),
            accepted_at: None,
            header: yserver_protocol::x11::RequestHeader {
                opcode,
                data: 0,
                length_units: 1,
            },
            body: Vec::new(),
            attached_fd: None,
        }
    }

    #[test]
    fn server_grab_blocks_only_non_owner_requests() {
        let mut state = ServerState::new();
        state.server_grab_owner = Some(yserver_protocol::x11::ClientId(7));

        assert!(!blocked_by_server_grab(&state, &deferred_request(7, 127)));
        assert!(blocked_by_server_grab(&state, &deferred_request(8, 127)));
        state.server_grab_owner = None;
        assert!(!blocked_by_server_grab(&state, &deferred_request(8, 127)));
    }

    #[test]
    fn released_server_grab_waiters_join_round_robin_in_waiter_order() {
        let mut deferred = FairRequestQueue::default();
        deferred.push_back(deferred_request(9, 90));
        let mut waiters = VecDeque::from([deferred_request(2, 20), deferred_request(3, 30)]);

        release_server_grab_waiters(&mut deferred, &mut waiters, &mut LoopTelemetry::default());

        let mut order = Vec::new();
        while let Some(req) = deferred.pop_front() {
            order.push((req.id.0, req.header.opcode));
        }
        assert_eq!(order, [(9, 90), (2, 20), (3, 30)]);
        assert!(waiters.is_empty());
    }

    #[test]
    fn released_server_grab_prefix_stays_ahead_of_same_client_suffix() {
        let mut deferred = FairRequestQueue::default();
        // Requests 16 and 17 were popped and parked while another client held
        // GrabServer. Requests 18 and 19 were already the remaining suffix in
        // the fair queue. The old release path appended the parked prefix and
        // dispatched 18,19,16,17, corrupting Xlib/XCB sequence tracking.
        deferred.push_back(deferred_request(66, 18));
        deferred.push_back(deferred_request(66, 19));
        deferred.push_back(deferred_request(12, 90));
        let mut waiters = VecDeque::from([deferred_request(66, 16), deferred_request(66, 17)]);

        release_server_grab_waiters(&mut deferred, &mut waiters, &mut LoopTelemetry::default());

        let mut client_66_order = Vec::new();
        while let Some(req) = deferred.pop_front() {
            if req.id.0 == 66 {
                client_66_order.push(req.header.opcode);
            }
        }
        assert_eq!(client_66_order, [16, 17, 18, 19]);
        assert!(waiters.is_empty());
    }

    #[test]
    fn fair_queue_round_robins_clients_and_preserves_each_clients_order() {
        let mut queue = FairRequestQueue::default();
        queue.push_back(deferred_request(57, 1));
        queue.push_back(deferred_request(57, 2));
        queue.push_back(deferred_request(12, 10));
        queue.push_back(deferred_request(57, 3));
        queue.push_back(deferred_request(12, 11));

        let mut order = Vec::new();
        while let Some(req) = queue.pop_front() {
            order.push((req.id.0, req.header.opcode));
        }
        assert_eq!(order, [(57, 1), (12, 10), (57, 2), (12, 11), (57, 3)]);
        assert!(queue.is_empty());
    }

    #[test]
    fn pending_backend_request_blocks_only_its_clients_fifo() {
        use yserver_protocol::x11::ClientByteOrder;

        let blocked = yserver_protocol::x11::ClientId(57);
        let other = yserver_protocol::x11::ClientId(12);
        let token = CrtcConfigToken(91);
        let mut pending = PendingBackendRequests::default();
        pending
            .park_crtc(ParkedCrtcConfig {
                client_id: blocked,
                sequence: yserver_protocol::x11::SequenceNumber(1),
                continuation: PendingCrtcConfig {
                    token,
                    completion: crate::core_loop::process_request::CrtcConfigCompletion {
                        output_id: 1,
                        set_time: 0,
                        output_bbox_before: None,
                        byte_order: ClientByteOrder::LittleEndian,
                    },
                },
                request_wire_bytes: 28,
            })
            .unwrap();

        let mut queue = FairRequestQueue::default();
        queue.push_back(deferred_request(blocked.0, 2));
        queue.push_back(deferred_request(other.0, 10));
        queue.push_back(deferred_request(blocked.0, 3));

        let runnable = queue.pop_front_unblocked(&pending).unwrap();
        assert_eq!((runnable.id, runnable.header.opcode), (other, 10));
        assert!(
            queue.pop_front_unblocked(&pending).is_none(),
            "later requests from the pending client must stay parked"
        );
        assert!(
            !queue.has_runnable(&pending),
            "a blocked-only queue must not force a zero-timeout poll spin"
        );

        pending.take_crtc(token).unwrap();
        let first = queue.pop_front_unblocked(&pending).unwrap();
        let second = queue.pop_front_unblocked(&pending).unwrap();
        assert_eq!((first.header.opcode, second.header.opcode), (2, 3));
    }

    #[test]
    fn ready_crtc_completion_replies_unblocks_and_returns_reader_credit() {
        use crate::{backend::recording::RecordingBackend, server::ClientState};
        use std::{
            collections::{HashMap, HashSet, VecDeque},
            io::Read,
            os::unix::net::UnixStream,
            sync::{Arc, Mutex, atomic::AtomicU16},
        };
        use yserver_protocol::x11::{ClientByteOrder, ClientId, SequenceNumber};

        let client_id = ClientId(57);
        let token = CrtcConfigToken(92);
        let (mut peer, writer) = UnixStream::pair().unwrap();
        writer.set_nonblocking(true).unwrap();
        let (control_tx, control_rx) = crossbeam_channel::unbounded();
        let mut state = ServerState::new();
        state.clients.insert(
            client_id.0,
            ClientState {
                writer: Arc::new(Mutex::new(writer)),
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
                focused_window: crate::resources::ROOT_WINDOW,
                reader_control: Some(control_tx),
            },
        );

        let completion = crate::core_loop::process_request::CrtcConfigCompletion {
            output_id: state.randr.outputs[0].output_id,
            set_time: 123,
            output_bbox_before: enabled_output_bbox(&state),
            byte_order: ClientByteOrder::LittleEndian,
        };
        let continuation = PendingCrtcConfig { token, completion };
        let mut pending = PendingBackendRequests::default();
        pending
            .park_crtc(ParkedCrtcConfig {
                client_id,
                sequence: SequenceNumber(9),
                continuation,
                request_wire_bytes: 28,
            })
            .unwrap();
        let mut backend = RecordingBackend::new();
        backend.ready_crtc_configs.push(token);
        backend.crtc_config_results.insert(token, Ok(false));

        drain_ready_crtc_configs(&mut state, &mut backend, &mut pending);

        let mut reply = [0_u8; 32];
        peer.read_exact(&mut reply).unwrap();
        assert_eq!((reply[0], reply[1]), (1, 0), "success reply, status=0");
        assert!(!pending.client_is_blocked(client_id));
        assert_eq!(backend.finished_crtc_configs, [token]);
        assert!(backend.cancelled_crtc_configs.is_empty());
        assert!(matches!(
            control_rx.try_recv(),
            Ok(crate::server::ReaderControl::GrantRequestBytes(28))
        ));

        // Disconnecting a still-pending client cancels its backend token and
        // never waits for the worker to finish.
        let cancel_token = CrtcConfigToken(93);
        pending
            .park_crtc(ParkedCrtcConfig {
                client_id,
                sequence: SequenceNumber(10),
                continuation: PendingCrtcConfig {
                    token: cancel_token,
                    completion,
                },
                request_wire_bytes: 28,
            })
            .unwrap();
        disconnect_with_pending_cleanup(&mut state, &mut backend, &mut pending, client_id);
        assert_eq!(backend.cancelled_crtc_configs, [cancel_token]);
        assert!(!state.clients.contains_key(&client_id.0));
    }

    /// The grab owner can be dropped by paths that carry no release check of
    /// their own — `process_disconnect` runs at two sites outside the message
    /// loop (a failed outbound write, and the writable-interest reconcile).
    /// The loop therefore re-checks once per iteration. This pins the state
    /// that made that necessary: waiters parked while `deferred_requests` is
    /// EMPTY, because the poll timeout keys off `deferred_requests` alone, so
    /// a waiter left in the side queue would strand its client until
    /// unrelated traffic happened to wake the loop.
    #[test]
    fn owner_disconnect_outside_the_message_loop_still_frees_waiters() {
        let mut state = ServerState::new();
        state.server_grab_owner = Some(yserver_protocol::x11::ClientId(1));
        let mut deferred = FairRequestQueue::default();
        let mut waiters = VecDeque::from([deferred_request(2, 20)]);

        // While the grab is held, a waiter must stay parked.
        if state.server_grab_owner.is_none() {
            release_server_grab_waiters(&mut deferred, &mut waiters, &mut LoopTelemetry::default());
        }
        assert_eq!(waiters.len(), 1, "grab still held: waiter stays parked");
        assert!(deferred.is_empty(), "nothing runnable while grabbed");

        // Owner reaped by a path with no release check of its own (this is
        // what process_disconnect does at run.rs' two non-message sites).
        state.server_grab_owner = None;

        // The per-iteration re-check must pick it up on its own.
        if state.server_grab_owner.is_none() {
            release_server_grab_waiters(&mut deferred, &mut waiters, &mut LoopTelemetry::default());
        }
        assert!(waiters.is_empty(), "released grab must free its waiters");
        assert_eq!(deferred.pop_front().map(|r| r.id.0), Some(2));
        assert!(deferred.is_empty());
    }
    use crate::{
        backend::recording::RecordingBackend,
        core_loop::sender::channel,
        server::{ScreenSaverActive, ServerState},
    };
    use std::time::Duration;

    /// I5 test: `reconcile_client_writable_interest` toggles a client's
    /// `watching_writable` flag in lock-step with `outbound`'s emptiness,
    /// and is a no-op when nothing changed. Tests against a real
    /// `mio::Registry` so the reregister error path is also exercised.
    #[test]
    fn reconcile_writable_interest_tracks_outbound_state() {
        use crate::server::ClientState;
        use mio::{Interest, Poll, unix::SourceFd};
        use std::{
            collections::{HashMap, HashSet, VecDeque},
            io::{Read, Write},
            os::{fd::AsRawFd, unix::net::UnixStream},
            sync::{Arc, Mutex, atomic::AtomicU16},
        };
        use yserver_protocol::x11::{ClientByteOrder, ClientId as Cid};

        let poll = Poll::new().unwrap();
        // We just need a real fd registered with the poller.
        let (mut peer, writer) = UnixStream::pair().unwrap();
        writer.set_nonblocking(true).unwrap();
        let writer_arc = Arc::new(Mutex::new(writer));
        let raw = writer_arc.lock().unwrap().as_raw_fd();
        let token = client_token(Cid(7));
        poll.registry()
            .register(&mut SourceFd(&raw), token, Interest::READABLE)
            .unwrap();

        let mut state = ServerState::new();
        state.clients.insert(
            7,
            ClientState {
                writer: writer_arc,
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0)),
                resource_id_base: 0,
                resource_id_mask: 0,
                event_masks: HashMap::new(),
                save_set: HashSet::new(),
                big_requests_enabled: false,
                xi2_masks: HashMap::new(),
                xi1_event_classes: HashSet::new(),
                xi1_window_event_classes: HashMap::new(),
                outbound: VecDeque::new(),
                watching_writable: false,
                focused_window: crate::resources::ROOT_WINDOW,
                reader_control: None,
            },
        );

        // outbound is empty, watching_writable is false → no-op.
        let disc = reconcile_client_writable_interest(poll.registry(), &mut state);
        assert!(disc.is_empty());
        assert!(!state.clients[&7].watching_writable);

        // Outbound becomes non-empty AND the peer doesn't read → reconcile's
        // proactive drain attempt cannot empty it, so watching_writable
        // flips on.
        //
        // Fill the kernel buffer first so any drain attempt returns
        // WouldBlock instead of writing through to `peer`.
        //
        // Fill until the kernel actually reports WouldBlock rather than
        // writing one fixed-size buffer: the capacity is a tunable the
        // test cannot assume. On the Linux box this was reported from,
        // `net.core.wmem_default` was the stock 212992 (~228 KiB
        // absorbed), so a single 256 KiB write cleared it by only ~11%
        // and was swallowed whole where that sysctl had been raised;
        // other platforms size it differently again. The drain
        // inside reconcile then succeeded, `outbound` emptied, and
        // `watching_writable` never flipped on — #107.
        //
        // SO_SNDBUF is also raised, best-effort, so a machine with the
        // stock sysctl still exercises the large-buffer case. It is only
        // an amplifier: the kernel may clamp it (Linux) or refuse it
        // (FreeBSD ENOBUFS), and the loop below is correct either way,
        // so the result is deliberately not asserted on.
        unsafe {
            let sz: libc::c_int = 512 * 1024;
            libc::setsockopt(
                raw,
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                std::ptr::addr_of!(sz).cast(),
                u32::try_from(std::mem::size_of::<libc::c_int>()).unwrap(),
            );
        }
        let chunk = vec![0xABu8; 64 * 1024];
        let mut filled = false;
        // Bounded so a kernel that never reports WouldBlock fails the
        // assertion below instead of spinning.
        for _ in 0..1024 {
            match state.clients[&7].writer.lock().unwrap().write(&chunk) {
                Ok(0) => break,
                Ok(_) => {}
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    filled = true;
                    break;
                }
                // Anything else (EPIPE, EINTR…) is a broken fixture, not
                // a full buffer — surface it rather than folding it into
                // the generic "never reported WouldBlock" failure.
                Err(err) => panic!("unexpected error filling the send buffer: {err}"),
            }
        }
        assert!(filled, "kernel send buffer never reported WouldBlock");
        state
            .clients
            .get_mut(&7)
            .unwrap()
            .outbound
            .extend([1u8, 2, 3]);
        let disc = reconcile_client_writable_interest(poll.registry(), &mut state);
        assert!(disc.is_empty());
        assert!(state.clients[&7].watching_writable);

        // Peer drains → kernel buffer empties → drain succeeds inside reconcile,
        // outbound goes empty, watching_writable flips off.
        //
        // Read until WouldBlock for the same reason the fill loops: one
        // read of a fixed size is not guaranteed to empty the queue, and
        // leftover bytes would make reconcile's drain block again and
        // leave `outbound` non-empty.
        let mut sink = vec![0u8; 64 * 1024];
        peer.set_nonblocking(true).unwrap();
        loop {
            match peer.read(&mut sink) {
                Ok(0) => break,
                Ok(_) => {}
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(err) => panic!("unexpected error draining the peer: {err}"),
            }
        }
        let disc = reconcile_client_writable_interest(poll.registry(), &mut state);
        assert!(disc.is_empty());
        assert!(state.clients[&7].outbound.is_empty());
        assert!(!state.clients[&7].watching_writable);

        drop(peer);
    }

    /// Multi-device regression: two DRM fds of the same kind must get
    /// distinct poll tokens, and readiness on the second fd must carry
    /// that exact fd through `Backend::on_page_flip_ready`.
    #[test]
    fn drm_readiness_routes_to_the_exact_backend_fd() {
        use crate::backend::{BackendFdKind, recording::RecordingBackend};
        use std::{io::Write, os::fd::AsRawFd};

        let (poll, sender, rx) = channel().unwrap();
        let sender_for_core = sender.clone_handle();
        let (drm_reader_a, _drm_writer_a) = UnixStream::pair().unwrap();
        let (drm_reader_b, mut drm_writer_b) = UnixStream::pair().unwrap();
        let drm_fd_a = drm_reader_a.as_raw_fd();
        let drm_fd_b = drm_reader_b.as_raw_fd();
        let (ready_tx, ready_rx) = crossbeam_channel::unbounded();
        let mut backend = RecordingBackend::new().with_poll_sources(
            vec![
                (drm_fd_a, BackendFdKind::Drm),
                (drm_fd_b, BackendFdKind::Drm),
            ],
            ready_tx,
        );
        let handle = std::thread::spawn(move || {
            // `RecordingBackend` intentionally stores only raw fds; keep
            // their owners alive for the duration of the core loop.
            let _drm_readers = (drm_reader_a, drm_reader_b);
            let mut state = ServerState::new();
            let alloc = ClientIdAllocator::new();
            let result = run_core(
                poll,
                rx,
                sender_for_core,
                &mut state,
                &mut backend,
                None,
                &alloc,
                AuthState::new(None),
            );
            (result, backend)
        });

        drm_writer_b.write_all(&[1]).unwrap();
        assert_eq!(
            ready_rx.recv_timeout(Duration::from_secs(10)).unwrap(),
            drm_fd_b,
            "readiness from the second DRM source must retain its fd identity"
        );
        sender.send(Message::Shutdown).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !handle.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(handle.is_finished(), "run_core did not return");
        let (result, backend) = handle.join().unwrap();
        result.unwrap();
        let dispatched_fds = backend.page_flip_fds.lock().unwrap();
        assert!(
            !dispatched_fds.is_empty(),
            "the readable DRM source must be dispatched"
        );
        assert!(
            dispatched_fds.iter().all(|fd| *fd == drm_fd_b),
            "idle DRM fd {drm_fd_a} was dispatched: {dispatched_fds:?}",
        );
    }

    #[test]
    fn copied_scanout_completion_fd_dispatches_dedicated_hook() {
        use crate::backend::{BackendFdKind, recording::RecordingBackend};
        use std::{io::Write, os::fd::AsRawFd, sync::atomic::Ordering};

        let (poll, sender, rx) = channel().unwrap();
        let sender_for_core = sender.clone_handle();
        let (completion_reader, mut completion_writer) = UnixStream::pair().unwrap();
        let completion_fd = completion_reader.as_raw_fd();
        let (unused_page_tx, _unused_page_rx) = crossbeam_channel::unbounded();
        let (ready_tx, ready_rx) = crossbeam_channel::unbounded();
        let mut backend = RecordingBackend::new()
            .with_poll_sources(
                vec![(completion_fd, BackendFdKind::ScanoutRenderCompletion)],
                unused_page_tx,
            )
            .with_scanout_render_completion_notification(ready_tx);
        let handle = std::thread::spawn(move || {
            // `RecordingBackend` stores only the raw fd, so retain its owner
            // until `run_core` has unregistered every backend source.
            let _completion_reader = completion_reader;
            let mut state = ServerState::new();
            let alloc = ClientIdAllocator::new();
            let result = run_core(
                poll,
                rx,
                sender_for_core,
                &mut state,
                &mut backend,
                None,
                &alloc,
                AuthState::new(None),
            );
            (result, backend)
        });

        completion_writer.write_all(&[1]).unwrap();
        ready_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        sender.send(Message::Shutdown).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !handle.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(handle.is_finished(), "run_core did not return");
        let (result, backend) = handle.join().unwrap();
        result.unwrap();
        assert!(
            backend
                .scanout_render_completion_count
                .load(Ordering::Relaxed)
                >= 1,
            "readiness must dispatch the copied-scanout completion hook"
        );
        assert_eq!(
            backend.page_flip_count.load(Ordering::Relaxed),
            0,
            "copied-scanout readiness must not be misrouted as a DRM page flip"
        );
    }

    /// Regression (project_reclamation_starvation_leak): the core loop
    /// must drive backend GPU-resource reclamation (`before_block`) every
    /// iteration, INDEPENDENT of page-flips. The KMS v2 backend reaped
    /// per-op command buffers only from `on_page_flip_ready`; while the
    /// display was dark (DPMS-off / standby / VT-away) no flips occurred,
    /// so a client that kept drawing grew the engine `submitted` queue
    /// without bound until the GPU lost its device. Here we run the loop
    /// with ZERO DRM readiness events and assert `before_block` still
    /// fired — i.e. reclamation rides the dispatch loop, not scanout.
    #[test]
    fn before_block_runs_without_any_page_flip() {
        use crate::backend::recording::RecordingBackend;
        use std::sync::atomic::Ordering;

        let (poll, sender, rx) = channel().unwrap();
        let sender_for_core = sender.clone_handle();
        let mut backend = RecordingBackend::new();
        let handle = std::thread::spawn(move || {
            let mut state = ServerState::new();
            let alloc = ClientIdAllocator::new();
            let result = run_core(
                poll,
                rx,
                sender_for_core,
                &mut state,
                &mut backend,
                None,
                &alloc,
                AuthState::new(None),
            );
            (result, backend)
        });
        // No DRM readiness — only a Shutdown. The loop must still run at
        // least one iteration, calling before_block before it blocks.
        sender.send(Message::Shutdown).unwrap();
        // Generous deadline so a slow/loaded CI box can't spuriously fail:
        // the loop breaks the instant the thread finishes, so this only
        // bounds the pathological-hang case.
        let deadline = Instant::now() + Duration::from_secs(10);
        while !handle.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(handle.is_finished(), "run_core did not return");
        let (result, backend) = handle.join().unwrap();
        result.unwrap();
        assert_eq!(
            backend.page_flip_count.load(Ordering::Relaxed),
            0,
            "test must exercise the no-page-flip path",
        );
        assert!(
            backend.before_block_count.load(Ordering::Relaxed) >= 1,
            "before_block must run each iteration even with no page-flips \
             (reclamation must not be gated on scanout)",
        );
    }

    /// `handle_host_input` arms the auto-repeat timer on a real
    /// KeyPress, replaces it on a different KeyPress, and clears it
    /// on the matching KeyRelease. Regression coverage for backend-owned input
    /// dispatch paths that must not call `backend.on_host_input` directly,
    /// bypassing this wrapper.
    #[test]
    fn handle_host_input_arms_repeat_state() {
        use crate::{backend::recording::RecordingBackend, host_x11::HostKeyEvent};

        let mut state = ServerState::new();
        let mut backend = RecordingBackend::new();

        let key = |keycode: u8, pressed: bool| {
            HostInputEvent::Key(HostKeyEvent {
                pressed,
                keycode,
                time: 0,
                root_x: 0,
                root_y: 0,
                event_x: 0,
                event_y: 0,
                state: 0,
            })
        };

        // Press A → armed on A.
        handle_host_input(&mut state, &mut backend, key(38, true));
        let armed = state.repeat_state.expect("press should arm repeat_state");
        assert_eq!(armed.event.keycode, 38);
        assert!(armed.event.pressed);

        // Press B (different keycode) → replaces armed key.
        handle_host_input(&mut state, &mut backend, key(39, true));
        let armed = state.repeat_state.expect("second press should re-arm");
        assert_eq!(armed.event.keycode, 39);

        // Release A while B is armed → ignored (only the armed key's
        // release clears).
        handle_host_input(&mut state, &mut backend, key(38, false));
        assert!(
            state.repeat_state.is_some(),
            "release of non-armed key must not clear",
        );

        // Release B → clears.
        handle_host_input(&mut state, &mut backend, key(39, false));
        assert!(state.repeat_state.is_none());
    }

    /// Regression guard for the idle free-run fix (cut 2a): the caller
    /// pokes the compositor (`mark_dirty`) only when a repeat actually
    /// fires. `fire_pending_repeats` must return `false` on the
    /// every-iteration "armed but not yet due" path (else a held/stuck
    /// key busy-spins the compositor at the loop rate) and `true` only
    /// when it fans out.
    #[test]
    fn fire_pending_repeats_reports_whether_it_fired() {
        use std::time::{Duration, Instant};

        use crate::{backend::recording::RecordingBackend, host_x11::HostKeyEvent};

        let mut state = ServerState::new();
        let mut backend = RecordingBackend::new();

        // Nothing armed → no fire.
        assert!(!fire_pending_repeats(&mut state, &mut backend));

        // Arm a repeatable key (keycode 38 auto-repeats by default —
        // the sibling test relies on this too).
        handle_host_input(
            &mut state,
            &mut backend,
            HostInputEvent::Key(HostKeyEvent {
                pressed: true,
                keycode: 38,
                time: 0,
                root_x: 0,
                root_y: 0,
                event_x: 0,
                event_y: 0,
                state: 0,
            }),
        );
        assert!(state.repeat_state.is_some());

        // Freshly armed → `next_fire` is INITIAL_DELAY in the future →
        // NOT due → must return false (the idle busy-spin case).
        assert!(
            !fire_pending_repeats(&mut state, &mut backend),
            "armed-but-not-due must not report a fire",
        );

        // Force the deadline into the past → must fire.
        if let Some(s) = state.repeat_state.as_mut() {
            s.next_fire = Instant::now() - Duration::from_millis(1);
        }
        assert!(
            fire_pending_repeats(&mut state, &mut backend),
            "a due repeat must report a fire",
        );
    }

    /// Helper: a touchpad `DeviceInfo` mirroring libinput's enumeration
    /// of a Synaptics pad (matches xinput.rs's `touchpad_info`).
    #[cfg(test)]
    fn probe_touchpad_info() -> crate::core_loop::DeviceInfo {
        use crate::core_loop::message::{BoolSetting, LibinputConfigSnapshot};
        crate::core_loop::DeviceInfo {
            name: "SynPS/2 Synaptics TouchPad".into(),
            device_node: "/dev/input/event4".into(),
            sysname: "event4".into(),
            vendor_id: 0x046d,
            product_id: 0xc52f,
            is_touchpad: true,
            config: LibinputConfigSnapshot {
                tap: BoolSetting {
                    available: true,
                    current: true,
                    default: false,
                },
                natural_scroll: BoolSetting {
                    available: true,
                    current: false,
                    default: true,
                },
                dwt: BoolSetting {
                    available: true,
                    current: true,
                    default: true,
                },
                ..Default::default()
            },
        }
    }

    #[cfg(test)]
    fn slave_pointer_name(state: &ServerState) -> String {
        state
            .xi_devices
            .iter()
            .find(|d| d.id == crate::xinput::DEVICEID_SLAVE_POINTER)
            .expect("slave pointer (id 4) always present")
            .name
            .clone()
    }

    /// A backend with no on-core libinput (the trait default, and what
    /// Direct-mode / host-X11 / ynest present) is a clean no-op probe:
    /// returns 0 and leaves the static device model untouched.
    #[test]
    fn probe_input_devices_default_is_noop() {
        use crate::backend::recording::RecordingBackend;

        let mut state = ServerState::new();
        let mut backend = RecordingBackend::new(); // no probe_rounds configured
        let before = slave_pointer_name(&state);

        let seeded = backend.probe_input_devices(&mut state);

        assert_eq!(seeded, 0, "no-op probe seeds nothing");
        assert_eq!(
            slave_pointer_name(&state),
            before,
            "device 4 unchanged when nothing to probe",
        );
    }

    /// A backend whose startup probe enumerates a touchpad seeds the
    /// XI2 registry: device 4 becomes the real touchpad BEFORE the
    /// serve loop — the whole point of the Xorg-style startup probe.
    #[test]
    fn probe_input_devices_seeds_touchpad_before_loop() {
        use crate::backend::recording::RecordingBackend;

        let mut state = ServerState::new();
        let mut backend = RecordingBackend::new();
        // One non-empty round (the touchpad), then libinput goes quiet.
        backend.probe_rounds.push_back(vec![probe_touchpad_info()]);

        assert_ne!(
            slave_pointer_name(&state),
            "SynPS/2 Synaptics TouchPad",
            "precondition: device 4 starts as the generic slave pointer",
        );

        let seeded = backend.probe_input_devices(&mut state);

        assert_eq!(seeded, 1, "exactly one device seeded");
        assert_eq!(
            slave_pointer_name(&state),
            "SynPS/2 Synaptics TouchPad",
            "device 4 is the touchpad after the startup probe",
        );
    }

    /// The bounded drain TERMINATES: with libinput perpetually empty it
    /// stops after two consecutive empty rounds (not the MAX_ROUNDS
    /// ceiling), and even an adversarial always-non-empty source is
    /// capped at the ceiling rather than spinning forever.
    #[test]
    fn probe_input_devices_bounded_drain_terminates() {
        use crate::backend::recording::RecordingBackend;

        // Empty source → stops after the 2 empty rounds, well under the
        // 8-round ceiling.
        let mut state = ServerState::new();
        let mut backend = RecordingBackend::new();
        let seeded = backend.probe_input_devices(&mut state);
        assert_eq!(seeded, 0);
        assert_eq!(
            backend.probe_rounds_run.get(),
            2,
            "two consecutive empty rounds end the drain",
        );

        // Adversarial source that never goes empty → capped at the
        // MAX_ROUNDS ceiling, never unbounded.
        let mut state = ServerState::new();
        let mut backend = RecordingBackend::new();
        for _ in 0..100 {
            backend.probe_rounds.push_back(vec![probe_touchpad_info()]);
        }
        let seeded = backend.probe_input_devices(&mut state);
        assert_eq!(
            backend.probe_rounds_run.get(),
            8,
            "drain is capped at the MAX_ROUNDS ceiling",
        );
        assert_eq!(seeded, 8, "one device seeded per capped round");
    }

    #[test]
    fn shutdown_returns() {
        use crate::{
            backend::recording::RecordingBackend, core_loop::poll_tokens::ClientIdAllocator,
        };

        let (poll, sender, rx) = channel().unwrap();
        let sender_for_core = sender.clone_handle();
        let handle = std::thread::spawn(move || {
            let mut state = ServerState::new();
            let mut backend = RecordingBackend::new();
            let alloc = ClientIdAllocator::new();
            run_core(
                poll,
                rx,
                sender_for_core,
                &mut state,
                &mut backend,
                None,
                &alloc,
                AuthState::new(None),
            )
        });
        sender.send(Message::Shutdown).unwrap();
        // Bound the wait so a regression that fails to return does not
        // hang the test runner.
        for _ in 0..50 {
            if handle.is_finished() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(handle.is_finished(), "run_core did not return on Shutdown");
        handle.join().unwrap().unwrap();
    }

    #[test]
    fn evaluator_fires_idle_activation_when_deadline_elapsed() {
        let mut state = ServerState::new();
        state.screensaver.timeout_ms = 60_000;
        state.dpms.last_activity = Instant::now() - Duration::from_secs(61);
        // No client installed — emit_screen_saver_notify short-circuits
        // on empty selected_by; we're asserting state transition only.
        let mut backend = RecordingBackend::default();

        super::evaluate_screen_saver_post_poll(&mut state, &mut backend);

        assert_eq!(
            state.screensaver.active,
            ScreenSaverActive::On,
            "elapsed idle deadline must drive SS On"
        );
    }

    #[test]
    fn evaluator_fires_cycle_and_advances_next_cycle() {
        let mut state = ServerState::new();
        state.screensaver.active = ScreenSaverActive::On;
        state.screensaver.interval_ms = 60_000;
        let past = Instant::now() - Duration::from_millis(10);
        state.screensaver.next_cycle = Some(past);
        let mut backend = RecordingBackend::default();

        super::evaluate_screen_saver_post_poll(&mut state, &mut backend);

        let next = state.screensaver.next_cycle.expect("re-armed by evaluator");
        assert!(
            next > past,
            "next_cycle must advance past the prior deadline"
        );
    }

    #[test]
    fn evaluator_idle_path_skipped_while_dpms_blanked() {
        // Xorg WaitFor.c:457 — when DPMS is non-On the SS idle timer
        // is suppressed; the DPMS→SS coupling already handled it.
        let mut state = ServerState::new();
        state.screensaver.timeout_ms = 60_000;
        state.dpms.last_activity = Instant::now() - Duration::from_secs(120);
        state.dpms.power_level = 3; // Off
        let mut backend = RecordingBackend::default();

        super::evaluate_screen_saver_post_poll(&mut state, &mut backend);

        assert_eq!(
            state.screensaver.active,
            ScreenSaverActive::Off,
            "evaluator must not fire SS when DPMS is blanked"
        );
    }

    #[test]
    fn idletime_evaluator_fires_pos_transition_when_deadline_elapsed() {
        use std::time::Duration;
        use yserver_protocol::x11::{ClientId, sync as x11sync};
        let mut state = ServerState::new();
        // Pre-arm: a PositiveTransition alarm at 60_000ms, last_activity 61s ago.
        state.dpms.last_activity = std::time::Instant::now() - Duration::from_secs(61);
        let alarm_id = 0x2000;
        state.sync_alarms.insert(
            alarm_id,
            crate::server::SyncAlarm {
                owner: ClientId(1),
                counter: x11sync::IDLETIME_COUNTER,
                wait_value: 60_000,
                delta: 0,
                test_type: x11sync::TEST_POSITIVE_TRANSITION as u8,
                events: false, // skip wire delivery; assert state mutation only
                state: x11sync::ALARM_STATE_ACTIVE,
            },
        );
        let mut backend = RecordingBackend::default();

        super::evaluate_idletime_alarms_post_poll(&mut state, &mut backend);

        // PositiveTransition + delta=0 stays Active (Task 2 fix).
        let after = &state.sync_alarms[&alarm_id];
        assert_eq!(after.state, x11sync::ALARM_STATE_ACTIVE);
        // last_evaluated cache updated for global IDLETIME.
        assert!(
            state
                .idletime_last_evaluated
                .get(&x11sync::IDLETIME_COUNTER)
                .copied()
                .unwrap_or(0)
                >= 60_000,
            "last_evaluated cache should advance past the trigger value"
        );
    }

    #[test]
    fn idletime_evaluator_skips_when_no_idletime_alarms() {
        let mut state = ServerState::new();
        let mut backend = RecordingBackend::default();
        // No alarms at all — must not panic, must not insert spurious cache entries.
        super::evaluate_idletime_alarms_post_poll(&mut state, &mut backend);
        assert!(state.idletime_last_evaluated.is_empty());
    }

    /// Task 4/7 completion pacing: a completion whose gate targets a future
    /// vblank parks on drain (its wake is NOT signalled yet), and only fires —
    /// signalling `signal_present_wake` — once `fire_due_present_completions`
    /// runs at an MSC that has reached the target. Sibling to the NotifyMSC
    /// `parks_then_fires_on_vblank_advance` test.
    #[test]
    fn gated_present_completion_parks_then_fires_on_vblank_advance() {
        use crate::{
            backend::{CompletedPresentEvent, PresentWake, recording::RecordingBackend},
            server::PresentCompleteGate,
        };
        use yserver_protocol::x11::ClientId;

        const PRESENT_ID: u64 = 0x42;
        const TARGET_MSC: u64 = 200;
        const WINDOW_XID: u32 = 0x0000_0101;

        let mut state = ServerState::new();
        let mut backend = RecordingBackend::new();

        // A standalone sequence has advanced the general clock beyond the
        // target, but the completion-eligible clock is still zero. The gate
        // must park rather than taking the old already-due immediate path.
        state.present_crtc_clocks.insert(
            (0, 0),
            crate::server::PresentCrtcClock {
                epoch: 0,
                msc: 250,
                ust: 0,
                completion: crate::backend::PresentClockSample {
                    msc: 0,
                    ust: 0,
                    source: crate::backend::PresentClockSource::Immediate,
                },
            },
        );
        state.present_complete_gate.insert(
            PRESENT_ID,
            PresentCompleteGate {
                crtc_id: 0,
                crtc_epoch: 0,
                msc_offset: 0,
                effective_target_msc: TARGET_MSC,
                owner: ClientId(1),
                dst_window_xid: WINDOW_XID,
            },
        );
        // Backend reports the copy's GPU completion this iteration.
        backend
            .completed_present_events_to_drain
            .push(CompletedPresentEvent {
                client_id: ClientId(1),
                serial: 7,
                host_xid: WINDOW_XID,
                dst_host_xid: WINDOW_XID,
                options: 0,
                present_id: PRESENT_ID,
                window_generation: 0,
                crtc_id: 0,
                crtc_epoch: 0,
                msc_offset: 0,
                completion_clock: Some(crate::backend::PresentClockSample {
                    msc: 0,
                    ust: 0,
                    source: crate::backend::PresentClockSource::Immediate,
                }),
                wake: PresentWake::Pixmap { idle_fence_xid: 0 },
                completion_mode: yserver_protocol::x11::present::COMPLETE_MODE_COPY,
                emit_idle: true,
            });

        // Drain: the future-target gate parks the completion — no wake yet.
        // (RecordingBackend's completion clock is (0,0), so the drain's own
        // `fire_due_present_completions` is skipped and the park holds.)
        drain_present_completions(&mut state, &mut backend);
        assert_eq!(
            state.present_pending_complete.len(),
            1,
            "future-target completion parks on drain"
        );
        assert!(
            state.present_complete_gate.is_empty(),
            "gate consumed when the copy completes"
        );
        assert!(
            backend.signalled_present_wakes.is_empty(),
            "parked completion's wake is NOT signalled before the vblank"
        );

        // Vblank advances to the target: the parked completion fires + signals.
        // The zero sample above is a fixture-only stand-in for the initial
        // copy-completion clock. Real copy completions carry `None`, allowing
        // the later selected-domain vblank sample to stamp the paced event.
        state.present_pending_complete[0].event.completion_clock = None;
        crate::core_loop::process_request::fire_due_present_completions(
            &mut state,
            &mut backend,
            crate::backend::PresentClockSample {
                msc: TARGET_MSC,
                ust: 0x1234,
                source: crate::backend::PresentClockSource::PageFlip,
            },
        );
        assert!(
            state.present_pending_complete.is_empty(),
            "parked completion released once its target MSC is reached"
        );
        assert_eq!(
            backend.signalled_present_wakes,
            vec![PRESENT_ID],
            "signal_present_wake fires exactly once at the target vblank"
        );
    }

    /// The gate-absent / already-reached path must NOT park: the completion
    /// fires immediately on drain and signals its wake once.
    #[test]
    fn ungated_present_completion_fires_immediately_without_parking() {
        use crate::backend::{CompletedPresentEvent, PresentWake, recording::RecordingBackend};
        use yserver_protocol::x11::ClientId;

        const PRESENT_ID: u64 = 0x43;

        let mut state = ServerState::new();
        let mut backend = RecordingBackend::new();
        // No gate recorded for this present_id → complete-now arm.
        backend
            .completed_present_events_to_drain
            .push(CompletedPresentEvent {
                client_id: ClientId(1),
                serial: 8,
                host_xid: 0x0000_0202,
                dst_host_xid: 0x0000_0202,
                options: 0,
                present_id: PRESENT_ID,
                window_generation: 0,
                crtc_id: 0,
                crtc_epoch: 0,
                msc_offset: 0,
                completion_clock: None,
                wake: PresentWake::Pixmap { idle_fence_xid: 0 },
                completion_mode: yserver_protocol::x11::present::COMPLETE_MODE_COPY,
                emit_idle: true,
            });

        drain_present_completions(&mut state, &mut backend);
        assert!(
            state.present_pending_complete.is_empty(),
            "gate-absent completion does not park"
        );
        assert_eq!(
            backend.signalled_present_wakes,
            vec![PRESENT_ID],
            "gate-absent completion signals its wake immediately"
        );
    }

    /// Spec §"Ordered completion delivery" item 2: the due arm of the
    /// drain (a completion whose gate is already satisfied when its GPU
    /// fence retires) must route through `present_pending_complete`
    /// instead of firing inline via `complete_present_with_clock` — so
    /// that the per-window sweep in `fire_due_present_completions`, not
    /// raw arrival order, decides delivery order against anything else
    /// already parked for the same window. Pre-fix this fired here
    /// directly and never touched the queue at all.
    #[test]
    fn due_gate_arm_pushes_into_queue_instead_of_firing_inline() {
        use crate::{
            backend::{CompletedPresentEvent, PresentWake, recording::RecordingBackend},
            server::PresentCompleteGate,
        };
        use yserver_protocol::x11::ClientId;

        const PRESENT_ID: u64 = 0x44;
        const WINDOW_XID: u32 = 0x0000_0303;

        let mut state = ServerState::new();
        let mut backend = RecordingBackend::new();
        // effective_target_msc 0 is already satisfied against
        // RecordingBackend's default (0, 0) completion clock — the "due"
        // arm, not the "still future" park arm.
        state.present_complete_gate.insert(
            PRESENT_ID,
            PresentCompleteGate {
                crtc_id: 0,
                crtc_epoch: 0,
                msc_offset: 0,
                effective_target_msc: 0,
                owner: ClientId(1),
                dst_window_xid: WINDOW_XID,
            },
        );
        backend
            .completed_present_events_to_drain
            .push(CompletedPresentEvent {
                client_id: ClientId(1),
                serial: 9,
                host_xid: WINDOW_XID,
                dst_host_xid: WINDOW_XID,
                options: 0,
                present_id: PRESENT_ID,
                window_generation: 0,
                crtc_id: 0,
                crtc_epoch: 0,
                msc_offset: 0,
                completion_clock: None,
                wake: PresentWake::Pixmap { idle_fence_xid: 0 },
                completion_mode: yserver_protocol::x11::present::COMPLETE_MODE_COPY,
                emit_idle: true,
            });

        drain_present_completions(&mut state, &mut backend);
        assert!(
            state.present_complete_gate.is_empty(),
            "gate consumed when the copy completes"
        );
        assert_eq!(
            state.present_pending_complete.len(),
            1,
            "the due arm pushes into the ordered queue rather than firing \
             inline (RecordingBackend's zero completion clock means the \
             same-pass sweep can't drain it yet, which is fine — this test \
             only pins that it did NOT fire inline)"
        );
        assert!(
            backend.signalled_present_wakes.is_empty(),
            "must not signal the wake inline — delivery is the sweep's job"
        );
    }

    /// Spec round-4 F6: async presents (`effective_target_msc == None`,
    /// no gate entry — the drain's gate-absent arm) sit outside the
    /// per-window hold-back entirely and complete immediately, even ahead
    /// of an earlier-arrived, still-unresolved synced present parked for
    /// the same window. This is Xorg-parity and pre-existing; documented
    /// so it isn't mistaken for a hold-back bug.
    #[test]
    fn async_present_completion_bypasses_per_window_hold_back() {
        use crate::{
            backend::{CompletedPresentEvent, PresentWake, recording::RecordingBackend},
            server::PendingPresentComplete,
        };
        use yserver_protocol::x11::{ClientId, present as x11present};

        const WINDOW_XID: u32 = 0x0000_0606;
        const PARKED_SMALLER_ID: u64 = 5;
        const ASYNC_ID: u64 = 6;

        let mut state = ServerState::new();
        let mut backend = RecordingBackend::new();

        // An earlier, smaller-id synced present is still parked/unresolved
        // for this window.
        state.present_pending_complete.push(PendingPresentComplete {
            event: CompletedPresentEvent {
                client_id: ClientId(1),
                serial: 1,
                host_xid: WINDOW_XID,
                dst_host_xid: WINDOW_XID,
                options: 0,
                present_id: PARKED_SMALLER_ID,
                window_generation: 0,
                crtc_id: 0,
                crtc_epoch: 0,
                msc_offset: 0,
                completion_clock: None,
                wake: PresentWake::Pixmap { idle_fence_xid: 0 },
                completion_mode: yserver_protocol::x11::present::COMPLETE_MODE_COPY,
                emit_idle: true,
            },
            effective_target_msc: 0,
            mode: x11present::COMPLETE_MODE_COPY,
            emit_idle: true,
        });

        // A later async completion for the same window: no gate entry at all.
        backend
            .completed_present_events_to_drain
            .push(CompletedPresentEvent {
                client_id: ClientId(1),
                serial: 2,
                host_xid: WINDOW_XID,
                dst_host_xid: WINDOW_XID,
                options: 0,
                present_id: ASYNC_ID,
                window_generation: 0,
                crtc_id: 0,
                crtc_epoch: 0,
                msc_offset: 0,
                completion_clock: None,
                wake: PresentWake::Pixmap { idle_fence_xid: 0 },
                completion_mode: yserver_protocol::x11::present::COMPLETE_MODE_COPY,
                emit_idle: true,
            });

        drain_present_completions(&mut state, &mut backend);
        assert_eq!(
            backend.signalled_present_wakes,
            vec![ASYNC_ID],
            "the async completion fires immediately, bypassing hold-back"
        );
        assert_eq!(
            state.present_pending_complete.len(),
            1,
            "the earlier parked synced present is untouched by the async path"
        );
    }

    /// Review fix (post-Task-6): the async exemption above covers async
    /// firing ahead of a still-HELD entry — it must NOT cover an async
    /// completion overtaking a gated Copy that is already due and simply
    /// hasn't been swept yet. In one `drain_present_completions` pass,
    /// `completed = [X(gated, due, id=5), Y(async, id=7)]` for the SAME
    /// window: X's due-arm pushes into the queue (per Task 6 Step 3), then
    /// Y's gate-absent arm used to fire straight through, landing before
    /// X's post-loop sweep — id=7 then id=5, a backward serial that
    /// didn't exist pre-Task-6 (eager firing kept them in arrival order).
    /// Fixed by flushing due-and-unblocked entries from the queue before
    /// the async arm fires inline, so id=5 goes out first.
    #[test]
    fn gated_due_copy_delivers_before_same_drain_async_completion() {
        use crate::{
            backend::{CompletedPresentEvent, PresentWake, recording::RecordingBackend},
            server::PresentCompleteGate,
        };
        use yserver_protocol::x11::ClientId;

        const WINDOW_XID: u32 = 0x0000_0808;
        const GATED_ID: u64 = 5;
        const ASYNC_ID: u64 = 7;
        const TARGET_MSC: u64 = 300;

        let mut state = ServerState::new();
        let mut backend = RecordingBackend::new();
        // A real, nonzero completion clock this time (RecordingBackend
        // defaults to (0,0), which would make fire_due_present_completions
        // bail before ever reaching the ordering bug this test pins).
        backend.present_ust_msc = (TARGET_MSC, 0xABCD);

        state.present_complete_gate.insert(
            GATED_ID,
            PresentCompleteGate {
                crtc_id: 0,
                crtc_epoch: 0,
                msc_offset: 0,
                effective_target_msc: TARGET_MSC,
                owner: ClientId(1),
                dst_window_xid: WINDOW_XID,
            },
        );
        // Arrival order within one drain: the gated-due entry first, the
        // async one second — matching the reviewer's vector.
        backend
            .completed_present_events_to_drain
            .push(CompletedPresentEvent {
                client_id: ClientId(1),
                serial: 1,
                host_xid: WINDOW_XID,
                dst_host_xid: WINDOW_XID,
                options: 0,
                present_id: GATED_ID,
                window_generation: 0,
                crtc_id: 0,
                crtc_epoch: 0,
                msc_offset: 0,
                completion_clock: Some(crate::backend::PresentClockSample {
                    msc: TARGET_MSC,
                    ust: 0xABCD,
                    source: crate::backend::PresentClockSource::PageFlip,
                }),
                wake: PresentWake::Pixmap { idle_fence_xid: 0 },
                completion_mode: yserver_protocol::x11::present::COMPLETE_MODE_COPY,
                emit_idle: true,
            });
        backend
            .completed_present_events_to_drain
            .push(CompletedPresentEvent {
                client_id: ClientId(1),
                serial: 2,
                host_xid: WINDOW_XID,
                dst_host_xid: WINDOW_XID,
                options: 0,
                present_id: ASYNC_ID,
                window_generation: 0,
                crtc_id: 0,
                crtc_epoch: 0,
                msc_offset: 0,
                completion_clock: None,
                wake: PresentWake::Pixmap { idle_fence_xid: 0 },
                completion_mode: yserver_protocol::x11::present::COMPLETE_MODE_COPY,
                emit_idle: true,
            });

        drain_present_completions(&mut state, &mut backend);
        assert_eq!(
            backend.signalled_present_wakes,
            vec![GATED_ID, ASYNC_ID],
            "Copy(5) must deliver before async(7) in the same drain pass — \
             pre-fix this reads [7, 5]"
        );
    }

    /// Task 4 (spec "Loop-order and clock contract" item 1): the tail's
    /// drain must run BEFORE `maybe_composite`, so a present executed in
    /// this iteration's drain is visible to this iteration's compose
    /// instead of slipping a full period behind unrelated damage. Drives
    /// both halves of the drain — a source-ready `PresentPixmap` copy
    /// (whose execution marks dirty, `process_request.rs:8714`) and a
    /// canned GPU-completion event (`drain_completed_present_events`) —
    /// and asserts both are recorded before `maybe_composite` in
    /// `RecordingBackend`'s call log. Fails against the pre-Task-4 order
    /// (`maybe_composite` before the drain).
    #[test]
    fn run_iteration_tail_drains_present_work_before_compositing() {
        use crate::{
            backend::{
                CompletedPresentEvent, PresentWake,
                recording::{RecordedCall, RecordingBackend},
            },
            server::{PendingPresentEntry, PendingPresentPixmap, PendingPresentRequest},
        };
        use yserver_protocol::x11::{ClientId, present::PixmapRequest};

        const WAIT_ID: u64 = 7;
        const DEFERRED_PRESENT_ID: u64 = 0x77;
        const PRESENT_ID: u64 = 0x99;
        const WINDOW_XID: u32 = 0x0000_0303;

        let mut state = ServerState::new();
        let mut backend = RecordingBackend::new();

        // A source-ready PresentPixmap copy: draining it runs
        // `execute_present_pixmap_copy` then `mark_dirty` — the real
        // production link between "the drain executed something" and
        // "compose must see it this iteration".
        state
            .present_wait_to_id
            .insert(WAIT_ID, DEFERRED_PRESENT_ID);
        state.present_pending_exec.insert(
            DEFERRED_PRESENT_ID,
            PendingPresentEntry {
                pending: PendingPresentPixmap {
                    origin: None,
                    client_id: ClientId(1),
                    request: PendingPresentRequest::Pixmap(PixmapRequest {
                        window: WINDOW_XID,
                        pixmap: 0x304,
                        serial: 9,
                        valid: 0,
                        update: 0,
                        x_off: 0,
                        y_off: 0,
                        target_crtc: 0,
                        wait_fence: 0,
                        idle_fence: 0,
                        options: 0,
                        target_msc: 0,
                        divisor: 0,
                        remainder: 0,
                        notifies: Vec::new(),
                    }),
                    wake: crate::backend::PresentWake::Pixmap { idle_fence_xid: 0 },
                    masked_options: 0,
                    src_host_xid: 0x0040_0304,
                    paint_dst_host_xid: 0x0040_0303,
                    completion_dst_host_xid: 0x0040_0303,
                    src_width: 10,
                    src_height: 10,
                    update_rects: None,
                    present_id: DEFERRED_PRESENT_ID,
                    window_generation: 0,
                    crtc_id: 0,
                    crtc_epoch: 0,
                    msc_offset: 0,
                    effective_target_msc: None,
                },
                source_ready: false,
                wait_id: Some(WAIT_ID),
                pin: None,
            },
        );
        backend.ready_present_source_waits.push(WAIT_ID);

        // A canned GPU-completion event: exercises the second drain half.
        backend
            .completed_present_events_to_drain
            .push(CompletedPresentEvent {
                client_id: ClientId(1),
                serial: 9,
                host_xid: WINDOW_XID,
                dst_host_xid: WINDOW_XID,
                options: 0,
                present_id: PRESENT_ID,
                window_generation: 0,
                crtc_id: 0,
                crtc_epoch: 0,
                msc_offset: 0,
                completion_clock: None,
                wake: PresentWake::Pixmap { idle_fence_xid: 0 },
                completion_mode: yserver_protocol::x11::present::COMPLETE_MODE_COPY,
                emit_idle: true,
            });

        run_iteration_tail(&mut state, &mut backend);

        let calls = backend.calls();
        let mark_dirty_idx = calls
            .iter()
            .position(|c| matches!(c, RecordedCall::MarkDirty))
            .expect("source-ready copy executed and marked dirty");
        let drain_completed_idx = calls
            .iter()
            .position(|c| matches!(c, RecordedCall::DrainCompletedPresentEvents))
            .expect("completed present events drained");
        let composite_idx = calls
            .iter()
            .position(|c| matches!(c, RecordedCall::MaybeComposite))
            .expect("maybe_composite invoked");

        assert!(
            mark_dirty_idx < composite_idx,
            "drain's mark_dirty ({mark_dirty_idx}) must precede maybe_composite ({composite_idx})"
        );
        assert!(
            drain_completed_idx < composite_idx,
            "drain_completed_present_events ({drain_completed_idx}) must precede maybe_composite ({composite_idx})"
        );
    }

    /// Fix-forward: idle-vblank arming for a parked Present completion must
    /// run AFTER `maybe_composite`, not inside the pre-compose drain.
    /// `mark_dirty()` alone (no output damage) makes a real KMS compose
    /// return `Skipped(EmptyDamage)`, which still clears
    /// `scene_wants_compose()` — so `present_completion_is_idle()` only
    /// reports idle post-compose. Arming pre-compose would see a dirty
    /// scene and arm nothing, stranding the parked `CompleteNotify` with no
    /// fd left to wake `poll`. Fails against the arm folded into
    /// `drain_present_completions` (landing before `MaybeComposite`).
    #[test]
    fn run_iteration_tail_arms_present_completion_idle_vblanks_after_compositing() {
        use crate::{
            backend::{
                CompletedPresentEvent, PresentWake,
                recording::{RecordedCall, RecordingBackend},
            },
            server::PendingPresentComplete,
        };
        use yserver_protocol::x11::ClientId;

        const PARKED_PRESENT_ID: u64 = 0x55;
        const DRAINED_PRESENT_ID: u64 = 0x56;
        const WINDOW_XID: u32 = 0x0000_0505;

        let mut state = ServerState::new();
        let mut backend = RecordingBackend::new();

        // Something for the arm to arm: a completion already parked on a
        // future target MSC.
        state.present_pending_complete.push(PendingPresentComplete {
            event: CompletedPresentEvent {
                client_id: ClientId(1),
                serial: 10,
                host_xid: WINDOW_XID,
                dst_host_xid: WINDOW_XID,
                options: 0,
                present_id: PARKED_PRESENT_ID,
                window_generation: 0,
                crtc_id: 0,
                crtc_epoch: 0,
                msc_offset: 0,
                completion_clock: None,
                wake: PresentWake::Pixmap { idle_fence_xid: 0 },
                completion_mode: yserver_protocol::x11::present::COMPLETE_MODE_COPY,
                emit_idle: true,
            },
            effective_target_msc: 500,
            mode: yserver_protocol::x11::present::COMPLETE_MODE_COPY,
            emit_idle: true,
        });

        // A canned GPU-completion event so the drain half also runs.
        backend
            .completed_present_events_to_drain
            .push(CompletedPresentEvent {
                client_id: ClientId(1),
                serial: 11,
                host_xid: WINDOW_XID,
                dst_host_xid: WINDOW_XID,
                options: 0,
                present_id: DRAINED_PRESENT_ID,
                window_generation: 0,
                crtc_id: 0,
                crtc_epoch: 0,
                msc_offset: 0,
                completion_clock: None,
                wake: PresentWake::Pixmap { idle_fence_xid: 0 },
                completion_mode: yserver_protocol::x11::present::COMPLETE_MODE_COPY,
                emit_idle: true,
            });

        run_iteration_tail(&mut state, &mut backend);

        let calls = backend.calls();
        let drain_completed_idx = calls
            .iter()
            .position(|c| matches!(c, RecordedCall::DrainCompletedPresentEvents))
            .expect("completed present events drained");
        let composite_idx = calls
            .iter()
            .position(|c| matches!(c, RecordedCall::MaybeComposite))
            .expect("maybe_composite invoked");
        let arm_idx = calls
            .iter()
            .position(|c| matches!(c, RecordedCall::ArmPresentCompletionIdleVblanks))
            .expect("parked completion armed an idle vblank");

        assert!(
            drain_completed_idx < composite_idx,
            "drain ({drain_completed_idx}) must still precede compose ({composite_idx})"
        );
        assert!(
            composite_idx < arm_idx,
            "arm ({arm_idx}) must run after compose ({composite_idx}), not inside the pre-compose drain"
        );
    }

    #[test]
    fn run_iteration_tail_flushes_damage_before_compositing() {
        use crate::backend::recording::{RecordedCall, RecordingBackend};

        let mut state = ServerState::new();
        let mut backend = RecordingBackend::new();
        state.damage_notify_flush_pending = true;

        run_iteration_tail(&mut state, &mut backend);

        let calls = backend.calls();
        let flush = calls
            .iter()
            .position(|call| matches!(call, RecordedCall::FlushBeforeDamageNotify))
            .expect("damage boundary flushed");
        let compose = calls
            .iter()
            .position(|call| matches!(call, RecordedCall::MaybeComposite))
            .expect("compose attempted");
        assert!(flush < compose);
        assert!(!state.damage_notify_flush_pending);
    }
}
