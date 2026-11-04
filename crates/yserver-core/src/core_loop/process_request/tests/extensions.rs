use super::*;
use crate::access::{AccessContext, AccessPolicy, Decision, DenyError, RequestOpcodes};
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Debug, Eq, PartialEq)]
struct RecordedExtensionCheck {
    client: ClientId,
    extension: String,
    request: Option<RequestOpcodes>,
    peer: Option<crate::peer::PeerIdentity>,
    admission: Option<crate::access::Admission>,
}

struct RecordingExtensionPolicy {
    decision: Decision,
    calls: Arc<Mutex<Vec<RecordedExtensionCheck>>>,
}

impl AccessPolicy for RecordingExtensionPolicy {
    fn extension_access(
        &self,
        ctx: &AccessContext<'_>,
        client: ClientId,
        extension: &str,
    ) -> Decision {
        self.calls.lock().unwrap().push(RecordedExtensionCheck {
            client,
            extension: extension.to_owned(),
            request: ctx.request(),
            peer: ctx.peer(client).copied(),
            admission: ctx.admission(client).cloned(),
        });
        self.decision
    }
}

struct NameSetDenier(HashSet<String>);

impl AccessPolicy for NameSetDenier {
    fn extension_access(
        &self,
        _ctx: &AccessContext<'_>,
        _client: ClientId,
        extension: &str,
    ) -> Decision {
        if self.0.contains(extension) {
            Decision::Deny(DenyError {
                code: x11::error::BAD_VALUE,
                value: 0x55,
            })
        } else {
            Decision::Allow
        }
    }
}

struct RevocableExtensionPolicy(Arc<AtomicBool>);

impl AccessPolicy for RevocableExtensionPolicy {
    fn extension_access(
        &self,
        _ctx: &AccessContext<'_>,
        _client: ClientId,
        _extension: &str,
    ) -> Decision {
        if self.0.load(Ordering::SeqCst) {
            Decision::Deny(DenyError {
                code: x11::error::BAD_VALUE,
                value: 0x55,
            })
        } else {
            Decision::Allow
        }
    }
}

fn name_denier(names: &[&str]) -> NameSetDenier {
    NameSetDenier(names.iter().map(|name| (*name).to_owned()).collect())
}

fn query_extension_body(name: &str) -> Vec<u8> {
    let name = name.as_bytes();
    let mut body = Vec::with_capacity(4 + name.len().div_ceil(4) * 4);
    body.extend_from_slice(&u16::try_from(name.len()).unwrap().to_le_bytes());
    body.extend_from_slice(&[0, 0]);
    body.extend_from_slice(name);
    body.resize(body.len().div_ceil(4) * 4, 0);
    body
}

fn dispatch_extension_request(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    client_id: ClientId,
    sequence: u16,
    opcode: u8,
    minor: u8,
    body: &[u8],
) {
    process_request(
        state,
        backend,
        client_id,
        SequenceNumber(sequence),
        RequestHeader {
            opcode,
            data: minor,
            length_units: u32::try_from(1 + body.len() / 4).unwrap(),
        },
        body,
        None,
    )
    .expect("process_request");
}

fn query_extension(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    sequence: u16,
    name: &str,
) -> Vec<u8> {
    let mut peer = install_client(state, 1);
    let body = query_extension_body(name);
    dispatch_extension_request(state, backend, ClientId(1), sequence, 98, 0, &body);
    read_all_available(&mut peer)
}

fn list_extensions(state: &mut ServerState, backend: &mut RecordingBackend) -> Vec<u8> {
    let mut peer = install_client(state, 1);
    dispatch_extension_request(state, backend, ClientId(1), 1, 99, 0, &[]);
    read_all_available(&mut peer)
}

fn names_from_list_reply(reply: &[u8]) -> Vec<String> {
    assert_eq!(reply.first(), Some(&1), "ListExtensions must reply");
    let name_count = usize::from(reply[1]);
    let extra_units = u32::from_le_bytes(reply[4..8].try_into().unwrap()) as usize;
    assert_eq!(reply.len(), 32 + extra_units * 4, "reply length field");

    let encoded_names = &reply[32..];
    let mut names = Vec::with_capacity(name_count);
    let mut offset = 0;
    for _ in 0..name_count {
        let length = usize::from(encoded_names[offset]);
        offset += 1;
        names.push(String::from_utf8(encoded_names[offset..offset + length].to_vec()).unwrap());
        offset += length;
    }
    assert_eq!(encoded_names.len(), offset.div_ceil(4) * 4);
    assert!(encoded_names[offset..].iter().all(|byte| *byte == 0));
    names
}

fn absent_query_reply(reply: &[u8]) {
    assert_eq!(reply.len(), 32);
    assert_eq!(reply[0], 1, "reply, not error");
    assert_eq!(
        &reply[8..12],
        &[0, 0, 0, 0],
        "absent extensions have zero presence, opcode, event, and error fields"
    );
}

#[test]
fn query_extension_denied_answers_like_an_unknown_name() {
    // Mutation: drop the access check; XFIXES becomes present and differs from the unknown-name reply.
    let mut unknown_state = ServerState::new();
    let mut unknown_backend = RecordingBackend::new();
    let unknown = query_extension(
        &mut unknown_state,
        &mut unknown_backend,
        0x1234,
        "NO-SUCH-EXTENSION",
    );

    let mut denied_state = ServerState::new();
    denied_state.install_access_policy(Arc::new(name_denier(&["XFIXES"])));
    let mut denied_backend = RecordingBackend::new();
    let denied = query_extension(&mut denied_state, &mut denied_backend, 0x1234, "XFIXES");

    absent_query_reply(&unknown);
    absent_query_reply(&denied);
    assert_eq!(
        denied, unknown,
        "denied and unknown extensions are indistinguishable"
    );
}

#[test]
fn list_extensions_leaves_out_denied_names() {
    // Mutation: ignore the access decision; the denied names remain in the list.
    let mut baseline_state = ServerState::new();
    let mut baseline_backend = RecordingBackend::new();
    let baseline = list_extensions(&mut baseline_state, &mut baseline_backend);
    let baseline_names = names_from_list_reply(&baseline);
    assert!(baseline_names.iter().any(|name| name == "XFIXES"));
    assert!(baseline_names.iter().any(|name| name == "SHAPE"));
    let expected: Vec<_> = baseline_names
        .iter()
        .filter(|name| name.as_str() != "XFIXES" && name.as_str() != "SHAPE")
        .cloned()
        .collect();

    let mut denied_state = ServerState::new();
    denied_state.install_access_policy(Arc::new(name_denier(&["XFIXES", "SHAPE"])));
    let mut denied_backend = RecordingBackend::new();
    let denied = list_extensions(&mut denied_state, &mut denied_backend);
    assert_eq!(names_from_list_reply(&denied), expected);
    assert_eq!(usize::from(denied[1]), expected.len());
    assert_eq!(
        denied.len(),
        32 + u32::from_le_bytes(denied[4..8].try_into().unwrap()) as usize * 4
    );

    let mut all_denied_state = ServerState::new();
    all_denied_state.install_access_policy(Arc::new(name_denier(
        &baseline_names
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
    )));
    let mut all_denied_backend = RecordingBackend::new();
    let all_denied = list_extensions(&mut all_denied_state, &mut all_denied_backend);
    assert_eq!(all_denied[0], 1);
    assert_eq!(all_denied[1], 0);
    assert_eq!(&all_denied[4..8], &[0, 0, 0, 0]);
    assert_eq!(all_denied.len(), 32);
    assert!(names_from_list_reply(&all_denied).is_empty());
}

#[test]
fn extension_access_asks_once_per_available_extension() {
    // Mutation: ask before availability is checked; unavailable RENDER is then recorded.
    let mut baseline_state = ServerState::new();
    let mut baseline_backend = RecordingBackend::new();
    let advertised =
        names_from_list_reply(&list_extensions(&mut baseline_state, &mut baseline_backend));
    assert!(advertised.iter().any(|name| name == "SYNC"));

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let expected_peer = state.clients[&1].peer;
    let expected_admission = state.clients[&1].admission;
    let calls = Arc::new(Mutex::new(Vec::new()));
    state.install_access_policy(Arc::new(RecordingExtensionPolicy {
        decision: Decision::Allow,
        calls: Arc::clone(&calls),
    }));
    let mut backend = RecordingBackend::new();

    dispatch_extension_request(
        &mut state,
        &mut backend,
        ClientId(1),
        10,
        98,
        0,
        &query_extension_body("SYNC"),
    );
    let _ = read_all_available(&mut peer);
    dispatch_extension_request(&mut state, &mut backend, ClientId(1), 11, 99, 0, &[]);
    let _ = read_all_available(&mut peer);
    dispatch_extension_request(
        &mut state,
        &mut backend,
        ClientId(1),
        12,
        98,
        0,
        &query_extension_body("RENDER"),
    );
    let _ = read_all_available(&mut peer);
    dispatch_extension_request(
        &mut state,
        &mut backend,
        ClientId(1),
        13,
        98,
        0,
        &query_extension_body("xfixes"),
    );
    let _ = read_all_available(&mut peer);

    let mut expected = vec![RecordedExtensionCheck {
        client: ClientId(1),
        extension: "SYNC".to_owned(),
        request: Some(RequestOpcodes {
            major: 98,
            minor: 0,
        }),
        peer: Some(expected_peer),
        admission: Some(expected_admission),
    }];
    expected.extend(advertised.iter().map(|name| RecordedExtensionCheck {
        client: ClientId(1),
        extension: name.clone(),
        request: Some(RequestOpcodes {
            major: 99,
            minor: 0,
        }),
        peer: Some(expected_peer),
        admission: Some(expected_admission),
    }));
    assert_eq!(*calls.lock().unwrap(), expected);
}

#[test]
fn extension_access_allow_keeps_bytes() {
    // Mutation: use the denial reply path after an Allow; every query and the list reply changes.
    let mut baseline_state = ServerState::new();
    let mut baseline_backend = RecordingBackend::new();
    let baseline_list = list_extensions(&mut baseline_state, &mut baseline_backend);
    let advertised = names_from_list_reply(&baseline_list);

    let mut allow_state = ServerState::new();
    allow_state.install_access_policy(Arc::new(RecordingExtensionPolicy {
        decision: Decision::Allow,
        calls: Arc::new(Mutex::new(Vec::new())),
    }));
    let mut allow_backend = RecordingBackend::new();
    assert_eq!(
        list_extensions(&mut allow_state, &mut allow_backend),
        baseline_list
    );

    for (index, name) in advertised.iter().enumerate() {
        let sequence = u16::try_from(index + 2).unwrap();
        let mut baseline_state = ServerState::new();
        let mut baseline_backend = RecordingBackend::new();
        let baseline = query_extension(&mut baseline_state, &mut baseline_backend, sequence, name);

        let mut allow_state = ServerState::new();
        allow_state.install_access_policy(Arc::new(RecordingExtensionPolicy {
            decision: Decision::Allow,
            calls: Arc::new(Mutex::new(Vec::new())),
        }));
        let mut allow_backend = RecordingBackend::new();
        let allowed = query_extension(&mut allow_state, &mut allow_backend, sequence, name);
        assert_eq!(allowed, baseline, "QueryExtension({name}) wire reply");
    }
}

#[test]
fn extension_access_first_denial_wins() {
    // Mutation: keep asking after a denial; the later policy records a call.
    let deny = Decision::Deny(DenyError {
        code: x11::error::BAD_VALUE,
        value: 0x55,
    });
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let allow_calls = Arc::new(Mutex::new(Vec::new()));
    let deny_calls = Arc::new(Mutex::new(Vec::new()));
    let after_deny_calls = Arc::new(Mutex::new(Vec::new()));
    for (decision, calls) in [
        (Decision::Allow, Arc::clone(&allow_calls)),
        (deny, Arc::clone(&deny_calls)),
        (Decision::Allow, Arc::clone(&after_deny_calls)),
    ] {
        state.install_access_policy(Arc::new(RecordingExtensionPolicy { decision, calls }));
    }
    let mut backend = RecordingBackend::new();
    dispatch_extension_request(
        &mut state,
        &mut backend,
        ClientId(1),
        1,
        98,
        0,
        &query_extension_body("XFIXES"),
    );
    let reply = read_all_available(&mut peer);
    absent_query_reply(&reply);
    assert_eq!(allow_calls.lock().unwrap().len(), 1);
    assert_eq!(deny_calls.lock().unwrap().len(), 1);
    assert!(after_deny_calls.lock().unwrap().is_empty());

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let deny_calls = Arc::new(Mutex::new(Vec::new()));
    let allow_calls = Arc::new(Mutex::new(Vec::new()));
    state.install_access_policy(Arc::new(RecordingExtensionPolicy {
        decision: deny,
        calls: Arc::clone(&deny_calls),
    }));
    state.install_access_policy(Arc::new(RecordingExtensionPolicy {
        decision: Decision::Allow,
        calls: Arc::clone(&allow_calls),
    }));
    let mut backend = RecordingBackend::new();
    dispatch_extension_request(
        &mut state,
        &mut backend,
        ClientId(1),
        1,
        98,
        0,
        &query_extension_body("XFIXES"),
    );
    absent_query_reply(&read_all_available(&mut peer));
    assert_eq!(deny_calls.lock().unwrap().len(), 1);
    assert!(allow_calls.lock().unwrap().is_empty());
}

#[test]
fn extension_access_revocation_applies_to_the_next_request() {
    // Mutation: reuse the prior Allow for the next request; revocation must make that reply absent.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let revoked = Arc::new(AtomicBool::new(false));
    state.install_access_policy(Arc::new(RevocableExtensionPolicy(Arc::clone(&revoked))));
    let mut backend = RecordingBackend::new();

    dispatch_extension_request(
        &mut state,
        &mut backend,
        ClientId(1),
        1,
        98,
        0,
        &query_extension_body("XFIXES"),
    );
    let allowed = read_all_available(&mut peer);
    assert_eq!(allowed[0], 1);
    assert_eq!(allowed[8], 1);

    revoked.store(true, Ordering::SeqCst);
    dispatch_extension_request(
        &mut state,
        &mut backend,
        ClientId(1),
        2,
        98,
        0,
        &query_extension_body("XFIXES"),
    );
    absent_query_reply(&read_all_available(&mut peer));
}

#[test]
fn extension_access_denial_does_not_deny_dispatch() {
    // Mutation: apply the discovery denial to extension dispatch; QueryVersion must still reply.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    state.install_access_policy(Arc::new(name_denier(&["XFIXES"])));
    let mut backend = RecordingBackend::new();

    dispatch_extension_request(
        &mut state,
        &mut backend,
        ClientId(1),
        1,
        98,
        0,
        &query_extension_body("XFIXES"),
    );
    absent_query_reply(&read_all_available(&mut peer));

    dispatch_extension_request(
        &mut state,
        &mut backend,
        ClientId(1),
        2,
        140,
        0,
        &[5, 0, 0, 0, 0, 0, 0, 0],
    );
    let version = read_all_available(&mut peer);
    assert_eq!(version.len(), 32);
    assert_eq!(version[0], 1, "XFIXES QueryVersion reply");
    assert_eq!(version[1], 0, "successful version reply");
    assert_eq!(u32::from_le_bytes(version[8..12].try_into().unwrap()), 5);
    assert_eq!(u32::from_le_bytes(version[12..16].try_into().unwrap()), 0);
}
