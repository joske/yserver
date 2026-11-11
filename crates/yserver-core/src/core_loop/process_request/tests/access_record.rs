use super::{
    extensions::{ExtensionHook, RecordedExtensionCheck, RecordingExtensionPolicy},
    *,
};
use crate::access::{
    AccessMode, Creation, Decision, DenyError, RequestOpcodes, ResourceKind, ResourceTarget,
};
use yserver_protocol::x11::{ResourceId, record as x11rec};

const CLIENT: ClientId = ClientId(1);
const CLIENT_TWO: ClientId = ClientId(2);
const CLIENT_THREE: ClientId = ClientId(3);
const CONTEXT: ResourceId = ResourceId(0x0010_8001);
const NEW_CONTEXT: ResourceId = ResourceId(0x0010_8002);
const CLIENT_BASE: u32 = 0x0100_0000;
const CLIENT_TWO_BASE: u32 = 0x0200_0000;
const CLIENT_THREE_BASE: u32 = 0x0300_0000;
const DENY_ERROR: DenyError = DenyError {
    code: x11::error::BAD_ACCESS,
    value: 0xA55A_1234,
};

struct RecordAccessFixture {
    state: ServerState,
    peers: Vec<(ClientId, UnixStream)>,
    backend: RecordingBackend,
}

fn record_access_fixture() -> RecordAccessFixture {
    let mut state = ServerState::new();
    let peer = install_client(&mut state, CLIENT.0);
    RecordAccessFixture {
        state,
        peers: vec![(CLIENT, peer)],
        backend: RecordingBackend::new(),
    }
}

fn add_client(fixture: &mut RecordAccessFixture, client: ClientId, base: u32) {
    let peer = install_client(&mut fixture.state, client.0);
    set_client_range(fixture, client, base);
    fixture.peers.push((client, peer));
}

fn set_client_range(fixture: &mut RecordAccessFixture, client: ClientId, base: u32) {
    let client_state = fixture
        .state
        .clients
        .get_mut(&client.0)
        .expect("installed client");
    client_state.resource_id_base = base;
    client_state.resource_id_mask = 0x00ff_ffff;
}

fn run_record_request(
    fixture: &mut RecordAccessFixture,
    client: ClientId,
    sequence: u16,
    minor: u8,
    body: &[u8],
) -> Vec<u8> {
    process_request(
        &mut fixture.state,
        &mut fixture.backend,
        client,
        SequenceNumber(sequence),
        RequestHeader {
            opcode: crate::nested::RECORD_MAJOR_OPCODE,
            data: minor,
            length_units: u32::try_from(1 + body.len() / 4).unwrap(),
        },
        body,
        None,
    )
    .expect("RECORD request through process_request");
    let peer = fixture
        .peers
        .iter_mut()
        .find_map(|(id, peer)| (*id == client).then_some(peer))
        .expect("client peer");
    read_all_available(peer)
}

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn context_request_body(context: ResourceId) -> Vec<u8> {
    context.0.to_le_bytes().to_vec()
}

fn register_body(context: ResourceId, specs: &[u32], ranges: &[x11rec::Range]) -> Vec<u8> {
    let mut body = Vec::with_capacity(
        x11rec::REGISTER_FIXED_BODY + specs.len() * 4 + ranges.len() * x11rec::RANGE_SIZE,
    );
    put_u32(&mut body, context.0);
    body.extend_from_slice(&[0; 4]);
    put_u32(&mut body, u32::try_from(specs.len()).unwrap());
    put_u32(&mut body, u32::try_from(ranges.len()).unwrap());
    for spec in specs {
        put_u32(&mut body, *spec);
    }
    for range in ranges {
        range.encode(ClientByteOrder::LittleEndian, &mut body);
    }
    body
}

fn unregister_body(context: ResourceId, specs: &[u32]) -> Vec<u8> {
    let mut body = Vec::with_capacity(x11rec::UNREGISTER_FIXED_BODY + specs.len() * 4);
    put_u32(&mut body, context.0);
    put_u32(&mut body, u32::try_from(specs.len()).unwrap());
    for spec in specs {
        put_u32(&mut body, *spec);
    }
    body
}

fn create_context(
    fixture: &mut RecordAccessFixture,
    client: ClientId,
    context: ResourceId,
    specs: &[u32],
    ranges: &[x11rec::Range],
) {
    let bytes = run_record_request(
        fixture,
        client,
        1,
        x11rec::CREATE_CONTEXT,
        &register_body(context, specs, ranges),
    );
    assert!(
        bytes.is_empty(),
        "CreateContext should be silent: {bytes:02x?}"
    );
}

fn resource_target(
    id: ResourceId,
    kind: ResourceKind,
    creation: Option<Creation>,
) -> ResourceTarget {
    ResourceTarget {
        id,
        kind,
        creation,
        owner: None,
    }
}

fn resource_calls(calls: &[RecordedExtensionCheck]) -> Vec<&RecordedExtensionCheck> {
    calls
        .iter()
        .filter(|call| call.hook == ExtensionHook::Resource)
        .collect()
}

fn assert_policy_error(bytes: &[u8], sequence: u16, minor: u8) {
    assert_eq!(bytes.len(), 32, "expected one policy error: {bytes:02x?}");
    assert_eq!(bytes[0], 0, "expected X error packet");
    assert_eq!(bytes[1], DENY_ERROR.code);
    assert_eq!(
        u16::from_le_bytes(bytes[2..4].try_into().unwrap()),
        sequence
    );
    assert_eq!(
        u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
        DENY_ERROR.value,
        "policy error value must be forwarded verbatim"
    );
    assert_eq!(
        u16::from_le_bytes(bytes[8..10].try_into().unwrap()),
        u16::from(minor)
    );
    assert_eq!(bytes[10], crate::nested::RECORD_MAJOR_OPCODE);
}

fn assert_recorded_resource(
    call: &RecordedExtensionCheck,
    client: ClientId,
    target: ResourceTarget,
    mode: AccessMode,
    minor: u8,
    owner: Option<ClientId>,
    target_owner: Option<ClientId>,
) {
    assert_eq!(call.hook, ExtensionHook::Resource);
    assert_eq!(call.client, client);
    assert_eq!(call.target, Some(target));
    assert_eq!(call.mode, Some(mode));
    assert_eq!(
        call.request,
        Some(RequestOpcodes {
            major: crate::nested::RECORD_MAJOR_OPCODE,
            minor,
        })
    );
    assert_eq!(call.owner, owner);
    assert_eq!(call.target_owner, target_owner);
}

#[derive(Clone, Copy)]
struct ContextSite {
    label: &'static str,
    minor: u8,
    context: ResourceId,
    mode: AccessMode,
    creates: bool,
}

fn context_sites() -> Vec<ContextSite> {
    vec![
        ContextSite {
            label: "CreateContext",
            minor: x11rec::CREATE_CONTEXT,
            context: NEW_CONTEXT,
            mode: AccessMode::CREATE,
            creates: true,
        },
        ContextSite {
            label: "RegisterClients",
            minor: x11rec::REGISTER_CLIENTS,
            context: CONTEXT,
            mode: AccessMode::USE,
            creates: false,
        },
        ContextSite {
            label: "UnregisterClients",
            minor: x11rec::UNREGISTER_CLIENTS,
            context: CONTEXT,
            mode: AccessMode::USE,
            creates: false,
        },
        ContextSite {
            label: "GetContext",
            minor: x11rec::GET_CONTEXT,
            context: CONTEXT,
            mode: AccessMode::USE,
            creates: false,
        },
        ContextSite {
            label: "EnableContext",
            minor: x11rec::ENABLE_CONTEXT,
            context: CONTEXT,
            mode: AccessMode::USE,
            creates: false,
        },
        ContextSite {
            label: "DisableContext",
            minor: x11rec::DISABLE_CONTEXT,
            context: CONTEXT,
            mode: AccessMode::USE,
            creates: false,
        },
        ContextSite {
            label: "FreeContext",
            minor: x11rec::FREE_CONTEXT,
            context: CONTEXT,
            mode: AccessMode::USE,
            creates: false,
        },
    ]
}

fn context_site_body(site: ContextSite) -> Vec<u8> {
    match site.minor {
        x11rec::CREATE_CONTEXT | x11rec::REGISTER_CLIENTS => register_body(site.context, &[], &[]),
        x11rec::UNREGISTER_CLIENTS => unregister_body(site.context, &[]),
        _ => context_request_body(site.context),
    }
}

fn install_deny_policy(
    fixture: &mut RecordAccessFixture,
    target: ResourceTarget,
    mode: AccessMode,
    calls: &Arc<Mutex<Vec<RecordedExtensionCheck>>>,
) {
    fixture
        .state
        .install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
            Arc::clone(calls),
            target.id,
            Some(mode),
            DENY_ERROR,
        )));
}

#[test]
fn record_context_denials_match_the_site_table() {
    // Mutation: omit any listed RECORD context check; that row's policy-error assertion must fail.
    for site in context_sites() {
        let mut fixture = record_access_fixture();
        if !site.creates {
            create_context(&mut fixture, CLIENT, CONTEXT, &[], &[]);
        }
        let target = resource_target(
            site.context,
            ResourceKind::RecordContext,
            site.creates.then_some(Creation {
                parent: None,
                owner: CLIENT,
            }),
        );
        let calls = Arc::new(Mutex::new(Vec::new()));
        install_deny_policy(&mut fixture, target, site.mode, &calls);
        let before = format!("{:?}", fixture.state.record);

        let bytes = run_record_request(
            &mut fixture,
            CLIENT,
            2,
            site.minor,
            &context_site_body(site),
        );

        assert_policy_error(&bytes, 2, site.minor);
        assert_eq!(
            format!("{:?}", fixture.state.record),
            before,
            "{} changed RECORD state after denial",
            site.label
        );
        assert!(
            fixture.backend.calls().is_empty(),
            "{} reached backend",
            site.label
        );
        let calls = calls.lock().unwrap();
        let resources = resource_calls(&calls);
        assert_eq!(resources.len(), 1, "{} resource checks", site.label);
        assert_recorded_resource(
            resources[0],
            CLIENT,
            target,
            site.mode,
            site.minor,
            (!site.creates).then_some(CLIENT),
            (!site.creates).then_some(CLIENT),
        );
    }
}

#[test]
fn record_context_asks_match_the_site_table() {
    // Mutation: change any site's mode or omit its extension minor opcode; the row assertion must fail.
    for site in context_sites() {
        let mut fixture = record_access_fixture();
        if !site.creates {
            create_context(&mut fixture, CLIENT, CONTEXT, &[], &[]);
        }
        let calls = Arc::new(Mutex::new(Vec::new()));
        fixture
            .state
            .install_access_policy(Arc::new(RecordingExtensionPolicy::new(
                Decision::Allow,
                Arc::clone(&calls),
            )));

        let bytes = run_record_request(
            &mut fixture,
            CLIENT,
            2,
            site.minor,
            &context_site_body(site),
        );

        assert_ne!(
            bytes.first(),
            Some(&0),
            "{} unexpectedly failed",
            site.label
        );
        let calls = calls.lock().unwrap();
        let resources = resource_calls(&calls);
        assert_eq!(resources.len(), 1, "{} resource checks", site.label);
        let target = resource_target(
            site.context,
            ResourceKind::RecordContext,
            site.creates.then_some(Creation {
                parent: None,
                owner: CLIENT,
            }),
        );
        assert_recorded_resource(
            resources[0],
            CLIENT,
            target,
            site.mode,
            site.minor,
            (!site.creates).then_some(CLIENT),
            (!site.creates).then_some(CLIENT),
        );
    }
}

#[test]
fn record_resource_spec_extension_resource_is_asked() {
    // Mutation: use the core-only resolver; this SyncCounter GET_ATTR assertion must fail.
    let mut fixture = record_access_fixture();
    add_client(&mut fixture, CLIENT_TWO, CLIENT_TWO_BASE);
    set_client_range(&mut fixture, CLIENT, CLIENT_BASE);
    let counter = ResourceId(CLIENT_TWO_BASE | 0x8001);
    fixture.state.sync_counters.insert(
        counter.0,
        crate::server::SyncCounter {
            owner: CLIENT_TWO,
            value: 7,
        },
    );
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .state
        .install_access_policy(Arc::new(RecordingExtensionPolicy::new(
            Decision::Allow,
            Arc::clone(&calls),
        )));

    let bytes = run_record_request(
        &mut fixture,
        CLIENT,
        1,
        x11rec::CREATE_CONTEXT,
        &register_body(ResourceId(CLIENT_BASE | 0x8001), &[counter.0], &[]),
    );

    assert!(bytes.is_empty(), "CreateContext failed: {bytes:02x?}");
    let calls = calls.lock().unwrap();
    let resources = resource_calls(&calls);
    assert_eq!(resources.len(), 2, "specifier and context creation checks");
    assert_recorded_resource(
        resources[0],
        CLIENT,
        resource_target(counter, ResourceKind::SyncCounter, None),
        AccessMode::GET_ATTR,
        x11rec::CREATE_CONTEXT,
        Some(CLIENT_TWO),
        Some(CLIENT_TWO),
    );
    assert_recorded_resource(
        resources[1],
        CLIENT,
        resource_target(
            ResourceId(CLIENT_BASE | 0x8001),
            ResourceKind::RecordContext,
            Some(Creation {
                parent: None,
                owner: CLIENT,
            }),
        ),
        AccessMode::CREATE,
        x11rec::CREATE_CONTEXT,
        None,
        None,
    );
}

#[test]
fn record_resource_specifier_asks_match_the_site_table() {
    // Mutation: omit any per-specifier check; its request row must lose the GET_ATTR record.
    let sites = [
        ("CreateContext", x11rec::CREATE_CONTEXT),
        ("RegisterClients", x11rec::REGISTER_CLIENTS),
        ("UnregisterClients", x11rec::UNREGISTER_CLIENTS),
    ];
    for (label, minor) in sites {
        let mut fixture = record_access_fixture();
        add_client(&mut fixture, CLIENT_TWO, CLIENT_TWO_BASE);
        set_client_range(&mut fixture, CLIENT, CLIENT_BASE);
        let context = ResourceId(CLIENT_BASE | 0x8001);
        if minor != x11rec::CREATE_CONTEXT {
            create_context(&mut fixture, CLIENT, context, &[CLIENT_BASE], &[]);
        }
        let counter = ResourceId(CLIENT_TWO_BASE | 0x8001);
        fixture.state.sync_counters.insert(
            counter.0,
            crate::server::SyncCounter {
                owner: CLIENT_TWO,
                value: 7,
            },
        );
        let calls = Arc::new(Mutex::new(Vec::new()));
        fixture
            .state
            .install_access_policy(Arc::new(RecordingExtensionPolicy::new(
                Decision::Allow,
                Arc::clone(&calls),
            )));
        let body = if minor == x11rec::UNREGISTER_CLIENTS {
            unregister_body(context, &[counter.0])
        } else {
            register_body(context, &[counter.0], &[])
        };

        let bytes = run_record_request(&mut fixture, CLIENT, 2, minor, &body);

        assert_ne!(bytes.first(), Some(&0), "{label} failed: {bytes:02x?}");
        let calls = calls.lock().unwrap();
        let resources = resource_calls(&calls);
        if minor == x11rec::CREATE_CONTEXT {
            assert_eq!(resources.len(), 2, "{label} checks");
            assert_recorded_resource(
                resources[0],
                CLIENT,
                resource_target(counter, ResourceKind::SyncCounter, None),
                AccessMode::GET_ATTR,
                minor,
                Some(CLIENT_TWO),
                Some(CLIENT_TWO),
            );
            assert_recorded_resource(
                resources[1],
                CLIENT,
                resource_target(
                    context,
                    ResourceKind::RecordContext,
                    Some(Creation {
                        parent: None,
                        owner: CLIENT,
                    }),
                ),
                AccessMode::CREATE,
                minor,
                None,
                None,
            );
        } else {
            assert_eq!(resources.len(), 2, "{label} checks");
            assert_recorded_resource(
                resources[0],
                CLIENT,
                resource_target(context, ResourceKind::RecordContext, None),
                AccessMode::USE,
                minor,
                Some(CLIENT),
                Some(CLIENT),
            );
            assert_recorded_resource(
                resources[1],
                CLIENT,
                resource_target(counter, ResourceKind::SyncCounter, None),
                AccessMode::GET_ATTR,
                minor,
                Some(CLIENT_TWO),
                Some(CLIENT_TWO),
            );
        }
    }
}

#[test]
fn record_resource_specifier_denials_match_the_site_table() {
    // Mutation: ignore a specifier denial; UnregisterClients must preserve its registered client and range.
    for (label, minor) in [
        ("CreateContext", x11rec::CREATE_CONTEXT),
        ("RegisterClients", x11rec::REGISTER_CLIENTS),
        ("UnregisterClients", x11rec::UNREGISTER_CLIENTS),
    ] {
        let mut fixture = record_access_fixture();
        add_client(&mut fixture, CLIENT_TWO, CLIENT_TWO_BASE);
        set_client_range(&mut fixture, CLIENT, CLIENT_BASE);
        let context = ResourceId(CLIENT_BASE | 0x8001);
        if minor != x11rec::CREATE_CONTEXT {
            create_context(&mut fixture, CLIENT, context, &[CLIENT_BASE], &[]);
        }
        let counter = ResourceId(CLIENT_TWO_BASE | 0x8001);
        fixture.state.sync_counters.insert(
            counter.0,
            crate::server::SyncCounter {
                owner: CLIENT_TWO,
                value: 7,
            },
        );
        if minor == x11rec::UNREGISTER_CLIENTS {
            let range = x11rec::Range {
                core_requests: (1, 2),
                ..Default::default()
            };
            let setup = run_record_request(
                &mut fixture,
                CLIENT,
                2,
                x11rec::REGISTER_CLIENTS,
                &register_body(context, &[counter.0], &[range]),
            );
            assert!(setup.is_empty(), "Unregister setup failed: {setup:02x?}");
        }
        let before = format!("{:?}", fixture.state.record);
        let report_before =
            (minor != x11rec::CREATE_CONTEXT).then(|| get_context_report(&mut fixture, context));
        let calls = Arc::new(Mutex::new(Vec::new()));
        install_deny_policy(
            &mut fixture,
            resource_target(counter, ResourceKind::SyncCounter, None),
            AccessMode::GET_ATTR,
            &calls,
        );
        let body = if minor == x11rec::UNREGISTER_CLIENTS {
            unregister_body(context, &[counter.0])
        } else {
            register_body(context, &[counter.0], &[])
        };
        let sequence = if minor == x11rec::UNREGISTER_CLIENTS {
            3
        } else {
            2
        };

        let bytes = run_record_request(&mut fixture, CLIENT, sequence, minor, &body);

        assert_policy_error(&bytes, sequence, minor);
        assert_eq!(
            format!("{:?}", fixture.state.record),
            before,
            "{label} changed context state"
        );
        if let Some(report_before) = report_before {
            assert_eq!(
                get_context_report(&mut fixture, context),
                report_before,
                "{label} lists changed"
            );
        } else {
            assert!(
                !fixture.state.record.contains(context.0),
                "{label} inserted a context"
            );
        }
        assert!(
            fixture.backend.calls().is_empty(),
            "{label} reached backend"
        );
    }
}

#[test]
fn record_client_base_spec_is_not_asked() {
    // Mutation: check a client-base spec as a resource; its SyncCounter alias must expose the extra ask.
    let mut fixture = record_access_fixture();
    add_client(&mut fixture, CLIENT_TWO, CLIENT_TWO_BASE);
    set_client_range(&mut fixture, CLIENT, CLIENT_BASE);
    fixture.state.sync_counters.insert(
        CLIENT_TWO_BASE,
        crate::server::SyncCounter {
            owner: CLIENT_TWO,
            value: 7,
        },
    );
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .state
        .install_access_policy(Arc::new(RecordingExtensionPolicy::new(
            Decision::Allow,
            Arc::clone(&calls),
        )));

    let bytes = run_record_request(
        &mut fixture,
        CLIENT,
        1,
        x11rec::CREATE_CONTEXT,
        &register_body(ResourceId(CLIENT_BASE | 0x8001), &[CLIENT_TWO_BASE], &[]),
    );

    assert!(bytes.is_empty(), "CreateContext failed: {bytes:02x?}");
    let calls = calls.lock().unwrap();
    let resources = resource_calls(&calls);
    assert_eq!(resources.len(), 1, "only the new context is checked");
    assert_recorded_resource(
        resources[0],
        CLIENT,
        resource_target(
            ResourceId(CLIENT_BASE | 0x8001),
            ResourceKind::RecordContext,
            Some(Creation {
                parent: None,
                owner: CLIENT,
            }),
        ),
        AccessMode::CREATE,
        x11rec::CREATE_CONTEXT,
        None,
        None,
    );
}

fn get_context_report(fixture: &mut RecordAccessFixture, context: ResourceId) -> Vec<u8> {
    run_record_request(
        fixture,
        CLIENT,
        9,
        x11rec::GET_CONTEXT,
        &context_request_body(context),
    )
}

#[test]
fn register_denied_spec_keeps_lists() {
    // Mutation: ignore the denied spec check; GetContext must report the newly registered client.
    let mut fixture = record_access_fixture();
    add_client(&mut fixture, CLIENT_TWO, CLIENT_TWO_BASE);
    set_client_range(&mut fixture, CLIENT, CLIENT_BASE);
    let range = x11rec::Range {
        core_requests: (1, 2),
        ..Default::default()
    };
    let context = ResourceId(CLIENT_BASE | 0x8001);
    create_context(&mut fixture, CLIENT, context, &[CLIENT_BASE], &[range]);
    let before = get_context_report(&mut fixture, context);
    let counter = ResourceId(CLIENT_TWO_BASE | 0x8001);
    fixture.state.sync_counters.insert(
        counter.0,
        crate::server::SyncCounter {
            owner: CLIENT_TWO,
            value: 7,
        },
    );
    let calls = Arc::new(Mutex::new(Vec::new()));
    install_deny_policy(
        &mut fixture,
        resource_target(counter, ResourceKind::SyncCounter, None),
        AccessMode::GET_ATTR,
        &calls,
    );

    let bytes = run_record_request(
        &mut fixture,
        CLIENT,
        2,
        x11rec::REGISTER_CLIENTS,
        &register_body(context, &[counter.0], &[]),
    );

    assert_policy_error(&bytes, 2, x11rec::REGISTER_CLIENTS);
    let register_calls = calls.lock().unwrap().clone();
    assert_eq!(get_context_report(&mut fixture, context), before);
    assert!(
        fixture.backend.calls().is_empty(),
        "denied RegisterClients reached backend"
    );
    let resources = resource_calls(&register_calls);
    assert_eq!(resources.len(), 2, "context and denied spec checks");
    assert_recorded_resource(
        resources[0],
        CLIENT,
        resource_target(context, ResourceKind::RecordContext, None),
        AccessMode::USE,
        x11rec::REGISTER_CLIENTS,
        Some(CLIENT),
        Some(CLIENT),
    );
    assert_recorded_resource(
        resources[1],
        CLIENT,
        resource_target(counter, ResourceKind::SyncCounter, None),
        AccessMode::GET_ATTR,
        x11rec::REGISTER_CLIENTS,
        Some(CLIENT_TWO),
        Some(CLIENT_TWO),
    );
}

#[test]
fn record_denied_first_spec_before_second_spec_error() {
    // Mutation: defer all specifier checks until after the loop; the recorder BadMatch would win.
    let mut fixture = record_access_fixture();
    add_client(&mut fixture, CLIENT_TWO, CLIENT_TWO_BASE);
    add_client(&mut fixture, CLIENT_THREE, CLIENT_THREE_BASE);
    set_client_range(&mut fixture, CLIENT, CLIENT_BASE);
    let context = ResourceId(CLIENT_BASE | 0x8001);
    create_context(&mut fixture, CLIENT, context, &[], &[]);
    let enable_bytes = run_record_request(
        &mut fixture,
        CLIENT_TWO,
        2,
        x11rec::ENABLE_CONTEXT,
        &context_request_body(context),
    );
    assert_eq!(enable_bytes.len(), 32, "EnableContext start marker");
    let counter = ResourceId(CLIENT_THREE_BASE | 0x8001);
    fixture.state.sync_counters.insert(
        counter.0,
        crate::server::SyncCounter {
            owner: CLIENT_THREE,
            value: 7,
        },
    );
    let calls = Arc::new(Mutex::new(Vec::new()));
    install_deny_policy(
        &mut fixture,
        resource_target(counter, ResourceKind::SyncCounter, None),
        AccessMode::GET_ATTR,
        &calls,
    );
    let before = format!("{:?}", fixture.state.record);
    let recorder_resource = CLIENT_TWO_BASE | 0x8002;

    let bytes = run_record_request(
        &mut fixture,
        CLIENT,
        3,
        x11rec::REGISTER_CLIENTS,
        &register_body(context, &[counter.0, recorder_resource], &[]),
    );

    assert_policy_error(&bytes, 3, x11rec::REGISTER_CLIENTS);
    assert_eq!(
        format!("{:?}", fixture.state.record),
        before,
        "denied RegisterClients changed context state"
    );
    let calls = calls.lock().unwrap();
    let resources = resource_calls(&calls);
    assert_eq!(resources.len(), 2, "context and denied first spec checks");
    assert_recorded_resource(
        resources[0],
        CLIENT,
        resource_target(context, ResourceKind::RecordContext, None),
        AccessMode::USE,
        x11rec::REGISTER_CLIENTS,
        Some(CLIENT),
        Some(CLIENT),
    );
    assert_recorded_resource(
        resources[1],
        CLIENT,
        resource_target(counter, ResourceKind::SyncCounter, None),
        AccessMode::GET_ATTR,
        x11rec::REGISTER_CLIENTS,
        Some(CLIENT_THREE),
        Some(CLIENT_THREE),
    );
}

#[test]
fn create_context_denied_creates_nothing() {
    // Mutation: move CREATE after insertion; the same-server retry must then fail with BadIDChoice.
    let mut fixture = record_access_fixture();
    let context = ResourceId(CLIENT_BASE | 0x8001);
    set_client_range(&mut fixture, CLIENT, CLIENT_BASE);
    let target = resource_target(
        context,
        ResourceKind::RecordContext,
        Some(Creation {
            parent: None,
            owner: CLIENT,
        }),
    );
    let calls = Arc::new(Mutex::new(Vec::new()));
    install_deny_policy(&mut fixture, target, AccessMode::CREATE, &calls);

    let bytes = run_record_request(
        &mut fixture,
        CLIENT,
        1,
        x11rec::CREATE_CONTEXT,
        &register_body(context, &[], &[]),
    );

    assert_policy_error(&bytes, 1, x11rec::CREATE_CONTEXT);
    assert!(
        !fixture.state.record.contains(context.0),
        "denied context was inserted"
    );
    assert!(
        fixture.backend.calls().is_empty(),
        "denied CreateContext reached backend"
    );

    // Remove the denial and retry on the same server; the denied XID must remain reusable.
    fixture.state.host_access.policies.clear();
    let retry = run_record_request(
        &mut fixture,
        CLIENT,
        2,
        x11rec::CREATE_CONTEXT,
        &register_body(context, &[], &[]),
    );
    assert!(
        retry.is_empty(),
        "free context id could not be reused: {retry:02x?}"
    );
    assert!(fixture.state.record.contains(context.0));
}

#[test]
fn enable_context_denied_before_bad_match() {
    // Mutation: check USE after the already-enabled BadMatch; the request must then return BadMatch.
    let mut fixture = record_access_fixture();
    create_context(&mut fixture, CLIENT, CONTEXT, &[], &[]);
    let enabled = run_record_request(
        &mut fixture,
        CLIENT,
        2,
        x11rec::ENABLE_CONTEXT,
        &context_request_body(CONTEXT),
    );
    assert_eq!(enabled.len(), 32, "EnableContext start marker");
    let target = resource_target(CONTEXT, ResourceKind::RecordContext, None);
    let calls = Arc::new(Mutex::new(Vec::new()));
    install_deny_policy(&mut fixture, target, AccessMode::USE, &calls);
    let before = format!("{:?}", fixture.state.record);

    let bytes = run_record_request(
        &mut fixture,
        CLIENT,
        3,
        x11rec::ENABLE_CONTEXT,
        &context_request_body(CONTEXT),
    );

    assert_policy_error(&bytes, 3, x11rec::ENABLE_CONTEXT);
    assert_eq!(format!("{:?}", fixture.state.record), before);
    assert!(crate::core_loop::record::client_is_recording(
        &fixture.state,
        CLIENT
    ));
    let calls = calls.lock().unwrap();
    let resources = resource_calls(&calls);
    assert_eq!(
        resources.len(),
        1,
        "EnableContext must check before BadMatch"
    );
    assert_recorded_resource(
        resources[0],
        CLIENT,
        target,
        AccessMode::USE,
        x11rec::ENABLE_CONTEXT,
        Some(CLIENT),
        Some(CLIENT),
    );
}
