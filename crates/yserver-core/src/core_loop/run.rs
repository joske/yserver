//! Skeleton of the single-threaded core loop.
//!
//! B4 established the shape; D4 wired `Message::Request` against the
//! new `process_request` entry point and the lifecycle arms
//! (SetupAllocate, ClientSetupComplete, ClientDisconnected, HostInput).
//! E3/E4 (DRM + signalfd) and F2
//! (host-X11) supply the missing token arms; D5 supplies the
//! listener.

/// The armed reset trigger, driven through a live `run_core` rather than
/// against `ResetTrigger` directly (that state machine is unit-tested in
/// `core_loop::reset`). What these pin is the *wiring*: which loop events
/// arm it, which fire it, and what the boundary does afterwards.
///
/// A reset is observed through the generation counter — the boundary
/// bumps it, and nothing else in the loop does — read from the clone the
/// test keeps before handing `CoreReceiver` to the loop.
#[cfg(test)]
mod server_reset;
#[cfg(test)]
mod tests;

use std::{
    collections::{HashMap, HashSet, VecDeque},
    io,
    os::fd::{AsRawFd, OwnedFd, RawFd},
    sync::Arc,
    time::{Duration, Instant},
};

use log::{error, warn};
use mio::{Events, Interest, Poll, unix::SourceFd};

use super::{
    auth::AuthState,
    client_io::{self, WriteOutcome},
    generation::{self, Generation},
    input_inventory::InputInventory,
    message::{HostInputEvent, Message, SetupAllocateResponse, XiConfigRequest},
    poll_tokens::{
        ClientIdAllocator, NOTIFY_TOKEN, backend_token, client_token, listener_token,
        token_to_backend_index, token_to_client, token_to_listener_index,
    },
    process_request::{
        PendingCrtcConfig, PropertyDispatchError, RequestOutcome, ValidatedXiChange,
        complete_crtc_config, fire_present_configure_notify_for_window, process_request,
    },
    reset::{GenerationLocals, ResetAction, ResetPolicy, ResetTrigger, reset_generation},
    sender::{CoreReceiver, CoreSender},
    setup_thread::{self, SetupRegistry},
    xdmcp::{XDMCP_TOKEN, XdmcpOutcome, XdmcpService},
};
use crate::{
    backend::{Backend, BackendFdKind, CrtcConfigToken, HostSocketStatus},
    host_x11::HostEvent,
    server::{KeyRepeatState, ServerState},
    transport::{Listener, Transport},
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

/// Maximum time the core waits for the input thread's VT pause barrier. Xorg
/// handles XI property requests synchronously before its VT disable sequence
/// (`Xi/xiproperty.c:1156-1158`, `hw/xfree86/common/xf86Events.c:310-313`);
/// this deadline prevents a failed producer from freezing that boundary.
const VT_INPUT_PAUSE_TIMEOUT: Duration = Duration::from_millis(500);

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
pub(crate) struct LoopTelemetry {
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
    /// Last `report_export_holders` run, and whether it saw a change.
    export_holders_last: Option<Instant>,
    export_holders_changed: bool,
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

    /// Whether the export-holders report is due: enabled and 1 s since the last run.
    fn export_holders_due(&self, now: Instant) -> bool {
        self.enabled
            && self
                .export_holders_last
                .is_none_or(|last| now.saturating_duration_since(last) >= TELEMETRY_EMIT_INTERVAL)
    }

    fn note_export_holders(&mut self, now: Instant, changed: bool) {
        self.export_holders_last = Some(now);
        self.export_holders_changed = changed;
    }

    /// Re-check a second after a change so a set that settles while idle still gets logged.
    fn export_holders_deadline(&self) -> Option<Instant> {
        let last = self.export_holders_last?;
        self.export_holders_changed
            .then(|| last + TELEMETRY_EMIT_INTERVAL)
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

    /// Drop every per-client row and zero the deferred-depth gauges.
    ///
    /// Called only from the server-reset boundary, and only because the
    /// window rollover above prunes a client row when its
    /// `deferred_current` reaches zero — which happens through
    /// `record_deferred_pop`, i.e. only when a request is actually
    /// dispatched. A reset DISCARDS the queues instead, so without this
    /// the gauge would stay permanently non-zero, the dead client's row
    /// would never be pruned, and — since client ids are reused across
    /// generations — the next generation's client 7 would inherit the
    /// previous one's numbers. Diagnostics only; nothing on the
    /// protocol path reads these.
    #[allow(dead_code)] // called by `reset::reset_generation`; armed in step 5
    pub(crate) fn forget_clients(&mut self) {
        self.clients.clear();
        self.deferred_current = 0;
        self.max_deferred_depth = 0;
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
pub(crate) struct DeferredRequest {
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
pub(crate) struct PendingBackendRequests {
    crtc_by_token: HashMap<CrtcConfigToken, ParkedCrtcConfig>,
    crtc_by_client: HashMap<yserver_protocol::x11::ClientId, CrtcConfigToken>,
    xi_config_clients: HashSet<yserver_protocol::x11::ClientId>,
}

impl PendingBackendRequests {
    fn client_is_blocked(&self, client: yserver_protocol::x11::ClientId) -> bool {
        self.crtc_by_client.contains_key(&client) || self.xi_config_clients.contains(&client)
    }

    fn block_xi_config(&mut self, client: yserver_protocol::x11::ClientId) -> bool {
        self.xi_config_clients.insert(client)
    }

    fn unblock_xi_config(&mut self, client: yserver_protocol::x11::ClientId) {
        self.xi_config_clients.remove(&client);
    }

    #[cfg(test)]
    pub(crate) fn xi_config_client_is_blocked_for_test(
        &self,
        client: yserver_protocol::x11::ClientId,
    ) -> bool {
        self.xi_config_clients.contains(&client)
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

    /// Park a CRTC configuration with only the fields a lifetime test
    /// needs. The protocol continuation is inert filler: nothing here
    /// completes the request, it only has to be cancellable.
    #[cfg(test)]
    pub(crate) fn park_crtc_for_test(
        &mut self,
        client: yserver_protocol::x11::ClientId,
        token: CrtcConfigToken,
    ) -> Result<(), &'static str> {
        self.park_crtc(ParkedCrtcConfig {
            client_id: client,
            sequence: yserver_protocol::x11::SequenceNumber(1),
            continuation: PendingCrtcConfig {
                token,
                completion: crate::core_loop::process_request::CrtcConfigCompletion {
                    output_id: 1,
                    set_time: 0,
                    output_bbox_before: None,
                    byte_order: yserver_protocol::x11::ClientByteOrder::LittleEndian,
                    apply_transform: None,
                    apply_rotation: None,
                    reply: crate::core_loop::process_request::CrtcConfigReply::CrtcConfig,
                },
            },
            request_wire_bytes: 0,
        })
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.crtc_by_token.is_empty()
            && self.crtc_by_client.is_empty()
            && self.xi_config_clients.is_empty()
    }

    pub(crate) fn take_all_crtc_tokens(&mut self) -> Vec<CrtcConfigToken> {
        self.crtc_by_client.clear();
        self.crtc_by_token.drain().map(|(token, _)| token).collect()
    }
}

/// An accepted recognized property write waiting for its FIFO turn. The
/// generation is stamped by the runner, after request parsing has captured
/// only request-local wire data.
struct QueuedXiConfig {
    request: XiConfigRequest,
    generation: Generation,
    request_wire_bytes: usize,
}

struct XiConfigCompletion {
    validated: ValidatedXiChange,
    generation: Generation,
    request_wire_bytes: usize,
}

/// The one submitted input operation remains here through reset or client
/// disconnect. `protocol=None` drops generation/atom/reply metadata while the
/// source, setting and token continue to occupy the global FIFO lane.
struct XiConfigInFlight {
    token: crate::xinput::libinput_props::DeviceConfigToken,
    source: crate::xinput::InputSourceId,
    change: crate::xinput::libinput_props::DeviceConfigChange,
    cancel: crate::xinput::libinput_props::DeviceConfigCancelToken,
    protocol: Option<XiConfigCompletion>,
}

/// The client has already received BadMatch, but the input thread may have
/// crossed its pre-apply cancellation check before the timeout. Keep enough
/// information to reconcile a later confirmation without another reply.
struct TimedOutXiConfig {
    change: crate::xinput::libinput_props::DeviceConfigChange,
}

#[derive(Default)]
pub(crate) struct XiConfigLane {
    queued: VecDeque<QueuedXiConfig>,
    in_flight: Option<XiConfigInFlight>,
    timed_out: HashMap<
        (
            crate::xinput::libinput_props::DeviceConfigToken,
            crate::xinput::InputSourceId,
        ),
        TimedOutXiConfig,
    >,
    reject_unsubmitted_for_vt_release: bool,
}

impl XiConfigLane {
    #[cfg(test)]
    pub(super) fn is_empty(&self) -> bool {
        self.queued.is_empty() && self.in_flight.is_none() && self.timed_out.is_empty()
    }
}

/// Per-client FIFO queues behind a round-robin ready ring.
///
/// Request order is preserved within each client, as required by X11, while a
/// continuously busy client gets at most one request before every other ready
/// client gets a turn. Cross-client request order has no protocol meaning.
#[derive(Default)]
pub(crate) struct FairRequestQueue {
    by_client: HashMap<yserver_protocol::x11::ClientId, VecDeque<DeferredRequest>>,
    ready: VecDeque<yserver_protocol::x11::ClientId>,
    len: usize,
}

impl FairRequestQueue {
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Drop every queued request. Used only by the server-reset
    /// generation boundary: the clients that issued them are gone, and
    /// a request is meaningless in a generation whose resource ids mean
    /// something else.
    #[allow(dead_code)] // called by `reset::reset_generation`; armed in step 5
    pub(crate) fn clear(&mut self) {
        self.by_client.clear();
        self.ready.clear();
        self.len = 0;
    }

    pub(crate) fn push_back(&mut self, req: DeferredRequest) {
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

    /// Whether some queued client may run: not waiting on backend work and
    /// not suspended by a SYNC await.
    fn has_runnable(&self, pending: &PendingBackendRequests, state: &ServerState) -> bool {
        self.ready
            .iter()
            .any(|client| client_runnable(pending, state, *client))
    }

    fn pop_front_unblocked(
        &mut self,
        pending: &PendingBackendRequests,
        state: &ServerState,
    ) -> Option<DeferredRequest> {
        self.pop_front_if(|client| client_runnable(pending, state, client))
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

/// A minimal `DeferredRequest` for tests outside this module. The
/// opcode is arbitrary: the reset boundary discards these without ever
/// decoding one.
#[cfg(test)]
pub(crate) fn deferred_request_for_test(id: u32) -> DeferredRequest {
    DeferredRequest {
        id: yserver_protocol::x11::ClientId(id),
        sequence: yserver_protocol::x11::SequenceNumber(1),
        accepted_at: None,
        header: yserver_protocol::x11::RequestHeader {
            opcode: 127,
            data: 0,
            length_units: 1,
        },
        body: Vec::new(),
        attached_fd: None,
    }
}

/// A client's queued requests may be dispatched unless it waits on
/// asynchronous backend work, a SYNC `Await` / `AwaitFence` suspended it, or
/// it is the data connection of an enabled RECORD context (Xorg
/// `IgnoreClient`), or a write to it failed and the core loop has yet to
/// disconnect it (Xorg closes its fd at once, `AbortClient`). Either way
/// its requests keep their order and every other client keeps running.
fn client_runnable(
    pending: &PendingBackendRequests,
    state: &ServerState,
    client: yserver_protocol::x11::ClientId,
) -> bool {
    !pending.client_is_blocked(client)
        && !crate::core_loop::sync_await::client_is_suspended(state, client)
        && !crate::core_loop::record::client_blocks_requests(state, client)
        && !state.clients.get(&client.0).is_some_and(|c| c.write_failed)
}

fn blocked_by_server_grab(state: &ServerState, req: &DeferredRequest) -> bool {
    state.server_grab_owner.is_some_and(|owner| owner != req.id)
}

/// Restore parked server-grab requests to the fair queue without changing
/// their per-client arrival order.
pub(crate) fn release_server_grab_waiters(
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

/// Complete one client's disconnect and tell the reset trigger about
/// it.
///
/// Every path in this file that removes a client from `state.clients`
/// funnels through here — a request handler asking for a disconnect, a
/// failed `park_crtc`, an asynchronous CRTC completion, a failed
/// `ClientSetupComplete`, the reader thread's `ClientDisconnected`, a
/// failed outbound drain and the writable-interest reconcile — which is
/// what lets the trigger be an *event* rather than a state check.
pub(super) fn disconnect_with_pending_cleanup(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    pending: &mut PendingBackendRequests,
    xi_config_lane: &mut XiConfigLane,
    reset_trigger: &mut ResetTrigger,
    client: yserver_protocol::x11::ClientId,
) {
    if let Some(token) = pending.take_client_crtc(client) {
        backend.cancel_crtc_config(token);
    }
    xi_config_lane
        .queued
        .retain(|queued| queued.request.client != client);
    if xi_config_lane
        .in_flight
        .as_ref()
        .and_then(|in_flight| in_flight.protocol.as_ref())
        .is_some_and(|protocol| protocol.validated.request.client == client)
        && let Some(in_flight) = xi_config_lane.in_flight.as_mut()
    {
        in_flight.protocol = None;
    }
    pending.unblock_xi_config(client);
    crate::core_loop::process_disconnect::process_disconnect(state, backend, client);
    reset_trigger.note_client_departed(state.clients.len());
}

/// Disconnect every client a write failed on (`client_io` flags it; the
/// fan-out that hit the failure keeps iterating). Runs between dispatch
/// rounds, never inside one, and repeats because a disconnect fans out
/// events of its own that can fail further clients.
pub(super) fn disconnect_failed_writers(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    pending: &mut PendingBackendRequests,
    xi_config_lane: &mut XiConfigLane,
    reset_trigger: &mut ResetTrigger,
) {
    loop {
        let failed = client_io::failed_writers(&state.clients);
        if failed.is_empty() {
            return;
        }
        for client in failed {
            warn!(
                "client {}: output could not be written (peer gone or {} bytes unread); disconnecting",
                client.0,
                client_io::OUTBOUND_CAP
            );
            disconnect_with_pending_cleanup(
                state,
                backend,
                pending,
                xi_config_lane,
                reset_trigger,
                client,
            );
        }
    }
}

/// Disconnect every client whose output failed (RECORD data connections
/// and flagged writers), then drain and reconcile WRITABLE interest for
/// the rest. Repeats until a pass disconnects nobody: a disconnect's own
/// notifications can buffer output for, or fail, other clients.
fn settle_client_output(
    registry: &mio::Registry,
    state: &mut ServerState,
    backend: &mut dyn Backend,
    pending: &mut PendingBackendRequests,
    xi_config_lane: &mut XiConfigLane,
    reset_trigger: &mut ResetTrigger,
) {
    loop {
        // RECORD failures are queued, since that write can happen inside
        // another client's disconnect.
        let recorders = crate::core_loop::record::take_failed_recorders(state);
        let failed = if recorders.is_empty() && client_io::failed_writers(&state.clients).is_empty()
        {
            let failed = reconcile_client_writable_interest(registry, state);
            if failed.is_empty() {
                return;
            }
            failed
        } else {
            recorders
        };
        for disc_id in failed {
            disconnect_with_pending_cleanup(
                state,
                backend,
                pending,
                xi_config_lane,
                reset_trigger,
                disc_id,
            );
        }
        disconnect_failed_writers(state, backend, pending, xi_config_lane, reset_trigger);
    }
}

pub(super) fn cancel_unsubmitted_xi_configs(
    lane: &mut XiConfigLane,
    pending: &mut PendingBackendRequests,
) {
    lane.queued.clear();
    pending.xi_config_clients.clear();
    if let Some(in_flight) = lane.in_flight.as_mut() {
        in_flight.protocol = None;
    }
}

fn xi_error_for_config(
    request: &XiConfigRequest,
    error: crate::xinput::libinput_props::DeviceConfigError,
) -> PropertyDispatchError {
    use crate::xinput::libinput_props::DeviceConfigError as ConfigError;
    match error {
        ConfigError::Unsupported => PropertyDispatchError::BadMatch,
        ConfigError::Invalid => PropertyDispatchError::BadValue {
            error_value: u32::from(request.format),
        },
        ConfigError::Cancelled => PropertyDispatchError::BadMatch,
        // xf86-input-libinput returns BadMatch when its shared handle is
        // absent (`xf86libinput.c:4392-4409, 4579-4607`).
        ConfigError::SourceGone => PropertyDispatchError::BadMatch,
    }
}

fn emit_xi_config_error(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    pending: &mut PendingBackendRequests,
    lane: &mut XiConfigLane,
    reset_trigger: &mut ResetTrigger,
    request: &XiConfigRequest,
    error: PropertyDispatchError,
    generation: Generation,
    request_wire_bytes: usize,
    current_generation: Generation,
) {
    if generation == current_generation && state.clients.contains_key(&request.client.0) {
        match crate::core_loop::process_request::emit_property_dispatch_error(
            state,
            request.client,
            request.sequence,
            error,
            request.minor_opcode,
        ) {
            Ok(RequestOutcome::Disconnect(client)) => disconnect_with_pending_cleanup(
                state,
                backend,
                pending,
                lane,
                reset_trigger,
                client,
            ),
            Ok(RequestOutcome::Handled) => {}
            Ok(RequestOutcome::PendingCrtcConfig(_) | RequestOutcome::PendingXiConfig(_)) => {
                unreachable!("error emission cannot start backend work")
            }
            Err(err) => warn!("XI config error reply failed: {err}"),
        }
        grant_request_credit(state, request.client, request_wire_bytes);
    }
    pending.unblock_xi_config(request.client);
}

fn apply_confirmed_xi_config(
    state: &mut ServerState,
    input_inventory: &mut InputInventory,
    source: crate::xinput::InputSourceId,
    change: crate::xinput::libinput_props::DeviceConfigChange,
    validated: Option<&ValidatedXiChange>,
) -> Result<(u16, yserver_protocol::x11::AtomId, crate::xinput::PropWhat), PropertyDispatchError> {
    if let Some(info) = state.xi_devices.source_mut(source) {
        info.config.apply_confirmed(change);
    }
    input_inventory.update_config(source, change);
    crate::core_loop::process_request::commit_confirmed_xi_change(state, source, change, validated)
}

fn drive_xi_config_lane(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    input_inventory: &mut InputInventory,
    pending: &mut PendingBackendRequests,
    lane: &mut XiConfigLane,
    reset_trigger: &mut ResetTrigger,
    current_generation: Generation,
) {
    while lane.in_flight.is_none() {
        let Some(queued) = lane.queued.pop_front() else {
            break;
        };
        let client = queued.request.client;
        if queued.generation != current_generation || !state.clients.contains_key(&client.0) {
            pending.unblock_xi_config(client);
            continue;
        }
        let validated =
            match crate::core_loop::process_request::validate_xi_change(state, &queued.request) {
                Ok(validated) => validated,
                Err(error) => {
                    emit_xi_config_error(
                        state,
                        backend,
                        pending,
                        lane,
                        reset_trigger,
                        &queued.request,
                        error,
                        queued.generation,
                        queued.request_wire_bytes,
                        current_generation,
                    );
                    continue;
                }
            };
        let cancel = crate::xinput::libinput_props::DeviceConfigCancelToken::new();
        match backend.start_device_config(validated.source_id, validated.change, cancel.clone()) {
            Ok(crate::xinput::libinput_props::DeviceConfigStart::Applied) => {
                match apply_confirmed_xi_config(
                    state,
                    input_inventory,
                    validated.source_id,
                    validated.change,
                    Some(&validated),
                ) {
                    Ok((facet_id, property, what)) => {
                        let _ = crate::core_loop::process_request::emit_property_change(
                            state, facet_id, property, what,
                        );
                        backend.mark_dirty();
                        pending.unblock_xi_config(client);
                        grant_request_credit(state, client, queued.request_wire_bytes);
                    }
                    Err(error) => emit_xi_config_error(
                        state,
                        backend,
                        pending,
                        lane,
                        reset_trigger,
                        &queued.request,
                        error,
                        queued.generation,
                        queued.request_wire_bytes,
                        current_generation,
                    ),
                }
            }
            Ok(crate::xinput::libinput_props::DeviceConfigStart::Pending(token)) => {
                lane.in_flight = Some(XiConfigInFlight {
                    token,
                    source: validated.source_id,
                    change: validated.change,
                    cancel,
                    protocol: Some(XiConfigCompletion {
                        validated,
                        generation: queued.generation,
                        request_wire_bytes: queued.request_wire_bytes,
                    }),
                });
                break;
            }
            Err(error) => {
                let error = xi_error_for_config(&queued.request, error);
                emit_xi_config_error(
                    state,
                    backend,
                    pending,
                    lane,
                    reset_trigger,
                    &queued.request,
                    error,
                    queued.generation,
                    queued.request_wire_bytes,
                    current_generation,
                );
            }
        }
    }
}

/// Accept the owned request outcome produced by XI1/XI2 request parsing.
/// This is shared by the main request dispatcher and focused tests so the
/// production lane performs generation stamping, disabled-source rejection,
/// client blocking, dequeue-time validation, backend start, and commit.
pub(super) fn route_pending_xi_config(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    input_inventory: &mut InputInventory,
    pending: &mut PendingBackendRequests,
    lane: &mut XiConfigLane,
    reset_trigger: &mut ResetTrigger,
    request: XiConfigRequest,
    request_wire_bytes: usize,
    current_generation: Generation,
) {
    let client = request.client;
    let source_disabled = !state
        .xi_devices
        .source_has_enabled_facet(request.expected_source);
    if lane.reject_unsubmitted_for_vt_release || source_disabled {
        let error = crate::core_loop::process_request::validate_xi_change(state, &request)
            .err()
            .unwrap_or(PropertyDispatchError::BadMatch);
        emit_xi_config_error(
            state,
            backend,
            pending,
            lane,
            reset_trigger,
            &request,
            error,
            current_generation,
            request_wire_bytes,
            current_generation,
        );
    } else if !pending.block_xi_config(client) {
        log::error!(
            "client {} entered XI config lane while already blocked",
            client.0
        );
        disconnect_with_pending_cleanup(state, backend, pending, lane, reset_trigger, client);
    } else {
        lane.queued.push_back(QueuedXiConfig {
            request,
            generation: current_generation,
            request_wire_bytes,
        });
        drive_xi_config_lane(
            state,
            backend,
            input_inventory,
            pending,
            lane,
            reset_trigger,
            current_generation,
        );
    }
}

pub(super) fn cancel_queued_xi_configs_for_source(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    pending: &mut PendingBackendRequests,
    lane: &mut XiConfigLane,
    reset_trigger: &mut ResetTrigger,
    source: crate::xinput::InputSourceId,
    current_generation: Generation,
) {
    let mut retained = VecDeque::new();
    while let Some(queued) = lane.queued.pop_front() {
        if queued.request.expected_source != source {
            retained.push_back(queued);
            continue;
        }
        let error = crate::core_loop::process_request::validate_xi_change(state, &queued.request)
            .err()
            .unwrap_or_else(|| {
                if state.xi_devices.source(source).is_some() {
                    PropertyDispatchError::BadMatch
                } else {
                    PropertyDispatchError::BadDevice {
                        deviceid: queued.request.deviceid,
                    }
                }
            });
        emit_xi_config_error(
            state,
            backend,
            pending,
            lane,
            reset_trigger,
            &queued.request,
            error,
            queued.generation,
            queued.request_wire_bytes,
            current_generation,
        );
    }
    lane.queued = retained;
}

/// Fail config requests still queued in the core lane when VT release starts.
/// They have not reached an input-thread handle, so they cannot be committed;
/// the in-flight operation is completed by the input pause barrier instead.
fn fail_unsubmitted_xi_configs_for_vt_release(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    pending: &mut PendingBackendRequests,
    lane: &mut XiConfigLane,
    reset_trigger: &mut ResetTrigger,
    current_generation: Generation,
) {
    while let Some(queued) = lane.queued.pop_front() {
        let error = crate::core_loop::process_request::validate_xi_change(state, &queued.request)
            .err()
            .unwrap_or(PropertyDispatchError::BadMatch);
        emit_xi_config_error(
            state,
            backend,
            pending,
            lane,
            reset_trigger,
            &queued.request,
            error,
            queued.generation,
            queued.request_wire_bytes,
            current_generation,
        );
    }
    lane.reject_unsubmitted_for_vt_release = true;
}

/// Cancel the one submitted XI config write if the input thread cannot
/// confirm it before VT release finishes. A later Applied result is retained
/// for reconciliation, while the client's BadMatch remains final.
fn fail_in_flight_xi_config_for_vt_release(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    pending: &mut PendingBackendRequests,
    lane: &mut XiConfigLane,
    reset_trigger: &mut ResetTrigger,
    current_generation: Generation,
) {
    let Some(in_flight) = lane.in_flight.take() else {
        return;
    };
    in_flight.cancel.cancel();
    let protocol = in_flight.protocol;
    lane.timed_out.insert(
        (in_flight.token, in_flight.source),
        TimedOutXiConfig {
            change: in_flight.change,
        },
    );
    if let Some(protocol) = protocol {
        emit_xi_config_error(
            state,
            backend,
            pending,
            lane,
            reset_trigger,
            &protocol.validated.request,
            PropertyDispatchError::BadMatch,
            protocol.generation,
            protocol.request_wire_bytes,
            current_generation,
        );
    }
}

/// Dispatch the production `Message::VtRelease` lifecycle callback. Keep the
/// process-lifetime source inventory and backend device facets in the same
/// unavailable boundary before the backend releases DRM master. The callback
/// drains already-submitted config results after the input thread's FIFO pause
/// and before KMS performs the yielding operations.
/// Xorg's `ProcXIChangeProperty` runs `change_property` before returning
/// (`Xi/xiproperty.c:1156-1158`), and its VT handler calls `DisableDevice`
/// after processing held keys (`xf86Events.c:302-313`).
pub fn dispatch_vt_release(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    input_inventory: &mut InputInventory,
    before_yield: impl FnOnce(&mut ServerState, &mut dyn Backend, &mut InputInventory, bool),
) {
    if backend.vt_switching_armed() {
        input_inventory.suspend_all();
        let pause_barrier_queued = backend.begin_vt_release();
        before_yield(state, backend, input_inventory, pause_barrier_queued);
        backend.finish_vt_release(state, input_inventory);
    }
}

/// Dispatch the production `Message::VtAcquire` lifecycle callback.
pub fn dispatch_vt_acquire(state: &mut ServerState, backend: &mut dyn Backend) {
    if backend.vt_switching_armed() {
        backend.on_vt_acquire(state);
    }
}

/// Dispatch one host input event through the same lifecycle and XI-config
/// cancellation path used by `run_core`.
pub(super) fn dispatch_host_input(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    input_inventory: &mut InputInventory,
    pending: &mut PendingBackendRequests,
    xi_config_lane: &mut XiConfigLane,
    reset_trigger: &mut ResetTrigger,
    ev: HostInputEvent,
    current_generation: Generation,
) {
    // Process-lifetime bookkeeping: maintain `InputInventory` regardless of
    // generation (it always dispatches, see above) so a device lifecycle
    // transition is never missed, even mid-reset once resets exist. Nothing
    // consumes the inventory yet — purely additive.
    let config_source_lifecycle = match &ev {
        HostInputEvent::DeviceAdded(info) => {
            input_inventory.add(info.clone());
            None
        }
        HostInputEvent::DeviceSuspended { source_id, .. } => {
            input_inventory.suspend(*source_id);
            Some(*source_id)
        }
        HostInputEvent::DeviceResumed(info) => {
            input_inventory.resume(info.clone());
            None
        }
        HostInputEvent::DeviceRemoved { source_id, .. } => {
            input_inventory.remove(*source_id);
            Some(*source_id)
        }
        _ => None,
    };
    handle_host_input(state, backend, ev);
    if let Some(source_id) = config_source_lifecycle {
        cancel_queued_xi_configs_for_source(
            state,
            backend,
            pending,
            xi_config_lane,
            reset_trigger,
            source_id,
            current_generation,
        );
    }
    backend.mark_dirty();
}

fn finish_xi_config_result(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    input_inventory: &mut InputInventory,
    pending: &mut PendingBackendRequests,
    lane: &mut XiConfigLane,
    reset_trigger: &mut ResetTrigger,
    token: crate::xinput::libinput_props::DeviceConfigToken,
    source: crate::xinput::InputSourceId,
    result: Result<(), crate::xinput::libinput_props::DeviceConfigError>,
    current_generation: Generation,
) {
    let matches_submitted = lane
        .in_flight
        .as_ref()
        .is_some_and(|in_flight| in_flight.token == token && in_flight.source == source);
    if !matches_submitted {
        if let Some(timed_out) = lane.timed_out.remove(&(token, source)) {
            if result.is_ok() {
                match apply_confirmed_xi_config(
                    state,
                    input_inventory,
                    source,
                    timed_out.change,
                    None,
                ) {
                    Ok((facet_id, property, what)) => {
                        let _ = crate::core_loop::process_request::emit_property_change(
                            state, facet_id, property, what,
                        );
                        backend.mark_dirty();
                    }
                    Err(error) => {
                        warn!(
                            "late applied input config could not be reconciled token={} source={}: {error:?}",
                            token.0, source.0
                        );
                    }
                }
            }
            drive_xi_config_lane(
                state,
                backend,
                input_inventory,
                pending,
                lane,
                reset_trigger,
                current_generation,
            );
            return;
        }
        warn!(
            "discarding stale input config result token={} source={}",
            token.0, source.0
        );
        return;
    }
    let in_flight = lane
        .in_flight
        .take()
        .expect("matching submitted config exists");
    let protocol = in_flight.protocol.filter(|protocol| {
        protocol.generation == current_generation
            && state
                .clients
                .contains_key(&protocol.validated.request.client.0)
    });
    match result {
        Ok(()) => {
            // Xorg stores the property only after all check-only handlers
            // succeed (`Xi/xiproperty.c:759-801`); commit only the confirmed
            // libinput result here.
            let validated = protocol.as_ref().map(|protocol| &protocol.validated);
            match apply_confirmed_xi_config(
                state,
                input_inventory,
                in_flight.source,
                in_flight.change,
                validated,
            ) {
                Ok((facet_id, property, what)) => {
                    let _ = crate::core_loop::process_request::emit_property_change(
                        state, facet_id, property, what,
                    );
                    backend.mark_dirty();
                    if let Some(protocol) = protocol.as_ref() {
                        let client = protocol.validated.request.client;
                        pending.unblock_xi_config(client);
                        grant_request_credit(state, client, protocol.request_wire_bytes);
                    }
                }
                Err(error) => {
                    if let Some(protocol) = protocol.as_ref() {
                        let request = &protocol.validated.request;
                        emit_xi_config_error(
                            state,
                            backend,
                            pending,
                            lane,
                            reset_trigger,
                            request,
                            error,
                            protocol.generation,
                            protocol.request_wire_bytes,
                            current_generation,
                        );
                    }
                }
            }
        }
        Err(error) => {
            if let Some(protocol) = protocol.as_ref() {
                let request = &protocol.validated.request;
                emit_xi_config_error(
                    state,
                    backend,
                    pending,
                    lane,
                    reset_trigger,
                    request,
                    xi_error_for_config(request, error),
                    protocol.generation,
                    protocol.request_wire_bytes,
                    current_generation,
                );
            }
        }
    }
    drive_xi_config_lane(
        state,
        backend,
        input_inventory,
        pending,
        lane,
        reset_trigger,
        current_generation,
    );
}

/// Dispatch the process-lifetime completion message through the same runner
/// path used by `run_core`. Keeping the `Message` boundary here lets tests
/// exercise token/source matching and late-generation commits without
/// substituting a completion-only test helper.
pub(super) fn dispatch_device_config_result(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    input_inventory: &mut InputInventory,
    pending: &mut PendingBackendRequests,
    lane: &mut XiConfigLane,
    reset_trigger: &mut ResetTrigger,
    message: Message,
    current_generation: Generation,
) {
    let Message::DeviceConfigResult {
        token,
        source,
        result,
    } = message
    else {
        unreachable!("device config dispatcher received a different message")
    };
    finish_xi_config_result(
        state,
        backend,
        input_inventory,
        pending,
        lane,
        reset_trigger,
        token,
        source,
        result,
        current_generation,
    );
}

pub(crate) fn cancel_all_pending_backend_requests(
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
    input_inventory: &mut InputInventory,
    telemetry: &mut LoopTelemetry,
    pending: &mut PendingBackendRequests,
    xi_config_lane: &mut XiConfigLane,
    reset_trigger: &mut ResetTrigger,
    current_generation: Generation,
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
    let clients_before = state.clients.len();
    let outcome = process_request_inline(
        state,
        backend,
        req.id,
        req.sequence,
        req.header,
        &req.body,
        req.attached_fd,
    );
    // The one client removal that does NOT come back as
    // `RequestOutcome::Disconnect`: `KillClient` naming a resource owned
    // by a *different* client calls `process_disconnect` inline
    // (`process_request.rs`, "Force-disconnect the other client"). That
    // is still a departure and the trigger has to hear about it. Gated
    // on the count actually dropping, so this stays an event — a
    // request that removes nobody reports nothing.
    //
    // Today it can never be the departure that drains the session (the
    // killer is still connected, so the set is non-empty), but nothing
    // in the handler guarantees that, and an unreported departure is a
    // trigger that silently never fires again.
    if state.clients.len() < clients_before {
        reset_trigger.note_client_departed(state.clients.len());
    }
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
            disconnect_with_pending_cleanup(
                state,
                backend,
                pending,
                xi_config_lane,
                reset_trigger,
                disc_id,
            );
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
                disconnect_with_pending_cleanup(
                    state,
                    backend,
                    pending,
                    xi_config_lane,
                    reset_trigger,
                    req_client,
                );
            }
        }
        RequestOutcome::PendingXiConfig(request) => {
            route_pending_xi_config(
                state,
                backend,
                input_inventory,
                pending,
                xi_config_lane,
                reset_trigger,
                request,
                req_wire_bytes,
                current_generation,
            );
        }
    }
}

fn drain_pending_requests(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    input_inventory: &mut InputInventory,
    telemetry: &mut LoopTelemetry,
    pending: &mut PendingBackendRequests,
    xi_config_lane: &mut XiConfigLane,
    reset_trigger: &mut ResetTrigger,
    current_generation: Generation,
    deferred_requests: &mut FairRequestQueue,
    server_grab_waiters: &mut VecDeque<DeferredRequest>,
    requests_this_iter: &mut u32,
    request_budget: &mut usize,
    drain_start: Instant,
) {
    while !budget_exhausted(*request_budget, drain_start.elapsed()) {
        let Some(req) = deferred_requests.pop_front_unblocked(pending, state) else {
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
            input_inventory,
            telemetry,
            pending,
            xi_config_lane,
            reset_trigger,
            current_generation,
            requests_this_iter,
            request_budget,
            req,
        );
        if state.server_grab_owner.is_none() {
            release_server_grab_waiters(deferred_requests, server_grab_waiters, telemetry);
        }
    }
}

fn drain_vt_release_requests(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    input_inventory: &mut InputInventory,
    telemetry: &mut LoopTelemetry,
    pending: &mut PendingBackendRequests,
    lane: &mut XiConfigLane,
    reset_trigger: &mut ResetTrigger,
    current_generation: Generation,
    deferred_requests: &mut FairRequestQueue,
    server_grab_waiters: &mut VecDeque<DeferredRequest>,
) {
    let mut requests_this_iter = 0;
    let mut request_budget = usize::MAX;
    drain_pending_requests(
        state,
        backend,
        input_inventory,
        telemetry,
        pending,
        lane,
        reset_trigger,
        current_generation,
        deferred_requests,
        server_grab_waiters,
        &mut requests_this_iter,
        &mut request_budget,
        Instant::now(),
    );
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
    if !matches!(
        &outcome,
        RequestOutcome::PendingCrtcConfig(_) | RequestOutcome::PendingXiConfig(_)
    ) {
        if std::mem::take(&mut state.damage_notify_flush_pending) {
            backend.flush_before_damage_notify();
        }
        backend.mark_dirty();
    }
    // A request that changed the displayed cursor (DefineCursor, a grab,
    // XFIXES ChangeCursor, a map under the pointer) reports it before the
    // client's next request runs, as Xorg does from DisplayCursor.
    crate::core_loop::process_request::emit_xfixes_cursor_notify(state, backend);
    outcome
}

/// Resume every asynchronous CRTC request whose backend result is ready.
/// `finish_crtc_config` is called only while the originating client is still
/// waiting, so a late worker completion can never install a cancelled mode.
pub(crate) fn drain_ready_crtc_configs(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    pending: &mut PendingBackendRequests,
    xi_config_lane: &mut XiConfigLane,
    reset_trigger: &mut ResetTrigger,
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
                disconnect_with_pending_cleanup(
                    state,
                    backend,
                    pending,
                    xi_config_lane,
                    reset_trigger,
                    client,
                );
            }
            RequestOutcome::Handled => {
                grant_request_credit(state, parked.client_id, parked.request_wire_bytes)
            }
            RequestOutcome::PendingCrtcConfig(_) | RequestOutcome::PendingXiConfig(_) => {
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
    poll: Poll,
    rx: CoreReceiver,
    sender: CoreSender,
    state: &mut ServerState,
    backend: &mut dyn Backend,
    listeners: impl IntoIterator<Item = Listener>,
    client_id_allocator: &ClientIdAllocator,
    auth: Arc<AuthState>,
    reset_policy: ResetPolicy,
    xdmcp: Option<XdmcpService>,
) -> io::Result<()> {
    let mut input_inventory = InputInventory::new();
    run_core_with_inventory(
        poll,
        rx,
        sender,
        state,
        backend,
        listeners,
        client_id_allocator,
        auth,
        reset_policy,
        xdmcp,
        &mut input_inventory,
    )
}

fn run_core_with_inventory(
    mut poll: Poll,
    rx: CoreReceiver,
    sender: CoreSender,
    state: &mut ServerState,
    backend: &mut dyn Backend,
    listeners: impl IntoIterator<Item = Listener>,
    client_id_allocator: &ClientIdAllocator,
    auth: Arc<AuthState>,
    reset_policy: ResetPolicy,
    xdmcp: Option<XdmcpService>,
    input_inventory: &mut InputInventory,
) -> io::Result<()> {
    let setup_registry = setup_thread::make_registry();
    // The generation counter is shared with every `CoreSender`; the
    // receiver is the loop's sole handle to it, and the reset boundary
    // is the only thing that bumps it.
    let generations = rx.generation_counter();
    // The armed trigger (server-reset design, "The trigger must be
    // armed, not inferred"). False until a client becomes established,
    // and false again immediately after every reset.
    let mut reset_trigger = ResetTrigger::new(reset_policy);
    let listeners: Vec<_> = listeners
        .into_iter()
        .enumerate()
        .map(|(index, listener)| {
            listener.set_nonblocking(true)?;
            let raw = listener.as_raw_fd();
            let token = listener_token(index)
                .ok_or_else(|| io::Error::other("too many client listeners"))?;
            poll.registry()
                .register(&mut SourceFd(&raw), token, Interest::READABLE)?;
            Ok(listener)
        })
        .collect::<io::Result<_>>()?;
    let mut listener_readiness = ListenerReadiness::new(listeners.len());

    // XDMCP: one UDP socket in this same poll set, and the first query.
    // `None` unless argv named `-query`/`-broadcast`/`-indirect`, in which
    // case nothing below this point does anything at all (invariant 4).
    let mut xdmcp = xdmcp;
    if let Some(service) = xdmcp.as_mut() {
        service.register(poll.registry())?;
        // `XdmcpInit` (`xdmcp.c:600`): the query goes out before the first
        // poll, so a manager on the same host can answer within the first
        // iteration.
        service.start(&auth, rx.current_generation());
    }

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
    // device model (including a physical touchpad facet with its dynamic
    // ID) immediately. Without this the registry carries only the static
    // master and XTEST devices until
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
    // Xorg's XkbFinishInit: the keyboard's per-key auto-repeat comes from
    // the keymap.
    crate::core_loop::xkb_layout::seed_keyboard_auto_repeats(state, backend);

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
    let mut xi_config_lane = XiConfigLane::default();
    // Process-lifetime, not per-generation — see `input_inventory`'s
    // module docs. Populated below on every `HostInput` device event;
    // nothing consumes it yet (step 1 of the server-reset plan).
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
        let poll_timeout = if deferred_requests.has_runnable(&pending_backend_requests, state)
            || listener_readiness.has_pending()
        {
            Some(Duration::ZERO)
        } else {
            // Wake for the earliest deadline owned by either core
            // key-repeat or the backend (for example, a compositor
            // commit retry). `Duration::ZERO` keeps mio returning
            // immediately when a deadline is already due.
            let now = Instant::now();
            let repeat_deadline = state
                .key_repeats
                .values()
                .map(|repeat| repeat.next_fire)
                .min();
            let backend_deadline = backend.next_wakeup();
            let dpms_deadline = state.dpms_transition_deadline();
            let ss_idle_deadline = state.screensaver_idle_deadline();
            let ss_cycle_deadline = state.screensaver_cycle_deadline();
            let idletime_alarm_deadline = state.idletime_alarm_deadline();
            let sync_counter_deadline =
                crate::core_loop::sync_await::system_counter_deadline(state);
            // The XDMCP retransmission/dormancy deadline joins the existing
            // computation rather than bringing a thread of its own — the
            // state machine belongs on this loop, where it can see the
            // generation boundary directly.
            let xdmcp_deadline = xdmcp.as_ref().and_then(XdmcpService::next_deadline);
            let holders_deadline = telemetry.export_holders_deadline();
            repeat_deadline
                .into_iter()
                .chain(backend_deadline)
                .chain(holders_deadline)
                .chain(dpms_deadline)
                .chain(ss_idle_deadline)
                .chain(ss_cycle_deadline)
                .chain(idletime_alarm_deadline)
                .chain(sync_counter_deadline)
                .chain(xdmcp_deadline)
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
            input_inventory,
            &mut telemetry,
            &mut pending_backend_requests,
            &mut xi_config_lane,
            &mut reset_trigger,
            rx.current_generation(),
            &mut deferred_requests,
            &mut server_grab_waiters,
            &mut requests_this_iter,
            &mut request_budget,
            drain_start,
        );
        for ev in events.iter() {
            if let Some(index) = token_to_listener_index(ev.token()) {
                listener_readiness.mark_ready(index);
                continue;
            }
            if ev.token() == XDMCP_TOKEN {
                // `XdmcpSocketNotify` (`xdmcp.c:655`). Whatever the machine
                // decides is latched on the service and acted on at the
                // tail of this iteration, where the reset boundary lives.
                if let Some(service) = xdmcp.as_mut() {
                    service.handle_readable(&auth, rx.current_generation());
                }
                continue;
            }
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
                NOTIFY_TOKEN => {
                    let mut channel_requests = 0_usize;
                    let mut channel_requests_by_client = HashMap::new();
                    let mut channel_messages: VecDeque<_> = rx.try_recv_all_tagged().collect();
                    while let Some((msg_generation, msg)) = channel_messages.pop_front() {
                        // Discard stale session-scoped traffic at the top
                        // of dispatch (server-reset plan step 2). Inert
                        // today: the generation never advances yet, so
                        // `msg_generation` always equals the current one
                        // and every message dispatches exactly as before.
                        if !generation::should_dispatch(
                            rx.current_generation(),
                            msg_generation,
                            &msg,
                        ) {
                            continue;
                        }
                        match msg {
                            Message::Shutdown => {
                                setup_thread::shutdown_all(&setup_registry);
                                cancel_all_pending_backend_requests(
                                    backend,
                                    &mut pending_backend_requests,
                                );
                                return Ok(());
                            }
                            Message::ResetRequested => {
                                // SIGHUP under `-reset` / `-terminate`.
                                // Latched, not executed here: the
                                // boundary needs the loop-local
                                // collections that this dispatch arm
                                // has borrowed, and it runs at the tail
                                // of this same iteration.
                                log::info!("reset: SIGHUP requested a server reset");
                                reset_trigger.note_reset_requested();
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
                                generation,
                                stream,
                                resource_id_base,
                                resource_id_mask,
                                byte_order,
                                is_local,
                                fd_passing,
                                setup_reply,
                            } => {
                                if let Err(err) = handle_client_setup_complete(
                                    poll.registry(),
                                    &sender,
                                    &setup_registry,
                                    state,
                                    id,
                                    generation,
                                    stream,
                                    resource_id_base,
                                    resource_id_mask,
                                    byte_order,
                                    is_local,
                                    fd_passing,
                                    &setup_reply,
                                ) {
                                    error!("ClientSetupComplete for client {} failed: {err}", id.0);
                                    disconnect_with_pending_cleanup(
                                        state,
                                        backend,
                                        &mut pending_backend_requests,
                                        &mut xi_config_lane,
                                        &mut reset_trigger,
                                        id,
                                    );
                                } else if let Some(service) = xdmcp.as_mut() {
                                    // `XdmcpOpenDisplay` (`xdmcp.c:632`),
                                    // called from `ClientAuthorized`
                                    // (`os/connection.c:581`) for every
                                    // client that completes an authorized
                                    // setup. Immediately after
                                    // establishment, not deferred to the
                                    // tail: this is the ordering that
                                    // decides a `Refuse` racing an
                                    // in-flight setup, and the service
                                    // reports a client the race left with
                                    // no session to belong to.
                                    if service.note_client_established(
                                        id,
                                        is_local,
                                        &auth,
                                        rx.current_generation(),
                                    ) {
                                        // Orphaned by a lost `Refuse`
                                        // race. Drop it WITHOUT having
                                        // armed the reset trigger: an
                                        // orphan never counted as an
                                        // established client, so its
                                        // departure must not drain an
                                        // armed set and start a
                                        // generation mid-retry.
                                        disconnect_with_pending_cleanup(
                                            state,
                                            backend,
                                            &mut pending_backend_requests,
                                            &mut xi_config_lane,
                                            &mut reset_trigger,
                                            id,
                                        );
                                    } else {
                                        reset_trigger.note_client_established();
                                    }
                                } else {
                                    reset_trigger.note_client_established();
                                }
                            }
                            Message::ClientDisconnected { id, reason: _ } => {
                                disconnect_with_pending_cleanup(
                                    state,
                                    backend,
                                    &mut pending_backend_requests,
                                    &mut xi_config_lane,
                                    &mut reset_trigger,
                                    id,
                                );
                            }
                            Message::HostInput(ev) => {
                                if telemetry.enabled {
                                    telemetry.record_host_input(Instant::now());
                                }
                                dispatch_host_input(
                                    state,
                                    backend,
                                    input_inventory,
                                    &mut pending_backend_requests,
                                    &mut xi_config_lane,
                                    &mut reset_trigger,
                                    ev,
                                    rx.current_generation(),
                                );
                            }
                            Message::CrtcConfigReady => {
                                drain_ready_crtc_configs(
                                    state,
                                    backend,
                                    &mut pending_backend_requests,
                                    &mut xi_config_lane,
                                    &mut reset_trigger,
                                );
                            }
                            message @ Message::DeviceConfigResult { .. } => {
                                dispatch_device_config_result(
                                    state,
                                    backend,
                                    input_inventory,
                                    &mut pending_backend_requests,
                                    &mut xi_config_lane,
                                    &mut reset_trigger,
                                    message,
                                    rx.current_generation(),
                                )
                            }
                            Message::InputPaused => {
                                // Consumed only by the synchronous VT-release
                                // barrier; ignore any duplicate or stale ack.
                            }
                            Message::VtRelease => {
                                // When VT switching isn't armed there is no
                                // switch to service — ignore. (Deliberate
                                // diagnostic dumps go through DumpScanout /
                                // DumpDrawables via the Ctrl-Alt-Enter /
                                // Ctrl-Alt-F12 hotkeys, not this path.)
                                if backend.vt_switching_armed() {
                                    // Dispatch accepted requests before the
                                    // pause is queued so their writes can be
                                    // covered by the input thread's FIFO.
                                    drain_vt_release_requests(
                                        state,
                                        backend,
                                        input_inventory,
                                        &mut telemetry,
                                        &mut pending_backend_requests,
                                        &mut xi_config_lane,
                                        &mut reset_trigger,
                                        rx.current_generation(),
                                        &mut deferred_requests,
                                        &mut server_grab_waiters,
                                    );
                                    fail_unsubmitted_xi_configs_for_vt_release(
                                        state,
                                        backend,
                                        &mut pending_backend_requests,
                                        &mut xi_config_lane,
                                        &mut reset_trigger,
                                        rx.current_generation(),
                                    );
                                }
                                dispatch_vt_release(
                                    state,
                                    backend,
                                    input_inventory,
                                    |state, backend, input_inventory, pause_barrier_queued| {
                                        let mut deferred = VecDeque::new();
                                        let mut pause_acknowledged = false;
                                        if pause_barrier_queued {
                                            let deadline = Instant::now() + VT_INPUT_PAUSE_TIMEOUT;
                                            loop {
                                                let remaining = deadline
                                                    .saturating_duration_since(Instant::now());
                                                if remaining.is_zero() {
                                                    break;
                                                }
                                                // Messages after VtRelease may already be in
                                                // this batch: prefer them before receiving, or a
                                                // pre-drained pause ack could be stranded behind
                                                // the release.
                                                let message = if let Some(message) =
                                                    channel_messages.pop_front()
                                                {
                                                    Ok(message)
                                                } else {
                                                    rx.recv_tagged_timeout(remaining)
                                                };
                                                let Ok((completion_generation, completion)) =
                                                    message
                                                else {
                                                    break;
                                                };
                                                if !generation::should_dispatch(
                                                    rx.current_generation(),
                                                    completion_generation,
                                                    &completion,
                                                ) {
                                                    continue;
                                                }
                                                match completion {
                                                    message @ Message::DeviceConfigResult {
                                                        ..
                                                    } => {
                                                        dispatch_device_config_result(
                                                            state,
                                                            backend,
                                                            input_inventory,
                                                            &mut pending_backend_requests,
                                                            &mut xi_config_lane,
                                                            &mut reset_trigger,
                                                            message,
                                                            rx.current_generation(),
                                                        );
                                                        drain_vt_release_requests(
                                                            state,
                                                            backend,
                                                            input_inventory,
                                                            &mut telemetry,
                                                            &mut pending_backend_requests,
                                                            &mut xi_config_lane,
                                                            &mut reset_trigger,
                                                            rx.current_generation(),
                                                            &mut deferred_requests,
                                                            &mut server_grab_waiters,
                                                        );
                                                    }
                                                    Message::InputPaused => {
                                                        drain_vt_release_requests(
                                                            state,
                                                            backend,
                                                            input_inventory,
                                                            &mut telemetry,
                                                            &mut pending_backend_requests,
                                                            &mut xi_config_lane,
                                                            &mut reset_trigger,
                                                            rx.current_generation(),
                                                            &mut deferred_requests,
                                                            &mut server_grab_waiters,
                                                        );
                                                        pause_acknowledged = true;
                                                        break;
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
                                                            *channel_requests_by_client
                                                                .entry(id)
                                                                .or_insert(0) += 1;
                                                            telemetry.record_request_accepted(
                                                                id, sequence,
                                                            );
                                                        }
                                                        telemetry.record_deferred_push(id);
                                                        deferred_requests.push_back(
                                                            DeferredRequest {
                                                                id,
                                                                sequence,
                                                                accepted_at,
                                                                header,
                                                                body,
                                                                attached_fd,
                                                            },
                                                        );
                                                        drain_vt_release_requests(
                                                            state,
                                                            backend,
                                                            input_inventory,
                                                            &mut telemetry,
                                                            &mut pending_backend_requests,
                                                            &mut xi_config_lane,
                                                            &mut reset_trigger,
                                                            rx.current_generation(),
                                                            &mut deferred_requests,
                                                            &mut server_grab_waiters,
                                                        );
                                                    }
                                                    message => deferred.push_back((
                                                        completion_generation,
                                                        message,
                                                    )),
                                                }
                                            }
                                        }
                                        if pause_barrier_queued && !pause_acknowledged {
                                            warn!(
                                                "VT input pause barrier was not acknowledged within {:?}; failing outstanding XI config write",
                                                VT_INPUT_PAUSE_TIMEOUT,
                                            );
                                        }
                                        // This is normally empty after InputPaused because
                                        // input-thread config results precede the FIFO ack.
                                        // Still reject any residue, including when no barrier
                                        // could be queued, before KMS starts yielding the VT.
                                        fail_in_flight_xi_config_for_vt_release(
                                            state,
                                            backend,
                                            &mut pending_backend_requests,
                                            &mut xi_config_lane,
                                            &mut reset_trigger,
                                            rx.current_generation(),
                                        );
                                        channel_messages.append(&mut deferred);
                                    },
                                );
                                xi_config_lane.reject_unsubmitted_for_vt_release = false;
                            }
                            Message::VtAcquire => {
                                dispatch_vt_acquire(state, backend);
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
                        input_inventory,
                        &mut telemetry,
                        &mut pending_backend_requests,
                        &mut xi_config_lane,
                        &mut reset_trigger,
                        rx.current_generation(),
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
                                &mut xi_config_lane,
                                &mut reset_trigger,
                                client_id,
                            );
                        }
                    }
                }
            }
        }
        listener_readiness.accept_ready(
            &listeners,
            client_id_allocator,
            &sender,
            &setup_registry,
            &auth,
        );
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
        if !state.key_repeats.is_empty() {
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
        crate::core_loop::sync_await::evaluate_servertime(state);

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
        settle_client_output(
            poll.registry(),
            state,
            backend,
            &mut pending_backend_requests,
            &mut xi_config_lane,
            &mut reset_trigger,
        );

        run_iteration_tail(state, backend);
        // The tail delivers deferred input and Present events too: settle
        // again so nothing it buffered waits without WRITABLE interest.
        settle_client_output(
            poll.registry(),
            state,
            backend,
            &mut pending_backend_requests,
            &mut xi_config_lane,
            &mut reset_trigger,
        );

        // Diagnostic: per-iteration accounting + per-second telemetry
        // emit. Both are no-ops when `YSERVER_LOOP_TELEMETRY` is unset.
        if let Some(start) = iter_start {
            let now = Instant::now();
            let wall = now.saturating_duration_since(start);
            telemetry.record_iteration(requests_this_iter, wall);
            telemetry.maybe_emit(now);
            if telemetry.export_holders_due(now) {
                let core_state: &ServerState = state;
                let changed = backend.report_export_holders(&|| {
                    crate::backend::export_holders::collect_core_holders(core_state)
                });
                telemetry.note_export_holders(now, changed);
            }
        }

        // The generation boundary. Reached only from an action the
        // trigger LATCHED earlier in this iteration — a departure that
        // drained an armed generation, or a SIGHUP — never from a state
        // check here: an idle client set is indistinguishable from a
        // drained one, and a `-reset` server that inspected
        // `state.clients` would reset itself repeatedly at startup.
        //
        // The boundary runs at the tail rather than at the disconnect
        // site because it needs the loop-local collections
        // (`GenerationLocals`) that the dispatch arms have borrowed.
        // Deferring it inside one iteration is also what makes the
        // cancellation in `note_client_established` meaningful: a
        // client that completes setup after the drain, in this same
        // batch, un-drains the session before the boundary is reached.
        // XDMCP, once per iteration and immediately before the boundary:
        // fire a due timer, notice the session client leaving, and act on
        // whatever the machine decided.
        if let Some(service) = xdmcp.as_mut() {
            service.service_timer(Instant::now(), &auth, rx.current_generation());
            // `XdmcpCloseDisplay` (`xdmcp.c:642`). Ids are allocated
            // monotonically and only `disconnect_with_pending_cleanup`
            // removes an entry, so a recorded session client that is no
            // longer in `state.clients` HAS departed — this is the
            // departure, not a guess about one.
            if let Some(client) = service.live_session_client()
                && !state.clients.contains_key(&client.0)
            {
                service.note_session_client_disconnected(client, &auth, rx.current_generation());
            }
            match service.take_outcome() {
                None => {}
                Some(XdmcpOutcome::Terminate) => {
                    log::info!("xdmcp: terminating the server");
                    setup_thread::shutdown_all(&setup_registry);
                    cancel_all_pending_backend_requests(backend, &mut pending_backend_requests);
                    return Ok(());
                }
                Some(XdmcpOutcome::Reset) => {
                    // Forced, like SIGHUP: a client connecting between the
                    // session ending and the boundary must not veto the
                    // renewal the protocol already committed to.
                    reset_trigger.note_reset_requested();
                }
            }
        }

        match reset_trigger.take_pending() {
            None => {}
            Some(ResetAction::Terminate) => {
                log::info!("reset: -terminate — last client left, shutting down");
                setup_thread::shutdown_all(&setup_registry);
                cancel_all_pending_backend_requests(backend, &mut pending_backend_requests);
                return Ok(());
            }
            Some(ResetAction::Reset) => {
                // Unsubmitted client requests belong to the generation
                // being retired. Keep an already submitted input command in
                // the process-lifetime lane, but discard its old protocol
                // continuation so a late success commits using current atoms.
                cancel_unsubmitted_xi_configs(&mut xi_config_lane, &mut pending_backend_requests);
                let outcome = reset_generation(
                    state,
                    backend,
                    poll.registry(),
                    &generations,
                    &setup_registry,
                    input_inventory,
                    GenerationLocals {
                        deferred_requests: &mut deferred_requests,
                        server_grab_waiters: &mut server_grab_waiters,
                        pending_backend_requests: &mut pending_backend_requests,
                        telemetry: &mut telemetry,
                    },
                );
                // The boundary refused: the old session's composite overlay
                // could not be released, so there is no safe generation to
                // continue into. `reset_generation` has already logged why.
                // Shut down the same way `-terminate` does — under XDMCP the
                // display manager re-queries and gets a clean process.
                let Some(generation) = outcome else {
                    setup_thread::shutdown_all(&setup_registry);
                    cancel_all_pending_backend_requests(backend, &mut pending_backend_requests);
                    return Ok(());
                };
                // Disarm for the generation just installed. Without
                // this the empty client set the reset leaves behind
                // would be re-latched by the next departure-shaped
                // event and reset a second time.
                reset_trigger.begin_generation();
                log::info!("reset: new generation installed ({generation:?})");
                // `XdmcpReset` (`xdmcp.c:618`), AFTER the new generation is
                // installed — the cookie the re-query is about to earn
                // belongs to this generation, and binding it to the old one
                // would refuse the very session it is fetching.
                if let Some(service) = xdmcp.as_mut() {
                    service.restart(&auth, generation);
                }
            }
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

    // Pointer motion and other input-driven sprite changes.
    crate::core_loop::process_request::emit_xfixes_cursor_notify(state, backend);

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
        // (no clock / target already reached) complete now.
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
pub(crate) fn dispatch_pending_host_events(
    state: &mut ServerState,
    backend: &mut dyn Backend,
) -> bool {
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
                crate::core_loop::record::record_device_event(
                    state,
                    crate::core_loop::record::RecordedDeviceEvent {
                        event_type: if ev.pressed { 2 } else { 3 },
                        detail: ev.keycode,
                        repeat: false,
                        time: ev.time,
                        root_x: ev.root_x,
                        root_y: ev.root_y,
                        state: ev.state,
                    },
                );
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
    let (screen_rotation, width, height, width_mm, height_mm) = state.randr.screen_change_fields();
    // Resolve each changed output's current crtc/mode for the notify payload.
    let changed: Vec<(u32, u32, u32, u8, u16)> = changed_outputs
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
                        if o.mode_id != 0 {
                            o.rotation
                        } else {
                            crate::randr::RR_ROTATE_0
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
                    rotation: screen_rotation,
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
            for &(output, crtc, mode, connection, rotation) in &changed {
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
                        rotation,
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

    // Xorg stamps property notifies with the current time and leaves
    // lastSetTime alone (`rrproperty.c:75`): mutter/muffin compare
    // lastSetTime with their own SetCrtcConfig reply to tell their
    // configuration from an external one.
    let timestamp = state.timestamp_now();
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
        let (width, height) = output.footprint();
        max_x = max_x.max(i32::from(output.x).saturating_add(i32::from(width)));
        max_y = max_y.max(i32::from(output.y).saturating_add(i32::from(height)));
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
    let (screen_rotation, width, height, width_mm, height_mm) = state.randr.screen_change_fields();
    // Per-CRTC geometry (position AND mode size). CrtcChangeNotify must
    // report each CRTC's own mode dimensions — NOT the logical screen
    // size — or a multi-monitor client sees every CRTC as e.g. 5120×1440
    // instead of its real 2560×1440. An off CRTC (no mode) reports 0×0.
    let crtc_geom: std::collections::HashMap<u32, (i16, i16, u16, u16, u16)> = state
        .randr
        .outputs
        .iter()
        .map(|o| (o.crtc_id, (o.x, o.y, o.width, o.height, o.rotation)))
        .collect();
    let output_states: std::collections::HashMap<u32, (u8, u32, u16)> = state
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
                    if output.mode_id != 0 {
                        output.rotation
                    } else {
                        crate::randr::RR_ROTATE_0
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
                    rotation: screen_rotation,
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
                let (x, y, crtc_w, crtc_h, rotation) = crtc_geom.get(&crtc).copied().unwrap_or((
                    0,
                    0,
                    0,
                    0,
                    crate::randr::RR_ROTATE_0,
                ));
                let event = x11randr::encode_crtc_change_notify_event(
                    client.byte_order,
                    RANDR_FIRST_EVENT,
                    sequence,
                    x11randr::CrtcChangeNotify {
                        timestamp,
                        request_window: request_window.0,
                        crtc,
                        mode,
                        rotation,
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
                let (connection, current_crtc, rotation) =
                    output_states.get(&output).copied().unwrap_or((
                        x11randr::CONNECTION_CONNECTED,
                        projected_crtc,
                        crate::randr::RR_ROTATE_0,
                    ));
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
                        rotation,
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
            screen_width_mm: u16::try_from(state.randr.width_mm).unwrap_or(u16::MAX),
            screen_height_mm: u16::try_from(state.randr.height_mm).unwrap_or(u16::MAX),
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
            screen_width_mm: 0,
            screen_height_mm: 0,
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

/// Arm / refresh / clear repeat state for the origin of an incoming host
/// input event. Each keyboard view has one current repeat key, matching
/// Xorg's per-device XKB repeat state: a key press from another keyboard
/// cannot replace it. A matching release clears that origin's timer.
fn update_repeat_state(state: &mut ServerState, ev: &HostInputEvent) {
    use crate::core_loop::{
        InputOrigin,
        message::HostInputEvent::{DeviceRemoved, DeviceSuspended, Key},
    };
    let Key(key) = ev else {
        let source_id = match ev {
            DeviceRemoved { source_id } | DeviceSuspended { source_id } => *source_id,
            _ => return,
        };
        state.key_repeats.remove(&InputOrigin::Physical(source_id));
        return;
    };
    if !crate::core_loop::key_fanout::keyboard_origin_is_live(state, key.origin) {
        state.key_repeats.remove(&key.origin);
        return;
    }
    if key.pressed {
        if crate::core_loop::key_fanout::keyboard_key_is_down(state, key.origin, key.keycode) {
            // A duplicate press is not a new XKB press and does not restart
            // this origin's repeat delay.
            return;
        }
        // ChangeKeyboardControl gate: global auto-repeat off disables all
        // repeat; otherwise the per-key bitmap decides. A non-repeating key
        // replaces the current repeat key for this origin only.
        if !state.keyboard_control.key_auto_repeats(key.keycode) {
            state.key_repeats.remove(&key.origin);
            return;
        }
        state.key_repeats.insert(
            key.origin,
            KeyRepeatState {
                event: *key,
                next_fire: Instant::now() + REPEAT_INITIAL_DELAY,
            },
        );
    } else if state
        .key_repeats
        .get(&key.origin)
        .is_some_and(|repeat| repeat.event.keycode == key.keycode)
    {
        state.key_repeats.remove(&key.origin);
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
pub fn fire_pending_repeats(state: &mut ServerState, backend: &mut dyn Backend) -> bool {
    let mut origins: Vec<_> = state.key_repeats.keys().copied().collect();
    origins.sort_by_key(|origin| match origin {
        crate::core_loop::InputOrigin::Physical(source) => (0, source.0),
        crate::core_loop::InputOrigin::XTest(device_id) => (1, u64::from(*device_id)),
        crate::core_loop::InputOrigin::NestedHost => (2, 0),
    });

    // A drained source may lose its held key before its timer fires. Do not
    // synthesize a new press for that key; the origin's held set is the
    // authority for whether repeat remains active.
    let stale_origins: Vec<_> = origins
        .iter()
        .copied()
        .filter(|origin| {
            let Some(armed) = state.key_repeats.get(origin) else {
                return true;
            };
            !crate::core_loop::key_fanout::keyboard_origin_is_live(state, *origin)
                || !crate::core_loop::key_fanout::keyboard_key_is_down(
                    state,
                    *origin,
                    armed.event.keycode,
                )
                || !state.keyboard_control.key_auto_repeats(armed.event.keycode)
        })
        .collect();
    for origin in stale_origins {
        state.key_repeats.remove(&origin);
    }

    let now = Instant::now();
    let due: Vec<_> = origins
        .into_iter()
        .filter_map(|origin| {
            state
                .key_repeats
                .get(&origin)
                .filter(|armed| armed.next_fire <= now)
                .copied()
                .map(|armed| (origin, armed))
        })
        .collect();
    if due.is_empty() {
        return false;
    }

    for (origin, armed) in due {
        let mut next_fire = armed.next_fire;
        while now >= next_fire {
            next_fire += REPEAT_PERIOD;
        }
        // Update the timer first so any reentrant arming during fan-out
        // doesn't double-fire.
        if let Some(repeat) = state.key_repeats.get_mut(&origin) {
            repeat.next_fire = next_fire;
        }
        let mut release = armed.event;
        release.pressed = false;
        let mut press = armed.event;
        press.pressed = true;
        backend.on_host_input(state, HostInputEvent::KeyRepeat(release));
        backend.on_host_input(state, HostInputEvent::KeyRepeat(press));
    }
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
        // Skip if no alarm or await references this counter.
        let has_alarm = state
            .sync_alarms
            .values()
            .any(|a| a.counter == counter && a.state == x11sync::ALARM_STATE_ACTIVE);
        if !has_alarm && !crate::core_loop::sync_await::idletime_awaited(state, counter) {
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
        // Record the new value first: firing an await can re-enter the
        // IDLETIME bookkeeping through a fresh await's baseline.
        state.idletime_last_evaluated.insert(counter, current_idle);
        crate::core_loop::sync_await::counter_changed(state, counter, old_idle, current_idle);
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
    generation: crate::core_loop::Generation,
    stream: Transport,
    resource_id_base: u32,
    resource_id_mask: u32,
    byte_order: yserver_protocol::x11::ClientByteOrder,
    is_local: bool,
    fd_passing: bool,
    setup_reply: &[u8],
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
            write_failed: false,
            focused_window: crate::resources::ROOT_WINDOW,
            reader_control: Some(reader_control_tx),
            is_local,
            fd_passing,
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
    // Read the transport off the stream before it is moved into the
    // reader. It has to be the transport, not `is_local`: locality is a
    // property of the ADDRESS, so a loopback TCP client is local and
    // would otherwise be reported as "unix" on the same line that says
    // fd passing is off — misleading exactly where same-host XDMCP is
    // being debugged. The branch already had to learn this distinction
    // once, for authorization.
    let transport_label = match &stream {
        Transport::Unix(_) => "unix",
        Transport::Tcp(_) => "TCP",
        #[cfg(test)]
        Transport::Capture(_) => "test capture",
    };
    // The reader inherits the setup thread's binding rather than
    // re-reading the counter, so the connection keeps ONE generation
    // from accept to disconnect.
    crate::core_loop::client_reader::spawn(
        id,
        stream,
        byte_order,
        BIG_REQUESTS_MAJOR_OPCODE,
        reader_control_rx,
        sender.bind_to(generation),
    )?;

    // At INFO deliberately: this is the only place a log says which
    // transport a client arrived on. In an XDMCP deployment that is the
    // first question worth asking, because it decides whether DRI3 and
    // MIT-SHM were available to that client at all — and it should not
    // require raising the log level of a whole session to find out.
    log::info!(
        "client {} established over {} (fd passing {})",
        id.0,
        transport_label,
        if fd_passing { "on" } else { "off" },
    );
    // Xorg's ClientStateRunning callback: FutureClients contexts take the
    // client on, and enabled ones record its setup reply.
    crate::core_loop::record::client_started(state, id, setup_reply);

    // Reaching here is what "ESTABLISHED" means: the poller registration
    // and the reader spawn have both succeeded, so the client can
    // actually participate in the loop. The reset trigger is NOT armed
    // here — see the note at the end of this function — but this is the
    // point the caller's arming decision is about.
    //
    // Xorg's equivalent is `client->clientState = ClientStateRunning`
    // (`dix/dispatch.c:3762`), set only after the setup reply is written
    // and establishment has fully succeeded; `CloseDownClient`
    // (`:3537`) then triggers the last-client reset only for a client
    // that reached Running. A client of ours whose `register` or
    // `spawn` fails cannot participate in the core loop at all — it
    // produces no request and no reader thread — so it is not the
    // analogue of Running. Arming at the insert instead would let the
    // caller's own error path — which disconnects a failed setup — fire a
    // reset for a client that never ran.
    //
    // Everything that ends before this line must arm nothing: a port
    // scan on the TCP listener, a handshake that drops half-way, a
    // connection refused for a bad cookie, and a failed registration or
    // reader spawn.
    //
    // Arming itself is the CALLER's, deliberately. Under XDMCP a setup
    // can complete and then immediately lose a `Refuse` race, and the
    // service disconnects it as orphaned. Arming here would let that
    // drop drain an armed client set and schedule a generation — a
    // spurious reset in the middle of the negotiation's own retry. Only
    // a RETAINED client may arm, so the decision has to sit after
    // XDMCP admission.
    Ok(())
}

/// Is `peer` one of this machine's own addresses?
///
/// Xorg's `xtransLocalClient` (`os/access.c`) treats an AF_UNIX peer as
/// local, and otherwise compares the peer against `selfhosts` — the
/// addresses `DefineSelf` collected from the interfaces. So a TCP
/// connection from the machine's own address is a LOCAL client there, and
/// keeps the locality-gated extensions.
///
/// Queried per accept rather than snapshotted at startup: accepts are
/// rare, `getifaddrs` is cheap, and a cached set goes stale across a
/// hotplug or a DHCP renewal. Xorg snapshots and then patches with
/// `AugmentSelf`; asking each time is simpler and cannot drift.
///
/// Not implemented: Xorg additionally treats a client whose command name
/// is `ssh` as non-local, to catch a forwarded connection. That is a
/// heuristic on `/proc`, and `ssh -X` reaches us over a UNIX socket
/// anyway.
fn address_is_ours(peer: std::net::IpAddr) -> bool {
    if peer.is_loopback() {
        return true;
    }
    let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: `getifaddrs` fills `ifap` with an owned list on success; we
    // walk it without retaining anything and free it before returning.
    if unsafe { libc::getifaddrs(&raw mut ifap) } != 0 {
        return false;
    }
    let mut found = false;
    let mut cur = ifap;
    while !cur.is_null() {
        // SAFETY: `cur` is a node of the list `getifaddrs` just built, and
        // `ifa_addr` is either null or a valid `sockaddr` for its family.
        let addr = unsafe { (*cur).ifa_addr };
        if !addr.is_null() && unsafe { (*addr).sa_family } == libc::AF_INET as libc::sa_family_t {
            let sin = addr.cast::<libc::sockaddr_in>();
            // SAFETY: family said AF_INET, so the node is a sockaddr_in.
            let raw = unsafe { (*sin).sin_addr.s_addr };
            if std::net::IpAddr::V4(std::net::Ipv4Addr::from(u32::from_be(raw))) == peer {
                found = true;
                break;
            }
        }
        // SAFETY: as above; `ifa_next` is null at the end of the list.
        cur = unsafe { (*cur).ifa_next };
    }
    // SAFETY: `ifap` is exactly what `getifaddrs` returned and is freed once.
    unsafe { libc::freeifaddrs(ifap) };
    found
}

/// Accept at most this many connections per listener and core iteration.
const ACCEPT_BUDGET: usize = 16;

/// Preserve readiness across budget-limited accepts, and rotate the first
/// listener served each iteration independently of the poller's event order.
struct ListenerReadiness {
    ready: Vec<bool>,
    next: usize,
}

impl ListenerReadiness {
    fn new(count: usize) -> Self {
        Self {
            ready: vec![false; count],
            next: 0,
        }
    }

    fn mark_ready(&mut self, index: usize) {
        if let Some(ready) = self.ready.get_mut(index) {
            *ready = true;
        }
    }

    fn has_pending(&self) -> bool {
        self.ready.iter().any(|ready| *ready)
    }

    fn accept_ready(
        &mut self,
        listeners: &[Listener],
        allocator: &ClientIdAllocator,
        sender: &CoreSender,
        registry: &SetupRegistry,
        auth: &Arc<AuthState>,
    ) {
        let mut first = None;
        for offset in 0..listeners.len() {
            let index = (self.next + offset) % listeners.len();
            if self.ready[index] {
                first.get_or_insert(index);
                self.ready[index] =
                    accept_pending(&listeners[index], allocator, sender, registry, auth);
            }
        }
        if let Some(first) = first {
            self.next = (first + 1) % listeners.len();
        }
    }
}

/// Accept one bounded batch, returning whether readiness must be retained.
/// mio is edge-triggered: after hitting the budget, keep polling this listener
/// without blocking until an accept reaches WouldBlock.
fn accept_pending(
    listener: &Listener,
    client_id_allocator: &ClientIdAllocator,
    sender: &CoreSender,
    registry: &SetupRegistry,
    auth: &Arc<AuthState>,
) -> bool {
    for _ in 0..ACCEPT_BUDGET {
        let accepted = match listener {
            Listener::Unix(listener) => listener
                .accept()
                .map(|(stream, _)| (Transport::Unix(stream), true, true)),
            Listener::Tcp(listener) => listener.accept().map(|(stream, peer)| {
                // `is_local` is an ADDRESS property, `fd_passing` is a
                // TRANSPORT one, and this is the site that must not
                // conflate them. `SCM_RIGHTS` is impossible over TCP
                // whoever the peer is, so fd passing is always off here.
                // Locality is not: Xorg's `xtransLocalClient`
                // (`os/access.c`) answers TRUE for a TCP peer whose
                // address is one of the server's own, which is why a
                // same-machine client keeps MIT-SHM — its `Attach` passes
                // a SysV shmid, an integer on the wire, so shared memory
                // works fine without a descriptor.
                (Transport::Tcp(stream), address_is_ours(peer.ip()), false)
            }),
        };
        match accepted {
            Ok((stream, is_local, fd_passing)) => {
                let id = client_id_allocator.allocate();
                // Bind the connection's producer HERE, at accept: this
                // is the moment that decides which session the client
                // belongs to. Everything it later sends — its setup
                // thread's messages, and its reader thread's, which
                // inherit this binding — is tagged with the generation
                // running now, so a reset retires all of it even if the
                // thread only wakes up on the far side of the boundary.
                if let Err(err) = setup_thread::spawn(
                    id,
                    stream,
                    sender.bind(),
                    registry.clone(),
                    auth.clone(),
                    is_local,
                    fd_passing,
                ) {
                    error!("setup thread spawn failed for client {}: {err}", id.0);
                }
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => return false,
            // These do not mean the accept queue is empty. Count failed
            // syscalls toward the budget too, so even repeated errors yield.
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::ConnectionAborted
                ) => {}
            Err(err) => {
                warn!("accept failed: {err}");
                return false;
            }
        }
    }
    true
}

// Silence unused-import lints when the listener path is only exercised
// indirectly. Concrete uses below.
#[allow(dead_code)]
fn _hint(_: Transport) {}
