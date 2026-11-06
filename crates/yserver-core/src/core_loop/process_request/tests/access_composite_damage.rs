use super::{
    access_resources::{resource_request, words},
    extensions::{ExtensionHook, RecordedExtensionCheck, RecordingExtensionPolicy},
    *,
};
use crate::access::{
    AccessMode, Creation, Decision, DenyError, RequestOpcodes, ResourceKind, ResourceTarget,
};

const CLIENT: ClientId = ClientId(1);

// ---- Task 7: COMPOSITE and DAMAGE resource access ---------------------

const CD_WINDOW: ResourceId = ResourceId(0x0010_7001);
const CD_PIXMAP: ResourceId = ResourceId(0x0020_7001);
const CD_NEW_PIXMAP: ResourceId = ResourceId(0x0020_7002);
const CD_REGION_REPAIR: ResourceId = ResourceId(0x0030_7001);
const CD_REGION_PARTS: ResourceId = ResourceId(0x0030_7002);
const CD_DAMAGE: ResourceId = ResourceId(0x0080_7001);
const CD_DAMAGE_OTHER: ResourceId = ResourceId(0x0080_7002);
const CD_DAMAGE_NEW: ResourceId = ResourceId(0x0080_7003);

fn composite_damage_access_fixture() -> (ServerState, UnixStream, RecordingBackend) {
    let mut state = ServerState::new();
    let peer = install_client(&mut state, CLIENT.0);
    state.resources.create_window(
        CLIENT,
        CreateWindowRequest {
            depth: 24,
            window: CD_WINDOW,
            parent: ROOT_WINDOW,
            width: 80,
            height: 60,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    {
        let window = state.resources.window_mut(CD_WINDOW).expect("test window");
        window.host_xid = Some(crate::backend::WindowHandle::from_raw_for_test(0x0040_7001));
        window.map_state = crate::resources::MapState::Viewable;
    }
    state.resources.create_pixmap(
        CLIENT,
        CreatePixmapRequest {
            pixmap: CD_PIXMAP,
            drawable: ROOT_WINDOW,
            width: 80,
            height: 60,
            depth: 24,
        },
    );
    for (id, x) in [(CD_REGION_REPAIR, 0), (CD_REGION_PARTS, 10)] {
        state.xfixes_regions.insert(
            id.0,
            crate::server::XFixesRegion {
                owner: CLIENT,
                region: crate::region::Region::from_rects(&[
                    yserver_protocol::x11::xfixes::RegionRect {
                        x,
                        y: 0,
                        width: 8,
                        height: 8,
                    },
                ]),
            },
        );
    }
    state
        .damage_objects
        .insert(CD_DAMAGE.0, cd_damage_object(CD_DAMAGE));
    state
        .damage_objects
        .insert(CD_DAMAGE_OTHER.0, cd_damage_object(CD_DAMAGE_OTHER));
    state
        .composite_redirects
        .redirect_window(
            CD_WINDOW,
            crate::server::RedirectRecord {
                mode: crate::server::CompositeRedirectMode::Automatic,
                owner: CLIENT,
            },
        )
        .unwrap();
    state
        .composite_redirects
        .redirect_subwindows(
            ROOT_WINDOW,
            &[CD_WINDOW],
            crate::server::RedirectRecord {
                mode: crate::server::CompositeRedirectMode::Automatic,
                owner: CLIENT,
            },
        )
        .unwrap();
    let backend = RecordingBackend::new()
        .with_composite_support()
        .with_redirect_activation();
    (state, peer, backend)
}

fn cd_damage_object(_id: ResourceId) -> crate::server::DamageObject {
    crate::server::DamageObject {
        owner: CLIENT,
        drawable: CD_WINDOW,
        level: yserver_protocol::x11::damage::report_level::RAW_RECTANGLES,
        rects: vec![yserver_protocol::x11::xfixes::RegionRect {
            x: 1,
            y: 2,
            width: 3,
            height: 4,
        }],
        pending_notify_fired: true,
        last_reported_geometry: None,
        list_pos: 3,
        created: 3,
    }
}

fn composite_damage_request(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    peer: &mut UnixStream,
    major: u8,
    minor: u8,
    body: &[u8],
) -> Vec<u8> {
    resource_request(state, backend, peer, 1, major, minor, body)
}

fn cd_target(id: ResourceId, kind: ResourceKind, creation: Option<Creation>) -> ResourceTarget {
    ResourceTarget {
        owner: None,
        id,
        kind,
        creation,
    }
}

fn cd_damage_body(id: ResourceId) -> Vec<u8> {
    words(&[id.0, CD_WINDOW.0, 0])
}

type CompositeDamageSnapshot = (
    Vec<crate::server::RedirectRecord>,
    Vec<crate::server::RedirectRecord>,
    Vec<crate::server::RedirectRecord>,
    bool,
    usize,
    HashMap<u32, crate::server::DamageObject>,
    HashMap<u32, crate::server::XFixesRegion>,
    Vec<ClientId>,
    bool,
);

fn cd_snapshot(state: &ServerState) -> CompositeDamageSnapshot {
    (
        state.composite_redirects.window_records(CD_WINDOW).to_vec(),
        state
            .composite_redirects
            .subwindows_records(ROOT_WINDOW)
            .to_vec(),
        state
            .composite_redirects
            .window_records(ROOT_WINDOW)
            .to_vec(),
        state
            .resources
            .window(CD_WINDOW)
            .is_some_and(|window| window.redirected_backing.is_some()),
        state
            .resources
            .window(CD_WINDOW)
            .map_or(0, |window| window.composite_named_pixmaps.len()),
        state.damage_objects.clone(),
        state.xfixes_regions.clone(),
        state.cow_claims.clone(),
        state.resources.pixmap(CD_NEW_PIXMAP).is_some(),
    )
}

fn assert_policy_x_error(bytes: &[u8], major: u8, minor: u8) {
    assert_eq!(bytes.len(), 32, "expected one X error, got {bytes:02x?}");
    assert_eq!(bytes[0], 0, "expected an error packet");
    assert_eq!(bytes[1], x11::error::BAD_ACCESS);
    assert_eq!(
        u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
        0xA55A_1234,
        "policy error value must be forwarded verbatim"
    );
    assert_eq!(
        u16::from_le_bytes(bytes[8..10].try_into().unwrap()),
        u16::from(minor)
    );
    assert_eq!(bytes[10], major);
}

#[derive(Clone)]
struct CompositeDamageDenyRow {
    label: &'static str,
    major: u8,
    minor: u8,
    body: Vec<u8>,
    target: ResourceTarget,
    mode: AccessMode,
}

fn composite_damage_deny_rows() -> Vec<CompositeDamageDenyRow> {
    use yserver_protocol::x11::{composite, damage};
    let redirect_mode = AccessMode::SET_ATTR | AccessMode::MANAGE | AccessMode::BLEND;
    vec![
        CompositeDamageDenyRow {
            label: "Composite RedirectWindow",
            major: 144,
            minor: composite::REDIRECT_WINDOW,
            body: words(&[CD_WINDOW.0, 0]),
            target: cd_target(CD_WINDOW, ResourceKind::Window, None),
            mode: redirect_mode,
        },
        CompositeDamageDenyRow {
            label: "Composite RedirectSubwindows parent",
            major: 144,
            minor: composite::REDIRECT_SUBWINDOWS,
            body: words(&[ROOT_WINDOW.0, 0]),
            target: cd_target(ROOT_WINDOW, ResourceKind::Window, None),
            mode: redirect_mode,
        },
        CompositeDamageDenyRow {
            label: "Composite UnredirectWindow",
            major: 144,
            minor: composite::UNREDIRECT_WINDOW,
            body: words(&[CD_WINDOW.0, 0]),
            target: cd_target(CD_WINDOW, ResourceKind::Window, None),
            mode: redirect_mode,
        },
        CompositeDamageDenyRow {
            label: "Composite UnredirectSubwindows parent",
            major: 144,
            minor: composite::UNREDIRECT_SUBWINDOWS,
            body: words(&[ROOT_WINDOW.0, 0]),
            target: cd_target(ROOT_WINDOW, ResourceKind::Window, None),
            mode: redirect_mode,
        },
        CompositeDamageDenyRow {
            label: "Composite NameWindowPixmap window",
            major: 144,
            minor: composite::NAME_WINDOW_PIXMAP,
            body: words(&[CD_WINDOW.0, CD_NEW_PIXMAP.0]),
            target: cd_target(CD_WINDOW, ResourceKind::Window, None),
            mode: AccessMode::GET_ATTR,
        },
        CompositeDamageDenyRow {
            label: "Composite NameWindowPixmap new pixmap",
            major: 144,
            minor: composite::NAME_WINDOW_PIXMAP,
            body: words(&[CD_WINDOW.0, CD_NEW_PIXMAP.0]),
            target: cd_target(
                CD_NEW_PIXMAP,
                ResourceKind::Pixmap,
                Some(Creation {
                    parent: Some(CD_WINDOW),
                    owner: CLIENT,
                }),
            ),
            mode: AccessMode::CREATE,
        },
        CompositeDamageDenyRow {
            label: "Composite GetOverlayWindow operand",
            major: 144,
            minor: composite::GET_OVERLAY_WINDOW,
            body: words(&[ROOT_WINDOW.0]),
            target: cd_target(ROOT_WINDOW, ResourceKind::Window, None),
            mode: AccessMode::GET_ATTR,
        },
        CompositeDamageDenyRow {
            label: "Composite ReleaseOverlayWindow operand",
            major: 144,
            minor: composite::RELEASE_OVERLAY_WINDOW,
            body: words(&[ROOT_WINDOW.0]),
            target: cd_target(ROOT_WINDOW, ResourceKind::Window, None),
            mode: AccessMode::GET_ATTR,
        },
        CompositeDamageDenyRow {
            label: "DAMAGE Create drawable",
            major: 143,
            minor: damage::CREATE,
            body: cd_damage_body(CD_DAMAGE_NEW),
            target: cd_target(CD_WINDOW, ResourceKind::Window, None),
            mode: AccessMode::GET_ATTR | AccessMode::READ,
        },
        CompositeDamageDenyRow {
            label: "DAMAGE Create new damage",
            major: 143,
            minor: damage::CREATE,
            body: cd_damage_body(CD_DAMAGE_NEW),
            target: cd_target(
                CD_DAMAGE_NEW,
                ResourceKind::Damage,
                Some(Creation {
                    parent: Some(CD_WINDOW),
                    owner: CLIENT,
                }),
            ),
            mode: AccessMode::CREATE,
        },
        CompositeDamageDenyRow {
            label: "DAMAGE Destroy",
            major: 143,
            minor: damage::DESTROY,
            body: words(&[CD_DAMAGE.0]),
            target: cd_target(CD_DAMAGE, ResourceKind::Damage, None),
            mode: AccessMode::DESTROY,
        },
        CompositeDamageDenyRow {
            label: "DAMAGE Subtract damage",
            major: 143,
            minor: damage::SUBTRACT,
            body: words(&[CD_DAMAGE.0, CD_REGION_REPAIR.0, CD_REGION_PARTS.0]),
            target: cd_target(CD_DAMAGE, ResourceKind::Damage, None),
            mode: AccessMode::WRITE,
        },
        CompositeDamageDenyRow {
            label: "DAMAGE Subtract repair region",
            major: 143,
            minor: damage::SUBTRACT,
            body: words(&[CD_DAMAGE.0, CD_REGION_REPAIR.0, CD_REGION_PARTS.0]),
            target: cd_target(CD_REGION_REPAIR, ResourceKind::Region, None),
            mode: AccessMode::WRITE,
        },
        CompositeDamageDenyRow {
            label: "DAMAGE Subtract parts region",
            major: 143,
            minor: damage::SUBTRACT,
            body: words(&[CD_DAMAGE.0, CD_REGION_REPAIR.0, CD_REGION_PARTS.0]),
            target: cd_target(CD_REGION_PARTS, ResourceKind::Region, None),
            mode: AccessMode::WRITE,
        },
        CompositeDamageDenyRow {
            label: "DAMAGE Add drawable",
            major: 143,
            minor: damage::ADD,
            body: words(&[CD_WINDOW.0, CD_REGION_REPAIR.0]),
            target: cd_target(CD_WINDOW, ResourceKind::Window, None),
            mode: AccessMode::WRITE,
        },
        CompositeDamageDenyRow {
            label: "DAMAGE Add region",
            major: 143,
            minor: damage::ADD,
            body: words(&[CD_WINDOW.0, CD_REGION_REPAIR.0]),
            target: cd_target(CD_REGION_REPAIR, ResourceKind::Region, None),
            mode: AccessMode::WRITE,
        },
    ]
}

#[test]
fn composite_damage_resource_denials_match_the_site_table() {
    // Mutation: omit the 144/1 Window check; this table's exact BadAccess value assertion must fail.
    let error = DenyError {
        code: x11::error::BAD_ACCESS,
        value: 0xA55A_1234,
    };
    for row in composite_damage_deny_rows() {
        let (mut state, mut peer, mut backend) = composite_damage_access_fixture();
        if row.minor == yserver_protocol::x11::composite::RELEASE_OVERLAY_WINDOW {
            state.cow_claims.push(CLIENT);
        }
        let calls = Arc::new(Mutex::new(Vec::new()));
        state.install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
            Arc::clone(&calls),
            row.target.id,
            Some(row.mode),
            error,
        )));
        let before = cd_snapshot(&state);
        let bytes = composite_damage_request(
            &mut state,
            &mut backend,
            &mut peer,
            row.major,
            row.minor,
            &row.body,
        );
        assert_policy_x_error(&bytes, row.major, row.minor);
        assert_eq!(cd_snapshot(&state), before, "{} changed state", row.label);
        assert!(
            backend.calls().is_empty(),
            "{} reached the backend",
            row.label
        );
        let calls = calls.lock().unwrap();
        let call = calls
            .iter()
            .find(|call| {
                call.hook == ExtensionHook::Resource
                    && call.target == Some(row.target)
                    && call.mode == Some(row.mode)
            })
            .unwrap_or_else(|| panic!("{} did not ask: {calls:?}", row.label));
        assert_cd_resource_check(call, &state, row.target, row.mode, row.major, row.minor);
    }
}

fn assert_cd_resource_check(
    call: &RecordedExtensionCheck,
    state_before: &ServerState,
    target: ResourceTarget,
    mode: AccessMode,
    major: u8,
    minor: u8,
) {
    let owner = target
        .owner
        .or_else(|| state_before.access_resource_owner(target.id, target.kind));
    assert_eq!(call.hook, ExtensionHook::Resource);
    assert_eq!(call.client, CLIENT);
    assert_eq!(call.request, Some(RequestOpcodes { major, minor }));
    assert_eq!(call.target, Some(target));
    assert_eq!(call.mode, Some(mode));
    assert_eq!(call.owner, owner);
    assert_eq!(call.target_owner, owner);
}

#[test]
fn composite_damage_resource_asks_match_the_site_table() {
    // Mutation: omit the 144/6 pixmap CREATE check; this table's expected resource-check count must fail.
    use yserver_protocol::x11::{composite, damage};
    let rows = [
        (144, composite::REDIRECT_WINDOW, words(&[CD_WINDOW.0, 0])),
        (
            144,
            composite::REDIRECT_SUBWINDOWS,
            words(&[ROOT_WINDOW.0, 0]),
        ),
        (144, composite::UNREDIRECT_WINDOW, words(&[CD_WINDOW.0, 0])),
        (
            144,
            composite::UNREDIRECT_SUBWINDOWS,
            words(&[ROOT_WINDOW.0, 0]),
        ),
        (
            144,
            composite::NAME_WINDOW_PIXMAP,
            words(&[CD_WINDOW.0, CD_NEW_PIXMAP.0]),
        ),
        (144, composite::GET_OVERLAY_WINDOW, words(&[ROOT_WINDOW.0])),
        (
            144,
            composite::RELEASE_OVERLAY_WINDOW,
            words(&[ROOT_WINDOW.0]),
        ),
        (143, damage::CREATE, cd_damage_body(CD_DAMAGE_NEW)),
        (143, damage::DESTROY, words(&[CD_DAMAGE.0])),
        (
            143,
            damage::SUBTRACT,
            words(&[CD_DAMAGE.0, CD_REGION_REPAIR.0, CD_REGION_PARTS.0]),
        ),
        (143, damage::ADD, words(&[CD_WINDOW.0, CD_REGION_REPAIR.0])),
    ];
    for (major, minor, body) in rows {
        let (mut state, mut peer, mut backend) = composite_damage_access_fixture();
        if major == 144 && minor == composite::RELEASE_OVERLAY_WINDOW {
            state.cow_claims.push(CLIENT);
        }
        let calls = Arc::new(Mutex::new(Vec::new()));
        state.install_access_policy(Arc::new(RecordingExtensionPolicy::new(
            Decision::Allow,
            Arc::clone(&calls),
        )));
        let mut expected = Vec::<(ResourceTarget, AccessMode, Option<ClientId>)>::new();
        macro_rules! expect {
            ($id:expr, $kind:expr, $mode:expr, $creation:expr) => {{
                let target = cd_target($id, $kind, $creation);
                let owner = target
                    .owner
                    .or_else(|| state.access_resource_owner(target.id, target.kind));
                expected.push((target, $mode, owner));
            }};
        }
        let redirect_mode = AccessMode::SET_ATTR | AccessMode::MANAGE | AccessMode::BLEND;
        match (major, minor) {
            (144, composite::REDIRECT_WINDOW) | (144, composite::UNREDIRECT_WINDOW) => {
                expect!(CD_WINDOW, ResourceKind::Window, redirect_mode, None);
            }
            (144, composite::REDIRECT_SUBWINDOWS) | (144, composite::UNREDIRECT_SUBWINDOWS) => {
                expect!(ROOT_WINDOW, ResourceKind::Window, redirect_mode, None);
                for child in state
                    .resources
                    .children(ROOT_WINDOW)
                    .iter()
                    .copied()
                    .filter(|child| *child != crate::resources::COMPOSITE_OVERLAY_WINDOW)
                {
                    expect!(child, ResourceKind::Window, redirect_mode, None);
                }
            }
            (144, composite::NAME_WINDOW_PIXMAP) => {
                expect!(CD_WINDOW, ResourceKind::Window, AccessMode::GET_ATTR, None);
                expect!(
                    CD_NEW_PIXMAP,
                    ResourceKind::Pixmap,
                    AccessMode::CREATE,
                    Some(Creation {
                        parent: Some(CD_WINDOW),
                        owner: CLIENT,
                    })
                );
            }
            (144, composite::GET_OVERLAY_WINDOW) | (144, composite::RELEASE_OVERLAY_WINDOW) => {
                expect!(
                    ROOT_WINDOW,
                    ResourceKind::Window,
                    AccessMode::GET_ATTR,
                    None
                );
            }
            (143, damage::CREATE) => {
                expect!(
                    CD_WINDOW,
                    ResourceKind::Window,
                    AccessMode::GET_ATTR | AccessMode::READ,
                    None
                );
                expect!(
                    CD_DAMAGE_NEW,
                    ResourceKind::Damage,
                    AccessMode::CREATE,
                    Some(Creation {
                        parent: Some(CD_WINDOW),
                        owner: CLIENT,
                    })
                );
            }
            (143, damage::DESTROY) => {
                expect!(CD_DAMAGE, ResourceKind::Damage, AccessMode::DESTROY, None);
            }
            (143, damage::SUBTRACT) => {
                expect!(CD_DAMAGE, ResourceKind::Damage, AccessMode::WRITE, None);
                expect!(
                    CD_REGION_REPAIR,
                    ResourceKind::Region,
                    AccessMode::WRITE,
                    None
                );
                expect!(
                    CD_REGION_PARTS,
                    ResourceKind::Region,
                    AccessMode::WRITE,
                    None
                );
            }
            (143, damage::ADD) => {
                expect!(CD_WINDOW, ResourceKind::Window, AccessMode::WRITE, None);
                expect!(
                    CD_REGION_REPAIR,
                    ResourceKind::Region,
                    AccessMode::WRITE,
                    None
                );
                expect!(CD_DAMAGE, ResourceKind::Damage, AccessMode::WRITE, None);
                expect!(
                    CD_DAMAGE_OTHER,
                    ResourceKind::Damage,
                    AccessMode::WRITE,
                    None
                );
            }
            _ => unreachable!("site table row {major}/{minor}"),
        }
        let _bytes =
            composite_damage_request(&mut state, &mut backend, &mut peer, major, minor, &body);
        let calls = calls.lock().unwrap();
        let resource_calls: Vec<_> = calls
            .iter()
            .filter(|call| call.hook == ExtensionHook::Resource)
            .collect();
        assert_eq!(resource_calls.len(), expected.len(), "{major}/{minor}");
        for (target, mode, owner) in expected {
            let call = resource_calls
                .iter()
                .find(|call| call.target == Some(target) && call.mode == Some(mode))
                .unwrap_or_else(|| {
                    panic!("missing {major}/{minor} {target:?} {mode:?}: {resource_calls:?}")
                });
            assert_eq!(call.client, CLIENT, "{major}/{minor}");
            assert_eq!(call.request, Some(RequestOpcodes { major, minor }));
            assert_eq!(call.owner, owner, "{major}/{minor}");
            assert_eq!(call.target_owner, owner, "{major}/{minor}");
        }
        if major == 143 && minor == damage::ADD {
            assert_eq!(
                resource_calls[0].target,
                Some(cd_target(CD_WINDOW, ResourceKind::Window, None))
            );
            assert_eq!(
                resource_calls[1].target,
                Some(cd_target(CD_REGION_REPAIR, ResourceKind::Region, None))
            );
        }
    }

    let unknown_window = ResourceId(0xDEAD_7001);
    let body = words(&[unknown_window.0]);
    let (mut no_policy_state, mut no_policy_peer, mut no_policy_backend) =
        composite_damage_access_fixture();
    let no_policy_bytes = composite_damage_request(
        &mut no_policy_state,
        &mut no_policy_backend,
        &mut no_policy_peer,
        144,
        composite::GET_OVERLAY_WINDOW,
        &body,
    );
    let (mut policy_state, mut policy_peer, mut policy_backend) = composite_damage_access_fixture();
    let calls = Arc::new(Mutex::new(Vec::new()));
    policy_state.install_access_policy(Arc::new(RecordingExtensionPolicy::new(
        Decision::Allow,
        Arc::clone(&calls),
    )));
    let policy_bytes = composite_damage_request(
        &mut policy_state,
        &mut policy_backend,
        &mut policy_peer,
        144,
        composite::GET_OVERLAY_WINDOW,
        &body,
    );
    assert_eq!(policy_bytes, no_policy_bytes);
    assert!(
        calls
            .lock()
            .unwrap()
            .iter()
            .all(|call| call.hook != ExtensionHook::Resource),
        "an unresolved ignored window operand must not reach the resource policy"
    );
}

#[test]
fn redirect_subwindows_skips_denied_child() {
    use yserver_protocol::x11::composite;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, CLIENT.0);
    let mut backend = RecordingBackend::new().with_redirect_activation();
    for (index, xid) in [0x0010_7101, 0x0010_7102, 0x0010_7103]
        .into_iter()
        .enumerate()
    {
        let child = ResourceId(xid);
        state.resources.create_window(
            CLIENT,
            CreateWindowRequest {
                depth: 24,
                window: child,
                parent: ROOT_WINDOW,
                width: 80,
                height: 60,
                class: 1,
                visual: crate::resources::ROOT_VISUAL,
                ..Default::default()
            },
        );
        let window = state.resources.window_mut(child).unwrap();
        window.host_xid = Some(crate::backend::WindowHandle::from_raw_for_test(
            0x0040_7101 + u32::try_from(index).unwrap(),
        ));
        window.map_state = crate::resources::MapState::Viewable;
    }
    let targets =
        crate::core_loop::process_request::subwindows_redirect_targets(&state, ROOT_WINDOW);
    assert_eq!(targets.len(), 3);
    let denied = targets[1];
    let calls = Arc::new(Mutex::new(Vec::new()));
    state.install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
        Arc::clone(&calls),
        denied,
        Some(AccessMode::SET_ATTR | AccessMode::MANAGE | AccessMode::BLEND),
        DenyError {
            code: x11::error::BAD_ACCESS,
            value: 0xA55A_1234,
        },
    )));
    let body = words(&[ROOT_WINDOW.0, 1]);

    // Mutation: remove the per-child filter (or redirect its denied child); this assertion must observe a backing on the denied child.
    let bytes = composite_damage_request(
        &mut state,
        &mut backend,
        &mut peer,
        144,
        composite::REDIRECT_SUBWINDOWS,
        &body,
    );
    assert!(
        bytes.is_empty(),
        "derived denial is not an X error: {bytes:02x?}"
    );
    for target in targets.iter().copied().filter(|target| *target != denied) {
        let window = state.resources.window(target).unwrap();
        assert!(
            state.composite_redirects.window_mode(target).is_some(),
            "allowed child {target:?} was not redirected"
        );
        assert!(
            window.redirected_backing.is_some(),
            "child {target:?} lacks backing"
        );
    }
    let denied_window = state.resources.window(denied).unwrap();
    assert_eq!(state.composite_redirects.window_mode(denied), None);
    assert!(denied_window.redirected_backing.is_none());
    assert_eq!(
        state.composite_redirects.subwindows_records(ROOT_WINDOW),
        &[crate::server::RedirectRecord {
            mode: crate::server::CompositeRedirectMode::Manual,
            owner: CLIENT,
        }],
        "the parent's persistent future-child rule remains installed"
    );
    let calls = calls.lock().unwrap();
    assert!(calls.iter().any(|call| {
        call.hook == ExtensionHook::Resource
            && call.client == CLIENT
            && call.request
                == Some(RequestOpcodes {
                    major: 144,
                    minor: composite::REDIRECT_SUBWINDOWS,
                })
            && call.target == Some(cd_target(ROOT_WINDOW, ResourceKind::Window, None))
            && call.mode == Some(AccessMode::SET_ATTR | AccessMode::MANAGE | AccessMode::BLEND)
    }));
    for target in targets {
        let call = calls
            .iter()
            .find(|call| {
                call.hook == ExtensionHook::Resource
                    && call.target == Some(cd_target(target, ResourceKind::Window, None))
            })
            .expect("each current child is checked, including the denied child");
        assert_cd_resource_check(
            call,
            &state,
            cd_target(target, ResourceKind::Window, None),
            AccessMode::SET_ATTR | AccessMode::MANAGE | AccessMode::BLEND,
            144,
            composite::REDIRECT_SUBWINDOWS,
        );
    }
}

#[test]
fn damage_add_skips_denied_damage() {
    use yserver_protocol::x11::damage;

    let (mut state, mut peer, mut backend) = composite_damage_access_fixture();
    let denied_before = state.damage_objects[&CD_DAMAGE.0].clone();
    let allowed_before = state.damage_objects[&CD_DAMAGE_OTHER.0].clone();
    let calls = Arc::new(Mutex::new(Vec::new()));
    state.install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
        Arc::clone(&calls),
        CD_DAMAGE,
        Some(AccessMode::WRITE),
        DenyError {
            code: x11::error::BAD_ACCESS,
            value: 0xA55A_1234,
        },
    )));
    let body = words(&[CD_WINDOW.0, CD_REGION_REPAIR.0]);

    // Mutation: let the denied Damage object accumulate; this assertion must find the denied object's rectangles changed.
    let bytes =
        composite_damage_request(&mut state, &mut backend, &mut peer, 143, damage::ADD, &body);
    assert!(
        bytes.is_empty(),
        "derived denial is not an X error: {bytes:02x?}"
    );
    assert_eq!(state.damage_objects[&CD_DAMAGE.0], denied_before);
    assert_ne!(
        state.damage_objects[&CD_DAMAGE_OTHER.0].rects,
        allowed_before.rects
    );
    let calls = calls.lock().unwrap();
    let damage_calls: Vec<_> = calls
        .iter()
        .filter(|call| {
            call.hook == ExtensionHook::Resource
                && call
                    .target
                    .is_some_and(|target| target.kind == ResourceKind::Damage)
        })
        .collect();
    assert_eq!(damage_calls.len(), 2);
    assert!(damage_calls.iter().any(|call| {
        call.target == Some(cd_target(CD_DAMAGE, ResourceKind::Damage, None))
            && call.mode == Some(AccessMode::WRITE)
            && call.request
                == Some(RequestOpcodes {
                    major: 143,
                    minor: damage::ADD,
                })
    }));
    assert!(damage_calls.iter().any(|call| {
        call.target == Some(cd_target(CD_DAMAGE_OTHER, ResourceKind::Damage, None))
            && call.mode == Some(AccessMode::WRITE)
            && call.request
                == Some(RequestOpcodes {
                    major: 143,
                    minor: damage::ADD,
                })
    }));
    for call in damage_calls.iter() {
        let target = call.target.expect("Damage access target");
        assert!([CD_DAMAGE, CD_DAMAGE_OTHER].contains(&target.id));
        assert_cd_resource_check(
            call,
            &state,
            cd_target(target.id, ResourceKind::Damage, None),
            AccessMode::WRITE,
            143,
            damage::ADD,
        );
    }
}

#[test]
fn damage_subtract_denied_parts_writes_nothing() {
    use yserver_protocol::x11::damage;

    let (mut state, mut peer, mut backend) = composite_damage_access_fixture();
    let calls = Arc::new(Mutex::new(Vec::new()));
    state.install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
        Arc::clone(&calls),
        CD_REGION_PARTS,
        Some(AccessMode::WRITE),
        DenyError {
            code: x11::error::BAD_ACCESS,
            value: 0xA55A_1234,
        },
    )));
    let before = cd_snapshot(&state);
    let body = words(&[CD_DAMAGE.0, CD_REGION_REPAIR.0, CD_REGION_PARTS.0]);

    // Mutation: omit the parts WRITE check; assert_policy_x_error must fail before the state snapshot assertion.
    let bytes = composite_damage_request(
        &mut state,
        &mut backend,
        &mut peer,
        143,
        damage::SUBTRACT,
        &body,
    );
    assert_policy_x_error(&bytes, 143, damage::SUBTRACT);
    assert_eq!(cd_snapshot(&state), before);
    assert!(
        backend.calls().is_empty(),
        "denied subtract reached the backend"
    );
    let calls = calls.lock().unwrap();
    let resource_calls: Vec<_> = calls
        .iter()
        .filter(|call| call.hook == ExtensionHook::Resource)
        .collect();
    assert_eq!(resource_calls.len(), 3);
    for (call, target) in resource_calls.iter().zip([
        cd_target(CD_DAMAGE, ResourceKind::Damage, None),
        cd_target(CD_REGION_REPAIR, ResourceKind::Region, None),
        cd_target(CD_REGION_PARTS, ResourceKind::Region, None),
    ]) {
        assert_cd_resource_check(
            call,
            &state,
            target,
            AccessMode::WRITE,
            143,
            damage::SUBTRACT,
        );
    }
}
