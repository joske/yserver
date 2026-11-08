use super::{
    extensions::{ExtensionHook, RecordedExtensionCheck, RecordingExtensionPolicy},
    *,
};
use crate::{
    access::{AccessMode, Decision, DenyError, RequestOpcodes, ResourceKind, ResourceTarget},
    properties::{PropertyFormat, PropertyValue},
};
use yserver_protocol::x11::AtomId;

const CLIENT: ClientId = ClientId(1);
const WINDOW: ResourceId = ResourceId(0x0059_0001);
const DENY_ERROR: DenyError = DenyError {
    code: yserver_protocol::x11::error::BAD_VALUE,
    value: 0x55,
};

#[derive(Clone, Debug, Eq, PartialEq)]
enum AccessCall {
    Resource {
        client: ClientId,
        request: Option<RequestOpcodes>,
        target: ResourceTarget,
        mode: AccessMode,
        owner: Option<ClientId>,
        target_owner: Option<ClientId>,
    },
    Property {
        client: ClientId,
        request: Option<RequestOpcodes>,
        window: ResourceId,
        property: AtomId,
        mode: AccessMode,
    },
}

struct Fixture {
    state: ServerState,
    backend: RecordingBackend,
    peer: UnixStream,
    window: ResourceId,
    type_atom: AtomId,
    properties: Vec<AtomId>,
}

impl Fixture {
    fn new(property_count: usize) -> Self {
        let mut state = ServerState::new();
        let peer = install_client(&mut state, CLIENT.0);
        create_root_child(&mut state, WINDOW.0);
        let type_atom = state.atoms.intern("TASK9_PROPERTY_TYPE", false);
        let properties = (0..property_count)
            .map(|i| state.atoms.intern(&format!("TASK9_PROPERTY_{i}"), false))
            .collect::<Vec<_>>();
        for (index, property) in properties.iter().copied().enumerate() {
            state.resources.set_window_property(
                WINDOW,
                property,
                PropertyValue {
                    r#type: type_atom,
                    format: PropertyFormat::F8,
                    data: vec![u8::try_from(index + 1).unwrap()],
                },
            );
        }
        Self {
            state,
            backend: RecordingBackend::new(),
            peer,
            window: WINDOW,
            type_atom,
            properties,
        }
    }

    fn atom(&mut self, name: &str) -> AtomId {
        self.state.atoms.intern(name, false)
    }

    fn request(&mut self, sequence: u16, opcode: u8, data: u8, body: &[u8]) -> Vec<u8> {
        process_request(
            &mut self.state,
            &mut self.backend,
            CLIENT,
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
        read_all_available(&mut self.peer)
    }

    fn install_policy(
        &mut self,
        deny_at: Option<usize>,
    ) -> Arc<Mutex<Vec<RecordedExtensionCheck>>> {
        self.install_policy_with_deny(deny_at, false)
    }

    fn install_policy_with_deny(
        &mut self,
        deny_at: Option<usize>,
        deny_all: bool,
    ) -> Arc<Mutex<Vec<RecordedExtensionCheck>>> {
        let calls = Arc::new(Mutex::new(Vec::new()));
        self.state
            .install_access_policy(Arc::new(RecordingExtensionPolicy::custom(
                Arc::clone(&calls),
                "",
                move |index, _| {
                    if deny_all || deny_at == Some(index) {
                        Decision::Deny(DENY_ERROR)
                    } else {
                        Decision::Allow
                    }
                },
            )));
        calls
    }

    fn property_snapshot(&self) -> HashMap<AtomId, PropertyValue> {
        self.state
            .resources
            .window(self.window)
            .unwrap()
            .properties
            .clone()
    }
}

fn words(values: &[u32]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

fn resource_call(opcode: u8, mode: AccessMode) -> AccessCall {
    AccessCall::Resource {
        client: CLIENT,
        request: Some(RequestOpcodes {
            major: opcode,
            minor: 0,
        }),
        target: ResourceTarget {
            owner: None,
            id: WINDOW,
            kind: ResourceKind::Window,
            creation: None,
        },
        mode,
        owner: Some(CLIENT),
        target_owner: Some(CLIENT),
    }
}

fn property_call(opcode: u8, property: AtomId, mode: AccessMode) -> AccessCall {
    AccessCall::Property {
        client: CLIENT,
        request: Some(RequestOpcodes {
            major: opcode,
            minor: 0,
        }),
        window: WINDOW,
        property,
        mode,
    }
}

fn access_calls(calls: &[RecordedExtensionCheck]) -> Vec<AccessCall> {
    calls
        .iter()
        .map(|call| match call.hook {
            ExtensionHook::Resource => AccessCall::Resource {
                client: call.client,
                request: call.request,
                target: call.target.expect("resource check target"),
                mode: call.mode.expect("resource check mode"),
                owner: call.owner,
                target_owner: call.target_owner,
            },
            ExtensionHook::Property => {
                let (window, property) = call.property.expect("property check target");
                AccessCall::Property {
                    client: call.client,
                    request: call.request,
                    window,
                    property,
                    mode: call.mode.expect("property check mode"),
                }
            }
            _ => unreachable!("property test only records resource and property checks"),
        })
        .collect()
}

fn change_body(fixture: &Fixture, property: AtomId) -> Vec<u8> {
    let mut body = words(&[fixture.window.0, property.0, fixture.type_atom.0]);
    body.extend_from_slice(&[8, 0, 0, 0]);
    body.extend_from_slice(&1u32.to_le_bytes());
    body.push(0x7f);
    body.resize(body.len().div_ceil(4) * 4, 0);
    body
}

fn get_body(fixture: &Fixture, property: AtomId) -> Vec<u8> {
    words(&[fixture.window.0, property.0, 0, 0, 100])
}

fn assert_policy_error(bytes: &[u8], opcode: u8, sequence: u16) {
    let mut expected = vec![0, DENY_ERROR.code];
    expected.extend_from_slice(&sequence.to_le_bytes());
    expected.extend_from_slice(&DENY_ERROR.value.to_le_bytes());
    expected.extend_from_slice(&0u16.to_le_bytes());
    expected.push(opcode);
    expected.extend_from_slice(&[0; 21]);
    assert_eq!(bytes, expected, "denial for core request {opcode}");
}

#[derive(Clone, Copy, Debug)]
enum AccessCase {
    Replace,
    Append,
    Prepend,
    AbsentAppend,
    Delete,
    Get,
    GetDelete,
    ListWindow,
    RotateWindow,
}

type AccessRow = (u8, u8, Vec<u8>, Vec<AccessCall>);
type AccessRowData = (Vec<u8>, Vec<AccessCall>);
fn access_row((opcode, data): (u8, u8), (body, expected): AccessRowData) -> AccessRow {
    (opcode, data, body, expected)
}

fn change_property_checks(property: AtomId, property_mode: AccessMode) -> Vec<AccessCall> {
    vec![
        resource_call(18, AccessMode::SET_PROP),
        property_call(18, property, property_mode),
    ]
}

fn change_property_row(
    fixture: &Fixture,
    data: u8,
    property: AtomId,
    property_mode: AccessMode,
) -> AccessRow {
    let body = change_body(fixture, property);
    let expected = change_property_checks(property, property_mode);
    access_row((18, data), (body, expected))
}

fn existing_property_change_row(
    fixture: &Fixture,
    data: u8,
    property_mode: AccessMode,
) -> AccessRow {
    change_property_row(fixture, data, fixture.properties[0], property_mode)
}

fn absent_append_row(fixture: &mut Fixture) -> AccessRow {
    let property = fixture.atom("TASK9_ABSENT_PROPERTY");
    change_property_row(fixture, 2, property, AccessMode::CREATE | AccessMode::WRITE)
}

fn delete_property_row(fixture: &Fixture) -> AccessRow {
    let property = fixture.properties[0];
    access_row((19, 0), delete_property_data(fixture, property))
}

fn delete_property_data(fixture: &Fixture, property: AtomId) -> AccessRowData {
    (
        words(&[fixture.window.0, property.0]),
        delete_property_checks(property),
    )
}

fn delete_property_checks(property: AtomId) -> Vec<AccessCall> {
    vec![
        resource_call(19, AccessMode::SET_PROP),
        property_call(19, property, AccessMode::DESTROY),
    ]
}

fn get_property_checks(property: AtomId) -> Vec<AccessCall> {
    vec![
        resource_call(20, AccessMode::GET_PROP),
        property_call(20, property, AccessMode::READ),
    ]
}

fn get_property_data(fixture: &Fixture, property: AtomId) -> AccessRowData {
    (get_body(fixture, property), get_property_checks(property))
}

fn get_property_delete_checks(property: AtomId) -> Vec<AccessCall> {
    vec![
        resource_call(20, AccessMode::GET_PROP | AccessMode::SET_PROP),
        property_call(20, property, AccessMode::READ | AccessMode::DESTROY),
    ]
}

fn get_property_row(fixture: &Fixture) -> AccessRow {
    let property = fixture.properties[0];
    access_row((20, 0), get_property_data(fixture, property))
}

fn get_property_delete_row(fixture: &Fixture) -> AccessRow {
    let property = fixture.properties[0];
    access_row((20, 1), get_property_delete_data(fixture, property))
}

fn get_property_delete_data(fixture: &Fixture, property: AtomId) -> AccessRowData {
    (
        get_body(fixture, property),
        get_property_delete_checks(property),
    )
}

fn resource_only_row(opcode: u8, data: u8, body: Vec<u8>, mode: AccessMode) -> AccessRow {
    access_row((opcode, data), (body, vec![resource_call(opcode, mode)]))
}

fn list_properties_row(fixture: &Fixture) -> AccessRow {
    resource_only_row(21, 0, words(&[fixture.window.0]), AccessMode::LIST_PROP)
}

fn rotate_properties_row(fixture: &Fixture) -> AccessRow {
    resource_only_row(114, 0, words(&[fixture.window.0, 0]), AccessMode::SET_PROP)
}

fn run_access_case(fixture: &mut Fixture, case: AccessCase) -> AccessRow {
    match case {
        AccessCase::Replace => existing_property_change_row(fixture, 0, AccessMode::WRITE),
        AccessCase::Append => existing_property_change_row(fixture, 2, AccessMode::BLEND),
        AccessCase::Prepend => existing_property_change_row(fixture, 1, AccessMode::BLEND),
        AccessCase::AbsentAppend => absent_append_row(fixture),
        AccessCase::Delete => delete_property_row(fixture),
        AccessCase::Get => get_property_row(fixture),
        AccessCase::GetDelete => get_property_delete_row(fixture),
        AccessCase::ListWindow => list_properties_row(fixture),
        AccessCase::RotateWindow => rotate_properties_row(fixture),
    }
}

#[test]
fn property_request_access_modes_are_asked_and_denied_before_effects() {
    // Mutation: omit the Append property's BLEND check; the exact call row and denied Append result
    // must fail.
    let cases = [
        AccessCase::Replace,
        AccessCase::Append,
        AccessCase::Prepend,
        AccessCase::AbsentAppend,
        AccessCase::Delete,
        AccessCase::Get,
        AccessCase::GetDelete,
        AccessCase::ListWindow,
        AccessCase::RotateWindow,
    ];
    for case in cases {
        let property_count = if matches!(
            case,
            AccessCase::ListWindow | AccessCase::RotateWindow | AccessCase::AbsentAppend
        ) {
            0
        } else {
            1
        };
        let mut allowed = Fixture::new(property_count);
        let (opcode, data, body, expected) = run_access_case(&mut allowed, case);
        let calls = allowed.install_policy(None);
        let reply = allowed.request(51, opcode, data, &body);
        assert!(
            reply.is_empty() || reply[0] != 0,
            "allowed {opcode} reply: {reply:?}"
        );
        assert_eq!(
            access_calls(&calls.lock().unwrap()),
            expected,
            "asked modes for {case:?}"
        );

        for deny_at in 0..expected.len() {
            let mut denied = Fixture::new(property_count);
            let (deny_opcode, deny_data, deny_body, deny_expected) =
                run_access_case(&mut denied, case);
            assert_eq!(deny_opcode, opcode);
            let before_properties = denied.property_snapshot();
            let before_backend = denied.backend.calls();
            let calls = denied.install_policy(Some(deny_at));
            let sequence = u16::try_from(60 + deny_at).unwrap();
            let reply = denied.request(sequence, opcode, deny_data, &deny_body);
            assert_policy_error(&reply, opcode, sequence);
            assert_eq!(
                denied.property_snapshot(),
                before_properties,
                "denied {opcode} state"
            );
            assert_eq!(
                denied.backend.calls(),
                before_backend,
                "denied {opcode} backend"
            );
            assert_eq!(
                access_calls(&calls.lock().unwrap()),
                deny_expected[..=deny_at],
                "denied {opcode} check order"
            );
        }
    }
}

#[test]
fn list_properties_omits_denied_property() {
    // Mutation: deny the whole request when one property is denied; the filtered success reply and
    // matching length fail.
    let mut fixture = Fixture::new(3);
    let order = fixture
        .state
        .resources
        .window(fixture.window)
        .unwrap()
        .properties
        .keys()
        .copied()
        .collect::<Vec<_>>();
    // Use the order the request handler enumerates, then deny its middle entry.
    let denied = order[1];
    let calls = fixture.install_policy(Some(2)); // window, first property, middle property
    let before_backend = fixture.backend.calls();
    let reply = fixture.request(71, 21, 0, &words(&[fixture.window.0]));

    assert_eq!(reply[0], 1, "ListProperties succeeds: {reply:?}");
    assert_eq!(reply.len(), 32 + 2 * 4);
    assert_eq!(u32::from_le_bytes(reply[4..8].try_into().unwrap()), 2);
    assert_eq!(u16::from_le_bytes(reply[8..10].try_into().unwrap()), 2);
    let listed = reply[32..]
        .chunks_exact(4)
        .map(|bytes| AtomId(u32::from_le_bytes(bytes.try_into().unwrap())))
        .collect::<Vec<_>>();
    assert_eq!(listed, [order[0], order[2]]);
    assert_eq!(fixture.backend.calls(), before_backend);
    assert_eq!(
        access_calls(&calls.lock().unwrap()),
        [
            resource_call(21, AccessMode::LIST_PROP),
            property_call(21, order[0], AccessMode::GET_ATTR),
            property_call(21, denied, AccessMode::GET_ATTR),
            property_call(21, order[2], AccessMode::GET_ATTR),
        ]
    );
}

#[test]
fn rotate_properties_denied_item_rotates_nothing() {
    // Mutation: skip per-item READ|WRITE denial; the state changes and PropertyNotify events make
    // this fail.
    let mut fixture = Fixture::new(3);
    let properties = fixture.properties.clone();
    let before_properties = fixture.property_snapshot();
    fixture
        .state
        .clients
        .get_mut(&CLIENT.0)
        .unwrap()
        .event_masks
        .insert(fixture.window, 0x0040_0000);
    let _ = read_all_available(&mut fixture.peer);
    let calls = fixture.install_policy(Some(3));
    // window then first, second and denied third property
    let before_backend = fixture.backend.calls();
    let mut body = words(&[fixture.window.0]);
    body.extend_from_slice(&3u16.to_le_bytes());
    body.extend_from_slice(&1u16.to_le_bytes());
    body.extend(
        properties
            .iter()
            .flat_map(|property| property.0.to_le_bytes()),
    );

    let reply = fixture.request(72, 114, 0, &body);
    assert_policy_error(&reply, 114, 72);
    assert_eq!(fixture.property_snapshot(), before_properties);
    assert_eq!(fixture.backend.calls(), before_backend);
    assert_eq!(
        access_calls(&calls.lock().unwrap()),
        [
            resource_call(114, AccessMode::SET_PROP),
            property_call(114, properties[0], AccessMode::READ | AccessMode::WRITE),
            property_call(114, properties[1], AccessMode::READ | AccessMode::WRITE),
            property_call(114, properties[2], AccessMode::READ | AccessMode::WRITE),
        ]
    );
}

#[test]
fn server_property_writes_are_not_checked() {
    // Mutation: route the server helper through check_property; the deny-all policy must still see
    // no calls.
    let mut fixture = Fixture::new(0);
    let property = fixture.atom("_XKB_RULES_NAMES");
    let calls = fixture.install_policy_with_deny(None, true);
    let value = PropertyValue {
        r#type: fixture.type_atom,
        format: PropertyFormat::F8,
        data: b"evdev\0pc105\0us\0".to_vec(),
    };

    change_property_as_server(
        &mut fixture.state,
        &mut fixture.backend,
        fixture.window,
        property,
        value.r#type,
        value.format,
        value.data.clone(),
    );

    assert_eq!(
        fixture
            .state
            .resources
            .window_property(fixture.window, property),
        Some(&value)
    );
    assert!(calls.lock().unwrap().is_empty());
}
