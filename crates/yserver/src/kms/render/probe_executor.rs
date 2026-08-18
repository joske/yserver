//! Process-isolated PRIME route qualification and conflict scheduling.
//!
//! The core thread only enqueues owned jobs and drains scalar results. One
//! coordinator thread admits one helper at a time in deterministic FIFO order.
//! Conflict domains are still tracked explicitly so a terminal helper failure
//! poisons only work sharing its source GPU, optional sink GPU, or KMS card.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque},
    io,
    os::fd::AsFd,
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::Duration,
};

use yserver_core::{
    backend::CrtcConfigToken,
    core_loop::{CoreSender, Message},
};

use crate::{
    internal_probe::{
        ProbeFailure, ProbeHelperRunError, ProbeHelperSupervisor, ProbeVulkanDeviceSelector,
        RouteProbeOutcome, RouteProbeRequest, RouteProbeResponse,
    },
    platform::drm::DrmDeviceKey,
};

use super::backend::{CrtcConfigProbeCompletion, CrtcConfigProbeExecutor, CrtcConfigProbeJob};

// The pending backend snapshot currently uses one global topology epoch. Keep
// application order identical to request order until that snapshot becomes
// resource-scoped; parallel disjoint completions could otherwise stale one
// another nondeterministically.
const MAX_CONCURRENT_PROBES: usize = 1;

/// Whole-child containment bound, deliberately independent of the 200 ms
/// budget attached to each submitted fence. A copied route may consume twelve
/// independent fence waits plus allocation, TEST_ONLY, readback, and teardown.
/// Expiry leaves the whole route indeterminate and therefore poisons every
/// resource in that request's conflict set.
const PROBE_PROCESS_WATCHDOG: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
enum ProbeResource {
    Vulkan {
        device_uuid: [u8; 16],
        driver_uuid: [u8; 16],
    },
    Kms(DrmDeviceKey),
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct ProbeConflictSet(BTreeSet<ProbeResource>);

impl ProbeConflictSet {
    fn for_request(request: &RouteProbeRequest) -> Self {
        let mut resources = BTreeSet::new();
        resources.insert(vulkan_resource(request.source_selector));
        if let Some(sink) = request.copied_sink {
            resources.insert(vulkan_resource(sink.selector));
        }
        resources.insert(ProbeResource::Kms(request.source_route.kms_device_key));
        Self(resources)
    }

    fn intersects(&self, other: &Self) -> bool {
        self.0.iter().any(|resource| other.0.contains(resource))
    }

    fn extend(&mut self, other: &Self) {
        self.0.extend(other.0.iter().cloned());
    }
}

fn vulkan_resource(selector: ProbeVulkanDeviceSelector) -> ProbeResource {
    ProbeResource::Vulkan {
        device_uuid: selector.device_uuid,
        driver_uuid: selector.driver_uuid,
    }
}

#[derive(Clone, Debug)]
struct PoisonCause {
    source: CrtcConfigToken,
    kind: io::ErrorKind,
    detail: Arc<str>,
}

impl PoisonCause {
    fn error_for(&self, token: CrtcConfigToken) -> io::Error {
        io::Error::new(
            self.kind,
            format!(
                "PRIME probe {token:?} conflicts with resources poisoned by {:?}: {}",
                self.source, self.detail
            ),
        )
    }
}

#[derive(Clone, Debug)]
struct ProbeTicket {
    token: CrtcConfigToken,
    conflicts: ProbeConflictSet,
}

#[derive(Debug, Eq, PartialEq)]
enum CancelDisposition {
    Queued,
    Running,
    Unknown,
}

/// Pure admission state. Jobs/fds live in the coordinator beside this type so
/// scheduling and poison behavior can be tested without threads or DRM.
struct ConflictScheduler {
    cap: usize,
    queued: VecDeque<ProbeTicket>,
    running: HashMap<CrtcConfigToken, ProbeConflictSet>,
    poisoned: BTreeMap<ProbeResource, PoisonCause>,
}

impl ConflictScheduler {
    fn new(cap: usize) -> Self {
        Self {
            cap,
            queued: VecDeque::new(),
            running: HashMap::new(),
            poisoned: BTreeMap::new(),
        }
    }

    fn contains(&self, token: CrtcConfigToken) -> bool {
        self.running.contains_key(&token) || self.queued.iter().any(|ticket| ticket.token == token)
    }

    fn enqueue(&mut self, ticket: ProbeTicket) -> Result<(), PoisonCause> {
        if let Some(cause) = self.poison_for(&ticket.conflicts) {
            return Err(cause);
        }
        self.queued.push_back(ticket);
        Ok(())
    }

    fn cancel(&mut self, token: CrtcConfigToken) -> CancelDisposition {
        if let Some(index) = self.queued.iter().position(|ticket| ticket.token == token) {
            self.queued.remove(index);
            return CancelDisposition::Queued;
        }
        if self.running.contains_key(&token) {
            return CancelDisposition::Running;
        }
        CancelDisposition::Unknown
    }

    /// Admit in queue order. A job blocked by a running resource reserves all
    /// of its own resources against later jobs, preventing a later conflicting
    /// request from overtaking it. With a future cap above one, only disjoint
    /// later work may fill another slot.
    fn take_runnable(&mut self) -> Vec<CrtcConfigToken> {
        let slots = self.cap.saturating_sub(self.running.len());
        if slots == 0 {
            return Vec::new();
        }

        let mut active = ProbeConflictSet::default();
        for conflicts in self.running.values() {
            active.extend(conflicts);
        }
        let mut blocked_by_earlier = ProbeConflictSet::default();
        let mut selected = Vec::new();
        for (index, ticket) in self.queued.iter().enumerate() {
            if selected.len() == slots {
                break;
            }
            if ticket.conflicts.intersects(&active)
                || ticket.conflicts.intersects(&blocked_by_earlier)
            {
                blocked_by_earlier.extend(&ticket.conflicts);
                continue;
            }
            selected.push(index);
            active.extend(&ticket.conflicts);
        }

        let mut tickets = Vec::with_capacity(selected.len());
        for index in selected.into_iter().rev() {
            tickets.push(
                self.queued
                    .remove(index)
                    .expect("selected scheduler index remains valid"),
            );
        }
        tickets.reverse();
        let mut tokens = Vec::with_capacity(tickets.len());
        for ticket in tickets {
            tokens.push(ticket.token);
            self.running.insert(ticket.token, ticket.conflicts);
        }
        tokens
    }

    /// Retire a running job and, on a terminal result, permanently poison all
    /// resources it touched. Returns queued tokens which now conflict and must
    /// receive terminal failures without being spawned.
    fn finish(
        &mut self,
        token: CrtcConfigToken,
        poison: Option<PoisonCause>,
    ) -> Option<Vec<(CrtcConfigToken, PoisonCause)>> {
        let conflicts = self.running.remove(&token)?;
        if let Some(cause) = poison {
            for resource in &conflicts.0 {
                self.poisoned
                    .entry(resource.clone())
                    .or_insert_with(|| cause.clone());
            }
        }

        let mut failures = Vec::new();
        let mut index = 0;
        while index < self.queued.len() {
            let cause = self.poison_for(&self.queued[index].conflicts);
            let Some(cause) = cause else {
                index += 1;
                continue;
            };
            let ticket = self
                .queued
                .remove(index)
                .expect("queued poison index remains valid");
            failures.push((ticket.token, cause));
        }
        Some(failures)
    }

    fn poison_for(&self, conflicts: &ProbeConflictSet) -> Option<PoisonCause> {
        conflicts
            .0
            .iter()
            .find_map(|resource| self.poisoned.get(resource).cloned())
    }
}

struct MappedProbeResult {
    result: io::Result<super::platform::QualifiedScanoutPlan>,
    poison: Option<PoisonCause>,
}

fn map_helper_result(
    token: CrtcConfigToken,
    result: Result<RouteProbeResponse, ProbeHelperRunError>,
) -> MappedProbeResult {
    match result {
        Ok(response) => match response.outcome {
            RouteProbeOutcome::Compatible(plan) => MappedProbeResult {
                result: Ok(plan),
                poison: None,
            },
            RouteProbeOutcome::Rejected(failure) => MappedProbeResult {
                result: Err(probe_failure_error(
                    io::ErrorKind::Unsupported,
                    "route rejected",
                    failure,
                )),
                poison: None,
            },
            RouteProbeOutcome::Indeterminate(failure) => {
                let error = probe_failure_error(
                    io::ErrorKind::TimedOut,
                    "route probe became indeterminate",
                    failure,
                );
                terminal_mapping(token, error)
            }
            RouteProbeOutcome::Internal(failure) => {
                let error = probe_failure_error(
                    io::ErrorKind::Other,
                    "route probe helper failed internally",
                    failure,
                );
                terminal_mapping(token, error)
            }
        },
        Err(error @ ProbeHelperRunError::NotStarted(_)) => MappedProbeResult {
            result: Err(helper_run_io_error(error)),
            poison: None,
        },
        Err(error @ ProbeHelperRunError::ChildStartedUncertain(_)) => {
            terminal_mapping(token, helper_run_io_error(error))
        }
    }
}

fn helper_run_io_error(error: ProbeHelperRunError) -> io::Error {
    io::Error::new(error.kind(), error)
}

fn probe_failure_error(kind: io::ErrorKind, context: &str, failure: ProbeFailure) -> io::Error {
    io::Error::new(
        kind,
        format!(
            "{context} (error_code={}, detail_code={})",
            failure.error_code, failure.detail_code
        ),
    )
}

fn terminal_mapping(token: CrtcConfigToken, error: io::Error) -> MappedProbeResult {
    let poison = PoisonCause {
        source: token,
        kind: error.kind(),
        detail: Arc::<str>::from(error.to_string()),
    };
    MappedProbeResult {
        result: Err(error),
        poison: Some(poison),
    }
}

#[derive(Default)]
struct WakeState {
    inner: Mutex<WakeStateInner>,
}

#[derive(Default)]
struct WakeStateInner {
    sender: Option<CoreSender>,
    pending: bool,
}

impl WakeState {
    fn install_sender(&self, sender: CoreSender) {
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        inner.sender = Some(sender);
        if inner.pending {
            inner.pending = false;
            if let Some(sender) = inner.sender.as_ref()
                && let Err(error) = sender.send(Message::CrtcConfigReady)
            {
                log::warn!("failed to deliver deferred CRTC probe wake: {error}");
            }
        }
    }

    fn notify(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let Some(sender) = inner.sender.as_ref() else {
            inner.pending = true;
            return;
        };
        if let Err(error) = sender.send(Message::CrtcConfigReady) {
            log::warn!("failed to wake core for CRTC probe completion: {error}");
        }
    }
}

enum CoordinatorCommand {
    Enqueue(CrtcConfigProbeJob),
    Cancel(CrtcConfigToken),
    Finished {
        token: CrtcConfigToken,
        result: Result<RouteProbeResponse, ProbeHelperRunError>,
    },
    Shutdown,
}

/// Production executor. The object itself remains on the core thread; all
/// scheduling, helper spawning, watchdog waits, and IPC run off-thread.
pub(crate) struct ProcessProbeExecutor {
    command_tx: Sender<CoordinatorCommand>,
    ready_rx: Receiver<CrtcConfigProbeCompletion>,
    wake: Arc<WakeState>,
}

impl ProcessProbeExecutor {
    pub(crate) fn new() -> io::Result<Self> {
        let supervisor = Arc::new(ProbeHelperSupervisor::for_current_exe(
            PROBE_PROCESS_WATCHDOG,
        )?);
        let (command_tx, command_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let wake = Arc::new(WakeState::default());
        let coordinator_tx = command_tx.clone();
        let coordinator_wake = Arc::clone(&wake);
        thread::Builder::new()
            .name("yserver-prime-probe-scheduler".into())
            .spawn(move || {
                run_coordinator(
                    command_rx,
                    coordinator_tx,
                    ready_tx,
                    coordinator_wake,
                    supervisor,
                );
            })?;
        Ok(Self {
            command_tx,
            ready_rx,
            wake,
        })
    }
}

impl CrtcConfigProbeExecutor for ProcessProbeExecutor {
    fn set_core_sender(&mut self, sender: CoreSender) {
        self.wake.install_sender(sender);
    }

    fn enqueue(&mut self, job: CrtcConfigProbeJob) -> io::Result<()> {
        self.command_tx
            .send(CoordinatorCommand::Enqueue(job))
            .map_err(|_| io::Error::other("PRIME probe coordinator stopped"))
    }

    fn drain_ready(&mut self) -> Vec<CrtcConfigProbeCompletion> {
        let mut ready = Vec::new();
        while let Ok(completion) = self.ready_rx.try_recv() {
            ready.push(completion);
        }
        ready
    }

    fn cancel(&mut self, token: CrtcConfigToken) {
        let _ = self.command_tx.send(CoordinatorCommand::Cancel(token));
    }
}

impl Drop for ProcessProbeExecutor {
    fn drop(&mut self) {
        let _ = self.command_tx.send(CoordinatorCommand::Shutdown);
    }
}

fn run_coordinator(
    command_rx: Receiver<CoordinatorCommand>,
    command_tx: Sender<CoordinatorCommand>,
    ready_tx: Sender<CrtcConfigProbeCompletion>,
    wake: Arc<WakeState>,
    supervisor: Arc<ProbeHelperSupervisor>,
) {
    let mut scheduler = ConflictScheduler::new(MAX_CONCURRENT_PROBES);
    let mut queued_jobs = HashMap::new();
    let mut cancelled_running = HashSet::new();

    while let Ok(command) = command_rx.recv() {
        match command {
            CoordinatorCommand::Enqueue(job) => {
                let token = job.request.token;
                if scheduler.contains(token) || queued_jobs.contains_key(&token) {
                    // Tokens are allocated uniquely by the backend. Do not let
                    // an impossible duplicate complete/cancel the original
                    // request that already owns this token.
                    log::error!("ignoring duplicate asynchronous PRIME probe token {token:?}");
                    continue;
                }
                let ticket = ProbeTicket {
                    token,
                    conflicts: ProbeConflictSet::for_request(&job.request),
                };
                match scheduler.enqueue(ticket) {
                    Ok(()) => {
                        queued_jobs.insert(token, job);
                    }
                    Err(cause) => {
                        publish_completion(&ready_tx, &wake, token, Err(cause.error_for(token)));
                    }
                }
            }
            CoordinatorCommand::Cancel(token) => match scheduler.cancel(token) {
                CancelDisposition::Queued => {
                    queued_jobs.remove(&token);
                }
                CancelDisposition::Running => {
                    cancelled_running.insert(token);
                }
                CancelDisposition::Unknown => {}
            },
            CoordinatorCommand::Finished { token, result } => {
                let mapped = map_helper_result(token, result);
                let Some(poisoned_queued) = scheduler.finish(token, mapped.poison) else {
                    continue;
                };
                if !cancelled_running.remove(&token) {
                    publish_completion(&ready_tx, &wake, token, mapped.result);
                }
                for (failed_token, cause) in poisoned_queued {
                    queued_jobs.remove(&failed_token);
                    publish_completion(
                        &ready_tx,
                        &wake,
                        failed_token,
                        Err(cause.error_for(failed_token)),
                    );
                }
            }
            CoordinatorCommand::Shutdown => break,
        }

        launch_runnable(
            &mut scheduler,
            &mut queued_jobs,
            &command_tx,
            &ready_tx,
            &wake,
            &supervisor,
            &mut cancelled_running,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn launch_runnable(
    scheduler: &mut ConflictScheduler,
    queued_jobs: &mut HashMap<CrtcConfigToken, CrtcConfigProbeJob>,
    command_tx: &Sender<CoordinatorCommand>,
    ready_tx: &Sender<CrtcConfigProbeCompletion>,
    wake: &Arc<WakeState>,
    supervisor: &Arc<ProbeHelperSupervisor>,
    cancelled_running: &mut HashSet<CrtcConfigToken>,
) {
    loop {
        let runnable = scheduler.take_runnable();
        if runnable.is_empty() {
            return;
        }
        for token in runnable {
            let Some(job) = queued_jobs.remove(&token) else {
                let error =
                    io::Error::other(format!("PRIME probe scheduler lost queued job {token:?}"));
                retire_pre_helper_failure(
                    scheduler,
                    queued_jobs,
                    ready_tx,
                    wake,
                    token,
                    error,
                    cancelled_running,
                );
                continue;
            };
            let worker_tx = command_tx.clone();
            let worker_supervisor = Arc::clone(supervisor);
            let spawn = thread::Builder::new()
                .name(format!("yserver-prime-probe-{}", token.0))
                .spawn(move || {
                    let result = worker_supervisor.run(job.kms_fd.as_fd(), job.request);
                    let _ = worker_tx.send(CoordinatorCommand::Finished { token, result });
                });
            if let Err(error) = spawn {
                retire_pre_helper_failure(
                    scheduler,
                    queued_jobs,
                    ready_tx,
                    wake,
                    token,
                    io::Error::new(
                        error.kind(),
                        format!("failed to spawn PRIME probe worker: {error}"),
                    ),
                    cancelled_running,
                );
            }
        }
    }
}

/// Retire an admitted request which failed before a helper thread/process or
/// any GPU/KMS operation started. This fails only the current token: no shared
/// resource state can have become uncertain, so later conflicting work remains
/// admissible.
fn retire_pre_helper_failure(
    scheduler: &mut ConflictScheduler,
    queued_jobs: &mut HashMap<CrtcConfigToken, CrtcConfigProbeJob>,
    ready_tx: &Sender<CrtcConfigProbeCompletion>,
    wake: &Arc<WakeState>,
    token: CrtcConfigToken,
    error: io::Error,
    cancelled_running: &mut HashSet<CrtcConfigToken>,
) {
    let Some(conflicting_failures) = scheduler.finish(token, None) else {
        return;
    };
    if !cancelled_running.remove(&token) {
        publish_completion(ready_tx, wake, token, Err(error));
    }
    for (failed_token, cause) in conflicting_failures {
        queued_jobs.remove(&failed_token);
        publish_completion(
            ready_tx,
            wake,
            failed_token,
            Err(cause.error_for(failed_token)),
        );
    }
}

fn publish_completion(
    ready_tx: &Sender<CrtcConfigProbeCompletion>,
    wake: &WakeState,
    token: CrtcConfigToken,
    result: io::Result<super::platform::QualifiedScanoutPlan>,
) {
    if ready_tx
        .send(CrtcConfigProbeCompletion { token, result })
        .is_ok()
    {
        wake.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        internal_probe::{ProbeCopiedSink, ProbeKmsHandles},
        kms::{
            render::platform::QualifiedScanoutPlan,
            scanout_route::{RenderDeviceId, RenderKmsRelationship, ScanoutRoute},
            vk::scanout::ScanoutAllocationPlan,
        },
    };
    use ::drm::control::from_u32;
    use std::fs::File;
    use yserver_core::backend::ModeSpec;

    fn token(value: u64) -> CrtcConfigToken {
        CrtcConfigToken(value)
    }

    fn selector(seed: u8) -> ProbeVulkanDeviceSelector {
        ProbeVulkanDeviceSelector {
            device_uuid: [seed; 16],
            driver_uuid: [seed.wrapping_add(0x40); 16],
        }
    }

    fn request(
        token_value: u64,
        source_seed: u8,
        sink_seed: Option<u8>,
        kms_minor: u32,
    ) -> RouteProbeRequest {
        let kms = DrmDeviceKey {
            major: 226,
            minor: kms_minor,
        };
        RouteProbeRequest {
            token: token(token_value),
            mode: ModeSpec {
                width: 1920,
                height: 1080,
                vrefresh: 60,
            },
            source_route: ScanoutRoute::new(
                RenderDeviceId::DrmRender(DrmDeviceKey {
                    major: 226,
                    minor: u32::from(source_seed),
                }),
                kms,
                RenderKmsRelationship::Different,
            ),
            source_selector: selector(source_seed),
            copied_sink: sink_seed.map(|seed| ProbeCopiedSink {
                render_device_id: RenderDeviceId::DrmRender(DrmDeviceKey {
                    major: 226,
                    minor: u32::from(seed),
                }),
                selector: selector(seed),
            }),
            kms: ProbeKmsHandles {
                connector: from_u32(1).unwrap(),
                encoder: from_u32(2).unwrap(),
                crtc: from_u32(3).unwrap(),
                plane: from_u32(4).unwrap(),
            },
            fence_timeout_ns: 200_000_000,
        }
    }

    fn ticket(request: &RouteProbeRequest) -> ProbeTicket {
        ProbeTicket {
            token: request.token,
            conflicts: ProbeConflictSet::for_request(request),
        }
    }

    #[test]
    fn conflict_set_contains_source_optional_sink_and_kms_card() {
        let with_sink = request(1, 10, Some(20), 2);
        let conflicts = ProbeConflictSet::for_request(&with_sink);
        assert_eq!(conflicts.0.len(), 3);
        assert!(conflicts.0.contains(&vulkan_resource(selector(10))));
        assert!(conflicts.0.contains(&vulkan_resource(selector(20))));
        assert!(conflicts.0.contains(&ProbeResource::Kms(DrmDeviceKey {
            major: 226,
            minor: 2,
        })));

        let without_sink = request(2, 11, None, 3);
        assert_eq!(ProbeConflictSet::for_request(&without_sink).0.len(), 2);
    }

    #[test]
    fn queued_job_owns_its_kms_fd_independently_of_platform_owner() {
        let platform_owner = File::open("/dev/null").unwrap();
        let job = CrtcConfigProbeJob {
            kms_fd: platform_owner.as_fd().try_clone_to_owned().unwrap(),
            request: request(1, 10, None, 1),
        };
        drop(platform_owner);

        assert!(job.kms_fd.as_fd().try_clone_to_owned().is_ok());
    }

    #[test]
    fn completion_before_sender_install_is_retained_and_wakes_core() {
        let (ready_tx, ready_rx) = mpsc::channel();
        let wake = WakeState::default();
        publish_completion(
            &ready_tx,
            &wake,
            token(1),
            Err(io::Error::other("test completion")),
        );
        assert_eq!(ready_rx.try_recv().unwrap().token, token(1));

        let (_poll, sender, receiver) = yserver_core::core_loop::channel().unwrap();
        wake.install_sender(sender);
        assert!(
            receiver
                .try_recv_all()
                .any(|message| matches!(message, Message::CrtcConfigReady))
        );
    }

    #[test]
    fn production_scheduler_is_global_single_flight_fifo() {
        let first = request(1, 10, None, 1);
        let second = request(2, 10, None, 2);
        let disjoint = request(3, 30, None, 3);
        let mut scheduler = ConflictScheduler::new(MAX_CONCURRENT_PROBES);
        scheduler.enqueue(ticket(&first)).unwrap();
        scheduler.enqueue(ticket(&second)).unwrap();
        scheduler.enqueue(ticket(&disjoint)).unwrap();

        assert_eq!(scheduler.take_runnable(), vec![token(1)]);
        assert!(scheduler.take_runnable().is_empty());
        assert!(scheduler.finish(token(1), None).is_some());
        assert_eq!(scheduler.take_runnable(), vec![token(2)]);
        assert!(scheduler.finish(token(2), None).is_some());
        assert_eq!(scheduler.take_runnable(), vec![token(3)]);
    }

    #[test]
    fn earlier_blocked_job_reserves_its_other_resources() {
        let running = request(1, 10, None, 1);
        let blocked = request(2, 10, Some(20), 2);
        let would_overtake = request(3, 20, None, 3);
        let mut scheduler = ConflictScheduler::new(2);
        scheduler.enqueue(ticket(&running)).unwrap();
        assert_eq!(scheduler.take_runnable(), vec![token(1)]);
        scheduler.enqueue(ticket(&blocked)).unwrap();
        scheduler.enqueue(ticket(&would_overtake)).unwrap();
        assert!(scheduler.take_runnable().is_empty());
    }

    #[test]
    fn terminal_result_poisons_resources_and_fails_conflicting_queue() {
        let first = request(1, 10, None, 1);
        let conflicting = request(2, 10, None, 2);
        let disjoint = request(3, 30, None, 3);
        let mut scheduler = ConflictScheduler::new(1);
        scheduler.enqueue(ticket(&first)).unwrap();
        scheduler.enqueue(ticket(&conflicting)).unwrap();
        scheduler.enqueue(ticket(&disjoint)).unwrap();
        assert_eq!(scheduler.take_runnable(), vec![token(1)]);

        let cause = PoisonCause {
            source: token(1),
            kind: io::ErrorKind::TimedOut,
            detail: Arc::<str>::from("watchdog timeout"),
        };
        let failed = scheduler.finish(token(1), Some(cause)).unwrap();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].0, token(2));
        assert_eq!(scheduler.take_runnable(), vec![token(3)]);
        assert!(scheduler.enqueue(ticket(&conflicting)).is_err());
    }

    #[test]
    fn rejection_does_not_poison_and_queued_conflict_runs_next() {
        let first = request(1, 10, None, 1);
        let second = request(2, 10, None, 2);
        let mut scheduler = ConflictScheduler::new(1);
        scheduler.enqueue(ticket(&first)).unwrap();
        scheduler.enqueue(ticket(&second)).unwrap();
        assert_eq!(scheduler.take_runnable(), vec![token(1)]);
        assert!(scheduler.finish(token(1), None).unwrap().is_empty());
        assert_eq!(scheduler.take_runnable(), vec![token(2)]);
    }

    #[test]
    fn pre_helper_failure_does_not_poison_conflicting_fifo_work() {
        let first = request(1, 10, None, 1);
        let conflicting = request(2, 10, None, 2);
        let mut scheduler = ConflictScheduler::new(MAX_CONCURRENT_PROBES);
        scheduler.enqueue(ticket(&first)).unwrap();
        scheduler.enqueue(ticket(&conflicting)).unwrap();
        assert_eq!(scheduler.take_runnable(), vec![token(1)]);

        let fd_owner = File::open("/dev/null").unwrap();
        let mut queued_jobs = HashMap::from([(
            token(2),
            CrtcConfigProbeJob {
                kms_fd: fd_owner.as_fd().try_clone_to_owned().unwrap(),
                request: conflicting,
            },
        )]);
        let (ready_tx, ready_rx) = mpsc::channel();
        let wake = Arc::new(WakeState::default());
        let mut cancelled_running = HashSet::new();
        retire_pre_helper_failure(
            &mut scheduler,
            &mut queued_jobs,
            &ready_tx,
            &wake,
            token(1),
            io::Error::other("worker thread did not start"),
            &mut cancelled_running,
        );

        let completion = ready_rx.try_recv().unwrap();
        assert_eq!(completion.token, token(1));
        assert_eq!(
            completion.result.err().unwrap().kind(),
            io::ErrorKind::Other
        );
        assert!(queued_jobs.contains_key(&token(2)));
        assert_eq!(scheduler.take_runnable(), vec![token(2)]);
        assert!(scheduler.finish(token(2), None).is_some());

        let later_same_resource = request(3, 10, None, 3);
        assert!(scheduler.enqueue(ticket(&later_same_resource)).is_ok());
    }

    #[test]
    fn typed_supervisor_failure_stage_controls_fifo_and_poison() {
        let first = request(1, 10, None, 1);
        let conflicting = request(2, 10, None, 2);
        let mut scheduler = ConflictScheduler::new(MAX_CONCURRENT_PROBES);
        scheduler.enqueue(ticket(&first)).unwrap();
        scheduler.enqueue(ticket(&conflicting)).unwrap();
        assert_eq!(scheduler.take_runnable(), vec![token(1)]);

        let not_started = map_helper_result(
            token(1),
            Err(ProbeHelperRunError::NotStarted(io::Error::new(
                io::ErrorKind::WouldBlock,
                "fork unavailable",
            ))),
        );
        assert_eq!(
            not_started.result.err().unwrap().kind(),
            io::ErrorKind::WouldBlock
        );
        assert!(not_started.poison.is_none());
        assert!(
            scheduler
                .finish(token(1), not_started.poison)
                .unwrap()
                .is_empty()
        );
        assert_eq!(scheduler.take_runnable(), vec![token(2)]);
        assert!(scheduler.finish(token(2), None).is_some());

        let uncertain = request(3, 10, None, 3);
        let queued_conflict = request(4, 10, None, 4);
        scheduler.enqueue(ticket(&uncertain)).unwrap();
        scheduler.enqueue(ticket(&queued_conflict)).unwrap();
        assert_eq!(scheduler.take_runnable(), vec![token(3)]);
        let child_started = map_helper_result(
            token(3),
            Err(ProbeHelperRunError::ChildStartedUncertain(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "lost child IPC",
            ))),
        );
        assert_eq!(
            child_started.result.as_ref().unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
        assert!(child_started.poison.is_some());
        let failed = scheduler
            .finish(token(3), child_started.poison)
            .expect("uncertain child was running");
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].0, token(4));
        assert!(scheduler.enqueue(ticket(&queued_conflict)).is_err());
    }

    #[test]
    fn queued_cancel_removes_work_while_running_cancel_waits_for_late_result() {
        let first = request(1, 10, None, 1);
        let second = request(2, 20, None, 2);
        let mut scheduler = ConflictScheduler::new(1);
        scheduler.enqueue(ticket(&first)).unwrap();
        scheduler.enqueue(ticket(&second)).unwrap();
        assert_eq!(scheduler.take_runnable(), vec![token(1)]);
        assert_eq!(scheduler.cancel(token(2)), CancelDisposition::Queued);
        assert_eq!(scheduler.cancel(token(1)), CancelDisposition::Running);
        assert_eq!(scheduler.cancel(token(99)), CancelDisposition::Unknown);
    }

    #[test]
    fn cancelled_running_indeterminate_poisons_before_result_is_discarded() {
        let first = request(1, 10, None, 1);
        let conflicting = request(2, 10, None, 2);
        let mut scheduler = ConflictScheduler::new(MAX_CONCURRENT_PROBES);
        scheduler.enqueue(ticket(&first)).unwrap();
        scheduler.enqueue(ticket(&conflicting)).unwrap();
        assert_eq!(scheduler.take_runnable(), vec![token(1)]);
        assert_eq!(scheduler.cancel(token(1)), CancelDisposition::Running);

        let mapped = map_helper_result(
            token(1),
            Ok(RouteProbeResponse {
                token: token(1),
                outcome: RouteProbeOutcome::Indeterminate(ProbeFailure {
                    error_code: 7,
                    detail_code: 8,
                }),
                elapsed_ns: 1,
            }),
        );
        let failed = scheduler
            .finish(token(1), mapped.poison)
            .expect("cancelled running job remains admitted until its result");
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].0, token(2));
        assert!(scheduler.enqueue(ticket(&conflicting)).is_err());
    }

    #[test]
    fn helper_outcomes_map_to_expected_poison_policy() {
        let compatible = map_helper_result(
            token(1),
            Ok(RouteProbeResponse {
                token: token(1),
                outcome: RouteProbeOutcome::Compatible(QualifiedScanoutPlan::Shared(
                    ScanoutAllocationPlan::LegacyLinear,
                )),
                elapsed_ns: 1,
            }),
        );
        assert!(compatible.result.is_ok());
        assert!(compatible.poison.is_none());

        let rejected = map_helper_result(
            token(2),
            Ok(RouteProbeResponse {
                token: token(2),
                outcome: RouteProbeOutcome::Rejected(ProbeFailure {
                    error_code: 1,
                    detail_code: 2,
                }),
                elapsed_ns: 2,
            }),
        );
        assert!(rejected.result.is_err());
        assert!(rejected.poison.is_none());

        for (token_value, outcome) in [
            (
                3,
                RouteProbeOutcome::Indeterminate(ProbeFailure {
                    error_code: 3,
                    detail_code: 4,
                }),
            ),
            (
                4,
                RouteProbeOutcome::Internal(ProbeFailure {
                    error_code: 5,
                    detail_code: 6,
                }),
            ),
        ] {
            let mapped = map_helper_result(
                token(token_value),
                Ok(RouteProbeResponse {
                    token: token(token_value),
                    outcome,
                    elapsed_ns: 3,
                }),
            );
            assert!(mapped.result.is_err());
            assert!(mapped.poison.is_some());
        }
    }

    #[test]
    fn process_watchdog_is_separate_from_per_fence_budget() {
        assert_eq!(PROBE_PROCESS_WATCHDOG, Duration::from_secs(30));
        let per_fence = Duration::from_nanos(request(1, 10, None, 1).fence_timeout_ns);
        let minimum_policy = per_fence
            .saturating_mul(12)
            .saturating_add(Duration::from_secs(5));
        assert!(
            PROBE_PROCESS_WATCHDOG >= minimum_policy,
            "whole-route containment must cover twelve copied-route fence waits plus five seconds of host allocation, TEST_ONLY, IPC, readback, and teardown work"
        );
    }
}
