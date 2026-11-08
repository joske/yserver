use super::{
    access_resources::{
        CLIENT, assert_policy_error, assert_x11_error, drawing_resource_policy,
        expected_resource_checks, resource_request, window_resource_policy, words,
    },
    extensions::{ExtensionHook, RecordingExtensionPolicy},
    *,
};
use crate::access::{
    AccessMode, Creation, Decision, DenyError, RequestOpcodes, ResourceKind, ResourceTarget,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU32, Ordering},
};

struct GcFontAccessFixture {
    state: ServerState,
    backend: RecordingBackend,
    peer: UnixStream,
    font: ResourceId,
    second_font: ResourceId,
    source: ResourceId,
    destination: ResourceId,
    tile: ResourceId,
    stipple: ResourceId,
    clip_mask: ResourceId,
    free_pixmap: ResourceId,
    gc: ResourceId,
    copy_gc: ResourceId,
    free_gc: ResourceId,
    cursor: ResourceId,
    free_cursor: ResourceId,
}

fn gc_font_access_fixture() -> GcFontAccessFixture {
    const FONT: ResourceId = ResourceId(0x0055_0020);
    const SECOND_FONT: ResourceId = ResourceId(0x0055_0021);
    const SOURCE: ResourceId = ResourceId(0x0055_0001);
    const DESTINATION: ResourceId = ResourceId(0x0055_0002);
    const TILE: ResourceId = ResourceId(0x0055_0003);
    const STIPPLE: ResourceId = ResourceId(0x0055_0004);
    const CLIP_MASK: ResourceId = ResourceId(0x0055_0005);
    const FREE_PIXMAP: ResourceId = ResourceId(0x0055_0006);
    const GC: ResourceId = ResourceId(0x0055_0010);
    const COPY_GC: ResourceId = ResourceId(0x0055_0011);
    const FREE_GC: ResourceId = ResourceId(0x0055_0012);
    const CURSOR: ResourceId = ResourceId(0x0055_0030);
    const FREE_CURSOR: ResourceId = ResourceId(0x0055_0031);

    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    let mut peer = install_client(&mut state, CLIENT.0);
    let mut sequence = 1;

    for (font, name) in [(FONT, "fixed"), (SECOND_FONT, "cursor")] {
        let mut body = font.0.to_le_bytes().to_vec();
        body.extend_from_slice(&u16::try_from(name.len()).unwrap().to_le_bytes());
        body.extend_from_slice(&[0, 0]);
        body.extend_from_slice(name.as_bytes());
        body.resize(body.len().next_multiple_of(4), 0);
        let bytes = resource_request(&mut state, &mut backend, &mut peer, sequence, 45, 0, &body);
        assert_no_error(&bytes, "GC access fixture OpenFont");
        sequence += 1;
    }

    for (pixmap, depth) in [
        (SOURCE, 24),
        (DESTINATION, 24),
        (TILE, 24),
        (STIPPLE, 1),
        (CLIP_MASK, 1),
        (FREE_PIXMAP, 24),
    ] {
        let body = words(&[pixmap.0, ROOT_WINDOW.0, 8 | (8 << 16)]);
        let bytes = resource_request(
            &mut state,
            &mut backend,
            &mut peer,
            sequence,
            53,
            depth,
            &body,
        );
        assert_no_error(&bytes, "GC access fixture CreatePixmap");
        sequence += 1;
    }

    for gc in [GC, COPY_GC, FREE_GC] {
        let foreground = match gc {
            GC => 0x0011_2233,
            COPY_GC => 0x0044_5566,
            _ => 0x0077_8899,
        };
        let body = words(&[
            gc.0,
            ROOT_WINDOW.0,
            (1 << 2) | (1 << 14),
            foreground,
            FONT.0,
        ]);
        let bytes = resource_request(&mut state, &mut backend, &mut peer, sequence, 55, 0, &body);
        assert_no_error(&bytes, "GC access fixture CreateGC");
        sequence += 1;
    }

    for cursor in [CURSOR, FREE_CURSOR] {
        let body = words(&[cursor.0, SOURCE.0, STIPPLE.0, 0, 0, 0, 0]);
        let bytes = resource_request(&mut state, &mut backend, &mut peer, sequence, 93, 0, &body);
        assert_no_error(&bytes, "GC access fixture CreateCursor");
        sequence += 1;
    }

    let glyph_cursor = ResourceId(0x0055_0032);
    let glyph_body = words(&[glyph_cursor.0, FONT.0, SECOND_FONT.0, 0, 0, 0, 0]);
    let bytes = resource_request(
        &mut state,
        &mut backend,
        &mut peer,
        sequence,
        94,
        0,
        &glyph_body,
    );
    assert_no_error(&bytes, "GC access fixture CreateGlyphCursor");

    GcFontAccessFixture {
        state,
        backend,
        peer,
        font: FONT,
        second_font: SECOND_FONT,
        source: SOURCE,
        destination: DESTINATION,
        tile: TILE,
        stipple: STIPPLE,
        clip_mask: CLIP_MASK,
        free_pixmap: FREE_PIXMAP,
        gc: GC,
        copy_gc: COPY_GC,
        free_gc: FREE_GC,
        cursor: CURSOR,
        free_cursor: FREE_CURSOR,
    }
}

#[derive(Clone)]
struct GcFontAccessRequest {
    label: &'static str,
    opcode: u8,
    data: u8,
    body: Vec<u8>,
    expected: Vec<(ResourceId, ResourceKind, AccessMode, Option<Creation>)>,
    deny_index: usize,
}

struct GcFontExpected {
    checks: Vec<(ResourceId, ResourceKind, AccessMode, Option<Creation>)>,
    deny_index: usize,
}

fn gc_font_expected(
    checks: Vec<(ResourceId, ResourceKind, AccessMode, Option<Creation>)>,
    deny_index: usize,
) -> GcFontExpected {
    GcFontExpected { checks, deny_index }
}

fn gc(
    (label, opcode, data): (&'static str, u8, u8),
    body: Vec<u8>,
    expected: GcFontExpected,
) -> GcFontAccessRequest {
    GcFontAccessRequest {
        label,
        opcode,
        data,
        body,
        expected: expected.checks,
        deny_index: expected.deny_index,
    }
}

fn gc_font_access_target(
    id: ResourceId,
    kind: ResourceKind,
    mode: AccessMode,
) -> (ResourceId, ResourceKind, AccessMode, Option<Creation>) {
    (id, kind, mode, None)
}

const R_OPEN: (&str, u8, u8) = ("OpenFont", 45, 0);
const R_QF: (&str, u8, u8) = ("QueryFont", 47, 0);
const R_QFGC: (&str, u8, u8) = ("QueryFont(GC fontable)", 47, 0);
const R_QTE: (&str, u8, u8) = ("QueryTextExtents", 48, 0);
const R_QTEGC: (&str, u8, u8) = ("QueryTextExtents(GC fontable)", 48, 0);
const R_CPX: (&str, u8, u8) = ("CreatePixmap", 53, 24);
const R_CG: (&str, u8, u8) = ("CreateGC", 55, 0);
const R_CHG: (&str, u8, u8) = ("ChangeGC", 56, 0);
const R_COPY: (&str, u8, u8) = ("CopyGC", 57, 0);
const R_DASHES: (&str, u8, u8) = ("SetDashes", 58, 0);
const R_CLIP: (&str, u8, u8) = ("SetClipRectangles", 59, 0);
const R_CC: (&str, u8, u8) = ("CreateCursor", 93, 0);
const R_CGC: (&str, u8, u8) = ("CreateGlyphCursor", 94, 0);
const R_RECOLOR: (&str, u8, u8) = ("RecolorCursor", 96, 0);
const R_CLOSE: (&str, u8, u8) = ("CloseFont", 46, 0);
const R_FPX: (&str, u8, u8) = ("FreePixmap", 54, 0);
const R_FGC: (&str, u8, u8) = ("FreeGC", 60, 0);
const R_FCUR: (&str, u8, u8) = ("FreeCursor", 95, 0);

type GcFontTarget = (ResourceId, ResourceKind, AccessMode, Option<Creation>);

fn created_target(
    id: ResourceId,
    kind: ResourceKind,
    mode: AccessMode,
    parent: Option<ResourceId>,
) -> GcFontTarget {
    (
        id,
        kind,
        mode,
        Some(Creation {
            parent,
            owner: CLIENT,
        }),
    )
}

fn one(id: ResourceId, kind: ResourceKind, mode: AccessMode) -> GcFontExpected {
    gc_font_expected(vec![gc_font_access_target(id, kind, mode)], 0)
}

fn open_font_expected() -> GcFontExpected {
    gc_font_expected(
        vec![created_target(
            ResourceId(0x0055_0022),
            ResourceKind::Font,
            AccessMode::CREATE,
            None,
        )],
        0,
    )
}

fn pixmap_expected(id: ResourceId) -> GcFontExpected {
    gc_font_expected(
        vec![
            gc_font_access_target(ROOT_WINDOW, ResourceKind::Window, AccessMode::GET_ATTR),
            created_target(
                id,
                ResourceKind::Pixmap,
                AccessMode::CREATE,
                Some(ROOT_WINDOW),
            ),
        ],
        1,
    )
}

fn gc_expected(fx: &GcFontAccessFixture) -> GcFontExpected {
    gc_font_expected(
        vec![
            gc_font_access_target(ROOT_WINDOW, ResourceKind::Window, AccessMode::GET_ATTR),
            gc_font_access_target(fx.tile, ResourceKind::Pixmap, AccessMode::READ),
            gc_font_access_target(fx.stipple, ResourceKind::Pixmap, AccessMode::READ),
            gc_font_access_target(fx.font, ResourceKind::Font, AccessMode::USE),
            gc_font_access_target(fx.clip_mask, ResourceKind::Pixmap, AccessMode::READ),
            created_target(
                ResourceId(0x0055_0013),
                ResourceKind::Gc,
                AccessMode::CREATE | AccessMode::SET_ATTR,
                Some(ROOT_WINDOW),
            ),
        ],
        5,
    )
}

fn changed_gc_expected(fx: &GcFontAccessFixture) -> GcFontExpected {
    gc_font_expected(
        vec![
            gc_font_access_target(fx.gc, ResourceKind::Gc, AccessMode::SET_ATTR),
            gc_font_access_target(fx.tile, ResourceKind::Pixmap, AccessMode::READ),
            gc_font_access_target(fx.stipple, ResourceKind::Pixmap, AccessMode::READ),
            gc_font_access_target(fx.font, ResourceKind::Font, AccessMode::USE),
            gc_font_access_target(fx.clip_mask, ResourceKind::Pixmap, AccessMode::READ),
        ],
        0,
    )
}

fn copy_gc_expected(fx: &GcFontAccessFixture) -> GcFontExpected {
    gc_font_expected(
        vec![
            gc_font_access_target(fx.gc, ResourceKind::Gc, AccessMode::GET_ATTR),
            gc_font_access_target(fx.copy_gc, ResourceKind::Gc, AccessMode::SET_ATTR),
        ],
        0,
    )
}

fn cursor_expected(fx: &GcFontAccessFixture, id: ResourceId) -> GcFontExpected {
    gc_font_expected(
        vec![
            gc_font_access_target(fx.source, ResourceKind::Pixmap, AccessMode::READ),
            gc_font_access_target(fx.stipple, ResourceKind::Pixmap, AccessMode::READ),
            created_target(id, ResourceKind::Cursor, AccessMode::CREATE, None),
        ],
        2,
    )
}

fn glyph_exp(fx: &GcFontAccessFixture, id: ResourceId) -> GcFontExpected {
    gc_font_expected(
        vec![
            gc_font_access_target(fx.font, ResourceKind::Font, AccessMode::USE),
            gc_font_access_target(fx.second_font, ResourceKind::Font, AccessMode::USE),
            created_target(id, ResourceKind::Cursor, AccessMode::CREATE, None),
        ],
        2,
    )
}

fn cursor_write(fx: &GcFontAccessFixture) -> GcFontExpected {
    one(fx.cursor, ResourceKind::Cursor, AccessMode::WRITE)
}

fn gc_font_access_requests(fx: &GcFontAccessFixture) -> Vec<GcFontAccessRequest> {
    use AccessMode as M;
    use ResourceKind as K;

    let gc_mask = (1 << 10) | (1 << 11) | (1 << 14) | (1 << 19);
    let create_gc_values = words(&[
        0x0055_0013,
        ROOT_WINDOW.0,
        gc_mask,
        fx.tile.0,
        fx.stipple.0,
        fx.font.0,
        fx.clip_mask.0,
    ]);
    let change_gc_values = words(&[
        fx.gc.0,
        gc_mask,
        fx.tile.0,
        fx.stipple.0,
        fx.font.0,
        fx.clip_mask.0,
    ]);
    let mut set_dashes = fx.gc.0.to_le_bytes().to_vec();
    set_dashes.extend_from_slice(&0u16.to_le_bytes());
    set_dashes.extend_from_slice(&2u16.to_le_bytes());
    set_dashes.extend_from_slice(&[1, 2, 0, 0]);
    let mut rc = fx.cursor.0.to_le_bytes().to_vec();
    rc.extend_from_slice(&[0; 12]);

    let mut open_font = 0x0055_0022u32.to_le_bytes().to_vec();
    open_font.extend_from_slice(&5u16.to_le_bytes());
    open_font.extend_from_slice(&[0, 0]);
    open_font.extend_from_slice(b"fixed");
    open_font.resize(open_font.len().next_multiple_of(4), 0);

    let create_pixmap_id = ResourceId(0x0055_0007);
    let create_cursor_id = ResourceId(0x0055_0033);
    let create_glyph_cursor_id = ResourceId(0x0055_0034);
    let qf = words(&[fx.font.0]);
    let qg = words(&[fx.gc.0]);
    let cpx = words(&[create_pixmap_id.0, ROOT_WINDOW.0, 8 | (8 << 16)]);
    let cc = words(&[create_cursor_id.0, fx.source.0, fx.stipple.0, 0, 0, 0, 0]);
    let cgc = words(&[
        create_glyph_cursor_id.0,
        fx.font.0,
        fx.second_font.0,
        0,
        0,
        0,
        0,
    ]);
    let copy = words(&[fx.gc.0, fx.copy_gc.0, 1 << 2]);
    let clip = words(&[fx.gc.0, 0]);
    let close = words(&[fx.second_font.0]);
    let fpx = words(&[fx.free_pixmap.0]);
    let fgc = words(&[fx.free_gc.0]);
    let fcur = words(&[fx.free_cursor.0]);

    vec![
        gc(R_OPEN, open_font, open_font_expected()),
        gc(R_QF, qf.clone(), one(fx.font, K::Font, M::GET_ATTR)),
        gc(R_QFGC, qg.clone(), one(fx.gc, K::Gc, M::GET_ATTR)),
        gc(R_QTE, qf, one(fx.font, K::Font, M::GET_ATTR)),
        gc(R_QTEGC, qg, one(fx.gc, K::Gc, M::GET_ATTR)),
        gc(R_CPX, cpx, pixmap_expected(create_pixmap_id)),
        gc(R_CG, create_gc_values, gc_expected(fx)),
        gc(R_CHG, change_gc_values, changed_gc_expected(fx)),
        gc(R_COPY, copy, copy_gc_expected(fx)),
        gc(R_DASHES, set_dashes, one(fx.gc, K::Gc, M::SET_ATTR)),
        gc(R_CLIP, clip, one(fx.gc, K::Gc, M::SET_ATTR)),
        gc(R_CC, cc, cursor_expected(fx, create_cursor_id)),
        gc(R_CGC, cgc, glyph_exp(fx, create_glyph_cursor_id)),
        gc(R_RECOLOR, rc, cursor_write(fx)),
        gc(R_CLOSE, close, one(fx.second_font, K::Font, M::DESTROY)),
        gc(R_FPX, fpx, one(fx.free_pixmap, K::Pixmap, M::DESTROY)),
        gc(R_FGC, fgc, one(fx.free_gc, K::Gc, M::DESTROY)),
        gc(R_FCUR, fcur, one(fx.free_cursor, K::Cursor, M::DESTROY)),
    ]
}

fn access_target_snapshot(
    state: &ServerState,
    id: ResourceId,
    kind: ResourceKind,
) -> Option<String> {
    match kind {
        ResourceKind::Pixmap => state.resources.pixmap(id).map(|entry| format!("{entry:?}")),
        ResourceKind::Gc => state.resources.gc(id).map(|entry| format!("{entry:?}")),
        ResourceKind::Font => state.resources.font(id).map(|entry| format!("{entry:?}")),
        ResourceKind::Cursor => state
            .resources
            .cursor_exists(id)
            .then(|| format!("host={:?}", state.resources.cursor_host_xid(id))),
        ResourceKind::Window => state.resources.window(id).map(|entry| format!("{entry:?}")),
        _ => panic!("unexpected GC/font access fixture kind: {kind:?}"),
    }
}

#[test]
fn gc_pixmap_cursor_font_requests_ask_expected_targets() {
    // Mutation: remove or reorder a GC, pixmap, font, or cursor check; the recorded target, mode,
    // creation, and core minor must differ.
    let mut fixture = gc_font_access_fixture();
    let cases = gc_font_access_requests(&fixture);
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .state
        .install_access_policy(Arc::new(RecordingExtensionPolicy::new(
            Decision::Allow,
            Arc::clone(&calls),
        )));

    let mut expected = Vec::new();
    for (sequence, case) in (70u16..).zip(cases) {
        let bytes = resource_request(
            &mut fixture.state,
            &mut fixture.backend,
            &mut fixture.peer,
            sequence,
            case.opcode,
            case.data,
            &case.body,
        );
        if matches!(case.opcode, 47 | 48) {
            assert_eq!(bytes.first(), Some(&1), "{} reply", case.label);
        } else {
            assert_no_error(&bytes, case.label);
        }
        for (id, kind, mode, creation) in case.expected {
            let owner = if creation.is_some() {
                None
            } else if id == ROOT_WINDOW {
                Some(crate::resources::SERVER_OWNER)
            } else {
                Some(CLIENT)
            };
            expected.push((
                CLIENT,
                ResourceTarget {
                    owner: None,
                    id,
                    kind,
                    creation,
                },
                mode,
                Some(RequestOpcodes {
                    major: case.opcode,
                    minor: 0,
                }),
                owner,
            ));
        }
    }
    let actual: Vec<_> = calls
        .lock()
        .unwrap()
        .iter()
        .map(|call| {
            (
                call.client,
                call.target.expect("resource target"),
                call.mode.expect("resource mode"),
                call.request,
                call.owner,
            )
        })
        .collect();
    assert_eq!(actual, expected);
    assert_eq!(
        fixture
            .state
            .resources
            .pixmap(ResourceId(0x0055_0007))
            .map(|_| true),
        Some(true),
        "CreatePixmap created its checked ID"
    );
    assert!(
        fixture
            .state
            .resources
            .gc(ResourceId(0x0055_0013))
            .is_some()
    );
    for cursor in [ResourceId(0x0055_0033), ResourceId(0x0055_0034)] {
        assert!(fixture.state.resources.cursor_exists(cursor));
    }
}

#[test]
fn gc_pixmap_cursor_font_requests_deny_before_effects() {
    // Mutation: remove the selected target check from a request row; that row must stop returning
    // the policy error before changing state or calling the backend.
    for case in gc_font_access_requests(&gc_font_access_fixture()) {
        let mut fixture = gc_font_access_fixture();
        let (target_id, target_kind, target_mode, creation) = case.expected[case.deny_index];
        let before_resource = access_target_snapshot(&fixture.state, target_id, target_kind);
        let before_backend = fixture.backend.calls();
        let calls = Arc::new(Mutex::new(Vec::new()));
        fixture
            .state
            .install_access_policy(Arc::new(window_resource_policy(
                Arc::clone(&calls),
                Some(Arc::new(AtomicU32::new(target_id.0))),
                Some(target_mode),
                false,
            )));

        let bytes = resource_request(
            &mut fixture.state,
            &mut fixture.backend,
            &mut fixture.peer,
            90,
            case.opcode,
            case.data,
            &case.body,
        );
        assert_x11_error(&bytes, x11::error::BAD_VALUE, 0x55, case.opcode, 90);
        assert_eq!(
            fixture.backend.calls(),
            before_backend,
            "{} backend",
            case.label
        );
        assert_eq!(
            access_target_snapshot(&fixture.state, target_id, target_kind),
            before_resource,
            "{} target state",
            case.label
        );
        if creation.is_some() {
            assert!(
                access_target_snapshot(&fixture.state, target_id, target_kind).is_none(),
                "{} denied creation installed a resource",
                case.label
            );
        }
        let recorded = calls.lock().unwrap();
        assert!(
            recorded.iter().any(|call| {
                call.client == CLIENT
                    && call
                        .target
                        .is_some_and(|target| target.id == target_id && target.kind == target_kind)
                    && call.mode == Some(target_mode)
            }),
            "{} selected check was not asked",
            case.label
        );
    }
}

fn deny_pixmap_reads() -> RecordingExtensionPolicy {
    RecordingExtensionPolicy::custom(Arc::new(Mutex::new(Vec::new())), "", |_, call| {
        if call.hook == ExtensionHook::Resource
            && call
                .target
                .is_some_and(|target| target.kind == ResourceKind::Pixmap)
            && call.mode == Some(AccessMode::READ)
        {
            Decision::Deny(DenyError {
                code: x11::error::BAD_ACCESS,
                value: 0x55,
            })
        } else {
            Decision::Allow
        }
    })
}

#[test]
fn close_font_and_free_pixmap_denied_keep_the_object() {
    // Mutation: perform the CloseFont and FreePixmap removals before checking DESTROY; after denial
    // the still-open font and pixmap must remain usable.
    let mut fixture = gc_font_access_fixture();
    let denied_id = Arc::new(AtomicU32::new(fixture.font.0));
    fixture
        .state
        .install_access_policy(Arc::new(window_resource_policy(
            Arc::new(Mutex::new(Vec::new())),
            Some(Arc::clone(&denied_id)),
            Some(AccessMode::DESTROY),
            false,
        )));

    let close_backend_before = fixture.backend.calls();
    let bytes = resource_request(
        &mut fixture.state,
        &mut fixture.backend,
        &mut fixture.peer,
        100,
        46,
        0,
        &words(&[fixture.font.0]),
    );
    assert_x11_error(&bytes, x11::error::BAD_VALUE, 0x55, 46, 100);
    assert_eq!(fixture.backend.calls(), close_backend_before);
    assert!(fixture.state.resources.font(fixture.font).is_some());
    let query_font = resource_request(
        &mut fixture.state,
        &mut fixture.backend,
        &mut fixture.peer,
        101,
        47,
        0,
        &words(&[fixture.font.0]),
    );
    assert_eq!(query_font.first(), Some(&1), "font remains queryable");

    denied_id.store(fixture.source.0, Ordering::SeqCst);
    let free_backend_before = fixture.backend.calls();
    let bytes = resource_request(
        &mut fixture.state,
        &mut fixture.backend,
        &mut fixture.peer,
        102,
        54,
        0,
        &words(&[fixture.source.0]),
    );
    assert_x11_error(&bytes, x11::error::BAD_VALUE, 0x55, 54, 102);
    assert_eq!(fixture.backend.calls(), free_backend_before);
    assert!(fixture.state.resources.pixmap(fixture.source).is_some());

    let before_copy = fixture.backend.calls();
    let mut copy_body = words(&[fixture.source.0, fixture.destination.0, fixture.gc.0]);
    copy_body.extend_from_slice(&0i16.to_le_bytes());
    copy_body.extend_from_slice(&0i16.to_le_bytes());
    copy_body.extend_from_slice(&0i16.to_le_bytes());
    copy_body.extend_from_slice(&0i16.to_le_bytes());
    copy_body.extend_from_slice(&8u16.to_le_bytes());
    copy_body.extend_from_slice(&8u16.to_le_bytes());
    let bytes = resource_request(
        &mut fixture.state,
        &mut fixture.backend,
        &mut fixture.peer,
        103,
        62,
        0,
        &copy_body,
    );
    assert_no_error(&bytes, "CopyArea from retained pixmap");
    assert!(
        fixture.backend.calls()[before_copy.len()..]
            .iter()
            .any(|call| matches!(call, RecordedCall::CopyArea { .. }))
    );
}

#[test]
fn create_gc_unresolved_tile_keeps_todays_behaviour() {
    // Mutation: check the CreateGC tile XID without first resolving it as a pixmap; the deny-pixmap
    // policy must then reject the request.
    let mut fixture = gc_font_access_fixture();
    fixture
        .state
        .install_access_policy(Arc::new(deny_pixmap_reads()));
    let gc = ResourceId(0x0055_0040);
    let missing_tile = ResourceId(0x0055_00ff);
    let body = words(&[gc.0, ROOT_WINDOW.0, 1 << 10, missing_tile.0]);

    let bytes = resource_request(
        &mut fixture.state,
        &mut fixture.backend,
        &mut fixture.peer,
        110,
        55,
        0,
        &body,
    );

    assert!(
        bytes.is_empty(),
        "unresolved tile changed CreateGC bytes: {bytes:02x?}"
    );
    assert!(fixture.state.resources.gc(gc).is_some());
}

#[test]
fn copy_gc_denial_preserves_destination_for_source_and_destination_denial() {
    // Mutation: copy the selected foreground before checking either GC; each denied row must leave
    // the destination snapshot unchanged.
    for deny_source in [true, false] {
        let mut fixture = gc_font_access_fixture();
        let (denied_id, denied_mode) = if deny_source {
            (fixture.gc, AccessMode::GET_ATTR)
        } else {
            (fixture.copy_gc, AccessMode::SET_ATTR)
        };
        let destination_before =
            access_target_snapshot(&fixture.state, fixture.copy_gc, ResourceKind::Gc);
        assert_ne!(
            fixture.state.resources.gc_foreground(fixture.gc),
            fixture.state.resources.gc_foreground(fixture.copy_gc),
            "CopyGC source and destination start with different foregrounds"
        );
        let backend_before = fixture.backend.calls();
        fixture
            .state
            .install_access_policy(Arc::new(window_resource_policy(
                Arc::new(Mutex::new(Vec::new())),
                Some(Arc::new(AtomicU32::new(denied_id.0))),
                Some(denied_mode),
                false,
            )));

        let bytes = resource_request(
            &mut fixture.state,
            &mut fixture.backend,
            &mut fixture.peer,
            120,
            57,
            0,
            &words(&[fixture.gc.0, fixture.copy_gc.0, 1 << 2]),
        );

        assert_x11_error(&bytes, x11::error::BAD_VALUE, 0x55, 57, 120);
        assert_eq!(fixture.backend.calls(), backend_before);
        assert_eq!(
            access_target_snapshot(&fixture.state, fixture.copy_gc, ResourceKind::Gc),
            destination_before,
            "denied CopyGC changed its destination"
        );
    }
}

#[test]
#[ignore]
fn bench_poly_point_with_and_without_policy() {
    const REQUESTS: u32 = 100_000;

    /// The unrecorded shared policy keeps the counter outside recorder bookkeeping.
    const GC_ID: ResourceId = ResourceId(0x0055_0090);

    let run = |with_policy: bool, counting: Option<Arc<std::sync::atomic::AtomicUsize>>| {
        let mut state = ServerState::new();
        let mut peer = install_client(&mut state, CLIENT.0);
        state.resources.create_gc(
            CLIENT,
            CreateGcRequest {
                gc: GC_ID,
                drawable: ROOT_WINDOW,
                function: None,
                plane_mask: None,
                foreground: None,
                background: None,
                line_width: None,
                line_style: None,
                cap_style: None,
                join_style: None,
                fill_style: None,
                fill_rule: None,
                tile: None,
                stipple: None,
                tile_x_origin: None,
                tile_y_origin: None,
                font: None,
                subwindow_mode: None,
                graphics_exposures: None,
                clip_x_origin: None,
                clip_y_origin: None,
                clip_mask: None,
                dash_offset: None,
                dashes: None,
                arc_mode: None,
            },
        );
        state
            .resources
            .window_mut(ROOT_WINDOW)
            .expect("root window")
            .host_xid = Some(crate::backend::WindowHandle::from_raw_for_test(0x100));
        let calls = Arc::new(Mutex::new(Vec::new()));
        if let Some(counting) = counting {
            state.install_access_policy(Arc::new(RecordingExtensionPolicy::custom_unrecorded(
                move |_, hook| {
                    if hook == ExtensionHook::Resource {
                        counting.fetch_add(1, Ordering::Relaxed);
                    }
                    Decision::Allow
                },
            )));
        } else if with_policy {
            state.install_access_policy(Arc::new(drawing_resource_policy(
                "PolyPoint benchmark allow-all",
                Arc::clone(&calls),
                None,
                false,
                Decision::Deny(DenyError {
                    code: x11::error::BAD_ACCESS,
                    value: 0x55,
                }),
            )));
        }

        let mut backend = RecordingBackend::new();
        let mut body = [0u8; 12];
        body[..4].copy_from_slice(&ROOT_WINDOW.0.to_le_bytes());
        body[4..8].copy_from_slice(&GC_ID.0.to_le_bytes());
        let start = std::time::Instant::now();
        for sequence in 0..REQUESTS {
            // Mutation: make `handle_poly_point` propagate an I/O error
            // instead of `Handled`; this request-level expect must fail.
            process_request(
                &mut state,
                &mut backend,
                CLIENT,
                SequenceNumber(sequence as u16),
                RequestHeader {
                    opcode: 64,
                    data: 0,
                    length_units: 4,
                },
                &body,
                None,
            )
            .expect("valid one-point PolyPoint request");
        }
        let elapsed = start.elapsed();
        let wire_bytes = read_all_available(&mut peer);
        let point_draws = backend
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| matches!(call, RecordedCall::PolyPoint { .. }))
            .count();
        let access_counts = {
            let calls = calls.lock().unwrap();
            let drawable_checks = calls
                .iter()
                .filter(|call| {
                    call.client == CLIENT
                        && call.request
                            == Some(RequestOpcodes {
                                major: 64,
                                minor: 0,
                            })
                        && call.target.is_some_and(|target| {
                            target.id == ROOT_WINDOW && target.kind == ResourceKind::Window
                        })
                        && call.mode == Some(AccessMode::WRITE)
                })
                .count();
            let gc_checks = calls
                .iter()
                .filter(|call| {
                    call.client == CLIENT
                        && call.request
                            == Some(RequestOpcodes {
                                major: 64,
                                minor: 0,
                            })
                        && call.target.is_some_and(|target| {
                            target.id == GC_ID && target.kind == ResourceKind::Gc
                        })
                        && call.mode == Some(AccessMode::USE)
                })
                .count();
            (calls.len(), drawable_checks, gc_checks)
        };
        (elapsed, wire_bytes, point_draws, access_counts)
    };

    let without_policy = run(false, None);
    let with_policy = run(true, None);
    let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counting = run(true, Some(Arc::clone(&counter)));
    assert!(
        counting.1.is_empty(),
        "counting-policy PolyPoint requests wrote an error or reply"
    );
    assert_eq!(counting.2, REQUESTS as usize, "counting-policy point draws");
    assert_eq!(
        counter.load(Ordering::Relaxed),
        REQUESTS as usize * 2,
        "counting policy asked once for drawable and once for GC per request"
    );
    assert!(
        without_policy.1.is_empty(),
        "no-policy PolyPoint requests wrote an error or reply"
    );
    assert!(
        with_policy.1.is_empty(),
        "allow-all-policy PolyPoint requests wrote an error or reply"
    );
    // Mutation: skip RecordingBackend::poly_point recording; each phase must record all request
    // draws.
    assert_eq!(without_policy.2, REQUESTS as usize, "no-policy point draws");
    assert_eq!(with_policy.2, REQUESTS as usize, "policy point draws");
    assert_eq!(without_policy.3, (0, 0, 0), "no-policy resource checks");
    assert_eq!(
        with_policy.3,
        (REQUESTS as usize * 2, REQUESTS as usize, REQUESTS as usize,),
        "allow-all policy asks once for drawable and once for GC per request"
    );
    println!(
        concat!(
            "PolyPoint ({} requests): no policy = {:.1} ns/request; ",
            "counting allow-all = {:.1} ns/request; ",
            "recording allow-all = {:.1} ns/request",
        ),
        REQUESTS,
        without_policy.0.as_nanos() as f64 / f64::from(REQUESTS),
        counting.0.as_nanos() as f64 / f64::from(REQUESTS),
        with_policy.0.as_nanos() as f64 / f64::from(REQUESTS),
    );
}
#[test]
fn set_dashes_checks_gc_before_zero_dash_validation() {
    // Mutation: move SetDashes' SET_ATTR check after zero-dash validation; the policy denial must
    // win over BadValue.
    let mut baseline = gc_font_access_fixture();
    let dashes = [baseline.gc.0.to_le_bytes().as_slice(), &[0, 0, 1, 0, 0]].concat();
    let baseline_bytes = resource_request(
        &mut baseline.state,
        &mut baseline.backend,
        &mut baseline.peer,
        110,
        58,
        0,
        &dashes,
    );
    assert_x11_error(&baseline_bytes, x11::error::BAD_VALUE, 0, 58, 110);

    let mut fixture = gc_font_access_fixture();
    let gc_before = format!("{:?}", fixture.state.resources.gc(fixture.gc));
    let backend_before = fixture.backend.calls();
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .state
        .install_access_policy(Arc::new(drawing_resource_policy(
            "SetDashes zero value order",
            Arc::clone(&calls),
            Some(Arc::new(AtomicU32::new(fixture.gc.0))),
            false,
            Decision::Deny(DenyError {
                code: x11::error::BAD_ACCESS,
                value: 0x55,
            }),
        )));
    let denied = resource_request(
        &mut fixture.state,
        &mut fixture.backend,
        &mut fixture.peer,
        111,
        58,
        0,
        &dashes,
    );

    assert_policy_error(&denied, 58, 111);
    assert_eq!(fixture.backend.calls(), backend_before);
    assert_eq!(
        format!("{:?}", fixture.state.resources.gc(fixture.gc)),
        gc_before
    );
    assert_eq!(
        expected_resource_checks(&calls.lock().unwrap()),
        [(
            CLIENT,
            fixture.gc,
            ResourceKind::Gc,
            AccessMode::SET_ATTR,
            Some(RequestOpcodes {
                major: 58,
                minor: 0,
            }),
        )]
    );
}
