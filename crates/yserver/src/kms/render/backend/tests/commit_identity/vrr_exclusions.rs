//! Application exclusion tests over the Backend/Present request paths.

use super::*;
use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

const FAST_INTERVAL_NS: u64 = 16_000_000;

#[derive(Clone, Copy)]
enum OutputCapability {
    Capable,
    ConnectorVrrCapableMissing,
    ConnectorVrrCapableZero,
    CrtcVrrEnabledMissing,
    RejectionLatched,
}

struct ExclusionFixture {
    _tree: VrrTestTree,
    proc_root: PathBuf,
    fixture: super::super::recording_scene::RecordingSceneFixtureGuard,
    direct: super::super::recording_scene::DirectPresentSetup,
    elapsed_ns: std::sync::Arc<AtomicU64>,
    fd: std::os::fd::RawFd,
    crtc: ::drm::control::crtc::Handle,
    records: Vec<AtomicCommitRecord>,
}

impl ExclusionFixture {
    #[allow(clippy::too_many_arguments)]
    fn new(
        present_id: u64,
        mode: crate::launch::VrrMode,
        driver_name: &str,
        client_pid: Option<u32>,
        process_name: Option<&str>,
        environment: &[&str],
        variable_refresh: bool,
        capability: OutputCapability,
        nvidia_list: Option<&str>,
    ) -> Self {
        let tree = VrrTestTree::new();
        let roots = vrr_test_roots(&tree);
        write_vrr_test_file(
            roots.mesa_system_dir.join("00-mesa-defaults.conf"),
            "<driconf/>",
        );
        if let Some(contents) = nvidia_list {
            write_vrr_test_file(
                roots
                    .nvidia_driver_dir
                    .join("nvidia-application-profiles-580-rc"),
                contents,
            );
        }
        let proc_root = tree.path().join("proc");
        if let (Some(pid), Some(process_name)) = (client_pid, process_name) {
            let executable = format!("/usr/bin/{process_name}");
            write_fake_process_fixture(
                &proc_root,
                pid,
                &executable,
                &executable,
                environment,
                None,
            );
        }

        let (mut raw, mut direct) =
            super::direct_fixture(present_id, 0).expect("lavapipe recording direct fixture");
        raw.events.backend.for_tests_configure_vrr_mode(
            mode,
            driver_name,
            roots,
            proc_root.clone(),
        );
        direct.candidate.client_pid = client_pid;
        direct.candidate.variable_refresh = variable_refresh;
        super::set_vrr_capable(&mut raw);
        let output = &mut raw.events.backend.platform.outputs[0].output;
        match capability {
            OutputCapability::Capable => {}
            OutputCapability::ConnectorVrrCapableMissing
            | OutputCapability::ConnectorVrrCapableZero => output.connector_vrr_capable = false,
            OutputCapability::CrtcVrrEnabledMissing => output.crtc_vrr_enabled_prop = None,
            OutputCapability::RejectionLatched => output.vrr_disabled.set(true),
        }

        let (_base, elapsed_ns) =
            super::super::support::install_controlled_clock(&mut raw.events.backend);
        let mut fixture = raw.cleanup_guard();
        let fd = super::super::direct_unflip::fd(&fixture);
        let crtc = fixture.events.backend.platform.outputs[0].output.crtc;
        let initial = super::accepted_vrr_direct(&mut fixture, &mut direct);
        super::complete_direct_and_drain_present(
            &mut fixture,
            &initial,
            direct.candidate.present_id,
            None,
        );
        Self {
            _tree: tree,
            proc_root,
            fixture,
            direct,
            elapsed_ns,
            fd,
            crtc,
            records: vec![initial],
        }
    }

    fn present(&mut self) -> AtomicCommitRecord {
        self.elapsed_ns
            .fetch_add(FAST_INTERVAL_NS, Ordering::SeqCst);
        let previous_present_id = self.direct.candidate.present_id;
        let record = super::submit_direct_successor(&mut self.fixture, &mut self.direct, self.fd);
        super::complete_direct_and_drain_present(
            &mut self.fixture,
            &record,
            self.direct.candidate.present_id,
            Some(previous_present_id),
        );
        self.records.push(record.clone());
        record
    }

    fn qualify(&mut self) {
        for _ in 0..3 {
            self.present();
        }
    }

    fn enabled_vrr(&self) -> bool {
        self.records
            .iter()
            .any(|record| contains_vrr_property(record, self.crtc, 1))
    }

    fn signatures(&self) -> Vec<RequestSignature> {
        super::request_signatures(&self.records)
    }

    fn finish(mut self) {
        super::finish_direct_scanout(&mut self.fixture, &self.direct, self.fd);
        self.fixture.finish_after_normal_drain();
    }
}

fn source_yserver_lines(pid: u32, process_name: &str) -> usize {
    super::super::recording_scene::vrr_test_logs()
        .lines()
        .filter(|line| {
            line.contains(&format!("pid={pid}"))
                && line.contains(&format!("process={process_name}"))
                && line.contains("source=yserver")
        })
        .count()
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: bypass the shared policy in property mode (at both the activity gate and wanted).
fn vrr_builtin_exclusion_steamwebhelper_property_mode() {
    super::super::recording_scene::init_tearing_test_logger();
    super::super::recording_scene::clear_vrr_test_logs();

    for (present_id, pid, mode) in [
        (21_001, 301, crate::launch::VrrMode::Property),
        (21_002, 302, crate::launch::VrrMode::All),
    ] {
        let mut fixture = ExclusionFixture::new(
            present_id,
            mode,
            "amdgpu",
            Some(pid),
            Some("steamwebhelper"),
            &[],
            true,
            OutputCapability::Capable,
            None,
        );
        fixture.qualify();
        assert!(
            !fixture.enabled_vrr(),
            "Property mode and --vrr all fallback exclude steamwebhelper after four fast Presents"
        );
        assert_eq!(
            source_yserver_lines(pid, "steamwebhelper"),
            1,
            "each excluded client has one source=yserver line"
        );
        fixture.finish();
    }
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: drop any one name.
fn vrr_builtin_exclusion_discord_all_mode() {
    super::super::recording_scene::init_tearing_test_logger();
    super::super::recording_scene::clear_vrr_test_logs();
    let processes = [
        ("Discord", 311),
        ("DiscordPTB", 312),
        ("DiscordCanary", 313),
    ];

    for (index, (process_name, pid)) in processes.into_iter().enumerate() {
        let mut fixture = ExclusionFixture::new(
            21_010 + index as u64,
            crate::launch::VrrMode::All,
            "nvidia-drm",
            Some(pid),
            Some(process_name),
            &[],
            true,
            OutputCapability::Capable,
            None,
        );
        fixture.qualify();
        assert!(
            !fixture.enabled_vrr(),
            "{process_name} remains at fixed refresh after four fast Presents"
        );
        assert_eq!(
            source_yserver_lines(pid, process_name),
            1,
            "{process_name} is attributed to the built-in yserver list"
        );
        fixture.finish();
    }
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: check the list before the environment, or let true beat false.
fn vrr_builtin_exclusion_environment_opt_in() {
    super::super::recording_scene::init_tearing_test_logger();
    super::super::recording_scene::clear_vrr_test_logs();

    for (present_id, pid, environment) in [
        (21_020, 321, vec!["adaptive_sync=true"]),
        (21_021, 322, vec!["__GL_VRR_ALLOWED=1"]),
    ] {
        let mut fixture = ExclusionFixture::new(
            present_id,
            crate::launch::VrrMode::Property,
            "amdgpu",
            Some(pid),
            Some("steamwebhelper"),
            &environment,
            true,
            OutputCapability::Capable,
            None,
        );
        fixture.qualify();
        assert!(
            fixture.enabled_vrr(),
            "{environment:?} opts the client back in"
        );
        assert_eq!(
            source_yserver_lines(pid, "steamwebhelper"),
            0,
            "an environment opt-in bypasses the built-in exclusion"
        );
        fixture.finish();
    }

    let mut conflicting = ExclusionFixture::new(
        21_022,
        crate::launch::VrrMode::Property,
        "amdgpu",
        Some(323),
        Some("steamwebhelper"),
        &["__GL_VRR_ALLOWED=1", "adaptive_sync=0"],
        true,
        OutputCapability::Capable,
        None,
    );
    conflicting.qualify();
    assert!(!conflicting.enabled_vrr());
    assert_eq!(
        super::super::recording_scene::vrr_test_logs()
            .lines()
            .filter(|line| line.contains("pid=323") && line.contains("source=environment"))
            .count(),
        1,
        "either false environment value wins and is reported as environment"
    );
    conflicting.finish();

    let mut property_off = ExclusionFixture::new(
        21_023,
        crate::launch::VrrMode::Property,
        "amdgpu",
        Some(324),
        Some("steamwebhelper"),
        &["adaptive_sync=true"],
        false,
        OutputCapability::Capable,
        None,
    );
    property_off.qualify();
    assert!(
        !property_off.enabled_vrr(),
        "an opt-in does not set _VARIABLE_REFRESH"
    );
    property_off.finish();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: substring match.
fn vrr_builtin_exclusion_exact_name() {
    super::super::recording_scene::init_tearing_test_logger();
    super::super::recording_scene::clear_vrr_test_logs();
    let nvidia_list = r#"{"rules":[{"pattern":"steam","profile":["GLVRRAllowed",false]},{"pattern":"steamwebhelper2","profile":["GLVRRAllowed",false]}]}"#;

    for (index, process_name) in ["steam", "steamwebhelper2"].into_iter().enumerate() {
        let mut fixture = ExclusionFixture::new(
            21_030 + index as u64,
            crate::launch::VrrMode::Property,
            "amdgpu",
            Some(331 + index as u32),
            Some(process_name),
            &[],
            true,
            OutputCapability::Capable,
            Some(nvidia_list),
        );
        fixture.qualify();
        assert!(
            fixture.enabled_vrr(),
            "Property mode allows exact non-matching process name {process_name}"
        );
        fixture.finish();
    }
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: a missing pid denies in the new path.
fn vrr_builtin_exclusion_no_pid_allowed() {
    super::super::recording_scene::init_tearing_test_logger();
    super::super::recording_scene::clear_vrr_test_logs();
    let mut fixture = ExclusionFixture::new(
        21_040,
        crate::launch::VrrMode::Property,
        "amdgpu",
        None,
        None,
        &[],
        true,
        OutputCapability::Capable,
        None,
    );
    fixture.qualify();
    assert!(
        fixture.enabled_vrr(),
        "a client without a pid is not excluded"
    );
    assert_eq!(
        super::super::recording_scene::vrr_test_logs()
            .lines()
            .filter(|line| line.contains("source=yserver"))
            .count(),
        0,
        "a client without a pid has no yserver exclusion line"
    );
    fixture.finish();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: run the lookup before the mode or capability check.
fn vrr_builtin_exclusion_off_or_incapable_reads_nothing() {
    super::super::recording_scene::init_tearing_test_logger();
    super::super::recording_scene::clear_vrr_test_logs();
    let cases = [
        (
            "VRR off",
            crate::launch::VrrMode::Off,
            true,
            OutputCapability::Capable,
        ),
        (
            "connector capability missing",
            crate::launch::VrrMode::Property,
            true,
            OutputCapability::ConnectorVrrCapableMissing,
        ),
        (
            "connector capability zero",
            crate::launch::VrrMode::Property,
            true,
            OutputCapability::ConnectorVrrCapableZero,
        ),
        (
            "VRR_ENABLED property missing",
            crate::launch::VrrMode::Property,
            true,
            OutputCapability::CrtcVrrEnabledMissing,
        ),
        (
            "rejection latched off",
            crate::launch::VrrMode::Property,
            true,
            OutputCapability::RejectionLatched,
        ),
        (
            "_VARIABLE_REFRESH missing",
            crate::launch::VrrMode::Property,
            false,
            OutputCapability::Capable,
        ),
    ];

    for (index, (label, mode, variable_refresh, capability)) in cases.into_iter().enumerate() {
        let pid = 341 + index as u32;
        let present_id = 21_050 + index as u64;
        let mut fixture = ExclusionFixture::new(
            present_id,
            mode,
            "amdgpu",
            Some(pid),
            Some("steamwebhelper"),
            &[],
            variable_refresh,
            capability,
            None,
        );
        fixture.qualify();
        assert_eq!(
            crate::vrr_exclusions::test_support::process_read_count(&fixture.proc_root),
            0,
            "{label}: the process-name reader was not entered"
        );
        assert!(
            !fixture.enabled_vrr(),
            "{label}: no VRR enable commit is sent"
        );
        assert_eq!(
            source_yserver_lines(pid, "steamwebhelper"),
            0,
            "{label}: no yserver exclusion line is logged"
        );
        assert_eq!(
            super::super::recording_scene::vrr_test_logs()
                .lines()
                .filter(|line| line.contains("kms: vrr excluded"))
                .count(),
            0,
            "{label}: no exclusion log line is added"
        );
        assert!(
            !super::super::recording_scene::vrr_test_logs()
                .lines()
                .any(|line| line.starts_with("kms: vrr ")),
            "{label}: no VRR log line is added"
        );

        let mut baseline = ExclusionFixture::new(
            present_id,
            mode,
            "amdgpu",
            None,
            None,
            &[],
            variable_refresh,
            capability,
            None,
        );
        baseline.qualify();
        assert_eq!(
            fixture.signatures(),
            baseline.signatures(),
            "{label}: commits retain the baseline request signatures"
        );
        fixture.finish();
        baseline.finish();
    }
}
