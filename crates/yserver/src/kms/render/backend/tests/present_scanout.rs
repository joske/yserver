use super::*;

fn output_crtc_key(backend: &KmsBackend, output_idx: usize) -> CrtcKey {
    CrtcKey::for_output(&backend.platform.outputs[output_idx])
}

pub(super) fn bind_test_randr_crtc(backend: &mut KmsBackend, output_idx: usize, crtc_id: u32) {
    let output_key = backend.platform.outputs[output_idx].key.clone();
    backend.pstate.crtc_key_by_id.insert(crtc_id, output_key);
    backend.refresh_present_crtc_clock_epochs();
}

fn test_crtc_key(device_key: DrmDeviceKey, crtc_id: u32) -> CrtcKey {
    CrtcKey::new(
        device_key,
        ::drm::control::from_u32(crtc_id).expect("nonzero test CRTC"),
    )
}

fn dispatch_test_sequence(backend: &mut KmsBackend, user_data: u64, time_ns: i64, sequence: u64) {
    let device_key = backend
        .platform
        .primary_device()
        .expect("test fixture has a DRM device")
        .key;
    backend.on_crtc_sequence_event(device_key, user_data, time_ns, sequence);
}

fn shared_wake_test_backend() -> (
    KmsBackend,
    CrtcKey,
    u32,
    std::rc::Rc<crate::kms::render::platform::flip_events::RecordingPageFlipEventSource>,
) {
    let mut backend = KmsBackend::for_tests();
    let key = output_crtc_key(&backend, 0);
    let randr_crtc_id = 0x5f01;
    bind_test_randr_crtc(&mut backend, 0, randr_crtc_id);
    backend.platform.vblank_fallback.pipes.insert(key, 0);
    let source = backend.platform.devices[0]
        .test_page_flip_event_source
        .clone()
        .expect("fixture records kernel events");
    source.set_legacy_vblank(Ok(5));
    (backend, key, randr_crtc_id, source)
}

fn drain_shared_wake_test_events(backend: &mut KmsBackend, state: &mut ServerState) {
    use std::os::fd::{AsFd as _, AsRawFd as _};

    let fd = backend.platform.devices[0].device.as_fd().as_raw_fd();
    Backend::on_page_flip_ready(backend, state, fd);
}

// ── Idle vblank arming (DRM_CRTC_QUEUE_SEQUENCE) ───────────────────────

#[test]
fn armed_vblank_targets_starts_empty() {
    let b = crate::kms::render::backend::KmsBackend::for_tests();
    assert!(b.platform.vblank_fallback.kernel_arm_watch.is_empty());
    assert!(b.pstate.crtc_queue_sequence_unsupported_devices.is_empty());
}

#[test]
fn clear_all_armed_vblank_targets_empties_map() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let h = test_crtc_key(test_device_key(7), 7);
    super::seed_vblank_arm_watch(&mut b, h, super::IDLE_VBLANK_ARM_SLOT);
    super::seed_vblank_arm_watch(&mut b, h, super::SHARED_WAKE_VBLANK_ARM_SLOT);
    b.clear_all_armed_vblank_targets();
    assert!(b.platform.vblank_fallback.kernel_arm_watch.is_empty());
}

#[test]
fn prune_armed_targets_drops_stale_keeps_live() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let live = output_crtc_key(&b, 0);
    let stale = test_crtc_key(live.device_key, 999);
    super::seed_vblank_arm_watch(&mut b, live, super::IDLE_VBLANK_ARM_SLOT);
    super::seed_vblank_arm_watch(&mut b, stale, super::IDLE_VBLANK_ARM_SLOT);
    b.prune_armed_targets_to_live_outputs();
    assert!(super::vblank_arm_is_watched(
        &b,
        live,
        super::IDLE_VBLANK_ARM_SLOT
    ));
    assert!(!super::vblank_arm_is_watched(
        &b,
        stale,
        super::IDLE_VBLANK_ARM_SLOT
    ));
}

#[test]
fn prune_armed_targets_drops_stale_shared_wakes_keeps_live() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let live = output_crtc_key(&b, 0);
    let stale = test_crtc_key(live.device_key, 999);
    super::seed_vblank_arm_watch(&mut b, live, super::SHARED_WAKE_VBLANK_ARM_SLOT);
    super::seed_vblank_arm_watch(&mut b, stale, super::SHARED_WAKE_VBLANK_ARM_SLOT);
    b.prune_armed_targets_to_live_outputs();
    assert!(super::vblank_arm_is_watched(
        &b,
        live,
        super::SHARED_WAKE_VBLANK_ARM_SLOT
    ));
    assert!(!super::vblank_arm_is_watched(
        &b,
        stale,
        super::SHARED_WAKE_VBLANK_ARM_SLOT
    ));
}

#[test]
fn on_crtc_sequence_event_happy_path_records_msc_and_clears_arm() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let crtc = b.platform.outputs[0].output.crtc;
    let crtc_key = output_crtc_key(&b, 0);
    super::seed_vblank_arm_watch(&mut b, crtc_key, super::IDLE_VBLANK_ARM_SLOT);

    dispatch_test_sequence(
        &mut b,
        u64::from(u32::from(crtc)),
        1_500_000, /* 1.5ms */
        7,
    );

    assert!(
        b.platform.vblank_fallback.kernel_arm_watch.is_empty(),
        "arm must be cleared"
    );
    // ust_msc stores microseconds; primary output is index 0.
    assert_eq!(b.platform.present_get_ust_msc(crtc_key), (7, 1_500));
    assert_eq!(
        b.platform.present_get_completion_clock(crtc_key),
        yserver_core::backend::PresentClockSample {
            msc: 7,
            ust: 1_500,
            source: yserver_core::backend::PresentClockSource::IdleSequence,
        },
        "an idle sequence is eligible to release completions"
    );
}

#[test]
fn active_crtc_sequence_advances_general_clock_only() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let crtc = b.platform.outputs[0].output.crtc;
    let crtc_key = output_crtc_key(&b, 0);
    super::seed_vblank_arm_watch(&mut b, crtc_key, super::IDLE_VBLANK_ARM_SLOT);
    b.scene.scene_structure_dirty = true;

    dispatch_test_sequence(&mut b, u64::from(u32::from(crtc)), 2_000_000, 9);

    assert_eq!(b.platform.present_get_ust_msc(crtc_key), (9, 2_000));
    assert_eq!(
        b.platform.present_get_completion_clock(crtc_key).msc,
        0,
        "active standalone sequence must not release Pixmap completions"
    );
}

#[test]
fn active_tagged_shared_wake_advances_completion_clock() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let crtc = b.platform.outputs[0].output.crtc;
    let crtc_id = u32::from(crtc);
    let crtc_key = output_crtc_key(&b, 0);
    super::seed_vblank_arm_watch(&mut b, crtc_key, super::SHARED_WAKE_VBLANK_ARM_SLOT);
    b.scene.scene_structure_dirty = true;

    dispatch_test_sequence(
        &mut b,
        crate::kms::render::backend::absolute_seq_user_data(crtc_id),
        2_000_000,
        9,
    );

    assert_eq!(b.platform.present_get_ust_msc(crtc_key), (9, 2_000));
    assert_eq!(
        b.platform.present_get_completion_clock(crtc_key).msc,
        9,
        "tagged shared wake must release due completions",
    );
}

#[test]
fn completion_clock_prefers_pageflip_at_equal_msc_and_never_regresses() {
    use yserver_core::backend::{PresentClockSample, PresentClockSource};

    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let crtc_key = output_crtc_key(&b, 0);
    b.platform.record_completion_clock(
        crtc_key,
        PresentClockSample {
            msc: 20,
            ust: 2_000,
            source: PresentClockSource::IdleSequence,
        },
        None,
    );
    b.platform.record_completion_clock(
        crtc_key,
        PresentClockSample {
            msc: 20,
            ust: 2_001,
            source: PresentClockSource::PageFlip,
        },
        None,
    );
    b.platform.record_completion_clock(
        crtc_key,
        PresentClockSample {
            msc: 19,
            ust: 1_900,
            source: PresentClockSource::IdleSequence,
        },
        None,
    );

    assert_eq!(
        b.platform.present_get_completion_clock(crtc_key),
        PresentClockSample {
            msc: 20,
            ust: 2_001,
            source: PresentClockSource::PageFlip,
        }
    );
}

#[test]
fn on_crtc_sequence_event_negative_time_clears_arm_and_drops() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let crtc = b.platform.outputs[0].output.crtc;
    let crtc_key = output_crtc_key(&b, 0);
    super::seed_vblank_arm_watch(&mut b, crtc_key, super::IDLE_VBLANK_ARM_SLOT);

    dispatch_test_sequence(&mut b, u64::from(u32::from(crtc)), -1, 7);

    assert!(
        b.platform.vblank_fallback.kernel_arm_watch.is_empty(),
        "arm cleared even on drop"
    );
    assert_eq!(
        b.platform.present_get_ust_msc(crtc_key),
        (0, 0),
        "negative time_ns must not advance the clock"
    );
}

#[test]
fn on_crtc_sequence_event_stale_crtc_clears_arm_and_drops() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let device_key = b.platform.outputs[0].key.device_key;
    let stale = test_crtc_key(device_key, 999);
    super::seed_vblank_arm_watch(&mut b, stale, super::IDLE_VBLANK_ARM_SLOT);

    dispatch_test_sequence(&mut b, 999, 1_000_000, 5);

    assert!(
        b.platform.vblank_fallback.kernel_arm_watch.is_empty(),
        "stale arm cleared so the CRTC can't strand"
    );
    assert_eq!(b.platform.present_get_ust_msc(stale), (0, 0));
}

#[test]
fn on_crtc_sequence_event_from_wrong_device_does_not_advance_clock() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let crtc = b.platform.outputs[0].output.crtc;
    let crtc_key = output_crtc_key(&b, 0);
    super::seed_vblank_arm_watch(&mut b, crtc_key, super::IDLE_VBLANK_ARM_SLOT);
    let wrong_device = test_device_key(99);

    b.on_crtc_sequence_event(wrong_device, u64::from(u32::from(crtc)), 1_000_000, 5);

    assert!(
        super::vblank_arm_is_watched(&b, crtc_key, super::IDLE_VBLANK_ARM_SLOT),
        "an equal raw CRTC on another DRM device must not spend this device's arm"
    );
    assert_eq!(
        b.platform.present_get_ust_msc(crtc_key),
        (0, 0),
        "an equal CRTC handle from another DRM device must not update this output"
    );
}

#[test]
fn same_raw_crtc_on_two_devices_keeps_present_state_independent() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let primary = output_crtc_key(&b, 0);
    let raw_crtc = u32::from(primary.crtc);
    push_test_output(&mut b, raw_crtc);
    b.platform.outputs[1].key.device_key = test_device_key(99);
    let secondary = output_crtc_key(&b, 1);
    assert_eq!(primary.crtc, secondary.crtc);
    assert_ne!(primary.device_key, secondary.device_key);

    super::seed_vblank_arm_watch(&mut b, primary, super::IDLE_VBLANK_ARM_SLOT);
    super::seed_vblank_arm_watch(&mut b, secondary, super::IDLE_VBLANK_ARM_SLOT);
    b.on_crtc_sequence_event(secondary.device_key, u64::from(raw_crtc), 3_000_000, 17);

    assert!(
        super::vblank_arm_is_watched(&b, primary, super::IDLE_VBLANK_ARM_SLOT),
        "the event must not retire the equal raw CRTC on the primary card"
    );
    assert!(
        !super::vblank_arm_is_watched(&b, secondary, super::IDLE_VBLANK_ARM_SLOT),
        "the event retires only the device-qualified secondary arm"
    );
    assert_eq!(b.platform.ust_msc.get(&secondary), Some(&(17, 3_000)));
    assert!(!b.platform.ust_msc.contains_key(&primary));
}

#[test]
fn present_get_ust_msc_keeps_output_domains_independent() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let primary = output_crtc_key(&b, 0);
    push_test_output(&mut b, 2);
    let secondary = output_crtc_key(&b, 1);
    b.platform.ust_msc.insert(primary, (10, 100));
    b.platform.ust_msc.insert(secondary, (42, 424));
    assert_eq!(
        b.platform.present_get_ust_msc(primary),
        (10, 100),
        "a faster secondary must not advance the primary CRTC's counter"
    );
    assert_eq!(
        b.platform.present_get_ust_msc(secondary),
        (42, 424),
        "the secondary reads only its own (msc, ust)"
    );
}

#[test]
fn present_randr_crtc_routes_equal_raw_handles_to_the_owning_device() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let raw_crtc = u32::from(b.platform.outputs[0].output.crtc);
    let primary_xid = 0x5100;
    bind_test_randr_crtc(&mut b, 0, primary_xid);

    push_test_output(&mut b, raw_crtc);
    let secondary_device = test_device_key(99);
    b.platform.outputs[1].key.device_key = secondary_device;
    push_test_device(&mut b, secondary_device);
    let secondary_xid = 0x5101;
    bind_test_randr_crtc(&mut b, 1, secondary_xid);

    let primary = output_crtc_key(&b, 0);
    let secondary = output_crtc_key(&b, 1);
    assert_eq!(primary.crtc, secondary.crtc);
    assert_ne!(primary.device_key, secondary.device_key);
    assert_eq!(b.present_crtc_key(primary_xid), Some(primary));
    assert_eq!(b.present_crtc_key(secondary_xid), Some(secondary));

    b.platform.ust_msc.insert(primary, (11, 1_100));
    b.platform.ust_msc.insert(secondary, (77, 7_700));
    assert_eq!(b.present_get_ust_msc(primary_xid), (11, 1_100));
    assert_eq!(b.present_get_ust_msc(secondary_xid), (77, 7_700));

    let mut armed_on = Vec::new();
    b.arm_idle_vblanks_with(secondary, &[78], |key| {
        armed_on.push(key);
        Ok(true)
    })
    .expect("arm selected secondary domain");
    assert_eq!(armed_on, vec![secondary]);
    assert!(!super::vblank_arm_is_watched(
        &b,
        primary,
        super::IDLE_VBLANK_ARM_SLOT
    ));
    assert!(super::vblank_arm_is_watched(
        &b,
        secondary,
        super::IDLE_VBLANK_ARM_SLOT
    ));

    b.pstate
        .crtc_queue_sequence_unsupported_devices
        .insert(primary.device_key);
    assert!(
        b.present_absolute_vblank_arm_supported(primary_xid),
        "WAIT_VBLANK still supports tagged shared wakes"
    );
    b.platform
        .vblank_fallback
        .wait_vblank_unsupported_devices
        .insert(primary.device_key);
    assert!(!b.present_absolute_vblank_arm_supported(primary_xid));
    assert!(b.present_absolute_vblank_arm_supported(secondary_xid));
    assert!(
        !b.direct_present_crtc_eligible(
            crate::kms::render::backend::DirectScope::Root,
            secondary_xid,
            b.present_crtc_clock_epoch(secondary_xid),
        ),
        "a secondary-card CRTC cannot own the primary-device grouped framebuffer"
    );
}

#[test]
fn present_clock_epoch_changes_on_route_removal_and_readd() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let crtc_id = 0x5200;
    bind_test_randr_crtc(&mut b, 0, crtc_id);
    let first = b.present_crtc_clock_epoch(crtc_id);
    assert_ne!(first, 0);
    assert!(b.direct_present_crtc_eligible(
        crate::kms::render::backend::DirectScope::Root,
        crtc_id,
        first
    ));

    b.refresh_present_crtc_clock_epochs();
    assert_eq!(b.present_crtc_clock_epoch(crtc_id), first);

    let output = b.platform.outputs.pop().expect("fixture output");
    b.pstate.direct.sync_output_count(b.platform.outputs.len());
    b.refresh_present_crtc_clock_epochs();
    assert_eq!(b.present_crtc_clock_epoch(crtc_id), 0);

    b.platform.outputs.push(output);
    b.pstate.direct.sync_output_count(b.platform.outputs.len());
    b.refresh_present_crtc_clock_epochs();
    let second = b.present_crtc_clock_epoch(crtc_id);
    assert_ne!(second, 0);
    assert_ne!(second, first, "off-to-on starts a new MSC epoch");
    assert!(
        !b.direct_present_crtc_eligible(
            crate::kms::render::backend::DirectScope::Root,
            crtc_id,
            first
        ),
        "a candidate captured in the old domain cannot enter direct scanout"
    );
}

#[test]
fn off_randr_crtc_has_no_present_domain_or_direct_path() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let crtc_id = 0x5300;
    b.pstate
        .crtc_key_by_id
        .insert(crtc_id, test_output_key(0, "not-live"));
    b.refresh_present_crtc_clock_epochs();

    assert_eq!(b.present_crtc_clock_epoch(crtc_id), 0);
    assert_eq!(b.present_get_ust_msc(crtc_id), (0, 0));
    assert!(!b.present_flip_in_flight(crtc_id));
    assert!(!b.present_absolute_vblank_arm_supported(crtc_id));
    assert!(!b.direct_present_crtc_eligible(
        crate::kms::render::backend::DirectScope::Root,
        crtc_id,
        0
    ));
}

/// With DPMS off the CRTCs are powered down and CRTC_QUEUE_SEQUENCE
/// fails with EINVAL; arming anyway retried on every loop iteration
/// (~160 `PRESENT-DBG ... ERR Invalid argument` warnings/s on bee).
/// Xorg arms no kernel vblank for a blanked screen either (modesetting's
/// get_crtc finds no active CRTC, so Present falls back to its fake
/// clock). Neither arm may issue the ioctl while the outputs are off.
#[test]
fn vblank_arms_skip_the_ioctl_while_outputs_are_off() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let primary = output_crtc_key(&b, 0);
    b.kms_outputs_active = false;
    let mut calls = 0u32;
    let armed = b
        .arm_idle_vblanks_with(primary, &[100], |_| {
            calls += 1;
            Ok(true)
        })
        .unwrap();
    let shared = b
        .arm_present_shared_wake_with(primary, |_| {
            calls += 1;
            Ok(true)
        })
        .unwrap();
    assert_eq!(
        (armed, shared, calls),
        (0, false, 0),
        "no ioctl with the outputs off"
    );

    // Outputs back on: arming works again.
    b.kms_outputs_active = true;
    let armed = b
        .arm_idle_vblanks_with(primary, &[100], |_| {
            calls += 1;
            Ok(true)
        })
        .unwrap();
    assert_eq!((armed, calls), (1, 1));
}

#[test]
fn arm_idle_vblanks_with_empty_targets_is_noop() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let primary = output_crtc_key(&b, 0);
    let mut calls = 0u32;
    let armed = b
        .arm_idle_vblanks_with(primary, &[], |_| {
            calls += 1;
            Ok(true)
        })
        .unwrap();
    assert_eq!(armed, 0);
    assert_eq!(calls, 0);
    assert!(b.platform.vblank_fallback.kernel_arm_watch.is_empty());
}

#[test]
fn arm_idle_vblanks_with_arms_primary_once_then_dedups() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let primary = output_crtc_key(&b, 0);
    let mut calls = 0u32;

    let armed = b
        .arm_idle_vblanks_with(primary, &[100], |_| {
            calls += 1;
            Ok(true)
        })
        .unwrap();
    assert_eq!(armed, 1);
    assert_eq!(calls, 1);
    assert!(super::vblank_arm_is_watched(
        &b,
        primary,
        super::IDLE_VBLANK_ARM_SLOT
    ));

    // Already armed → no second ioctl while the sequence is in flight.
    let armed2 = b
        .arm_idle_vblanks_with(primary, &[200], |_| {
            calls += 1;
            Ok(true)
        })
        .unwrap();
    assert_eq!(armed2, 0);
    assert_eq!(calls, 1, "no re-arm while a sequence is queued");
}

#[test]
fn arm_idle_vblanks_without_kernel_vblank_arms_only_a_software_vblank() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let primary = output_crtc_key(&b, 0);
    push_test_output(&mut b, 2);
    b.platform.outputs[1].key.device_key = test_device_key(1);
    let secondary = output_crtc_key(&b, 1);
    b.pstate
        .crtc_queue_sequence_unsupported_devices
        .insert(primary.device_key);
    b.platform
        .vblank_fallback
        .wait_vblank_unsupported_devices
        .insert(primary.device_key);

    let mut attempted = Vec::new();
    let armed = b
        .arm_idle_vblanks_with(primary, &[100], |crtc_key| {
            attempted.push(crtc_key);
            Ok(true)
        })
        .unwrap();

    assert_eq!(armed, 1, "the software vblank covers the wait");
    assert!(attempted.is_empty(), "no kernel arm on that device");
    assert!(!super::vblank_arm_is_watched(
        &b,
        primary,
        super::IDLE_VBLANK_ARM_SLOT
    ));
    assert!(b.platform.vblank_fallback.software.contains_key(&primary));
    assert!(!b.platform.vblank_fallback.software.contains_key(&secondary));

    let secondary_armed = b
        .arm_idle_vblanks_with(secondary, &[100], |crtc_key| {
            attempted.push(crtc_key);
            Ok(true)
        })
        .unwrap();
    assert_eq!(secondary_armed, 1);
    assert_eq!(attempted, vec![secondary]);
    assert!(super::vblank_arm_is_watched(
        &b,
        secondary,
        super::IDLE_VBLANK_ARM_SLOT
    ));
}

#[test]
fn arm_idle_vblanks_transient_error_does_not_cross_domains() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let primary = output_crtc_key(&b, 0);
    push_test_output(&mut b, 2);
    b.platform.outputs[1].key.device_key = test_device_key(1);
    let secondary = output_crtc_key(&b, 1);

    let mut attempted = Vec::new();
    let error = b
        .arm_idle_vblanks_with(primary, &[100], |crtc_key| {
            attempted.push(crtc_key);
            Err(std::io::Error::from_raw_os_error(libc::EINVAL))
        })
        .expect_err("the selected domain's transient error is reported");

    assert_eq!(error.raw_os_error(), Some(libc::EINVAL));
    assert_eq!(attempted, vec![primary]);
    assert!(!super::vblank_arm_is_watched(
        &b,
        primary,
        super::IDLE_VBLANK_ARM_SLOT
    ));
    assert!(!super::vblank_arm_is_watched(
        &b,
        secondary,
        super::IDLE_VBLANK_ARM_SLOT
    ));
}

#[test]
fn arm_idle_vblanks_with_scanout_disallowed_clears_and_returns_zero() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let primary = output_crtc_key(&b, 0);
    super::seed_vblank_arm_watch(&mut b, primary, super::IDLE_VBLANK_ARM_SLOT);
    b.pstate.vt_state = crate::vt::state::VtState::Suspended;

    let mut calls = 0u32;
    let armed = b
        .arm_idle_vblanks_with(primary, &[100], |_| {
            calls += 1;
            Ok(true)
        })
        .unwrap();
    assert_eq!(armed, 0);
    assert_eq!(calls, 0, "no arming without DRM master");
    assert!(
        b.platform.vblank_fallback.kernel_arm_watch.is_empty(),
        "master loss drops queued sequences → clear bookkeeping"
    );
}

// ── Tagged shared Present wake ─────────────────────────────────────────

#[test]
fn shared_wake_slot_starts_empty() {
    let b = crate::kms::render::backend::KmsBackend::for_tests();
    assert!(b.platform.vblank_fallback.kernel_arm_watch.is_empty());
}

#[test]
fn tagged_sequence_event_does_not_clear_relative_slot() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let crtc = b.platform.outputs[0].output.crtc;
    let crtc_id = u32::from(crtc);
    let crtc_key = output_crtc_key(&b, 0);
    super::seed_vblank_arm_watch(&mut b, crtc_key, super::IDLE_VBLANK_ARM_SLOT);
    super::seed_vblank_arm_watch(&mut b, crtc_key, super::SHARED_WAKE_VBLANK_ARM_SLOT);

    // Tagged (absolute) event for the same CRTC.
    dispatch_test_sequence(
        &mut b,
        crate::kms::render::backend::ABSOLUTE_SEQ_TAG | u64::from(crtc_id),
        1_000_000,
        50,
    );

    assert!(
        super::vblank_arm_is_watched(&b, crtc_key, super::IDLE_VBLANK_ARM_SLOT),
        "a tagged event must not spend the untagged relative-1 slot"
    );
    assert!(
        !super::vblank_arm_is_watched(&b, crtc_key, super::SHARED_WAKE_VBLANK_ARM_SLOT),
        "the tagged event spends its own shared-wake slot"
    );
}

#[test]
fn untagged_sequence_event_does_not_clear_shared_wake() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let crtc = b.platform.outputs[0].output.crtc;
    let crtc_id = u32::from(crtc);
    let crtc_key = output_crtc_key(&b, 0);
    super::seed_vblank_arm_watch(&mut b, crtc_key, super::IDLE_VBLANK_ARM_SLOT);
    super::seed_vblank_arm_watch(&mut b, crtc_key, super::SHARED_WAKE_VBLANK_ARM_SLOT);

    // Untagged (relative) event for the same CRTC.
    dispatch_test_sequence(&mut b, u64::from(crtc_id), 1_000_000, 50);

    assert!(
        super::vblank_arm_is_watched(&b, crtc_key, super::SHARED_WAKE_VBLANK_ARM_SLOT),
        "an untagged event spends only the relative idle slot"
    );
}

#[test]
fn shared_wake_tagged_event_advances_completion_clock_and_clears_slot() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let crtc = b.platform.outputs[0].output.crtc;
    let crtc_id = u32::from(crtc);
    let crtc_key = output_crtc_key(&b, 0);
    super::seed_vblank_arm_watch(&mut b, crtc_key, super::IDLE_VBLANK_ARM_SLOT);
    super::seed_vblank_arm_watch(&mut b, crtc_key, super::SHARED_WAKE_VBLANK_ARM_SLOT);

    // Uses the tagged user data that the shared wake feeds to the ioctl.
    dispatch_test_sequence(
        &mut b,
        crate::kms::render::backend::absolute_seq_user_data(crtc_id),
        1_000_000,
        50,
    );

    assert!(
        !super::vblank_arm_is_watched(&b, crtc_key, super::SHARED_WAKE_VBLANK_ARM_SLOT),
        "the shared slot must clear through the tagged event"
    );
    assert!(
        super::vblank_arm_is_watched(&b, crtc_key, super::IDLE_VBLANK_ARM_SLOT),
        "and must not touch the relative slot"
    );
}

#[test]
fn clear_all_armed_vblank_targets_clears_shared_wake_too() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let crtc_key = output_crtc_key(&b, 0);
    super::seed_vblank_arm_watch(&mut b, crtc_key, super::IDLE_VBLANK_ARM_SLOT);
    super::seed_vblank_arm_watch(&mut b, crtc_key, super::SHARED_WAKE_VBLANK_ARM_SLOT);

    b.clear_all_armed_vblank_targets();

    assert!(b.platform.vblank_fallback.kernel_arm_watch.is_empty());
}

#[test]
fn arm_present_shared_wake_with_dedups_one_kernel_call() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let crtc_key = output_crtc_key(&b, 0);
    let mut calls = 0u32;

    let covered = b
        .arm_present_shared_wake_with(crtc_key, |_| {
            calls += 1;
            Ok(true)
        })
        .unwrap();
    assert!(covered);
    assert_eq!(calls, 1);

    // An existing shared wake covers every future target on this CRTC.
    let covered2 = b
        .arm_present_shared_wake_with(crtc_key, |_| {
            calls += 1;
            Ok(true)
        })
        .unwrap();
    assert!(covered2, "an existing wake remains covered");
    assert_eq!(calls, 1, "the slot prevents a second kernel arm");
}

#[test]
fn arm_present_shared_wake_false_result_is_not_tracked_as_covered() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let crtc_key = output_crtc_key(&b, 0);
    let mut calls = 0;

    let armed = b
        .arm_present_shared_wake_with(crtc_key, |_| {
            calls += 1;
            Ok(false)
        })
        .unwrap();

    assert_eq!(calls, 1);
    assert!(!armed);
    assert!(b.platform.vblank_fallback.kernel_arm_watch.is_empty());
}

// Mutation: restore per-target arms or omit Backend's cursor-only flip submission.
#[test]
fn shared_wake_outstanding_arms_stay_bounded() {
    let (mut backend, crtc_key, randr_crtc_id, source) = shared_wake_test_backend();
    let mut state = ServerState::new();
    const DESTROYED_WINDOW: u32 = 0x5f10;
    let destroyed_window_id = seed_window(&mut backend, DESTROYED_WINDOW, None, 0, 0);
    let crtc = backend.platform.outputs[0].output.crtc;
    let recorder = atomic_cursor_recording_sink(&mut backend);
    let position = backend.platform.for_tests_seed_atomic_cursor_position(
        0,
        atomic_cursor_raw(0x9501),
        atomic_cursor_raw(0x9502),
        atomic_cursor_raw(0x9503),
        40,
        50,
        0,
        0,
    );
    backend.scene.scene_structure_dirty = false;
    backend.scene.test_set_flip_in_flight(false);
    for drawable in backend.store.armed_damaged_ids() {
        if let Some(snapshot) = backend.store.peek_presentation_damage(drawable) {
            backend.store.ack_presentation_damage(snapshot);
        }
    }
    let mut issued = 0usize;
    let mut delivered = 0usize;
    let mut max_outstanding = 0usize;
    let mut flip_submitted = false;

    for (iteration, target) in (1_u64..=140).rev().enumerate() {
        Backend::arm_idle_vblanks(&mut backend, randr_crtc_id, &[target])
            .expect("relative idle arm");
        assert!(
            Backend::arm_present_shared_wake(&mut backend, randr_crtc_id)
                .expect("tagged shared wake")
        );
        let outstanding_arms = source.legacy_vblank_arms().len();
        issued += outstanding_arms.saturating_sub(issued - delivered);
        let outstanding = issued - delivered;
        max_outstanding = max_outstanding.max(outstanding);
        assert!(
            outstanding <= 2,
            "outstanding tagged + idle arms: {outstanding}"
        );

        if !flip_submitted {
            Backend::maybe_composite(&mut backend)
                .expect("submit cursor-only flip while the shared wake is outstanding");
            let record = recorder
                .records()
                .into_iter()
                .find(|record| record.kind == crate::drm::atomic::AtomicCommitKind::CursorOnly)
                .expect("Backend submitted a cursor-only atomic flip");
            assert_eq!(
                record.result,
                crate::drm::atomic::tests_support::ScriptedResult::Ok
            );
            assert!(
                record
                    .flags
                    .contains(::drm::control::AtomicCommitFlags::PAGE_FLIP_EVENT)
            );
            assert!(record.events.iter().any(|event| event.crtc == crtc));
            assert!(
                backend
                    .platform
                    .for_tests_atomic_cursor_commit_is_in_flight(
                        position.device_key,
                        position.crtc
                    ),
                "the accepted page-flip commit is in flight with the Present wake armed"
            );

            // Deliver the driver event only after Backend has submitted the
            // commit above; this retires that actual flip through the normal
            // page-flip event path.
            let frame = 10;
            source.queue_flip(crtc, frame);
            drain_shared_wake_test_events(&mut backend, &mut state);
            assert_eq!(
                backend.platform.present_get_ust_msc(crtc_key).0,
                u64::from(frame)
            );
            assert!(
                !backend
                    .platform
                    .for_tests_atomic_cursor_commit_is_in_flight(
                        position.device_key,
                        position.crtc
                    ),
            );
            flip_submitted = true;
        }

        if (iteration + 1) % 64 == 0 {
            let retiring = source.legacy_vblank_arms().len();
            let sequence = u32::try_from(iteration + 11).unwrap();
            source.fire_legacy_vblank_arms(2_000_000 + i64::from(sequence), sequence);
            delivered += retiring;
            drain_shared_wake_test_events(&mut backend, &mut state);
        }
    }

    // Destroy the real backend window while the final shared wake remains
    // queued. Core-side Present cancellation with a parked target is covered
    // through the DestroyWindow request in the yserver-core regression.
    Backend::destroy_subwindow(&mut backend, None, DESTROYED_WINDOW)
        .expect("destroy the backend window through Backend");
    assert!(!backend.windows.contains_key(&DESTROYED_WINDOW));
    assert!(backend.store.get(destroyed_window_id).is_none());
    let retiring = source.legacy_vblank_arms().len();
    source.fire_legacy_vblank_arms(4_000_000, 200);
    delivered += retiring;
    drain_shared_wake_test_events(&mut backend, &mut state);

    assert_eq!(max_outstanding, 2);
    assert_eq!(issued - delivered, 0);
    assert!(flip_submitted);
    assert!(backend.platform.vblank_fallback.kernel_arm_watch.is_empty());
}

// Mutation: never clear the tagged slot when its sequence event is delivered.
#[test]
fn shared_wake_rearms_only_after_its_event() {
    let (mut backend, crtc_key, randr_crtc_id, source) = shared_wake_test_backend();
    let mut state = ServerState::new();

    assert!(Backend::arm_present_shared_wake(&mut backend, randr_crtc_id).unwrap());
    assert!(Backend::arm_present_shared_wake(&mut backend, randr_crtc_id).unwrap());
    assert_eq!(source.legacy_vblank_arms().len(), 1);
    assert!(super::vblank_arm_is_watched(
        &backend,
        crtc_key,
        super::SHARED_WAKE_VBLANK_ARM_SLOT
    ));

    source.fire_legacy_vblank_arms(1_000_000, 6);
    drain_shared_wake_test_events(&mut backend, &mut state);
    assert!(!super::vblank_arm_is_watched(
        &backend,
        crtc_key,
        super::SHARED_WAKE_VBLANK_ARM_SLOT
    ));

    assert!(Backend::arm_present_shared_wake(&mut backend, randr_crtc_id).unwrap());
    assert_eq!(source.legacy_vblank_arms().len(), 1);
    assert!(super::vblank_arm_is_watched(
        &backend,
        crtc_key,
        super::SHARED_WAKE_VBLANK_ARM_SLOT
    ));
}

// Mutation: omit the tagged slot clear on DPMS or VT release.
#[test]
fn shared_wake_cleared_by_lifecycle_edges() {
    use yserver_core::{
        backend::Backend, core_loop::input_inventory::InputInventory, server::ServerState,
    };

    let (mut dpms, dpms_crtc, randr_crtc_id, source) = shared_wake_test_backend();
    dpms.platform.for_tests_accept_output_power_commits();
    super::seed_vblank_arm_watch(&mut dpms, dpms_crtc, super::SHARED_WAKE_VBLANK_ARM_SLOT);
    Backend::set_dpms_power(&mut dpms, 3).expect("DPMS off");
    assert!(dpms.platform.vblank_fallback.kernel_arm_watch.is_empty());
    // The fd-less fixture has no framebuffer for DPMS-on; restore its active
    // state to exercise the next production arm call after the off edge.
    dpms.kms_outputs_active = true;
    assert!(Backend::arm_present_shared_wake(&mut dpms, randr_crtc_id).unwrap());
    assert_eq!(source.legacy_vblank_arms().len(), 1);

    let mut vt = KmsBackend::for_tests();
    let vt_crtc = output_crtc_key(&vt, 0);
    super::seed_vblank_arm_watch(&mut vt, vt_crtc, super::SHARED_WAKE_VBLANK_ARM_SLOT);
    let mut state = ServerState::new();
    Backend::begin_vt_release(&mut vt);
    Backend::finish_vt_release(&mut vt, &mut state, &InputInventory::new());
    assert!(vt.platform.vblank_fallback.kernel_arm_watch.is_empty());
}

#[test]
fn destroy_subwindow_without_direct_frame_does_not_request_unflip() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let target_xid = 0x5600;
    let target_id = seed_window(&mut b, target_xid, None, 0, 0);

    b.destroy_subwindow(None, target_xid)
        .expect("destroy inactive target");

    assert!(!b.pstate.direct.root().active());
    assert!(!b.pstate.direct.root().unflip_requested);
    assert!(!b.pstate.direct.root().hold_direct);
    assert!(!b.windows.contains_key(&target_xid));
    assert!(b.store.get(target_id).is_none());
}

#[test]
fn destroy_unrelated_subwindow_keeps_direct_scanout_active() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let target_xid = 0x5680;
    let unrelated_xid = 0x5690;
    let _target_id = seed_window(&mut b, target_xid, None, 0, 0);
    let unrelated_id = seed_window(&mut b, unrelated_xid, None, 0, 0);
    b.get_overlay_window(None).expect("materialize COW");
    let cow_id = b.cow_id.expect("COW id");
    let (source_id, _, source_pin, cow_pin) =
        install_direct_frame_for_target_test(&mut b, target_xid, cow_id, true);

    b.destroy_subwindow(None, unrelated_xid)
        .expect("destroy unrelated window");

    assert!(!b.windows.contains_key(&unrelated_xid));
    assert!(b.store.get(unrelated_id).is_none());
    assert!(!b.pstate.direct.root().unflip_requested);
    assert!(b.pstate.direct.root().hold_direct);
    assert_eq!(b.present_source_pins.get(&source_pin), Some(&source_id));
    assert_eq!(b.present_source_pins.get(&cow_pin), Some(&cow_id));
}

#[test]
fn destroy_subwindow_requests_pending_direct_unflip_without_releasing_pins() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let target_xid = 0x5700;
    let target_id = seed_window(&mut b, target_xid, None, 0, 0);
    b.get_overlay_window(None).expect("materialize COW");
    let cow_id = b.cow_id.expect("COW id");
    let (source_id, _, source_pin, cow_pin) =
        install_direct_frame_for_target_test(&mut b, target_xid, cow_id, false);

    b.destroy_subwindow(None, target_xid)
        .expect("destroy pending direct target");

    assert!(b.pstate.direct.root().unflip_requested);
    assert!(!b.pstate.direct.root().hold_direct);
    assert!(b.pstate.direct.root().pending.is_some());
    assert!(b.pstate.direct.root().current.is_none());
    assert!(b.pstate.direct.root().completed.is_empty());
    assert!(b.pstate.direct.root().idled.is_empty());
    assert_eq!(b.present_source_pins.get(&source_pin), Some(&source_id));
    assert_eq!(b.present_source_pins.get(&cow_pin), Some(&cow_id));
    assert_eq!(b.store.get(source_id).map(|d| d.refcount), Some(2));
    assert!(
        b.store.get(target_id).is_none(),
        "the destroyed destination leaf has no direct lifetime pin"
    );
    assert_eq!(
        b.store.get(cow_id).map(|d| d.refcount),
        Some(2),
        "the separately pinned COW fallback survives until replacement"
    );
}

#[test]
fn destroy_subwindow_retires_current_direct_idle_once_after_replacement() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let target_xid = 0x5800;
    let target_id = seed_window(&mut b, target_xid, None, 0, 0);
    b.get_overlay_window(None).expect("materialize COW");
    let cow_id = b.cow_id.expect("COW id");
    let (source_id, _, source_pin, cow_pin) =
        install_direct_frame_for_target_test(&mut b, target_xid, cow_id, true);

    b.destroy_subwindow(None, target_xid)
        .expect("destroy current direct target");

    assert!(b.pstate.direct.root().unflip_requested);
    assert!(!b.pstate.direct.root().hold_direct);
    assert!(b.pstate.direct.root().current.is_some());
    assert!(b.pstate.direct.root().idled.is_empty());
    assert_eq!(b.present_source_pins.get(&source_pin), Some(&source_id));
    assert_eq!(b.present_source_pins.get(&cow_pin), Some(&cow_id));
    assert_eq!(b.store.get(source_id).map(|d| d.refcount), Some(2));
    assert!(b.store.get(target_id).is_none());
    assert_eq!(b.store.get(cow_id).map(|d| d.refcount), Some(2));

    b.stop_direct_after_scanout_replaced(
        crate::kms::render::backend::DirectScope::Root,
        "destroy-subwindow test replacement",
    );
    b.stop_direct_after_scanout_replaced(
        crate::kms::render::backend::DirectScope::Root,
        "destroy-subwindow duplicate replacement",
    );

    assert!(!b.present_source_pins.contains_key(&source_pin));
    assert!(!b.present_source_pins.contains_key(&cow_pin));
    assert_eq!(b.store.get(source_id).map(|d| d.refcount), Some(1));
    assert_eq!(b.store.get(cow_id).map(|d| d.refcount), Some(1));
    let retired = b.drain_retired_present_idle_events();
    assert_eq!(retired.len(), 1);
    assert_eq!(retired[0].present_id, 77);
    assert!(b.drain_retired_present_idle_events().is_empty());
}

#[test]
fn final_cow_release_defers_storage_until_direct_replacement() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let target_xid = 0x5900;
    let _target_id = seed_window(&mut b, target_xid, None, 0, 0);
    b.get_overlay_window(None).expect("materialize COW");
    let cow_xid = yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0;
    let cow_id = b.cow_id.expect("COW id");
    let (source_id, _, source_pin, cow_pin) =
        install_direct_frame_for_target_test(&mut b, target_xid, cow_id, true);
    // The production path may already have prepared the lazy COW shadow
    // before the protocol release reaches us. Keep this unit test free of
    // Vulkan while exercising the ownership/deferral transition itself.
    b.pstate.direct.root_mut().unflip_shadow_ready = true;

    let final_release = b.prepare_overlay_release(None).expect("final COW release");
    b.retire_overlay_window(None);

    assert!(final_release);
    assert!(b.deferred_cow_release);
    assert_eq!(b.cow_id, Some(cow_id));
    assert!(b.windows.contains_key(&cow_xid));
    assert!(b.pstate.direct.root().current.is_some());
    assert!(b.pstate.direct.root().unflip_requested);
    assert!(!b.pstate.direct.root().hold_direct);
    assert_eq!(b.present_source_pins.get(&source_pin), Some(&source_id));
    assert_eq!(b.present_source_pins.get(&cow_pin), Some(&cow_id));
    assert_eq!(b.store.get(cow_id).map(|d| d.refcount), Some(2));

    b.stop_direct_after_scanout_replaced(
        crate::kms::render::backend::DirectScope::Root,
        "final COW release test replacement",
    );

    assert!(!b.deferred_cow_release);
    assert!(b.cow_id.is_none());
    assert!(!b.windows.contains_key(&cow_xid));
    assert!(b.store.get(cow_id).is_none());
    assert!(!b.present_source_pins.contains_key(&source_pin));
    assert!(!b.present_source_pins.contains_key(&cow_pin));
    assert_eq!(b.store.get(source_id).map(|d| d.refcount), Some(1));
    assert_eq!(b.drain_retired_present_idle_events().len(), 1);
    assert!(b.drain_retired_present_idle_events().is_empty());
}

#[test]
// Mutation: call finish_deferred_cow_release on every owner stop.
fn deferred_cow_release_last_owner() {
    use crate::kms::render::backend::DirectScope;
    use std::{
        os::fd::{AsFd, AsRawFd},
        time::Duration,
    };
    use yserver_core::{backend::Backend, server::ServerState};

    fn submit_replacement(backend: &KmsBackend, output_idx: usize, framebuffer_id: u32) -> u64 {
        let device = backend
            .platform
            .primary_device()
            .expect("fixture has a primary DRM device");
        let output = &backend.platform.outputs[output_idx].output;
        let plane = crate::drm::modeset::ComposedScanoutPlaneState {
            output,
            fb: ::drm::control::from_u32(framebuffer_id).expect("nonzero test framebuffer handle"),
            in_fence_fd: None,
        };
        crate::drm::modeset::submit_composed_scanout(
            &device.device,
            &device.atomic_commit_sink,
            &[plane],
            &[],
        )
        .expect("record accepted composed replacement")
    }

    fn stage_replacement(backend: &mut KmsBackend, output_idx: usize, event_id: u64) {
        let owner = &mut backend.pstate.direct.outputs[output_idx];
        owner.unflip_outputs_to_replace.insert(output_idx);
        owner.unflip_awaiting_outputs.insert(output_idx, event_id);
    }

    fn complete_replacement(
        backend: &mut KmsBackend,
        state: &mut ServerState,
        output_idx: usize,
        event_id: u64,
        frame: u32,
    ) {
        let output = &backend.platform.outputs[output_idx];
        let event_source = backend.platform.devices[0]
            .test_page_flip_event_source
            .as_ref()
            .expect("headless fixture has an injected page-flip source")
            .clone();
        event_source.queue_flip_with_cookie(
            output.output.crtc,
            frame,
            event_id,
            Duration::from_micros(u64::from(frame) * 1_000),
        );
        let device_fd = backend.platform.devices[0].device.as_fd().as_raw_fd();
        Backend::on_page_flip_ready(backend, state, device_fd);
    }

    let mut b = KmsBackend::for_tests();
    push_test_output(&mut b, 0x5912);
    let _recorder = atomic_cursor_recording_sink(&mut b);
    let cow_xid = yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0;
    assert!(
        Backend::get_overlay_window(&mut b, None).expect("materialize shared COW"),
        "both Output owners use one materialized COW"
    );
    let cow_id = b.cow_id.expect("materialized COW id");

    let (source0, _, source_pin0, cow_pin0) =
        install_direct_frame_for_target_test(&mut b, 0x5910, cow_id, true);
    let mut frame0 = b
        .pstate
        .direct
        .root_mut()
        .current
        .take()
        .expect("first seeded direct frame");
    frame0.completion_output_idx = 0;
    frame0.candidate.crtc_id = u32::from(b.platform.outputs[0].output.crtc);
    frame0.event.crtc_id = frame0.candidate.crtc_id;
    b.pstate.direct.outputs[0].current = Some(frame0);
    b.pstate.direct.outputs[0].hold_direct = true;

    let (source1, _, source_pin1, cow_pin1) =
        install_direct_frame_for_target_test(&mut b, 0x5920, cow_id, true);
    let mut frame1 = b
        .pstate
        .direct
        .root_mut()
        .current
        .take()
        .expect("second seeded direct frame");
    frame1.candidate.present_id = 78;
    frame1.event.present_id = 78;
    frame1.event.serial = 10;
    frame1.completion_output_idx = 1;
    frame1.candidate.crtc_id = u32::from(b.platform.outputs[1].output.crtc);
    frame1.event.crtc_id = frame1.candidate.crtc_id;
    b.pstate.direct.outputs[1].current = Some(frame1);
    b.pstate.direct.outputs[1].hold_direct = true;
    b.pstate.direct.root_mut().hold_direct = false;
    b.pstate.direct.outputs[0].unflip_shadow_ready = true;
    b.pstate.direct.outputs[1].unflip_shadow_ready = true;

    assert_eq!(
        b.pstate.direct.active_scopes(),
        [DirectScope::Output(0), DirectScope::Output(1)],
        "two independent Output owners share the seeded COW"
    );
    assert_eq!(
        b.store.get(cow_id).map(|drawable| drawable.refcount),
        Some(3)
    );

    assert!(
        Backend::prepare_overlay_release(&mut b, None).expect("prepare first COW release"),
        "first release has a materialized COW"
    );
    Backend::retire_overlay_window(&mut b, None);
    assert!(
        b.deferred_cow_release,
        "first release waits for both owners"
    );
    assert_eq!(
        b.cow_id,
        Some(cow_id),
        "first release preserves COW identity"
    );
    assert!(
        b.windows.contains_key(&cow_xid),
        "first release preserves COW projection"
    );
    assert_eq!(
        b.store.get(cow_id).map(|drawable| drawable.refcount),
        Some(3)
    );
    assert!(b.pstate.direct.outputs[0].unflip_requested);
    assert!(b.pstate.direct.outputs[1].unflip_requested);
    assert_eq!(b.present_source_pins.get(&source_pin0), Some(&source0));
    assert_eq!(b.present_source_pins.get(&cow_pin0), Some(&cow_id));
    assert_eq!(b.present_source_pins.get(&source_pin1), Some(&source1));
    assert_eq!(b.present_source_pins.get(&cow_pin1), Some(&cow_id));

    let output1_event = submit_replacement(&b, 1, 0x5922);
    stage_replacement(&mut b, 1, output1_event);
    let mut state = ServerState::new();
    complete_replacement(&mut b, &mut state, 1, output1_event, 58);

    assert_eq!(
        b.pstate.direct.active_scopes(),
        [DirectScope::Output(0)],
        "Output 1 replacement retires only its owner"
    );
    assert!(
        b.deferred_cow_release,
        "first owner stop defers final COW release"
    );
    assert_eq!(
        b.cow_id,
        Some(cow_id),
        "first owner stop preserves COW identity"
    );
    assert!(
        b.windows.contains_key(&cow_xid),
        "first owner stop preserves COW projection"
    );
    assert_eq!(
        b.store.get(cow_id).map(|drawable| drawable.refcount),
        Some(2)
    );
    assert_eq!(b.present_source_pins.get(&source_pin0), Some(&source0));
    assert_eq!(b.present_source_pins.get(&cow_pin0), Some(&cow_id));
    assert!(!b.present_source_pins.contains_key(&source_pin1));
    assert!(!b.present_source_pins.contains_key(&cow_pin1));
    assert_eq!(
        b.store.get(source1).map(|drawable| drawable.refcount),
        Some(1)
    );
    assert_eq!(
        Backend::drain_retired_present_idle_events(&mut b)
            .into_iter()
            .map(|event| event.present_id)
            .collect::<Vec<_>>(),
        [78],
        "Output 1 replacement retires its idle notification exactly once"
    );
    assert!(
        Backend::get_overlay_window(&mut b, None).expect("reacquire deferred COW"),
        "reacquisition reuses deferred COW storage"
    );
    assert_eq!(
        b.cow_id,
        Some(cow_id),
        "reacquisition keeps the same COW identity"
    );
    assert!(
        b.windows.contains_key(&cow_xid),
        "reacquisition restores COW projection"
    );
    assert!(
        Backend::prepare_overlay_release(&mut b, None).expect("prepare final COW release"),
        "final release has the reacquired COW"
    );
    Backend::retire_overlay_window(&mut b, None);
    assert!(
        b.deferred_cow_release,
        "final release waits for Output 0 replacement"
    );
    assert_eq!(
        b.cow_id,
        Some(cow_id),
        "final release still pins the COW before replacement"
    );

    let output0_event = submit_replacement(&b, 0, 0x5921);
    stage_replacement(&mut b, 0, output0_event);
    complete_replacement(&mut b, &mut state, 0, output0_event, 59);

    assert!(
        b.pstate.direct.active_scopes().is_empty(),
        "last replacement stops all owners"
    );
    assert!(
        !b.deferred_cow_release,
        "last owner stop finishes deferred COW release"
    );
    assert!(
        b.cow_id.is_none(),
        "last owner stop clears the COW identity"
    );
    assert!(
        !b.windows.contains_key(&cow_xid),
        "last owner stop removes the COW projection"
    );
    assert!(
        b.store.get(cow_id).is_none(),
        "last owner stop deletes COW storage"
    );
    assert!(!b.present_source_pins.contains_key(&source_pin0));
    assert!(!b.present_source_pins.contains_key(&cow_pin0));
    assert!(!b.present_source_pins.contains_key(&source_pin1));
    assert!(!b.present_source_pins.contains_key(&cow_pin1));
    assert_eq!(
        b.store.get(source0).map(|drawable| drawable.refcount),
        Some(1)
    );
    assert_eq!(
        b.store.get(source1).map(|drawable| drawable.refcount),
        Some(1)
    );
    assert_eq!(
        Backend::drain_retired_present_idle_events(&mut b)
            .into_iter()
            .map(|event| event.present_id)
            .collect::<Vec<_>>(),
        [77],
        "Output 0 replacement retires its idle notification exactly once"
    );
    assert!(
        Backend::drain_retired_present_idle_events(&mut b).is_empty(),
        "both replacement notifications have been drained"
    );
}

#[test]
fn final_cow_release_materialization_failure_keeps_cow_and_pins() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let target_xid = 0x5a00;
    let _target_id = seed_window(&mut b, target_xid, None, 0, 0);
    b.get_overlay_window(None).expect("materialize COW");
    let cow_id = b.cow_id.expect("COW id");
    let redirected_fallback = seed_window(&mut b, 0x5a10, None, 0, 0);
    let (source_id, _, source_pin, fallback_pin) =
        install_direct_frame_for_target_test(&mut b, target_xid, redirected_fallback, true);

    let error = b
        .prepare_overlay_release(None)
        .expect_err("test backend has no Vulkan engine for fallback materialization");

    assert!(error.to_string().contains("NoVk"), "{error}");
    assert!(!b.deferred_cow_release);
    assert_eq!(b.cow_id, Some(cow_id));
    assert!(b.store.get(cow_id).is_some());
    assert!(b.pstate.direct.root().current.is_some());
    assert!(!b.pstate.direct.root().unflip_requested);
    assert!(b.pstate.direct.root().hold_direct);
    assert_eq!(b.present_source_pins.get(&source_pin), Some(&source_id));
    assert_eq!(
        b.present_source_pins.get(&fallback_pin),
        Some(&redirected_fallback)
    );
}

#[test]
fn get_cow_during_deferred_release_reuses_backend_identity() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let target_xid = 0x5b00;
    let _target_id = seed_window(&mut b, target_xid, None, 0, 0);
    b.get_overlay_window(None).expect("materialize COW");
    let cow_id = b.cow_id.expect("COW id");
    let _ = install_direct_frame_for_target_test(&mut b, target_xid, cow_id, true);
    b.pstate.direct.root_mut().unflip_shadow_ready = true;
    assert!(b.prepare_overlay_release(None).expect("final release"));
    b.retire_overlay_window(None);

    let rematerialized = b.get_overlay_window(None).expect("reclaim deferred COW");

    assert!(rematerialized, "core must rebuild its logical COW resource");
    assert!(!b.deferred_cow_release);
    assert_eq!(b.cow_id, Some(cow_id));
    b.stop_direct_after_scanout_replaced(
        crate::kms::render::backend::DirectScope::Root,
        "reclaimed COW test replacement",
    );
    assert_eq!(b.cow_id, Some(cow_id));
    assert_eq!(b.store.get(cow_id).map(|d| d.refcount), Some(1));
}

#[test]
fn unmap_subwindow_requests_direct_unflip_without_releasing_pins() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let target_xid = 0x5c00;
    let _target_id = seed_window(&mut b, target_xid, None, 0, 0);
    b.get_overlay_window(None).expect("materialize COW");
    let cow_id = b.cow_id.expect("COW id");
    let (source_id, _, source_pin, cow_pin) =
        install_direct_frame_for_target_test(&mut b, target_xid, cow_id, true);

    b.unmap_subwindow(None, target_xid)
        .expect("unmap direct destination");

    assert!(!b.windows[&target_xid].mapped);
    assert!(b.pstate.direct.root().unflip_requested);
    assert!(!b.pstate.direct.root().hold_direct);
    assert_eq!(b.present_source_pins.get(&source_pin), Some(&source_id));
    assert_eq!(b.present_source_pins.get(&cow_pin), Some(&cow_id));
}

#[test]
fn unmap_unrelated_subwindow_keeps_direct_scanout_active() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let target_xid = 0x5c40;
    let unrelated_xid = 0x5c50;
    let _target_id = seed_window(&mut b, target_xid, None, 0, 0);
    let _unrelated_id = seed_window(&mut b, unrelated_xid, None, 0, 0);
    b.get_overlay_window(None).expect("materialize COW");
    let cow_id = b.cow_id.expect("COW id");
    let (source_id, _, source_pin, cow_pin) =
        install_direct_frame_for_target_test(&mut b, target_xid, cow_id, true);

    b.unmap_subwindow(None, unrelated_xid)
        .expect("unmap unrelated window");

    assert!(!b.windows[&unrelated_xid].mapped);
    assert!(!b.pstate.direct.root().unflip_requested);
    assert!(b.pstate.direct.root().hold_direct);
    assert_eq!(b.present_source_pins.get(&source_pin), Some(&source_id));
    assert_eq!(b.present_source_pins.get(&cow_pin), Some(&cow_id));
}

#[test]
fn map_subwindow_requests_direct_unflip_without_releasing_pins() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let target_xid = 0x5c80;
    let _target_id = seed_window(&mut b, target_xid, None, 0, 0);
    b.windows
        .get_mut(&target_xid)
        .expect("target geometry")
        .mapped = false;
    b.get_overlay_window(None).expect("materialize COW");
    let cow_id = b.cow_id.expect("COW id");
    let (source_id, _, source_pin, cow_pin) =
        install_direct_frame_for_target_test(&mut b, target_xid, cow_id, true);

    b.map_window_for_tests(target_xid)
        .expect("map direct destination");

    assert!(b.windows[&target_xid].mapped);
    assert!(b.pstate.direct.root().unflip_requested);
    assert!(!b.pstate.direct.root().hold_direct);
    assert_eq!(b.present_source_pins.get(&source_pin), Some(&source_id));
    assert_eq!(b.present_source_pins.get(&cow_pin), Some(&cow_id));
}

#[test]
fn map_unrelated_subwindow_keeps_direct_scanout_active() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let target_xid = 0x5cc0;
    let unrelated_xid = 0x5cd0;
    let _target_id = seed_window(&mut b, target_xid, None, 0, 0);
    let _unrelated_id = seed_window(&mut b, unrelated_xid, None, 0, 0);
    b.windows
        .get_mut(&unrelated_xid)
        .expect("unrelated geometry")
        .mapped = false;
    b.get_overlay_window(None).expect("materialize COW");
    let cow_id = b.cow_id.expect("COW id");
    let (source_id, _, source_pin, cow_pin) =
        install_direct_frame_for_target_test(&mut b, target_xid, cow_id, true);

    b.map_window_for_tests(unrelated_xid)
        .expect("map unrelated window");

    assert!(b.windows[&unrelated_xid].mapped);
    assert!(!b.pstate.direct.root().unflip_requested);
    assert!(b.pstate.direct.root().hold_direct);
    assert_eq!(b.present_source_pins.get(&source_pin), Some(&source_id));
    assert_eq!(b.present_source_pins.get(&cow_pin), Some(&cow_id));
}

#[test]
fn configure_subwindow_requests_direct_unflip_without_releasing_pins() {
    use yserver_core::host_x11::HostSubwindowConfig;

    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let target_xid = 0x5d00;
    let _target_id = seed_window(&mut b, target_xid, None, 0, 0);
    b.get_overlay_window(None).expect("materialize COW");
    let cow_id = b.cow_id.expect("COW id");
    let (source_id, _, source_pin, cow_pin) =
        install_direct_frame_for_target_test(&mut b, target_xid, cow_id, true);

    b.configure_subwindow(
        None,
        target_xid,
        HostSubwindowConfig {
            x: Some(12),
            y: None,
            width: None,
            height: None,
            border_width: None,
            sibling: None,
            stack_mode: None,
        },
    )
    .expect("configure direct destination");

    assert_eq!(b.windows[&target_xid].x, 12);
    assert!(b.pstate.direct.root().unflip_requested);
    assert!(!b.pstate.direct.root().hold_direct);
    assert_eq!(b.present_source_pins.get(&source_pin), Some(&source_id));
    assert_eq!(b.present_source_pins.get(&cow_pin), Some(&cow_id));
}

#[test]
fn configure_unrelated_subwindow_keeps_direct_scanout_active() {
    use yserver_core::host_x11::HostSubwindowConfig;

    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let target_xid = 0x5d80;
    let unrelated_xid = 0x5d90;
    let _target_id = seed_window(&mut b, target_xid, None, 0, 0);
    let _unrelated_id = seed_window(&mut b, unrelated_xid, None, 0, 0);
    b.get_overlay_window(None).expect("materialize COW");
    let cow_id = b.cow_id.expect("COW id");
    let (source_id, _, source_pin, cow_pin) =
        install_direct_frame_for_target_test(&mut b, target_xid, cow_id, true);

    b.configure_subwindow(
        None,
        unrelated_xid,
        HostSubwindowConfig {
            x: Some(12),
            y: None,
            width: None,
            height: None,
            border_width: None,
            sibling: None,
            stack_mode: None,
        },
    )
    .expect("configure unrelated window");

    assert_eq!(b.windows[&unrelated_xid].x, 12);
    assert!(!b.pstate.direct.root().unflip_requested);
    assert!(b.pstate.direct.root().hold_direct);
    assert_eq!(b.present_source_pins.get(&source_pin), Some(&source_id));
    assert_eq!(b.present_source_pins.get(&cow_pin), Some(&cow_id));
}

#[test]
fn logical_resize_keeps_old_cow_pinned_until_direct_replacement() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let target_xid = 0x5e00;
    let _target_id = seed_window(&mut b, target_xid, None, 0, 0);
    b.get_overlay_window(None).expect("materialize COW");
    let old_cow_id = b.cow_id.expect("old COW id");
    let (source_id, _, source_pin, cow_pin) =
        install_direct_frame_for_target_test(&mut b, target_xid, old_cow_id, true);
    b.pstate.direct.root_mut().unflip_shadow_ready = true;
    let new_w = b.platform.fb_w.saturating_add(1);
    let new_h = b.platform.fb_h.saturating_add(1);

    b.set_logical_screen_size(new_w, new_h)
        .expect("resize after direct shadow materialization");

    let new_cow_id = b.cow_id.expect("replacement COW id");
    assert_ne!(new_cow_id, old_cow_id);
    assert!(b.pstate.direct.root().unflip_requested);
    assert!(!b.pstate.direct.root().hold_direct);
    assert_eq!(b.present_source_pins.get(&source_pin), Some(&source_id));
    assert_eq!(b.present_source_pins.get(&cow_pin), Some(&old_cow_id));
    assert_eq!(
        b.store.get(old_cow_id).map(|d| d.refcount),
        Some(1),
        "the old COW survives only through the direct fallback pin"
    );
    assert_eq!(
        b.store
            .lookup(yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0),
        Some(new_cow_id)
    );

    b.stop_direct_after_scanout_replaced(
        crate::kms::render::backend::DirectScope::Root,
        "logical resize test replacement",
    );
    assert!(b.store.get(old_cow_id).is_none());
    assert_eq!(b.cow_id, Some(new_cow_id));
    assert!(b.store.get(new_cow_id).is_some());
}

#[test]
fn logical_resize_materialization_failure_preserves_old_dimensions_and_cow() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let target_xid = 0x5f00;
    let _target_id = seed_window(&mut b, target_xid, None, 0, 0);
    b.get_overlay_window(None).expect("materialize COW");
    let old_cow_id = b.cow_id.expect("old COW id");
    let redirected_fallback = seed_window(&mut b, 0x5f10, None, 0, 0);
    let (source_id, _, source_pin, fallback_pin) =
        install_direct_frame_for_target_test(&mut b, target_xid, redirected_fallback, true);
    let old_size = (b.platform.fb_w, b.platform.fb_h);
    let old_root_id = b.store.lookup(b.core.window_id);

    let error = b
        .set_logical_screen_size(old_size.0.saturating_add(1), old_size.1.saturating_add(1))
        .expect_err("materialization failure must abort before resize mutation");

    assert!(error.to_string().contains("NoVk"), "{error}");
    assert_eq!((b.platform.fb_w, b.platform.fb_h), old_size);
    assert_eq!(b.store.lookup(b.core.window_id), old_root_id);
    assert_eq!(b.cow_id, Some(old_cow_id));
    assert_eq!(
        b.store
            .lookup(yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0),
        Some(old_cow_id)
    );
    assert!(b.store.get(old_cow_id).is_some());
    assert!(!b.pstate.direct.root().unflip_requested);
    assert!(b.pstate.direct.root().hold_direct);
    assert_eq!(b.present_source_pins.get(&source_pin), Some(&source_id));
    assert_eq!(
        b.present_source_pins.get(&fallback_pin),
        Some(&redirected_fallback)
    );
}

#[test]
fn direct_scanout_topology_requires_one_drm_device() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    push_test_output(&mut b, 2);
    assert!(
        b.direct_scanout_topology_eligible(crate::kms::render::backend::DirectScope::Root),
        "two equal-refresh outputs on the primary card may use grouped direct scanout"
    );

    b.platform.outputs[1].key.device_key = test_device_key(1);
    assert!(
        !b.direct_scanout_topology_eligible(crate::kms::render::backend::DirectScope::Root),
        "one atomic direct transaction cannot span DRM devices"
    );
}

#[test]
fn grouped_direct_waits_all_outputs_and_uses_selected_reference_sample() {
    use yserver_core::backend::{
        CompletedPresentEvent, PresentClockSample, PresentClockSource, PresentScanoutCandidate,
        PresentWake,
    };

    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    push_test_output(&mut b, 2);
    b.pstate.direct.root_mut().cursor_bound_all = true;

    let crtc_id = 0x5400;
    bind_test_randr_crtc(&mut b, 0, crtc_id);
    let crtc_epoch = b.present_crtc_clock_epoch(crtc_id);
    let source_id = crate::kms::render::store::DrawableId::for_tests(90);
    let fallback_id = crate::kms::render::store::DrawableId::for_tests(91);
    let candidate = PresentScanoutCandidate {
        client_id: 1,
        client_pid: None,
        present_id: 44,
        crtc_id,
        crtc_epoch,
        src_pixmap_xid: 0x100,
        dst_window_xid: 0x200,
        variable_refresh: false,
        src_host_xid: 0x300,
        paint_dst_host_xid: 0x400,
        completion_dst_host_xid: 0x400,
        src_width: 800,
        src_height: 600,
        x_off: 0,
        y_off: 0,
        valid_region_xid: 0,
        update_region_xid: 0,
        update_is_full: true,
        explicit_sync: false,
        options: 0,
    };
    let event = CompletedPresentEvent {
        client_id: yserver_protocol::x11::ClientId(1),
        serial: 7,
        host_xid: 0x300,
        dst_host_xid: 0x400,
        options: 0,
        present_id: 44,
        window_generation: 0,
        crtc_id,
        crtc_epoch,
        msc_offset: 3,
        completion_clock: None,
        wake: PresentWake::Pixmap { idle_fence_xid: 0 },
        completion_mode: yserver_protocol::x11::present::COMPLETE_MODE_COPY,
        emit_idle: true,
    };
    b.pstate.direct.root_mut().pending = Some(crate::kms::render::backend::DirectPresentFrame {
        source_pin: 1,
        fallback_target_pin: 2,
        source_id,
        candidate,
        target: crate::kms::render::backend::ScanoutM0Target::Unredirected,
        present_received_at: std::time::Instant::now(),
        present_received_monotonic: None,
        fallback_target: PaintTarget::new(fallback_id, (0, 0), None, 24),
        event,
        completion_output_idx: 0,
        completion_clock: None,
        awaiting_outputs: std::collections::HashSet::from([0, 1]),
        accepted_event_id: Some(0x5a),
        async_outcome: crate::drm::async_flip::AsyncOutcome::NotRequested,
        in_fence: None,
        fence_tally: None,
    });

    let reference = PresentClockSample {
        msc: 123,
        ust: 4_567,
        source: PresentClockSource::PageFlip,
    };
    let other = PresentClockSample {
        msc: 999,
        ust: 9_999,
        source: PresentClockSource::PageFlip,
    };
    assert!(b.retire_direct_output(0, Some(0x5a), reference));
    assert!(b.pstate.direct.root().completed.is_empty());
    assert!(b.pstate.direct.root().current.is_none());
    assert_eq!(
        b.pstate
            .direct
            .root()
            .pending
            .as_ref()
            .and_then(|frame| frame.completion_clock),
        Some(reference),
        "the selected CRTC sample is cached without completing early"
    );

    assert!(b.retire_direct_output(1, Some(0x5a), other));
    assert_eq!(b.pstate.direct.root().completed.len(), 1);
    assert_eq!(
        b.pstate.direct.root().completed[0].completion_clock,
        Some(reference),
        "the last retiring output must not replace the selected CRTC timestamp"
    );
    assert!(b.pstate.direct.root().pending.is_none());
    assert!(b.pstate.direct.root().current.is_some());
    assert!(b.pstate.direct.root().idled.is_empty());
    assert!(
        !b.retire_direct_output(1, Some(0x5a), other),
        "a duplicate retirement cannot emit a second completion"
    );
    assert_eq!(b.pstate.direct.root().completed.len(), 1);
}

#[test]
fn async_fullscreen_direct_successors_coalesce_until_flip_retirement() {
    use yserver_core::backend::{PresentClockSample, PresentClockSource};

    let mut b = crate::kms::render::backend::KmsBackend::for_tests_with_direct_submitter(
        crate::kms::render::backend::KmsBackend::submit_direct_frame_for_tests,
    );
    b.pstate.direct.root_mut().cursor_bound_all = true;
    let fallback_id = seed_window(&mut b, 0x6a00, None, 0, 0);

    install_direct_frame_for_target_test(&mut b, 0x6b00, fallback_id, false);
    let predecessor = b
        .pstate
        .direct
        .root_mut()
        .pending
        .take()
        .expect("predecessor");

    install_direct_frame_for_target_test(&mut b, 0x6c00, fallback_id, false);
    let mut first_successor = b
        .pstate
        .direct
        .root_mut()
        .pending
        .take()
        .expect("first successor");
    first_successor.candidate.present_id = 78;
    first_successor.event.present_id = 78;
    first_successor.event.serial = 10;

    install_direct_frame_for_target_test(&mut b, 0x6d00, fallback_id, false);
    let mut latest_successor = b
        .pstate
        .direct
        .root_mut()
        .pending
        .take()
        .expect("latest successor");
    latest_successor.candidate.present_id = 79;
    latest_successor.event.present_id = 79;
    latest_successor.event.serial = 11;

    b.pstate.direct.root_mut().pending = Some(predecessor);
    b.queue_direct_successor(
        crate::kms::render::backend::DirectScope::Root,
        first_successor,
    );
    b.queue_direct_successor(
        crate::kms::render::backend::DirectScope::Root,
        latest_successor,
    );

    assert_eq!(
        b.pstate
            .direct
            .root()
            .queued_successor
            .as_ref()
            .map(|frame| frame.candidate.present_id),
        Some(79),
        "the hardware-successor state is one-slot latest-wins"
    );
    assert_eq!(
        b.pstate
            .direct
            .root()
            .deferred_successor_skips
            .iter()
            .map(|event| event.present_id)
            .collect::<Vec<_>>(),
        vec![78],
        "the coalesced successor is retained only as an ordered Skip"
    );
    assert_eq!(
        b.pstate
            .direct
            .root()
            .idled
            .iter()
            .map(|event| event.present_id)
            .collect::<Vec<_>>(),
        vec![78],
        "the coalesced buffer idles immediately and exactly once"
    );
    assert!(!b.pstate.direct.root().deferred_successor_skips[0].emit_idle);
    assert!(!b.pstate.direct.root().unflip_requested);
    assert!(b.pstate.direct.root().hold_direct);
    assert!(b.pstate.direct.root().completed.is_empty());

    let first_clock = PresentClockSample {
        msc: 100,
        ust: 1_000,
        source: PresentClockSource::PageFlip,
    };
    assert!(b.retire_direct_output(0, None, first_clock));

    assert_eq!(
        b.pstate
            .direct
            .root()
            .completed
            .iter()
            .map(|event| (event.present_id, event.completion_mode))
            .collect::<Vec<_>>(),
        vec![
            (77, yserver_protocol::x11::present::COMPLETE_MODE_FLIP),
            (78, yserver_protocol::x11::present::COMPLETE_MODE_SKIP),
        ],
        "the predecessor completes before the coalesced successor Skip"
    );
    assert_eq!(
        b.pstate
            .direct
            .root()
            .pending
            .as_ref()
            .map(|frame| frame.candidate.present_id),
        Some(79),
        "retirement immediately submits the retained direct successor"
    );
    assert!(b.pstate.direct.root().queued_successor.is_none());
    assert!(!b.pstate.direct.root().unflip_requested);
    assert!(b.pstate.direct.root().hold_direct);

    let second_clock = PresentClockSample {
        msc: 101,
        ust: 1_100,
        source: PresentClockSource::PageFlip,
    };
    assert!(b.retire_direct_output(0, None, second_clock));
    assert_eq!(
        b.pstate
            .direct
            .root()
            .completed
            .iter()
            .map(|event| (event.present_id, event.completion_mode))
            .collect::<Vec<_>>(),
        vec![
            (77, yserver_protocol::x11::present::COMPLETE_MODE_FLIP),
            (78, yserver_protocol::x11::present::COMPLETE_MODE_SKIP),
            (79, yserver_protocol::x11::present::COMPLETE_MODE_FLIP),
        ]
    );
}

#[test]
fn direct_scanout_topology_rejects_heterogeneous_refresh() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    push_test_output(&mut b, 2);
    b.platform.outputs[1].output.picked.vrefresh = 75;

    assert!(
        !b.direct_scanout_topology_eligible(crate::kms::render::backend::DirectScope::Root),
        "heterogeneous synthetic refresh rates stay on per-output composition"
    );
}

#[test]
fn direct_scanout_topology_compares_precise_fractional_timings() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    push_test_output(&mut b, 2);
    let timing = crate::platform::drm::Mode {
        name: "1920x1080".to_string(),
        width: 1920,
        height: 1080,
        vrefresh: 60,
        preferred: true,
        clock_khz: 148_500,
        htotal: 2200,
        vtotal: 1125,
        ..Default::default()
    };
    b.platform.outputs[0].output.picked = timing.clone();
    b.platform.outputs[1].output.picked = timing;
    assert!(b.direct_scanout_topology_eligible(crate::kms::render::backend::DirectScope::Root));

    // 60 vs 59.94 Hz: two "60 Hz" monitors, within the grouping tolerance.
    b.platform.outputs[1].output.picked.clock_khz = 148_352;
    assert!(b.direct_scanout_topology_eligible(crate::kms::render::backend::DirectScope::Root));

    // The advertised integer still says 60, but the kernel timing is 50 Hz.
    // The grouped path must go by the timing, not the integer.
    b.platform.outputs[1].output.picked.clock_khz = 123_750;
    assert!(
        !b.direct_scanout_topology_eligible(crate::kms::render::backend::DirectScope::Root),
        "same integer vrefresh is insufficient when kernel timing is available"
    );
}

#[test]
fn direct_scanout_refresh_comparison_accounts_for_scan_multipliers() {
    let base = crate::platform::drm::Mode {
        clock_khz: 148_500,
        htotal: 2200,
        vtotal: 1125,
        vscan: 1,
        ..Default::default()
    };
    let mut adjusted = base.clone();

    adjusted.flags = 1 << 4; // interlace doubles the effective rate
    assert!(!crate::kms::render::backend::effective_refresh_matches(
        &base, &adjusted
    ));

    adjusted.flags |= 1 << 5; // doublescan divides it back by two
    assert!(crate::kms::render::backend::effective_refresh_matches(
        &base, &adjusted
    ));

    adjusted.vscan = 2;
    assert!(
        !crate::kms::render::backend::effective_refresh_matches(&base, &adjusted),
        "vscan > 1 divides the effective refresh"
    );
}

#[test]
fn direct_scanout_refresh_comparison_groups_near_equal_rates_only() {
    // 1680x1050 CVT-RB, 119.0 MHz / (1840 * 1080) = 59.883 Hz.
    let reduced = crate::platform::drm::Mode {
        clock_khz: 119_000,
        htotal: 1840,
        vtotal: 1080,
        vscan: 1,
        ..Default::default()
    };
    // 1680x1050 CVT, 146.25 MHz / (2240 * 1089) = 59.954 Hz.
    let cvt = crate::platform::drm::Mode {
        clock_khz: 146_250,
        htotal: 2240,
        vtotal: 1089,
        vscan: 1,
        ..Default::default()
    };
    assert!(crate::kms::render::backend::effective_refresh_matches(
        &reduced, &cvt
    ));
    // 1680x1050@75: 187.0 MHz / (2272 * 1099) = 74.892 Hz.
    let seventy_five = crate::platform::drm::Mode {
        clock_khz: 187_000,
        htotal: 2272,
        vtotal: 1099,
        vscan: 1,
        ..Default::default()
    };
    assert!(!crate::kms::render::backend::effective_refresh_matches(
        &cvt,
        &seventy_five
    ));
}

#[test]
fn arm_present_shared_wake_with_arms_only_selected_crtc() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let crtc0 = output_crtc_key(&b, 0);
    push_test_output(&mut b, 2);
    let crtc1 = output_crtc_key(&b, 1);

    // Output 1 holds the max general-clock sample.
    b.platform.ust_msc.insert(crtc0, (5, 5_000));
    b.platform.ust_msc.insert(crtc1, (50, 50_000));

    let mut armed_on: Vec<CrtcKey> = Vec::new();
    let armed = b
        .arm_present_shared_wake_with(crtc0, |crtc_key| {
            armed_on.push(crtc_key);
            Ok(true)
        })
        .unwrap();

    assert!(armed);
    assert_eq!(
        armed_on,
        vec![crtc0],
        "must arm the selected CRTC even when another domain has a larger counter"
    );
    assert!(
        !super::vblank_arm_is_watched(&b, crtc1, super::SHARED_WAKE_VBLANK_ARM_SLOT),
        "the unrelated CRTC must not gain an armed entry"
    );
}

#[test]
fn arm_present_shared_wake_with_uses_selected_crtc_before_first_sample() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let first = output_crtc_key(&b, 0);
    assert!(b.platform.ust_msc.is_empty());

    let mut armed_on = Vec::new();
    let armed = b
        .arm_present_shared_wake_with(first, |crtc_key| {
            armed_on.push(crtc_key);
            Ok(true)
        })
        .unwrap();

    assert!(armed);
    assert_eq!(armed_on, vec![first]);
}

// ── Present deferred-execution capability surface (Task 2) ─────────────

#[test]
fn phase_b_flip_visibility_includes_all_scanout_transactions() {
    assert!(crate::kms::render::backend::phase_b_flip_in_flight_for_scheduler(true, false, false));
    assert!(crate::kms::render::backend::phase_b_flip_in_flight_for_scheduler(false, true, false));
    assert!(crate::kms::render::backend::phase_b_flip_in_flight_for_scheduler(false, false, true));
    assert!(
        !crate::kms::render::backend::phase_b_flip_in_flight_for_scheduler(false, false, false)
    );
}

#[test]
fn present_flip_in_flight_mirrors_scene_state() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let crtc_id = 0x5000;
    bind_test_randr_crtc(&mut b, 0, crtc_id);
    assert!(
        !b.present_flip_in_flight(crtc_id),
        "no flip queued at fixture init"
    );

    b.scene.test_set_flip_in_flight(true);
    assert_eq!(
        b.present_flip_in_flight(crtc_id),
        b.scene.has_pending_page_flips(),
        "must mirror scene.has_pending_page_flips(), not a copy of it"
    );
    assert!(b.present_flip_in_flight(crtc_id));
    assert!(
        !b.present_flip_in_flight(0x5fff),
        "an unknown/off CRTC never borrows another output's flip state"
    );
}

#[test]
// Mutation: read Root pending/replacement fields for every output query.
fn output_present_in_flight_policy() {
    use crate::kms::render::{
        backend::{DirectPresentFrame, ScanoutM0Target},
        store::DrawableId,
        target::PaintTarget,
    };
    use std::collections::HashSet;
    use yserver_core::backend::{CompletedPresentEvent, PresentScanoutCandidate, PresentWake};

    // Each row gets a fresh backend so pending frames and replacement markers
    // cannot leak into another query's classification.
    for (row, queried_output, phase, expected_in_flight) in [
        // D12: pending direct state is visible on its owner output.
        ("pending_owner_query", 1, 0_u32, true),
        // D12: the pending output does not make its peer look busy.
        ("pending_other_output_query", 0, 0_u32, false),
        // D12: a requested replacement marker remains output-local.
        ("replacement_requested_owner_query", 1, 1_u32, true),
        // D12: a peer query does not borrow the replacement marker.
        ("replacement_requested_other_output_query", 0, 1_u32, false),
    ] {
        let mut backend = KmsBackend::for_tests();
        push_test_output(&mut backend, 0x5001);
        backend.scene.scene_structure_dirty = false;
        backend.scene.test_set_flip_in_flight(false);
        let output_crtcs = [
            u32::from(backend.platform.outputs[0].output.crtc),
            u32::from(backend.platform.outputs[1].output.crtc),
        ];
        bind_test_randr_crtc(&mut backend, 0, output_crtcs[0]);
        bind_test_randr_crtc(&mut backend, 1, output_crtcs[1]);
        let output_idx = 1;
        let present_id = 0xD1_2000
            + u64::from(phase) * 10
            + u64::try_from(queried_output).expect("output index fits");
        let source_id = DrawableId::for_tests(present_id);
        let fallback_id = DrawableId::for_tests(present_id + 1);
        let crtc_id = output_crtcs[output_idx];
        let crtc_epoch = backend.present_crtc_clock_epoch(crtc_id);
        let candidate = PresentScanoutCandidate {
            client_id: 1,
            client_pid: None,
            present_id,
            crtc_id,
            crtc_epoch,
            src_pixmap_xid: 0xD120,
            dst_window_xid: 0xD121,
            variable_refresh: false,
            src_host_xid: 0xD122,
            paint_dst_host_xid: 0xD121,
            completion_dst_host_xid: 0xD121,
            src_width: 800,
            src_height: 600,
            x_off: 0,
            y_off: 0,
            valid_region_xid: 0,
            update_region_xid: 0,
            update_is_full: true,
            explicit_sync: false,
            options: 0,
        };
        let frame = DirectPresentFrame {
            source_pin: 1,
            fallback_target_pin: 2,
            source_id,
            candidate,
            target: ScanoutM0Target::Unredirected,
            present_received_at: std::time::Instant::now(),
            present_received_monotonic: None,
            fallback_target: PaintTarget::new(fallback_id, (0, 0), None, 24),
            event: CompletedPresentEvent {
                client_id: yserver_protocol::x11::ClientId(1),
                serial: u32::try_from(present_id).expect("Present serial fits"),
                host_xid: 0xD122,
                dst_host_xid: 0xD121,
                options: 0,
                present_id,
                window_generation: 0,
                crtc_id,
                crtc_epoch,
                msc_offset: 0,
                completion_clock: None,
                wake: PresentWake::Pixmap { idle_fence_xid: 0 },
                completion_mode: yserver_protocol::x11::present::COMPLETE_MODE_FLIP,
                emit_idle: false,
            },
            completion_output_idx: output_idx,
            completion_clock: None,
            awaiting_outputs: HashSet::from([output_idx]),
            accepted_event_id: Some(0xD120),
            async_outcome: crate::drm::async_flip::AsyncOutcome::NotRequested,
            in_fence: None,
            fence_tally: None,
        };
        let owner = &mut backend.pstate.direct.outputs[output_idx];
        if phase == 0 {
            owner.pending = Some(frame);
        } else {
            owner.current = Some(frame);
            owner.unflip_requested = true;
            owner.unflip_outputs_to_replace = HashSet::from([output_idx]);
        }
        assert!(
            owner.active(),
            "{row}: the output-scoped marker is paired with an active owner"
        );

        let queried_crtc = output_crtcs[queried_output];
        assert_eq!(
            Backend::present_flip_in_flight(&backend, queried_crtc),
            expected_in_flight,
            "{row}: Present sees only the queried output's direct transaction"
        );
        assert_eq!(
            Backend::present_display_idle(&backend, queried_crtc),
            !expected_in_flight,
            "{row}: idle-display classification follows the queried output owner"
        );
    }
}

#[test]
fn present_display_idle_false_when_scene_wants_compose_even_with_no_flips() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let crtc_id = 0x5000;
    bind_test_randr_crtc(&mut b, 0, crtc_id);
    assert!(!b.present_flip_in_flight(crtc_id), "no flip in flight");
    b.scene.scene_structure_dirty = true;
    assert!(
        !b.present_display_idle(crtc_id),
        "pending compose damage must gate the idle-display fallback \
             even though no flip is in flight (spec round-4 F1)"
    );
}

#[test]
fn present_scanout_blackout_true_when_kms_outputs_active_false_even_while_scanout_allowed() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    assert!(b.scanout_allowed(), "VT is Active in the test fixture");
    assert!(!b.present_scanout_blackout(), "not blacked out initially");

    // DPMS-off toggles kms_outputs_active, not vt_state — round-4 F1a:
    // scanout_allowed() alone is VT-only and would never see this.
    b.kms_outputs_active = false;
    assert!(b.scanout_allowed(), "DPMS-off does not change VT state");
    assert!(
        b.present_scanout_blackout(),
        "kms_outputs_active=false must blackout even while scanout_allowed()"
    );
}

#[test]
fn crtc_enable_from_zero_outputs_opens_kms_output_gate() {
    assert!(crate::kms::render::backend::kms_outputs_active_after_crtc_config(false, true, 1));
}

#[test]
fn crtc_disable_last_output_closes_kms_output_gate() {
    assert!(!crate::kms::render::backend::kms_outputs_active_after_crtc_config(true, false, 0));
}

#[test]
fn headless_output_inventory_keeps_present_in_blackout() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    b.platform.outputs.clear();
    b.pstate.direct.sync_output_count(b.platform.outputs.len());
    b.kms_outputs_active = !b.platform.outputs.is_empty();

    assert!(b.scanout_allowed(), "the fixture VT remains active");
    assert!(
        b.present_scanout_blackout(),
        "zero active outputs must flush Present through blackout fallback instead of parking for an MSC that cannot advance"
    );
}

#[test]
fn zero_device_backend_disables_dri3_and_vblank_arms() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    b.platform.devices.clear();
    b.platform.outputs.clear();
    b.pstate.direct.sync_output_count(b.platform.outputs.len());
    b.kms_outputs_active = false;

    assert!(b.platform.primary_device().is_none());
    assert_eq!(b.dri3_capabilities(), Dri3Caps::unsupported());
    assert!(b.dri3_open(0).is_err());
    assert_eq!(
        b.arm_idle_vblanks_ioctl(0, &[1])
            .expect("relative arm no-op"),
        0
    );
    assert!(!Backend::arm_present_shared_wake(&mut b, 0).expect("shared wake no-op"));
    assert!(b.present_scanout_blackout());
}

#[test]
fn pin_present_source_survives_by_xid_invalidation() {
    use ash::vk;

    use crate::kms::render::store::DrawableKind;

    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let xid: u32 = 0xCAFE_1234;
    let storage = Storage::for_tests_null(
        vk::Extent2D {
            width: 4,
            height: 4,
        },
        vk::Format::R8_UNORM,
    );
    let did = b
        .store
        .allocate(xid, DrawableKind::Pixmap, 1, false, storage)
        .expect("allocate");
    assert_eq!(b.store.get(did).unwrap().refcount, 1);

    let pin = b
        .pin_present_source(xid)
        .expect("pin must resolve a live xid");
    assert_eq!(
        b.store.get(did).unwrap().refcount,
        2,
        "pin must incref the resolved drawable"
    );

    // Simulate FreePixmap / xid reuse: the by_xid mapping is gone, but
    // the pin already captured the DrawableId at pin time and must not
    // re-resolve through the xid.
    b.store.detach_xid(xid);
    assert!(b.store.lookup(xid).is_none());

    b.release_present_source(pin);
    assert_eq!(
        b.store.get(did).map(|d| d.refcount),
        Some(1),
        "release must decref the captured id exactly once even though \
             the xid no longer resolves"
    );

    // Unknown token release is a silent no-op — no double-decref.
    b.release_present_source(pin);
    assert_eq!(b.store.get(did).map(|d| d.refcount), Some(1));
}

#[test]
fn pin_present_source_unknown_xid_returns_none() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    assert!(b.pin_present_source(0xDEAD_0000).is_none());
}

#[test]
fn scanout_m0_classifies_root_and_per_output_geometry() {
    let outputs = [(0, 0, 1920, 1080), (1920, 0, 2560, 1440)];
    assert_eq!(
        crate::kms::render::backend::classify_scanout_m0_coverage(
            Some((0, 0, 4480, 1440)),
            (4480, 1440),
            (4480, 1440),
            &outputs,
        ),
        crate::kms::render::backend::ScanoutM0Coverage::Root,
    );
    assert_eq!(
        crate::kms::render::backend::classify_scanout_m0_coverage(
            Some((1920, 0, 2560, 1440)),
            (2560, 1440),
            (4480, 1440),
            &outputs,
        ),
        crate::kms::render::backend::ScanoutM0Coverage::Output(1),
    );
}

#[test]
fn scanout_m0_rejects_geometry_or_source_extent_mismatch() {
    let outputs = [(0, 0, 1920, 1080)];
    assert_eq!(
        crate::kms::render::backend::classify_scanout_m0_coverage(
            Some((0, 0, 1920, 1080)),
            (1280, 720),
            (1920, 1080),
            &outputs,
        ),
        crate::kms::render::backend::ScanoutM0Coverage::None,
    );
    assert_eq!(
        crate::kms::render::backend::classify_scanout_m0_coverage(
            None,
            (1920, 1080),
            (1920, 1080),
            &outputs
        ),
        crate::kms::render::backend::ScanoutM0Coverage::None,
    );
}

#[test]
fn scanout_m1_accepts_exact_dual_head_root_tiling() {
    let outputs = [
        crate::kms::render::backend::ScanoutM1OutputGeometry {
            x: 0,
            y: 0,
            width: 2560,
            height: 1440,
            mode_width: 2560,
            mode_height: 1440,
        },
        crate::kms::render::backend::ScanoutM1OutputGeometry {
            x: 2560,
            y: 0,
            width: 2560,
            height: 1440,
            mode_width: 2560,
            mode_height: 1440,
        },
    ];
    assert!(crate::kms::render::backend::scanout_m1_outputs_cover_root(
        (5120, 1440),
        &outputs
    ));
}

#[test]
fn scanout_m1_rejects_gap_overlap_bounds_and_mode_mismatch() {
    let output = |x, width, mode_width| crate::kms::render::backend::ScanoutM1OutputGeometry {
        x,
        y: 0,
        width,
        height: 1440,
        mode_width,
        mode_height: 1440,
    };
    assert!(!crate::kms::render::backend::scanout_m1_outputs_cover_root(
        (5120, 1440),
        &[output(0, 2500, 2500), output(2560, 2560, 2560)],
    ));
    assert!(!crate::kms::render::backend::scanout_m1_outputs_cover_root(
        (5120, 1440),
        &[output(0, 2600, 2600), output(2560, 2560, 2560)],
    ));
    assert!(!crate::kms::render::backend::scanout_m1_outputs_cover_root(
        (5120, 1440),
        &[output(-1, 2560, 2560), output(2560, 2560, 2560)],
    ));
    assert!(!crate::kms::render::backend::scanout_m1_outputs_cover_root(
        (5120, 1440),
        &[output(0, 2560, 1920), output(2560, 2560, 2560)],
    ));
}

fn m1_valid_layout() -> crate::kms::render::backend::ScanoutSourceLayout {
    crate::kms::render::backend::ScanoutSourceLayout {
        fourcc: 0x3432_5258,
        vk_format: ash::vk::Format::B8G8R8A8_UNORM,
        depth: 24,
        bpp: 32,
        width: 5120,
        height: 1440,
        plane_count: 1,
        offset: 0,
        pitch: 20480,
        modifier: 0,
    }
}

fn m1_validate(
    layout: &crate::kms::render::backend::ScanoutSourceLayout,
    output_modifiers: &[&[u64]],
) -> Result<(), crate::kms::render::backend::ScanoutM1Reject> {
    crate::kms::render::backend::scanout_m1_validate(layout, (5120, 1440), output_modifiers)
}

#[test]
fn scanout_m1_validate_accepts_scope_sized_xrgb_linear() {
    assert_eq!(m1_validate(&m1_valid_layout(), &[&[0], &[]]), Ok(()));
}

#[test]
fn scanout_m1_validate_names_each_reject() {
    use crate::kms::render::backend::ScanoutM1Reject as R;
    let any: [&[u64]; 2] = [&[], &[]];
    let with = |edit: fn(&mut crate::kms::render::backend::ScanoutSourceLayout)| {
        let mut layout = m1_valid_layout();
        edit(&mut layout);
        m1_validate(&layout, &any)
    };
    assert_eq!(with(|l| l.fourcc = 0x3432_4152), Err(R::Fourcc));
    assert_eq!(
        with(|l| l.vk_format = ash::vk::Format::R8G8B8A8_UNORM),
        Err(R::VkFormat)
    );
    assert_eq!(with(|l| l.plane_count = 2), Err(R::PlaneCount));
    assert_eq!(with(|l| l.depth = 32), Err(R::Depth));
    assert_eq!(with(|l| l.bpp = 24), Err(R::Bpp));
    assert_eq!(with(|l| l.pitch = 0), Err(R::ZeroPitch));
    assert_eq!(with(|l| l.width = 2560), Err(R::ScopeSizeMismatch));
    assert_eq!(
        m1_validate(&m1_valid_layout(), &[&[0], &[0x0100_0000_0000_0001]]),
        Err(R::Modifier)
    );
}

#[test]
fn scanout_m1_redirected_game_and_region_present_never_reach_probe_cache() {
    use yserver_core::backend::PresentScanoutCandidate;

    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let crtc_id = 0x5500;
    bind_test_randr_crtc(&mut b, 0, crtc_id);
    let candidate = PresentScanoutCandidate {
        client_id: 1,
        client_pid: None,
        present_id: 1,
        crtc_id,
        crtc_epoch: b.present_crtc_clock_epoch(crtc_id),
        src_pixmap_xid: 0x100,
        dst_window_xid: 0x200,
        variable_refresh: false,
        src_host_xid: 0x300,
        paint_dst_host_xid: 0x400,
        completion_dst_host_xid: 0x400,
        src_width: 800,
        src_height: 600,
        x_off: 0,
        y_off: 0,
        valid_region_xid: 0,
        update_region_xid: 0,
        update_is_full: true,
        explicit_sync: false,
        options: 0,
    };
    b.maybe_probe_scanout_m1(
        Some(crate::kms::render::store::DrawableId::for_tests(99)),
        crate::kms::render::backend::ScanoutM0Target::Other,
        crate::kms::render::backend::ScanoutM0Coverage::Output(0),
        None,
        candidate,
    );
    b.maybe_probe_scanout_m1(
        Some(crate::kms::render::store::DrawableId::for_tests(100)),
        crate::kms::render::backend::ScanoutM0Target::CowDescendant,
        crate::kms::render::backend::ScanoutM0Coverage::Root,
        Some(crate::kms::render::backend::DirectScope::Root),
        PresentScanoutCandidate {
            valid_region_xid: 0xDEAD,
            ..candidate
        },
    );
    assert!(b.pstate.scanout_m1.entries.is_empty());
    assert_eq!(b.pstate.scanout_m0.m1_probe_pass, 0);
    assert_eq!(b.pstate.scanout_m0.m1_probe_reject, 0);
    assert_eq!(b.pstate.scanout_m0.m1_probe_error, 0);
}

#[test]
fn scanout_m1_probe_eligible_accepts_unredirected_fullscreen() {
    use crate::kms::render::backend::{ScanoutM0Coverage, ScanoutM0Target};
    assert!(crate::kms::render::backend::scanout_m1_probe_eligible(
        true,
        true,
        true,
        true,
        ScanoutM0Target::Unredirected,
        ScanoutM0Coverage::Root,
        0,
        0,
        0,
    ));
    assert!(!crate::kms::render::backend::scanout_m1_probe_eligible(
        true,
        true,
        true,
        true,
        ScanoutM0Target::Other,
        ScanoutM0Coverage::Root,
        0,
        0,
        0,
    ));
    assert!(!crate::kms::render::backend::scanout_m1_probe_eligible(
        true,
        true,
        true,
        true,
        ScanoutM0Target::Unredirected,
        ScanoutM0Coverage::None,
        0,
        0,
        0,
    ));
    assert!(!crate::kms::render::backend::scanout_m1_probe_eligible(
        true,
        true,
        false,
        true,
        ScanoutM0Target::Unredirected,
        ScanoutM0Coverage::Root,
        0,
        0,
        0,
    ));
}

type ScanoutM1PlaneObservation = (u32, u32, u32, u32, u32);
type ScanoutM1ProbeObservation = (
    crate::kms::render::backend::DirectScope,
    Vec<ScanoutM1PlaneObservation>,
);

thread_local! {
    static SCANOUT_M1_PROBES: std::cell::RefCell<Vec<ScanoutM1ProbeObservation>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

pub(super) fn record_scanout_m1_probe(
    _: &crate::kms::render::backend::KmsBackend,
    _: crate::kms::render::store::DrawableId,
    scope: crate::kms::render::backend::DirectScope,
    planes: &[crate::drm::modeset::DirectScanoutPlaneState<'_>],
) -> std::io::Result<crate::kms::render::backend::ScanoutM1ProbeEntry> {
    let observations: Vec<_> = planes
        .iter()
        .map(|plane| {
            (
                u32::from(plane.output.crtc),
                plane.src_x,
                plane.src_y,
                plane.src_w,
                plane.src_h,
            )
        })
        .collect();
    SCANOUT_M1_PROBES.with(|probes| probes.borrow_mut().push((scope, observations)));
    let crtcs = planes
        .iter()
        .map(|plane| plane.output.crtc)
        .collect::<Vec<_>>();
    let plane = planes.first().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "probe has no output plane",
        )
    })?;
    Ok(
        crate::kms::render::backend::ScanoutM1ProbeEntry::accepted_with_shape(
            crate::drm::modeset::DirectScanoutProbeFramebuffer::for_tests(
                ::drm::control::from_u32(0x8f42).expect("nonzero test framebuffer"),
            ),
            crate::drm::async_flip::AsyncShape {
                fourcc: 0x3432_5258,
                modifier: 0,
                pitch: plane.src_w * 4,
                width: plane.src_w,
                height: plane.src_h,
                crtcs,
                direct: true,
            },
        ),
    )
}

fn output_admission_fixture_with_geometry(
    output_idx: usize,
    present_id: u64,
    screen_size: (u16, u16),
    outputs: &[(i32, i32, u16, u16, u32)],
) -> Option<(
    super::commit_identity::recording_scene::RecordingSceneFixture,
    super::commit_identity::recording_scene::DirectPresentSetup,
)> {
    use yserver_core::backend::Backend;

    let mut fixture =
        match super::commit_identity::recording_scene::RecordingSceneFixture::with_output_geometry(
            screen_size,
            outputs,
        ) {
            Ok(Some(fixture)) => fixture,
            Ok(None) => {
                eprintln!("SKIP output admission: Vulkan scene fixture unavailable");
                return None;
            }
            Err(error) => panic!("output admission fixture construction failed: {error}"),
        };
    let direct = fixture.install_direct_output_present(output_idx, present_id, 0);
    let backend = &mut fixture.events.backend;
    SCANOUT_M1_PROBES.with(|probes| probes.borrow_mut().clear());
    backend.for_tests_set_scanout_m1_prober(record_scanout_m1_probe);
    Backend::note_present_scanout_candidate(backend, direct.candidate);
    Some((fixture, direct))
}

pub(super) fn output_admission_fixture(
    output_idx: usize,
    present_id: u64,
) -> Option<(
    super::commit_identity::recording_scene::RecordingSceneFixture,
    super::commit_identity::recording_scene::DirectPresentSetup,
)> {
    output_admission_fixture_with_geometry(
        output_idx,
        present_id,
        (3840, 1080),
        &[(0, 0, 1920, 1080, 75), (1920, 0, 1920, 1080, 240)],
    )
}

fn scanout_m1_probe_observations() -> Vec<ScanoutM1ProbeObservation> {
    SCANOUT_M1_PROBES.with(|probes| probes.borrow().clone())
}

fn draw_direct_admission_overlay(backend: &mut KmsBackend, client_id: u32, x: i16) {
    use yserver_core::backend::{Backend, DrawState, GcFunction, OriginContext, SubwindowMode};

    let origin = Some(OriginContext {
        client_id: yserver_protocol::x11::ClientId(client_id),
        nested_seq: 0,
        opcode: 70,
    });
    Backend::apply_draw_state(
        backend,
        origin,
        &DrawState {
            function: GcFunction::Invert,
            subwindow_mode: SubwindowMode::IncludeInferiors,
            ..DrawState::default()
        },
    )
    .expect("set reversible root overlay draw state");
    Backend::fill_rectangle(
        backend,
        origin,
        backend.core.window_id,
        u32::MAX,
        x,
        10,
        50,
        1,
    )
    .expect("draw root overlay through Backend");
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation rows: P15 keeps probation after rejection; P16 ignores an active owner's rejection.
fn output_rejection_by_owner_phase() {
    use yserver_core::backend::Backend;

    let probation = usize::from(crate::kms::render::backend::SCANOUT_M2_ELIGIBLE_ROOT_PROBATION);
    for (row, present_id) in [("probation", 816), ("active_pending", 817)] {
        let Some((mut fixture, direct)) = output_admission_fixture(1, present_id) else {
            eprintln!(
                "SKIP output_rejection_by_owner_phase: Vulkan scene fixture unavailable ({row})"
            );
            return;
        };
        let fd = fixture.kms_fd(1);
        let output0_crtc = fixture.events.backend.platform.outputs[0].output.crtc;
        let output1_crtc = fixture.events.backend.platform.outputs[1].output.crtc;
        let start = fixture.events.recorders[&fd].records().len();

        // Mutation P15: a scoped rejection must reset this owner's streak.
        if row == "probation" {
            for _ in 0..probation - 1 {
                assert!(
                    !Backend::try_present_direct(
                        &mut fixture.events.backend,
                        direct.candidate,
                        direct.event.clone(),
                    )
                    .expect("eligible Output Present before rejection"),
                    "{row}: probation must not admit before its final eligible attempt"
                );
            }
            let mut rejected = direct.candidate;
            rejected.valid_region_xid = 0x55;
            assert!(
                !Backend::try_present_direct(
                    &mut fixture.events.backend,
                    rejected,
                    direct.event.clone(),
                )
                .expect("region-limited Output Present remains composed"),
                "{row}: region-limited Present stays composed"
            );
            assert!(
                !fixture.events.backend.pstate.direct.outputs[1].active(),
                "{row}: rejected Present does not create an Output owner"
            );

            for attempt in 0..probation {
                let entered = Backend::try_present_direct(
                    &mut fixture.events.backend,
                    direct.candidate,
                    direct.event.clone(),
                )
                .expect("eligible Output Present after rejection");
                assert_eq!(
                    entered,
                    attempt + 1 == probation,
                    "{row}: rejection restarts probation at attempt {attempt}"
                );
            }
            assert!(
                fixture.events.backend.pstate.direct.outputs[1].active(),
                "{row}: a fresh full probation run admits Output 1"
            );
            let records = fixture.events.recorders[&fd].records();
            assert!(
                records[start..].iter().any(|record| {
                    record
                        .flags
                        .contains(::drm::control::AtomicCommitFlags::PAGE_FLIP_EVENT)
                        && record.events.len() == 1
                        && record.events[0].crtc == output1_crtc
                }),
                "{row}: final admission records an Output 1 direct commit"
            );
            assert!(
                records[start..].iter().all(|record| {
                    !record
                        .flags
                        .contains(::drm::control::AtomicCommitFlags::PAGE_FLIP_EVENT)
                        || record.events.iter().all(|event| event.crtc != output0_crtc)
                }),
                "{row}: admission emits no direct commit for Output 0"
            );
        // Mutation P16: a rejected successor must unflip this active owner.
        } else {
            for attempt in 0..probation {
                let entered = Backend::try_present_direct(
                    &mut fixture.events.backend,
                    direct.candidate,
                    direct.event.clone(),
                )
                .expect("eligible Output Present through Backend");
                assert_eq!(
                    entered,
                    attempt + 1 == probation,
                    "{row}: initial owner enters on the final probation attempt"
                );
            }
            let owner = &fixture.events.backend.pstate.direct.outputs[1];
            assert!(
                owner.active() && (owner.pending.is_some() || owner.current.is_some()),
                "{row}: accepted direct Present leaves Output 1 active or pending"
            );

            let mut rejected = direct.candidate;
            rejected.valid_region_xid = 0x56;
            assert!(
                !Backend::try_present_direct(
                    &mut fixture.events.backend,
                    rejected,
                    direct.event.clone(),
                )
                .expect("region-limited Output successor stays composed"),
                "{row}: rejected successor does not enter direct scanout"
            );
            let owner = &fixture.events.backend.pstate.direct.outputs[1];
            assert!(
                owner.unflip_requested,
                "{row}: active owner requests unflip"
            );
            assert_eq!(
                owner.unflip_fallback_source,
                Some(direct.source_id),
                "{row}: rejected successor becomes the composed fallback source"
            );
            assert!(
                !owner.unflip_shadow_ready,
                "{row}: rejected successor leaves its shadow pending"
            );
            assert!(
                owner.current.is_some() || owner.pending.is_some(),
                "{row}: source owner remains retained until replacement"
            );
            assert!(
                fixture.events.recorders[&fd].records()[start..]
                    .iter()
                    .any(|record| {
                        record
                            .flags
                            .contains(::drm::control::AtomicCommitFlags::PAGE_FLIP_EVENT)
                            && record.events.iter().any(|event| event.crtc == output1_crtc)
                    }),
                "{row}: accepted Output 1 commit remains recorded"
            );
        }
        fixture.finish();
    }
}

#[test]
// Mutation rows: P19 omits mode-size equality; P20 omits pairwise separation.
fn output_geometry_admission() {
    use crate::kms::render::backend::{ScanoutM1OutputGeometry, scanout_m1_output_is_unoverlapped};

    let output = |x, width, mode_width| ScanoutM1OutputGeometry {
        x,
        y: 0,
        width,
        height: 1080,
        mode_width,
        mode_height: 1080,
    };
    let rows = [
        (
            "valid_separated_control",
            [output(0, 1920, 1920), output(1920, 1920, 1920)],
            true,
        ),
        // Mutation P19: omit selected output's logical-to-mode size equality.
        (
            "mode_size_mismatch",
            [output(0, 1920, 1920), output(1920, 1920, 1919)],
            false,
        ),
        // Mutation P20: omit pairwise output separation for Output scope.
        (
            "overlap",
            [output(0, 1920, 1920), output(960, 1920, 1920)],
            false,
        ),
    ];

    for (row, geometries, expected) in rows {
        let actual = scanout_m1_output_is_unoverlapped(1, &geometries);
        assert_eq!(
            actual, expected,
            "{row}: selected Output geometry admission result"
        );
    }
}

#[test]
// Mutation rows: P17 drops selected-CRTC ownership; P18 admits mixed-refresh Root.
fn scoped_crtc_admission() {
    use crate::kms::render::{
        backend::{DirectScope, KmsBackend},
        store::{DrawableKind, Storage},
    };
    use yserver_core::backend::{
        Backend, CompletedPresentEvent, PresentScanoutCandidate, PresentWake,
    };

    fn fixture(
        row: &str,
        present_id: u64,
        root: bool,
        selected_output: usize,
        output1_refresh: u32,
    ) -> (
        KmsBackend,
        PresentScanoutCandidate,
        CompletedPresentEvent,
        DirectScope,
        crate::kms::render::store::DrawableId,
    ) {
        let mut backend =
            KmsBackend::for_tests_with_direct_submitter(KmsBackend::submit_direct_frame_for_tests);
        push_test_output(&mut backend, 0x5a01);
        backend.platform.outputs[1].x = 800;
        backend.platform.outputs[1].output.picked.vrefresh = output1_refresh;
        backend.platform.fb_w = 1600;

        let target_xid = 0x5a10 + u32::try_from(present_id).expect("test Present id fits");
        let source_xid = target_xid + 1;
        let (x, width) = if root { (0, 1600) } else { (800, 800) };
        let old_target_id = seed_window(&mut backend, target_xid, None, x, 0);
        let geometry = backend
            .windows
            .get_mut(&target_xid)
            .expect("seeded candidate window exists");
        geometry.width = width;
        geometry.height = 600;
        backend.core.top_level_order.push(target_xid);
        backend.store.detach_xid(target_xid);
        backend.store_decref_with_invalidate(old_target_id);
        let target_id = backend
            .store
            .allocate(
                target_xid,
                DrawableKind::Window,
                24,
                true,
                Storage::for_tests_null(
                    ash::vk::Extent2D {
                        width: u32::from(width),
                        height: 600,
                    },
                    ash::vk::Format::B8G8R8A8_UNORM,
                ),
            )
            .expect("allocate null fallback storage");

        let mut source_storage = Storage::for_tests_null(
            ash::vk::Extent2D {
                width: u32::from(width),
                height: 600,
            },
            ash::vk::Format::B8G8R8A8_UNORM,
        );
        source_storage.imported_dmabuf = Some(imported_dmabuf_for_test(width, 600));
        let source_id = backend
            .store
            .allocate(source_xid, DrawableKind::Pixmap, 24, false, source_storage)
            .expect("allocate imported source storage");
        assert_eq!(
            backend
                .store
                .get(target_id)
                .map(|drawable| drawable.scene_participating),
            Some(true),
            "{row}: fallback window remains scene participating"
        );

        let output_crtcs = [
            u32::from(backend.platform.outputs[0].output.crtc),
            u32::from(backend.platform.outputs[1].output.crtc),
        ];
        bind_test_randr_crtc(&mut backend, 0, output_crtcs[0]);
        bind_test_randr_crtc(&mut backend, 1, output_crtcs[1]);
        let crtc_id = output_crtcs[selected_output];
        let crtc_epoch = backend.present_crtc_clock_epoch(crtc_id);
        let candidate = PresentScanoutCandidate {
            client_id: 1,
            client_pid: None,
            present_id,
            crtc_id,
            crtc_epoch,
            src_pixmap_xid: source_xid,
            dst_window_xid: target_xid,
            variable_refresh: false,
            src_host_xid: source_xid,
            paint_dst_host_xid: target_xid,
            completion_dst_host_xid: target_xid,
            src_width: width,
            src_height: 600,
            x_off: 0,
            y_off: 0,
            valid_region_xid: 0,
            update_region_xid: 0,
            update_is_full: true,
            explicit_sync: false,
            options: 0,
        };
        let event = CompletedPresentEvent {
            client_id: yserver_protocol::x11::ClientId(1),
            serial: u32::try_from(present_id).expect("test serial fits"),
            host_xid: source_xid,
            dst_host_xid: target_xid,
            options: 0,
            present_id,
            window_generation: 0,
            crtc_id,
            crtc_epoch,
            msc_offset: 0,
            completion_clock: None,
            wake: PresentWake::Pixmap { idle_fence_xid: 0 },
            completion_mode: yserver_protocol::x11::present::COMPLETE_MODE_FLIP,
            emit_idle: false,
        };
        Backend::set_cursor_hidden(&mut backend, true);
        backend.for_tests_set_scanout_m1_prober(record_scanout_m1_probe);
        Backend::note_present_scanout_candidate(&mut backend, candidate);
        let scope = if root {
            DirectScope::Root
        } else {
            DirectScope::Output(1)
        };
        let output_indices = match scope {
            DirectScope::Root => vec![0, 1],
            DirectScope::Output(output_idx) => vec![output_idx],
        };
        let planes: Vec<_> = output_indices
            .iter()
            .map(|&output_idx| {
                let layout = &backend.platform.outputs[output_idx];
                let (src_x, src_y) = match scope {
                    DirectScope::Root => (
                        u32::try_from(layout.x)
                            .unwrap_or_else(|_| panic!("{row}: Root output x is non-negative")),
                        u32::try_from(layout.y)
                            .unwrap_or_else(|_| panic!("{row}: Root output y is non-negative")),
                    ),
                    DirectScope::Output(_) => (0, 0),
                };
                crate::drm::modeset::DirectScanoutPlaneState {
                    output: &layout.output,
                    src_x,
                    src_y,
                    src_w: u32::from(layout.width),
                    src_h: u32::from(layout.height),
                    in_fence_fd: None,
                }
            })
            .collect();
        let controlled_m1 = record_scanout_m1_probe(&backend, source_id, scope, &planes)
            .unwrap_or_else(|error| panic!("{row}: controlled M1 result: {error}"));
        backend
            .pstate
            .scanout_m1
            .entries
            .insert((source_id, scope), controlled_m1);
        (backend, candidate, event, scope, source_id)
    }

    let probation = usize::from(crate::kms::render::backend::SCANOUT_M2_ELIGIBLE_ROOT_PROBATION);
    let rows = [
        // Mutation P17: output 1 must reject a candidate selected on output 0.
        ("wrong_selected_crtc", false, 0, 60, 0x5b01, false),
        ("valid_output_control", false, 1, 240, 0x5b02, true),
        // Mutation P18: mixed-refresh Root remains composed on this topology.
        ("mixed_refresh_root", true, 0, 240, 0x5b03, false),
    ];
    for (row, root, selected_output, output1_refresh, present_id, admitted) in rows {
        let (mut backend, candidate, event, scope, source_id) =
            fixture(row, present_id, root, selected_output, output1_refresh);
        assert!(
            backend
                .pstate
                .scanout_m1
                .entries
                .get(&(source_id, scope))
                .and_then(crate::kms::render::backend::ScanoutM1ProbeEntry::framebuffer)
                .is_some(),
            "{row}: controlled M1 result accepts this candidate scope"
        );
        if row == "mixed_refresh_root" {
            assert_ne!(
                backend.platform.outputs[0].output.picked.vrefresh,
                backend.platform.outputs[1].output.picked.vrefresh,
                "{row}: Root row uses mixed refresh rates"
            );
        }
        for attempt in 0..probation {
            let entered = Backend::try_present_direct(&mut backend, candidate, event.clone())
                .unwrap_or_else(|error| panic!("{row}: Present admission failed: {error}"));
            assert_eq!(
                entered,
                admitted && attempt + 1 == probation,
                "{row}: direct admission result at probation attempt {attempt}"
            );
        }

        if admitted {
            let owner = backend
                .pstate
                .direct
                .get(scope)
                .unwrap_or_else(|| panic!("{row}: valid Output scope exists"));
            assert!(owner.active(), "{row}: valid Output owns the pending frame");
            let pending = owner
                .pending
                .as_ref()
                .unwrap_or_else(|| panic!("{row}: pending direct frame"));
            assert_eq!(
                pending.candidate.present_id, present_id,
                "{row}: pending frame belongs to this Present"
            );
            assert_eq!(
                pending.completion_output_idx, 1,
                "{row}: completion is attributed to the selected Output"
            );
            assert_eq!(
                pending.awaiting_outputs,
                std::collections::HashSet::from([1]),
                "{row}: Output submission waits only for its own CRTC"
            );
            assert!(
                backend
                    .pstate
                    .scanout_m1
                    .entries
                    .get(&(source_id, scope))
                    .and_then(crate::kms::render::backend::ScanoutM1ProbeEntry::framebuffer)
                    .is_some(),
                "{row}: controlled M1 probe accepted this source and scope"
            );
        } else {
            let owner = backend
                .pstate
                .direct
                .get(scope)
                .unwrap_or_else(|| panic!("{row}: rejected candidate scope exists"));
            assert!(!owner.active(), "{row}: rejected scope has no direct owner");
            assert!(
                owner.pending.is_none() && owner.current.is_none(),
                "{row}: rejected scope retains no submitted or current frame"
            );
            assert!(
                !backend.pstate.direct.root().active(),
                "{row}: rejected candidate does not create Root ownership"
            );
        }
    }
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: key M1 results by DrawableId alone, so the second output reuses the first probe.
fn output_probe_per_scope() {
    use yserver_core::{backend::Backend, host_x11::HostSubwindowConfig};

    let Some((mut fixture, mut direct)) = output_admission_fixture_with_geometry(
        0,
        815,
        (3840, 1080),
        &[(0, 0, 1920, 1080, 60), (1920, 0, 1920, 1080, 60)],
    ) else {
        eprintln!("SKIP output_probe_per_scope: Vulkan scene fixture unavailable");
        return;
    };
    let output1_crtc = u32::from(fixture.events.backend.platform.outputs[1].output.crtc);
    Backend::configure_subwindow(
        &mut fixture.events.backend,
        None,
        direct.target_xid,
        HostSubwindowConfig {
            x: Some(1920),
            ..Default::default()
        },
    )
    .expect("move the same window to output 1 through Backend");
    direct.candidate.crtc_id = output1_crtc;
    direct.candidate.crtc_epoch = fixture
        .events
        .backend
        .present_crtc_clock_epoch(output1_crtc);
    direct.event.crtc_id = output1_crtc;
    direct.event.crtc_epoch = direct.candidate.crtc_epoch;
    Backend::note_present_scanout_candidate(&mut fixture.events.backend, direct.candidate);

    let observations = scanout_m1_probe_observations();
    assert_eq!(
        observations,
        vec![
            (
                crate::kms::render::backend::DirectScope::Output(0),
                vec![(
                    u32::from(fixture.events.backend.platform.outputs[0].output.crtc),
                    0,
                    0,
                    1920,
                    1080,
                )],
            ),
            (
                crate::kms::render::backend::DirectScope::Output(1),
                vec![(output1_crtc, 0, 0, 1920, 1080)],
            ),
        ],
        "the same source is probed once for each output's plane at source origin (0, 0)",
    );
    assert!(
        fixture
            .events
            .backend
            .pstate
            .scanout_m1
            .entries
            .contains_key(&(
                direct.source_id,
                crate::kms::render::backend::DirectScope::Output(0),
            ))
    );
    assert!(
        fixture
            .events
            .backend
            .pstate
            .scanout_m1
            .entries
            .contains_key(&(
                direct.source_id,
                crate::kms::render::backend::DirectScope::Output(1),
            ))
    );

    Backend::free_pixmap(
        &mut fixture.events.backend,
        None,
        direct.candidate.src_host_xid,
    )
    .expect("retire source pixmap through Backend");
    let root = fixture.events.backend.core.window_id;
    let (root_width, root_height) = (
        fixture.events.backend.platform.fb_w,
        fixture.events.backend.platform.fb_h,
    );
    Backend::clear_area(
        &mut fixture.events.backend,
        None,
        root,
        0,
        None,
        0,
        0,
        root_width,
        root_height,
        (0, 0),
    )
    .expect("submit pending source writes through Backend damage");
    Backend::maybe_composite(&mut fixture.events.backend)
        .expect("flush pending source writes through the scene scheduler");
    fixture.events.wait_for_submitted_gpu_work();
    for _ in 0..32 {
        Backend::before_block(&mut fixture.events.backend);
    }
    assert!(
        fixture.events.backend.store.get(direct.source_id).is_none(),
        "Backend free and render retirement drop the drawable before its probe entries are checked"
    );
    assert!(
        !fixture
            .events
            .backend
            .pstate
            .scanout_m1
            .entries
            .keys()
            .any(|(source_id, _)| *source_id == direct.source_id)
    );
    fixture
        .extra_drawables
        .retain(|&xid| xid != direct.candidate.src_host_xid);
    fixture.finish();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: use global root-overlay emptiness instead of filtering to scope outputs.
fn root_overlay_on_other_output_does_not_block_output_admission() {
    use yserver_core::backend::Backend;

    let Some((mut other_overlay_fixture, other_direct)) = output_admission_fixture(1, 816) else {
        eprintln!(
            "SKIP root_overlay_on_other_output_does_not_block_output_admission: Vulkan scene fixture unavailable"
        );
        return;
    };
    draw_direct_admission_overlay(&mut other_overlay_fixture.events.backend, 817, 10);
    assert!(
        !other_overlay_fixture
            .events
            .backend
            .scene
            .root_overlay
            .is_empty()
    );
    assert!(
        other_overlay_fixture
            .events
            .backend
            .scene
            .root_overlay
            .apply_list_for_output((
                other_overlay_fixture.events.backend.platform.outputs[1].x,
                other_overlay_fixture.events.backend.platform.outputs[1].y,
                1920,
                1080,
            ))
            .is_empty()
    );
    for attempt in 0..crate::kms::render::backend::SCANOUT_M2_ELIGIBLE_ROOT_PROBATION {
        assert_eq!(
            Backend::try_present_direct(
                &mut other_overlay_fixture.events.backend,
                other_direct.candidate,
                other_direct.event.clone(),
            )
            .expect("unrelated root overlay does not block Output(1)"),
            attempt + 1 == crate::kms::render::backend::SCANOUT_M2_ELIGIBLE_ROOT_PROBATION,
        );
    }
    assert!(other_overlay_fixture.events.backend.pstate.direct.outputs[1].active());
    other_overlay_fixture.finish();

    let Some((mut selected_overlay_fixture, selected_direct)) = output_admission_fixture(1, 818)
    else {
        eprintln!(
            "SKIP root_overlay_on_other_output_does_not_block_output_admission: second Vulkan scene fixture unavailable"
        );
        return;
    };
    draw_direct_admission_overlay(&mut selected_overlay_fixture.events.backend, 819, 1930);
    assert!(
        !selected_overlay_fixture
            .events
            .backend
            .scene
            .root_overlay
            .apply_list_for_output((
                selected_overlay_fixture.events.backend.platform.outputs[1].x,
                selected_overlay_fixture.events.backend.platform.outputs[1].y,
                1920,
                1080,
            ))
            .is_empty()
    );
    for _ in 0..crate::kms::render::backend::SCANOUT_M2_ELIGIBLE_ROOT_PROBATION {
        assert!(
            !Backend::try_present_direct(
                &mut selected_overlay_fixture.events.backend,
                selected_direct.candidate,
                selected_direct.event.clone(),
            )
            .expect("overlay on Output(1) keeps Present composed")
        );
    }
    assert!(
        !selected_overlay_fixture
            .events
            .backend
            .pstate
            .direct
            .outputs[1]
            .active()
    );
    selected_overlay_fixture.finish();
}

#[test]
fn scanout_m2_only_authoritative_root_present_invalidates_direct_frame() {
    use crate::kms::render::backend::ScanoutM0Target;

    assert!(
        crate::kms::render::backend::scanout_m2_is_authoritative_root(ScanoutM0Target::Cow, true)
    );
    assert!(
        crate::kms::render::backend::scanout_m2_is_authoritative_root(
            ScanoutM0Target::CowDescendant,
            true
        )
    );
    assert!(
        !crate::kms::render::backend::scanout_m2_is_authoritative_root(
            ScanoutM0Target::CowDescendant,
            false
        )
    );
    assert!(
        crate::kms::render::backend::scanout_m2_is_authoritative_root(
            ScanoutM0Target::Unredirected,
            true
        )
    );
    assert!(
        !crate::kms::render::backend::scanout_m2_is_authoritative_root(
            ScanoutM0Target::Other,
            false
        )
    );
}

#[test]
fn scanout_direct_eligible_accepts_fullscreen_game_candidate() {
    assert!(crate::kms::render::backend::scanout_direct_eligible(
        true, true, true, true, true, true, 0, 0, 0
    ));
    assert!(!crate::kms::render::backend::scanout_direct_eligible(
        true, true, true, true, false, true, 0, 0, 0
    ));
    assert!(!crate::kms::render::backend::scanout_direct_eligible(
        true, true, false, true, true, true, 0, 0, 0
    ));
    assert!(!crate::kms::render::backend::scanout_direct_eligible(
        true, true, true, true, true, true, 1, 0, 0
    ));
}

/// A root-sized stage under the COW, as muffin lays it out.
pub(super) fn seed_cow_stage(
    b: &mut crate::kms::render::backend::KmsBackend,
    stage: u32,
) -> (u32, u32) {
    let cow = yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0;
    let (w, h) = (b.platform.fb_w, b.platform.fb_h);
    let root_window = b.core.window_id;
    seed_window(b, cow, Some(root_window), 0, 0);
    seed_window(b, stage, Some(cow), 0, 0);
    for xid in [cow, stage] {
        let geometry = b.windows.get_mut(&xid).unwrap();
        geometry.width = w;
        geometry.height = h;
    }
    (u32::from(w), u32::from(h))
}

fn try_present_root_pixmap_for_shape_test(
    backend: &mut crate::kms::render::backend::KmsBackend,
    present_id: u64,
    source_xid: u32,
    target_xid: u32,
    x_off: i16,
) -> (bool, crate::kms::render::store::DrawableId) {
    use crate::kms::render::store::{DrawableKind, Storage};
    use yserver_core::backend::{
        Backend, CompletedPresentEvent, PresentScanoutCandidate, PresentWake,
    };

    let width = backend.platform.fb_w;
    let height = backend.platform.fb_h;
    let source_id = backend
        .store
        .allocate(
            source_xid,
            DrawableKind::Pixmap,
            24,
            false,
            Storage::for_tests_null(
                ash::vk::Extent2D {
                    width: u32::from(width),
                    height: u32::from(height),
                },
                ash::vk::Format::B8G8R8A8_UNORM,
            ),
        )
        .expect("allocate root-sized test Present pixmap");
    let crtc_id = u32::from(backend.platform.outputs[0].output.crtc);
    bind_test_randr_crtc(backend, 0, crtc_id);
    let crtc_epoch = backend.present_crtc_clock_epoch(crtc_id);
    let candidate = PresentScanoutCandidate {
        client_id: 1,
        present_id,
        crtc_id,
        crtc_epoch,
        src_pixmap_xid: source_xid,
        dst_window_xid: target_xid,
        variable_refresh: false,
        client_pid: None,
        src_host_xid: source_xid,
        paint_dst_host_xid: target_xid,
        completion_dst_host_xid: target_xid,
        src_width: width,
        src_height: height,
        x_off,
        y_off: 0,
        valid_region_xid: 0,
        update_region_xid: 0,
        update_is_full: true,
        explicit_sync: false,
        options: 0,
    };
    let event = CompletedPresentEvent {
        client_id: yserver_protocol::x11::ClientId(1),
        serial: u32::try_from(present_id).expect("test Present serial fits"),
        host_xid: source_xid,
        dst_host_xid: target_xid,
        options: 0,
        present_id,
        window_generation: 1,
        crtc_id,
        crtc_epoch,
        msc_offset: 0,
        completion_clock: None,
        wake: PresentWake::Pixmap { idle_fence_xid: 0 },
        completion_mode: yserver_protocol::x11::present::COMPLETE_MODE_COPY,
        emit_idle: true,
    };
    let presented = Backend::try_present_direct(backend, candidate, event)
        .expect("try root-sized Present through the backend entry point");
    (presented, source_id)
}

#[test]
// Mutation: make the hidden-shape predicate return false for every chain.
fn direct_shape_chain_hides_root_only_when_a_shape_misses_the_root() {
    use yserver_protocol::x11::xfixes::RegionRect;

    let cow = yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0;
    let stage = 0x0040_0013;
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let root = seed_cow_stage(&mut b, stage);
    let (w, h) = (
        u16::try_from(root.0).unwrap(),
        u16::try_from(root.1).unwrap(),
    );
    let rect = |x: i16, width: u16| RegionRect {
        x,
        y: 0,
        width,
        height: h,
    };
    let root_width_i16 = i16::try_from(w).unwrap();
    let root_rect = b.direct_scope_rect(crate::kms::render::backend::DirectScope::Root);
    let hides = |b: &crate::kms::render::backend::KmsBackend| {
        b.direct_shape_chain_hides_root(stage, root_rect)
    };

    assert!(!hides(&b), "unshaped chain");
    b.core.shape_bounding.insert(cow, Vec::new());
    assert!(hides(&b), "empty COW Bounding shape");
    b.core.shape_bounding.remove(&cow);
    b.core.shape_clip.insert(stage, Vec::new());
    assert!(hides(&b), "empty stage Clip shape");
    b.core.shape_clip.remove(&stage);
    b.core
        .shape_bounding
        .insert(cow, vec![rect(root_width_i16, w)]);
    assert!(hides(&b), "COW Bounding rect entirely right of root");
    b.core.shape_bounding.insert(cow, vec![rect(0, w / 2)]);
    assert!(!hides(&b), "partial COW Bounding shape");
    b.core.shape_bounding.insert(cow, vec![rect(0, w)]);
    assert!(!hides(&b), "root-sized COW Bounding shape");

    b.core.shape_bounding.remove(&cow);
    b.windows.get_mut(&stage).unwrap().x = root_width_i16;
    b.core
        .shape_clip
        .insert(stage, vec![rect(-root_width_i16, w)]);
    assert!(
        !hides(&b),
        "translated stage Clip lands on root; skipping translation would hide it"
    );
    b.core.shape_clip.insert(stage, vec![rect(0, w)]);
    assert!(
        hides(&b),
        "translated stage Clip lands right of root; skipping translation would show it"
    );
}

#[test]
// Mutation: a shaped-away COW stage Present still requests a direct unflip.
fn hidden_cow_stage_present_keeps_unredirected_direct_frame() {
    use yserver_core::backend::Backend;

    let cow = yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0;
    let (stage, client, client_source, stage_source) =
        (0x0040_0023, 0x0040_0027, 0x0040_002b, 0x0040_002f);
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let (w, h) = seed_cow_stage(&mut b, stage);
    seed_window(&mut b, client, None, 0, 0);
    b.windows.get_mut(&client).unwrap().width = b.platform.fb_w;
    b.windows.get_mut(&client).unwrap().height = b.platform.fb_h;
    seed_window(&mut b, client_source, None, 0, 0);
    retain_direct_frame_from_source_test(
        &mut b,
        client_source,
        client,
        u16::try_from(w).unwrap(),
        u16::try_from(h).unwrap(),
    );
    let current_source = b.pstate.direct.root().current.as_ref().unwrap().source_id;
    let fallback_before = b.pstate.direct.root().unflip_fallback_source;

    Backend::set_shape_rectangles(&mut b, None, cow, 0, Some(&[]))
        .expect("shape COW Bounding region empty");
    let (presented, stage_source_id) =
        try_present_root_pixmap_for_shape_test(&mut b, 81, stage_source, stage, 0);

    assert!(!presented, "a hidden stage Present is not direct-eligible");
    assert_eq!(
        b.pstate
            .direct
            .root()
            .current
            .as_ref()
            .map(|frame| frame.source_id),
        Some(current_source),
        "the fullscreen client's direct frame remains current"
    );
    assert!(!b.pstate.direct.root().unflip_requested);
    assert_ne!(
        b.pstate.direct.root().unflip_last_reason,
        Some("ineligible_authoritative_root_present")
    );
    assert_eq!(
        b.pstate.direct.root().unflip_fallback_source,
        fallback_before
    );
    assert_ne!(stage_source_id, current_source);
}

#[test]
// Mutation: classify a partial COW Bounding shape as hiding the root.
fn stage_present_is_authoritative_again_after_cow_shape_removed() {
    use yserver_core::backend::Backend;
    use yserver_protocol::x11::xfixes::RegionRect;

    let cow = yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0;
    for (partial_shape, (stage, client, client_source, stage_source), hidden_present_id) in [
        (
            false,
            (0x0040_0033, 0x0040_0037, 0x0040_003b, 0x0040_003f),
            82,
        ),
        (
            true,
            (0x0040_0043, 0x0040_0047, 0x0040_004b, 0x0040_004f),
            84,
        ),
    ] {
        let mut b = crate::kms::render::backend::KmsBackend::for_tests();
        let (w, h) = seed_cow_stage(&mut b, stage);
        seed_window(&mut b, client, None, 0, 0);
        b.windows.get_mut(&client).unwrap().width = b.platform.fb_w;
        b.windows.get_mut(&client).unwrap().height = b.platform.fb_h;
        seed_window(&mut b, client_source, None, 0, 0);
        retain_direct_frame_from_source_test(
            &mut b,
            client_source,
            client,
            u16::try_from(w).unwrap(),
            u16::try_from(h).unwrap(),
        );
        let current_source = b.pstate.direct.root().current.as_ref().unwrap().source_id;
        let fallback_before = b.pstate.direct.root().unflip_fallback_source;

        Backend::set_shape_rectangles(&mut b, None, cow, 0, Some(&[]))
            .expect("shape COW Bounding region empty");
        let (presented, hidden_source_id) = try_present_root_pixmap_for_shape_test(
            &mut b,
            hidden_present_id,
            stage_source,
            stage,
            0,
        );
        assert!(
            !presented,
            "the hidden stage Present is not direct-eligible"
        );
        assert_eq!(
            b.pstate
                .direct
                .root()
                .current
                .as_ref()
                .map(|frame| frame.source_id),
            Some(current_source),
            "the fullscreen client's direct frame remains current while hidden"
        );
        assert!(!b.pstate.direct.root().unflip_requested);
        assert_ne!(
            b.pstate.direct.root().unflip_last_reason,
            Some("ineligible_authoritative_root_present")
        );
        assert_eq!(
            b.pstate.direct.root().unflip_fallback_source,
            fallback_before
        );
        assert_ne!(hidden_source_id, current_source);

        if partial_shape {
            Backend::set_shape_rectangles(
                &mut b,
                None,
                cow,
                0,
                Some(&[RegionRect {
                    x: 0,
                    y: 0,
                    width: u16::try_from(w).unwrap() / 2,
                    height: u16::try_from(h).unwrap(),
                }]),
            )
            .expect("set partial COW Bounding shape");
        } else {
            Backend::set_shape_rectangles(&mut b, None, cow, 0, None)
                .expect("remove COW Bounding shape");
        }
        let (presented, source_id) = try_present_root_pixmap_for_shape_test(
            &mut b,
            hidden_present_id + 1,
            stage_source + 1,
            stage,
            1,
        );

        assert!(
            !presented,
            "the independent x offset gate rejects this Present"
        );
        assert!(b.pstate.direct.root().unflip_requested);
        assert_eq!(
            b.pstate.direct.root().unflip_reason,
            Some("ineligible_authoritative_root_present")
        );
        assert_eq!(
            b.pstate.direct.root().unflip_fallback_source,
            Some(source_id)
        );
    }
}

#[test]
// Mutation: apply the COW hidden-shape authority rule to Unredirected targets.
fn hidden_unredirected_present_still_invalidates_direct_frame() {
    use yserver_core::backend::Backend;

    let (current_client, candidate_client, current_source, candidate_source) =
        (0x0040_0053, 0x0040_0057, 0x0040_005b, 0x0040_005f);
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let (w, h) = (b.platform.fb_w, b.platform.fb_h);
    for xid in [current_client, candidate_client] {
        seed_window(&mut b, xid, None, 0, 0);
        let geometry = b.windows.get_mut(&xid).unwrap();
        geometry.width = w;
        geometry.height = h;
    }
    seed_window(&mut b, current_source, None, 0, 0);
    retain_direct_frame_from_source_test(&mut b, current_source, current_client, w, h);
    Backend::set_shape_rectangles(&mut b, None, candidate_client, 0, Some(&[]))
        .expect("shape the candidate Unredirected window empty");

    let (presented, source_id) =
        try_present_root_pixmap_for_shape_test(&mut b, 85, candidate_source, candidate_client, 1);

    assert!(!presented);
    assert!(b.pstate.direct.root().unflip_requested);
    assert_eq!(
        b.pstate.direct.root().unflip_reason,
        Some("ineligible_authoritative_root_present")
    );
    assert_eq!(
        b.pstate.direct.root().unflip_fallback_source,
        Some(source_id)
    );
}

/// Xorg flips a Present only when the window's clip list is the root's
/// `winSize` (`present/present_scmd.c:102`), and a Bounding or Clip shape
/// on the window or any ancestor narrows that clip list (`SetWinSize`,
/// `dix/window.c:1713`; `miComputeClips`). Muffin's lock screen shapes
/// the COW to an EMPTY region while its stage keeps presenting.
#[test]
fn direct_shape_chain_requires_every_shape_to_cover_the_root() {
    use yserver_protocol::x11::xfixes::RegionRect;

    let cow = yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0;
    let stage = 0x0040_0003;
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let root = seed_cow_stage(&mut b, stage);
    let (w, h) = (
        u16::try_from(root.0).unwrap(),
        u16::try_from(root.1).unwrap(),
    );
    let rect = |x: i16, width: u16| RegionRect {
        x,
        y: 0,
        width,
        height: h,
    };
    let half = i16::try_from(w / 2).unwrap();
    let root_rect = b.direct_scope_rect(crate::kms::render::backend::DirectScope::Root);
    let covers = |b: &crate::kms::render::backend::KmsBackend| {
        b.direct_shape_chain_covers_root(stage, root_rect)
    };

    assert!(covers(&b), "unshaped chain");
    b.core.shape_bounding.insert(cow, Vec::new());
    assert!(!covers(&b), "empty COW Bounding shape clips the stage away");
    b.core.shape_bounding.insert(cow, vec![rect(0, w)]);
    assert!(covers(&b), "a root-sized COW shape");
    b.core
        .shape_bounding
        .insert(cow, vec![rect(0, w / 2), rect(half, w - w / 2)]);
    assert!(covers(&b), "two rects that tile the root");
    b.core.shape_bounding.insert(cow, vec![rect(0, w / 2)]);
    assert!(!covers(&b), "a hole punched in the COW");
    b.core.shape_bounding.remove(&cow);
    b.core.shape_clip.insert(stage, Vec::new());
    assert!(!covers(&b), "an empty Clip shape on the presented window");
    b.core.shape_clip.insert(stage, vec![rect(0, w)]);
    assert!(covers(&b));
    // Shape rects are window-relative: a stage shifted right by `half`
    // with a shape starting at `-half` still covers the root.
    b.windows.get_mut(&stage).unwrap().x = half;
    b.core.shape_clip.insert(stage, vec![rect(-half, w)]);
    assert!(covers(&b), "window-relative shape on an offset window");
    b.core.shape_clip.insert(stage, vec![rect(0, w)]);
    assert!(!covers(&b), "the same rect, not translated back");
}

/// The shape change itself must hand a direct frame back to the composed
/// scene: muffin shapes the COW and need not Present again before the
/// locker is expected on screen.
#[test]
fn an_empty_cow_bounding_shape_unflips_the_direct_stage_frame() {
    use yserver_core::backend::Backend;
    use yserver_protocol::x11::xfixes::RegionRect;

    let cow = yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0;
    let (stage, source) = (0x0040_0003, 0x0040_0007);
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let (w, h) = seed_cow_stage(&mut b, stage);
    seed_window(&mut b, source, None, 0, 0);
    let (w, h) = (u16::try_from(w).unwrap(), u16::try_from(h).unwrap());
    retain_direct_frame_from_source_test(&mut b, source, stage, w, h);
    assert!(b.pstate.direct.root().active());

    b.set_shape_rectangles(None, cow, 2, Some(&[])).unwrap();
    b.set_shape_rectangles(
        None,
        cow,
        0,
        Some(&[RegionRect {
            x: 0,
            y: 0,
            width: w,
            height: h,
        }]),
    )
    .unwrap();
    assert!(
        !b.pstate.direct.root().unflip_requested,
        "an input shape or a root-covering bounding shape keeps the flip"
    );

    b.set_shape_rectangles(None, cow, 0, Some(&[])).unwrap();
    assert!(b.pstate.direct.root().unflip_requested);
    assert_eq!(
        b.pstate.direct.root().unflip_reason,
        Some("shape_clips_direct_frame")
    );
}

/// #133 step 3 (3.5) — a bordered paint chain is rejected outright,
/// however perfect the rest of the candidate is: the flip path
/// assumes content at storage (0, 0) and bordered storage starts at
/// the window's OUTER origin (`composite/compalloc.c:610`).
#[test]
fn scanout_direct_eligible_rejects_bordered_candidate() {
    assert!(!crate::kms::render::backend::scanout_direct_eligible(
        true, true, true, true, true, false, 0, 0, 0
    ));
}

#[test]
fn scanout_m2_authoritative_root_accepts_unredirected_fullscreen() {
    use crate::kms::render::backend::ScanoutM0Target;
    assert!(
        crate::kms::render::backend::scanout_m2_is_authoritative_root(
            ScanoutM0Target::Unredirected,
            true
        )
    );
    assert!(
        !crate::kms::render::backend::scanout_m2_is_authoritative_root(
            ScanoutM0Target::Unredirected,
            false
        )
    );
}

#[test]
fn scanout_m2_requires_stable_eligible_root_stream_before_entry() {
    let mut state = crate::kms::render::backend::ScanoutM2State::new();
    for _ in 1..crate::kms::render::backend::SCANOUT_M2_ELIGIBLE_ROOT_PROBATION {
        assert!(!state.admit_eligible_direct());
    }
    assert!(state.admit_eligible_direct());
    assert!(state.admit_eligible_direct(), "admission remains saturated");

    state.reset_eligible_direct_probation();
    assert!(!state.admit_eligible_direct());
}

#[test]
fn scanout_m2_ineligible_root_resets_short_eligible_bursts() {
    let mut state = crate::kms::render::backend::ScanoutM2State::new();
    for _ in 0..16 {
        for _ in 0..(crate::kms::render::backend::SCANOUT_M2_ELIGIBLE_ROOT_PROBATION - 1) {
            assert!(!state.admit_eligible_direct());
        }
        state.reset_eligible_direct_probation();
    }
}

#[test]
fn active_direct_scanout_unflips_on_cursor_output_fallback() {
    let mut backend = crate::kms::render::backend::KmsBackend::for_tests();
    let fallback_id = seed_window(&mut backend, 0x6e00, None, 0, 0);
    install_direct_frame_for_target_test(&mut backend, 0x6e10, fallback_id, true);
    let affected_crtc = output_crtc_key(&backend, 0);
    backend.handle_cursor_move_outcome(crate::kms::render::platform::CursorMoveOutcome {
        ebusy_count: 0,
        fallback_changed: true,
        retry_required: false,
        affected_crtcs: std::collections::HashSet::from([affected_crtc]),
    });
    assert!(backend.pstate.direct.root().unflip_requested);
    assert!(!backend.pstate.direct.root().hold_direct);
}

#[test]
fn active_direct_scanout_unflips_and_wakes_for_cursor_rollback_retry() {
    let mut backend = crate::kms::render::backend::KmsBackend::for_tests();
    let fallback_id = seed_window(&mut backend, 0x6e20, None, 0, 0);
    install_direct_frame_for_target_test(&mut backend, 0x6e30, fallback_id, true);
    backend.scene.scene_structure_dirty = false;
    let affected_crtc = output_crtc_key(&backend, 0);
    backend.handle_cursor_move_outcome(crate::kms::render::platform::CursorMoveOutcome {
        ebusy_count: 0,
        fallback_changed: false,
        retry_required: true,
        affected_crtcs: std::collections::HashSet::from([affected_crtc]),
    });
    assert!(backend.scene.scene_structure_dirty);
    assert!(backend.pstate.direct.root().unflip_requested);
    assert!(!backend.pstate.direct.root().hold_direct);
}

#[test]
fn scanout_m0_classifies_cow_descendant_and_unredirected_targets() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    b.get_overlay_window(None).expect("materialize COW");
    let cow_xid = yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0;
    let cow_id = b.cow_id.expect("COW id");
    assert_eq!(
        b.scanout_m0_target(cow_xid, Some(cow_id), Some(cow_id)),
        crate::kms::render::backend::ScanoutM0Target::Cow,
    );

    let child_id = seed_window(&mut b, 0xF00D, Some(cow_xid), 0, 0);
    assert_eq!(
        b.scanout_m0_target(0xF00D, Some(child_id), Some(child_id)),
        crate::kms::render::backend::ScanoutM0Target::CowDescendant,
    );

    let ordinary_id = seed_window(&mut b, 0xCAFE, None, 0, 0);
    assert_eq!(
        b.scanout_m0_target(0xCAFE, Some(ordinary_id), Some(ordinary_id)),
        crate::kms::render::backend::ScanoutM0Target::Unredirected,
    );
    b.store.set_scene_participating(ordinary_id, false);
    assert_eq!(
        b.scanout_m0_target(0xCAFE, Some(ordinary_id), Some(ordinary_id)),
        crate::kms::render::backend::ScanoutM0Target::Other,
    );
}

#[test]
fn scanout_m0_steady_candidate_keeps_one_shape_and_source_record() {
    use crate::kms::render::store::{DrawableKind, Storage};
    use ash::vk;
    use yserver_core::backend::PresentScanoutCandidate;

    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let dst_id = seed_window(&mut b, 0xD57, None, 0, 0);
    let source_id = b
        .store
        .allocate(
            0x5AC,
            DrawableKind::Pixmap,
            24,
            false,
            Storage::for_tests_null(
                vk::Extent2D {
                    width: 100,
                    height: 100,
                },
                vk::Format::B8G8R8A8_UNORM,
            ),
        )
        .expect("source");
    let candidate = PresentScanoutCandidate {
        client_id: 1,
        client_pid: None,
        present_id: 1,
        crtc_id: 0,
        crtc_epoch: 0,
        src_pixmap_xid: 0x100,
        dst_window_xid: 0x200,
        variable_refresh: false,
        src_host_xid: 0x5AC,
        paint_dst_host_xid: 0xD57,
        completion_dst_host_xid: 0xD57,
        src_width: 100,
        src_height: 100,
        x_off: 0,
        y_off: 0,
        valid_region_xid: 0,
        update_region_xid: 0,
        update_is_full: true,
        explicit_sync: false,
        options: 0,
    };
    b.pstate.scanout_m0.interval_start = std::time::Instant::now();
    b.observe_scanout_m0(candidate);
    b.observe_scanout_m0(PresentScanoutCandidate {
        present_id: 2,
        ..candidate
    });

    assert_eq!(b.pstate.scanout_m0.last_shape_by_dst.len(), 1);
    assert_eq!(b.pstate.scanout_m0.recent_sources_by_dst[&0xD57].len(), 1);
    assert_eq!(b.pstate.scanout_m0.presents, 2);
    assert_eq!(b.store.lookup(0xD57), Some(dst_id));
    assert_eq!(b.store.lookup(0x5AC), Some(source_id));
}

/// Issue #146 — the dedup key must not move when only the region XIDs do.
///
/// `scanout_m0 shape` logs when the shape CHANGES, and the key used to
/// carry the Present valid/update region XIDs. A compositor creates a
/// fresh update region every frame (`0x4144dd` → `0x4144eb` → `0x4144f9`
/// across three consecutive Presents in the #146 report), so the key
/// changed on every Present by construction and the guard suppressed
/// nothing — one ~700-byte line per composited frame.
///
/// The sibling test above cannot catch this: it sends
/// `update_region_xid: 0` on both Presents, so the key is identical
/// either way and it passes before and after the fix.
///
/// Both directions are asserted. Identity must NOT move the key, or the
/// flood returns; presence must STILL move it, or the fix has simply
/// thrown the information away.
#[test]
fn scanout_m0_shape_ignores_region_xid_identity_but_not_presence() {
    use crate::kms::render::store::{DrawableKind, Storage};
    use ash::vk;
    use yserver_core::backend::PresentScanoutCandidate;

    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    seed_window(&mut b, 0xD57, None, 0, 0);
    b.store
        .allocate(
            0x5AC,
            DrawableKind::Pixmap,
            24,
            false,
            Storage::for_tests_null(
                vk::Extent2D {
                    width: 100,
                    height: 100,
                },
                vk::Format::B8G8R8A8_UNORM,
            ),
        )
        .expect("source");
    let base = PresentScanoutCandidate {
        client_id: 1,
        client_pid: None,
        present_id: 1,
        crtc_id: 0,
        crtc_epoch: 0,
        src_pixmap_xid: 0x100,
        dst_window_xid: 0x200,
        variable_refresh: false,
        src_host_xid: 0x5AC,
        paint_dst_host_xid: 0xD57,
        completion_dst_host_xid: 0xD57,
        src_width: 100,
        src_height: 100,
        x_off: 0,
        y_off: 0,
        // A real compositor's regions: present, and a different XID every
        // frame. These are the exact values from the #146 report.
        valid_region_xid: 0,
        update_region_xid: 0x0041_44dd,
        update_is_full: false,
        explicit_sync: false,
        options: 0,
    };

    b.observe_scanout_m0(base);
    let after_first = b.pstate.scanout_m0.last_shape_by_dst[&0xD57].clone();

    for (present_id, update) in [(2u64, 0x0041_44ebu32), (3, 0x0041_44f9)] {
        b.observe_scanout_m0(PresentScanoutCandidate {
            present_id,
            update_region_xid: update,
            ..base
        });
        assert_eq!(
            b.pstate.scanout_m0.last_shape_by_dst[&0xD57], after_first,
            "a fresh update-region XID must not count as a shape change \
                 (present {present_id}, update {update:#x}) — that is the \
                 per-frame log flood in #146",
        );
    }

    // ...but losing the region entirely IS a real change, so the key has
    // to notice. Otherwise the fix would just have dropped the signal.
    b.observe_scanout_m0(PresentScanoutCandidate {
        present_id: 4,
        update_region_xid: 0,
        update_is_full: true,
        ..base
    });
    assert_ne!(
        b.pstate.scanout_m0.last_shape_by_dst[&0xD57], after_first,
        "a Present that carries no update region at all is a different \
             shape and must still be logged",
    );
}

fn atomic_cursor_raw<T: ::drm::control::ResourceHandle>(value: u32) -> T {
    ::drm::control::from_u32(value).expect("nonzero test handle")
}

fn atomic_cursor_recording_sink(
    backend: &mut KmsBackend,
) -> std::rc::Rc<crate::drm::atomic::tests_support::RecordingAtomicCommitExecutor> {
    use crate::drm::atomic::{AtomicCommitSink, tests_support::RecordingAtomicCommitExecutor};

    let recorder = std::rc::Rc::new(RecordingAtomicCommitExecutor::default());
    backend.platform.devices[0].atomic_commit_sink =
        std::rc::Rc::new(AtomicCommitSink::new(recorder.clone()));
    recorder
}

fn assert_atomic_cursor_properties(
    record: &crate::drm::atomic::tests_support::AtomicCommitRecord,
    positions: &[crate::drm::atomic::CursorPlanePosition],
) {
    for position in positions {
        for (property, value) in [
            (position.crtc_x, i64::from(position.x) as u64),
            (position.crtc_y, i64::from(position.y) as u64),
        ] {
            assert!(
                record.properties.iter().any(|update| {
                    update.object == position.plane.into()
                        && update.property == property
                        && update.value == value
                }),
                "missing cursor property {property:?}={value} on plane {:?}: {:?}",
                position.plane,
                record.properties,
            );
        }
    }
}

struct AtomicCursorDirectPresent {
    candidate: yserver_core::backend::PresentScanoutCandidate,
    event: yserver_core::backend::CompletedPresentEvent,
    source_id: crate::kms::render::store::DrawableId,
    target_id: crate::kms::render::store::DrawableId,
}

fn atomic_cursor_scanout_backend() -> Option<KmsBackend> {
    atomic_cursor_scanout_backend_with_hidden_cursor(false)
}

fn atomic_cursor_hidden_scanout_backend() -> Option<KmsBackend> {
    atomic_cursor_scanout_backend_with_hidden_cursor(true)
}

fn atomic_cursor_scanout_backend_with_hidden_cursor(hidden: bool) -> Option<KmsBackend> {
    let mut backend = match KmsBackend::for_tests_with_vk() {
        Ok(backend) => backend,
        Err(error) => {
            eprintln!("skipping: no Vulkan test device: {error}");
            return None;
        }
    };
    push_test_output(&mut backend, 0x5401);
    backend.platform.fb_w = 1600;
    backend.platform.fb_h = 600;
    backend.platform.outputs[1].x = 800;
    backend.platform.outputs[1].y = 0;
    let layouts = backend.platform.scene_output_layouts();
    backend.scene = crate::kms::render::scene::SceneCompositor::new(&backend.platform, &layouts)
        .expect("construct live two-output scene");
    if !hidden {
        let cursor_xid = 0x7f10;
        backend.insert_cursor_record(cursor_xid, 1, 1, 0, 0, vec![0xff; 4]);
        let root = backend.core.window_id;
        yserver_core::backend::Backend::define_cursor(&mut backend, None, root, cursor_xid)
            .expect("select the fixture's visible root cursor");
        assert_eq!(backend.effective_cursor_xid, Some(cursor_xid));
        assert!(!backend.effective_cursor_hidden());
        backend.scene.for_tests_set_hw_cursor_mode();
    }
    backend.scene.scene_structure_dirty = false;
    for drawable in backend.store.armed_damaged_ids() {
        if let Some(snapshot) = backend.store.peek_presentation_damage(drawable) {
            backend.store.ack_presentation_damage(snapshot);
        }
    }
    backend.pstate.direct.root_mut().cursor_bound_all = true;
    backend.pstate.direct.root_mut().eligible_streak =
        crate::kms::render::backend::SCANOUT_M2_ELIGIBLE_ROOT_PROBATION - 1;
    Some(backend)
}

#[test]
#[ignore = "needs live Vulkan ICD"]
// Mutation: mark a cursor-only commit in flight before its ioctl result.
fn cursor_only_ebusy_leaves_position_for_the_next_direct_scene_transaction() {
    use yserver_core::{backend::Backend, server::ServerState};

    let Some(mut backend) = atomic_cursor_scanout_backend() else {
        return;
    };
    let recorder = atomic_cursor_recording_sink(&mut backend);
    let position = backend.platform.for_tests_seed_atomic_cursor_position(
        1,
        atomic_cursor_raw(0x9201),
        atomic_cursor_raw(0x9301),
        atomic_cursor_raw(0x9401),
        1100,
        170,
        0,
        0,
    );
    let device_key = position.device_key;
    let crtc = position.crtc;
    recorder.script_result(Err(std::io::Error::from_raw_os_error(libc::EBUSY)));
    let mut state = ServerState::new();

    Backend::maybe_composite(&mut backend).expect("cursor-only EBUSY scheduling pass");
    assert_eq!(recorder.records().len(), 1);
    assert_eq!(
        recorder.records()[0].kind,
        crate::drm::atomic::AtomicCommitKind::CursorOnly,
    );
    assert_eq!(
        recorder.records()[0].result,
        crate::drm::atomic::tests_support::ScriptedResult::Error {
            kind: std::io::ErrorKind::ResourceBusy,
            raw_os_error: Some(libc::EBUSY),
        },
    );
    assert_eq!(
        backend.platform.devices[0]
            .atomic_commit_sink
            .for_tests_ebusy_count(),
        1,
    );
    assert!(
        backend
            .platform
            .for_tests_atomic_position_is_unsent(device_key, crtc)
    );
    assert!(
        !backend
            .platform
            .for_tests_atomic_cursor_commit_is_in_flight(device_key, crtc)
    );
    assert!(backend.platform.cursor_retry_waits_for_flip(1));
    assert!(
        backend.platform.devices[0]
            .test_page_flip_event_source
            .as_ref()
            .unwrap()
            .queued_flip_count()
            == 0,
        "no synthetic flip is delivered before the next scene transaction",
    );

    let direct = prepare_atomic_cursor_direct_present(&mut backend, 81);
    assert!(
        Backend::try_present_direct(&mut backend, direct.candidate, direct.event,)
            .expect("direct scene transaction carries the unsent cursor position")
    );
    let records = recorder.records();
    assert_eq!(records.len(), 2);
    assert_eq!(
        records.iter().map(|record| record.kind).collect::<Vec<_>>(),
        vec![
            crate::drm::atomic::AtomicCommitKind::CursorOnly,
            crate::drm::atomic::AtomicCommitKind::SceneWithCursor,
        ],
    );
    assert_atomic_cursor_properties(&records[1], &[position]);
    assert!(
        !backend
            .platform
            .for_tests_atomic_position_is_unsent(device_key, crtc)
    );
    assert!(backend.platform.cursor_retry_waits_for_flip(1));

    retire_atomic_cursor_test_output(&mut backend, &mut state, 0, 1);
    retire_atomic_cursor_test_output(&mut backend, &mut state, 1, 1);
    assert!(!backend.platform.cursor_retry_waits_for_flip(1));
    assert_atomic_cursor_quiescent(&backend);
}
#[test]
#[ignore = "needs live Vulkan ICD"]
// Mutation: leave cursor CRTCs out of pending_pageflip_crtcs during RANDR quiesce.
fn randr_change_discards_an_inflight_cursor_flip_before_the_next_scene_flip() {
    use std::os::fd::{AsFd as _, AsRawFd as _};
    use yserver_core::backend::Backend;

    let Some(mut backend) = atomic_cursor_scanout_backend() else {
        return;
    };
    backend.platform.for_tests_accept_output_power_commits();
    let recorder = atomic_cursor_recording_sink(&mut backend);
    let position = backend.platform.for_tests_seed_atomic_cursor_position(
        0,
        atomic_cursor_raw(0x9801),
        atomic_cursor_raw(0x9901),
        atomic_cursor_raw(0x9a01),
        110,
        80,
        0,
        0,
    );

    Backend::maybe_composite(&mut backend).expect("accepted cursor-only commit");
    assert_eq!(recorder.records().len(), 1);
    assert_eq!(
        recorder.records()[0].kind,
        crate::drm::atomic::AtomicCommitKind::CursorOnly,
    );
    assert!(
        backend
            .platform
            .for_tests_atomic_cursor_commit_is_in_flight(position.device_key, position.crtc)
    );

    let event_source = std::rc::Rc::clone(
        backend.platform.devices[0]
            .test_page_flip_event_source
            .as_ref()
            .expect("recording page-flip event source"),
    );
    event_source.queue_flip(position.crtc, 51);
    event_source.delay_next_ready_checks(1);

    let connector = backend.platform.outputs[1].key.connector_name.clone();
    let output_id = Backend::randr_outputs_and_modes(&mut backend)
        .0
        .into_iter()
        .find(|output| output.name == connector)
        .expect("second output in RandR projection")
        .output_id;
    // Keep the topology dark through this test boundary. The fixture has no
    // scanout framebuffer to re-light, and this test is about old-event
    // ownership across the RandR transition.
    backend.kms_outputs_active = false;
    assert!(
        Backend::apply_crtc_config(&mut backend, output_id, &connector, None, 0, 0,)
            .expect("disable second output through the topology entry point")
    );

    assert_eq!(
        event_source.queued_flip_count(),
        0,
        "the old cursor-only event must be discarded before CRTC reuse",
    );
    assert!(
        !backend
            .platform
            .for_tests_atomic_cursor_commit_is_in_flight(position.device_key, position.crtc)
    );
    assert_eq!(backend.platform.outputs.len(), 1);
    backend.kms_outputs_active = true;
    backend.scene.for_tests_set_hw_cursor_mode();

    let direct = prepare_atomic_cursor_direct_present_with_extent(&mut backend, 84, 800, 600);
    assert!(
        Backend::try_present_direct(&mut backend, direct.candidate, direct.event,)
            .expect("first post-RANDR scene transaction")
    );
    assert_eq!(
        recorder.records().len(),
        3,
        "discarding the old cursor event detaches its synthetic applied binding before the scene commit",
    );
    assert_eq!(
        recorder.records()[1].kind,
        crate::drm::atomic::AtomicCommitKind::CursorOnly,
        "topology cleanup detaches the old atomic cursor binding",
    );
    assert_eq!(
        recorder.records()[2].kind,
        crate::drm::atomic::AtomicCommitKind::Scene,
    );
    assert!(
        recorder.records()[2]
            .properties
            .iter()
            .all(|update| update.object != position.plane.into()),
        "topology discard must not resurrect cursor properties",
    );
    assert_eq!(event_source.queued_flip_count(), 0);

    event_source.queue_flip(position.crtc, 52);
    let drm_fd = backend.platform.devices[0].device.as_fd().as_raw_fd();
    let mut state = yserver_core::server::ServerState::new();
    Backend::on_page_flip_ready(&mut backend, &mut state, drm_fd);
    assert!(backend.pstate.direct.root().pending.is_none());
    assert_eq!(
        backend
            .pstate
            .direct
            .root()
            .current
            .as_ref()
            .map(|frame| frame.candidate.present_id),
        Some(84),
        "the new scene frame, rather than the old cursor flip, must retire",
    );
    assert!(
        !backend
            .platform
            .for_tests_atomic_cursor_commit_is_in_flight(position.device_key, position.crtc)
    );
    assert!(
        backend
            .platform
            .for_tests_atomic_cursor_pending_position(position.device_key, position.crtc)
            .is_none()
    );
}

fn prepare_atomic_cursor_direct_present(
    backend: &mut KmsBackend,
    present_id: u64,
) -> AtomicCursorDirectPresent {
    prepare_atomic_cursor_direct_present_with_extent(backend, present_id, 1600, 600)
}

fn prepare_atomic_cursor_direct_present_with_extent(
    backend: &mut KmsBackend,
    present_id: u64,
    width: u16,
    height: u16,
) -> AtomicCursorDirectPresent {
    let mut storage = crate::kms::render::store::Storage::for_tests_null(
        ash::vk::Extent2D {
            width: u32::from(width),
            height: u32::from(height),
        },
        ash::vk::Format::B8G8R8A8_UNORM,
    );
    storage.imported_dmabuf = Some(imported_dmabuf_for_test(width, height));
    prepare_atomic_cursor_direct_present_with_storage(backend, present_id, width, height, storage)
}

fn prepare_atomic_cursor_direct_present_with_storage(
    backend: &mut KmsBackend,
    present_id: u64,
    width: u16,
    height: u16,
    storage: crate::kms::render::store::Storage,
) -> AtomicCursorDirectPresent {
    use yserver_core::backend::{CompletedPresentEvent, PresentScanoutCandidate, PresentWake};

    use crate::kms::render::store::DrawableKind;

    let suffix = u32::try_from(present_id).expect("test Present id fits in XID");
    let target_xid = 0x7300 + suffix * 0x10;
    let source_xid = target_xid + 1;
    let target_id = seed_window(backend, target_xid, None, 0, 0);
    {
        let target = backend
            .windows
            .get_mut(&target_xid)
            .expect("fullscreen test target");
        target.width = width;
        target.height = height;
    }
    backend.core.top_level_order.push(target_xid);
    let source_id = backend
        .store
        .allocate(source_xid, DrawableKind::Pixmap, 24, false, storage)
        .expect("allocate fullscreen Present source");
    backend.pstate.scanout_m1.entries.insert(
        (source_id, crate::kms::render::backend::DirectScope::Root),
        crate::kms::render::backend::ScanoutM1ProbeEntry::accepted(
            crate::drm::modeset::DirectScanoutProbeFramebuffer::for_tests(atomic_cursor_raw(
                0x8100 + suffix,
            )),
        ),
    );

    let crtc_id = u32::from(backend.platform.outputs[0].output.crtc);
    bind_test_randr_crtc(backend, 0, crtc_id);
    let crtc_epoch = backend.present_crtc_clock_epoch(crtc_id);
    let candidate = PresentScanoutCandidate {
        client_id: 1,
        client_pid: None,
        present_id,
        crtc_id,
        crtc_epoch,
        src_pixmap_xid: source_xid,
        dst_window_xid: target_xid,
        variable_refresh: false,
        src_host_xid: source_xid,
        paint_dst_host_xid: target_xid,
        completion_dst_host_xid: target_xid,
        src_width: width,
        src_height: height,
        x_off: 0,
        y_off: 0,
        valid_region_xid: 0,
        update_region_xid: 0,
        update_is_full: true,
        explicit_sync: false,
        options: 0,
    };
    let event = CompletedPresentEvent {
        client_id: yserver_protocol::x11::ClientId(1),
        serial: u32::try_from(present_id).expect("test serial fits"),
        host_xid: source_xid,
        dst_host_xid: target_xid,
        options: 0,
        present_id,
        window_generation: 0,
        crtc_id,
        crtc_epoch,
        msc_offset: 0,
        completion_clock: None,
        wake: PresentWake::Pixmap { idle_fence_xid: 0 },
        completion_mode: yserver_protocol::x11::present::COMPLETE_MODE_COPY,
        emit_idle: true,
    };
    AtomicCursorDirectPresent {
        candidate,
        event,
        source_id,
        target_id,
    }
}

#[test]
#[ignore = "needs live Vulkan ICD"]
// Mutation: keep the M1/M2 direct gate or composite unflip gate Hw-only.
fn hidden_cursor_allows_fullscreen_direct_scanout_without_unflip() {
    use yserver_core::backend::Backend;

    let Some(mut backend) = atomic_cursor_hidden_scanout_backend() else {
        return;
    };
    let recorder = atomic_cursor_recording_sink(&mut backend);
    assert_eq!(
        backend.scene.cursor_mode(),
        crate::kms::render::scene::CursorPlaneMode::Hidden
    );

    // Exercise XFIXES HideCursor through the backend contract while every
    // scene output is already retired in Hidden mode.
    Backend::set_cursor_hidden(&mut backend, true);
    assert!(backend.cursor_hidden);
    Backend::maybe_composite(&mut backend).expect("schedule hidden-cursor scene");

    let direct = prepare_atomic_cursor_direct_present(&mut backend, 91);
    backend.pstate.scanout_m1.remove(direct.source_id);
    Backend::note_present_scanout_candidate(&mut backend, direct.candidate);
    assert_eq!(
        backend.pstate.scanout_m0.m1_probe_reject, 1,
        "the M1 probe must reach its later geometry rejection after Hidden passes cursor eligibility"
    );
    assert_eq!(
        backend.pstate.scanout_m0.m1_gate_open, 1,
        "the M1 diagnostic must report Hidden as direct-compatible"
    );
    backend.pstate.scanout_m1.entries.insert(
        (
            direct.source_id,
            crate::kms::render::backend::DirectScope::Root,
        ),
        crate::kms::render::backend::ScanoutM1ProbeEntry::accepted(
            crate::drm::modeset::DirectScanoutProbeFramebuffer::for_tests(atomic_cursor_raw(
                0x9e01,
            )),
        ),
    );
    assert!(
        Backend::try_present_direct(&mut backend, direct.candidate, direct.event,)
            .expect("eligible fullscreen Present"),
        "a fully hidden cursor must not reject direct scanout"
    );
    assert!(backend.pstate.direct.root().pending.is_some());

    Backend::maybe_composite(&mut backend).expect("composite tick during direct scanout");
    assert!(
        !backend.pstate.direct.root().unflip_requested,
        "Hidden must not request a direct unflip"
    );
    assert_eq!(recorder.records().len(), 1);
}
fn seed_two_atomic_cursor_positions(
    backend: &mut KmsBackend,
    root_x: i32,
    root_y: i32,
) -> Vec<crate::drm::atomic::CursorPlanePosition> {
    (0..2)
        .map(|output_idx| {
            backend.platform.for_tests_seed_atomic_cursor_position(
                output_idx,
                atomic_cursor_raw(0x8200 + u32::try_from(output_idx).unwrap()),
                atomic_cursor_raw(0x8300 + u32::try_from(output_idx).unwrap()),
                atomic_cursor_raw(0x8400 + u32::try_from(output_idx).unwrap()),
                root_x,
                root_y,
                0,
                0,
            )
        })
        .collect()
}

fn retire_atomic_cursor_test_output(
    backend: &mut KmsBackend,
    state: &mut yserver_core::server::ServerState,
    output_idx: usize,
    frame: u32,
) {
    use std::os::fd::{AsFd as _, AsRawFd as _};
    use yserver_core::backend::Backend;

    let output = &backend.platform.outputs[output_idx].output;
    backend.platform.devices[0]
        .test_page_flip_event_source
        .as_ref()
        .expect("recording test page-flip source")
        .queue_flip(output.crtc, frame);
    let drm_fd = backend.platform.devices[0].device.as_fd().as_raw_fd();
    Backend::on_page_flip_ready(backend, state, drm_fd);
}

fn assert_atomic_cursor_quiescent(backend: &KmsBackend) {
    for layout in &backend.platform.outputs {
        assert!(
            !backend
                .platform
                .for_tests_atomic_cursor_commit_is_in_flight(
                    layout.key.device_key,
                    layout.output.crtc,
                ),
            "cursor-only commit remains in flight on {:?}",
            layout.output.crtc,
        );
        assert!(
            backend
                .platform
                .for_tests_atomic_cursor_pending_position(
                    layout.key.device_key,
                    layout.output.crtc,
                )
                .is_none(),
            "unsent cursor position remains on {:?}",
            layout.output.crtc,
        );
    }
}

fn assert_no_immediate_cursor_wakeup(backend: &KmsBackend) {
    let deadline = yserver_core::backend::Backend::next_wakeup(backend);
    assert!(
        deadline.is_none_or(|deadline| deadline > std::time::Instant::now()),
        "blocked direct/cursor work must wait for its flip event, got {deadline:?}",
    );
}

fn draw_atomic_cursor_test_root_overlay(backend: &mut KmsBackend, client_id: u32) {
    use yserver_core::backend::{Backend, DrawState, GcFunction, OriginContext, SubwindowMode};

    let origin = Some(OriginContext {
        client_id: yserver_protocol::x11::ClientId(client_id),
        nested_seq: 0,
        opcode: 70,
    });
    let state = DrawState {
        function: GcFunction::Invert,
        subwindow_mode: SubwindowMode::IncludeInferiors,
        ..DrawState::default()
    };
    let root = backend.core.window_id;
    Backend::apply_draw_state(backend, origin, &state).expect("set root-overlay GC state");
    Backend::fill_rectangle(backend, origin, root, u32::MAX, 10, 10, 50, 1)
        .expect("draw reversible root overlay");
}

#[test]
#[ignore = "needs live Vulkan ICD"]
// Mutation: allow cursor-only submission during a pending direct frame or
// remove the grouped-unflip cursor blocker. Either sends an extra commit.
fn direct_successor_waits_for_retirement_and_carries_motion() {
    use yserver_core::{backend::Backend, server::ServerState};

    let Some(mut backend) = atomic_cursor_scanout_backend() else {
        return;
    };
    let recorder = atomic_cursor_recording_sink(&mut backend);
    let initial_positions = seed_two_atomic_cursor_positions(&mut backend, 850, 140);
    let mut state = ServerState::new();
    let first = prepare_atomic_cursor_direct_present(&mut backend, 51);
    let first_source_refcount = backend.store.get(first.source_id).unwrap().refcount;
    assert!(
        Backend::try_present_direct(&mut backend, first.candidate, first.event,)
            .expect("initial direct Present")
    );
    assert_eq!(recorder.records().len(), 1);
    assert_atomic_cursor_properties(&recorder.records()[0], &initial_positions);

    let successor = prepare_atomic_cursor_direct_present(&mut backend, 52);
    assert_eq!(
        backend
            .pstate
            .direct
            .root()
            .pending
            .as_ref()
            .expect("direct pending")
            .awaiting_outputs
            .len(),
        2,
    );
    assert!(
        Backend::try_present_direct(&mut backend, successor.candidate, successor.event,)
            .expect("retain direct successor")
    );
    assert_eq!(
        backend
            .pstate
            .direct
            .root()
            .queued_successor
            .as_ref()
            .map(|frame| frame.candidate.present_id),
        Some(52),
    );

    Backend::warp_pointer_root(&mut backend, &mut state, 1120, 210);
    let latest_positions = backend
        .platform
        .cursor_plane_atomic_positions_for_outputs(&[0, 1]);
    assert_eq!(latest_positions.len(), 2);
    assert_no_immediate_cursor_wakeup(&backend);
    Backend::maybe_composite(&mut backend).expect("direct hold scheduling pass");
    assert_eq!(
        recorder.records().len(),
        1,
        "motion waits behind the pending direct transaction",
    );

    retire_atomic_cursor_test_output(&mut backend, &mut state, 0, 1);
    Backend::maybe_composite(&mut backend).expect("partial direct retirement pass");
    assert_eq!(recorder.records().len(), 1);
    retire_atomic_cursor_test_output(&mut backend, &mut state, 1, 1);

    let records = recorder.records();
    assert_eq!(records.len(), 2);
    assert_eq!(
        records.iter().map(|record| record.kind).collect::<Vec<_>>(),
        vec![
            crate::drm::atomic::AtomicCommitKind::SceneWithCursor,
            crate::drm::atomic::AtomicCommitKind::SceneWithCursor,
        ],
    );
    assert_atomic_cursor_properties(&records[1], &latest_positions);
    assert_eq!(
        backend
            .pstate
            .direct
            .root()
            .pending
            .as_ref()
            .map(|frame| frame.candidate.present_id),
        Some(52),
    );
    assert_atomic_cursor_quiescent(&backend);

    retire_atomic_cursor_test_output(&mut backend, &mut state, 0, 2);
    retire_atomic_cursor_test_output(&mut backend, &mut state, 1, 2);
    // A Copy completion is ready only once its GPU signal has executed; wait for the
    // device (bounded by the driver) instead of racing a slow software ICD.
    backend.platform.wait_idle_bounded();
    let completed = Backend::drain_completed_present_events(&mut backend);
    assert_eq!(
        completed
            .iter()
            .map(|event| event.present_id)
            .collect::<Vec<_>>(),
        vec![51, 52],
    );
    assert_eq!(
        backend.store.get(first.source_id).unwrap().refcount,
        first_source_refcount,
        "replacing the predecessor releases its scanout pin",
    );

    // Keep the same direct frame current and exercise the composed-unflip
    // blocker. Cursor motion while the unflip waits must be carried by the
    // grouped replacement instead of starting another cursor-only stream.
    backend.platform.scanout_pools.resize_with(2, || None);
    backend.platform.bo_generations.resize_with(2, Vec::new);
    let vk = std::sync::Arc::clone(
        backend
            .platform
            .gpu
            .vk
            .as_ref()
            .expect("test fixture Vulkan context"),
    );
    let kms_device = std::rc::Rc::clone(&backend.platform.devices[0].device);
    for (output_idx, fb) in [(0, 0x8801), (1, 0x8802)] {
        let layout = &backend.platform.outputs[output_idx];
        let pool = crate::kms::vk::scanout::ScanoutBoPool::for_tests_on_screen_framebuffer(
            std::sync::Arc::clone(&vk),
            std::rc::Rc::clone(&kms_device),
            layout.scanout_route,
            u32::from(layout.width),
            u32::from(layout.height),
            atomic_cursor_raw(fb),
        );
        backend.platform.scanout_pools[output_idx] =
            Some(crate::kms::vk::scanout::OutputScanout::Shared(pool));
    }
    Backend::warp_pointer_root(&mut backend, &mut state, 1160, 260);
    Backend::maybe_composite(&mut backend).expect("submit cursor-only commits on direct current");
    assert_eq!(recorder.records().len(), 4);
    assert_eq!(
        recorder.records()[2].kind,
        crate::drm::atomic::AtomicCommitKind::CursorOnly
    );
    assert_eq!(
        recorder.records()[3].kind,
        crate::drm::atomic::AtomicCommitKind::CursorOnly
    );

    draw_atomic_cursor_test_root_overlay(&mut backend, 2);
    assert!(!backend.scene.root_overlay.is_empty());
    backend.pstate.direct.root_mut().unflip_shadow_ready = true;
    Backend::warp_pointer_root(&mut backend, &mut state, 1320, 330);
    let unflip_positions = backend
        .platform
        .cursor_plane_atomic_positions_for_outputs(&[0, 1]);
    assert_no_immediate_cursor_wakeup(&backend);
    Backend::maybe_composite(&mut backend).expect("unflip waits for both cursor commits");
    assert_eq!(recorder.records().len(), 4);
    retire_atomic_cursor_test_output(&mut backend, &mut state, 0, 3);
    Backend::maybe_composite(&mut backend).expect("unflip still waits for the second CRTC");
    assert_eq!(recorder.records().len(), 4);
    retire_atomic_cursor_test_output(&mut backend, &mut state, 1, 3);
    Backend::maybe_composite(&mut backend).expect("submit grouped composed unflip");
    let records = recorder.records();
    assert_eq!(records.len(), 5);
    assert_eq!(
        records[4].kind,
        crate::drm::atomic::AtomicCommitKind::SceneWithCursor
    );
    assert_atomic_cursor_properties(&records[4], &unflip_positions);
    assert_eq!(
        backend.pstate.direct.root().unflip_outputs_to_replace,
        std::collections::HashSet::from([0, 1]),
    );
    assert_atomic_cursor_quiescent(&backend);

    retire_atomic_cursor_test_output(&mut backend, &mut state, 0, 4);
    retire_atomic_cursor_test_output(&mut backend, &mut state, 1, 4);
    assert!(!backend.pstate.direct.root().active());
    Backend::client_disconnected(&mut backend, yserver_protocol::x11::ClientId(2));
    assert_atomic_cursor_quiescent(&backend);
}

#[test]
#[ignore = "needs live Vulkan ICD"]
// Mutation: submit a cursor-only move on a free CRTC while the successor is
// queued, or omit the scheduling-pass resume after the last retirement.
fn cursor_retirement_resumes_a_direct_successor_on_the_last_crtc() {
    use yserver_core::{backend::Backend, server::ServerState};

    let Some(mut backend) = atomic_cursor_scanout_backend() else {
        return;
    };
    let recorder = atomic_cursor_recording_sink(&mut backend);
    let first_positions = seed_two_atomic_cursor_positions(&mut backend, 850, 140);
    let mut state = ServerState::new();
    let first = prepare_atomic_cursor_direct_present(&mut backend, 61);
    assert!(
        Backend::try_present_direct(&mut backend, first.candidate, first.event,)
            .expect("initial direct Present")
    );
    assert_atomic_cursor_properties(&recorder.records()[0], &first_positions);
    retire_atomic_cursor_test_output(&mut backend, &mut state, 0, 1);
    retire_atomic_cursor_test_output(&mut backend, &mut state, 1, 1);
    assert!(backend.pstate.direct.root().current.is_some());

    Backend::warp_pointer_root(&mut backend, &mut state, 980, 230);
    Backend::maybe_composite(&mut backend).expect("submit two cursor-only commits");
    assert_eq!(recorder.records().len(), 3);
    assert!(
        backend
            .platform
            .cursor_commit_in_flight_for_outputs(&[0, 1])
    );

    let successor = prepare_atomic_cursor_direct_present(&mut backend, 62);
    assert!(
        Backend::try_present_direct(&mut backend, successor.candidate, successor.event,)
            .expect("retain Present behind cursor commits")
    );
    assert_eq!(
        backend
            .pstate
            .direct
            .root()
            .queued_successor
            .as_ref()
            .unwrap()
            .candidate
            .present_id,
        62
    );

    Backend::warp_pointer_root(&mut backend, &mut state, 1230, 310);
    let latest_positions = backend
        .platform
        .cursor_plane_atomic_positions_for_outputs(&[0, 1]);
    assert_no_immediate_cursor_wakeup(&backend);
    Backend::maybe_composite(&mut backend).expect("wait behind both cursor commits");
    assert_eq!(recorder.records().len(), 3);

    retire_atomic_cursor_test_output(&mut backend, &mut state, 0, 2);
    Backend::maybe_composite(&mut backend).expect("one CRTC remains blocked");
    assert_eq!(recorder.records().len(), 3);
    assert!(backend.pstate.direct.root().queued_successor.is_some());

    retire_atomic_cursor_test_output(&mut backend, &mut state, 1, 2);
    Backend::maybe_composite(&mut backend).expect("last cursor retirement arbitration");
    let records = recorder.records();
    assert_eq!(records.len(), 4);
    assert_eq!(
        records[1].kind,
        crate::drm::atomic::AtomicCommitKind::CursorOnly
    );
    assert_eq!(
        records[2].kind,
        crate::drm::atomic::AtomicCommitKind::CursorOnly
    );
    assert_eq!(
        records[3].kind,
        crate::drm::atomic::AtomicCommitKind::SceneWithCursor
    );
    assert_atomic_cursor_properties(&records[3], &latest_positions);
    assert!(backend.pstate.direct.root().queued_successor.is_none());
    assert_eq!(
        backend
            .pstate
            .direct
            .root()
            .pending
            .as_ref()
            .map(|frame| frame.candidate.present_id),
        Some(62),
    );
    assert_atomic_cursor_quiescent(&backend);

    retire_atomic_cursor_test_output(&mut backend, &mut state, 0, 3);
    retire_atomic_cursor_test_output(&mut backend, &mut state, 1, 3);
    // A Copy completion is ready only once its GPU signal has executed; wait for the
    // device (bounded by the driver) instead of racing a slow software ICD.
    backend.platform.wait_idle_bounded();
    let completed = Backend::drain_completed_present_events(&mut backend);
    assert_eq!(
        completed
            .iter()
            .map(|event| event.present_id)
            .collect::<Vec<_>>(),
        vec![61, 62],
    );
    assert_atomic_cursor_quiescent(&backend);
}

#[test]
#[ignore = "needs live Vulkan ICD"]
// Mutation: retain a first-entry Present in `queued_successor` when any CRTC
// has a cursor-only commit in flight. It must return to the composed caller.
fn first_direct_entry_declines_behind_cursor_commit_without_retaining_state() {
    use yserver_core::{backend::Backend, server::ServerState};

    let Some(mut backend) = atomic_cursor_scanout_backend() else {
        return;
    };
    let recorder = atomic_cursor_recording_sink(&mut backend);
    let mut state = ServerState::new();
    let blocked_position = backend.platform.for_tests_seed_atomic_cursor_position(
        1,
        atomic_cursor_raw(0x8501),
        atomic_cursor_raw(0x8601),
        atomic_cursor_raw(0x8701),
        900,
        180,
        0,
        0,
    );
    Backend::maybe_composite(&mut backend).expect("submit cursor-only on the second output");
    assert_eq!(recorder.records().len(), 1);
    assert_eq!(
        recorder.records()[0].kind,
        crate::drm::atomic::AtomicCommitKind::CursorOnly
    );
    assert_atomic_cursor_properties(&recorder.records()[0], &[blocked_position]);

    let declined = prepare_atomic_cursor_direct_present(&mut backend, 71);
    let declined_source_refcount = backend.store.get(declined.source_id).unwrap().refcount;
    let declined_target_refcount = backend.store.get(declined.target_id).unwrap().refcount;
    assert!(
        !Backend::try_present_direct(&mut backend, declined.candidate, declined.event.clone(),)
            .expect("first direct entry declines")
    );
    assert!(backend.pstate.direct.root().current.is_none());
    assert!(backend.pstate.direct.root().pending.is_none());
    assert!(backend.pstate.direct.root().queued_successor.is_none());
    assert_eq!(
        backend.store.get(declined.source_id).unwrap().refcount,
        declined_source_refcount
    );
    assert_eq!(
        backend.store.get(declined.target_id).unwrap().refcount,
        declined_target_refcount
    );

    // This is the caller's normal composed completion path after `Ok(false)`.
    // Its completion stays an ordinary Copy completion and is not owned by
    // direct scanout.
    Backend::enqueue_present_completion(
        &mut backend,
        declined.event,
        declined.candidate.dst_window_xid,
    );

    draw_atomic_cursor_test_root_overlay(&mut backend, 1);
    assert!(!backend.scene.root_overlay.is_empty());
    retire_atomic_cursor_test_output(&mut backend, &mut state, 1, 1);

    let overlay_present = prepare_atomic_cursor_direct_present(&mut backend, 72);
    assert!(
        !Backend::try_present_direct(
            &mut backend,
            overlay_present.candidate,
            overlay_present.event,
        )
        .expect("root overlay keeps direct scanout off")
    );
    assert!(backend.pstate.direct.root().queued_successor.is_none());
    assert!(backend.pstate.direct.root().pending.is_none());

    Backend::client_disconnected(&mut backend, yserver_protocol::x11::ClientId(1));
    assert!(backend.scene.root_overlay.is_empty());
    // The root overlay correctly reset direct-scanout probation. Restore the
    // admitted fixture state so this last Present isolates cursor retirement.
    backend.pstate.direct.root_mut().eligible_streak =
        crate::kms::render::backend::SCANOUT_M2_ELIGIBLE_ROOT_PROBATION - 1;
    // A Copy completion is ready only once its GPU signal has executed; wait for the
    // device (bounded by the driver) instead of racing a slow software ICD.
    backend.platform.wait_idle_bounded();
    let completed = Backend::drain_completed_present_events(&mut backend);
    assert!(
        completed.iter().any(|event| {
            event.present_id == 71
                && event.completion_mode == yserver_protocol::x11::present::COMPLETE_MODE_COPY
        }),
        "the declined direct Present completes on the composed path"
    );

    let next_eligible = prepare_atomic_cursor_direct_present(&mut backend, 73);
    assert!(
        Backend::try_present_direct(&mut backend, next_eligible.candidate, next_eligible.event,)
            .expect("next eligible first entry can use direct scanout")
    );
    assert_eq!(recorder.records().len(), 2);
    assert_eq!(
        recorder.records()[1].kind,
        crate::drm::atomic::AtomicCommitKind::SceneWithCursor
    );
    assert_atomic_cursor_properties(&recorder.records()[1], &[blocked_position]);
    assert!(backend.pstate.direct.root().pending.is_some());
    assert!(backend.pstate.direct.root().queued_successor.is_none());
    assert_atomic_cursor_quiescent(&backend);
}

#[test]
#[ignore = "needs live Vulkan ICD"]
// Mutation: `submit_direct_frame` gathers an empty output list. The direct
// Present caller must record both CRTC-local cursor positions.
fn eligible_fullscreen_present_records_cursor_positions_for_both_outputs() {
    use yserver_core::backend::{
        Backend, CompletedPresentEvent, PresentScanoutCandidate, PresentWake,
    };

    let mut backend = match KmsBackend::for_tests_with_vk() {
        Ok(backend) => backend,
        Err(error) => {
            eprintln!("skipping: no Vulkan device: {error}");
            return;
        }
    };
    push_test_output(&mut backend, 0x5200);
    backend.platform.fb_w = 1600;
    backend.platform.outputs[1].x = 800;
    backend.platform.outputs[1].y = 100;
    let layouts = backend.platform.scene_output_layouts();
    backend.scene = crate::kms::render::scene::SceneCompositor::new(&backend.platform, &layouts)
        .expect("live SceneCompositor");
    let cursor_xid = 0x7f10;
    backend.insert_cursor_record(cursor_xid, 1, 1, 0, 0, vec![0xff; 4]);
    let root = backend.core.window_id;
    Backend::define_cursor(&mut backend, None, root, cursor_xid)
        .expect("select the fixture's visible root cursor");
    assert_eq!(backend.effective_cursor_xid, Some(cursor_xid));
    assert!(!backend.effective_cursor_hidden());
    backend.scene.for_tests_set_hw_cursor_mode();

    let recorder = atomic_cursor_recording_sink(&mut backend);
    let positions = [
        backend.platform.for_tests_seed_atomic_cursor_position(
            0,
            atomic_cursor_raw(51),
            atomic_cursor_raw(61),
            atomic_cursor_raw(62),
            850,
            140,
            5,
            10,
        ),
        backend.platform.for_tests_seed_atomic_cursor_position(
            1,
            atomic_cursor_raw(52),
            atomic_cursor_raw(63),
            atomic_cursor_raw(64),
            850,
            140,
            5,
            10,
        ),
    ];
    assert_eq!((positions[0].x, positions[0].y), (845, 130));
    assert_eq!((positions[1].x, positions[1].y), (45, 30));

    let target_xid = 0x6e00;
    let source_xid = target_xid + 1;
    seed_window(&mut backend, target_xid, None, 0, 0);
    let target = backend
        .windows
        .get_mut(&target_xid)
        .expect("fullscreen target");
    target.width = 1600;
    target.height = 600;
    backend.core.top_level_order.push(target_xid);
    let source_id = backend
        .store
        .allocate(
            source_xid,
            crate::kms::render::store::DrawableKind::Pixmap,
            24,
            false,
            {
                let mut storage = Storage::for_tests_null(
                    ash::vk::Extent2D {
                        width: 1600,
                        height: 600,
                    },
                    ash::vk::Format::B8G8R8A8_UNORM,
                );
                storage.imported_dmabuf = Some(imported_dmabuf_for_test(1600, 600));
                storage
            },
        )
        .expect("fullscreen Present source");
    backend.pstate.scanout_m1.entries.insert(
        (source_id, crate::kms::render::backend::DirectScope::Root),
        crate::kms::render::backend::ScanoutM1ProbeEntry::accepted(
            crate::drm::modeset::DirectScanoutProbeFramebuffer::for_tests(atomic_cursor_raw(71)),
        ),
    );
    backend.pstate.direct.root_mut().eligible_streak =
        crate::kms::render::backend::SCANOUT_M2_ELIGIBLE_ROOT_PROBATION - 1;

    let crtc_id = u32::from(backend.platform.outputs[0].output.crtc);
    bind_test_randr_crtc(&mut backend, 0, crtc_id);
    let candidate = PresentScanoutCandidate {
        client_id: 1,
        client_pid: None,
        present_id: 45,
        crtc_id,
        crtc_epoch: backend.present_crtc_clock_epoch(crtc_id),
        src_pixmap_xid: source_xid,
        dst_window_xid: target_xid,
        variable_refresh: false,
        src_host_xid: source_xid,
        paint_dst_host_xid: target_xid,
        completion_dst_host_xid: target_xid,
        src_width: 1600,
        src_height: 600,
        x_off: 0,
        y_off: 0,
        valid_region_xid: 0,
        update_region_xid: 0,
        update_is_full: true,
        explicit_sync: false,
        options: 0,
    };
    let event = CompletedPresentEvent {
        client_id: yserver_protocol::x11::ClientId(1),
        serial: 12,
        host_xid: source_xid,
        dst_host_xid: target_xid,
        options: 0,
        present_id: 45,
        window_generation: 0,
        crtc_id,
        crtc_epoch: candidate.crtc_epoch,
        msc_offset: 0,
        completion_clock: None,
        wake: PresentWake::Pixmap { idle_fence_xid: 0 },
        completion_mode: yserver_protocol::x11::present::COMPLETE_MODE_COPY,
        emit_idle: true,
    };

    assert!(
        Backend::try_present_direct(&mut backend, candidate, event)
            .expect("eligible fullscreen direct Present"),
        "the fullscreen Present should enter direct scanout"
    );
    let records = recorder.records();
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0].kind,
        crate::drm::atomic::AtomicCommitKind::SceneWithCursor
    );
    assert_atomic_cursor_properties(&records[0], &positions);
    assert!(backend.pstate.direct.root().pending.is_some());
    assert!(positions.iter().all(|position| {
        !backend
            .platform
            .for_tests_atomic_position_is_unsent(position.device_key, position.crtc)
    }));
}

#[test]
#[ignore = "needs live Vulkan ICD"]
// Mutation: `submit_composed_unflip` gathers an empty output list. The real
// shape-triggered unflip must record each output's local cursor coordinates.
fn shape_triggered_composed_unflip_records_cursor_positions_for_both_outputs() {
    use yserver_core::backend::Backend;

    let mut backend = match KmsBackend::for_tests_with_vk() {
        Ok(backend) => backend,
        Err(error) => {
            eprintln!("skipping: no Vulkan device: {error}");
            return;
        }
    };
    push_test_output(&mut backend, 0x5300);
    backend.platform.outputs[1].x = 800;
    backend.platform.outputs[1].y = 100;
    let output_count = backend.platform.outputs.len();
    backend
        .platform
        .scanout_pools
        .resize_with(output_count, || None);
    backend
        .platform
        .bo_generations
        .resize_with(output_count, Vec::new);
    let recorder = atomic_cursor_recording_sink(&mut backend);
    let positions = [
        backend.platform.for_tests_seed_atomic_cursor_position(
            0,
            atomic_cursor_raw(51),
            atomic_cursor_raw(61),
            atomic_cursor_raw(62),
            850,
            140,
            5,
            10,
        ),
        backend.platform.for_tests_seed_atomic_cursor_position(
            1,
            atomic_cursor_raw(52),
            atomic_cursor_raw(63),
            atomic_cursor_raw(64),
            850,
            140,
            5,
            10,
        ),
    ];
    assert_eq!((positions[0].x, positions[0].y), (845, 130));
    assert_eq!((positions[1].x, positions[1].y), (45, 30));

    let vk = std::sync::Arc::clone(
        backend
            .platform
            .gpu
            .vk
            .as_ref()
            .expect("test fixture Vulkan context"),
    );
    let kms_device = std::rc::Rc::clone(&backend.platform.devices[0].device);
    for (output_idx, fb) in [(0, 701), (1, 702)] {
        let layout = &backend.platform.outputs[output_idx];
        let pool = crate::kms::vk::scanout::ScanoutBoPool::for_tests_on_screen_framebuffer(
            std::sync::Arc::clone(&vk),
            std::rc::Rc::clone(&kms_device),
            layout.scanout_route,
            u32::from(layout.width),
            u32::from(layout.height),
            atomic_cursor_raw(fb),
        );
        backend.platform.scanout_pools[output_idx] =
            Some(crate::kms::vk::scanout::OutputScanout::Shared(pool));
    }

    let cow = yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0;
    let (stage, source) = (0x6f00_0003, 0x6f00_0007);
    let (width, height) = seed_cow_stage(&mut backend, stage);
    seed_window(&mut backend, source, None, 0, 0);
    retain_direct_frame_from_source_test(
        &mut backend,
        source,
        stage,
        u16::try_from(width).expect("test root width"),
        u16::try_from(height).expect("test root height"),
    );
    backend.pstate.direct.root_mut().unflip_shadow_ready = true;

    Backend::set_shape_rectangles(&mut backend, None, cow, 0, Some(&[]))
        .expect("empty COW bounding shape");
    assert!(backend.pstate.direct.root().unflip_requested);
    assert_eq!(
        backend.pstate.direct.root().unflip_reason,
        Some("shape_clips_direct_frame")
    );

    Backend::maybe_composite(&mut backend).expect("submit composed unflip");

    let records = recorder.records();
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0].kind,
        crate::drm::atomic::AtomicCommitKind::SceneWithCursor
    );
    assert_atomic_cursor_properties(&records[0], &positions);
    assert_eq!(
        backend.pstate.direct.root().unflip_outputs_to_replace,
        std::collections::HashSet::from([0, 1])
    );
    assert!(positions.iter().all(|position| {
        !backend
            .platform
            .for_tests_atomic_position_is_unsent(position.device_key, position.crtc)
    }));
}

#[test]
fn present_outcome_telemetry_counts_a_copy_present() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let stage = 0x0040_0021;
    seed_cow_stage(&mut b, stage);
    let (presented, _) = try_present_root_pixmap_for_shape_test(&mut b, 1, 0x0040_0121, stage, 0);
    assert!(!presented, "a first Present is inside M2 probation");
    let server = &b.pstate.scanout_m0.server;
    assert_eq!(
        (
            server.presents_copy,
            server.presents_direct_imported,
            server.presents_direct_server
        ),
        (1, 0, 0)
    );
}

#[test]
#[ignore = "needs live Vulkan ICD"]
fn present_outcome_telemetry_counts_an_imported_direct_present() {
    use yserver_core::backend::Backend;

    let Some(mut backend) = atomic_cursor_hidden_scanout_backend() else {
        return;
    };
    let _recorder = atomic_cursor_recording_sink(&mut backend);
    Backend::set_cursor_hidden(&mut backend, true);
    Backend::maybe_composite(&mut backend).expect("schedule hidden-cursor scene");

    let mut storage = crate::kms::render::store::Storage::for_tests_null(
        ash::vk::Extent2D {
            width: 1600,
            height: 600,
        },
        ash::vk::Format::B8G8R8A8_UNORM,
    );
    storage.imported_dmabuf = Some(crate::kms::render::store::ImportedDmabufMetadata {
        fourcc: 0x3432_5258,
        vk_format: ash::vk::Format::B8G8R8A8_UNORM,
        modifier: 0,
        implicit_layout: false,
        planes: vec![crate::kms::render::store::ImportedDmabufPlane {
            offset: 0,
            pitch: 1600 * 4,
        }],
        width: 1600,
        height: 600,
        depth: 24,
        bpp: 32,
    });
    let direct =
        prepare_atomic_cursor_direct_present_with_storage(&mut backend, 92, 1600, 600, storage);
    assert!(
        Backend::try_present_direct(&mut backend, direct.candidate, direct.event)
            .expect("eligible fullscreen Present"),
    );
    let server = &backend.pstate.scanout_m0.server;
    assert_eq!(
        (
            server.presents_direct_imported,
            server.presents_direct_server,
            server.presents_copy
        ),
        (1, 0, 0)
    );
}

#[path = "present_scanout_a3.rs"]
pub(crate) mod a3;
