use super::{
    extensions::{ExtensionHook, RecordedExtensionCheck, RecordingExtensionPolicy},
    *,
};
use crate::access::{
    AccessMode, Creation, Decision, DenyError, RequestOpcodes, ResourceKind, ResourceTarget,
};

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
