//! Atomic commit request assembly, timing, and CRTC-local commit telemetry.

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, VecDeque},
    io,
    os::fd::{AsFd as _, AsRawFd as _, BorrowedFd, RawFd},
    rc::Rc,
    time::{Duration, Instant},
};

use drm::control::{AtomicCommitFlags, RawResourceHandle, ResourceHandle, crtc, plane, property};

use crate::drm::Device;

/// Kernel-shaped property arrays for `DRM_IOCTL_MODE_ATOMIC`.
#[derive(Clone, Debug, Default)]
pub(crate) struct RawAtomicModeReq {
    objects: Vec<RawResourceHandle>,
    count_props_per_object: Vec<u32>,
    props: Vec<u32>,
    values: Vec<u64>,
}

#[allow(non_camel_case_types)]
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
struct drm_mode_atomic {
    flags: u32,
    count_objs: u32,
    objs_ptr: u64,
    count_props_ptr: u64,
    props_ptr: u64,
    prop_values_ptr: u64,
    reserved: u64,
    user_data: u64,
}

const DRM_IOCTL_MODE_ATOMIC: crate::drm::page_flip::IoctlReq = ((3u32 << 30)
    | ((std::mem::size_of::<drm_mode_atomic>() as u32) << 16)
    | (0x64u32 << 8)
    | 0xBC)
    as crate::drm::page_flip::IoctlReq;

impl RawAtomicModeReq {
    pub(crate) fn add_raw_property(
        &mut self,
        object: RawResourceHandle,
        property: property::Handle,
        value: u64,
    ) {
        let property = u32::from(property);
        let (idx, prop_count) = match self.objects.binary_search(&object) {
            Ok(idx) => (idx, self.count_props_per_object[idx]),
            Err(new_idx) => {
                self.objects.insert(new_idx, object);
                self.count_props_per_object.insert(new_idx, 0);
                (new_idx, 0)
            }
        };
        let start = self.count_props_per_object[..idx].iter().sum::<u32>() as usize;
        let end = start + prop_count as usize;
        match self.props[start..end].binary_search(&property) {
            Ok(prop_idx) => self.values[start + prop_idx] = value,
            Err(prop_idx) => {
                self.count_props_per_object[idx] += 1;
                self.props.insert(start + prop_idx, property);
                self.values.insert(start + prop_idx, value);
            }
        }
    }

    /// Construct the exact ABI value consumed by `submit`.
    fn kernel_request(
        &self,
        flags: AtomicCommitFlags,
        user_data: u64,
    ) -> io::Result<drm_mode_atomic> {
        let count_objs = u32::try_from(self.objects.len())
            .map_err(|_| io::Error::other("atomic request has too many objects"))?;
        Ok(drm_mode_atomic {
            flags: flags.bits(),
            count_objs,
            objs_ptr: self.objects.as_ptr() as u64,
            count_props_ptr: self.count_props_per_object.as_ptr() as u64,
            props_ptr: self.props.as_ptr() as u64,
            prop_values_ptr: self.values.as_ptr() as u64,
            reserved: 0,
            user_data,
        })
    }

    fn submit(
        &self,
        fd: BorrowedFd<'_>,
        flags: AtomicCommitFlags,
        user_data: u64,
    ) -> io::Result<()> {
        let mut atomic = self.kernel_request(flags, user_data)?;
        // SAFETY: `atomic` points at this request's live, immutable vectors
        // for the duration of the ioctl. The kernel writes only caller-owned
        // property targets such as OUT_FENCE_PTR.
        let rc = unsafe {
            libc::ioctl(
                fd.as_raw_fd(),
                DRM_IOCTL_MODE_ATOMIC,
                &mut atomic as *mut drm_mode_atomic,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(dead_code)] // the commit producers arrive in Tasks 2–3
pub(crate) enum AtomicCommitKind {
    CursorOnly,
    GammaOnly,
    VrrOnly,
    SceneWithCursor,
    Scene,
    Async,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TearingCommitPath {
    Direct,
    Composed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TearingCommitContext {
    pub(crate) path: Option<TearingCommitPath>,
    pub(crate) present_received_at: Option<Instant>,
    pub(crate) present_received_monotonic: Option<Duration>,
    /// The retry is a Vsync commit after an attempted async ioctl.
    pub(crate) async_fallback: bool,
}

impl TearingCommitContext {
    pub(crate) const NONE: Self = Self {
        path: None,
        present_received_at: None,
        present_received_monotonic: None,
        async_fallback: false,
    };

    pub(crate) const fn direct(
        present_received_at: Option<Instant>,
        present_received_monotonic: Option<Duration>,
        async_fallback: bool,
    ) -> Self {
        Self {
            path: Some(TearingCommitPath::Direct),
            present_received_at,
            present_received_monotonic,
            async_fallback,
        }
    }

    pub(crate) const fn composed(
        present_received_at: Option<Instant>,
        present_received_monotonic: Option<Duration>,
    ) -> Self {
        Self {
            path: Some(TearingCommitPath::Composed),
            present_received_at,
            present_received_monotonic,
            async_fallback: false,
        }
    }
}

impl AtomicCommitKind {
    const fn name(self) -> &'static str {
        match self {
            Self::CursorOnly => "cursor-only",
            Self::GammaOnly => "gamma-only",
            Self::VrrOnly => "vrr-only",
            Self::SceneWithCursor => "scene-with-cursor",
            Self::Scene => "scene",
            Self::Async => "async",
        }
    }

    const fn ebusy_log_level(self) -> log::Level {
        match self {
            Self::CursorOnly | Self::GammaOnly | Self::VrrOnly => log::Level::Debug,
            Self::SceneWithCursor | Self::Scene | Self::Async => log::Level::Warn,
        }
    }

    const fn index(self) -> usize {
        match self {
            Self::CursorOnly => 0,
            Self::GammaOnly => 1,
            Self::VrrOnly => 2,
            Self::SceneWithCursor => 3,
            Self::Scene => 4,
            Self::Async => 5,
        }
    }
}

/// One cursor plane's CRTC-local position. `motion_timestamp` is captured
/// when the pointer event producing this position is handled and travels with
/// the position until an accepted commit and its retiring flip are observed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CursorPlanePosition {
    /// DRM node that owns the following CRTC/plane handles. Raw DRM handles
    /// are only unique within one device.
    pub(crate) device_key: crate::platform::drm::DrmDeviceKey,
    pub(crate) crtc: crtc::Handle,
    pub(crate) plane: plane::Handle,
    pub(crate) crtc_x: property::Handle,
    pub(crate) crtc_y: property::Handle,
    pub(crate) x: i32,
    pub(crate) y: i32,
    pub(crate) motion_timestamp: Option<Instant>,
}

impl CursorPlanePosition {
    /// Append this cursor's `CRTC_X` and `CRTC_Y` values to an atomic request.
    #[allow(dead_code)] // consumed by the scene commit path in Task 2
    pub(crate) fn add_to(&self, req: &mut RawAtomicModeReq) {
        req.add_raw_property(self.plane.into(), self.crtc_x, i64::from(self.x) as u64);
        req.add_raw_property(self.plane.into(), self.crtc_y, i64::from(self.y) as u64);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AtomicPropertyUpdate {
    pub(crate) object: RawResourceHandle,
    pub(crate) property: property::Handle,
    pub(crate) value: u64,
}

#[allow(dead_code)] // request assembly starts in Task 2
impl AtomicPropertyUpdate {
    pub(crate) fn new<H: ResourceHandle>(
        object: H,
        property: property::Handle,
        value: u64,
    ) -> Self {
        Self {
            object: object.into(),
            property,
            value,
        }
    }
}

/// An atomic request paired with its flattened updates for the test recorder.
#[allow(dead_code)] // scene and direct submitters start using this in Task 2
#[derive(Clone)]
pub(crate) struct AtomicCommitRequest {
    request: RawAtomicModeReq,
    properties: Vec<AtomicPropertyUpdate>,
    cursor_properties: Vec<(RawResourceHandle, property::Handle)>,
    /// Kernel event cookie assigned by the sink before submission.
    user_data: u64,
}

#[allow(dead_code)] // request assembly starts in Task 2
impl AtomicCommitRequest {
    pub(crate) fn new() -> Self {
        Self {
            request: RawAtomicModeReq::default(),
            properties: Vec::new(),
            cursor_properties: Vec::new(),
            user_data: 0,
        }
    }

    pub(crate) fn add_raw_property(
        &mut self,
        object: RawResourceHandle,
        property: property::Handle,
        value: u64,
    ) {
        self.request.add_raw_property(object, property, value);
        self.record_property(AtomicPropertyUpdate {
            object,
            property,
            value,
        });
    }

    pub(crate) fn add_cursor_property(
        &mut self,
        object: RawResourceHandle,
        property: property::Handle,
        value: u64,
    ) {
        self.request.add_raw_property(object, property, value);
        self.cursor_properties.push((object, property));
        self.record_property(AtomicPropertyUpdate {
            object,
            property,
            value,
        });
    }

    pub(crate) fn add_cursor_position(&mut self, position: CursorPlanePosition) {
        self.add_cursor_property(
            position.plane.into(),
            position.crtc_x,
            i64::from(position.x) as u64,
        );
        self.add_cursor_property(
            position.plane.into(),
            position.crtc_y,
            i64::from(position.y) as u64,
        );
    }

    fn record_property(&mut self, update: AtomicPropertyUpdate) {
        if let Some(previous) = self.properties.iter_mut().find(|previous| {
            previous.object == update.object && previous.property == update.property
        }) {
            previous.value = update.value;
        } else {
            self.properties.push(update);
        }
    }

    pub(crate) fn submit(&self, device: &Device, flags: AtomicCommitFlags) -> io::Result<()> {
        self.request.submit(device.as_fd(), flags, self.user_data)
    }

    /// Convert the recorded updates to the drm crate's request so operations
    /// with a small submit seam can still use `Device::atomic_commit`.
    pub(crate) fn into_atomic_mode_req(self) -> drm::control::atomic::AtomicModeReq {
        let mut request = drm::control::atomic::AtomicModeReq::new();
        for update in self.properties {
            request.add_raw_property(update.object, update.property, update.value);
        }
        request
    }

    fn attribution_snapshot(&self) -> CommitAttributionSnapshot {
        CommitAttributionSnapshot {
            properties: self.properties.clone(),
            cursor_properties: self.cursor_properties.clone(),
        }
    }
}

/// Flattened copy of a cursor-carrying scene request, kept so a rejection can
/// be re-tested with and without the cursor fields.
struct CommitAttributionSnapshot {
    properties: Vec<AtomicPropertyUpdate>,
    cursor_properties: Vec<(RawResourceHandle, property::Handle)>,
}

impl CommitAttributionSnapshot {
    fn request(&self, with_cursor: bool) -> AtomicCommitRequest {
        let mut request = AtomicCommitRequest::new();
        for update in &self.properties {
            if with_cursor
                || !self
                    .cursor_properties
                    .contains(&(update.object, update.property))
            {
                request.add_raw_property(update.object, update.property, update.value);
            }
        }
        request
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AtomicCommitEvent {
    pub(crate) crtc: crtc::Handle,
    pub(crate) motion_timestamp: Option<Instant>,
}

pub(crate) const fn event_identity_required(
    flags: AtomicCommitFlags,
    events: &[AtomicCommitEvent],
) -> bool {
    flags.contains(AtomicCommitFlags::PAGE_FLIP_EVENT)
        && !flags.contains(AtomicCommitFlags::TEST_ONLY)
        && !events.is_empty()
}

/// Injectable executor. The production executor calls the device's
/// `atomic_commit`; tests provide a recorder that accepts scripted results.
pub(crate) trait AtomicCommitExecutor {
    fn atomic_commit(
        &self,
        device: &Device,
        flags: AtomicCommitFlags,
        kind: AtomicCommitKind,
        request: AtomicCommitRequest,
        events: &[AtomicCommitEvent],
    ) -> io::Result<()>;

    fn crtc_gamma_size(&self, device: &Device, crtc: crtc::Handle) -> io::Result<u16> {
        use ::drm::control::Device as ControlDevice;

        let info = device.get_crtc(crtc)?;
        Ok(u16::try_from(info.gamma_length()).unwrap_or(u16::MAX))
    }

    fn create_property_blob(&self, device: &Device, data: &[u8]) -> io::Result<u64> {
        use ::drm::control::Device as ControlDevice;

        let blob: u64 = device.create_property_blob(data)?.into();
        Ok(blob)
    }

    fn destroy_property_blob(&self, device: &Device, blob: u64) -> io::Result<()> {
        use ::drm::control::Device as ControlDevice;

        device.destroy_property_blob(blob)
    }

    #[cfg(test)]
    fn for_tests_record_count(&self) -> Option<usize> {
        None
    }
}

#[derive(Default)]
pub(crate) struct DrmAtomicCommitExecutor;

impl AtomicCommitExecutor for DrmAtomicCommitExecutor {
    fn atomic_commit(
        &self,
        device: &Device,
        flags: AtomicCommitFlags,
        _kind: AtomicCommitKind,
        request: AtomicCommitRequest,
        _events: &[AtomicCommitEvent],
    ) -> io::Result<()> {
        request.submit(device, flags)
    }
}

/// Per-device commit wrapper. It owns the injectable executor and aggregates
/// ioctl and pointer latency samples for the existing loop telemetry.
pub(crate) struct AtomicCommitSink {
    executor: Rc<dyn AtomicCommitExecutor>,
    monotonic_clock: Rc<dyn Fn() -> Option<Duration>>,
    metrics: RefCell<CommitWindow>,
    last_rejection_was_cursor: Cell<bool>,
    tearing_on: Cell<bool>,
    tearing_generation: Cell<u64>,
}

impl AtomicCommitSink {
    /// `executor` is normally `DrmAtomicCommitExecutor`; tests can inject the
    /// cfg(test) request recorder through the same constructor.
    pub(crate) fn new(executor: Rc<dyn AtomicCommitExecutor>) -> Self {
        Self::new_with_clock(executor, Rc::new(monotonic_timestamp))
    }

    #[cfg(test)]
    pub(crate) fn for_tests_record_count(&self) -> Option<usize> {
        self.executor.for_tests_record_count()
    }

    pub(crate) fn crtc_gamma_size(&self, device: &Device, crtc: crtc::Handle) -> io::Result<u16> {
        self.executor.crtc_gamma_size(device, crtc)
    }

    pub(crate) fn create_property_blob(&self, device: &Device, data: &[u8]) -> io::Result<u64> {
        self.executor.create_property_blob(device, data)
    }

    pub(crate) fn destroy_property_blob(&self, device: &Device, blob: u64) -> io::Result<()> {
        self.executor.destroy_property_blob(device, blob)
    }

    pub(crate) fn page_flip_commit_in_flight(&self, device: &Device, crtc: crtc::Handle) -> bool {
        let key = CrtcPageFlipKey {
            device_fd: device.as_fd().as_raw_fd(),
            crtc,
        };
        self.metrics
            .borrow()
            .pending_page_flips
            .get(&key)
            .is_some_and(|pending| !pending.is_empty())
    }

    #[cfg(test)]
    pub(crate) fn new_with_monotonic_clock(
        executor: Rc<dyn AtomicCommitExecutor>,
        clock: Rc<dyn Fn() -> Option<Duration>>,
    ) -> Self {
        Self::new_with_clock(executor, clock)
    }

    fn new_with_clock(
        executor: Rc<dyn AtomicCommitExecutor>,
        monotonic_clock: Rc<dyn Fn() -> Option<Duration>>,
    ) -> Self {
        Self {
            executor,
            monotonic_clock,
            metrics: RefCell::new(CommitWindow::new(Instant::now())),
            last_rejection_was_cursor: Cell::new(false),
            tearing_on: Cell::new(false),
            tearing_generation: Cell::new(0),
        }
    }

    /// Submit an atomic request and record one diagnostic row per target CRTC.
    #[allow(dead_code)] // no cursor commit producer exists until Tasks 2–3
    pub(crate) fn submit(
        &self,
        device: &Device,
        flags: AtomicCommitFlags,
        kind: AtomicCommitKind,
        request: AtomicCommitRequest,
        events: &[AtomicCommitEvent],
    ) -> io::Result<()> {
        self.submit_with_tearing_context(
            device,
            flags,
            kind,
            request,
            events,
            TearingCommitContext::NONE,
        )
        .map(|_| ())
    }

    #[allow(dead_code)] // retained for event requests whose callers do not consume identity
    pub(crate) fn submit_tearing(
        &self,
        device: &Device,
        flags: AtomicCommitFlags,
        kind: AtomicCommitKind,
        request: AtomicCommitRequest,
        events: &[AtomicCommitEvent],
        context: TearingCommitContext,
    ) -> io::Result<()> {
        self.submit_with_tearing_context(device, flags, kind, request, events, context)
            .map(|_| ())
    }

    #[allow(dead_code)] // primary producers consume this in later split tasks
    pub(crate) fn submit_with_commit_id(
        &self,
        device: &Device,
        flags: AtomicCommitFlags,
        kind: AtomicCommitKind,
        request: AtomicCommitRequest,
        events: &[AtomicCommitEvent],
    ) -> io::Result<u64> {
        if !event_identity_required(flags, events) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "commit identity requires PAGE_FLIP_EVENT, non-TEST_ONLY flags, and target CRTCs",
            ));
        }
        self.submit_with_tearing_context(
            device,
            flags,
            kind,
            request,
            events,
            TearingCommitContext::NONE,
        )?
        .ok_or_else(|| io::Error::other("event-producing commit was accepted without an identity"))
    }

    #[allow(dead_code)] // primary producers consume this in later split tasks
    pub(crate) fn submit_tearing_with_commit_id(
        &self,
        device: &Device,
        flags: AtomicCommitFlags,
        kind: AtomicCommitKind,
        request: AtomicCommitRequest,
        events: &[AtomicCommitEvent],
        context: TearingCommitContext,
    ) -> io::Result<u64> {
        if !event_identity_required(flags, events) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "commit identity requires PAGE_FLIP_EVENT, non-TEST_ONLY flags, and target CRTCs",
            ));
        }
        self.submit_with_tearing_context(device, flags, kind, request, events, context)?
            .ok_or_else(|| {
                io::Error::other("event-producing commit was accepted without an identity")
            })
    }

    fn submit_with_tearing_context(
        &self,
        device: &Device,
        flags: AtomicCommitFlags,
        kind: AtomicCommitKind,
        mut request: AtomicCommitRequest,
        events: &[AtomicCommitEvent],
        context: TearingCommitContext,
    ) -> io::Result<Option<u64>> {
        let event_commit_id = if event_identity_required(flags, events) {
            let id = self.metrics.borrow_mut().allocate_commit_id()?;
            request.user_data = id;
            Some(id)
        } else {
            request.user_data = 0;
            None
        };
        let attribution =
            (kind == AtomicCommitKind::SceneWithCursor).then(|| request.attribution_snapshot());
        let started = Instant::now();
        let result = self
            .executor
            .atomic_commit(device, flags, kind, request, events);
        let finished = Instant::now();
        let finished_monotonic = (kind == AtomicCommitKind::Async && result.is_ok())
            .then(|| (self.monotonic_clock)())
            .flatten();
        let cursor_rejected = match (&result, attribution) {
            (Ok(()), _) => false,
            (Err(error), None) => {
                kind == AtomicCommitKind::CursorOnly && error.raw_os_error() == Some(libc::EINVAL)
            }
            (Err(error), Some(snapshot)) => self.cursor_caused_rejection(device, error, &snapshot),
        };
        self.last_rejection_was_cursor.set(cursor_rejected);
        let elapsed = finished.saturating_duration_since(started);
        let device_fd = device.as_fd().as_raw_fd();
        self.metrics.borrow_mut().record_commit(
            device_fd,
            kind,
            events,
            elapsed,
            result.as_ref().err(),
            finished,
            context,
            self.tearing_on.get(),
            self.tearing_generation.get(),
            finished_monotonic,
            event_commit_id,
        );

        let result_text = result
            .as_ref()
            .map_or_else(|error| format!("error:{error}"), |_| "ok".to_owned());
        for event in events {
            log::debug!(
                target: "yserver::kms::render::backend::kms",
                "atomic commit kind={} crtc={:?} ioctl_us={} result={result_text}",
                kind.name(),
                event.crtc,
                elapsed.as_micros(),
            );
        }
        if result
            .as_ref()
            .is_err_and(|error| error.raw_os_error() == Some(libc::EBUSY))
        {
            log::log!(
                target: "yserver::kms::render::backend::kms",
                kind.ebusy_log_level(),
                "atomic commit returned EBUSY; the CRTC already has a pending commit",
            );
        }
        self.emit_telemetry_if_due();
        result.map(|()| event_commit_id)
    }

    pub(crate) fn set_tearing_on(&self, on: bool) {
        let was_on = self.tearing_on.replace(on);
        if was_on != on {
            self.tearing_generation
                .set(self.tearing_generation.get().saturating_add(1));
        }
        if was_on && !on {
            self.metrics
                .borrow_mut()
                .reset_tearing_window(Instant::now());
        }
    }

    /// Forget accepted-commit associations when the owning CRTC event stream
    /// is explicitly discarded at an all-off lifecycle boundary.
    pub(crate) fn discard_pending_page_flip_telemetry(&self) {
        let mut metrics = self.metrics.borrow_mut();
        metrics.pending_page_flips.clear();
        metrics.reset_vrr_flip_origins();
    }

    pub(crate) fn note_async_flip_event(
        &self,
        device: &Device,
        crtc: crtc::Handle,
        vrr_committed: bool,
        timestamps: crate::kms::render::platform::FlipEventTimestamps,
    ) -> Option<u64> {
        self.metrics.borrow_mut().record_page_flip_event(
            device.as_fd().as_raw_fd(),
            crtc,
            vrr_committed,
            timestamps.commit_id,
            timestamps.received_at_monotonic,
            timestamps.kernel_ust,
            timestamps.completion_at_monotonic,
            self.tearing_on.get(),
            self.tearing_generation.get(),
        )
    }

    pub(crate) fn note_vrr_cursor_deadline_commit(&self, crtc: crtc::Handle) {
        self.metrics
            .borrow_mut()
            .record_vrr_cursor_deadline_commit(crtc);
    }

    pub(crate) fn note_skip_completion(&self) {
        let mut metrics = self.metrics.borrow_mut();
        metrics.present_latency_skip_completions =
            metrics.present_latency_skip_completions.saturating_add(1);
        if self.tearing_on.get() {
            metrics.tearing_skips = metrics.tearing_skips.saturating_add(1);
        }
    }

    pub(crate) fn note_direct_present_received(&self) {
        let mut metrics = self.metrics.borrow_mut();
        metrics.present_latency_direct_presents =
            metrics.present_latency_direct_presents.saturating_add(1);
        if self.tearing_on.get() {
            metrics.client_present_direct = metrics.client_present_direct.saturating_add(1);
        }
    }

    pub(crate) fn emit_tearing_summary_if_due(
        &self,
        device_key: crate::platform::drm::DrmDeviceKey,
        on: bool,
        avg_gpu_render_ns: u64,
    ) {
        let now = Instant::now();
        let Some(snapshot) = self
            .metrics
            .borrow_mut()
            .take_tearing_summary_if_due(now, on)
        else {
            return;
        };
        let direct_submit = latency_percentiles(&snapshot.direct_present_to_submit_us);
        let composed_submit = latency_percentiles(&snapshot.composed_present_to_submit_us);
        let direct_turnaround = latency_percentiles(&snapshot.direct_async_turnaround_us);
        let composed_turnaround = latency_percentiles(&snapshot.composed_async_turnaround_us);
        let direct_present_to_flip =
            flip_latency_percentiles(&snapshot.direct_present_to_flip_event_us);
        let composed_present_to_flip =
            flip_latency_percentiles(&snapshot.composed_present_to_flip_event_us);
        log::info!(
            target: "yserver::kms::render::backend::kms",
            "tearing_summary device={}:{} async={} async_direct_flips/s={} async_composed_flips/s={} \
             client_present_direct/s={} vsync_while_on/s={} async_einval_direct={} \
             async_einval_composed={} skip_completions/s={} \
             present_to_submit_us_direct_p50/p99/max={}/{}/{} \
             present_to_submit_us_composed_p50/p99/max={}/{}/{} \
             async_flip_turnaround_us_direct_p50/p99/max={}/{}/{} \
             async_flip_turnaround_us_composed_p50/p99/max={}/{}/{} \
             present_to_flip_event_us_direct_p50/p99/max={}/{}/{} \
             present_to_flip_event_us_composed_p50/p99/max={}/{}/{} avg_gpu_render_ns={}",
            device_key.major,
            device_key.minor,
            snapshot.async_direct.saturating_add(snapshot.async_composed),
            snapshot.async_direct,
            snapshot.async_composed,
            snapshot.client_present_direct,
            snapshot.vsync_while_on,
            snapshot.async_einval_direct,
            snapshot.async_einval_composed,
            snapshot.tearing_skips,
            direct_submit.0,
            direct_submit.1,
            direct_submit.2,
            composed_submit.0,
            composed_submit.1,
            composed_submit.2,
            direct_turnaround.0,
            direct_turnaround.1,
            direct_turnaround.2,
            composed_turnaround.0,
            composed_turnaround.1,
            composed_turnaround.2,
            direct_present_to_flip.0,
            direct_present_to_flip.1,
            direct_present_to_flip.2,
            composed_present_to_flip.0,
            composed_present_to_flip.1,
            composed_present_to_flip.2,
            avg_gpu_render_ns,
        );
    }

    pub(crate) fn emit_present_latency_summary_if_due(
        &self,
        device_key: crate::platform::drm::DrmDeviceKey,
    ) {
        let now = Instant::now();
        let Some(snapshot) = self
            .metrics
            .borrow_mut()
            .take_present_latency_summary_if_due(now)
        else {
            return;
        };
        let vrr_sections = snapshot
            .vrr_crtcs
            .iter()
            .map(|vrr| {
                format!(
                    "vrr crtc={} flip_interval_us_min/p50/max={}/{}/{} cursor_deadline_commits={}",
                    u32::from(vrr.crtc),
                    vrr.flip_interval_us_min,
                    vrr.flip_interval_us_p50,
                    vrr.flip_interval_us_max,
                    vrr.cursor_deadline_commits,
                )
            })
            .collect::<Vec<_>>()
            .join(" ");
        let vrr_sections = if vrr_sections.is_empty() {
            String::new()
        } else {
            format!(" {vrr_sections}")
        };
        log::info!(
            target: "yserver::kms::render::backend::kms",
            "present_latency_summary device={}:{} eligible_presents/s={} flips/s={} \
             skip_completions/s={} present_to_submit_us_p50/p99/max={}/{}/{} \
             present_to_flip_event_us_p50/p99/max={}/{}/{}{}",
            device_key.major,
            device_key.minor,
            snapshot.direct_presents,
            snapshot.direct_flips,
            snapshot.skip_completions,
            latency_percentiles(&snapshot.present_to_submit_us).0,
            latency_percentiles(&snapshot.present_to_submit_us).1,
            latency_percentiles(&snapshot.present_to_submit_us).2,
            flip_latency_percentiles(&snapshot.present_to_flip_event_us).0,
            flip_latency_percentiles(&snapshot.present_to_flip_event_us).1,
            flip_latency_percentiles(&snapshot.present_to_flip_event_us).2,
            vrr_sections,
        );
    }

    /// Validate a request with `TEST_ONLY`; nothing is committed, no event is
    /// requested, and the commit telemetry is untouched.
    pub(crate) fn test_only(
        &self,
        device: &Device,
        kind: AtomicCommitKind,
        request: AtomicCommitRequest,
    ) -> io::Result<()> {
        self.executor
            .atomic_commit(device, AtomicCommitFlags::TEST_ONLY, kind, request, &[])
    }

    /// A failed scene request carrying cursor fields is the cursor's fault only
    /// for EINVAL, and only if `TEST_ONLY` rejects it as sent and accepts it
    /// without those fields. Other errors belong to the scene producer.
    fn cursor_caused_rejection(
        &self,
        device: &Device,
        error: &io::Error,
        snapshot: &CommitAttributionSnapshot,
    ) -> bool {
        let cursor_rejected = error.raw_os_error() == Some(libc::EINVAL)
            && self
                .test_only(
                    device,
                    AtomicCommitKind::SceneWithCursor,
                    snapshot.request(true),
                )
                .is_err()
            && self
                .test_only(device, AtomicCommitKind::Scene, snapshot.request(false))
                .is_ok();
        log::debug!(
            target: "yserver::kms::render::backend::kms",
            "atomic scene-with-cursor commit failed ({error}); attributed to {}",
            if cursor_rejected { "the cursor" } else { "the scene" },
        );
        cursor_rejected
    }

    /// Complete motion-to-visible timing when the corresponding accepted
    /// commit's page-flip event retires.
    pub(crate) fn note_cursor_commit_retired(&self, crtc: crtc::Handle, retired_at: Instant) {
        self.metrics
            .borrow_mut()
            .record_cursor_retirement(crtc, retired_at);
        self.emit_telemetry_if_due();
    }

    pub(crate) fn emit_telemetry_if_due(&self) {
        if std::env::var_os("YSERVER_LOOP_TELEMETRY").is_none() {
            return;
        }
        let now = Instant::now();
        let Some(snapshot) = self.metrics.borrow_mut().take_if_due(now) else {
            return;
        };
        let counts = snapshot.ioctl_us.each_ref().map(Vec::len);
        let summaries = snapshot
            .ioctl_us
            .each_ref()
            .map(|values| percentile(values));
        let accept = percentile(&snapshot.motion_to_accept_us);
        let visible = percentile(&snapshot.motion_to_visible_us);
        log::info!(
            target: "yserver::kms::render::backend::kms",
            "atomic commits/s cursor_only={} gamma_only={} vrr_only={} scene_with_cursor={} scene={} async={} ebusy={} ioctl_us_max/p99 cursor_only={}/{} gamma_only={}/{} vrr_only={}/{} scene_with_cursor={}/{} scene={}/{} async={}/{} motion_to_accept_us_max/p99={}/{} motion_to_visible_us_max/p99={}/{}",
            counts[0],
            counts[1],
            counts[2],
            counts[3],
            counts[4],
            counts[5],
            snapshot.ebusy,
            summaries[0].0,
            summaries[0].1,
            summaries[1].0,
            summaries[1].1,
            summaries[2].0,
            summaries[2].1,
            summaries[3].0,
            summaries[3].1,
            summaries[4].0,
            summaries[4].1,
            summaries[5].0,
            summaries[5].1,
            accept.0,
            accept.1,
            visible.0,
            visible.1,
        );
    }

    /// Whether the most recent rejected ioctl was caused by cursor fields:
    /// cursor-only EINVAL, or a scene EINVAL confirmed by `cursor_caused_rejection`.
    /// Other failures leave the cursor state untouched.
    pub(crate) fn take_last_cursor_rejection(&self) -> bool {
        self.last_rejection_was_cursor.replace(false)
    }

    #[cfg(test)]
    pub(crate) fn for_tests_motion_to_visible_sample_count(&self) -> usize {
        self.metrics.borrow().motion_to_visible_us.len()
    }

    #[cfg(test)]
    pub(crate) fn for_tests_ebusy_count(&self) -> u64 {
        self.metrics.borrow().ebusy
    }

    #[cfg(test)]
    pub(crate) fn for_tests_tearing_counts(&self) -> TearingCounts {
        let metrics = self.metrics.borrow();
        TearingCounts {
            async_direct: metrics.async_direct,
            async_composed: metrics.async_composed,
            async_einval_direct: metrics.async_einval_direct,
            async_einval_composed: metrics.async_einval_composed,
            vsync_while_on: metrics.vsync_while_on,
            client_present_direct: metrics.client_present_direct,
            direct_turnarounds: metrics.direct_async_turnaround_us.len(),
            composed_turnarounds: metrics.composed_async_turnaround_us.len(),
            direct_present_to_flip_events: metrics.direct_present_to_flip_event_us.len(),
            composed_present_to_flip_events: metrics.composed_present_to_flip_event_us.len(),
        }
    }

    #[cfg(test)]
    pub(crate) fn for_tests_composed_turnaround_samples(&self) -> Vec<u64> {
        self.metrics.borrow().composed_async_turnaround_us.clone()
    }

    #[cfg(test)]
    pub(crate) fn for_tests_direct_present_to_flip_summary(&self) -> (String, String, String) {
        let metrics = self.metrics.borrow();
        flip_latency_percentiles(&metrics.direct_present_to_flip_event_us)
    }

    #[cfg(test)]
    pub(crate) fn for_tests_direct_present_to_flip_samples(&self) -> Vec<u64> {
        self.metrics
            .borrow()
            .direct_present_to_flip_event_us
            .clone()
    }

    #[cfg(test)]
    pub(crate) fn for_tests_direct_turnaround_samples(&self) -> Vec<u64> {
        self.metrics.borrow().direct_async_turnaround_us.clone()
    }

    #[cfg(test)]
    pub(crate) fn for_tests_pending_direct_present_received_at(
        &self,
        device: &Device,
        crtc: crtc::Handle,
    ) -> Option<Duration> {
        let key = CrtcPageFlipKey {
            device_fd: device.as_fd().as_raw_fd(),
            crtc,
        };
        self.metrics
            .borrow()
            .pending_page_flips
            .get(&key)?
            .front()?
            .direct_present_received_at
    }

    #[cfg(test)]
    pub(crate) fn for_tests_pending_page_flip_commit_ids(
        &self,
        device: &Device,
        crtc: crtc::Handle,
    ) -> Vec<u64> {
        let key = CrtcPageFlipKey {
            device_fd: device.as_fd().as_raw_fd(),
            crtc,
        };
        self.metrics
            .borrow()
            .pending_page_flips
            .get(&key)
            .map(|queue| queue.iter().map(|pending| pending.commit_id).collect())
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub(crate) fn for_tests_pending_page_flip_entry_count(&self) -> usize {
        self.metrics
            .borrow()
            .pending_page_flips
            .values()
            .map(VecDeque::len)
            .sum()
    }

    #[cfg(test)]
    pub(crate) fn for_tests_unmatched_page_flip_event_count(&self) -> u64 {
        self.metrics.borrow().unmatched_page_flip_events
    }

    #[cfg(test)]
    pub(crate) fn for_tests_present_latency_state(&self) -> (u64, u64, u64, Vec<u64>, Vec<u64>) {
        let metrics = self.metrics.borrow();
        (
            metrics.present_latency_direct_presents,
            metrics.present_latency_direct_flips,
            metrics.present_latency_skip_completions,
            metrics.present_latency_present_to_submit_us.clone(),
            metrics.present_latency_present_to_flip_event_us.clone(),
        )
    }

    #[cfg(test)]
    pub(crate) fn for_tests_take_vrr_summary_snapshot(&self) -> Option<(u32, u64, u64, u64, u64)> {
        let now = Instant::now() + Duration::from_secs(1);
        let snapshot = self
            .metrics
            .borrow_mut()
            .take_present_latency_summary_if_due(now)?;
        let vrr = snapshot.vrr_crtcs.into_iter().next()?;
        Some((
            u32::from(vrr.crtc),
            vrr.flip_interval_us_min,
            vrr.flip_interval_us_p50,
            vrr.flip_interval_us_max,
            vrr.cursor_deadline_commits,
        ))
    }

    #[cfg(test)]
    pub(crate) fn for_tests_reset_vrr_summary(&self, initial_flip_ust: Duration) {
        let mut metrics = self.metrics.borrow_mut();
        for vrr in metrics.vrr_crtcs.values_mut() {
            vrr.last_retired_flip_ust = Some(initial_flip_ust);
            vrr.observed_this_window = false;
            vrr.flip_intervals_us.clear();
            vrr.cursor_deadline_commits = 0;
        }
        metrics.present_latency_direct_presents = 0;
        metrics.present_latency_direct_flips = 0;
        metrics.present_latency_skip_completions = 0;
        metrics.present_latency_present_to_submit_us.clear();
        metrics.present_latency_present_to_flip_event_us.clear();
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TearingCounts {
    pub(crate) async_direct: u64,
    pub(crate) async_composed: u64,
    pub(crate) async_einval_direct: u64,
    pub(crate) async_einval_composed: u64,
    pub(crate) vsync_while_on: u64,
    pub(crate) client_present_direct: u64,
    pub(crate) direct_turnarounds: usize,
    pub(crate) composed_turnarounds: usize,
    pub(crate) direct_present_to_flip_events: usize,
    pub(crate) composed_present_to_flip_events: usize,
}

#[derive(Clone, Copy)]
struct PendingPageFlip {
    commit_id: u64,
    async_path: Option<TearingCommitPath>,
    async_accepted_at: Option<Duration>,
    async_present_received_at: Option<Duration>,
    tearing_generation: Option<u64>,
    direct_present_received_at: Option<Duration>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct CrtcPageFlipKey {
    pub(crate) device_fd: RawFd,
    pub(crate) crtc: crtc::Handle,
}

fn pending_page_flip_index(
    queue: &VecDeque<PendingPageFlip>,
    event_id: Option<u64>,
) -> Option<usize> {
    match event_id {
        Some(event_id) => queue
            .iter()
            .position(|pending| pending.commit_id == event_id),
        None if !queue.is_empty() => Some(0),
        None => None,
    }
}

struct CommitWindow {
    started: Instant,
    tearing_started: Instant,
    present_latency_started: Instant,
    ioctl_us: [Vec<u64>; 6],
    ebusy: u64,
    motion_to_accept_us: Vec<u64>,
    motion_to_visible_us: Vec<u64>,
    pending_motion_to_visible: HashMap<crtc::Handle, Instant>,
    async_direct: u64,
    async_composed: u64,
    vsync_while_on: u64,
    async_einval_direct: u64,
    async_einval_composed: u64,
    client_present_direct: u64,
    tearing_skips: u64,
    direct_present_to_submit_us: Vec<u64>,
    composed_present_to_submit_us: Vec<u64>,
    /// Accepted async ioctl (CLOCK_MONOTONIC) to page-flip event receipt by
    /// yserver (CLOCK_MONOTONIC), including event-delivery delay.
    direct_async_turnaround_us: Vec<u64>,
    composed_async_turnaround_us: Vec<u64>,
    /// Present receipt (CLOCK_MONOTONIC) to kernel completion timestamp
    /// normalized into CLOCK_MONOTONIC. Invalid kernel timestamps are omitted.
    direct_present_to_flip_event_us: Vec<u64>,
    composed_present_to_flip_event_us: Vec<u64>,
    present_latency_direct_presents: u64,
    present_latency_direct_flips: u64,
    present_latency_skip_completions: u64,
    present_latency_present_to_submit_us: Vec<u64>,
    present_latency_present_to_flip_event_us: Vec<u64>,
    vrr_crtcs: HashMap<crtc::Handle, VrrCrtcWindowMetrics>,
    unmatched_page_flip_events: u64,
    pending_page_flips: HashMap<CrtcPageFlipKey, VecDeque<PendingPageFlip>>,
    next_commit_id: Option<u64>,
}

#[derive(Default)]
struct VrrCrtcWindowMetrics {
    last_retired_flip_ust: Option<Duration>,
    flip_intervals_us: Vec<u64>,
    cursor_deadline_commits: u64,
    observed_this_window: bool,
}

struct CommitWindowSnapshot {
    ioctl_us: [Vec<u64>; 6],
    ebusy: u64,
    motion_to_accept_us: Vec<u64>,
    motion_to_visible_us: Vec<u64>,
}

impl CommitWindow {
    fn new(started: Instant) -> Self {
        Self {
            started,
            tearing_started: started,
            present_latency_started: started,
            ioctl_us: std::array::from_fn(|_| Vec::new()),
            ebusy: 0,
            motion_to_accept_us: Vec::new(),
            motion_to_visible_us: Vec::new(),
            pending_motion_to_visible: HashMap::new(),
            async_direct: 0,
            async_composed: 0,
            vsync_while_on: 0,
            async_einval_direct: 0,
            async_einval_composed: 0,
            client_present_direct: 0,
            tearing_skips: 0,
            direct_present_to_submit_us: Vec::new(),
            composed_present_to_submit_us: Vec::new(),
            direct_async_turnaround_us: Vec::new(),
            composed_async_turnaround_us: Vec::new(),
            direct_present_to_flip_event_us: Vec::new(),
            composed_present_to_flip_event_us: Vec::new(),
            present_latency_direct_presents: 0,
            present_latency_direct_flips: 0,
            present_latency_skip_completions: 0,
            present_latency_present_to_submit_us: Vec::new(),
            present_latency_present_to_flip_event_us: Vec::new(),
            vrr_crtcs: HashMap::new(),
            unmatched_page_flip_events: 0,
            pending_page_flips: HashMap::new(),
            next_commit_id: Some(1),
        }
    }

    fn allocate_commit_id(&mut self) -> io::Result<u64> {
        let id = self
            .next_commit_id
            .ok_or_else(|| io::Error::other("atomic commit identity space is exhausted"))?;
        if id == 0 {
            return Err(io::Error::other("atomic commit identity zero is reserved"));
        }
        self.next_commit_id = id.checked_add(1);
        Ok(id)
    }

    fn record_commit(
        &mut self,
        device_fd: RawFd,
        kind: AtomicCommitKind,
        events: &[AtomicCommitEvent],
        elapsed: Duration,
        error: Option<&io::Error>,
        finished: Instant,
        context: TearingCommitContext,
        tearing_on: bool,
        tearing_generation: u64,
        finished_monotonic: Option<Duration>,
        event_commit_id: Option<u64>,
    ) {
        let ioctl_us = duration_us(elapsed);
        self.ioctl_us[kind.index()].push(ioctl_us);
        for event in events {
            if error.is_none()
                && let Some(motion_timestamp) = event.motion_timestamp
            {
                self.motion_to_accept_us
                    .push(elapsed_us(motion_timestamp, finished));
                self.pending_motion_to_visible
                    .insert(event.crtc, motion_timestamp);
            }
        }
        if error.is_some_and(|error| error.raw_os_error() == Some(libc::EBUSY)) {
            self.ebusy = self.ebusy.saturating_add(1);
        }

        if error.is_none() {
            if context.path == Some(TearingCommitPath::Direct)
                && let Some(received_at) = context.present_received_at
            {
                self.present_latency_present_to_submit_us
                    .push(elapsed_us(received_at, finished));
            }

            if let Some(commit_id) = event_commit_id {
                for event in events {
                    self.pending_page_flips
                        .entry(CrtcPageFlipKey {
                            device_fd,
                            crtc: event.crtc,
                        })
                        .or_default()
                        .push_back(PendingPageFlip {
                            commit_id,
                            async_path: (kind == AtomicCommitKind::Async)
                                .then_some(context.path)
                                .flatten(),
                            async_accepted_at: (kind == AtomicCommitKind::Async)
                                .then_some(finished_monotonic)
                                .flatten(),
                            async_present_received_at: (kind == AtomicCommitKind::Async)
                                .then_some(context.present_received_monotonic)
                                .flatten(),
                            tearing_generation: tearing_on.then_some(tearing_generation),
                            direct_present_received_at: (context.path
                                == Some(TearingCommitPath::Direct))
                            .then_some(context.present_received_monotonic)
                            .flatten(),
                        });
                }
            }
        }

        if !tearing_on {
            return;
        }
        if kind == AtomicCommitKind::Async {
            match (error, context.path) {
                (None, Some(TearingCommitPath::Direct)) => {
                    self.async_direct = self
                        .async_direct
                        .saturating_add(u64::try_from(events.len()).unwrap_or(u64::MAX));
                    if let Some(received_at) = context.present_received_at {
                        self.direct_present_to_submit_us
                            .push(elapsed_us(received_at, finished));
                    }
                }
                (None, Some(TearingCommitPath::Composed)) => {
                    self.async_composed = self
                        .async_composed
                        .saturating_add(u64::try_from(events.len()).unwrap_or(u64::MAX));
                    if let Some(received_at) = context.present_received_at {
                        self.composed_present_to_submit_us
                            .push(elapsed_us(received_at, finished));
                    }
                }
                (Some(error), Some(TearingCommitPath::Direct))
                    if error.raw_os_error() == Some(libc::EINVAL) =>
                {
                    self.async_einval_direct = self.async_einval_direct.saturating_add(1);
                }
                (Some(error), Some(TearingCommitPath::Composed))
                    if error.raw_os_error() == Some(libc::EINVAL) =>
                {
                    self.async_einval_composed = self.async_einval_composed.saturating_add(1);
                }
                _ => {}
            }
        } else if error.is_none()
            && (matches!(
                kind,
                AtomicCommitKind::Scene | AtomicCommitKind::SceneWithCursor
            ) || context.async_fallback)
        {
            self.vsync_while_on = self.vsync_while_on.saturating_add(1);
        }
    }

    fn record_page_flip_event(
        &mut self,
        device_fd: RawFd,
        crtc: crtc::Handle,
        vrr_committed: bool,
        event_id: Option<u64>,
        event_received_at_monotonic: Option<Duration>,
        kernel_ust: Duration,
        completion_at_monotonic: Option<Duration>,
        tearing_on: bool,
        tearing_generation: u64,
    ) -> Option<u64> {
        let key = CrtcPageFlipKey { device_fd, crtc };
        let index = self
            .pending_page_flips
            .get(&key)
            .and_then(|queue| pending_page_flip_index(queue, event_id));
        let Some(index) = index else {
            if let Some(event_id) = event_id {
                self.record_unmatched_page_flip_identity(device_fd, crtc, event_id);
            }
            return None;
        };
        let queue = self.pending_page_flips.get_mut(&key)?;
        let pending = queue.remove(index)?;
        if queue.is_empty() {
            self.pending_page_flips.remove(&key);
        }

        self.record_vrr_flip_retirement(crtc, kernel_ust, vrr_committed);

        log::trace!(
            target: "yserver::kms::render::backend::kms",
            "pageflip telemetry matched commit={} crtc={crtc:?}",
            pending.commit_id,
        );
        if let Some(present_received_at) = pending.direct_present_received_at {
            self.present_latency_direct_flips = self.present_latency_direct_flips.saturating_add(1);
            if let Some(completion_at_monotonic) = completion_at_monotonic {
                self.present_latency_present_to_flip_event_us
                    .push(duration_us(
                        completion_at_monotonic.saturating_sub(present_received_at),
                    ));
            }
        }
        if !tearing_on {
            return Some(pending.commit_id);
        }
        if pending.tearing_generation != Some(tearing_generation) {
            return Some(pending.commit_id);
        }
        let Some(path) = pending.async_path else {
            return Some(pending.commit_id);
        };
        if let (Some(accepted_at), Some(event_received_at_monotonic)) =
            (pending.async_accepted_at, event_received_at_monotonic)
        {
            // This deliberately ends at userspace receipt: event-delivery delay
            // bounds when the next flip can be submitted.
            let elapsed = duration_us(event_received_at_monotonic.saturating_sub(accepted_at));
            match path {
                TearingCommitPath::Direct => self.direct_async_turnaround_us.push(elapsed),
                TearingCommitPath::Composed => self.composed_async_turnaround_us.push(elapsed),
            }
        }
        if let (Some(present_received_at), Some(completion_at_monotonic)) =
            (pending.async_present_received_at, completion_at_monotonic)
        {
            // Present receipt and the normalized kernel completion timestamp
            // are both CLOCK_MONOTONIC. Unusable kernel timestamps add no sample.
            let elapsed = duration_us(completion_at_monotonic.saturating_sub(present_received_at));
            match path {
                TearingCommitPath::Direct => self.direct_present_to_flip_event_us.push(elapsed),
                TearingCommitPath::Composed => {
                    self.composed_present_to_flip_event_us.push(elapsed);
                }
            }
        }
        Some(pending.commit_id)
    }

    fn record_vrr_flip_retirement(
        &mut self,
        crtc: crtc::Handle,
        kernel_ust: Duration,
        vrr_committed: bool,
    ) {
        if !vrr_committed {
            if let Some(metrics) = self.vrr_crtcs.get_mut(&crtc) {
                metrics.last_retired_flip_ust = None;
            }
            return;
        }

        let metrics = self.vrr_crtcs.entry(crtc).or_default();
        metrics.observed_this_window = true;
        if let Some(previous) = metrics.last_retired_flip_ust {
            if kernel_ust <= previous {
                return;
            }
            metrics
                .flip_intervals_us
                .push(duration_us(kernel_ust - previous));
        }
        metrics.last_retired_flip_ust = Some(kernel_ust);
    }

    fn record_vrr_cursor_deadline_commit(&mut self, crtc: crtc::Handle) {
        let metrics = self.vrr_crtcs.entry(crtc).or_default();
        metrics.cursor_deadline_commits = metrics.cursor_deadline_commits.saturating_add(1);
        metrics.observed_this_window = true;
    }

    fn reset_vrr_flip_origins(&mut self) {
        for metrics in self.vrr_crtcs.values_mut() {
            metrics.last_retired_flip_ust = None;
        }
    }

    fn record_unmatched_page_flip_identity(
        &mut self,
        device_fd: RawFd,
        crtc: crtc::Handle,
        event_id: u64,
    ) {
        self.unmatched_page_flip_events = self.unmatched_page_flip_events.saturating_add(1);
        log::warn!(
            target: "yserver::kms::render::backend::kms",
            "unmatched pageflip identity device_fd={device_fd} crtc={crtc:?} cookie={event_id}"
        );
    }

    fn take_tearing_summary_if_due(
        &mut self,
        now: Instant,
        on: bool,
    ) -> Option<TearingSummarySnapshot> {
        if !on {
            self.tearing_started = now;
            self.reset_tearing_counters();
            return None;
        }
        if now.saturating_duration_since(self.tearing_started) < Duration::from_secs(1) {
            return None;
        }
        self.tearing_started = now;
        Some(TearingSummarySnapshot {
            async_direct: std::mem::take(&mut self.async_direct),
            async_composed: std::mem::take(&mut self.async_composed),
            vsync_while_on: std::mem::take(&mut self.vsync_while_on),
            async_einval_direct: std::mem::take(&mut self.async_einval_direct),
            async_einval_composed: std::mem::take(&mut self.async_einval_composed),
            client_present_direct: std::mem::take(&mut self.client_present_direct),
            tearing_skips: std::mem::take(&mut self.tearing_skips),
            direct_present_to_submit_us: std::mem::take(&mut self.direct_present_to_submit_us),
            composed_present_to_submit_us: std::mem::take(&mut self.composed_present_to_submit_us),
            direct_async_turnaround_us: std::mem::take(&mut self.direct_async_turnaround_us),
            composed_async_turnaround_us: std::mem::take(&mut self.composed_async_turnaround_us),
            direct_present_to_flip_event_us: std::mem::take(
                &mut self.direct_present_to_flip_event_us,
            ),
            composed_present_to_flip_event_us: std::mem::take(
                &mut self.composed_present_to_flip_event_us,
            ),
        })
    }

    fn take_present_latency_summary_if_due(
        &mut self,
        now: Instant,
    ) -> Option<PresentLatencySummarySnapshot> {
        if now.saturating_duration_since(self.present_latency_started) < Duration::from_secs(1) {
            return None;
        }
        self.present_latency_started = now;
        let mut vrr_crtcs: Vec<_> = self
            .vrr_crtcs
            .iter_mut()
            .filter_map(|(&crtc, metrics)| {
                if !metrics.observed_this_window {
                    return None;
                }
                metrics.observed_this_window = false;
                let cursor_deadline_commits = std::mem::take(&mut metrics.cursor_deadline_commits);
                let (flip_interval_us_min, flip_interval_us_p50, flip_interval_us_max) =
                    flip_interval_min_p50_max(&metrics.flip_intervals_us);
                metrics.flip_intervals_us.clear();
                Some(VrrCrtcSummarySnapshot {
                    crtc,
                    flip_interval_us_min,
                    flip_interval_us_p50,
                    flip_interval_us_max,
                    cursor_deadline_commits,
                })
            })
            .collect();
        vrr_crtcs.sort_unstable_by_key(|vrr| u32::from(vrr.crtc));
        let snapshot = PresentLatencySummarySnapshot {
            direct_presents: std::mem::take(&mut self.present_latency_direct_presents),
            direct_flips: std::mem::take(&mut self.present_latency_direct_flips),
            skip_completions: std::mem::take(&mut self.present_latency_skip_completions),
            present_to_submit_us: std::mem::take(&mut self.present_latency_present_to_submit_us),
            present_to_flip_event_us: std::mem::take(
                &mut self.present_latency_present_to_flip_event_us,
            ),
            vrr_crtcs,
        };
        (snapshot.direct_presents > 0
            || snapshot.direct_flips > 0
            || snapshot.skip_completions > 0
            || !snapshot.vrr_crtcs.is_empty())
        .then_some(snapshot)
    }

    fn reset_tearing_counters(&mut self) {
        self.async_direct = 0;
        self.async_composed = 0;
        self.vsync_while_on = 0;
        self.async_einval_direct = 0;
        self.async_einval_composed = 0;
        self.client_present_direct = 0;
        self.tearing_skips = 0;
        self.direct_present_to_submit_us.clear();
        self.composed_present_to_submit_us.clear();
        self.direct_async_turnaround_us.clear();
        self.composed_async_turnaround_us.clear();
        self.direct_present_to_flip_event_us.clear();
        self.composed_present_to_flip_event_us.clear();
    }

    fn reset_tearing_window(&mut self, now: Instant) {
        self.tearing_started = now;
        self.reset_tearing_counters();
    }

    fn record_cursor_retirement(&mut self, crtc: crtc::Handle, retired_at: Instant) {
        if let Some(motion_timestamp) = self.pending_motion_to_visible.remove(&crtc) {
            self.motion_to_visible_us
                .push(elapsed_us(motion_timestamp, retired_at));
        }
    }

    fn take_if_due(&mut self, now: Instant) -> Option<CommitWindowSnapshot> {
        if now.saturating_duration_since(self.started) < Duration::from_secs(1) {
            return None;
        }
        self.started = now;
        Some(CommitWindowSnapshot {
            ioctl_us: std::mem::replace(&mut self.ioctl_us, std::array::from_fn(|_| Vec::new())),
            ebusy: std::mem::take(&mut self.ebusy),
            motion_to_accept_us: std::mem::take(&mut self.motion_to_accept_us),
            motion_to_visible_us: std::mem::take(&mut self.motion_to_visible_us),
        })
    }
}

struct TearingSummarySnapshot {
    async_direct: u64,
    async_composed: u64,
    vsync_while_on: u64,
    async_einval_direct: u64,
    async_einval_composed: u64,
    client_present_direct: u64,
    tearing_skips: u64,
    direct_present_to_submit_us: Vec<u64>,
    composed_present_to_submit_us: Vec<u64>,
    /// Accepted async ioctl (CLOCK_MONOTONIC) to event receipt by yserver
    /// (CLOCK_MONOTONIC); includes event-delivery delay.
    direct_async_turnaround_us: Vec<u64>,
    composed_async_turnaround_us: Vec<u64>,
    /// Present receipt (CLOCK_MONOTONIC) to kernel completion converted into
    /// CLOCK_MONOTONIC. Unusable event timestamps are omitted.
    direct_present_to_flip_event_us: Vec<u64>,
    composed_present_to_flip_event_us: Vec<u64>,
}

struct PresentLatencySummarySnapshot {
    direct_presents: u64,
    direct_flips: u64,
    skip_completions: u64,
    present_to_submit_us: Vec<u64>,
    present_to_flip_event_us: Vec<u64>,
    vrr_crtcs: Vec<VrrCrtcSummarySnapshot>,
}

struct VrrCrtcSummarySnapshot {
    crtc: crtc::Handle,
    flip_interval_us_min: u64,
    flip_interval_us_p50: u64,
    flip_interval_us_max: u64,
    cursor_deadline_commits: u64,
}

pub(crate) fn monotonic_timestamp() -> Option<Duration> {
    clock_timestamp(libc::CLOCK_MONOTONIC)
}

/// Read CLOCK_BOOTTIME at page-flip event receipt where the platform exposes
/// it. Linux DRM drivers may use this suspend-inclusive domain for event UST.
pub(crate) fn boottime_timestamp() -> Option<Duration> {
    #[cfg(target_os = "linux")]
    {
        clock_timestamp(libc::CLOCK_BOOTTIME)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

fn clock_timestamp(clock: libc::clockid_t) -> Option<Duration> {
    let mut timestamp = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `timestamp` is initialized writable storage for clock_gettime.
    let result = unsafe { libc::clock_gettime(clock, &mut timestamp) };
    if result != 0 {
        return None;
    }
    Some(Duration::new(
        u64::try_from(timestamp.tv_sec).ok()?,
        u32::try_from(timestamp.tv_nsec).ok()?,
    ))
}

fn duration_us(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

fn elapsed_us(started: Instant, finished: Instant) -> u64 {
    duration_us(finished.saturating_duration_since(started))
}

fn percentile(values: &[u64]) -> (u64, u64) {
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let max = sorted.last().copied().unwrap_or(0);
    let p99_index = sorted
        .len()
        .saturating_mul(99)
        .div_ceil(100)
        .saturating_sub(1);
    (max, sorted.get(p99_index).copied().unwrap_or(0))
}

fn latency_percentiles(values: &[u64]) -> (u64, u64, u64) {
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    if sorted.is_empty() {
        return (0, 0, 0);
    }
    let p50_index = sorted.len().div_ceil(2).saturating_sub(1);
    let p99_index = sorted
        .len()
        .saturating_mul(99)
        .div_ceil(100)
        .saturating_sub(1);
    (
        sorted[p50_index],
        sorted[p99_index],
        sorted.last().copied().unwrap_or(0),
    )
}

fn flip_interval_min_p50_max(values: &[u64]) -> (u64, u64, u64) {
    let min = values.iter().copied().min().unwrap_or(0);
    let (p50, _, max) = latency_percentiles(values);
    (min, p50, max)
}

fn flip_latency_percentiles(values: &[u64]) -> (String, String, String) {
    if values.is_empty() {
        return ("n/a".to_owned(), "n/a".to_owned(), "n/a".to_owned());
    }
    let (p50, p99, max) = latency_percentiles(values);
    (p50.to_string(), p99.to_string(), max.to_string())
}

#[cfg(test)]
#[allow(dead_code)]
pub(crate) mod tests_support {
    use super::*;
    use std::collections::VecDeque;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub(crate) enum ScriptedResult {
        Ok,
        Error {
            kind: io::ErrorKind,
            raw_os_error: Option<i32>,
        },
    }

    impl ScriptedResult {
        fn from_result(result: io::Result<()>) -> Self {
            match result {
                Ok(()) => Self::Ok,
                Err(error) => Self::Error {
                    kind: error.kind(),
                    raw_os_error: error.raw_os_error(),
                },
            }
        }

        fn into_result(self) -> io::Result<()> {
            match self {
                Self::Ok => Ok(()),
                Self::Error {
                    kind: _,
                    raw_os_error: Some(raw_os_error),
                } => Err(io::Error::from_raw_os_error(raw_os_error)),
                Self::Error {
                    kind,
                    raw_os_error: None,
                } => Err(io::Error::from(kind)),
            }
        }
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    pub(crate) struct AtomicCommitRecord {
        pub(crate) flags: AtomicCommitFlags,
        pub(crate) kind: AtomicCommitKind,
        pub(crate) properties: Vec<AtomicPropertyUpdate>,
        pub(crate) events: Vec<AtomicCommitEvent>,
        pub(crate) result: ScriptedResult,
        pub(crate) device_fd: std::os::fd::RawFd,
        pub(crate) user_data: u64,
    }

    #[derive(Default)]
    pub(crate) struct RecordingAtomicCommitExecutor {
        records: RefCell<Vec<AtomicCommitRecord>>,
        scripted_results: RefCell<VecDeque<ScriptedResult>>,
        gamma_size: Cell<Option<u16>>,
        created_property_blobs: RefCell<Vec<(u64, Vec<u8>)>>,
        destroyed_property_blobs: RefCell<Vec<u64>>,
        next_property_blob_id: Cell<u64>,
    }

    impl RecordingAtomicCommitExecutor {
        pub(crate) fn records(&self) -> Vec<AtomicCommitRecord> {
            self.records.borrow().clone()
        }

        pub(crate) fn clear_records(&self) {
            self.records.borrow_mut().clear();
        }

        pub(crate) fn set_gamma_size_for_tests(&self, size: u16) {
            self.gamma_size.set(Some(size));
        }

        pub(crate) fn created_property_blobs(&self) -> Vec<(u64, Vec<u8>)> {
            self.created_property_blobs.borrow().clone()
        }

        pub(crate) fn destroyed_property_blobs(&self) -> Vec<u64> {
            self.destroyed_property_blobs.borrow().clone()
        }

        pub(crate) fn script_result(&self, result: io::Result<()>) {
            self.scripted_results
                .borrow_mut()
                .push_back(ScriptedResult::from_result(result));
        }

        pub(crate) fn clear_scripted_results(&self) {
            self.scripted_results.borrow_mut().clear();
        }
    }

    impl AtomicCommitExecutor for RecordingAtomicCommitExecutor {
        #[cfg(test)]
        fn for_tests_record_count(&self) -> Option<usize> {
            Some(self.records.borrow().len())
        }

        fn atomic_commit(
            &self,
            device: &Device,
            flags: AtomicCommitFlags,
            kind: AtomicCommitKind,
            request: AtomicCommitRequest,
            events: &[AtomicCommitEvent],
        ) -> io::Result<()> {
            let result = self
                .scripted_results
                .borrow_mut()
                .pop_front()
                .unwrap_or(ScriptedResult::Ok);
            self.records.borrow_mut().push(AtomicCommitRecord {
                flags,
                kind,
                properties: request.properties,
                events: events.to_vec(),
                result,
                device_fd: device.as_fd().as_raw_fd(),
                user_data: request.user_data,
            });
            result.into_result()
        }

        fn crtc_gamma_size(&self, device: &Device, crtc: crtc::Handle) -> io::Result<u16> {
            if let Some(size) = self.gamma_size.get() {
                return Ok(size);
            }
            use ::drm::control::Device as ControlDevice;

            let info = device.get_crtc(crtc)?;
            Ok(u16::try_from(info.gamma_length()).unwrap_or(u16::MAX))
        }

        fn create_property_blob(&self, _device: &Device, data: &[u8]) -> io::Result<u64> {
            let blob_id = self.next_property_blob_id.get().saturating_add(1);
            self.next_property_blob_id.set(blob_id);
            self.created_property_blobs
                .borrow_mut()
                .push((blob_id, data.to_vec()));
            Ok(blob_id)
        }

        fn destroy_property_blob(&self, _device: &Device, blob: u64) -> io::Result<()> {
            self.destroyed_property_blobs.borrow_mut().push(blob);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests;
