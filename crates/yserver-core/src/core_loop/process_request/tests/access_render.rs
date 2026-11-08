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
const BAD_GLYPHSET: ResourceId = ResourceId(0xdead_beef);
const FRAME_CURSOR: ResourceId = ResourceId(0x0020_0040);
const SECOND_FRAME_CURSOR: ResourceId = ResourceId(0x0020_0041);
const NEW_CURSOR: ResourceId = ResourceId(0x0020_0042);
const ERROR_VALUE: u32 = 0x55aa;
const CREATE_DRAWABLE_ROW: (&str, u8) = ("CreatePicture drawable", 4);
const CREATE_ALPHA_MAP_ROW: (&str, u8) = ("CreatePicture alpha map", 4);
const CREATE_CLIP_MASK_ROW: (&str, u8) = ("CreatePicture clip mask", 4);
const CREATE_NEW_PICTURE_ROW: (&str, u8) = ("CreatePicture new picture", 4);
const CHANGE_PICTURE_ROW: (&str, u8) = ("ChangePicture picture", 5);
const CHANGE_ALPHA_MAP_ROW: (&str, u8) = ("ChangePicture alpha map", 5);
const CHANGE_CLIP_MASK_ROW: (&str, u8) = ("ChangePicture clip mask", 5);
const CLIP_RECTANGLES_ROW: (&str, u8) = ("SetPictureClipRectangles", 6);
const TRANSFORM_ROW: (&str, u8) = ("SetPictureTransform", 28);
const FILTER_ROW: (&str, u8) = ("SetPictureFilter", 30);
const FREE_PICTURE_ROW: (&str, u8) = ("FreePicture", 7);
const FILL_RECTANGLES_ROW: (&str, u8) = ("FillRectangles dst", 26);
const QUERY_FILTERS_ROW: (&str, u8) = ("QueryFilters drawable", 29);
const ADD_TRAPS_ROW: (&str, u8) = ("AddTraps picture", 32);

#[derive(Clone, Copy)]
struct ExpectedCheck {
    target: ResourceTarget,
    mode: AccessMode,
    denied: bool,
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

type PictureRequestData = (Vec<u8>, Vec<ExpectedCheck>);

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
        denied: false,
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
        denied: false,
    }
}

fn denied(check: ExpectedCheck) -> ExpectedCheck {
    ExpectedCheck {
        denied: true,
        ..check
    }
}

fn checks2(first: ExpectedCheck, second: ExpectedCheck) -> Vec<ExpectedCheck> {
    vec![first, second]
}

fn checks3(
    first: ExpectedCheck,
    second: ExpectedCheck,
    third: ExpectedCheck,
) -> Vec<ExpectedCheck> {
    vec![first, second, third]
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
    glyph_items_with_switches(minor, &[glyphset])
}

fn glyph_items_with_switches(minor: u8, glyphsets: &[ResourceId]) -> Vec<u8> {
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
    for glyphset in glyphsets {
        items.extend_from_slice(&yserver_protocol::x11::render_composite_glyphset_switch(
            glyphset.0,
        ));
        items.extend_from_slice(&[1, 0, 0, 0, 0, 0, 0, 0]);
        items.extend(std::iter::repeat_n(0, id_size));
        items.resize(items.len() + ((4 - id_size % 4) % 4), 0);
    }
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

fn picture_attribute_request_body(picture: ResourceId, length: usize) -> Vec<u8> {
    let mut body = words(&[picture.0]);
    body.resize(length, 0);
    body
}

fn query_filters_body() -> Vec<u8> {
    DRAWABLE.0.to_le_bytes().to_vec()
}

fn add_traps_body() -> Vec<u8> {
    words(&[DST.0])
}

fn free_picture_body() -> Vec<u8> {
    words(&[SRC.0])
}

fn build_picture_request_case(
    (name, minor): (&'static str, u8),
    (body, expected): (Vec<u8>, Vec<ExpectedCheck>),
) -> PictureRequestCase {
    let denied = expected
        .iter()
        .find(|check| check.denied)
        .expect("picture case has a denied access check");
    let created_picture = expected
        .iter()
        .find(|check| check.target.kind == ResourceKind::Picture && check.target.creation.is_some())
        .map(|check| check.target.id);
    PictureRequestCase {
        name,
        minor,
        body,
        denied_target: denied.target.id,
        denied_kind: denied.target.kind,
        denied_mode: denied.mode,
        expected,
        created_picture,
    }
}

fn denied_source_set_attr() -> ExpectedCheck {
    denied_picture_check(SRC, AccessMode::SET_ATTR)
}

fn denied_source_destroy() -> ExpectedCheck {
    denied_picture_check(SRC, AccessMode::DESTROY)
}

fn clip_rectangles_body() -> Vec<u8> {
    picture_attribute_request_body(SRC, 8)
}

fn transform_body() -> Vec<u8> {
    picture_attribute_request_body(SRC, 40)
}

fn filter_body() -> Vec<u8> {
    picture_attribute_request_body(SRC, 8)
}

fn denied_picture_check(id: ResourceId, mode: AccessMode) -> ExpectedCheck {
    denied(existing(id, ResourceKind::Picture, mode))
}

fn denied_destination_write() -> ExpectedCheck {
    denied_picture_check(DST, AccessMode::WRITE)
}

fn denied_window_check(id: ResourceId, mode: AccessMode) -> ExpectedCheck {
    denied(existing(id, ResourceKind::Window, mode))
}

fn denied_query_filters_drawable() -> ExpectedCheck {
    denied_window_check(DRAWABLE, AccessMode::GET_ATTR)
}

fn denied_add_traps_picture() -> ExpectedCheck {
    denied_picture_check(DST, AccessMode::WRITE)
}

fn create_drawable_data() -> PictureRequestData {
    (
        picture_create_body(NEW_PICTURE, 0, &[]),
        drawable_creation_checks(),
    )
}

fn create_alpha_map_data() -> PictureRequestData {
    (
        picture_create_body(NEW_PICTURE, 1 << 1, &[SRC.0]),
        alpha_map_creation_checks(),
    )
}

fn create_clip_mask_data() -> PictureRequestData {
    (
        picture_create_body(NEW_PICTURE, 1 << 6, &[PIXMAP.0]),
        clip_mask_creation_checks(),
    )
}

fn create_new_picture_data() -> PictureRequestData {
    (
        picture_create_body(NEW_PICTURE, 0, &[]),
        new_picture_creation_checks(),
    )
}

fn change_picture_data() -> PictureRequestData {
    (change_picture_body(1, &[1]), vec![denied_source_set_attr()])
}

fn change_alpha_map_data() -> PictureRequestData {
    (
        change_picture_body(1 << 1, &[DST.0]),
        alpha_map_change_checks(),
    )
}

fn change_clip_mask_data() -> PictureRequestData {
    (
        change_picture_body(1 << 6, &[PIXMAP.0]),
        clip_mask_change_checks(),
    )
}

fn clip_rectangles_data() -> PictureRequestData {
    (clip_rectangles_body(), vec![denied_source_set_attr()])
}

fn transform_data() -> PictureRequestData {
    (transform_body(), vec![denied_source_set_attr()])
}

fn filter_data() -> PictureRequestData {
    (filter_body(), vec![denied_source_set_attr()])
}

fn free_picture_data() -> PictureRequestData {
    (free_picture_body(), vec![denied_source_destroy()])
}

fn fill_rectangles_data() -> PictureRequestData {
    (fill_rectangles_body(), vec![denied_destination_write()])
}

fn query_filters_data() -> PictureRequestData {
    (query_filters_body(), vec![denied_query_filters_drawable()])
}

fn add_traps_data() -> PictureRequestData {
    (add_traps_body(), vec![denied_add_traps_picture()])
}

fn drawable_creation_checks() -> Vec<ExpectedCheck> {
    checks2(
        denied(existing(
            DRAWABLE,
            ResourceKind::Window,
            AccessMode::READ | AccessMode::ADD,
        )),
        created(
            NEW_PICTURE,
            ResourceKind::Picture,
            AccessMode::CREATE | AccessMode::SET_ATTR,
            Some(DRAWABLE),
        ),
    )
}

fn alpha_map_creation_checks() -> Vec<ExpectedCheck> {
    checks3(
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
        denied(existing(SRC, ResourceKind::Picture, AccessMode::READ)),
    )
}

fn clip_mask_creation_checks() -> Vec<ExpectedCheck> {
    checks3(
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
        denied(existing(PIXMAP, ResourceKind::Pixmap, AccessMode::READ)),
    )
}

fn new_picture_creation_checks() -> Vec<ExpectedCheck> {
    checks2(
        existing(
            DRAWABLE,
            ResourceKind::Window,
            AccessMode::READ | AccessMode::ADD,
        ),
        denied(created(
            NEW_PICTURE,
            ResourceKind::Picture,
            AccessMode::CREATE | AccessMode::SET_ATTR,
            Some(DRAWABLE),
        )),
    )
}

fn alpha_map_change_checks() -> Vec<ExpectedCheck> {
    checks2(
        existing(SRC, ResourceKind::Picture, AccessMode::SET_ATTR),
        denied(existing(DST, ResourceKind::Picture, AccessMode::READ)),
    )
}

fn clip_mask_change_checks() -> Vec<ExpectedCheck> {
    checks2(
        existing(SRC, ResourceKind::Picture, AccessMode::SET_ATTR),
        denied(existing(PIXMAP, ResourceKind::Pixmap, AccessMode::READ)),
    )
}

fn composite_picture_request_case(
    name: &'static str,
    target: ResourceId,
    mode: AccessMode,
) -> PictureRequestCase {
    let checks = checks3(
        existing(DST, ResourceKind::Picture, AccessMode::WRITE),
        existing(SRC, ResourceKind::Picture, AccessMode::READ),
        existing(MASK, ResourceKind::Picture, AccessMode::READ),
    );
    let expected = checks
        .into_iter()
        .map(|check| {
            if check.target.id == target && check.mode == mode {
                denied(check)
            } else {
                check
            }
        })
        .collect();
    build_picture_request_case((name, 8), (composite_body(), expected))
}

fn vector_picture_request_case(
    minor: u8,
    target: ResourceId,
    mode: AccessMode,
) -> PictureRequestCase {
    let name = if target == SRC {
        "vector src"
    } else {
        "vector dst"
    };
    let checks = checks2(
        existing(SRC, ResourceKind::Picture, AccessMode::READ),
        existing(DST, ResourceKind::Picture, AccessMode::WRITE),
    );
    let expected = checks
        .into_iter()
        .map(|check| {
            if check.target.id == target && check.mode == mode {
                denied(check)
            } else {
                check
            }
        })
        .collect();
    build_picture_request_case((name, minor), (trapezoid_body(), expected))
}

fn gradient_case(name: &'static str, minor: u8, body_len: usize) -> PictureRequestCase {
    let mut body = vec![0u8; body_len];
    body[..4].copy_from_slice(&NEW_PICTURE.0.to_le_bytes());
    build_picture_request_case(
        (name, minor),
        (
            body,
            vec![denied(created(
                NEW_PICTURE,
                ResourceKind::Picture,
                AccessMode::CREATE,
                None,
            ))],
        ),
    )
}

fn cases() -> Vec<PictureRequestCase> {
    let mut cases: Vec<_> = [
        build_picture_request_case(CREATE_DRAWABLE_ROW, create_drawable_data()),
        build_picture_request_case(CREATE_ALPHA_MAP_ROW, create_alpha_map_data()),
        build_picture_request_case(CREATE_CLIP_MASK_ROW, create_clip_mask_data()),
        build_picture_request_case(CREATE_NEW_PICTURE_ROW, create_new_picture_data()),
        build_picture_request_case(CHANGE_PICTURE_ROW, change_picture_data()),
        build_picture_request_case(CHANGE_ALPHA_MAP_ROW, change_alpha_map_data()),
        build_picture_request_case(CHANGE_CLIP_MASK_ROW, change_clip_mask_data()),
        build_picture_request_case(CLIP_RECTANGLES_ROW, clip_rectangles_data()),
        build_picture_request_case(TRANSFORM_ROW, transform_data()),
        build_picture_request_case(FILTER_ROW, filter_data()),
        build_picture_request_case(FREE_PICTURE_ROW, free_picture_data()),
        composite_picture_request_case("Composite dst", DST, AccessMode::WRITE),
        composite_picture_request_case("Composite src", SRC, AccessMode::READ),
        composite_picture_request_case("Composite mask", MASK, AccessMode::READ),
    ]
    .into_iter()
    .collect();
    for minor in 10..=13 {
        cases.push(vector_picture_request_case(minor, SRC, AccessMode::READ));
        cases.push(vector_picture_request_case(minor, DST, AccessMode::WRITE));
    }
    cases.extend([
        build_picture_request_case(FILL_RECTANGLES_ROW, fill_rectangles_data()),
        build_picture_request_case(QUERY_FILTERS_ROW, query_filters_data()),
        build_picture_request_case(ADD_TRAPS_ROW, add_traps_data()),
        gradient_case("CreateSolidFill", 33, 12),
        gradient_case("CreateLinearGradient", 34, 24),
        gradient_case("CreateRadialGradient", 35, 32),
        gradient_case("CreateConicalGradient", 36, 4),
    ]);
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
    assert_request_error(bytes, x11::error::BAD_VALUE, ERROR_VALUE, minor, name);
}

fn assert_request_error(bytes: &[u8], code: u8, value: u32, minor: u8, name: &str) {
    assert_eq!(
        bytes.len(),
        32,
        "{name}: expected one X error: {bytes:02x?}"
    );
    assert_eq!(bytes[0], 0, "{name}: error packet type");
    assert_eq!(bytes[1], code, "{name}: error code");
    assert_eq!(
        u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
        value,
        "{name}: error value",
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

fn assert_resource_check(
    call: &RecordedExtensionCheck,
    target: ResourceTarget,
    mode: AccessMode,
    request: RequestOpcodes,
    owner: Option<ClientId>,
) {
    assert_eq!(call.hook, ExtensionHook::Resource);
    assert_eq!(call.client, CLIENT);
    assert_eq!(call.target, Some(target));
    assert_eq!(call.mode, Some(mode));
    assert_eq!(call.request, Some(request));
    assert_eq!(call.owner, owner);
    assert_eq!(call.target_owner, target.owner.or(owner));
}

fn assert_resource_checks(calls: &[RecordedExtensionCheck], expected: &[ExpectedCheck], minor: u8) {
    let resource_calls = calls
        .iter()
        .filter(|call| call.hook == ExtensionHook::Resource)
        .collect::<Vec<_>>();
    assert_eq!(resource_calls.len(), expected.len(), "resource check count");
    for (call, expected) in resource_calls.into_iter().zip(expected) {
        let owner = expected.target.creation.is_none().then_some(CLIENT);
        assert_resource_check(
            call,
            expected.target,
            expected.mode,
            RequestOpcodes { major: 133, minor },
            owner,
        );
    }
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

const N_GS: &str = "CreateGlyphSet";
const N_REF: &str = "ReferenceGlyphSet";
const N_FREE_GS: &str = "FreeGlyphSet";
const N_ADD_GLYPHS: &str = "AddGlyphs";
const N_FREE_GLYPHS: &str = "FreeGlyphs";
const N_CURSOR: &str = "CreateCursor";
const N_ANIM_CURSOR: &str = "CreateAnimCursor";

struct GlyphPlan {
    seeded: Vec<SeedResource>,
    expected: Vec<ExpectedCheck>,
}

fn plan(seeded: Vec<SeedResource>, expected: Vec<ExpectedCheck>) -> GlyphPlan {
    GlyphPlan { seeded, expected }
}

fn gc((name, minor): (&'static str, u8), body: Vec<u8>, plan: GlyphPlan) -> GlyphCursorRequestCase {
    let created_resource = plan
        .expected
        .iter()
        .find(|check| check.target.creation.is_some())
        .map(|check| (check.target.id, check.target.kind));
    GlyphCursorRequestCase {
        name,
        minor,
        body,
        seeded: plan.seeded,
        expected: plan.expected,
        created_resource,
    }
}

fn gs_body() -> Vec<u8> {
    words(&[NEW_GLYPHSET.0, yserver_protocol::x11::RENDER_FMT_XRGB32])
}

fn ref_body() -> Vec<u8> {
    words(&[NEW_GLYPHSET.0, GLYPHSET.0])
}

fn free_gs_body() -> Vec<u8> {
    words(&[GLYPHSET.0])
}

fn add_glyphs_body() -> Vec<u8> {
    words(&[GLYPHSET.0, 0])
}

fn cg_name(minor: u8) -> &'static str {
    match minor {
        23 => "CompositeGlyphs8",
        24 => "CompositeGlyphs16",
        _ => "CompositeGlyphs32",
    }
}

fn cg_body(minor: u8) -> Vec<u8> {
    let items = glyph_items_with_switch(minor, SWITCH_GLYPHSET);
    let mut body = vec![0u8; 24];
    body[0] = 3;
    body[4..8].copy_from_slice(&SRC.0.to_le_bytes());
    body[8..12].copy_from_slice(&DST.0.to_le_bytes());
    body[16..20].copy_from_slice(&GLYPHSET.0.to_le_bytes());
    body.extend_from_slice(&items);
    body
}

fn cursor_body() -> Vec<u8> {
    let mut body = words(&[NEW_CURSOR.0, SRC.0]);
    body.extend_from_slice(&words(&[1, 2]));
    body
}

fn anim_body() -> Vec<u8> {
    let mut body = words(&[NEW_CURSOR.0]);
    body.extend_from_slice(&words(&[FRAME_CURSOR.0, 50, SECOND_FRAME_CURSOR.0, 75]));
    body
}

fn gs_plan() -> GlyphPlan {
    plan(
        vec![],
        vec![created(
            NEW_GLYPHSET,
            ResourceKind::GlyphSet,
            AccessMode::CREATE,
            None,
        )],
    )
}

fn ref_plan() -> GlyphPlan {
    plan(
        vec![SeedResource::GlyphSet(GLYPHSET, 0xbb30)],
        vec![
            existing(GLYPHSET, ResourceKind::GlyphSet, AccessMode::GET_ATTR),
            created(
                NEW_GLYPHSET,
                ResourceKind::GlyphSet,
                AccessMode::CREATE,
                None,
            ),
        ],
    )
}

fn free_gs_plan() -> GlyphPlan {
    plan(
        vec![SeedResource::GlyphSet(GLYPHSET, 0xbb30)],
        vec![existing(
            GLYPHSET,
            ResourceKind::GlyphSet,
            AccessMode::DESTROY,
        )],
    )
}

fn add_glyphs_plan() -> GlyphPlan {
    plan(
        vec![SeedResource::GlyphSet(GLYPHSET, 0xbb30)],
        vec![existing(GLYPHSET, ResourceKind::GlyphSet, AccessMode::ADD)],
    )
}

fn free_glyphs_plan() -> GlyphPlan {
    plan(
        vec![SeedResource::GlyphSet(GLYPHSET, 0xbb30)],
        vec![existing(
            GLYPHSET,
            ResourceKind::GlyphSet,
            AccessMode::REMOVE,
        )],
    )
}

fn cg_plan() -> GlyphPlan {
    plan(
        vec![
            SeedResource::GlyphSet(GLYPHSET, 0xbb30),
            SeedResource::GlyphSet(SWITCH_GLYPHSET, 0xbb31),
        ],
        vec![
            existing(SRC, ResourceKind::Picture, AccessMode::READ),
            existing(DST, ResourceKind::Picture, AccessMode::WRITE),
            existing(GLYPHSET, ResourceKind::GlyphSet, AccessMode::USE),
            existing(SWITCH_GLYPHSET, ResourceKind::GlyphSet, AccessMode::USE),
        ],
    )
}

fn cursor_plan() -> GlyphPlan {
    plan(
        vec![],
        vec![
            existing(SRC, ResourceKind::Picture, AccessMode::READ),
            created(NEW_CURSOR, ResourceKind::Cursor, AccessMode::CREATE, None),
        ],
    )
}

fn anim_plan() -> GlyphPlan {
    plan(
        vec![
            SeedResource::Cursor(FRAME_CURSOR, 0xcc40),
            SeedResource::Cursor(SECOND_FRAME_CURSOR, 0xcc41),
        ],
        vec![
            existing(FRAME_CURSOR, ResourceKind::Cursor, AccessMode::READ),
            existing(SECOND_FRAME_CURSOR, ResourceKind::Cursor, AccessMode::READ),
            created(NEW_CURSOR, ResourceKind::Cursor, AccessMode::CREATE, None),
        ],
    )
}

fn glyph_cursor_cases() -> Vec<GlyphCursorRequestCase> {
    let mut cases = vec![
        gc((N_GS, 17), gs_body(), gs_plan()),
        gc((N_REF, 18), ref_body(), ref_plan()),
        gc((N_FREE_GS, 19), free_gs_body(), free_gs_plan()),
        gc((N_ADD_GLYPHS, 20), add_glyphs_body(), add_glyphs_plan()),
        gc((N_FREE_GLYPHS, 22), free_gs_body(), free_glyphs_plan()),
    ];
    for minor in 23..=25 {
        cases.push(gc((cg_name(minor), minor), cg_body(minor), cg_plan()));
    }
    cases.extend([
        gc((N_CURSOR, 27), cursor_body(), cursor_plan()),
        gc((N_ANIM_CURSOR, 31), anim_body(), anim_plan()),
    ]);
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
        let resource_calls = recorded
            .iter()
            .filter(|call| call.hook == ExtensionHook::Resource)
            .collect::<Vec<_>>();
        for (call, expected) in resource_calls.iter().zip(&case.expected) {
            let owner = expected.target.creation.is_none().then_some(CLIENT);
            assert_resource_check(
                call,
                expected.target,
                expected.mode,
                RequestOpcodes {
                    major: 133,
                    minor: case.minor,
                },
                owner,
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
    // Mutation: check a stream switch only after forwarding the preceding glyph run.
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
    assert_resource_checks(&calls.lock().unwrap(), &case.expected, case.minor);
    assert!(state.resources.glyphset(GLYPHSET).is_some());
    assert!(state.resources.glyphset(SWITCH_GLYPHSET).is_some());
}

#[test]
fn composite_glyphs_bad_switch_before_denied_switch_reports_bad_glyphset() {
    // Mutation: ask the policy about a switch XID before confirming it is a client GlyphSet.
    let case = glyph_cursor_cases()
        .into_iter()
        .find(|case| case.minor == 23)
        .expect("CompositeGlyphs8 case");
    let (mut state, mut backend, mut peer) = fixture();
    setup_glyph_cursor_case(&mut state, &case);
    let invalid_switch_calls = Arc::new(Mutex::new(Vec::new()));
    let denied_switch_calls = Arc::new(Mutex::new(Vec::new()));
    state.install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
        Arc::clone(&invalid_switch_calls),
        BAD_GLYPHSET,
        Some(AccessMode::USE),
        DenyError {
            code: x11::error::BAD_VALUE,
            value: ERROR_VALUE,
        },
    )));
    state.install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
        Arc::clone(&denied_switch_calls),
        SWITCH_GLYPHSET,
        Some(AccessMode::USE),
        DenyError {
            code: x11::error::BAD_VALUE,
            value: ERROR_VALUE,
        },
    )));

    let mut body = case.body[..24].to_vec();
    body.extend_from_slice(&glyph_items_with_switches(
        case.minor,
        &[BAD_GLYPHSET, SWITCH_GLYPHSET],
    ));
    let bytes = dispatch_with_peer(&mut state, &mut backend, &mut peer, case.minor, &body);

    assert_request_error(
        &bytes,
        crate::nested::RENDER_BAD_GLYPHSET_ERROR,
        BAD_GLYPHSET.0,
        case.minor,
        "invalid stream switch precedes denied switch",
    );
    assert!(
        backend
            .calls()
            .iter()
            .all(|call| !matches!(call, RecordedCall::RenderCompositeGlyphs { .. })),
        "invalid stream switch must reject the whole request before drawing",
    );
    let invalid_calls = invalid_switch_calls.lock().unwrap().clone();
    assert!(
        invalid_calls
            .iter()
            .all(|call| { call.target.is_none_or(|target| target.id != BAD_GLYPHSET) }),
        "the invalid XID was sent to the resource policy",
    );
    let denied_calls = denied_switch_calls.lock().unwrap().clone();
    assert!(
        denied_calls.iter().all(|call| {
            call.target
                .is_none_or(|target| target.id != SWITCH_GLYPHSET)
        }),
        "a later switch was checked after the earlier invalid XID",
    );
    let expected_before_switch = &case.expected[..3];
    assert_resource_checks(&invalid_calls, expected_before_switch, case.minor);
    assert_resource_checks(&denied_calls, expected_before_switch, case.minor);
    assert!(state.resources.glyphset(GLYPHSET).is_some());
    assert!(state.resources.glyphset(SWITCH_GLYPHSET).is_some());
}

#[test]
fn composite_glyphs_denied_switch_before_bad_switch_reports_policy_error() {
    // Mutation: pre-scan all switch XIDs for BadGlyphSet before checking access in stream order.
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

    let mut body = case.body[..24].to_vec();
    body.extend_from_slice(&glyph_items_with_switches(
        case.minor,
        &[SWITCH_GLYPHSET, BAD_GLYPHSET],
    ));
    let bytes = dispatch_with_peer(&mut state, &mut backend, &mut peer, case.minor, &body);

    assert_policy_error(
        &bytes,
        case.minor,
        "denied stream switch precedes invalid switch",
    );
    assert!(
        backend
            .calls()
            .iter()
            .all(|call| !matches!(call, RecordedCall::RenderCompositeGlyphs { .. })),
        "denied stream switch must reject the whole request before drawing",
    );
    let recorded = calls.lock().unwrap().clone();
    assert_resource_checks(&recorded, &case.expected, case.minor);
    assert!(
        recorded
            .iter()
            .all(|call| { call.target.is_none_or(|target| target.id != BAD_GLYPHSET) }),
        "the invalid switch after a denial was checked",
    );
    assert!(state.resources.glyphset(GLYPHSET).is_some());
    assert!(state.resources.glyphset(SWITCH_GLYPHSET).is_some());
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
        assert_resource_checks(&recorded, &case.expected, case.minor);
    }
}

#[test]
fn create_picture_creation_check_precedes_alpha_and_clip_checks() {
    // Mutation: check CreatePicture's attributes before CREATE; the distinct policy error and
    // recorded target order must then identify the earlier attribute denial.
    for case in cases().into_iter().filter(|case| {
        matches!(
            case.name,
            "CreatePicture alpha map" | "CreatePicture clip mask"
        )
    }) {
        let attribute = case
            .expected
            .iter()
            .find(|check| check.target.id != DRAWABLE && check.target.creation.is_none())
            .expect("CreatePicture attribute row")
            .target
            .id;
        let (mut state, mut backend, mut peer) = fixture();
        let bytes = dispatch_with_peer(&mut state, &mut backend, &mut peer, case.minor, &case.body);
        assert!(
            bytes.is_empty(),
            "{}: no-policy error: {bytes:02x?}",
            case.name
        );
        assert!(
            state.resources.picture(NEW_PICTURE).is_some(),
            "{}: picture missing",
            case.name
        );

        let (mut state, mut backend, mut peer) = fixture();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let creation_error = DenyError {
            code: x11::error::BAD_ACCESS,
            value: NEW_PICTURE.0,
        };
        let attribute_error = DenyError {
            code: x11::error::BAD_ACCESS,
            value: attribute.0,
        };
        state.install_access_policy(Arc::new(RecordingExtensionPolicy::custom(
            Arc::clone(&calls),
            "RENDER",
            move |_, check| {
                if check
                    .target
                    .is_some_and(|target| target.id == NEW_PICTURE && target.creation.is_some())
                {
                    Decision::Deny(creation_error)
                } else if check.target.is_some_and(|target| target.id == attribute) {
                    Decision::Deny(attribute_error)
                } else {
                    Decision::Allow
                }
            },
        )));
        let bytes = dispatch_with_peer(&mut state, &mut backend, &mut peer, case.minor, &case.body);
        assert_request_error(
            &bytes,
            creation_error.code,
            creation_error.value,
            case.minor,
            case.name,
        );
        assert!(
            state.resources.picture(NEW_PICTURE).is_none(),
            "{}: denied picture registered",
            case.name
        );
        assert!(
            backend.calls().is_empty(),
            "{}: denied request reached backend",
            case.name
        );
        let recorded = calls.lock().unwrap().clone();
        assert_resource_checks(
            &recorded,
            &checks2(
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
            ),
            case.minor,
        );
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
