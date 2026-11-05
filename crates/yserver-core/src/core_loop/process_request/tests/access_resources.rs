use super::{
    extensions::{ExtensionHook, RecordedExtensionCheck, RecordingExtensionPolicy},
    *,
};
use crate::access::{
    AccessContext, AccessMode, AccessPolicy, Decision, DenyError, RequestOpcodes, ResourceKind,
    ResourceTarget,
};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

const CLIENT: ClientId = ClientId(1);
const REQUEST: RequestOpcodes = RequestOpcodes { major: 1, minor: 0 };

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

fn resource_request(
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

fn words(words: &[u32]) -> Vec<u8> {
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
    // Mutation: make the recording renderer return no glyphset; this row's owner assertion must fail.
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
    state.install_access_policy(Arc::new(RecordingExtensionPolicy {
        decision: Decision::Allow,
        calls: Arc::clone(&calls),
    }));

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
    // Mutation: resolve owners only while their client is connected; retained resources still belong to client 1.
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
    state.install_access_policy(Arc::new(RecordingExtensionPolicy {
        decision: Decision::Allow,
        calls: Arc::clone(&calls),
    }));
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
        state.install_access_policy(Arc::new(RecordingExtensionPolicy {
            decision: Decision::Allow,
            calls: Arc::clone(&calls),
        }));
    }

    let colormap = ResourceId(0x0052_0001);
    let mut create_body = Vec::new();
    create_body.extend_from_slice(&colormap.0.to_le_bytes());
    create_body.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
    create_body.extend_from_slice(&crate::resources::ROOT_VISUAL.0.to_le_bytes());
    dispatch_wire_request(&mut state, &mut peer, 1, 78, 0, &create_body);
    assert!(state.resources.colormap(colormap).is_some());
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
    // Mutation: resolve KillClient through the typed owner resolver; the colormap must stop returning BadValue.
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
    state.install_access_policy(Arc::new(RecordingExtensionPolicy {
        decision: Decision::Deny(DenyError {
            code: yserver_protocol::x11::error::BAD_VALUE,
            value: 0x55,
        }),
        calls: Arc::clone(&calls),
    }));
    let server = crate::resources::SERVER_OWNER;
    assert_eq!(
        state.check_resource(
            server,
            &ResourceTarget {
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
    // Mutations: deny the no-policy resource or property check; the matching Allow assertion must fail.
    let state = ServerState::new();
    assert!(!state.policies_installed());
    assert_eq!(
        state.check_resource(
            CLIENT,
            &ResourceTarget {
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

struct DrawingResourcePolicy {
    label: &'static str,
    calls: Arc<Mutex<Vec<RecordedExtensionCheck>>>,
    deny_id: Option<Arc<AtomicU32>>,
    deny_all: bool,
    decision: Decision,
}

impl AccessPolicy for DrawingResourcePolicy {
    fn resource(
        &self,
        ctx: &AccessContext<'_>,
        client: ClientId,
        target: &ResourceTarget,
        mode: AccessMode,
    ) -> Decision {
        self.calls.lock().unwrap().push(RecordedExtensionCheck {
            hook: ExtensionHook::Resource,
            client,
            extension: self.label.to_owned(),
            request: ctx.request(),
            peer: None,
            admission: None,
            owner: ctx.owner(target.id, target.kind),
            target: Some(*target),
            property: None,
            mode: Some(mode),
        });
        if self.deny_all
            || self
                .deny_id
                .as_ref()
                .is_some_and(|deny_id| deny_id.load(Ordering::SeqCst) == target.id.0)
        {
            self.decision
        } else {
            Decision::Allow
        }
    }
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

fn drawing_request_cases(resources: &DrawingResources) -> Vec<DrawingRequestCase> {
    let drawable_gc = |opcode, body: Vec<u8>, data| DrawingRequestCase {
        opcode,
        data,
        body,
        targets: vec![
            (resources.src, ResourceKind::Pixmap, AccessMode::WRITE),
            (resources.gc, ResourceKind::Gc, AccessMode::USE),
        ],
    };
    let mut image = words(&[resources.src.0, resources.gc.0, 1 | (1 << 16), 0]);
    image.extend_from_slice(&[0, 24, 0, 0, 0, 0, 0, 0]);
    vec![
        DrawingRequestCase {
            opcode: 61,
            data: 0,
            body: words(&[resources.window.0, 0, 1 | (1 << 16)]),
            targets: vec![(resources.window, ResourceKind::Window, AccessMode::WRITE)],
        },
        DrawingRequestCase {
            opcode: 62,
            data: 0,
            body: words(&[
                resources.src.0,
                resources.dst.0,
                resources.gc.0,
                0,
                0,
                1 | (1 << 16),
            ]),
            targets: vec![
                (resources.src, ResourceKind::Pixmap, AccessMode::READ),
                (resources.dst, ResourceKind::Pixmap, AccessMode::WRITE),
                (resources.gc, ResourceKind::Gc, AccessMode::USE),
            ],
        },
        DrawingRequestCase {
            opcode: 63,
            data: 0,
            body: words(&[
                resources.src.0,
                resources.dst.0,
                resources.gc.0,
                0,
                0,
                1 | (1 << 16),
                1,
            ]),
            targets: vec![
                (resources.src, ResourceKind::Pixmap, AccessMode::READ),
                (resources.dst, ResourceKind::Pixmap, AccessMode::WRITE),
                (resources.gc, ResourceKind::Gc, AccessMode::USE),
            ],
        },
        drawable_gc(64, words(&[resources.src.0, resources.gc.0, 0]), 0),
        drawable_gc(65, words(&[resources.src.0, resources.gc.0, 0]), 0),
        drawable_gc(66, words(&[resources.src.0, resources.gc.0, 0, 0, 0, 0]), 0),
        drawable_gc(
            67,
            words(&[resources.src.0, resources.gc.0, 0, 1 | (1 << 16)]),
            0,
        ),
        drawable_gc(
            68,
            words(&[resources.src.0, resources.gc.0, 0, 1 | (1 << 16), 0]),
            0,
        ),
        drawable_gc(69, words(&[resources.src.0, resources.gc.0, 0]), 0),
        drawable_gc(
            70,
            words(&[resources.src.0, resources.gc.0, 0, 1 | (1 << 16)]),
            0,
        ),
        drawable_gc(
            71,
            words(&[resources.src.0, resources.gc.0, 0, 1 | (1 << 16), 0]),
            0,
        ),
        drawable_gc(72, image, 2),
        DrawingRequestCase {
            opcode: 73,
            data: 2,
            body: words(&[resources.src.0, 0, 1 | (1 << 16), u32::MAX]),
            targets: vec![(resources.src, ResourceKind::Pixmap, AccessMode::READ)],
        },
        drawable_gc(74, words(&[resources.src.0, resources.gc.0, 0]), 0),
        drawable_gc(75, words(&[resources.src.0, resources.gc.0, 0]), 0),
        drawable_gc(76, words(&[resources.src.0, resources.gc.0, 0]), 0),
        drawable_gc(77, words(&[resources.src.0, resources.gc.0, 0]), 0),
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

fn assert_x11_error(bytes: &[u8], code: u8, value: u32, opcode: u8, sequence: u16) {
    let mut expected = vec![0, code];
    expected.extend_from_slice(&sequence.to_le_bytes());
    expected.extend_from_slice(&value.to_le_bytes());
    expected.extend_from_slice(&0u16.to_le_bytes());
    expected.push(opcode);
    expected.extend_from_slice(&[0; 21]);
    assert_eq!(bytes, expected, "request {opcode} denial packet");
}

fn assert_policy_error(bytes: &[u8], opcode: u8, sequence: u16) {
    assert_x11_error(bytes, x11::error::BAD_ACCESS, 0x55, opcode, sequence);
}

fn expected_resource_checks(
    calls: &[RecordedExtensionCheck],
) -> Vec<(
    ClientId,
    ResourceId,
    ResourceKind,
    AccessMode,
    Option<RequestOpcodes>,
)> {
    calls
        .iter()
        .map(|call| {
            let target = call.target.expect("resource target");
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

#[test]
fn every_drawing_request_denial_reports_policy_error() {
    // Mutation: remove any drawing target check, including CopyArea/CopyPlane destination; that target must stop returning the policy error.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, CLIENT.0);
    let mut backend = RecordingBackend::new();
    let resources = drawing_resources(&mut state, &mut backend, &mut peer);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let denied_id = Arc::new(AtomicU32::new(0));
    state.install_access_policy(Arc::new(DrawingResourcePolicy {
        label: "drawing deny",
        calls: Arc::clone(&calls),
        deny_id: Some(Arc::clone(&denied_id)),
        deny_all: false,
        decision: Decision::Deny(DenyError {
            code: x11::error::BAD_ACCESS,
            value: 0x55,
        }),
    }));

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
    // Mutation: move drawable authorization after GC lookup; the policy denial must still win over BadGC.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, CLIENT.0);
    let mut backend = RecordingBackend::new();
    let resources = drawing_resources(&mut state, &mut backend, &mut peer);
    let calls = Arc::new(Mutex::new(Vec::new()));
    state.install_access_policy(Arc::new(DrawingResourcePolicy {
        label: "drawable precedence",
        calls: Arc::clone(&calls),
        deny_id: Some(Arc::new(AtomicU32::new(resources.src.0))),
        deny_all: false,
        decision: Decision::Deny(DenyError {
            code: x11::error::BAD_ACCESS,
            value: 0x55,
        }),
    }));
    let missing_gc = ResourceId(0x0053_00ff);
    let request = DrawingRequestCase {
        opcode: 64,
        data: 0,
        body: words(&[resources.src.0, missing_gc.0, 0]),
        targets: Vec::new(),
    };
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
    // Mutation: skip missing-GC validation after an allowed drawable; BadGC with the GC id must remain observable.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, CLIENT.0);
    let mut backend = RecordingBackend::new();
    let resources = unbacked_drawing_resources(&mut state);
    let calls = Arc::new(Mutex::new(Vec::new()));
    state.install_access_policy(Arc::new(DrawingResourcePolicy {
        label: "drawable allow",
        calls: Arc::clone(&calls),
        deny_id: Some(Arc::new(AtomicU32::new(0))),
        deny_all: false,
        decision: Decision::Deny(DenyError {
            code: x11::error::BAD_ACCESS,
            value: 0x55,
        }),
    }));
    let missing_gc = ResourceId(0x0053_00ff);
    let request = DrawingRequestCase {
        opcode: 64,
        data: 0,
        body: words(&[resources.src.0, missing_gc.0, 0]),
        targets: Vec::new(),
    };
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
    state.install_access_policy(Arc::new(DrawingResourcePolicy {
        label: "GC precedence",
        calls: Arc::clone(&calls),
        deny_id: Some(Arc::new(AtomicU32::new(resources.gc.0))),
        deny_all: false,
        decision: Decision::Deny(DenyError {
            code: x11::error::BAD_ACCESS,
            value: 0x55,
        }),
    }));
    let request = DrawingRequestCase {
        opcode: 64,
        data: 0,
        body: words(&[narrow_pixmap.0, resources.gc.0, 0]),
        targets: Vec::new(),
    };
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
    // Mutation: pass the wrong drawable, GC, kind, mode, or request context; the literal target list must fail.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, CLIENT.0);
    let mut backend = RecordingBackend::new();
    let resources = unbacked_drawing_resources(&mut state);
    let calls = Arc::new(Mutex::new(Vec::new()));
    state.install_access_policy(Arc::new(DrawingResourcePolicy {
        label: "drawing recorder",
        calls: Arc::clone(&calls),
        deny_id: None,
        deny_all: false,
        decision: Decision::Allow,
    }));

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
                        minor: request.data,
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
    // Mutation: move a check after a GC mutation or QueryBestSize reply; the exact denial and unchanged GC state must fail.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, CLIENT.0);
    let mut backend = RecordingBackend::new();
    let resources = drawing_resources(&mut state, &mut backend, &mut peer);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let denied_id = Arc::new(AtomicU32::new(0));
    state.install_access_policy(Arc::new(DrawingResourcePolicy {
        label: "GC and size denial",
        calls: Arc::clone(&calls),
        deny_id: Some(Arc::clone(&denied_id)),
        deny_all: false,
        decision: Decision::Deny(DenyError {
            code: x11::error::BAD_ACCESS,
            value: 0x55,
        }),
    }));
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
        let request = DrawingRequestCase {
            opcode,
            data: 0,
            body,
            targets: Vec::new(),
        };
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
    // Mutation: check inline fonts while drawing; a denied second font must prevent the draw and GC font update.
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
    state.install_access_policy(Arc::new(DrawingResourcePolicy {
        label: "second inline font deny",
        calls,
        deny_id: Some(Arc::new(AtomicU32::new(font_ids[1].0))),
        deny_all: false,
        decision: Decision::Deny(DenyError {
            code: x11::error::BAD_ACCESS,
            value: 0x55,
        }),
    }));
    let mut body = words(&[resources.src.0, resources.gc.0, 0]);
    body.push(255);
    body.extend_from_slice(&font_ids[0].0.to_be_bytes());
    body.extend_from_slice(&[3, 0, b'o', b'n', b'e']);
    body.push(255);
    body.extend_from_slice(&font_ids[1].0.to_be_bytes());
    body.extend_from_slice(&[3, 0, b't', b'w', b'o']);
    body.resize(body.len().div_ceil(4) * 4, 0);
    let before = backend.calls();
    let request = DrawingRequestCase {
        opcode: 74,
        data: 0,
        body,
        targets: Vec::new(),
    };
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
    // Mutation: continue asking policies after a denial or return a later policy's error; first-denier order must hold.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, CLIENT.0);
    let mut backend = RecordingBackend::new();
    let resources = unbacked_drawing_resources(&mut state);
    let request = DrawingRequestCase {
        opcode: 64,
        data: 0,
        body: words(&[resources.src.0, resources.gc.0, 0]),
        targets: Vec::new(),
    };

    let calls = Arc::new(Mutex::new(Vec::new()));
    state.install_access_policy(Arc::new(DrawingResourcePolicy {
        label: "allow before deny",
        calls: Arc::clone(&calls),
        deny_id: None,
        deny_all: false,
        decision: Decision::Allow,
    }));
    state.install_access_policy(Arc::new(DrawingResourcePolicy {
        label: "first denier",
        calls: Arc::clone(&calls),
        deny_id: None,
        deny_all: true,
        decision: Decision::Deny(DenyError {
            code: x11::error::BAD_ACCESS,
            value: 0x55,
        }),
    }));
    state.install_access_policy(Arc::new(DrawingResourcePolicy {
        label: "must not run after denial",
        calls: Arc::clone(&calls),
        deny_id: None,
        deny_all: true,
        decision: Decision::Deny(DenyError {
            code: x11::error::BAD_VALUE,
            value: 0x66,
        }),
    }));
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
    let second_request = DrawingRequestCase {
        opcode: 64,
        data: 0,
        body: words(&[second_resources.src.0, second_resources.gc.0, 0]),
        targets: Vec::new(),
    };
    let second_calls = Arc::new(Mutex::new(Vec::new()));
    second_state.install_access_policy(Arc::new(DrawingResourcePolicy {
        label: "denier first",
        calls: Arc::clone(&second_calls),
        deny_id: None,
        deny_all: true,
        decision: Decision::Deny(DenyError {
            code: x11::error::BAD_VALUE,
            value: 0x66,
        }),
    }));
    second_state.install_access_policy(Arc::new(DrawingResourcePolicy {
        label: "allow after denial",
        calls: Arc::clone(&second_calls),
        deny_id: None,
        deny_all: false,
        decision: Decision::Allow,
    }));
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

struct RevocableDrawingPolicy {
    revoked: Arc<AtomicBool>,
    calls: Arc<Mutex<Vec<RecordedExtensionCheck>>>,
}

impl AccessPolicy for RevocableDrawingPolicy {
    fn resource(
        &self,
        ctx: &AccessContext<'_>,
        client: ClientId,
        target: &ResourceTarget,
        mode: AccessMode,
    ) -> Decision {
        self.calls.lock().unwrap().push(RecordedExtensionCheck {
            hook: ExtensionHook::Resource,
            client,
            extension: "revocable drawing".to_owned(),
            request: ctx.request(),
            peer: None,
            admission: None,
            owner: ctx.owner(target.id, target.kind),
            target: Some(*target),
            property: None,
            mode: Some(mode),
        });
        if self.revoked.load(Ordering::SeqCst) {
            Decision::Deny(DenyError {
                code: x11::error::BAD_ACCESS,
                value: 0x55,
            })
        } else {
            Decision::Allow
        }
    }
}

#[test]
fn revoked_drawable_denies_the_next_request() {
    // Mutation: cache an earlier Allow; revoking the same drawable must affect its next PolyPoint.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, CLIENT.0);
    let mut backend = RecordingBackend::new();
    let resources = unbacked_drawing_resources(&mut state);
    let request = DrawingRequestCase {
        opcode: 64,
        data: 0,
        body: words(&[resources.src.0, resources.gc.0, 0]),
        targets: Vec::new(),
    };
    let revoked = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(Mutex::new(Vec::new()));
    state.install_access_policy(Arc::new(RevocableDrawingPolicy {
        revoked: Arc::clone(&revoked),
        calls: Arc::clone(&calls),
    }));
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
    // Mutation: remove the SERVER_OWNER fast path; server work through PolyPoint must remain unchecked.
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, crate::resources::SERVER_OWNER.0);
    let resources = unbacked_drawing_resources(&mut state);
    let calls = Arc::new(Mutex::new(Vec::new()));
    state.install_access_policy(Arc::new(DrawingResourcePolicy {
        label: "deny server work",
        calls: Arc::clone(&calls),
        deny_id: None,
        deny_all: true,
        decision: Decision::Deny(DenyError {
            code: x11::error::BAD_ACCESS,
            value: 0x55,
        }),
    }));
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
