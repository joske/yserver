use super::*;

fn xi_xtest_grab_body(device_id: u16, owner_events: bool) -> Vec<u8> {
    let mut body = Vec::with_capacity(20);
    body.extend_from_slice(&yserver_core::resources::ROOT_WINDOW.0.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes()); // CurrentTime
    body.extend_from_slice(&0u32.to_le_bytes()); // cursor
    body.extend_from_slice(&device_id.to_le_bytes());
    body.extend_from_slice(&[1, 1, u8::from(owner_events), 0]); // async modes
    body.extend_from_slice(&0u16.to_le_bytes()); // mask_len
    body
}

fn xi_xtest_query_attachment(
    state: &mut ServerState,
    backend: &mut KmsBackend,
    peer: &mut std::os::unix::net::UnixStream,
    sequence: u16,
    device_id: u16,
) -> (u8, u16, u16) {
    let mut body = Vec::with_capacity(4);
    body.extend_from_slice(&device_id.to_le_bytes());
    body.extend_from_slice(&[0; 2]);
    let reply = xi_xtest_grab_request(state, backend, peer, sequence, 137, 48, &body);
    assert_eq!(reply.first(), Some(&1), "XIQueryDevice reply: {reply:02x?}");
    assert_eq!(
        u16::from_le_bytes(reply[8..10].try_into().unwrap()),
        1,
        "one requested device"
    );
    assert_eq!(
        u16::from_le_bytes(reply[32..34].try_into().unwrap()),
        device_id,
        "queried device id"
    );
    (
        reply[42],
        u16::from_le_bytes(reply[34..36].try_into().unwrap()),
        u16::from_le_bytes(reply[36..38].try_into().unwrap()),
    )
}

fn xi_xtest_generic_event_types(wire: &[u8]) -> Vec<u16> {
    let mut event_types = Vec::new();
    let mut offset = 0;
    while offset < wire.len() {
        let event_type = wire[offset];
        let extra_units = usize::try_from(u32::from_le_bytes(
            wire[offset + 4..offset + 8].try_into().unwrap(),
        ))
        .unwrap();
        let length = 32 + extra_units * 4;
        if event_type == 35 {
            event_types.push(u16::from_le_bytes(
                wire[offset + 8..offset + 10].try_into().unwrap(),
            ));
        } else {
            assert_eq!(event_type, 1, "reply or XI2 GenericEvent: {wire:02x?}");
        }
        offset += length;
    }
    assert_eq!(offset, wire.len(), "complete X11 event stream");
    event_types
}

fn xi_xtest_grab_select_hierarchy(
    state: &mut ServerState,
    backend: &mut KmsBackend,
    peer: &mut std::os::unix::net::UnixStream,
    sequence: u16,
    enabled: bool,
) -> Vec<u8> {
    let mut body = Vec::with_capacity(16);
    body.extend_from_slice(&yserver_core::resources::ROOT_WINDOW.0.to_le_bytes());
    body.extend_from_slice(&1u16.to_le_bytes()); // one event mask
    body.extend_from_slice(&[0; 2]);
    body.extend_from_slice(&0u16.to_le_bytes()); // XIAllDevices
    body.extend_from_slice(&1u16.to_le_bytes()); // one 32-bit mask unit
    let mask = if enabled { 1u32 << 11 } else { 0 };
    body.extend_from_slice(&mask.to_le_bytes());
    xi_xtest_grab_request(state, backend, peer, sequence, 137, 46, &body)
}

fn xi_xtest_fake_motion(detail: u8, x: i16, y: i16) -> Vec<u8> {
    let mut body = vec![0u8; 32];
    body[0] = 6; // MotionNotify
    body[1] = detail;
    body[20..22].copy_from_slice(&x.to_le_bytes());
    body[22..24].copy_from_slice(&y.to_le_bytes());
    body
}

fn xi_xtest_fake_key(event_type: u8, keycode: u8, device_id: u8) -> Vec<u8> {
    let mut body = vec![0u8; 32];
    body[0] = event_type;
    body[1] = keycode;
    body[31] = device_id;
    body
}

/// Checks the complete empty end state for the local `ServerState` rows in
/// this module. Existing KMS end-state helpers take fixture wrappers, while
/// these XTest validation rows own state directly, so this helper verifies
/// their grabs, freezes, detaches, and every held-button store in one place.
fn assert_xtest_valuator_end_state(state: &ServerState) {
    assert!(state.active_pointer_grab.is_none(), "master grab ended");
    assert!(state.xi2_pointer_grabs.is_empty(), "slave grabs ended");
    assert!(state.xi1_active_grabs.is_empty(), "XI1 grabs ended");
    assert!(state.xi1_frozen.is_empty(), "XI1 freeze entries ended");
    assert!(
        state.xi2_detached_masters.is_empty(),
        "no slave remains detached"
    );
    assert_eq!(state.buttons_down, 0, "all master buttons are released");
    assert!(
        state
            .xi_devices
            .devices()
            .iter()
            .all(|device| device.buttons_down == 0),
        "every source's held-button set is empty"
    );
    assert!(
        state
            .xi1_device_input_state
            .values()
            .all(|device| device.buttons_down.iter().all(|byte| *byte == 0)),
        "every XI1 device button set is empty"
    );
    assert!(
        state
            .unpublished_pointer_buttons_down
            .values()
            .all(|buttons| *buttons == 0),
        "unpublished source held-button sets are empty"
    );
}

/// Exercises each Xorg DeviceValuator validation row through the existing
/// `process_request` harness. One table-driven test keeps the XTEST event,
/// XI2 selection and device records real while allowing the red run to reach
/// every malformed row and both passing guards on the unmodified handler.
#[test]
fn xtest_device_valuator_validation_matches_xorg() {
    use std::os::unix::net::UnixStream;

    const CLIENT: u32 = 5;
    const XTEST: u8 = 146;
    const FAKE_INPUT: u8 = 2;
    const XINPUT: u8 = 137;
    const XI_SELECT_EVENTS: u8 = 46;
    const XI_KEY_PRESS: u8 = 2;
    const XI_BUTTON_PRESS: u8 = 4;
    const XI_BUTTON_RELEASE: u8 = 5;
    const XI_MOTION: u8 = 6;

    // DeviceValuator is XInput event 66, followed by key, button and motion
    // events 67 through 71. The XTEST pointer's CorePointer record has two
    // ValuatorClass axes; the XTEST keyboard record has none.
    let make_fake =
        |event_type: u8, detail: u8, device_id: u8, trailing: &[(u8, u8, u8, [i32; 6])]| {
            let mut body = vec![0u8; 32];
            body[0] = event_type;
            body[1] = detail;
            body[31] = device_id;
            for &(trailing_type, num_valuators, first_valuator, values) in trailing {
                let mut valuator = vec![0u8; 32];
                valuator[0] = trailing_type;
                valuator[1] = device_id;
                valuator[6] = num_valuators;
                valuator[7] = first_valuator;
                for (index, value) in values.into_iter().enumerate() {
                    let offset = 8 + index * 4;
                    valuator[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
                }
                body.extend_from_slice(&valuator);
            }
            body
        };
    let select = |state: &mut ServerState,
                  backend: &mut KmsBackend,
                  peer: &mut UnixStream,
                  sequence: u16,
                  device_id: u16,
                  mask: u32| {
        let mut body = Vec::with_capacity(16);
        body.extend_from_slice(&yserver_core::resources::ROOT_WINDOW.0.to_le_bytes());
        body.extend_from_slice(&1u16.to_le_bytes()); // one event mask
        body.extend_from_slice(&[0; 2]);
        body.extend_from_slice(&device_id.to_le_bytes());
        body.extend_from_slice(&1u16.to_le_bytes()); // one 32-bit mask unit
        body.extend_from_slice(&mask.to_le_bytes());
        xi_xtest_grab_request(
            state,
            backend,
            peer,
            sequence,
            XINPUT,
            XI_SELECT_EVENTS,
            &body,
        )
    };

    let malformed_rows = [
        (
            "first_valuator_past_num_axes",
            4u8,
            71u8,
            0u8,
            vec![(66u8, 1u8, 3u8, [300, 200, 0, 0, 0, 0])],
            71u32,
            1u32 << XI_MOTION,
            None,
        ),
        (
            "trailing_event_on_device_without_valuators",
            5,
            67,
            38,
            vec![(66, 1, 0, [0; 6])],
            0,
            1u32 << XI_KEY_PRESS,
            Some(68u8),
        ),
        (
            "trailing_event_type_is_not_device_valuator",
            4,
            69,
            1,
            vec![(67, 1, 0, [0; 6])],
            67,
            1u32 << XI_BUTTON_PRESS,
            Some(70u8),
        ),
        (
            "first_valuator_is_not_contiguous",
            4,
            71,
            0,
            vec![
                (66, 1, 0, [300, 200, 0, 0, 0, 0]),
                (66, 1, 2, [100, 0, 0, 0, 0, 0]),
            ],
            2,
            1u32 << XI_MOTION,
            None,
        ),
        (
            "num_valuators_is_outside_one_through_six",
            4,
            71,
            0,
            vec![(66, 0, 0, [300, 200, 0, 0, 0, 0])],
            0,
            1u32 << XI_MOTION,
            None,
        ),
        (
            "valuator_sum_exceeds_num_axes",
            4,
            71,
            0,
            vec![(66, 3, 0, [300, 200, 100, 0, 0, 0])],
            3,
            1u32 << XI_MOTION,
            None,
        ),
        (
            "device_motion_detail_is_outside_zero_or_one",
            4,
            71,
            2,
            vec![(66, 2, 0, [300, 200, 0, 0, 0, 0])],
            2,
            1u32 << XI_MOTION,
            None,
        ),
        (
            "review_first_zero_num_six_on_two_axis_pointer",
            4,
            71,
            0,
            vec![(66, 6, 0, [300, 200, 100, 50, 25, 10])],
            6,
            1u32 << XI_MOTION,
            None,
        ),
    ];

    let mut failures = Vec::new();
    for (name, device_id, event_type, detail, trailing, error_value, mask, cleanup) in
        malformed_rows
    {
        let mut backend = KmsBackend::for_tests();
        let mut state = ServerState::new();
        let mut peer = kbd_map_client_id(&mut state, CLIENT);
        let start_position = state.pointer_root;
        let start_held = xi_xtest_held_snapshot(&state, &backend);
        let selected = select(
            &mut state,
            &mut backend,
            &mut peer,
            1,
            u16::from(device_id),
            mask,
        );
        if !selected.is_empty() {
            failures.push(format!(
                "{name}: XISelectEvents setup returned {selected:02x?}"
            ));
        }

        let request = make_fake(event_type, detail, device_id, &trailing);
        let wire = xi_xtest_grab_request(
            &mut state,
            &mut backend,
            &mut peer,
            2,
            XTEST,
            FAKE_INPUT,
            &request,
        );
        if wire.len() != 32 || wire.first() != Some(&0) {
            failures.push(format!(
                "{name}: expected only one BadValue packet; got {wire:02x?}"
            ));
        } else {
            let got_error_value = u32::from_le_bytes(wire[4..8].try_into().unwrap());
            let got_minor = u16::from_le_bytes(wire[8..10].try_into().unwrap());
            if (
                wire[1],
                got_error_value,
                got_minor,
                wire[10],
                u16::from_le_bytes([wire[2], wire[3]]),
            ) != (2, error_value, 2, XTEST, 2)
            {
                failures.push(format!(
                    "{name}: error tuple was (code={}, value={got_error_value}, minor={got_minor}, major={}, sequence={}), expected (2, {error_value}, 2, {XTEST}, 2)",
                    wire[1], wire[10], u16::from_le_bytes([wire[2], wire[3]])
                ));
            }
        }
        if state.pointer_root != start_position {
            failures.push(format!(
                "{name}: rejected request moved sprite from {start_position:?} to {:?}",
                state.pointer_root
            ));
        }

        // If the untouched handler accepted an invalid press, release it
        // through the same request path so the row still finishes cleanly.
        if let Some(release_type) = cleanup {
            let release = make_fake(release_type, detail, device_id, &[]);
            let _ = xi_xtest_grab_request(
                &mut state,
                &mut backend,
                &mut peer,
                3,
                XTEST,
                FAKE_INPUT,
                &release,
            );
        }

        if xi_xtest_held_snapshot(&state, &backend) != start_held
            || !state.xi2_pointer_grabs.is_empty()
            || !state.xi2_keyboard_grabs.is_empty()
            || !state.xi1_active_grabs.is_empty()
            || state.active_pointer_grab.is_some()
        {
            failures.push(format!(
                "{name}: input/grab state did not return to its start state"
            ));
        }
        assert_xtest_valuator_end_state(&state);
    }

    // GUARD: valid absolute and relative DeviceMotionNotify chunks retain the
    // base behavior on the two-axis XTEST pointer and produce encoded events.
    {
        let mut backend = KmsBackend::for_tests();
        let mut state = ServerState::new();
        let mut peer = kbd_map_client_id(&mut state, CLIENT);
        let start_held = xi_xtest_held_snapshot(&state, &backend);
        assert!(select(&mut state, &mut backend, &mut peer, 1, 4, 1 << XI_MOTION).is_empty());

        let absolute = make_fake(71, 0, 4, &[(66, 2, 0, [220, 240, 0, 0, 0, 0])]);
        let absolute_wire = xi_xtest_grab_request(
            &mut state,
            &mut backend,
            &mut peer,
            2,
            XTEST,
            FAKE_INPUT,
            &absolute,
        );
        assert!(
            xi_xtest_generic_event_types(&absolute_wire).contains(&u16::from(XI_MOTION)),
            "valid absolute DeviceMotionNotify must be delivered: {absolute_wire:02x?}"
        );
        assert_eq!(state.pointer_root, (220, 240));

        let relative = make_fake(71, 1, 4, &[(66, 2, 0, [12, -7, 0, 0, 0, 0])]);
        let relative_wire = xi_xtest_grab_request(
            &mut state,
            &mut backend,
            &mut peer,
            3,
            XTEST,
            FAKE_INPUT,
            &relative,
        );
        assert!(
            xi_xtest_generic_event_types(&relative_wire).contains(&u16::from(XI_MOTION)),
            "valid relative DeviceMotionNotify must be delivered: {relative_wire:02x?}"
        );
        assert_eq!(state.pointer_root, (232, 233));
        assert_eq!(xi_xtest_held_snapshot(&state, &backend), start_held);
        assert!(state.xi2_pointer_grabs.is_empty());
        assert!(state.xi2_keyboard_grabs.is_empty());
        assert!(state.xi1_active_grabs.is_empty());
        assert!(state.active_pointer_grab.is_none());
        assert_xtest_valuator_end_state(&state);
    }

    // GUARD: DeviceButtonPress without a trailing DeviceValuator is legal.
    {
        let mut backend = KmsBackend::for_tests();
        let mut state = ServerState::new();
        let mut peer = kbd_map_client_id(&mut state, CLIENT);
        let start_held = xi_xtest_held_snapshot(&state, &backend);
        assert!(
            select(
                &mut state,
                &mut backend,
                &mut peer,
                1,
                4,
                (1 << XI_BUTTON_PRESS) | (1 << XI_BUTTON_RELEASE),
            )
            .is_empty()
        );

        let press = make_fake(69, 1, 4, &[]);
        let press_wire = xi_xtest_grab_request(
            &mut state,
            &mut backend,
            &mut peer,
            2,
            XTEST,
            FAKE_INPUT,
            &press,
        );
        assert!(
            xi_xtest_generic_event_types(&press_wire).contains(&u16::from(XI_BUTTON_PRESS)),
            "DeviceButtonPress without valuators is accepted: {press_wire:02x?}"
        );

        let release = make_fake(70, 1, 4, &[]);
        let release_wire = xi_xtest_grab_request(
            &mut state,
            &mut backend,
            &mut peer,
            3,
            XTEST,
            FAKE_INPUT,
            &release,
        );
        assert!(
            xi_xtest_generic_event_types(&release_wire).contains(&u16::from(XI_BUTTON_RELEASE)),
            "DeviceButtonRelease without valuators is accepted: {release_wire:02x?}"
        );
        assert_eq!(xi_xtest_held_snapshot(&state, &backend), start_held);
        assert!(state.xi2_pointer_grabs.is_empty());
        assert!(state.xi2_keyboard_grabs.is_empty());
        assert!(state.xi1_active_grabs.is_empty());
        assert!(state.active_pointer_grab.is_none());
        assert_xtest_valuator_end_state(&state);
    }

    assert!(
        failures.is_empty(),
        "Xorg DeviceValuator validation mismatches (the guard scenarios above must pass):\n{}",
        failures.join("\n")
    );
}

/// XTEST FakeInput MotionNotify with detail = 1 is a relative move: Xorg
/// (Xext/xtest.c ProcXTestFakeInput) passes rootX/rootY as the valuators
/// without POINTER_ABSOLUTE, so GetPointerEvents adds them to the current
/// position (no acceleration: XTEST doesn't set POINTER_ACCELERATE) and
/// clips to the screen. yserver dropped relative fakes, so e.g.
/// `xdotool mousemove_relative` did nothing.
#[test]
fn xtest_relative_motion_moves_by_the_delta() {
    const XTEST: u8 = 146;
    const FAKE_INPUT: u8 = 2;
    const MOTION_NOTIFY: u8 = 6;
    let fake_motion = |detail: u8, x: i16, y: i16| {
        let mut body = vec![0u8; 32];
        body[0] = MOTION_NOTIFY;
        body[1] = detail;
        body[20..22].copy_from_slice(&x.to_le_bytes());
        body[22..24].copy_from_slice(&y.to_le_bytes());
        body
    };
    let mut backend = KmsBackend::for_tests();
    let mut state = yserver_core::server::ServerState::new();
    let _peer = kbd_map_client(&mut state);
    kbd_map_request(
        &mut state,
        &mut backend,
        XTEST,
        FAKE_INPUT,
        &fake_motion(0, 200, 200),
    );
    assert_eq!(state.pointer_root, (200, 200), "absolute fake motion");
    kbd_map_request(
        &mut state,
        &mut backend,
        XTEST,
        FAKE_INPUT,
        &fake_motion(1, 10, -5),
    );
    assert_eq!(state.pointer_root, (210, 195), "relative +10,-5");
    kbd_map_request(
        &mut state,
        &mut backend,
        XTEST,
        FAKE_INPUT,
        &fake_motion(1, -300, -300),
    );
    assert_eq!(
        state.pointer_root,
        (0, 0),
        "relative move clipped to the screen"
    );
}

// Kills: restoring `detach_xi2_slave`'s `facet.is_none()` early return,
// which leaves XTEST pointer 4 attached during an explicit XI2 grab.
#[test]
fn xi_xtest_grab_pointer_floats_routes_and_reattaches() {
    use yserver_core::server::ServerState;

    const CLIENT: u32 = 5;
    const XTEST: u8 = 146;
    const FAKE_INPUT: u8 = 2;
    let mut backend = KmsBackend::for_tests();
    let mut state = ServerState::new();
    let mut peer = kbd_map_client_id(&mut state, CLIENT);
    let registry = xi_xtest_registry_snapshot(&state);
    let properties = xi_xtest_property_snapshot(&state);
    let held = xi_xtest_held_snapshot(&state, &backend);
    let selections = state.clients[&CLIENT].xi2_masks.clone();
    let xi1_selections = state.clients[&CLIENT].xi1_event_classes.clone();
    let xi1_window_selections = state.clients[&CLIENT].xi1_window_event_classes.clone();
    let detached = state.xi2_detached_masters.clone();
    let floating_positions = state.floating_pointer_positions.clone();
    let master_position = state.pointer_root;
    let master_cursor = (backend.core.cursor_x, backend.core.cursor_y);

    assert!(
        xi_xtest_grab_select_hierarchy(&mut state, &mut backend, &mut peer, 1, true).is_empty()
    );
    let grab = xi_xtest_grab_request(
        &mut state,
        &mut backend,
        &mut peer,
        2,
        137,
        51,
        &xi_xtest_grab_body(4, false),
    );
    assert_eq!(
        xi_xtest_grab_reply_status(&grab),
        Some(0),
        "successful XIGrabDevice reply"
    );
    assert!(
        !xi_xtest_generic_event_types(&grab).contains(&11),
        "Xorg's DetachFromMaster → AttachDevice sends no hierarchy event"
    );
    assert_eq!(
        xi_xtest_query_attachment(&mut state, &mut backend, &mut peer, 3, 4),
        (1, 5, 0),
        "XTEST pointer 4 floats for the grab"
    );

    let motion = xi_xtest_fake_motion(1, 13, 7);
    assert!(
        xi_xtest_grab_request(
            &mut state,
            &mut backend,
            &mut peer,
            4,
            XTEST,
            FAKE_INPUT,
            &motion
        )
        .is_empty()
    );
    assert_eq!(
        state.pointer_root, master_position,
        "floating XTEST motion leaves the master pointer alone"
    );
    assert_eq!(
        (backend.core.cursor_x, backend.core.cursor_y),
        master_cursor
    );
    assert_eq!(
        state.floating_pointer_positions.get(&4),
        Some(&(
            f32::from(master_position.0) + 13.0,
            f32::from(master_position.1) + 7.0
        )),
        "XTEST pointer keeps its own relative position"
    );

    let ungrab = xi_xtest_grab_request(
        &mut state,
        &mut backend,
        &mut peer,
        5,
        137,
        52,
        &xi_xtest_ungrab_body(4),
    );
    assert!(ungrab.is_empty(), "XIUngrabDevice has no reply");
    assert_eq!(state.xi_devices.device(4).unwrap().attached_master, Some(2));
    assert!(!state.floating_pointer_positions.contains_key(&4));
    let next_motion = xi_xtest_fake_motion(1, 3, 0);
    let _ = xi_xtest_grab_request(
        &mut state,
        &mut backend,
        &mut peer,
        6,
        XTEST,
        FAKE_INPUT,
        &next_motion,
    );
    assert_eq!(
        state.pointer_root,
        (master_position.0 + 3, master_position.1)
    );

    assert!(
        xi_xtest_grab_select_hierarchy(&mut state, &mut backend, &mut peer, 7, false).is_empty()
    );
    assert_eq!(xi_xtest_registry_snapshot(&state), registry);
    assert_eq!(xi_xtest_property_snapshot(&state), properties);
    assert_eq!(xi_xtest_held_snapshot(&state, &backend), held);
    assert_eq!(state.xi2_detached_masters, detached);
    assert_eq!(state.floating_pointer_positions, floating_positions);
    assert!(state.xi2_pointer_grabs.is_empty());
    assert!(state.xi2_keyboard_grabs.is_empty());
    assert!(backend.floating_keyboard_states.is_empty());
    assert_eq!(state.clients[&CLIENT].xi2_masks, selections);
    assert_eq!(state.clients[&CLIENT].xi1_event_classes, xi1_selections);
    assert_eq!(
        state.clients[&CLIENT].xi1_window_event_classes,
        xi1_window_selections
    );
}

// Kills: allowing detach only for pointer facets, which leaves XTEST
// keyboard 5 attached and routes its explicit-grab key state to master XKB.
#[test]
fn xi_xtest_grab_keyboard_uses_floating_xkb_state() {
    use yserver_core::server::ServerState;

    const CLIENT: u32 = 5;
    const XTEST: u8 = 146;
    const FAKE_INPUT: u8 = 2;
    const KEYCODE: u8 = 38;
    const XI_KEY_PRESS: u8 = 66 + yserver_core::xinput::XI_DEVICE_KEY_PRESS_OFFSET;
    const XI_KEY_RELEASE: u8 = 66 + yserver_core::xinput::XI_DEVICE_KEY_RELEASE_OFFSET;

    let mut backend = KmsBackend::for_tests();
    let mut state = ServerState::new();
    let mut peer = kbd_map_client_id(&mut state, CLIENT);
    let registry = xi_xtest_registry_snapshot(&state);
    let properties = xi_xtest_property_snapshot(&state);
    let held = xi_xtest_held_snapshot(&state, &backend);
    let selections = state.clients[&CLIENT].xi2_masks.clone();
    let master_modifiers = backend.serialize_modifiers();

    let grab = xi_xtest_grab_request(
        &mut state,
        &mut backend,
        &mut peer,
        1,
        137,
        51,
        &xi_xtest_grab_body(5, false),
    );
    assert_eq!(xi_xtest_grab_reply_status(&grab), Some(0));
    assert_eq!(
        xi_xtest_query_attachment(&mut state, &mut backend, &mut peer, 2, 5),
        (1, 5, 0),
        "XTEST keyboard 5 floats for the grab"
    );

    let press = xi_xtest_fake_key(XI_KEY_PRESS, KEYCODE, 5);
    let _ = xi_xtest_grab_request(
        &mut state,
        &mut backend,
        &mut peer,
        3,
        XTEST,
        FAKE_INPUT,
        &press,
    );
    assert!(
        backend.floating_keyboard_states[&5]
            .down_keys
            .contains(&KEYCODE),
        "the XTEST key is held in its floating keyboard state"
    );
    assert_eq!(
        backend.core.down_keys, held.3,
        "the master keyboard did not acquire XTEST's key"
    );
    assert_eq!(backend.serialize_modifiers(), master_modifiers);

    let release = xi_xtest_fake_key(XI_KEY_RELEASE, KEYCODE, 5);
    let _ = xi_xtest_grab_request(
        &mut state,
        &mut backend,
        &mut peer,
        4,
        XTEST,
        FAKE_INPUT,
        &release,
    );
    assert!(backend.floating_keyboard_states[&5].down_keys.is_empty());
    let ungrab = xi_xtest_grab_request(
        &mut state,
        &mut backend,
        &mut peer,
        5,
        137,
        52,
        &xi_xtest_ungrab_body(5),
    );
    assert_eq!(xi_xtest_grab_reply_status(&ungrab), None);
    assert_eq!(state.xi_devices.device(5).unwrap().attached_master, Some(3));
    assert!(backend.floating_keyboard_states.is_empty());

    assert_eq!(xi_xtest_registry_snapshot(&state), registry);
    assert_eq!(xi_xtest_property_snapshot(&state), properties);
    assert_eq!(xi_xtest_held_snapshot(&state, &backend), held);
    assert!(state.xi2_detached_masters.is_empty());
    assert!(state.floating_pointer_positions.is_empty());
    assert!(state.xi2_pointer_grabs.is_empty());
    assert!(state.xi2_keyboard_grabs.is_empty());
    assert_eq!(state.clients[&CLIENT].xi2_masks, selections);
    assert_eq!(backend.serialize_modifiers(), master_modifiers);
}

// Kills: treating the saved master as an attached master on replacement,
// which re-detaches and resets the floating position. Also kills using
// the master root for each relative XTEST motion instead of its saved pos.
#[test]
fn xi_xtest_grab_pointer_replacement_preserves_floating_position() {
    use yserver_core::server::ServerState;

    const CLIENT: u32 = 5;
    const XTEST: u8 = 146;
    const FAKE_INPUT: u8 = 2;
    let mut backend = KmsBackend::for_tests();
    let mut state = ServerState::new();
    let mut peer = kbd_map_client_id(&mut state, CLIENT);
    let registry = xi_xtest_registry_snapshot(&state);
    let properties = xi_xtest_property_snapshot(&state);
    let held = xi_xtest_held_snapshot(&state, &backend);
    let selections = state.clients[&CLIENT].xi2_masks.clone();
    let master_position = state.pointer_root;

    let first = xi_xtest_grab_request(
        &mut state,
        &mut backend,
        &mut peer,
        1,
        137,
        51,
        &xi_xtest_grab_body(4, false),
    );
    assert_eq!(xi_xtest_grab_reply_status(&first), Some(0));
    let _ = xi_xtest_grab_request(
        &mut state,
        &mut backend,
        &mut peer,
        2,
        XTEST,
        FAKE_INPUT,
        &xi_xtest_fake_motion(1, 12, 4),
    );
    let first_position = state.floating_pointer_positions[&4];
    let _ = xi_xtest_grab_request(
        &mut state,
        &mut backend,
        &mut peer,
        3,
        XTEST,
        FAKE_INPUT,
        &xi_xtest_fake_motion(1, 5, 2),
    );
    let moved_position = state.floating_pointer_positions[&4];
    assert_eq!(
        moved_position,
        (first_position.0 + 5.0, first_position.1 + 2.0)
    );
    assert_eq!(state.pointer_root, master_position);

    let replacement = xi_xtest_grab_request(
        &mut state,
        &mut backend,
        &mut peer,
        4,
        137,
        51,
        &xi_xtest_grab_body(4, true),
    );
    assert_eq!(xi_xtest_grab_reply_status(&replacement), Some(0));
    assert_eq!(state.xi_devices.device(4).unwrap().attached_master, None);
    assert_eq!(state.xi2_detached_masters.get(&4), Some(&2));
    assert_eq!(
        state.floating_pointer_positions.get(&4),
        Some(&moved_position)
    );

    let ungrab = xi_xtest_grab_request(
        &mut state,
        &mut backend,
        &mut peer,
        5,
        137,
        52,
        &xi_xtest_ungrab_body(4),
    );
    assert_eq!(xi_xtest_grab_reply_status(&ungrab), None);
    assert_eq!(state.xi_devices.device(4).unwrap().attached_master, Some(2));
    assert_eq!(xi_xtest_registry_snapshot(&state), registry);
    assert_eq!(xi_xtest_property_snapshot(&state), properties);
    assert_eq!(xi_xtest_held_snapshot(&state, &backend), held);
    assert!(state.xi2_detached_masters.is_empty());
    assert!(state.floating_pointer_positions.is_empty());
    assert!(state.xi2_pointer_grabs.is_empty());
    assert!(state.xi2_keyboard_grabs.is_empty());
    assert!(backend.floating_keyboard_states.is_empty());
    assert_eq!(state.clients[&CLIENT].xi2_masks, selections);
}

// Kills: skipping `reattach_xi2_slave` in client-disconnect cleanup,
// leaking the XTEST pointer's detached master and private position.
#[test]
fn xi_xtest_grab_pointer_disconnect_reattaches_without_leaks() {
    use yserver_core::server::ServerState;
    use yserver_protocol::x11::ClientId;

    const CLIENT: u32 = 5;
    let mut backend = KmsBackend::for_tests();
    let mut state = ServerState::new();
    let mut peer = kbd_map_client_id(&mut state, CLIENT);
    let registry = xi_xtest_registry_snapshot(&state);
    let properties = xi_xtest_property_snapshot(&state);
    let held = xi_xtest_held_snapshot(&state, &backend);

    assert!(
        xi_xtest_grab_select_hierarchy(&mut state, &mut backend, &mut peer, 1, true).is_empty()
    );
    let grab = xi_xtest_grab_request(
        &mut state,
        &mut backend,
        &mut peer,
        2,
        137,
        51,
        &xi_xtest_grab_body(4, false),
    );
    assert_eq!(xi_xtest_grab_reply_status(&grab), Some(0));
    assert_eq!(state.xi_devices.device(4).unwrap().attached_master, None);
    assert_eq!(state.xi2_detached_masters.get(&4), Some(&2));
    assert!(state.floating_pointer_positions.contains_key(&4));

    yserver_core::core_loop::process_disconnect::process_disconnect(
        &mut state,
        &mut backend,
        ClientId(CLIENT),
    );

    assert!(
        !state.clients.contains_key(&CLIENT),
        "disconnect removes the selecting client"
    );
    assert_eq!(state.xi_devices.device(4).unwrap().attached_master, Some(2));
    assert_eq!(xi_xtest_registry_snapshot(&state), registry);
    assert_eq!(xi_xtest_property_snapshot(&state), properties);
    assert_eq!(xi_xtest_held_snapshot(&state, &backend), held);
    assert!(state.xi2_detached_masters.is_empty());
    assert!(state.floating_pointer_positions.is_empty());
    assert!(state.xi2_pointer_grabs.is_empty());
    assert!(state.xi2_keyboard_grabs.is_empty());
    assert!(backend.floating_keyboard_states.is_empty());
    assert!(
        state
            .clients
            .values()
            .all(|client| client.xi2_masks.is_empty())
    );
}

/// Xorg's XkbSendLegacyMapNotify only sends the core MappingNotify to a
/// client whose current master keyboard changed (`XIShouldNotify`). An XI
/// request on a slave reaches the master only when that slave is the
/// master's lastSlave, and on Xorg XTEST drives its own slave, so the
/// device an XI client remaps isn't. xts5 XIproto
/// SetDeviceModifierMapping-1 (passes on Xorg): DeviceMappingNotify, then
/// the reply, no core MappingNotify in between. On the master keyboard
/// the core MappingNotify goes out.
#[test]
fn xi_modifier_mapping_core_notify_only_for_the_master_keyboard() {
    const MAPPING_NOTIFY: u8 = 34;
    // Golden held-nonmodifier-unaffected, gb (`smmx:-66@1,+66@2`).
    let keys: [u8; 24] = [
        50, 62, 0, 0, 0, 0, 37, 105, 66, 64, 204, 205, 77, 0, 0, 203, 0, 0, 133, 134, 206, 92, 0, 0,
    ];
    for (dev, want_core) in [(5u8, false), (3u8, true)] {
        let mut backend = kbd_map_backend("gb", None);
        let mut state = yserver_core::server::ServerState::new();
        let mut peer = kbd_map_client(&mut state);
        let mut body = vec![dev, 3, 0, 0];
        body.extend_from_slice(&keys);
        kbd_map_request(&mut state, &mut backend, 137, 27, &body);
        let got = kbd_map_drain(&mut peer);
        let types: Vec<u8> = got.chunks(32).map(|e| e[0] & 0x7f).collect();
        assert_eq!(
            types.contains(&MAPPING_NOTIFY),
            want_core,
            "device {dev}: core MappingNotify expected={want_core}, got {types:02x?}"
        );
        let reply = got.chunks(32).last().unwrap();
        assert_eq!((reply[0], reply[1], reply[8]), (1, 27, 0), "Success reply");
    }
}

/// XI SetDeviceModifierMapping reaches the same keymap as the core
/// request (Xorg: both are `change_modmap`): the XKB listener gets the
/// same MapNotify, core GetModifierMapping reads the change, a held
/// modifier makes it MappingBusy, and the reply carries RepType and the
/// status (Xi/setmmap.c).
#[test]
fn xi_set_device_modifier_mapping_edits_the_keymap() {
    let mut backend = kbd_map_backend("gb", None);
    let mut state = yserver_core::server::ServerState::new();
    let mut peer = kbd_map_client(&mut state);
    yserver_core::core_loop::xkb_select::xkb_select_events(&mut state, 5, 0x0100, 0x0002);
    // The requester also selects DeviceMappingNotify on device 3 (class
    // = deviceid << 8 | type; type = XI first event 66 + offset 11).
    const DEVICE_MAPPING_NOTIFY: u8 = 66 + 11;
    state
        .clients
        .get_mut(&5)
        .unwrap()
        .xi1_event_classes
        .insert((3 << 8) | u32::from(DEVICE_MAPPING_NOTIFY));
    // Golden held-nonmodifier-unaffected, gb: 66 moved from Lock to
    // Control (`smmx:-66@1,+66@2`).
    let keys: [u8; 24] = [
        50, 62, 0, 0, 0, 0, 37, 105, 66, 64, 204, 205, 77, 0, 0, 203, 0, 0, 133, 134, 206, 92, 0, 0,
    ];
    let mut body = vec![3u8, 3, 0, 0];
    body.extend_from_slice(&keys);
    kbd_map_request(&mut state, &mut backend, 137, 27, &body);
    let got = kbd_map_drain(&mut peer);
    // Xorg (Xi/setmmap.c SendDeviceMappingNotify, before the XKB
    // notifications reach the client): the XI requester sees its
    // DeviceMappingNotify first — xts5 XIproto SetDeviceModifierMapping-1
    // "wanted DeviceMappingNotify, got MappingNotify", and XI
    // SetDeviceModifierMapping-2, both pass on Xorg.
    assert_eq!(
        got.first().map(|b| b & 0x7f),
        Some(DEVICE_MAPPING_NOTIFY),
        "first event must be DeviceMappingNotify: {:02x?}",
        got.chunks(32).map(|e| e[0]).collect::<Vec<_>>()
    );
    let map_notify = got
        .chunks(32)
        .find(|e| e[0] == 85 && e[1] == 1)
        .expect("XkbMapNotify");
    // changed=ModifierMap|KeyActions, acts 8+75, modmap 8+248, as
    // Xorg's MapNotify for that request.
    assert_eq!(&map_notify[10..12], &0x0014u16.to_le_bytes());
    assert_eq!(&map_notify[18..20], &[8, 75]);
    assert_eq!(&map_notify[24..26], &[8, 248]);
    let reply = got.chunks(32).last().unwrap();
    assert_eq!((reply[0], reply[1], reply[8]), (1, 27, 0), "Success reply");
    let (kpm, map) = get_modifier_mapping_reply(&mut state, &mut backend, &mut peer);
    assert_eq!(kpm, 3);
    assert_eq!(&map[6..9], &[37, 66, 105], "Control: 37, 66, 105");
    assert_eq!(&map[3..6], &[0, 0, 0], "Lock empty");

    // Shift_L held: MappingBusy, nothing changes.
    host_key(&mut backend, &mut state, 50, true);
    let _ = kbd_map_drain(&mut peer);
    let mut other = keys;
    other[0] = 0;
    let mut body = vec![3u8, 3, 0, 0];
    body.extend_from_slice(&other);
    kbd_map_request(&mut state, &mut backend, 137, 27, &body);
    let got = kbd_map_drain(&mut peer);
    assert_eq!(got.len(), 32, "only the reply: {got:02x?}");
    assert_eq!((got[0], got[1], got[8]), (1, 27, 1), "MappingBusy");
    let (_, after) = get_modifier_mapping_reply(&mut state, &mut backend, &mut peer);
    assert_eq!(after, map);
}
