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
const GLYPHSET: ResourceId = ResourceId(0x0020_0030);
const SWITCH_GLYPHSET: ResourceId = ResourceId(0x0020_0031);
const NEW_GLYPHSET: ResourceId = ResourceId(0x0020_0032);
const FRAME_CURSOR: ResourceId = ResourceId(0x0020_0040);
const SECOND_FRAME_CURSOR: ResourceId = ResourceId(0x0020_0041);
const NEW_CURSOR: ResourceId = ResourceId(0x0020_0042);
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
    ResourceTarget {
        owner: None,
        id,
        kind,
        creation,
    }
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

fn glyph_items_with_switch(minor: u8, glyphset: ResourceId) -> Vec<u8> {
    let id_size = match minor {
        23 => 1,
        24 => 2,
        25 => 4,
        _ => unreachable!("CompositeGlyphs8/16/32 minor opcode"),
    };
    let mut items = vec![2, 0, 0, 0, 0, 0, 0, 0];
    let payload_bytes = 2 * id_size;
    items.extend((0..payload_bytes).map(|n| u8::try_from(n).unwrap()));
    items.resize(items.len() + ((4 - payload_bytes % 4) % 4), 0);
    items.extend_from_slice(&[255, 0, 0, 0]);
    items.extend_from_slice(&glyphset.0.to_le_bytes());
    items.extend_from_slice(&[1, 0, 0, 0, 0, 0, 0, 0]);
    items.extend(std::iter::repeat_n(0, id_size));
    items.resize(items.len() + ((4 - id_size % 4) % 4), 0);
    items
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

#[derive(Clone, Copy)]
enum SeedResource {
    GlyphSet(ResourceId, u32),
    Cursor(ResourceId, u32),
}

impl SeedResource {
    fn install(self, state: &mut ServerState) {
        match self {
            Self::GlyphSet(id, host) => {
                state.resources.create_glyphset(
                    id,
                    crate::resources::GlyphSetState {
                        client: CLIENT,
                        host_glyphset_xid: crate::backend::GlyphSetHandle::from_raw_for_test(host),
                    },
                );
            }
            Self::Cursor(id, host) => {
                state.resources.create_glyph_cursor(CLIENT, id);
                state
                    .resources
                    .set_cursor_host_xid(id, crate::backend::CursorHandle::from_raw_for_test(host));
            }
        }
    }

    fn exists(self, state: &ServerState) -> bool {
        match self {
            Self::GlyphSet(id, _) => state.resources.glyphset(id).is_some(),
            Self::Cursor(id, _) => state.resources.cursor_exists(id),
        }
    }
}

struct GlyphCursorRequestCase {
    name: &'static str,
    minor: u8,
    body: Vec<u8>,
    seeded: Vec<SeedResource>,
    expected: Vec<ExpectedCheck>,
    created_resource: Option<(ResourceId, ResourceKind)>,
}

fn glyph_cursor_cases() -> Vec<GlyphCursorRequestCase> {
    use crate::access::ResourceKind::{Cursor, GlyphSet, Picture};

    let mut cases = vec![GlyphCursorRequestCase {
        name: "CreateGlyphSet",
        minor: 17,
        body: words(&[NEW_GLYPHSET.0, yserver_protocol::x11::RENDER_FMT_XRGB32]),
        seeded: vec![],
        expected: vec![created(NEW_GLYPHSET, GlyphSet, AccessMode::CREATE, None)],
        created_resource: Some((NEW_GLYPHSET, GlyphSet)),
    }];

    cases.push(GlyphCursorRequestCase {
        name: "ReferenceGlyphSet",
        minor: 18,
        body: words(&[NEW_GLYPHSET.0, GLYPHSET.0]),
        seeded: vec![SeedResource::GlyphSet(GLYPHSET, 0xbb30)],
        expected: vec![
            existing(GLYPHSET, GlyphSet, AccessMode::GET_ATTR),
            created(NEW_GLYPHSET, GlyphSet, AccessMode::CREATE, None),
        ],
        created_resource: Some((NEW_GLYPHSET, GlyphSet)),
    });
    cases.push(GlyphCursorRequestCase {
        name: "FreeGlyphSet",
        minor: 19,
        body: words(&[GLYPHSET.0]),
        seeded: vec![SeedResource::GlyphSet(GLYPHSET, 0xbb30)],
        expected: vec![existing(GLYPHSET, GlyphSet, AccessMode::DESTROY)],
        created_resource: None,
    });
    cases.push(GlyphCursorRequestCase {
        name: "AddGlyphs",
        minor: 20,
        body: words(&[GLYPHSET.0, 0]),
        seeded: vec![SeedResource::GlyphSet(GLYPHSET, 0xbb30)],
        expected: vec![existing(GLYPHSET, GlyphSet, AccessMode::ADD)],
        created_resource: None,
    });
    cases.push(GlyphCursorRequestCase {
        name: "FreeGlyphs",
        minor: 22,
        body: words(&[GLYPHSET.0]),
        seeded: vec![SeedResource::GlyphSet(GLYPHSET, 0xbb30)],
        expected: vec![existing(GLYPHSET, GlyphSet, AccessMode::REMOVE)],
        created_resource: None,
    });

    for minor in 23..=25 {
        let items = glyph_items_with_switch(minor, SWITCH_GLYPHSET);
        let mut body = vec![0u8; 24];
        body[0] = 3;
        body[4..8].copy_from_slice(&SRC.0.to_le_bytes());
        body[8..12].copy_from_slice(&DST.0.to_le_bytes());
        body[16..20].copy_from_slice(&GLYPHSET.0.to_le_bytes());
        body.extend_from_slice(&items);
        cases.push(GlyphCursorRequestCase {
            name: match minor {
                23 => "CompositeGlyphs8",
                24 => "CompositeGlyphs16",
                _ => "CompositeGlyphs32",
            },
            minor,
            body,
            seeded: vec![
                SeedResource::GlyphSet(GLYPHSET, 0xbb30),
                SeedResource::GlyphSet(SWITCH_GLYPHSET, 0xbb31),
            ],
            expected: vec![
                existing(SRC, Picture, AccessMode::READ),
                existing(DST, Picture, AccessMode::WRITE),
                existing(GLYPHSET, GlyphSet, AccessMode::USE),
                existing(SWITCH_GLYPHSET, GlyphSet, AccessMode::USE),
            ],
            created_resource: None,
        });
    }

    let mut create_cursor = words(&[NEW_CURSOR.0, SRC.0]);
    create_cursor.extend_from_slice(&words(&[1, 2]));
    cases.push(GlyphCursorRequestCase {
        name: "CreateCursor",
        minor: 27,
        body: create_cursor,
        seeded: vec![],
        expected: vec![
            existing(SRC, Picture, AccessMode::READ),
            created(NEW_CURSOR, Cursor, AccessMode::CREATE, None),
        ],
        created_resource: Some((NEW_CURSOR, Cursor)),
    });

    let mut anim_cursor = words(&[NEW_CURSOR.0]);
    anim_cursor.extend_from_slice(&words(&[FRAME_CURSOR.0, 50, SECOND_FRAME_CURSOR.0, 75]));
    cases.push(GlyphCursorRequestCase {
        name: "CreateAnimCursor",
        minor: 31,
        body: anim_cursor,
        seeded: vec![
            SeedResource::Cursor(FRAME_CURSOR, 0xcc40),
            SeedResource::Cursor(SECOND_FRAME_CURSOR, 0xcc41),
        ],
        expected: vec![
            existing(FRAME_CURSOR, Cursor, AccessMode::READ),
            existing(SECOND_FRAME_CURSOR, Cursor, AccessMode::READ),
            created(NEW_CURSOR, Cursor, AccessMode::CREATE, None),
        ],
        created_resource: Some((NEW_CURSOR, Cursor)),
    });

    cases
}

fn setup_glyph_cursor_case(state: &mut ServerState, case: &GlyphCursorRequestCase) {
    for resource in &case.seeded {
        resource.install(state);
    }
}

fn assert_created_resource_absent(
    state: &ServerState,
    created: Option<(ResourceId, ResourceKind)>,
) {
    let Some((id, kind)) = created else {
        return;
    };
    let exists = match kind {
        ResourceKind::GlyphSet => state.resources.glyphset(id).is_some(),
        ResourceKind::Cursor => state.resources.cursor_exists(id),
        _ => unreachable!("glyph/cursor test creates only those resource kinds"),
    };
    assert!(!exists, "denied creation registered resource {:#x}", id.0);
}

#[test]
fn render_glyph_cursor_requests_ask_expected_targets() {
    // Mutation: omit or change a glyphset/cursor check, mode, or order.
    for case in glyph_cursor_cases() {
        let (mut state, mut backend, mut peer) = fixture();
        setup_glyph_cursor_case(&mut state, &case);
        backend.render_create_glyphset_returns_handle = true;
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
                    minor: case.minor,
                }),
                "{}: request context",
                case.name,
            );
        }
    }
}

#[test]
fn render_glyph_cursor_requests_deny_before_effects() {
    // Mutation: bypass a RENDER glyphset or cursor resource check.
    for case in glyph_cursor_cases() {
        for (denied_index, denied) in case.expected.iter().enumerate() {
            let (mut state, mut backend, mut peer) = fixture();
            setup_glyph_cursor_case(&mut state, &case);
            backend.render_create_glyphset_returns_handle = true;
            let calls = Arc::new(Mutex::new(Vec::new()));
            state.install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
                Arc::clone(&calls),
                denied.target.id,
                Some(denied.mode),
                DenyError {
                    code: x11::error::BAD_VALUE,
                    value: ERROR_VALUE,
                },
            )));

            let bytes =
                dispatch_with_peer(&mut state, &mut backend, &mut peer, case.minor, &case.body);
            assert_policy_error(&bytes, case.minor, case.name);
            assert_eq!(
                recorded_checks(&calls.lock().unwrap()),
                case.expected[..=denied_index]
                    .iter()
                    .map(|expected| (expected.target, expected.mode))
                    .collect::<Vec<_>>(),
                "{}: checks before denial",
                case.name,
            );
            assert!(
                backend.calls().is_empty(),
                "{}: backend was reached",
                case.name
            );
            for resource in &case.seeded {
                assert!(
                    resource.exists(&state),
                    "{}: seeded resource changed",
                    case.name
                );
            }
            assert_created_resource_absent(&state, case.created_resource);
        }
    }
}

#[test]
fn composite_glyphs_denied_stream_switch_draws_nothing() {
    // Mutation: check each stream switch only after preceding glyphs were drawn.
    let case = glyph_cursor_cases()
        .into_iter()
        .find(|case| case.minor == 23)
        .expect("CompositeGlyphs8 case");
    let (mut state, mut backend, mut peer) = fixture();
    setup_glyph_cursor_case(&mut state, &case);
    let calls = Arc::new(Mutex::new(Vec::new()));
    state.install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
        Arc::clone(&calls),
        SWITCH_GLYPHSET,
        Some(AccessMode::USE),
        DenyError {
            code: x11::error::BAD_VALUE,
            value: ERROR_VALUE,
        },
    )));

    let bytes = dispatch_with_peer(&mut state, &mut backend, &mut peer, case.minor, &case.body);

    assert_policy_error(&bytes, case.minor, case.name);
    assert!(
        backend
            .calls()
            .iter()
            .all(|call| !matches!(call, RecordedCall::RenderCompositeGlyphs { .. })),
        "glyphs before denied stream switch reached the backend",
    );
    assert_eq!(
        recorded_checks(&calls.lock().unwrap()),
        case.expected[..]
            .iter()
            .map(|expected| (expected.target, expected.mode))
            .collect::<Vec<_>>(),
    );
}

#[test]
fn create_anim_cursor_denied_frame_creates_nothing() {
    // Mutation: skip the frame cursor READ check before animated cursor creation.
    let case = glyph_cursor_cases()
        .into_iter()
        .find(|case| case.minor == 31)
        .expect("CreateAnimCursor case");
    let (mut state, mut backend, mut peer) = fixture();
    setup_glyph_cursor_case(&mut state, &case);
    let calls = Arc::new(Mutex::new(Vec::new()));
    state.install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
        Arc::clone(&calls),
        FRAME_CURSOR,
        Some(AccessMode::READ),
        DenyError {
            code: x11::error::BAD_VALUE,
            value: ERROR_VALUE,
        },
    )));

    let bytes = dispatch_with_peer(&mut state, &mut backend, &mut peer, case.minor, &case.body);

    assert_policy_error(&bytes, case.minor, case.name);
    assert!(!state.resources.cursor_exists(NEW_CURSOR));
    assert!(state.resources.cursor_exists(FRAME_CURSOR));
    assert!(state.resources.cursor_exists(SECOND_FRAME_CURSOR));
    assert!(backend.calls().is_empty());
    assert_eq!(
        recorded_checks(&calls.lock().unwrap()),
        vec![(
            render_target(FRAME_CURSOR, ResourceKind::Cursor, None),
            AccessMode::READ,
        )],
    );
}

#[test]
fn render_picture_requests_deny_before_effects() {
    // Mutation: bypass a RENDER resource check or move AddTraps' check after
    // damage accumulation; the denial/error and state snapshots must fail.
    for case in cases() {
        let (mut state, mut backend, mut peer) = fixture();
        let _ = state.resources.map_window(DRAWABLE);
        let _damage_peer = install_client(&mut state, 2);
        const DAMAGE: u32 = 0x0020_0050;
        state.damage_objects.insert(
            DAMAGE,
            crate::server::DamageObject {
                owner: ClientId(2),
                drawable: DRAWABLE,
                level: 3,
                rects: Vec::new(),
                pending_notify_fired: false,
                last_reported_geometry: None,
                list_pos: 0,
                created: 0,
            },
        );
        let damage_before = state.damage_objects.clone();
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
        assert_eq!(
            state.damage_objects, damage_before,
            "{}: denied request changed DAMAGE state",
            case.name,
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
fn render_checks_each_picture_before_resolving_the_next() {
    // Mutation: resolve all RENDER pictures before checking the first one.
    const MISSING: ResourceId = ResourceId(0x0020_00ff);
    let mut body = trapezoid_body();
    body[8..12].copy_from_slice(&MISSING.0.to_le_bytes());

    let (mut state, mut backend, mut peer) = fixture();
    let calls = Arc::new(Mutex::new(Vec::new()));
    state.install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
        Arc::clone(&calls),
        SRC,
        Some(AccessMode::READ),
        DenyError {
            code: x11::error::BAD_VALUE,
            value: ERROR_VALUE,
        },
    )));
    let bytes = dispatch_with_peer(&mut state, &mut backend, &mut peer, 10, &body);
    assert_policy_error(
        &bytes,
        10,
        "Trapezoids denied source before missing destination",
    );
    let recorded = calls.lock().unwrap();
    let resource_checks = recorded
        .iter()
        .filter(|call| call.hook == ExtensionHook::Resource)
        .collect::<Vec<_>>();
    assert_eq!(resource_checks.len(), 1);
    let source = resource_checks[0];
    assert_eq!(source.client, CLIENT);
    assert_eq!(
        source.target,
        Some(render_target(SRC, ResourceKind::Picture, None))
    );
    assert_eq!(source.mode, Some(AccessMode::READ));
    assert_eq!(source.owner, Some(CLIENT));
    assert_eq!(source.target_owner, Some(CLIENT));
    assert_eq!(
        source.request,
        Some(RequestOpcodes {
            major: 133,
            minor: 10,
        })
    );
    drop(recorded);
    assert!(
        backend.calls().is_empty(),
        "denied Composite reached backend"
    );

    let (mut state, mut backend, mut peer) = fixture();
    let bytes = dispatch_with_peer(&mut state, &mut backend, &mut peer, 10, &body);
    assert_eq!(
        bytes.len(),
        32,
        "missing destination should yield one error"
    );
    assert_eq!(bytes[1], crate::nested::RENDER_FIRST_ERROR + 1);
    assert_eq!(
        u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
        MISSING.0
    );
    assert_eq!(u16::from_le_bytes(bytes[8..10].try_into().unwrap()), 10);
    assert_eq!(bytes[10], 133);
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
