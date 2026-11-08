use super::{
    extensions::{ExtensionHook, RecordedExtensionCheck, RecordingExtensionPolicy},
    *,
};
use crate::access::{
    AccessMode, Decision, DenyError, RequestOpcodes, ResourceKind, ResourceTarget,
};

const CLIENT: ClientId = ClientId(1);
const OTHER_CLIENT: ClientId = ClientId(2);
const BAD_VALUE: u8 = yserver_protocol::x11::error::BAD_VALUE;
const BAD_ATOM: u8 = yserver_protocol::x11::error::BAD_ATOM;
const DENY_VALUE: u32 = 0x55;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ExpectedCheck {
    id: ResourceId,
    kind: ResourceKind,
    mode: AccessMode,
}

impl ExpectedCheck {
    fn target(self) -> ResourceTarget {
        ResourceTarget {
            owner: None,
            id: self.id,
            kind: self.kind,
            creation: None,
        }
    }
}

fn expected_check(id: ResourceId, kind: ResourceKind, mode: AccessMode) -> ExpectedCheck {
    ExpectedCheck { id, kind, mode }
}

fn access_row(
    opcode: u8,
    data: u8,
    body: Vec<u8>,
    checks: Vec<ExpectedCheck>,
) -> (u8, u8, Vec<u8>, Vec<ExpectedCheck>) {
    (opcode, data, body, checks)
}

struct Fixture {
    state: ServerState,
    backend: RecordingBackend,
    peer: UnixStream,
    _other_peer: UnixStream,
    window: ResourceId,
    other_window: ResourceId,
    confine: ResourceId,
    foreign_window: ResourceId,
    cursor: ResourceId,
}

impl Fixture {
    fn new() -> Self {
        let mut state = ServerState::new();
        let mut peer = install_client(&mut state, CLIENT.0);
        let other_peer = install_client(&mut state, OTHER_CLIENT.0);
        let window = ResourceId(0x0055_0001);
        let other_window = ResourceId(0x0055_0002);
        let confine = ResourceId(0x0055_0003);
        let foreign_parent = ResourceId(0x0055_0004);
        let foreign_window = ResourceId(0x0055_0005);
        let pixmap = ResourceId(0x0055_0006);
        let cursor = ResourceId(0x0055_0007);
        for (owner, id, parent) in [
            (CLIENT, window, ROOT_WINDOW),
            (CLIENT, other_window, ROOT_WINDOW),
            (CLIENT, confine, ROOT_WINDOW),
            (OTHER_CLIENT, foreign_parent, ROOT_WINDOW),
            (OTHER_CLIENT, foreign_window, foreign_parent),
        ] {
            state.resources.create_window(
                owner,
                CreateWindowRequest {
                    depth: 24,
                    window: id,
                    parent,
                    width: 32,
                    height: 32,
                    class: 1,
                    visual: crate::resources::ROOT_VISUAL,
                    ..Default::default()
                },
            );
        }
        state.resources.window_mut(window).unwrap().map_state =
            crate::resources::MapState::Viewable;
        state.resources.window_mut(confine).unwrap().map_state =
            crate::resources::MapState::Viewable;
        state.button_grabs.push(crate::server::PassiveButtonGrab {
            device_id: 0,
            owner: CLIENT,
            grab_window: window,
            button: 1,
            modifiers: 0,
            owner_events: false,
            event_mask: 4,
            pointer_mode: 0,
            keyboard_mode: 0,
            confine_to: ResourceId(0),
            via_xi2: false,
        });
        state.key_grabs.push(crate::server::KeyGrab {
            device_id: 0,
            owner: CLIENT,
            grab_window: window,
            keycode: 24,
            modifiers: 0,
            owner_events: false,
            pointer_mode: 0,
            keyboard_mode: 0,
            via_xi2: false,
            xi2_mask: 0,
        });
        state.set_pointer_grab(crate::server::ActivePointerGrab {
            owner: CLIENT,
            grab_window: window,
            event_mask: 0,
            cursor: ResourceId(0),
            time: 0,
            owner_events: false,
            via_xi2: false,
            implicit: false,
            passive: false,
            xi2_mask: 0,
        });

        let mut backend = RecordingBackend::new();
        let pixmap_body = words(&[pixmap.0, ROOT_WINDOW.0, 16 | (16 << 16)]);
        let bytes = issue_request(
            &mut state,
            &mut backend,
            &mut peer,
            CLIENT,
            1,
            53,
            24,
            &pixmap_body,
        );
        assert_no_error(&bytes, "access grab fixture CreatePixmap");
        let mut cursor_body = words(&[cursor.0, pixmap.0, 0]);
        cursor_body.extend_from_slice(&[0; 16]);
        let bytes = issue_request(
            &mut state,
            &mut backend,
            &mut peer,
            CLIENT,
            2,
            93,
            0,
            &cursor_body,
        );
        assert_no_error(&bytes, "access grab fixture CreateCursor");

        Self {
            state,
            backend,
            peer,
            _other_peer: other_peer,
            window,
            other_window,
            confine,
            foreign_window,
            cursor,
        }
    }

    fn install_policy(
        &mut self,
        calls: Arc<Mutex<Vec<RecordedExtensionCheck>>>,
        deny: Option<ExpectedCheck>,
    ) {
        self.state
            .install_access_policy(Arc::new(RecordingExtensionPolicy::custom(
                calls,
                "",
                move |_, call| {
                    if deny.is_some_and(|expected| {
                        call.target == Some(expected.target()) && call.mode == Some(expected.mode)
                    }) {
                        Decision::Deny(DenyError {
                            code: BAD_VALUE,
                            value: DENY_VALUE,
                        })
                    } else {
                        Decision::Allow
                    }
                },
            )));
    }

    fn run(&mut self, sequence: u16, opcode: u8, data: u8, body: &[u8]) -> Vec<u8> {
        issue_request(
            &mut self.state,
            &mut self.backend,
            &mut self.peer,
            CLIENT,
            sequence,
            opcode,
            data,
            body,
        )
    }
}

fn issue_request(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    peer: &mut UnixStream,
    client: ClientId,
    sequence: u16,
    opcode: u8,
    data: u8,
    body: &[u8],
) -> Vec<u8> {
    process_request(
        state,
        backend,
        client,
        SequenceNumber(sequence),
        RequestHeader {
            opcode,
            data,
            length_units: u32::try_from((4 + body.len()).div_ceil(4)).unwrap(),
        },
        body,
        None,
    )
    .expect("request through process_request");
    read_all_available(peer)
}

fn words(words: &[u32]) -> Vec<u8> {
    words.iter().flat_map(|word| word.to_le_bytes()).collect()
}

fn append_u16(body: &mut Vec<u8>, value: u16) {
    body.extend_from_slice(&value.to_le_bytes());
}

fn assert_x11_error(bytes: &[u8], code: u8, value: u32, opcode: u8, sequence: u16) {
    let mut expected = vec![0, code];
    expected.extend_from_slice(&sequence.to_le_bytes());
    expected.extend_from_slice(&value.to_le_bytes());
    expected.extend_from_slice(&0u16.to_le_bytes());
    expected.push(opcode);
    expected.extend_from_slice(&[0; 21]);
    assert_eq!(bytes, expected, "request {opcode} denial packet");
}

fn access_cases(fixture: &Fixture) -> Vec<(u8, u8, Vec<u8>, Vec<ExpectedCheck>)> {
    use AccessMode as M;
    use ResourceKind::{Cursor, Window};

    let mut grab_pointer = words(&[fixture.window.0]);
    append_u16(&mut grab_pointer, 0);
    grab_pointer.extend_from_slice(&[1, 1]);
    grab_pointer.extend_from_slice(&fixture.confine.0.to_le_bytes());
    grab_pointer.extend_from_slice(&fixture.cursor.0.to_le_bytes());
    grab_pointer.extend_from_slice(&0u32.to_le_bytes());

    let mut grab_button = words(&[fixture.window.0]);
    append_u16(&mut grab_button, 0);
    grab_button.extend_from_slice(&[1, 1]);
    grab_button.extend_from_slice(&fixture.confine.0.to_le_bytes());
    grab_button.extend_from_slice(&fixture.cursor.0.to_le_bytes());
    grab_button.extend_from_slice(&[1, 0]);
    append_u16(&mut grab_button, 0);

    let mut ungrab_button = words(&[fixture.window.0]);
    append_u16(&mut ungrab_button, 0);
    let mut change_pointer_grab = words(&[fixture.cursor.0, 0]);
    append_u16(&mut change_pointer_grab, 4);
    append_u16(&mut change_pointer_grab, 0);
    let mut grab_keyboard = words(&[fixture.window.0, 0]);
    grab_keyboard.extend_from_slice(&[1, 1, 0, 0]);
    let mut grab_key = words(&[fixture.window.0]);
    append_u16(&mut grab_key, 0);
    grab_key.extend_from_slice(&[24, 1, 1, 0, 0, 0]);
    let mut ungrab_key = words(&[fixture.window.0]);
    append_u16(&mut ungrab_key, 0);
    ungrab_key.extend_from_slice(&[0, 0]);
    let set_focus = words(&[fixture.window.0, 0]);
    let set_selection_owner = words(&[fixture.other_window.0, 1, 0]);
    let convert_selection = words(&[fixture.window.0, 1, 31, 0, 0]);
    let foreign_window_body = words(&[fixture.foreign_window.0]);
    let root_window_body = words(&[ROOT_WINDOW.0]);
    let foreign_window_check = vec![expected_check(fixture.foreign_window, Window, M::MANAGE)];
    let selection_owner_check = vec![expected_check(fixture.other_window, Window, M::SET_ATTR)];
    let convert_selection_check = vec![expected_check(fixture.window, Window, M::SET_ATTR)];
    let grab_pointer_checks = vec![
        expected_check(fixture.confine, Window, M::SET_ATTR),
        expected_check(fixture.window, Window, M::SET_ATTR),
        expected_check(fixture.cursor, Cursor, M::USE),
    ];
    let grab_button_checks = vec![
        expected_check(fixture.window, Window, M::SET_ATTR),
        expected_check(fixture.confine, Window, M::SET_ATTR),
        expected_check(fixture.cursor, Cursor, M::USE),
    ];
    let ungrab_button_check = vec![expected_check(fixture.window, Window, M::READ)];
    let change_pointer_grab_check = vec![expected_check(fixture.cursor, Cursor, M::USE)];
    let set_window_check = vec![expected_check(fixture.window, Window, M::SET_ATTR)];
    let get_window_check = vec![expected_check(fixture.window, Window, M::GET_ATTR)];
    let root_window_check = vec![expected_check(ROOT_WINDOW, Window, M::GET_ATTR)];

    let cases = vec![
        access_row(6, 0, foreign_window_body, foreign_window_check),
        access_row(22, 0, set_selection_owner, selection_owner_check),
        access_row(24, 0, convert_selection, convert_selection_check),
        access_row(26, 0, grab_pointer, grab_pointer_checks),
        access_row(28, 0, grab_button, grab_button_checks),
        access_row(29, 1, ungrab_button, ungrab_button_check),
        access_row(30, 0, change_pointer_grab, change_pointer_grab_check),
        access_row(31, 0, grab_keyboard, set_window_check.clone()),
        access_row(33, 0, grab_key, set_window_check.clone()),
        access_row(34, 24, ungrab_key, get_window_check),
        access_row(42, 0, set_focus, set_window_check),
        access_row(113, 0, root_window_body, root_window_check),
    ];
    cases
}

fn snapshot(state: &ServerState) -> String {
    format!(
        "{:?}",
        (
            &state.clients[&CLIENT.0].save_set,
            &state.selections,
            &state.button_grabs,
            &state.key_grabs,
            state.active_pointer_grab,
            state.active_keyboard_grab,
            state.pointer_confine_to,
            state.core_focus,
        )
    )
}

fn expected_owner(fixture: &Fixture, id: ResourceId) -> Option<ClientId> {
    if id == ROOT_WINDOW {
        Some(crate::resources::SERVER_OWNER)
    } else if id == fixture.foreign_window {
        Some(OTHER_CLIENT)
    } else {
        Some(CLIENT)
    }
}

#[test]
fn task8_core_resource_requests_deny_before_effects() {
    // Mutation: remove or move a listed access check past its first effect; that row's denial or
    // snapshot assertion must fail.
    for (opcode, data, body, checks) in access_cases(&Fixture::new()) {
        for (index, denied) in checks.iter().copied().enumerate() {
            let mut fixture = Fixture::new();
            match opcode {
                26 => {
                    assert_eq!(
                        fixture
                            .state
                            .resources
                            .window(fixture.confine)
                            .unwrap()
                            .map_state,
                        crate::resources::MapState::Viewable
                    );
                    assert!(fixture.state.active_pointer_grab.is_some());
                }
                29 => assert!(fixture.state.button_grabs.iter().any(|grab| {
                    grab.owner == CLIENT
                        && grab.grab_window == fixture.window
                        && grab.button == 1
                        && grab.modifiers == 0
                })),
                30 => assert!(fixture.state.active_pointer_grab.is_some()),
                33 => assert!(fixture.state.key_grabs.iter().any(|grab| {
                    grab.owner == CLIENT
                        && grab.grab_window == fixture.window
                        && grab.keycode == 24
                        && grab.modifiers == 0
                })),
                34 => assert!(fixture.state.key_grabs.iter().any(|grab| {
                    grab.owner == CLIENT
                        && grab.grab_window == fixture.window
                        && grab.keycode == 24
                        && grab.modifiers == 0
                })),
                _ => {}
            }
            let calls = Arc::new(Mutex::new(Vec::new()));
            fixture.install_policy(Arc::clone(&calls), Some(denied));
            let state_before = snapshot(&fixture.state);
            let backend_before = fixture.backend.calls();
            let bytes = fixture.run(50, opcode, data, &body);
            assert_x11_error(&bytes, BAD_VALUE, DENY_VALUE, opcode, 50);
            assert_eq!(fixture.backend.calls(), backend_before, "opcode {opcode}");
            assert_eq!(snapshot(&fixture.state), state_before, "opcode {opcode}");
            let recorded = calls.lock().unwrap().clone();
            assert_eq!(
                recorded.len(),
                index + 1,
                "opcode {opcode} must stop at denied operand"
            );
            for (actual, expected) in recorded.iter().zip(checks.iter()) {
                assert_eq!(actual.client, CLIENT, "opcode {opcode}");
                assert_eq!(
                    actual.request,
                    Some(RequestOpcodes {
                        major: opcode,
                        minor: 0,
                    }),
                    "opcode {opcode} request"
                );
                assert_eq!(
                    actual.target,
                    Some(expected.target()),
                    "opcode {opcode} target"
                );
                assert_eq!(actual.mode, Some(expected.mode), "opcode {opcode} mode");
                assert_eq!(
                    actual.owner,
                    expected_owner(&fixture, expected.id),
                    "opcode {opcode} owner"
                );
                assert_eq!(actual.target_owner, expected_owner(&fixture, expected.id));
            }
        }
    }
}

#[test]
fn task8_core_resource_requests_ask_in_protocol_order() {
    // Mutation: change a checked target, mode, request opcode, or operand order; the recorded call
    // sequence must fail.
    for (index, (opcode, data, body, checks)) in
        access_cases(&Fixture::new()).into_iter().enumerate()
    {
        let mut fixture = Fixture::new();
        let calls = Arc::new(Mutex::new(Vec::new()));
        fixture.install_policy(Arc::clone(&calls), None);
        let bytes = fixture.run(u16::try_from(80 + index).unwrap(), opcode, data, &body);
        assert!(
            bytes.is_empty() || bytes[0] != 0,
            "opcode {opcode} unexpectedly errored"
        );
        let recorded = calls.lock().unwrap().clone();
        assert_eq!(recorded.len(), checks.len(), "opcode {opcode} call count");
        for (actual, expected) in recorded.iter().zip(checks) {
            assert_eq!(actual.client, CLIENT, "opcode {opcode} client");
            assert_eq!(
                actual.request,
                Some(RequestOpcodes {
                    major: opcode,
                    minor: 0,
                }),
                "opcode {opcode} request"
            );
            assert_eq!(
                actual.target,
                Some(expected.target()),
                "opcode {opcode} target"
            );
            assert_eq!(actual.mode, Some(expected.mode), "opcode {opcode} mode");
            assert_eq!(
                actual.owner,
                expected_owner(&fixture, expected.id),
                "opcode {opcode} owner"
            );
            assert_eq!(actual.target_owner, expected_owner(&fixture, expected.id));
        }
    }
}

#[test]
fn change_save_set_denied_leaves_save_set_unchanged() {
    // Mutation: bypass the ChangeSaveSet MANAGE check; the denial and unchanged save-set assertions
    // must fail.
    let mut fixture = Fixture::new();
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture.install_policy(
        Arc::clone(&calls),
        Some(ExpectedCheck {
            id: fixture.foreign_window,
            kind: ResourceKind::Window,
            mode: AccessMode::MANAGE,
        }),
    );
    let original_parent = fixture
        .state
        .resources
        .window(fixture.foreign_window)
        .unwrap()
        .parent;
    let bytes = fixture.run(90, 6, 0, &words(&[fixture.foreign_window.0]));
    assert_x11_error(&bytes, BAD_VALUE, DENY_VALUE, 6, 90);
    assert!(
        !fixture.state.clients[&CLIENT.0]
            .save_set
            .contains(&fixture.foreign_window)
    );
    // Limitation: yserver does not consume save-set entries at disconnect today; this pre-existing
    // behavior is out of scope.
    crate::core_loop::process_disconnect::process_disconnect(
        &mut fixture.state,
        &mut fixture.backend,
        CLIENT,
    );
    let window = fixture
        .state
        .resources
        .window(fixture.foreign_window)
        .expect("the other client's save-set window survives");
    assert_eq!(window.parent, original_parent);
    assert_ne!(window.parent, ROOT_WINDOW);
}

#[test]
fn kill_client_resource_check_precedes_kill() {
    // Mutation: bypass the KillClient resource check; the policy error and owner-connected
    // assertions must fail.
    let mut fixture = Fixture::new();
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture.install_policy(
        Arc::clone(&calls),
        Some(ExpectedCheck {
            id: fixture.foreign_window,
            kind: ResourceKind::Window,
            mode: AccessMode::GET_ATTR,
        }),
    );
    let backend_before = fixture.backend.calls();
    let bytes = fixture.run(91, 113, 0, &words(&[fixture.foreign_window.0]));
    assert_x11_error(&bytes, BAD_VALUE, DENY_VALUE, 113, 91);
    assert_eq!(fixture.backend.calls(), backend_before);
    assert!(fixture.state.clients.contains_key(&OTHER_CLIENT.0));
    assert_eq!(
        fixture.state.resources.window_owner(fixture.foreign_window),
        Some(OTHER_CLIENT)
    );
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    let call = &calls[0];
    assert_eq!(call.hook, ExtensionHook::Resource);
    assert_eq!(call.client, CLIENT);
    assert_eq!(
        call.request,
        Some(RequestOpcodes {
            major: 113,
            minor: 0
        })
    );
    assert_eq!(
        call.target,
        Some(ResourceTarget {
            owner: None,
            id: fixture.foreign_window,
            kind: ResourceKind::Window,
            creation: None,
        })
    );
    assert_eq!(call.mode, Some(AccessMode::GET_ATTR));
    assert_eq!(call.owner, Some(OTHER_CLIENT));
    assert_eq!(call.target_owner, Some(OTHER_CLIENT));
}

#[test]
fn task8_set_selection_owner_access_check_precedes_invalid_atom() {
    // Mutation: move SetSelectionOwner's SET_ATTR check below atom validation; the policy-error
    // assertion must fail.
    let invalid_atom = u32::MAX;
    let mut denied = Fixture::new();
    let calls = Arc::new(Mutex::new(Vec::new()));
    denied.install_policy(
        Arc::clone(&calls),
        Some(ExpectedCheck {
            id: denied.other_window,
            kind: ResourceKind::Window,
            mode: AccessMode::SET_ATTR,
        }),
    );
    let body = words(&[denied.other_window.0, invalid_atom, 0]);
    let bytes = denied.run(92, 22, 0, &body);
    assert_x11_error(&bytes, BAD_VALUE, DENY_VALUE, 22, 92);
    assert_eq!(calls.lock().unwrap().len(), 1);

    let mut no_policy = Fixture::new();
    let body = words(&[no_policy.other_window.0, invalid_atom, 0]);
    let bytes = no_policy.run(93, 22, 0, &body);
    assert_x11_error(&bytes, BAD_ATOM, invalid_atom, 22, 93);
}

#[test]
fn task8_convert_selection_access_check_precedes_invalid_atom() {
    // Mutation: move ConvertSelection's SET_ATTR check below atom validation; the policy-error
    // assertion must fail.
    let invalid_atom = u32::MAX;
    let mut denied = Fixture::new();
    let calls = Arc::new(Mutex::new(Vec::new()));
    denied.install_policy(
        Arc::clone(&calls),
        Some(ExpectedCheck {
            id: denied.window,
            kind: ResourceKind::Window,
            mode: AccessMode::SET_ATTR,
        }),
    );
    let body = words(&[denied.window.0, invalid_atom, 1, 0, 0]);
    let bytes = denied.run(94, 24, 0, &body);
    assert_x11_error(&bytes, BAD_VALUE, DENY_VALUE, 24, 94);
    assert_eq!(calls.lock().unwrap().len(), 1);

    let mut no_policy = Fixture::new();
    let body = words(&[no_policy.window.0, invalid_atom, 1, 0, 0]);
    let bytes = no_policy.run(95, 24, 0, &body);
    assert_x11_error(&bytes, BAD_ATOM, 0, 24, 95);
}

#[test]
fn task8_stale_set_selection_owner_still_checks_window() {
    // Mutation: keep the stale-time return before SetSelectionOwner's SET_ATTR check; the
    // policy-error assertion must fail.
    let mut denied = Fixture::new();
    denied.state.start_instant = std::time::Instant::now() - std::time::Duration::from_secs(1);
    let selection = AtomId(1);
    let prior_time = denied.state.timestamp_now();
    let prior = crate::server::SelectionOwner {
        window: denied.other_window,
        last_time_changed: prior_time,
        client: Some(OTHER_CLIENT),
    };
    denied.state.selections.insert(selection, prior);
    let calls = Arc::new(Mutex::new(Vec::new()));
    denied.install_policy(
        Arc::clone(&calls),
        Some(ExpectedCheck {
            id: denied.window,
            kind: ResourceKind::Window,
            mode: AccessMode::SET_ATTR,
        }),
    );
    let body = words(&[denied.window.0, selection.0, prior_time - 1]);
    let bytes = denied.run(96, 22, 0, &body);
    assert_x11_error(&bytes, BAD_VALUE, DENY_VALUE, 22, 96);
    assert_eq!(calls.lock().unwrap().len(), 1);
    assert_eq!(denied.state.selections.get(&selection), Some(&prior));

    let mut no_policy = Fixture::new();
    no_policy.state.start_instant = std::time::Instant::now() - std::time::Duration::from_secs(1);
    let prior_time = no_policy.state.timestamp_now();
    let prior = crate::server::SelectionOwner {
        window: no_policy.other_window,
        last_time_changed: prior_time,
        client: Some(OTHER_CLIENT),
    };
    no_policy.state.selections.insert(selection, prior);
    let body = words(&[no_policy.window.0, selection.0, prior_time - 1]);
    let bytes = no_policy.run(97, 22, 0, &body);
    assert!(bytes.is_empty(), "stale SetSelectionOwner stays silent");
    assert_eq!(no_policy.state.selections.get(&selection), Some(&prior));
}
