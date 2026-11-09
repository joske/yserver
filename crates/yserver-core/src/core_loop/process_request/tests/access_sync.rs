use super::{
    access_resources::{
        RecordedResourceExpectation, expected_resource_checks, resource_request, words,
    },
    extensions::{ExtensionHook, RecordedExtensionCheck, RecordingExtensionPolicy},
    *,
};
use crate::access::{
    AccessMode, Creation, Decision, DenyError, RequestOpcodes, ResourceKind, ResourceTarget,
};
use std::sync::atomic::{AtomicU32, Ordering};

const CLIENT: ClientId = ClientId(1);
const COUNTER: ResourceId = ResourceId(0x0010_8001);
const NEW_COUNTER: ResourceId = ResourceId(0x0010_8002);
const PRIORITY_WINDOW: ResourceId = ResourceId(0x0010_8003);
const DENY_ERROR: DenyError = DenyError {
    code: x11::error::BAD_ACCESS,
    value: 0xA55A_1234,
};

struct SyncAccessFixture {
    state: ServerState,
    peer: UnixStream,
    backend: RecordingBackend,
}

fn sync_access_fixture() -> SyncAccessFixture {
    let mut state = ServerState::new();
    let peer = install_client(&mut state, CLIENT.0);
    SyncAccessFixture {
        state,
        peer,
        backend: RecordingBackend::new(),
    }
}

fn sync_access_request(fixture: &mut SyncAccessFixture, minor: u8, body: &[u8]) -> Vec<u8> {
    process_request(
        &mut fixture.state,
        &mut fixture.backend,
        CLIENT,
        SequenceNumber(1),
        RequestHeader {
            opcode: 142,
            data: minor,
            length_units: u32::try_from((4 + body.len()).div_ceil(4)).unwrap(),
        },
        body,
        None,
    )
    .expect("SYNC request through process_request");
    read_all_available(&mut fixture.peer)
}

fn sync_i64(value: i64) -> [u8; 8] {
    #[allow(clippy::cast_possible_truncation)]
    let high = (value >> 32) as i32;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let low = value as u32;
    let mut bytes = [0; 8];
    bytes[..4].copy_from_slice(&high.to_le_bytes());
    bytes[4..].copy_from_slice(&low.to_le_bytes());
    bytes
}

fn counter_body(id: ResourceId, value: i64) -> Vec<u8> {
    let mut body = id.0.to_le_bytes().to_vec();
    body.extend_from_slice(&sync_i64(value));
    body
}

fn resource_body(id: ResourceId) -> Vec<u8> {
    id.0.to_le_bytes().to_vec()
}

fn priority_body(id: ResourceId) -> Vec<u8> {
    let mut body = resource_body(id);
    body.extend_from_slice(&0i32.to_le_bytes());
    body
}

fn counter_target(id: ResourceId, creation: Option<Creation>) -> ResourceTarget {
    ResourceTarget {
        id,
        kind: ResourceKind::SyncCounter,
        creation,
        owner: None,
    }
}

fn create_priority_window(state: &mut ServerState) {
    state.resources.create_window(
        CLIENT,
        CreateWindowRequest {
            depth: 24,
            window: PRIORITY_WINDOW,
            parent: ROOT_WINDOW,
            width: 16,
            height: 16,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
}

fn assert_policy_error(bytes: &[u8], minor: u8) {
    assert_eq!(bytes.len(), 32, "expected one X error packet: {bytes:02x?}");
    assert_eq!(bytes[0], 0, "expected X error packet");
    assert_eq!(bytes[1], DENY_ERROR.code);
    assert_eq!(u16::from_le_bytes(bytes[2..4].try_into().unwrap()), 1);
    assert_eq!(
        u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
        DENY_ERROR.value,
        "policy error value must be forwarded verbatim"
    );
    assert_eq!(
        u16::from_le_bytes(bytes[8..10].try_into().unwrap()),
        u16::from(minor)
    );
    assert_eq!(bytes[10], 142);
}

fn assert_recorded_resource(
    call: &RecordedExtensionCheck,
    target: ResourceTarget,
    mode: AccessMode,
    minor: u8,
    owner: Option<ClientId>,
    target_owner: Option<ClientId>,
) {
    assert_eq!(call.hook, ExtensionHook::Resource);
    assert_eq!(call.client, CLIENT);
    assert_eq!(call.target, Some(target));
    assert_eq!(call.mode, Some(mode));
    assert_eq!(call.request, Some(RequestOpcodes { major: 142, minor }));
    assert_eq!(call.owner, owner);
    assert_eq!(call.target_owner, target_owner);
}

fn assert_sync_error(bytes: &[u8], code: u8, value: u32, minor: u8, sequence: u16) {
    let mut expected = vec![0, code];
    expected.extend_from_slice(&sequence.to_le_bytes());
    expected.extend_from_slice(&value.to_le_bytes());
    expected.extend_from_slice(&u16::from(minor).to_le_bytes());
    expected.push(142);
    expected.extend_from_slice(&[0; 21]);
    assert_eq!(bytes, expected, "SYNC request {minor} error packet");
}

fn expected_sync_resource_checks(
    calls: &[RecordedExtensionCheck],
) -> Vec<RecordedResourceExpectation> {
    let resources = calls
        .iter()
        .filter(|call| call.hook == ExtensionHook::Resource)
        .cloned()
        .collect::<Vec<_>>();
    expected_resource_checks(&resources)
}

fn expected_sync_resource_checks_after(
    calls: &[RecordedExtensionCheck],
    start: usize,
) -> Vec<RecordedResourceExpectation> {
    let resources = calls
        .iter()
        .filter(|call| call.hook == ExtensionHook::Resource)
        .skip(start)
        .cloned()
        .collect::<Vec<_>>();
    expected_resource_checks(&resources)
}

struct DenyRow {
    label: &'static str,
    minor: u8,
    body: Vec<u8>,
    target: ResourceTarget,
    mode: AccessMode,
    seed_counter: Option<i64>,
    seed_window: bool,
    owner: Option<ClientId>,
    target_owner: Option<ClientId>,
}

fn sync_deny_rows() -> Vec<DenyRow> {
    use yserver_protocol::x11::sync;

    vec![
        DenyRow {
            label: "CreateCounter",
            minor: sync::CREATE_COUNTER,
            body: counter_body(NEW_COUNTER, 17),
            target: counter_target(
                NEW_COUNTER,
                Some(Creation {
                    parent: None,
                    owner: CLIENT,
                }),
            ),
            mode: AccessMode::CREATE,
            seed_counter: None,
            seed_window: false,
            owner: None,
            target_owner: None,
        },
        DenyRow {
            label: "SetCounter",
            minor: sync::SET_COUNTER,
            body: counter_body(COUNTER, 17),
            target: counter_target(COUNTER, None),
            mode: AccessMode::WRITE,
            seed_counter: Some(3),
            seed_window: false,
            owner: Some(CLIENT),
            target_owner: Some(CLIENT),
        },
        DenyRow {
            label: "ChangeCounter overflow precedence",
            minor: sync::CHANGE_COUNTER,
            body: counter_body(COUNTER, 1),
            target: counter_target(COUNTER, None),
            mode: AccessMode::WRITE,
            seed_counter: Some(i64::MAX),
            seed_window: false,
            owner: Some(CLIENT),
            target_owner: Some(CLIENT),
        },
        DenyRow {
            label: "QueryCounter",
            minor: sync::QUERY_COUNTER,
            body: resource_body(COUNTER),
            target: counter_target(COUNTER, None),
            mode: AccessMode::READ,
            seed_counter: Some(3),
            seed_window: false,
            owner: Some(CLIENT),
            target_owner: Some(CLIENT),
        },
        DenyRow {
            label: "DestroyCounter",
            minor: sync::DESTROY_COUNTER,
            body: resource_body(COUNTER),
            target: counter_target(COUNTER, None),
            mode: AccessMode::DESTROY,
            seed_counter: Some(3),
            seed_window: false,
            owner: Some(CLIENT),
            target_owner: Some(CLIENT),
        },
        DenyRow {
            label: "SetPriority",
            minor: sync::SET_PRIORITY,
            body: priority_body(PRIORITY_WINDOW),
            target: ResourceTarget {
                id: PRIORITY_WINDOW,
                kind: ResourceKind::Window,
                creation: None,
                owner: None,
            },
            mode: AccessMode::GET_ATTR,
            seed_counter: None,
            seed_window: true,
            owner: Some(CLIENT),
            target_owner: Some(CLIENT),
        },
        DenyRow {
            label: "GetPriority",
            minor: sync::GET_PRIORITY,
            body: resource_body(PRIORITY_WINDOW),
            target: ResourceTarget {
                id: PRIORITY_WINDOW,
                kind: ResourceKind::Window,
                creation: None,
                owner: None,
            },
            mode: AccessMode::GET_ATTR,
            seed_counter: None,
            seed_window: true,
            owner: Some(CLIENT),
            target_owner: Some(CLIENT),
        },
    ]
}

#[test]
fn sync_counter_and_priority_denials_match_the_site_table() {
    // Mutations: move ChangeCounter's check after overflow BadValue; omit any listed check.
    for row in sync_deny_rows() {
        let mut fixture = sync_access_fixture();
        if let Some(value) = row.seed_counter {
            fixture.state.sync_counters.insert(
                COUNTER.0,
                crate::server::SyncCounter {
                    owner: CLIENT,
                    value,
                },
            );
        }
        if row.seed_window {
            create_priority_window(&mut fixture.state);
        }
        let calls = Arc::new(Mutex::new(Vec::new()));
        fixture
            .state
            .install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
                Arc::clone(&calls),
                row.target.id,
                Some(row.mode),
                DENY_ERROR,
            )));
        let counters_before = fixture.state.sync_counters.clone();
        let target_owner_before = fixture
            .state
            .access_resource_owner(row.target.id, row.target.kind);

        let bytes = sync_access_request(&mut fixture, row.minor, &row.body);

        assert_policy_error(&bytes, row.minor);
        assert_eq!(
            fixture.state.sync_counters, counters_before,
            "{} changed counters",
            row.label
        );
        assert_eq!(
            fixture
                .state
                .access_resource_owner(row.target.id, row.target.kind),
            target_owner_before,
            "{} changed its target",
            row.label
        );
        assert!(
            fixture.backend.calls().is_empty(),
            "{} reached backend",
            row.label
        );
        let calls = calls.lock().unwrap();
        let resource_calls: Vec<_> = calls
            .iter()
            .filter(|call| call.hook == ExtensionHook::Resource)
            .collect();
        assert_eq!(resource_calls.len(), 1, "{} resource checks", row.label);
        assert_recorded_resource(
            resource_calls[0],
            row.target,
            row.mode,
            row.minor,
            row.owner,
            row.target_owner,
        );
    }
}

#[test]
fn sync_counter_and_priority_asks_match_the_site_table() {
    // Mutations: change a mode, omit Creation metadata, or omit the extension minor opcode.
    use yserver_protocol::x11::sync;

    let rows = [
        (
            sync::CREATE_COUNTER,
            counter_body(NEW_COUNTER, 17),
            counter_target(
                NEW_COUNTER,
                Some(Creation {
                    parent: None,
                    owner: CLIENT,
                }),
            ),
            AccessMode::CREATE,
            None,
            None,
        ),
        (
            sync::SET_COUNTER,
            counter_body(COUNTER, 17),
            counter_target(COUNTER, None),
            AccessMode::WRITE,
            Some(CLIENT),
            Some(CLIENT),
        ),
        (
            sync::CHANGE_COUNTER,
            counter_body(COUNTER, 1),
            counter_target(COUNTER, None),
            AccessMode::WRITE,
            Some(CLIENT),
            Some(CLIENT),
        ),
        (
            sync::QUERY_COUNTER,
            resource_body(COUNTER),
            counter_target(COUNTER, None),
            AccessMode::READ,
            Some(CLIENT),
            Some(CLIENT),
        ),
        (
            sync::DESTROY_COUNTER,
            resource_body(COUNTER),
            counter_target(COUNTER, None),
            AccessMode::DESTROY,
            Some(CLIENT),
            Some(CLIENT),
        ),
        (
            sync::SET_PRIORITY,
            priority_body(PRIORITY_WINDOW),
            ResourceTarget {
                id: PRIORITY_WINDOW,
                kind: ResourceKind::Window,
                creation: None,
                owner: None,
            },
            AccessMode::GET_ATTR,
            Some(CLIENT),
            Some(CLIENT),
        ),
        (
            sync::GET_PRIORITY,
            resource_body(PRIORITY_WINDOW),
            ResourceTarget {
                id: PRIORITY_WINDOW,
                kind: ResourceKind::Window,
                creation: None,
                owner: None,
            },
            AccessMode::GET_ATTR,
            Some(CLIENT),
            Some(CLIENT),
        ),
    ];

    for (minor, body, target, mode, owner, target_owner) in rows {
        let mut fixture = sync_access_fixture();
        if matches!(
            minor,
            sync::SET_COUNTER | sync::CHANGE_COUNTER | sync::QUERY_COUNTER | sync::DESTROY_COUNTER
        ) {
            fixture.state.sync_counters.insert(
                COUNTER.0,
                crate::server::SyncCounter {
                    owner: CLIENT,
                    value: 3,
                },
            );
        }
        if matches!(minor, sync::SET_PRIORITY | sync::GET_PRIORITY) {
            create_priority_window(&mut fixture.state);
        }
        let calls = Arc::new(Mutex::new(Vec::new()));
        fixture
            .state
            .install_access_policy(Arc::new(RecordingExtensionPolicy::new(
                Decision::Allow,
                Arc::clone(&calls),
            )));

        let _bytes = sync_access_request(&mut fixture, minor, &body);

        let calls = calls.lock().unwrap();
        let resource_calls: Vec<_> = calls
            .iter()
            .filter(|call| call.hook == ExtensionHook::Resource)
            .collect();
        assert_eq!(resource_calls.len(), 1, "SYNC minor {minor}");
        assert_recorded_resource(resource_calls[0], target, mode, minor, owner, target_owner);
        if minor == sync::CREATE_COUNTER {
            assert_eq!(
                fixture
                    .state
                    .sync_counters
                    .get(&NEW_COUNTER.0)
                    .unwrap()
                    .value,
                17
            );
        }
    }
}

#[test]
fn system_counter_query_is_asked_with_server_owner() {
    // Mutation: skip QueryCounter checks for system counters; this must lose its recorded ask.
    use yserver_protocol::x11::sync;

    let mut fixture = sync_access_fixture();
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .state
        .install_access_policy(Arc::new(RecordingExtensionPolicy::new(
            Decision::Allow,
            Arc::clone(&calls),
        )));

    let bytes = sync_access_request(
        &mut fixture,
        sync::QUERY_COUNTER,
        &resource_body(ResourceId(sync::SERVERTIME_COUNTER)),
    );

    assert!(!bytes.is_empty(), "QueryCounter should return its reply");
    let calls = calls.lock().unwrap();
    let resource_calls: Vec<_> = calls
        .iter()
        .filter(|call| call.hook == ExtensionHook::Resource)
        .collect();
    assert_eq!(resource_calls.len(), 1);
    assert_recorded_resource(
        resource_calls[0],
        counter_target(ResourceId(sync::SERVERTIME_COUNTER), None),
        AccessMode::READ,
        sync::QUERY_COUNTER,
        Some(crate::resources::SERVER_OWNER),
        Some(crate::resources::SERVER_OWNER),
    );
}

#[test]
fn set_priority_unresolved_resource_is_not_asked() {
    // Mutation: check an unresolved SetPriority id instead of preserving its if-present behavior.
    use yserver_protocol::x11::sync;

    let unresolved = ResourceId(0x0010_8FFF);
    let body = priority_body(unresolved);
    let mut no_policy_fixture = sync_access_fixture();
    let no_policy_bytes = sync_access_request(&mut no_policy_fixture, sync::SET_PRIORITY, &body);

    let mut deny_fixture = sync_access_fixture();
    let calls = Arc::new(Mutex::new(Vec::new()));
    deny_fixture.state.install_access_policy(Arc::new(
        RecordingExtensionPolicy::denying_all_resources(Arc::clone(&calls), DENY_ERROR),
    ));
    let deny_bytes = sync_access_request(&mut deny_fixture, sync::SET_PRIORITY, &body);

    assert_eq!(
        deny_bytes, no_policy_bytes,
        "unresolved resource changed SetPriority bytes"
    );
    assert!(
        deny_bytes.is_empty(),
        "SetPriority stub should still succeed"
    );
    assert!(
        calls
            .lock()
            .unwrap()
            .iter()
            .all(|call| call.hook != ExtensionHook::Resource),
        "unresolved SetPriority resource must not reach the resource policy"
    );
    assert!(deny_fixture.backend.calls().is_empty());
}

#[test]
fn await_denied_second_counter_installs_nothing() {
    // Mutation: install an Await after each entry; denial of the second counter must leave no await.
    use yserver_protocol::x11::sync;
    const FIRST: ResourceId = ResourceId(0x0056_0001);
    const SECOND: ResourceId = ResourceId(0x0056_0002);
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, CLIENT.0);
    let mut backend = RecordingBackend::new();
    for counter in [FIRST, SECOND] {
        let bytes = resource_request(
            &mut state,
            &mut backend,
            &mut peer,
            1,
            142,
            sync::CREATE_COUNTER,
            &words(&[counter.0, 0, 0]),
        );
        assert_no_error(&bytes, "Await counter fixture");
    }

    let calls = Arc::new(Mutex::new(Vec::new()));
    state.install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
        Arc::clone(&calls),
        SECOND,
        Some(AccessMode::READ),
        DenyError {
            code: x11::error::BAD_VALUE,
            value: 0x55,
        },
    )));
    let await_body = words(&[
        FIRST.0,
        sync::VALUE_TYPE_ABSOLUTE,
        0,
        100,
        sync::TEST_POSITIVE_COMPARISON,
        0,
        0,
        SECOND.0,
        sync::VALUE_TYPE_ABSOLUTE,
        0,
        100,
        sync::TEST_POSITIVE_COMPARISON,
        0,
        0,
    ]);
    let backend_before = backend.calls();
    let bytes = resource_request(
        &mut state,
        &mut backend,
        &mut peer,
        2,
        142,
        sync::AWAIT,
        &await_body,
    );

    assert_sync_error(&bytes, x11::error::BAD_VALUE, 0x55, sync::AWAIT, 2);
    assert!(
        state.sync_awaits.is_empty(),
        "denied Await installed nothing"
    );
    assert_eq!(backend.calls(), backend_before);
    assert_eq!(
        expected_sync_resource_checks(&calls.lock().unwrap()),
        [
            (
                CLIENT,
                FIRST,
                ResourceKind::SyncCounter,
                AccessMode::READ,
                Some(RequestOpcodes {
                    major: 142,
                    minor: sync::AWAIT
                })
            ),
            (
                CLIENT,
                SECOND,
                ResourceKind::SyncCounter,
                AccessMode::READ,
                Some(RequestOpcodes {
                    major: 142,
                    minor: sync::AWAIT
                })
            ),
        ]
    );
}

#[test]
fn create_alarm_denied_creation_creates_nothing() {
    // Mutation: move or omit CreateAlarm's CREATE check after insertion; the denied alarm must remain absent.
    use yserver_protocol::x11::sync;
    const ALARM: ResourceId = ResourceId(0x0056_0010);
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, CLIENT.0);
    let mut backend = RecordingBackend::new();
    let calls = Arc::new(Mutex::new(Vec::new()));
    state.install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
        Arc::clone(&calls),
        ALARM,
        Some(AccessMode::CREATE),
        DenyError {
            code: x11::error::BAD_VALUE,
            value: 0x55,
        },
    )));
    let backend_before = backend.calls();
    let bytes = resource_request(
        &mut state,
        &mut backend,
        &mut peer,
        1,
        142,
        sync::CREATE_ALARM,
        &words(&[ALARM.0, 0]),
    );

    assert_sync_error(&bytes, x11::error::BAD_VALUE, 0x55, sync::CREATE_ALARM, 1);
    assert!(!state.sync_alarms.contains_key(&ALARM.0));
    assert_eq!(backend.calls(), backend_before);
    let recorded = calls.lock().unwrap();
    assert_eq!(
        expected_sync_resource_checks(&recorded),
        [(
            CLIENT,
            ALARM,
            ResourceKind::SyncAlarm,
            AccessMode::CREATE,
            Some(RequestOpcodes {
                major: 142,
                minor: sync::CREATE_ALARM
            })
        )]
    );
    let creation_check = recorded
        .iter()
        .find(|call| call.hook == ExtensionHook::Resource)
        .expect("CreateAlarm resource check");
    let target = creation_check.target.expect("CreateAlarm target");
    assert_eq!(
        target.creation,
        Some(Creation {
            parent: None,
            owner: CLIENT,
        })
    );
    assert_eq!(creation_check.target_owner, None);
}

#[test]
fn change_alarm_denied_counter_keeps_todays_partial_effects() {
    // Mutation: check the counter before earlier fields are applied; the state must match BadCounter's partial effect.
    use yserver_protocol::x11::sync;
    const COUNTER: ResourceId = ResourceId(0x0056_0020);
    const BASELINE_ALARM: ResourceId = ResourceId(0x0056_0021);
    const DENIED_ALARM: ResourceId = ResourceId(0x0056_0022);
    const OTHER_CLIENT: ClientId = ClientId(2);
    let mut state = ServerState::new();
    let mut owner_peer = install_client(&mut state, CLIENT.0);
    let mut requester_peer = install_client(&mut state, OTHER_CLIENT.0);
    let mut backend = RecordingBackend::new();
    for (sequence, opcode, body) in [
        (1, sync::CREATE_COUNTER, words(&[COUNTER.0, 0, 0])),
        (2, sync::CREATE_ALARM, words(&[BASELINE_ALARM.0, 0])),
        (3, sync::CREATE_ALARM, words(&[DENIED_ALARM.0, 0])),
    ] {
        let bytes = resource_request(
            &mut state,
            &mut backend,
            &mut owner_peer,
            sequence,
            142,
            opcode,
            &body,
        );
        assert_no_error(&bytes, "ChangeAlarm partial-effect fixture");
    }

    let change_body = |alarm: ResourceId, counter: ResourceId| {
        words(&[alarm.0, sync::CA_COUNTER | sync::CA_EVENTS, counter.0, 1])
    };
    let baseline_counter = ResourceId(0x0056_00ff);
    let baseline = change_body(BASELINE_ALARM, baseline_counter);
    process_request(
        &mut state,
        &mut backend,
        OTHER_CLIENT,
        SequenceNumber(4),
        RequestHeader {
            opcode: 142,
            data: sync::CHANGE_ALARM,
            length_units: 1 + u32::try_from(baseline.len().div_ceil(4)).unwrap(),
        },
        &baseline,
        None,
    )
    .unwrap();
    let baseline_error = read_all_available(&mut requester_peer);
    assert_sync_error(
        &baseline_error,
        crate::nested::SYNC_FIRST_ERROR + sync::BAD_COUNTER,
        baseline_counter.0,
        sync::CHANGE_ALARM,
        4,
    );
    let baseline_state = state.sync_alarms[&BASELINE_ALARM.0].clone();
    assert!(baseline_state.event_clients.contains(&OTHER_CLIENT));

    let calls = Arc::new(Mutex::new(Vec::new()));
    state.install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
        Arc::clone(&calls),
        COUNTER,
        Some(AccessMode::READ),
        DenyError {
            code: x11::error::BAD_VALUE,
            value: 0x55,
        },
    )));
    let backend_before = backend.calls();
    let denied_body = change_body(DENIED_ALARM, COUNTER);
    process_request(
        &mut state,
        &mut backend,
        OTHER_CLIENT,
        SequenceNumber(5),
        RequestHeader {
            opcode: 142,
            data: sync::CHANGE_ALARM,
            length_units: 1 + u32::try_from(denied_body.len().div_ceil(4)).unwrap(),
        },
        &denied_body,
        None,
    )
    .unwrap();
    let denied = read_all_available(&mut requester_peer);

    assert_sync_error(&denied, x11::error::BAD_VALUE, 0x55, sync::CHANGE_ALARM, 5);
    assert_eq!(state.sync_alarms[&DENIED_ALARM.0], baseline_state);
    assert_eq!(backend.calls(), backend_before);
    assert_eq!(
        expected_sync_resource_checks(&calls.lock().unwrap()),
        [
            (
                OTHER_CLIENT,
                DENIED_ALARM,
                ResourceKind::SyncAlarm,
                AccessMode::WRITE,
                Some(RequestOpcodes {
                    major: 142,
                    minor: sync::CHANGE_ALARM
                })
            ),
            (
                OTHER_CLIENT,
                COUNTER,
                ResourceKind::SyncCounter,
                AccessMode::READ,
                Some(RequestOpcodes {
                    major: 142,
                    minor: sync::CHANGE_ALARM
                })
            ),
        ]
    );
}

#[test]
fn sync_alarm_resource_denials_match_modes_and_stop_before_effects() {
    // Mutation: remove or change the mode of any alarm/counter check; that row must stop returning the policy error.
    use yserver_protocol::x11::sync;
    const COUNTER: ResourceId = ResourceId(0x0056_0030);
    const ALARM: ResourceId = ResourceId(0x0056_0031);
    const CREATE_COUNTER_ALARM: ResourceId = ResourceId(0x0056_0032);
    const CREATE_COUNTER_MISSING: ResourceId = ResourceId(0x0056_00ff);
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, CLIENT.0);
    let mut backend = RecordingBackend::new();
    for (sequence, opcode, body) in [
        (1, sync::CREATE_COUNTER, words(&[COUNTER.0, 0, 5])),
        (2, sync::CREATE_ALARM, words(&[ALARM.0, 0])),
    ] {
        let bytes = resource_request(
            &mut state,
            &mut backend,
            &mut peer,
            sequence,
            142,
            opcode,
            &body,
        );
        assert_no_error(&bytes, "SYNC alarm denial fixture");
    }

    let calls = Arc::new(Mutex::new(Vec::new()));
    let denied_id = Arc::new(AtomicU32::new(0));
    let denied_id_for_policy = Arc::clone(&denied_id);
    state.install_access_policy(Arc::new(RecordingExtensionPolicy::custom(
        Arc::clone(&calls),
        "SYNC",
        move |_, call| {
            if call.hook == ExtensionHook::Resource
                && call.target.is_some_and(|target| {
                    target.id.0 == denied_id_for_policy.load(Ordering::SeqCst)
                })
            {
                Decision::Deny(DenyError {
                    code: x11::error::BAD_VALUE,
                    value: 0x55,
                })
            } else {
                Decision::Allow
            }
        },
    )));
    let backend_before = backend.calls();

    denied_id.store(CREATE_COUNTER_MISSING.0, Ordering::SeqCst);
    let calls_before = calls
        .lock()
        .unwrap()
        .iter()
        .filter(|call| call.hook == ExtensionHook::Resource)
        .count();
    let missing_counter_alarm = resource_request(
        &mut state,
        &mut backend,
        &mut peer,
        10,
        142,
        sync::CREATE_ALARM,
        &words(&[
            CREATE_COUNTER_ALARM.0,
            sync::CA_COUNTER,
            CREATE_COUNTER_MISSING.0,
        ]),
    );
    assert_sync_error(
        &missing_counter_alarm,
        crate::nested::SYNC_FIRST_ERROR + sync::BAD_COUNTER,
        CREATE_COUNTER_MISSING.0,
        sync::CREATE_ALARM,
        10,
    );
    assert_eq!(
        calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.hook == ExtensionHook::Resource)
            .count(),
        calls_before,
        "BadCounter precedes the counter check"
    );
    assert!(!state.sync_alarms.contains_key(&CREATE_COUNTER_ALARM.0));

    denied_id.store(COUNTER.0, Ordering::SeqCst);
    let denied_counter_alarm = resource_request(
        &mut state,
        &mut backend,
        &mut peer,
        11,
        142,
        sync::CREATE_ALARM,
        &words(&[CREATE_COUNTER_ALARM.0, sync::CA_COUNTER, COUNTER.0]),
    );
    assert_sync_error(
        &denied_counter_alarm,
        x11::error::BAD_VALUE,
        0x55,
        sync::CREATE_ALARM,
        11,
    );
    assert!(!state.sync_alarms.contains_key(&CREATE_COUNTER_ALARM.0));
    assert_eq!(
        expected_sync_resource_checks_after(&calls.lock().unwrap(), calls_before),
        [(
            CLIENT,
            COUNTER,
            ResourceKind::SyncCounter,
            AccessMode::READ,
            Some(RequestOpcodes {
                major: 142,
                minor: sync::CREATE_ALARM
            })
        )]
    );

    denied_id.store(ALARM.0, Ordering::SeqCst);
    let before_alarm = state.sync_alarms[&ALARM.0].clone();
    let denied_change = resource_request(
        &mut state,
        &mut backend,
        &mut peer,
        12,
        142,
        sync::CHANGE_ALARM,
        &words(&[ALARM.0, sync::CA_EVENTS, 0]),
    );
    assert_sync_error(
        &denied_change,
        x11::error::BAD_VALUE,
        0x55,
        sync::CHANGE_ALARM,
        12,
    );
    assert_eq!(state.sync_alarms[&ALARM.0], before_alarm);
    assert_eq!(
        expected_sync_resource_checks_after(&calls.lock().unwrap(), calls_before + 1),
        [(
            CLIENT,
            ALARM,
            ResourceKind::SyncAlarm,
            AccessMode::WRITE,
            Some(RequestOpcodes {
                major: 142,
                minor: sync::CHANGE_ALARM
            })
        )]
    );

    let denied_query = resource_request(
        &mut state,
        &mut backend,
        &mut peer,
        13,
        142,
        sync::QUERY_ALARM,
        &ALARM.0.to_le_bytes(),
    );
    assert_sync_error(
        &denied_query,
        x11::error::BAD_VALUE,
        0x55,
        sync::QUERY_ALARM,
        13,
    );
    assert_eq!(state.sync_alarms[&ALARM.0], before_alarm);
    assert_eq!(
        expected_sync_resource_checks_after(&calls.lock().unwrap(), calls_before + 2),
        [(
            CLIENT,
            ALARM,
            ResourceKind::SyncAlarm,
            AccessMode::READ,
            Some(RequestOpcodes {
                major: 142,
                minor: sync::QUERY_ALARM
            })
        )]
    );

    let denied_destroy = resource_request(
        &mut state,
        &mut backend,
        &mut peer,
        14,
        142,
        sync::DESTROY_ALARM,
        &ALARM.0.to_le_bytes(),
    );
    assert_sync_error(
        &denied_destroy,
        x11::error::BAD_VALUE,
        0x55,
        sync::DESTROY_ALARM,
        14,
    );
    assert_eq!(state.sync_alarms[&ALARM.0], before_alarm);
    assert_eq!(backend.calls(), backend_before);
    assert_eq!(
        expected_sync_resource_checks_after(&calls.lock().unwrap(), calls_before + 3),
        [(
            CLIENT,
            ALARM,
            ResourceKind::SyncAlarm,
            AccessMode::DESTROY,
            Some(RequestOpcodes {
                major: 142,
                minor: sync::DESTROY_ALARM
            })
        )]
    );
}

#[test]
fn sync_alarm_resource_requests_present_expected_checks() {
    // Mutation: omit/reorder a check or erase the creation record; this exact ASK table must fail.
    use yserver_protocol::x11::sync;
    const FIRST: ResourceId = ResourceId(0x0056_0040);
    const SECOND: ResourceId = ResourceId(0x0056_0041);
    const ALARM: ResourceId = ResourceId(0x0056_0042);
    const CREATED_ALARM: ResourceId = ResourceId(0x0056_0043);
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, CLIENT.0);
    let mut backend = RecordingBackend::new();
    for (sequence, opcode, body) in [
        (1, sync::CREATE_COUNTER, words(&[FIRST.0, 0, 0])),
        (2, sync::CREATE_COUNTER, words(&[SECOND.0, 0, 0])),
        (3, sync::CREATE_ALARM, words(&[ALARM.0, 0])),
    ] {
        let bytes = resource_request(
            &mut state,
            &mut backend,
            &mut peer,
            sequence,
            142,
            opcode,
            &body,
        );
        assert_no_error(&bytes, "SYNC alarm ASK fixture");
    }
    let calls = Arc::new(Mutex::new(Vec::new()));
    state.install_access_policy(Arc::new(RecordingExtensionPolicy::new(
        Decision::Allow,
        Arc::clone(&calls),
    )));
    let backend_before = backend.calls();

    let await_body = words(&[
        FIRST.0,
        sync::VALUE_TYPE_ABSOLUTE,
        0,
        0,
        sync::TEST_POSITIVE_COMPARISON,
        0,
        0,
        SECOND.0,
        sync::VALUE_TYPE_ABSOLUTE,
        0,
        0,
        sync::TEST_POSITIVE_COMPARISON,
        0,
        0,
    ]);
    assert_no_error(
        &resource_request(
            &mut state,
            &mut backend,
            &mut peer,
            10,
            142,
            sync::AWAIT,
            &await_body,
        ),
        "allowed SYNC Await",
    );
    assert!(
        state.sync_awaits.is_empty(),
        "already satisfied Await completes inline"
    );
    assert!(read_all_available(&mut peer).is_empty());

    assert_no_error(
        &resource_request(
            &mut state,
            &mut backend,
            &mut peer,
            11,
            142,
            sync::CREATE_ALARM,
            &words(&[CREATED_ALARM.0, sync::CA_COUNTER, FIRST.0]),
        ),
        "allowed SYNC CreateAlarm",
    );
    assert_no_error(
        &resource_request(
            &mut state,
            &mut backend,
            &mut peer,
            12,
            142,
            sync::CHANGE_ALARM,
            &words(&[ALARM.0, sync::CA_COUNTER, FIRST.0]),
        ),
        "allowed SYNC ChangeAlarm",
    );
    let query = resource_request(
        &mut state,
        &mut backend,
        &mut peer,
        13,
        142,
        sync::QUERY_ALARM,
        &ALARM.0.to_le_bytes(),
    );
    assert_eq!(query.first(), Some(&1), "allowed QueryAlarm reply");
    assert_no_error(
        &resource_request(
            &mut state,
            &mut backend,
            &mut peer,
            14,
            142,
            sync::DESTROY_ALARM,
            &ALARM.0.to_le_bytes(),
        ),
        "allowed SYNC DestroyAlarm",
    );

    let request = |minor| Some(RequestOpcodes { major: 142, minor });
    assert_eq!(
        expected_sync_resource_checks(&calls.lock().unwrap()),
        [
            (
                CLIENT,
                FIRST,
                ResourceKind::SyncCounter,
                AccessMode::READ,
                request(sync::AWAIT)
            ),
            (
                CLIENT,
                SECOND,
                ResourceKind::SyncCounter,
                AccessMode::READ,
                request(sync::AWAIT)
            ),
            (
                CLIENT,
                FIRST,
                ResourceKind::SyncCounter,
                AccessMode::READ,
                request(sync::CREATE_ALARM)
            ),
            (
                CLIENT,
                CREATED_ALARM,
                ResourceKind::SyncAlarm,
                AccessMode::CREATE,
                request(sync::CREATE_ALARM)
            ),
            (
                CLIENT,
                ALARM,
                ResourceKind::SyncAlarm,
                AccessMode::WRITE,
                request(sync::CHANGE_ALARM)
            ),
            (
                CLIENT,
                FIRST,
                ResourceKind::SyncCounter,
                AccessMode::READ,
                request(sync::CHANGE_ALARM)
            ),
            (
                CLIENT,
                ALARM,
                ResourceKind::SyncAlarm,
                AccessMode::READ,
                request(sync::QUERY_ALARM)
            ),
            (
                CLIENT,
                ALARM,
                ResourceKind::SyncAlarm,
                AccessMode::DESTROY,
                request(sync::DESTROY_ALARM)
            ),
        ]
    );
    let recorded = calls.lock().unwrap();
    let creation = recorded
        .iter()
        .find(|call| call.target.is_some_and(|target| target.id == CREATED_ALARM))
        .expect("CreateAlarm creation check");
    assert_eq!(creation.client, CLIENT);
    assert_eq!(creation.target_owner, None);
    assert_eq!(
        creation.target.unwrap().creation,
        Some(Creation {
            parent: None,
            owner: CLIENT,
        })
    );
    assert_eq!(backend.calls(), backend_before);
}

#[test]
fn alarm_none_counter_zero_is_not_asked() {
    // Mutation: remove the counter != 0 guard; a denied READ on counter 0 must not reject None.
    use yserver_protocol::x11::sync;

    const COUNTER_ZERO: ResourceId = ResourceId(0);
    const ALARM: ResourceId = ResourceId(0x0010_80f0);
    let mut fixture = sync_access_fixture();
    let bytes = sync_access_request(
        &mut fixture,
        sync::CREATE_COUNTER,
        &counter_body(COUNTER_ZERO, 0),
    );
    assert!(
        bytes.is_empty(),
        "counter 0 fixture creation should succeed"
    );

    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .state
        .install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
            Arc::clone(&calls),
            COUNTER_ZERO,
            Some(AccessMode::READ),
            DENY_ERROR,
        )));

    let create_alarm = sync_access_request(
        &mut fixture,
        sync::CREATE_ALARM,
        &words(&[ALARM.0, sync::CA_COUNTER, COUNTER_ZERO.0]),
    );
    assert_no_error(&create_alarm, "CreateAlarm naming None");

    let change_alarm = sync_access_request(
        &mut fixture,
        sync::CHANGE_ALARM,
        &words(&[ALARM.0, sync::CA_COUNTER, COUNTER_ZERO.0]),
    );
    assert_no_error(&change_alarm, "ChangeAlarm naming None");

    let recorded = calls.lock().unwrap();
    let resource_calls = recorded
        .iter()
        .filter(|call| call.hook == ExtensionHook::Resource)
        .collect::<Vec<_>>();
    assert_eq!(resource_calls.len(), 2);
    assert_recorded_resource(
        resource_calls[0],
        ResourceTarget {
            id: ALARM,
            kind: ResourceKind::SyncAlarm,
            creation: Some(Creation {
                parent: None,
                owner: CLIENT,
            }),
            owner: None,
        },
        AccessMode::CREATE,
        sync::CREATE_ALARM,
        None,
        None,
    );
    assert_recorded_resource(
        resource_calls[1],
        ResourceTarget {
            id: ALARM,
            kind: ResourceKind::SyncAlarm,
            creation: None,
            owner: None,
        },
        AccessMode::WRITE,
        sync::CHANGE_ALARM,
        Some(CLIENT),
        Some(CLIENT),
    );
    assert!(
        resource_calls
            .iter()
            .all(|call| { call.target.is_none_or(|target| target.id != COUNTER_ZERO) })
    );
}
