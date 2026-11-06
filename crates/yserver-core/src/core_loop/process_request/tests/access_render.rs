use super::{
    extensions::{ExtensionHook, RecordedExtensionCheck, RecordingExtensionPolicy},
    *,
};
use crate::access::{
    AccessMode, Creation, Decision, DenyError, RequestOpcodes, ResourceKind, ResourceTarget,
};

const CLIENT: ClientId = ClientId(1);
const DRAWABLE: ResourceId = ResourceId(0x0020_0001);
const PIXMAP: ResourceId = ResourceId(0x0020_0002);
const SRC: ResourceId = ResourceId(0x0020_0010);
const DST: ResourceId = ResourceId(0x0020_0011);
const MASK: ResourceId = ResourceId(0x0020_0012);
const NEW_PICTURE: ResourceId = ResourceId(0x0020_0020);
const ERROR_VALUE: u32 = 0x55aa;

#[derive(Clone)]
struct ExpectedCheck {
    target: ResourceTarget,
    mode: AccessMode,
}

struct PictureRequestCase {
    name: &'static str,
    minor: u8,
    body: Vec<u8>,
    denied_target: ResourceId,
    denied_kind: ResourceKind,
    denied_mode: AccessMode,
    expected: Vec<ExpectedCheck>,
    created_picture: Option<ResourceId>,
}

fn render_target(id: ResourceId, kind: ResourceKind, creation: Option<Creation>) -> ResourceTarget {
    ResourceTarget { id, kind, creation }
}

fn existing(id: ResourceId, kind: ResourceKind, mode: AccessMode) -> ExpectedCheck {
    ExpectedCheck {
        target: render_target(id, kind, None),
        mode,
    }
}

fn created(
    id: ResourceId,
    kind: ResourceKind,
    mode: AccessMode,
    parent: Option<ResourceId>,
) -> ExpectedCheck {
    ExpectedCheck {
        target: render_target(
            id,
            kind,
            Some(Creation {
                parent,
                owner: CLIENT,
            }),
        ),
        mode,
    }
}

fn words(values: &[u32]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

fn picture_create_body(picture: ResourceId, mask: u32, values: &[u32]) -> Vec<u8> {
    let mut body = words(&[picture.0, DRAWABLE.0, 0, mask]);
    body.extend_from_slice(&words(values));
    body
}

fn change_picture_body(mask: u32, values: &[u32]) -> Vec<u8> {
    let mut body = words(&[SRC.0, mask]);
    body.extend_from_slice(&words(values));
    body
}

fn composite_body() -> Vec<u8> {
    let mut body = vec![0u8; 32];
    body[4..8].copy_from_slice(&SRC.0.to_le_bytes());
    body[8..12].copy_from_slice(&MASK.0.to_le_bytes());
    body[12..16].copy_from_slice(&DST.0.to_le_bytes());
    body[28..30].copy_from_slice(&8u16.to_le_bytes());
    body[30..32].copy_from_slice(&8u16.to_le_bytes());
    body
}

fn trapezoid_body() -> Vec<u8> {
    let mut body = vec![0u8; 20];
    body[4..8].copy_from_slice(&SRC.0.to_le_bytes());
    body[8..12].copy_from_slice(&DST.0.to_le_bytes());
    body
}

fn fill_rectangles_body() -> Vec<u8> {
    let mut body = vec![0u8; 16];
    body[4..8].copy_from_slice(&DST.0.to_le_bytes());
    body
}

fn cases() -> Vec<PictureRequestCase> {
    let mut cases = Vec::new();
    let mut add =
        |name, minor, body, denied_target, denied_kind, denied_mode, expected, created_picture| {
            cases.push(PictureRequestCase {
                name,
                minor,
                body,
                denied_target,
                denied_kind,
                denied_mode,
                expected,
                created_picture,
            });
        };

    add(
        "CreatePicture drawable",
        4,
        picture_create_body(NEW_PICTURE, 0, &[]),
        DRAWABLE,
        ResourceKind::Window,
        AccessMode::READ | AccessMode::ADD,
        vec![
            existing(
                DRAWABLE,
                ResourceKind::Window,
                AccessMode::READ | AccessMode::ADD,
            ),
            created(
                NEW_PICTURE,
                ResourceKind::Picture,
                AccessMode::CREATE | AccessMode::SET_ATTR,
                Some(DRAWABLE),
            ),
        ],
        Some(NEW_PICTURE),
    );
    add(
        "CreatePicture alpha map",
        4,
        picture_create_body(NEW_PICTURE, 1 << 1, &[SRC.0]),
        SRC,
        ResourceKind::Picture,
        AccessMode::READ,
        vec![
            existing(
                DRAWABLE,
                ResourceKind::Window,
                AccessMode::READ | AccessMode::ADD,
            ),
            existing(SRC, ResourceKind::Picture, AccessMode::READ),
            created(
                NEW_PICTURE,
                ResourceKind::Picture,
                AccessMode::CREATE | AccessMode::SET_ATTR,
                Some(DRAWABLE),
            ),
        ],
        Some(NEW_PICTURE),
    );
    add(
        "CreatePicture clip mask",
        4,
        picture_create_body(NEW_PICTURE, 1 << 6, &[PIXMAP.0]),
        PIXMAP,
        ResourceKind::Pixmap,
        AccessMode::READ,
        vec![
            existing(
                DRAWABLE,
                ResourceKind::Window,
                AccessMode::READ | AccessMode::ADD,
            ),
            existing(PIXMAP, ResourceKind::Pixmap, AccessMode::READ),
            created(
                NEW_PICTURE,
                ResourceKind::Picture,
                AccessMode::CREATE | AccessMode::SET_ATTR,
                Some(DRAWABLE),
            ),
        ],
        Some(NEW_PICTURE),
    );
    add(
        "CreatePicture new picture",
        4,
        picture_create_body(NEW_PICTURE, 0, &[]),
        NEW_PICTURE,
        ResourceKind::Picture,
        AccessMode::CREATE | AccessMode::SET_ATTR,
        vec![
            existing(
                DRAWABLE,
                ResourceKind::Window,
                AccessMode::READ | AccessMode::ADD,
            ),
            created(
                NEW_PICTURE,
                ResourceKind::Picture,
                AccessMode::CREATE | AccessMode::SET_ATTR,
                Some(DRAWABLE),
            ),
        ],
        Some(NEW_PICTURE),
    );

    add(
        "ChangePicture picture",
        5,
        change_picture_body(1, &[1]),
        SRC,
        ResourceKind::Picture,
        AccessMode::SET_ATTR,
        vec![existing(SRC, ResourceKind::Picture, AccessMode::SET_ATTR)],
        None,
    );
    add(
        "ChangePicture alpha map",
        5,
        change_picture_body(1 << 1, &[DST.0]),
        DST,
        ResourceKind::Picture,
        AccessMode::READ,
        vec![
            existing(SRC, ResourceKind::Picture, AccessMode::SET_ATTR),
            existing(DST, ResourceKind::Picture, AccessMode::READ),
        ],
        None,
    );
    add(
        "ChangePicture clip mask",
        5,
        change_picture_body(1 << 6, &[PIXMAP.0]),
        PIXMAP,
        ResourceKind::Pixmap,
        AccessMode::READ,
        vec![
            existing(SRC, ResourceKind::Picture, AccessMode::SET_ATTR),
            existing(PIXMAP, ResourceKind::Pixmap, AccessMode::READ),
        ],
        None,
    );
    for (minor, name, mode) in [
        (6, "SetPictureClipRectangles", AccessMode::SET_ATTR),
        (28, "SetPictureTransform", AccessMode::SET_ATTR),
        (30, "SetPictureFilter", AccessMode::SET_ATTR),
    ] {
        let mut body = words(&[SRC.0]);
        body.resize(if minor == 28 { 40 } else { 8 }, 0);
        add(
            name,
            minor,
            body,
            SRC,
            ResourceKind::Picture,
            mode,
            vec![existing(SRC, ResourceKind::Picture, mode)],
            None,
        );
    }
    add(
        "FreePicture",
        7,
        words(&[SRC.0]),
        SRC,
        ResourceKind::Picture,
        AccessMode::DESTROY,
        vec![existing(SRC, ResourceKind::Picture, AccessMode::DESTROY)],
        None,
    );

    for (name, target, mode, expected) in [
        (
            "Composite dst",
            DST,
            AccessMode::WRITE,
            vec![
                existing(DST, ResourceKind::Picture, AccessMode::WRITE),
                existing(SRC, ResourceKind::Picture, AccessMode::READ),
                existing(MASK, ResourceKind::Picture, AccessMode::READ),
            ],
        ),
        (
            "Composite src",
            SRC,
            AccessMode::READ,
            vec![
                existing(DST, ResourceKind::Picture, AccessMode::WRITE),
                existing(SRC, ResourceKind::Picture, AccessMode::READ),
                existing(MASK, ResourceKind::Picture, AccessMode::READ),
            ],
        ),
        (
            "Composite mask",
            MASK,
            AccessMode::READ,
            vec![
                existing(DST, ResourceKind::Picture, AccessMode::WRITE),
                existing(SRC, ResourceKind::Picture, AccessMode::READ),
                existing(MASK, ResourceKind::Picture, AccessMode::READ),
            ],
        ),
    ] {
        add(
            name,
            8,
            composite_body(),
            target,
            ResourceKind::Picture,
            mode,
            expected,
            None,
        );
    }

    for minor in 10..=13 {
        for (name, target, mode, expected) in [
            (
                "vector src",
                SRC,
                AccessMode::READ,
                vec![
                    existing(SRC, ResourceKind::Picture, AccessMode::READ),
                    existing(DST, ResourceKind::Picture, AccessMode::WRITE),
                ],
            ),
            (
                "vector dst",
                DST,
                AccessMode::WRITE,
                vec![
                    existing(SRC, ResourceKind::Picture, AccessMode::READ),
                    existing(DST, ResourceKind::Picture, AccessMode::WRITE),
                ],
            ),
        ] {
            add(
                name,
                minor,
                trapezoid_body(),
                target,
                ResourceKind::Picture,
                mode,
                expected,
                None,
            );
        }
    }

    add(
        "FillRectangles dst",
        26,
        fill_rectangles_body(),
        DST,
        ResourceKind::Picture,
        AccessMode::WRITE,
        vec![existing(DST, ResourceKind::Picture, AccessMode::WRITE)],
        None,
    );
    add(
        "QueryFilters drawable",
        29,
        DRAWABLE.0.to_le_bytes().to_vec(),
        DRAWABLE,
        ResourceKind::Window,
        AccessMode::GET_ATTR,
        vec![existing(
            DRAWABLE,
            ResourceKind::Window,
            AccessMode::GET_ATTR,
        )],
        None,
    );
    add(
        "AddTraps picture",
        32,
        words(&[DST.0]),
        DST,
        ResourceKind::Picture,
        AccessMode::WRITE,
        vec![existing(DST, ResourceKind::Picture, AccessMode::WRITE)],
        None,
    );

    for (minor, name, body_len) in [
        (33, "CreateSolidFill", 12),
        (34, "CreateLinearGradient", 24),
        (35, "CreateRadialGradient", 32),
        (36, "CreateConicalGradient", 4),
    ] {
        let mut body = vec![0u8; body_len];
        body[..4].copy_from_slice(&NEW_PICTURE.0.to_le_bytes());
        add(
            name,
            minor,
            body,
            NEW_PICTURE,
            ResourceKind::Picture,
            AccessMode::CREATE,
            vec![created(
                NEW_PICTURE,
                ResourceKind::Picture,
                AccessMode::CREATE,
                None,
            )],
            Some(NEW_PICTURE),
        );
    }
    cases
}

fn fixture() -> (ServerState, RecordingBackend, UnixStream) {
    use crate::{
        backend::PictureHandle,
        resources::{PictureKind, PictureState},
    };

    let mut state = ServerState::new();
    let backend = RecordingBackend::new();
    let peer = install_client(&mut state, CLIENT.0);
    state.resources.create_window(
        CLIENT,
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: DRAWABLE,
            parent: crate::resources::ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 16,
            height: 16,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    state.resources.create_pixmap(
        CLIENT,
        yserver_protocol::x11::CreatePixmapRequest {
            depth: 24,
            pixmap: PIXMAP,
            drawable: DRAWABLE,
            width: 16,
            height: 16,
        },
    );
    for (id, host) in [(SRC, 0xaa10), (DST, 0xaa11), (MASK, 0xaa12)] {
        state.resources.create_picture(
            id,
            PictureState {
                client: CLIENT,
                host_picture_xid: Some(PictureHandle::from_raw_for_test(host)),
                host_owned_pixmap: None,
                kind: PictureKind::Drawable,
                drawable: Some(DRAWABLE),
                window: None,
            },
        );
    }
    backend.calls.lock().unwrap().clear();
    (state, backend, peer)
}

fn dispatch_with_peer(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    peer: &mut UnixStream,
    minor: u8,
    body: &[u8],
) -> Vec<u8> {
    process_request(
        state,
        backend,
        CLIENT,
        SequenceNumber(1),
        RequestHeader {
            opcode: 133,
            data: minor,
            length_units: u32::try_from(1 + body.len().div_ceil(4)).unwrap(),
        },
        body,
        None,
    )
    .expect("RENDER request through process_request");
    read_all_available(peer)
}

fn assert_policy_error(bytes: &[u8], minor: u8, name: &str) {
    assert_eq!(
        bytes.len(),
        32,
        "{name}: expected one X error: {bytes:02x?}"
    );
    assert_eq!(bytes[0], 0, "{name}: error packet type");
    assert_eq!(bytes[1], x11::error::BAD_VALUE, "{name}: policy error code");
    assert_eq!(
        u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
        ERROR_VALUE,
        "{name}: policy error value",
    );
    assert_eq!(
        u16::from_le_bytes(bytes[8..10].try_into().unwrap()),
        u16::from(minor),
        "{name}: minor opcode",
    );
    assert_eq!(bytes[10], 133, "{name}: major opcode");
}

fn recorded_checks(calls: &[RecordedExtensionCheck]) -> Vec<(ResourceTarget, AccessMode)> {
    calls
        .iter()
        .filter(|call| call.hook == ExtensionHook::Resource)
        .map(|call| {
            assert_eq!(call.client, CLIENT);
            assert_eq!(call.extension, "");
            (
                call.target.expect("resource check target"),
                call.mode.expect("resource check mode"),
            )
        })
        .collect()
}

#[test]
fn render_picture_requests_deny_before_effects() {
    // Mutation: bypass the RENDER resource checks.
    for case in cases() {
        let (mut state, mut backend, mut peer) = fixture();
        let calls = Arc::new(Mutex::new(Vec::new()));
        state.install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
            Arc::clone(&calls),
            case.denied_target,
            Some(case.denied_mode),
            DenyError {
                code: x11::error::BAD_VALUE,
                value: ERROR_VALUE,
            },
        )));
        let bytes = dispatch_with_peer(&mut state, &mut backend, &mut peer, case.minor, &case.body);
        assert_policy_error(&bytes, case.minor, case.name);
        let denied_check = case
            .expected
            .iter()
            .find(|expected| {
                expected.target.id == case.denied_target
                    && expected.target.kind == case.denied_kind
                    && expected.mode == case.denied_mode
            })
            .expect("denied target appears in expected check list");
        assert!(
            recorded_checks(&calls.lock().unwrap())
                .contains(&(denied_check.target, denied_check.mode)),
            "{}: shared policy did not see the denied target",
            case.name,
        );
        assert!(
            backend.calls().is_empty(),
            "{}: backend was reached",
            case.name
        );
        for id in [SRC, DST, MASK] {
            assert!(
                state.resources.picture(id).is_some(),
                "{}: existing picture removed",
                case.name
            );
        }
        if let Some(id) = case.created_picture {
            assert!(
                state.resources.picture(id).is_none(),
                "{}: denied creation registered picture {:#x}",
                case.name,
                id.0,
            );
        }
    }
}

#[test]
fn render_picture_requests_ask_expected_targets() {
    // Mutation: change Composite's dst mode from WRITE to READ.
    for case in cases() {
        let (mut state, mut backend, mut peer) = fixture();
        let calls = Arc::new(Mutex::new(Vec::new()));
        state.install_access_policy(Arc::new(RecordingExtensionPolicy::new(
            Decision::Allow,
            Arc::clone(&calls),
        )));
        let _bytes =
            dispatch_with_peer(&mut state, &mut backend, &mut peer, case.minor, &case.body);
        let recorded = calls.lock().unwrap().clone();
        assert_eq!(
            recorded_checks(&recorded),
            case.expected
                .iter()
                .map(|expected| (expected.target, expected.mode))
                .collect::<Vec<_>>(),
            "{}: checked targets",
            case.name,
        );
        for call in &recorded {
            assert_eq!(
                call.request,
                Some(RequestOpcodes {
                    major: 133,
                    minor: case.minor
                })
            );
        }
    }
}

#[test]
fn change_picture_denied_alpha_map_changes_nothing() {
    // Mutation: skip the alpha-map READ check and forward its translated value.
    let (mut state, mut backend, mut peer) = fixture();
    let calls = Arc::new(Mutex::new(Vec::new()));
    state.install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
        Arc::clone(&calls),
        DST,
        Some(AccessMode::READ),
        DenyError {
            code: x11::error::BAD_VALUE,
            value: ERROR_VALUE,
        },
    )));

    let body = change_picture_body((1 << 0) | (1 << 1), &[1, DST.0]);
    let bytes = dispatch_with_peer(&mut state, &mut backend, &mut peer, 5, &body);

    assert_policy_error(&bytes, 5, "ChangePicture denied alpha map");
    assert!(
        backend.calls().is_empty(),
        "ChangePicture reached backend before all attributes passed"
    );
    assert_eq!(
        state
            .resources
            .picture(SRC)
            .unwrap()
            .host_picture_xid
            .unwrap()
            .as_raw(),
        0xaa10
    );
    assert_eq!(
        state
            .resources
            .picture(DST)
            .unwrap()
            .host_picture_xid
            .unwrap()
            .as_raw(),
        0xaa11
    );
    assert_eq!(
        recorded_checks(&calls.lock().unwrap()),
        vec![
            (
                render_target(SRC, ResourceKind::Picture, None),
                AccessMode::SET_ATTR
            ),
            (
                render_target(DST, ResourceKind::Picture, None),
                AccessMode::READ
            ),
        ],
    );
}

#[test]
fn composite_denied_mask_after_valid_dst_and_src() {
    // Mutation: omit the mask READ check after valid dst and src.
    let (mut state, mut backend, mut peer) = fixture();
    let mut body = composite_body();
    let missing_mask = ResourceId(0x0020_00ff);
    body[8..12].copy_from_slice(&missing_mask.0.to_le_bytes());
    let bytes = dispatch_with_peer(&mut state, &mut backend, &mut peer, 8, &body);
    assert_eq!(bytes[1], crate::nested::RENDER_FIRST_ERROR + 1);
    assert_eq!(
        u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
        missing_mask.0,
        "missing mask keeps BadPicture precedence without a policy",
    );
    assert!(backend.calls().is_empty());

    let (mut state, mut backend, mut peer) = fixture();
    let calls = Arc::new(Mutex::new(Vec::new()));
    // This id-only rule also exercises the shared selective-decision path.
    state.install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
        Arc::clone(&calls),
        MASK,
        None,
        DenyError {
            code: x11::error::BAD_VALUE,
            value: ERROR_VALUE,
        },
    )));
    let bytes = dispatch_with_peer(&mut state, &mut backend, &mut peer, 8, &composite_body());
    assert_policy_error(&bytes, 8, "Composite denied mask");
    assert!(
        backend.calls().is_empty(),
        "Composite reached backend after denied mask"
    );
    assert_eq!(
        recorded_checks(&calls.lock().unwrap()),
        vec![
            (
                render_target(DST, ResourceKind::Picture, None),
                AccessMode::WRITE
            ),
            (
                render_target(SRC, ResourceKind::Picture, None),
                AccessMode::READ
            ),
            (
                render_target(MASK, ResourceKind::Picture, None),
                AccessMode::READ
            ),
        ],
    );
}
