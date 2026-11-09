use super::{
    extensions::{ExtensionHook, RecordedExtensionCheck, RecordingExtensionPolicy},
    *,
};
use crate::access::{
    AccessMode, Creation, Decision, DenyError, RequestOpcodes, ResourceKind, ResourceTarget,
};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

pub(super) const CLIENT: ClientId = ClientId(1);
const REQUEST: RequestOpcodes = RequestOpcodes { major: 1, minor: 0 };
type ExpectedResourceAccess = (ResourceId, ResourceKind, AccessMode);
type WindowAccessCase = (u8, u8, Vec<u8>, Vec<ExpectedResourceAccess>);
pub(super) type RecordedResourceExpectation = (
    ClientId,
    ResourceId,
    ResourceKind,
    AccessMode,
    Option<RequestOpcodes>,
);

fn ask_owner(
    state: &ServerState,
    id: ResourceId,
    kind: ResourceKind,
    calls: &Arc<Mutex<Vec<RecordedExtensionCheck>>>,
) -> Option<ClientId> {
    let before = calls.lock().unwrap().len();
    assert_eq!(
        state.check_resource(
            CLIENT,
            &ResourceTarget {
                owner: None,
                id,
                kind,
                creation: None,
            },
            AccessMode::READ,
            Some(REQUEST),
        ),
        Decision::Allow
    );
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), before + 1);
    let call = &calls[before];
    assert_eq!(call.hook, ExtensionHook::Resource);
    assert_eq!(
        call.target.as_ref().map(|target| (target.id, target.kind)),
        Some((id, kind))
    );
    assert_eq!(call.request, Some(REQUEST));
    call.owner
}

pub(super) fn resource_request(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    peer: &mut UnixStream,
    sequence: u16,
    opcode: u8,
    data: u8,
    body: &[u8],
) -> Vec<u8> {
    process_request(
        state,
        backend,
        CLIENT,
        SequenceNumber(sequence),
        RequestHeader {
            opcode,
            data,
            length_units: u32::try_from(1 + body.len().div_ceil(4)).unwrap(),
        },
        body,
        None,
    )
    .expect("resource creation request through process_request");
    read_all_available(peer)
}

#[cfg(unix)]
fn resource_request_with_fd(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    peer: &mut UnixStream,
    sequence: u16,
    opcode: u8,
    data: u8,
    body: &[u8],
    fd: std::os::fd::OwnedFd,
) -> Vec<u8> {
    process_request(
        state,
        backend,
        CLIENT,
        SequenceNumber(sequence),
        RequestHeader {
            opcode,
            data,
            length_units: u32::try_from(1 + body.len().div_ceil(4)).unwrap(),
        },
        body,
        Some(fd),
    )
    .expect("fd-backed resource creation request through process_request");
    read_all_available(peer)
}

pub(super) fn words(words: &[u32]) -> Vec<u8> {
    words
        .iter()
        .fold(
            WireBody(ClientByteOrder::LittleEndian, Vec::new()),
            |body, word| body.u32(*word),
        )
        .1
}

fn create_owner_rows(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    peer: &mut UnixStream,
) -> Vec<(ResourceKind, ResourceId, ClientId)> {
    let id = |offset: u32| ResourceId(0x0050_0000 + offset);
    let mut sequence = 1;
    let mut rows = Vec::new();
    macro_rules! request {
        ($opcode:expr, $data:expr, $body:expr $(,)?) => {{
            let bytes = resource_request(state, backend, peer, sequence, $opcode, $data, $body);
            assert_no_error(&bytes, "resource creation request");
            sequence += 1;
        }};
    }

    let window = id(1);
    let create_window = words(&[
        window.0,
        ROOT_WINDOW.0,
        0,
        8 | (8 << 16),
        0,
        crate::resources::ROOT_VISUAL.0,
        0,
    ]);
    request!(1, 24, &create_window);
    rows.push((ResourceKind::Window, window, CLIENT));

    let pixmap = id(2);
    request!(53, 24, &words(&[pixmap.0, ROOT_WINDOW.0, 8 | (8 << 16)]),);
    rows.push((ResourceKind::Pixmap, pixmap, CLIENT));

    let gc = id(3);
    request!(55, 0, &words(&[gc.0, ROOT_WINDOW.0, 0]));
    rows.push((ResourceKind::Gc, gc, CLIENT));

    let font = id(4);
    let mut open_font = font.0.to_le_bytes().to_vec();
    open_font.extend_from_slice(&5u16.to_le_bytes());
    open_font.extend_from_slice(&[0, 0]);
    open_font.extend_from_slice(b"fixed");
    open_font.resize(open_font.len().div_ceil(4) * 4, 0);
    request!(45, 0, &open_font);
    rows.push((ResourceKind::Font, font, CLIENT));

    let cursor = id(5);
    let mut create_cursor = words(&[cursor.0, pixmap.0, 0]);
    create_cursor.extend_from_slice(&[0; 16]);
    request!(93, 0, &create_cursor);
    rows.push((ResourceKind::Cursor, cursor, CLIENT));

    let colormap = id(6);
    request!(
        78,
        0,
        &words(&[colormap.0, ROOT_WINDOW.0, crate::resources::ROOT_VISUAL.0,]),
    );
    rows.push((ResourceKind::Colormap, colormap, CLIENT));

    let picture = id(7);
    request!(
        133,
        4,
        &words(&[
            picture.0,
            ROOT_WINDOW.0,
            yserver_protocol::x11::RENDER_FMT_XRGB32,
            0,
        ]),
    );
    rows.push((ResourceKind::Picture, picture, CLIENT));

    let glyphset = id(8);
    // Mutation: make the recording renderer return no glyphset; this row's owner assertion must
    // fail.
    backend.render_create_glyphset_returns_handle = true;
    request!(
        133,
        17,
        &words(&[glyphset.0, yserver_protocol::x11::RENDER_FMT_XRGB32]),
    );
    rows.push((ResourceKind::GlyphSet, glyphset, CLIENT));

    state.xfixes_client_major.insert(CLIENT.0, 5);
    let region = id(9);
    request!(
        140,
        yserver_protocol::x11::xfixes::CREATE_REGION,
        &region.0.to_le_bytes()
    );
    rows.push((ResourceKind::Region, region, CLIENT));

    let barrier = id(10);
    let mut create_barrier = words(&[barrier.0, ROOT_WINDOW.0]);
    create_barrier.extend_from_slice(&0i16.to_le_bytes());
    create_barrier.extend_from_slice(&0i16.to_le_bytes());
    create_barrier.extend_from_slice(&1i16.to_le_bytes());
    create_barrier.extend_from_slice(&0i16.to_le_bytes());
    create_barrier.extend_from_slice(&0u32.to_le_bytes());
    create_barrier.extend_from_slice(&[0; 4]);
    request!(
        140,
        yserver_protocol::x11::xfixes::CREATE_POINTER_BARRIER,
        &create_barrier,
    );
    rows.push((ResourceKind::PointerBarrier, barrier, CLIENT));

    let counter = id(11);
    request!(
        142,
        yserver_protocol::x11::sync::CREATE_COUNTER,
        &words(&[counter.0, 0, 0]),
    );
    rows.push((ResourceKind::SyncCounter, counter, CLIENT));

    let alarm = id(12);
    request!(
        142,
        yserver_protocol::x11::sync::CREATE_ALARM,
        &words(&[alarm.0, 0]),
    );
    rows.push((ResourceKind::SyncAlarm, alarm, CLIENT));

    let fence = id(13);
    request!(
        142,
        yserver_protocol::x11::sync::CREATE_FENCE,
        &[ROOT_WINDOW.0.to_le_bytes(), fence.0.to_le_bytes(), [0; 4],].concat(),
    );
    rows.push((ResourceKind::SyncFence, fence, CLIENT));

    let damage = id(14);
    request!(
        143,
        yserver_protocol::x11::damage::CREATE,
        &words(&[damage.0, ROOT_WINDOW.0, 0])
    );
    rows.push((ResourceKind::Damage, damage, CLIENT));

    let present_selection = id(15);
    request!(
        145,
        yserver_protocol::x11::present::SELECT_INPUT,
        &words(&[present_selection.0, ROOT_WINDOW.0, 0]),
    );
    rows.push((
        ResourceKind::PresentEventSelection,
        present_selection,
        CLIENT,
    ));

    #[cfg(unix)]
    {
        let shmseg = id(16);
        let path = std::env::temp_dir().join(format!(
            "yserver-access-owner-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        file.set_len(4096).unwrap();
        let mut shm_attach = shmseg.0.to_le_bytes().to_vec();
        shm_attach.extend_from_slice(&[0; 4]);
        let bytes = resource_request_with_fd(
            state,
            backend,
            peer,
            sequence,
            130,
            yserver_protocol::x11::mit_shm::ATTACH_FD,
            &shm_attach,
            file.into(),
        );
        assert_no_error(&bytes, "MIT-SHM AttachFd");
        sequence += 1;
        std::fs::remove_file(path).unwrap();
        rows.push((ResourceKind::ShmSegment, shmseg, CLIENT));
    }

    let context = id(17);
    request!(
        crate::nested::GLX_MAJOR_OPCODE,
        yserver_protocol::x11::glx::CREATE_CONTEXT,
        &words(&[context.0, crate::resources::ROOT_VISUAL.0, 0, 0, 1]),
    );
    rows.push((ResourceKind::GlxContext, context, CLIENT));

    let glx_drawable = id(18);
    request!(
        crate::nested::GLX_MAJOR_OPCODE,
        yserver_protocol::x11::glx::CREATE_WINDOW,
        &words(&[0, 0x101, ROOT_WINDOW.0, glx_drawable.0,]),
    );
    rows.push((ResourceKind::GlxDrawable, glx_drawable, CLIENT));

    #[cfg(unix)]
    {
        let syncobj = id(19);
        backend.dri3_caps = crate::backend::Dri3Caps {
            version: (1, 4),
            modifiers: false,
            fence_fd: false,
            syncobj: true,
        };
        let path = std::env::temp_dir().join(format!(
            "yserver-access-syncobj-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        file.set_len(4096).unwrap();
        let bytes = resource_request_with_fd(
            state,
            backend,
            peer,
            sequence,
            crate::nested::DRI3_MAJOR_OPCODE,
            yserver_protocol::x11::dri3::IMPORT_SYNCOBJ,
            &words(&[syncobj.0, ROOT_WINDOW.0]),
            file.into(),
        );
        assert_no_error(&bytes, "DRI3 ImportSyncobj");
        sequence += 1;
        std::fs::remove_file(path).unwrap();
        rows.push((ResourceKind::Dri3Syncobj, syncobj, CLIENT));
    }

    let record = id(20);
    let mut create_record = words(&[record.0]);
    create_record.extend_from_slice(&[0; 4]);
    create_record.extend_from_slice(&words(&[0, 0]));
    let bytes = resource_request(
        state,
        backend,
        peer,
        sequence,
        crate::nested::RECORD_MAJOR_OPCODE,
        yserver_protocol::x11::record::CREATE_CONTEXT,
        &create_record,
    );
    assert_no_error(&bytes, "RECORD CreateContext");
    rows.push((ResourceKind::RecordContext, record, CLIENT));

    rows
}

#[test]
fn owner_resolver_answers_every_kind() {
    // Mutation: drop the Colormap resolver arm; the server-owned default-colormap row must fail.
    // Mutation: skip the property hook; the recorded property assertion must fail.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, CLIENT.0);
    let mut backend = RecordingBackend::new();
    crate::core_loop::process_request::install_default_font(&mut state, &mut backend)
        .expect("install server default font");
    let mut rows = create_owner_rows(&mut state, &mut backend, &mut peer);
    let default_font = crate::resources::DEFAULT_FONT;
    state.randr.set_providers(vec![crate::randr::RandrProvider {
        provider_id: 4,
        name: "server-provider".to_owned(),
        capabilities: 0,
        is_gpu: false,
        crtcs: vec![2],
        outputs: vec![1],
        associations: Vec::new(),
    }]);
    rows.extend([
        (
            ResourceKind::Window,
            ROOT_WINDOW,
            crate::resources::SERVER_OWNER,
        ),
        (
            ResourceKind::Colormap,
            crate::resources::ROOT_COLORMAP,
            crate::resources::SERVER_OWNER,
        ),
        (
            ResourceKind::Font,
            default_font,
            crate::resources::SERVER_OWNER,
        ),
        (
            ResourceKind::SyncCounter,
            ResourceId(yserver_protocol::x11::sync::SERVERTIME_COUNTER),
            crate::resources::SERVER_OWNER,
        ),
        (
            ResourceKind::RandrOutput,
            ResourceId(1),
            crate::resources::SERVER_OWNER,
        ),
        (
            ResourceKind::RandrCrtc,
            ResourceId(2),
            crate::resources::SERVER_OWNER,
        ),
        (
            ResourceKind::RandrMode,
            ResourceId(3),
            crate::resources::SERVER_OWNER,
        ),
        (
            ResourceKind::RandrProvider,
            ResourceId(4),
            crate::resources::SERVER_OWNER,
        ),
    ]);

    let expected_kinds = [
        ResourceKind::Window,
        ResourceKind::Pixmap,
        ResourceKind::Gc,
        ResourceKind::Font,
        ResourceKind::Cursor,
        ResourceKind::Colormap,
        ResourceKind::Picture,
        ResourceKind::GlyphSet,
        ResourceKind::Region,
        ResourceKind::PointerBarrier,
        ResourceKind::SyncCounter,
        ResourceKind::SyncAlarm,
        ResourceKind::SyncFence,
        ResourceKind::Damage,
        ResourceKind::PresentEventSelection,
        ResourceKind::ShmSegment,
        ResourceKind::GlxContext,
        ResourceKind::GlxDrawable,
        ResourceKind::Dri3Syncobj,
        ResourceKind::RecordContext,
        ResourceKind::RandrOutput,
        ResourceKind::RandrCrtc,
        ResourceKind::RandrMode,
        ResourceKind::RandrProvider,
    ];
    let calls = Arc::new(Mutex::new(Vec::new()));
    state.install_access_policy(Arc::new(RecordingExtensionPolicy::new(
        Decision::Allow,
        Arc::clone(&calls),
    )));

    for kind in expected_kinds {
        assert!(
            rows.iter().any(|(row_kind, _, _)| *row_kind == kind),
            "missing live owner row for {kind:?}"
        );
    }
    for &(kind, id, expected) in &rows {
        assert_eq!(
            ask_owner(&state, id, kind, &calls),
            Some(expected),
            "owner for {kind:?} 0x{:x}",
            id.0
        );
    }
    for kind in expected_kinds {
        assert_eq!(
            ask_owner(&state, ResourceId(0x6fff_ffff), kind, &calls),
            None,
            "unused {kind:?}"
        );
    }

    let property = AtomId(69);
    let mode = AccessMode::GET_PROP | AccessMode::SET_PROP;
    assert_eq!(
        state.check_property(CLIENT, ROOT_WINDOW, property, mode, Some(REQUEST)),
        Decision::Allow
    );
    let calls = calls.lock().unwrap();
    let property_call = calls.last().expect("property hook call");
    assert_eq!(property_call.hook, ExtensionHook::Property);
    assert_eq!(property_call.property, Some((ROOT_WINDOW, property)));
    assert_eq!(property_call.mode, Some(mode));
    assert_eq!(property_call.request, Some(REQUEST));
}

#[test]
fn owner_resolver_keeps_retained_owner() {
    // Mutation: resolve owners only while their client is connected; retained resources still
    // belong to client 1.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, CLIENT.0);
    let pixmap = ResourceId(0x0051_0001);
    let colormap = ResourceId(0x0051_0002);

    dispatch_wire_request(&mut state, &mut peer, 1, 112, 1, &[]);
    let mut pixmap_body = Vec::new();
    pixmap_body.extend_from_slice(&pixmap.0.to_le_bytes());
    pixmap_body.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
    pixmap_body.extend_from_slice(&8u16.to_le_bytes());
    pixmap_body.extend_from_slice(&8u16.to_le_bytes());
    dispatch_wire_request(&mut state, &mut peer, 2, 53, 24, &pixmap_body);
    let mut colormap_body = Vec::new();
    colormap_body.extend_from_slice(&colormap.0.to_le_bytes());
    colormap_body.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
    colormap_body.extend_from_slice(&crate::resources::ROOT_VISUAL.0.to_le_bytes());
    dispatch_wire_request(&mut state, &mut peer, 3, 78, 0, &colormap_body);

    drop(peer);
    let mut backend = RecordingBackend::new();
    crate::core_loop::process_disconnect::process_disconnect(&mut state, &mut backend, CLIENT);
    assert!(state.zombie_clients.contains_key(&CLIENT.0));

    let calls = Arc::new(Mutex::new(Vec::new()));
    state.install_access_policy(Arc::new(RecordingExtensionPolicy::new(
        Decision::Allow,
        Arc::clone(&calls),
    )));
    assert_eq!(
        ask_owner(&state, pixmap, ResourceKind::Pixmap, &calls),
        Some(CLIENT)
    );
    assert_eq!(
        ask_owner(&state, colormap, ResourceKind::Colormap, &calls),
        Some(CLIENT)
    );
}

fn kill_client_colormap_result(with_policy: bool) -> (Vec<u8>, Vec<RecordedExtensionCheck>) {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, CLIENT.0);
    let calls = Arc::new(Mutex::new(Vec::new()));
    if with_policy {
        state.install_access_policy(Arc::new(RecordingExtensionPolicy::new(
            Decision::Allow,
            Arc::clone(&calls),
        )));
    }

    let colormap = ResourceId(0x0052_0001);
    let mut create_body = Vec::new();
    create_body.extend_from_slice(&colormap.0.to_le_bytes());
    create_body.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
    create_body.extend_from_slice(&crate::resources::ROOT_VISUAL.0.to_le_bytes());
    dispatch_wire_request(&mut state, &mut peer, 1, 78, 0, &create_body);
    assert!(state.resources.colormap(colormap).is_some());
    calls.lock().unwrap().clear();
    let mut backend = RecordingBackend::new();
    let _outcome = process_request(
        &mut state,
        &mut backend,
        CLIENT,
        SequenceNumber(2),
        RequestHeader {
            opcode: 113,
            data: 0,
            length_units: 2,
        },
        &colormap.0.to_le_bytes(),
        None,
    )
    .expect("KillClient");
    (read_all_available(&mut peer), calls.lock().unwrap().clone())
}

#[test]
fn kill_client_on_a_colormap_keeps_bad_value_with_a_policy() {
    // Mutation: resolve KillClient through the typed owner resolver; the colormap must stop
    // returning BadValue.
    let baseline = kill_client_colormap_result(false).0;
    let (with_policy, calls) = kill_client_colormap_result(true);
    assert_eq!(baseline[1], yserver_protocol::x11::error::BAD_VALUE);
    assert_eq!(with_policy, baseline);
    assert_eq!(with_policy[1], yserver_protocol::x11::error::BAD_VALUE);
    assert!(calls.is_empty());
}

#[test]
fn server_owner_is_never_checked() {
    // Mutation: remove either SERVER_OWNER fast path; its matching Allow assertion must fail.
    let mut state = ServerState::new();
    let calls = Arc::new(Mutex::new(Vec::new()));
    state.install_access_policy(Arc::new(RecordingExtensionPolicy::new(
        Decision::Deny(DenyError {
            code: yserver_protocol::x11::error::BAD_VALUE,
            value: 0x55,
        }),
        Arc::clone(&calls),
    )));
    let server = crate::resources::SERVER_OWNER;
    assert_eq!(
        state.check_resource(
            server,
            &ResourceTarget {
                owner: None,
                id: ROOT_WINDOW,
                kind: ResourceKind::Window,
                creation: None,
            },
            AccessMode::DESTROY,
            Some(REQUEST),
        ),
        Decision::Allow
    );
    assert_eq!(
        state.check_property(
            server,
            ROOT_WINDOW,
            AtomId(69),
            AccessMode::SET_PROP,
            Some(REQUEST),
        ),
        Decision::Allow
    );
    assert!(calls.lock().unwrap().is_empty());
}

#[test]
fn empty_policy_list_checks_allow() {
    // Mutations: deny the no-policy resource or property check; the matching Allow assertion must
    // fail.
    let state = ServerState::new();
    assert!(!state.policies_installed());
    assert_eq!(
        state.check_resource(
            CLIENT,
            &ResourceTarget {
                owner: None,
                id: ResourceId(0x0060_0001),
                kind: ResourceKind::Pixmap,
                creation: None,
            },
            AccessMode::READ,
            Some(REQUEST),
        ),
        Decision::Allow
    );
    assert_eq!(
        state.check_property(
            CLIENT,
            ROOT_WINDOW,
            AtomId(69),
            AccessMode::GET_PROP,
            Some(REQUEST),
        ),
        Decision::Allow
    );
}

pub(super) fn drawing_resource_policy(
    label: &'static str,
    calls: Arc<Mutex<Vec<RecordedExtensionCheck>>>,
    deny_id: Option<Arc<AtomicU32>>,
    deny_all: bool,
    decision: Decision,
) -> RecordingExtensionPolicy {
    RecordingExtensionPolicy::custom(calls, label, move |_, call| {
        let denied_id = deny_id.as_ref().is_some_and(|deny_id| {
            call.target
                .is_some_and(|target| deny_id.load(Ordering::SeqCst) == target.id.0)
        });
        if call.hook == ExtensionHook::Resource && (deny_all || denied_id) {
            decision
        } else {
            Decision::Allow
        }
    })
}

struct DrawingResources {
    window: ResourceId,
    src: ResourceId,
    dst: ResourceId,
    gc: ResourceId,
}

fn drawing_resources(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    peer: &mut UnixStream,
) -> DrawingResources {
    let resources = DrawingResources {
        window: ResourceId(0x0053_0001),
        src: ResourceId(0x0053_0002),
        dst: ResourceId(0x0053_0003),
        gc: ResourceId(0x0053_0004),
    };
    let create_window = words(&[
        resources.window.0,
        ROOT_WINDOW.0,
        0,
        16 | (16 << 16),
        0,
        crate::resources::ROOT_VISUAL.0,
        0,
    ]);
    assert_no_error(
        &resource_request(state, backend, peer, 1, 1, 24, &create_window),
        "drawing test window",
    );
    for (sequence, pixmap) in [(2, resources.src), (3, resources.dst)] {
        assert_no_error(
            &resource_request(
                state,
                backend,
                peer,
                sequence,
                53,
                24,
                &words(&[pixmap.0, ROOT_WINDOW.0, 16 | (16 << 16)]),
            ),
            "drawing test pixmap",
        );
    }
    assert_no_error(
        &resource_request(
            state,
            backend,
            peer,
            4,
            55,
            0,
            &words(&[resources.gc.0, resources.src.0, 0]),
        ),
        "drawing test GC",
    );
    resources
}

fn unbacked_drawing_resources(state: &mut ServerState) -> DrawingResources {
    let resources = DrawingResources {
        window: ResourceId(0x0054_0001),
        src: ResourceId(0x0054_0002),
        dst: ResourceId(0x0054_0003),
        gc: ResourceId(0x0054_0004),
    };
    state.resources.create_window(
        CLIENT,
        CreateWindowRequest {
            depth: 24,
            window: resources.window,
            parent: ROOT_WINDOW,
            width: 16,
            height: 16,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    for pixmap in [resources.src, resources.dst] {
        state.resources.create_pixmap(
            CLIENT,
            yserver_protocol::x11::CreatePixmapRequest {
                depth: 24,
                pixmap,
                drawable: ROOT_WINDOW,
                width: 16,
                height: 16,
            },
        );
    }
    state.resources.create_gc(
        CLIENT,
        x11::create_gc_request(&words(&[resources.gc.0, resources.src.0, 0]))
            .expect("unbacked test GC request"),
    );
    resources
}

#[derive(Clone)]
struct DrawingRequestCase {
    opcode: u8,
    data: u8,
    body: Vec<u8>,
    targets: Vec<(ResourceId, ResourceKind, AccessMode)>,
}

fn drawing_case(
    opcode: u8,
    data: u8,
    body: Vec<u8>,
    targets: Vec<(ResourceId, ResourceKind, AccessMode)>,
) -> DrawingRequestCase {
    DrawingRequestCase {
        opcode,
        data,
        body,
        targets,
    }
}

fn drawable_gc_case(
    resources: &DrawingResources,
    opcode: u8,
    data: u8,
    body: Vec<u8>,
) -> DrawingRequestCase {
    drawing_case(
        opcode,
        data,
        body,
        vec![
            (resources.src, ResourceKind::Pixmap, AccessMode::WRITE),
            (resources.gc, ResourceKind::Gc, AccessMode::USE),
        ],
    )
}

fn copy_drawable_case(
    resources: &DrawingResources,
    opcode: u8,
    body: Vec<u8>,
) -> DrawingRequestCase {
    drawing_case(
        opcode,
        0,
        body,
        vec![
            (resources.src, ResourceKind::Pixmap, AccessMode::READ),
            (resources.dst, ResourceKind::Pixmap, AccessMode::WRITE),
            (resources.gc, ResourceKind::Gc, AccessMode::USE),
        ],
    )
}

fn window_drawable_case(
    resources: &DrawingResources,
    opcode: u8,
    body: Vec<u8>,
) -> DrawingRequestCase {
    drawing_case(
        opcode,
        0,
        body,
        vec![(resources.window, ResourceKind::Window, AccessMode::WRITE)],
    )
}

fn read_pixmap_case(
    resources: &DrawingResources,
    opcode: u8,
    data: u8,
    body: Vec<u8>,
) -> DrawingRequestCase {
    drawing_case(
        opcode,
        data,
        body,
        vec![(resources.src, ResourceKind::Pixmap, AccessMode::READ)],
    )
}

fn drawing_gc_body(resources: &DrawingResources, operands: &[u32]) -> Vec<u8> {
    let mut body = vec![resources.src.0, resources.gc.0];
    body.extend_from_slice(operands);
    words(&body)
}

fn copy_drawing_body(resources: &DrawingResources, operands: &[u32]) -> Vec<u8> {
    let mut body = vec![resources.src.0, resources.dst.0, resources.gc.0];
    body.extend_from_slice(operands);
    words(&body)
}

fn clear_area_body(resources: &DrawingResources) -> Vec<u8> {
    words(&[resources.window.0, 0, 1 | (1 << 16)])
}

fn get_image_body(resources: &DrawingResources) -> Vec<u8> {
    words(&[resources.src.0, 0, 1 | (1 << 16), u32::MAX])
}

fn drawing_request_cases(resources: &DrawingResources) -> Vec<DrawingRequestCase> {
    let mut image = words(&[resources.src.0, resources.gc.0, 1 | (1 << 16), 0]);
    image.extend_from_slice(&[0, 24, 0, 0, 0, 0, 0, 0]);
    let copy_area = copy_drawing_body(resources, &[0, 0, 1 | (1 << 16)]);
    let copy_plane = copy_drawing_body(resources, &[0, 0, 1 | (1 << 16), 1]);
    let gc_rectangle = drawing_gc_body(resources, &[0, 1 | (1 << 16)]);
    let gc_arc = drawing_gc_body(resources, &[0, 1 | (1 << 16), 0]);
    vec![
        window_drawable_case(resources, 61, clear_area_body(resources)),
        copy_drawable_case(resources, 62, copy_area),
        copy_drawable_case(resources, 63, copy_plane),
        drawable_gc_case(resources, 64, 0, drawing_gc_body(resources, &[0])),
        drawable_gc_case(resources, 65, 0, drawing_gc_body(resources, &[0])),
        drawable_gc_case(resources, 66, 0, drawing_gc_body(resources, &[0, 0, 0, 0])),
        drawable_gc_case(resources, 67, 0, gc_rectangle.clone()),
        drawable_gc_case(resources, 68, 0, gc_arc.clone()),
        drawable_gc_case(resources, 69, 0, drawing_gc_body(resources, &[0])),
        drawable_gc_case(resources, 70, 0, gc_rectangle),
        drawable_gc_case(resources, 71, 0, gc_arc),
        drawable_gc_case(resources, 72, 2, image),
        read_pixmap_case(resources, 73, 2, get_image_body(resources)),
        drawable_gc_case(resources, 74, 0, drawing_gc_body(resources, &[0])),
        drawable_gc_case(resources, 75, 0, drawing_gc_body(resources, &[0])),
        drawable_gc_case(resources, 76, 0, drawing_gc_body(resources, &[0])),
        drawable_gc_case(resources, 77, 0, drawing_gc_body(resources, &[0])),
    ]
}

fn run_drawing_request(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    peer: &mut UnixStream,
    sequence: u16,
    request: &DrawingRequestCase,
) -> Vec<u8> {
    process_request(
        state,
        backend,
        CLIENT,
        SequenceNumber(sequence),
        RequestHeader {
            opcode: request.opcode,
            data: request.data,
            length_units: u32::try_from(1 + request.body.len().div_ceil(4)).unwrap(),
        },
        &request.body,
        None,
    )
    .expect("drawing request through process_request");
    read_all_available(peer)
}

pub(super) fn assert_x11_error(bytes: &[u8], code: u8, value: u32, opcode: u8, sequence: u16) {
    let mut expected = vec![0, code];
    expected.extend_from_slice(&sequence.to_le_bytes());
    expected.extend_from_slice(&value.to_le_bytes());
    expected.extend_from_slice(&0u16.to_le_bytes());
    expected.push(opcode);
    expected.extend_from_slice(&[0; 21]);
    assert_eq!(bytes, expected, "request {opcode} denial packet");
}

pub(super) fn assert_policy_error(bytes: &[u8], opcode: u8, sequence: u16) {
    assert_x11_error(bytes, x11::error::BAD_ACCESS, 0x55, opcode, sequence);
}

#[test]
fn get_image_checks_read_before_bounds_validation() {
    // Mutation: move GetImage's READ check after rectangle/viewability validation; the denial must
    // win over BadMatch.
    const DRAWABLE: ResourceId = ResourceId(0x0053_00f0);
    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    let mut peer = install_client(&mut state, CLIENT.0);
    state.resources.create_pixmap(
        CLIENT,
        x11::CreatePixmapRequest {
            depth: 24,
            pixmap: DRAWABLE,
            drawable: ROOT_WINDOW,
            width: 8,
            height: 8,
        },
    );
    let body = words(&[DRAWABLE.0, 8, 1 | (1 << 16), u32::MAX]);

    let baseline = resource_request(&mut state, &mut backend, &mut peer, 1, 73, 2, &body);
    assert_x11_error(&baseline, x11::error::BAD_MATCH, DRAWABLE.0, 73, 1);

    let calls = Arc::new(Mutex::new(Vec::new()));
    let drawable_before = format!("{:?}", state.resources.pixmap(DRAWABLE));
    state.install_access_policy(Arc::new(drawing_resource_policy(
        "GetImage bounds order",
        Arc::clone(&calls),
        Some(Arc::new(AtomicU32::new(DRAWABLE.0))),
        false,
        Decision::Deny(DenyError {
            code: x11::error::BAD_ACCESS,
            value: 0x55,
        }),
    )));
    let backend_before = backend.calls();
    let denied = resource_request(&mut state, &mut backend, &mut peer, 2, 73, 2, &body);

    assert_policy_error(&denied, 73, 2);
    assert_eq!(backend.calls(), backend_before);
    assert_eq!(
        format!("{:?}", state.resources.pixmap(DRAWABLE)),
        drawable_before
    );
    assert_eq!(
        expected_resource_checks(&calls.lock().unwrap()),
        [(
            CLIENT,
            DRAWABLE,
            ResourceKind::Pixmap,
            AccessMode::READ,
            Some(RequestOpcodes {
                major: 73,
                minor: 0,
            }),
        )]
    );
}

pub(super) fn expected_resource_checks(
    calls: &[RecordedExtensionCheck],
) -> Vec<RecordedResourceExpectation> {
    calls
        .iter()
        .map(|call| {
            let target = call.target.expect("resource target");
            let expected_target_owner = target.owner.or(call.owner);
            assert_eq!(
                call.target_owner, expected_target_owner,
                "recorded target owner for {:?}",
                call.request
            );
            (
                call.client,
                target.id,
                target.kind,
                call.mode.expect("resource mode"),
                call.request,
            )
        })
        .collect()
}

fn core_check(
    id: ResourceId,
    kind: ResourceKind,
    mode: AccessMode,
    major: u8,
) -> RecordedResourceExpectation {
    (
        CLIENT,
        id,
        kind,
        mode,
        Some(RequestOpcodes { major, minor: 0 }),
    )
}

#[test]
fn every_drawing_request_denial_reports_policy_error() {
    // Mutation: remove any drawing target check, including CopyArea/CopyPlane destination; that
    // target must stop returning the policy error.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, CLIENT.0);
    let mut backend = RecordingBackend::new();
    let resources = drawing_resources(&mut state, &mut backend, &mut peer);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let denied_id = Arc::new(AtomicU32::new(0));
    state.install_access_policy(Arc::new(drawing_resource_policy(
        "drawing deny",
        Arc::clone(&calls),
        Some(Arc::clone(&denied_id)),
        false,
        Decision::Deny(DenyError {
            code: x11::error::BAD_ACCESS,
            value: 0x55,
        }),
    )));

    let cases = drawing_request_cases(&resources);
    let mut sequence = 10;
    for request in &cases {
        for (target, _, _) in &request.targets {
            denied_id.store(target.0, Ordering::SeqCst);
            let before = backend.calls();
            let window_before = format!("{:?}", state.resources.window(resources.window).unwrap());
            let gc_before = format!("{:?}", state.resources.gc(resources.gc).unwrap());
            let damage_before = state.damage_objects.clone();
            let bytes = run_drawing_request(&mut state, &mut backend, &mut peer, sequence, request);
            assert_policy_error(&bytes, request.opcode, sequence);
            assert_eq!(
                backend.calls(),
                before,
                "request {} drew after target denial",
                request.opcode
            );
            assert_eq!(
                format!("{:?}", state.resources.window(resources.window).unwrap()),
                window_before,
                "request {} changed window state after denial",
                request.opcode
            );
            assert_eq!(
                format!("{:?}", state.resources.gc(resources.gc).unwrap()),
                gc_before,
                "request {} changed GC state after denial",
                request.opcode
            );
            assert_eq!(
                state.damage_objects, damage_before,
                "request {} changed damage state",
                request.opcode
            );
            sequence += 1;
        }
    }
}

#[test]
fn denied_drawable_precedes_missing_gc() {
    // Mutation: move drawable authorization after GC lookup; the policy denial must still win over
    // BadGC.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, CLIENT.0);
    let mut backend = RecordingBackend::new();
    let resources = drawing_resources(&mut state, &mut backend, &mut peer);
    let calls = Arc::new(Mutex::new(Vec::new()));
    state.install_access_policy(Arc::new(drawing_resource_policy(
        "drawable precedence",
        Arc::clone(&calls),
        Some(Arc::new(AtomicU32::new(resources.src.0))),
        false,
        Decision::Deny(DenyError {
            code: x11::error::BAD_ACCESS,
            value: 0x55,
        }),
    )));
    let missing_gc = ResourceId(0x0053_00ff);
    let request = drawing_case(
        64,
        0,
        words(&[resources.src.0, missing_gc.0, 0]),
        Vec::new(),
    );
    let before = backend.calls();
    let bytes = run_drawing_request(&mut state, &mut backend, &mut peer, 50, &request);
    assert_policy_error(&bytes, 64, 50);
    assert_eq!(backend.calls(), before);
    assert_eq!(
        expected_resource_checks(&calls.lock().unwrap()),
        [(
            CLIENT,
            resources.src,
            ResourceKind::Pixmap,
            AccessMode::WRITE,
            Some(RequestOpcodes {
                major: 64,
                minor: 0,
            }),
        )]
    );
}

#[test]
fn allowed_drawable_keeps_missing_gc_error() {
    // Mutation: skip missing-GC validation after an allowed drawable; BadGC with the GC id must
    // remain observable.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, CLIENT.0);
    let mut backend = RecordingBackend::new();
    let resources = unbacked_drawing_resources(&mut state);
    let calls = Arc::new(Mutex::new(Vec::new()));
    state.install_access_policy(Arc::new(drawing_resource_policy(
        "drawable allow",
        Arc::clone(&calls),
        Some(Arc::new(AtomicU32::new(0))),
        false,
        Decision::Deny(DenyError {
            code: x11::error::BAD_ACCESS,
            value: 0x55,
        }),
    )));
    let missing_gc = ResourceId(0x0053_00ff);
    let request = drawing_case(
        64,
        0,
        words(&[resources.src.0, missing_gc.0, 0]),
        Vec::new(),
    );
    let before = backend.calls();
    let bytes = run_drawing_request(&mut state, &mut backend, &mut peer, 51, &request);
    assert_x11_error(&bytes, x11::error::BAD_GC, missing_gc.0, 64, 51);
    assert_eq!(backend.calls(), before);
    assert_eq!(
        expected_resource_checks(&calls.lock().unwrap()),
        [(
            CLIENT,
            resources.src,
            ResourceKind::Pixmap,
            AccessMode::WRITE,
            Some(RequestOpcodes {
                major: 64,
                minor: 0,
            }),
        )]
    );
}

#[test]
fn denied_gc_precedes_depth_mismatch() {
    // Mutation: move GC authorization after the depth check; the policy denial must beat BadMatch.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, CLIENT.0);
    let mut backend = RecordingBackend::new();
    let resources = drawing_resources(&mut state, &mut backend, &mut peer);
    let narrow_pixmap = ResourceId(0x0053_0010);
    assert_no_error(
        &resource_request(
            &mut state,
            &mut backend,
            &mut peer,
            5,
            53,
            1,
            &words(&[narrow_pixmap.0, ROOT_WINDOW.0, 8 | (8 << 16)]),
        ),
        "depth-mismatch pixmap",
    );
    let calls = Arc::new(Mutex::new(Vec::new()));
    state.install_access_policy(Arc::new(drawing_resource_policy(
        "GC precedence",
        Arc::clone(&calls),
        Some(Arc::new(AtomicU32::new(resources.gc.0))),
        false,
        Decision::Deny(DenyError {
            code: x11::error::BAD_ACCESS,
            value: 0x55,
        }),
    )));
    let request = drawing_case(
        64,
        0,
        words(&[narrow_pixmap.0, resources.gc.0, 0]),
        Vec::new(),
    );
    let before = backend.calls();
    let bytes = run_drawing_request(&mut state, &mut backend, &mut peer, 52, &request);
    assert_policy_error(&bytes, 64, 52);
    assert_eq!(backend.calls(), before);
    assert_eq!(
        expected_resource_checks(&calls.lock().unwrap()),
        [
            (
                CLIENT,
                narrow_pixmap,
                ResourceKind::Pixmap,
                AccessMode::WRITE,
                Some(RequestOpcodes {
                    major: 64,
                    minor: 0,
                }),
            ),
            (
                CLIENT,
                resources.gc,
                ResourceKind::Gc,
                AccessMode::USE,
                Some(RequestOpcodes {
                    major: 64,
                    minor: 0,
                }),
            ),
        ]
    );
}

#[test]
fn every_drawing_request_asks_with_expected_target() {
    // Mutation: pass the wrong drawable, GC, kind, mode, or request context; the literal target
    // list must fail.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, CLIENT.0);
    let mut backend = RecordingBackend::new();
    let resources = unbacked_drawing_resources(&mut state);
    let calls = Arc::new(Mutex::new(Vec::new()));
    state.install_access_policy(Arc::new(drawing_resource_policy(
        "drawing recorder",
        Arc::clone(&calls),
        None,
        false,
        Decision::Allow,
    )));

    for (index, request) in drawing_request_cases(&resources).iter().enumerate() {
        calls.lock().unwrap().clear();
        let sequence = u16::try_from(100 + index).unwrap();
        let bytes = run_drawing_request(&mut state, &mut backend, &mut peer, sequence, request);
        assert!(
            bytes.is_empty() || bytes[0] != 0,
            "request {} unexpectedly errored",
            request.opcode
        );
        let expected: Vec<_> = request
            .targets
            .iter()
            .map(|(id, kind, mode)| {
                (
                    CLIENT,
                    *id,
                    *kind,
                    *mode,
                    Some(RequestOpcodes {
                        major: request.opcode,
                        minor: 0,
                    }),
                )
            })
            .collect();
        assert_eq!(
            expected_resource_checks(&calls.lock().unwrap()),
            expected,
            "request {}",
            request.opcode
        );
    }
}

#[test]
fn gc_mutators_and_query_best_size_check_their_targets() {
    // Mutation: move a check after a GC mutation or QueryBestSize reply; the exact denial and
    // unchanged GC state must fail.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, CLIENT.0);
    let mut backend = RecordingBackend::new();
    let resources = drawing_resources(&mut state, &mut backend, &mut peer);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let denied_id = Arc::new(AtomicU32::new(0));
    state.install_access_policy(Arc::new(drawing_resource_policy(
        "GC and size denial",
        Arc::clone(&calls),
        Some(Arc::clone(&denied_id)),
        false,
        Decision::Deny(DenyError {
            code: x11::error::BAD_ACCESS,
            value: 0x55,
        }),
    )));
    let mut dashes = words(&[resources.gc.0, 2 << 16]);
    dashes.extend_from_slice(&[3, 5, 0, 0]);
    let clip_rectangles = words(&[resources.gc.0, 0, 1 | (1 << 16), 0]);
    let cases = [
        (58, resources.gc, dashes),
        (59, resources.gc, clip_rectangles),
        (97, resources.src, words(&[resources.src.0, 8 | (8 << 16)])),
    ];
    for (index, (opcode, target, body)) in cases.into_iter().enumerate() {
        let sequence = u16::try_from(60 + index).unwrap();
        denied_id.store(target.0, Ordering::SeqCst);
        calls.lock().unwrap().clear();
        let before = backend.calls();
        let request = drawing_case(opcode, 0, body, Vec::new());
        let bytes = run_drawing_request(&mut state, &mut backend, &mut peer, sequence, &request);
        assert_policy_error(&bytes, opcode, sequence);
        assert_eq!(
            backend.calls(),
            before,
            "request {opcode} has no backend effect"
        );
        assert_eq!(
            expected_resource_checks(&calls.lock().unwrap()),
            [(
                CLIENT,
                target,
                if opcode == 97 {
                    ResourceKind::Pixmap
                } else {
                    ResourceKind::Gc
                },
                if opcode == 97 {
                    AccessMode::GET_ATTR
                } else {
                    AccessMode::SET_ATTR
                },
                Some(RequestOpcodes {
                    major: opcode,
                    minor: 0
                }),
            )],
            "request {opcode} access target"
        );
        if opcode == 58 || opcode == 59 {
            let gc = state.resources.gc(resources.gc).unwrap();
            assert_eq!(gc.dashes, vec![4, 4]);
            assert_eq!(gc.dash_offset, 0);
            assert_eq!(gc.clip_rectangles, None);
        }
    }
}

#[test]
fn poly_text_denied_inline_font_draws_nothing() {
    // Mutation: check inline fonts while drawing; a denied second font must prevent the draw and GC
    // font update.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, CLIENT.0);
    let mut backend = RecordingBackend::new();
    let resources = drawing_resources(&mut state, &mut backend, &mut peer);
    let font_ids = [ResourceId(0x0053_0005), ResourceId(0x0053_0006)];
    for (index, font) in font_ids.iter().enumerate() {
        let mut body = font.0.to_le_bytes().to_vec();
        body.extend_from_slice(&5u16.to_le_bytes());
        body.extend_from_slice(&[0, 0]);
        body.extend_from_slice(b"fixed");
        body.resize(body.len().div_ceil(4) * 4, 0);
        assert_no_error(
            &resource_request(
                &mut state,
                &mut backend,
                &mut peer,
                u16::try_from(5 + index).unwrap(),
                45,
                0,
                &body,
            ),
            "inline font",
        );
    }
    let original_gc_font = state.resources.gc(resources.gc).unwrap().font;
    let calls = Arc::new(Mutex::new(Vec::new()));
    state.install_access_policy(Arc::new(drawing_resource_policy(
        "second inline font deny",
        calls,
        Some(Arc::new(AtomicU32::new(font_ids[1].0))),
        false,
        Decision::Deny(DenyError {
            code: x11::error::BAD_ACCESS,
            value: 0x55,
        }),
    )));
    let mut body = words(&[resources.src.0, resources.gc.0, 0]);
    body.push(255);
    body.extend_from_slice(&font_ids[0].0.to_be_bytes());
    body.extend_from_slice(&[3, 0, b'o', b'n', b'e']);
    body.push(255);
    body.extend_from_slice(&font_ids[1].0.to_be_bytes());
    body.extend_from_slice(&[3, 0, b't', b'w', b'o']);
    body.resize(body.len().div_ceil(4) * 4, 0);
    let before = backend.calls();
    let request = drawing_case(74, 0, body, Vec::new());
    let bytes = run_drawing_request(&mut state, &mut backend, &mut peer, 20, &request);
    assert_policy_error(&bytes, 74, 20);
    assert_eq!(
        backend.calls(),
        before,
        "denied inline font reached drawing backend"
    );
    assert_eq!(
        state.resources.gc(resources.gc).unwrap().font,
        original_gc_font
    );
}

#[test]
fn several_policies_resource_order() {
    // Mutation: continue asking policies after a denial or return a later policy's error;
    // first-denier order must hold.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, CLIENT.0);
    let mut backend = RecordingBackend::new();
    let resources = unbacked_drawing_resources(&mut state);
    let request = drawing_case(
        64,
        0,
        words(&[resources.src.0, resources.gc.0, 0]),
        Vec::new(),
    );

    let calls = Arc::new(Mutex::new(Vec::new()));
    state.install_access_policy(Arc::new(drawing_resource_policy(
        "allow before deny",
        Arc::clone(&calls),
        None,
        false,
        Decision::Allow,
    )));
    state.install_access_policy(Arc::new(drawing_resource_policy(
        "first denier",
        Arc::clone(&calls),
        None,
        true,
        Decision::Deny(DenyError {
            code: x11::error::BAD_ACCESS,
            value: 0x55,
        }),
    )));
    state.install_access_policy(Arc::new(drawing_resource_policy(
        "must not run after denial",
        Arc::clone(&calls),
        None,
        true,
        Decision::Deny(DenyError {
            code: x11::error::BAD_VALUE,
            value: 0x66,
        }),
    )));
    let before = backend.calls();
    let bytes = run_drawing_request(&mut state, &mut backend, &mut peer, 30, &request);
    assert_policy_error(&bytes, 64, 30);
    assert_eq!(backend.calls(), before);
    assert_eq!(
        calls
            .lock()
            .unwrap()
            .iter()
            .map(|call| call.extension.as_str())
            .collect::<Vec<_>>(),
        ["allow before deny", "first denier"]
    );

    let mut second_state = ServerState::new();
    let mut second_peer = install_client(&mut second_state, CLIENT.0);
    let mut second_backend = RecordingBackend::new();
    let second_resources = unbacked_drawing_resources(&mut second_state);
    let second_request = drawing_case(
        64,
        0,
        words(&[second_resources.src.0, second_resources.gc.0, 0]),
        Vec::new(),
    );
    let second_calls = Arc::new(Mutex::new(Vec::new()));
    second_state.install_access_policy(Arc::new(drawing_resource_policy(
        "denier first",
        Arc::clone(&second_calls),
        None,
        true,
        Decision::Deny(DenyError {
            code: x11::error::BAD_VALUE,
            value: 0x66,
        }),
    )));
    second_state.install_access_policy(Arc::new(drawing_resource_policy(
        "allow after denial",
        Arc::clone(&second_calls),
        None,
        false,
        Decision::Allow,
    )));
    let second_before = second_backend.calls();
    let bytes = run_drawing_request(
        &mut second_state,
        &mut second_backend,
        &mut second_peer,
        31,
        &second_request,
    );
    let mut expected = vec![0, x11::error::BAD_VALUE];
    expected.extend_from_slice(&31u16.to_le_bytes());
    expected.extend_from_slice(&0x66u32.to_le_bytes());
    expected.extend_from_slice(&0u16.to_le_bytes());
    expected.extend_from_slice(&[64, 0]);
    expected.extend_from_slice(&[0; 20]);
    assert_eq!(bytes, expected);
    assert_eq!(second_backend.calls(), second_before);
    assert_eq!(
        second_calls
            .lock()
            .unwrap()
            .iter()
            .map(|call| call.extension.as_str())
            .collect::<Vec<_>>(),
        ["denier first"]
    );
}

fn revocable_drawing_policy(
    calls: Arc<Mutex<Vec<RecordedExtensionCheck>>>,
    revoked: Arc<AtomicBool>,
) -> RecordingExtensionPolicy {
    RecordingExtensionPolicy::custom(calls, "revocable drawing", move |_, call| {
        if call.hook == ExtensionHook::Resource && revoked.load(Ordering::SeqCst) {
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
fn revoked_drawable_denies_the_next_request() {
    // Mutation: cache an earlier Allow; revoking the same drawable must affect its next PolyPoint.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, CLIENT.0);
    let mut backend = RecordingBackend::new();
    let resources = unbacked_drawing_resources(&mut state);
    let request = drawing_case(
        64,
        0,
        words(&[resources.src.0, resources.gc.0, 0]),
        Vec::new(),
    );
    let revoked = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(Mutex::new(Vec::new()));
    state.install_access_policy(Arc::new(revocable_drawing_policy(
        Arc::clone(&calls),
        Arc::clone(&revoked),
    )));
    assert!(run_drawing_request(&mut state, &mut backend, &mut peer, 40, &request).is_empty());
    let before = backend.calls();
    revoked.store(true, Ordering::SeqCst);
    let bytes = run_drawing_request(&mut state, &mut backend, &mut peer, 41, &request);
    assert_policy_error(&bytes, 64, 41);
    assert_eq!(backend.calls(), before);
    assert_eq!(
        calls.lock().unwrap().len(),
        3,
        "first check visits drawable and GC, revoked request stops at drawable"
    );
}

#[test]
fn server_owner_drawing_stays_unchecked() {
    // Mutation: remove the SERVER_OWNER fast path; server work through PolyPoint must remain
    // unchecked.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, crate::resources::SERVER_OWNER.0);
    let resources = unbacked_drawing_resources(&mut state);
    let calls = Arc::new(Mutex::new(Vec::new()));
    state.install_access_policy(Arc::new(drawing_resource_policy(
        "deny server work",
        Arc::clone(&calls),
        None,
        true,
        Decision::Deny(DenyError {
            code: x11::error::BAD_ACCESS,
            value: 0x55,
        }),
    )));
    let body = words(&[resources.src.0, resources.gc.0, 0]);
    process_request(
        &mut state,
        &mut RecordingBackend::new(),
        crate::resources::SERVER_OWNER,
        SequenceNumber(70),
        RequestHeader {
            opcode: 64,
            data: 0,
            length_units: 4,
        },
        &body,
        None,
    )
    .expect("server-owned PolyPoint");
    assert!(read_all_available(&mut peer).is_empty());
    assert!(calls.lock().unwrap().is_empty());
}

pub(super) fn window_resource_policy(
    calls: Arc<Mutex<Vec<RecordedExtensionCheck>>>,
    deny_id: Option<Arc<AtomicU32>>,
    deny_mode: Option<AccessMode>,
    deny_hides: bool,
) -> RecordingExtensionPolicy {
    RecordingExtensionPolicy::custom(calls, "window resources", move |_, call| {
        let mode = call.mode;
        let denied_id = deny_id.as_ref().is_some_and(|deny_id| {
            call.target
                .is_some_and(|target| deny_id.load(Ordering::SeqCst) == target.id.0)
        });
        if call.hook == ExtensionHook::Resource
            && (deny_hides && mode == Some(AccessMode::HIDE)
                || denied_id && deny_mode.is_none_or(|deny_mode| Some(deny_mode) == mode))
        {
            Decision::Deny(DenyError {
                code: x11::error::BAD_VALUE,
                value: 0x55,
            })
        } else {
            Decision::Allow
        }
    })
}

struct WindowAccessFixture {
    state: ServerState,
    backend: RecordingBackend,
    peer: UnixStream,
    parent: ResourceId,
    window: ResourceId,
    destroy_parent: ResourceId,
    destroy_window: ResourceId,
    subwindows_parent: ResourceId,
    subwindow_children: [ResourceId; 2],
    background: ResourceId,
    border: ResourceId,
    colormap: ResourceId,
    cursor: ResourceId,
}

fn simple_window_body(window: ResourceId, parent: ResourceId) -> Vec<u8> {
    words(&[
        window.0,
        parent.0,
        0,
        32 | (32 << 16),
        0,
        crate::resources::ROOT_VISUAL.0,
        0,
    ])
}

fn create_test_window(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    peer: &mut UnixStream,
    sequence: &mut u16,
    window: ResourceId,
    parent: ResourceId,
) {
    let bytes = resource_request(
        state,
        backend,
        peer,
        *sequence,
        1,
        24,
        &simple_window_body(window, parent),
    );
    assert_no_error(&bytes, "window access fixture CreateWindow");
    *sequence += 1;
}

fn create_window_with_access_attributes(
    window: ResourceId,
    parent: ResourceId,
    background: ResourceId,
    border: ResourceId,
    colormap: ResourceId,
    cursor: ResourceId,
) -> Vec<u8> {
    let mask = 0x0001 | 0x0004 | 0x2000 | 0x4000;
    words(&[
        window.0,
        parent.0,
        0,
        32 | (32 << 16),
        0,
        crate::resources::ROOT_VISUAL.0,
        mask,
        background.0,
        border.0,
        colormap.0,
        cursor.0,
    ])
}

fn change_window_attributes_with_access_operands(
    window: ResourceId,
    background: ResourceId,
    border: ResourceId,
    colormap: ResourceId,
    cursor: ResourceId,
) -> Vec<u8> {
    let mask = 0x0001 | 0x0004 | 0x0200 | 0x0800 | 0x2000 | 0x4000;
    words(&[
        window.0,
        mask,
        background.0,
        border.0,
        0,
        1,
        colormap.0,
        cursor.0,
    ])
}

fn window_access_fixture() -> WindowAccessFixture {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, CLIENT.0);
    let mut backend = RecordingBackend::new();
    let parent = ResourceId(0x0054_0001);
    let window = ResourceId(0x0054_0002);
    let destroy_parent = ResourceId(0x0054_0003);
    let destroy_window = ResourceId(0x0054_0004);
    let subwindows_parent = ResourceId(0x0054_0005);
    let subwindow_children = [ResourceId(0x0054_0006), ResourceId(0x0054_0007)];
    let background = ResourceId(0x0054_0008);
    let border = ResourceId(0x0054_0009);
    let colormap = ResourceId(0x0054_000a);
    let cursor = ResourceId(0x0054_000b);
    let mut sequence = 1;

    create_test_window(
        &mut state,
        &mut backend,
        &mut peer,
        &mut sequence,
        parent,
        ROOT_WINDOW,
    );
    create_test_window(
        &mut state,
        &mut backend,
        &mut peer,
        &mut sequence,
        window,
        parent,
    );
    create_test_window(
        &mut state,
        &mut backend,
        &mut peer,
        &mut sequence,
        destroy_parent,
        ROOT_WINDOW,
    );
    create_test_window(
        &mut state,
        &mut backend,
        &mut peer,
        &mut sequence,
        destroy_window,
        destroy_parent,
    );
    create_test_window(
        &mut state,
        &mut backend,
        &mut peer,
        &mut sequence,
        subwindows_parent,
        ROOT_WINDOW,
    );
    for child in subwindow_children {
        create_test_window(
            &mut state,
            &mut backend,
            &mut peer,
            &mut sequence,
            child,
            subwindows_parent,
        );
    }
    for pixmap in [background, border] {
        let bytes = resource_request(
            &mut state,
            &mut backend,
            &mut peer,
            sequence,
            53,
            24,
            &words(&[pixmap.0, ROOT_WINDOW.0, 16 | (16 << 16)]),
        );
        assert_no_error(&bytes, "window access fixture CreatePixmap");
        sequence += 1;
    }
    let bytes = resource_request(
        &mut state,
        &mut backend,
        &mut peer,
        sequence,
        78,
        0,
        &words(&[colormap.0, ROOT_WINDOW.0, crate::resources::ROOT_VISUAL.0]),
    );
    assert_no_error(&bytes, "window access fixture CreateColormap");
    sequence += 1;
    let mut cursor_body = words(&[cursor.0, background.0, 0]);
    cursor_body.extend_from_slice(&[0; 16]);
    let bytes = resource_request(
        &mut state,
        &mut backend,
        &mut peer,
        sequence,
        93,
        0,
        &cursor_body,
    );
    assert_no_error(&bytes, "window access fixture CreateCursor");

    WindowAccessFixture {
        state,
        backend,
        peer,
        parent,
        window,
        destroy_parent,
        destroy_window,
        subwindows_parent,
        subwindow_children,
        background,
        border,
        colormap,
        cursor,
    }
}

struct WindowDenyCase {
    label: &'static str,
    opcode: u8,
    data: u8,
    body: Vec<u8>,
    target: ResourceTarget,
    mode: AccessMode,
}

fn existing(id: ResourceId, kind: ResourceKind, mode: AccessMode) -> (ResourceTarget, AccessMode) {
    (
        ResourceTarget {
            owner: None,
            id,
            kind,
            creation: None,
        },
        mode,
    )
}

fn create_window(
    id: ResourceId,
    mode: AccessMode,
    parent: ResourceId,
) -> (ResourceTarget, AccessMode) {
    (
        ResourceTarget {
            owner: None,
            id,
            kind: ResourceKind::Window,
            creation: Some(Creation {
                parent: Some(parent),
                owner: CLIENT,
            }),
        },
        mode,
    )
}

fn wr(
    request: (&'static str, u8, u8),
    body: Vec<u8>,
    expected: (ResourceTarget, AccessMode),
) -> WindowDenyCase {
    WindowDenyCase {
        label: request.0,
        opcode: request.1,
        data: request.2,
        body,
        target: expected.0,
        mode: expected.1,
    }
}

#[test]
fn window_resource_request_denials_match_modes_and_stop_before_effects() {
    // Mutation: remove a listed check or use the wrong CWA mode; its row's error/mode assertion
    // must fail.
    // Mutation: authorize an unresolved ChangeSaveSet window; it must keep its current no-check
    // success behavior.
    // Mutation: apply the CWA event-mask selection before checking CW operands; the event-mask
    // snapshot must fail.
    let mut fixture = window_access_fixture();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let denied_id = Arc::new(AtomicU32::new(0));
    fixture
        .state
        .install_access_policy(Arc::new(window_resource_policy(
            Arc::clone(&calls),
            Some(Arc::clone(&denied_id)),
            None,
            false,
        )));
    let created = ResourceId(0x0054_000c);
    let create = create_window_with_access_attributes(
        created,
        ROOT_WINDOW,
        fixture.background,
        fixture.border,
        fixture.colormap,
        fixture.cursor,
    );
    let attrs = change_window_attributes_with_access_operands(
        fixture.window,
        fixture.background,
        fixture.border,
        fixture.colormap,
        fixture.cursor,
    );
    use AccessMode as M;
    use ResourceKind as K;
    const CREATE_PARENT: (&str, u8, u8) = ("CreateWindow parent ADD", 1, 24);
    const CREATE_BACKGROUND: (&str, u8, u8) = ("CreateWindow background READ", 1, 24);
    const CREATE_COLORMAP: (&str, u8, u8) = ("CreateWindow colormap USE", 1, 24);
    const CREATE_CURSOR: (&str, u8, u8) = ("CreateWindow cursor USE", 1, 24);
    const CREATE_BORDER: (&str, u8, u8) = ("CreateWindow border pixmap READ", 1, 24);
    const CREATE_TARGET: (&str, u8, u8) = ("CreateWindow creation", 1, 24);
    const CWA_WINDOW: (&str, u8, u8) = ("ChangeWindowAttributes window", 2, 0);
    const CWA_EVENT_MASK: (&str, u8, u8) = ("ChangeWindowAttributes event mask only", 2, 0);
    const CWA_OTHER_ATTR: (&str, u8, u8) =
        ("ChangeWindowAttributes non-event attribute only", 2, 0);
    const CWA_BACKGROUND: (&str, u8, u8) = ("ChangeWindowAttributes background READ", 2, 0);
    const CWA_COLORMAP: (&str, u8, u8) = ("ChangeWindowAttributes colormap USE", 2, 0);
    const CWA_CURSOR: (&str, u8, u8) = ("ChangeWindowAttributes cursor USE", 2, 0);
    const CWA_BORDER: (&str, u8, u8) = ("ChangeWindowAttributes border pixmap READ", 2, 0);
    const GET_WINDOW_ATTRS: (&str, u8, u8) = ("GetWindowAttributes", 3, 0);
    const DESTROY_TARGET: (&str, u8, u8) = ("DestroyWindow target", 4, 0);
    const DESTROY_PARENT: (&str, u8, u8) = ("DestroyWindow parent", 4, 0);
    const DESTROY_SUBWINDOWS: (&str, u8, u8) = ("DestroySubwindows parent", 5, 0);
    const CHANGE_SAVE_SET: (&str, u8, u8) = ("ChangeSaveSet", 6, 0);
    let event_mask = words(&[fixture.window.0, 0x0800, 1]);
    let other_attr = words(&[fixture.window.0, 0x0200, 1]);
    let get_attrs = words(&[fixture.window.0]);
    let destroy = words(&[fixture.destroy_window.0]);
    let subwins = words(&[fixture.parent.0]);
    let save_set = words(&[fixture.window.0]);
    let root_add = existing(ROOT_WINDOW, K::Window, M::ADD);
    let bg_read = existing(fixture.background, K::Pixmap, M::READ);
    let cmap_use = existing(fixture.colormap, K::Colormap, M::USE);
    let cursor_use = existing(fixture.cursor, K::Cursor, M::USE);
    let border_read = existing(fixture.border, K::Pixmap, M::READ);
    let new_window = create_window(created, M::CREATE | M::SET_ATTR, ROOT_WINDOW);
    let win_rx_set = existing(fixture.window, K::Window, M::RECEIVE | M::SET_ATTR);
    let win_rx = existing(fixture.window, K::Window, M::RECEIVE);
    let win_set = existing(fixture.window, K::Window, M::SET_ATTR);
    let win_get = existing(fixture.window, K::Window, M::GET_ATTR);
    let destroy_target = existing(fixture.destroy_window, K::Window, M::DESTROY);
    let destroy_parent = existing(fixture.destroy_parent, K::Window, M::REMOVE);
    let parent_rm = existing(fixture.parent, K::Window, M::REMOVE);
    let win_manage = existing(fixture.window, K::Window, M::MANAGE);
    let cases = vec![
        wr(CREATE_PARENT, create.clone(), root_add),
        wr(CREATE_BACKGROUND, create.clone(), bg_read),
        wr(CREATE_COLORMAP, create.clone(), cmap_use),
        wr(CREATE_CURSOR, create.clone(), cursor_use),
        wr(CREATE_BORDER, create.clone(), border_read),
        wr(CREATE_TARGET, create, new_window),
        wr(CWA_WINDOW, attrs.clone(), win_rx_set),
        wr(CWA_EVENT_MASK, event_mask, win_rx),
        wr(CWA_OTHER_ATTR, other_attr, win_set),
        wr(CWA_BACKGROUND, attrs.clone(), bg_read),
        wr(CWA_COLORMAP, attrs.clone(), cmap_use),
        wr(CWA_CURSOR, attrs.clone(), cursor_use),
        wr(CWA_BORDER, attrs, border_read),
        wr(GET_WINDOW_ATTRS, get_attrs, win_get),
        wr(DESTROY_TARGET, destroy.clone(), destroy_target),
        wr(DESTROY_PARENT, destroy, destroy_parent),
        wr(DESTROY_SUBWINDOWS, subwins, parent_rm),
        wr(CHANGE_SAVE_SET, save_set, win_manage),
    ];

    let mut sequence = 50;
    for case in cases {
        denied_id.store(case.target.id.0, Ordering::SeqCst);
        let backend_before = fixture.backend.calls();
        let mut windows_before = fixture
            .state
            .resources
            .windows_iter()
            .map(|window| format!("{window:?}"))
            .collect::<Vec<_>>();
        windows_before.sort();
        let save_set_before = fixture.state.clients[&CLIENT.0].save_set.clone();
        let event_masks_before = fixture.state.clients[&CLIENT.0].event_masks.clone();
        let calls_before = calls.lock().unwrap().len();
        let bytes = resource_request(
            &mut fixture.state,
            &mut fixture.backend,
            &mut fixture.peer,
            sequence,
            case.opcode,
            case.data,
            &case.body,
        );
        assert_x11_error(&bytes, x11::error::BAD_VALUE, 0x55, case.opcode, sequence);
        assert_eq!(
            fixture.backend.calls(),
            backend_before,
            "{} backend",
            case.label
        );
        let mut windows_after = fixture
            .state
            .resources
            .windows_iter()
            .map(|window| format!("{window:?}"))
            .collect::<Vec<_>>();
        windows_after.sort();
        assert_eq!(windows_after, windows_before, "{} window state", case.label);
        assert_eq!(
            fixture.state.clients[&CLIENT.0].save_set, save_set_before,
            "{} save set",
            case.label
        );
        assert_eq!(
            fixture.state.clients[&CLIENT.0].event_masks, event_masks_before,
            "{} event selections",
            case.label
        );
        if case.opcode == 1 {
            assert!(
                fixture.state.resources.window(created).is_none(),
                "{} created a window",
                case.label
            );
        }
        let calls = calls.lock().unwrap();
        assert!(calls.len() > calls_before, "{} check count", case.label);
        let call = calls.last().expect("denial was recorded");
        assert_eq!(call.client, CLIENT, "{} client", case.label);
        assert_eq!(
            call.request,
            Some(RequestOpcodes {
                major: case.opcode,
                minor: 0
            }),
            "{} request",
            case.label
        );
        assert_eq!(call.mode, Some(case.mode), "{} mode", case.label);
        assert_eq!(call.target, Some(case.target), "{} target", case.label);
        drop(calls);
        sequence += 1;
    }
    let unresolved_window = ResourceId(0x0054_00ff);
    denied_id.store(unresolved_window.0, Ordering::SeqCst);
    let calls_before = calls.lock().unwrap().len();
    let bytes = resource_request(
        &mut fixture.state,
        &mut fixture.backend,
        &mut fixture.peer,
        sequence,
        6,
        0,
        &words(&[unresolved_window.0]),
    );
    assert_no_error(
        &bytes,
        "unresolved ChangeSaveSet keeps its no-error behavior",
    );
    assert_eq!(
        calls.lock().unwrap().len(),
        calls_before,
        "unresolved window is not checked"
    );
    assert!(
        fixture.state.clients[&CLIENT.0]
            .save_set
            .contains(&unresolved_window)
    );
}

#[test]
fn window_resource_requests_present_expected_checks() {
    // Mutation: remove or reorder any row's check; the exact policy call sequence must fail.
    // Mutation: erase CreateWindow's Creation record; its parent/owner assertion must fail.
    let mut fixture = window_access_fixture();
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .state
        .install_access_policy(Arc::new(RecordingExtensionPolicy::new(
            Decision::Allow,
            Arc::clone(&calls),
        )));
    let created = ResourceId(0x0054_000c);
    let create_body = create_window_with_access_attributes(
        created,
        ROOT_WINDOW,
        fixture.background,
        fixture.border,
        fixture.colormap,
        fixture.cursor,
    );
    let attrs_body = change_window_attributes_with_access_operands(
        fixture.window,
        fixture.background,
        fixture.border,
        fixture.colormap,
        fixture.cursor,
    );
    for (sequence, (opcode, data, body)) in (70u16..).zip([
        (1, 24, create_body),
        (2, 0, attrs_body),
        (2, 0, words(&[fixture.window.0, 0x0800, 1])),
        (2, 0, words(&[fixture.window.0, 0x0200, 1])),
        (3, 0, words(&[fixture.window.0])),
        (4, 0, words(&[fixture.destroy_window.0])),
        (5, 0, words(&[fixture.subwindows_parent.0])),
        (6, 0, words(&[fixture.window.0])),
    ]) {
        let bytes = resource_request(
            &mut fixture.state,
            &mut fixture.backend,
            &mut fixture.peer,
            sequence,
            opcode,
            data,
            &body,
        );
        if opcode == 3 {
            assert_eq!(bytes.first(), Some(&1), "GetWindowAttributes reply");
        } else {
            assert_no_error(&bytes, "allowed window access request");
        }
    }

    use AccessMode as M;
    use ResourceKind as K;
    let expected = vec![
        core_check(ROOT_WINDOW, K::Window, M::ADD, 1),
        core_check(fixture.background, K::Pixmap, M::READ, 1),
        core_check(fixture.colormap, K::Colormap, M::USE, 1),
        core_check(fixture.cursor, K::Cursor, M::USE, 1),
        core_check(fixture.border, K::Pixmap, M::READ, 1),
        core_check(created, K::Window, M::CREATE | M::SET_ATTR, 1),
        core_check(fixture.window, K::Window, M::RECEIVE | M::SET_ATTR, 2),
        core_check(fixture.background, K::Pixmap, M::READ, 2),
        core_check(fixture.colormap, K::Colormap, M::USE, 2),
        core_check(fixture.cursor, K::Cursor, M::USE, 2),
        core_check(fixture.border, K::Pixmap, M::READ, 2),
        core_check(fixture.window, K::Window, M::RECEIVE, 2),
        core_check(fixture.window, K::Window, M::SET_ATTR, 2),
        core_check(fixture.window, K::Window, M::GET_ATTR, 3),
        core_check(fixture.destroy_window, K::Window, M::DESTROY, 4),
        core_check(fixture.destroy_parent, K::Window, M::REMOVE, 4),
        core_check(fixture.subwindows_parent, K::Window, M::REMOVE, 5),
        core_check(fixture.subwindow_children[0], K::Window, M::DESTROY, 5),
        core_check(fixture.subwindow_children[1], K::Window, M::DESTROY, 5),
        core_check(fixture.window, K::Window, M::MANAGE, 6),
    ];
    assert_eq!(expected_resource_checks(&calls.lock().unwrap()), expected);
    let recorded = calls.lock().unwrap();
    let creation = recorded
        .iter()
        .find(|call| call.target.is_some_and(|target| target.id == created))
        .expect("CreateWindow creation check");
    assert_eq!(
        creation.target.unwrap().creation,
        Some(Creation {
            parent: Some(ROOT_WINDOW),
            owner: CLIENT,
        })
    );
    assert!(fixture.state.resources.window(created).is_some());
}

struct DestroySubwindowsFixture {
    state: ServerState,
    backend: RecordingBackend,
    requester: UnixStream,
    observer: UnixStream,
    parent: ResourceId,
    children: Vec<ResourceId>,
}

fn mapped_destroy_subwindows_fixture() -> DestroySubwindowsFixture {
    let mut state = ServerState::new();
    let mut requester = install_client(&mut state, CLIENT.0);
    let observer = install_client(&mut state, 2);
    let mut backend = RecordingBackend::new();
    let parent = ResourceId(0x0055_0001);
    let children = [
        ResourceId(0x0055_0002),
        ResourceId(0x0055_0003),
        ResourceId(0x0055_0004),
    ];
    let mut sequence = 1;
    create_test_window(
        &mut state,
        &mut backend,
        &mut requester,
        &mut sequence,
        parent,
        ROOT_WINDOW,
    );
    for child in children {
        create_test_window(
            &mut state,
            &mut backend,
            &mut requester,
            &mut sequence,
            child,
            parent,
        );
    }
    for window in [parent, children[0], children[1], children[2]] {
        let bytes = resource_request(
            &mut state,
            &mut backend,
            &mut requester,
            sequence,
            8,
            0,
            &words(&[window.0]),
        );
        assert_no_error(&bytes, "DestroySubwindows fixture MapWindow");
        sequence += 1;
    }
    let children = state.resources.children(parent).to_vec();
    let observer_state = state.clients.get_mut(&2).expect("observer client");
    for child in &children {
        observer_state.event_masks.insert(*child, 0x0002_0000);
    }
    DestroySubwindowsFixture {
        state,
        backend,
        requester,
        observer,
        parent,
        children,
    }
}

fn event_type_and_window(bytes: &[u8]) -> Vec<(u8, ResourceId)> {
    assert_eq!(bytes.len() % 32, 0, "event stream has complete X11 events");
    bytes
        .chunks_exact(32)
        .map(|event| {
            (
                event[0] & 0x7f,
                ResourceId(u32::from_le_bytes(event[8..12].try_into().unwrap())),
            )
        })
        .collect()
}

fn destroy_subwindows_wire_bytes(child_count: usize, allow_policy: bool) -> (Vec<u8>, Vec<u8>) {
    let mut state = ServerState::new();
    let mut requester = install_client(&mut state, CLIENT.0);
    let mut observer = install_client(&mut state, 2);
    let mut backend = RecordingBackend::new();
    let parent = ResourceId(0x0056_0001);
    let children = [
        ResourceId(0x0056_0002),
        ResourceId(0x0056_0003),
        ResourceId(0x0056_0004),
    ];
    let mut sequence = 1;
    create_test_window(
        &mut state,
        &mut backend,
        &mut requester,
        &mut sequence,
        parent,
        ROOT_WINDOW,
    );
    for child in children.iter().take(child_count) {
        create_test_window(
            &mut state,
            &mut backend,
            &mut requester,
            &mut sequence,
            *child,
            parent,
        );
    }
    for window in std::iter::once(parent).chain(children.into_iter().take(child_count)) {
        let bytes = resource_request(
            &mut state,
            &mut backend,
            &mut requester,
            sequence,
            8,
            0,
            &words(&[window.0]),
        );
        assert_no_error(&bytes, "DestroySubwindows byte fixture MapWindow");
        sequence += 1;
    }
    for child in state.resources.children(parent).to_vec() {
        state
            .clients
            .get_mut(&2)
            .expect("observer client")
            .event_masks
            .insert(child, 0x0002_0000);
    }
    if allow_policy {
        state.install_access_policy(Arc::new(RecordingExtensionPolicy::new(
            Decision::Allow,
            Arc::new(Mutex::new(Vec::new())),
        )));
    }
    let request_bytes = resource_request(
        &mut state,
        &mut backend,
        &mut requester,
        sequence,
        5,
        0,
        &words(&[parent.0]),
    );
    let event_bytes = read_all_available(&mut observer);
    (request_bytes, event_bytes)
}

#[test]
fn destroy_subwindows_denied_child_stops_the_loop() {
    // Mutation: check before the bulk unmap, or continue after the first child denial; each changes
    // these event/state assertions.
    let mut fixture = mapped_destroy_subwindows_fixture();
    let [c1, c2, c3] = fixture.children.as_slice() else {
        panic!("three children");
    };
    let c1 = *c1;
    let c2 = *c2;
    let c3 = *c3;
    let child_hosts = fixture
        .children
        .iter()
        .map(|child| {
            (
                *child,
                fixture
                    .state
                    .resources
                    .window(*child)
                    .and_then(|window| window.host_xid)
                    .expect("mapped fixture host window")
                    .as_raw(),
            )
        })
        .collect::<Vec<_>>();
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .state
        .install_access_policy(Arc::new(window_resource_policy(
            Arc::clone(&calls),
            Some(Arc::new(AtomicU32::new(c2.0))),
            Some(AccessMode::DESTROY),
            false,
        )));
    let backend_before = fixture.backend.calls();
    let bytes = resource_request(
        &mut fixture.state,
        &mut fixture.backend,
        &mut fixture.requester,
        20,
        5,
        0,
        &words(&[fixture.parent.0]),
    );
    assert!(bytes.is_empty(), "derived-target denial returns Success");
    let events = read_all_available(&mut fixture.observer);
    assert_eq!(
        event_type_and_window(&events),
        [(18, c1), (18, c2), (18, c3), (17, c1)],
        "bulk UnmapNotify events precede the allowed child's DestroyNotify"
    );
    assert!(
        fixture.state.resources.window(c1).is_none(),
        "first child destroyed"
    );
    for child in [c2, c3] {
        assert_eq!(
            fixture
                .state
                .resources
                .window(child)
                .map(|window| window.map_state),
            Some(crate::resources::MapState::Unmapped),
            "remaining child stays present and unmapped"
        );
    }
    assert_eq!(fixture.state.resources.children(fixture.parent), &[c2, c3]);
    let calls_after = fixture.backend.calls();
    let new_calls = &calls_after[backend_before.len()..];
    let unmapped = new_calls
        .iter()
        .filter_map(|call| match call {
            RecordedCall::UnmapSubwindow(host) => Some(*host),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        unmapped,
        fixture
            .children
            .iter()
            .map(|child| child_hosts.iter().find(|(id, _)| id == child).unwrap().1)
            .collect::<Vec<_>>(),
        "all children are unmapped before the destroy loop"
    );
    let destroyed = new_calls
        .iter()
        .filter_map(|call| match call {
            RecordedCall::DestroySubwindow(host) => Some(*host),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        destroyed,
        [child_hosts.iter().find(|(id, _)| *id == c1).unwrap().1]
    );
    assert_eq!(
        expected_resource_checks(&calls.lock().unwrap()),
        [
            (
                CLIENT,
                fixture.parent,
                ResourceKind::Window,
                AccessMode::REMOVE,
                Some(RequestOpcodes { major: 5, minor: 0 })
            ),
            (
                CLIENT,
                c1,
                ResourceKind::Window,
                AccessMode::DESTROY,
                Some(RequestOpcodes { major: 5, minor: 0 })
            ),
            (
                CLIENT,
                c2,
                ResourceKind::Window,
                AccessMode::DESTROY,
                Some(RequestOpcodes { major: 5, minor: 0 })
            ),
        ]
    );

    let empty_without_policy = destroy_subwindows_wire_bytes(0, false);
    let empty_with_policy = destroy_subwindows_wire_bytes(0, true);
    assert_eq!(
        empty_with_policy, empty_without_policy,
        "empty list preserves today's bytes"
    );
    let populated_without_policy = destroy_subwindows_wire_bytes(3, false);
    let populated_with_policy = destroy_subwindows_wire_bytes(3, true);
    assert_eq!(
        populated_with_policy, populated_without_policy,
        "allow-all policy preserves today's event bytes"
    );
}

#[test]
fn destroy_subwindows_bulk_unmap_ignores_hide() {
    // Mutation: put a HIDE check in the shared unmap helper; denying HIDE must not keep a child
    // mapped here.
    let mut fixture = mapped_destroy_subwindows_fixture();
    let [c1, c2, c3] = fixture.children.as_slice() else {
        panic!("three children");
    };
    let (c1, c2, c3) = (*c1, *c2, *c3);
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .state
        .install_access_policy(Arc::new(window_resource_policy(
            Arc::clone(&calls),
            Some(Arc::new(AtomicU32::new(c2.0))),
            Some(AccessMode::DESTROY),
            true,
        )));
    let bytes = resource_request(
        &mut fixture.state,
        &mut fixture.backend,
        &mut fixture.requester,
        21,
        5,
        0,
        &words(&[fixture.parent.0]),
    );
    assert!(bytes.is_empty(), "derived-target denial returns Success");
    assert_eq!(
        event_type_and_window(&read_all_available(&mut fixture.observer)),
        [(18, c1), (18, c2), (18, c3), (17, c1)]
    );
    assert!(fixture.state.resources.window(c1).is_none());
    for child in [c2, c3] {
        assert_eq!(
            fixture
                .state
                .resources
                .window(child)
                .map(|window| window.map_state),
            Some(crate::resources::MapState::Unmapped)
        );
    }
    assert!(
        expected_resource_checks(&calls.lock().unwrap())
            .iter()
            .all(|(_, _, _, mode, _)| *mode != AccessMode::HIDE),
        "DestroySubwindows asks only REMOVE and DESTROY"
    );
}

#[test]
fn map_window_denied_show_returns_success_and_maps_nothing() {
    // Mutation: report the MapWindow SHOW denial as an X error; this request must instead succeed
    // without mapping or notifying.
    let mut fixture = window_access_fixture();
    fixture
        .state
        .clients
        .get_mut(&CLIENT.0)
        .expect("requesting client")
        .event_masks
        .insert(fixture.window, 0x0002_0000);
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .state
        .install_access_policy(Arc::new(window_resource_policy(
            Arc::clone(&calls),
            Some(Arc::new(AtomicU32::new(fixture.window.0))),
            Some(AccessMode::SHOW),
            false,
        )));
    let backend_before = fixture.backend.calls();

    let bytes = resource_request(
        &mut fixture.state,
        &mut fixture.backend,
        &mut fixture.peer,
        80,
        8,
        0,
        &words(&[fixture.window.0]),
    );

    assert!(
        bytes.is_empty(),
        "denied MapWindow has no error or MapNotify"
    );
    assert_eq!(
        fixture
            .state
            .resources
            .window(fixture.window)
            .unwrap()
            .map_state,
        crate::resources::MapState::Unmapped
    );
    assert_eq!(fixture.backend.calls(), backend_before);
    assert_eq!(
        expected_resource_checks(&calls.lock().unwrap()),
        [(
            CLIENT,
            fixture.window,
            ResourceKind::Window,
            AccessMode::SHOW,
            Some(RequestOpcodes { major: 8, minor: 0 }),
        )]
    );
}

#[test]
fn window_tree_resource_request_access_calls_follow_site_order() {
    // Mutation: omit or reorder any scoped whole-request or derived-target check; its recorded
    // target/mode sequence must differ.
    let labels = [
        "reparent",
        "map_window",
        "map_subwindows",
        "unmap_window",
        "unmap_subwindows",
        "configure_window",
        "circulate_window",
        "get_geometry_window",
        "get_geometry_pixmap",
        "query_tree",
        "query_pointer",
        "get_motion_events",
        "translate_coordinates",
        "warp_pointer",
    ];

    for (index, label) in labels.into_iter().enumerate() {
        let mut fixture = window_access_fixture();
        let mut setup_sequence = 1;
        let mut map = |window: ResourceId| {
            let bytes = resource_request(
                &mut fixture.state,
                &mut fixture.backend,
                &mut fixture.peer,
                setup_sequence,
                8,
                0,
                &words(&[window.0]),
            );
            assert_no_error(&bytes, "window tree access fixture MapWindow");
            setup_sequence += 1;
        };
        let (opcode, data, body, expected): WindowAccessCase = match label {
            "reparent" => {
                map(fixture.parent);
                map(fixture.window);
                (
                    7,
                    0,
                    words(&[fixture.window.0, fixture.destroy_parent.0, 0]),
                    vec![
                        (fixture.window, ResourceKind::Window, AccessMode::MANAGE),
                        (
                            fixture.destroy_parent,
                            ResourceKind::Window,
                            AccessMode::ADD,
                        ),
                        (fixture.window, ResourceKind::Window, AccessMode::SHOW),
                    ],
                )
            }
            "map_window" => (
                8,
                0,
                words(&[fixture.window.0]),
                vec![(fixture.window, ResourceKind::Window, AccessMode::SHOW)],
            ),
            "map_subwindows" => (
                9,
                0,
                words(&[fixture.subwindows_parent.0]),
                vec![
                    (
                        fixture.subwindows_parent,
                        ResourceKind::Window,
                        AccessMode::LIST,
                    ),
                    (
                        fixture.subwindow_children[1],
                        ResourceKind::Window,
                        AccessMode::SHOW,
                    ),
                    (
                        fixture.subwindow_children[0],
                        ResourceKind::Window,
                        AccessMode::SHOW,
                    ),
                ],
            ),
            "unmap_window" => {
                map(fixture.parent);
                map(fixture.window);
                (
                    10,
                    0,
                    words(&[fixture.window.0]),
                    vec![(fixture.window, ResourceKind::Window, AccessMode::HIDE)],
                )
            }
            "unmap_subwindows" => {
                map(fixture.subwindows_parent);
                for child in fixture.subwindow_children {
                    map(child);
                }
                (
                    11,
                    0,
                    words(&[fixture.subwindows_parent.0]),
                    vec![
                        (
                            fixture.subwindows_parent,
                            ResourceKind::Window,
                            AccessMode::LIST,
                        ),
                        (
                            fixture.subwindow_children[0],
                            ResourceKind::Window,
                            AccessMode::HIDE,
                        ),
                        (
                            fixture.subwindow_children[1],
                            ResourceKind::Window,
                            AccessMode::HIDE,
                        ),
                    ],
                )
            }
            "configure_window" => {
                let sibling = ResourceId(0x0054_000c);
                create_test_window(
                    &mut fixture.state,
                    &mut fixture.backend,
                    &mut fixture.peer,
                    &mut setup_sequence,
                    sibling,
                    fixture.parent,
                );
                (
                    12,
                    0,
                    words(&[fixture.window.0, 0x0060, sibling.0, 0]),
                    vec![
                        (
                            fixture.window,
                            ResourceKind::Window,
                            AccessMode::MANAGE | AccessMode::SET_ATTR,
                        ),
                        (sibling, ResourceKind::Window, AccessMode::GET_ATTR),
                    ],
                )
            }
            "circulate_window" => {
                map(fixture.subwindows_parent);
                for child in fixture.subwindow_children {
                    map(child);
                }
                let child = fixture
                    .state
                    .circulate_candidate(fixture.subwindows_parent, 0)
                    .expect("overlapping mapped child is a CirculateWindow target");
                (
                    13,
                    0,
                    words(&[fixture.subwindows_parent.0]),
                    vec![
                        (
                            fixture.subwindows_parent,
                            ResourceKind::Window,
                            AccessMode::MANAGE,
                        ),
                        (
                            child,
                            ResourceKind::Window,
                            AccessMode::MANAGE | AccessMode::SET_ATTR,
                        ),
                    ],
                )
            }
            "get_geometry_window" => (
                14,
                0,
                words(&[fixture.window.0]),
                vec![(fixture.window, ResourceKind::Window, AccessMode::GET_ATTR)],
            ),
            "get_geometry_pixmap" => (
                14,
                0,
                words(&[fixture.border.0]),
                vec![(fixture.border, ResourceKind::Pixmap, AccessMode::GET_ATTR)],
            ),
            "query_tree" => (
                15,
                0,
                words(&[fixture.window.0]),
                vec![(fixture.window, ResourceKind::Window, AccessMode::LIST)],
            ),
            "query_pointer" => (
                38,
                0,
                words(&[fixture.window.0]),
                vec![(fixture.window, ResourceKind::Window, AccessMode::GET_ATTR)],
            ),
            "get_motion_events" => (
                39,
                0,
                words(&[fixture.window.0, 0, 0]),
                vec![(fixture.window, ResourceKind::Window, AccessMode::GET_ATTR)],
            ),
            "translate_coordinates" => (
                40,
                0,
                words(&[fixture.window.0, fixture.parent.0, 0]),
                vec![
                    (fixture.window, ResourceKind::Window, AccessMode::GET_ATTR),
                    (fixture.parent, ResourceKind::Window, AccessMode::GET_ATTR),
                ],
            ),
            "warp_pointer" => {
                let mut body = words(&[fixture.parent.0, fixture.window.0]);
                body.extend_from_slice(&[0; 12]);
                (
                    41,
                    0,
                    body,
                    vec![
                        (fixture.window, ResourceKind::Window, AccessMode::GET_ATTR),
                        (fixture.parent, ResourceKind::Window, AccessMode::GET_ATTR),
                    ],
                )
            }
            _ => unreachable!(),
        };
        let calls = Arc::new(Mutex::new(Vec::new()));
        fixture
            .state
            .install_access_policy(Arc::new(window_resource_policy(
                Arc::clone(&calls),
                None,
                None,
                false,
            )));

        let bytes = resource_request(
            &mut fixture.state,
            &mut fixture.backend,
            &mut fixture.peer,
            80 + u16::try_from(index).unwrap(),
            opcode,
            data,
            &body,
        );

        assert!(
            bytes.first().is_none_or(|first| *first != 0),
            "{label} unexpectedly returned an X error: {bytes:02x?}"
        );
        let actual = expected_resource_checks(&calls.lock().unwrap());
        let expected = expected
            .into_iter()
            .map(|(id, kind, mode)| {
                (
                    CLIENT,
                    id,
                    kind,
                    mode,
                    Some(RequestOpcodes {
                        major: opcode,
                        minor: 0,
                    }),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(actual, expected, "{label} check order");
    }
}

#[test]
fn window_tree_resource_request_denials_match_modes_and_stop_before_effects() {
    // Mutation: remove or change a whole-request target check; the policy error, request opcode,
    // backend log, or window snapshot must fail.
    let mut fixture = window_access_fixture();
    let mut setup_sequence = 1;
    for window in [fixture.parent, fixture.window, fixture.subwindows_parent] {
        let bytes = resource_request(
            &mut fixture.state,
            &mut fixture.backend,
            &mut fixture.peer,
            setup_sequence,
            8,
            0,
            &words(&[window.0]),
        );
        assert_no_error(&bytes, "window request denial fixture MapWindow");
        setup_sequence += 1;
    }
    for child in fixture.subwindow_children {
        let bytes = resource_request(
            &mut fixture.state,
            &mut fixture.backend,
            &mut fixture.peer,
            setup_sequence,
            8,
            0,
            &words(&[child.0]),
        );
        assert_no_error(&bytes, "window request denial fixture MapWindow child");
        setup_sequence += 1;
    }
    let sibling = ResourceId(0x0054_000c);
    create_test_window(
        &mut fixture.state,
        &mut fixture.backend,
        &mut fixture.peer,
        &mut setup_sequence,
        sibling,
        fixture.parent,
    );
    let reparent = words(&[fixture.window.0, fixture.destroy_parent.0, 0]);
    let config = words(&[fixture.window.0, 0x0001, 17]);
    let config_sib = words(&[fixture.window.0, 0x0060, sibling.0, 0]);
    let mut warp = words(&[fixture.parent.0, fixture.window.0]);
    warp.extend_from_slice(&[0; 12]);
    use AccessMode as M;
    use ResourceKind as K;
    let map_subs = words(&[fixture.subwindows_parent.0]);
    let unmap = words(&[fixture.window.0]);
    let circulate = words(&[fixture.subwindows_parent.0]);
    let geom_win = words(&[fixture.window.0]);
    let geom_pix = words(&[fixture.border.0]);
    let query_ptr = words(&[fixture.window.0]);
    let motion = words(&[fixture.window.0, 0, 0]);
    let translate = words(&[fixture.window.0, fixture.parent.0, 0]);
    let win_manage = existing(fixture.window, K::Window, M::MANAGE);
    let parent_add = existing(fixture.destroy_parent, K::Window, M::ADD);
    let sub_list = existing(fixture.subwindows_parent, K::Window, M::LIST);
    let win_hide = existing(fixture.window, K::Window, M::HIDE);
    let config_win = existing(fixture.window, K::Window, M::MANAGE | M::SET_ATTR);
    let sibling_get = existing(sibling, K::Window, M::GET_ATTR);
    let container = existing(fixture.subwindows_parent, K::Window, M::MANAGE);
    let win_get = existing(fixture.window, K::Window, M::GET_ATTR);
    let pixmap_get = existing(fixture.border, K::Pixmap, M::GET_ATTR);
    let win_list = existing(fixture.window, K::Window, M::LIST);
    let parent_get = existing(fixture.parent, K::Window, M::GET_ATTR);
    const W_REPARENT: (&str, u8, u8) = ("ReparentWindow window", 7, 0);
    const W_REPARENT_PARENT: (&str, u8, u8) = ("ReparentWindow new parent", 7, 0);
    const W_MAP_SUBS: (&str, u8, u8) = ("MapSubwindows parent", 9, 0);
    const W_UNMAP: (&str, u8, u8) = ("UnmapWindow", 10, 0);
    const W_UNMAP_SUBS: (&str, u8, u8) = ("UnmapSubwindows parent", 11, 0);
    const W_CONFIG: (&str, u8, u8) = ("ConfigureWindow", 12, 0);
    const W_CONFIG_SIBLING: (&str, u8, u8) = ("ConfigureWindow sibling", 12, 0);
    const W_CIRCULATE: (&str, u8, u8) = ("CirculateWindow container", 13, 0);
    const W_GEOM_WINDOW: (&str, u8, u8) = ("GetGeometry window", 14, 0);
    const W_GEOM_PIXMAP: (&str, u8, u8) = ("GetGeometry pixmap", 14, 0);
    const W_QUERY_TREE: (&str, u8, u8) = ("QueryTree", 15, 0);
    const W_QUERY_POINTER: (&str, u8, u8) = ("QueryPointer", 38, 0);
    const W_MOTION_EVENTS: (&str, u8, u8) = ("GetMotionEvents", 39, 0);
    const W_TRANSLATE_SRC: (&str, u8, u8) = ("TranslateCoordinates source", 40, 0);
    const W_TRANSLATE_DST: (&str, u8, u8) = ("TranslateCoordinates destination", 40, 0);
    const W_WARP_DST: (&str, u8, u8) = ("WarpPointer destination", 41, 0);
    const W_WARP_SRC: (&str, u8, u8) = ("WarpPointer source", 41, 0);
    let cases = vec![
        wr(W_REPARENT, reparent.clone(), win_manage),
        wr(W_REPARENT_PARENT, reparent, parent_add),
        wr(W_MAP_SUBS, map_subs.clone(), sub_list),
        wr(W_UNMAP, unmap, win_hide),
        wr(W_UNMAP_SUBS, map_subs, sub_list),
        wr(W_CONFIG, config, config_win),
        wr(W_CONFIG_SIBLING, config_sib, sibling_get),
        wr(W_CIRCULATE, circulate, container),
        wr(W_GEOM_WINDOW, geom_win.clone(), win_get),
        wr(W_GEOM_PIXMAP, geom_pix, pixmap_get),
        wr(W_QUERY_TREE, geom_win, win_list),
        wr(W_QUERY_POINTER, query_ptr, win_get),
        wr(W_MOTION_EVENTS, motion, win_get),
        wr(W_TRANSLATE_SRC, translate.clone(), win_get),
        wr(W_TRANSLATE_DST, translate, parent_get),
        wr(W_WARP_DST, warp.clone(), win_get),
        wr(W_WARP_SRC, warp, parent_get),
    ];
    let calls = Arc::new(Mutex::new(Vec::new()));
    let denied_id = Arc::new(AtomicU32::new(0));
    fixture
        .state
        .install_access_policy(Arc::new(window_resource_policy(
            Arc::clone(&calls),
            Some(Arc::clone(&denied_id)),
            None,
            false,
        )));

    for (index, case) in cases.into_iter().enumerate() {
        denied_id.store(case.target.id.0, Ordering::SeqCst);
        let sequence = 100 + u16::try_from(index).unwrap();
        let backend_before = fixture.backend.calls();
        let mut windows_before = fixture
            .state
            .resources
            .windows_iter()
            .map(|window| format!("{window:?}"))
            .collect::<Vec<_>>();
        windows_before.sort();
        let calls_before = calls.lock().unwrap().len();

        let bytes = resource_request(
            &mut fixture.state,
            &mut fixture.backend,
            &mut fixture.peer,
            sequence,
            case.opcode,
            case.data,
            &case.body,
        );

        assert_x11_error(&bytes, x11::error::BAD_VALUE, 0x55, case.opcode, sequence);
        assert_eq!(
            fixture.backend.calls(),
            backend_before,
            "{} backend",
            case.label
        );
        let mut windows_after = fixture
            .state
            .resources
            .windows_iter()
            .map(|window| format!("{window:?}"))
            .collect::<Vec<_>>();
        windows_after.sort();
        assert_eq!(windows_after, windows_before, "{} state", case.label);
        let checks = calls.lock().unwrap();
        assert!(checks.len() > calls_before, "{} check count", case.label);
        let denied = checks.last().expect("denial was recorded");
        assert_eq!(denied.client, CLIENT, "{} client", case.label);
        assert_eq!(
            denied.target.map(|target| (target.id, target.kind)),
            Some((case.target.id, case.target.kind)),
            "{} target",
            case.label
        );
        assert_eq!(denied.mode, Some(case.mode), "{} mode", case.label);
        assert_eq!(
            denied.request,
            Some(RequestOpcodes {
                major: case.opcode,
                minor: 0
            }),
            "{} request",
            case.label
        );
    }
}

fn three_child_window_access_fixture(map_children: bool) -> DestroySubwindowsFixture {
    let mut state = ServerState::new();
    let mut requester = install_client(&mut state, CLIENT.0);
    let mut observer = install_client(&mut state, 2);
    let mut backend = RecordingBackend::new();
    let parent = ResourceId(0x0057_0001);
    let children = [
        ResourceId(0x0057_0002),
        ResourceId(0x0057_0003),
        ResourceId(0x0057_0004),
    ];
    let mut sequence = 1;
    create_test_window(
        &mut state,
        &mut backend,
        &mut requester,
        &mut sequence,
        parent,
        ROOT_WINDOW,
    );
    for child in children {
        create_test_window(
            &mut state,
            &mut backend,
            &mut requester,
            &mut sequence,
            child,
            parent,
        );
    }
    for window in
        std::iter::once(parent).chain(map_children.then_some(children).into_iter().flatten())
    {
        let bytes = resource_request(
            &mut state,
            &mut backend,
            &mut requester,
            sequence,
            8,
            0,
            &words(&[window.0]),
        );
        assert_no_error(&bytes, "three-child access fixture MapWindow");
        sequence += 1;
    }
    for child in children {
        state
            .clients
            .get_mut(&2)
            .expect("observer client")
            .event_masks
            .insert(child, 0x0002_0000);
    }
    let _ = read_all_available(&mut observer);
    DestroySubwindowsFixture {
        state,
        backend,
        requester,
        observer,
        parent,
        children: children.to_vec(),
    }
}

#[test]
fn map_subwindows_skips_denied_child() {
    // Mutation: map the denied middle child or stop the loop there; only the two allowed children
    // may map and notify.
    let mut fixture = three_child_window_access_fixture(false);
    let [c1, c2, c3] = fixture.children.as_slice() else {
        panic!("three children");
    };
    let (c1, c2, c3) = (*c1, *c2, *c3);
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .state
        .install_access_policy(Arc::new(window_resource_policy(
            Arc::clone(&calls),
            Some(Arc::new(AtomicU32::new(c2.0))),
            Some(AccessMode::SHOW),
            false,
        )));
    let host_c1 = fixture
        .state
        .resources
        .window(c1)
        .and_then(|window| window.host_xid)
        .expect("child has a backend window")
        .as_raw();
    let host_c3 = fixture
        .state
        .resources
        .window(c3)
        .and_then(|window| window.host_xid)
        .expect("child has a backend window")
        .as_raw();
    let backend_before = fixture.backend.calls();

    let bytes = resource_request(
        &mut fixture.state,
        &mut fixture.backend,
        &mut fixture.requester,
        20,
        9,
        0,
        &words(&[fixture.parent.0]),
    );

    assert!(bytes.is_empty(), "denied child is a successful skip");
    assert_eq!(
        event_type_and_window(&read_all_available(&mut fixture.observer)),
        [(19, c3), (19, c1)]
    );
    assert_eq!(
        fixture.state.resources.window(c1).unwrap().map_state,
        crate::resources::MapState::Viewable
    );
    assert_eq!(
        fixture.state.resources.window(c2).unwrap().map_state,
        crate::resources::MapState::Unmapped
    );
    assert_eq!(
        fixture.state.resources.window(c3).unwrap().map_state,
        crate::resources::MapState::Viewable
    );
    let mapped = fixture.backend.calls()[backend_before.len()..]
        .iter()
        .filter_map(|call| match call {
            RecordedCall::MapSubwindow(host) => Some(*host),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(mapped, [host_c3, host_c1]);
    assert_eq!(
        expected_resource_checks(&calls.lock().unwrap()),
        [
            (
                CLIENT,
                fixture.parent,
                ResourceKind::Window,
                AccessMode::LIST,
                Some(RequestOpcodes { major: 9, minor: 0 })
            ),
            (
                CLIENT,
                c3,
                ResourceKind::Window,
                AccessMode::SHOW,
                Some(RequestOpcodes { major: 9, minor: 0 })
            ),
            (
                CLIENT,
                c2,
                ResourceKind::Window,
                AccessMode::SHOW,
                Some(RequestOpcodes { major: 9, minor: 0 })
            ),
            (
                CLIENT,
                c1,
                ResourceKind::Window,
                AccessMode::SHOW,
                Some(RequestOpcodes { major: 9, minor: 0 })
            ),
        ]
    );
}

#[test]
fn unmap_subwindows_skips_denied_child() {
    // Mutation: unmap the denied middle child or stop the loop there; the two allowed children must
    // still unmap.
    let mut fixture = three_child_window_access_fixture(true);
    let [c1, c2, c3] = fixture.children.as_slice() else {
        panic!("three children");
    };
    let (c1, c2, c3) = (*c1, *c2, *c3);
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .state
        .install_access_policy(Arc::new(window_resource_policy(
            Arc::clone(&calls),
            Some(Arc::new(AtomicU32::new(c2.0))),
            Some(AccessMode::HIDE),
            false,
        )));
    let host_c1 = fixture
        .state
        .resources
        .window(c1)
        .and_then(|window| window.host_xid)
        .expect("child has a backend window")
        .as_raw();
    let host_c3 = fixture
        .state
        .resources
        .window(c3)
        .and_then(|window| window.host_xid)
        .expect("child has a backend window")
        .as_raw();
    let backend_before = fixture.backend.calls();

    let bytes = resource_request(
        &mut fixture.state,
        &mut fixture.backend,
        &mut fixture.requester,
        21,
        11,
        0,
        &words(&[fixture.parent.0]),
    );

    assert!(bytes.is_empty(), "denied child is a successful skip");
    assert_eq!(
        event_type_and_window(&read_all_available(&mut fixture.observer)),
        [(18, c1), (18, c3)]
    );
    assert_eq!(
        fixture.state.resources.window(c1).unwrap().map_state,
        crate::resources::MapState::Unmapped
    );
    assert_eq!(
        fixture.state.resources.window(c2).unwrap().map_state,
        crate::resources::MapState::Viewable
    );
    assert_eq!(
        fixture.state.resources.window(c3).unwrap().map_state,
        crate::resources::MapState::Unmapped
    );
    let unmapped = fixture.backend.calls()[backend_before.len()..]
        .iter()
        .filter_map(|call| match call {
            RecordedCall::UnmapSubwindow(host) => Some(*host),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(unmapped, [host_c1, host_c3]);
    assert_eq!(
        expected_resource_checks(&calls.lock().unwrap()),
        [
            (
                CLIENT,
                fixture.parent,
                ResourceKind::Window,
                AccessMode::LIST,
                Some(RequestOpcodes {
                    major: 11,
                    minor: 0
                })
            ),
            (
                CLIENT,
                c1,
                ResourceKind::Window,
                AccessMode::HIDE,
                Some(RequestOpcodes {
                    major: 11,
                    minor: 0
                })
            ),
            (
                CLIENT,
                c2,
                ResourceKind::Window,
                AccessMode::HIDE,
                Some(RequestOpcodes {
                    major: 11,
                    minor: 0
                })
            ),
            (
                CLIENT,
                c3,
                ResourceKind::Window,
                AccessMode::HIDE,
                Some(RequestOpcodes {
                    major: 11,
                    minor: 0
                })
            ),
        ]
    );
}

#[test]
fn circulate_denied_child_is_not_restacked() {
    // Mutation: restack the chosen child before or despite its MANAGE+SET_ATTR denial; order,
    // events, and backend calls must stay unchanged.
    let mut fixture = three_child_window_access_fixture(true);
    let child = fixture
        .state
        .circulate_candidate(fixture.parent, 0)
        .expect("mapped overlapping child is selected");
    for window in fixture.children.iter().copied().chain([fixture.parent]) {
        fixture
            .state
            .clients
            .get_mut(&2)
            .expect("observer client")
            .event_masks
            .insert(window, 0x0002_0000 | 0x0008_0000);
    }
    let _ = read_all_available(&mut fixture.observer);
    let children_before = fixture.state.resources.children(fixture.parent).to_vec();
    let backend_before = fixture.backend.calls();
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .state
        .install_access_policy(Arc::new(window_resource_policy(
            Arc::clone(&calls),
            Some(Arc::new(AtomicU32::new(child.0))),
            Some(AccessMode::MANAGE | AccessMode::SET_ATTR),
            false,
        )));

    let bytes = resource_request(
        &mut fixture.state,
        &mut fixture.backend,
        &mut fixture.requester,
        22,
        13,
        0,
        &words(&[fixture.parent.0]),
    );

    assert!(bytes.is_empty(), "denied chosen child is a successful skip");
    assert_eq!(
        fixture.state.resources.children(fixture.parent),
        children_before
    );
    assert!(read_all_available(&mut fixture.observer).is_empty());
    assert_eq!(fixture.backend.calls(), backend_before);
    assert_eq!(
        expected_resource_checks(&calls.lock().unwrap()),
        [
            (
                CLIENT,
                fixture.parent,
                ResourceKind::Window,
                AccessMode::MANAGE,
                Some(RequestOpcodes {
                    major: 13,
                    minor: 0
                })
            ),
            (
                CLIENT,
                child,
                ResourceKind::Window,
                AccessMode::MANAGE | AccessMode::SET_ATTR,
                Some(RequestOpcodes {
                    major: 13,
                    minor: 0
                })
            ),
        ]
    );
}

struct ReparentAccessFixture {
    state: ServerState,
    backend: RecordingBackend,
    requester: UnixStream,
    observer: UnixStream,
    new_parent: ResourceId,
    window: ResourceId,
    child: ResourceId,
}

fn reparent_access_fixture(with_pointer_grab: bool) -> ReparentAccessFixture {
    let mut state = ServerState::new();
    let mut requester = install_client(&mut state, CLIENT.0);
    let mut observer = install_client(&mut state, 2);
    let mut backend = RecordingBackend::new();
    let old_parent = ResourceId(0x0058_0001);
    let new_parent = ResourceId(0x0058_0002);
    let window = ResourceId(0x0058_0003);
    let child = ResourceId(0x0058_0004);
    let mut sequence = 1;
    for (id, parent) in [
        (old_parent, ROOT_WINDOW),
        (new_parent, ROOT_WINDOW),
        (window, old_parent),
        (child, window),
    ] {
        create_test_window(
            &mut state,
            &mut backend,
            &mut requester,
            &mut sequence,
            id,
            parent,
        );
    }
    for id in [old_parent, new_parent, window, child] {
        let bytes = resource_request(
            &mut state,
            &mut backend,
            &mut requester,
            sequence,
            8,
            0,
            &words(&[id.0]),
        );
        assert_no_error(&bytes, "ReparentWindow access fixture MapWindow");
        sequence += 1;
    }
    for (window_id, mask) in [
        (window, 0x0002_0000),
        (old_parent, 0x0008_0000),
        (new_parent, 0x0008_0000),
    ] {
        state
            .clients
            .get_mut(&2)
            .expect("observer client")
            .event_masks
            .insert(window_id, mask);
    }
    let _ = read_all_available(&mut requester);
    let _ = read_all_available(&mut observer);
    state.core_focus.raw = child.0;
    for client in state.clients.values_mut() {
        client.focused_window = child;
    }
    if with_pointer_grab {
        set_test_pointer_grab(&mut state, CLIENT.0, child.0, false, false);
    }
    ReparentAccessFixture {
        state,
        backend,
        requester,
        observer,
        new_parent,
        window,
        child,
    }
}

fn unmap_then_reparent_baseline(with_pointer_grab: bool) -> (Vec<RecordedCall>, u32, u8) {
    let mut fixture = reparent_access_fixture(with_pointer_grab);
    let start = fixture.backend.calls().len();
    let unmap = resource_request(
        &mut fixture.state,
        &mut fixture.backend,
        &mut fixture.requester,
        30,
        10,
        0,
        &words(&[fixture.window.0]),
    );
    assert!(unmap.is_empty());
    let reparent = resource_request(
        &mut fixture.state,
        &mut fixture.backend,
        &mut fixture.requester,
        31,
        7,
        0,
        &words(&[fixture.window.0, fixture.new_parent.0, 0]),
    );
    assert!(reparent.is_empty());
    let calls = fixture.backend.calls()[start..].to_vec();
    (
        calls,
        fixture.state.core_focus.raw,
        fixture.state.core_focus.revert_to,
    )
}

#[test]
fn reparent_denied_remap_leaves_window_reparented_and_unmapped() {
    // Mutation: set only W's map state instead of running UnmapWindow's subtree/focus/grab
    // transition; C or focus/grab state must diverge.
    for with_pointer_grab in [false, true] {
        let (expected_backend, expected_focus, expected_revert) =
            unmap_then_reparent_baseline(with_pointer_grab);
        let mut fixture = reparent_access_fixture(with_pointer_grab);
        let start = fixture.backend.calls().len();
        let calls = Arc::new(Mutex::new(Vec::new()));
        fixture
            .state
            .install_access_policy(Arc::new(window_resource_policy(
                Arc::clone(&calls),
                Some(Arc::new(AtomicU32::new(fixture.window.0))),
                Some(AccessMode::SHOW),
                false,
            )));

        let bytes = resource_request(
            &mut fixture.state,
            &mut fixture.backend,
            &mut fixture.requester,
            40,
            7,
            0,
            &words(&[fixture.window.0, fixture.new_parent.0, 0]),
        );

        assert!(bytes.is_empty(), "denied remap returns Success");
        assert_eq!(
            event_type_and_window(&read_all_available(&mut fixture.observer)),
            [
                (18, fixture.window),
                (18, fixture.window),
                (21, fixture.window),
                (21, fixture.window),
                (21, fixture.window),
            ]
        );
        assert_eq!(
            fixture
                .state
                .resources
                .window(fixture.window)
                .unwrap()
                .parent,
            fixture.new_parent
        );
        assert_eq!(
            fixture
                .state
                .resources
                .window(fixture.window)
                .unwrap()
                .map_state,
            crate::resources::MapState::Unmapped
        );
        assert_eq!(
            fixture
                .state
                .resources
                .window(fixture.child)
                .unwrap()
                .map_state,
            crate::resources::MapState::Unviewable
        );
        assert_eq!(fixture.state.core_focus.raw, expected_focus);
        assert_eq!(fixture.state.core_focus.revert_to, expected_revert);
        assert!(fixture.state.active_pointer_grab.is_none());
        assert_eq!(fixture.backend.calls()[start..], expected_backend);
        assert_eq!(
            expected_resource_checks(&calls.lock().unwrap()),
            [
                (
                    CLIENT,
                    fixture.window,
                    ResourceKind::Window,
                    AccessMode::MANAGE,
                    Some(RequestOpcodes { major: 7, minor: 0 })
                ),
                (
                    CLIENT,
                    fixture.new_parent,
                    ResourceKind::Window,
                    AccessMode::ADD,
                    Some(RequestOpcodes { major: 7, minor: 0 })
                ),
                (
                    CLIENT,
                    fixture.window,
                    ResourceKind::Window,
                    AccessMode::SHOW,
                    Some(RequestOpcodes { major: 7, minor: 0 })
                ),
            ]
        );
    }
}

fn reparent_wire_with_allow_policy(install_policy: bool) -> (Vec<u8>, Vec<u8>, Vec<RecordedCall>) {
    let mut fixture = reparent_access_fixture(false);
    if install_policy {
        fixture
            .state
            .install_access_policy(Arc::new(window_resource_policy(
                Arc::new(Mutex::new(Vec::new())),
                None,
                None,
                false,
            )));
    }
    let start = fixture.backend.calls().len();
    let bytes = resource_request(
        &mut fixture.state,
        &mut fixture.backend,
        &mut fixture.requester,
        50,
        7,
        0,
        &words(&[fixture.window.0, fixture.new_parent.0, 0]),
    );
    let events = read_all_available(&mut fixture.observer);
    let backend = fixture.backend.calls()[start..].to_vec();
    (bytes, events, backend)
}

#[test]
fn reparent_with_allow_all_policy_keeps_todays_bytes() {
    // Mutation: take a policy-installed remap branch or change its event/backend sequence;
    // allow-all must match no-policy bytes.
    let without_policy = reparent_wire_with_allow_policy(false);
    let with_policy = reparent_wire_with_allow_policy(true);
    assert_eq!(with_policy, without_policy);
    assert_eq!(
        event_type_and_window(&with_policy.1),
        [
            (18, ResourceId(0x0058_0003)),
            (18, ResourceId(0x0058_0003)),
            (21, ResourceId(0x0058_0003)),
            (21, ResourceId(0x0058_0003)),
            (21, ResourceId(0x0058_0003)),
            (19, ResourceId(0x0058_0003)),
            (19, ResourceId(0x0058_0003)),
        ]
    );
}

#[test]
fn destroy_window_checks_parent_remove() {
    // Mutation: omit the parent REMOVE check; DestroyWindow must then destroy the target instead of
    // returning this denial.
    let mut fixture = window_access_fixture();
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .state
        .install_access_policy(Arc::new(window_resource_policy(
            Arc::clone(&calls),
            Some(Arc::new(AtomicU32::new(fixture.destroy_parent.0))),
            Some(AccessMode::REMOVE),
            false,
        )));
    let target_before = format!(
        "{:?}",
        fixture.state.resources.window(fixture.destroy_window)
    );
    let backend_before = fixture.backend.calls();
    let bytes = resource_request(
        &mut fixture.state,
        &mut fixture.backend,
        &mut fixture.peer,
        30,
        4,
        0,
        &words(&[fixture.destroy_window.0]),
    );
    assert_x11_error(&bytes, x11::error::BAD_VALUE, 0x55, 4, 30);
    assert_eq!(fixture.backend.calls(), backend_before);
    assert_eq!(
        format!(
            "{:?}",
            fixture.state.resources.window(fixture.destroy_window)
        ),
        target_before
    );
    assert_eq!(
        expected_resource_checks(&calls.lock().unwrap()),
        [
            (
                CLIENT,
                fixture.destroy_window,
                ResourceKind::Window,
                AccessMode::DESTROY,
                Some(RequestOpcodes { major: 4, minor: 0 })
            ),
            (
                CLIENT,
                fixture.destroy_parent,
                ResourceKind::Window,
                AccessMode::REMOVE,
                Some(RequestOpcodes { major: 4, minor: 0 })
            ),
        ]
    );
}

#[test]
fn configure_window_access_checks_precede_later_validation() {
    // Mutation: defer ConfigureWindow's window or sibling check until after sibling validation; the
    // earlier policy denial must win.
    let invalid_sibling = ResourceId(0x0054_00ff);
    let mut fixture = window_access_fixture();
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .state
        .install_access_policy(Arc::new(window_resource_policy(
            Arc::clone(&calls),
            Some(Arc::new(AtomicU32::new(fixture.window.0))),
            Some(AccessMode::MANAGE | AccessMode::SET_ATTR),
            false,
        )));
    let backend_before = fixture.backend.calls().to_vec();
    let window_before = format!("{:?}", fixture.state.resources.window(fixture.window));
    let stacking_before = fixture.state.resources.children(fixture.parent).to_vec();
    let bytes = resource_request(
        &mut fixture.state,
        &mut fixture.backend,
        &mut fixture.peer,
        80,
        12,
        0,
        &words(&[fixture.window.0, 0x0060, invalid_sibling.0, 0]),
    );
    assert_x11_error(&bytes, x11::error::BAD_VALUE, 0x55, 12, 80);
    assert_eq!(fixture.backend.calls(), backend_before);
    assert_eq!(
        format!("{:?}", fixture.state.resources.window(fixture.window)),
        window_before
    );
    assert_eq!(
        fixture.state.resources.children(fixture.parent),
        stacking_before
    );
    assert_eq!(
        expected_resource_checks(&calls.lock().unwrap()),
        [(
            CLIENT,
            fixture.window,
            ResourceKind::Window,
            AccessMode::MANAGE | AccessMode::SET_ATTR,
            Some(RequestOpcodes {
                major: 12,
                minor: 0
            }),
        )]
    );

    let mut no_policy = window_access_fixture();
    let bytes = resource_request(
        &mut no_policy.state,
        &mut no_policy.backend,
        &mut no_policy.peer,
        81,
        12,
        0,
        &words(&[no_policy.window.0, 0x0060, invalid_sibling.0, 0]),
    );
    assert_x11_error(&bytes, x11::error::BAD_WINDOW, invalid_sibling.0, 12, 81);

    let mut fixture = window_access_fixture();
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .state
        .install_access_policy(Arc::new(window_resource_policy(
            Arc::clone(&calls),
            Some(Arc::new(AtomicU32::new(fixture.destroy_parent.0))),
            Some(AccessMode::GET_ATTR),
            false,
        )));
    let backend_before = fixture.backend.calls().to_vec();
    let window_before = format!("{:?}", fixture.state.resources.window(fixture.window));
    let stacking_before = fixture.state.resources.children(fixture.parent).to_vec();
    let body = words(&[fixture.window.0, 0x0060, fixture.destroy_parent.0, 0]);
    let bytes = resource_request(
        &mut fixture.state,
        &mut fixture.backend,
        &mut fixture.peer,
        82,
        12,
        0,
        &body,
    );
    assert_x11_error(&bytes, x11::error::BAD_VALUE, 0x55, 12, 82);
    assert_eq!(fixture.backend.calls(), backend_before);
    assert_eq!(
        format!("{:?}", fixture.state.resources.window(fixture.window)),
        window_before
    );
    assert_eq!(
        fixture.state.resources.children(fixture.parent),
        stacking_before
    );
    assert_eq!(
        expected_resource_checks(&calls.lock().unwrap()),
        [
            (
                CLIENT,
                fixture.window,
                ResourceKind::Window,
                AccessMode::MANAGE | AccessMode::SET_ATTR,
                Some(RequestOpcodes {
                    major: 12,
                    minor: 0
                }),
            ),
            (
                CLIENT,
                fixture.destroy_parent,
                ResourceKind::Window,
                AccessMode::GET_ATTR,
                Some(RequestOpcodes {
                    major: 12,
                    minor: 0
                }),
            ),
        ]
    );

    let mut no_policy = window_access_fixture();
    let bytes = resource_request(
        &mut no_policy.state,
        &mut no_policy.backend,
        &mut no_policy.peer,
        83,
        12,
        0,
        &words(&[no_policy.window.0, 0x0060, no_policy.destroy_parent.0, 0]),
    );
    assert_x11_error(&bytes, x11::error::BAD_MATCH, 0, 12, 83);
}

#[test]
fn warp_pointer_checks_destination_before_invalid_source() {
    // Mutation: validate the source before checking the already-resolved destination; the
    // destination's policy error must win.
    let mut fixture = window_access_fixture();
    let invalid_source = ResourceId(0x0054_00fe);
    let mut body = words(&[invalid_source.0, fixture.window.0]);
    body.extend_from_slice(&[0; 12]);
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .state
        .install_access_policy(Arc::new(window_resource_policy(
            Arc::clone(&calls),
            Some(Arc::new(AtomicU32::new(fixture.window.0))),
            Some(AccessMode::GET_ATTR),
            false,
        )));
    let backend_before = fixture.backend.calls().to_vec();
    let pointer_before = format!("{:?}", fixture.state.pointer_root);
    let bytes = resource_request(
        &mut fixture.state,
        &mut fixture.backend,
        &mut fixture.peer,
        84,
        41,
        0,
        &body,
    );
    assert_x11_error(&bytes, x11::error::BAD_VALUE, 0x55, 41, 84);
    assert_eq!(fixture.backend.calls(), backend_before);
    assert_eq!(format!("{:?}", fixture.state.pointer_root), pointer_before);
    assert_eq!(
        expected_resource_checks(&calls.lock().unwrap()),
        [(
            CLIENT,
            fixture.window,
            ResourceKind::Window,
            AccessMode::GET_ATTR,
            Some(RequestOpcodes {
                major: 41,
                minor: 0
            }),
        )]
    );

    let mut no_policy = window_access_fixture();
    let mut body = words(&[invalid_source.0, no_policy.window.0]);
    body.extend_from_slice(&[0; 12]);
    let bytes = resource_request(
        &mut no_policy.state,
        &mut no_policy.backend,
        &mut no_policy.peer,
        85,
        41,
        0,
        &body,
    );
    assert_x11_error(&bytes, x11::error::BAD_WINDOW, invalid_source.0, 41, 85);
}

#[test]
fn reparent_access_checks_precede_later_operand_validation() {
    // Mutation: keep ReparentWindow checks behind check_reparent_window; MANAGE and ADD denials
    // must beat later BadWindow/BadMatch.
    let mut fixture = window_access_fixture();
    let invalid_parent = ResourceId(0x0054_00fd);
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .state
        .install_access_policy(Arc::new(window_resource_policy(
            Arc::clone(&calls),
            Some(Arc::new(AtomicU32::new(fixture.window.0))),
            Some(AccessMode::MANAGE),
            false,
        )));
    let backend_before = fixture.backend.calls().to_vec();
    let window_before = format!("{:?}", fixture.state.resources.window(fixture.window));
    let old_parent = fixture
        .state
        .resources
        .window(fixture.window)
        .expect("fixture window")
        .parent;
    let old_parent_children = fixture.state.resources.children(old_parent).to_vec();
    let bytes = resource_request(
        &mut fixture.state,
        &mut fixture.backend,
        &mut fixture.peer,
        86,
        7,
        0,
        &words(&[fixture.window.0, invalid_parent.0, 0]),
    );
    assert_x11_error(&bytes, x11::error::BAD_VALUE, 0x55, 7, 86);
    assert_eq!(fixture.backend.calls(), backend_before);
    assert_eq!(
        format!("{:?}", fixture.state.resources.window(fixture.window)),
        window_before
    );
    assert_eq!(
        fixture.state.resources.children(old_parent),
        old_parent_children
    );
    assert_eq!(
        expected_resource_checks(&calls.lock().unwrap()),
        [(
            CLIENT,
            fixture.window,
            ResourceKind::Window,
            AccessMode::MANAGE,
            Some(RequestOpcodes { major: 7, minor: 0 }),
        )]
    );

    let mut no_policy = window_access_fixture();
    let bytes = resource_request(
        &mut no_policy.state,
        &mut no_policy.backend,
        &mut no_policy.peer,
        87,
        7,
        0,
        &words(&[no_policy.window.0, invalid_parent.0, 0]),
    );
    assert_x11_error(&bytes, x11::error::BAD_WINDOW, no_policy.window.0, 7, 87);

    let mut fixture = window_access_fixture();
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .state
        .install_access_policy(Arc::new(window_resource_policy(
            Arc::clone(&calls),
            Some(Arc::new(AtomicU32::new(fixture.window.0))),
            Some(AccessMode::ADD),
            false,
        )));
    let backend_before = fixture.backend.calls().to_vec();
    let window_before = format!("{:?}", fixture.state.resources.window(fixture.window));
    let old_parent = fixture
        .state
        .resources
        .window(fixture.window)
        .expect("fixture window")
        .parent;
    let old_parent_children = fixture.state.resources.children(old_parent).to_vec();
    let target_children = fixture.state.resources.children(fixture.window).to_vec();
    let bytes = resource_request(
        &mut fixture.state,
        &mut fixture.backend,
        &mut fixture.peer,
        88,
        7,
        0,
        &words(&[fixture.window.0, fixture.window.0, 0]),
    );
    assert_x11_error(&bytes, x11::error::BAD_VALUE, 0x55, 7, 88);
    assert_eq!(fixture.backend.calls(), backend_before);
    assert_eq!(
        format!("{:?}", fixture.state.resources.window(fixture.window)),
        window_before
    );
    assert_eq!(
        fixture.state.resources.children(old_parent),
        old_parent_children
    );
    assert_eq!(
        fixture.state.resources.children(fixture.window),
        target_children
    );
    assert_eq!(
        expected_resource_checks(&calls.lock().unwrap()),
        [
            (
                CLIENT,
                fixture.window,
                ResourceKind::Window,
                AccessMode::MANAGE,
                Some(RequestOpcodes { major: 7, minor: 0 }),
            ),
            (
                CLIENT,
                fixture.window,
                ResourceKind::Window,
                AccessMode::ADD,
                Some(RequestOpcodes { major: 7, minor: 0 }),
            ),
        ]
    );

    let mut no_policy = window_access_fixture();
    let bytes = resource_request(
        &mut no_policy.state,
        &mut no_policy.backend,
        &mut no_policy.peer,
        89,
        7,
        0,
        &words(&[no_policy.window.0, no_policy.window.0, 0]),
    );
    assert_x11_error(&bytes, x11::error::BAD_MATCH, no_policy.window.0, 7, 89);
}

#[test]
fn circulate_core_policy_context_uses_zero_minor() {
    // Mutation: use header.data as the minor for a core request; direction 1 must still reach
    // policy as minor 0 and the error packet must encode 0.
    let mut fixture = window_access_fixture();
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .state
        .install_access_policy(Arc::new(window_resource_policy(
            Arc::clone(&calls),
            Some(Arc::new(AtomicU32::new(fixture.subwindows_parent.0))),
            Some(AccessMode::MANAGE),
            false,
        )));
    let backend_before = fixture.backend.calls().to_vec();
    let children_before = fixture
        .state
        .resources
        .children(fixture.subwindows_parent)
        .to_vec();
    let bytes = resource_request(
        &mut fixture.state,
        &mut fixture.backend,
        &mut fixture.peer,
        90,
        13,
        1,
        &words(&[fixture.subwindows_parent.0]),
    );
    assert_x11_error(&bytes, x11::error::BAD_VALUE, 0x55, 13, 90);
    assert_eq!(fixture.backend.calls(), backend_before);
    assert_eq!(
        fixture.state.resources.children(fixture.subwindows_parent),
        children_before
    );
    assert_eq!(
        expected_resource_checks(&calls.lock().unwrap()),
        [(
            CLIENT,
            fixture.subwindows_parent,
            ResourceKind::Window,
            AccessMode::MANAGE,
            Some(RequestOpcodes {
                major: 13,
                minor: 0
            }),
        )]
    );
}
