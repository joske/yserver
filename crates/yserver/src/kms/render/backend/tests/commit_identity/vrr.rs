use crate::{
    drm::{
        atomic::{
            AtomicCommitKind, AtomicCommitSink, AtomicPropertyUpdate,
            tests_support::{AtomicCommitRecord, RecordingAtomicCommitExecutor, ScriptedResult},
        },
        modeset::PropMap,
    },
    kms::{
        render::{
            backend::KmsBackend,
            platform::{
                ConnectorSnapshot, KmsDevice, OutputPowerExecutor, PlatformBackend, RenderDeviceId,
            },
        },
        scanout_route::{RenderKmsRelationship, ScanoutRoute},
    },
    vrr_exclusions::test_support::{
        TempTree as VrrTestTree, process as write_fake_process_fixture, roots as vrr_test_roots,
        write as write_vrr_test_file,
    },
};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    io,
    path::Path,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU16, AtomicU64, Ordering},
    },
    time::Duration,
};
use yserver_core::backend::{Backend, ModeSpec};

const VRR_PROP_ID: u32 = 90;

#[path = "vrr_per_output.rs"]
mod per_output;

type PropertySignature = (String, String, u64);
type CommitSignature = (
    AtomicCommitKind,
    ::drm::control::AtomicCommitFlags,
    Vec<PropertySignature>,
);
type RequestSignature = (
    AtomicCommitKind,
    ::drm::control::AtomicCommitFlags,
    Vec<AtomicPropertyUpdate>,
);

fn handle<H: ::drm::control::ResourceHandle>(value: u32) -> H {
    ::drm::control::from_u32(value).expect("nonzero VRR test handle")
}

fn vrr_backend() -> KmsBackend {
    let mut backend = KmsBackend::for_tests();
    let display_key = backend.platform.devices[0].key;
    let render_id = RenderDeviceId::DrmRender(super::super::test_device_key(128));
    backend.platform.render_devices = vec![super::super::randr::test_render_device(
        render_id,
        Some(display_key),
    )];
    backend.platform.selected_render_device = Some(render_id);
    backend.platform.outputs[0].scanout_route =
        ScanoutRoute::new(render_id, display_key, RenderKmsRelationship::Same);
    let recorder = Rc::new(RecordingAtomicCommitExecutor::default());
    backend.platform.devices[0].atomic_commit_sink =
        Rc::new(AtomicCommitSink::new(recorder.clone()));
    let output = &mut backend.platform.outputs[0].output;
    output.crtc_vrr_enabled_prop = Some(handle(VRR_PROP_ID));
    output.connector_vrr_capable = true;
    output.vrr_committed.set(false);
    output.vrr_disabled.set(false);
    output.vrr_clear_retry_used.set(false);
    backend.scene.scene_structure_dirty = false;
    backend
        .initialize_provider_output_sources()
        .expect("authorize the test KMS output");
    backend.platform.for_tests_accept_output_power_commits();
    backend
}

fn randr_output_id(backend: &mut KmsBackend, connector: &str) -> u32 {
    Backend::randr_outputs_and_modes(backend)
        .0
        .into_iter()
        .find(|output| output.name == connector)
        .expect("test output is in the RandR projection")
        .output_id
}

fn apply_modeset_replacement(backend: &mut KmsBackend, mode_vrefresh: u32, crtc: u32) {
    let (connector, output_key, x, y) = {
        let layout = &backend.platform.outputs[0];
        (
            layout.key.connector_name.clone(),
            layout.key.clone(),
            layout.x,
            layout.y,
        )
    };
    let output_id = randr_output_id(backend, &connector);
    let mode = ModeSpec {
        width: 800,
        height: 600,
        vrefresh: mode_vrefresh,
    };
    let mut replacement =
        super::super::randr::clone_test_drm_output(&backend.platform.outputs[0].output);
    replacement.crtc = handle(crtc);
    let advertised = super::super::test_advertised_mode(800, 600, mode_vrefresh, false);
    replacement.picked = advertised.clone();
    replacement.modes.push(advertised.clone());
    backend
        .pstate
        .randr_id_alloc
        .entry_mut(&output_key)
        .modes
        .push(advertised);
    backend.pstate.crtc_config_discovery_override = Some(replacement);
    backend
        .initialize_provider_output_sources()
        .expect("authorize the test KMS output");
    assert!(
        Backend::apply_crtc_config(backend, output_id, &connector, Some(mode), x, y,)
            .expect("apply RandR modeset through Backend")
    );
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: drop same-CRTC VRR state carry-over, including the bounded-clear marker.
fn vrr_state_survives_output_replacement_on_same_crtc() {
    let Some(fixture) = super::recording_scene::RecordingSceneFixture::new(1)
        .expect("recording scene fixture construction")
    else {
        eprintln!("skip: no Vulkan scene fixture available");
        return;
    };
    let mut fixture = fixture.cleanup_guard();
    let backend = &mut fixture.events.backend;
    let output = &mut backend.platform.outputs[0].output;
    output.crtc_vrr_enabled_prop = Some(handle(VRR_PROP_ID));
    output.connector_vrr_capable = true;
    backend.platform.for_tests_accept_output_power_commits();
    let crtc = u32::from(backend.platform.outputs[0].output.crtc);
    backend.platform.outputs[0].output.vrr_committed.set(true);
    backend.platform.outputs[0].output.vrr_disabled.set(true);
    backend.platform.outputs[0]
        .output
        .vrr_clear_retry_used
        .set(true);

    apply_modeset_replacement(backend, 59, crtc);

    let output = &backend.platform.outputs[0].output;
    let same_crtc_state = (
        u32::from(output.crtc),
        output.vrr_committed.get(),
        output.vrr_disabled.get(),
        output.vrr_clear_retry_used.get(),
    );

    output.vrr_committed.set(true);
    output.vrr_disabled.set(true);
    output.vrr_clear_retry_used.set(true);
    apply_modeset_replacement(backend, 58, crtc + 1);
    let output = &backend.platform.outputs[0].output;
    let new_crtc_state = (
        u32::from(output.crtc),
        output.vrr_committed.get(),
        output.vrr_disabled.get(),
        output.vrr_clear_retry_used.get(),
    );
    fixture.finish_after_normal_drain();

    assert_eq!(same_crtc_state.0, crtc, "the replacement keeps its CRTC");
    assert!(same_crtc_state.1, "the kernel-held value carries over");
    assert!(same_crtc_state.2, "the EINVAL latch carries over");
    assert!(same_crtc_state.3, "the bounded-clear marker carries over");
    assert_eq!(
        new_crtc_state.0,
        crtc + 1,
        "the replacement uses a new CRTC"
    );
    assert!(!new_crtc_state.1, "a new CRTC starts with kernel state 0");
    assert!(
        !new_crtc_state.2,
        "a new CRTC starts without an EINVAL latch"
    );
    assert!(
        !new_crtc_state.3,
        "a new CRTC starts with a fresh clear budget"
    );
}

struct VrrSnapshot(Vec<ConnectorSnapshot>);

impl crate::kms::render::backend::kms::session::VtResumeExecutor for VrrSnapshot {
    fn acquire_master(&self, _device: &KmsDevice) -> io::Result<()> {
        Ok(())
    }

    fn release_master(&self, _device: &KmsDevice) -> io::Result<()> {
        Ok(())
    }

    fn probe_connector_snapshot(
        &self,
        _platform: &PlatformBackend,
    ) -> io::Result<Vec<ConnectorSnapshot>> {
        Ok(self.0.clone())
    }
}

#[test]
// Mutation: skip refreshing connector_vrr_capable from the rescan snapshot.
fn vrr_capable_follows_connector_rescan() {
    let mut backend = vrr_backend();
    let live = &backend.platform.outputs[0];
    let snapshot = ConnectorSnapshot {
        key: live.key.clone(),
        modes: vec![live.output.picked.clone()],
        mm_width: live.output.mm_width,
        mm_height: live.output.mm_height,
        edid: live.output.edid.clone(),
        connector_type: live.output.connector_type.clone(),
        vrr_capable: false,
    };
    backend.for_tests_set_vt_resume_executor(Rc::new(VrrSnapshot(vec![snapshot])));
    backend.pstate.hotplug_rescan_deadline = Some(std::time::Instant::now());
    let mut state = yserver_core::server::ServerState::new();

    backend.backend_session_poll_deferred_input(&mut state);

    let output = &backend.platform.outputs[0].output;
    assert!(
        !output.connector_vrr_capable,
        "the live output adopts snapshot value 0"
    );
    assert!(
        !output.vrr_capable(),
        "VRR is unavailable after the monitor replacement"
    );
}

fn set_vrr_capable(fixture: &mut super::recording_scene::RecordingSceneFixture) {
    let output = &mut fixture.events.backend.platform.outputs[0].output;
    output.crtc_vrr_enabled_prop = Some(handle(VRR_PROP_ID));
    output.connector_vrr_capable = true;
    output.vrr_committed.set(false);
    output.vrr_disabled.set(false);
    output.vrr_clear_retry_used.set(false);
}

fn contains_vrr_property(
    record: &AtomicCommitRecord,
    crtc: ::drm::control::crtc::Handle,
    value: u64,
) -> bool {
    record.properties.iter().any(|update| {
        update.object == crtc.into()
            && update.property == handle(VRR_PROP_ID)
            && update.value == value
    })
}

fn has_vrr_property(record: &AtomicCommitRecord, crtc: ::drm::control::crtc::Handle) -> bool {
    record
        .properties
        .iter()
        .any(|update| update.object == crtc.into() && update.property == handle(VRR_PROP_ID))
}

type PropertyRow = (String, ::drm::control::property::Handle, u64);

#[derive(Clone)]
struct ObjectPropertyRows(Vec<PropertyRow>);

impl ObjectPropertyRows {
    fn map(&self) -> PropMap {
        PropMap::from_discovered_properties(self.0.clone())
    }
}

#[derive(Clone)]
struct DevicePropertyRows {
    planes: Vec<(::drm::control::plane::Handle, ObjectPropertyRows)>,
    crtcs: Vec<(::drm::control::crtc::Handle, ObjectPropertyRows)>,
    connectors: Vec<(::drm::control::connector::Handle, ObjectPropertyRows)>,
}

struct DevicePropertyMaps {
    planes: Vec<(::drm::control::plane::Handle, PropMap)>,
    crtcs: Vec<(::drm::control::crtc::Handle, PropMap)>,
    connectors: Vec<(::drm::control::connector::Handle, PropMap)>,
}

impl DevicePropertyRows {
    fn for_output(output: &crate::drm::modeset::Output, kernel_vrr: u64) -> Self {
        let mut crtc_rows = vec![
            ("ACTIVE".to_owned(), handle(91), 1),
            ("MODE_ID".to_owned(), handle(92), 1),
        ];
        if let Some(vrr) = output.crtc_vrr_enabled_prop {
            crtc_rows.push(("VRR_ENABLED".to_owned(), vrr, kernel_vrr));
        }
        Self {
            planes: vec![(
                output.plane,
                ObjectPropertyRows(vec![
                    ("FB_ID".to_owned(), output.plane_fb_id_prop, 1),
                    (
                        "CRTC_ID".to_owned(),
                        output.plane_crtc_id_prop,
                        u64::from(u32::from(output.crtc)),
                    ),
                ]),
            )],
            crtcs: vec![(output.crtc, ObjectPropertyRows(crtc_rows))],
            connectors: vec![(
                output.connector,
                ObjectPropertyRows(vec![(
                    "CRTC_ID".to_owned(),
                    handle(93),
                    u64::from(u32::from(output.crtc)),
                )]),
            )],
        }
    }

    fn maps(&self) -> DevicePropertyMaps {
        DevicePropertyMaps {
            planes: self
                .planes
                .iter()
                .map(|(handle, properties)| (*handle, properties.map()))
                .collect(),
            crtcs: self
                .crtcs
                .iter()
                .map(|(handle, properties)| (*handle, properties.map()))
                .collect(),
            connectors: self
                .connectors
                .iter()
                .map(|(handle, properties)| (*handle, properties.map()))
                .collect(),
        }
    }
}

struct RecordingOutputPower {
    sink: Rc<AtomicCommitSink>,
    device_properties: DevicePropertyRows,
    events: Rc<RefCell<Vec<&'static str>>>,
}

impl RecordingOutputPower {
    fn submit(
        &self,
        device: &crate::drm::Device,
        request: crate::drm::atomic::AtomicCommitRequest,
    ) -> io::Result<()> {
        let result = self.sink.submit(
            device,
            ::drm::control::AtomicCommitFlags::ALLOW_MODESET,
            AtomicCommitKind::Scene,
            request,
            &[],
        );
        self.events.borrow_mut().push("atomic_submit");
        result
    }
}

impl OutputPowerExecutor for RecordingOutputPower {
    fn disable(
        &self,
        device: &crate::drm::Device,
        output: &crate::drm::modeset::Output,
    ) -> io::Result<()> {
        let properties = DevicePropertyRows::for_output(output, 0);
        let connector = properties.connectors[0].1.map();
        let crtc = properties.crtcs[0].1.map();
        crate::drm::modeset::disable_output_with_properties(output, &connector, &crtc, |request| {
            self.submit(device, request)
        })
    }

    fn enable(
        &self,
        _device: &crate::drm::Device,
        _output: &crate::drm::modeset::Output,
        _framebuffer: ::drm::control::framebuffer::Handle,
    ) -> io::Result<()> {
        self.events.borrow_mut().push("enable");
        Ok(())
    }

    fn disable_device(&self, device: &crate::drm::Device) -> io::Result<()> {
        let maps = self.device_properties.maps();
        crate::drm::modeset::disable_all_outputs_with_properties(
            maps.planes,
            maps.crtcs,
            maps.connectors,
            |request| self.submit(device, request),
        )
    }
}

fn install_recording_power(
    backend: &mut KmsBackend,
    recorder: Rc<RecordingAtomicCommitExecutor>,
    kernel_vrr: u64,
    events: Rc<RefCell<Vec<&'static str>>>,
) {
    let sink = Rc::new(AtomicCommitSink::new(recorder));
    backend.platform.devices[0].atomic_commit_sink = Rc::clone(&sink);
    let properties =
        DevicePropertyRows::for_output(&backend.platform.outputs[0].output, kernel_vrr);
    backend
        .platform
        .for_tests_set_output_power_executor(Rc::new(RecordingOutputPower {
            sink,
            device_properties: properties,
            events,
        }));
}

fn backend_snapshot(backend: &KmsBackend) -> Vec<ConnectorSnapshot> {
    backend
        .platform
        .outputs
        .iter()
        .map(|layout| ConnectorSnapshot {
            key: layout.key.clone(),
            modes: vec![layout.output.picked.clone()],
            mm_width: layout.output.mm_width,
            mm_height: layout.output.mm_height,
            edid: layout.output.edid.clone(),
            connector_type: layout.output.connector_type.clone(),
            vrr_capable: layout.output.connector_vrr_capable,
        })
        .collect()
}

struct KernelVrrVtResume {
    snapshots: Vec<ConnectorSnapshot>,
    kernel_vrr: u64,
    events: Rc<RefCell<Vec<&'static str>>>,
}

impl crate::kms::render::backend::kms::session::VtResumeExecutor for KernelVrrVtResume {
    fn acquire_master(&self, _device: &KmsDevice) -> io::Result<()> {
        Ok(())
    }

    fn release_master(&self, _device: &KmsDevice) -> io::Result<()> {
        self.events.borrow_mut().push("release_master");
        Ok(())
    }

    fn crtc_vrr_enabled_value(
        &self,
        _device: &KmsDevice,
        _crtc: ::drm::control::crtc::Handle,
        property: ::drm::control::property::Handle,
    ) -> io::Result<Option<u64>> {
        assert_eq!(property, handle(VRR_PROP_ID));
        self.events.borrow_mut().push("read_vrr_enabled");
        Ok(Some(self.kernel_vrr))
    }

    fn probe_connector_snapshot(
        &self,
        _platform: &PlatformBackend,
    ) -> io::Result<Vec<ConnectorSnapshot>> {
        Ok(self.snapshots.clone())
    }
}

#[test]
// Mutation: omit VRR_ENABLED = 0 from disable_output; the recorded VT off commit must fail this assertion.
fn vrr_vt_release_clears_before_drop_master() {
    let mut backend = vrr_backend();
    let crtc = backend.platform.outputs[0].output.crtc;
    backend.platform.outputs[0].output.vrr_committed.set(true);
    let events = Rc::new(RefCell::new(Vec::new()));
    let recorder = Rc::new(RecordingAtomicCommitExecutor::default());
    install_recording_power(&mut backend, Rc::clone(&recorder), 1, Rc::clone(&events));
    backend.for_tests_set_vt_resume_executor(Rc::new(KernelVrrVtResume {
        snapshots: backend_snapshot(&backend),
        kernel_vrr: 1,
        events: Rc::clone(&events),
    }));
    let mut state = yserver_core::server::ServerState::new();

    Backend::begin_vt_release(&mut backend);
    Backend::finish_vt_release(
        &mut backend,
        &mut state,
        &yserver_core::core_loop::input_inventory::InputInventory::default(),
    );

    let off = recorder
        .records()
        .into_iter()
        .find(|record| has_vrr_property(record, crtc))
        .expect("VT release sends the production-built output disable");
    assert!(contains_vrr_property(&off, crtc, 0));
    assert_eq!(off.result, ScriptedResult::Ok);
    assert!(
        !backend.platform.outputs[0].output.vrr_committed.get(),
        "accepted off commit updates the cached kernel-held value"
    );
    let events = events.borrow();
    assert!(
        events.iter().position(|event| *event == "atomic_submit")
            < events.iter().position(|event| *event == "release_master"),
        "the all-off commit is submitted before DRM master is released"
    );
    assert_eq!(
        backend.pstate.vt_state,
        crate::vt::state::VtState::Suspended,
        "the VT release reaches its suspended end state"
    );
}

#[test]
// Mutation: omit VRR_ENABLED = 0 from disable_all_outputs_on_device; shutdown's device commit must fail this assertion.
fn vrr_shutdown_clears() {
    let mut backend = vrr_backend();
    let crtc = backend.platform.outputs[0].output.crtc;
    backend.platform.outputs[0].output.vrr_committed.set(true);
    let events = Rc::new(RefCell::new(Vec::new()));
    let recorder = Rc::new(RecordingAtomicCommitExecutor::default());
    install_recording_power(&mut backend, Rc::clone(&recorder), 1, events);

    backend
        .disable_output()
        .expect("shutdown disables the device");

    let clear = recorder
        .records()
        .into_iter()
        .find(|record| has_vrr_property(record, crtc))
        .expect("device-wide shutdown commit carries the inherited VRR property");
    assert!(contains_vrr_property(&clear, crtc, 0));
    assert_eq!(clear.result, ScriptedResult::Ok);
    assert!(
        backend.platform.shutting_down,
        "Backend completed shutdown teardown"
    );
}

#[test]
// Mutation: skip the one stripped retry after a rejected VRR clear; release must still reach its suspended end state.
fn vrr_clear_rejected_release_completes() {
    super::recording_scene::init_tearing_test_logger();
    super::recording_scene::clear_vrr_test_logs();
    let mut backend = vrr_backend();
    let crtc = backend.platform.outputs[0].output.crtc;
    backend.platform.outputs[0].output.vrr_committed.set(true);
    let events = Rc::new(RefCell::new(Vec::new()));
    let recorder = Rc::new(RecordingAtomicCommitExecutor::default());
    install_recording_power(&mut backend, Rc::clone(&recorder), 1, Rc::clone(&events));
    recorder.script_result(Err(io::Error::from_raw_os_error(libc::EIO)));
    backend.for_tests_set_vt_resume_executor(Rc::new(KernelVrrVtResume {
        snapshots: backend_snapshot(&backend),
        kernel_vrr: 1,
        events: Rc::clone(&events),
    }));
    let mut state = yserver_core::server::ServerState::new();

    Backend::begin_vt_release(&mut backend);
    Backend::finish_vt_release(
        &mut backend,
        &mut state,
        &yserver_core::core_loop::input_inventory::InputInventory::default(),
    );

    let records = recorder.records();
    let attempts: Vec<_> = records
        .iter()
        .filter(|record| record.kind == AtomicCommitKind::Scene)
        .collect();
    assert_eq!(attempts.len(), 2, "the rejected clear gets one retry");
    assert!(contains_vrr_property(attempts[0], crtc, 0));
    assert!(matches!(
        attempts[0].result,
        ScriptedResult::Error {
            raw_os_error: Some(error),
            ..
        } if error == libc::EIO
    ));
    assert!(!has_vrr_property(attempts[1], crtc));
    assert_eq!(attempts[1].result, ScriptedResult::Ok);
    assert!(
        backend.platform.outputs[0].output.vrr_committed.get(),
        "a successful retry without VRR leaves the cached value unchanged"
    );
    assert_eq!(
        backend.pstate.vt_state,
        crate::vt::state::VtState::Suspended,
        "the failed clear and stripped retry do not block VT release"
    );
    let events = events.borrow();
    let release = events
        .iter()
        .position(|event| *event == "release_master")
        .expect("VT release drops DRM master");
    assert!(
        events
            .iter()
            .enumerate()
            .filter(|(_, event)| **event == "atomic_submit")
            .all(|(index, _)| index < release),
        "both clear attempts precede release_master"
    );
    assert!(
        super::recording_scene::vrr_test_logs().contains(&format!(
            "kms: vrr clear rejected on CRTC {}:",
            u32::from(crtc)
        )),
        "the rejected release clear is logged"
    );
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: leave vrr_committed true after an accepted DPMS clear; the output end-state assertion must fail.
fn vrr_dpms_off_clears_and_direct_frame_reenables() {
    let Some((fixture, direct)) = direct_fixture(18_010, 0) else {
        eprintln!(
            "SKIP vrr_dpms_off_clears_and_direct_frame_reenables: no Vulkan scene fixture available"
        );
        return;
    };
    let mut fixture = fixture.cleanup_guard();
    set_vrr_capable(&mut fixture);
    let fd = super::direct_unflip::fd(&fixture);
    let crtc = fixture.events.backend.platform.outputs[0].output.crtc;
    let recorder = fixture.events.recorders[&fd].clone();
    let events = Rc::new(RefCell::new(Vec::new()));
    install_recording_power(&mut fixture.events.backend, Rc::clone(&recorder), 0, events);

    let initial = accepted_direct_record(&mut fixture, &direct, fd);
    let initial_id = super::support::accepted_event_id(&initial).expect("initial direct event ID");
    super::direct_unflip::complete_direct_initial(&mut fixture, &initial, initial_id, fd);
    super::direct_unflip::drain_present_notifications(
        &mut fixture,
        &[direct.candidate.present_id],
        &[],
    );

    let dpms_offset = recorder.records().len();
    Backend::set_dpms_power(&mut fixture.events.backend, 3).expect("DPMS off through Backend");
    let dpms_clear = recorder.records()[dpms_offset..]
        .iter()
        .find(|record| contains_vrr_property(record, crtc, 0))
        .cloned()
        .expect("DPMS off commit clears VRR_ENABLED");
    assert_eq!(dpms_clear.result, ScriptedResult::Ok);
    assert!(
        !fixture.events.backend.platform.outputs[0]
            .output
            .vrr_committed
            .get(),
        "accepted DPMS off commit records VRR as disabled"
    );
    super::direct_unflip::drain_present_notifications(
        &mut fixture,
        &[],
        &[direct.candidate.present_id],
    );

    Backend::set_dpms_power(&mut fixture.events.backend, 0).expect("DPMS on through Backend");
    let resumed_direct = fixture.install_direct_present(18_011, 0);
    let direct_frame = accepted_direct_record(&mut fixture, &resumed_direct, fd);
    assert!(contains_vrr_property(&direct_frame, crtc, 1));
    assert!(
        fixture.events.backend.platform.outputs[0]
            .output
            .vrr_committed
            .get(),
        "the next direct frame restores the committed VRR state"
    );
    let resumed_id = super::support::accepted_event_id(&direct_frame)
        .expect("post-DPMS direct commit has an event ID");
    super::direct_unflip::complete_direct_initial(&mut fixture, &direct_frame, resumed_id, fd);
    super::direct_unflip::drain_present_notifications(
        &mut fixture,
        &[resumed_direct.candidate.present_id],
        &[],
    );
    finish_direct_scanout(&mut fixture, &resumed_direct, fd);
    fixture.finish_after_normal_drain();
}

#[test]
// Mutation: skip refresh_vrr_committed_from_kernel after device-wide disable; the inherited value must be observed.
fn vrr_vt_acquire_reads_value_left_by_other_master() {
    let mut backend = vrr_backend();
    let events = Rc::new(RefCell::new(Vec::new()));
    let recorder = Rc::new(RecordingAtomicCommitExecutor::default());
    install_recording_power(&mut backend, Rc::clone(&recorder), 1, Rc::clone(&events));
    backend.for_tests_set_vt_resume_executor(Rc::new(KernelVrrVtResume {
        snapshots: backend_snapshot(&backend),
        kernel_vrr: 1,
        events: Rc::clone(&events),
    }));
    let mut state = yserver_core::server::ServerState::new();

    // Enter the suspended state through Backend before modeling a value left
    // by the other master; on_vt_acquire is a no-op while already Active.
    Backend::begin_vt_release(&mut backend);
    Backend::finish_vt_release(
        &mut backend,
        &mut state,
        &yserver_core::core_loop::input_inventory::InputInventory::default(),
    );
    backend.platform.outputs[0].output.vrr_committed.set(false);
    recorder.clear_records();
    events.borrow_mut().clear();
    recorder.script_result(Err(io::Error::from_raw_os_error(libc::EIO)));
    recorder.script_result(Err(io::Error::from_raw_os_error(libc::EIO)));

    Backend::on_vt_acquire(&mut backend, &mut state);

    let crtc = backend.platform.outputs[0].output.crtc;
    let attempts: Vec<_> = recorder
        .records()
        .into_iter()
        .filter(|record| record.kind == AtomicCommitKind::Scene)
        .collect();
    assert_eq!(attempts.len(), 2, "the refused device clear gets one retry");
    assert!(contains_vrr_property(&attempts[0], crtc, 0));
    assert!(matches!(
        attempts[0].result,
        ScriptedResult::Error {
            raw_os_error: Some(error),
            ..
        } if error == libc::EIO
    ));
    assert!(!has_vrr_property(&attempts[1], crtc));
    assert!(matches!(
        attempts[1].result,
        ScriptedResult::Error {
            raw_os_error: Some(error),
            ..
        } if error == libc::EIO
    ));
    assert!(
        backend.platform.outputs[0].output.vrr_committed.get(),
        "VT acquire refreshes the value left by the previous DRM master"
    );
    let events = events.borrow();
    let read = events
        .iter()
        .position(|event| *event == "read_vrr_enabled")
        .expect("VT acquire reads VRR_ENABLED from the CRTC");
    assert!(
        events
            .iter()
            .enumerate()
            .filter(|(_, event)| **event == "atomic_submit")
            .all(|(index, _)| index < read),
        "the refresh follows the device-wide clear attempt"
    );
}

fn accepted_direct_record(
    fixture: &mut super::recording_scene::RecordingSceneFixture,
    direct: &super::recording_scene::DirectPresentSetup,
    fd: std::os::fd::RawFd,
) -> AtomicCommitRecord {
    super::direct_unflip::submit_direct(fixture, direct, fd)
        .into_iter()
        .find(|record| super::support::accepted_event_id(record).is_some())
        .expect("Backend accepts the direct frame")
}

fn submit_direct_successor(
    fixture: &mut super::recording_scene::RecordingSceneFixture,
    direct: &mut super::recording_scene::DirectPresentSetup,
    fd: std::os::fd::RawFd,
) -> AtomicCommitRecord {
    let mut candidate = direct.candidate;
    candidate.present_id = candidate.present_id.wrapping_add(1);
    let mut event = direct.event.clone();
    event.present_id = candidate.present_id;
    event.serial = event.serial.wrapping_add(1);
    let offset = super::direct_unflip::record_count(fixture, fd);
    let mut accepted = false;
    for _ in 0..8 {
        accepted =
            Backend::try_present_direct(&mut fixture.events.backend, candidate, event.clone())
                .expect("successor Present through Backend");
        if accepted {
            break;
        }
    }
    assert!(
        accepted,
        "eligible direct successor {} reaches scanout (current={}, pending={}, queued={}, unflip={}, probation={})",
        candidate.present_id,
        fixture
            .events
            .backend
            .pstate
            .direct
            .root()
            .current
            .is_some(),
        fixture
            .events
            .backend
            .pstate
            .direct
            .root()
            .pending
            .is_some(),
        fixture
            .events
            .backend
            .pstate
            .direct
            .root()
            .queued_successor
            .is_some(),
        fixture.events.backend.pstate.direct.root().unflip_requested,
        fixture.events.backend.pstate.direct.root().eligible_streak,
    );
    direct.candidate = candidate;
    direct.event = event;
    super::direct_unflip::records_since(fixture, fd, offset)
        .into_iter()
        .find(|record| super::support::accepted_event_id(record).is_some())
        .expect("Backend accepts the direct successor")
}

fn queue_direct_successor(
    fixture: &mut super::recording_scene::RecordingSceneFixture,
    direct: &mut super::recording_scene::DirectPresentSetup,
) {
    let mut candidate = direct.candidate;
    candidate.present_id = candidate.present_id.wrapping_add(1);
    let mut event = direct.event.clone();
    event.present_id = candidate.present_id;
    event.serial = event.serial.wrapping_add(1);
    assert!(
        Backend::try_present_direct(&mut fixture.events.backend, candidate, event.clone())
            .expect("queued successor Present through Backend"),
        "eligible direct successor remains queued behind its predecessor"
    );
    direct.candidate = candidate;
    direct.event = event;
}

fn finish_direct_scanout(
    fixture: &mut super::recording_scene::RecordingSceneFixtureGuard,
    direct: &super::recording_scene::DirectPresentSetup,
    fd: std::os::fd::RawFd,
) {
    super::direct_unflip::request_unflip(fixture, direct);
    let offset = super::direct_unflip::record_count(fixture, fd);
    let unflip = super::direct_unflip::grouped_unflip_record(fixture, fd, offset, 1);
    let id = super::support::accepted_event_id(&unflip).expect("composed replacement ID");
    super::atomic_events::complete_recorded_attempt(fixture, fd, &unflip, id, 80);
    super::direct_unflip::drain_present_notifications(fixture, &[], &[direct.candidate.present_id]);
}

fn direct_fixture(
    present_id: u64,
    options: u32,
) -> Option<(
    super::recording_scene::RecordingSceneFixture,
    super::recording_scene::DirectPresentSetup,
)> {
    super::direct_unflip::direct_fixture(
        1,
        super::recording_scene::P1SceneRoute::Shared,
        present_id,
        options,
    )
}

fn direct_fixture_with_vrr_mode(
    present_id: u64,
    options: u32,
    mode: crate::launch::VrrMode,
) -> Option<(
    super::recording_scene::RecordingSceneFixture,
    super::recording_scene::DirectPresentSetup,
)> {
    let (mut fixture, direct) = direct_fixture(present_id, options)?;
    fixture.events.backend.pstate.vrr_mode = mode;
    Some((fixture, direct))
}

fn present_request_state(
    direct: &super::recording_scene::DirectPresentSetup,
) -> yserver_core::server::ServerState {
    use yserver_core::{
        backend::{PixmapHandle, WindowHandle},
        randr::{RR_ROTATE_0, RandrOutput, RandrState},
        resources::{ROOT_VISUAL, ROOT_WINDOW},
        server::ServerState,
    };
    use yserver_protocol::x11::{ClientId, CreatePixmapRequest, CreateWindowRequest, ResourceId};

    let mut state = ServerState::new();
    add_present_test_client(&mut state, 1);
    let window = direct.target_xid;
    let pixmap = direct.candidate.src_pixmap_xid;
    let width = direct.candidate.src_width;
    let height = direct.candidate.src_height;
    state.resources.create_window(
        ClientId(1),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(window),
            parent: ROOT_WINDOW,
            width,
            height,
            class: 1,
            visual: ROOT_VISUAL,
            ..Default::default()
        },
    );
    let _ = state.resources.map_window(ResourceId(window));
    state
        .resources
        .window_mut(ResourceId(window))
        .expect("direct destination window")
        .host_xid = WindowHandle::from_raw(window);
    state.resources.create_pixmap(
        ClientId(1),
        CreatePixmapRequest {
            depth: 24,
            pixmap: ResourceId(pixmap),
            drawable: ResourceId(window),
            width,
            height,
        },
    );
    assert!(state.resources.set_pixmap_host_xid(
        ResourceId(pixmap),
        PixmapHandle::from_raw(pixmap).expect("direct source pixmap host xid"),
    ));
    let crtc_id = direct.candidate.crtc_id;
    let output = RandrOutput {
        name: "test-vrr".to_owned(),
        output_id: 1,
        crtc_id,
        mode_id: crtc_id.wrapping_add(0x1000),
        connected: true,
        x: 0,
        y: 0,
        width,
        height,
        vrefresh: 60,
        timing: None,
        mm_width: 0,
        mm_height: 0,
        mode_ids: vec![crtc_id.wrapping_add(0x1000)],
        num_preferred: 1,
        pending_transform: Default::default(),
        current_transform: Default::default(),
        rotation: RR_ROTATE_0,
    };
    state.randr = RandrState::from_outputs(1, vec![output]);
    state.randr.primary_output = 1;
    state
}

fn add_present_test_client(state: &mut yserver_core::server::ServerState, client_id: u32) {
    use yserver_core::{resources::ROOT_WINDOW, server::ClientState, transport::Transport};
    use yserver_protocol::x11::ClientByteOrder;

    let (writer, _peer) = Transport::pair().expect("VRR test client transport");
    state.clients.insert(
        client_id,
        ClientState {
            writer: Arc::new(Mutex::new(writer)),
            is_local: true,
            fd_passing: true,
            internal: false,
            peer: yserver_core::peer::PeerIdentity::unknown_unix(),
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
            outbound: Default::default(),
            watching_writable: false,
            write_failed: false,
            gone: false,
            focused_window: ROOT_WINDOW,
            reader_control: None,
        },
    );
    assert!(state.clients.contains_key(&client_id));
}

fn request_vrr_property(
    state: &mut yserver_core::server::ServerState,
    backend: &mut KmsBackend,
    window: u32,
    property: u32,
    property_type: u32,
    format: u8,
    items: &[u32],
    sequence: u16,
) {
    request_vrr_property_for_client(
        state,
        backend,
        1,
        window,
        property,
        property_type,
        format,
        items,
        sequence,
    );
}

fn request_vrr_property_for_client(
    state: &mut yserver_core::server::ServerState,
    backend: &mut KmsBackend,
    client_id: u32,
    window: u32,
    property: u32,
    property_type: u32,
    format: u8,
    items: &[u32],
    sequence: u16,
) {
    use yserver_protocol::x11::{ClientId, RequestHeader, SequenceNumber};

    let mut body = vec![0_u8; 20];
    body[0..4].copy_from_slice(&window.to_le_bytes());
    body[4..8].copy_from_slice(&property.to_le_bytes());
    body[8..12].copy_from_slice(&property_type.to_le_bytes());
    body[12] = format;
    body[16..20].copy_from_slice(&u32::try_from(items.len()).unwrap().to_le_bytes());
    for item in items {
        match format {
            8 => body.push(u8::try_from(*item).unwrap()),
            _ => body.extend_from_slice(&item.to_le_bytes()),
        }
    }
    while !body.len().is_multiple_of(4) {
        body.push(0);
    }
    yserver_core::core_loop::process_request::process_request(
        state,
        backend,
        ClientId(client_id),
        SequenceNumber(sequence),
        RequestHeader {
            opcode: 18,
            data: 0,
            length_units: u32::try_from(1 + body.len() / 4).unwrap(),
        },
        &body,
        None,
    )
    .expect("ChangeProperty request");
}

fn delete_vrr_property(
    state: &mut yserver_core::server::ServerState,
    backend: &mut KmsBackend,
    window: u32,
    property: u32,
    sequence: u16,
) {
    delete_vrr_property_for_client(state, backend, 1, window, property, sequence);
}

fn delete_vrr_property_for_client(
    state: &mut yserver_core::server::ServerState,
    backend: &mut KmsBackend,
    client_id: u32,
    window: u32,
    property: u32,
    sequence: u16,
) {
    use yserver_protocol::x11::{ClientId, RequestHeader, SequenceNumber};

    let mut body = Vec::with_capacity(8);
    body.extend_from_slice(&window.to_le_bytes());
    body.extend_from_slice(&property.to_le_bytes());
    yserver_core::core_loop::process_request::process_request(
        state,
        backend,
        ClientId(client_id),
        SequenceNumber(sequence),
        RequestHeader {
            opcode: 19,
            data: 0,
            length_units: 3,
        },
        &body,
        None,
    )
    .expect("DeleteProperty request");
}

fn request_present(
    state: &mut yserver_core::server::ServerState,
    backend: &mut KmsBackend,
    window: u32,
    pixmap: u32,
    serial: u32,
    sequence: u16,
) {
    use yserver_protocol::x11::{ClientId, RequestHeader, SequenceNumber, present as x11present};

    let mut body = vec![0_u8; 68];
    body[0..4].copy_from_slice(&window.to_le_bytes());
    body[4..8].copy_from_slice(&pixmap.to_le_bytes());
    body[8..12].copy_from_slice(&serial.to_le_bytes());
    yserver_core::core_loop::process_request::process_request(
        state,
        backend,
        ClientId(1),
        SequenceNumber(sequence),
        RequestHeader {
            opcode: 145,
            data: x11present::PIXMAP,
            length_units: 18,
        },
        &body,
        None,
    )
    .expect("Present request");
}

fn prepare_property_present_source(
    fixture: &mut super::recording_scene::RecordingSceneFixture,
    direct: &super::recording_scene::DirectPresentSetup,
) {
    Backend::note_present_scanout_candidate(&mut fixture.events.backend, direct.candidate);
    let width = direct.candidate.src_width;
    let height = direct.candidate.src_height;
    let shape = crate::drm::async_flip::AsyncShape {
        fourcc: 0x3432_5258,
        modifier: 0,
        pitch: u32::from(width) * 4,
        width: u32::from(width),
        height: u32::from(height),
        crtcs: vec![handle(direct.candidate.crtc_id)],
        direct: true,
    };
    fixture.events.backend.pstate.scanout_m1.entries.insert(
        (
            direct.source_id,
            crate::kms::render::backend::DirectScope::Root,
        ),
        crate::kms::render::backend::ScanoutM1ProbeEntry::accepted_with_shape(
            crate::drm::modeset::DirectScanoutProbeFramebuffer::for_tests(
                super::recording_scene::raw(0x8130),
            ),
            shape,
        ),
    );
}

fn start_property_direct(
    fixture: &mut super::recording_scene::RecordingSceneFixture,
    direct: &super::recording_scene::DirectPresentSetup,
    variable_refresh: bool,
) -> (yserver_core::server::ServerState, u32, AtomicCommitRecord) {
    let fd = super::direct_unflip::fd(fixture);
    let mut state = present_request_state(direct);
    let window = direct.target_xid;
    let pixmap = direct.candidate.src_pixmap_xid;
    let property = state.atoms.intern("_VARIABLE_REFRESH", false).0;
    prepare_property_present_source(fixture, direct);
    if variable_refresh {
        request_vrr_property(
            &mut state,
            &mut fixture.events.backend,
            window,
            property,
            yserver_core::xinput::XA_CARDINAL.0,
            32,
            &[1],
            1,
        );
    }
    fixture
        .events
        .backend
        .pstate
        .direct
        .root_mut()
        .eligible_streak = crate::kms::render::backend::SCANOUT_M2_ELIGIBLE_ROOT_PROBATION - 1;
    let offset = super::direct_unflip::record_count(fixture, fd);
    request_present(
        &mut state,
        &mut fixture.events.backend,
        window,
        pixmap,
        u32::try_from(direct.candidate.present_id).expect("Present serial fits"),
        2,
    );
    let initial = super::direct_unflip::records_since(fixture, fd, offset)
        .into_iter()
        .find(|record| super::support::accepted_event_id(record).is_some())
        .expect("property-controlled initial Present is accepted");
    (state, property, initial)
}

fn submit_property_present(
    fixture: &mut super::recording_scene::RecordingSceneFixture,
    state: &mut yserver_core::server::ServerState,
    direct: &super::recording_scene::DirectPresentSetup,
    serial: u32,
    sequence: u16,
) -> Vec<AtomicCommitRecord> {
    let fd = super::direct_unflip::fd(fixture);
    let offset = super::direct_unflip::record_count(fixture, fd);
    request_present(
        state,
        &mut fixture.events.backend,
        direct.target_xid,
        direct.candidate.src_pixmap_xid,
        serial,
        sequence,
    );
    super::direct_unflip::records_since(fixture, fd, offset)
}

fn finish_property_direct_scanout(
    fixture: &mut super::recording_scene::RecordingSceneFixtureGuard,
    fd: std::os::fd::RawFd,
) {
    let current = fixture
        .events
        .backend
        .pstate
        .direct
        .root()
        .current
        .as_ref()
        .expect("property direct frame remains current");
    let direct = super::recording_scene::DirectPresentSetup {
        candidate: current.candidate,
        event: current.event.clone(),
        source_id: current.source_id,
        target_xid: current.candidate.dst_window_xid,
    };
    super::direct_unflip::request_unflip(fixture, &direct);
    let offset = super::direct_unflip::record_count(fixture, fd);
    let unflip = super::direct_unflip::grouped_unflip_record(fixture, fd, offset, 1);
    super::atomic_events::complete_recorded_attempt(
        fixture,
        fd,
        &unflip,
        super::support::accepted_event_id(&unflip).expect("unflip event ID"),
        89,
    );
    let completed = Backend::drain_completed_present_events(&mut fixture.events.backend);
    let idled = Backend::drain_retired_present_idle_events(&mut fixture.events.backend);
    assert!(
        completed.len() <= 1,
        "unflip retires at most one direct frame"
    );
    assert!(idled.len() <= 1, "unflip idles at most one direct frame");
}

fn add_property_window(state: &mut yserver_core::server::ServerState, window: u32, host_xid: u32) {
    add_property_window_for_client(state, 1, window, host_xid);
}

fn add_property_window_for_client(
    state: &mut yserver_core::server::ServerState,
    client_id: u32,
    window: u32,
    host_xid: u32,
) {
    use yserver_core::resources::ROOT_WINDOW;
    use yserver_protocol::x11::{ClientId, CreateWindowRequest, ResourceId};

    state.resources.create_window(
        ClientId(client_id),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(window),
            parent: ROOT_WINDOW,
            width: 800,
            height: 600,
            class: 1,
            visual: yserver_core::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    state
        .resources
        .window_mut(ResourceId(window))
        .expect("unrelated test window")
        .host_xid = yserver_core::backend::WindowHandle::from_raw(host_xid);
}

fn complete_direct_and_drain_present(
    fixture: &mut super::recording_scene::RecordingSceneFixture,
    record: &AtomicCommitRecord,
    present_id: u64,
    previous_present_id: Option<u64>,
) {
    let fd = super::direct_unflip::fd(fixture);
    let id = super::support::accepted_event_id(record).expect("direct frame event ID");
    super::direct_unflip::complete_direct_initial(fixture, record, id, fd);
    super::direct_unflip::drain_present_notifications(
        fixture,
        &[present_id],
        &previous_present_id.into_iter().collect::<Vec<_>>(),
    );
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: omit the primary-plane FB_ID from a standalone VRR clear.
fn vrr_standalone_clear_carries_current_primary_fb() {
    let Some((mut fixture, direct)) =
        direct_fixture_with_vrr_mode(18_047, 0, crate::launch::VrrMode::Property)
    else {
        eprintln!(
            "SKIP vrr_standalone_clear_carries_current_primary_fb: no Vulkan scene fixture available"
        );
        return;
    };
    set_vrr_capable(&mut fixture);
    let mut fixture = fixture.cleanup_guard();
    let fd = super::direct_unflip::fd(&fixture);
    let output = &fixture.events.backend.platform.outputs[0].output;
    let crtc = output.crtc;
    let plane = output.plane;
    let fb_id_property = output.plane_fb_id_prop;
    let (mut state, property, initial) = start_property_direct(&mut fixture, &direct, true);
    let direct_framebuffer = initial
        .properties
        .iter()
        .find(|update| update.object == plane.into() && update.property == fb_id_property)
        .map(|update| update.value)
        .expect("accepted direct frame sets the primary FB_ID");
    complete_direct_and_drain_present(&mut fixture, &initial, 1, None);

    let offset = super::direct_unflip::record_count(&fixture, fd);
    delete_vrr_property(
        &mut state,
        &mut fixture.events.backend,
        direct.target_xid,
        property,
        4,
    );
    Backend::maybe_composite(&mut fixture.events.backend)
        .expect("idle property clear scheduling pass");
    let clear = super::direct_unflip::records_since(&fixture, fd, offset)
        .into_iter()
        .find(|record| super::support::accepted_event_id(record).is_some())
        .expect("idle deletion submits one standalone clear");
    assert_eq!(clear.kind, AtomicCommitKind::VrrOnly);
    assert!(
        clear.properties.iter().any(|update| {
            update.object == crtc.into()
                && update.property == handle(VRR_PROP_ID)
                && update.value == 0
        }),
        "standalone clear sets VRR_ENABLED to zero"
    );
    let primary_plane_updates: Vec<_> = clear
        .properties
        .iter()
        .filter(|update| update.property == fb_id_property)
        .map(|update| (update.property, update.value))
        .collect();
    assert_eq!(
        primary_plane_updates,
        vec![(fb_id_property, direct_framebuffer)],
        "standalone clear changes only FB_ID, keeping the current direct framebuffer"
    );
    assert_eq!(
        clear.properties.len(),
        2,
        "standalone clear carries only VRR_ENABLED and the primary FB_ID"
    );
    assert_eq!(
        super::direct_unflip::records_since(&fixture, fd, offset)
            .iter()
            .filter(|record| super::support::accepted_event_id(record).is_some())
            .count(),
        1,
        "property deletion submits one accepted standalone clear"
    );

    let clear_id = super::support::accepted_event_id(&clear).expect("clear event ID");
    super::atomic_events::complete_recorded_attempt(&mut fixture, fd, &clear, clear_id, 90);
    finish_property_direct_scanout(&mut fixture, fd);
    fixture.finish_after_normal_drain();
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
    let fd = super::direct_unflip::fd(&fixture);
    let crtc = fixture.events.backend.platform.outputs[0].output.crtc;
    let (mut state, property, initial) = start_property_direct(&mut fixture, &direct, true);
    let direct_id = super::support::accepted_event_id(&initial).expect("direct event ID");
    super::direct_unflip::complete_direct_initial(&mut fixture, &initial, direct_id, fd);

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

    let offset = super::direct_unflip::record_count(&fixture, fd);
    delete_vrr_property(
        &mut state,
        &mut fixture.events.backend,
        direct.target_xid,
        property,
        4,
    );
    Backend::maybe_composite(&mut fixture.events.backend)
        .expect("idle property clear scheduling pass");
    let clear = super::direct_unflip::records_since(&fixture, fd, offset)
        .into_iter()
        .find(|record| super::support::accepted_event_id(record).is_some())
        .expect("idle deletion submits one standalone clear");
    assert_eq!(clear.kind, AtomicCommitKind::VrrOnly);
    assert!(contains_vrr_property(&clear, crtc, 0));

    let clear_id = super::support::accepted_event_id(&clear).expect("clear event ID");
    super::atomic_events::complete_recorded_attempt(&mut fixture, fd, &clear, clear_id, 91);
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
        "only the direct frame's original completion is pending"
    );
    assert!(Backend::drain_retired_present_idle_events(&mut fixture.events.backend).is_empty());

    finish_property_direct_scanout(&mut fixture, fd);
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: restore the retained-composed-framebuffer fallback for standalone VRR clears.
fn vrr_standalone_clear_without_direct_frame_is_not_submitted() {
    let Some(fixture) = super::recording_scene::RecordingSceneFixture::new(1)
        .expect("recording scene fixture construction")
    else {
        eprintln!(
            "SKIP vrr_standalone_clear_without_direct_frame_is_not_submitted: no Vulkan scene fixture available"
        );
        return;
    };
    let mut fixture = fixture.cleanup_guard();
    let (base, _elapsed_ns) = super::support::install_controlled_clock(&mut fixture.events.backend);
    set_vrr_capable(&mut fixture);
    let fd = super::direct_unflip::fd(&fixture);
    let recorder = fixture.events.recorders[&fd].clone();
    let crtc = fixture.events.backend.platform.outputs[0].output.crtc;
    let output_name = fixture.events.backend.platform.outputs[0]
        .key
        .connector_name
        .clone();
    let initial = super::atomic_events::submit_damaged_frame(&mut fixture)
        .into_iter()
        .find(|(record_fd, record)| {
            *record_fd == fd && super::support::accepted_event_id(record).is_some()
        })
        .map(|(_, record)| record)
        .expect("scene path accepts an initial composed frame");
    let initial_id = super::support::accepted_event_id(&initial).expect("scene event ID");
    super::atomic_events::complete_recorded_attempt(&mut fixture, fd, &initial, initial_id, 92);
    assert!(
        fixture
            .events
            .backend
            .pstate
            .direct
            .root()
            .current
            .is_none(),
        "there is no live direct frame on the output"
    );
    assert!(
        fixture
            .events
            .backend
            .platform
            .retained_composed_framebuffer(0)
            .is_some(),
        "the preexisting composed framebuffer is retained"
    );
    let output = &fixture.events.backend.platform.outputs[0].output;
    output.vrr_committed.set(true);
    output.vrr_clear_retry_deadline.set(Some(base));
    let before = recorder.records().len();
    super::recording_scene::clear_vrr_test_logs();
    Backend::maybe_composite(&mut fixture.events.backend)
        .expect("due standalone clear scheduling pass");
    assert_eq!(
        recorder.records().len(),
        before,
        "no standalone VRR request is submitted without a live direct frame"
    );
    assert!(
        fixture.events.backend.platform.outputs[0]
            .output
            .vrr_committed
            .get(),
        "the unsent VRR clear remains pending"
    );
    let logs = super::recording_scene::vrr_test_logs();
    assert!(
        logs.lines()
            .any(|line| { line.contains("standalone VRR clear") && line.contains(&output_name) }),
        "skipped standalone clear debug log names the output; logs={logs:?}"
    );

    let composed = super::atomic_events::submit_damaged_frame(&mut fixture)
        .into_iter()
        .find(|(record_fd, record)| {
            *record_fd == fd && super::support::accepted_event_id(record).is_some()
        })
        .map(|(_, record)| record)
        .expect("next composed frame is accepted");
    assert_eq!(composed.kind, AtomicCommitKind::Scene);
    assert!(
        contains_vrr_property(&composed, crtc, 0),
        "next composed commit carries the pending VRR clear"
    );
    assert!(
        !fixture.events.backend.platform.outputs[0]
            .output
            .vrr_committed
            .get(),
        "accepted composed commit records the clear"
    );
    let composed_id = super::support::accepted_event_id(&composed).expect("scene event ID");
    super::atomic_events::complete_recorded_attempt(&mut fixture, fd, &composed, composed_id, 93);
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: default VRR mode to Property; no direct commit may enable it by default.
fn vrr_off_mode_never_enables() {
    let Some((fixture, direct)) =
        direct_fixture_with_vrr_mode(18_101, 0, crate::launch::VrrMode::default())
    else {
        eprintln!("SKIP vrr_off_mode_never_enables: no Vulkan scene fixture available");
        return;
    };
    let mut fixture = fixture.cleanup_guard();
    set_vrr_capable(&mut fixture);
    let fd = super::direct_unflip::fd(&fixture);
    let crtc = fixture.events.backend.platform.outputs[0].output.crtc;

    let _direct = accepted_direct_record(&mut fixture, &direct, fd);
    let records = fixture.events.recorders[&fd].records();

    assert!(
        records
            .iter()
            .all(|record| !contains_vrr_property(record, crtc, 1)),
        "default Off mode never enables VRR on any commit"
    );
    assert!(
        !fixture.events.backend.platform.outputs[0]
            .output
            .vrr_committed
            .get(),
        "the kernel-held VRR state remains off"
    );
    let id = super::support::accepted_event_id(&_direct).expect("direct event ID");
    super::direct_unflip::complete_direct_initial(&mut fixture, &_direct, id, fd);
    super::direct_unflip::drain_present_notifications(
        &mut fixture,
        &[direct.candidate.present_id],
        &[],
    );
    finish_direct_scanout(&mut fixture, &direct, fd);
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: skip the clear when mode is Off; an inherited 1 must still be cleared.
fn vrr_off_mode_still_clears_inherited() {
    let Some((fixture, direct)) =
        direct_fixture_with_vrr_mode(18_102, 0, crate::launch::VrrMode::Off)
    else {
        eprintln!("SKIP vrr_off_mode_still_clears_inherited: no Vulkan scene fixture available");
        return;
    };
    let mut fixture = fixture.cleanup_guard();
    set_vrr_capable(&mut fixture);
    fixture.events.backend.platform.outputs[0]
        .output
        .vrr_committed
        .set(true);
    let fd = super::direct_unflip::fd(&fixture);
    let crtc = fixture.events.backend.platform.outputs[0].output.crtc;

    let direct_record = accepted_direct_record(&mut fixture, &direct, fd);

    assert!(
        contains_vrr_property(&direct_record, crtc, 0),
        "the next direct commit clears the inherited value in default Off mode"
    );
    assert!(
        !fixture.events.backend.platform.outputs[0]
            .output
            .vrr_committed
            .get(),
        "the accepted clear updates the recorded kernel-held value"
    );
    let id = super::support::accepted_event_id(&direct_record).expect("direct event ID");
    super::direct_unflip::complete_direct_initial(&mut fixture, &direct_record, id, fd);
    super::direct_unflip::drain_present_notifications(
        &mut fixture,
        &[direct.candidate.present_id],
        &[],
    );
    finish_direct_scanout(&mut fixture, &direct, fd);
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: read Root's unflip reason for every output.
fn vrr_off_log_names_the_output_owner_reason() {
    use crate::kms::render::backend::DirectScope;

    super::recording_scene::init_tearing_test_logger();
    let Some(fixture) = super::recording_scene::RecordingSceneFixture::with_route(
        2,
        super::recording_scene::P1SceneRoute::Shared,
    )
    .expect("two-output VRR logging fixture") else {
        eprintln!("SKIP vrr_off_log_names_the_output_owner_reason: Vulkan fixture unavailable");
        return;
    };
    let mut fixture = fixture.cleanup_guard();
    fixture.events.backend.pstate.vrr_mode = crate::launch::VrrMode::Property;
    for layout in &mut fixture.events.backend.platform.outputs {
        layout.output.crtc_vrr_enabled_prop = Some(handle(VRR_PROP_ID));
        layout.output.connector_vrr_capable = true;
    }

    let direct = fixture.install_direct_output_present(1, 18_201, 0);
    let fd = super::direct_unflip::fd(&fixture);
    let output0_crtc = fixture.events.backend.platform.outputs[0].output.crtc;
    let output1_crtc = fixture.events.backend.platform.outputs[1].output.crtc;
    let output_name = fixture.events.backend.platform.outputs[1]
        .key
        .connector_name
        .clone();
    let direct_record =
        super::direct_unflip::submit_output_present_via_backend(&mut fixture, &direct, 1);
    let direct_id = super::support::accepted_event_id(&direct_record)
        .expect("Output(1) direct commit has an accepted event ID");
    assert!(contains_vrr_property(&direct_record, output1_crtc, 1));
    assert!(!has_vrr_property(&direct_record, output0_crtc));
    assert!(
        fixture.events.backend.platform.outputs[1]
            .output
            .vrr_committed
            .get(),
        "the accepted Output(1) direct commit records VRR on"
    );
    assert_eq!(
        fixture.events.backend.pstate.direct.scope_for_output(1),
        Some(DirectScope::Output(1)),
        "the direct frame is owned by Output(1)"
    );
    super::atomic_events::complete_recorded_attempt(
        &mut fixture,
        fd,
        &direct_record,
        direct_id,
        92,
    );
    super::direct_unflip::drain_present_notifications(
        &mut fixture,
        &[direct.candidate.present_id],
        &[],
    );

    super::recording_scene::clear_vrr_test_logs();
    Backend::destroy_subwindow(&mut fixture.events.backend, None, direct.target_xid)
        .expect("destroy Output(1)'s direct window through Backend");
    assert_eq!(
        fixture.events.backend.pstate.direct.outputs[1].unflip_last_reason,
        Some("destroy_direct_frame_drawable"),
        "the affected owner records the window-destroy reason"
    );
    let replacement_start = super::direct_unflip::record_count(&fixture, fd);
    Backend::maybe_composite(&mut fixture.events.backend)
        .expect("submit Output(1)'s composed replacement through Backend");
    let replacements = super::direct_unflip::records_since(&fixture, fd, replacement_start);
    let replacement = replacements
        .iter()
        .find(|record| {
            super::support::accepted_event_id(record).is_some()
                && record.events.iter().any(|event| event.crtc == output1_crtc)
        })
        .expect("an accepted composed replacement reaches Output(1)");
    assert!(
        contains_vrr_property(replacement, output1_crtc, 0),
        "the composed replacement clears Output(1)'s committed VRR property"
    );
    assert!(
        !fixture.events.backend.platform.outputs[1]
            .output
            .vrr_committed
            .get(),
        "the accepted replacement records VRR off for Output(1)"
    );
    let output_owner_reason = fixture.events.backend.pstate.direct.outputs[1]
        .unflip_last_reason
        .expect("Output(1) retains its latest unflip reason through replacement submission");
    let expected_output_reason = format!(
        "kms: vrr off output={output_name} crtc={} reason={output_owner_reason}",
        u32::from(output1_crtc)
    );
    let logs = super::recording_scene::vrr_test_logs();
    assert!(
        logs.lines().any(|line| line == expected_output_reason),
        "Output(1)'s VRR-off line uses its owner reason; logs={logs:?}"
    );
    for (offset, record) in replacements.iter().enumerate() {
        let Some(event_id) = super::support::accepted_event_id(record) else {
            continue;
        };
        super::atomic_events::complete_recorded_attempt(
            &mut fixture,
            fd,
            record,
            event_id,
            93 + u32::try_from(offset).expect("replacement index fits"),
        );
    }
    super::direct_unflip::drain_present_notifications(
        &mut fixture,
        &[],
        &[direct.candidate.present_id],
    );
    assert!(
        fixture.events.backend.pstate.direct.outputs[1]
            .current
            .is_none(),
        "Output(1)'s direct frame retires after its accepted replacement"
    );
    fixture.finish_after_normal_drain();

    let Some((fixture, direct)) = direct_fixture(18_202, 0) else {
        eprintln!(
            "SKIP vrr_off_log_names_the_output_owner_reason: Root Vulkan fixture unavailable"
        );
        return;
    };
    let mut fixture = fixture.cleanup_guard();
    set_vrr_capable(&mut fixture);
    fixture.events.backend.pstate.vrr_mode = crate::launch::VrrMode::Property;
    let fd = super::direct_unflip::fd(&fixture);
    let crtc = fixture.events.backend.platform.outputs[0].output.crtc;
    let output_name = fixture.events.backend.platform.outputs[0]
        .key
        .connector_name
        .clone();
    let direct_record = accepted_direct_record(&mut fixture, &direct, fd);
    let direct_id = super::support::accepted_event_id(&direct_record)
        .expect("Root direct commit has an accepted event ID");
    assert!(contains_vrr_property(&direct_record, crtc, 1));
    super::direct_unflip::complete_direct_initial(&mut fixture, &direct_record, direct_id, fd);
    super::direct_unflip::drain_present_notifications(
        &mut fixture,
        &[direct.candidate.present_id],
        &[],
    );
    assert_eq!(
        fixture.events.backend.pstate.direct.scope_for_output(0),
        Some(DirectScope::Root),
        "the single output is owned by Root"
    );

    super::recording_scene::clear_vrr_test_logs();
    Backend::destroy_subwindow(&mut fixture.events.backend, None, direct.target_xid)
        .expect("destroy Root's direct window through Backend");
    assert_eq!(
        fixture
            .events
            .backend
            .pstate
            .direct
            .root()
            .unflip_last_reason,
        Some("destroy_direct_frame_drawable"),
        "Root records the window-destroy reason"
    );
    let replacement_start = super::direct_unflip::record_count(&fixture, fd);
    Backend::maybe_composite(&mut fixture.events.backend)
        .expect("submit Root's composed replacement through Backend");
    let replacements = super::direct_unflip::records_since(&fixture, fd, replacement_start);
    let replacement = replacements
        .iter()
        .find(|record| {
            super::support::accepted_event_id(record).is_some()
                && record.events.iter().any(|event| event.crtc == crtc)
        })
        .expect("an accepted composed replacement reaches the Root output");
    assert!(contains_vrr_property(replacement, crtc, 0));
    assert!(
        !fixture.events.backend.platform.outputs[0]
            .output
            .vrr_committed
            .get(),
        "the accepted Root replacement records VRR off"
    );
    let root_owner_reason = fixture
        .events
        .backend
        .pstate
        .direct
        .root()
        .unflip_last_reason
        .expect("Root retains its latest unflip reason through replacement submission");
    let expected_root_reason = format!(
        "kms: vrr off output={output_name} crtc={} reason={root_owner_reason}",
        u32::from(crtc)
    );
    let logs = super::recording_scene::vrr_test_logs();
    assert!(
        logs.lines().any(|line| line == expected_root_reason),
        "the Root-owned output keeps Root's unflip reason; logs={logs:?}"
    );
    for (offset, record) in replacements.iter().enumerate() {
        let Some(event_id) = super::support::accepted_event_id(record) else {
            continue;
        };
        super::atomic_events::complete_recorded_attempt(
            &mut fixture,
            fd,
            record,
            event_id,
            94 + u32::try_from(offset).expect("replacement index fits"),
        );
    }
    super::direct_unflip::drain_present_notifications(
        &mut fixture,
        &[],
        &[direct.candidate.present_id],
    );
    assert!(
        fixture
            .events
            .backend
            .pstate
            .direct
            .root()
            .current
            .is_none()
    );
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: restore the CARDINAL type check or the `== 1` check; INTEGER/1 and CARDINAL/2 must enable VRR.
fn vrr_property_mode_reads_like_xorg() {
    let Some((fixture, mut direct)) =
        direct_fixture_with_vrr_mode(18_103, 0, crate::launch::VrrMode::Property)
    else {
        eprintln!("SKIP vrr_property_mode_reads_like_xorg: no Vulkan scene fixture available");
        return;
    };
    let mut fixture = fixture.cleanup_guard();
    set_vrr_capable(&mut fixture);
    let fd = super::direct_unflip::fd(&fixture);
    let crtc = fixture.events.backend.platform.outputs[0].output.crtc;
    let mut state = present_request_state(&direct);
    let window = direct.target_xid;
    let pixmap = direct.candidate.src_pixmap_xid;
    let property = state.atoms.intern("_VARIABLE_REFRESH", false).0;
    Backend::note_present_scanout_candidate(&mut fixture.events.backend, direct.candidate);
    let width = direct.candidate.src_width;
    let height = direct.candidate.src_height;
    let crtc_handle = handle(direct.candidate.crtc_id);
    let shape = crate::drm::async_flip::AsyncShape {
        fourcc: 0x3432_5258,
        modifier: 0,
        pitch: u32::from(width) * 4,
        width: u32::from(width),
        height: u32::from(height),
        crtcs: vec![crtc_handle],
        direct: true,
    };
    fixture.events.backend.pstate.scanout_m1.entries.insert(
        (
            direct.source_id,
            crate::kms::render::backend::DirectScope::Root,
        ),
        crate::kms::render::backend::ScanoutM1ProbeEntry::accepted_with_shape(
            crate::drm::modeset::DirectScanoutProbeFramebuffer::for_tests(
                super::recording_scene::raw(0x8130),
            ),
            shape,
        ),
    );
    request_vrr_property(
        &mut state,
        &mut fixture.events.backend,
        window,
        property,
        yserver_core::xinput::XA_INTEGER.0,
        32,
        &[1],
        1,
    );

    // The permission comes from the same ChangeProperty -> Present request
    // path used by clients. The first direct frame accepted after the
    // existing probation must carry VRR_ENABLED=1.
    let first_request_serial = u32::try_from(direct.candidate.present_id).unwrap();
    fixture
        .events
        .backend
        .pstate
        .direct
        .root_mut()
        .eligible_streak = crate::kms::render::backend::SCANOUT_M2_ELIGIBLE_ROOT_PROBATION - 1;
    let start = super::direct_unflip::record_count(&fixture, fd);
    request_present(
        &mut state,
        &mut fixture.events.backend,
        window,
        pixmap,
        first_request_serial,
        2,
    );
    let enabled = super::direct_unflip::records_since(&fixture, fd, start)
        .into_iter()
        .find(|record| super::support::accepted_event_id(record).is_some())
        .expect("first direct Present request is accepted through Backend");
    assert!(
        contains_vrr_property(&enabled, crtc, 1),
        "the first direct commit from an INTEGER/32 value 1 Present enables VRR"
    );
    let fb_id_property = fixture.events.backend.platform.outputs[0]
        .output
        .plane_fb_id_prop;
    let direct_framebuffer = enabled
        .properties
        .iter()
        .find(|update| update.property == fb_id_property)
        .map(|update| update.value)
        .expect("accepted direct frame sets the primary FB_ID");
    assert!(
        fixture.events.backend.platform.outputs[0]
            .output
            .vrr_committed
            .get(),
        "the accepted request records VRR as committed"
    );
    let id = super::support::accepted_event_id(&enabled).expect("enabled direct event ID");
    super::direct_unflip::complete_direct_initial(&mut fixture, &enabled, id, fd);
    super::direct_unflip::drain_present_notifications(&mut fixture, &[1], &[]);

    let property_cases = [
        (yserver_core::xinput::XA_CARDINAL.0, 32, vec![0], false),
        (yserver_core::xinput::XA_CARDINAL.0, 32, vec![2], true),
        (yserver_core::xinput::XA_CARDINAL.0, 8, vec![1], false),
        (yserver_core::xinput::XA_CARDINAL.0, 32, vec![1, 2], false),
    ];
    let mut previous_present_id = 1_u64;
    let mut previous_request_serial = first_request_serial;
    let mut sequence = 10_u16;
    for (property_type, format, values, expected_enabled) in property_cases {
        let was_committed = fixture.events.backend.platform.outputs[0]
            .output
            .vrr_committed
            .get();
        let property_change_offset = super::direct_unflip::record_count(&fixture, fd);
        request_vrr_property(
            &mut state,
            &mut fixture.events.backend,
            window,
            property,
            property_type,
            format,
            &values,
            sequence,
        );
        if was_committed && !expected_enabled {
            let clear = super::direct_unflip::records_since(&fixture, fd, property_change_offset)
                .into_iter()
                .find(|record| super::support::accepted_event_id(record).is_some())
                .expect("property change clears committed VRR while the CRTC is idle");
            assert_eq!(clear.kind, AtomicCommitKind::VrrOnly);
            assert!(contains_vrr_property(&clear, crtc, 0));
            assert_eq!(
                clear
                    .properties
                    .iter()
                    .filter(|update| update.property == fb_id_property)
                    .map(|update| update.value)
                    .collect::<Vec<_>>(),
                vec![direct_framebuffer],
                "property clear carries the current primary framebuffer"
            );
            assert_eq!(clear.properties.len(), 2);
            let clear_id = super::support::accepted_event_id(&clear).expect("clear event ID");
            super::atomic_events::complete_recorded_attempt(&mut fixture, fd, &clear, clear_id, 71);
        }
        let request_serial = previous_request_serial + 1;
        let present_id = previous_present_id + 1;
        let offset = super::direct_unflip::record_count(&fixture, fd);
        request_present(
            &mut state,
            &mut fixture.events.backend,
            window,
            pixmap,
            request_serial,
            sequence + 1,
        );
        let record = super::direct_unflip::records_since(&fixture, fd, offset)
            .into_iter()
            .find(|record| super::support::accepted_event_id(record).is_some())
            .expect("request-driven direct successor is accepted");
        assert_eq!(
            fixture.events.backend.platform.outputs[0]
                .output
                .vrr_committed
                .get(),
            expected_enabled,
            "CARDINAL/{format} {values:?} records the expected VRR end state"
        );
        if expected_enabled && !was_committed {
            assert!(
                contains_vrr_property(&record, crtc, 1),
                "CARDINAL/32 value 2 enables VRR on the next direct Present"
            );
        } else if !expected_enabled {
            assert!(
                !contains_vrr_property(&record, crtc, 1),
                "CARDINAL/{format} {values:?} must not set VRR_ENABLED"
            );
        }
        let event_id = super::support::accepted_event_id(&record).expect("successor event ID");
        super::direct_unflip::complete_direct_initial(&mut fixture, &record, event_id, fd);
        super::direct_unflip::drain_present_notifications(
            &mut fixture,
            &[present_id],
            &[previous_present_id],
        );
        previous_present_id = present_id;
        previous_request_serial = request_serial;
        sequence += 2;
    }

    delete_vrr_property(
        &mut state,
        &mut fixture.events.backend,
        window,
        property,
        sequence,
    );
    let request_serial = previous_request_serial + 1;
    let present_id = previous_present_id + 1;
    let offset = super::direct_unflip::record_count(&fixture, fd);
    request_present(
        &mut state,
        &mut fixture.events.backend,
        window,
        pixmap,
        request_serial,
        sequence + 1,
    );
    let no_property = super::direct_unflip::records_since(&fixture, fd, offset)
        .into_iter()
        .find(|record| super::support::accepted_event_id(record).is_some())
        .expect("property-less request-driven direct successor is accepted");
    assert!(
        !contains_vrr_property(&no_property, crtc, 1),
        "a missing _VARIABLE_REFRESH property must not set VRR_ENABLED"
    );
    assert!(
        !fixture.events.backend.platform.outputs[0]
            .output
            .vrr_committed
            .get(),
        "a missing property leaves the committed VRR state off"
    );
    let event_id = super::support::accepted_event_id(&no_property).expect("successor event ID");
    super::direct_unflip::complete_direct_initial(&mut fixture, &no_property, event_id, fd);
    super::direct_unflip::drain_present_notifications(
        &mut fixture,
        &[present_id],
        &[previous_present_id],
    );
    direct.candidate.present_id = present_id;
    direct.event.present_id = present_id;
    direct.event.serial = request_serial;
    finish_direct_scanout(&mut fixture, &direct, fd);
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: never append VRR_ENABLED to an eligible direct frame.
fn vrr_direct_unredirected_enables() {
    let Some((fixture, direct)) = direct_fixture(18_001, 0) else {
        eprintln!("skip: no Vulkan scene fixture available");
        return;
    };
    let mut fixture = fixture.cleanup_guard();
    set_vrr_capable(&mut fixture);
    let fd = super::direct_unflip::fd(&fixture);
    let crtc = fixture.events.backend.platform.outputs[0].output.crtc;
    let record = accepted_direct_record(&mut fixture, &direct, fd);

    assert!(contains_vrr_property(&record, crtc, 1));
    assert!(
        fixture.events.backend.platform.outputs[0]
            .output
            .vrr_committed
            .get()
    );
    assert_eq!(
        fixture
            .events
            .backend
            .pstate
            .direct
            .root()
            .pending
            .as_ref()
            .and_then(|frame| frame.accepted_event_id),
        super::support::accepted_event_id(&record),
        "the frame carrying VRR is retained for retirement"
    );
    let id = super::support::accepted_event_id(&record).expect("accepted direct commit ID");
    super::direct_unflip::complete_direct_initial(&mut fixture, &record, id, fd);
    super::direct_unflip::drain_present_notifications(
        &mut fixture,
        &[direct.candidate.present_id],
        &[],
    );
    assert!(
        fixture
            .events
            .backend
            .pstate
            .direct
            .root()
            .current
            .is_some()
    );
    finish_direct_scanout(&mut fixture, &direct, fd);
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: omit VRR_ENABLED = 0 from the composed replacement after unflip.
fn vrr_unflip_clears() {
    let Some((fixture, direct)) = direct_fixture(18_002, 0) else {
        eprintln!("skip: no Vulkan scene fixture available");
        return;
    };
    let mut fixture = fixture.cleanup_guard();
    set_vrr_capable(&mut fixture);
    let fd = super::direct_unflip::fd(&fixture);
    let crtc = fixture.events.backend.platform.outputs[0].output.crtc;
    let direct_record = accepted_direct_record(&mut fixture, &direct, fd);
    let id = super::support::accepted_event_id(&direct_record).expect("direct event ID");
    super::direct_unflip::complete_direct_initial(&mut fixture, &direct_record, id, fd);
    super::direct_unflip::drain_present_notifications(
        &mut fixture,
        &[direct.candidate.present_id],
        &[],
    );

    super::direct_unflip::request_unflip(&mut fixture, &direct);
    let offset = super::direct_unflip::record_count(&fixture, fd);
    let unflip = super::direct_unflip::grouped_unflip_record(&mut fixture, fd, offset, 1);
    assert!(contains_vrr_property(&unflip, crtc, 0));
    assert!(
        !fixture.events.backend.platform.outputs[0]
            .output
            .vrr_committed
            .get()
    );
    let unflip_id = super::support::accepted_event_id(&unflip).expect("unflip event ID");
    super::atomic_events::complete_recorded_attempt(&mut fixture, fd, &unflip, unflip_id, 51);
    super::direct_unflip::drain_present_notifications(
        &mut fixture,
        &[],
        &[direct.candidate.present_id],
    );

    let later = super::atomic_events::submit_damaged_frame(&mut fixture);
    let later = later
        .iter()
        .find(|(_, record)| super::support::accepted_event_id(record).is_some())
        .expect("later scene frame accepted")
        .1
        .clone();
    assert!(!contains_vrr_property(&later, crtc, 0));
    assert!(!contains_vrr_property(&later, crtc, 1));
    super::atomic_events::complete_recorded_attempt(
        &mut fixture,
        fd,
        &later,
        super::support::accepted_event_id(&later).unwrap(),
        52,
    );
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: accept every direct owner as VRR-worthy, including Cow and CowDescendant.
fn vrr_cow_owner_never_enables() {
    for owner in ["cow", "cow_descendant"] {
        let Some((fixture, mut direct)) = direct_fixture(18_003, 0) else {
            eprintln!("skip: no Vulkan scene fixture available");
            return;
        };
        let mut fixture = fixture.cleanup_guard();
        set_vrr_capable(&mut fixture);
        Backend::get_overlay_window(&mut fixture.events.backend, None)
            .expect("materialize compositor overlay window");
        let cow_xid = fixture
            .events
            .backend
            .cow_host_xid()
            .expect("overlay window host XID");
        let root_width = fixture.events.backend.platform.fb_w;
        let root_height = fixture.events.backend.platform.fb_h;
        if owner == "cow" {
            direct.candidate.paint_dst_host_xid = cow_xid;
            direct.candidate.dst_window_xid = cow_xid;
            direct.candidate.completion_dst_host_xid = cow_xid;
            direct.event.dst_host_xid = cow_xid;
        } else {
            let target = fixture
                .events
                .backend
                .windows
                .get_mut(&direct.target_xid)
                .expect("direct target geometry");
            target.parent = Some(cow_xid);
            target.width = root_width;
            target.height = root_height;
        }
        let fd = super::direct_unflip::fd(&fixture);
        let crtc = fixture.events.backend.platform.outputs[0].output.crtc;
        let record = accepted_direct_record(&mut fixture, &direct, fd);
        assert!(!has_vrr_property(&record, crtc), "owner={owner}");
        assert!(
            !fixture.events.backend.platform.outputs[0]
                .output
                .vrr_committed
                .get(),
            "owner={owner} must leave VRR off"
        );
        let id = super::support::accepted_event_id(&record).expect("direct event ID");
        super::direct_unflip::complete_direct_initial(&mut fixture, &record, id, fd);
        super::direct_unflip::drain_present_notifications(
            &mut fixture,
            &[direct.candidate.present_id],
            &[],
        );
        finish_direct_scanout(&mut fixture, &direct, fd);
        if Backend::prepare_overlay_release(&mut fixture.events.backend, None)
            .expect("release compositor overlay window")
        {
            Backend::retire_overlay_window(&mut fixture.events.backend, None);
        }
        fixture.finish_after_normal_drain();
    }
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutations: write zero unconditionally, add a frame, alter property values, or change flags.
fn vrr_not_capable_stream_unchanged() {
    let cases = [
        (
            "connector vrr_capable absent",
            false,
            Some(handle(VRR_PROP_ID)),
        ),
        (
            "connector vrr_capable is zero",
            false,
            Some(handle(VRR_PROP_ID)),
        ),
        ("CRTC has no VRR_ENABLED", true, None),
    ];
    let flags = ::drm::control::AtomicCommitFlags::PAGE_FLIP_EVENT
        | ::drm::control::AtomicCommitFlags::NONBLOCK;
    let prop = |id, value| ("1".to_owned(), format!("property::Handle({id})"), value);
    let direct_properties = vec![
        prop(71, 51028),
        prop(72, 1),
        prop(73, 0),
        prop(74, 0),
        prop(75, 52428800),
        prop(76, 39321600),
        prop(77, 0),
        prop(78, 0),
        prop(79, 800),
        prop(80, 600),
    ];
    let expected: Vec<CommitSignature> = vec![
        (AtomicCommitKind::Scene, flags, direct_properties.clone()),
        (AtomicCommitKind::Scene, flags, direct_properties),
        (
            AtomicCommitKind::Scene,
            flags,
            vec![
                prop(71, 20993),
                prop(72, 1),
                prop(73, 0),
                prop(74, 0),
                prop(75, 0),
                prop(76, 0),
                prop(77, 0),
                prop(78, 0),
                prop(79, 0),
                prop(80, 0),
                prop(81, 0),
            ],
        ),
        (
            AtomicCommitKind::Scene,
            flags,
            vec![prop(71, 20992), prop(72, 1), prop(81, 0), prop(82, 0)],
        ),
    ];
    for (label, connector_capable, property) in cases {
        let Some((fixture, mut direct)) = direct_fixture(18_004, 0) else {
            eprintln!("skip: no Vulkan scene fixture available");
            return;
        };
        let mut fixture = fixture.cleanup_guard();
        let output = &mut fixture.events.backend.platform.outputs[0].output;
        output.crtc_vrr_enabled_prop = property;
        output.connector_vrr_capable = connector_capable;
        output.vrr_committed.set(false);
        output.vrr_disabled.set(false);
        output.vrr_clear_retry_used.set(false);
        let fd = super::direct_unflip::fd(&fixture);
        let recorder = fixture.events.recorders[&fd].clone();
        recorder.clear_records();
        let crtc = fixture.events.backend.platform.outputs[0].output.crtc;
        let first = accepted_direct_record(&mut fixture, &direct, fd);
        let first_id = super::support::accepted_event_id(&first).expect("direct event ID");
        let first_present_id = direct.candidate.present_id;
        super::direct_unflip::complete_direct_initial(&mut fixture, &first, first_id, fd);
        super::direct_unflip::drain_present_notifications(
            &mut fixture,
            &[direct.candidate.present_id],
            &[],
        );
        {
            let events = &mut fixture.events;
            Backend::warp_pointer_root(&mut events.backend, &mut events.state, 111, 77);
        }
        let second = submit_direct_successor(&mut fixture, &mut direct, fd);
        let second_id = super::support::accepted_event_id(&second).expect("second event ID");
        assert_eq!(
            (
                fixture.events.backend.core.cursor_x,
                fixture.events.backend.core.cursor_y
            ),
            (111.0, 77.0),
            "cursor motion is applied while direct scanout continues"
        );
        super::direct_unflip::complete_direct_initial(&mut fixture, &second, second_id, fd);
        super::direct_unflip::drain_present_notifications(
            &mut fixture,
            &[direct.candidate.present_id],
            &[first_present_id],
        );
        super::direct_unflip::request_unflip(&mut fixture, &direct);
        let offset = super::direct_unflip::record_count(&fixture, fd);
        let unflip = super::direct_unflip::grouped_unflip_record(&mut fixture, fd, offset, 1);
        let unflip_id = super::support::accepted_event_id(&unflip).expect("unflip event ID");
        super::atomic_events::complete_recorded_attempt(&mut fixture, fd, &unflip, unflip_id, 61);
        super::direct_unflip::drain_present_notifications(
            &mut fixture,
            &[],
            &[direct.candidate.present_id],
        );
        let scene = super::atomic_events::submit_damaged_frame(&mut fixture);
        let scene = scene
            .iter()
            .find(|(_, record)| super::support::accepted_event_id(record).is_some())
            .expect("scene frame accepted")
            .1
            .clone();
        let records = recorder.records();
        let signature: Vec<_> = records
            .iter()
            .map(|record| {
                (
                    record.kind,
                    record.flags,
                    record
                        .properties
                        .iter()
                        .map(|update| {
                            (
                                format!("{:?}", update.object),
                                format!("{:?}", update.property),
                                if matches!(u32::from(update.property), 81 | 82) {
                                    0
                                } else {
                                    update.value
                                },
                            )
                        })
                        .collect(),
                )
            })
            .collect();
        super::atomic_events::complete_recorded_attempt(
            &mut fixture,
            fd,
            &scene,
            super::support::accepted_event_id(&scene).unwrap(),
            62,
        );
        fixture.finish_after_normal_drain();
        assert!(
            records.iter().all(|record| {
                !record.properties.iter().any(|update| {
                    update.object == crtc.into() && update.property == handle(VRR_PROP_ID)
                })
            }),
            "{label} emits no VRR property"
        );
        assert_eq!(
            signature, expected,
            "{label} changes the pre-VRR commit stream"
        );
    }
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: append VRR_ENABLED to the async direct request instead of deferring it.
fn vrr_async_flip_defers_to_next_vsync_commit() {
    let options = yserver_core::present_scheduler::PRESENT_OPTION_ASYNC;
    let Some((fixture, mut direct)) = direct_fixture(18_005, options) else {
        eprintln!("skip: no Vulkan scene fixture available");
        return;
    };
    let mut fixture = fixture.cleanup_guard();
    set_vrr_capable(&mut fixture);
    let fd = super::direct_unflip::fd(&fixture);
    super::direct_unflip::enter_async_tearing(&mut fixture, &direct);
    let crtc = fixture.events.backend.platform.outputs[0].output.crtc;
    let async_record = accepted_direct_record(&mut fixture, &direct, fd);
    assert_eq!(async_record.kind, AtomicCommitKind::Async);
    assert!(!has_vrr_property(&async_record, crtc));
    let async_id = super::support::accepted_event_id(&async_record).expect("async event ID");
    super::direct_unflip::complete_direct_initial(&mut fixture, &async_record, async_id, fd);
    super::direct_unflip::drain_present_notifications(
        &mut fixture,
        &[direct.candidate.present_id],
        &[],
    );

    direct.candidate.options = 0;
    let vsync = submit_direct_successor(&mut fixture, &mut direct, fd);
    assert_ne!(vsync.kind, AtomicCommitKind::Async);
    assert!(contains_vrr_property(&vsync, crtc, 1));
    assert!(
        fixture.events.backend.platform.outputs[0]
            .output
            .vrr_committed
            .get()
    );
    let vsync_id = super::support::accepted_event_id(&vsync).expect("vsync event ID");
    super::direct_unflip::complete_direct_initial(&mut fixture, &vsync, vsync_id, fd);
    super::direct_unflip::drain_present_notifications(
        &mut fixture,
        &[direct.candidate.present_id],
        &[direct.candidate.present_id - 1],
    );
    finish_direct_scanout(&mut fixture, &direct, fd);
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: fail to latch VRR off after EINVAL and an accepted no-property retry.
fn vrr_einval_latches_and_frame_goes_out() {
    let Some((fixture, mut direct)) = direct_fixture(18_006, 0) else {
        eprintln!("skip: no Vulkan scene fixture available");
        return;
    };
    let mut fixture = fixture.cleanup_guard();
    let fd = super::direct_unflip::fd(&fixture);
    let recorder = fixture.events.recorders[&fd].clone();
    let first = accepted_direct_record(&mut fixture, &direct, fd);
    let first_id = super::support::accepted_event_id(&first).expect("initial frame event ID");
    let first_present_id = direct.candidate.present_id;
    super::direct_unflip::complete_direct_initial(&mut fixture, &first, first_id, fd);
    super::direct_unflip::drain_present_notifications(&mut fixture, &[first_present_id], &[]);
    set_vrr_capable(&mut fixture);
    let crtc = fixture.events.backend.platform.outputs[0].output.crtc;
    recorder.clear_records();
    let offset = super::direct_unflip::record_count(&fixture, fd);
    recorder.script_result(Err(io::Error::from_raw_os_error(libc::EINVAL)));
    let retry_record = submit_direct_successor(&mut fixture, &mut direct, fd);
    assert!(!has_vrr_property(&retry_record, crtc));
    let records = super::direct_unflip::records_since(&fixture, fd, offset);
    let rejected = records
        .iter()
        .find(|record| contains_vrr_property(record, crtc, 1))
        .expect("first direct request includes VRR_ENABLED = 1");
    assert!(matches!(
        &rejected.result,
        crate::drm::atomic::tests_support::ScriptedResult::Error {
            raw_os_error: Some(error),
            ..
        } if *error == libc::EINVAL
    ));
    let accepted = records
        .iter()
        .find(|record| super::support::accepted_event_id(record).is_some())
        .expect("same direct frame is accepted by retry");
    assert!(!has_vrr_property(accepted, crtc));
    assert_eq!(
        records.len(),
        2,
        "one rejected request and its accepted retry"
    );
    assert!(
        fixture.events.backend.platform.outputs[0]
            .output
            .vrr_disabled
            .get()
    );
    assert!(
        !fixture.events.backend.platform.outputs[0]
            .output
            .vrr_committed
            .get(),
        "the rejected value was not committed"
    );
    let id = super::support::accepted_event_id(accepted).expect("retry event ID");
    super::direct_unflip::complete_direct_initial(&mut fixture, accepted, id, fd);
    super::direct_unflip::drain_present_notifications(
        &mut fixture,
        &[direct.candidate.present_id],
        &[first_present_id],
    );

    let second_present_id = direct.candidate.present_id;
    let later = submit_direct_successor(&mut fixture, &mut direct, fd);
    assert!(!has_vrr_property(&later, crtc));
    assert!(
        fixture
            .events
            .backend
            .pstate
            .direct
            .root()
            .pending
            .is_some()
    );
    let later_id = super::support::accepted_event_id(&later).expect("later frame event ID");
    super::direct_unflip::complete_direct_initial(&mut fixture, &later, later_id, fd);
    super::direct_unflip::drain_present_notifications(
        &mut fixture,
        &[direct.candidate.present_id],
        &[second_present_id],
    );
    finish_direct_scanout(&mut fixture, &direct, fd);
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: keep appending VRR_ENABLED = 0 after the bounded retry also gets EINVAL.
fn vrr_rejected_clear_is_bounded() {
    let Some((fixture, mut direct)) = direct_fixture(18_008, 0) else {
        eprintln!("skip: no Vulkan scene fixture available");
        return;
    };
    let mut fixture = fixture.cleanup_guard();
    set_vrr_capable(&mut fixture);
    let fd = super::direct_unflip::fd(&fixture);
    let recorder = fixture.events.recorders[&fd].clone();
    let initial = accepted_direct_record(&mut fixture, &direct, fd);
    let initial_id = super::support::accepted_event_id(&initial).expect("direct event ID");
    super::direct_unflip::complete_direct_initial(&mut fixture, &initial, initial_id, fd);
    super::direct_unflip::drain_present_notifications(
        &mut fixture,
        &[direct.candidate.present_id],
        &[],
    );
    let crtc = fixture.events.backend.platform.outputs[0].output.crtc;

    recorder.script_result(Err(io::Error::from_raw_os_error(libc::EINVAL)));
    super::direct_unflip::request_unflip(&mut fixture, &direct);
    let unflip_offset = super::direct_unflip::record_count(&fixture, fd);
    let unflip = super::direct_unflip::grouped_unflip_record(&mut fixture, fd, unflip_offset, 1);
    let unflip_attempts = super::direct_unflip::records_since(&fixture, fd, unflip_offset);
    let unflip_id = super::support::accepted_event_id(&unflip).expect("unflip retry event ID");
    let committed_after_unflip = fixture.events.backend.platform.outputs[0]
        .output
        .vrr_committed
        .get();
    super::atomic_events::complete_recorded_attempt(&mut fixture, fd, &unflip, unflip_id, 81);
    super::direct_unflip::drain_present_notifications(
        &mut fixture,
        &[],
        &[direct.candidate.present_id],
    );

    recorder.script_result(Err(io::Error::from_raw_os_error(libc::EINVAL)));
    let scene_offset = super::direct_unflip::record_count(&fixture, fd);
    let scene_attempts = super::atomic_events::submit_damaged_frame(&mut fixture);
    let scene = scene_attempts
        .iter()
        .find(|(_, record)| super::support::accepted_event_id(record).is_some())
        .expect("scene frame accepted by the no-property retry")
        .1
        .clone();
    let scene_records = super::direct_unflip::records_since(&fixture, fd, scene_offset);
    let committed_after_scene = fixture.events.backend.platform.outputs[0]
        .output
        .vrr_committed
        .get();
    let disabled_after_scene = fixture.events.backend.platform.outputs[0]
        .output
        .vrr_disabled
        .get();
    let scene_id = super::support::accepted_event_id(&scene).expect("scene retry event ID");
    super::atomic_events::complete_recorded_attempt(&mut fixture, fd, &scene, scene_id, 82);

    let mut later_candidate = direct.candidate;
    later_candidate.present_id = later_candidate.present_id.wrapping_add(2);
    let mut later_event = direct.event.clone();
    later_event.present_id = later_candidate.present_id;
    later_event.serial = later_event.serial.wrapping_add(2);
    let later_offset = super::direct_unflip::record_count(&fixture, fd);
    let mut later_admitted = false;
    for _ in 0..8 {
        later_admitted = Backend::try_present_direct(
            &mut fixture.events.backend,
            later_candidate,
            later_event.clone(),
        )
        .expect("later direct Present through Backend");
        if later_admitted {
            break;
        }
    }
    assert!(later_admitted, "later direct Present passes admission");
    let later = super::direct_unflip::records_since(&fixture, fd, later_offset)
        .into_iter()
        .find(|record| super::support::accepted_event_id(record).is_some())
        .expect("later direct frame accepted");
    let later_has_vrr = has_vrr_property(&later, crtc);
    let later_id = super::support::accepted_event_id(&later).expect("later scene event ID");
    super::direct_unflip::complete_direct_initial(&mut fixture, &later, later_id, fd);
    super::direct_unflip::drain_present_notifications(
        &mut fixture,
        &[later_candidate.present_id],
        &[],
    );
    direct.candidate = later_candidate;
    direct.event = later_event;
    finish_direct_scanout(&mut fixture, &direct, fd);
    fixture.finish_after_normal_drain();

    assert_eq!(unflip_attempts.len(), 2, "one clear request and its retry");
    assert!(contains_vrr_property(&unflip_attempts[0], crtc, 0));
    assert!(!has_vrr_property(&unflip_attempts[1], crtc));
    assert!(
        committed_after_unflip,
        "rejected zero does not change kernel state"
    );
    assert_eq!(
        scene_records.len(),
        2,
        "one bounded clear request and its retry"
    );
    assert!(contains_vrr_property(&scene_records[0], crtc, 0));
    assert!(!has_vrr_property(&scene_records[1], crtc));
    assert!(
        committed_after_scene,
        "rejected zero does not falsify committed state"
    );
    assert!(disabled_after_scene, "EINVAL latches VRR off for this CRTC");
    assert!(
        !later_has_vrr,
        "the failed bounded clear is not repeated forever"
    );
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: keep gamma first when a VRR clear is due in the same request.
fn vrr_clear_goes_before_pending_gamma() {
    let Some((fixture, direct)) = direct_fixture(18_009, 0) else {
        eprintln!("skip: no Vulkan scene fixture available");
        return;
    };
    let mut fixture = fixture.cleanup_guard();
    set_vrr_capable(&mut fixture);
    let fd = super::direct_unflip::fd(&fixture);
    let recorder = fixture.events.recorders[&fd].clone();
    recorder.set_gamma_size_for_tests(4);
    let initial = accepted_direct_record(&mut fixture, &direct, fd);
    let initial_id = super::support::accepted_event_id(&initial).expect("direct event ID");
    super::direct_unflip::complete_direct_initial(&mut fixture, &initial, initial_id, fd);
    super::direct_unflip::drain_present_notifications(
        &mut fixture,
        &[direct.candidate.present_id],
        &[],
    );
    let _ = super::support::install_controlled_clock(&mut fixture.events.backend);
    super::recording_scene::clear_vrr_test_logs();
    let output_name = "vrr-deferred-exit-test";
    fixture.events.backend.platform.outputs[0]
        .key
        .connector_name = output_name.to_owned();
    fixture.events.backend.platform.outputs[0]
        .output
        .connector_name = output_name.to_owned();

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
    Backend::set_crtc_gamma(
        &mut fixture.events.backend,
        u32::from(crtc),
        &[0x1111; 4],
        &[0x2222; 4],
        &[0x3333; 4],
    )
    .expect("queue gamma before unflip");
    let gamma_pending_before_unflip = fixture.events.backend.platform.outputs[0]
        .output
        .pending_gamma_lut
        .borrow()
        .is_some();
    let gamma_deadline_before_unflip = fixture.events.backend.platform.outputs[0]
        .output
        .pending_gamma_deadline
        .get()
        .is_some();

    super::direct_unflip::request_unflip(&mut fixture, &direct);
    let unflip_offset = super::direct_unflip::record_count(&fixture, fd);
    let unflip = super::direct_unflip::grouped_unflip_record(&mut fixture, fd, unflip_offset, 1);
    let unflip_has_gamma = unflip
        .properties
        .iter()
        .any(|update| update.object == crtc.into() && update.property == handle(70));
    let unflip_has_vrr_clear = contains_vrr_property(&unflip, crtc, 0);
    assert!(
        gamma_pending_before_unflip,
        "RandR gamma is pending before unflip"
    );
    assert!(
        gamma_deadline_before_unflip,
        "the held LUT has a refresh deadline"
    );
    assert!(unflip_has_vrr_clear, "the unflip carries VRR_ENABLED = 0");
    assert!(!unflip_has_gamma, "the clear request carries no GAMMA_LUT");
    let unflip_id = super::support::accepted_event_id(&unflip).expect("unflip event ID");
    let gamma_record_offset = recorder.records().len();
    super::atomic_events::complete_recorded_attempt(&mut fixture, fd, &unflip, unflip_id, 84);
    super::direct_unflip::drain_present_notifications(
        &mut fixture,
        &[],
        &[direct.candidate.present_id],
    );
    let committed_after_clear = fixture.events.backend.platform.outputs[0]
        .output
        .vrr_committed
        .get();
    let gamma_pending_after_clear = fixture.events.backend.platform.outputs[0]
        .output
        .pending_gamma_lut
        .borrow()
        .is_some();
    let gamma_deadline_after_clear = fixture.events.backend.platform.outputs[0]
        .output
        .pending_gamma_deadline
        .get()
        .is_some();

    let mut next_crtc_commit = recorder
        .records()
        .into_iter()
        .skip(gamma_record_offset)
        .find(|record| {
            super::support::accepted_event_id(record).is_some()
                && record.events.iter().any(|event| event.crtc == crtc)
        });
    if next_crtc_commit.is_none() {
        let scene_attempts = super::atomic_events::submit_damaged_frame(&mut fixture);
        next_crtc_commit = scene_attempts
            .into_iter()
            .map(|(_, record)| record)
            .find(|record| {
                super::support::accepted_event_id(record).is_some()
                    && record.events.iter().any(|event| event.crtc == crtc)
            });
    }
    let next_crtc_commit =
        next_crtc_commit.expect("the next CRTC commit is recorded after the clear");
    let next_crtc_id =
        super::support::accepted_event_id(&next_crtc_commit).expect("next commit event ID");
    let next_commit_has_gamma = next_crtc_commit
        .properties
        .iter()
        .any(|update| update.object == crtc.into() && update.property == handle(70));
    let next_commit_has_vrr = has_vrr_property(&next_crtc_commit, crtc);
    super::atomic_events::complete_recorded_attempt(
        &mut fixture,
        fd,
        &next_crtc_commit,
        next_crtc_id,
        85,
    );
    let gamma_pending_after_next_commit = fixture.events.backend.platform.outputs[0]
        .output
        .pending_gamma_lut
        .borrow()
        .is_some();
    let logs = super::recording_scene::vrr_test_logs();
    fixture.finish_after_normal_drain();

    assert!(
        !committed_after_clear,
        "the accepted clear updates the committed VRR state"
    );
    assert!(
        gamma_pending_after_clear,
        "the clear leaves the LUT pending"
    );
    assert!(
        gamma_deadline_after_clear,
        "the held LUT keeps its deadline"
    );
    assert!(
        next_commit_has_gamma,
        "the next commit for this CRTC carries GAMMA_LUT"
    );
    assert!(
        !next_commit_has_vrr,
        "the gamma commit does not repeat VRR_ENABLED"
    );
    assert!(
        !gamma_pending_after_next_commit,
        "the accepted gamma commit consumes the pending LUT"
    );
    assert!(
        logs.lines().any(|line| {
            line == format!(
                "kms: vrr off output={output_name} crtc={} reason=ineligible_authoritative_root_present",
                u32::from(crtc)
            )
        }),
        "scene transition log must use the retained direct exit reason; logs={logs:?}"
    );
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: append VRR_ENABLED in the same request that carries GAMMA_LUT.
fn vrr_enable_waits_behind_pending_gamma() {
    let Some((fixture, mut direct)) = direct_fixture(18_007, 0) else {
        eprintln!("skip: no Vulkan scene fixture available");
        return;
    };
    let mut fixture = fixture.cleanup_guard();
    let fd = super::direct_unflip::fd(&fixture);
    let recorder = fixture.events.recorders[&fd].clone();
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
    let first = accepted_direct_record(&mut fixture, &direct, fd);
    let first_id = super::support::accepted_event_id(&first).expect("direct event ID");
    super::direct_unflip::complete_direct_initial(&mut fixture, &first, first_id, fd);
    let first_present_id = direct.candidate.present_id;
    super::direct_unflip::drain_present_notifications(&mut fixture, &[first_present_id], &[]);
    let _ = super::support::install_controlled_clock(&mut fixture.events.backend);

    let vrr_output = &mut fixture.events.backend.platform.outputs[0].output;
    vrr_output.crtc_vrr_enabled_prop = Some(handle(VRR_PROP_ID));
    vrr_output.connector_vrr_capable = true;
    vrr_output.vrr_committed.set(false);
    let crtc_xid = u32::from(vrr_output.crtc);
    Backend::set_crtc_gamma(
        &mut fixture.events.backend,
        crtc_xid,
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
    let gamma_and_vrr_due = submit_direct_successor(&mut fixture, &mut direct, fd);
    assert!(
        gamma_and_vrr_due
            .properties
            .iter()
            .any(|update| { update.object == crtc.into() && update.property == handle(70) }),
        "the due LUT rides the first eligible direct vsync frame"
    );
    assert!(!has_vrr_property(&gamma_and_vrr_due, crtc));
    assert!(
        !fixture.events.backend.platform.outputs[0]
            .output
            .vrr_committed
            .get()
    );
    let gamma_frame_id =
        super::support::accepted_event_id(&gamma_and_vrr_due).expect("gamma frame event ID");
    super::direct_unflip::complete_direct_initial(
        &mut fixture,
        &gamma_and_vrr_due,
        gamma_frame_id,
        fd,
    );
    let gamma_present_id = direct.candidate.present_id;
    super::direct_unflip::drain_present_notifications(
        &mut fixture,
        &[gamma_present_id],
        &[first_present_id],
    );

    let vrr_frame = submit_direct_successor(&mut fixture, &mut direct, fd);
    assert!(contains_vrr_property(&vrr_frame, crtc, 1));
    assert!(
        !vrr_frame
            .properties
            .iter()
            .any(|update| { update.object == crtc.into() && update.property == handle(70) }),
        "VRR follows the gamma frame"
    );
    assert!(
        fixture.events.backend.platform.outputs[0]
            .output
            .vrr_committed
            .get()
    );
    assert!(
        fixture.events.backend.platform.outputs[0]
            .output
            .pending_gamma_lut
            .borrow()
            .is_none(),
        "the gamma frame consumes its pending LUT"
    );
    let vrr_id = super::support::accepted_event_id(&vrr_frame).expect("VRR frame event ID");
    super::direct_unflip::complete_direct_initial(&mut fixture, &vrr_frame, vrr_id, fd);
    super::direct_unflip::drain_present_notifications(
        &mut fixture,
        &[direct.candidate.present_id],
        &[gamma_present_id],
    );
    finish_direct_scanout(&mut fixture, &direct, fd);
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: omit the inherited VRR_ENABLED = 0 clear from the next scene commit.
fn vrr_inherited_on_is_cleared() {
    let Some(mut fixture) = super::recording_scene::RecordingSceneFixture::new(1)
        .expect("recording scene fixture construction")
    else {
        eprintln!("skip: no Vulkan scene fixture available");
        return;
    };
    let output = &mut fixture.events.backend.platform.outputs[0].output;
    output.crtc_vrr_enabled_prop = Some(handle(VRR_PROP_ID));
    output.connector_vrr_capable = false;
    output.vrr_committed.set(true);
    let crtc = output.crtc;
    let frame = super::atomic_events::submit_damaged_frame(&mut fixture);
    let frame = frame
        .iter()
        .find(|(_, record)| super::support::accepted_event_id(record).is_some())
        .expect("scene commit accepted")
        .1
        .clone();
    assert!(contains_vrr_property(&frame, crtc, 0));
    assert!(
        !fixture.events.backend.platform.outputs[0]
            .output
            .vrr_committed
            .get()
    );
    let id = super::support::accepted_event_id(&frame).expect("scene event ID");
    super::atomic_events::complete_recorded_attempt(&mut fixture, frame.device_fd, &frame, id, 71);
    fixture.finish_after_normal_drain();
}

struct VrrCursorHarness {
    fixture: Option<super::super::cursor_sprites::support::AtomicPathFixture>,
    direct: Option<super::recording_scene::DirectPresentSetup>,
    delivered: HashSet<super::super::cursor_sprites::support::CompletionKey>,
    device_fd: std::os::fd::RawFd,
}

impl std::ops::Deref for VrrCursorHarness {
    type Target = super::super::cursor_sprites::support::AtomicPathFixture;

    fn deref(&self) -> &Self::Target {
        self.fixture.as_ref().expect("cursor fixture is live")
    }
}

impl std::ops::DerefMut for VrrCursorHarness {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.fixture.as_mut().expect("cursor fixture is live")
    }
}

impl VrrCursorHarness {
    fn warp_pointer(&mut self, x: i32, y: i32) {
        let events = &mut self
            .fixture
            .as_mut()
            .expect("cursor fixture is live")
            .scene
            .events;
        Backend::warp_pointer_root(&mut events.backend, &mut events.state, x, y);
    }

    fn retire_cursor_move(&mut self, frame: u32) {
        let (fixture, delivered) = (&mut self.fixture, &mut self.delivered);
        super::super::cursor_sprites::support::retire_pending_output(
            &mut fixture
                .as_mut()
                .expect("cursor fixture is live")
                .scene
                .events,
            delivered,
            0,
            frame,
        );
    }

    fn inject_crtc_flip(&mut self, crtc: ::drm::control::crtc::Handle, id: u64, frame: u32) {
        let fixture = self.fixture.as_mut().expect("cursor fixture is live");
        fixture.scene.events.queue_commit_completion(
            self.device_fd,
            crtc,
            id,
            frame,
            Duration::from_millis(u64::from(frame) * 10),
        );
        Backend::on_page_flip_ready(
            &mut fixture.scene.events.backend,
            &mut fixture.scene.events.state,
            self.device_fd,
        );
    }

    fn note_direct_frame_retired(&mut self, crtc: ::drm::control::crtc::Handle, commit_id: u64) {
        self.delivered
            .insert(super::super::cursor_sprites::support::CompletionKey {
                device_fd: self.device_fd,
                crtc,
                commit_id,
            });
    }

    fn finish(mut self) {
        use super::super::cursor_sprites::support as cursor_support;

        let mut fixture = self.fixture.take().expect("cursor fixture is live");
        let Some(direct) = self.direct.take() else {
            let detach = cursor_support::accept_cursor_detach(&mut fixture, &mut self.delivered, 0);
            cursor_support::retire_cursor_record(
                &mut fixture,
                &mut self.delivered,
                0,
                &detach,
                100,
            );
            cursor_support::finish_atomic_fixture(fixture, &mut self.delivered);
            return;
        };
        cursor_support::drain_unretired_output_events(&mut fixture, &mut self.delivered, 0);
        super::direct_unflip::request_unflip(&mut fixture.scene, &direct);
        let offset = super::direct_unflip::record_count(&fixture.scene, self.device_fd);
        let unflip = super::direct_unflip::grouped_unflip_record(
            &mut fixture.scene,
            self.device_fd,
            offset,
            1,
        );
        let id = super::support::accepted_event_id(&unflip).expect("unflip event ID");
        super::atomic_events::complete_recorded_attempt(
            &mut fixture.scene,
            self.device_fd,
            &unflip,
            id,
            92,
        );
        super::direct_unflip::drain_present_notifications(
            &mut fixture.scene,
            &[],
            &[direct.candidate.present_id],
        );
        Backend::set_grab_cursor(&mut fixture.scene.events.backend, None, None)
            .expect("clear the cursor-image test override");
        Backend::define_cursor(
            &mut fixture.scene.events.backend,
            None,
            direct.target_xid,
            0x7f10,
        )
        .expect("restore the fixture cursor on the direct target");
        let detach = cursor_support::accept_cursor_detach(&mut fixture, &mut self.delivered, 0);
        cursor_support::retire_cursor_record(&mut fixture, &mut self.delivered, 0, &detach, 100);
        cursor_support::finish_atomic_fixture(fixture, &mut self.delivered);
    }
}

fn vrr_composed_cursor_harness() -> Option<VrrCursorHarness> {
    use super::super::cursor_sprites::support::{
        AtomicPathFixture, PathKind, create_argb_cursor, drain_unretired_output_events,
        establish_hardware_cursor,
    };

    let fixture = AtomicPathFixture::new(PathKind::Physical)
        .expect("VRR composed cursor scene fixture construction")?;
    let device_fd = super::direct_unflip::fd(&fixture.scene);
    let mut harness = VrrCursorHarness {
        fixture: Some(fixture),
        direct: None,
        delivered: HashSet::new(),
        device_fd,
    };
    let cursor = create_argb_cursor(
        &mut harness.scene.events.backend,
        (8, 8),
        [0xff, 0xff, 0xff, 0xff],
        (0, 0),
    );
    establish_hardware_cursor(
        harness.fixture.as_mut().expect("cursor fixture is live"),
        &mut harness.delivered,
        cursor,
        (120, 100),
    );
    drain_unretired_output_events(
        harness.fixture.as_mut().expect("cursor fixture is live"),
        &mut harness.delivered,
        0,
    );
    Some(harness)
}

impl Drop for VrrCursorHarness {
    fn drop(&mut self) {
        if let Some(fixture) = self.fixture.take() {
            fixture.scene.finish();
        }
    }
}

fn vrr_cursor_harness(present_id: u64, capable: bool) -> Option<VrrCursorHarness> {
    use super::super::cursor_sprites::support::{
        AtomicPathFixture, PathKind, create_argb_cursor, drain_unretired_output_events,
        enter_held_direct, establish_hardware_cursor,
    };

    let fixture = AtomicPathFixture::new(PathKind::Physical)
        .expect("VRR cursor scene fixture construction")?;
    let device_fd = super::direct_unflip::fd(&fixture.scene);
    let mut harness = VrrCursorHarness {
        fixture: Some(fixture),
        direct: None,
        delivered: HashSet::new(),
        device_fd,
    };
    let cursor = create_argb_cursor(
        &mut harness.scene.events.backend,
        (8, 8),
        [0xff, 0xff, 0xff, 0xff],
        (0, 0),
    );
    let hardware = establish_hardware_cursor(
        harness.fixture.as_mut().expect("cursor fixture is live"),
        &mut harness.delivered,
        cursor,
        (120, 100),
    );
    assert_eq!(hardware.device_fd, device_fd);
    drain_unretired_output_events(
        harness.fixture.as_mut().expect("cursor fixture is live"),
        &mut harness.delivered,
        0,
    );
    {
        let output = &mut harness.scene.events.backend.platform.outputs[0].output;
        output.crtc_vrr_enabled_prop = Some(handle(VRR_PROP_ID));
        output.connector_vrr_capable = capable;
        output.vrr_committed.set(false);
        output.vrr_disabled.set(false);
        output.vrr_clear_retry_used.set(false);
    }
    let direct = enter_held_direct(
        harness.fixture.as_mut().expect("cursor fixture is live"),
        &mut harness.delivered,
        present_id,
        None,
    )
    .expect("enter held direct scanout through Backend");
    assert_eq!(
        harness.scene.events.backend.platform.outputs[0]
            .output
            .vrr_committed
            .get(),
        capable,
        "direct frame establishes the requested VRR state"
    );
    super::direct_unflip::drain_present_notifications(
        &mut harness.scene,
        &[direct.candidate.present_id],
        &[],
    );
    harness.direct = Some(direct);
    Some(harness)
}

fn advance_vrr_cursor_clock(elapsed_ns: &AtomicU64, duration: Duration) {
    elapsed_ns.fetch_add(
        u64::try_from(duration.as_nanos()).expect("cursor deadline fits nanoseconds"),
        Ordering::SeqCst,
    );
}

fn vrr_cursor_records_since(harness: &VrrCursorHarness, offset: usize) -> Vec<AtomicCommitRecord> {
    harness.scene.events.recorders[&harness.device_fd]
        .records()
        .into_iter()
        .skip(offset)
        .filter(|record| record.device_fd == harness.device_fd)
        .collect()
}

fn assert_zero_vrr_cursor_deadline_summary(
    harness: &mut VrrCursorHarness,
    crtc: ::drm::control::crtc::Handle,
) {
    let summary_deadline = std::time::Instant::now() + Duration::from_secs(5);
    let summary = loop {
        Backend::before_block(&mut harness.scene.events.backend);
        if let Some(line) = super::recording_scene::vrr_test_logs()
            .lines()
            .find(|line| line.starts_with("present_latency_summary "))
        {
            break line.to_owned();
        }
        assert!(
            std::time::Instant::now() < summary_deadline,
            "VRR summary did not become due"
        );
        std::thread::yield_now();
    };
    assert!(summary.contains(&format!("vrr crtc={} ", u32::from(crtc))));
    assert!(summary.contains("cursor_deadline_commits=0"), "{summary}");
}

fn request_signatures(records: &[AtomicCommitRecord]) -> Vec<RequestSignature> {
    records
        .iter()
        .map(|record| (record.kind, record.flags, record.properties.clone()))
        .collect()
}

fn assert_record_cursor_position(
    harness: &VrrCursorHarness,
    record: &AtomicCommitRecord,
    expected: (i32, i32),
) {
    let properties = super::super::cursor_sprites::support::cursor_properties_for_output(
        &harness.scene.events.backend,
        0,
    );
    assert!(record.properties.iter().any(|update| {
        update.object == properties.plane.into()
            && update.property == properties.crtc_x
            && update.value == i64::from(expected.0) as u64
    }));
    assert!(record.properties.iter().any(|update| {
        update.object == properties.plane.into()
            && update.property == properties.crtc_y
            && update.value == i64::from(expected.1) as u64
    }));
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: send a cursor-only position commit immediately while VRR is committed on.
fn vrr_cursor_motion_rides_direct_frame() {
    let Some(mut harness) = vrr_cursor_harness(18_020, true) else {
        eprintln!("SKIP vrr_cursor_motion_rides_direct_frame: no Vulkan scene fixture available");
        return;
    };
    let (base, elapsed_ns) =
        super::support::install_controlled_clock(&mut harness.scene.events.backend);
    let offset = super::direct_unflip::record_count(&harness.scene, harness.device_fd);
    let mut direct = harness.direct.take().expect("direct scanout is active");
    harness.warp_pointer(180, 130);
    Backend::maybe_composite(&mut harness.scene.events.backend)
        .expect("hold cursor motion for the next direct frame");
    assert_eq!(
        vrr_cursor_records_since(&harness, offset)
            .iter()
            .filter(|record| record.kind == AtomicCommitKind::CursorOnly)
            .count(),
        0,
        "VRR position motion is held instead of committed immediately"
    );
    advance_vrr_cursor_clock(&elapsed_ns, Duration::from_millis(10));
    let previous_present = direct.candidate.present_id;
    let fd = harness.device_fd;
    let frame = submit_direct_successor(&mut harness.scene, &mut direct, fd);
    assert_eq!(
        vrr_cursor_records_since(&harness, offset)
            .iter()
            .filter(|record| record.kind == AtomicCommitKind::CursorOnly)
            .count(),
        0,
        "direct frame inside the deadline prevents every standalone cursor commit"
    );
    assert_record_cursor_position(&harness, &frame, (180, 130));
    let frame_id = super::support::accepted_event_id(&frame).expect("direct frame ID");
    let crtc = harness.scene.events.backend.platform.outputs[0].output.crtc;
    super::direct_unflip::complete_direct_initial(&mut harness.scene, &frame, frame_id, fd);
    harness.note_direct_frame_retired(crtc, frame_id);
    super::direct_unflip::drain_present_notifications(
        &mut harness.scene,
        &[direct.candidate.present_id],
        &[previous_present],
    );
    harness.direct = Some(direct);
    assert!(
        Backend::next_wakeup(&harness.scene.events.backend)
            .is_none_or(|wakeup| wakeup > base + Duration::from_millis(33)),
        "the direct frame consumes the pending cursor deadline"
    );
    harness.finish();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: omit the cursor deadline wake or submit before the deadline expires.
fn vrr_cursor_motion_without_frame_commits_once_at_deadline() {
    let Some(mut harness) = vrr_cursor_harness(18_021, true) else {
        eprintln!(
            "SKIP vrr_cursor_motion_without_frame_commits_once_at_deadline: no Vulkan scene fixture available"
        );
        return;
    };
    let (base, elapsed_ns) =
        super::support::install_controlled_clock(&mut harness.scene.events.backend);
    let offset = super::direct_unflip::record_count(&harness.scene, harness.device_fd);
    harness.warp_pointer(180, 130);
    Backend::maybe_composite(&mut harness.scene.events.backend)
        .expect("record pending cursor motion");
    assert_eq!(
        Backend::next_wakeup(&harness.scene.events.backend),
        Some(base + Duration::from_millis(33)),
        "next_wakeup exposes the 33 ms cursor deadline"
    );
    advance_vrr_cursor_clock(&elapsed_ns, Duration::from_millis(32));
    Backend::maybe_composite(&mut harness.scene.events.backend)
        .expect("do not send cursor motion before deadline");
    assert!(
        vrr_cursor_records_since(&harness, offset)
            .iter()
            .all(|record| record.kind != AtomicCommitKind::CursorOnly),
        "no cursor-only request is submitted before the deadline"
    );
    advance_vrr_cursor_clock(&elapsed_ns, Duration::from_millis(1));
    Backend::maybe_composite(&mut harness.scene.events.backend)
        .expect("send cursor motion when deadline expires");
    Backend::maybe_composite(&mut harness.scene.events.backend)
        .expect("in-flight cursor move is not resubmitted");
    let attempts: Vec<_> = vrr_cursor_records_since(&harness, offset)
        .into_iter()
        .filter(|record| record.kind == AtomicCommitKind::CursorOnly)
        .collect();
    assert_eq!(
        attempts.len(),
        1,
        "one cursor-only request is sent at deadline"
    );
    assert_eq!(attempts[0].result, ScriptedResult::Ok);
    assert_record_cursor_position(&harness, &attempts[0], (180, 130));
    harness.retire_cursor_move(93);
    harness.finish();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: replace a pending move's original deadline when a later position arrives.
fn vrr_cursor_later_motion_keeps_deadline() {
    let Some(mut harness) = vrr_cursor_harness(18_022, true) else {
        eprintln!("SKIP vrr_cursor_later_motion_keeps_deadline: no Vulkan scene fixture available");
        return;
    };
    let (base, elapsed_ns) =
        super::support::install_controlled_clock(&mut harness.scene.events.backend);
    let offset = super::direct_unflip::record_count(&harness.scene, harness.device_fd);
    harness.warp_pointer(180, 130);
    Backend::maybe_composite(&mut harness.scene.events.backend)
        .expect("start the first cursor deadline");
    advance_vrr_cursor_clock(&elapsed_ns, Duration::from_millis(20));
    harness.warp_pointer(190, 140);
    Backend::maybe_composite(&mut harness.scene.events.backend)
        .expect("later motion replaces the held position");
    assert_eq!(
        Backend::next_wakeup(&harness.scene.events.backend),
        Some(base + Duration::from_millis(33)),
        "later motion keeps the first position's deadline"
    );
    advance_vrr_cursor_clock(&elapsed_ns, Duration::from_millis(13));
    Backend::maybe_composite(&mut harness.scene.events.backend)
        .expect("latest position sends at the original deadline");
    let attempt = vrr_cursor_records_since(&harness, offset)
        .into_iter()
        .find(|record| record.kind == AtomicCommitKind::CursorOnly)
        .expect("deadline sends one cursor-only request");
    assert_eq!(attempt.result, ScriptedResult::Ok);
    assert_record_cursor_position(&harness, &attempt, (190, 140));
    harness.retire_cursor_move(94);
    harness.finish();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: key the cursor hold on vrr_committed alone after direct scanout has ended.
fn vrr_cursor_not_held_after_rejected_clear() {
    let Some(mut harness) = vrr_cursor_harness(18_052, true) else {
        eprintln!(
            "SKIP vrr_cursor_not_held_after_rejected_clear: no Vulkan scene fixture available"
        );
        return;
    };
    let direct = harness.direct.take().expect("direct scanout is active");
    let device_fd = harness.device_fd;
    let recorder = Rc::clone(&harness.scene.events.recorders[&device_fd]);
    let crtc = harness.scene.events.backend.platform.outputs[0].output.crtc;
    let mut state = present_request_state(&direct);
    let property = state.atoms.intern("_VARIABLE_REFRESH", false).0;
    request_vrr_property(
        &mut state,
        &mut harness.scene.events.backend,
        direct.target_xid,
        property,
        yserver_core::xinput::XA_CARDINAL.0,
        32,
        &[1],
        1,
    );
    let clear_offset = super::direct_unflip::record_count(&harness.scene, device_fd);
    recorder.script_result(Err(io::Error::from_raw_os_error(libc::EINVAL)));
    delete_vrr_property(
        &mut state,
        &mut harness.scene.events.backend,
        direct.target_xid,
        property,
        2,
    );
    Backend::maybe_composite(&mut harness.scene.events.backend)
        .expect("try the direct owner's property-deleted clear");
    let standalone_clear =
        super::direct_unflip::records_since(&harness.scene, device_fd, clear_offset);
    assert_eq!(
        standalone_clear
            .iter()
            .filter(|record| {
                contains_vrr_property(record, crtc, 0)
                    && matches!(
                        record.result,
                        ScriptedResult::Error {
                            raw_os_error: Some(error),
                            ..
                        } if error == libc::EINVAL
                    )
            })
            .count(),
        1,
        "the direct owner's first clear is rejected once: {standalone_clear:?}"
    );

    recorder.script_result(Err(io::Error::from_raw_os_error(libc::EINVAL)));
    super::direct_unflip::request_unflip(&mut harness.scene, &direct);
    let unflip_offset = super::direct_unflip::record_count(&harness.scene, device_fd);
    let unflip = super::direct_unflip::grouped_unflip_record(
        &mut harness.scene,
        device_fd,
        unflip_offset,
        1,
    );
    let unflip_attempts =
        super::direct_unflip::records_since(&harness.scene, device_fd, unflip_offset);
    let rejected_clears = unflip_attempts
        .iter()
        .filter(|record| {
            contains_vrr_property(record, crtc, 0)
                && matches!(
                    record.result,
                    ScriptedResult::Error {
                        raw_os_error: Some(error),
                        ..
                    } if error == libc::EINVAL
                )
        })
        .count();
    assert_eq!(
        rejected_clears, 1,
        "the owner-leave clear is rejected once: {unflip_attempts:?}"
    );
    assert!(
        unflip_attempts
            .iter()
            .any(|record| !has_vrr_property(record, crtc) && record.result == ScriptedResult::Ok)
    );
    let unflip_id = super::support::accepted_event_id(&unflip).expect("accepted unflip ID");
    super::atomic_events::complete_recorded_attempt(
        &mut harness.scene,
        device_fd,
        &unflip,
        unflip_id,
        101,
    );
    harness
        .delivered
        .insert(super::super::cursor_sprites::support::CompletionKey {
            device_fd,
            crtc,
            commit_id: unflip_id,
        });
    super::direct_unflip::drain_present_notifications(
        &mut harness.scene,
        &[],
        &[direct.candidate.present_id],
    );

    assert!(
        harness
            .scene
            .events
            .backend
            .pstate
            .direct
            .root()
            .current
            .is_none()
    );
    let output = &harness.scene.events.backend.platform.outputs[0].output;
    assert!(
        output.vrr_committed.get(),
        "both rejected clears preserve kernel state"
    );
    assert!(
        output.vrr_disabled.get(),
        "the first EINVAL latches VRR off"
    );
    assert!(
        output.vrr_clear_retry_used.get(),
        "the second EINVAL bounds future clears"
    );
    harness.direct = None;

    let root = harness.scene.events.backend.core.window_id;
    let width = harness.scene.events.backend.platform.fb_w;
    let height = harness.scene.events.backend.platform.fb_h;
    let mut settled_scene = None;
    for _ in 0..8 {
        let settle_offset = super::direct_unflip::record_count(&harness.scene, device_fd);
        Backend::clear_area(
            &mut harness.scene.events.backend,
            None,
            root,
            0,
            None,
            0,
            0,
            width,
            height,
            (0, 0),
        )
        .expect("damage composed desktop through Backend");
        Backend::maybe_composite(&mut harness.scene.events.backend)
            .expect("compose the desktop after direct unflip");
        settled_scene =
            super::direct_unflip::records_since(&harness.scene, device_fd, settle_offset)
                .into_iter()
                .find(|record| {
                    record.events.iter().any(|event| event.crtc == crtc)
                        && matches!(
                            record.kind,
                            AtomicCommitKind::Scene | AtomicCommitKind::SceneWithCursor
                        )
                        && super::support::accepted_event_id(record).is_some()
                });
        if settled_scene.is_some() {
            break;
        }
        harness.scene.events.wait_for_submitted_gpu_work();
        let events = &mut harness.scene.events;
        Backend::on_scanout_render_completion(&mut events.backend, &mut events.state);
    }
    let settled_scene = settled_scene.expect("composed desktop accepts its settling scene frame");
    assert!(
        matches!(
            settled_scene.kind,
            AtomicCommitKind::Scene | AtomicCommitKind::SceneWithCursor
        ),
        "the desktop settles with a composed scene request"
    );
    let settled_scene_id =
        super::support::accepted_event_id(&settled_scene).expect("settling scene event ID");
    super::atomic_events::complete_recorded_attempt(
        &mut harness.scene,
        device_fd,
        &settled_scene,
        settled_scene_id,
        102,
    );
    harness
        .delivered
        .insert(super::super::cursor_sprites::support::CompletionKey {
            device_fd,
            crtc,
            commit_id: settled_scene_id,
        });

    super::recording_scene::clear_vrr_test_logs();
    let (base, _) = super::support::install_controlled_clock(&mut harness.scene.events.backend);
    let cursor_offset = super::direct_unflip::record_count(&harness.scene, harness.device_fd);
    harness.warp_pointer(180, 130);
    Backend::maybe_composite(&mut harness.scene.events.backend)
        .expect("submit composed-desktop cursor motion immediately");
    let cursor_records = vrr_cursor_records_since(&harness, cursor_offset);
    let cursor_commits: Vec<_> = cursor_records
        .iter()
        .filter(|record| record.kind == AtomicCommitKind::CursorOnly)
        .collect();
    assert_eq!(
        cursor_commits.len(),
        1,
        "composed desktop sends one cursor-only commit"
    );
    assert_eq!(cursor_commits[0].result, ScriptedResult::Ok);
    assert_record_cursor_position(&harness, cursor_commits[0], (180, 130));
    assert!(
        Backend::next_wakeup(&harness.scene.events.backend).is_none_or(|wakeup| wakeup <= base),
        "the composed cursor has no 33 ms deadline"
    );
    harness.retire_cursor_move(103);
    assert_zero_vrr_cursor_deadline_summary(&mut harness, crtc);
    harness.finish();
    drop(direct);
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: key the cursor hold on inherited vrr_committed before any direct frame exists.
fn vrr_cursor_not_held_with_inherited_vrr() {
    let Some(mut harness) = vrr_composed_cursor_harness() else {
        eprintln!("SKIP vrr_cursor_not_held_with_inherited_vrr: no Vulkan scene fixture available");
        return;
    };
    let output = &mut harness.scene.events.backend.platform.outputs[0].output;
    output.crtc_vrr_enabled_prop = Some(handle(VRR_PROP_ID));
    output.connector_vrr_capable = true;
    output.vrr_committed.set(true);
    harness.scene.events.backend.pstate.vrr_mode = crate::launch::VrrMode::Off;
    assert!(
        harness
            .scene
            .events
            .backend
            .pstate
            .direct
            .root()
            .current
            .is_none()
    );

    super::recording_scene::clear_vrr_test_logs();
    let (base, _) = super::support::install_controlled_clock(&mut harness.scene.events.backend);
    let cursor_offset = super::direct_unflip::record_count(&harness.scene, harness.device_fd);
    harness.warp_pointer(180, 130);
    Backend::maybe_composite(&mut harness.scene.events.backend)
        .expect("submit cursor motion on composed startup desktop");
    let cursor_records = vrr_cursor_records_since(&harness, cursor_offset);
    let cursor_commits: Vec<_> = cursor_records
        .iter()
        .filter(|record| record.kind == AtomicCommitKind::CursorOnly)
        .collect();
    assert_eq!(
        cursor_commits.len(),
        1,
        "inherited VRR does not delay cursor motion"
    );
    assert_eq!(cursor_commits[0].result, ScriptedResult::Ok);
    assert_record_cursor_position(&harness, cursor_commits[0], (180, 130));
    assert!(
        Backend::next_wakeup(&harness.scene.events.backend).is_none_or(|wakeup| wakeup <= base),
        "startup cursor has no 33 ms deadline"
    );
    assert!(
        harness.scene.events.backend.platform.outputs[0]
            .output
            .vrr_committed
            .get(),
        "no composed commit has retired to clear the inherited value"
    );
    harness.retire_cursor_move(104);

    let crtc = harness.scene.events.backend.platform.outputs[0].output.crtc;
    assert_zero_vrr_cursor_deadline_summary(&mut harness, crtc);
    harness.finish();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: leave an expired deadline attached after a direct frame carried its move.
fn vrr_cursor_new_move_after_carried_move_gets_fresh_deadline() {
    let Some(mut harness) = vrr_cursor_harness(18_023, true) else {
        eprintln!(
            "SKIP vrr_cursor_new_move_after_carried_move_gets_fresh_deadline: no Vulkan scene fixture available"
        );
        return;
    };
    let (base, elapsed_ns) =
        super::support::install_controlled_clock(&mut harness.scene.events.backend);
    let offset = super::direct_unflip::record_count(&harness.scene, harness.device_fd);
    let mut direct = harness.direct.take().expect("direct scanout is active");
    harness.warp_pointer(180, 130);
    Backend::maybe_composite(&mut harness.scene.events.backend)
        .expect("hold first cursor position");
    advance_vrr_cursor_clock(&elapsed_ns, Duration::from_millis(10));
    let previous_present = direct.candidate.present_id;
    let fd = harness.device_fd;
    let frame = submit_direct_successor(&mut harness.scene, &mut direct, fd);
    assert_record_cursor_position(&harness, &frame, (180, 130));
    let frame_id = super::support::accepted_event_id(&frame).expect("direct frame ID");
    let crtc = harness.scene.events.backend.platform.outputs[0].output.crtc;
    super::direct_unflip::complete_direct_initial(&mut harness.scene, &frame, frame_id, fd);
    harness.note_direct_frame_retired(crtc, frame_id);
    super::direct_unflip::drain_present_notifications(
        &mut harness.scene,
        &[direct.candidate.present_id],
        &[previous_present],
    );
    advance_vrr_cursor_clock(&elapsed_ns, Duration::from_millis(40));
    harness.warp_pointer(220, 160);
    Backend::maybe_composite(&mut harness.scene.events.backend)
        .expect("a post-frame move starts a fresh deadline");
    assert!(
        vrr_cursor_records_since(&harness, offset)
            .iter()
            .all(|record| record.kind != AtomicCommitKind::CursorOnly),
        "the new move does not inherit the carried move's expired deadline"
    );
    assert_eq!(
        Backend::next_wakeup(&harness.scene.events.backend),
        Some(base + Duration::from_millis(83)),
        "new movement starts a fresh 33 ms deadline at +50 ms"
    );
    advance_vrr_cursor_clock(&elapsed_ns, Duration::from_millis(32));
    Backend::maybe_composite(&mut harness.scene.events.backend)
        .expect("new move remains held before its deadline");
    assert!(
        vrr_cursor_records_since(&harness, offset)
            .iter()
            .all(|record| record.kind != AtomicCommitKind::CursorOnly)
    );
    advance_vrr_cursor_clock(&elapsed_ns, Duration::from_millis(1));
    Backend::maybe_composite(&mut harness.scene.events.backend)
        .expect("new move sends at its fresh deadline");
    let attempt = vrr_cursor_records_since(&harness, offset)
        .into_iter()
        .find(|record| record.kind == AtomicCommitKind::CursorOnly)
        .expect("fresh deadline sends the pending move");
    assert_record_cursor_position(&harness, &attempt, (220, 160));
    harness.retire_cursor_move(95);
    harness.direct = Some(direct);
    harness.finish();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: compute vrr_cursor_deadline across moves blocked by cursor_retry_waits_for_flip.
fn vrr_cursor_deadline_ebusy_waits_for_flip() {
    let Some(mut harness) = vrr_cursor_harness(18_024, true) else {
        eprintln!(
            "SKIP vrr_cursor_deadline_ebusy_waits_for_flip: no Vulkan scene fixture available"
        );
        return;
    };
    let (base, elapsed_ns) =
        super::support::install_controlled_clock(&mut harness.scene.events.backend);
    let recorder = Rc::clone(&harness.scene.events.recorders[&harness.device_fd]);
    let offset = super::direct_unflip::record_count(&harness.scene, harness.device_fd);
    let crtc = harness.scene.events.backend.platform.outputs[0].output.crtc;
    harness.warp_pointer(180, 130);
    Backend::maybe_composite(&mut harness.scene.events.backend)
        .expect("start cursor move deadline");
    recorder.script_result(Err(io::Error::from_raw_os_error(libc::EBUSY)));
    advance_vrr_cursor_clock(&elapsed_ns, Duration::from_millis(33));
    Backend::maybe_composite(&mut harness.scene.events.backend)
        .expect("record cursor-only EBUSY for retry-after-flip");
    let first = vrr_cursor_records_since(&harness, offset)
        .into_iter()
        .find(|record| record.kind == AtomicCommitKind::CursorOnly)
        .expect("deadline cursor request reaches the recorder");
    assert!(matches!(
        first.result,
        ScriptedResult::Error {
            raw_os_error: Some(error),
            ..
        } if error == libc::EBUSY
    ));
    assert!(
        harness
            .scene
            .events
            .backend
            .platform
            .cursor_retry_waits_for_flip(0)
    );
    let wake = Backend::next_wakeup(&harness.scene.events.backend);
    assert!(
        wake.is_none_or(|wakeup| wakeup > base + Duration::from_millis(33)),
        "an EBUSY-held move does not expose an immediate retry wake"
    );
    harness.inject_crtc_flip(crtc, 0, 96);
    assert!(
        !harness
            .scene
            .events
            .backend
            .platform
            .cursor_retry_waits_for_flip(0)
    );
    assert!(
        Backend::next_wakeup(&harness.scene.events.backend)
            .is_some_and(|wakeup| wakeup <= base + Duration::from_millis(33))
    );
    Backend::maybe_composite(&mut harness.scene.events.backend)
        .expect("the matching flip makes the expired move runnable");
    let attempts: Vec<_> = vrr_cursor_records_since(&harness, offset)
        .into_iter()
        .filter(|record| record.kind == AtomicCommitKind::CursorOnly)
        .collect();
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[1].result, ScriptedResult::Ok);
    assert_record_cursor_position(&harness, &attempts[1], (180, 130));
    harness.retire_cursor_move(97);
    harness.finish();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: hold cursor image changes behind the VRR position deadline.
fn vrr_cursor_image_change_commits_immediately() {
    use super::super::cursor_sprites::support::create_argb_cursor;

    let Some(mut harness) = vrr_cursor_harness(18_025, true) else {
        eprintln!(
            "SKIP vrr_cursor_image_change_commits_immediately: no Vulkan scene fixture available"
        );
        return;
    };
    let _ = super::support::install_controlled_clock(&mut harness.scene.events.backend);
    let position_offset = super::direct_unflip::record_count(&harness.scene, harness.device_fd);
    harness.warp_pointer(180, 130);
    Backend::maybe_composite(&mut harness.scene.events.backend)
        .expect("hold the changed position while the image update is prepared");
    assert!(
        vrr_cursor_records_since(&harness, position_offset)
            .iter()
            .all(|record| record.kind != AtomicCommitKind::CursorOnly),
        "position motion remains held while VRR is on"
    );
    let offset = super::direct_unflip::record_count(&harness.scene, harness.device_fd);
    let new_cursor = create_argb_cursor(
        &mut harness.scene.events.backend,
        (12, 12),
        [0x31, 0x72, 0xb4, 0xff],
        (0, 0),
    );
    let prior_framebuffer = super::super::cursor_sprites::support::applied_cursor_binding(
        &harness.scene.events.backend,
        0,
    )
    .expect("visible cursor binding before shape change")
    .framebuffer;
    Backend::set_grab_cursor(
        &mut harness.scene.events.backend,
        None,
        Some(new_cursor.as_raw()),
    )
    .expect("change cursor image through the grab-cursor Backend path");
    Backend::maybe_composite(&mut harness.scene.events.backend)
        .expect("submit cursor image change through Backend");
    let attempt = vrr_cursor_records_since(&harness, offset)
        .into_iter()
        .find(|record| record.kind == AtomicCommitKind::CursorOnly)
        .expect("cursor image change retains the immediate cursor-only path");
    assert_eq!(attempt.result, ScriptedResult::Ok);
    let properties = super::super::cursor_sprites::support::cursor_properties_for_output(
        &harness.scene.events.backend,
        0,
    );
    let image_framebuffer =
        super::super::cursor_sprites::support::record_property(&attempt, properties.fb_id)
            .expect("image replacement writes FB_ID");
    assert_ne!(image_framebuffer, u64::from(prior_framebuffer));
    harness.retire_cursor_move(98);
    harness.finish();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: change a VRR-off request or insert another request into its recorded stream.
fn vrr_off_cursor_only_unchanged() {
    let Some(mut harness) = vrr_cursor_harness(18_026, false) else {
        eprintln!("SKIP vrr_off_cursor_only_unchanged: no Vulkan scene fixture available");
        return;
    };
    let (base, _) = super::support::install_controlled_clock(&mut harness.scene.events.backend);
    let offset = super::direct_unflip::record_count(&harness.scene, harness.device_fd);
    harness.warp_pointer(180, 130);
    Backend::maybe_composite(&mut harness.scene.events.backend)
        .expect("submit ordinary cursor motion with VRR off");
    let records = vrr_cursor_records_since(&harness, offset);
    let attempts: Vec<_> = records
        .iter()
        .filter(|record| record.kind == AtomicCommitKind::CursorOnly)
        .cloned()
        .collect();
    assert_eq!(
        attempts.len(),
        1,
        "VRR-off motion keeps its immediate request"
    );
    assert_eq!(attempts[0].result, ScriptedResult::Ok);
    assert_record_cursor_position(&harness, &attempts[0], (180, 130));
    assert!(
        Backend::next_wakeup(&harness.scene.events.backend).is_none_or(|wakeup| wakeup <= base),
        "VRR-off motion does not add a 33 ms deadline"
    );
    assert_eq!(records.len(), 1, "motion records one complete request");

    let Some(mut baseline) = vrr_cursor_harness(18_027, false) else {
        eprintln!("SKIP vrr_off_cursor_only_unchanged: no Vulkan baseline fixture available");
        return;
    };
    baseline.scene.events.backend.platform.outputs[0]
        .output
        .crtc_vrr_enabled_prop = None;
    let baseline_offset = super::direct_unflip::record_count(&baseline.scene, baseline.device_fd);
    baseline.warp_pointer(180, 130);
    Backend::maybe_composite(&mut baseline.scene.events.backend)
        .expect("submit baseline cursor motion without a VRR property");
    let baseline_records = vrr_cursor_records_since(&baseline, baseline_offset);
    assert_eq!(
        request_signatures(&records),
        request_signatures(&baseline_records),
        "VRR-off motion preserves the complete atomic request stream without VRR support"
    );
    harness.retire_cursor_move(99);
    harness.finish();
    baseline.finish();
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

        for (frame, ust_step_us, received_step_ms) in [
            (10_u32, 0_u64, 0_u64),
            (20, 10_000, 10),
            (30, 5_000, 20),
            (40, 20_000, 30),
        ] {
            let previous_present = direct.candidate.present_id;
            let record = submit_direct_successor(&mut harness.scene, &mut direct, device_fd);
            assert_eq!(record.events.len(), 1, "direct frame targets the VRR CRTC");
            let commit_id = super::support::accepted_event_id(&record)
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
            super::direct_unflip::drain_present_notifications(
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
    super::recording_scene::clear_vrr_test_logs();

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
        let commit_id = super::support::accepted_event_id(&record)
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
        super::direct_unflip::drain_present_notifications(
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

    let (base, elapsed_ns) =
        super::support::install_controlled_clock(&mut harness.scene.events.backend);
    advance_vrr_cursor_clock(&elapsed_ns, Duration::from_millis(45));
    let cursor_offset = super::direct_unflip::record_count(&harness.scene, harness.device_fd);
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
        super::support::accepted_event_id(&deadline_commits[0]).is_some(),
        "the deadline cursor request was accepted"
    );

    let summary_deadline = std::time::Instant::now() + Duration::from_secs(5);
    let vrr_summary = loop {
        Backend::before_block(&mut harness.scene.events.backend);
        if let Some(line) = super::recording_scene::vrr_test_logs()
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

    let Some((off_fixture, off_direct)) = super::direct_unflip::direct_fixture(
        1,
        super::recording_scene::P1SceneRoute::Shared,
        18_031,
        0,
    ) else {
        eprintln!(
            "SKIP vrr_summary_reports_intervals_and_deadline_commits: no VRR-off Vulkan fixture available"
        );
        return;
    };
    let mut off_fixture = off_fixture.cleanup_guard();
    super::recording_scene::clear_vrr_test_logs();
    let off_fd = super::direct_unflip::fd(&off_fixture);
    let off_record = accepted_direct_record(&mut off_fixture, &off_direct, off_fd);
    let off_id = super::support::accepted_event_id(&off_record)
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
    super::direct_unflip::drain_present_notifications(
        &mut off_fixture,
        &[off_direct.candidate.present_id],
        &[],
    );
    let off_summary_deadline = std::time::Instant::now() + Duration::from_secs(5);
    let off_summary = loop {
        Backend::before_block(&mut off_fixture.events.backend);
        if let Some(line) = super::recording_scene::vrr_test_logs()
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
// Mutations: compare the RANDR candidate CRTC XID to output.crtc, or report inherited instead of property.
fn vrr_property_deleted_clears_without_frame_distinct_crtc_ids() {
    let Some((mut fixture, mut direct)) =
        direct_fixture_with_vrr_mode(18_040, 0, crate::launch::VrrMode::Property)
    else {
        eprintln!(
            "SKIP vrr_property_deleted_clears_without_frame_distinct_crtc_ids: no Vulkan scene fixture available"
        );
        return;
    };
    set_vrr_capable(&mut fixture);
    let mut fixture = fixture.cleanup_guard();
    let fd = super::direct_unflip::fd(&fixture);
    let output_idx = 0;
    let crtc = fixture.events.backend.platform.outputs[output_idx]
        .output
        .crtc;
    let randr_crtc_xid = direct.candidate.crtc_id.wrapping_add(0x10000);
    assert_ne!(randr_crtc_xid, u32::from(crtc));
    let output_key = fixture.events.backend.platform.outputs[output_idx]
        .key
        .clone();
    fixture
        .events
        .backend
        .pstate
        .crtc_key_by_id
        .insert(randr_crtc_xid, output_key);
    fixture.events.backend.refresh_present_crtc_clock_epochs();
    direct.candidate.crtc_id = randr_crtc_xid;
    direct.candidate.crtc_epoch = fixture
        .events
        .backend
        .present_crtc_clock_epoch(randr_crtc_xid);
    assert_ne!(direct.candidate.crtc_id, u32::from(crtc));
    let output_name = fixture.events.backend.platform.outputs[0]
        .key
        .connector_name
        .clone();
    let (mut state, property, initial) = start_property_direct(&mut fixture, &direct, true);
    assert!(contains_vrr_property(&initial, crtc, 1));
    let fb_id_property = fixture.events.backend.platform.outputs[output_idx]
        .output
        .plane_fb_id_prop;
    let direct_framebuffer = initial
        .properties
        .iter()
        .find(|update| update.property == fb_id_property)
        .map(|update| update.value)
        .expect("accepted direct frame sets the primary FB_ID");
    complete_direct_and_drain_present(&mut fixture, &initial, 1, None);
    assert!(
        fixture
            .events
            .backend
            .pstate
            .direct
            .root()
            .pending
            .is_none()
    );
    assert!(
        fixture
            .events
            .backend
            .pstate
            .direct
            .root()
            .queued_successor
            .is_none()
    );
    assert!(
        !fixture
            .events
            .backend
            .platform
            .scanout_commit_in_flight_for_output(output_idx),
        "the initial direct frame is retired before deleting the property"
    );
    let recorder = fixture.events.recorders[&fd].clone();
    let offset = super::direct_unflip::record_count(&fixture, fd);
    super::recording_scene::clear_vrr_test_logs();

    delete_vrr_property(
        &mut state,
        &mut fixture.events.backend,
        direct.target_xid,
        property,
        3,
    );
    Backend::maybe_composite(&mut fixture.events.backend)
        .expect("idle property clear scheduling pass");
    let clear = super::direct_unflip::records_since(&fixture, fd, offset)
        .into_iter()
        .find(|record| super::support::accepted_event_id(record).is_some())
        .expect("idle deletion submits one standalone clear");
    assert_eq!(clear.kind, AtomicCommitKind::VrrOnly);
    assert!(contains_vrr_property(&clear, crtc, 0));
    assert_eq!(
        clear
            .properties
            .iter()
            .filter(|update| update.property == fb_id_property)
            .map(|update| update.value)
            .collect::<Vec<_>>(),
        vec![direct_framebuffer],
        "standalone clear carries the current primary framebuffer"
    );
    assert_eq!(clear.properties.len(), 2);
    assert!(
        !fixture.events.backend.platform.outputs[0]
            .output
            .vrr_committed
            .get()
    );
    assert_eq!(
        recorder
            .records()
            .iter()
            .skip(offset)
            .filter(|record| super::support::accepted_event_id(record).is_some())
            .count(),
        1,
        "one accepted standalone clear is enough"
    );
    let logs = super::recording_scene::vrr_test_logs();
    assert!(
        logs.lines().any(|line| {
            line == format!(
                "kms: vrr off output={output_name} crtc={} reason=property",
                u32::from(crtc)
            )
        }),
        "the immediate standalone clear logs its accepted VRR transition; logs={logs:?}"
    );
    let clear_id = super::support::accepted_event_id(&clear).expect("clear event ID");
    super::atomic_events::complete_recorded_attempt(&mut fixture, fd, &clear, clear_id, 84);

    let later = submit_property_present(&mut fixture, &mut state, &direct, 18_041, 4)
        .into_iter()
        .find(|record| super::support::accepted_event_id(record).is_some())
        .expect("later Present reaches direct scanout");
    assert!(
        !has_vrr_property(&later, crtc),
        "a fresh Present without the property does not write VRR_ENABLED"
    );
    complete_direct_and_drain_present(&mut fixture, &later, 2, Some(1));
    finish_property_direct_scanout(&mut fixture, fd);
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutations: retry before flip retirement, omit the event retry, or omit its accepted-clear log.
fn vrr_property_deleted_during_flip_clears_at_event() {
    let Some((mut fixture, direct)) =
        direct_fixture_with_vrr_mode(18_042, 0, crate::launch::VrrMode::Property)
    else {
        eprintln!(
            "SKIP vrr_property_deleted_during_flip_clears_at_event: no Vulkan scene fixture available"
        );
        return;
    };
    set_vrr_capable(&mut fixture);
    let mut fixture = fixture.cleanup_guard();
    let fd = super::direct_unflip::fd(&fixture);
    let crtc = fixture.events.backend.platform.outputs[0].output.crtc;
    let output_name = fixture.events.backend.platform.outputs[0]
        .key
        .connector_name
        .clone();
    let (mut state, property, initial) = start_property_direct(&mut fixture, &direct, true);
    complete_direct_and_drain_present(&mut fixture, &initial, 1, None);
    let pending = submit_property_present(&mut fixture, &mut state, &direct, 18_043, 3)
        .into_iter()
        .find(|record| super::support::accepted_event_id(record).is_some())
        .expect("direct successor flip is in flight");
    let fb_id_property = fixture.events.backend.platform.outputs[0]
        .output
        .plane_fb_id_prop;
    let current_direct_framebuffer = pending
        .properties
        .iter()
        .find(|update| update.property == fb_id_property)
        .map(|update| update.value)
        .expect("accepted direct successor sets the primary FB_ID");
    let offset = super::direct_unflip::record_count(&fixture, fd);
    super::recording_scene::clear_vrr_test_logs();

    delete_vrr_property(
        &mut state,
        &mut fixture.events.backend,
        direct.target_xid,
        property,
        4,
    );
    assert!(
        super::direct_unflip::records_since(&fixture, fd, offset)
            .iter()
            .all(|record| !contains_vrr_property(record, crtc, 0)),
        "the clear waits while the CRTC flip is in flight"
    );
    assert!(
        fixture.events.backend.platform.outputs[0]
            .output
            .vrr_committed
            .get()
    );

    super::atomic_events::complete_recorded_attempt(
        &mut fixture,
        fd,
        &pending,
        super::support::accepted_event_id(&pending).expect("pending event ID"),
        85,
    );
    super::direct_unflip::drain_present_notifications(&mut fixture, &[2], &[1]);
    let clear = super::direct_unflip::records_since(&fixture, fd, offset)
        .into_iter()
        .find(|record| super::support::accepted_event_id(record).is_some())
        .expect("flip event releases the standalone clear");
    assert_eq!(clear.kind, AtomicCommitKind::VrrOnly);
    assert!(contains_vrr_property(&clear, crtc, 0));
    assert_eq!(
        clear
            .properties
            .iter()
            .filter(|update| update.property == fb_id_property)
            .map(|update| update.value)
            .collect::<Vec<_>>(),
        vec![current_direct_framebuffer],
        "event-driven clear carries the current direct framebuffer"
    );
    assert_eq!(clear.properties.len(), 2);
    assert!(
        !fixture.events.backend.platform.outputs[0]
            .output
            .vrr_committed
            .get()
    );
    let logs = super::recording_scene::vrr_test_logs();
    assert!(
        logs.lines().any(|line| {
            line == format!(
                "kms: vrr off output={output_name} crtc={} reason=property",
                u32::from(crtc)
            )
        }),
        "the event-driven standalone clear logs its accepted VRR transition; logs={logs:?}"
    );
    let clear_id = super::support::accepted_event_id(&clear).expect("clear event ID");
    super::atomic_events::complete_recorded_attempt(&mut fixture, fd, &clear, clear_id, 86);

    finish_property_direct_scanout(&mut fixture, fd);
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: keep the direct-frame clear fallback as inherited; queued property deletion must log property.
fn vrr_queued_property_delete_clear_logs_property_reason() {
    let Some((mut fixture, mut direct)) =
        direct_fixture_with_vrr_mode(18_046, 0, crate::launch::VrrMode::Property)
    else {
        eprintln!(
            "SKIP vrr_queued_property_delete_clear_logs_property_reason: no Vulkan scene fixture available"
        );
        return;
    };
    set_vrr_capable(&mut fixture);
    let mut fixture = fixture.cleanup_guard();
    let fd = super::direct_unflip::fd(&fixture);
    let crtc = fixture.events.backend.platform.outputs[0].output.crtc;
    let output_name = fixture.events.backend.platform.outputs[0]
        .key
        .connector_name
        .clone();
    let mut state = present_request_state(&direct);
    let property = state.atoms.intern("_VARIABLE_REFRESH", false).0;
    prepare_property_present_source(&mut fixture, &direct);
    request_vrr_property(
        &mut state,
        &mut fixture.events.backend,
        direct.target_xid,
        property,
        yserver_core::xinput::XA_CARDINAL.0,
        32,
        &[1],
        1,
    );
    direct.candidate.variable_refresh = true;
    let initial = accepted_direct_record(&mut fixture, &direct, fd);
    assert!(contains_vrr_property(&initial, crtc, 1));
    complete_direct_and_drain_present(&mut fixture, &initial, direct.candidate.present_id, None);

    let pending = submit_direct_successor(&mut fixture, &mut direct, fd);
    let pending_present_id = direct.candidate.present_id;
    queue_direct_successor(&mut fixture, &mut direct);
    let queued_present_id = direct.candidate.present_id;
    let offset = super::direct_unflip::record_count(&fixture, fd);
    super::recording_scene::clear_vrr_test_logs();

    delete_vrr_property(
        &mut state,
        &mut fixture.events.backend,
        direct.target_xid,
        property,
        4,
    );
    assert!(
        super::direct_unflip::records_since(&fixture, fd, offset).is_empty(),
        "property deletion waits for the queued successor instead of a standalone clear"
    );
    assert!(
        fixture.events.backend.platform.outputs[0]
            .output
            .vrr_committed
            .get()
    );

    super::atomic_events::complete_recorded_attempt(
        &mut fixture,
        fd,
        &pending,
        super::support::accepted_event_id(&pending).expect("pending event ID"),
        87,
    );
    super::direct_unflip::drain_present_notifications(
        &mut fixture,
        &[pending_present_id],
        &[pending_present_id - 1],
    );
    let successor = super::direct_unflip::records_since(&fixture, fd, offset)
        .into_iter()
        .find(|record| super::support::accepted_event_id(record).is_some())
        .expect("queued successor submits after predecessor retirement");
    assert!(contains_vrr_property(&successor, crtc, 0));
    assert!(
        !fixture.events.backend.platform.outputs[0]
            .output
            .vrr_committed
            .get(),
        "accepted queued successor clears VRR_ENABLED"
    );
    let logs = super::recording_scene::vrr_test_logs();
    assert!(
        logs.lines().any(|line| {
            line == format!(
                "kms: vrr off output={output_name} crtc={} reason=property",
                u32::from(crtc)
            )
        }),
        "the queued property clear logs its cause; logs={logs:?}"
    );

    complete_direct_and_drain_present(
        &mut fixture,
        &successor,
        queued_present_id,
        Some(pending_present_id),
    );
    finish_property_direct_scanout(&mut fixture, fd);
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: issue a standalone VRR enable on property set; enabling must ride the next direct frame.
fn vrr_property_set_enables_on_next_frame() {
    let Some((mut fixture, direct)) =
        direct_fixture_with_vrr_mode(18_044, 0, crate::launch::VrrMode::Property)
    else {
        eprintln!("SKIP vrr_property_set_enables_on_next_frame: no Vulkan scene fixture available");
        return;
    };
    set_vrr_capable(&mut fixture);
    let mut fixture = fixture.cleanup_guard();
    let fd = super::direct_unflip::fd(&fixture);
    let crtc = fixture.events.backend.platform.outputs[0].output.crtc;
    let (mut state, property, initial) = start_property_direct(&mut fixture, &direct, false);
    assert!(!has_vrr_property(&initial, crtc));
    complete_direct_and_drain_present(&mut fixture, &initial, 1, None);
    let offset = super::direct_unflip::record_count(&fixture, fd);

    request_vrr_property(
        &mut state,
        &mut fixture.events.backend,
        direct.target_xid,
        property,
        yserver_core::xinput::XA_CARDINAL.0,
        32,
        &[1],
        3,
    );
    Backend::maybe_composite(&mut fixture.events.backend)
        .expect("property set waits for a direct frame");
    assert!(
        super::direct_unflip::records_since(&fixture, fd, offset).is_empty(),
        "setting the property does not make a standalone commit"
    );
    assert!(
        !fixture.events.backend.platform.outputs[0]
            .output
            .vrr_committed
            .get()
    );

    let next = submit_property_present(&mut fixture, &mut state, &direct, 18_045, 4)
        .into_iter()
        .find(|record| super::support::accepted_event_id(record).is_some())
        .expect("next Present reaches direct scanout");
    assert!(contains_vrr_property(&next, crtc, 1));
    assert!(
        fixture.events.backend.platform.outputs[0]
            .output
            .vrr_committed
            .get()
    );
    complete_direct_and_drain_present(&mut fixture, &next, 2, Some(1));
    finish_property_direct_scanout(&mut fixture, fd);
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: apply another window's property value to the direct owner and emit an unrelated clear.
fn vrr_property_change_other_window_ignored() {
    let Some((mut fixture, direct)) =
        direct_fixture_with_vrr_mode(18_046, 0, crate::launch::VrrMode::Property)
    else {
        eprintln!(
            "SKIP vrr_property_change_other_window_ignored: no Vulkan scene fixture available"
        );
        return;
    };
    set_vrr_capable(&mut fixture);
    let mut fixture = fixture.cleanup_guard();
    let fd = super::direct_unflip::fd(&fixture);
    let (mut state, property, initial) = start_property_direct(&mut fixture, &direct, true);
    complete_direct_and_drain_present(&mut fixture, &initial, 1, None);
    let other_window = direct.target_xid.wrapping_add(0x100);
    add_property_window(&mut state, other_window, other_window.wrapping_add(0x1000));
    let recorder = fixture.events.recorders[&fd].clone();
    let offset = super::direct_unflip::record_count(&fixture, fd);

    request_vrr_property(
        &mut state,
        &mut fixture.events.backend,
        other_window,
        property,
        yserver_core::xinput::XA_CARDINAL.0,
        32,
        &[0],
        3,
    );
    Backend::maybe_composite(&mut fixture.events.backend)
        .expect("unrelated property notification is ignored");
    assert!(
        super::direct_unflip::records_since(&fixture, fd, offset).is_empty(),
        "an unrelated window never submits a CRTC property commit"
    );
    assert!(
        fixture.events.backend.platform.outputs[0]
            .output
            .vrr_committed
            .get()
    );
    assert!(
        fixture
            .events
            .backend
            .pstate
            .direct
            .root()
            .current
            .as_ref()
            .unwrap()
            .candidate
            .variable_refresh,
        "the direct owner's stored permission remains enabled"
    );
    assert_eq!(recorder.records().len(), offset);
    finish_property_direct_scanout(&mut fixture, fd);
    fixture.finish_after_normal_drain();
}

fn queued_property_change_case(initial: bool, changed: bool, present_id: u64) {
    let Some((mut fixture, mut direct)) =
        direct_fixture_with_vrr_mode(present_id, 0, crate::launch::VrrMode::Property)
    else {
        eprintln!(
            "SKIP vrr_property_change_reaches_queued_successor: no Vulkan scene fixture available"
        );
        return;
    };
    set_vrr_capable(&mut fixture);
    let mut fixture = fixture.cleanup_guard();
    let fd = super::direct_unflip::fd(&fixture);
    let crtc = fixture.events.backend.platform.outputs[0].output.crtc;
    let mut state = present_request_state(&direct);
    let property = state.atoms.intern("_VARIABLE_REFRESH", false).0;
    prepare_property_present_source(&mut fixture, &direct);
    if initial {
        request_vrr_property(
            &mut state,
            &mut fixture.events.backend,
            direct.target_xid,
            property,
            yserver_core::xinput::XA_CARDINAL.0,
            32,
            &[1],
            1,
        );
    }
    direct.candidate.variable_refresh = initial;
    let first = accepted_direct_record(&mut fixture, &direct, fd);
    assert_eq!(has_vrr_property(&first, crtc), initial);
    complete_direct_and_drain_present(&mut fixture, &first, direct.candidate.present_id, None);

    let pending = submit_direct_successor(&mut fixture, &mut direct, fd);
    let pending_present_id = direct.candidate.present_id;
    queue_direct_successor(&mut fixture, &mut direct);
    let queued_present_id = direct.candidate.present_id;
    assert!(
        fixture
            .events
            .backend
            .pstate
            .direct
            .root()
            .queued_successor
            .is_some()
    );
    let change_sequence = 5;
    if changed {
        request_vrr_property(
            &mut state,
            &mut fixture.events.backend,
            direct.target_xid,
            property,
            yserver_core::xinput::XA_CARDINAL.0,
            32,
            &[1],
            change_sequence,
        );
    } else {
        delete_vrr_property(
            &mut state,
            &mut fixture.events.backend,
            direct.target_xid,
            property,
            change_sequence,
        );
    }
    for frame in [
        fixture.events.backend.pstate.direct.root().current.as_ref(),
        fixture.events.backend.pstate.direct.root().pending.as_ref(),
        fixture
            .events
            .backend
            .pstate
            .direct
            .root()
            .queued_successor
            .as_ref(),
    ] {
        assert_eq!(
            frame
                .expect("all three direct records remain live")
                .candidate
                .variable_refresh,
            changed,
            "property change updates current, pending, and queued direct records"
        );
    }

    let offset = super::direct_unflip::record_count(&fixture, fd);
    super::atomic_events::complete_recorded_attempt(
        &mut fixture,
        fd,
        &pending,
        super::support::accepted_event_id(&pending).expect("pending event ID"),
        87,
    );
    super::direct_unflip::drain_present_notifications(
        &mut fixture,
        &[pending_present_id],
        &[pending_present_id - 1],
    );
    let successor = super::direct_unflip::records_since(&fixture, fd, offset)
        .into_iter()
        .find(|record| super::support::accepted_event_id(record).is_some())
        .expect("queued successor submits after predecessor retirement");
    assert_eq!(
        has_vrr_property(&successor, crtc),
        initial != changed,
        "queued successor carries the new value only when it differs from committed state"
    );
    if initial != changed {
        assert!(contains_vrr_property(&successor, crtc, u64::from(changed)));
    }
    let successor_id = super::support::accepted_event_id(&successor).expect("successor event ID");
    super::direct_unflip::complete_direct_initial(&mut fixture, &successor, successor_id, fd);
    super::direct_unflip::drain_present_notifications(
        &mut fixture,
        &[queued_present_id],
        &[pending_present_id],
    );
    finish_property_direct_scanout(&mut fixture, fd);
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: update only current; a property change must also reach the queued successor.
fn vrr_property_change_reaches_queued_successor() {
    queued_property_change_case(false, true, 18_047);
    queued_property_change_case(true, false, 18_048);
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutations: drop the retry deadline or its next_wakeup wiring, or omit the timed clear log.
fn vrr_standalone_clear_retried_after_error() {
    let Some((mut fixture, direct)) =
        direct_fixture_with_vrr_mode(18_049, 0, crate::launch::VrrMode::Property)
    else {
        eprintln!(
            "SKIP vrr_standalone_clear_retried_after_error: no Vulkan scene fixture available"
        );
        return;
    };
    set_vrr_capable(&mut fixture);
    let mut fixture = fixture.cleanup_guard();
    let fd = super::direct_unflip::fd(&fixture);
    let crtc = fixture.events.backend.platform.outputs[0].output.crtc;
    let output_name = fixture.events.backend.platform.outputs[0]
        .key
        .connector_name
        .clone();
    let (mut state, property, initial) = start_property_direct(&mut fixture, &direct, true);
    let fb_id_property = fixture.events.backend.platform.outputs[0]
        .output
        .plane_fb_id_prop;
    let direct_framebuffer = initial
        .properties
        .iter()
        .find(|update| update.property == fb_id_property)
        .map(|update| update.value)
        .expect("accepted direct frame sets the primary FB_ID");
    complete_direct_and_drain_present(&mut fixture, &initial, 1, None);
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
    let recorder = fixture.events.recorders[&fd].clone();
    let (base, elapsed_ns) = super::support::install_controlled_clock(&mut fixture.events.backend);
    let refresh_period = Duration::from_nanos(1_000_000_000 / 60);
    let offset = super::direct_unflip::record_count(&fixture, fd);
    recorder.script_result(Err(io::Error::from_raw_os_error(libc::ENOMEM)));

    delete_vrr_property(
        &mut state,
        &mut fixture.events.backend,
        direct.target_xid,
        property,
        3,
    );
    let failed = super::direct_unflip::records_since(&fixture, fd, offset);
    assert_eq!(failed.len(), 1, "the standalone clear is attempted once");
    assert!(contains_vrr_property(&failed[0], crtc, 0));
    assert!(matches!(
        failed[0].result,
        ScriptedResult::Error {
            raw_os_error: Some(error),
            ..
        } if error == libc::ENOMEM
    ));
    let deadline = fixture.events.backend.platform.outputs[0]
        .output
        .vrr_clear_retry_deadline
        .get();
    assert_eq!(
        deadline,
        Some(base + refresh_period),
        "the retry deadline is one output refresh period after ENOMEM"
    );
    assert_eq!(
        Backend::next_wakeup(&fixture.events.backend),
        Some(base + refresh_period),
        "with unrelated KMS timers drained, next_wakeup is the standalone-clear retry deadline"
    );
    Backend::maybe_composite(&mut fixture.events.backend)
        .expect("do not retry the failed standalone clear early");
    assert_eq!(
        super::direct_unflip::record_count(&fixture, fd) - offset,
        1,
        "no immediate retry spins while waiting for the deadline"
    );

    advance_vrr_cursor_clock(&elapsed_ns, refresh_period - Duration::from_nanos(1));
    Backend::maybe_composite(&mut fixture.events.backend)
        .expect("stay idle just before the retry deadline");
    assert_eq!(
        super::direct_unflip::record_count(&fixture, fd) - offset,
        1,
        "one nanosecond before the deadline still submits nothing"
    );
    advance_vrr_cursor_clock(&elapsed_ns, Duration::from_nanos(1));
    Backend::maybe_composite(&mut fixture.events.backend).expect("retry clear at its deadline");
    let attempts = super::direct_unflip::records_since(&fixture, fd, offset);
    assert_eq!(attempts.len(), 2, "the failed clear is retried once");
    assert_eq!(attempts[1].result, ScriptedResult::Ok);
    assert_eq!(attempts[1].kind, AtomicCommitKind::VrrOnly);
    assert!(contains_vrr_property(&attempts[1], crtc, 0));
    assert_eq!(
        attempts[1]
            .properties
            .iter()
            .filter(|update| update.property == fb_id_property)
            .map(|update| update.value)
            .collect::<Vec<_>>(),
        vec![direct_framebuffer],
        "timed retry carries the current direct framebuffer"
    );
    assert_eq!(attempts[1].properties.len(), 2);
    assert!(
        !fixture.events.backend.platform.outputs[0]
            .output
            .vrr_committed
            .get()
    );
    let logs = super::recording_scene::vrr_test_logs();
    assert!(
        logs.lines().any(|line| {
            line == format!(
                "kms: vrr off output={output_name} crtc={} reason=property",
                u32::from(crtc)
            )
        }),
        "the timed standalone clear logs its accepted VRR transition; logs={logs:?}"
    );
    let retry_id = super::support::accepted_event_id(&attempts[1]).expect("retry event ID");
    super::atomic_events::complete_recorded_attempt(&mut fixture, fd, &attempts[1], retry_id, 88);
    finish_property_direct_scanout(&mut fixture, fd);
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: select only the first matching direct owner with `find_map`.
fn vrr_property_change_reaches_every_owner_of_the_window() {
    use crate::kms::render::backend::DirectScope;
    use yserver_core::backend::WindowHandle;
    use yserver_protocol::x11::ResourceId;

    for (retiring_idx, current_idx) in [(0, 1), (1, 0)] {
        let Some(fixture) = super::recording_scene::RecordingSceneFixture::with_route(
            2,
            super::recording_scene::P1SceneRoute::Shared,
        )
        .expect("two-output VRR recording scene") else {
            eprintln!(
                "SKIP vrr_property_change_reaches_every_owner_of_the_window: Vulkan fixture unavailable"
            );
            return;
        };
        let mut fixture = fixture.cleanup_guard();
        fixture.events.backend.pstate.vrr_mode = crate::launch::VrrMode::Property;
        for layout in &mut fixture.events.backend.platform.outputs {
            layout.output.crtc_vrr_enabled_prop = Some(handle(VRR_PROP_ID));
            layout.output.connector_vrr_capable = true;
        }

        let output0 = fixture.install_direct_output_present(0, 18_060, 0);
        let output1 = fixture.install_direct_output_present(1, 18_061, 0);
        let fd = super::direct_unflip::fd(&fixture);
        let mut state = present_request_state(&output0);
        let shared_host_xid = 0x7b20;
        state
            .resources
            .window_mut(ResourceId(output0.target_xid))
            .expect("Output(0) property window")
            .host_xid = WindowHandle::from_raw(shared_host_xid);
        add_property_window(&mut state, output1.target_xid, shared_host_xid);
        let property = state.atoms.intern("_VARIABLE_REFRESH", false).0;
        request_vrr_property(
            &mut state,
            &mut fixture.events.backend,
            output0.target_xid,
            property,
            yserver_core::xinput::XA_CARDINAL.0,
            32,
            &[1],
            1,
        );

        let direct0 =
            super::direct_unflip::submit_output_present_via_backend(&mut fixture, &output0, 0);
        let direct1 =
            super::direct_unflip::submit_output_present_via_backend(&mut fixture, &output1, 1);
        let direct_ids = [
            super::support::accepted_event_id(&direct0).expect("Output(0) direct ID"),
            super::support::accepted_event_id(&direct1).expect("Output(1) direct ID"),
        ];
        let crtcs = [
            fixture.events.backend.platform.outputs[0].output.crtc,
            fixture.events.backend.platform.outputs[1].output.crtc,
        ];
        assert!(contains_vrr_property(&direct0, crtcs[0], 1));
        assert!(contains_vrr_property(&direct1, crtcs[1], 1));
        super::atomic_events::complete_recorded_attempt(
            &mut fixture,
            fd,
            &direct0,
            direct_ids[0],
            92,
        );
        super::atomic_events::complete_recorded_attempt(
            &mut fixture,
            fd,
            &direct1,
            direct_ids[1],
            93,
        );
        assert_eq!(
            Backend::drain_completed_present_events(&mut fixture.events.backend)
                .into_iter()
                .map(|event| event.present_id)
                .collect::<Vec<_>>(),
            [output0.candidate.present_id, output1.candidate.present_id]
        );
        let before_pins = fixture.events.backend.present_source_pins.len();

        let retiring = if retiring_idx == 0 {
            &output0
        } else {
            &output1
        };
        Backend::unmap_subwindow(&mut fixture.events.backend, None, retiring.target_xid)
            .expect("retiring owner's unmap enters through Backend");
        assert!(fixture.events.backend.pstate.direct.outputs[retiring_idx].unflip_requested);

        let clear_start = super::direct_unflip::record_count(&fixture, fd);
        delete_vrr_property(
            &mut state,
            &mut fixture.events.backend,
            output0.target_xid,
            property,
            4,
        );
        Backend::maybe_composite(&mut fixture.events.backend)
            .expect("run the property-change scheduling pass");

        for output_idx in 0..2 {
            let owner = &fixture.events.backend.pstate.direct.outputs[output_idx];
            let frame = owner
                .current
                .as_ref()
                .expect("both owners still retain the window");
            assert!(
                !frame.candidate.variable_refresh,
                "Output({output_idx}) current candidate sees the deleted property"
            );
            assert_eq!(
                fixture
                    .events
                    .backend
                    .pstate
                    .direct
                    .scope_for_output(output_idx),
                Some(DirectScope::Output(output_idx))
            );
        }
        let current_crtc = crtcs[current_idx];
        let after_change = super::direct_unflip::records_since(&fixture, fd, clear_start);
        let current_clear = after_change.iter().find(|record| {
            super::support::accepted_event_id(record).is_some()
                && record.kind == AtomicCommitKind::VrrOnly
                && contains_vrr_property(record, current_crtc, 0)
        });
        assert!(
            current_clear.is_some(),
            "the still-current owner on output {current_idx} gets a standalone VRR clear"
        );
        assert!(
            after_change.iter().all(|record| {
                super::support::accepted_event_id(record).is_none()
                    || record.kind == AtomicCommitKind::VrrOnly
            }),
            "property deletion does not require or submit another Present"
        );
        assert_eq!(
            fixture.events.backend.present_source_pins.len(),
            before_pins,
            "the standalone property clear retains the existing direct frames"
        );
        for (offset, record) in after_change.iter().enumerate() {
            let Some(event_id) = super::support::accepted_event_id(record) else {
                continue;
            };
            super::atomic_events::complete_recorded_attempt(
                &mut fixture,
                fd,
                record,
                event_id,
                94 + u32::try_from(offset).expect("record index fits"),
            );
        }
        for (output_idx, direct) in [(0, &output0), (1, &output1)] {
            if !fixture.events.backend.pstate.direct.outputs[output_idx].unflip_requested {
                Backend::unmap_subwindow(&mut fixture.events.backend, None, direct.target_xid)
                    .expect("request remaining owner unflip through Backend");
            }
        }
        let cleanup_start = super::direct_unflip::record_count(&fixture, fd);
        Backend::maybe_composite(&mut fixture.events.backend)
            .expect("submit both output-owner replacements");
        fixture.events.wait_for_submitted_gpu_work();
        let cleanup_records = super::direct_unflip::records_since(&fixture, fd, cleanup_start);
        for (offset, record) in cleanup_records.iter().enumerate() {
            let Some(event_id) = super::support::accepted_event_id(record) else {
                continue;
            };
            super::atomic_events::complete_recorded_attempt(
                &mut fixture,
                fd,
                record,
                event_id,
                100 + u32::try_from(offset).expect("cleanup record index fits"),
            );
        }
        assert!(
            fixture
                .events
                .backend
                .pstate
                .direct
                .outputs
                .iter()
                .all(|owner| owner.current.is_none()),
            "both direct owners release their current frames after replacement"
        );
        assert!(fixture.events.backend.present_source_pins.is_empty());
        let mut idled = Backend::drain_retired_present_idle_events(&mut fixture.events.backend)
            .into_iter()
            .map(|event| event.present_id)
            .collect::<Vec<_>>();
        idled.sort_unstable();
        assert_eq!(
            idled,
            [output0.candidate.present_id, output1.candidate.present_id]
        );
        fixture.finish_after_normal_drain();
    }
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: route a standalone VRR clear through an empty stripped retry; EINVAL must latch directly without that retry.
fn vrr_standalone_clear_einval_latches_without_empty_retry() {
    let Some((mut fixture, direct)) =
        direct_fixture_with_vrr_mode(18_050, 0, crate::launch::VrrMode::Property)
    else {
        eprintln!(
            "SKIP vrr_standalone_clear_einval_latches_without_empty_retry: no Vulkan scene fixture available"
        );
        return;
    };
    set_vrr_capable(&mut fixture);
    let mut fixture = fixture.cleanup_guard();
    let fd = super::direct_unflip::fd(&fixture);
    let crtc = fixture.events.backend.platform.outputs[0].output.crtc;
    let (mut state, property, initial) = start_property_direct(&mut fixture, &direct, true);
    complete_direct_and_drain_present(&mut fixture, &initial, 1, None);
    let recorder = fixture.events.recorders[&fd].clone();
    let offset = super::direct_unflip::record_count(&fixture, fd);
    recorder.script_result(Err(io::Error::from_raw_os_error(libc::EINVAL)));

    delete_vrr_property(
        &mut state,
        &mut fixture.events.backend,
        direct.target_xid,
        property,
        3,
    );
    let attempts = super::direct_unflip::records_since(&fixture, fd, offset);
    assert_eq!(
        attempts.len(),
        1,
        "standalone EINVAL does not submit an empty fallback request"
    );
    assert!(contains_vrr_property(&attempts[0], crtc, 0));
    assert!(matches!(
        attempts[0].result,
        ScriptedResult::Error {
            raw_os_error: Some(error),
            ..
        } if error == libc::EINVAL
    ));
    let output = &fixture.events.backend.platform.outputs[0].output;
    assert!(
        output.vrr_disabled.get(),
        "standalone EINVAL latches VRR off"
    );
    assert!(
        output.vrr_committed.get(),
        "the rejected clear is not recorded as committed"
    );
    assert_eq!(output.vrr_clear_retry_deadline.get(), None);

    Backend::maybe_composite(&mut fixture.events.backend)
        .expect("latched standalone EINVAL does not spin or submit an empty request");
    assert_eq!(
        super::direct_unflip::record_count(&fixture, fd) - offset,
        1,
        "no empty fallback or immediate retry follows standalone EINVAL"
    );
    finish_property_direct_scanout(&mut fixture, fd);
    fixture.finish_after_normal_drain();
}

fn write_fake_process(proc_root: &Path, pid: u32, name: &str) {
    let executable = format!("/usr/bin/{name}");
    write_fake_process_fixture(proc_root, pid, &executable, &executable, &[], None);
}

fn accepted_vrr_direct(
    fixture: &mut super::recording_scene::RecordingSceneFixtureGuard,
    direct: &super::recording_scene::DirectPresentSetup,
) -> AtomicCommitRecord {
    let fd = super::direct_unflip::fd(fixture);
    super::direct_unflip::submit_direct(fixture, direct, fd)
        .into_iter()
        .find(|record| super::support::accepted_event_id(record).is_some())
        .expect("Backend accepts the Present direct frame")
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: remove the non-NVIDIA fallback and always treat `All` as list mode.
fn vrr_all_mode_is_property_mode_outside_nvidia() {
    let tree = VrrTestTree::new();
    let roots = vrr_test_roots(&tree);
    let proc_root = tree.path().join("proc");

    let Some((mut fixture, mut direct)) = direct_fixture(19_001, 0) else {
        eprintln!(
            "SKIP vrr_all_mode_is_property_mode_outside_nvidia: no Vulkan scene fixture available"
        );
        return;
    };
    fixture.events.backend.for_tests_configure_vrr_mode(
        crate::launch::VrrMode::All,
        "amdgpu",
        roots.clone(),
        proc_root.clone(),
    );
    direct.candidate.variable_refresh = false;
    set_vrr_capable(&mut fixture);
    let mut fixture = fixture.cleanup_guard();
    let crtc = fixture.events.backend.platform.outputs[0].output.crtc;
    let without_property = accepted_vrr_direct(&mut fixture, &direct);
    assert!(
        !contains_vrr_property(&without_property, crtc, 1),
        "outside NVIDIA, a direct window without _VARIABLE_REFRESH remains off"
    );
    assert!(
        fixture.events.backend.pstate.vrr_exclusions.is_none(),
        "non-NVIDIA startup does not load either exclusion list"
    );
    complete_direct_and_drain_present(
        &mut fixture,
        &without_property,
        direct.candidate.present_id,
        None,
    );
    let fd = super::direct_unflip::fd(&fixture);
    finish_direct_scanout(&mut fixture, &direct, fd);
    fixture.finish_after_normal_drain();

    let Some((mut fixture, mut direct)) = direct_fixture(19_002, 0) else {
        eprintln!(
            "SKIP vrr_all_mode_is_property_mode_outside_nvidia: no Vulkan scene fixture available for property case"
        );
        return;
    };
    fixture.events.backend.for_tests_configure_vrr_mode(
        crate::launch::VrrMode::All,
        "amdgpu",
        roots.clone(),
        proc_root.clone(),
    );
    direct.candidate.variable_refresh = true;
    set_vrr_capable(&mut fixture);
    let mut fixture = fixture.cleanup_guard();
    let crtc = fixture.events.backend.platform.outputs[0].output.crtc;
    let with_property = accepted_vrr_direct(&mut fixture, &direct);
    assert!(
        contains_vrr_property(&with_property, crtc, 1),
        "outside NVIDIA, a direct window with _VARIABLE_REFRESH enables VRR"
    );
    complete_direct_and_drain_present(
        &mut fixture,
        &with_property,
        direct.candidate.present_id,
        None,
    );
    let fd = super::direct_unflip::fd(&fixture);
    finish_direct_scanout(&mut fixture, &direct, fd);
    fixture.finish_after_normal_drain();

    let Some((mut fixture, mut direct)) = direct_fixture(19_003, 0) else {
        eprintln!(
            "SKIP vrr_all_mode_is_property_mode_outside_nvidia: no Vulkan scene fixture available for NVIDIA case"
        );
        return;
    };
    fixture.events.backend.for_tests_configure_vrr_mode(
        crate::launch::VrrMode::All,
        "nvidia-drm",
        roots,
        proc_root,
    );
    direct.candidate.variable_refresh = false;
    set_vrr_capable(&mut fixture);
    let mut fixture = fixture.cleanup_guard();
    let crtc = fixture.events.backend.platform.outputs[0].output.crtc;
    let nvidia_without_property = accepted_vrr_direct(&mut fixture, &direct);
    assert!(
        contains_vrr_property(&nvidia_without_property, crtc, 1),
        "NVIDIA All mode ignores _VARIABLE_REFRESH"
    );
    complete_direct_and_drain_present(
        &mut fixture,
        &nvidia_without_property,
        direct.candidate.present_id,
        None,
    );
    let fd = super::direct_unflip::fd(&fixture);
    finish_direct_scanout(&mut fixture, &direct, fd);
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutations: omit the missing Mesa-list warning or startup exclusion counts while loading NVIDIA profiles.
fn vrr_exclusions_without_mesa_files_warns() {
    super::recording_scene::init_tearing_test_logger();
    super::recording_scene::clear_vrr_test_logs();
    let tree = VrrTestTree::new();
    let roots = vrr_test_roots(&tree);
    let proc_root = tree.path().join("proc");
    write_vrr_test_file(
        roots
            .nvidia_driver_dir
            .join("nvidia-application-profiles-580-rc"),
        r#"{"rules":[{"pattern":"listed","profile":["GLVRRAllowed",false]}]}"#,
    );
    write_fake_process(&proc_root, 51, "listed");

    let Some((mut fixture, mut direct)) = direct_fixture(19_005, 0) else {
        eprintln!(
            "SKIP vrr_exclusions_without_mesa_files_warns: no Vulkan scene fixture available"
        );
        return;
    };
    fixture.events.backend.for_tests_configure_vrr_mode(
        crate::launch::VrrMode::All,
        "nvidia-drm",
        roots.clone(),
        proc_root,
    );
    direct.candidate.client_pid = Some(51);
    set_vrr_capable(&mut fixture);
    let mut fixture = fixture.cleanup_guard();
    let crtc = fixture.events.backend.platform.outputs[0].output.crtc;
    let record = accepted_vrr_direct(&mut fixture, &direct);
    assert!(
        !contains_vrr_property(&record, crtc, 1),
        "the NVIDIA list continues to exclude its listed process without Mesa files"
    );
    complete_direct_and_drain_present(&mut fixture, &record, direct.candidate.present_id, None);
    let logs = super::recording_scene::vrr_test_logs();
    let warnings: Vec<_> = logs
        .lines()
        .filter(|line| line.contains("requires Mesa") && line.contains("00-mesa-defaults.conf"))
        .collect();
    assert_eq!(
        warnings.len(),
        1,
        "one startup warning names the missing Mesa file"
    );
    assert!(
        warnings[0].contains("mesa-dri-drivers"),
        "the warning names the Fedora package for the missing Mesa file"
    );
    assert!(
        logs.contains("kms: vrr exclusions nvidia=1 mesa=0"),
        "startup reports the loaded NVIDIA and Mesa exclusion counts"
    );
    let fd = super::direct_unflip::fd(&fixture);
    finish_direct_scanout(&mut fixture, &direct, fd);
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: skip the cached client exclusion lookup before building the direct request.
fn vrr_all_mode_excluded_client_never_enables() {
    let tree = VrrTestTree::new();
    let roots = vrr_test_roots(&tree);
    let proc_root = tree.path().join("proc");
    write_vrr_test_file(
        roots
            .nvidia_driver_dir
            .join("nvidia-application-profiles-580-rc"),
        r#"{"rules":[{"pattern":"blocked","profile":["GLVRRAllowed",false]}]}"#,
    );
    write_fake_process(&proc_root, 61, "blocked");
    write_fake_process(&proc_root, 62, "allowed");

    let Some((mut fixture, mut direct)) = direct_fixture(19_010, 0) else {
        eprintln!(
            "SKIP vrr_all_mode_excluded_client_never_enables: no Vulkan scene fixture available"
        );
        return;
    };
    fixture.events.backend.for_tests_configure_vrr_mode(
        crate::launch::VrrMode::All,
        "nvidia-drm",
        roots,
        proc_root,
    );
    set_vrr_capable(&mut fixture);
    let mut fixture = fixture.cleanup_guard();
    let crtc = fixture.events.backend.platform.outputs[0].output.crtc;
    direct.candidate.client_pid = Some(61);
    let excluded = accepted_vrr_direct(&mut fixture, &direct);
    assert!(
        !contains_vrr_property(&excluded, crtc, 1),
        "a client whose process is in the NVIDIA list never enables VRR"
    );
    complete_direct_and_drain_present(&mut fixture, &excluded, direct.candidate.present_id, None);
    let mut unlisted = direct;
    unlisted.candidate.client_id = 2;
    unlisted.candidate.client_pid = Some(62);
    unlisted.event.client_id = yserver_protocol::x11::ClientId(2);
    let fd = super::direct_unflip::fd(&fixture);
    let previous_present_id = unlisted.candidate.present_id;
    let allowed = submit_direct_successor(&mut fixture, &mut unlisted, fd);
    assert!(
        contains_vrr_property(&allowed, crtc, 1),
        "an unlisted client's direct frame enables VRR"
    );
    complete_direct_and_drain_present(
        &mut fixture,
        &allowed,
        unlisted.candidate.present_id,
        Some(previous_present_id),
    );
    finish_direct_scanout(&mut fixture, &unlisted, fd);
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: treat a missing peer PID as an exclusion instead of the property-free All behavior.
// Mutation: omit the missing-PID debug diagnostic.
fn vrr_all_mode_client_without_pid_enables() {
    super::recording_scene::init_tearing_test_logger();
    super::recording_scene::clear_vrr_test_logs();
    let tree = VrrTestTree::new();
    let Some((mut fixture, mut direct)) = direct_fixture(19_020, 0) else {
        eprintln!(
            "SKIP vrr_all_mode_client_without_pid_enables: no Vulkan scene fixture available"
        );
        return;
    };
    fixture.events.backend.for_tests_configure_vrr_mode(
        crate::launch::VrrMode::All,
        "nvidia-drm",
        vrr_test_roots(&tree),
        tree.path().join("proc"),
    );
    set_vrr_capable(&mut fixture);
    let mut fixture = fixture.cleanup_guard();
    let crtc = fixture.events.backend.platform.outputs[0].output.crtc;
    let record = accepted_vrr_direct(&mut fixture, &direct);
    assert!(
        contains_vrr_property(&record, crtc, 1),
        "a connection without a peer PID is not excluded"
    );
    complete_direct_and_drain_present(&mut fixture, &record, direct.candidate.present_id, None);

    let fd = super::direct_unflip::fd(&fixture);
    let previous_present_id = direct.candidate.present_id;
    let successor = submit_direct_successor(&mut fixture, &mut direct, fd);
    assert!(
        super::support::accepted_event_id(&successor).is_some(),
        "the next Present remains accepted"
    );
    let no_pid_logs = super::recording_scene::vrr_test_logs()
        .lines()
        .filter(|line| line.starts_with("kms: vrr exclusion skipped client=1 reason=no-pid"))
        .count();
    assert_eq!(
        no_pid_logs, 1,
        "a missing peer PID is diagnosed once per connected client"
    );
    complete_direct_and_drain_present(
        &mut fixture,
        &successor,
        direct.candidate.present_id,
        Some(previous_present_id),
    );
    finish_direct_scanout(&mut fixture, &direct, fd);
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: apply All-mode file exclusions while the requested mode is Property.
fn vrr_property_mode_ignores_lists() {
    let tree = VrrTestTree::new();
    let roots = vrr_test_roots(&tree);
    let proc_root = tree.path().join("proc");
    write_vrr_test_file(
        roots
            .nvidia_driver_dir
            .join("nvidia-application-profiles-580-rc"),
        r#"{"rules":[{"pattern":"listed","profile":["GLVRRAllowed",false]}]}"#,
    );
    write_fake_process(&proc_root, 71, "listed");
    let Some((mut fixture, mut direct)) = direct_fixture(19_030, 0) else {
        eprintln!("SKIP vrr_property_mode_ignores_lists: no Vulkan scene fixture available");
        return;
    };
    fixture.events.backend.for_tests_configure_vrr_mode(
        crate::launch::VrrMode::Property,
        "nvidia-drm",
        roots,
        proc_root,
    );
    direct.candidate.client_pid = Some(71);
    direct.candidate.variable_refresh = true;
    set_vrr_capable(&mut fixture);
    let mut fixture = fixture.cleanup_guard();
    let crtc = fixture.events.backend.platform.outputs[0].output.crtc;
    let record = accepted_vrr_direct(&mut fixture, &direct);
    assert!(
        contains_vrr_property(&record, crtc, 1),
        "Property mode follows _VARIABLE_REFRESH despite a listed process name"
    );
    assert!(
        fixture.events.backend.pstate.vrr_exclusions.is_none(),
        "Property mode does not load NVIDIA or Mesa lists"
    );
    complete_direct_and_drain_present(&mut fixture, &record, direct.candidate.present_id, None);
    let fd = super::direct_unflip::fd(&fixture);
    finish_direct_scanout(&mut fixture, &direct, fd);
    fixture.finish_after_normal_drain();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: repeat the proc lookup for every Present or retain a cache entry after disconnect.
// Mutation: log a cached exclusion again on the next Present.
fn vrr_exclusion_computed_once_per_client() {
    super::recording_scene::init_tearing_test_logger();
    super::recording_scene::clear_vrr_test_logs();
    let tree = VrrTestTree::new();
    let roots = vrr_test_roots(&tree);
    let proc_root = tree.path().join("proc");
    write_vrr_test_file(
        roots
            .nvidia_driver_dir
            .join("nvidia-application-profiles-580-rc"),
        r#"{"rules":[{"pattern":"blocked","profile":["GLVRRAllowed",false]}]}"#,
    );
    write_fake_process(&proc_root, 81, "blocked");
    let Some((mut fixture, mut direct)) = direct_fixture(19_040, 0) else {
        eprintln!("SKIP vrr_exclusion_computed_once_per_client: no Vulkan scene fixture available");
        return;
    };
    fixture.events.backend.for_tests_configure_vrr_mode(
        crate::launch::VrrMode::All,
        "nvidia-drm",
        roots,
        proc_root.clone(),
    );
    set_vrr_capable(&mut fixture);
    let mut fixture = fixture.cleanup_guard();
    let crtc = fixture.events.backend.platform.outputs[0].output.crtc;
    direct.candidate.client_pid = Some(81);
    let first = accepted_vrr_direct(&mut fixture, &direct);
    assert!(
        !contains_vrr_property(&first, crtc, 1),
        "the first excluded Present carries no VRR enable"
    );
    complete_direct_and_drain_present(&mut fixture, &first, direct.candidate.present_id, None);

    write_fake_process(&proc_root, 81, "allowed");
    let fd = super::direct_unflip::fd(&fixture);
    let previous_present_id = direct.candidate.present_id;
    let second = submit_direct_successor(&mut fixture, &mut direct, fd);
    assert!(
        !contains_vrr_property(&second, crtc, 1),
        "the cached exclusion remains in force for the same connected client"
    );
    let exclusion_logs = super::recording_scene::vrr_test_logs()
        .lines()
        .filter(|line| line.starts_with("kms: vrr excluded client=1 pid=81"))
        .count();
    assert_eq!(
        exclusion_logs, 1,
        "a cached excluded client is logged only once across Presents"
    );
    complete_direct_and_drain_present(
        &mut fixture,
        &second,
        direct.candidate.present_id,
        Some(previous_present_id),
    );

    Backend::client_disconnected(
        &mut fixture.events.backend,
        yserver_protocol::x11::ClientId(1),
    );
    let previous_present_id = direct.candidate.present_id;
    let third = submit_direct_successor(&mut fixture, &mut direct, fd);
    assert!(
        contains_vrr_property(&third, crtc, 1),
        "disconnect clears the decision so a later connection with the same id is read again"
    );
    complete_direct_and_drain_present(
        &mut fixture,
        &third,
        direct.candidate.present_id,
        Some(previous_present_id),
    );
    finish_direct_scanout(&mut fixture, &direct, fd);
    fixture.finish_after_normal_drain();
}
