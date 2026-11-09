use super::{
    extensions::{ExtensionHook, RecordedExtensionCheck, RecordingExtensionPolicy},
    *,
};
use crate::access::{
    AccessMode, Creation, Decision, DenyError, RequestOpcodes, ResourceKind, ResourceTarget,
};
use std::sync::{Arc, Mutex};
use yserver_protocol::x11::{self, CreateWindowRequest, ResourceId, glx as x11glx};

const CLIENT: ClientId = ClientId(1);
const CONTEXT: ResourceId = ResourceId(0x0010_7101);
const SHARE_CONTEXT: ResourceId = ResourceId(0x0010_7102);
const NEW_CONTEXT: ResourceId = ResourceId(0x0010_7111);
const WINDOW: ResourceId = ResourceId(0x0010_7121);
const DRAWABLE: ResourceId = ResourceId(0x0010_7122);
const PIXMAP: ResourceId = ResourceId(0x0010_7123);
const MISSING: ResourceId = ResourceId(0x0010_71f0);
const DENY_ERROR: DenyError = DenyError {
    code: x11::error::BAD_ACCESS,
    value: 0xA55A_1234,
};

struct GlxAccessFixture {
    state: ServerState,
    peer: UnixStream,
    backend: RecordingBackend,
}

fn fixture() -> GlxAccessFixture {
    let mut state = ServerState::new();
    let peer = install_client(&mut state, CLIENT.0);
    GlxAccessFixture {
        state,
        peer,
        backend: RecordingBackend::new(),
    }
}

fn dispatch(fixture: &mut GlxAccessFixture, minor: u8, body: &[u8]) -> Vec<u8> {
    process_request(
        &mut fixture.state,
        &mut fixture.backend,
        CLIENT,
        SequenceNumber(1),
        RequestHeader {
            opcode: crate::nested::GLX_MAJOR_OPCODE,
            data: minor,
            length_units: u32::try_from((4 + body.len()).div_ceil(4)).unwrap(),
        },
        body,
        None,
    )
    .expect("GLX request through process_request");
    read_all_or_buffered(&mut fixture.state, CLIENT.0, &mut fixture.peer)
}

fn words(values: &[u32]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

fn create_context_body(minor: u8, context: ResourceId, share: ResourceId) -> Vec<u8> {
    match minor {
        x11glx::CREATE_CONTEXT => {
            let mut body = words(&[context.0, crate::resources::ROOT_VISUAL.0, 0, share.0]);
            body.extend_from_slice(&[1, 0, 0, 0]);
            body
        }
        x11glx::CREATE_NEW_CONTEXT => {
            let mut body = words(&[context.0, 0x101, 0, x11glx::GLX_RGBA_TYPE, share.0]);
            body.extend_from_slice(&[1, 0, 0, 0]);
            body
        }
        x11glx::CREATE_CONTEXT_ATTRIBS_ARB => {
            let mut body = words(&[context.0, 0x101, 0, share.0]);
            body.extend_from_slice(&[1, 0, 0, 0]); // isDirect and padding
            body.extend_from_slice(&0u32.to_le_bytes()); // numAttribs
            body
        }
        _ => unreachable!("unsupported context creation minor {minor}"),
    }
}

fn create_context_sgix_body(context: ResourceId, share: ResourceId) -> Vec<u8> {
    let mut body = words(&[
        x11glx::VENDOR_CODE_CREATE_CONTEXT_WITH_CONFIG_SGIX,
        0,
        context.0,
        0x101,
        0,
        x11glx::GLX_RGBA_TYPE,
        share.0,
    ]);
    body.extend_from_slice(&[1, 0, 0, 0]); // isDirect and padding
    body
}

fn seed_context(state: &mut ServerState, id: ResourceId) {
    state.glx_contexts.insert(
        id.0,
        crate::server::GlxContext {
            owner: CLIENT,
            screen: 0,
            visual_id: crate::resources::ROOT_VISUAL.0,
            fbconfig: 0x101,
            render_type: x11glx::GLX_RGBA_TYPE,
            share_list: 0,
            is_direct: true,
        },
    );
}

fn seed_context_only(state: &mut ServerState) {
    seed_context(state, CONTEXT);
}

fn seed_nothing(_state: &mut ServerState) {}

fn seed_window_only(state: &mut ServerState) {
    seed_window(state, WINDOW);
}

fn seed_share_context(state: &mut ServerState) {
    seed_context(state, SHARE_CONTEXT);
}

fn seed_context_and_drawable(state: &mut ServerState) {
    seed_context(state, CONTEXT);
    seed_glx_drawable(state, DRAWABLE, WINDOW);
}

fn seed_context_drawable_and_window(state: &mut ServerState) {
    seed_context(state, CONTEXT);
    seed_glx_drawable(state, DRAWABLE, PIXMAP);
    seed_window(state, WINDOW);
}

fn seed_context_drawable_window_and_pixmap(state: &mut ServerState) {
    seed_context_drawable_and_window(state);
    seed_pixmap(state, PIXMAP);
}

fn seed_window(state: &mut ServerState, id: ResourceId) {
    state.resources.create_window(
        CLIENT,
        CreateWindowRequest {
            depth: 24,
            window: id,
            parent: crate::resources::ROOT_WINDOW,
            width: 8,
            height: 8,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
}

fn seed_pixmap(state: &mut ServerState, id: ResourceId) {
    state.resources.create_pixmap(
        CLIENT,
        x11::CreatePixmapRequest {
            depth: 24,
            pixmap: id,
            drawable: crate::resources::ROOT_WINDOW,
            width: 8,
            height: 8,
        },
    );
}

fn seed_glx_drawable(state: &mut ServerState, id: ResourceId, backing: ResourceId) {
    state.glx_drawables.insert(
        id.0,
        crate::server::GlxDrawable {
            owner: CLIENT,
            kind: crate::server::GlxDrawableKind::Window,
            x_drawable: backing.0,
            fbconfig: 0x101,
            width: 0,
            height: 0,
            event_mask: 0,
            glx_export_host_xid: None,
            texture_target: x11glx::GLX_TEXTURE_2D_EXT,
        },
    );
}

fn existing(id: ResourceId, kind: ResourceKind, mode: AccessMode) -> ExpectedCheck {
    ExpectedCheck {
        target: ResourceTarget {
            id,
            kind,
            creation: None,
            owner: None,
        },
        mode,
        owner: Some(CLIENT),
        target_owner: Some(CLIENT),
    }
}

fn creation(id: ResourceId) -> ExpectedCheck {
    ExpectedCheck {
        target: ResourceTarget {
            id,
            kind: ResourceKind::GlxContext,
            creation: Some(Creation {
                parent: None,
                owner: CLIENT,
            }),
            owner: None,
        },
        mode: AccessMode::CREATE,
        owner: None,
        target_owner: None,
    }
}

#[derive(Debug)]
struct ExpectedCheck {
    target: ResourceTarget,
    mode: AccessMode,
    owner: Option<ClientId>,
    target_owner: Option<ClientId>,
}

type AskRow = (u8, Vec<u8>, Vec<ExpectedCheck>, fn(&mut ServerState));

fn allow_policy(fixture: &mut GlxAccessFixture) -> Arc<Mutex<Vec<RecordedExtensionCheck>>> {
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .state
        .install_access_policy(Arc::new(RecordingExtensionPolicy::new(
            Decision::Allow,
            Arc::clone(&calls),
        )));
    calls
}

fn deny_policy(
    fixture: &mut GlxAccessFixture,
    id: ResourceId,
    mode: AccessMode,
) -> Arc<Mutex<Vec<RecordedExtensionCheck>>> {
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .state
        .install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
            Arc::clone(&calls),
            id,
            Some(mode),
            DENY_ERROR,
        )));
    calls
}

fn deny_all_policy(fixture: &mut GlxAccessFixture) -> Arc<Mutex<Vec<RecordedExtensionCheck>>> {
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .state
        .install_access_policy(Arc::new(RecordingExtensionPolicy::denying_all_resources(
            Arc::clone(&calls),
            DENY_ERROR,
        )));
    calls
}

fn assert_recorded(actual: &[RecordedExtensionCheck], expected: &[ExpectedCheck], minor: u8) {
    let resources = actual
        .iter()
        .filter(|call| call.hook == ExtensionHook::Resource)
        .collect::<Vec<_>>();
    assert_eq!(
        resources.len(),
        expected.len(),
        "actual checks: {actual:#?}"
    );
    for (call, expected) in resources.into_iter().zip(expected) {
        assert_eq!(call.hook, ExtensionHook::Resource);
        assert_eq!(call.client, CLIENT);
        assert_eq!(call.target, Some(expected.target));
        assert_eq!(call.mode, Some(expected.mode));
        assert_eq!(call.owner, expected.owner);
        assert_eq!(call.target_owner, expected.target_owner);
        assert_eq!(
            call.request,
            Some(RequestOpcodes {
                major: crate::nested::GLX_MAJOR_OPCODE,
                minor,
            })
        );
    }
}

fn assert_policy_error(bytes: &[u8], minor: u8) {
    assert_eq!(bytes.len(), 32, "expected one error packet: {bytes:02x?}");
    assert_eq!(bytes[0], 0);
    assert_eq!(bytes[1], DENY_ERROR.code);
    assert_eq!(u16::from_le_bytes(bytes[2..4].try_into().unwrap()), 1);
    assert_eq!(
        u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
        DENY_ERROR.value
    );
    assert_eq!(
        u16::from_le_bytes(bytes[8..10].try_into().unwrap()),
        u16::from(minor)
    );
    assert_eq!(bytes[10], crate::nested::GLX_MAJOR_OPCODE);
}

fn assert_error(bytes: &[u8], code: u8, value: u32, minor: u8) {
    assert_eq!(bytes.len(), 32, "expected one error packet: {bytes:02x?}");
    assert_eq!(bytes[0], 0);
    assert_eq!(bytes[1], code);
    assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), value);
    assert_eq!(
        u16::from_le_bytes(bytes[8..10].try_into().unwrap()),
        u16::from(minor)
    );
    assert_eq!(bytes[10], crate::nested::GLX_MAJOR_OPCODE);
}

fn current_body(minor: u8, draw: ResourceId, read: ResourceId, context: ResourceId) -> Vec<u8> {
    if minor == x11glx::MAKE_CURRENT {
        words(&[draw.0, context.0, 0])
    } else {
        words(&[0, draw.0, read.0, context.0])
    }
}

fn unsupported_vendor_body(code: u32) -> Vec<u8> {
    match code {
        1024 => words(&[code, CONTEXT.0]),
        5154 => words(&[code, 0, DRAWABLE.0]),
        65537 => words(&[code, 0, DRAWABLE.0, DRAWABLE.0, CONTEXT.0]),
        65543 => words(&[code, 0, CONTEXT.0, 0x101, 8, 8]),
        65544 => words(&[code, 0, DRAWABLE.0]),
        65545 | 65546 => words(&[code, DRAWABLE.0, 0]),
        _ => unreachable!("not a scoped unsupported private code: {code}"),
    }
}

// Mutation: parse SwapBuffers' contextTag at body[0..4] as the drawable instead of its
// drawable at body[4..8]; the resource-valued contextTag row must then miss the drawable check.
// Omitting or reordering the other context, share-list, drawable, or fallback checks also fails.
#[test]
fn glx_access_ask_table_records_each_target_and_opcode() {
    let mut rows: Vec<AskRow> = Vec::new();

    for minor in [
        x11glx::CREATE_CONTEXT,
        x11glx::CREATE_NEW_CONTEXT,
        x11glx::CREATE_CONTEXT_ATTRIBS_ARB,
    ] {
        rows.push((
            minor,
            create_context_body(minor, NEW_CONTEXT, SHARE_CONTEXT),
            vec![
                existing(SHARE_CONTEXT, ResourceKind::GlxContext, AccessMode::READ),
                creation(NEW_CONTEXT),
            ],
            seed_share_context,
        ));
    }
    rows.push((
        x11glx::CREATE_CONTEXT,
        create_context_body(x11glx::CREATE_CONTEXT, NEW_CONTEXT, MISSING),
        vec![creation(NEW_CONTEXT)],
        seed_nothing,
    ));
    for minor in [x11glx::VENDOR_PRIVATE, x11glx::VENDOR_PRIVATE_WITH_REPLY] {
        rows.push((
            minor,
            create_context_sgix_body(NEW_CONTEXT, SHARE_CONTEXT),
            vec![
                existing(SHARE_CONTEXT, ResourceKind::GlxContext, AccessMode::READ),
                creation(NEW_CONTEXT),
            ],
            seed_share_context,
        ));
    }
    rows.extend([
        (
            x11glx::DESTROY_CONTEXT,
            words(&[CONTEXT.0]),
            vec![existing(
                CONTEXT,
                ResourceKind::GlxContext,
                AccessMode::DESTROY,
            )],
            seed_context_only as fn(&mut ServerState),
        ),
        (
            x11glx::MAKE_CURRENT,
            current_body(x11glx::MAKE_CURRENT, DRAWABLE, ResourceId(0), CONTEXT),
            vec![
                existing(CONTEXT, ResourceKind::GlxContext, AccessMode::USE),
                existing(DRAWABLE, ResourceKind::GlxDrawable, AccessMode::WRITE),
            ],
            seed_context_and_drawable as fn(&mut ServerState),
        ),
        (
            x11glx::IS_DIRECT,
            words(&[CONTEXT.0]),
            vec![existing(
                CONTEXT,
                ResourceKind::GlxContext,
                AccessMode::READ,
            )],
            seed_context_only as fn(&mut ServerState),
        ),
        (
            x11glx::SWAP_BUFFERS,
            words(&[CONTEXT.0, DRAWABLE.0]),
            vec![existing(
                DRAWABLE,
                ResourceKind::GlxDrawable,
                AccessMode::WRITE,
            )],
            seed_context_and_drawable as fn(&mut ServerState),
        ),
        (
            x11glx::QUERY_CONTEXT,
            words(&[CONTEXT.0]),
            vec![existing(
                CONTEXT,
                ResourceKind::GlxContext,
                AccessMode::READ,
            )],
            seed_context_only as fn(&mut ServerState),
        ),
        (
            x11glx::MAKE_CONTEXT_CURRENT,
            current_body(x11glx::MAKE_CONTEXT_CURRENT, DRAWABLE, WINDOW, CONTEXT),
            vec![
                existing(CONTEXT, ResourceKind::GlxContext, AccessMode::USE),
                existing(DRAWABLE, ResourceKind::GlxDrawable, AccessMode::WRITE),
                existing(WINDOW, ResourceKind::Window, AccessMode::GET_ATTR),
            ],
            seed_context_drawable_and_window as fn(&mut ServerState),
        ),
    ]);

    for (minor, body, expected, seed) in rows {
        let mut fixture = fixture();
        seed(&mut fixture.state);
        let calls = allow_policy(&mut fixture);
        let _bytes = dispatch(&mut fixture, minor, &body);
        let calls = calls.lock().unwrap();
        let resources = calls
            .iter()
            .filter(|call| call.hook == ExtensionHook::Resource)
            .cloned()
            .collect::<Vec<_>>();
        assert_recorded(&resources, &expected, minor);
    }
}

// Mutation: bypass the make-current draw fallback resolver or assign WRITE to a core window.
#[test]
fn make_current_draw_falls_back_to_core_window() {
    let mut fixture = fixture();
    seed_context(&mut fixture.state, CONTEXT);
    seed_window(&mut fixture.state, WINDOW);
    let calls = allow_policy(&mut fixture);
    let _bytes = dispatch(
        &mut fixture,
        x11glx::MAKE_CURRENT,
        &current_body(x11glx::MAKE_CURRENT, WINDOW, ResourceId(0), CONTEXT),
    );
    assert_recorded(
        &calls.lock().unwrap(),
        &[
            existing(CONTEXT, ResourceKind::GlxContext, AccessMode::USE),
            existing(WINDOW, ResourceKind::Window, AccessMode::GET_ATTR),
        ],
        x11glx::MAKE_CURRENT,
    );
}

// Mutation: treat a core pixmap read-drawable as a GLX drawable or skip its GET_ATTR check.
#[test]
fn make_context_current_read_falls_back_to_core_pixmap() {
    let mut fixture = fixture();
    seed_context(&mut fixture.state, CONTEXT);
    seed_window(&mut fixture.state, WINDOW);
    seed_pixmap(&mut fixture.state, PIXMAP);
    let calls = allow_policy(&mut fixture);
    let _bytes = dispatch(
        &mut fixture,
        x11glx::MAKE_CONTEXT_CURRENT,
        &current_body(x11glx::MAKE_CONTEXT_CURRENT, WINDOW, PIXMAP, CONTEXT),
    );
    assert_recorded(
        &calls.lock().unwrap(),
        &[
            existing(CONTEXT, ResourceKind::GlxContext, AccessMode::USE),
            existing(WINDOW, ResourceKind::Window, AccessMode::GET_ATTR),
            existing(PIXMAP, ResourceKind::Pixmap, AccessMode::GET_ATTR),
        ],
        x11glx::MAKE_CONTEXT_CURRENT,
    );
}

// Mutation: ask for any unresolved context/draw/read XID during current-state dispatch.
#[test]
fn make_current_unresolved_ids_are_not_asked() {
    let rows = [
        (
            x11glx::MAKE_CURRENT,
            current_body(x11glx::MAKE_CURRENT, MISSING, ResourceId(0), CONTEXT),
            seed_nothing as fn(&mut ServerState),
        ),
        (
            x11glx::MAKE_CONTEXT_CURRENT,
            current_body(x11glx::MAKE_CONTEXT_CURRENT, MISSING, MISSING, CONTEXT),
            seed_nothing,
        ),
        (x11glx::DESTROY_CONTEXT, words(&[MISSING.0]), seed_nothing),
        (
            x11glx::DESTROY_CONTEXT,
            words(&[WINDOW.0]),
            seed_window_only,
        ),
        (x11glx::SWAP_BUFFERS, words(&[0, MISSING.0]), seed_nothing),
        (
            x11glx::SWAP_BUFFERS,
            words(&[0, CONTEXT.0]),
            seed_context_only,
        ),
        (
            x11glx::MAKE_CURRENT,
            current_body(x11glx::MAKE_CURRENT, MISSING, ResourceId(0), WINDOW),
            seed_window_only,
        ),
    ];
    for (minor, body, seed) in rows {
        let mut no_policy = fixture();
        seed(&mut no_policy.state);
        let no_policy_bytes = dispatch(&mut no_policy, minor, &body);

        let mut fixture = fixture();
        seed(&mut fixture.state);
        let calls = allow_policy(&mut fixture);
        let bytes = dispatch(&mut fixture, minor, &body);
        assert_eq!(bytes, no_policy_bytes, "policy changed minor {minor}");
        assert!(
            calls
                .lock()
                .unwrap()
                .iter()
                .all(|call| call.hook != ExtensionHook::Resource),
            "minor {minor}"
        );
    }

    for minor in [x11glx::IS_DIRECT, x11glx::QUERY_CONTEXT] {
        let body = words(&[MISSING.0]);
        let mut no_policy = fixture();
        let no_policy_bytes = dispatch(&mut no_policy, minor, &body);

        let mut fixture = fixture();
        let calls = allow_policy(&mut fixture);
        let bytes = dispatch(&mut fixture, minor, &body);
        assert_eq!(bytes, no_policy_bytes, "policy changed minor {minor}");
        assert_error(
            &bytes,
            crate::nested::GLX_FIRST_ERROR + x11glx::ERROR_GLX_BAD_CONTEXT,
            MISSING.0,
            minor,
        );
        assert!(
            calls
                .lock()
                .unwrap()
                .iter()
                .all(|call| call.hook != ExtensionHook::Resource),
            "minor {minor}"
        );
    }
}

// Mutation: parse a one-word SwapBuffers contextTag as the drawable; without the second wire
// word, even a contextTag matching a core Window must not trigger a resource check.
#[test]
fn swap_buffers_short_body_keeps_no_op_without_access_check() {
    let mut no_policy = fixture();
    seed_window(&mut no_policy.state, WINDOW);
    let no_policy_bytes = dispatch(&mut no_policy, x11glx::SWAP_BUFFERS, &words(&[WINDOW.0]));

    let mut fixture = fixture();
    seed_window(&mut fixture.state, WINDOW);
    let calls = allow_policy(&mut fixture);
    let bytes = dispatch(&mut fixture, x11glx::SWAP_BUFFERS, &words(&[WINDOW.0]));

    assert_eq!(bytes, no_policy_bytes);
    assert!(bytes.is_empty());
    assert_recorded(&calls.lock().unwrap(), &[], x11glx::SWAP_BUFFERS);
}

// Mutation: run the create check before the share READ, or keep the new record after share denial.
#[test]
fn create_context_denied_share_creates_nothing() {
    let mut fixture = fixture();
    seed_context(&mut fixture.state, SHARE_CONTEXT);
    let calls = deny_policy(&mut fixture, SHARE_CONTEXT, AccessMode::READ);
    let bytes = dispatch(
        &mut fixture,
        x11glx::CREATE_CONTEXT,
        &create_context_body(x11glx::CREATE_CONTEXT, NEW_CONTEXT, SHARE_CONTEXT),
    );
    assert_policy_error(&bytes, x11glx::CREATE_CONTEXT);
    assert!(!fixture.state.glx_contexts.contains_key(&NEW_CONTEXT.0));
    assert_recorded(
        &calls.lock().unwrap(),
        &[existing(
            SHARE_CONTEXT,
            ResourceKind::GlxContext,
            AccessMode::READ,
        )],
        x11glx::CREATE_CONTEXT,
    );
    assert!(fixture.backend.calls().is_empty());
}

// Mutation: parse SwapBuffers' contextTag at body[0..4] as its drawable; the drawable denial
// row then fails the policy-error and exact-target assertions. Removing any other GLX denial
// check also fails its row below.
#[test]
fn glx_access_deny_table_returns_error_and_preserves_state() {
    struct DenyRow {
        minor: u8,
        body: Vec<u8>,
        target: ResourceId,
        mode: AccessMode,
        expected: Vec<ExpectedCheck>,
        seed: fn(&mut ServerState),
        absent_after: Option<ResourceId>,
    }

    let mut rows = Vec::new();
    for minor in [
        x11glx::CREATE_CONTEXT,
        x11glx::CREATE_NEW_CONTEXT,
        x11glx::CREATE_CONTEXT_ATTRIBS_ARB,
    ] {
        rows.push(DenyRow {
            minor,
            body: create_context_body(minor, NEW_CONTEXT, ResourceId(0)),
            target: NEW_CONTEXT,
            mode: AccessMode::CREATE,
            expected: vec![creation(NEW_CONTEXT)],
            seed: |_| {},
            absent_after: Some(NEW_CONTEXT),
        });
    }
    for minor in [x11glx::VENDOR_PRIVATE, x11glx::VENDOR_PRIVATE_WITH_REPLY] {
        rows.push(DenyRow {
            minor,
            body: create_context_sgix_body(NEW_CONTEXT, ResourceId(0)),
            target: NEW_CONTEXT,
            mode: AccessMode::CREATE,
            expected: vec![creation(NEW_CONTEXT)],
            seed: |_| {},
            absent_after: Some(NEW_CONTEXT),
        });
    }
    for minor in [
        x11glx::CREATE_CONTEXT,
        x11glx::CREATE_NEW_CONTEXT,
        x11glx::CREATE_CONTEXT_ATTRIBS_ARB,
    ] {
        rows.push(DenyRow {
            minor,
            body: create_context_body(minor, NEW_CONTEXT, SHARE_CONTEXT),
            target: SHARE_CONTEXT,
            mode: AccessMode::READ,
            expected: vec![existing(
                SHARE_CONTEXT,
                ResourceKind::GlxContext,
                AccessMode::READ,
            )],
            seed: seed_share_context,
            absent_after: Some(NEW_CONTEXT),
        });
    }
    for minor in [x11glx::VENDOR_PRIVATE, x11glx::VENDOR_PRIVATE_WITH_REPLY] {
        rows.push(DenyRow {
            minor,
            body: create_context_sgix_body(NEW_CONTEXT, SHARE_CONTEXT),
            target: SHARE_CONTEXT,
            mode: AccessMode::READ,
            expected: vec![existing(
                SHARE_CONTEXT,
                ResourceKind::GlxContext,
                AccessMode::READ,
            )],
            seed: seed_share_context,
            absent_after: Some(NEW_CONTEXT),
        });
    }
    rows.extend([
        DenyRow {
            minor: x11glx::DESTROY_CONTEXT,
            body: words(&[CONTEXT.0]),
            target: CONTEXT,
            mode: AccessMode::DESTROY,
            expected: vec![existing(
                CONTEXT,
                ResourceKind::GlxContext,
                AccessMode::DESTROY,
            )],
            seed: |state| seed_context(state, CONTEXT),
            absent_after: None,
        },
        DenyRow {
            minor: x11glx::MAKE_CURRENT,
            body: current_body(x11glx::MAKE_CURRENT, WINDOW, ResourceId(0), CONTEXT),
            target: CONTEXT,
            mode: AccessMode::USE,
            expected: vec![existing(CONTEXT, ResourceKind::GlxContext, AccessMode::USE)],
            seed: |state| {
                seed_context(state, CONTEXT);
                seed_window(state, WINDOW);
            },
            absent_after: None,
        },
        DenyRow {
            minor: x11glx::IS_DIRECT,
            body: words(&[CONTEXT.0]),
            target: CONTEXT,
            mode: AccessMode::READ,
            expected: vec![existing(
                CONTEXT,
                ResourceKind::GlxContext,
                AccessMode::READ,
            )],
            seed: |state| seed_context(state, CONTEXT),
            absent_after: None,
        },
        DenyRow {
            minor: x11glx::QUERY_CONTEXT,
            body: words(&[CONTEXT.0]),
            target: CONTEXT,
            mode: AccessMode::READ,
            expected: vec![existing(
                CONTEXT,
                ResourceKind::GlxContext,
                AccessMode::READ,
            )],
            seed: |state| seed_context(state, CONTEXT),
            absent_after: None,
        },
        DenyRow {
            minor: x11glx::SWAP_BUFFERS,
            body: words(&[CONTEXT.0, DRAWABLE.0]),
            target: DRAWABLE,
            mode: AccessMode::WRITE,
            expected: vec![existing(
                DRAWABLE,
                ResourceKind::GlxDrawable,
                AccessMode::WRITE,
            )],
            seed: |state| {
                seed_context(state, CONTEXT);
                seed_glx_drawable(state, DRAWABLE, WINDOW);
            },
            absent_after: None,
        },
        DenyRow {
            minor: x11glx::MAKE_CURRENT,
            body: current_body(x11glx::MAKE_CURRENT, DRAWABLE, ResourceId(0), CONTEXT),
            target: DRAWABLE,
            mode: AccessMode::WRITE,
            expected: vec![
                existing(CONTEXT, ResourceKind::GlxContext, AccessMode::USE),
                existing(DRAWABLE, ResourceKind::GlxDrawable, AccessMode::WRITE),
            ],
            seed: seed_context_and_drawable,
            absent_after: None,
        },
        DenyRow {
            minor: x11glx::MAKE_CURRENT,
            body: current_body(x11glx::MAKE_CURRENT, WINDOW, ResourceId(0), CONTEXT),
            target: WINDOW,
            mode: AccessMode::GET_ATTR,
            expected: vec![
                existing(CONTEXT, ResourceKind::GlxContext, AccessMode::USE),
                existing(WINDOW, ResourceKind::Window, AccessMode::GET_ATTR),
            ],
            seed: |state| {
                seed_context(state, CONTEXT);
                seed_window(state, WINDOW);
            },
            absent_after: None,
        },
        DenyRow {
            minor: x11glx::MAKE_CONTEXT_CURRENT,
            body: current_body(x11glx::MAKE_CONTEXT_CURRENT, DRAWABLE, PIXMAP, CONTEXT),
            target: PIXMAP,
            mode: AccessMode::GET_ATTR,
            expected: vec![
                existing(CONTEXT, ResourceKind::GlxContext, AccessMode::USE),
                existing(DRAWABLE, ResourceKind::GlxDrawable, AccessMode::WRITE),
                existing(PIXMAP, ResourceKind::Pixmap, AccessMode::GET_ATTR),
            ],
            seed: seed_context_drawable_window_and_pixmap,
            absent_after: None,
        },
        DenyRow {
            minor: x11glx::SWAP_BUFFERS,
            body: words(&[CONTEXT.0, WINDOW.0]),
            target: WINDOW,
            mode: AccessMode::GET_ATTR,
            expected: vec![existing(WINDOW, ResourceKind::Window, AccessMode::GET_ATTR)],
            seed: |state| seed_window(state, WINDOW),
            absent_after: None,
        },
    ]);

    for row in rows {
        let mut fixture = fixture();
        (row.seed)(&mut fixture.state);
        let before = format!(
            "contexts={:?};drawables={:?};tag={}",
            fixture.state.glx_contexts,
            fixture.state.glx_drawables,
            fixture.state.glx_next_context_tag
        );
        let calls = deny_policy(&mut fixture, row.target, row.mode);
        let bytes = dispatch(&mut fixture, row.minor, &row.body);
        assert_policy_error(&bytes, row.minor);
        if let Some(id) = row.absent_after {
            assert!(!fixture.state.glx_contexts.contains_key(&id.0));
        }
        assert_eq!(
            format!(
                "contexts={:?};drawables={:?};tag={}",
                fixture.state.glx_contexts,
                fixture.state.glx_drawables,
                fixture.state.glx_next_context_tag
            ),
            before,
            "GLX state changed on denial for minor {}",
            row.minor
        );
        assert!(fixture.backend.calls().is_empty());
        assert_recorded(&calls.lock().unwrap(), &row.expected, row.minor);
    }
}

// Mutation: recognize unsupported GLX object operations and consult the deny-all policy.
#[test]
fn unsupported_glx_requests_keep_their_reply_and_error_without_checks() {
    let unsupported_private = [1024_u32, 5154, 65537, 65543, 65544, 65545, 65546];
    let mut requests = vec![
        (
            x11glx::RENDER,
            words(&[CONTEXT.0]),
            Some(crate::nested::GLX_FIRST_ERROR + x11glx::ERROR_GLX_BAD_RENDER_REQUEST),
        ),
        (
            2,
            words(&[CONTEXT.0]),
            Some(crate::nested::GLX_FIRST_ERROR + x11glx::ERROR_GLX_BAD_RENDER_REQUEST),
        ),
        (
            x11glx::COPY_CONTEXT,
            words(&[CONTEXT.0, CONTEXT.0, 0]),
            Some(crate::nested::GLX_FIRST_ERROR + x11glx::ERROR_GLX_BAD_RENDER_REQUEST),
        ),
        (
            12,
            words(&[0, CONTEXT.0, 1, 0]),
            Some(crate::nested::GLX_FIRST_ERROR + x11glx::ERROR_GLX_BAD_RENDER_REQUEST),
        ),
        (x11glx::WAIT_GL, words(&[CONTEXT.0]), None),
        (x11glx::WAIT_X, words(&[CONTEXT.0]), None),
    ];
    for minor in [x11glx::VENDOR_PRIVATE, x11glx::VENDOR_PRIVATE_WITH_REPLY] {
        for code in unsupported_private {
            requests.push((
                minor,
                unsupported_vendor_body(code),
                Some(
                    crate::nested::GLX_FIRST_ERROR + x11glx::ERROR_GLX_UNSUPPORTED_PRIVATE_REQUEST,
                ),
            ));
        }
    }

    for (minor, body, error) in requests {
        let mut no_policy = fixture();
        seed_context_and_drawable(&mut no_policy.state);
        seed_window(&mut no_policy.state, WINDOW);
        seed_pixmap(&mut no_policy.state, PIXMAP);
        let no_policy_bytes = dispatch(&mut no_policy, minor, &body);
        let mut fixture = fixture();
        seed_context_and_drawable(&mut fixture.state);
        seed_window(&mut fixture.state, WINDOW);
        seed_pixmap(&mut fixture.state, PIXMAP);
        let calls = deny_all_policy(&mut fixture);
        let bytes = dispatch(&mut fixture, minor, &body);
        assert_eq!(bytes, no_policy_bytes, "deny-all changed minor {minor}");
        if let Some(code) = error {
            assert_error(&bytes, code, 0, minor);
        } else {
            assert!(bytes.is_empty(), "minor {minor} should remain a no-op");
        }
        assert!(
            calls
                .lock()
                .unwrap()
                .iter()
                .all(|call| call.hook != ExtensionHook::Resource),
            "minor {minor}"
        );
    }
}
