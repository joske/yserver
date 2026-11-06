use super::{
    extensions::{ExtensionHook, RecordedExtensionCheck, RecordingExtensionPolicy},
    *,
};
use crate::access::{
    AccessMode, Decision, DenyError, RequestOpcodes, ResourceKind, ResourceTarget,
};

const CLIENT: ClientId = ClientId(1);
const WINDOW: ResourceId = ResourceId(0x0010_8001);
const OTHER_WINDOW: ResourceId = ResourceId(0x0010_8002);
const CURSOR: ResourceId = ResourceId(0x0090_8001);
const BARRIER_ONE: ResourceId = ResourceId(0x00a0_8001);
const BARRIER_TWO: ResourceId = ResourceId(0x00a0_8002);
const XI_MAJOR: u8 = 137;
const XKB_MAJOR: u8 = 136;
const XTEST_MAJOR: u8 = 146;

#[derive(Clone, Copy, Debug)]
struct ExpectedInputCheck {
    id: ResourceId,
    kind: ResourceKind,
    mode: AccessMode,
}

struct InputRequestCase {
    label: &'static str,
    major: u8,
    minor: u8,
    body: Vec<u8>,
    checks: Vec<ExpectedInputCheck>,
}

fn put_u16(body: &mut Vec<u8>, value: u16) {
    body.extend_from_slice(&value.to_le_bytes());
}

fn put_u32(body: &mut Vec<u8>, value: u32) {
    body.extend_from_slice(&value.to_le_bytes());
}

fn input_words(values: &[u32]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

fn input_fixture() -> (ServerState, UnixStream, RecordingBackend) {
    let mut state = ServerState::new();
    let peer = install_client(&mut state, CLIENT.0);
    for (id, host) in [(WINDOW, 0x0040_8001), (OTHER_WINDOW, 0x0040_8002)] {
        state.resources.create_window(
            CLIENT,
            CreateWindowRequest {
                depth: 24,
                window: id,
                parent: ROOT_WINDOW,
                width: 80,
                height: 60,
                class: 1,
                visual: crate::resources::ROOT_VISUAL,
                ..Default::default()
            },
        );
        let window = state.resources.window_mut(id).expect("test window");
        window.host_xid = Some(crate::backend::WindowHandle::from_raw_for_test(host));
        window.map_state = crate::resources::MapState::Viewable;
    }
    state.resources.create_cursor(CLIENT, CURSOR);
    for (id, event_id) in [(BARRIER_ONE, 7), (BARRIER_TWO, 9)] {
        state.pointer_barriers.insert(
            id.0,
            crate::server::PointerBarrier {
                owner: CLIENT,
                window: WINDOW,
                x1: 1,
                y1: 2,
                x2: 3,
                y2: 4,
                directions: 0,
                devices: vec![2],
                hit: true,
                seen: true,
                event_id,
                release_event_id: 0,
                last_timestamp: 1,
            },
        );
    }
    (state, peer, RecordingBackend::new())
}

fn input_request(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    peer: &mut UnixStream,
    major: u8,
    minor: u8,
    body: &[u8],
) -> Vec<u8> {
    super::access_resources::resource_request(state, backend, peer, 1, major, minor, body)
}

fn enable_xkb(state: &mut ServerState, backend: &mut RecordingBackend, peer: &mut UnixStream) {
    let reply = input_request(state, backend, peer, XKB_MAJOR, 0, &[1, 0, 0, 0]);
    assert_eq!(reply.len(), 32, "XkbUseExtension setup reply");
    assert!(crate::core_loop::xkb_select::xkb_initialized(state, CLIENT));
}

fn check(id: ResourceId, kind: ResourceKind, mode: AccessMode) -> ExpectedInputCheck {
    ExpectedInputCheck { id, kind, mode }
}

fn input_request_cases() -> Vec<InputRequestCase> {
    use ResourceKind::{Cursor, PointerBarrier, Window};

    let class = ((crate::xinput::DEVICEID_XTEST_POINTER as u32) << 8) | 82;
    let mut xi1_select = input_words(&[WINDOW.0]);
    put_u16(&mut xi1_select, 1);
    put_u16(&mut xi1_select, 0);
    put_u32(&mut xi1_select, class);

    let mut xi1_dont_propagate = input_words(&[WINDOW.0]);
    put_u16(&mut xi1_dont_propagate, 1);
    xi1_dont_propagate.extend_from_slice(&[0, 0]); // AddToList, pad
    put_u32(&mut xi1_dont_propagate, class);

    let mut xi1_grab_device = input_words(&[WINDOW.0, 0]);
    put_u16(&mut xi1_grab_device, 0); // class count
    xi1_grab_device.extend_from_slice(&[1, 1, 1, 2, 0, 0]);

    let mut xi1_grab_key = input_words(&[WINDOW.0]);
    put_u16(&mut xi1_grab_key, 0); // class count
    put_u16(&mut xi1_grab_key, 0); // modifiers
    xi1_grab_key.extend_from_slice(&[255, 3, 8, 1, 1, 1, 0, 0]);

    let mut xi1_ungrab_key = input_words(&[WINDOW.0]);
    put_u16(&mut xi1_ungrab_key, 0); // modifiers
    xi1_ungrab_key.extend_from_slice(&[255, 8, 3, 0, 0, 0]);

    let mut xi1_grab_button = input_words(&[WINDOW.0]);
    xi1_grab_button.extend_from_slice(&[2, 255]); // devices
    put_u16(&mut xi1_grab_button, 0); // class count
    put_u16(&mut xi1_grab_button, 0); // modifiers
    xi1_grab_button.extend_from_slice(&[1, 1, 1, 1, 0, 0]);

    let mut xi1_ungrab_button = input_words(&[WINDOW.0]);
    put_u16(&mut xi1_ungrab_button, 0); // modifiers
    xi1_ungrab_button.extend_from_slice(&[255, 1, 2, 0, 0, 0]);

    let mut xi1_set_focus = input_words(&[WINDOW.0, 0]);
    xi1_set_focus.extend_from_slice(&[2, 3, 0, 0]); // RevertToParent, keyboard

    let mut xi2_set_focus = input_words(&[WINDOW.0, 0]);
    put_u16(&mut xi2_set_focus, 3); // master keyboard
    put_u16(&mut xi2_set_focus, 0);

    let mut xi2_select = input_words(&[WINDOW.0]);
    put_u16(&mut xi2_select, 1); // one event mask
    put_u16(&mut xi2_select, 0);
    put_u16(&mut xi2_select, 2); // master pointer
    put_u16(&mut xi2_select, 1); // one mask word
    put_u32(&mut xi2_select, 1);

    let mut xi2_grab = input_words(&[WINDOW.0, 0, CURSOR.0]);
    put_u16(&mut xi2_grab, 2); // master pointer
    xi2_grab.extend_from_slice(&[1, 1, 1, 0]);
    put_u16(&mut xi2_grab, 0); // mask length
    put_u16(&mut xi2_grab, 0);

    let mut xi2_passive_grab = input_words(&[0, WINDOW.0, CURSOR.0, 1]);
    put_u16(&mut xi2_passive_grab, 2); // master pointer
    put_u16(&mut xi2_passive_grab, 0); // modifiers
    put_u16(&mut xi2_passive_grab, 0); // mask length
    xi2_passive_grab.extend_from_slice(&[0, 1, 1, 1, 0, 0]);

    let mut xi2_passive_ungrab = input_words(&[WINDOW.0, 1]);
    put_u16(&mut xi2_passive_ungrab, 2); // master pointer
    put_u16(&mut xi2_passive_ungrab, 0); // modifiers
    xi2_passive_ungrab.extend_from_slice(&[0, 0, 0, 0]);

    let mut xi_warp = input_words(&[OTHER_WINDOW.0, WINDOW.0, 0, 0]);
    put_u16(&mut xi_warp, 10);
    put_u16(&mut xi_warp, 10);
    put_u32(&mut xi_warp, 0);
    put_u32(&mut xi_warp, 0);
    put_u16(&mut xi_warp, 2);
    put_u16(&mut xi_warp, 0);

    let mut xi_change_cursor = input_words(&[WINDOW.0, CURSOR.0]);
    put_u16(&mut xi_change_cursor, 2);
    put_u16(&mut xi_change_cursor, 0);

    let mut xi_set_client_pointer = input_words(&[WINDOW.0]);
    put_u16(&mut xi_set_client_pointer, 2);
    put_u16(&mut xi_set_client_pointer, 0);

    let mut xi_barrier = vec![0; 4];
    xi_barrier[..4].copy_from_slice(&1u32.to_le_bytes());
    put_u16(&mut xi_barrier, 2);
    put_u16(&mut xi_barrier, 0);
    put_u32(&mut xi_barrier, BARRIER_ONE.0);
    put_u32(&mut xi_barrier, 7);

    let mut xkb_bell = vec![0; 24];
    xkb_bell[..2].copy_from_slice(&0x0100u16.to_le_bytes()); // XkbUseCoreKbd
    xkb_bell[16..20].copy_from_slice(&0x1234_5678u32.to_le_bytes()); // name atom
    xkb_bell[20..24].copy_from_slice(&WINDOW.0.to_le_bytes());

    let mut xtest_fake_input = vec![0; 28];
    xtest_fake_input[0] = 6; // MotionNotify
    xtest_fake_input[8..12].copy_from_slice(&WINDOW.0.to_le_bytes());

    vec![
        InputRequestCase {
            label: "XI1 SelectExtensionEvent",
            major: XI_MAJOR,
            minor: 6,
            body: xi1_select,
            checks: vec![check(WINDOW, Window, AccessMode::RECEIVE)],
        },
        InputRequestCase {
            label: "XI1 GetSelectedExtensionEvents",
            major: XI_MAJOR,
            minor: 7,
            body: input_words(&[WINDOW.0]),
            checks: vec![check(WINDOW, Window, AccessMode::GET_ATTR)],
        },
        InputRequestCase {
            label: "XI1 ChangeDeviceDontPropagateList",
            major: XI_MAJOR,
            minor: 8,
            body: xi1_dont_propagate,
            checks: vec![check(WINDOW, Window, AccessMode::SET_ATTR)],
        },
        InputRequestCase {
            label: "XI1 GetDeviceDontPropagateList",
            major: XI_MAJOR,
            minor: 9,
            body: input_words(&[WINDOW.0]),
            checks: vec![check(WINDOW, Window, AccessMode::GET_ATTR)],
        },
        InputRequestCase {
            label: "XI1 GrabDevice",
            major: XI_MAJOR,
            minor: 13,
            body: xi1_grab_device,
            checks: vec![check(WINDOW, Window, AccessMode::SET_ATTR)],
        },
        InputRequestCase {
            label: "XI1 GrabDeviceKey",
            major: XI_MAJOR,
            minor: 15,
            body: xi1_grab_key,
            checks: vec![check(WINDOW, Window, AccessMode::SET_ATTR)],
        },
        InputRequestCase {
            label: "XI1 UngrabDeviceKey",
            major: XI_MAJOR,
            minor: 16,
            body: xi1_ungrab_key,
            checks: vec![check(WINDOW, Window, AccessMode::SET_ATTR)],
        },
        InputRequestCase {
            label: "XI1 GrabDeviceButton",
            major: XI_MAJOR,
            minor: 17,
            body: xi1_grab_button,
            checks: vec![check(WINDOW, Window, AccessMode::SET_ATTR)],
        },
        InputRequestCase {
            label: "XI1 UngrabDeviceButton",
            major: XI_MAJOR,
            minor: 18,
            body: xi1_ungrab_button,
            checks: vec![check(WINDOW, Window, AccessMode::SET_ATTR)],
        },
        InputRequestCase {
            label: "XI1 SetDeviceFocus",
            major: XI_MAJOR,
            minor: 21,
            body: xi1_set_focus,
            checks: vec![check(WINDOW, Window, AccessMode::SET_ATTR)],
        },
        InputRequestCase {
            label: "XI2 SelectEvents",
            major: XI_MAJOR,
            minor: 46,
            body: xi2_select,
            checks: vec![check(WINDOW, Window, AccessMode::RECEIVE)],
        },
        InputRequestCase {
            label: "XI2 GetSelectedEvents",
            major: XI_MAJOR,
            minor: 60,
            body: input_words(&[WINDOW.0]),
            checks: vec![check(WINDOW, Window, AccessMode::GET_ATTR)],
        },
        InputRequestCase {
            label: "XI2 QueryPointer",
            major: XI_MAJOR,
            minor: 40,
            body: {
                let mut body = input_words(&[WINDOW.0]);
                put_u16(&mut body, 2);
                put_u16(&mut body, 0);
                body
            },
            checks: vec![check(WINDOW, Window, AccessMode::GET_ATTR)],
        },
        InputRequestCase {
            label: "XI2 GrabDevice window and cursor",
            major: XI_MAJOR,
            minor: 51,
            body: xi2_grab,
            checks: vec![
                check(WINDOW, Window, AccessMode::SET_ATTR),
                check(CURSOR, Cursor, AccessMode::USE),
            ],
        },
        InputRequestCase {
            label: "XI2 PassiveGrabDevice window and cursor",
            major: XI_MAJOR,
            minor: 54,
            body: xi2_passive_grab,
            checks: vec![
                check(WINDOW, Window, AccessMode::SET_ATTR),
                check(CURSOR, Cursor, AccessMode::USE),
            ],
        },
        InputRequestCase {
            label: "XI2 PassiveUngrabDevice",
            major: XI_MAJOR,
            minor: 55,
            body: xi2_passive_ungrab,
            checks: vec![check(WINDOW, Window, AccessMode::SET_ATTR)],
        },
        InputRequestCase {
            label: "XI2 WarpPointer destination then source",
            major: XI_MAJOR,
            minor: 41,
            body: xi_warp,
            checks: vec![
                check(WINDOW, Window, AccessMode::GET_ATTR),
                check(OTHER_WINDOW, Window, AccessMode::GET_ATTR),
            ],
        },
        InputRequestCase {
            label: "XI2 ChangeCursor window and cursor",
            major: XI_MAJOR,
            minor: 42,
            body: xi_change_cursor,
            checks: vec![
                check(WINDOW, Window, AccessMode::SET_ATTR),
                check(CURSOR, Cursor, AccessMode::USE),
            ],
        },
        InputRequestCase {
            label: "XI2 SetFocus",
            major: XI_MAJOR,
            minor: 49,
            body: xi2_set_focus,
            checks: vec![check(WINDOW, Window, AccessMode::SET_ATTR)],
        },
        InputRequestCase {
            label: "XI2 SetClientPointer",
            major: XI_MAJOR,
            minor: 44,
            body: xi_set_client_pointer,
            checks: vec![check(WINDOW, Window, AccessMode::GET_ATTR)],
        },
        InputRequestCase {
            label: "XI2 GetClientPointer",
            major: XI_MAJOR,
            minor: 45,
            body: input_words(&[WINDOW.0]),
            checks: vec![check(WINDOW, Window, AccessMode::GET_ATTR)],
        },
        InputRequestCase {
            label: "XI2 BarrierReleasePointer",
            major: XI_MAJOR,
            minor: 61,
            body: xi_barrier,
            checks: vec![check(BARRIER_ONE, PointerBarrier, AccessMode::READ)],
        },
        InputRequestCase {
            label: "XKB Bell",
            major: XKB_MAJOR,
            minor: 3,
            body: xkb_bell,
            checks: vec![check(WINDOW, Window, AccessMode::GET_ATTR)],
        },
        InputRequestCase {
            label: "XTEST CompareCursor",
            major: XTEST_MAJOR,
            minor: 1,
            body: input_words(&[WINDOW.0, CURSOR.0]),
            checks: vec![
                check(WINDOW, Window, AccessMode::GET_ATTR),
                check(CURSOR, Cursor, AccessMode::READ),
            ],
        },
        InputRequestCase {
            label: "XTEST FakeInput root",
            major: XTEST_MAJOR,
            minor: 2,
            body: xtest_fake_input,
            checks: vec![check(WINDOW, Window, AccessMode::GET_ATTR)],
        },
    ]
}

fn assert_input_check(
    call: &RecordedExtensionCheck,
    expected: ExpectedInputCheck,
    major: u8,
    minor: u8,
    label: &str,
) {
    let target = ResourceTarget {
        id: expected.id,
        kind: expected.kind,
        creation: None,
        owner: None,
    };
    assert_eq!(call.hook, ExtensionHook::Resource, "{label}");
    assert_eq!(call.client, CLIENT, "{label}");
    assert_eq!(call.target, Some(target), "{label}");
    assert_eq!(call.mode, Some(expected.mode), "{label}");
    assert_eq!(call.owner, Some(CLIENT), "{label}");
    assert_eq!(call.target_owner, Some(CLIENT), "{label}");
    assert_eq!(
        call.request,
        Some(RequestOpcodes { major, minor }),
        "{label}"
    );
}

#[test]
fn input_operands_ask_every_site_in_table_order() {
    // Mutation: omit any listed check or read XKB Bell's name atom as its window; the exact target table must fail.
    for case in input_request_cases() {
        let (mut state, mut peer, mut backend) = input_fixture();
        if case.major == XKB_MAJOR {
            enable_xkb(&mut state, &mut backend, &mut peer);
        }
        let calls = Arc::new(Mutex::new(Vec::new()));
        state.install_access_policy(Arc::new(RecordingExtensionPolicy::new(
            Decision::Allow,
            Arc::clone(&calls),
        )));
        let _reply = input_request(
            &mut state,
            &mut backend,
            &mut peer,
            case.major,
            case.minor,
            &case.body,
        );
        let calls = calls.lock().unwrap();
        let resource_calls: Vec<_> = calls
            .iter()
            .filter(|call| call.hook == ExtensionHook::Resource)
            .collect();
        assert_eq!(resource_calls.len(), case.checks.len(), "{}", case.label);
        for (call, expected) in resource_calls.into_iter().zip(case.checks) {
            assert_input_check(call, expected, case.major, case.minor, case.label);
        }
    }
}

fn assert_policy_error(bytes: &[u8], major: u8, minor: u8, error: DenyError, label: &str) {
    assert_eq!(
        bytes.len(),
        32,
        "{label}: policy denial must be one X error"
    );
    assert_eq!(bytes[0], 0, "{label}: X error response");
    assert_eq!(bytes[1], error.code, "{label}: policy error code");
    assert_eq!(
        u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
        error.value,
        "{label}: policy error value"
    );
    assert_eq!(
        u16::from_le_bytes(bytes[8..10].try_into().unwrap()),
        u16::from(minor),
        "{label}: minor opcode"
    );
    assert_eq!(bytes[10], major, "{label}: major opcode");
}

fn input_state_snapshot(state: &ServerState) -> String {
    let client = state.clients.get(&CLIENT.0).expect("test client");
    format!(
        "{:?}{:?}{:?}",
        (
            client.xi1_event_classes.clone(),
            client.xi1_window_event_classes.clone(),
            client.xi2_masks.clone(),
            state.xi1_window_dont_propagate.clone(),
            state.xi1_device_focus.clone(),
            state.xi1_active_grabs.clone(),
            state.xi1_passive_grabs.clone(),
            state.xi2_pointer_grabs.clone(),
            state.xi2_keyboard_grabs.clone(),
            state.active_pointer_grab,
            state.active_keyboard_grab,
            state.button_grabs.clone(),
        ),
        (
            state.key_grabs.clone(),
            state.pointer_barriers.clone(),
            state.core_focus,
            state.pointer_root,
        ),
        (
            state.buttons_down,
            state.keys_down,
            state.pointer_motion_history.len(),
            state
                .resources
                .window(WINDOW)
                .and_then(|window| window.cursor),
            state
                .resources
                .window(OTHER_WINDOW)
                .and_then(|window| window.cursor),
        )
    )
}

#[test]
fn input_operands_deny_each_site_without_state_or_backend_effects() {
    // Mutation: ignore a resource-policy denial, or call XKB Bell's backend proxy before its check; these denial assertions must fail.
    let error = DenyError {
        code: x11::error::BAD_ACCESS,
        value: 0xA55A_1234,
    };
    for case in input_request_cases() {
        for (denied_index, denied) in case.checks.iter().copied().enumerate() {
            let (mut state, mut peer, mut backend) = input_fixture();
            if case.major == XKB_MAJOR {
                enable_xkb(&mut state, &mut backend, &mut peer);
            }
            let calls = Arc::new(Mutex::new(Vec::new()));
            state.install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
                Arc::clone(&calls),
                denied.id,
                Some(denied.mode),
                error,
            )));
            let before = input_state_snapshot(&state);
            let backend_before = backend.calls();
            let bytes = input_request(
                &mut state,
                &mut backend,
                &mut peer,
                case.major,
                case.minor,
                &case.body,
            );
            assert_policy_error(&bytes, case.major, case.minor, error, case.label);
            assert_eq!(input_state_snapshot(&state), before, "{}", case.label);
            assert_eq!(
                backend.calls(),
                backend_before,
                "{} reached backend",
                case.label
            );
            match (case.major, case.minor) {
                (XKB_MAJOR, 3) => assert!(
                    !backend.calls().iter().any(|call| matches!(
                        call,
                        crate::backend::recording::RecordedCall::XkbProxy { minor: 3, .. }
                    )),
                    "XKB Bell reached xkb_proxy before denial"
                ),
                (XI_MAJOR, 40) => assert!(
                    !backend
                        .calls()
                        .contains(&crate::backend::recording::RecordedCall::QueryPointer),
                    "XI2 QueryPointer reached the backend before denial"
                ),
                _ => {}
            }
            let calls = calls.lock().unwrap();
            let resource_calls: Vec<_> = calls
                .iter()
                .filter(|call| call.hook == ExtensionHook::Resource)
                .collect();
            assert_eq!(resource_calls.len(), denied_index + 1, "{}", case.label);
            for (call, expected) in resource_calls.into_iter().zip(case.checks.iter().copied()) {
                assert_input_check(call, expected, case.major, case.minor, case.label);
            }
        }
    }
}

fn barrier_release_body(entries: &[(ResourceId, u32)]) -> Vec<u8> {
    let mut body = Vec::with_capacity(4 + entries.len() * 12);
    put_u32(&mut body, u32::try_from(entries.len()).unwrap());
    for (barrier, event_id) in entries {
        put_u16(&mut body, 2);
        put_u16(&mut body, 0);
        put_u32(&mut body, barrier.0);
        put_u32(&mut body, *event_id);
    }
    body
}

#[test]
fn barrier_release_denied_second_entry_keeps_first_release() {
    // Mutation: check every barrier before releasing any; the first release must still be recorded.
    let (mut state, mut peer, mut backend) = input_fixture();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let error = DenyError {
        code: x11::error::BAD_ACCESS,
        value: BARRIER_TWO.0,
    };
    state.install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
        Arc::clone(&calls),
        BARRIER_TWO,
        Some(AccessMode::READ),
        error,
    )));
    let bytes = input_request(
        &mut state,
        &mut backend,
        &mut peer,
        XI_MAJOR,
        61,
        &barrier_release_body(&[(BARRIER_ONE, 7), (BARRIER_TWO, 9)]),
    );
    assert_policy_error(&bytes, XI_MAJOR, 61, error, "second barrier denial");
    assert_eq!(state.pointer_barriers[&BARRIER_ONE.0].release_event_id, 7);
    assert_eq!(state.pointer_barriers[&BARRIER_TWO.0].release_event_id, 0);
    assert_eq!(
        backend.calls(),
        Vec::<crate::backend::recording::RecordedCall>::new()
    );
    let calls = calls.lock().unwrap();
    let resource_calls: Vec<_> = calls
        .iter()
        .filter(|call| call.hook == ExtensionHook::Resource)
        .collect();
    assert_eq!(resource_calls.len(), 2);
    assert_input_check(
        resource_calls[0],
        check(BARRIER_ONE, ResourceKind::PointerBarrier, AccessMode::READ),
        XI_MAJOR,
        61,
        "first barrier",
    );
    assert_input_check(
        resource_calls[1],
        check(BARRIER_TWO, ResourceKind::PointerBarrier, AccessMode::READ),
        XI_MAJOR,
        61,
        "second barrier",
    );
}

#[test]
fn xi_set_client_pointer_unresolved_id_is_not_asked() {
    // Mutation: ask the policy for an unresolved resource id; this must become an unexpected resource check.
    let (mut state, mut peer, mut backend) = input_fixture();
    let calls = Arc::new(Mutex::new(Vec::new()));
    state.install_access_policy(Arc::new(RecordingExtensionPolicy::new(
        Decision::Allow,
        Arc::clone(&calls),
    )));
    let unknown = ResourceId(0xdead_8001);
    let body = input_words(&[unknown.0, 2]);
    let bytes = input_request(&mut state, &mut backend, &mut peer, XI_MAJOR, 44, &body);
    assert!(
        bytes.is_empty(),
        "XISetClientPointer remains a no-op: {bytes:02x?}"
    );
    assert!(
        calls
            .lock()
            .unwrap()
            .iter()
            .all(|call| call.hook != ExtensionHook::Resource),
        "unresolved id was asked for resource access"
    );
}
