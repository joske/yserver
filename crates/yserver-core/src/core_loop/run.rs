//! Skeleton of the single-threaded core loop.
//!
//! B4 established the shape; D4 wired `Message::Request` against the
//! new `process_request` entry point and the lifecycle arms
//! (SetupAllocate, ClientSetupComplete, ClientDisconnected,
//! HostInput, PageFlipReady). E3/E4 (DRM + signalfd) and F2
//! (host-X11) supply the missing token arms; D5 supplies the
//! listener.

use std::{
    collections::{HashMap, VecDeque},
    io,
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::net::{UnixListener, UnixStream},
    },
    time::{Duration, Instant},
};

use log::{error, warn};
use mio::{Events, Interest, Poll, unix::SourceFd};

use super::{
    client_io::{self, WriteOutcome},
    message::{HostInputEvent, Message, SetupAllocateResponse},
    poll_tokens::{
        ClientIdAllocator, DRM_TOKEN, HOST_X11_TOKEN, LIBINPUT_TOKEN, LISTENER_TOKEN, NOTIFY_TOKEN,
        PRESENT_COMPLETION_TOKEN, SEAT_TOKEN, client_token, token_to_client,
    },
    process_request::{RequestOutcome, process_request},
    sender::{CoreReceiver, CoreSender},
    setup_thread::{self, SetupRegistry},
};
use crate::{
    backend::{Backend, BackendFdKind, HostSocketStatus},
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
///
/// Costs: one HashMap lookup + counter increment per request, one
/// `Instant::now()` per iteration boundary, and one `info!` line per
/// second. Should be <0.5% overhead even at high request rates.
const TELEMETRY_EMIT_INTERVAL: Duration = Duration::from_secs(1);

/// Number of opcodes to show in the per-second telemetry emit.
const TELEMETRY_TOP_N: usize = 3;

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

    fn record_request(&mut self, opcode: u8, dur: Duration) {
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

        log::info!(
            "loop telemetry [{:.2}s]: iter/s={:.0} req/s={:.0} drain_max={} \
             req_time={:.1}ms ({:.1}%) longest=op{}:{:.2}ms \
             host_input/s={:.1} gap_max={:.1}ms \
             page_flip/s={:.1} iter_wall_max={:.1}ms \
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
    }
}

/// Core-loop fairness cap. Each main-loop iteration processes at
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
/// `HostInput` and `PageFlipReady` messages sit in the same
/// channel undelivered, so the cursor visibly freezes (gap_max
/// up to 8.5 seconds between consecutive cursor events).
///
/// 32 chosen as the initial cap because: typical request cost is
/// ~0.25 ms, so 32 × 0.25 ≈ 8 ms per iteration worst case — about
/// one frame at 120 Hz, well below the perceptual cursor-lag
/// threshold. The cap is intentionally NOT time-based at this
/// stage; count-based is simpler and the telemetry shows
/// per-request costs are tightly clustered. If we ever encounter
/// per-request outliers above ~5 ms, revisit with a time budget.
const MAX_REQUESTS_PER_ITER: usize = 32;

/// One pending X protocol request held over from a prior iteration
/// because the per-iteration `MAX_REQUESTS_PER_ITER` cap was hit
/// before it could be processed. Buffered locally rather than
/// pushed back into the channel: re-sending would re-trigger the
/// channel's waker (one atomic CAS per push-back) and the local
/// VecDeque avoids that overhead.
struct DeferredRequest {
    id: yserver_protocol::x11::ClientId,
    sequence: yserver_protocol::x11::SequenceNumber,
    header: yserver_protocol::x11::RequestHeader,
    body: Vec<u8>,
    attached_fd: Option<OwnedFd>,
}

/// Process one X protocol request and run its post-handler bookkeeping
/// (mark_dirty + disconnect-on-error). Factored so the two drain paths
/// in `run_core` (the deferred queue at the top of each iteration and
/// the channel drain inside `NOTIFY_TOKEN`) share identical semantics.
///
/// Returns `Some(disconnect_id)` if the handler signalled a
/// disconnect; the caller runs `process_disconnect` immediately after.
fn process_request_inline(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    id: yserver_protocol::x11::ClientId,
    sequence: yserver_protocol::x11::SequenceNumber,
    header: yserver_protocol::x11::RequestHeader,
    body: &[u8],
    attached_fd: Option<OwnedFd>,
) -> Option<yserver_protocol::x11::ClientId> {
    // Half-closed-socket / post-disconnect guard. The `Message::Request`
    // channel preserves arrival order — when a client crashes (e.g.
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
        return None;
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
    backend.mark_dirty();
    match outcome {
        RequestOutcome::Disconnect(disc_id) => Some(disc_id),
        _ => None,
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
    for (fd, kind) in backend.poll_fds() {
        let token = match kind {
            BackendFdKind::Drm => DRM_TOKEN,
            BackendFdKind::Libinput => LIBINPUT_TOKEN,
            BackendFdKind::HostX11 => HOST_X11_TOKEN,
            BackendFdKind::PresentCompletion => PRESENT_COMPLETION_TOKEN,
            BackendFdKind::Seat => SEAT_TOKEN,
        };
        poll.registry()
            .register(&mut SourceFd(&fd), token, Interest::READABLE)?;
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
    // which shrinks the race versus the old libseat gap. Fully closing
    // it would mean draining already-queued `Message::HostInput` device
    // events from `rx` here before the serve loop — left out for now to
    // avoid reordering/duplicating the loop's own message handling for a
    // path that isn't the primary (M2/Asahi) target.

    let mut events = Events::with_capacity(64);
    let mut telemetry = LoopTelemetry::new();
    if telemetry.enabled {
        log::info!(
            "loop telemetry: enabled (YSERVER_LOOP_TELEMETRY set); \
             1s rollups via info!"
        );
    }
    let mut deferred_requests: VecDeque<DeferredRequest> = VecDeque::new();
    loop {
        // Fairness: if we already have unprocessed work queued from a
        // prior iteration, don't block on the poller — we have things
        // to do right now. Without this, an idle moment where the
        // channel is briefly empty would let `poll.poll` block until
        // a fresh fd event, leaving the backlog stranded.
        let poll_timeout = if !deferred_requests.is_empty() {
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
        poll.poll(&mut events, poll_timeout)?;
        let iter_start = if telemetry.enabled {
            Some(Instant::now())
        } else {
            None
        };
        let mut requests_this_iter: u32 = 0;
        let mut request_budget: usize = MAX_REQUESTS_PER_ITER;

        // Fairness: drain the backlog from prior iterations FIRST,
        // counted against this iteration's request budget. If the
        // backlog itself exceeds the budget, we'll bail out before
        // touching the channel — guarantees that `HostInput` /
        // `PageFlipReady` arriving via fd events (DRM_TOKEN,
        // LIBINPUT_TOKEN, etc.) still get serviced in the outer
        // for-ev loop below.
        while request_budget > 0 {
            let Some(req) = deferred_requests.pop_front() else {
                break;
            };
            let req_opcode = req.header.opcode;
            let req_start = if telemetry.enabled {
                Some(Instant::now())
            } else {
                None
            };
            let disc = process_request_inline(
                state,
                backend,
                req.id,
                req.sequence,
                req.header,
                &req.body,
                req.attached_fd,
            );
            if let Some(start) = req_start {
                telemetry.record_request(req_opcode, start.elapsed());
            }
            requests_this_iter += 1;
            request_budget -= 1;
            if let Some(disc_id) = disc {
                crate::core_loop::process_disconnect::process_disconnect(state, backend, disc_id);
            }
        }
        for ev in events.iter() {
            match ev.token() {
                LISTENER_TOKEN => {
                    if let Some(listener) = listener.as_ref() {
                        accept_pending(listener, client_id_allocator, &sender, &setup_registry);
                    }
                }
                DRM_TOKEN => {
                    // DRM completion event(s). Backend drains
                    // page-flip completions and submits the next
                    // composite/flip if the screen is dirty. mio is
                    // edge-triggered; the backend itself owns
                    // batching, so this dispatch never coalesces.
                    if telemetry.enabled {
                        telemetry.page_flip_count += 1;
                    }
                    backend.on_page_flip_ready(state);
                }
                LIBINPUT_TOKEN => {
                    // Libseat mode: the backend owns libinput on the
                    // core thread and dispatches it inline. Direct
                    // mode never registers this fd (the input thread
                    // owns it).
                    backend.on_libinput_ready(state);
                }
                HOST_X11_TOKEN => {
                    // F2: host X11 connection is readable. Drain
                    // whatever frames are buffered into the backend's
                    // pending_replies / pending_events queues. Fanout
                    // happens at the outer-loop boundary so a host
                    // request issued from inside fanout cannot
                    // recursively re-enter event dispatch.
                    match backend.drain_host_socket() {
                        Ok(HostSocketStatus::WouldBlock) => {}
                        Ok(HostSocketStatus::Eof) => {
                            log::info!("host X11 connection closed; shutting down");
                            return Ok(());
                        }
                        Err(err) => {
                            log::warn!("drain_host_socket: {err}");
                            return Ok(());
                        }
                    }
                }
                PRESENT_COMPLETION_TOKEN => {
                    drain_present_completions(state, backend);
                }
                SEAT_TOKEN => {
                    backend.on_seat_ready(state);
                }
                tok if let Some(client_id) = token_to_client(tok) => {
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
                            crate::core_loop::process_disconnect::process_disconnect(
                                state, backend, client_id,
                            );
                        }
                    }
                }
                NOTIFY_TOKEN => {
                    for msg in rx.try_recv_all() {
                        match msg {
                            Message::Shutdown => {
                                setup_thread::shutdown_all(&setup_registry);
                                return Ok(());
                            }
                            Message::Request {
                                id,
                                sequence,
                                header,
                                body,
                                attached_fd,
                            } => {
                                if request_budget == 0 {
                                    // Hit the fairness cap. Buffer the
                                    // request locally and stop pulling
                                    // more Request messages this
                                    // iteration. Priorities (HostInput,
                                    // PageFlipReady, Shutdown, etc.)
                                    // arriving LATER in the channel
                                    // still get processed because we
                                    // continue the `try_recv_all`
                                    // iteration — only the Request arm
                                    // defers.
                                    deferred_requests.push_back(DeferredRequest {
                                        id,
                                        sequence,
                                        header,
                                        body,
                                        attached_fd,
                                    });
                                } else {
                                    let req_opcode = header.opcode;
                                    let req_start = if telemetry.enabled {
                                        Some(Instant::now())
                                    } else {
                                        None
                                    };
                                    let disc = process_request_inline(
                                        state,
                                        backend,
                                        id,
                                        sequence,
                                        header,
                                        &body,
                                        attached_fd,
                                    );
                                    if let Some(start) = req_start {
                                        telemetry.record_request(req_opcode, start.elapsed());
                                    }
                                    requests_this_iter += 1;
                                    request_budget -= 1;
                                    if let Some(disc_id) = disc {
                                        crate::core_loop::process_disconnect::process_disconnect(
                                            state, backend, disc_id,
                                        );
                                    }
                                }
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
                                    crate::core_loop::process_disconnect::process_disconnect(
                                        state, backend, id,
                                    );
                                }
                            }
                            Message::ClientDisconnected { id, reason: _ } => {
                                crate::core_loop::process_disconnect::process_disconnect(
                                    state, backend, id,
                                );
                            }
                            Message::HostInput(ev) => {
                                if telemetry.enabled {
                                    telemetry.record_host_input(Instant::now());
                                }
                                handle_host_input(state, backend, ev);
                                backend.mark_dirty();
                            }
                            Message::PageFlipReady => {
                                if telemetry.enabled {
                                    telemetry.page_flip_count += 1;
                                }
                                backend.on_page_flip_ready(state);
                            }
                            Message::VtRelease => {
                                if backend.vt_switching_armed() {
                                    backend.on_vt_release(state);
                                } else {
                                    backend.dump_scanout();
                                }
                            }
                            Message::VtAcquire => {
                                if backend.vt_switching_armed() {
                                    backend.on_vt_acquire(state);
                                } else {
                                    backend.dump_drawables();
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
                    }
                }
                tok => {
                    warn!("core_loop::run: unhandled poll token {tok:?}");
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
            fire_pending_repeats(state, backend);
            backend.mark_dirty();
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
            crate::core_loop::process_disconnect::process_disconnect(state, backend, disc_id);
        }

        // Wake the composite path back up if the backend went dormant
        // after the previous pageflip-complete (because nothing was
        // dirty) and fresh damage has since arrived. No-op for
        // backends that don't drive their own composite loop, and
        // no-op if a flip is still in flight on the KMS path.
        if let Err(e) = backend.maybe_composite() {
            log::warn!("core_loop::run: maybe_composite failed: {e}");
        }
        drain_present_completions(state, backend);

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

fn drain_present_completions(state: &mut ServerState, backend: &mut dyn Backend) {
    let completed = backend.drain_completed_present_events();
    for entry in completed {
        if let crate::backend::PresentWake::Pixmap { idle_fence_xid } = entry.wake
            && idle_fence_xid != 0
            && let Some(f) = state.sync_fences.get_mut(&idle_fence_xid)
        {
            f.triggered = true;
        }
        // Wake-signal already fired inside the backend's drain via
        // the Arc-pinned handle; we only do X11-side event fan-out
        // here.
        crate::core_loop::process_request::fire_present_completion_events(state, &entry);
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
                    handle_host_container_resize(state, ev);
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
    ev: crate::host_x11::HostConfigureEvent,
) {
    use std::sync::atomic::Ordering;
    use yserver_protocol::x11::{self, SequenceNumber, randr as x11randr};

    const RANDR_FIRST_EVENT: u8 = 89;

    if ev.width == 0
        || ev.height == 0
        || (state.randr.screen_width == ev.width && state.randr.screen_height == ev.height)
    {
        return;
    }
    let timestamp = state.timestamp_now();
    state.randr.resize(timestamp, ev.width, ev.height);
    if let Some(root) = state.resources.window_mut(crate::resources::ROOT_WINDOW) {
        root.width = ev.width;
        root.height = ev.height;
    }
    if let Some(overlay) = state
        .resources
        .window_mut(crate::resources::COMPOSITE_OVERLAY_WINDOW)
    {
        overlay.width = ev.width;
        overlay.height = ev.height;
    }
    let width = ev.width;
    let height = ev.height;
    let width_mm = u16::try_from(state.randr.width_mm).unwrap_or(u16::MAX);
    let height_mm = u16::try_from(state.randr.height_mm).unwrap_or(u16::MAX);

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
                x11::Geometry {
                    root: crate::resources::ROOT_WINDOW,
                    x: 0,
                    y: 0,
                    width,
                    height,
                    border_width: 0,
                    depth: 24,
                },
                false,
            );
        },
    );

    // RANDR ScreenChangeNotify / CrtcChangeNotify / OutputChangeNotify
    // fanout. Snapshot the subscribers first so the per-client mut
    // borrow on `state.clients` in the inner loop doesn't conflict.
    let subscribers: Vec<(u32, yserver_protocol::x11::ResourceId, u16)> = state
        .randr_select_masks
        .iter()
        .map(|((owner, window), mask)| (*owner, *window, *mask))
        .collect();
    // Capture the IDs *after* `state.randr.resize` (above) so the
    // fanout uses the current values. Defensive defaults for the
    // (unreachable post-init) empty-outputs case.
    let (output, crtc, mode) = state
        .randr
        .outputs
        .first()
        .map_or((0, 0, 0), |o| (o.output_id, o.crtc_id, o.mode_id));
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
                    config_timestamp: timestamp,
                    root: crate::resources::ROOT_WINDOW.0,
                    request_window: request_window.0,
                    width,
                    height,
                    width_mm,
                    height_mm,
                },
            );
            let _ = client_io::write_or_buffer(client, &event);
        }
        if mask & x11randr::NOTIFY_MASK_CRTC_CHANGE != 0 {
            let event = x11randr::encode_crtc_change_notify_event(
                client.byte_order,
                RANDR_FIRST_EVENT,
                sequence,
                x11randr::CrtcChangeNotify {
                    timestamp,
                    request_window: request_window.0,
                    crtc,
                    mode,
                    x: ev.x,
                    y: ev.y,
                    width,
                    height,
                },
            );
            let _ = client_io::write_or_buffer(client, &event);
        }
        if mask & x11randr::NOTIFY_MASK_OUTPUT_CHANGE != 0 {
            let event = x11randr::encode_output_change_notify_event(
                client.byte_order,
                RANDR_FIRST_EVENT,
                sequence,
                x11randr::OutputChangeNotify {
                    timestamp,
                    config_timestamp: timestamp,
                    request_window: request_window.0,
                    output,
                    crtc,
                    mode,
                },
            );
            let _ = client_io::write_or_buffer(client, &event);
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
/// Reachable from the backend's on-core libinput dispatch in libseat
/// mode (the backend owns libinput on the core thread there), hence
/// `pub`.
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
fn fire_pending_repeats(state: &mut ServerState, backend: &mut dyn Backend) {
    let Some(armed) = state.repeat_state else {
        return;
    };
    // Repeat may have been disabled (ChangeKeyboardControl) after the
    // key was armed — disarm instead of firing.
    if !key_auto_repeats(&state.keyboard_control, armed.event.keycode) {
        state.repeat_state = None;
        return;
    }
    let now = Instant::now();
    if now < armed.next_fire {
        return;
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
) {
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                let id = client_id_allocator.allocate();
                if let Err(err) =
                    setup_thread::spawn(id, stream, sender.clone_handle(), registry.clone())
                {
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
        let big = vec![0xABu8; 256 * 1024];
        let _ = state
            .clients
            .get_mut(&7)
            .unwrap()
            .writer
            .lock()
            .unwrap()
            .write(&big); // partial write fills the kernel buffer
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
        let mut sink = vec![0u8; 1024 * 1024];
        peer.set_nonblocking(true).unwrap();
        let _ = peer.read(&mut sink);
        let disc = reconcile_client_writable_interest(poll.registry(), &mut state);
        assert!(disc.is_empty());
        assert!(state.clients[&7].outbound.is_empty());
        assert!(!state.clients[&7].watching_writable);

        drop(peer);
    }

    /// E3 liveness test: back-to-back `PageFlipReady` messages each
    /// reach `Backend::on_page_flip_ready` — no dedup, no rate-limit,
    /// no message-coalescing. Codex's missing-test bullet for E3.
    #[test]
    fn back_to_back_page_flip_ready_dispatches_each_time() {
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
            );
            (result, backend)
        });
        for _ in 0..3 {
            sender.send(Message::PageFlipReady).unwrap();
        }
        sender.send(Message::Shutdown).unwrap();
        for _ in 0..50 {
            if handle.is_finished() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(handle.is_finished(), "run_core did not return");
        let (result, backend) = handle.join().unwrap();
        result.unwrap();
        assert_eq!(
            backend.page_flip_count.load(Ordering::Relaxed),
            3,
            "expected 3 on_page_flip_ready dispatches",
        );
    }

    /// `handle_host_input` arms the auto-repeat timer on a real
    /// KeyPress, replaces it on a different KeyPress, and clears it
    /// on the matching KeyRelease. Regression: when on-core libinput
    /// dispatch (libseat mode) called `backend.on_host_input` directly
    /// it bypassed this wrapper and keys never repeated.
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
}
