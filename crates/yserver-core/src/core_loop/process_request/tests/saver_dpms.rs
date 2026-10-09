use super::*;

#[test]
fn dpms_force_level_rejects_when_disabled() {
    let mut state = ServerState::new();
    state.dpms.kms_capable = true;
    state.dpms.enabled = false;
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let header = RequestHeader {
        opcode: 134,
        data: 6,
        length_units: 2,
    };
    let body = [3u8, 0, 0, 0]; // level=Off
    let _ = handle_dpms_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        header,
        &body,
    );
    let calls = backend.calls();
    assert!(
        !calls
            .iter()
            .any(|c| matches!(c, RecordedCall::SetDpmsPower(_))),
        "ForceLevel must not call backend when !enabled"
    );
    assert_eq!(state.dpms.power_level, 0, "state unchanged on BadMatch");
    // Error reply: byte 0 == 0 (Error), byte 1 == x11::error::BAD_MATCH.
    let bytes = read_all_available(&mut peer);
    assert_eq!(bytes[0], 0, "reply is an X11 error");
    assert_eq!(bytes[1], x11::error::BAD_MATCH);
}

#[test]
fn dpms_force_level_changes_state_and_notifies() {
    let mut state = ServerState::new();
    state.dpms.kms_capable = true;
    state.dpms.enabled = true;
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    state.dpms.selected_by.insert(ClientId(1));
    let header = RequestHeader {
        opcode: 134,
        data: 6,
        length_units: 2,
    };
    let body = [3u8, 0, 0, 0]; // level=Off
    let _ = handle_dpms_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        header,
        &body,
    );
    assert_eq!(state.dpms.power_level, 3);
    assert!(
        backend
            .calls()
            .iter()
            .any(|c| matches!(c, RecordedCall::SetDpmsPower(3)))
    );
    // GenericEvent tag = 35 should appear exactly once in this client's stream.
    let bytes = read_all_available(&mut peer);
    let notifies = bytes.iter().filter(|&&b| b == 35).count();
    assert_eq!(notifies, 1, "ForceLevel(Off) from On: exactly one notify");
}

#[test]
fn dpms_disable_from_off_emits_two_notifies() {
    let mut state = ServerState::new();
    state.dpms.kms_capable = true;
    state.dpms.enabled = true;
    state.dpms.power_level = 3; // Off
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    state.dpms.selected_by.insert(ClientId(1));
    let header = RequestHeader {
        opcode: 134,
        data: 5,
        length_units: 1,
    }; // Disable
    let _ = handle_dpms_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        header,
        &[],
    );
    // First notify: Off→On (level change). Second: enabled true→false.
    let bytes = read_all_available(&mut peer);
    let notifies = bytes.iter().filter(|&&b| b == 35).count();
    assert_eq!(notifies, 2, "Disable from Off must emit two XGE notifies");
    assert!(!state.dpms.enabled);
    assert_eq!(state.dpms.power_level, 0);
    // Verify ordering: byte 18 of each event is the `state`
    // (enabled) field. The first event fires from
    // apply_dpms_transition before `enabled` is cleared, so it
    // should show enabled=1; the second event fires after the
    // enable flip, so it should show enabled=0.
    assert_eq!(
        bytes[18], 1,
        "first notify is the level-change notify; enabled still true"
    );
    assert_eq!(
        bytes[32 + 18],
        0,
        "second notify is the enable-change notify; enabled now false"
    );
}

#[test]
fn dpms_disable_from_on_emits_one_notify() {
    let mut state = ServerState::new();
    state.dpms.kms_capable = true;
    state.dpms.enabled = true;
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    state.dpms.selected_by.insert(ClientId(1));
    let header = RequestHeader {
        opcode: 134,
        data: 5,
        length_units: 1,
    };
    let _ = handle_dpms_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        header,
        &[],
    );
    let bytes = read_all_available(&mut peer);
    let notifies = bytes.iter().filter(|&&b| b == 35).count();
    assert_eq!(
        notifies, 1,
        "Disable from On: only the enable-change notify fires"
    );
    assert!(!state.dpms.enabled);
}

#[test]
fn dpms_enable_when_already_enabled_emits_no_notify() {
    let mut state = ServerState::new();
    state.dpms.kms_capable = true;
    state.dpms.enabled = true;
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    state.dpms.selected_by.insert(ClientId(1));
    let header = RequestHeader {
        opcode: 134,
        data: 4,
        length_units: 1,
    }; // Enable
    let _ = handle_dpms_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        header,
        &[],
    );
    let bytes = read_all_available(&mut peer);
    assert!(!bytes.contains(&35), "no notify when state didn't change");
}

#[test]
fn dpms_set_timeouts_rejects_inverted_ordering() {
    // off=10 but suspend=20 violates the non-zero
    // off >= suspend ordering — must produce BadValue.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let header = RequestHeader {
        opcode: 134,
        data: 3,
        length_units: 2,
    }; // SetTimeouts
    // Body: standby=30, suspend=20, off=10, pad=0 — clearly out of order.
    let body = [30u8, 0, 20, 0, 10, 0, 0, 0];
    let _ = handle_dpms_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        header,
        &body,
    );
    let bytes = read_all_available(&mut peer);
    assert_eq!(bytes[0], 0, "reply is an X11 error");
    assert_eq!(bytes[1], x11::error::BAD_VALUE);
    // Timeouts must NOT have been stored.
    assert_eq!(state.dpms.standby_ms, 600_000);
    assert_eq!(state.dpms.suspend_ms, 600_000);
    assert_eq!(state.dpms.off_ms, 600_000);
}

#[test]
fn dpms_select_input_zero_removes_subscriber() {
    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    // Pre-seed: client is already subscribed.
    state.dpms.selected_by.insert(ClientId(1));
    assert!(state.dpms.selected_by.contains(&ClientId(1)));

    let header = RequestHeader {
        opcode: 134,
        data: 8,
        length_units: 2,
    }; // SelectInput
    let body = [0u8, 0, 0, 0]; // mask = 0 → unsubscribe
    let _ = handle_dpms_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        header,
        &body,
    );
    assert!(
        !state.dpms.selected_by.contains(&ClientId(1)),
        "mask=0 must remove the client from selected_by"
    );
}

#[test]
fn dpms_off_drives_screensaver_on_with_forced_true() {
    // Xorg dpms.c:269-279 — DPMS Non-On + SS Off →
    // dixSaveScreens(SCREEN_SAVER_FORCER, ScreenSaverActive)
    // → SendScreenSaverNotify(... forced=true).
    let mut state = ServerState::new();
    state.dpms.kms_capable = true;
    state.dpms.enabled = true;
    let mut peer = install_client(&mut state, 1);
    state
        .screensaver
        .selected_by
        .insert(ClientId(1), x11screensaver::SCREEN_SAVER_NOTIFY_MASK);
    let mut backend = RecordingBackend::new();

    apply_dpms_transition(&mut state, &mut backend, 3); // Off

    assert_eq!(state.screensaver.active, ScreenSaverActive::On);
    assert!(state.screensaver.forced);

    let bytes = read_all_available(&mut peer);
    // Sequential event tag is first_event + 0 = 92.
    let idx = bytes
        .iter()
        .position(|&b| b == 92)
        .expect("ScreenSaverNotify must be present");
    assert_eq!(bytes[idx + 1], x11screensaver::SCREEN_SAVER_ON, "state=On");
    assert_eq!(bytes[idx + 17], 1, "forced byte = 1");
}

#[test]
fn dpms_on_drives_screensaver_off_with_forced_false_and_no_activity_reset() {
    // Xorg dpms.c:275-278 + window.c:3187-3193 — DPMS On + SS On
    // takes the SCREEN_SAVER_OFF (not FORCER) path, so forced=0.
    // NoticeTime only fires on the FORCER+Reset combination, so
    // last_activity must NOT be touched by the coupling itself.
    let mut state = ServerState::new();
    state.dpms.kms_capable = true;
    state.dpms.enabled = true;
    state.dpms.power_level = 3;
    state.screensaver.active = ScreenSaverActive::On;
    let prior = state.dpms.last_activity;

    let mut peer = install_client(&mut state, 1);
    state
        .screensaver
        .selected_by
        .insert(ClientId(1), x11screensaver::SCREEN_SAVER_NOTIFY_MASK);
    let mut backend = RecordingBackend::new();

    apply_dpms_transition(&mut state, &mut backend, 0);

    assert_eq!(state.screensaver.active, ScreenSaverActive::Off);
    assert!(
        !state.screensaver.forced,
        "DPMS-On→SS-Off is non-FORCER path"
    );
    assert_eq!(state.dpms.last_activity, prior, "coupling must NOT reset");

    let bytes = read_all_available(&mut peer);
    let idx = bytes.iter().position(|&b| b == 92).unwrap();
    assert_eq!(bytes[idx + 1], x11screensaver::SCREEN_SAVER_OFF);
    assert_eq!(bytes[idx + 17], 0, "forced byte = 0");
}

#[test]
fn dpms_coupling_emits_screensaver_notify_before_dpms_notify() {
    // SS notify (sequential event tag 92) must appear at a lower
    // wire offset than the DPMS XGE notify (GenericEvent tag 35)
    // — matches Xorg ordering in DPMSSet (dpms.c:262-293).
    let mut state = ServerState::new();
    state.dpms.kms_capable = true;
    state.dpms.enabled = true;
    let mut peer = install_client(&mut state, 1);
    state.dpms.selected_by.insert(ClientId(1));
    state
        .screensaver
        .selected_by
        .insert(ClientId(1), x11screensaver::SCREEN_SAVER_NOTIFY_MASK);
    let mut backend = RecordingBackend::new();

    apply_dpms_transition(&mut state, &mut backend, 3);

    let bytes = read_all_available(&mut peer);
    let ss_pos = bytes.iter().position(|&b| b == 92).unwrap();
    let dpms_pos = bytes.iter().position(|&b| b == 35).unwrap();
    assert!(
        ss_pos < dpms_pos,
        "SS notify (offset {ss_pos}) must precede DPMS notify (offset {dpms_pos})"
    );
}

#[test]
fn force_screen_saver_activate_emits_notify_with_forced_true() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    state
        .screensaver
        .selected_by
        .insert(ClientId(1), x11screensaver::SCREEN_SAVER_NOTIFY_MASK);
    let mut backend = RecordingBackend::new();

    apply_screen_saver_transition(
        &mut state,
        &mut backend,
        ScreenSaverActive::On,
        /*forced=*/ true,
    );

    let bytes = read_all_available(&mut peer);
    let idx = bytes.iter().position(|&b| b == 92).unwrap();
    assert_eq!(bytes[idx + 1], x11screensaver::SCREEN_SAVER_ON);
    assert_eq!(bytes[idx + 17], 1, "forced=1");
}

#[test]
fn apply_screen_saver_transition_does_not_touch_last_activity() {
    // last_activity update is the *handler's* responsibility
    // (FORCER+Reset path runs Xorg's NoticeTime, window.c:3187-3193).
    // The pure helper must NOT touch last_activity — otherwise the
    // input-fanout SS-only sibling check (Task 3 step 5) would
    // double-bump it, and the DPMS coupling would too.
    let mut state = ServerState::new();
    state.screensaver.active = ScreenSaverActive::On;
    let prior = state.dpms.last_activity;
    let mut backend = RecordingBackend::new();

    apply_screen_saver_transition(
        &mut state,
        &mut backend,
        ScreenSaverActive::Off,
        /*forced=*/ true,
    );

    assert_eq!(state.screensaver.active, ScreenSaverActive::Off);
    assert_eq!(
        state.dpms.last_activity, prior,
        "helper alone must not touch last_activity"
    );
}

#[test]
fn force_screen_saver_invalid_mode_returns_bad_value() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let header = RequestHeader {
        opcode: 115,
        data: 2, // mode=2 — only 0 (Reset) and 1 (Activate) are valid
        length_units: 1,
    };
    let _ = handle_force_screen_saver(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        header,
    );
    let bytes = read_all_available(&mut peer);
    assert_eq!(bytes[0], 0, "error reply tag");
    assert_eq!(bytes[1], x11::error::BAD_VALUE);
    // bad_value at offset 4 = 2
    assert_eq!(
        u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
        2
    );
}

#[test]
fn force_screen_saver_activate_via_handler_transitions_to_on() {
    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let header = RequestHeader {
        opcode: 115,
        data: 1,
        length_units: 1,
    }; // mode=1 (Activate)

    let _ = handle_force_screen_saver(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        header,
    );

    assert_eq!(state.screensaver.active, ScreenSaverActive::On);
    assert!(
        state.screensaver.forced,
        "Activate is the FORCER path → forced=true"
    );
}

#[test]
fn force_screen_saver_reset_via_handler_advances_last_activity() {
    // Reset is the FORCER+Reset path Xorg runs NoticeTime on
    // (window.c:3187-3193) — the handler must bump last_activity.
    use std::time::Duration;
    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    state.screensaver.active = ScreenSaverActive::On;
    state.dpms.last_activity = std::time::Instant::now() - Duration::from_secs(120);
    let stale = state.dpms.last_activity;
    let mut backend = RecordingBackend::new();
    let header = RequestHeader {
        opcode: 115,
        data: 0,
        length_units: 1,
    }; // mode=0 (Reset)

    let _ = handle_force_screen_saver(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        header,
    );

    assert_eq!(state.screensaver.active, ScreenSaverActive::Off);
    assert!(
        state.dpms.last_activity > stale,
        "handler must bump last_activity on Reset"
    );
}

#[test]
fn set_screen_saver_stores_fields() {
    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    let header = RequestHeader {
        opcode: 107,
        data: 0,
        length_units: 3,
    };
    // timeout=300, interval=900, prefer_blanking=0 (no),
    // allow_exposures=1 (yes), pad:u16
    let body = [
        44, 1, // 300 (LE)
        132, 3, // 900 (LE)
        0, // prefer_blanking
        1, // allow_exposures
        0, 0, // pad
    ];
    let _ = handle_set_screen_saver(&mut state, ClientId(1), SequenceNumber(1), header, &body);
    assert_eq!(state.screensaver.timeout_ms, 300_000);
    assert_eq!(state.screensaver.interval_ms, 900_000);
    assert!(!state.screensaver.prefer_blanking);
    assert!(state.screensaver.allow_exposures);
}

#[test]
fn set_screen_saver_minus_one_restores_default() {
    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    state.screensaver.timeout_ms = 12_345;
    state.screensaver.interval_ms = 67_890;
    let header = RequestHeader {
        opcode: 107,
        data: 0,
        length_units: 3,
    };
    // -1 (0xffff), -1, default(2), default(2), pad
    let body = [0xff, 0xff, 0xff, 0xff, 2, 2, 0, 0];
    let _ = handle_set_screen_saver(&mut state, ClientId(1), SequenceNumber(1), header, &body);
    // defaults: timeout=600s, interval=600s, prefer=true, allow=true
    assert_eq!(state.screensaver.timeout_ms, 600_000);
    assert_eq!(state.screensaver.interval_ms, 600_000);
    assert!(state.screensaver.prefer_blanking);
    assert!(state.screensaver.allow_exposures);
}

#[test]
fn set_screen_saver_invalid_prefer_blanking_returns_bad_value() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let header = RequestHeader {
        opcode: 107,
        data: 0,
        length_units: 3,
    };
    // prefer_blanking=3 is invalid (only 0/1/2 valid).
    let body = [60, 0, 60, 0, 3, 1, 0, 0];
    let _ = handle_set_screen_saver(&mut state, ClientId(1), SequenceNumber(1), header, &body);
    let bytes = read_all_available(&mut peer);
    assert_eq!(bytes[0], 0);
    assert_eq!(bytes[1], x11::error::BAD_VALUE);
    assert_eq!(
        u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
        3
    );
}

#[test]
fn get_screen_saver_reflects_current_state() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    state.screensaver.timeout_ms = 120_000;
    state.screensaver.interval_ms = 240_000;
    state.screensaver.prefer_blanking = false;
    state.screensaver.allow_exposures = true;
    let _ = handle_get_screen_saver(&mut state, ClientId(1), SequenceNumber(1));
    let bytes = read_all_available(&mut peer);
    // Reply layout: tag(0) data(1) seq(2-3) length(4-7) timeout(8-9)
    // interval(10-11) prefer(12) allow(13) pad to 32.
    assert_eq!(bytes[0], 1, "reply tag");
    assert_eq!(
        u16::from_le_bytes([bytes[8], bytes[9]]),
        120,
        "timeout in s"
    );
    assert_eq!(
        u16::from_le_bytes([bytes[10], bytes[11]]),
        240,
        "interval in s"
    );
    assert_eq!(bytes[12], 0, "prefer_blanking = false");
    assert_eq!(bytes[13], 1, "allow_exposures = true");
}

#[test]
fn suspend_per_client_refcount_stacks() {
    // Two Suspend(true) from the same client stacks the refcount.
    // It takes two Suspend(false) calls to drain.
    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();

    let true_body = [1u8, 0, 0, 0];
    let false_body = [0u8, 0, 0, 0];
    let header = RequestHeader {
        opcode: 150,
        data: x11screensaver::SUSPEND,
        length_units: 2,
    };

    let _ = handle_screen_saver_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        header,
        &true_body,
    );
    let _ = handle_screen_saver_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(2),
        header,
        &true_body,
    );
    assert!(state.screensaver_idle_deadline().is_none(), "suspended");
    assert_eq!(
        state.screensaver.suspend_counts.get(&ClientId(1)).copied(),
        Some(2)
    );

    let _ = handle_screen_saver_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(3),
        header,
        &false_body,
    );
    assert!(
        state.screensaver_idle_deadline().is_none(),
        "still suspended"
    );

    state.screensaver.timeout_ms = 60_000; // arm the timer
    let _ = handle_screen_saver_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(4),
        header,
        &false_body,
    );
    assert!(state.screensaver_idle_deadline().is_some(), "drained");
    assert!(!state.screensaver.suspend_counts.contains_key(&ClientId(1)));
}

#[test]
fn suspend_release_last_resets_last_activity_when_screensaver_off_and_dpms_on() {
    use std::time::Duration;
    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    state.screensaver.timeout_ms = 60_000;
    state.dpms.last_activity = std::time::Instant::now() - Duration::from_secs(120);
    let stale = state.dpms.last_activity;
    state.screensaver.suspend_counts.insert(ClientId(1), 1);
    let mut backend = RecordingBackend::new();

    let header = RequestHeader {
        opcode: 150,
        data: x11screensaver::SUSPEND,
        length_units: 2,
    };
    let _ = handle_screen_saver_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        header,
        &[0u8, 0, 0, 0],
    );

    assert!(
        state.dpms.last_activity > stale,
        "last_activity must advance"
    );
}

#[test]
fn force_screen_saver_activate_still_works_while_suspended() {
    // Xorg saver.c: "suspending it (by design) doesn't prevent it
    // from being forcibly activated".
    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    state.screensaver.suspend_counts.insert(ClientId(2), 1);
    let mut backend = RecordingBackend::new();

    let header = RequestHeader {
        opcode: 115,
        data: 1,
        length_units: 1,
    }; // Activate
    let _ = handle_force_screen_saver(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        header,
    );

    assert_eq!(state.screensaver.active, ScreenSaverActive::On);
}

#[test]
fn screen_saver_query_version_returns_one_one() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let header = RequestHeader {
        opcode: 150,
        data: x11screensaver::QUERY_VERSION,
        length_units: 2,
    };
    let _ = handle_screen_saver_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        header,
        &[1, 1, 0, 0],
    );

    let bytes = read_all_available(&mut peer);
    assert_eq!(bytes[0], 1, "reply tag");
    assert_eq!(
        u16::from_le_bytes([bytes[8], bytes[9]]),
        x11screensaver::SERVER_MAJOR_VERSION
    );
    assert_eq!(
        u16::from_le_bytes([bytes[10], bytes[11]]),
        x11screensaver::SERVER_MINOR_VERSION
    );
}

#[test]
fn screen_saver_query_info_returns_disabled_when_timeout_zero() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    state.screensaver.timeout_ms = 0;
    let mut backend = RecordingBackend::new();
    let header = RequestHeader {
        opcode: 150,
        data: x11screensaver::QUERY_INFO,
        length_units: 2,
    };
    let drawable = ROOT_WINDOW.0.to_le_bytes();
    let _ = handle_screen_saver_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        header,
        &drawable,
    );

    let bytes = read_all_available(&mut peer);
    assert_eq!(bytes[1], x11screensaver::SCREEN_SAVER_DISABLED);
    assert_eq!(
        u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
        crate::resources::SCREEN_SAVER_WINDOW.0,
        "the screen's saver window id, set or not (`Xext/saver.c:665`)"
    );
    assert_eq!(
        u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]),
        0,
        "til_or_since = 0 in Disabled"
    );
    assert_eq!(
        bytes[24],
        x11screensaver::SCREEN_SAVER_BLANKED,
        "kind = Blanked (prefer_blanking default true)"
    );
}

#[test]
fn screen_saver_query_info_off_carries_til_remaining_and_caller_mask() {
    use std::time::Duration;
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let _ = install_client(&mut state, 2); // other subscriber; must not influence reply
    state.screensaver.timeout_ms = 60_000;
    state.dpms.last_activity = std::time::Instant::now() - Duration::from_millis(20_000);
    state
        .screensaver
        .selected_by
        .insert(ClientId(1), x11screensaver::SCREEN_SAVER_NOTIFY_MASK);
    state
        .screensaver
        .selected_by
        .insert(ClientId(2), x11screensaver::SCREEN_SAVER_CYCLE_MASK);
    let mut backend = RecordingBackend::new();

    let header = RequestHeader {
        opcode: 150,
        data: x11screensaver::QUERY_INFO,
        length_units: 2,
    };
    let _ = handle_screen_saver_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        header,
        &ROOT_WINDOW.0.to_le_bytes(),
    );

    let bytes = read_all_available(&mut peer);
    assert_eq!(bytes[1], x11screensaver::SCREEN_SAVER_OFF, "state Off");
    let til = u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);
    assert!(
        til > 30_000 && til <= 40_000,
        "til_remaining ≈ 60_000 - idle(~20_000); got {til}"
    );
    let idle = u32::from_le_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    assert!(
        (19_000..=30_000).contains(&idle),
        "idle≈20_000ms (slack for test scheduling); got {idle}"
    );
    let mask = u32::from_le_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    assert_eq!(
        mask,
        x11screensaver::SCREEN_SAVER_NOTIFY_MASK,
        "event_mask is caller's only, not union with client 2"
    );
}

#[test]
fn screen_saver_query_info_on_state_uses_til_since_underflow_when_idle_lt_timeout() {
    use std::time::Duration;
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    state.screensaver.active = ScreenSaverActive::On;
    state.screensaver.timeout_ms = 60_000;
    state.dpms.last_activity = std::time::Instant::now() - Duration::from_millis(5_000);
    let mut backend = RecordingBackend::new();

    let header = RequestHeader {
        opcode: 150,
        data: x11screensaver::QUERY_INFO,
        length_units: 2,
    };
    let _ = handle_screen_saver_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        header,
        &ROOT_WINDOW.0.to_le_bytes(),
    );

    let bytes = read_all_available(&mut peer);
    assert_eq!(bytes[1], x11screensaver::SCREEN_SAVER_ON);
    let til = u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);
    assert!(
        til > 0xff00_0000,
        "underflow wrap expected when idle < timeout; got 0x{til:08x}"
    );
}

#[test]
fn screen_saver_query_info_kind_reflects_prefer_blanking() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    state.screensaver.prefer_blanking = false;
    let mut backend = RecordingBackend::new();
    let header = RequestHeader {
        opcode: 150,
        data: x11screensaver::QUERY_INFO,
        length_units: 2,
    };
    let _ = handle_screen_saver_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        header,
        &ROOT_WINDOW.0.to_le_bytes(),
    );
    let bytes = read_all_available(&mut peer);
    assert_eq!(bytes[24], x11screensaver::SCREEN_SAVER_INTERNAL);
}

#[test]
fn screen_saver_select_input_accepts_unknown_mask_bits() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let header = RequestHeader {
        opcode: 150,
        data: x11screensaver::SELECT_INPUT,
        length_units: 3,
    };
    let mut body = Vec::new();
    body.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
    body.extend_from_slice(&0x0000_0004u32.to_le_bytes());
    let _ = handle_screen_saver_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        header,
        &body,
    );

    let bytes = read_all_available(&mut peer);
    assert!(bytes.is_empty(), "SelectInput has no reply on success");
    assert_eq!(
        state.screensaver.selected_by.get(&ClientId(1)).copied(),
        Some(0x04)
    );

    apply_screen_saver_transition(
        &mut state,
        &mut backend,
        ScreenSaverActive::On,
        /*forced=*/ false,
    );
    let bytes2 = read_all_available(&mut peer);
    assert!(
        !bytes2.contains(&92),
        "client without NOTIFY_MASK bit must not receive notify"
    );
}

/// `ScreenSaverSetAttributes` (`Xext/saver.c:840-845`): the first
/// client gets the attributes, another BadAccess until the first
/// unsets them; activation then creates and maps the server's saver
/// window, override-redirect, and deactivation destroys it
/// (`CreateSaverWindow` / `DestroySaverWindow`). dtsession sets
/// 1x1 InputOutput, mask 0, and exits on BadAccess.
#[test]
fn screen_saver_set_attributes_belongs_to_one_client_and_shows_a_window() {
    let mut state = ServerState::new();
    let mut peer1 = install_client(&mut state, 1);
    let mut peer2 = install_client(&mut state, 2);
    let mut backend = RecordingBackend::new();
    let mut body = ROOT_WINDOW.0.to_le_bytes().to_vec();
    for v in [0u16, 0, 1, 1, 0] {
        body.extend_from_slice(&v.to_le_bytes());
    }
    body.extend_from_slice(&[0, 0]); // CopyFromParent class and depth
    body.extend_from_slice(&0u32.to_le_bytes()); // visual
    body.extend_from_slice(&0u32.to_le_bytes()); // mask
    let request =
        |state: &mut ServerState, backend: &mut RecordingBackend, client, minor, body: &[u8]| {
            let header = RequestHeader {
                opcode: 150,
                data: minor,
                length_units: u32::try_from(1 + body.len() / 4).unwrap(),
            };
            let _ = handle_screen_saver_request(
                state,
                backend,
                ClientId(client),
                SequenceNumber(1),
                header,
                body,
            );
        };
    request(
        &mut state,
        &mut backend,
        1,
        x11screensaver::SET_ATTRIBUTES,
        &body,
    );
    assert!(
        read_all_available(&mut peer1).is_empty(),
        "client 1: no error"
    );
    request(
        &mut state,
        &mut backend,
        2,
        x11screensaver::SET_ATTRIBUTES,
        &body,
    );
    let bytes = read_all_available(&mut peer2);
    assert_eq!((bytes[0], bytes[1]), (0, x11::error::BAD_ACCESS));

    let saver = crate::resources::SCREEN_SAVER_WINDOW;
    apply_screen_saver_transition(&mut state, &mut backend, ScreenSaverActive::On, true);
    let w = state.resources.window(saver).expect("saver window");
    assert_eq!((w.width, w.height, w.parent), (1, 1, ROOT_WINDOW));
    assert!(w.override_redirect);
    assert_eq!(w.map_state, MapState::Viewable);
    apply_screen_saver_transition(&mut state, &mut backend, ScreenSaverActive::Off, true);
    assert!(state.resources.window(saver).is_none());

    request(
        &mut state,
        &mut backend,
        1,
        x11screensaver::UNSET_ATTRIBUTES,
        &ROOT_WINDOW.0.to_le_bytes(),
    );
    request(
        &mut state,
        &mut backend,
        2,
        x11screensaver::SET_ATTRIBUTES,
        &body,
    );
    assert!(
        read_all_available(&mut peer2).is_empty(),
        "client 2 after the unset"
    );
}

#[test]
fn screen_saver_unset_attributes_returns_success_noop() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let header = RequestHeader {
        opcode: 150,
        data: x11screensaver::UNSET_ATTRIBUTES,
        length_units: 2,
    };
    let drawable = ROOT_WINDOW.0.to_le_bytes();
    let _ = handle_screen_saver_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        header,
        &drawable,
    );

    let bytes = read_all_available(&mut peer);
    assert!(bytes.is_empty(), "UnsetAttributes must be a silent no-op");
}

#[test]
fn cycle_event_delivered_only_to_cycle_mask_subscribers() {
    // Two clients: A subscribed with NOTIFY_MASK, B with CYCLE_MASK.
    // After SS goes Active and we fire a Cycle event, only B sees it.
    let mut state = ServerState::new();
    let mut peer_a = install_client(&mut state, 1);
    let mut peer_b = install_client(&mut state, 2);
    state
        .screensaver
        .selected_by
        .insert(ClientId(1), x11screensaver::SCREEN_SAVER_NOTIFY_MASK);
    state
        .screensaver
        .selected_by
        .insert(ClientId(2), x11screensaver::SCREEN_SAVER_CYCLE_MASK);
    let mut backend = RecordingBackend::new();

    apply_screen_saver_transition(
        &mut state,
        &mut backend,
        ScreenSaverActive::On,
        /*forced=*/ false,
    );
    // A received the activation Notify; drain so the next read is clean.
    let _ = read_all_available(&mut peer_a);
    // B was NOT a NOTIFY subscriber; it should have received nothing yet.
    assert!(read_all_available(&mut peer_b).is_empty());

    emit_screen_saver_notify(&mut state, ScreenSaverActive::Cycle, /*forced=*/ false);
    assert!(
        read_all_available(&mut peer_a).is_empty(),
        "Cycle must NOT deliver to NOTIFY_MASK subscriber"
    );
    let b = read_all_available(&mut peer_b);
    let idx = b.iter().position(|&x| x == 92).expect("B receives Cycle");
    assert_eq!(b[idx + 1], x11screensaver::SCREEN_SAVER_CYCLE);
}

#[test]
fn suspend_release_resets_idletime_last_evaluated_and_per_device_baselines() {
    use std::time::Duration;
    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();

    // Pre-seed: stale idletime cache from before suspend.
    state
        .idletime_last_evaluated
        .insert(x11sync::IDLETIME_COUNTER, 999_999);
    state
        .per_device_last_activity
        .insert(3, std::time::Instant::now() - Duration::from_secs(120));

    // Insert a suspending client, then drain via Suspend(false).
    state.screensaver.suspend_counts.insert(ClientId(1), 1);
    let header = RequestHeader {
        opcode: 150,
        data: x11screensaver::SUSPEND,
        length_units: 2,
    };
    let _ = handle_screen_saver_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        header,
        &[0u8, 0, 0, 0],
    );

    // The drain hit the (drained && empty && SS=Off && DPMS=On) guard
    // and must have reset both IDLETIME bookkeeping fields.
    assert!(
        state.idletime_last_evaluated.is_empty(),
        "idletime_last_evaluated must be cleared on suspend release"
    );
    let vck_baseline = state
        .per_device_last_activity
        .get(&3)
        .copied()
        .expect("per_device_last_activity[3] still present");
    let elapsed = std::time::Instant::now().duration_since(vck_baseline);
    assert!(
        elapsed < Duration::from_millis(100),
        "per_device_last_activity[3] must be reset to ~now; got elapsed {elapsed:?}"
    );
}
