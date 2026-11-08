//! Active-client VRR policy tests. These share the recording scene and direct
//! Present fixture stack from `vrr.rs`.

use super::*;
use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};
use yserver_core::backend::Backend;

const FAST_INTERVAL: Duration = Duration::from_nanos(33_333_333);

struct ActiveFixture {
    fixture: super::super::recording_scene::RecordingSceneFixtureGuard,
    direct: super::super::recording_scene::DirectPresentSetup,
    state: yserver_core::server::ServerState,
    property: u32,
    base: Instant,
    elapsed_ns: std::sync::Arc<AtomicU64>,
    fd: std::os::fd::RawFd,
    crtc: ::drm::control::crtc::Handle,
    initial: AtomicCommitRecord,
}

impl ActiveFixture {
    fn new(present_id: u64) -> Self {
        let (mut fixture, direct) =
            super::direct_fixture_with_vrr_mode(present_id, 0, crate::launch::VrrMode::Property)
                .expect("lavapipe recording direct fixture");
        super::set_vrr_capable(&mut fixture);
        let mut fixture = fixture.cleanup_guard();
        // start_property_direct installs the controlled paired clock before its first Present.
        let (state, property, initial, base, elapsed_ns) =
            super::start_property_direct(&mut fixture, &direct, true);
        super::complete_direct_and_drain_present(&mut fixture, &initial, 1, None);
        fixture.events.backend.platform.clear_direct_cursor_retry(0);
        fixture
            .events
            .backend
            .pstate
            .direct
            .root_mut()
            .clear_all_direct_cursor_refresh_pending();
        Backend::flush_before_damage_notify(&mut fixture.events.backend);
        fixture.events.wait_for_submitted_gpu_work();
        Backend::before_block(&mut fixture.events.backend);
        fixture.events.backend.pstate.hotplug_rescan_deadline = None;
        let mut direct = direct;
        direct.candidate.present_id = 1;
        direct.event.present_id = 1;
        let fd = super::super::direct_unflip::fd(&fixture);
        let crtc = fixture.events.backend.platform.outputs[0].output.crtc;
        Self {
            fixture,
            direct,
            state,
            property,
            base,
            elapsed_ns,
            fd,
            crtc,
            initial,
        }
    }

    fn advance(&self, duration: Duration) {
        self.elapsed_ns.fetch_add(
            u64::try_from(duration.as_nanos()).expect("test interval fits u64 nanoseconds"),
            Ordering::SeqCst,
        );
    }

    fn now(&self) -> Instant {
        self.base + Duration::from_nanos(self.elapsed_ns.load(Ordering::SeqCst))
    }

    fn present(&mut self, interval: Duration) -> AtomicCommitRecord {
        self.advance(interval);
        let previous = self.direct.candidate.present_id;
        let record = super::submit_direct_successor(&mut self.fixture, &mut self.direct, self.fd);
        let id = super::super::support::accepted_event_id(&record).expect("direct frame event ID");
        super::super::direct_unflip::complete_direct_initial(
            &mut self.fixture,
            &record,
            id,
            self.fd,
        );
        super::super::direct_unflip::drain_present_notifications(
            &mut self.fixture,
            &[self.direct.candidate.present_id],
            &[previous],
        );
        record
    }

    fn present_without_retiring(&mut self, interval: Duration) -> AtomicCommitRecord {
        self.advance(interval);
        super::submit_direct_successor(&mut self.fixture, &mut self.direct, self.fd)
    }

    fn records(&self, offset: usize) -> Vec<AtomicCommitRecord> {
        super::super::direct_unflip::records_since(&self.fixture, self.fd, offset)
    }

    fn finish(mut self) {
        super::finish_direct_scanout(&mut self.fixture, &self.direct, self.fd);
        self.fixture.finish_after_normal_drain();
    }
}

fn qualify(fixture: &mut ActiveFixture) -> [AtomicCommitRecord; 3] {
    [
        fixture.present(Duration::from_millis(16)),
        fixture.present(Duration::from_millis(16)),
        fixture.present(Duration::from_millis(16)),
    ]
}

fn accepted_idle_clear(records: &[AtomicCommitRecord], crtc: ::drm::control::crtc::Handle) -> bool {
    records.iter().any(|record| {
        record.kind == AtomicCommitKind::VrrOnly
            && super::contains_vrr_property(record, crtc, 0)
            && super::super::support::accepted_event_id(record).is_some()
    })
}

fn complete_pending_record(fixture: &mut ActiveFixture, record: &AtomicCommitRecord, msc: u32) {
    let id = super::super::support::accepted_event_id(record).expect("pending direct event ID");
    super::super::atomic_events::complete_recorded_attempt(
        &mut fixture.fixture,
        fixture.fd,
        record,
        id,
        msc,
    );
}

fn assert_no_overdue_wakeup(backend: &KmsBackend, now: Instant, context: &str) {
    assert!(
        Backend::next_wakeup(backend).is_none_or(|wakeup| wakeup > now),
        "{context}: no already-due wakeup remains"
    );
}

fn assert_idle_clear_obligation_finished(fixture: &ActiveFixture, context: &str) {
    let output = &fixture.fixture.events.backend.platform.outputs[0].output;
    assert!(
        output.vrr_clear_retry_deadline.get().is_none(),
        "{context}: no retry deadline remains after clear acceptance"
    );
    assert!(
        !output.vrr_clear_waiting_for_flip.get(),
        "{context}: no event wait remains after clear acceptance"
    );
}

fn assert_client_idle_vrr_log() {
    let logs = super::super::recording_scene::vrr_test_logs();
    assert!(logs.contains("reason=client_idle"), "{logs}");
}

fn set_async_direct_stream(fixture: &mut ActiveFixture) {
    let candidate = fixture.direct.candidate;
    let options = yserver_core::present_scheduler::PRESENT_OPTION_ASYNC;
    fixture.direct.candidate.options |= options;
    fixture.direct.event.options |= options;
    let admission = crate::kms::render::backend::TearingAdmissionContext {
        scope: crate::kms::render::backend::DirectScope::Root,
        candidate: fixture.direct.candidate,
        target: crate::kms::render::backend::ScanoutM0Target::Unredirected,
        root_host_xid: fixture.fixture.events.backend.core.window_id,
    };
    fixture.fixture.events.backend.pstate.tearing.state =
        crate::kms::render::backend::TearingState::On {
            window: candidate.dst_window_xid,
            host_window: candidate.paint_dst_host_xid,
            crtc_epoch: candidate.crtc_epoch,
            admission,
        };
    fixture.fixture.events.backend.platform.devices[0].async_page_flip = true;
}

// Mutation: drop the activity condition; one static Present would enable VRR.
#[test]
#[ignore = "needs lavapipe Vulkan scene fixture"]
fn vrr_static_window_never_enables() {
    let mut fixture = ActiveFixture::new(30_001);
    assert!(!super::contains_vrr_property(
        &fixture.initial,
        fixture.crtc,
        1
    ));
    let offset = super::super::direct_unflip::record_count(&fixture.fixture, fixture.fd);
    fixture.advance(Duration::from_millis(200));
    Backend::maybe_composite(&mut fixture.fixture.events.backend).expect("service static owner");
    assert!(
        fixture
            .records(offset)
            .iter()
            .all(|record| { !super::contains_vrr_property(record, fixture.crtc, 1) }),
        "one Present followed by 200 ms never enables VRR"
    );
    fixture.finish();
}

// Mutation: use the 1000/20 ms entry interval; 40 ms Presents must stay off.
#[test]
#[ignore = "needs lavapipe Vulkan scene fixture"]
fn vrr_slow_intervals_never_enable() {
    let mut fixture = ActiveFixture::new(30_002);
    let offset = super::super::direct_unflip::record_count(&fixture.fixture, fixture.fd);
    for _ in 0..5 {
        fixture.present(Duration::from_millis(40));
    }
    assert!(
        fixture
            .records(offset)
            .iter()
            .all(|record| { !super::contains_vrr_property(record, fixture.crtc, 1) })
    );
    fixture.finish();
}

// Mutation: count frames instead of intervals; the fourth fast Present enables.
#[test]
#[ignore = "needs lavapipe Vulkan scene fixture"]
fn vrr_enables_after_three_fast_intervals() {
    let mut fixture = ActiveFixture::new(30_003);
    assert!(!super::contains_vrr_property(
        &fixture.initial,
        fixture.crtc,
        1
    ));
    let first = fixture.present(Duration::from_millis(16));
    let second = fixture.present(Duration::from_millis(16));
    assert!(!super::contains_vrr_property(&first, fixture.crtc, 1));
    assert!(!super::contains_vrr_property(&second, fixture.crtc, 1));
    let third = fixture.present(Duration::from_millis(16));
    assert!(super::contains_vrr_property(&third, fixture.crtc, 1));
    fixture.finish();
}

// Mutation: use the 33 ms cursor constant or compare with `<` at entry.
#[test]
#[ignore = "needs lavapipe Vulkan scene fixture"]
fn vrr_entry_interval_boundary() {
    let mut inclusive = ActiveFixture::new(30_004);
    let mut record = inclusive.initial.clone();
    for _ in 0..3 {
        record = inclusive.present(FAST_INTERVAL);
    }
    assert!(super::contains_vrr_property(&record, inclusive.crtc, 1));
    inclusive.finish();

    let mut over = ActiveFixture::new(30_005);
    let mut record = over.initial.clone();
    for _ in 0..3 {
        record = over.present(Duration::from_nanos(33_333_334));
    }
    assert!(!super::contains_vrr_property(&record, over.crtc, 1));
    over.finish();
}

// Mutation: count non-consecutive fast intervals across a slow interval.
#[test]
#[ignore = "needs lavapipe Vulkan scene fixture"]
fn vrr_slow_interval_breaks_entry_streak() {
    let mut fixture = ActiveFixture::new(30_006);
    let intervals = [16, 16, 40, 16, 16, 16];
    let mut records = Vec::new();
    for milliseconds in intervals {
        records.push(fixture.present(Duration::from_millis(milliseconds)));
    }
    assert!(
        records[..5]
            .iter()
            .all(|record| { !super::contains_vrr_property(record, fixture.crtc, 1) })
    );
    assert!(super::contains_vrr_property(&records[5], fixture.crtc, 1));
    fixture.finish();
}

// Mutation: apply the entry interval while active; active 40 ms Presents must not clear.
#[test]
#[ignore = "needs lavapipe Vulkan scene fixture"]
fn vrr_active_owner_keeps_vrr_below_entry_rate() {
    let mut fixture = ActiveFixture::new(30_007);
    let active = qualify(&mut fixture);
    assert!(super::contains_vrr_property(&active[2], fixture.crtc, 1));
    let offset = super::super::direct_unflip::record_count(&fixture.fixture, fixture.fd);
    for _ in 0..3 {
        let record = fixture.present(Duration::from_millis(40));
        assert!(!super::contains_vrr_property(&record, fixture.crtc, 0));
    }
    assert!(!accepted_idle_clear(&fixture.records(offset), fixture.crtc));
    fixture.finish();
}

// Mutation: fail to renew last_present_at or add a commit at an activity renewal.
#[test]
#[ignore = "needs lavapipe Vulkan scene fixture"]
fn vrr_active_presents_renew_without_commits() {
    let mut fixture = ActiveFixture::new(30_008);
    qualify(&mut fixture);
    let offset = super::super::direct_unflip::record_count(&fixture.fixture, fixture.fd);
    for _ in 0..18 {
        let frame = fixture.present(Duration::from_millis(16));
        assert!(!super::contains_vrr_property(&frame, fixture.crtc, 0));
        assert_eq!(
            Backend::next_wakeup(&fixture.fixture.events.backend),
            Some(fixture.now() + Duration::from_millis(100)),
            "each Present renews the idle wakeup"
        );
    }
    let records = fixture.records(offset);
    assert_eq!(records.len(), 18, "only the 18 direct frames committed");
    assert!(
        records
            .iter()
            .all(|record| record.kind != AtomicCommitKind::VrrOnly)
    );
    fixture.finish();
}

// Mutation: omit the idle wakeup or change `now >= last + 100 ms` to `>`.
#[test]
#[ignore = "needs lavapipe Vulkan scene fixture"]
fn vrr_idle_wakeup_and_clear() {
    super::super::recording_scene::init_tearing_test_logger();
    let mut fixture = ActiveFixture::new(30_009);
    qualify(&mut fixture);
    super::super::recording_scene::clear_vrr_test_logs();
    let idle_at = fixture.now() + Duration::from_millis(100);
    assert_eq!(
        Backend::next_wakeup(&fixture.fixture.events.backend),
        Some(idle_at)
    );
    let offset = super::super::direct_unflip::record_count(&fixture.fixture, fixture.fd);
    fixture.advance(Duration::from_millis(99));
    Backend::maybe_composite(&mut fixture.fixture.events.backend).expect("not yet idle");
    assert!(!accepted_idle_clear(&fixture.records(offset), fixture.crtc));
    fixture.advance(Duration::from_millis(1));
    Backend::maybe_composite(&mut fixture.fixture.events.backend).expect("clear at idle boundary");
    let clear = fixture
        .records(offset)
        .into_iter()
        .find(|record| {
            record.kind == AtomicCommitKind::VrrOnly
                && super::super::support::accepted_event_id(record).is_some()
        })
        .expect("accepted standalone idle clear");
    assert!(super::contains_vrr_property(&clear, fixture.crtc, 0));
    assert!(
        clear.properties.iter().any(|update| {
            update.property
                == fixture.fixture.events.backend.platform.outputs[0]
                    .output
                    .plane_fb_id_prop
                && update.value != 0
        }),
        "idle clear keeps the live primary FB_ID"
    );
    assert!(super::super::recording_scene::vrr_test_logs().contains("reason=client_idle"));
    complete_pending_record(&mut fixture, &clear, 179);
    fixture.finish();
}

// Mutation: leave both VRR clear retry fields unset when the wakeup query first observes expiry.
#[test]
#[ignore = "needs lavapipe Vulkan scene fixture"]
fn vrr_expiry_observed_by_wakeup_keeps_clear_due() {
    super::super::recording_scene::init_tearing_test_logger();
    let mut fixture = ActiveFixture::new(30_031);
    qualify(&mut fixture);
    fixture.advance(Duration::from_millis(100));
    let boundary = fixture.now();

    assert_eq!(
        Backend::next_wakeup(&fixture.fixture.events.backend),
        Some(boundary),
        "the first expiry observation publishes an immediately due clear retry"
    );
    let offset = super::super::direct_unflip::record_count(&fixture.fixture, fixture.fd);
    Backend::maybe_composite(&mut fixture.fixture.events.backend)
        .expect("service the clear made due by next_wakeup");
    let clear = fixture
        .records(offset)
        .into_iter()
        .find(|record| accepted_idle_clear(std::slice::from_ref(record), fixture.crtc))
        .expect("wake-query expiry is followed by an accepted VRR clear");
    assert!(super::super::recording_scene::vrr_test_logs().contains("reason=client_idle"));
    complete_pending_record(&mut fixture, &clear, 179);
    assert!(
        Backend::next_wakeup(&fixture.fixture.events.backend).is_none_or(|wake| wake > boundary),
        "an accepted idle clear leaves no repeatedly overdue wakeup"
    );
    fixture.finish();
}

// Mutation: let an unqualified late async Present erase the existing clear obligation.
#[test]
#[ignore = "needs lavapipe Vulkan scene fixture"]
fn vrr_late_async_present_still_retires_idle_clear() {
    super::super::recording_scene::init_tearing_test_logger();
    let mut fixture = ActiveFixture::new(30_032);
    qualify(&mut fixture);
    set_async_direct_stream(&mut fixture);
    let offset = super::super::direct_unflip::record_count(&fixture.fixture, fixture.fd);
    let late = fixture.present_without_retiring(Duration::from_millis(120));
    assert_eq!(late.kind, AtomicCommitKind::Async);
    assert!(!super::has_vrr_property(&late, fixture.crtc));
    assert!(
        fixture.fixture.events.backend.platform.outputs[0]
            .output
            .vrr_committed
            .get(),
        "the async frame carries no CRTC clear"
    );
    assert!(
        fixture.fixture.events.backend.platform.outputs[0]
            .output
            .vrr_clear_retry_deadline
            .get()
            .is_some()
            || fixture.fixture.events.backend.platform.outputs[0]
                .output
                .vrr_clear_waiting_for_flip
                .get(),
        "expiry remains represented while the late async frame is in flight"
    );

    let late_id = super::super::support::accepted_event_id(&late).expect("late async event ID");
    complete_pending_record(&mut fixture, &late, 180);
    let late_present = fixture.direct.candidate.present_id;
    super::super::direct_unflip::drain_present_notifications(
        &mut fixture.fixture,
        &[late_present],
        &[late_present - 1],
    );
    Backend::maybe_composite(&mut fixture.fixture.events.backend)
        .expect("service the outstanding idle clear after async retirement");
    let records = fixture.records(offset);
    let clear = records
        .iter()
        .find(|record| accepted_idle_clear(std::slice::from_ref(record), fixture.crtc))
        .cloned()
        .unwrap_or_else(|| {
            let output = &fixture.fixture.events.backend.platform.outputs[0].output;
            let owner = fixture.fixture.events.backend.pstate.direct.root();
            panic!(
                "retiring late async commit {late_id} must service the idle clear without another Present; records={}, retry={:?}, waiting={}, committed={}, pending={}, queued={}, reason={:?}, damaged={}",
                records.len(),
                output.vrr_clear_retry_deadline.get(),
                output.vrr_clear_waiting_for_flip.get(),
                output.vrr_committed.get(),
                owner.pending.is_some(),
                owner.queued_successor.is_some(),
                owner.unflip_last_reason.get(),
                fixture.fixture.events.backend.store.has_pending_presentation_damage(),
            )
        });
    assert!(super::super::recording_scene::vrr_test_logs().contains("reason=client_idle"));
    complete_pending_record(&mut fixture, &clear, 181);
    fixture.finish();
}

// Mutation: label an unchanged _VARIABLE_REFRESH notification's idle clear as property.
#[test]
#[ignore = "needs lavapipe Vulkan scene fixture"]
fn vrr_idle_property_reassert_clear_logs_client_idle() {
    super::super::recording_scene::init_tearing_test_logger();
    let mut fixture = ActiveFixture::new(30_033);
    qualify(&mut fixture);
    super::super::recording_scene::clear_vrr_test_logs();
    fixture.advance(Duration::from_millis(100));
    let offset = super::super::direct_unflip::record_count(&fixture.fixture, fixture.fd);

    super::request_vrr_property(
        &mut fixture.state,
        &mut fixture.fixture.events.backend,
        fixture.direct.target_xid,
        fixture.property,
        yserver_core::xinput::XA_CARDINAL.0,
        32,
        &[1],
        9,
    );

    let clear = fixture
        .records(offset)
        .into_iter()
        .find(|record| accepted_idle_clear(std::slice::from_ref(record), fixture.crtc))
        .expect("unchanged property notification submits the idle clear immediately");
    let logs = super::super::recording_scene::vrr_test_logs();
    assert!(logs.contains("reason=client_idle"), "{logs}");
    assert!(!logs.contains("reason=property"), "{logs}");
    complete_pending_record(&mut fixture, &clear, 182);
    fixture.finish();
}

// Mutation: pass property as the reason when an immediate gamma request first observes idle expiry.
#[test]
#[ignore = "needs lavapipe Vulkan scene fixture"]
fn vrr_idle_gamma_clear_logs_client_idle() {
    super::super::recording_scene::init_tearing_test_logger();
    let mut fixture = ActiveFixture::new(30_034);
    qualify(&mut fixture);
    Backend::flush_before_damage_notify(&mut fixture.fixture.events.backend);
    fixture.fixture.events.wait_for_submitted_gpu_work();
    Backend::before_block(&mut fixture.fixture.events.backend);
    let output_key = fixture.fixture.events.backend.platform.outputs[0]
        .key
        .clone();
    let output = &fixture.fixture.events.backend.platform.outputs[0].output;
    let crtc = output.crtc;
    let recorder = Rc::clone(&fixture.fixture.events.recorders[&fixture.fd]);
    recorder.set_gamma_size_for_tests(4);
    fixture
        .fixture
        .events
        .backend
        .pstate
        .crtc_key_by_id
        .insert(u32::from(crtc), output_key);
    let gamma_deadline = fixture.now();
    {
        let output = &mut fixture.fixture.events.backend.platform.outputs[0].output;
        output.crtc_gamma_lut_prop = Some(handle(70));
        output.crtc_gamma_lut_size = Some(4);
        *output.pending_gamma_lut.borrow_mut() = Some((vec![1; 4], vec![2; 4], vec![3; 4]));
        output.pending_gamma_deadline.set(Some(gamma_deadline));
    }
    fixture.advance(Duration::from_millis(100));
    let pending_damage = fixture
        .fixture
        .events
        .backend
        .store
        .armed_damaged_ids()
        .into_iter()
        .filter_map(|drawable| {
            fixture
                .fixture
                .events
                .backend
                .store
                .peek_presentation_damage(drawable)
        })
        .collect::<Vec<_>>();
    for snapshot in pending_damage {
        fixture
            .fixture
            .events
            .backend
            .store
            .ack_presentation_damage(snapshot);
    }
    assert!(
        !fixture
            .fixture
            .events
            .backend
            .store
            .has_pending_presentation_damage(),
        "gamma route starts after fixture presentation damage is acknowledged"
    );
    super::super::recording_scene::clear_vrr_test_logs();
    let offset = super::super::direct_unflip::record_count(&fixture.fixture, fixture.fd);

    Backend::set_crtc_gamma(
        &mut fixture.fixture.events.backend,
        u32::from(crtc),
        &[0x1111; 4],
        &[0x2222; 4],
        &[0x3333; 4],
    )
    .expect("immediate gamma route discovers and clears idle VRR");

    let records = fixture.records(offset);
    let clear = records
        .into_iter()
        .find(|record| accepted_idle_clear(std::slice::from_ref(record), fixture.crtc))
        .unwrap_or_else(|| {
            let output = &fixture.fixture.events.backend.platform.outputs[0].output;
            let owner = fixture.fixture.events.backend.pstate.direct.root();
            panic!(
                "gamma route submits the idle clear before its pending LUT; attempts={}, deadline={:?}, gamma_pending={}, committed={}, reason={:?}, pending={}, queued={}, damaged={}",
                fixture.records(offset).len(),
                output.pending_gamma_deadline.get(),
                output.pending_gamma_lut.borrow().is_some(),
                output.vrr_committed.get(),
                owner.unflip_last_reason.get(),
                owner.pending.is_some(),
                owner.queued_successor.is_some(),
                fixture.fixture.events.backend.store.has_pending_presentation_damage(),
            )
        });
    let logs = super::super::recording_scene::vrr_test_logs();
    assert!(logs.contains("reason=client_idle"), "{logs}");
    assert!(!logs.contains("reason=property"), "{logs}");
    complete_pending_record(&mut fixture, &clear, 183);
    for record in fixture.records(offset).into_iter().filter(|record| {
        record.kind == AtomicCommitKind::GammaOnly
            && super::super::support::accepted_event_id(record).is_some()
    }) {
        complete_pending_record(&mut fixture, &record, 184);
    }
    fixture.finish();
}

// Mutation: expire and log activity after EINVAL has latched this output incapable.
#[test]
#[ignore = "needs lavapipe Vulkan scene fixture"]
fn vrr_runtime_einval_latch_stops_activity_tracking() {
    super::super::recording_scene::init_tearing_test_logger();
    let mut fixture = ActiveFixture::new(30_035);
    fixture.present(Duration::from_millis(16));
    fixture.present(Duration::from_millis(16));
    let recorder = Rc::clone(&fixture.fixture.events.recorders[&fixture.fd]);
    recorder.script_result(Err(std::io::Error::from_raw_os_error(libc::EINVAL)));
    fixture.present(Duration::from_millis(16));
    let output = &fixture.fixture.events.backend.platform.outputs[0].output;
    assert!(
        output.vrr_disabled.get(),
        "EINVAL latches VRR off at runtime"
    );
    assert_eq!(
        fixture
            .fixture
            .events
            .backend
            .pstate
            .direct
            .root()
            .vrr_activity
            .get()
            .qualifying_intervals,
        3,
        "the qualifying activity episode survives the enable rejection"
    );

    super::super::recording_scene::clear_vrr_test_logs();
    fixture.advance(Duration::from_millis(100));
    let now = fixture.now();
    let offset = super::super::direct_unflip::record_count(&fixture.fixture, fixture.fd);
    assert!(
        Backend::next_wakeup(&fixture.fixture.events.backend).is_none_or(|wake| wake > now),
        "a newly incapable output reports no idle activity wakeup"
    );
    Backend::maybe_composite(&mut fixture.fixture.events.backend)
        .expect("service the runtime-ineligible output");
    assert_eq!(fixture.records(offset).len(), 0, "expiry adds no commit");
    assert!(
        Backend::next_wakeup(&fixture.fixture.events.backend).is_none_or(|wake| wake > now),
        "servicing a runtime-ineligible output leaves no overdue wakeup"
    );
    let logs = super::super::recording_scene::vrr_test_logs();
    assert!(!logs.contains("vrr activity"), "{logs}");
    assert!(!logs.contains("reason=client_idle"), "{logs}");
    fixture.finish();
}

// Mutation: read a stored active bit before counting a Present after a 100 ms gap.
#[test]
#[ignore = "needs lavapipe Vulkan scene fixture"]
fn vrr_late_present_after_gap_requalifies() {
    let mut fixture = ActiveFixture::new(30_010);
    qualify(&mut fixture);
    let late = fixture.present(Duration::from_millis(120));
    assert!(super::contains_vrr_property(&late, fixture.crtc, 0));
    for _ in 0..2 {
        let frame = fixture.present(Duration::from_millis(16));
        assert!(!super::contains_vrr_property(&frame, fixture.crtc, 1));
    }
    let active = fixture.present(Duration::from_millis(16));
    assert!(super::contains_vrr_property(&active, fixture.crtc, 1));
    fixture.finish();
}

// Mutation: retain the VRR cursor hold after the activity episode expires.
#[test]
#[ignore = "needs lavapipe Vulkan scene fixture"]
fn vrr_idle_releases_cursor_hold() {
    let mut harness = super::vrr_cursor_harness(30_011, true).expect("lavapipe cursor fixture");
    let offset = super::super::direct_unflip::record_count(&harness.scene, harness.device_fd);
    let crtc = harness.scene.events.backend.platform.outputs[0].output.crtc;
    let recorder = Rc::clone(&harness.scene.events.recorders[&harness.device_fd]);
    recorder.script_result(Err(std::io::Error::from_raw_os_error(libc::EIO)));
    let elapsed = Arc::clone(&harness.elapsed_ns);
    advance_vrr_cursor_clock(&elapsed, Duration::from_millis(90));
    harness.warp_pointer(180, 130);
    Backend::maybe_composite(&mut harness.scene.events.backend).expect("hold cursor before idle");
    advance_vrr_cursor_clock(&elapsed, Duration::from_millis(10));
    Backend::maybe_composite(&mut harness.scene.events.backend).expect("release cursor at idle");
    let records = vrr_cursor_records_since(&harness, offset);
    let cursor = records
        .iter()
        .find(|record| record.kind == AtomicCommitKind::CursorOnly)
        .expect("pending cursor position leaves at idle");
    assert_eq!(cursor.result, ScriptedResult::Ok);
    assert!(records.iter().any(|record| matches!(record.result,
        ScriptedResult::Error { raw_os_error: Some(error), .. } if error == libc::EIO)));
    assert!(
        harness.scene.events.backend.platform.outputs[0]
            .output
            .vrr_clear_retry_deadline
            .get()
            .is_some()
    );
    assert!(cursor.events.iter().any(|event| event.crtc == crtc));
    harness.retire_cursor_move(130);
    harness.finish();
}

// Mutation: cache activity on a queued frame or renew it when the successor dispatches.
#[test]
#[ignore = "needs lavapipe Vulkan scene fixture"]
fn vrr_queued_frame_cannot_reenable_after_idle() {
    let mut fixture = ActiveFixture::new(30_012);
    qualify(&mut fixture);
    let pending = fixture.present_without_retiring(Duration::from_millis(16));
    super::queue_direct_successor(&mut fixture.fixture, &mut fixture.direct);
    let queued = fixture.direct.candidate.present_id;
    let pending_id = queued - 1;
    fixture.advance(Duration::from_millis(100));
    let offset = super::super::direct_unflip::record_count(&fixture.fixture, fixture.fd);
    Backend::maybe_composite(&mut fixture.fixture.events.backend).expect("expire queued owner");
    assert!(
        fixture.fixture.events.backend.platform.outputs[0]
            .output
            .vrr_clear_waiting_for_flip
            .get()
    );
    complete_pending_record(&mut fixture, &pending, 150);
    super::super::direct_unflip::drain_present_notifications(
        &mut fixture.fixture,
        &[pending_id],
        &[pending_id - 1],
    );
    let successor = fixture
        .records(offset)
        .into_iter()
        .find(|record| {
            super::super::support::accepted_event_id(record).is_some()
                && record.kind != AtomicCommitKind::VrrOnly
        })
        .expect("queued synchronous successor dispatches");
    assert!(super::contains_vrr_property(&successor, fixture.crtc, 0));
    let successor_id = super::super::support::accepted_event_id(&successor).unwrap();
    super::super::atomic_events::complete_recorded_attempt(
        &mut fixture.fixture,
        fixture.fd,
        &successor,
        successor_id,
        151,
    );
    super::super::direct_unflip::drain_present_notifications(
        &mut fixture.fixture,
        &[queued],
        &[queued - 1],
    );
    for _ in 0..3 {
        let frame = fixture.present(Duration::from_millis(16));
        assert!(!super::contains_vrr_property(&frame, fixture.crtc, 1));
    }
    let active = fixture.present(Duration::from_millis(16));
    assert!(super::contains_vrr_property(&active, fixture.crtc, 1));
    fixture.finish();
}

// Mutation: count only submitted/retired frames; latest-wins replacements are Presents too.
#[test]
#[ignore = "needs lavapipe Vulkan scene fixture"]
fn vrr_continuous_with_replaced_successors_stays_active() {
    let mut fixture = ActiveFixture::new(30_013);
    qualify(&mut fixture);
    let pending = fixture.present_without_retiring(Duration::from_millis(16));
    for _ in 0..8 {
        fixture.advance(Duration::from_millis(16));
        super::queue_direct_successor(&mut fixture.fixture, &mut fixture.direct);
        assert_eq!(
            Backend::next_wakeup(&fixture.fixture.events.backend),
            Some(fixture.now() + Duration::from_millis(100)),
            "queued Presents renew the idle deadline"
        );
    }
    assert!(
        !fixture
            .fixture
            .events
            .backend
            .pstate
            .direct
            .root()
            .deferred_successor_skips
            .is_empty()
    );
    let offset = super::super::direct_unflip::record_count(&fixture.fixture, fixture.fd);
    let queued_id = fixture.direct.candidate.present_id;
    let pending_id = queued_id - 8;
    complete_pending_record(&mut fixture, &pending, 160);
    let completed = Backend::drain_completed_present_events(&mut fixture.fixture.events.backend)
        .into_iter()
        .map(|event| event.present_id)
        .collect::<Vec<_>>();
    assert_eq!(completed, (pending_id..queued_id).collect::<Vec<_>>());
    let idled = Backend::drain_retired_present_idle_events(&mut fixture.fixture.events.backend)
        .into_iter()
        .map(|event| event.present_id)
        .collect::<Vec<_>>();
    assert_eq!(
        idled,
        (pending_id + 1..queued_id)
            .chain([pending_id - 1])
            .collect::<Vec<_>>()
    );
    let successor = fixture
        .records(offset)
        .into_iter()
        .find(|record| {
            super::super::support::accepted_event_id(record).is_some()
                && record.kind != AtomicCommitKind::VrrOnly
        })
        .expect("latest queued synchronous frame submits after retirement");
    assert!(!super::contains_vrr_property(&successor, fixture.crtc, 0));
    let id = super::super::support::accepted_event_id(&successor).unwrap();
    super::super::atomic_events::complete_recorded_attempt(
        &mut fixture.fixture,
        fixture.fd,
        &successor,
        id,
        161,
    );
    super::super::direct_unflip::drain_present_notifications(
        &mut fixture.fixture,
        &[queued_id],
        &[pending_id],
    );
    fixture.finish();
}

fn run_scheduled_present_phase(
    fixture: &mut ActiveFixture,
    mut peer: std::os::unix::net::UnixStream,
    presents: Vec<(u16, u32, u64)>,
    barrier_sequence: u16,
) -> std::os::unix::net::UnixStream {
    use std::{io::Read as _, thread};
    use yserver_core::core_loop::{
        CoreSender, Message, ResetPolicy, auth::AuthState, channel, poll_tokens::ClientIdAllocator,
        run_core,
    };
    use yserver_protocol::x11::{ClientId, RequestHeader, SequenceNumber, present};

    let (poll, sender, receiver) = channel().expect("core loop channel");
    let requests = sender.bind();
    let shutdown = sender.clone_handle();
    let window = fixture.direct.target_xid;
    let pixmap = fixture.direct.candidate.src_pixmap_xid;
    let crtc = fixture.direct.candidate.crtc_id;
    let driver = thread::spawn(move || {
        struct ShutdownOnDrop(CoreSender);
        impl Drop for ShutdownOnDrop {
            fn drop(&mut self) {
                let _ = self.0.send(Message::Shutdown);
            }
        }
        let _shutdown_on_drop = ShutdownOnDrop(shutdown);
        for (sequence, serial, target_msc) in presents {
            let mut body = vec![0_u8; 68];
            body[0..4].copy_from_slice(&window.to_le_bytes());
            body[4..8].copy_from_slice(&pixmap.to_le_bytes());
            body[8..12].copy_from_slice(&serial.to_le_bytes());
            body[24..28].copy_from_slice(&crtc.to_le_bytes());
            body[44..52].copy_from_slice(&target_msc.to_le_bytes());
            requests
                .send(Message::Request {
                    id: ClientId(1),
                    sequence: SequenceNumber(sequence),
                    accepted_at: None,
                    header: RequestHeader {
                        opcode: 145,
                        data: present::PIXMAP,
                        length_units: 18,
                    },
                    body,
                    attached_fd: None,
                })
                .expect("queue scheduled PresentPixmap");
        }
        requests
            .send(Message::Request {
                id: ClientId(1),
                sequence: SequenceNumber(barrier_sequence),
                accepted_at: None,
                header: RequestHeader {
                    opcode: 43,
                    data: 0,
                    length_units: 1,
                },
                body: Vec::new(),
                attached_fd: None,
            })
            .expect("queue scheduled Present barrier");
        loop {
            let mut first = [0_u8; 1];
            peer.read_exact(&mut first).expect("read core response");
            match first[0] {
                35 => {
                    let mut event = [0_u8; 39];
                    peer.read_exact(&mut event).expect("read Present event");
                }
                1 => {
                    let mut reply = [0_u8; 31];
                    peer.read_exact(&mut reply).expect("read barrier reply");
                    break;
                }
                response => panic!("unexpected X11 response {response} from run_core"),
            }
        }
        peer
    });

    run_core(
        poll,
        receiver,
        sender.clone_handle(),
        &mut fixture.state,
        &mut fixture.fixture.events.backend,
        [],
        &ClientIdAllocator::new(),
        AuthState::new(None),
        ResetPolicy::NoReset,
        None,
    )
    .expect("run_core processes a scheduled Present phase");
    driver.join().expect("scheduled Present phase driver")
}

// Mutation: timestamp a scheduled Present at request arrival instead of execution.
// Mutation: accept a standalone VRR_ENABLED=0 commit after the first scheduled
// enable, then let a later direct frame re-enable VRR.
#[test]
#[ignore = "needs lavapipe Vulkan scene fixture"]
fn vrr_scheduled_msc_batch_stays_active() {
    use std::sync::{Arc, Mutex};
    use yserver_core::transport::Transport;
    use yserver_protocol::x11::ClientId;

    let mut fixture = ActiveFixture::new(30_014);
    let crtc_key = crate::kms::render::platform::CrtcKey::for_output(
        &fixture.fixture.events.backend.platform.outputs[0],
    );
    let (anchor_msc, anchor_ust) = Backend::present_get_ust_msc(
        &fixture.fixture.events.backend,
        fixture.direct.candidate.crtc_id,
    );
    fixture
        .fixture
        .events
        .backend
        .platform
        .vblank_fallback
        .frozen
        .insert(
            crtc_key,
            crate::kms::render::platform::vblank_fallback::FrozenClock {
                anchor_msc: anchor_msc.max(1),
                anchor_ust,
                anchor_at: fixture.base,
                interval: Duration::from_nanos(16_666_667),
            },
        );

    let (transport, peer) = Transport::pair().expect("run_core client transport");
    peer.set_read_timeout(Some(Duration::from_secs(10)))
        .expect("bound run_core barrier waits");
    fixture
        .state
        .clients
        .get_mut(&ClientId(1).0)
        .expect("Present client")
        .writer = Arc::new(Mutex::new(transport));
    fixture.fd = fixture.fixture.events.use_raw_event_source_for_device(0);

    let first_target_msc = anchor_msc.max(1).saturating_add(2);
    let mut peer = peer;
    let mut sequence = 3_u16;
    let mut serial = 1_u32;
    let mut barrier_sequence = 100_u16;
    let mut event_msc = 190_u32;
    let mut saw_enable = false;
    let mut first_enabled_record_index = None;
    for batch in 0..4_u64 {
        let requests = (0..12_u64)
            .map(|frame| {
                let request = (sequence, serial, first_target_msc + batch * 12 + frame);
                sequence = sequence.wrapping_add(1);
                serial = serial.wrapping_add(1);
                request
            })
            .collect();
        barrier_sequence = barrier_sequence.wrapping_add(1);
        peer = run_scheduled_present_phase(&mut fixture, peer, requests, barrier_sequence);
        assert_eq!(
            fixture.state.present_pending_exec.len(),
            12,
            "each scheduled batch is parked before its MSCs arrive"
        );
        for _ in 0..12 {
            fixture.advance(Duration::from_nanos(16_666_667));
            barrier_sequence = barrier_sequence.wrapping_add(1);
            peer = run_scheduled_present_phase(&mut fixture, peer, Vec::new(), barrier_sequence);
            let pending = fixture
                .fixture
                .events
                .backend
                .pstate
                .direct
                .root()
                .pending
                .as_ref()
                .and_then(|pending| pending.accepted_event_id);
            if let Some(pending_id) = pending {
                let record = fixture
                    .records(0)
                    .into_iter()
                    .find(|record| {
                        super::super::support::accepted_event_id(record) == Some(pending_id)
                    })
                    .expect("executed scheduled Present has a commit record");
                assert_eq!(record.result, ScriptedResult::Ok);
                if saw_enable {
                    assert!(
                        !super::contains_vrr_property(&record, fixture.crtc, 0),
                        "no accepted commit after VRR enable may clear it"
                    );
                }
                if super::contains_vrr_property(&record, fixture.crtc, 1) {
                    saw_enable = true;
                    let enable_index = fixture
                        .records(0)
                        .iter()
                        .position(|attempt| {
                            super::super::support::accepted_event_id(attempt) == Some(pending_id)
                        })
                        .expect("enable commit appears in the stream");
                    first_enabled_record_index.get_or_insert(enable_index);
                }
                super::super::atomic_events::complete_recorded_attempt(
                    &mut fixture.fixture,
                    fixture.fd,
                    &record,
                    pending_id,
                    event_msc,
                );
                event_msc = event_msc.saturating_add(1);
                let completed =
                    Backend::drain_completed_present_events(&mut fixture.fixture.events.backend);
                for event in completed {
                    Backend::signal_present_wake(
                        &mut fixture.fixture.events.backend,
                        event.present_id,
                    );
                }
                let idled =
                    Backend::drain_retired_present_idle_events(&mut fixture.fixture.events.backend);
                for event in idled {
                    Backend::signal_present_wake(
                        &mut fixture.fixture.events.backend,
                        event.present_id,
                    );
                }
            }
            assert!(
                fixture
                    .fixture
                    .events
                    .backend
                    .pstate
                    .direct
                    .root()
                    .pending
                    .is_none(),
                "retire the current commit during each scheduled batch step"
            );
            if let Some(enable_index) = first_enabled_record_index {
                assert!(
                    fixture.records(0)[enable_index..].iter().all(|record| {
                        super::super::support::accepted_event_id(record).is_none()
                            || !super::contains_vrr_property(record, fixture.crtc, 0)
                    }),
                    "every accepted commit since the first enable keeps VRR enabled"
                );
            }
        }
    }

    assert!(saw_enable, "scheduled Present execution commits VRR enable");
    let enable_index = first_enabled_record_index.expect("accepted VRR enable is observed");
    assert!(
        fixture.records(0)[enable_index..].iter().all(|record| {
            super::super::support::accepted_event_id(record).is_none()
                || !super::contains_vrr_property(record, fixture.crtc, 0)
        }),
        "every accepted commit since the first enable keeps VRR enabled"
    );

    assert!(
        fixture
            .fixture
            .events
            .backend
            .live_direct_vrr_wanted_by_output()[0]
    );
    let activity = fixture
        .fixture
        .events
        .backend
        .pstate
        .direct
        .root()
        .vrr_activity
        .get();
    assert_eq!(activity.qualifying_intervals, 3);
    assert_eq!(activity.last_present_at, Some(fixture.now()));
    assert!(
        fixture
            .fixture
            .events
            .backend
            .pstate
            .direct
            .root()
            .pending
            .is_none(),
        "test teardown retires the last full-core direct flip"
    );
    Backend::drain_completed_present_events(&mut fixture.fixture.events.backend);
    Backend::drain_retired_present_idle_events(&mut fixture.fixture.events.backend);
    let current = fixture
        .fixture
        .events
        .backend
        .pstate
        .direct
        .root()
        .current
        .as_ref()
        .expect("full-core direct owner remains current");
    fixture.direct.candidate = current.candidate;
    fixture.direct.event = current.event.clone();
    fixture.finish();
}

// Mutation: execute future-MSC Presents at arrival instead of parking them; pre-execution Skip assertions must fail.
#[test]
#[ignore = "needs lavapipe Vulkan scene fixture"]
fn vrr_parked_core_skips_do_not_qualify_activity() {
    use std::sync::{Arc, Mutex};
    use yserver_core::transport::Transport;
    use yserver_protocol::x11::ClientId;

    let mut fixture = ActiveFixture::new(30_036);
    let crtc_key = crate::kms::render::platform::CrtcKey::for_output(
        &fixture.fixture.events.backend.platform.outputs[0],
    );
    let (anchor_msc, anchor_ust) = Backend::present_get_ust_msc(
        &fixture.fixture.events.backend,
        fixture.direct.candidate.crtc_id,
    );
    fixture
        .fixture
        .events
        .backend
        .platform
        .vblank_fallback
        .frozen
        .insert(
            crtc_key,
            crate::kms::render::platform::vblank_fallback::FrozenClock {
                anchor_msc: anchor_msc.max(1),
                anchor_ust,
                anchor_at: fixture.base,
                interval: Duration::from_nanos(16_666_667),
            },
        );
    let (transport, peer) = Transport::pair().expect("run_core client transport");
    peer.set_read_timeout(Some(Duration::from_secs(10)))
        .expect("bound run_core barrier waits");
    fixture
        .state
        .clients
        .get_mut(&ClientId(1).0)
        .expect("Present client")
        .writer = Arc::new(Mutex::new(transport));
    fixture.fd = fixture.fixture.events.use_raw_event_source_for_device(0);

    let activity_before = fixture
        .fixture
        .events
        .backend
        .pstate
        .direct
        .root()
        .vrr_activity
        .get();
    let activity_before = (
        activity_before.window,
        activity_before.last_present_at,
        activity_before.qualifying_intervals,
    );
    let skips_before = fixture
        .fixture
        .events
        .backend
        .telemetry
        .lifetime
        .present_skips;
    let target_msc = anchor_msc.max(1).saturating_add(10);
    let mut peer = peer;
    let mut barrier_sequence = 700_u16;
    for arrival in 0..4_u16 {
        fixture.advance(Duration::from_millis(16));
        barrier_sequence = barrier_sequence.wrapping_add(1);
        peer = run_scheduled_present_phase(
            &mut fixture,
            peer,
            vec![(3 + arrival, u32::from(arrival + 1), target_msc)],
            barrier_sequence,
        );
        assert_eq!(
            fixture.state.present_pending_exec.len(),
            1,
            "the newest same-target request remains parked"
        );
        assert_eq!(
            fixture
                .fixture
                .events
                .backend
                .telemetry
                .lifetime
                .present_skips
                - skips_before,
            u64::from(arrival),
            "each replacement emits one pre-execution Skip"
        );
        let activity = fixture
            .fixture
            .events
            .backend
            .pstate
            .direct
            .root()
            .vrr_activity
            .get();
        assert_eq!(
            (
                activity.window,
                activity.last_present_at,
                activity.qualifying_intervals,
            ),
            activity_before,
            "parked and skipped requests do not enter VRR activity"
        );
        assert!(
            !fixture.fixture.events.backend.platform.outputs[0]
                .output
                .vrr_committed
                .get()
        );
    }

    let now_msc = anchor_msc.max(1).saturating_add(3);
    let remaining_frames = target_msc.saturating_sub(now_msc);
    fixture.advance(Duration::from_nanos(
        remaining_frames.saturating_mul(16_666_667),
    ));
    barrier_sequence = barrier_sequence.wrapping_add(1);
    let _peer = run_scheduled_present_phase(&mut fixture, peer, Vec::new(), barrier_sequence);
    assert!(fixture.state.present_pending_exec.is_empty());
    let executed = fixture
        .fixture
        .events
        .backend
        .pstate
        .direct
        .root()
        .vrr_activity
        .get();
    assert_eq!(executed.qualifying_intervals, 0);
    assert_eq!(executed.last_present_at, Some(fixture.now()));
    assert!(
        !fixture.fixture.events.backend.platform.outputs[0]
            .output
            .vrr_committed
            .get()
    );

    let pending_id = fixture
        .fixture
        .events
        .backend
        .pstate
        .direct
        .root()
        .pending
        .as_ref()
        .and_then(|pending| pending.accepted_event_id)
        .expect("the last parked Present executes once at its MSC");
    let record = fixture
        .records(0)
        .into_iter()
        .find(|record| super::super::support::accepted_event_id(record) == Some(pending_id))
        .expect("executed parked request records its one direct commit");
    assert_eq!(record.result, ScriptedResult::Ok);
    assert!(!super::contains_vrr_property(&record, fixture.crtc, 1));
    super::super::atomic_events::complete_recorded_attempt(
        &mut fixture.fixture,
        fixture.fd,
        &record,
        pending_id,
        230,
    );
    for event in Backend::drain_completed_present_events(&mut fixture.fixture.events.backend) {
        Backend::signal_present_wake(&mut fixture.fixture.events.backend, event.present_id);
    }
    for event in Backend::drain_retired_present_idle_events(&mut fixture.fixture.events.backend) {
        Backend::signal_present_wake(&mut fixture.fixture.events.backend, event.present_id);
    }
    assert!(
        fixture
            .fixture
            .events
            .backend
            .pstate
            .direct
            .root()
            .pending
            .is_none()
    );
    let current = fixture
        .fixture
        .events
        .backend
        .pstate
        .direct
        .root()
        .current
        .as_ref()
        .expect("the executed request remains current");
    fixture.direct.candidate = current.candidate;
    fixture.direct.event = current.event.clone();
    fixture.finish();
}

// Mutation: count only vsync submits; async Presents also renew the live wanted state.
#[test]
#[ignore = "needs lavapipe Vulkan scene fixture"]
fn vrr_tearing_owner_counts_async_presents() {
    let options = yserver_core::present_scheduler::PRESENT_OPTION_ASYNC;
    let (mut raw, direct) =
        super::direct_fixture_with_vrr_mode(30_015, options, crate::launch::VrrMode::Property)
            .expect("lavapipe async direct fixture");
    super::set_vrr_capable(&mut raw);
    let mut raw = raw.cleanup_guard();
    let (base, elapsed_ns) =
        super::super::support::install_controlled_clock(&mut raw.events.backend);
    super::super::direct_unflip::enter_async_tearing(&mut raw, &direct);
    let mut direct = direct;
    super::prepare_property_present_source(&mut raw, &direct);
    raw.events.backend.pstate.direct.root_mut().eligible_streak =
        crate::kms::render::backend::SCANOUT_M2_ELIGIBLE_ROOT_PROBATION - 1;
    let fd = super::super::direct_unflip::fd(&raw);
    let crtc = raw.events.backend.platform.outputs[0].output.crtc;
    let offset = super::super::direct_unflip::record_count(&raw, fd);
    assert!(
        Backend::try_present_direct(
            &mut raw.events.backend,
            direct.candidate,
            direct.event.clone(),
        )
        .expect("first async direct Present")
    );
    let initial = super::super::direct_unflip::records_since(&raw, fd, offset)
        .into_iter()
        .find(|record| super::super::support::accepted_event_id(record).is_some())
        .expect("async initial direct record");
    assert_eq!(initial.kind, AtomicCommitKind::Async);
    assert!(!super::has_vrr_property(&initial, crtc));
    let initial_id = super::super::support::accepted_event_id(&initial).unwrap();
    super::super::direct_unflip::complete_direct_initial(&mut raw, &initial, initial_id, fd);
    super::super::direct_unflip::drain_present_notifications(
        &mut raw,
        &[direct.candidate.present_id],
        &[],
    );
    for _ in 0..3 {
        elapsed_ns.fetch_add(16_000_000, Ordering::SeqCst);
        let record = super::submit_direct_successor(&mut raw, &mut direct, fd);
        assert_eq!(record.kind, AtomicCommitKind::Async);
        assert!(!super::has_vrr_property(&record, crtc));
        super::super::direct_unflip::complete_direct_initial(
            &mut raw,
            &record,
            super::super::support::accepted_event_id(&record).unwrap(),
            fd,
        );
        super::super::direct_unflip::drain_present_notifications(
            &mut raw,
            &[direct.candidate.present_id],
            &[direct.candidate.present_id - 1],
        );
    }
    raw.events.backend.platform.clear_direct_cursor_retry(0);
    raw.events.backend.pstate.hotplug_rescan_deadline = None;
    Backend::flush_before_damage_notify(&mut raw.events.backend);
    raw.events.wait_for_submitted_gpu_work();
    Backend::before_block(&mut raw.events.backend);
    raw.events.backend.pstate.hotplug_rescan_deadline = None;
    let expected_wakeup =
        base + Duration::from_nanos(elapsed_ns.load(Ordering::SeqCst)) + Duration::from_millis(100);
    assert!(raw.events.backend.live_direct_vrr_wanted_by_output()[0]);
    assert_eq!(
        Backend::next_wakeup(&raw.events.backend),
        Some(expected_wakeup)
    );
    super::finish_direct_scanout(&mut raw, &direct, fd);
    raw.finish_after_normal_drain();
}

// Mutation: keep activity across a direct-scanout window change.
#[test]
#[ignore = "needs lavapipe Vulkan scene fixture"]
fn vrr_new_window_starts_new_episode() {
    let mut fixture = ActiveFixture::new(30_016);
    qualify(&mut fixture);
    let previous = fixture.direct.candidate.present_id;
    let mut next = fixture.fixture.install_direct_present(30_116, 0);
    next.candidate.variable_refresh = true;
    super::prepare_property_present_source(&mut fixture.fixture, &next);
    next.candidate.present_id = previous + 1;
    next.event.present_id = next.candidate.present_id;
    next.event.serial = fixture.direct.event.serial + 1;
    Backend::note_present_scanout_candidate(&mut fixture.fixture.events.backend, next.candidate);
    let offset = super::super::direct_unflip::record_count(&fixture.fixture, fixture.fd);
    assert!(
        Backend::try_present_direct(
            &mut fixture.fixture.events.backend,
            next.candidate,
            next.event.clone(),
        )
        .expect("new-window Present")
    );
    let record = fixture
        .records(offset)
        .into_iter()
        .find(|record| super::super::support::accepted_event_id(record).is_some())
        .expect("new-window direct frame");
    assert!(!super::contains_vrr_property(&record, fixture.crtc, 1));
    assert!(
        super::contains_vrr_property(&record, fixture.crtc, 0),
        "the new unqualified window clears the prior window's committed VRR"
    );
    let present_id = next.candidate.present_id;
    fixture.direct = next;
    complete_pending_record(&mut fixture, &record, 177);
    super::super::direct_unflip::drain_present_notifications(
        &mut fixture.fixture,
        &[present_id],
        &[previous],
    );
    fixture.finish();
}

// Mutation: retain activity after ownership teardown and same-window re-entry.
#[test]
#[ignore = "needs lavapipe Vulkan scene fixture"]
fn vrr_same_window_reentry_starts_new_episode() {
    let mut fixture = ActiveFixture::new(30_017);
    qualify(&mut fixture);
    super::finish_direct_scanout(&mut fixture.fixture, &fixture.direct, fixture.fd);
    let scene_offset = super::super::direct_unflip::record_count(&fixture.fixture, fixture.fd);
    Backend::maybe_composite(&mut fixture.fixture.events.backend)
        .expect("submit composed frame after direct teardown");
    fixture.fixture.events.wait_for_submitted_gpu_work();
    let scene_records = fixture.records(scene_offset);
    for record in scene_records.iter().filter(|record| {
        matches!(
            record.kind,
            AtomicCommitKind::Scene | AtomicCommitKind::SceneWithCursor
        ) && super::super::support::accepted_event_id(record).is_some()
    }) {
        let id = super::super::support::accepted_event_id(record).unwrap();
        super::super::atomic_events::complete_recorded_attempt(
            &mut fixture.fixture,
            fixture.fd,
            record,
            id,
            178,
        );
    }
    assert!(
        !fixture
            .fixture
            .events
            .backend
            .pstate
            .direct
            .root()
            .reentry_blocked_until_composed
    );
    let candidate = fixture.direct.candidate;
    let mut candidate = candidate;
    candidate.present_id += 1;
    let mut event = fixture.direct.event.clone();
    event.present_id = candidate.present_id;
    event.serial += 1;
    fixture
        .fixture
        .events
        .backend
        .pstate
        .direct
        .root_mut()
        .eligible_streak = crate::kms::render::backend::SCANOUT_M2_ELIGIBLE_ROOT_PROBATION - 1;
    let offset = super::super::direct_unflip::record_count(&fixture.fixture, fixture.fd);
    assert!(
        Backend::try_present_direct(
            &mut fixture.fixture.events.backend,
            candidate,
            event.clone(),
        )
        .expect("same-window re-entry")
    );
    let record = fixture
        .records(offset)
        .into_iter()
        .find(|record| super::super::support::accepted_event_id(record).is_some())
        .expect("re-entered direct frame");
    assert!(!super::contains_vrr_property(&record, fixture.crtc, 1));
    super::super::atomic_events::complete_recorded_attempt(
        &mut fixture.fixture,
        fixture.fd,
        &record,
        super::super::support::accepted_event_id(&record).unwrap(),
        179,
    );
    fixture.direct.candidate = candidate;
    fixture.direct.event = event;
    super::super::direct_unflip::request_unflip(&mut fixture.fixture, &fixture.direct);
    let offset = super::super::direct_unflip::record_count(&fixture.fixture, fixture.fd);
    let unflip = super::super::direct_unflip::grouped_unflip_record(
        &mut fixture.fixture,
        fixture.fd,
        offset,
        1,
    );
    let unflip_id = super::super::support::accepted_event_id(&unflip).unwrap();
    super::super::atomic_events::complete_recorded_attempt(
        &mut fixture.fixture,
        fixture.fd,
        &unflip,
        unflip_id,
        180,
    );
    super::super::direct_unflip::drain_present_notifications(
        &mut fixture.fixture,
        &[fixture.direct.candidate.present_id],
        &[fixture.direct.candidate.present_id],
    );
    fixture.fixture.finish_after_normal_drain();
}

// Mutation: share cadence across scopes; one output's idle clear must stay local.
#[test]
#[ignore = "needs lavapipe Vulkan scene fixture"]
fn vrr_idle_is_per_output() {
    let mut fixture = super::super::recording_scene::RecordingSceneFixture::new(2)
        .expect("two-output lavapipe fixture construction")
        .expect("lavapipe supports the two-output recording scene");
    fixture.events.backend.pstate.vrr_mode = crate::launch::VrrMode::Property;
    for output in &mut fixture.events.backend.platform.outputs {
        output.output.crtc_vrr_enabled_prop = Some(handle(VRR_PROP_ID));
        output.output.connector_vrr_capable = true;
        output.output.vrr_committed.set(false);
        output.output.vrr_disabled.set(false);
    }
    let (base, elapsed) =
        super::super::support::install_controlled_clock(&mut fixture.events.backend);
    let mut fixture = fixture.cleanup_guard();
    let mut directs = [
        fixture.install_direct_output_present(0, 30_018, 0),
        fixture.install_direct_output_present(1, 30_019, 0),
    ];
    let crtcs = [
        fixture.events.backend.platform.outputs[0].output.crtc,
        fixture.events.backend.platform.outputs[1].output.crtc,
    ];
    let first = super::super::direct_unflip::submit_output_present_via_backend(
        &mut fixture,
        &directs[0],
        0,
    );
    super::per_output::retire_output_direct(&mut fixture, &directs[0], &first, 180);
    for _ in 0..3 {
        elapsed.fetch_add(16_000_000, Ordering::SeqCst);
        let record = submit_output_successor(&mut fixture, &mut directs[0], 0);
        super::per_output::retire_output_direct(&mut fixture, &directs[0], &record, 181);
    }
    let output0_idle_at =
        base + Duration::from_nanos(elapsed.load(Ordering::SeqCst)) + Duration::from_millis(100);
    for _ in 0..4 {
        elapsed.fetch_add(16_000_000, Ordering::SeqCst);
        let record = submit_output_successor(&mut fixture, &mut directs[1], 1);
        super::per_output::retire_output_direct(&mut fixture, &directs[1], &record, 182);
    }
    let now = base + Duration::from_nanos(elapsed.load(Ordering::SeqCst));
    assert!(now < output0_idle_at, "output 1 has a later idle boundary");
    elapsed.fetch_add(
        u64::try_from(output0_idle_at.duration_since(now).as_nanos())
            .expect("output idle offset fits u64 nanoseconds"),
        Ordering::SeqCst,
    );
    let offset = super::super::direct_unflip::record_count(
        &fixture,
        super::super::direct_unflip::fd(&fixture),
    );
    Backend::maybe_composite(&mut fixture.events.backend).expect("expire output 0");
    let records = super::super::direct_unflip::records_since(
        &fixture,
        super::super::direct_unflip::fd(&fixture),
        offset,
    );
    assert!(accepted_idle_clear(&records, crtcs[0]));
    assert!(
        records
            .iter()
            .all(|record| !super::contains_vrr_property(record, crtcs[1], 0))
    );
    assert!(now < base + Duration::from_nanos(elapsed.load(Ordering::SeqCst)));
    let idle_clear = records
        .iter()
        .find(|record| accepted_idle_clear(std::slice::from_ref(record), crtcs[0]))
        .expect("output 0 idle clear record");
    let clear_id = super::super::support::accepted_event_id(idle_clear).unwrap();
    let device_fd = super::super::direct_unflip::fd(&fixture);
    super::super::atomic_events::complete_recorded_attempt(
        &mut fixture,
        device_fd,
        idle_clear,
        clear_id,
        183,
    );
    super::per_output::unmap_output_direct(&mut fixture, &directs[0], 0, 183);
    super::per_output::unmap_output_direct(&mut fixture, &directs[1], 1, 184);
    fixture.finish_after_normal_drain();
}

// Mutations: let async work discharge the clear, retry EBUSY before a flip, or cancel on property 1 without activity.
#[test]
#[ignore = "needs lavapipe Vulkan scene fixture"]
fn vrr_idle_clear_matrix() {
    super::super::recording_scene::init_tearing_test_logger();

    let mut retained = ActiveFixture::new(30_020);
    qualify(&mut retained);
    let offset = super::super::direct_unflip::record_count(&retained.fixture, retained.fd);
    let idle_at = retained.now() + Duration::from_millis(100);
    retained.advance(Duration::from_millis(99));
    assert_eq!(
        Backend::next_wakeup(&retained.fixture.events.backend),
        Some(idle_at),
        "retained current owner exposes its idle deadline"
    );
    retained.advance(Duration::from_millis(1));
    Backend::maybe_composite(&mut retained.fixture.events.backend).expect("retained current clear");
    assert!(accepted_idle_clear(
        &retained.records(offset),
        retained.crtc
    ));
    let clear = retained
        .records(offset)
        .into_iter()
        .find(|record| accepted_idle_clear(std::slice::from_ref(record), retained.crtc))
        .expect("retained idle clear event");
    assert_client_idle_vrr_log();
    assert_idle_clear_obligation_finished(&retained, "retained clear");
    assert_no_overdue_wakeup(
        &retained.fixture.events.backend,
        retained.now(),
        "retained clear commit is accepted and awaiting its event",
    );
    complete_pending_record(&mut retained, &clear, 189);
    assert_no_overdue_wakeup(
        &retained.fixture.events.backend,
        retained.now(),
        "retained current clear",
    );
    retained.finish();

    let mut pending = ActiveFixture::new(30_021);
    qualify(&mut pending);
    let frame = pending.present_without_retiring(Duration::from_millis(16));
    pending.advance(Duration::from_millis(100));
    super::super::recording_scene::clear_vrr_test_logs();
    Backend::maybe_composite(&mut pending.fixture.events.backend).expect("pending flip clear wait");
    assert!(
        pending.fixture.events.backend.platform.outputs[0]
            .output
            .vrr_clear_waiting_for_flip
            .get()
    );
    assert!(
        pending.fixture.events.backend.platform.outputs[0]
            .output
            .vrr_clear_retry_deadline
            .get()
            .is_none()
    );
    assert_no_overdue_wakeup(
        &pending.fixture.events.backend,
        pending.now(),
        "pending flip waits for retirement",
    );
    complete_pending_record(&mut pending, &frame, 190);
    let pending_present = pending.direct.candidate.present_id;
    super::super::direct_unflip::drain_present_notifications(
        &mut pending.fixture,
        &[pending_present],
        &[pending_present - 1],
    );
    let pending_clear = pending
        .records(0)
        .into_iter()
        .find(|record| accepted_idle_clear(std::slice::from_ref(record), pending.crtc))
        .expect("pending flip retires into accepted idle clear");
    assert_client_idle_vrr_log();
    assert_idle_clear_obligation_finished(&pending, "pending clear");
    complete_pending_record(&mut pending, &pending_clear, 191);
    assert_no_overdue_wakeup(
        &pending.fixture.events.backend,
        pending.now(),
        "pending flip accepted clear",
    );
    pending.finish();

    let mut sync_queued = ActiveFixture::new(30_024);
    qualify(&mut sync_queued);
    let sync_pending = sync_queued.present_without_retiring(Duration::from_millis(16));
    super::queue_direct_successor(&mut sync_queued.fixture, &mut sync_queued.direct);
    let sync_queued_id = sync_queued.direct.candidate.present_id;
    let sync_pending_id = sync_queued_id - 1;
    sync_queued.advance(Duration::from_millis(100));
    let sync_offset =
        super::super::direct_unflip::record_count(&sync_queued.fixture, sync_queued.fd);
    super::super::recording_scene::clear_vrr_test_logs();
    Backend::maybe_composite(&mut sync_queued.fixture.events.backend)
        .expect("queued synchronous clear obligation");
    assert!(
        sync_queued.fixture.events.backend.platform.outputs[0]
            .output
            .vrr_clear_waiting_for_flip
            .get()
    );
    assert_no_overdue_wakeup(
        &sync_queued.fixture.events.backend,
        sync_queued.now(),
        "queued synchronous clear waits for retirement",
    );
    complete_pending_record(&mut sync_queued, &sync_pending, 195);
    super::super::direct_unflip::drain_present_notifications(
        &mut sync_queued.fixture,
        &[sync_pending_id],
        &[sync_pending_id - 1],
    );
    let sync_successor = sync_queued
        .records(sync_offset)
        .into_iter()
        .filter(|record| record.kind != AtomicCommitKind::VrrOnly)
        .find(|record| super::super::support::accepted_event_id(record).is_some())
        .expect("queued synchronous successor accepted");
    assert!(super::contains_vrr_property(
        &sync_successor,
        sync_queued.crtc,
        0
    ));
    assert_client_idle_vrr_log();
    assert_idle_clear_obligation_finished(&sync_queued, "queued synchronous clear");
    complete_pending_record(&mut sync_queued, &sync_successor, 196);
    super::super::direct_unflip::drain_present_notifications(
        &mut sync_queued.fixture,
        &[sync_queued_id],
        &[sync_pending_id],
    );
    assert_no_overdue_wakeup(
        &sync_queued.fixture.events.backend,
        sync_queued.now(),
        "queued synchronous clear accepted",
    );
    sync_queued.finish();

    let mut async_queued = ActiveFixture::new(30_025);
    qualify(&mut async_queued);
    set_async_direct_stream(&mut async_queued);
    let async_pending = async_queued.present_without_retiring(Duration::from_millis(16));
    assert_eq!(async_pending.kind, AtomicCommitKind::Async);
    super::queue_direct_successor(&mut async_queued.fixture, &mut async_queued.direct);
    let async_queued_id = async_queued.direct.candidate.present_id;
    let async_pending_id = async_queued_id - 1;
    async_queued.advance(Duration::from_millis(100));
    let async_offset =
        super::super::direct_unflip::record_count(&async_queued.fixture, async_queued.fd);
    super::super::recording_scene::clear_vrr_test_logs();
    Backend::maybe_composite(&mut async_queued.fixture.events.backend)
        .expect("queued asynchronous clear obligation");
    assert!(
        async_queued.fixture.events.backend.platform.outputs[0]
            .output
            .vrr_clear_waiting_for_flip
            .get()
    );
    assert!(
        async_queued.fixture.events.backend.platform.outputs[0]
            .output
            .vrr_clear_retry_deadline
            .get()
            .is_none()
    );
    assert_no_overdue_wakeup(
        &async_queued.fixture.events.backend,
        async_queued.now(),
        "queued async clear waits for retirement",
    );
    complete_pending_record(&mut async_queued, &async_pending, 197);
    super::super::direct_unflip::drain_present_notifications(
        &mut async_queued.fixture,
        &[async_pending_id],
        &[async_pending_id - 1],
    );
    let async_successor = async_queued
        .records(async_offset)
        .into_iter()
        .find(|record| record.kind == AtomicCommitKind::Async)
        .expect("queued async successor accepted");
    assert!(!super::has_vrr_property(
        &async_successor,
        async_queued.crtc
    ));
    let async_successor_id = super::super::support::accepted_event_id(&async_successor).unwrap();
    super::super::atomic_events::complete_recorded_attempt(
        &mut async_queued.fixture,
        async_queued.fd,
        &async_successor,
        async_successor_id,
        198,
    );
    super::super::direct_unflip::drain_present_notifications(
        &mut async_queued.fixture,
        &[async_queued_id],
        &[async_pending_id],
    );
    let async_clear = async_queued
        .records(async_offset)
        .into_iter()
        .find(|record| accepted_idle_clear(std::slice::from_ref(record), async_queued.crtc))
        .expect("async successor retirement submits the idle clear");
    assert_client_idle_vrr_log();
    assert_idle_clear_obligation_finished(&async_queued, "queued asynchronous clear");
    complete_pending_record(&mut async_queued, &async_clear, 199);
    assert_no_overdue_wakeup(
        &async_queued.fixture.events.backend,
        async_queued.now(),
        "queued async accepted clear",
    );
    async_queued.finish();

    let mut gamma = ActiveFixture::new(30_022);
    qualify(&mut gamma);
    Backend::flush_before_damage_notify(&mut gamma.fixture.events.backend);
    gamma.fixture.events.wait_for_submitted_gpu_work();
    Backend::before_block(&mut gamma.fixture.events.backend);
    {
        let output = &mut gamma.fixture.events.backend.platform.outputs[0].output;
        output.crtc_gamma_lut_prop = Some(handle(70));
        output.crtc_gamma_lut_size = Some(4);
        *output.pending_gamma_lut.borrow_mut() = Some((vec![1; 4], vec![2; 4], vec![3; 4]));
    }
    let offset = super::super::direct_unflip::record_count(&gamma.fixture, gamma.fd);
    gamma.advance(Duration::from_millis(100));
    Backend::maybe_composite(&mut gamma.fixture.events.backend).expect("clear before gamma");
    assert!(accepted_idle_clear(&gamma.records(offset), gamma.crtc));
    assert!(
        gamma
            .records(offset)
            .iter()
            .all(|record| record.kind != AtomicCommitKind::GammaOnly)
    );
    let gamma_clear = gamma
        .records(offset)
        .into_iter()
        .find(|record| accepted_idle_clear(std::slice::from_ref(record), gamma.crtc))
        .expect("gamma case idle clear record");
    assert_client_idle_vrr_log();
    assert_idle_clear_obligation_finished(&gamma, "gamma clear");
    complete_pending_record(&mut gamma, &gamma_clear, 192);
    let gamma_records = gamma.records(offset);
    let gamma_record = gamma_records
        .iter()
        .find(|record| record.kind == AtomicCommitKind::GammaOnly)
        .cloned()
        .unwrap_or_else(|| {
            panic!(
                "gamma follows the idle clear separately; kinds={:?}, pending={}, deadline={:?}, damage={}, scene_flip={}, clear_wait={}, vrr={}",
                gamma_records.iter().map(|record| record.kind).collect::<Vec<_>>(),
                gamma.fixture.events.backend.platform.outputs[0]
                    .output.pending_gamma_lut.borrow().is_some(),
                gamma.fixture.events.backend.platform.outputs[0]
                    .output.pending_gamma_deadline.get(),
                gamma.fixture.events.backend.store.has_pending_presentation_damage(),
                gamma.fixture.events.backend.scene.has_pending_page_flip(0),
                gamma.fixture.events.backend.platform.outputs[0]
                    .output.vrr_clear_waiting_for_flip.get(),
                gamma.fixture.events.backend.platform.outputs[0]
                    .output.vrr_committed.get(),
            )
        });
    complete_pending_record(&mut gamma, &gamma_record, 193);
    assert_no_overdue_wakeup(
        &gamma.fixture.events.backend,
        gamma.now(),
        "gamma route accepted the idle clear",
    );
    gamma.finish();

    let mut transient = ActiveFixture::new(30_023);
    qualify(&mut transient);
    let recorder = Rc::clone(&transient.fixture.events.recorders[&transient.fd]);
    recorder.script_result(Err(std::io::Error::from_raw_os_error(libc::EIO)));
    let offset = super::super::direct_unflip::record_count(&transient.fixture, transient.fd);
    transient.advance(Duration::from_millis(100));
    super::super::recording_scene::clear_vrr_test_logs();
    Backend::maybe_composite(&mut transient.fixture.events.backend).expect("retryable idle clear");
    let retry_deadline = transient.fixture.events.backend.platform.outputs[0]
        .output
        .vrr_clear_retry_deadline
        .get()
        .expect("EIO leaves a clear retry deadline");
    assert!(
        transient.fixture.events.backend.platform.outputs[0]
            .output
            .vrr_clear_retry_deadline
            .get()
            .is_some()
    );
    assert_eq!(
        Backend::next_wakeup(&transient.fixture.events.backend),
        Some(retry_deadline),
        "EIO reports its retry deadline"
    );
    assert!(
        transient
            .records(offset)
            .iter()
            .any(|record| matches!(record.result,
        ScriptedResult::Error { raw_os_error: Some(error), .. } if error == libc::EIO))
    );
    transient.advance(Duration::from_millis(17));
    Backend::maybe_composite(&mut transient.fixture.events.backend).expect("retry idle clear");
    assert!(accepted_idle_clear(
        &transient.records(offset),
        transient.crtc
    ));
    let transient_clear = transient
        .records(offset)
        .into_iter()
        .find(|record| accepted_idle_clear(std::slice::from_ref(record), transient.crtc))
        .expect("transient retry accepts idle clear");
    assert_client_idle_vrr_log();
    assert_idle_clear_obligation_finished(&transient, "transient clear");
    complete_pending_record(&mut transient, &transient_clear, 194);
    assert_no_overdue_wakeup(
        &transient.fixture.events.backend,
        transient.now(),
        "transient clear accepted",
    );
    transient.finish();

    let mut ebusy = super::vrr_cursor_harness(30_027, true).expect("lavapipe EBUSY clear fixture");
    let crtc = ebusy.scene.events.backend.platform.outputs[0].output.crtc;
    let recorder = Rc::clone(&ebusy.scene.events.recorders[&ebusy.device_fd]);
    let offset = super::super::direct_unflip::record_count(&ebusy.scene, ebusy.device_fd);
    recorder.script_result(Err(std::io::Error::from_raw_os_error(libc::EBUSY)));
    advance_vrr_cursor_clock(&ebusy.elapsed_ns, Duration::from_millis(100));
    super::super::recording_scene::clear_vrr_test_logs();
    Backend::maybe_composite(&mut ebusy.scene.events.backend)
        .expect("idle clear EBUSY waits for a flip event");
    assert!(
        ebusy.scene.events.backend.platform.outputs[0]
            .output
            .vrr_clear_waiting_for_flip
            .get()
    );
    assert!(
        ebusy.scene.events.backend.platform.outputs[0]
            .output
            .vrr_clear_retry_deadline
            .get()
            .is_none()
    );
    assert_no_overdue_wakeup(
        &ebusy.scene.events.backend,
        ebusy.clock_base + Duration::from_nanos(ebusy.elapsed_ns.load(Ordering::SeqCst)),
        "EBUSY clear waits for a flip event",
    );
    assert!(
        vrr_cursor_records_since(&ebusy, offset)
            .iter()
            .any(|record| matches!(
                record.result,
                ScriptedResult::Error { raw_os_error: Some(error), .. } if error == libc::EBUSY
            ))
    );
    ebusy.warp_pointer(181, 131);
    Backend::maybe_composite(&mut ebusy.scene.events.backend)
        .expect("cursor flip supplies the EBUSY retry event");
    ebusy.retire_cursor_move(202);
    let clear = vrr_cursor_records_since(&ebusy, offset)
        .into_iter()
        .find(|record| accepted_idle_clear(std::slice::from_ref(record), crtc))
        .expect("flip event retries and accepts the idle clear");
    assert_client_idle_vrr_log();
    let ebusy_output = &ebusy.scene.events.backend.platform.outputs[0].output;
    assert!(ebusy_output.vrr_clear_retry_deadline.get().is_none());
    assert!(!ebusy_output.vrr_clear_waiting_for_flip.get());
    let device_fd = ebusy.device_fd;
    super::super::atomic_events::complete_recorded_attempt(
        &mut ebusy.scene,
        device_fd,
        &clear,
        super::super::support::accepted_event_id(&clear).expect("idle clear event ID"),
        203,
    );
    assert_no_overdue_wakeup(
        &ebusy.scene.events.backend,
        ebusy.clock_base + Duration::from_nanos(ebusy.elapsed_ns.load(Ordering::SeqCst)),
        "EBUSY clear accepted",
    );
    ebusy.finish();

    let mut property_reassert = ActiveFixture::new(30_026);
    qualify(&mut property_reassert);
    let in_flight = property_reassert.present_without_retiring(Duration::from_millis(16));
    property_reassert.advance(Duration::from_millis(100));
    super::super::recording_scene::clear_vrr_test_logs();
    Backend::maybe_composite(&mut property_reassert.fixture.events.backend)
        .expect("expire while a flip is pending");
    assert!(
        property_reassert.fixture.events.backend.platform.outputs[0]
            .output
            .vrr_clear_waiting_for_flip
            .get()
    );
    let property = property_reassert.property;
    super::request_vrr_property(
        &mut property_reassert.state,
        &mut property_reassert.fixture.events.backend,
        property_reassert.direct.target_xid,
        property,
        yserver_core::xinput::XA_CARDINAL.0,
        32,
        &[1],
        9,
    );
    assert!(
        property_reassert.fixture.events.backend.platform.outputs[0]
            .output
            .vrr_clear_waiting_for_flip
            .get()
    );
    let waiting_id = property_reassert.direct.candidate.present_id;
    complete_pending_record(&mut property_reassert, &in_flight, 200);
    super::super::direct_unflip::drain_present_notifications(
        &mut property_reassert.fixture,
        &[waiting_id],
        &[waiting_id - 1],
    );
    let clear = property_reassert
        .records(0)
        .into_iter()
        .find(|record| accepted_idle_clear(std::slice::from_ref(record), property_reassert.crtc))
        .expect("reasserted property does not cancel the idle clear");
    assert_client_idle_vrr_log();
    assert_idle_clear_obligation_finished(&property_reassert, "property reassertion clear");
    complete_pending_record(&mut property_reassert, &clear, 201);
    assert_no_overdue_wakeup(
        &property_reassert.fixture.events.backend,
        property_reassert.now(),
        "property reassertion delayed clear accepted",
    );
    property_reassert.finish();
}

// Mutation: report idle deadlines in VRR-off mode or on an incapable output.
#[test]
#[ignore = "needs lavapipe Vulkan scene fixture"]
fn vrr_off_or_incapable_reports_nothing_new() {
    for (index, mode, capability, property, latched_off) in [
        (
            0_u64,
            crate::launch::VrrMode::Off,
            true,
            Some(handle(VRR_PROP_ID)),
            false,
        ),
        (
            1,
            crate::launch::VrrMode::Property,
            false,
            Some(handle(VRR_PROP_ID)),
            false,
        ),
        (2, crate::launch::VrrMode::Property, true, None, false),
        (
            3,
            crate::launch::VrrMode::Property,
            true,
            Some(handle(VRR_PROP_ID)),
            true,
        ),
    ] {
        let (mut raw, direct) = super::direct_fixture_with_vrr_mode(30_030 + index, 0, mode)
            .expect("lavapipe VRR-off control fixture");
        let output = &mut raw.events.backend.platform.outputs[0].output;
        output.crtc_vrr_enabled_prop = property;
        output.connector_vrr_capable = capability;
        output.vrr_committed.set(false);
        output.vrr_disabled.set(latched_off);
        let mut raw = raw.cleanup_guard();
        let (mut state, _, first, base, elapsed) =
            super::start_property_direct(&mut raw, &direct, true);
        let mut direct = direct;
        let fd = super::super::direct_unflip::fd(&raw);
        direct.candidate.present_id = 1;
        direct.event.present_id = 1;
        super::super::support::accepted_event_id(&first).expect("initial direct ID");
        super::complete_direct_and_drain_present(&mut raw, &first, 1, None);
        raw.events.backend.platform.clear_direct_cursor_retry(0);
        raw.events.backend.pstate.hotplug_rescan_deadline = None;
        Backend::flush_before_damage_notify(&mut raw.events.backend);
        raw.events.wait_for_submitted_gpu_work();
        Backend::before_block(&mut raw.events.backend);
        super::super::recording_scene::init_tearing_test_logger();
        super::super::recording_scene::clear_vrr_test_logs();
        let offset = super::super::direct_unflip::record_count(&raw, fd);
        for _ in 1..=3 {
            elapsed.fetch_add(16_000_000, Ordering::SeqCst);
            let frame = super::submit_direct_successor(&mut raw, &mut direct, fd);
            let id = super::super::support::accepted_event_id(&frame).expect("direct successor ID");
            super::super::direct_unflip::complete_direct_initial(&mut raw, &frame, id, fd);
            super::super::direct_unflip::drain_present_notifications(
                &mut raw,
                &[direct.candidate.present_id],
                &[direct.candidate.present_id - 1],
            );
        }
        let idle_deadline = base
            + Duration::from_nanos(elapsed.load(Ordering::SeqCst))
            + Duration::from_millis(100);
        let before_service = Backend::next_wakeup(&raw.events.backend);
        assert_ne!(before_service, Some(idle_deadline));
        elapsed.fetch_add(200_000_000, Ordering::SeqCst);
        Backend::maybe_composite(&mut raw.events.backend)
            .expect("service beyond the inactive idle boundary");
        let candidate_records = super::super::direct_unflip::records_since(&raw, fd, offset);
        assert_eq!(
            Backend::next_wakeup(&raw.events.backend),
            before_service,
            "off/incapable activity service adds no wakeup in row {index}"
        );
        assert_no_overdue_wakeup(
            &raw.events.backend,
            base + Duration::from_nanos(elapsed.load(Ordering::SeqCst)),
            "off/incapable activity service",
        );
        assert!(
            !raw.events.backend.platform.outputs[0]
                .output
                .vrr_committed
                .get()
        );
        let logs = super::super::recording_scene::vrr_test_logs();
        assert!(!logs.contains("reason=client_idle"));
        assert!(!logs.contains("vrr activity"));

        let (mut baseline, baseline_direct) =
            super::direct_fixture_with_vrr_mode(30_040 + index, 0, crate::launch::VrrMode::Off)
                .expect("lavapipe VRR-off baseline fixture");
        let baseline_output = &mut baseline.events.backend.platform.outputs[0].output;
        baseline_output.crtc_vrr_enabled_prop = property;
        baseline_output.connector_vrr_capable = capability;
        baseline_output.vrr_committed.set(false);
        baseline_output.vrr_disabled.set(latched_off);
        let mut baseline = baseline.cleanup_guard();
        let (mut baseline_state, _, baseline_first, _baseline_base, baseline_elapsed) =
            super::start_property_direct(&mut baseline, &baseline_direct, true);
        let mut baseline_direct = baseline_direct;
        let baseline_fd = super::super::direct_unflip::fd(&baseline);
        baseline_direct.candidate.present_id = 1;
        baseline_direct.event.present_id = 1;
        super::complete_direct_and_drain_present(&mut baseline, &baseline_first, 1, None);
        baseline
            .events
            .backend
            .platform
            .clear_direct_cursor_retry(0);
        baseline.events.backend.pstate.hotplug_rescan_deadline = None;
        Backend::flush_before_damage_notify(&mut baseline.events.backend);
        baseline.events.wait_for_submitted_gpu_work();
        Backend::before_block(&mut baseline.events.backend);
        let baseline_offset = super::super::direct_unflip::record_count(&baseline, baseline_fd);
        for _ in 1..=3 {
            baseline_elapsed.fetch_add(16_000_000, Ordering::SeqCst);
            let frame =
                super::submit_direct_successor(&mut baseline, &mut baseline_direct, baseline_fd);
            let id = super::super::support::accepted_event_id(&frame)
                .expect("baseline direct successor ID");
            super::super::direct_unflip::complete_direct_initial(
                &mut baseline,
                &frame,
                id,
                baseline_fd,
            );
            super::super::direct_unflip::drain_present_notifications(
                &mut baseline,
                &[baseline_direct.candidate.present_id],
                &[baseline_direct.candidate.present_id - 1],
            );
        }
        let baseline_wakeup = Backend::next_wakeup(&baseline.events.backend);
        baseline_elapsed.fetch_add(200_000_000, Ordering::SeqCst);
        Backend::maybe_composite(&mut baseline.events.backend)
            .expect("service baseline beyond idle boundary");
        let baseline_records =
            super::super::direct_unflip::records_since(&baseline, baseline_fd, baseline_offset);
        assert_eq!(
            request_signatures(&candidate_records),
            super::request_signatures(&baseline_records),
            "off/incapable row {index} preserves baseline commit requests"
        );
        assert_eq!(
            Backend::next_wakeup(&baseline.events.backend),
            baseline_wakeup,
            "baseline service itself adds no wakeup"
        );
        super::finish_direct_scanout(&mut raw, &direct, fd);
        raw.finish_after_normal_drain();
        let _ = (&mut state, &mut baseline_state);
        super::finish_direct_scanout(&mut baseline, &baseline_direct, baseline_fd);
        baseline.finish_after_normal_drain();
    }
}

pub(super) fn submit_output_successor(
    fixture: &mut super::super::recording_scene::RecordingSceneFixtureGuard,
    direct: &mut super::super::recording_scene::DirectPresentSetup,
    output_idx: usize,
) -> AtomicCommitRecord {
    let mut candidate = direct.candidate;
    candidate.present_id += 1;
    let mut event = direct.event.clone();
    event.present_id = candidate.present_id;
    event.serial += 1;
    let fd = super::super::direct_unflip::fd(fixture);
    let offset = super::super::direct_unflip::record_count(fixture, fd);
    fixture.events.backend.for_tests_set_scanout_m1_prober(
        super::super::super::present_scanout::record_scanout_m1_probe,
    );
    Backend::note_present_scanout_candidate(&mut fixture.events.backend, candidate);
    let mut accepted = false;
    for _ in 0..8 {
        accepted =
            Backend::try_present_direct(&mut fixture.events.backend, candidate, event.clone())
                .expect("output successor through Backend");
        if accepted {
            break;
        }
    }
    assert!(
        accepted,
        "output successor accepted (current={}, pending={}, queued={}, unflip={}, probation={})",
        fixture.events.backend.pstate.direct.outputs[output_idx]
            .current
            .is_some(),
        fixture.events.backend.pstate.direct.outputs[output_idx]
            .pending
            .is_some(),
        fixture.events.backend.pstate.direct.outputs[output_idx]
            .queued_successor
            .is_some(),
        fixture.events.backend.pstate.direct.outputs[output_idx].unflip_requested,
        fixture.events.backend.pstate.direct.outputs[output_idx].eligible_streak,
    );
    direct.candidate = candidate;
    direct.event = event;
    super::super::direct_unflip::records_since(fixture, fd, offset)
        .into_iter()
        .find(|record| {
            super::super::support::accepted_event_id(record).is_some()
                && record.events.iter().any(|event| {
                    event.crtc
                        == fixture.events.backend.platform.outputs[output_idx]
                            .output
                            .crtc
                })
        })
        .expect("output successor commit")
}

pub(super) fn qualify_output_successors(
    fixture: &mut super::super::recording_scene::RecordingSceneFixtureGuard,
    direct: &mut super::super::recording_scene::DirectPresentSetup,
    output_idx: usize,
    elapsed_ns: &AtomicU64,
    first_msc: u32,
) -> AtomicCommitRecord {
    let mut record = None;
    for interval in 0..3 {
        elapsed_ns.fetch_add(16_000_000, Ordering::SeqCst);
        let successor = submit_output_successor(fixture, direct, output_idx);
        super::per_output::retire_output_direct(fixture, direct, &successor, first_msc + interval);
        record = Some(successor);
    }
    record.expect("three qualifying output intervals")
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: use completion_at_monotonic for VRR intervals; both 2-second offsets must still count.
fn vrr_summary_intervals_use_raw_ust() {
    for (ust_start, received_start, expected_max_us, offset_name) in [
        (
            Duration::from_micros(10_001),
            Duration::from_micros(2_010_001),
            10_000,
            "2 seconds behind receipt",
        ),
        (
            Duration::from_micros(2_010_001),
            Duration::from_micros(10_001),
            2_010_000,
            "2 seconds ahead of receipt",
        ),
    ] {
        let Some(mut harness) = vrr_cursor_harness(18_032, true) else {
            eprintln!("SKIP vrr_summary_intervals_use_raw_ust: no Vulkan scene fixture available");
            return;
        };
        let output_crtc = harness.scene.events.backend.platform.outputs[0].output.crtc;
        let device_fd = harness.device_fd;
        let sink = Rc::clone(&harness.scene.events.backend.platform.devices[0].atomic_commit_sink);
        let mut direct = harness.direct.take().expect("direct scanout is active");
        sink.for_tests_reset_vrr_summary(Duration::from_micros(1));

        for (frame, ust_step_us, received_step_ms) in [
            (10_u32, 0_u64, 0_u64),
            (20, 10_000, 10),
            (30, 5_000, 20),
            (40, 20_000, 30),
        ] {
            let previous_present = direct.candidate.present_id;
            let record = submit_direct_successor(&mut harness.scene, &mut direct, device_fd);
            assert_eq!(record.events.len(), 1, "direct frame targets the VRR CRTC");
            let commit_id = super::super::support::accepted_event_id(&record)
                .expect("direct frame has an accepted commit ID");
            let ust = ust_start + Duration::from_micros(ust_step_us);
            let received = received_start + Duration::from_millis(received_step_ms);
            {
                let fixture = &mut harness.scene;
                fixture.events.wait_for_submitted_gpu_work();
                fixture.events.queue_commit_completion_at(
                    device_fd,
                    output_crtc,
                    commit_id,
                    frame,
                    ust,
                    received,
                );
                Backend::on_page_flip_ready(
                    &mut fixture.events.backend,
                    &mut fixture.events.state,
                    device_fd,
                );
            }
            harness.note_direct_frame_retired(output_crtc, commit_id);
            super::super::direct_unflip::drain_present_notifications(
                &mut harness.scene,
                &[direct.candidate.present_id],
                &[previous_present],
            );
        }
        harness.direct = Some(direct);

        assert_eq!(
            sink.for_tests_take_vrr_summary_snapshot(),
            Some((u32::from(output_crtc), 10_000, 10_000, expected_max_us, 0,)),
            "the VRR summary uses raw UST for completions {offset_name}; the fixture's initial flip contributes its expected interval"
        );
        harness.finish();
    }
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: record intervals for every CRTC, including CRTCs whose vrr_committed is false.
fn vrr_summary_reports_intervals_and_deadline_commits() {
    let Some(mut harness) = vrr_cursor_harness(18_030, true) else {
        eprintln!(
            "SKIP vrr_summary_reports_intervals_and_deadline_commits: no Vulkan scene fixture available"
        );
        return;
    };
    let output_crtc = harness.scene.events.backend.platform.outputs[0].output.crtc;
    assert!(
        harness.scene.events.backend.platform.outputs[0]
            .output
            .vrr_committed
            .get()
    );
    let summary_sink =
        Rc::clone(&harness.scene.events.backend.platform.devices[0].atomic_commit_sink);
    summary_sink.for_tests_reset_vrr_summary(Duration::from_micros(1));
    super::super::recording_scene::clear_vrr_test_logs();

    let mut direct = harness.direct.take().expect("direct scanout is active");
    for (frame, time_ms) in [(10_u32, 10_u64), (25, 25), (45, 45)] {
        let previous_present = direct.candidate.present_id;
        let record = submit_direct_successor(
            &mut harness
                .fixture
                .as_mut()
                .expect("cursor fixture is live")
                .scene,
            &mut direct,
            harness.device_fd,
        );
        assert_eq!(record.events.len(), 1, "direct frame targets the VRR CRTC");
        let commit_id = super::super::support::accepted_event_id(&record)
            .expect("direct frame has an accepted commit ID");
        let retired_crtcs: Vec<_> = record.events.iter().map(|event| event.crtc).collect();
        let ust = Duration::from_millis(time_ms);
        let received = Duration::from_millis(time_ms + 1);
        {
            let fixture = harness.fixture.as_mut().expect("cursor fixture is live");
            fixture.scene.events.wait_for_submitted_gpu_work();
            for event in &record.events {
                fixture.scene.events.queue_commit_completion_at(
                    harness.device_fd,
                    event.crtc,
                    commit_id,
                    frame,
                    ust,
                    received,
                );
            }
            Backend::on_page_flip_ready(
                &mut fixture.scene.events.backend,
                &mut fixture.scene.events.state,
                harness.device_fd,
            );
        }
        for crtc in retired_crtcs {
            harness.note_direct_frame_retired(crtc, commit_id);
        }
        super::super::direct_unflip::drain_present_notifications(
            &mut harness
                .fixture
                .as_mut()
                .expect("cursor fixture is live")
                .scene,
            &[direct.candidate.present_id],
            &[previous_present],
        );
    }
    harness.direct = Some(direct);

    let base = harness.clock_base;
    let elapsed_ns = Arc::clone(&harness.elapsed_ns);
    advance_vrr_cursor_clock(&elapsed_ns, Duration::from_millis(45));
    let cursor_offset =
        super::super::direct_unflip::record_count(&harness.scene, harness.device_fd);
    harness.warp_pointer(180, 130);
    Backend::maybe_composite(&mut harness.scene.events.backend)
        .expect("hold VRR cursor motion until its deadline");
    assert_eq!(
        Backend::next_wakeup(&harness.scene.events.backend),
        Some(base + Duration::from_millis(78)),
        "the cursor move gets a fresh 33 ms deadline after the 45 ms flip"
    );
    advance_vrr_cursor_clock(&elapsed_ns, Duration::from_millis(33));
    Backend::maybe_composite(&mut harness.scene.events.backend)
        .expect("submit the deadline-expired cursor move");
    let deadline_commits: Vec<_> = vrr_cursor_records_since(&harness, cursor_offset)
        .into_iter()
        .filter(|record| {
            record.kind == AtomicCommitKind::CursorOnly
                && record.events.iter().any(|event| event.crtc == output_crtc)
        })
        .collect();
    assert_eq!(
        deadline_commits.len(),
        1,
        "the VRR output sends exactly one cursor-only commit at the deadline"
    );
    assert_eq!(deadline_commits[0].result, ScriptedResult::Ok);
    assert!(
        super::super::support::accepted_event_id(&deadline_commits[0]).is_some(),
        "the deadline cursor request was accepted"
    );

    let summary_deadline = std::time::Instant::now() + Duration::from_secs(5);
    let vrr_summary = loop {
        Backend::before_block(&mut harness.scene.events.backend);
        if let Some(line) = super::super::recording_scene::vrr_test_logs()
            .lines()
            .find(|line| line.starts_with("present_latency_summary "))
        {
            break line.to_owned();
        }
        assert!(
            std::time::Instant::now() < summary_deadline,
            "present latency summary did not become due"
        );
        std::thread::yield_now();
    };
    // The setup direct flip retires at UST 1 us before these 10/25/45 ms samples.
    let expected_vrr_section = format!(
        "vrr crtc={} flip_interval_us_min/p50/max=9999/15000/20000 cursor_deadline_commits=1",
        u32::from(output_crtc)
    );
    assert!(
        vrr_summary.contains(&expected_vrr_section),
        "the VRR summary reports exact retired-flip intervals and one deadline commit; line={vrr_summary}"
    );
    assert_eq!(
        vrr_summary.matches(" vrr crtc=").count(),
        1,
        "the VRR summary omits the fixed-refresh CRTC"
    );
    harness.finish();

    let Some((off_fixture, off_direct)) = super::super::direct_unflip::direct_fixture(
        1,
        super::super::recording_scene::P1SceneRoute::Shared,
        18_031,
        0,
    ) else {
        eprintln!(
            "SKIP vrr_summary_reports_intervals_and_deadline_commits: no VRR-off Vulkan fixture available"
        );
        return;
    };
    let mut off_fixture = off_fixture.cleanup_guard();
    super::super::recording_scene::clear_vrr_test_logs();
    let off_fd = super::super::direct_unflip::fd(&off_fixture);
    let off_record = accepted_direct_record(&mut off_fixture, &off_direct, off_fd);
    let off_id = super::super::support::accepted_event_id(&off_record)
        .expect("VRR-off direct frame has an accepted event ID");
    off_fixture.events.wait_for_submitted_gpu_work();
    for event in &off_record.events {
        off_fixture.events.queue_commit_completion_at(
            off_fd,
            event.crtc,
            off_id,
            60,
            Duration::from_millis(60),
            Duration::from_millis(61),
        );
    }
    {
        let events = &mut off_fixture.events;
        Backend::on_page_flip_ready(&mut events.backend, &mut events.state, off_fd);
    }
    super::super::direct_unflip::drain_present_notifications(
        &mut off_fixture,
        &[off_direct.candidate.present_id],
        &[],
    );
    let off_summary_deadline = std::time::Instant::now() + Duration::from_secs(5);
    let off_summary = loop {
        Backend::before_block(&mut off_fixture.events.backend);
        if let Some(line) = super::super::recording_scene::vrr_test_logs()
            .lines()
            .find(|line| line.starts_with("present_latency_summary "))
        {
            break line.to_owned();
        }
        assert!(
            std::time::Instant::now() < off_summary_deadline,
            "VRR-off present latency summary did not become due"
        );
        std::thread::yield_now();
    };
    assert!(
        !off_summary.contains(" vrr crtc="),
        "the VRR-off snapshot has no VRR section; line={off_summary}"
    );
    finish_direct_scanout(&mut off_fixture, &off_direct, off_fd);
    off_fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: route the standalone clear's event through direct-frame retirement.
fn vrr_standalone_clear_event_retires_nothing() {
    let Some((mut fixture, direct)) =
        direct_fixture_with_vrr_mode(18_048, 0, crate::launch::VrrMode::Property)
    else {
        eprintln!(
            "SKIP vrr_standalone_clear_event_retires_nothing: no Vulkan scene fixture available"
        );
        return;
    };
    set_vrr_capable(&mut fixture);
    let mut fixture = fixture.cleanup_guard();
    let fd = super::super::direct_unflip::fd(&fixture);
    let crtc = fixture.events.backend.platform.outputs[0].output.crtc;
    let (mut state, property, initial, _, elapsed_ns) =
        start_property_direct(&mut fixture, &direct, true);
    let direct_id = super::super::support::accepted_event_id(&initial).expect("direct event ID");
    super::super::direct_unflip::complete_direct_initial(&mut fixture, &initial, direct_id, fd);
    super::super::direct_unflip::drain_present_notifications(&mut fixture, &[1], &[]);
    for interval in 1..=3_u64 {
        elapsed_ns.fetch_add(16_000_000, Ordering::SeqCst);
        let present_id = 1 + interval;
        let records = submit_property_present(
            &mut fixture,
            &mut state,
            &direct,
            u32::try_from(present_id).expect("qualifying serial fits"),
            u16::try_from(2 + interval).expect("qualifying sequence fits"),
        );
        let record = records
            .into_iter()
            .find(|record| super::super::support::accepted_event_id(record).is_some())
            .expect("qualifying Present is accepted through Backend");
        let event_id =
            super::super::support::accepted_event_id(&record).expect("qualifying event ID");
        super::super::direct_unflip::complete_direct_initial(&mut fixture, &record, event_id, fd);
        if interval < 3 {
            super::super::direct_unflip::drain_present_notifications(
                &mut fixture,
                &[present_id],
                &[present_id - 1],
            );
        }
    }

    let current_before = fixture
        .events
        .backend
        .pstate
        .direct
        .root()
        .current
        .as_ref()
        .map(|frame| {
            (
                frame.candidate.present_id,
                frame.source_id,
                frame.source_pin,
                frame.fallback_target_pin,
                frame.accepted_event_id,
            )
        })
        .expect("the direct frame remains current after its own flip event");
    let direct_framebuffer_before = fixture
        .events
        .backend
        .pstate
        .scanout_m1
        .entries
        .get(&(
            current_before.1,
            crate::kms::render::backend::DirectScope::Root,
        ))
        .and_then(crate::kms::render::backend::ScanoutM1ProbeEntry::framebuffer)
        .map(crate::drm::modeset::DirectScanoutProbeFramebuffer::handle)
        .expect("current direct frame retains its framebuffer");
    let pins_before = fixture.events.backend.present_source_pins.clone();
    let completed_before: Vec<_> = fixture
        .events
        .backend
        .pstate
        .direct
        .root()
        .completed
        .iter()
        .map(|event| event.present_id)
        .collect();
    let idled_before: Vec<_> = fixture
        .events
        .backend
        .pstate
        .direct
        .root()
        .idled
        .iter()
        .map(|event| event.present_id)
        .collect();
    let expected_present_id = fixture
        .events
        .backend
        .pstate
        .direct
        .root()
        .current
        .as_ref()
        .expect("current direct frame is retained")
        .event
        .present_id;
    assert_eq!(completed_before, vec![expected_present_id]);

    super::super::recording_scene::init_tearing_test_logger();
    super::super::recording_scene::clear_vrr_test_logs();
    let offset = super::super::direct_unflip::record_count(&fixture, fd);
    delete_vrr_property(
        &mut state,
        &mut fixture.events.backend,
        direct.target_xid,
        property,
        6,
    );
    Backend::maybe_composite(&mut fixture.events.backend)
        .expect("idle property clear scheduling pass");
    let clear = super::super::direct_unflip::records_since(&fixture, fd, offset)
        .into_iter()
        .find(|record| super::super::support::accepted_event_id(record).is_some())
        .expect("idle deletion submits one standalone clear");
    assert_eq!(clear.kind, AtomicCommitKind::VrrOnly);
    assert!(contains_vrr_property(&clear, crtc, 0));
    let logs = super::super::recording_scene::vrr_test_logs();
    assert!(logs.contains("reason=property"), "{logs}");
    assert!(!logs.contains("reason=client_idle"), "{logs}");

    let clear_id = super::super::support::accepted_event_id(&clear).expect("clear event ID");
    super::super::atomic_events::complete_recorded_attempt(&mut fixture, fd, &clear, clear_id, 91);
    let current_after = fixture
        .events
        .backend
        .pstate
        .direct
        .root()
        .current
        .as_ref()
        .map(|frame| {
            (
                frame.candidate.present_id,
                frame.source_id,
                frame.source_pin,
                frame.fallback_target_pin,
                frame.accepted_event_id,
            )
        })
        .expect("standalone clear event leaves the direct frame current");
    assert_eq!(current_after, current_before);
    let direct_framebuffer_after = fixture
        .events
        .backend
        .pstate
        .scanout_m1
        .entries
        .get(&(
            current_after.1,
            crate::kms::render::backend::DirectScope::Root,
        ))
        .and_then(crate::kms::render::backend::ScanoutM1ProbeEntry::framebuffer)
        .map(crate::drm::modeset::DirectScanoutProbeFramebuffer::handle)
        .expect("standalone clear event retains the direct framebuffer");
    assert_eq!(direct_framebuffer_after, direct_framebuffer_before);
    assert_eq!(
        fixture.events.backend.present_source_pins, pins_before,
        "standalone clear event releases none of the direct frame's buffer pins"
    );
    assert_eq!(
        fixture
            .events
            .backend
            .pstate
            .direct
            .root()
            .completed
            .iter()
            .map(|event| event.present_id)
            .collect::<Vec<_>>(),
        completed_before,
        "standalone clear event neither adds nor consumes a Present completion"
    );
    assert_eq!(
        fixture
            .events
            .backend
            .pstate
            .direct
            .root()
            .idled
            .iter()
            .map(|event| event.present_id)
            .collect::<Vec<_>>(),
        idled_before,
        "standalone clear event does not retire a Present buffer"
    );
    let completed = Backend::drain_completed_present_events(&mut fixture.events.backend);
    assert_eq!(
        completed
            .iter()
            .map(|event| event.present_id)
            .collect::<Vec<_>>(),
        vec![expected_present_id],
        "only the undrained current Present completion survives the clear"
    );
    assert_eq!(
        Backend::drain_retired_present_idle_events(&mut fixture.events.backend)
            .iter()
            .map(|event| event.present_id)
            .collect::<Vec<_>>(),
        idled_before,
        "standalone clear preserves the exact retired-idle queue"
    );

    finish_property_direct_scanout(&mut fixture, fd);
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: append VRR_ENABLED in the same request that carries GAMMA_LUT.
fn vrr_enable_waits_behind_pending_gamma() {
    let Some((fixture, mut direct)) =
        direct_fixture_with_vrr_mode(18_007, 0, crate::launch::VrrMode::Property)
    else {
        eprintln!("skip: no Vulkan scene fixture available");
        return;
    };
    let mut fixture = fixture.cleanup_guard();
    let fd = super::super::direct_unflip::fd(&fixture);
    let recorder = fixture.events.recorders[&fd].clone();
    set_vrr_capable(&mut fixture);
    direct.candidate.variable_refresh = true;
    {
        let output = &mut fixture.events.backend.platform.outputs[0].output;
        output.crtc_gamma_lut_prop = Some(handle(70));
        output.crtc_degamma_lut_prop = Some(handle(71));
        output.crtc_ctm_prop = Some(handle(72));
        output.crtc_gamma_lut_size = Some(4);
    }
    let output_key = fixture.events.backend.platform.outputs[0].key.clone();
    let crtc = fixture.events.backend.platform.outputs[0].output.crtc;
    fixture
        .events
        .backend
        .pstate
        .crtc_key_by_id
        .insert(u32::from(crtc), output_key);
    recorder.set_gamma_size_for_tests(4);
    let (base, elapsed_ns) =
        super::super::support::install_controlled_clock(&mut fixture.events.backend);

    let first = accepted_direct_record(&mut fixture, &direct, fd);
    let first_id = super::super::support::accepted_event_id(&first).expect("direct event ID");
    assert!(!contains_vrr_property(&first, crtc, 1));
    super::super::direct_unflip::complete_direct_initial(&mut fixture, &first, first_id, fd);
    let first_present_id = direct.candidate.present_id;
    super::super::direct_unflip::drain_present_notifications(
        &mut fixture,
        &[first_present_id],
        &[],
    );

    elapsed_ns.fetch_add(16_000_000, Ordering::SeqCst);
    let pending = submit_direct_successor(&mut fixture, &mut direct, fd);
    assert!(!contains_vrr_property(&pending, crtc, 1));
    for _ in 0..2 {
        elapsed_ns.fetch_add(16_000_000, Ordering::SeqCst);
        queue_direct_successor(&mut fixture, &mut direct);
    }
    assert_eq!(
        fixture
            .events
            .backend
            .pstate
            .direct
            .root()
            .vrr_activity
            .get()
            .qualifying_intervals,
        3,
        "the owner qualifies while its synchronous successor remains queued"
    );
    assert!(
        !fixture.events.backend.platform.outputs[0]
            .output
            .vrr_committed
            .get(),
        "the qualifying enable remains uncommitted"
    );

    Backend::set_crtc_gamma(
        &mut fixture.events.backend,
        u32::from(crtc),
        &[0x1111; 4],
        &[0x2222; 4],
        &[0x3333; 4],
    )
    .expect("queue gamma through RandR while direct scanout is active");
    assert!(
        fixture.events.backend.platform.outputs[0]
            .output
            .pending_gamma_lut
            .borrow()
            .is_some()
    );
    let gamma_deadline = fixture.events.backend.platform.outputs[0]
        .output
        .pending_gamma_deadline
        .get();
    elapsed_ns.fetch_add(17_000_000, Ordering::SeqCst);
    let gamma_frame_time = base + Duration::from_nanos(elapsed_ns.load(Ordering::SeqCst));
    assert!(
        gamma_deadline.is_none_or(|deadline| deadline <= gamma_frame_time),
        "the queued gamma LUT is due on the same frame as the VRR enable"
    );
    assert!(
        fixture.events.backend.live_direct_vrr_wanted_by_output()[0],
        "the already-qualified owner wants VRR on the gamma-bearing frame"
    );

    let offset = super::super::direct_unflip::record_count(&fixture, fd);
    let pending_id = super::super::support::accepted_event_id(&pending).expect("pending event ID");
    super::super::atomic_events::complete_recorded_attempt(
        &mut fixture,
        fd,
        &pending,
        pending_id,
        82,
    );
    let gamma_frame = super::super::direct_unflip::records_since(&fixture, fd, offset)
        .into_iter()
        .find(|record| {
            super::super::support::accepted_event_id(record).is_some()
                && record
                    .properties
                    .iter()
                    .any(|update| update.object == crtc.into() && update.property == handle(70))
        })
        .expect("the active queued successor carries the due gamma LUT");
    assert!(!has_vrr_property(&gamma_frame, crtc));
    assert!(
        !fixture.events.backend.platform.outputs[0]
            .output
            .vrr_committed
            .get(),
        "gamma is submitted alone while the already-due VRR enable remains pending"
    );
    let gamma_frame_id =
        super::super::support::accepted_event_id(&gamma_frame).expect("gamma frame event ID");
    super::super::atomic_events::complete_recorded_attempt(
        &mut fixture,
        fd,
        &gamma_frame,
        gamma_frame_id,
        83,
    );
    let _ = Backend::drain_completed_present_events(&mut fixture.events.backend);
    let _ = Backend::drain_retired_present_idle_events(&mut fixture.events.backend);
    assert!(
        fixture.events.backend.platform.outputs[0]
            .output
            .pending_gamma_lut
            .borrow()
            .is_none(),
        "the gamma frame consumes its pending LUT"
    );

    elapsed_ns.fetch_add(16_000_000, Ordering::SeqCst);
    let vrr_frame = submit_direct_successor(&mut fixture, &mut direct, fd);
    assert!(contains_vrr_property(&vrr_frame, crtc, 1));
    assert!(
        !vrr_frame
            .properties
            .iter()
            .any(|update| { update.object == crtc.into() && update.property == handle(70) }),
        "the VRR enable is carried by the next frame"
    );
    let vrr_frame_id =
        super::super::support::accepted_event_id(&vrr_frame).expect("enable event ID");
    super::super::direct_unflip::complete_direct_initial(
        &mut fixture,
        &vrr_frame,
        vrr_frame_id,
        fd,
    );
    super::super::direct_unflip::drain_present_notifications(
        &mut fixture,
        &[direct.candidate.present_id],
        &[direct.candidate.present_id - 1],
    );
    assert!(
        fixture.events.backend.platform.outputs[0]
            .output
            .vrr_committed
            .get()
    );
    finish_direct_scanout(&mut fixture, &direct, fd);
    fixture.finish_after_normal_drain();
}
