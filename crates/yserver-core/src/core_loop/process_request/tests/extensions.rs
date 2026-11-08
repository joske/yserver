use super::*;
use crate::access::{
    AccessContext, AccessMode, AccessPolicy, Decision, DenyError, RequestOpcodes, ResourceTarget,
};
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum ExtensionHook {
    Access,
    Dispatch,
    Resource,
    Property,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct RecordedExtensionCheck {
    pub(super) hook: ExtensionHook,
    pub(super) client: ClientId,
    pub(super) extension: String,
    pub(super) request: Option<RequestOpcodes>,
    pub(super) peer: Option<crate::peer::PeerIdentity>,
    pub(super) admission: Option<crate::access::Admission>,
    pub(super) target: Option<ResourceTarget>,
    pub(super) property: Option<(ResourceId, AtomId)>,
    pub(super) mode: Option<AccessMode>,
    /// Owner returned by the live-resource resolver.
    pub(super) owner: Option<ClientId>,
    /// Owner associated with this particular target instance.
    pub(super) target_owner: Option<ClientId>,
}

type RecordedDecision<'a> = Option<&'a RecordedExtensionCheck>;
type CustomDecision =
    dyn for<'a> Fn(usize, RecordedDecision<'a>, ExtensionHook) -> Decision + Send + Sync;

pub(super) struct RecordingExtensionPolicy {
    decision: Decision,
    calls: Arc<Mutex<Vec<RecordedExtensionCheck>>>,
    resource_denial: Option<(ResourceId, Option<AccessMode>, DenyError)>,
    deny_all_resources: Option<DenyError>,
    deny_resources_not_owned_by_requester: Option<DenyError>,
    custom_decision: Option<Arc<CustomDecision>>,
    resource_extension: Option<String>,
    record_calls: bool,
}

impl RecordingExtensionPolicy {
    pub(super) fn new(decision: Decision, calls: Arc<Mutex<Vec<RecordedExtensionCheck>>>) -> Self {
        Self {
            decision,
            calls,
            resource_denial: None,
            deny_all_resources: None,
            deny_resources_not_owned_by_requester: None,
            custom_decision: None,
            resource_extension: None,
            record_calls: true,
        }
    }

    pub(super) fn custom(
        calls: Arc<Mutex<Vec<RecordedExtensionCheck>>>,
        resource_extension: impl Into<String>,
        decide: impl Fn(usize, &RecordedExtensionCheck) -> Decision + Send + Sync + 'static,
    ) -> Self {
        let custom_decision: Arc<CustomDecision> = Arc::new(move |index, recorded, _hook| {
            decide(
                index,
                recorded.expect("recorded custom decision requires its check snapshot"),
            )
        });
        Self {
            decision: Decision::Allow,
            calls,
            resource_denial: None,
            deny_all_resources: None,
            deny_resources_not_owned_by_requester: None,
            custom_decision: Some(custom_decision),
            resource_extension: Some(resource_extension.into()),
            record_calls: true,
        }
    }

    /// Keeps the benchmark's policy counter out of recorder snapshot and bookkeeping costs.
    pub(super) fn custom_unrecorded(
        decide: impl Fn(usize, ExtensionHook) -> Decision + Send + Sync + 'static,
    ) -> Self {
        Self {
            decision: Decision::Allow,
            calls: Arc::new(Mutex::new(Vec::new())),
            resource_denial: None,
            deny_all_resources: None,
            deny_resources_not_owned_by_requester: None,
            custom_decision: Some(Arc::new(move |index, _recorded, hook| decide(index, hook))),
            resource_extension: None,
            record_calls: false,
        }
    }

    pub(super) fn denying_resource(
        calls: Arc<Mutex<Vec<RecordedExtensionCheck>>>,
        id: ResourceId,
        mode: Option<AccessMode>,
        error: DenyError,
    ) -> Self {
        Self {
            decision: Decision::Allow,
            calls,
            resource_denial: Some((id, mode, error)),
            deny_all_resources: None,
            deny_resources_not_owned_by_requester: None,
            custom_decision: None,
            resource_extension: None,
            record_calls: true,
        }
    }

    pub(super) fn denying_all_resources(
        calls: Arc<Mutex<Vec<RecordedExtensionCheck>>>,
        error: DenyError,
    ) -> Self {
        Self {
            decision: Decision::Allow,
            calls,
            resource_denial: None,
            deny_all_resources: Some(error),
            deny_resources_not_owned_by_requester: None,
            custom_decision: None,
            resource_extension: None,
            record_calls: true,
        }
    }

    pub(super) fn denying_resources_not_owned_by_requester(
        calls: Arc<Mutex<Vec<RecordedExtensionCheck>>>,
        error: DenyError,
    ) -> Self {
        Self {
            decision: Decision::Allow,
            calls,
            resource_denial: None,
            deny_all_resources: None,
            deny_resources_not_owned_by_requester: Some(error),
            custom_decision: None,
            resource_extension: None,
            record_calls: true,
        }
    }

    fn check(
        &self,
        hook: ExtensionHook,
        ctx: &AccessContext<'_>,
        client: ClientId,
        extension: &str,
        target: Option<ResourceTarget>,
        property: Option<(ResourceId, AtomId)>,
        mode: Option<AccessMode>,
    ) -> Decision {
        if !self.record_calls {
            return self
                .custom_decision
                .as_ref()
                .map_or(self.decision, |decide| decide(0, None, hook));
        }

        let recorded = RecordedExtensionCheck {
            hook: hook.clone(),
            client,
            extension: if hook == ExtensionHook::Resource {
                self.resource_extension
                    .clone()
                    .unwrap_or_else(|| extension.to_owned())
            } else {
                extension.to_owned()
            },
            request: ctx.request(),
            peer: ctx.peer(client).copied(),
            admission: ctx.admission(client).cloned(),
            owner: target.and_then(|target| ctx.owner(target.id, target.kind)),
            target_owner: target.and_then(|target| ctx.target_owner(&target)),
            target,
            property,
            mode,
        };
        let decide = |index| {
            if let Some(decide) = &self.custom_decision {
                decide(index, Some(&recorded), hook.clone())
            } else if hook == ExtensionHook::Resource {
                if let Some(error) = self.deny_all_resources {
                    Decision::Deny(error)
                } else if let Some((id, expected_mode, error)) = self.resource_denial
                    && target.is_some_and(|target| {
                        target.id == id
                            && expected_mode.is_none_or(|expected| mode == Some(expected))
                    })
                {
                    Decision::Deny(error)
                } else if let Some(error) = self.deny_resources_not_owned_by_requester
                    && target.is_some_and(|target| ctx.target_owner(&target) != Some(client))
                {
                    Decision::Deny(error)
                } else {
                    self.decision
                }
            } else {
                self.decision
            }
        };
        let mut calls = self.calls.lock().unwrap();
        let decision = decide(calls.len());
        calls.push(recorded);
        decision
    }
}

impl AccessPolicy for RecordingExtensionPolicy {
    fn extension_access(
        &self,
        ctx: &AccessContext<'_>,
        client: ClientId,
        extension: &str,
    ) -> Decision {
        self.check(
            ExtensionHook::Access,
            ctx,
            client,
            extension,
            None,
            None,
            None,
        )
    }

    fn extension_dispatch(
        &self,
        ctx: &AccessContext<'_>,
        client: ClientId,
        extension: &str,
    ) -> Decision {
        self.check(
            ExtensionHook::Dispatch,
            ctx,
            client,
            extension,
            None,
            None,
            None,
        )
    }

    fn resource(
        &self,
        ctx: &AccessContext<'_>,
        client: ClientId,
        target: &ResourceTarget,
        mode: AccessMode,
    ) -> Decision {
        self.check(
            ExtensionHook::Resource,
            ctx,
            client,
            "",
            Some(*target),
            None,
            Some(mode),
        )
    }

    fn property(
        &self,
        ctx: &AccessContext<'_>,
        client: ClientId,
        window: ResourceId,
        property: AtomId,
        mode: AccessMode,
    ) -> Decision {
        self.check(
            ExtensionHook::Property,
            ctx,
            client,
            "",
            None,
            Some((window, property)),
            Some(mode),
        )
    }
}

fn name_denier(names: &[&str]) -> RecordingExtensionPolicy {
    let names = names
        .iter()
        .map(|name| (*name).to_owned())
        .collect::<HashSet<_>>();
    RecordingExtensionPolicy::custom(Arc::new(Mutex::new(Vec::new())), "", move |_, call| {
        if call.hook == ExtensionHook::Access && names.contains(&call.extension) {
            Decision::Deny(DenyError {
                code: x11::error::BAD_VALUE,
                value: 0x55,
            })
        } else {
            Decision::Allow
        }
    })
}

fn revocable_extension_policy(
    calls: Arc<Mutex<Vec<RecordedExtensionCheck>>>,
    revoked: Arc<AtomicBool>,
) -> RecordingExtensionPolicy {
    RecordingExtensionPolicy::custom(calls, "", move |_, _| {
        if revoked.load(Ordering::SeqCst) {
            Decision::Deny(DenyError {
                code: x11::error::BAD_VALUE,
                value: 0x55,
            })
        } else {
            Decision::Allow
        }
    })
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

fn dispatch_error(reply: &[u8], sequence: u16, minor: u16, major: u8) {
    assert_eq!(reply.len(), 32, "one X error packet");
    assert_eq!(reply[0], 0, "error packet");
    assert_eq!(reply[1], x11::error::BAD_REQUEST, "BadRequest");
    assert_eq!(&reply[2..4], &sequence.to_le_bytes(), "sequence");
    assert_eq!(&reply[4..8], &[0, 0, 0, 0], "error value");
    assert_eq!(&reply[8..10], &minor.to_le_bytes(), "minor opcode");
    assert_eq!(reply[10], major, "major opcode");
}

fn request_reply_without_policy(opcode: u8, minor: u8, body: &[u8]) -> Vec<u8> {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    dispatch_extension_request(
        &mut state,
        &mut backend,
        ClientId(1),
        1,
        opcode,
        minor,
        body,
    );
    read_all_available(&mut peer)
}

#[test]
fn query_extension_denied_answers_like_an_unknown_name() {
    // Mutation: drop the access check; XFIXES becomes present and differs from the unknown-name
    // reply.
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
    state.install_access_policy(Arc::new(RecordingExtensionPolicy::new(
        Decision::Allow,
        Arc::clone(&calls),
    )));
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
        hook: ExtensionHook::Access,
        client: ClientId(1),
        extension: "SYNC".to_owned(),
        request: Some(RequestOpcodes {
            major: 98,
            minor: 0,
        }),
        peer: Some(expected_peer),
        admission: Some(expected_admission),
        target: None,
        property: None,
        mode: None,
        owner: None,
        target_owner: None,
    }];
    expected.extend(advertised.iter().map(|name| RecordedExtensionCheck {
        hook: ExtensionHook::Access,
        client: ClientId(1),
        extension: name.clone(),
        request: Some(RequestOpcodes {
            major: 99,
            minor: 0,
        }),
        peer: Some(expected_peer),
        admission: Some(expected_admission),
        target: None,
        property: None,
        mode: None,
        owner: None,
        target_owner: None,
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
    allow_state.install_access_policy(Arc::new(RecordingExtensionPolicy::new(
        Decision::Allow,
        Arc::new(Mutex::new(Vec::new())),
    )));
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
        allow_state.install_access_policy(Arc::new(RecordingExtensionPolicy::new(
            Decision::Allow,
            Arc::new(Mutex::new(Vec::new())),
        )));
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
        state.install_access_policy(Arc::new(RecordingExtensionPolicy::new(decision, calls)));
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
    state.install_access_policy(Arc::new(RecordingExtensionPolicy::new(
        deny,
        Arc::clone(&deny_calls),
    )));
    state.install_access_policy(Arc::new(RecordingExtensionPolicy::new(
        Decision::Allow,
        Arc::clone(&allow_calls),
    )));
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
    state.install_access_policy(Arc::new(revocable_extension_policy(
        Arc::new(Mutex::new(Vec::new())),
        Arc::clone(&revoked),
    )));
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

#[test]
fn every_extension_dispatch_denial_answers_bad_request() {
    // Value 0 is deliberate, not Xorg parity: Xorg sends the stale client->errorValue here.
    // Mutation: return the policy's DenyError; the packet must still be BadRequest with value 0.
    for extension in crate::nested::EXTENSIONS {
        let mut state = ServerState::new();
        let mut peer = install_client(&mut state, 1);
        let calls = Arc::new(Mutex::new(Vec::new()));
        state.install_access_policy(Arc::new(RecordingExtensionPolicy::new(
            Decision::Deny(DenyError {
                code: x11::error::BAD_VALUE,
                value: 0x55,
            }),
            Arc::clone(&calls),
        )));
        let mut backend = RecordingBackend::new();
        let body = [0u8; 8];
        dispatch_extension_request(
            &mut state,
            &mut backend,
            ClientId(1),
            0x1234,
            extension.major_opcode,
            0,
            &body,
        );

        dispatch_error(
            &read_all_available(&mut peer),
            0x1234,
            0,
            extension.major_opcode,
        );
        assert!(
            backend.calls().is_empty(),
            "{} handler must not run",
            extension.name
        );
        assert_eq!(
            *calls.lock().unwrap(),
            [RecordedExtensionCheck {
                hook: ExtensionHook::Dispatch,
                client: ClientId(1),
                extension: extension.name.to_owned(),
                request: Some(RequestOpcodes {
                    major: extension.major_opcode,
                    minor: 0,
                }),
                peer: Some(state.clients[&1].peer),
                admission: Some(state.clients[&1].admission),
                target: None,
                property: None,
                mode: None,
                owner: None,
                target_owner: None,
            }],
            "one dispatch check for {}",
            extension.name,
        );
        if extension.name == "BIG-REQUESTS" {
            assert!(!state.clients[&1].big_requests_enabled);
        }
    }
}

#[test]
fn extension_dispatch_preserves_nonzero_minor_in_check_and_error() {
    // Mutations: hard-code minor 0 in RequestOpcodes or in the denial error; each assertion must
    // fail.
    let major = crate::nested::EXTENSIONS
        .iter()
        .find(|extension| extension.name == "XFIXES")
        .unwrap()
        .major_opcode;
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    state.install_access_policy(Arc::new(RecordingExtensionPolicy::new(
        Decision::Deny(DenyError {
            code: x11::error::BAD_VALUE,
            value: 0x55,
        }),
        Arc::clone(&calls),
    )));
    let mut backend = RecordingBackend::new();
    dispatch_extension_request(&mut state, &mut backend, ClientId(1), 9, major, 1, &[0; 8]);

    dispatch_error(&read_all_available(&mut peer), 9, 1, major);
    assert_eq!(
        *calls.lock().unwrap(),
        [RecordedExtensionCheck {
            hook: ExtensionHook::Dispatch,
            client: ClientId(1),
            extension: "XFIXES".to_owned(),
            request: Some(RequestOpcodes { major, minor: 1 }),
            peer: Some(state.clients[&1].peer),
            admission: Some(state.clients[&1].admission),
            target: None,
            property: None,
            mode: None,
            owner: None,
            target_owner: None,
        }],
    );
}

#[test]
fn denied_big_requests_enable_releases_the_reader() {
    // Mutation: return BadRequest without sending IgnoreBigRequests; the reader-control assertion
    // fails.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let (control_tx, control_rx) = crossbeam_channel::unbounded();
    state.clients.get_mut(&1).unwrap().reader_control = Some(control_tx);
    state.install_access_policy(Arc::new(RecordingExtensionPolicy::new(
        Decision::Deny(DenyError {
            code: x11::error::BAD_VALUE,
            value: 0x55,
        }),
        Arc::new(Mutex::new(Vec::new())),
    )));
    let mut backend = RecordingBackend::new();

    dispatch_extension_request(
        &mut state,
        &mut backend,
        ClientId(1),
        7,
        crate::nested::EXTENSIONS
            .iter()
            .find(|extension| extension.name == "BIG-REQUESTS")
            .unwrap()
            .major_opcode,
        0,
        &[0; 8],
    );

    dispatch_error(&read_all_available(&mut peer), 7, 0, 135);
    assert!(!state.clients[&1].big_requests_enabled);
    assert!(matches!(
        control_rx.try_recv(),
        Ok(crate::server::ReaderControl::IgnoreBigRequests)
    ));
    assert!(control_rx.try_recv().is_err(), "one release signal only");
}

#[test]
fn extension_dispatch_asks_with_client_name_and_opcodes() {
    // Mutation: filter dispatch by discovery availability; unadvertised RENDER must still be
    // checked.
    let version_body = [5, 0, 0, 0, 0, 0, 0, 0];
    let xfixes_major = crate::nested::EXTENSIONS
        .iter()
        .find(|extension| extension.name == "XFIXES")
        .unwrap()
        .major_opcode;
    let render_major = crate::nested::EXTENSIONS
        .iter()
        .find(|extension| extension.name == "RENDER")
        .unwrap()
        .major_opcode;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let calls = Arc::new(Mutex::new(Vec::new()));
    state.install_access_policy(Arc::new(RecordingExtensionPolicy::new(
        Decision::Allow,
        Arc::clone(&calls),
    )));
    let mut backend = RecordingBackend::new();

    dispatch_extension_request(
        &mut state,
        &mut backend,
        ClientId(1),
        1,
        xfixes_major,
        0,
        &version_body,
    );
    let xfixes_reply = read_all_available(&mut peer);
    assert_eq!(
        xfixes_reply,
        request_reply_without_policy(xfixes_major, 0, &version_body)
    );

    dispatch_extension_request(
        &mut state,
        &mut backend,
        ClientId(1),
        1,
        render_major,
        0,
        &version_body,
    );
    let render_reply = read_all_available(&mut peer);
    assert_eq!(
        render_reply,
        request_reply_without_policy(render_major, 0, &version_body)
    );

    for opcode in [43, 200] {
        dispatch_extension_request(&mut state, &mut backend, ClientId(1), 1, opcode, 0, &[]);
        assert_eq!(
            read_all_available(&mut peer),
            request_reply_without_policy(opcode, 0, &[]),
            "opcode {opcode} keeps its no-policy bytes",
        );
    }
    assert_eq!(
        *calls.lock().unwrap(),
        [
            RecordedExtensionCheck {
                hook: ExtensionHook::Dispatch,
                client: ClientId(1),
                extension: "XFIXES".to_owned(),
                request: Some(RequestOpcodes {
                    major: xfixes_major,
                    minor: 0,
                }),
                peer: Some(state.clients[&1].peer),
                admission: Some(state.clients[&1].admission),
                target: None,
                property: None,
                mode: None,
                owner: None,
                target_owner: None,
            },
            RecordedExtensionCheck {
                hook: ExtensionHook::Dispatch,
                client: ClientId(1),
                extension: "RENDER".to_owned(),
                request: Some(RequestOpcodes {
                    major: render_major,
                    minor: 0,
                }),
                peer: Some(state.clients[&1].peer),
                admission: Some(state.clients[&1].admission),
                target: None,
                property: None,
                mode: None,
                owner: None,
                target_owner: None,
            },
        ]
    );
}

#[test]
fn transport_gate_and_max_length_answer_before_the_dispatch_check() {
    // Mutation: move the dispatch check above the locality gate; DRI3 must keep BadMatch and make
    // no policy call.
    let dri3_major = crate::nested::EXTENSIONS
        .iter()
        .find(|extension| extension.name == "DRI3")
        .unwrap()
        .major_opcode;
    let dri3_body = [0u8; 8];

    let mut baseline_state = ServerState::new();
    let mut baseline_peer = install_client(&mut baseline_state, 1);
    baseline_state.clients.get_mut(&1).unwrap().is_local = false;
    baseline_state.clients.get_mut(&1).unwrap().fd_passing = false;
    dispatch_extension_request(
        &mut baseline_state,
        &mut RecordingBackend::new(),
        ClientId(1),
        1,
        dri3_major,
        0,
        &dri3_body,
    );
    let baseline_gate = read_all_available(&mut baseline_peer);

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    state.clients.get_mut(&1).unwrap().is_local = false;
    state.clients.get_mut(&1).unwrap().fd_passing = false;
    let calls = Arc::new(Mutex::new(Vec::new()));
    state.install_access_policy(Arc::new(RecordingExtensionPolicy::new(
        Decision::Deny(DenyError {
            code: x11::error::BAD_VALUE,
            value: 0x55,
        }),
        Arc::clone(&calls),
    )));
    dispatch_extension_request(
        &mut state,
        &mut RecordingBackend::new(),
        ClientId(1),
        1,
        dri3_major,
        0,
        &dri3_body,
    );
    let gate = read_all_available(&mut peer);
    assert_eq!(gate, baseline_gate);
    assert_eq!(gate[1], x11::error::BAD_MATCH);
    assert!(calls.lock().unwrap().is_empty());

    let mut baseline_state = ServerState::new();
    let mut baseline_peer = install_client(&mut baseline_state, 1);
    let mut baseline_backend = RecordingBackend::new();
    process_request(
        &mut baseline_state,
        &mut baseline_backend,
        ClientId(1),
        SequenceNumber(2),
        RequestHeader {
            opcode: dri3_major,
            data: 0,
            length_units: 65_536,
        },
        &dri3_body,
        None,
    )
    .unwrap();
    let baseline_length = read_all_available(&mut baseline_peer);

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let calls = Arc::new(Mutex::new(Vec::new()));
    state.install_access_policy(Arc::new(RecordingExtensionPolicy::new(
        Decision::Deny(DenyError {
            code: x11::error::BAD_VALUE,
            value: 0x55,
        }),
        Arc::clone(&calls),
    )));
    process_request(
        &mut state,
        &mut RecordingBackend::new(),
        ClientId(1),
        SequenceNumber(2),
        RequestHeader {
            opcode: dri3_major,
            data: 0,
            length_units: 65_536,
        },
        &dri3_body,
        None,
    )
    .unwrap();
    let length = read_all_available(&mut peer);
    assert_eq!(length, baseline_length);
    assert_eq!(length[1], x11::error::BAD_LENGTH);
    assert!(calls.lock().unwrap().is_empty());
}

#[test]
fn extension_dispatch_first_denial_wins_and_revocation_applies() {
    // Mutations: let a later Allow override Deny, or reuse a prior Allow after revocation; each
    // request must re-check.
    let deny = Decision::Deny(DenyError {
        code: x11::error::BAD_VALUE,
        value: 0x55,
    });
    let major = crate::nested::EXTENSIONS
        .iter()
        .find(|extension| extension.name == "XFIXES")
        .unwrap()
        .major_opcode;
    let body = [5, 0, 0, 0, 0, 0, 0, 0];

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let first_calls = Arc::new(Mutex::new(Vec::new()));
    let deny_calls = Arc::new(Mutex::new(Vec::new()));
    let after_deny_calls = Arc::new(Mutex::new(Vec::new()));
    for (decision, calls) in [
        (Decision::Allow, Arc::clone(&first_calls)),
        (deny, Arc::clone(&deny_calls)),
        (Decision::Allow, Arc::clone(&after_deny_calls)),
    ] {
        state.install_access_policy(Arc::new(RecordingExtensionPolicy::new(decision, calls)));
    }
    let mut backend = RecordingBackend::new();
    dispatch_extension_request(&mut state, &mut backend, ClientId(1), 1, major, 0, &body);
    dispatch_error(&read_all_available(&mut peer), 1, 0, major);
    assert_eq!(first_calls.lock().unwrap().len(), 1);
    assert_eq!(deny_calls.lock().unwrap().len(), 1);
    assert!(after_deny_calls.lock().unwrap().is_empty());

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let deny_calls = Arc::new(Mutex::new(Vec::new()));
    let allow_calls = Arc::new(Mutex::new(Vec::new()));
    state.install_access_policy(Arc::new(RecordingExtensionPolicy::new(
        deny,
        Arc::clone(&deny_calls),
    )));
    state.install_access_policy(Arc::new(RecordingExtensionPolicy::new(
        Decision::Allow,
        Arc::clone(&allow_calls),
    )));
    dispatch_extension_request(&mut state, &mut backend, ClientId(1), 1, major, 0, &body);
    dispatch_error(&read_all_available(&mut peer), 1, 0, major);
    assert_eq!(deny_calls.lock().unwrap().len(), 1);
    assert!(allow_calls.lock().unwrap().is_empty());

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let revoked = Arc::new(AtomicBool::new(false));
    state.install_access_policy(Arc::new(revocable_extension_policy(
        Arc::new(Mutex::new(Vec::new())),
        Arc::clone(&revoked),
    )));
    dispatch_extension_request(&mut state, &mut backend, ClientId(1), 1, major, 0, &body);
    assert_eq!(
        read_all_available(&mut peer)[0],
        1,
        "first request is allowed"
    );
    revoked.store(true, Ordering::SeqCst);
    dispatch_extension_request(&mut state, &mut backend, ClientId(1), 2, major, 0, &body);
    dispatch_error(&read_all_available(&mut peer), 2, 0, major);
}
