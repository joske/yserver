use super::{
    extensions::{ExtensionHook, RecordedExtensionCheck, RecordingExtensionPolicy},
    *,
};
use crate::{
    access::{Decision, DenyError, EventDescription, EventSubject, RequestOpcodes},
    backend::recording::RecordingBackend,
};
use std::{
    collections::HashMap,
    os::unix::net::UnixStream,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

pub(super) const NOTIFY_SENDER: ClientId = ClientId(1);
pub(super) const NOTIFY_RECEIVER_DENIED: ClientId = ClientId(2);
pub(super) const NOTIFY_RECEIVER_ALLOWED: ClientId = ClientId(3);
pub(super) const NOTIFY_WINDOW: ResourceId = ResourceId(0x0051_0001);
pub(super) const PROPERTY_CHANGE_MASK: u32 = 1 << 22;

pub(super) fn literal_event_header(
    event_type: u8,
    detail: u8,
    sequence: u16,
    order: ClientByteOrder,
) -> Vec<u8> {
    let mut event = vec![event_type, detail];
    append_event_u16(&mut event, order, sequence);
    event
}

pub(super) fn append_event_u16(event: &mut Vec<u8>, order: ClientByteOrder, value: u16) {
    match order {
        ClientByteOrder::LittleEndian => event.extend_from_slice(&value.to_le_bytes()),
        ClientByteOrder::BigEndian => event.extend_from_slice(&value.to_be_bytes()),
    }
}

pub(super) fn append_event_u32(event: &mut Vec<u8>, order: ClientByteOrder, value: u32) {
    match order {
        ClientByteOrder::LittleEndian => event.extend_from_slice(&value.to_le_bytes()),
        ClientByteOrder::BigEndian => event.extend_from_slice(&value.to_be_bytes()),
    }
}

pub(super) fn finish_literal_event(mut event: Vec<u8>) -> Vec<u8> {
    assert!(event.len() <= 32, "core event fields fit one frame");
    event.resize(32, 0);
    event
}

pub(super) struct NotifyFixture {
    pub state: ServerState,
    pub backend: RecordingBackend,
    pub peers: HashMap<u32, UnixStream>,
}

impl NotifyFixture {
    pub(super) fn new() -> Self {
        let mut state = ServerState::new();
        let peers = [
            NOTIFY_SENDER,
            NOTIFY_RECEIVER_DENIED,
            NOTIFY_RECEIVER_ALLOWED,
        ]
        .into_iter()
        .map(|client| (client.0, install_client(&mut state, client.0)))
        .collect();
        Self {
            state,
            backend: RecordingBackend::new(),
            peers,
        }
    }

    pub(super) fn request(
        &mut self,
        client: ClientId,
        sequence: SequenceNumber,
        header: RequestHeader,
        body: &[u8],
    ) -> io::Result<RequestOutcome> {
        process_request(
            &mut self.state,
            &mut self.backend,
            client,
            sequence,
            header,
            body,
            None,
        )
    }

    pub(super) fn drain(&mut self, client: ClientId) -> Vec<u8> {
        let peer = self.peers.get_mut(&client.0).expect("fixture client peer");
        read_all_or_buffered(&mut self.state, client.0, peer)
    }

    pub(super) fn install_policy(
        &mut self,
        decide: impl Fn(usize, &RecordedExtensionCheck) -> Decision + Send + Sync + 'static,
    ) -> Arc<Mutex<Vec<RecordedExtensionCheck>>> {
        let calls = Arc::new(Mutex::new(Vec::new()));
        self.state
            .install_access_policy(Arc::new(RecordingExtensionPolicy::custom(
                Arc::clone(&calls),
                "",
                decide,
            )));
        calls
    }
}

pub(super) fn property_change_body(window: ResourceId, property: AtomId, data: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(20 + data.len().div_ceil(4) * 4);
    body.extend_from_slice(&window.0.to_le_bytes());
    body.extend_from_slice(&property.0.to_le_bytes());
    body.extend_from_slice(&31_u32.to_le_bytes()); // STRING
    body.extend_from_slice(&[8, 0, 0, 0]);
    body.extend_from_slice(&u32::try_from(data.len()).unwrap().to_le_bytes());
    body.extend_from_slice(data);
    while !body.len().is_multiple_of(4) {
        body.push(0);
    }
    body
}

pub(super) fn select_property_change(fixture: &mut NotifyFixture) {
    fixture.state.resources.create_window(
        NOTIFY_RECEIVER_DENIED,
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: NOTIFY_WINDOW,
            parent: crate::resources::ROOT_WINDOW,
            width: 100,
            height: 100,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    for client in [NOTIFY_RECEIVER_DENIED, NOTIFY_RECEIVER_ALLOWED] {
        fixture
            .state
            .clients
            .get_mut(&client.0)
            .expect("fixture client")
            .event_masks
            .insert(NOTIFY_WINDOW, PROPERTY_CHANGE_MASK);
    }
    fixture
        .state
        .clients
        .get_mut(&NOTIFY_RECEIVER_DENIED.0)
        .expect("denied receiver")
        .byte_order = ClientByteOrder::BigEndian;
    fixture
        .state
        .clients
        .get_mut(&NOTIFY_RECEIVER_DENIED.0)
        .expect("denied receiver")
        .last_sequence
        .store(0x1234, std::sync::atomic::Ordering::Relaxed);
    fixture
        .state
        .clients
        .get_mut(&NOTIFY_RECEIVER_ALLOWED.0)
        .expect("allowed receiver")
        .byte_order = ClientByteOrder::LittleEndian;
    fixture
        .state
        .clients
        .get_mut(&NOTIFY_RECEIVER_ALLOWED.0)
        .expect("allowed receiver")
        .last_sequence
        .store(0x5678, std::sync::atomic::Ordering::Relaxed);
    // Fix PropertyNotify's timestamp at zero for literal wire assertions.
    fixture.state.start_instant = std::time::Instant::now() + std::time::Duration::from_secs(30);
}

pub(super) fn run_property_change(
    fixture: &mut NotifyFixture,
    sequence: u16,
    property: AtomId,
    data: &[u8],
) -> RequestOutcome {
    fixture
        .request(
            NOTIFY_SENDER,
            SequenceNumber(sequence),
            RequestHeader {
                opcode: 18,
                data: 0,
                length_units: u32::try_from(
                    (4 + property_change_body(NOTIFY_WINDOW, property, data).len()) / 4,
                )
                .unwrap(),
            },
            &property_change_body(NOTIFY_WINDOW, property, data),
        )
        .expect("ChangeProperty through production dispatcher")
}

pub(super) fn literal_property_notify(
    order: ClientByteOrder,
    sequence: u16,
    property: AtomId,
) -> Vec<u8> {
    let mut bytes = vec![28, 0]; // PropertyNotify, detail
    match order {
        ClientByteOrder::LittleEndian => {
            bytes.extend_from_slice(&sequence.to_le_bytes());
            bytes.extend_from_slice(&NOTIFY_WINDOW.0.to_le_bytes());
            bytes.extend_from_slice(&property.0.to_le_bytes());
        }
        ClientByteOrder::BigEndian => {
            bytes.extend_from_slice(&sequence.to_be_bytes());
            bytes.extend_from_slice(&NOTIFY_WINDOW.0.to_be_bytes());
            bytes.extend_from_slice(&property.0.to_be_bytes());
        }
    }
    bytes.extend_from_slice(&[0; 20]); // timestamp, state, and pad
    bytes
}

pub(super) fn request_opcodes(major: u8, minor: u8) -> Option<RequestOpcodes> {
    Some(RequestOpcodes { major, minor })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ExpectedReceive {
    pub(super) client: ClientId,
    pub(super) request: Option<RequestOpcodes>,
    pub(super) subject: EventSubject,
    pub(super) events: Vec<EventDescription>,
}

#[derive(Clone, Copy)]
pub(super) enum CallOrder {
    Total,
    PerClient,
    Multiset,
}

pub(super) struct ReceiveCase<'a, C> {
    pub(super) name: &'static str,
    pub(super) build: &'a dyn Fn() -> (NotifyFixture, C),
    pub(super) act: &'a dyn Fn(&mut NotifyFixture, &C),
    pub(super) expect: &'a dyn Fn(&C) -> CaseExpect,
}

pub(super) type AfterDeny = Box<dyn Fn(&NotifyFixture, &NotifyFixture)>;

pub(super) struct CaseExpect {
    pub(super) streams: Vec<Stream>,
    pub(super) receives: Vec<ExpectedReceive>,
    pub(super) deny_receives: Option<Vec<ExpectedReceive>>,
    pub(super) order: CallOrder,
    pub(super) denied_calls: Vec<ExpectedReceive>,
    pub(super) deny_first_only: bool,
    // Keep a deny-mode probe ahead of allow-log checks when the old test did.
    pub(super) deny_before_allow: bool,
    pub(super) forbidden_hooks: &'static [ExtensionHook],
    pub(super) after_deny: Option<AfterDeny>,
}

impl CaseExpect {
    pub(super) fn new(
        streams: Vec<Stream>,
        receives: Vec<ExpectedReceive>,
        order: CallOrder,
        denied_calls: Vec<ExpectedReceive>,
    ) -> Self {
        Self {
            streams,
            receives,
            deny_receives: None,
            order,
            denied_calls,
            deny_first_only: false,
            deny_before_allow: false,
            forbidden_hooks: &[ExtensionHook::Send],
            after_deny: None,
        }
    }
}

pub(super) struct Stream {
    pub(super) client: ClientId,
    pub(super) bytes: Vec<u8>,
    pub(super) denied: Option<Vec<u8>>,
}

impl Stream {
    pub(super) fn unchanged(client: ClientId, bytes: Vec<u8>) -> Self {
        Self {
            client,
            bytes,
            denied: None,
        }
    }
}

pub(super) fn receive_checks(
    calls: &Arc<Mutex<Vec<RecordedExtensionCheck>>>,
) -> Vec<RecordedExtensionCheck> {
    calls
        .lock()
        .unwrap()
        .iter()
        .filter(|call| call.hook == ExtensionHook::Receive)
        .cloned()
        .collect()
}

fn receive_trace(name: &str, mode: &str, calls: &[RecordedExtensionCheck]) -> Vec<ExpectedReceive> {
    calls
        .iter()
        .filter(|call| call.hook == ExtensionHook::Receive)
        .map(|call| ExpectedReceive {
            client: call.client,
            request: call.request,
            subject: call
                .receive
                .unwrap_or_else(|| panic!("{name} [{mode}]: Receive call has no subject")),
            events: call
                .events
                .clone()
                .unwrap_or_else(|| panic!("{name} [{mode}]: Receive call has no event list")),
        })
        .collect()
}

fn receive_traces_equal(
    expected: &[ExpectedReceive],
    actual: &[ExpectedReceive],
    order: &CallOrder,
) -> bool {
    match order {
        CallOrder::Total => expected == actual,
        CallOrder::PerClient => {
            if expected.len() != actual.len() {
                return false;
            }
            let mut clients = Vec::new();
            for check in expected.iter().chain(actual) {
                if !clients.contains(&check.client) {
                    clients.push(check.client);
                }
            }
            clients.into_iter().all(|client| {
                let expected_client = expected
                    .iter()
                    .filter(|check| check.client == client)
                    .collect::<Vec<_>>();
                let actual_client = actual
                    .iter()
                    .filter(|check| check.client == client)
                    .collect::<Vec<_>>();
                expected_client == actual_client
            })
        }
        CallOrder::Multiset => {
            if expected.len() != actual.len() {
                return false;
            }
            let mut remaining = actual.to_vec();
            for check in expected {
                let Some(index) = remaining.iter().position(|candidate| candidate == check) else {
                    return false;
                };
                remaining.remove(index);
            }
            remaining.is_empty()
        }
    }
}

fn assert_receive_traces(
    name: &str,
    mode: &str,
    order: CallOrder,
    expected: &[ExpectedReceive],
    actual: &[ExpectedReceive],
    label: &str,
) {
    assert!(
        receive_traces_equal(expected, actual, &order),
        "{name} [{mode}]: {label} differ; expected {expected:?}, got {actual:?}"
    );
}

fn expected_receive_matches(expected: &ExpectedReceive, call: &RecordedExtensionCheck) -> bool {
    call.hook == ExtensionHook::Receive
        && call.client == expected.client
        && call.request == expected.request
        && call.receive == Some(expected.subject)
        && call.events.as_deref() == Some(expected.events.as_slice())
}

fn assert_case(name: &str, mode: &str, condition: bool, message: &str) {
    assert!(condition, "{name} [{mode}]: {message}");
}

fn collect_streams(
    name: &str,
    mode: &str,
    fixture: &mut NotifyFixture,
    expectations: &[Stream],
) -> Vec<(ClientId, Vec<u8>)> {
    let expected_clients = expectations
        .iter()
        .map(|stream| stream.client)
        .collect::<Vec<_>>();
    assert_case(
        name,
        mode,
        expected_clients.len() == fixture.peers.len()
            && fixture
                .peers
                .keys()
                .all(|id| expected_clients.contains(&ClientId(*id))),
        "every fixture peer must have exactly one Stream row",
    );
    let mut actual_streams = Vec::with_capacity(expectations.len());
    for expected in expectations {
        let actual = fixture.drain(expected.client);
        actual_streams.push((expected.client, actual));
    }
    actual_streams
}

fn assert_streams(
    name: &str,
    mode: &str,
    expectations: &[Stream],
    actual_streams: &[(ClientId, Vec<u8>)],
    denied_mode: bool,
) {
    assert_case(
        name,
        mode,
        expectations.len() == actual_streams.len(),
        "each expected Stream has one captured peer stream",
    );
    for (expected, (client, actual)) in expectations.iter().zip(actual_streams) {
        assert_case(
            name,
            mode,
            expected.client == *client,
            "captured peer order differs from Stream rows",
        );
        let bytes = if denied_mode {
            expected.denied.as_ref().unwrap_or(&expected.bytes)
        } else {
            &expected.bytes
        };
        assert_eq!(
            actual.as_slice(),
            bytes.as_slice(),
            "{name} [{mode}]: unexpected bytes for client {}",
            expected.client.0
        );
    }
}

fn assert_healthy_fixture(name: &str, mode: &str, fixture: &NotifyFixture) {
    assert_case(
        name,
        mode,
        fixture
            .peers
            .keys()
            .all(|id| fixture.state.clients.contains_key(id)),
        "notification delivery must keep every fixture client connected",
    );
    assert_case(
        name,
        mode,
        crate::core_loop::client_io::failed_writers(&fixture.state.clients).is_empty(),
        "notification delivery must not queue a writer for disconnect",
    );
}

fn assert_forbidden_hooks(
    name: &str,
    mode: &str,
    calls: &[RecordedExtensionCheck],
    forbidden: &[ExtensionHook],
) {
    assert_case(
        name,
        mode,
        calls.iter().all(|call| !forbidden.contains(&call.hook)),
        "a forbidden extension hook was called",
    );
}

fn assert_allow_case(
    name: &str,
    fixture: &NotifyFixture,
    expectations: &CaseExpect,
    streams: &[(ClientId, Vec<u8>)],
    calls: &[RecordedExtensionCheck],
) {
    assert_healthy_fixture(name, "allow-all", fixture);
    assert_streams(name, "allow-all", &expectations.streams, streams, false);
    let receives = receive_trace(name, "allow-all", calls);
    assert_receive_traces(
        name,
        "allow-all",
        expectations.order,
        &expectations.receives,
        &receives,
        "Receive trace",
    );
    assert_forbidden_hooks(name, "allow-all", calls, expectations.forbidden_hooks);
}

pub(super) fn run_with_context(name: &str, mode: &str, action: impl FnOnce()) {
    if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(action)) {
        let message = payload
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| payload.downcast_ref::<&str>().copied())
            .unwrap_or("action panicked with a non-string payload");
        panic!("{name} [{mode}]: {message}");
    }
}

pub(super) fn check_receive_case<C>(case: &ReceiveCase<'_, C>) {
    check_receive_case_with_deny(case, None);
}

fn check_receive_case_with_deny<C>(case: &ReceiveCase<'_, C>, test_deny: Option<ExpectedReceive>) {
    let (mut no_policy, no_policy_context) = (case.build)();
    let no_policy_expect = (case.expect)(&no_policy_context);
    run_with_context(case.name, "no policy", || {
        (case.act)(&mut no_policy, &no_policy_context);
    });
    assert_healthy_fixture(case.name, "no policy", &no_policy);
    let no_policy_streams = collect_streams(
        case.name,
        "no policy",
        &mut no_policy,
        &no_policy_expect.streams,
    );
    assert_streams(
        case.name,
        "no policy",
        &no_policy_expect.streams,
        &no_policy_streams,
        false,
    );

    let (mut allow_all, allow_context) = (case.build)();
    let allow_expect = (case.expect)(&allow_context);
    let allow_calls = allow_all.install_policy(|_, _| Decision::Allow);
    run_with_context(case.name, "allow-all", || {
        (case.act)(&mut allow_all, &allow_context);
    });
    let allow_streams = collect_streams(
        case.name,
        "allow-all",
        &mut allow_all,
        &allow_expect.streams,
    );
    let allow_log = allow_calls.lock().unwrap().clone();
    if !allow_expect.deny_before_allow {
        assert_allow_case(
            case.name,
            &allow_all,
            &allow_expect,
            &allow_streams,
            &allow_log,
        );
    }

    let (mut denied, deny_context) = (case.build)();
    let deny_expect = (case.expect)(&deny_context);
    assert_case(
        case.name,
        "deny",
        !deny_expect.denied_calls.is_empty(),
        "a denial row must name at least one Receive call",
    );
    let denied_actual = Arc::new(Mutex::new(Vec::<ExpectedReceive>::new()));
    let denied_actual_for_policy = Arc::clone(&denied_actual);
    let deny_once = AtomicUsize::new(0);
    let denied_calls = deny_expect.denied_calls.clone();
    let deny_first_only = deny_expect.deny_first_only;
    let deny_log = denied.install_policy(move |_, call| {
        let matches = if let Some(test_deny) = &test_deny {
            expected_receive_matches(test_deny, call)
        } else {
            denied_calls
                .iter()
                .any(|expected| expected_receive_matches(expected, call))
        };
        if !matches || (deny_first_only && deny_once.fetch_add(1, Ordering::Relaxed) != 0) {
            return Decision::Allow;
        }
        denied_actual_for_policy
            .lock()
            .unwrap()
            .push(ExpectedReceive {
                client: call.client,
                request: call.request,
                subject: call.receive.expect("matched Receive call has subject"),
                events: call
                    .events
                    .clone()
                    .expect("matched Receive call has events"),
            });
        Decision::Deny(DenyError {
            code: yserver_protocol::x11::error::BAD_ACCESS,
            value: 0,
        })
    });
    run_with_context(case.name, "deny", || {
        (case.act)(&mut denied, &deny_context);
    });
    assert_healthy_fixture(case.name, "deny", &denied);
    let deny_log = deny_log.lock().unwrap().clone();
    let actual_denied = denied_actual.lock().unwrap().clone();
    let deny_streams = collect_streams(case.name, "deny", &mut denied, &deny_expect.streams);
    assert_streams(case.name, "deny", &deny_expect.streams, &deny_streams, true);
    assert_case(
        case.name,
        "deny",
        !actual_denied.is_empty(),
        "deny policy matched no Receive call",
    );
    assert_receive_traces(
        case.name,
        "deny",
        deny_expect.order,
        &deny_expect.denied_calls,
        &actual_denied,
        "denied Receive calls",
    );
    let deny_receives = receive_trace(case.name, "deny", &deny_log);
    assert_receive_traces(
        case.name,
        "deny",
        deny_expect.order,
        deny_expect
            .deny_receives
            .as_deref()
            .unwrap_or(&deny_expect.receives),
        &deny_receives,
        "Receive trace",
    );
    assert_forbidden_hooks(case.name, "deny", &deny_log, deny_expect.forbidden_hooks);
    for denied_call in &deny_expect.denied_calls {
        let allow_bytes = allow_streams
            .iter()
            .find(|(client, _)| *client == denied_call.client)
            .map(|(_, bytes)| bytes);
        let deny_bytes = deny_streams
            .iter()
            .find(|(client, _)| *client == denied_call.client)
            .map(|(_, bytes)| bytes);
        assert_case(
            case.name,
            "deny",
            allow_bytes.is_some() && deny_bytes.is_some() && allow_bytes != deny_bytes,
            "each denied recipient must lose bytes compared with allow-all",
        );
    }
    if let Some(after_deny) = deny_expect.after_deny {
        run_with_context(case.name, "deny", || after_deny(&allow_all, &denied));
    }
    if allow_expect.deny_before_allow {
        assert_allow_case(
            case.name,
            &allow_all,
            &allow_expect,
            &allow_streams,
            &allow_log,
        );
    }
}

#[cfg(test)]
mod receive_case_tests {
    use super::*;

    #[test]
    #[should_panic(expected = "deny policy matched no Receive call")]
    fn receive_case_rejects_a_deny_that_matches_nothing() {
        // mutation: allow a denial row to pass without matching any Receive call.
        let build = || {
            let (mut fixture, device_id) = super::super::access_xi_notify::hotplug_fixture();
            super::super::access_xi_notify::select_hierarchy_on_root_and_child(
                &mut fixture,
                NOTIFY_RECEIVER_DENIED,
            );
            (fixture, device_id)
        };
        let act = |fixture: &mut NotifyFixture, device_id: &u16| {
            super::super::access_xi_notify::disable_device(fixture, *device_id);
        };
        let expect = |_: &u16| {
            let event = crate::access::EventDescription::extension(35, 137, 11);
            let receives = vec![
                ExpectedReceive {
                    client: NOTIFY_RECEIVER_DENIED,
                    request: request_opcodes(137, 57),
                    subject: crate::access::EventSubject::Window(crate::resources::ROOT_WINDOW),
                    events: vec![event],
                },
                ExpectedReceive {
                    client: NOTIFY_RECEIVER_DENIED,
                    request: request_opcodes(137, 57),
                    subject: crate::access::EventSubject::Window(
                        super::super::access_xi_notify::CHILD_WINDOW,
                    ),
                    events: vec![event],
                },
            ];
            CaseExpect::new(
                vec![
                    Stream::unchanged(NOTIFY_SENDER, Vec::new()),
                    Stream {
                        client: NOTIFY_RECEIVER_DENIED,
                        bytes: [
                            super::super::access_xi_notify::literal_hierarchy_disabled(4),
                            super::super::access_xi_notify::literal_hierarchy_disabled(4),
                        ]
                        .concat(),
                        denied: None,
                    },
                    Stream::unchanged(NOTIFY_RECEIVER_ALLOWED, Vec::new()),
                ],
                receives,
                CallOrder::Multiset,
                vec![ExpectedReceive {
                    client: NOTIFY_RECEIVER_DENIED,
                    request: request_opcodes(137, 57),
                    subject: crate::access::EventSubject::Global,
                    events: vec![event],
                }],
            )
        };
        let case = ReceiveCase {
            name: "unmatched denial self-test",
            build: &build,
            act: &act,
            expect: &expect,
        };

        check_receive_case(&case);
    }

    #[test]
    #[should_panic(expected = "denied Receive calls")]
    fn receive_case_distinguishes_equal_root_and_child_copies() {
        // mutation: stop checking which Receive calls the runner denied.
        let root = ExpectedReceive {
            client: NOTIFY_RECEIVER_DENIED,
            request: request_opcodes(137, 57),
            subject: crate::access::EventSubject::Window(crate::resources::ROOT_WINDOW),
            events: vec![crate::access::EventDescription::extension(35, 137, 11)],
        };
        let child = ExpectedReceive {
            subject: crate::access::EventSubject::Window(
                super::super::access_xi_notify::CHILD_WINDOW,
            ),
            ..root.clone()
        };
        let build = || {
            let (mut fixture, device_id) = super::super::access_xi_notify::hotplug_fixture();
            super::super::access_xi_notify::select_hierarchy_on_root_and_child(
                &mut fixture,
                NOTIFY_RECEIVER_DENIED,
            );
            (fixture, device_id)
        };
        let act = |fixture: &mut NotifyFixture, device_id: &u16| {
            super::super::access_xi_notify::disable_device(fixture, *device_id);
        };
        let expect = |_: &u16| {
            CaseExpect::new(
                vec![
                    Stream::unchanged(NOTIFY_SENDER, Vec::new()),
                    Stream {
                        client: NOTIFY_RECEIVER_DENIED,
                        bytes: [
                            super::super::access_xi_notify::literal_hierarchy_disabled(4),
                            super::super::access_xi_notify::literal_hierarchy_disabled(4),
                        ]
                        .concat(),
                        denied: Some(super::super::access_xi_notify::literal_hierarchy_disabled(
                            4,
                        )),
                    },
                    Stream::unchanged(NOTIFY_RECEIVER_ALLOWED, Vec::new()),
                ],
                vec![root.clone(), child.clone()],
                CallOrder::Multiset,
                vec![root.clone()],
            )
        };
        let case = ReceiveCase {
            name: "root-versus-child denied-call self-test",
            build: &build,
            act: &act,
            expect: &expect,
        };

        // The real disable-device path emits equal-byte root and child copies.
        // Force only the child copy to be denied while the row expects the root.
        check_receive_case_with_deny(&case, Some(child.clone()));
    }
}
