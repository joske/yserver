use super::{
    extensions::{ExtensionHook, RecordedExtensionCheck, RecordingExtensionPolicy},
    *,
};
use crate::{
    access::{AccessMode, Decision, DenyError, RequestOpcodes, ResourceKind, ResourceTarget},
    resources::BorderSource,
};
use std::sync::{Arc, Mutex};
use yserver_protocol::x11::screensaver as x11ss;

const CLIENT: ClientId = ClientId(1);
const OTHER: ClientId = ClientId(2);
const PIXMAP: ResourceId = ResourceId(0x0010_8B01);
const BORDER_PIXMAP: ResourceId = ResourceId(0x0010_8B06);
const CURSOR_SOURCE: ResourceId = ResourceId(0x0010_8B02);
const CURSOR: ResourceId = ResourceId(0x0010_8B03);
const COLORMAP: ResourceId = ResourceId(0x0010_8B04);
const UNKNOWN: ResourceId = ResourceId(0x0010_8B05);
const DENY_ERROR: DenyError = DenyError {
    code: x11::error::BAD_ACCESS,
    value: 0xA55A_1234,
};
const SAVER_MAJOR: u8 = crate::nested::MIT_SCREEN_SAVER_MAJOR_OPCODE;

struct SaverAccessFixture {
    state: ServerState,
    peer: UnixStream,
    other_peer: UnixStream,
    backend: RecordingBackend,
}

fn fixture() -> SaverAccessFixture {
    let mut state = ServerState::new();
    let peer = install_client(&mut state, CLIENT.0);
    let other_peer = install_client(&mut state, OTHER.0);
    SaverAccessFixture {
        state,
        peer,
        other_peer,
        backend: RecordingBackend::new(),
    }
}

fn run_wire(
    fixture: &mut SaverAccessFixture,
    client: ClientId,
    sequence: u16,
    opcode: u8,
    data: u8,
    body: &[u8],
) -> Vec<u8> {
    let peer = if client == CLIENT {
        &mut fixture.peer
    } else {
        &mut fixture.other_peer
    };
    process_request(
        &mut fixture.state,
        &mut fixture.backend,
        client,
        SequenceNumber(sequence),
        RequestHeader {
            opcode,
            data,
            length_units: u32::try_from(1 + body.len().div_ceil(4)).unwrap(),
        },
        body,
        None,
    )
    .expect("request through process_request");
    read_all_available(peer)
}

fn run_saver(
    fixture: &mut SaverAccessFixture,
    client: ClientId,
    sequence: u16,
    minor: u8,
    body: &[u8],
) -> Vec<u8> {
    run_wire(fixture, client, sequence, SAVER_MAJOR, minor, body)
}

fn words(values: &[u32]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

fn query_info_body(drawable: ResourceId) -> Vec<u8> {
    words(&[drawable.0])
}

fn select_input_body(drawable: ResourceId) -> Vec<u8> {
    words(&[drawable.0, 1])
}

fn unset_attributes_body(drawable: ResourceId) -> Vec<u8> {
    words(&[drawable.0])
}

fn set_attributes_body(drawable: ResourceId, mask: u32, values: &[u32]) -> Vec<u8> {
    let mut body = words(&[drawable.0]);
    for value in [0_u16, 0, 1, 1, 0] {
        body.extend_from_slice(&value.to_le_bytes());
    }
    body.extend_from_slice(&[0, 0]); // CopyFromParent class and depth.
    body.extend_from_slice(&0_u32.to_le_bytes()); // CopyFromParent visual.
    body.extend_from_slice(&mask.to_le_bytes());
    for value in values {
        body.extend_from_slice(&value.to_le_bytes());
    }
    body
}

fn create_pixmap(fixture: &mut SaverAccessFixture, owner: ClientId, id: ResourceId, depth: u8) {
    let body = words(&[id.0, ROOT_WINDOW.0, 8 | (8 << 16)]);
    let bytes = run_wire(fixture, owner, 1, 53, depth, &body);
    assert_no_error(&bytes, "fixture CreatePixmap");
}

fn create_colormap(fixture: &mut SaverAccessFixture, owner: ClientId, id: ResourceId) {
    let bytes = run_wire(
        fixture,
        owner,
        2,
        78,
        0,
        &words(&[id.0, ROOT_WINDOW.0, crate::resources::ROOT_VISUAL.0]),
    );
    assert_no_error(&bytes, "fixture CreateColormap");
}

fn create_cursor(fixture: &mut SaverAccessFixture, owner: ClientId) {
    let mut body = words(&[CURSOR.0, CURSOR_SOURCE.0, 0]);
    body.extend_from_slice(&[0; 16]);
    let bytes = run_wire(fixture, owner, 3, 93, 0, &body);
    assert_no_error(&bytes, "fixture CreateCursor");
}

fn create_value_resources(fixture: &mut SaverAccessFixture, owner: ClientId) {
    create_pixmap(fixture, owner, PIXMAP, crate::resources::ROOT_DEPTH);
    create_pixmap(fixture, owner, CURSOR_SOURCE, 1);
    create_cursor(fixture, owner);
    create_colormap(fixture, owner, COLORMAP);
}

fn resource_target(id: ResourceId, kind: ResourceKind) -> ResourceTarget {
    ResourceTarget {
        owner: None,
        id,
        kind,
        creation: None,
    }
}

fn install_allow_policy(
    fixture: &mut SaverAccessFixture,
    calls: &Arc<Mutex<Vec<RecordedExtensionCheck>>>,
) {
    fixture
        .state
        .install_access_policy(Arc::new(RecordingExtensionPolicy::new(
            Decision::Allow,
            Arc::clone(calls),
        )));
}

fn install_deny_policy(
    fixture: &mut SaverAccessFixture,
    id: ResourceId,
    mode: AccessMode,
    calls: &Arc<Mutex<Vec<RecordedExtensionCheck>>>,
) {
    fixture
        .state
        .install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
            Arc::clone(calls),
            id,
            Some(mode),
            DENY_ERROR,
        )));
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
    assert_eq!(bytes[10], SAVER_MAJOR);
}

fn assert_resource_call(
    call: &RecordedExtensionCheck,
    client: ClientId,
    id: ResourceId,
    kind: ResourceKind,
    mode: AccessMode,
    minor: u8,
    owner: ClientId,
) {
    assert_eq!(call.hook, ExtensionHook::Resource);
    assert_eq!(call.client, client);
    assert_eq!(call.target, Some(resource_target(id, kind)));
    assert_eq!(call.mode, Some(mode));
    assert_eq!(
        call.request,
        Some(RequestOpcodes {
            major: SAVER_MAJOR,
            minor,
        })
    );
    assert_eq!(call.owner, Some(owner));
    assert_eq!(call.target_owner, Some(owner));
}

#[derive(Clone)]
struct SaverSite {
    label: &'static str,
    minor: u8,
    body: Vec<u8>,
    target: ResourceId,
    kind: ResourceKind,
    mode: AccessMode,
    prior: Vec<(ResourceId, ResourceKind, AccessMode, ClientId)>,
    target_owner: ClientId,
}

fn site_table() -> Vec<SaverSite> {
    use AccessMode as M;
    use ResourceKind as K;

    let root_prior = vec![(
        ROOT_WINDOW,
        K::Window,
        M::GET_ATTR,
        crate::resources::SERVER_OWNER,
    )];
    vec![
        SaverSite {
            label: "QueryInfo window drawable",
            minor: x11ss::QUERY_INFO,
            body: query_info_body(ROOT_WINDOW),
            target: ROOT_WINDOW,
            kind: K::Window,
            mode: M::GET_ATTR,
            prior: Vec::new(),
            target_owner: crate::resources::SERVER_OWNER,
        },
        SaverSite {
            label: "QueryInfo pixmap drawable",
            minor: x11ss::QUERY_INFO,
            body: query_info_body(PIXMAP),
            target: PIXMAP,
            kind: K::Pixmap,
            mode: M::GET_ATTR,
            prior: Vec::new(),
            target_owner: CLIENT,
        },
        SaverSite {
            label: "SelectInput window drawable",
            minor: x11ss::SELECT_INPUT,
            body: select_input_body(ROOT_WINDOW),
            target: ROOT_WINDOW,
            kind: K::Window,
            mode: M::GET_ATTR,
            prior: Vec::new(),
            target_owner: crate::resources::SERVER_OWNER,
        },
        SaverSite {
            label: "SelectInput pixmap drawable",
            minor: x11ss::SELECT_INPUT,
            body: select_input_body(PIXMAP),
            target: PIXMAP,
            kind: K::Pixmap,
            mode: M::GET_ATTR,
            prior: Vec::new(),
            target_owner: CLIENT,
        },
        SaverSite {
            label: "SetAttributes window drawable",
            minor: x11ss::SET_ATTRIBUTES,
            body: set_attributes_body(ROOT_WINDOW, 0, &[]),
            target: ROOT_WINDOW,
            kind: K::Window,
            mode: M::GET_ATTR,
            prior: Vec::new(),
            target_owner: crate::resources::SERVER_OWNER,
        },
        SaverSite {
            label: "SetAttributes pixmap drawable",
            minor: x11ss::SET_ATTRIBUTES,
            body: set_attributes_body(PIXMAP, 0, &[]),
            target: PIXMAP,
            kind: K::Pixmap,
            mode: M::GET_ATTR,
            prior: Vec::new(),
            target_owner: CLIENT,
        },
        SaverSite {
            label: "SetAttributes background pixmap",
            minor: x11ss::SET_ATTRIBUTES,
            body: set_attributes_body(ROOT_WINDOW, 0x1, &[PIXMAP.0]),
            target: PIXMAP,
            kind: K::Pixmap,
            mode: M::READ,
            prior: root_prior.clone(),
            target_owner: CLIENT,
        },
        SaverSite {
            label: "SetAttributes border pixmap",
            minor: x11ss::SET_ATTRIBUTES,
            body: set_attributes_body(ROOT_WINDOW, 0x4, &[PIXMAP.0]),
            target: PIXMAP,
            kind: K::Pixmap,
            mode: M::READ,
            prior: root_prior.clone(),
            target_owner: CLIENT,
        },
        SaverSite {
            label: "SetAttributes colormap",
            minor: x11ss::SET_ATTRIBUTES,
            body: set_attributes_body(ROOT_WINDOW, 0x2000, &[COLORMAP.0]),
            target: COLORMAP,
            kind: K::Colormap,
            mode: M::USE,
            prior: root_prior.clone(),
            target_owner: CLIENT,
        },
        SaverSite {
            label: "SetAttributes cursor",
            minor: x11ss::SET_ATTRIBUTES,
            body: set_attributes_body(ROOT_WINDOW, 0x4000, &[CURSOR.0]),
            target: CURSOR,
            kind: K::Cursor,
            mode: M::USE,
            prior: root_prior,
            target_owner: CLIENT,
        },
        SaverSite {
            label: "UnsetAttributes window drawable",
            minor: x11ss::UNSET_ATTRIBUTES,
            body: unset_attributes_body(ROOT_WINDOW),
            target: ROOT_WINDOW,
            kind: K::Window,
            mode: M::GET_ATTR,
            prior: Vec::new(),
            target_owner: crate::resources::SERVER_OWNER,
        },
        SaverSite {
            label: "UnsetAttributes pixmap drawable",
            minor: x11ss::UNSET_ATTRIBUTES,
            body: unset_attributes_body(PIXMAP),
            target: PIXMAP,
            kind: K::Pixmap,
            mode: M::GET_ATTR,
            prior: Vec::new(),
            target_owner: CLIENT,
        },
    ]
}

#[test]
fn saver_request_denials_and_asks_match_the_site_table() {
    // Mutation: omit a listed check or unset before checking; the DENY/error and unchanged-state assertions must fail.
    for site in site_table() {
        let mut denied = fixture();
        create_value_resources(&mut denied, CLIENT);
        if site.minor == x11ss::UNSET_ATTRIBUTES {
            let bytes = run_saver(
                &mut denied,
                CLIENT,
                1,
                x11ss::SET_ATTRIBUTES,
                &set_attributes_body(ROOT_WINDOW, 0, &[]),
            );
            assert!(
                bytes.is_empty(),
                "fixture SetAttributes failed: {bytes:02x?}"
            );
            let bytes = activate_saver(&mut denied, 2);
            assert!(bytes.is_empty(), "fixture activation failed: {bytes:02x?}");
            assert!(denied.state.screensaver.window_shown);
            assert!(
                denied
                    .state
                    .resources
                    .window(crate::resources::SCREEN_SAVER_WINDOW)
                    .is_some()
            );
        }
        let calls = Arc::new(Mutex::new(Vec::new()));
        install_deny_policy(&mut denied, site.target, site.mode, &calls);
        let before_attrs = denied.state.screensaver.attributes.clone();
        let before_selected = denied.state.screensaver.selected_by.clone();
        let before_window_shown = denied.state.screensaver.window_shown;
        let before_saver_window = denied
            .state
            .resources
            .window(crate::resources::SCREEN_SAVER_WINDOW)
            .is_some();
        let before_backend = denied.backend.calls().to_vec();
        let bytes = run_saver(&mut denied, CLIENT, 7, site.minor, &site.body);
        assert_policy_error(&bytes, 7, site.minor);
        assert_eq!(
            denied.state.screensaver.attributes, before_attrs,
            "{}",
            site.label
        );
        assert_eq!(
            denied.state.screensaver.selected_by, before_selected,
            "{}",
            site.label
        );
        assert_eq!(
            denied.state.screensaver.window_shown, before_window_shown,
            "{}",
            site.label
        );
        assert_eq!(
            denied
                .state
                .resources
                .window(crate::resources::SCREEN_SAVER_WINDOW)
                .is_some(),
            before_saver_window,
            "{}",
            site.label
        );
        assert_eq!(
            denied.backend.calls(),
            before_backend,
            "{} reached backend",
            site.label
        );
        let calls = calls.lock().unwrap();
        let resources = resource_calls(&calls);
        assert_eq!(
            resources.len(),
            site.prior.len() + 1,
            "{} checks",
            site.label
        );
        for (call, (id, kind, mode, owner)) in resources.iter().zip(site.prior.iter()) {
            assert_resource_call(call, CLIENT, *id, *kind, *mode, site.minor, *owner);
        }
        assert_resource_call(
            resources[site.prior.len()],
            CLIENT,
            site.target,
            site.kind,
            site.mode,
            site.minor,
            site.target_owner,
        );

        let mut allowed = fixture();
        create_value_resources(&mut allowed, CLIENT);
        let calls = Arc::new(Mutex::new(Vec::new()));
        install_allow_policy(&mut allowed, &calls);
        let bytes = run_saver(&mut allowed, CLIENT, 8, site.minor, &site.body);
        assert_ne!(
            bytes.first(),
            Some(&0),
            "{} returned an X error",
            site.label
        );
        let calls = calls.lock().unwrap();
        let resources = resource_calls(&calls);
        assert_eq!(
            resources.len(),
            site.prior.len() + 1,
            "{} checks",
            site.label
        );
        for (call, (id, kind, mode, owner)) in resources.iter().zip(site.prior.iter()) {
            assert_resource_call(call, CLIENT, *id, *kind, *mode, site.minor, *owner);
        }
        assert_resource_call(
            resources[site.prior.len()],
            CLIENT,
            site.target,
            site.kind,
            site.mode,
            site.minor,
            site.target_owner,
        );
    }
}

#[test]
fn saver_set_attributes_value_resources_in_mask_order() {
    // Mutation: reorder any value-resource check; this exact request-opcode and target-order assertion must fail.
    let mut fixture = fixture();
    create_value_resources(&mut fixture, CLIENT);
    create_pixmap(
        &mut fixture,
        CLIENT,
        BORDER_PIXMAP,
        crate::resources::ROOT_DEPTH,
    );
    let calls = Arc::new(Mutex::new(Vec::new()));
    install_allow_policy(&mut fixture, &calls);
    let body = set_attributes_body(
        ROOT_WINDOW,
        0x1 | 0x4 | 0x2000 | 0x4000,
        &[PIXMAP.0, BORDER_PIXMAP.0, COLORMAP.0, CURSOR.0],
    );

    let bytes = run_saver(&mut fixture, CLIENT, 11, x11ss::SET_ATTRIBUTES, &body);
    assert!(bytes.is_empty(), "SetAttributes failed: {bytes:02x?}");
    let calls = calls.lock().unwrap();
    let resources = resource_calls(&calls);
    assert_eq!(resources.len(), 5);
    for (call, (id, kind, mode, owner)) in resources.iter().zip([
        (
            ROOT_WINDOW,
            ResourceKind::Window,
            AccessMode::GET_ATTR,
            crate::resources::SERVER_OWNER,
        ),
        (PIXMAP, ResourceKind::Pixmap, AccessMode::READ, CLIENT),
        (
            BORDER_PIXMAP,
            ResourceKind::Pixmap,
            AccessMode::READ,
            CLIENT,
        ),
        (COLORMAP, ResourceKind::Colormap, AccessMode::USE, CLIENT),
        (CURSOR, ResourceKind::Cursor, AccessMode::USE, CLIENT),
    ]) {
        assert_resource_call(call, CLIENT, id, kind, mode, x11ss::SET_ATTRIBUTES, owner);
    }
}

#[test]
fn saver_set_attributes_value_resource_sentinels_are_not_asked() {
    // Mutation: treat a value sentinel as a resource operand; its sentinel row must add an access call.
    let sentinels = [
        ("background None", 0x1, 0),
        ("background ParentRelative", 0x1, 1),
        ("border CopyFromParent", 0x4, 0),
        ("colormap CopyFromParent", 0x2000, 0),
        ("cursor None", 0x4000, 0),
    ];
    for (label, mask, value) in sentinels {
        let mut fixture = fixture();
        create_value_resources(&mut fixture, CLIENT);
        let calls = Arc::new(Mutex::new(Vec::new()));
        install_allow_policy(&mut fixture, &calls);
        let bytes = run_saver(
            &mut fixture,
            CLIENT,
            12,
            x11ss::SET_ATTRIBUTES,
            &set_attributes_body(ROOT_WINDOW, mask, &[value]),
        );
        assert!(bytes.is_empty(), "{label} failed: {bytes:02x?}");
        let calls = calls.lock().unwrap();
        let resources = resource_calls(&calls);
        assert_eq!(resources.len(), 1, "{label} must check only the drawable");
        assert_resource_call(
            resources[0],
            CLIENT,
            ROOT_WINDOW,
            ResourceKind::Window,
            AccessMode::GET_ATTR,
            x11ss::SET_ATTRIBUTES,
            crate::resources::SERVER_OWNER,
        );
    }
}

#[test]
fn saver_set_attributes_denied_keeps_old_attrs() {
    // Mutation: store the replacement before checking its value resources; activation must still use the old attributes.
    let mut fixture = fixture();
    create_value_resources(&mut fixture, OTHER);
    let old_body = set_attributes_body(ROOT_WINDOW, 0, &[]);
    let bytes = run_saver(&mut fixture, CLIENT, 1, x11ss::SET_ATTRIBUTES, &old_body);
    assert!(bytes.is_empty());
    let old = fixture
        .state
        .screensaver
        .attributes
        .clone()
        .expect("old attrs");
    let before_backend = fixture.backend.calls().to_vec();
    let calls = Arc::new(Mutex::new(Vec::new()));
    install_deny_policy(&mut fixture, PIXMAP, AccessMode::READ, &calls);
    let replacement = set_attributes_body(ROOT_WINDOW, 0x1, &[PIXMAP.0]);

    let bytes = run_saver(&mut fixture, CLIENT, 2, x11ss::SET_ATTRIBUTES, &replacement);
    assert_policy_error(&bytes, 2, x11ss::SET_ATTRIBUTES);
    assert_eq!(fixture.state.screensaver.attributes.as_ref(), Some(&old));
    assert_eq!(fixture.backend.calls(), before_backend);
    let calls = calls.lock().unwrap();
    let resources = resource_calls(&calls);
    assert_eq!(resources.len(), 2);
    assert_resource_call(
        resources[0],
        CLIENT,
        ROOT_WINDOW,
        ResourceKind::Window,
        AccessMode::GET_ATTR,
        x11ss::SET_ATTRIBUTES,
        crate::resources::SERVER_OWNER,
    );
    assert_resource_call(
        resources[1],
        CLIENT,
        PIXMAP,
        ResourceKind::Pixmap,
        AccessMode::READ,
        x11ss::SET_ATTRIBUTES,
        OTHER,
    );
    drop(calls);

    let bytes = run_wire(&mut fixture, CLIENT, 3, 115, 1, &[]);
    assert!(bytes.is_empty(), "ForceScreenSaver failed: {bytes:02x?}");
    let window = fixture
        .state
        .resources
        .window(crate::resources::SCREEN_SAVER_WINDOW)
        .expect("saver window from retained attributes");
    assert_eq!((window.width, window.height), (1, 1));
}

#[test]
fn saver_owner_conflict_precedes_value_checks() {
    // Mutation: move value checks before the owner-conflict BadAccess; the policy check must not run.
    let mut fixture = fixture();
    create_value_resources(&mut fixture, OTHER);
    let body = set_attributes_body(ROOT_WINDOW, 0, &[]);
    assert!(run_saver(&mut fixture, CLIENT, 1, x11ss::SET_ATTRIBUTES, &body).is_empty());
    let before_backend = fixture.backend.calls().to_vec();
    let calls = Arc::new(Mutex::new(Vec::new()));
    install_deny_policy(&mut fixture, PIXMAP, AccessMode::READ, &calls);

    let bytes = run_saver(
        &mut fixture,
        OTHER,
        2,
        x11ss::SET_ATTRIBUTES,
        &set_attributes_body(ROOT_WINDOW, 0x1, &[PIXMAP.0]),
    );
    assert_eq!(bytes.len(), 32);
    assert_eq!((bytes[0], bytes[1]), (0, x11::error::BAD_ACCESS));
    assert_eq!(
        u16::from_le_bytes(bytes[8..10].try_into().unwrap()),
        u16::from(x11ss::SET_ATTRIBUTES)
    );
    assert_eq!(
        fixture
            .state
            .screensaver
            .attributes
            .as_ref()
            .unwrap()
            .client,
        CLIENT
    );
    let calls = calls.lock().unwrap();
    let resources = resource_calls(&calls);
    assert_eq!(
        resources.len(),
        1,
        "only the drawable check precedes BadAccess"
    );
    assert_resource_call(
        resources[0],
        OTHER,
        ROOT_WINDOW,
        ResourceKind::Window,
        AccessMode::GET_ATTR,
        x11ss::SET_ATTRIBUTES,
        crate::resources::SERVER_OWNER,
    );
    assert_eq!(fixture.backend.calls(), before_backend);
}

#[test]
fn saver_query_info_unresolved_drawable_is_not_asked() {
    // Mutation: send an unresolved QueryInfo drawable directly to the policy; no check may be recorded.
    let mut fixture = fixture();
    let calls = Arc::new(Mutex::new(Vec::new()));
    install_allow_policy(&mut fixture, &calls);

    let bytes = run_saver(
        &mut fixture,
        CLIENT,
        1,
        x11ss::QUERY_INFO,
        &query_info_body(UNKNOWN),
    );
    assert_ne!(bytes.first(), Some(&0), "QueryInfo failed: {bytes:02x?}");
    assert!(resource_calls(&calls.lock().unwrap()).is_empty());
}

fn set_deferred_saver_values(fixture: &mut SaverAccessFixture, pixmap: ResourceId) {
    let body = set_attributes_body(ROOT_WINDOW, 0x7, &[0, pixmap.0, 0]);
    let bytes = run_saver(fixture, CLIENT, 1, x11ss::SET_ATTRIBUTES, &body);
    assert!(bytes.is_empty(), "SetAttributes failed: {bytes:02x?}");
}

fn activate_saver(fixture: &mut SaverAccessFixture, sequence: u16) -> Vec<u8> {
    run_wire(fixture, CLIENT, sequence, 115, 1, &[])
}

#[test]
fn saver_activation_denied_shifted_border_pixmap_is_dropped() {
    // Mutation: check only the original SetAttributes operands; the shifted border pixmap must be denied at activation.
    let mut fixture = fixture();
    create_pixmap(&mut fixture, OTHER, PIXMAP, crate::resources::ROOT_DEPTH);
    let calls = Arc::new(Mutex::new(Vec::new()));
    install_deny_policy(&mut fixture, PIXMAP, AccessMode::READ, &calls);
    set_deferred_saver_values(&mut fixture, PIXMAP);
    let after_install = resource_calls(&calls.lock().unwrap()).len();
    assert_eq!(
        after_install, 1,
        "original background-pixel operand is not a resource"
    );

    let bytes = activate_saver(&mut fixture, 2);
    assert!(
        bytes.is_empty(),
        "activation has no request error: {bytes:02x?}"
    );
    let calls = calls.lock().unwrap();
    let resources = resource_calls(&calls);
    assert_eq!(resources.len(), 2);
    assert_resource_call(
        resources[1],
        CLIENT,
        PIXMAP,
        ResourceKind::Pixmap,
        AccessMode::READ,
        x11ss::SET_ATTRIBUTES,
        OTHER,
    );
    let window = fixture
        .state
        .resources
        .window(crate::resources::SCREEN_SAVER_WINDOW)
        .expect("saver window should be created after neutralizing denied border");
    let root_border = fixture
        .state
        .resources
        .window(ROOT_WINDOW)
        .expect("root window")
        .border;
    assert_eq!(
        window.border, root_border,
        "denied border pixmap is replaced by CopyFromParent"
    );
    assert_eq!(
        fixture
            .state
            .screensaver
            .attributes
            .as_ref()
            .unwrap()
            .client,
        CLIENT
    );
    drop(calls);

    let mut asked = self::fixture();
    create_pixmap(&mut asked, OTHER, PIXMAP, crate::resources::ROOT_DEPTH);
    let calls = Arc::new(Mutex::new(Vec::new()));
    install_allow_policy(&mut asked, &calls);
    set_deferred_saver_values(&mut asked, PIXMAP);
    let bytes = activate_saver(&mut asked, 2);
    assert!(bytes.is_empty(), "allowed activation failed: {bytes:02x?}");
    let calls = calls.lock().unwrap();
    let resources = resource_calls(&calls);
    assert_eq!(resources.len(), 2);
    assert_resource_call(
        resources[1],
        CLIENT,
        PIXMAP,
        ResourceKind::Pixmap,
        AccessMode::READ,
        x11ss::SET_ATTRIBUTES,
        OTHER,
    );
    assert_eq!(
        asked
            .state
            .resources
            .window(crate::resources::SCREEN_SAVER_WINDOW)
            .unwrap()
            .border,
        BorderSource::Pixmap {
            id: PIXMAP,
            host_xid: asked
                .state
                .resources
                .pixmap(PIXMAP)
                .and_then(|pixmap| pixmap.host_xid),
        }
    );
}

#[test]
fn saver_activation_resource_created_after_install_is_asked() {
    // Mutation: check only at SetAttributes; a pixmap created later must still be checked and dropped at activation.
    let mut fixture = fixture();
    let calls = Arc::new(Mutex::new(Vec::new()));
    install_deny_policy(&mut fixture, PIXMAP, AccessMode::READ, &calls);
    set_deferred_saver_values(&mut fixture, PIXMAP);
    assert_eq!(resource_calls(&calls.lock().unwrap()).len(), 1);
    create_pixmap(&mut fixture, OTHER, PIXMAP, crate::resources::ROOT_DEPTH);
    let before_activation = resource_calls(&calls.lock().unwrap()).len();

    let bytes = activate_saver(&mut fixture, 3);
    assert!(
        bytes.is_empty(),
        "activation has no request error: {bytes:02x?}"
    );
    let calls = calls.lock().unwrap();
    let resources = resource_calls(&calls);
    assert_eq!(resources.len(), before_activation + 1);
    assert_resource_call(
        resources[before_activation],
        CLIENT,
        PIXMAP,
        ResourceKind::Pixmap,
        AccessMode::READ,
        x11ss::SET_ATTRIBUTES,
        OTHER,
    );
    let window = fixture
        .state
        .resources
        .window(crate::resources::SCREEN_SAVER_WINDOW)
        .expect("saver window after denied border is neutralized");
    let root_border = fixture.state.resources.window(ROOT_WINDOW).unwrap().border;
    assert_eq!(window.border, root_border);
}

#[test]
fn saver_activation_no_policy_is_unchanged() {
    // Mutation: replace the effective border pixmap before CreateWindow; without policy the original XID must remain.
    let mut fixture = fixture();
    create_pixmap(&mut fixture, OTHER, PIXMAP, crate::resources::ROOT_DEPTH);
    set_deferred_saver_values(&mut fixture, PIXMAP);

    let bytes = activate_saver(&mut fixture, 2);
    assert!(bytes.is_empty(), "activation failed: {bytes:02x?}");
    let window = fixture
        .state
        .resources
        .window(crate::resources::SCREEN_SAVER_WINDOW)
        .expect("saver window");
    assert_eq!(
        window.border,
        BorderSource::Pixmap {
            id: PIXMAP,
            host_xid: fixture
                .state
                .resources
                .pixmap(PIXMAP)
                .and_then(|pixmap| pixmap.host_xid),
        }
    );
}

#[test]
fn saver_activation_unresolved_effective_operand_is_not_asked() {
    // Mutation: validate the derived XID without resolving it; the policy must not see an unknown effective pixmap.
    let mut fixture = fixture();
    let calls = Arc::new(Mutex::new(Vec::new()));
    install_deny_policy(&mut fixture, UNKNOWN, AccessMode::READ, &calls);
    set_deferred_saver_values(&mut fixture, UNKNOWN);

    let bytes = activate_saver(&mut fixture, 2);
    assert!(
        bytes.is_empty(),
        "activation has no request error: {bytes:02x?}"
    );
    let calls = calls.lock().unwrap();
    assert!(
        resource_calls(&calls)
            .iter()
            .all(|call| call.target.is_none_or(|target| target.id != UNKNOWN)),
        "unresolved derived operand was asked"
    );
    assert!(
        fixture
            .state
            .resources
            .window(crate::resources::SCREEN_SAVER_WINDOW)
            .is_none(),
        "unresolved border pixmap keeps today's failed CreateWindow behavior"
    );
    assert!(!fixture.state.screensaver.window_shown);
}
