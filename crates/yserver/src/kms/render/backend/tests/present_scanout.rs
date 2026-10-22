use super::*;

fn output_crtc_key(backend: &KmsBackend, output_idx: usize) -> CrtcKey {
    CrtcKey::for_output(&backend.platform.outputs[output_idx])
}

fn bind_test_randr_crtc(backend: &mut KmsBackend, output_idx: usize, crtc_id: u32) {
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

// ── Idle vblank arming (DRM_CRTC_QUEUE_SEQUENCE) ───────────────────────

#[test]
fn armed_vblank_targets_starts_empty() {
    let b = crate::kms::render::backend::KmsBackend::for_tests();
    assert!(b.pstate.armed_vblank_targets.is_empty());
    assert!(b.pstate.crtc_queue_sequence_unsupported_devices.is_empty());
}

#[test]
fn clear_all_armed_vblank_targets_empties_map() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let h = test_crtc_key(test_device_key(7), 7);
    b.pstate.armed_vblank_targets.insert(h, 0);
    b.clear_all_armed_vblank_targets();
    assert!(b.pstate.armed_vblank_targets.is_empty());
}

#[test]
fn prune_armed_targets_drops_stale_keeps_live() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let live = output_crtc_key(&b, 0);
    let stale = test_crtc_key(live.device_key, 999);
    b.pstate.armed_vblank_targets.insert(live, 0);
    b.pstate.armed_vblank_targets.insert(stale, 0);
    b.prune_armed_targets_to_live_outputs();
    assert!(b.pstate.armed_vblank_targets.contains_key(&live));
    assert!(!b.pstate.armed_vblank_targets.contains_key(&stale));
}

#[test]
fn prune_armed_targets_drops_stale_absolute_entries_keeps_live() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let live = output_crtc_key(&b, 0);
    let stale = test_crtc_key(live.device_key, 999);
    b.pstate
        .absolute_vblank_targets
        .entry(live)
        .or_default()
        .insert(50);
    b.pstate
        .absolute_vblank_targets
        .entry(stale)
        .or_default()
        .insert(50);
    b.prune_armed_targets_to_live_outputs();
    assert!(b.pstate.absolute_vblank_targets.contains_key(&live));
    assert!(!b.pstate.absolute_vblank_targets.contains_key(&stale));
}

#[test]
fn on_crtc_sequence_event_happy_path_records_msc_and_clears_arm() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let crtc = b.platform.outputs[0].output.crtc;
    let crtc_key = output_crtc_key(&b, 0);
    b.pstate.armed_vblank_targets.insert(crtc_key, 0);

    dispatch_test_sequence(
        &mut b,
        u64::from(u32::from(crtc)),
        1_500_000, /* 1.5ms */
        7,
    );

    assert!(
        b.pstate.armed_vblank_targets.is_empty(),
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
    b.pstate.armed_vblank_targets.insert(crtc_key, 0);
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
fn active_absolute_sequence_advances_completion_clock() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let crtc = b.platform.outputs[0].output.crtc;
    let crtc_id = u32::from(crtc);
    let crtc_key = output_crtc_key(&b, 0);
    b.pstate
        .absolute_vblank_targets
        .entry(crtc_key)
        .or_default()
        .insert(9);
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
        "tagged Present-target sequence must release due completions",
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
    );
    b.platform.record_completion_clock(
        crtc_key,
        PresentClockSample {
            msc: 20,
            ust: 2_001,
            source: PresentClockSource::PageFlip,
        },
    );
    b.platform.record_completion_clock(
        crtc_key,
        PresentClockSample {
            msc: 19,
            ust: 1_900,
            source: PresentClockSource::IdleSequence,
        },
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
    b.pstate.armed_vblank_targets.insert(crtc_key, 0);

    dispatch_test_sequence(&mut b, u64::from(u32::from(crtc)), -1, 7);

    assert!(
        b.pstate.armed_vblank_targets.is_empty(),
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
    b.pstate.armed_vblank_targets.insert(stale, 0);

    dispatch_test_sequence(&mut b, 999, 1_000_000, 5);

    assert!(
        b.pstate.armed_vblank_targets.is_empty(),
        "stale arm cleared so the CRTC can't strand"
    );
    assert_eq!(b.platform.present_get_ust_msc(stale), (0, 0));
}

#[test]
fn on_crtc_sequence_event_from_wrong_device_does_not_advance_clock() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let crtc = b.platform.outputs[0].output.crtc;
    let crtc_key = output_crtc_key(&b, 0);
    b.pstate.armed_vblank_targets.insert(crtc_key, 0);
    let wrong_device = test_device_key(99);

    b.on_crtc_sequence_event(wrong_device, u64::from(u32::from(crtc)), 1_000_000, 5);

    assert!(
        b.pstate.armed_vblank_targets.contains_key(&crtc_key),
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

    b.pstate.armed_vblank_targets.insert(primary, 0);
    b.pstate.armed_vblank_targets.insert(secondary, 0);
    b.on_crtc_sequence_event(secondary.device_key, u64::from(raw_crtc), 3_000_000, 17);

    assert!(
        b.pstate.armed_vblank_targets.contains_key(&primary),
        "the event must not retire the equal raw CRTC on the primary card"
    );
    assert!(
        !b.pstate.armed_vblank_targets.contains_key(&secondary),
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
    assert!(!b.pstate.armed_vblank_targets.contains_key(&primary));
    assert!(b.pstate.armed_vblank_targets.contains_key(&secondary));

    b.pstate
        .crtc_queue_sequence_unsupported_devices
        .insert(primary.device_key);
    assert!(!b.present_absolute_vblank_arm_supported(primary_xid));
    assert!(b.present_absolute_vblank_arm_supported(secondary_xid));
    assert!(
        !b.direct_present_crtc_eligible(secondary_xid, b.present_crtc_clock_epoch(secondary_xid),),
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
    assert!(b.direct_present_crtc_eligible(crtc_id, first));

    b.refresh_present_crtc_clock_epochs();
    assert_eq!(b.present_crtc_clock_epoch(crtc_id), first);

    let output = b.platform.outputs.pop().expect("fixture output");
    b.refresh_present_crtc_clock_epochs();
    assert_eq!(b.present_crtc_clock_epoch(crtc_id), 0);

    b.platform.outputs.push(output);
    b.refresh_present_crtc_clock_epochs();
    let second = b.present_crtc_clock_epoch(crtc_id);
    assert_ne!(second, 0);
    assert_ne!(second, first, "off-to-on starts a new MSC epoch");
    assert!(
        !b.direct_present_crtc_eligible(crtc_id, first),
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
    assert!(!b.direct_present_crtc_eligible(crtc_id, 0));
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
    let absolute = b
        .arm_present_absolute_vblank_with(primary, &[500], |_, _| {
            calls += 1;
            Ok(true)
        })
        .unwrap();
    assert_eq!(
        (armed, absolute, calls),
        (0, 0, 0),
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
    assert!(b.pstate.armed_vblank_targets.is_empty());
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
    assert!(b.pstate.armed_vblank_targets.contains_key(&primary));

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
fn arm_idle_vblanks_skips_an_unsupported_selected_device() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let primary = output_crtc_key(&b, 0);
    push_test_output(&mut b, 2);
    b.platform.outputs[1].key.device_key = test_device_key(1);
    let secondary = output_crtc_key(&b, 1);
    b.pstate
        .crtc_queue_sequence_unsupported_devices
        .insert(primary.device_key);

    let mut attempted = Vec::new();
    let armed = b
        .arm_idle_vblanks_with(primary, &[100], |crtc_key| {
            attempted.push(crtc_key);
            Ok(true)
        })
        .unwrap();

    assert_eq!(armed, 0);
    assert!(attempted.is_empty());
    assert!(!b.pstate.armed_vblank_targets.contains_key(&primary));

    let secondary_armed = b
        .arm_idle_vblanks_with(secondary, &[100], |crtc_key| {
            attempted.push(crtc_key);
            Ok(true)
        })
        .unwrap();
    assert_eq!(secondary_armed, 1);
    assert_eq!(attempted, vec![secondary]);
    assert!(b.pstate.armed_vblank_targets.contains_key(&secondary));
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
    assert!(!b.pstate.armed_vblank_targets.contains_key(&primary));
    assert!(!b.pstate.armed_vblank_targets.contains_key(&secondary));
}

#[test]
fn arm_idle_vblanks_with_scanout_disallowed_clears_and_returns_zero() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let primary = output_crtc_key(&b, 0);
    b.pstate.armed_vblank_targets.insert(primary, 0);
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
        b.pstate.armed_vblank_targets.is_empty(),
        "master loss drops queued sequences → clear bookkeeping"
    );
}

// ── Absolute per-target vblank arm (Task 3, spec §msc-due) ─────────────

#[test]
fn absolute_vblank_targets_starts_empty() {
    let b = crate::kms::render::backend::KmsBackend::for_tests();
    assert!(b.pstate.absolute_vblank_targets.is_empty());
}

#[test]
fn tagged_sequence_event_does_not_clear_relative_slot() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let crtc = b.platform.outputs[0].output.crtc;
    let crtc_id = u32::from(crtc);
    let crtc_key = output_crtc_key(&b, 0);
    b.pstate.armed_vblank_targets.insert(crtc_key, 0);
    b.pstate
        .absolute_vblank_targets
        .entry(crtc_key)
        .or_default()
        .insert(50);

    // Tagged (absolute) event for the same CRTC.
    dispatch_test_sequence(
        &mut b,
        crate::kms::render::backend::ABSOLUTE_SEQ_TAG | u64::from(crtc_id),
        1_000_000,
        50,
    );

    assert!(
        b.pstate.armed_vblank_targets.contains_key(&crtc_key),
        "a tagged event must not spend the untagged relative-1 slot"
    );
}

#[test]
fn untagged_sequence_event_does_not_retire_absolute_targets() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let crtc = b.platform.outputs[0].output.crtc;
    let crtc_id = u32::from(crtc);
    let crtc_key = output_crtc_key(&b, 0);
    b.pstate.armed_vblank_targets.insert(crtc_key, 0);
    b.pstate
        .absolute_vblank_targets
        .entry(crtc_key)
        .or_default()
        .insert(50);

    // Untagged (relative) event for the same CRTC.
    dispatch_test_sequence(&mut b, u64::from(crtc_id), 1_000_000, 50);

    assert_eq!(
        b.pstate.absolute_vblank_targets.get(&crtc_key).unwrap(),
        &std::collections::BTreeSet::from([50u64]),
        "an untagged event must not retire the absolute per-target set"
    );
}

#[test]
fn tagged_sequence_event_retires_targets_at_or_before_sequence_keeps_later() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let crtc = b.platform.outputs[0].output.crtc;
    let crtc_id = u32::from(crtc);
    let crtc_key = output_crtc_key(&b, 0);
    b.pstate
        .absolute_vblank_targets
        .entry(crtc_key)
        .or_default()
        .extend([40u64, 50, 60]);

    dispatch_test_sequence(
        &mut b,
        crate::kms::render::backend::ABSOLUTE_SEQ_TAG | u64::from(crtc_id),
        1_000_000,
        50,
    );

    assert_eq!(
        b.pstate.absolute_vblank_targets.get(&crtc_key).unwrap(),
        &std::collections::BTreeSet::from([60u64]),
        "targets <= sequence retire; later targets stay armed"
    );
    assert_eq!(
        b.platform.present_get_ust_msc(crtc_key),
        (50, 1_000),
        "the general clock must advance on a TAGGED event too — Task 7's \
             due rule rests on this"
    );
}

#[test]
fn absolute_seq_user_data_round_trips_through_on_crtc_sequence_event() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let crtc = b.platform.outputs[0].output.crtc;
    let crtc_id = u32::from(crtc);
    let crtc_key = output_crtc_key(&b, 0);
    b.pstate.armed_vblank_targets.insert(crtc_key, 0);
    b.pstate
        .absolute_vblank_targets
        .entry(crtc_key)
        .or_default()
        .insert(50);

    // Uses the same producer `arm_present_absolute_vblank` feeds the
    // ioctl, pinning that producer and consumer agree on the encoding.
    dispatch_test_sequence(
        &mut b,
        crate::kms::render::backend::absolute_seq_user_data(crtc_id),
        1_000_000,
        50,
    );

    assert!(
        !b.pstate.absolute_vblank_targets.contains_key(&crtc_key),
        "the target must retire through the real producer encoding"
    );
    assert!(
        b.pstate.armed_vblank_targets.contains_key(&crtc_key),
        "and must not touch the relative slot"
    );
}

#[test]
fn clear_all_armed_vblank_targets_clears_absolute_targets_too() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let crtc_key = output_crtc_key(&b, 0);
    b.pstate.armed_vblank_targets.insert(crtc_key, 0);
    b.pstate
        .absolute_vblank_targets
        .entry(crtc_key)
        .or_default()
        .insert(50);

    b.clear_all_armed_vblank_targets();

    assert!(b.pstate.armed_vblank_targets.is_empty());
    assert!(b.pstate.absolute_vblank_targets.is_empty());
}

#[test]
fn arm_present_absolute_vblank_with_dedups_same_target_one_kernel_call() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let crtc_key = output_crtc_key(&b, 0);
    let mut calls = 0u32;

    let covered = b
        .arm_present_absolute_vblank_with(crtc_key, &[500], |_, _| {
            calls += 1;
            Ok(true)
        })
        .unwrap();
    assert_eq!(covered, 1);
    assert_eq!(calls, 1);

    // Re-arming the same target on the same CRTC must not re-issue the
    // ioctl (real invariant: `calls` stays 1) — but it IS still
    // covered by the earlier in-flight arm, so the return value must
    // stay 1, not fall to 0. `Ok(0)`/`Err` mean "arm failed, caller
    // must execute immediately" (spec §msc-due) — a still-parked,
    // already-armed target must never read as that.
    let covered2 = b
        .arm_present_absolute_vblank_with(crtc_key, &[500], |_, _| {
            calls += 1;
            Ok(true)
        })
        .unwrap();
    assert_eq!(covered2, 1, "duplicate target stays covered, not 0");
    assert_eq!(calls, 1, "duplicate target must not re-arm");

    // A distinct target on the same CRTC still arms (own per-target
    // set, unlike the relative arm's single in-flight slot).
    let covered3 = b
        .arm_present_absolute_vblank_with(crtc_key, &[600], |_, _| {
            calls += 1;
            Ok(true)
        })
        .unwrap();
    assert_eq!(covered3, 1);
    assert_eq!(calls, 2);
}

#[test]
fn arm_present_absolute_vblank_false_result_is_not_tracked_as_covered() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let crtc_key = output_crtc_key(&b, 0);
    let mut calls = 0;

    let covered = b
        .arm_present_absolute_vblank_with(crtc_key, &[500], |_, _| {
            calls += 1;
            Ok(false)
        })
        .unwrap();

    assert_eq!(calls, 1);
    assert_eq!(covered, 0);
    assert!(b.pstate.absolute_vblank_targets.is_empty());
}

#[test]
fn destroy_subwindow_without_direct_frame_does_not_request_unflip() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let target_xid = 0x5600;
    let target_id = seed_window(&mut b, target_xid, None, 0, 0);

    b.destroy_subwindow(None, target_xid)
        .expect("destroy inactive target");

    assert!(!b.pstate.scanout_m2.active());
    assert!(!b.pstate.scanout_m2.unflip_requested);
    assert!(!b.pstate.scanout_m2.hold_direct);
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
    assert!(!b.pstate.scanout_m2.unflip_requested);
    assert!(b.pstate.scanout_m2.hold_direct);
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

    assert!(b.pstate.scanout_m2.unflip_requested);
    assert!(!b.pstate.scanout_m2.hold_direct);
    assert!(b.pstate.scanout_m2.pending.is_some());
    assert!(b.pstate.scanout_m2.current.is_none());
    assert!(b.pstate.scanout_m2.completed.is_empty());
    assert!(b.pstate.scanout_m2.idled.is_empty());
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

    assert!(b.pstate.scanout_m2.unflip_requested);
    assert!(!b.pstate.scanout_m2.hold_direct);
    assert!(b.pstate.scanout_m2.current.is_some());
    assert!(b.pstate.scanout_m2.idled.is_empty());
    assert_eq!(b.present_source_pins.get(&source_pin), Some(&source_id));
    assert_eq!(b.present_source_pins.get(&cow_pin), Some(&cow_id));
    assert_eq!(b.store.get(source_id).map(|d| d.refcount), Some(2));
    assert!(b.store.get(target_id).is_none());
    assert_eq!(b.store.get(cow_id).map(|d| d.refcount), Some(2));

    b.stop_direct_after_scanout_replaced("destroy-subwindow test replacement");
    b.stop_direct_after_scanout_replaced("destroy-subwindow duplicate replacement");

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
    b.pstate.scanout_m2.unflip_shadow_ready = true;

    let final_release = b.release_overlay_window(None).expect("final COW release");

    assert!(final_release);
    assert!(b.deferred_cow_release);
    assert_eq!(b.cow_id, Some(cow_id));
    assert!(b.windows.contains_key(&cow_xid));
    assert!(b.pstate.scanout_m2.current.is_some());
    assert!(b.pstate.scanout_m2.unflip_requested);
    assert!(!b.pstate.scanout_m2.hold_direct);
    assert_eq!(b.present_source_pins.get(&source_pin), Some(&source_id));
    assert_eq!(b.present_source_pins.get(&cow_pin), Some(&cow_id));
    assert_eq!(b.store.get(cow_id).map(|d| d.refcount), Some(2));

    b.stop_direct_after_scanout_replaced("final COW release test replacement");

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
        .release_overlay_window(None)
        .expect_err("test backend has no Vulkan engine for fallback materialization");

    assert!(error.to_string().contains("NoVk"), "{error}");
    assert!(!b.deferred_cow_release);
    assert_eq!(b.cow_id, Some(cow_id));
    assert!(b.store.get(cow_id).is_some());
    assert!(b.pstate.scanout_m2.current.is_some());
    assert!(!b.pstate.scanout_m2.unflip_requested);
    assert!(b.pstate.scanout_m2.hold_direct);
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
    b.pstate.scanout_m2.unflip_shadow_ready = true;
    assert!(b.release_overlay_window(None).expect("final release"));

    let rematerialized = b.get_overlay_window(None).expect("reclaim deferred COW");

    assert!(rematerialized, "core must rebuild its logical COW resource");
    assert!(!b.deferred_cow_release);
    assert_eq!(b.cow_id, Some(cow_id));
    b.stop_direct_after_scanout_replaced("reclaimed COW test replacement");
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
    assert!(b.pstate.scanout_m2.unflip_requested);
    assert!(!b.pstate.scanout_m2.hold_direct);
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
    assert!(!b.pstate.scanout_m2.unflip_requested);
    assert!(b.pstate.scanout_m2.hold_direct);
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
    assert!(b.pstate.scanout_m2.unflip_requested);
    assert!(!b.pstate.scanout_m2.hold_direct);
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
    assert!(!b.pstate.scanout_m2.unflip_requested);
    assert!(b.pstate.scanout_m2.hold_direct);
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
    assert!(b.pstate.scanout_m2.unflip_requested);
    assert!(!b.pstate.scanout_m2.hold_direct);
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
    assert!(!b.pstate.scanout_m2.unflip_requested);
    assert!(b.pstate.scanout_m2.hold_direct);
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
    b.pstate.scanout_m2.unflip_shadow_ready = true;
    let new_w = b.platform.fb_w.saturating_add(1);
    let new_h = b.platform.fb_h.saturating_add(1);

    b.set_logical_screen_size(new_w, new_h)
        .expect("resize after direct shadow materialization");

    let new_cow_id = b.cow_id.expect("replacement COW id");
    assert_ne!(new_cow_id, old_cow_id);
    assert!(b.pstate.scanout_m2.unflip_requested);
    assert!(!b.pstate.scanout_m2.hold_direct);
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

    b.stop_direct_after_scanout_replaced("logical resize test replacement");
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
    assert!(!b.pstate.scanout_m2.unflip_requested);
    assert!(b.pstate.scanout_m2.hold_direct);
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
        b.direct_scanout_topology_eligible(),
        "two equal-refresh outputs on the primary card may use grouped direct scanout"
    );

    b.platform.outputs[1].key.device_key = test_device_key(1);
    assert!(
        !b.direct_scanout_topology_eligible(),
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
    b.pstate.scanout_m2.cursor_bound_all = true;

    let crtc_id = 0x5400;
    bind_test_randr_crtc(&mut b, 0, crtc_id);
    let crtc_epoch = b.present_crtc_clock_epoch(crtc_id);
    let source_id = crate::kms::render::store::DrawableId::for_tests(90);
    let fallback_id = crate::kms::render::store::DrawableId::for_tests(91);
    let candidate = PresentScanoutCandidate {
        client_id: 1,
        present_id: 44,
        crtc_id,
        crtc_epoch,
        src_pixmap_xid: 0x100,
        dst_window_xid: 0x200,
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
    b.pstate.scanout_m2.pending = Some(crate::kms::render::backend::DirectPresentFrame {
        source_pin: 1,
        fallback_target_pin: 2,
        source_id,
        candidate,
        present_received_at: std::time::Instant::now(),
        present_received_monotonic: None,
        fallback_target: PaintTarget::new(fallback_id, (0, 0), None, 24),
        event,
        completion_output_idx: 0,
        completion_clock: None,
        awaiting_outputs: std::collections::HashSet::from([0, 1]),
        accepted_event_id: Some(0x5a),
        async_outcome: crate::drm::async_flip::AsyncOutcome::NotRequested,
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
    assert!(b.pstate.scanout_m2.completed.is_empty());
    assert!(b.pstate.scanout_m2.current.is_none());
    assert_eq!(
        b.pstate
            .scanout_m2
            .pending
            .as_ref()
            .and_then(|frame| frame.completion_clock),
        Some(reference),
        "the selected CRTC sample is cached without completing early"
    );

    assert!(b.retire_direct_output(1, Some(0x5a), other));
    assert_eq!(b.pstate.scanout_m2.completed.len(), 1);
    assert_eq!(
        b.pstate.scanout_m2.completed[0].completion_clock,
        Some(reference),
        "the last retiring output must not replace the selected CRTC timestamp"
    );
    assert!(b.pstate.scanout_m2.pending.is_none());
    assert!(b.pstate.scanout_m2.current.is_some());
    assert!(b.pstate.scanout_m2.idled.is_empty());
    assert!(
        !b.retire_direct_output(1, Some(0x5a), other),
        "a duplicate retirement cannot emit a second completion"
    );
    assert_eq!(b.pstate.scanout_m2.completed.len(), 1);
}

#[test]
fn async_fullscreen_direct_successors_coalesce_until_flip_retirement() {
    use yserver_core::backend::{PresentClockSample, PresentClockSource};

    let mut b = crate::kms::render::backend::KmsBackend::for_tests_with_direct_submitter(
        crate::kms::render::backend::KmsBackend::submit_direct_frame_for_tests,
    );
    b.pstate.scanout_m2.cursor_bound_all = true;
    let fallback_id = seed_window(&mut b, 0x6a00, None, 0, 0);

    install_direct_frame_for_target_test(&mut b, 0x6b00, fallback_id, false);
    let predecessor = b.pstate.scanout_m2.pending.take().expect("predecessor");

    install_direct_frame_for_target_test(&mut b, 0x6c00, fallback_id, false);
    let mut first_successor = b.pstate.scanout_m2.pending.take().expect("first successor");
    first_successor.candidate.present_id = 78;
    first_successor.event.present_id = 78;
    first_successor.event.serial = 10;

    install_direct_frame_for_target_test(&mut b, 0x6d00, fallback_id, false);
    let mut latest_successor = b
        .pstate
        .scanout_m2
        .pending
        .take()
        .expect("latest successor");
    latest_successor.candidate.present_id = 79;
    latest_successor.event.present_id = 79;
    latest_successor.event.serial = 11;

    b.pstate.scanout_m2.pending = Some(predecessor);
    b.queue_direct_successor(first_successor);
    b.queue_direct_successor(latest_successor);

    assert_eq!(
        b.pstate
            .scanout_m2
            .queued_successor
            .as_ref()
            .map(|frame| frame.candidate.present_id),
        Some(79),
        "the hardware-successor state is one-slot latest-wins"
    );
    assert_eq!(
        b.pstate
            .scanout_m2
            .deferred_successor_skips
            .iter()
            .map(|event| event.present_id)
            .collect::<Vec<_>>(),
        vec![78],
        "the coalesced successor is retained only as an ordered Skip"
    );
    assert_eq!(
        b.pstate
            .scanout_m2
            .idled
            .iter()
            .map(|event| event.present_id)
            .collect::<Vec<_>>(),
        vec![78],
        "the coalesced buffer idles immediately and exactly once"
    );
    assert!(!b.pstate.scanout_m2.deferred_successor_skips[0].emit_idle);
    assert!(!b.pstate.scanout_m2.unflip_requested);
    assert!(b.pstate.scanout_m2.hold_direct);
    assert!(b.pstate.scanout_m2.completed.is_empty());

    let first_clock = PresentClockSample {
        msc: 100,
        ust: 1_000,
        source: PresentClockSource::PageFlip,
    };
    assert!(b.retire_direct_output(0, None, first_clock));

    assert_eq!(
        b.pstate
            .scanout_m2
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
            .scanout_m2
            .pending
            .as_ref()
            .map(|frame| frame.candidate.present_id),
        Some(79),
        "retirement immediately submits the retained direct successor"
    );
    assert!(b.pstate.scanout_m2.queued_successor.is_none());
    assert!(!b.pstate.scanout_m2.unflip_requested);
    assert!(b.pstate.scanout_m2.hold_direct);

    let second_clock = PresentClockSample {
        msc: 101,
        ust: 1_100,
        source: PresentClockSource::PageFlip,
    };
    assert!(b.retire_direct_output(0, None, second_clock));
    assert_eq!(
        b.pstate
            .scanout_m2
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
        !b.direct_scanout_topology_eligible(),
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
    assert!(b.direct_scanout_topology_eligible());

    // 60 vs 59.94 Hz: two "60 Hz" monitors, within the grouping tolerance.
    b.platform.outputs[1].output.picked.clock_khz = 148_352;
    assert!(b.direct_scanout_topology_eligible());

    // The advertised integer still says 60, but the kernel timing is 50 Hz.
    // The grouped path must go by the timing, not the integer.
    b.platform.outputs[1].output.picked.clock_khz = 123_750;
    assert!(
        !b.direct_scanout_topology_eligible(),
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
fn arm_present_absolute_vblank_with_arms_only_selected_crtc() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let crtc0 = output_crtc_key(&b, 0);
    push_test_output(&mut b, 2);
    let crtc1 = output_crtc_key(&b, 1);

    // Output 1 holds the max general-clock sample.
    b.platform.ust_msc.insert(crtc0, (5, 5_000));
    b.platform.ust_msc.insert(crtc1, (50, 50_000));

    let mut armed_on: Vec<CrtcKey> = Vec::new();
    let target = u64::from(u32::MAX) + 999;
    let covered = b
        .arm_present_absolute_vblank_with(crtc0, &[target], |crtc_key, armed_target| {
            armed_on.push(crtc_key);
            assert_eq!(armed_target, target, "MSC must stay full-width u64");
            Ok(true)
        })
        .unwrap();

    assert_eq!(covered, 1);
    assert_eq!(
        armed_on,
        vec![crtc0],
        "must arm the selected CRTC even when another domain has a larger counter"
    );
    assert!(
        !b.pstate.absolute_vblank_targets.contains_key(&crtc1),
        "the unrelated CRTC must not gain an armed entry"
    );
}

#[test]
fn arm_present_absolute_vblank_with_uses_selected_crtc_before_first_sample() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let first = output_crtc_key(&b, 0);
    assert!(b.platform.ust_msc.is_empty());

    let mut armed_on = Vec::new();
    let covered = b
        .arm_present_absolute_vblank_with(first, &[17], |crtc_key, target| {
            armed_on.push(crtc_key);
            assert_eq!(target, 17);
            Ok(true)
        })
        .unwrap();

    assert_eq!(covered, 1);
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
    b.kms_outputs_active = false;

    assert!(b.platform.primary_device().is_none());
    assert_eq!(b.dri3_capabilities(), Dri3Caps::unsupported());
    assert!(b.dri3_open(0).is_err());
    assert_eq!(
        b.arm_idle_vblanks_ioctl(0, &[1])
            .expect("relative arm no-op"),
        0
    );
    assert_eq!(
        b.arm_present_absolute_vblank(0, &[1])
            .expect("absolute arm no-op"),
        0
    );
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

#[test]
fn scanout_m1_redirected_game_and_region_present_never_reach_probe_cache() {
    use yserver_core::backend::PresentScanoutCandidate;

    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let crtc_id = 0x5500;
    bind_test_randr_crtc(&mut b, 0, crtc_id);
    let candidate = PresentScanoutCandidate {
        client_id: 1,
        present_id: 1,
        crtc_id,
        crtc_epoch: b.present_crtc_clock_epoch(crtc_id),
        src_pixmap_xid: 0x100,
        dst_window_xid: 0x200,
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
        candidate,
    );
    b.maybe_probe_scanout_m1(
        Some(crate::kms::render::store::DrawableId::for_tests(100)),
        crate::kms::render::backend::ScanoutM0Target::CowDescendant,
        crate::kms::render::backend::ScanoutM0Coverage::Root,
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
fn seed_cow_stage(b: &mut crate::kms::render::backend::KmsBackend, stage: u32) -> (u32, u32) {
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
    let covers =
        |b: &crate::kms::render::backend::KmsBackend| b.direct_shape_chain_covers_root(stage, root);

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
    assert!(b.pstate.scanout_m2.active());

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
        !b.pstate.scanout_m2.unflip_requested,
        "an input shape or a root-covering bounding shape keeps the flip"
    );

    b.set_shape_rectangles(None, cow, 0, Some(&[])).unwrap();
    assert!(b.pstate.scanout_m2.unflip_requested);
    assert_eq!(
        b.pstate.scanout_m2.unflip_reason,
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
        assert!(!state.admit_eligible_root());
    }
    assert!(state.admit_eligible_root());
    assert!(state.admit_eligible_root(), "admission remains saturated");

    state.reset_eligible_root_probation();
    assert!(!state.admit_eligible_root());
}

#[test]
fn scanout_m2_ineligible_root_resets_short_eligible_bursts() {
    let mut state = crate::kms::render::backend::ScanoutM2State::new();
    for _ in 0..16 {
        for _ in 0..(crate::kms::render::backend::SCANOUT_M2_ELIGIBLE_ROOT_PROBATION - 1) {
            assert!(!state.admit_eligible_root());
        }
        state.reset_eligible_root_probation();
    }
}

#[test]
fn active_direct_scanout_unflips_on_cursor_output_fallback() {
    let mut backend = crate::kms::render::backend::KmsBackend::for_tests();
    let fallback_id = seed_window(&mut backend, 0x6e00, None, 0, 0);
    install_direct_frame_for_target_test(&mut backend, 0x6e10, fallback_id, true);
    backend.handle_cursor_move_outcome(crate::kms::render::platform::CursorMoveOutcome {
        ebusy_count: 0,
        fallback_changed: true,
        retry_required: false,
    });
    assert!(backend.pstate.scanout_m2.unflip_requested);
    assert!(!backend.pstate.scanout_m2.hold_direct);
}

#[test]
fn active_direct_scanout_unflips_and_wakes_for_cursor_rollback_retry() {
    let mut backend = crate::kms::render::backend::KmsBackend::for_tests();
    let fallback_id = seed_window(&mut backend, 0x6e20, None, 0, 0);
    install_direct_frame_for_target_test(&mut backend, 0x6e30, fallback_id, true);
    backend.scene.scene_structure_dirty = false;
    backend.handle_cursor_move_outcome(crate::kms::render::platform::CursorMoveOutcome {
        ebusy_count: 0,
        fallback_changed: false,
        retry_required: true,
    });
    assert!(backend.scene.scene_structure_dirty);
    assert!(backend.pstate.scanout_m2.unflip_requested);
    assert!(!backend.pstate.scanout_m2.hold_direct);
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
        present_id: 1,
        crtc_id: 0,
        crtc_epoch: 0,
        src_pixmap_xid: 0x100,
        dst_window_xid: 0x200,
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
        present_id: 1,
        crtc_id: 0,
        crtc_epoch: 0,
        src_pixmap_xid: 0x100,
        dst_window_xid: 0x200,
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
    backend.pstate.scanout_m2.cursor_bound_all = true;
    backend.pstate.scanout_m2.eligible_root_streak =
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

fn assert_atomic_cursorless_scene_record(
    record: &crate::drm::atomic::tests_support::AtomicCommitRecord,
    position: crate::drm::atomic::CursorPlanePosition,
) {
    assert_eq!(record.kind, crate::drm::atomic::AtomicCommitKind::Scene);
    assert!(
        record
            .properties
            .iter()
            .all(|update| update.object != position.plane.into()),
        "cursor plane properties must be absent from the scene request: {:?}",
        record.properties,
    );
}

#[test]
#[ignore = "needs live Vulkan ICD"]
// Mutation: keep the unsent position after EINVAL; separately, never retry a failed hide.
fn cursor_einval_rolls_back_and_hide_recovery_is_motion_driven_and_bounded() {
    use std::io;
    use yserver_core::backend::Backend;

    // A successful rollback hide enters the legacy transient fallback and the
    // next accepted direct scene transaction has no cursor properties.
    let Some(mut hidden_backend) = atomic_cursor_scanout_backend() else {
        return;
    };
    let recorder = atomic_cursor_recording_sink(&mut hidden_backend);
    let position = hidden_backend
        .platform
        .for_tests_seed_atomic_cursor_position(
            0,
            atomic_cursor_raw(0x9501),
            atomic_cursor_raw(0x9601),
            atomic_cursor_raw(0x9701),
            90,
            70,
            0,
            0,
        );
    let hide = std::rc::Rc::new(super::RecordingCursorHideExecutor::default());
    hide.script([super::TestCursorHideResult::Ok]);
    super::install_recording_cursor_hide_executor(&mut hidden_backend, 0, hide.clone());
    recorder.script_result(Err(io::Error::from_raw_os_error(libc::EINVAL)));
    Backend::maybe_composite(&mut hidden_backend).expect("cursor-only EINVAL rollback");

    assert_eq!(recorder.records().len(), 1);
    assert_eq!(
        recorder.records()[0].kind,
        crate::drm::atomic::AtomicCommitKind::CursorOnly,
    );
    assert_eq!(
        recorder.records()[0].result,
        crate::drm::atomic::tests_support::ScriptedResult::Error {
            kind: io::ErrorKind::InvalidInput,
            raw_os_error: Some(libc::EINVAL),
        },
    );
    assert_eq!(hide.calls(), vec![position.crtc]);
    assert!(!hidden_backend.platform.cursor_plane_visible_for_output(0));
    assert_eq!(
        hidden_backend
            .platform
            .for_tests_cursor_fallback_remaining(position.device_key, position.crtc),
        Some(1),
    );
    assert!(hidden_backend.for_tests_scene_wants_compose());
    assert!(
        hidden_backend
            .platform
            .for_tests_atomic_cursor_pending_position(position.device_key, position.crtc)
            .is_none()
    );
    assert!(
        !hidden_backend
            .platform
            .for_tests_atomic_cursor_commit_is_in_flight(position.device_key, position.crtc)
    );
    assert!(
        hidden_backend
            .platform
            .cursor_plane_atomic_positions_for_outputs(&[0])
            .is_empty(),
        "fallback scene requests must not carry the rejected cursor position",
    );

    let direct = prepare_atomic_cursor_direct_present(&mut hidden_backend, 82);
    assert!(
        Backend::try_present_direct(&mut hidden_backend, direct.candidate, direct.event,)
            .expect("fallback direct scene request without cursor fields")
    );
    let records = recorder.records();
    assert_eq!(records.len(), 2);
    assert_atomic_cursorless_scene_record(&records[1], position);
    assert!(
        !hidden_backend
            .platform
            .for_tests_atomic_position_is_unsent(position.device_key, position.crtc)
    );
    let mut hidden_state = yserver_core::server::ServerState::new();
    retire_atomic_cursor_test_output(&mut hidden_backend, &mut hidden_state, 0, 1);
    retire_atomic_cursor_test_output(&mut hidden_backend, &mut hidden_state, 1, 1);
    assert_atomic_cursor_quiescent(&hidden_backend);

    // A failed rollback hide retains the visible hardware plane. Motion
    // retries it without a flip event; a successful retry enters software
    // fallback and drives a cursorless direct-to-scene unflip.
    let Some(mut recovered_backend) = atomic_cursor_scanout_backend() else {
        return;
    };
    let recorder = atomic_cursor_recording_sink(&mut recovered_backend);
    let position = recovered_backend
        .platform
        .for_tests_seed_atomic_cursor_position(
            0,
            atomic_cursor_raw(0x9502),
            atomic_cursor_raw(0x9602),
            atomic_cursor_raw(0x9702),
            90,
            70,
            0,
            0,
        );
    let hide = std::rc::Rc::new(super::RecordingCursorHideExecutor::default());
    hide.script([
        super::TestCursorHideResult::Error(libc::EIO),
        super::TestCursorHideResult::Error(libc::EIO),
        super::TestCursorHideResult::Ok,
    ]);
    super::install_recording_cursor_hide_executor(&mut recovered_backend, 0, hide.clone());
    let direct = prepare_atomic_cursor_direct_present(&mut recovered_backend, 85);
    assert!(
        Backend::try_present_direct(&mut recovered_backend, direct.candidate, direct.event,)
            .expect("initial direct scene frame carries the visible cursor")
    );
    assert_eq!(recorder.records().len(), 1);
    assert_eq!(
        recorder.records()[0].kind,
        crate::drm::atomic::AtomicCommitKind::SceneWithCursor,
    );
    assert_atomic_cursor_properties(&recorder.records()[0], &[position]);

    let mut recovered_state = yserver_core::server::ServerState::new();
    retire_atomic_cursor_test_output(&mut recovered_backend, &mut recovered_state, 0, 1);
    retire_atomic_cursor_test_output(&mut recovered_backend, &mut recovered_state, 1, 1);
    assert!(recovered_backend.pstate.scanout_m2.current.is_some());

    recorder.script_result(Err(io::Error::from_raw_os_error(libc::EINVAL)));
    Backend::warp_pointer_root(&mut recovered_backend, &mut recovered_state, 90, 70);
    Backend::maybe_composite(&mut recovered_backend).expect("failed rollback hide");
    assert_eq!(recorder.records().len(), 2);
    assert_eq!(
        recorder.records()[1].kind,
        crate::drm::atomic::AtomicCommitKind::CursorOnly,
    );
    assert_eq!(
        recorder.records()[1].result,
        crate::drm::atomic::tests_support::ScriptedResult::Error {
            kind: io::ErrorKind::InvalidInput,
            raw_os_error: Some(libc::EINVAL),
        },
    );
    assert!(recovered_backend.pstate.scanout_m2.unflip_requested);
    assert!(
        recovered_backend
            .platform
            .cursor_plane_visible_for_output(0)
    );
    assert_eq!(
        recovered_backend
            .platform
            .for_tests_cursor_hide_recovery_attempts(position.device_key, position.crtc),
        Some(0),
    );
    assert!(
        !recovered_backend
            .platform
            .for_tests_atomic_position_is_unsent(position.device_key, position.crtc)
    );
    assert_eq!(
        recovered_backend.platform.devices[0]
            .test_page_flip_event_source
            .as_ref()
            .unwrap()
            .queued_flip_count(),
        0,
        "the failed hide is recovered without waiting for a flip event",
    );

    Backend::warp_pointer_root(&mut recovered_backend, &mut recovered_state, 91, 71);
    assert_eq!(hide.calls(), vec![position.crtc, position.crtc]);
    assert_eq!(
        recovered_backend
            .platform
            .for_tests_cursor_hide_recovery_attempts(position.device_key, position.crtc),
        Some(1),
    );
    assert_eq!(
        recovered_backend.platform.devices[0]
            .test_page_flip_event_source
            .as_ref()
            .unwrap()
            .queued_flip_count(),
        0,
    );
    Backend::warp_pointer_root(&mut recovered_backend, &mut recovered_state, 92, 72);
    assert_eq!(
        hide.calls(),
        vec![position.crtc, position.crtc, position.crtc]
    );
    assert!(
        recovered_backend
            .platform
            .for_tests_cursor_hide_recovery_attempts(position.device_key, position.crtc)
            .is_none()
    );
    assert!(
        !recovered_backend
            .platform
            .cursor_plane_visible_for_output(0)
    );
    assert_eq!(
        recovered_backend
            .platform
            .for_tests_cursor_fallback_remaining(position.device_key, position.crtc),
        Some(1),
    );
    assert_eq!(
        recovered_backend.platform.devices[0]
            .test_page_flip_event_source
            .as_ref()
            .unwrap()
            .queued_flip_count(),
        0,
    );
    assert!(
        recovered_backend
            .platform
            .cursor_plane_atomic_positions_for_outputs(&[0])
            .is_empty()
    );

    recovered_backend
        .platform
        .scanout_pools
        .resize_with(2, || None);
    recovered_backend
        .platform
        .bo_generations
        .resize_with(2, Vec::new);
    let vk = std::sync::Arc::clone(
        recovered_backend
            .platform
            .gpu
            .vk
            .as_ref()
            .expect("test fixture Vulkan context"),
    );
    let kms_device = std::rc::Rc::clone(&recovered_backend.platform.devices[0].device);
    for (output_idx, fb) in [(0, 0x8803), (1, 0x8804)] {
        let layout = &recovered_backend.platform.outputs[output_idx];
        let pool = crate::kms::vk::scanout::ScanoutBoPool::for_tests_on_screen_framebuffer(
            std::sync::Arc::clone(&vk),
            std::rc::Rc::clone(&kms_device),
            layout.scanout_route,
            u32::from(layout.width),
            u32::from(layout.height),
            atomic_cursor_raw(fb),
        );
        recovered_backend.platform.scanout_pools[output_idx] =
            Some(crate::kms::vk::scanout::OutputScanout::Shared(pool));
    }
    recovered_backend.pstate.scanout_m2.unflip_shadow_ready = true;
    Backend::maybe_composite(&mut recovered_backend).expect("submit cursorless composed unflip");
    assert_eq!(recorder.records().len(), 3);
    assert_atomic_cursorless_scene_record(&recorder.records()[2], position);
    assert_eq!(
        recovered_backend
            .pstate
            .scanout_m2
            .unflip_outputs_to_replace,
        std::collections::HashSet::from([0, 1]),
    );
    assert_atomic_cursor_quiescent(&recovered_backend);
    retire_atomic_cursor_test_output(&mut recovered_backend, &mut recovered_state, 0, 2);
    retire_atomic_cursor_test_output(&mut recovered_backend, &mut recovered_state, 1, 2);
    assert!(!recovered_backend.pstate.scanout_m2.active());
    assert_atomic_cursor_quiescent(&recovered_backend);

    // Three failed motion-led retries permanently disable the cursor. Later
    // motion must not issue a fifth legacy hide or restore an atomic field, but
    // an explicit hide must still detach the plane those failures left bound.
    let Some(mut disabled_backend) = atomic_cursor_scanout_backend() else {
        return;
    };
    let recorder = atomic_cursor_recording_sink(&mut disabled_backend);
    let position = disabled_backend
        .platform
        .for_tests_seed_atomic_cursor_position(
            0,
            atomic_cursor_raw(0x9503),
            atomic_cursor_raw(0x9603),
            atomic_cursor_raw(0x9703),
            90,
            70,
            0,
            0,
        );
    let hide = std::rc::Rc::new(super::RecordingCursorHideExecutor::default());
    hide.script([
        super::TestCursorHideResult::Error(libc::EIO),
        super::TestCursorHideResult::Error(libc::EIO),
        super::TestCursorHideResult::Error(libc::EIO),
        super::TestCursorHideResult::Error(libc::EIO),
        super::TestCursorHideResult::Ok,
    ]);
    super::install_recording_cursor_hide_executor(&mut disabled_backend, 0, hide.clone());
    recorder.script_result(Err(io::Error::from_raw_os_error(libc::EINVAL)));
    Backend::maybe_composite(&mut disabled_backend).expect("initial hide failure");
    let mut disabled_state = yserver_core::server::ServerState::new();
    for (x, y) in [(91, 71), (92, 72), (93, 73)] {
        Backend::warp_pointer_root(&mut disabled_backend, &mut disabled_state, x, y);
    }
    assert_eq!(hide.calls(), vec![position.crtc; 4]);
    assert!(
        disabled_backend
            .platform
            .for_tests_cursor_permanently_disabled(position.device_key)
    );
    assert!(disabled_backend.platform.cursor_plane_visible_for_output(0));
    assert!(
        !disabled_backend
            .platform
            .for_tests_atomic_position_is_unsent(position.device_key, position.crtc)
    );
    assert!(
        disabled_backend
            .platform
            .cursor_plane_atomic_positions_for_outputs(&[0])
            .is_empty()
    );
    Backend::warp_pointer_root(&mut disabled_backend, &mut disabled_state, 94, 74);
    assert_eq!(hide.calls(), vec![position.crtc; 4]);
    assert_eq!(recorder.records().len(), 1);
    assert_eq!(
        recorder.records()[0].kind,
        crate::drm::atomic::AtomicCommitKind::CursorOnly,
    );
    assert_eq!(
        recorder.records()[0].result,
        crate::drm::atomic::tests_support::ScriptedResult::Error {
            kind: io::ErrorKind::InvalidInput,
            raw_os_error: Some(libc::EINVAL),
        },
    );
    assert!(
        disabled_backend
            .platform
            .cursor_plane_atomic_positions_for_outputs(&[0])
            .is_empty()
    );
    assert!(
        !disabled_backend
            .platform
            .for_tests_atomic_cursor_commit_is_in_flight(position.device_key, position.crtc)
    );
    disabled_backend
        .platform
        .cursor_plane_hide_on_crtc(0)
        .expect("hide the plane the failed retries left bound");
    assert_eq!(hide.calls(), vec![position.crtc; 5]);
    assert!(!disabled_backend.platform.cursor_plane_visible_for_output(0));
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
    let hide = std::rc::Rc::new(super::RecordingCursorHideExecutor::default());
    hide.script([
        super::TestCursorHideResult::Ok,
        super::TestCursorHideResult::Ok,
    ]);
    super::install_recording_cursor_hide_executor(&mut backend, 0, hide);

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
    assert_eq!(recorder.records().len(), 2);
    assert_eq!(
        recorder.records()[1].kind,
        crate::drm::atomic::AtomicCommitKind::Scene,
    );
    assert!(
        recorder.records()[1]
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
    assert!(backend.pstate.scanout_m2.pending.is_none());
    assert_eq!(
        backend
            .pstate
            .scanout_m2
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
    use ash::vk;
    use yserver_core::backend::{CompletedPresentEvent, PresentScanoutCandidate, PresentWake};

    use crate::kms::render::store::{DrawableKind, Storage};

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
        .allocate(
            source_xid,
            DrawableKind::Pixmap,
            24,
            false,
            Storage::for_tests_null(
                vk::Extent2D {
                    width: u32::from(width),
                    height: u32::from(height),
                },
                vk::Format::B8G8R8A8_UNORM,
            ),
        )
        .expect("allocate fullscreen Present source");
    backend.pstate.scanout_m1.entries.insert(
        source_id,
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
        present_id,
        crtc_id,
        crtc_epoch,
        src_pixmap_xid: source_xid,
        dst_window_xid: target_xid,
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
    backend.pstate.scanout_m1.entries.remove(&direct.source_id);
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
        direct.source_id,
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
    assert!(backend.pstate.scanout_m2.pending.is_some());

    Backend::maybe_composite(&mut backend).expect("composite tick during direct scanout");
    assert!(
        !backend.pstate.scanout_m2.unflip_requested,
        "Hidden must not request a direct unflip"
    );
    assert_eq!(recorder.records().len(), 1);
}

#[test]
#[ignore = "needs live Vulkan ICD"]
// Mutation: skip the direct cursor rebind/fallback on ShowCursor. The test
// requires either a visible plane or a scheduled composed unflip.
fn showing_cursor_during_direct_scanout_rebinds_or_requests_unflip() {
    use yserver_core::backend::Backend;

    let Some(mut backend) = atomic_cursor_hidden_scanout_backend() else {
        return;
    };
    let recorder = atomic_cursor_recording_sink(&mut backend);
    Backend::set_cursor_hidden(&mut backend, true);

    let positions = [0, 1].map(|output_idx| {
        backend.platform.for_tests_seed_atomic_cursor_position(
            output_idx,
            atomic_cursor_raw(0x9a00 + u32::try_from(output_idx).unwrap()),
            atomic_cursor_raw(0x9b00 + u32::try_from(output_idx).unwrap()),
            atomic_cursor_raw(0x9c00 + u32::try_from(output_idx).unwrap()),
            820,
            130,
            0,
            0,
        )
    });
    for position in positions {
        backend.platform.devices[0]
            .cursor
            .plane
            .as_mut()
            .expect("test cursor plane")
            .for_tests_set_visible(position.crtc, false);
    }

    let cursor_xid = 0x9d00;
    backend.insert_cursor_record(cursor_xid, 1, 1, 0, 0, vec![0xff; 4]);
    backend.effective_cursor_xid = Some(cursor_xid);

    let direct = prepare_atomic_cursor_direct_present(&mut backend, 92);
    assert!(
        Backend::try_present_direct(&mut backend, direct.candidate, direct.event,)
            .expect("fullscreen Present while hidden")
    );
    assert_eq!(recorder.records().len(), 1);
    assert!(
        backend
            .platform
            .cursor_plane_atomic_positions_for_outputs(&[0, 1])
            .is_empty(),
        "the hidden plane must not be rebound by the direct transaction"
    );

    // The test cursor plane intentionally has no dumb buffer, so its legacy
    // show operation fails and the backend must schedule a composed fallback.
    // A real plane may instead bind successfully and keep direct scanout.
    backend.pstate.scanout_m2.cursor_bound_all = false;
    Backend::set_cursor_hidden(&mut backend, false);
    assert!(!backend.cursor_hidden);
    assert!(backend.scene.scene_structure_dirty);
    let first_crtc = positions[0].crtc;
    let plane_visible = backend.platform.devices[0]
        .cursor
        .plane
        .as_ref()
        .is_some_and(|plane| plane.is_visible_on(first_crtc));
    assert!(
        plane_visible || backend.pstate.scanout_m2.unflip_requested,
        "ShowCursor must bind the plane or schedule a composed unflip"
    );
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
            .scanout_m2
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
            .scanout_m2
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
            .scanout_m2
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
    backend.pstate.scanout_m2.unflip_shadow_ready = true;
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
        backend.pstate.scanout_m2.unflip_outputs_to_replace,
        std::collections::HashSet::from([0, 1]),
    );
    assert_atomic_cursor_quiescent(&backend);

    retire_atomic_cursor_test_output(&mut backend, &mut state, 0, 4);
    retire_atomic_cursor_test_output(&mut backend, &mut state, 1, 4);
    assert!(!backend.pstate.scanout_m2.active());
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
    assert!(backend.pstate.scanout_m2.current.is_some());

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
            .scanout_m2
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
    assert!(backend.pstate.scanout_m2.queued_successor.is_some());

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
    assert!(backend.pstate.scanout_m2.queued_successor.is_none());
    assert_eq!(
        backend
            .pstate
            .scanout_m2
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
    assert!(backend.pstate.scanout_m2.current.is_none());
    assert!(backend.pstate.scanout_m2.pending.is_none());
    assert!(backend.pstate.scanout_m2.queued_successor.is_none());
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
    assert!(backend.pstate.scanout_m2.queued_successor.is_none());
    assert!(backend.pstate.scanout_m2.pending.is_none());

    Backend::client_disconnected(&mut backend, yserver_protocol::x11::ClientId(1));
    assert!(backend.scene.root_overlay.is_empty());
    // The root overlay correctly reset direct-scanout probation. Restore the
    // admitted fixture state so this last Present isolates cursor retirement.
    backend.pstate.scanout_m2.eligible_root_streak =
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
    assert!(backend.pstate.scanout_m2.pending.is_some());
    assert!(backend.pstate.scanout_m2.queued_successor.is_none());
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
            Storage::for_tests_null(
                ash::vk::Extent2D {
                    width: 1600,
                    height: 600,
                },
                ash::vk::Format::B8G8R8A8_UNORM,
            ),
        )
        .expect("fullscreen Present source");
    backend.pstate.scanout_m1.entries.insert(
        source_id,
        crate::kms::render::backend::ScanoutM1ProbeEntry::accepted(
            crate::drm::modeset::DirectScanoutProbeFramebuffer::for_tests(atomic_cursor_raw(71)),
        ),
    );
    backend.pstate.scanout_m2.eligible_root_streak =
        crate::kms::render::backend::SCANOUT_M2_ELIGIBLE_ROOT_PROBATION - 1;

    let crtc_id = u32::from(backend.platform.outputs[0].output.crtc);
    bind_test_randr_crtc(&mut backend, 0, crtc_id);
    let candidate = PresentScanoutCandidate {
        client_id: 1,
        present_id: 45,
        crtc_id,
        crtc_epoch: backend.present_crtc_clock_epoch(crtc_id),
        src_pixmap_xid: source_xid,
        dst_window_xid: target_xid,
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
    assert!(backend.pstate.scanout_m2.pending.is_some());
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
    backend.pstate.scanout_m2.unflip_shadow_ready = true;

    Backend::set_shape_rectangles(&mut backend, None, cow, 0, Some(&[]))
        .expect("empty COW bounding shape");
    assert!(backend.pstate.scanout_m2.unflip_requested);
    assert_eq!(
        backend.pstate.scanout_m2.unflip_reason,
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
        backend.pstate.scanout_m2.unflip_outputs_to_replace,
        std::collections::HashSet::from([0, 1])
    );
    assert!(positions.iter().all(|position| {
        !backend
            .platform
            .for_tests_atomic_position_is_unsent(position.device_key, position.crtc)
    }));
}
