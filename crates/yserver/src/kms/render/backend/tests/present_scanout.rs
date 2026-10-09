use super::*;

fn output_crtc_key(backend: &KmsBackend, output_idx: usize) -> CrtcKey {
    CrtcKey::for_output(&backend.platform.outputs[output_idx])
}

fn bind_test_randr_crtc(backend: &mut KmsBackend, output_idx: usize, crtc_id: u32) {
    let output_key = backend.platform.outputs[output_idx].key.clone();
    backend.crtc_key_by_id.insert(crtc_id, output_key);
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
    assert!(b.armed_vblank_targets.is_empty());
    assert!(b.crtc_queue_sequence_unsupported_devices.is_empty());
}

#[test]
fn clear_all_armed_vblank_targets_empties_map() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let h = test_crtc_key(test_device_key(7), 7);
    b.armed_vblank_targets.insert(h, 0);
    b.clear_all_armed_vblank_targets();
    assert!(b.armed_vblank_targets.is_empty());
}

#[test]
fn prune_armed_targets_drops_stale_keeps_live() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let live = output_crtc_key(&b, 0);
    let stale = test_crtc_key(live.device_key, 999);
    b.armed_vblank_targets.insert(live, 0);
    b.armed_vblank_targets.insert(stale, 0);
    b.prune_armed_targets_to_live_outputs();
    assert!(b.armed_vblank_targets.contains_key(&live));
    assert!(!b.armed_vblank_targets.contains_key(&stale));
}

#[test]
fn prune_armed_targets_drops_stale_absolute_entries_keeps_live() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let live = output_crtc_key(&b, 0);
    let stale = test_crtc_key(live.device_key, 999);
    b.absolute_vblank_targets
        .entry(live)
        .or_default()
        .insert(50);
    b.absolute_vblank_targets
        .entry(stale)
        .or_default()
        .insert(50);
    b.prune_armed_targets_to_live_outputs();
    assert!(b.absolute_vblank_targets.contains_key(&live));
    assert!(!b.absolute_vblank_targets.contains_key(&stale));
}

#[test]
fn on_crtc_sequence_event_happy_path_records_msc_and_clears_arm() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let crtc = b.platform.outputs[0].output.crtc;
    let crtc_key = output_crtc_key(&b, 0);
    b.armed_vblank_targets.insert(crtc_key, 0);

    dispatch_test_sequence(
        &mut b,
        u64::from(u32::from(crtc)),
        1_500_000, /* 1.5ms */
        7,
    );

    assert!(b.armed_vblank_targets.is_empty(), "arm must be cleared");
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
    b.armed_vblank_targets.insert(crtc_key, 0);
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
    b.absolute_vblank_targets
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
    b.armed_vblank_targets.insert(crtc_key, 0);

    dispatch_test_sequence(&mut b, u64::from(u32::from(crtc)), -1, 7);

    assert!(
        b.armed_vblank_targets.is_empty(),
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
    b.armed_vblank_targets.insert(stale, 0);

    dispatch_test_sequence(&mut b, 999, 1_000_000, 5);

    assert!(
        b.armed_vblank_targets.is_empty(),
        "stale arm cleared so the CRTC can't strand"
    );
    assert_eq!(b.platform.present_get_ust_msc(stale), (0, 0));
}

#[test]
fn on_crtc_sequence_event_from_wrong_device_does_not_advance_clock() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let crtc = b.platform.outputs[0].output.crtc;
    let crtc_key = output_crtc_key(&b, 0);
    b.armed_vblank_targets.insert(crtc_key, 0);
    let wrong_device = test_device_key(99);

    b.on_crtc_sequence_event(wrong_device, u64::from(u32::from(crtc)), 1_000_000, 5);

    assert!(
        b.armed_vblank_targets.contains_key(&crtc_key),
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

    b.armed_vblank_targets.insert(primary, 0);
    b.armed_vblank_targets.insert(secondary, 0);
    b.on_crtc_sequence_event(secondary.device_key, u64::from(raw_crtc), 3_000_000, 17);

    assert!(
        b.armed_vblank_targets.contains_key(&primary),
        "the event must not retire the equal raw CRTC on the primary card"
    );
    assert!(
        !b.armed_vblank_targets.contains_key(&secondary),
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
    assert!(!b.armed_vblank_targets.contains_key(&primary));
    assert!(b.armed_vblank_targets.contains_key(&secondary));

    b.crtc_queue_sequence_unsupported_devices
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
    b.crtc_key_by_id
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
    assert!(b.armed_vblank_targets.is_empty());
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
    assert!(b.armed_vblank_targets.contains_key(&primary));

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
    b.crtc_queue_sequence_unsupported_devices
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
    assert!(!b.armed_vblank_targets.contains_key(&primary));

    let secondary_armed = b
        .arm_idle_vblanks_with(secondary, &[100], |crtc_key| {
            attempted.push(crtc_key);
            Ok(true)
        })
        .unwrap();
    assert_eq!(secondary_armed, 1);
    assert_eq!(attempted, vec![secondary]);
    assert!(b.armed_vblank_targets.contains_key(&secondary));
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
    assert!(!b.armed_vblank_targets.contains_key(&primary));
    assert!(!b.armed_vblank_targets.contains_key(&secondary));
}

#[test]
fn arm_idle_vblanks_with_scanout_disallowed_clears_and_returns_zero() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let primary = output_crtc_key(&b, 0);
    b.armed_vblank_targets.insert(primary, 0);
    b.vt_state = crate::vt::state::VtState::Suspended;

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
        b.armed_vblank_targets.is_empty(),
        "master loss drops queued sequences → clear bookkeeping"
    );
}

// ── Absolute per-target vblank arm (Task 3, spec §msc-due) ─────────────

#[test]
fn absolute_vblank_targets_starts_empty() {
    let b = crate::kms::render::backend::KmsBackend::for_tests();
    assert!(b.absolute_vblank_targets.is_empty());
}

#[test]
fn tagged_sequence_event_does_not_clear_relative_slot() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let crtc = b.platform.outputs[0].output.crtc;
    let crtc_id = u32::from(crtc);
    let crtc_key = output_crtc_key(&b, 0);
    b.armed_vblank_targets.insert(crtc_key, 0);
    b.absolute_vblank_targets
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
        b.armed_vblank_targets.contains_key(&crtc_key),
        "a tagged event must not spend the untagged relative-1 slot"
    );
}

#[test]
fn untagged_sequence_event_does_not_retire_absolute_targets() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let crtc = b.platform.outputs[0].output.crtc;
    let crtc_id = u32::from(crtc);
    let crtc_key = output_crtc_key(&b, 0);
    b.armed_vblank_targets.insert(crtc_key, 0);
    b.absolute_vblank_targets
        .entry(crtc_key)
        .or_default()
        .insert(50);

    // Untagged (relative) event for the same CRTC.
    dispatch_test_sequence(&mut b, u64::from(crtc_id), 1_000_000, 50);

    assert_eq!(
        b.absolute_vblank_targets.get(&crtc_key).unwrap(),
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
    b.absolute_vblank_targets
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
        b.absolute_vblank_targets.get(&crtc_key).unwrap(),
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
    b.armed_vblank_targets.insert(crtc_key, 0);
    b.absolute_vblank_targets
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
        !b.absolute_vblank_targets.contains_key(&crtc_key),
        "the target must retire through the real producer encoding"
    );
    assert!(
        b.armed_vblank_targets.contains_key(&crtc_key),
        "and must not touch the relative slot"
    );
}

#[test]
fn clear_all_armed_vblank_targets_clears_absolute_targets_too() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let crtc_key = output_crtc_key(&b, 0);
    b.armed_vblank_targets.insert(crtc_key, 0);
    b.absolute_vblank_targets
        .entry(crtc_key)
        .or_default()
        .insert(50);

    b.clear_all_armed_vblank_targets();

    assert!(b.armed_vblank_targets.is_empty());
    assert!(b.absolute_vblank_targets.is_empty());
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
    assert!(b.absolute_vblank_targets.is_empty());
}

#[test]
fn destroy_subwindow_without_direct_frame_does_not_request_unflip() {
    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    let target_xid = 0x5600;
    let target_id = seed_window(&mut b, target_xid, None, 0, 0);

    b.destroy_subwindow(None, target_xid)
        .expect("destroy inactive target");

    assert!(!b.scanout_m2.active());
    assert!(!b.scanout_m2.unflip_requested);
    assert!(!b.scanout_m2.hold_direct);
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
    assert!(!b.scanout_m2.unflip_requested);
    assert!(b.scanout_m2.hold_direct);
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

    assert!(b.scanout_m2.unflip_requested);
    assert!(!b.scanout_m2.hold_direct);
    assert!(b.scanout_m2.pending.is_some());
    assert!(b.scanout_m2.current.is_none());
    assert!(b.scanout_m2.completed.is_empty());
    assert!(b.scanout_m2.idled.is_empty());
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

    assert!(b.scanout_m2.unflip_requested);
    assert!(!b.scanout_m2.hold_direct);
    assert!(b.scanout_m2.current.is_some());
    assert!(b.scanout_m2.idled.is_empty());
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
    b.scanout_m2.unflip_shadow_ready = true;

    let final_release = b.release_overlay_window(None).expect("final COW release");

    assert!(final_release);
    assert!(b.deferred_cow_release);
    assert_eq!(b.cow_id, Some(cow_id));
    assert!(b.windows.contains_key(&cow_xid));
    assert!(b.scanout_m2.current.is_some());
    assert!(b.scanout_m2.unflip_requested);
    assert!(!b.scanout_m2.hold_direct);
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
    assert!(b.scanout_m2.current.is_some());
    assert!(!b.scanout_m2.unflip_requested);
    assert!(b.scanout_m2.hold_direct);
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
    b.scanout_m2.unflip_shadow_ready = true;
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
    assert!(b.scanout_m2.unflip_requested);
    assert!(!b.scanout_m2.hold_direct);
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
    assert!(!b.scanout_m2.unflip_requested);
    assert!(b.scanout_m2.hold_direct);
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
    assert!(b.scanout_m2.unflip_requested);
    assert!(!b.scanout_m2.hold_direct);
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
    assert!(!b.scanout_m2.unflip_requested);
    assert!(b.scanout_m2.hold_direct);
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
    assert!(b.scanout_m2.unflip_requested);
    assert!(!b.scanout_m2.hold_direct);
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
    assert!(!b.scanout_m2.unflip_requested);
    assert!(b.scanout_m2.hold_direct);
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
    b.scanout_m2.unflip_shadow_ready = true;
    let new_w = b.platform.fb_w.saturating_add(1);
    let new_h = b.platform.fb_h.saturating_add(1);

    b.set_logical_screen_size(new_w, new_h)
        .expect("resize after direct shadow materialization");

    let new_cow_id = b.cow_id.expect("replacement COW id");
    assert_ne!(new_cow_id, old_cow_id);
    assert!(b.scanout_m2.unflip_requested);
    assert!(!b.scanout_m2.hold_direct);
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
    assert!(!b.scanout_m2.unflip_requested);
    assert!(b.scanout_m2.hold_direct);
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
    b.scanout_m2.cursor_bound_all = true;

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
    b.scanout_m2.pending = Some(crate::kms::render::backend::DirectPresentFrame {
        source_pin: 1,
        fallback_target_pin: 2,
        source_id,
        candidate,
        fallback_target: PaintTarget::new(fallback_id, (0, 0), None, 24),
        event,
        completion_output_idx: 0,
        completion_clock: None,
        awaiting_outputs: std::collections::HashSet::from([0, 1]),
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
    assert!(b.retire_direct_output(0, reference));
    assert!(b.scanout_m2.completed.is_empty());
    assert!(b.scanout_m2.current.is_none());
    assert_eq!(
        b.scanout_m2
            .pending
            .as_ref()
            .and_then(|frame| frame.completion_clock),
        Some(reference),
        "the selected CRTC sample is cached without completing early"
    );

    assert!(b.retire_direct_output(1, other));
    assert_eq!(b.scanout_m2.completed.len(), 1);
    assert_eq!(
        b.scanout_m2.completed[0].completion_clock,
        Some(reference),
        "the last retiring output must not replace the selected CRTC timestamp"
    );
    assert!(b.scanout_m2.pending.is_none());
    assert!(b.scanout_m2.current.is_some());
    assert!(b.scanout_m2.idled.is_empty());
    assert!(
        !b.retire_direct_output(1, other),
        "a duplicate retirement cannot emit a second completion"
    );
    assert_eq!(b.scanout_m2.completed.len(), 1);
}

#[test]
fn async_fullscreen_direct_successors_coalesce_until_flip_retirement() {
    use yserver_core::backend::{PresentClockSample, PresentClockSource};

    let mut b = crate::kms::render::backend::KmsBackend::for_tests();
    b.scanout_m2.test_submit_direct_without_drm = true;
    b.scanout_m2.cursor_bound_all = true;
    let fallback_id = seed_window(&mut b, 0x6a00, None, 0, 0);

    install_direct_frame_for_target_test(&mut b, 0x6b00, fallback_id, false);
    let predecessor = b.scanout_m2.pending.take().expect("predecessor");

    install_direct_frame_for_target_test(&mut b, 0x6c00, fallback_id, false);
    let mut first_successor = b.scanout_m2.pending.take().expect("first successor");
    first_successor.candidate.present_id = 78;
    first_successor.event.present_id = 78;
    first_successor.event.serial = 10;

    install_direct_frame_for_target_test(&mut b, 0x6d00, fallback_id, false);
    let mut latest_successor = b.scanout_m2.pending.take().expect("latest successor");
    latest_successor.candidate.present_id = 79;
    latest_successor.event.present_id = 79;
    latest_successor.event.serial = 11;

    b.scanout_m2.pending = Some(predecessor);
    b.queue_direct_successor(first_successor);
    b.queue_direct_successor(latest_successor);

    assert_eq!(
        b.scanout_m2
            .queued_successor
            .as_ref()
            .map(|frame| frame.candidate.present_id),
        Some(79),
        "the hardware-successor state is one-slot latest-wins"
    );
    assert_eq!(
        b.scanout_m2
            .deferred_successor_skips
            .iter()
            .map(|event| event.present_id)
            .collect::<Vec<_>>(),
        vec![78],
        "the coalesced successor is retained only as an ordered Skip"
    );
    assert_eq!(
        b.scanout_m2
            .idled
            .iter()
            .map(|event| event.present_id)
            .collect::<Vec<_>>(),
        vec![78],
        "the coalesced buffer idles immediately and exactly once"
    );
    assert!(!b.scanout_m2.deferred_successor_skips[0].emit_idle);
    assert!(!b.scanout_m2.unflip_requested);
    assert!(b.scanout_m2.hold_direct);
    assert!(b.scanout_m2.completed.is_empty());

    let first_clock = PresentClockSample {
        msc: 100,
        ust: 1_000,
        source: PresentClockSource::PageFlip,
    };
    assert!(b.retire_direct_output(0, first_clock));

    assert_eq!(
        b.scanout_m2
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
        b.scanout_m2
            .pending
            .as_ref()
            .map(|frame| frame.candidate.present_id),
        Some(79),
        "retirement immediately submits the retained direct successor"
    );
    assert!(b.scanout_m2.queued_successor.is_none());
    assert!(!b.scanout_m2.unflip_requested);
    assert!(b.scanout_m2.hold_direct);

    let second_clock = PresentClockSample {
        msc: 101,
        ust: 1_100,
        source: PresentClockSource::PageFlip,
    };
    assert!(b.retire_direct_output(0, second_clock));
    assert_eq!(
        b.scanout_m2
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

    // Both modes round to the same advertised integer refresh, but their
    // pixel-clock ratios differ. The grouped path must not conflate them.
    b.platform.outputs[1].output.picked.clock_khz = 148_352;
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
        !b.absolute_vblank_targets.contains_key(&crtc1),
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
    assert!(b.scanout_m1.entries.is_empty());
    assert_eq!(b.scanout_m0.m1_probe_pass, 0);
    assert_eq!(b.scanout_m0.m1_probe_reject, 0);
    assert_eq!(b.scanout_m0.m1_probe_error, 0);
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
    assert!(b.scanout_m2.active());

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
        !b.scanout_m2.unflip_requested,
        "an input shape or a root-covering bounding shape keeps the flip"
    );

    b.set_shape_rectangles(None, cow, 0, Some(&[])).unwrap();
    assert!(b.scanout_m2.unflip_requested);
    assert_eq!(b.scanout_m2.unflip_reason, Some("shape_clips_direct_frame"));
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
    backend.scanout_m2.test_force_active = true;
    backend.handle_cursor_move_outcome(crate::kms::render::platform::CursorMoveOutcome {
        ebusy_count: 0,
        fallback_changed: true,
        retry_required: false,
    });
    assert!(backend.scanout_m2.unflip_requested);
    assert!(!backend.scanout_m2.hold_direct);
}

#[test]
fn active_direct_scanout_unflips_and_wakes_for_cursor_rollback_retry() {
    let mut backend = crate::kms::render::backend::KmsBackend::for_tests();
    backend.scene.scene_structure_dirty = false;
    backend.scanout_m2.test_force_active = true;
    backend.handle_cursor_move_outcome(crate::kms::render::platform::CursorMoveOutcome {
        ebusy_count: 0,
        fallback_changed: false,
        retry_required: true,
    });
    assert!(backend.scene.scene_structure_dirty);
    assert!(backend.scanout_m2.unflip_requested);
    assert!(!backend.scanout_m2.hold_direct);
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

    assert_eq!(b.scanout_m0.last_shape_by_dst.len(), 1);
    assert_eq!(b.scanout_m0.recent_sources_by_dst[&0xD57].len(), 1);
    assert_eq!(b.scanout_m0.presents, 2);
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
    let after_first = b.scanout_m0.last_shape_by_dst[&0xD57].clone();

    for (present_id, update) in [(2u64, 0x0041_44ebu32), (3, 0x0041_44f9)] {
        b.observe_scanout_m0(PresentScanoutCandidate {
            present_id,
            update_region_xid: update,
            ..base
        });
        assert_eq!(
            b.scanout_m0.last_shape_by_dst[&0xD57], after_first,
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
        b.scanout_m0.last_shape_by_dst[&0xD57], after_first,
        "a Present that carries no update region at all is a different \
             shape and must still be logged",
    );
}
