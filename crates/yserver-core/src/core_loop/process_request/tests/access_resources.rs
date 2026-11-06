use super::{
    extensions::{ExtensionHook, RecordedExtensionCheck, RecordingExtensionPolicy},
    *,
};
use crate::access::{
    AccessContext, AccessMode, AccessPolicy, Creation, Decision, DenyError, RequestOpcodes,
    ResourceKind, ResourceTarget,
};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

const CLIENT: ClientId = ClientId(1);
const REQUEST: RequestOpcodes = RequestOpcodes { major: 1, minor: 0 };
type ExpectedResourceAccess = (ResourceId, ResourceKind, AccessMode);
type WindowAccessCase = (u8, u8, Vec<u8>, Vec<ExpectedResourceAccess>);

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
    // Mutations: deny the no-policy resource or property check; the matching Allow assertion must fail.
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
            target_owner: ctx.target_owner(target),
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

#[test]
fn get_image_checks_read_before_bounds_validation() {
    // Mutation: move GetImage's READ check after rectangle/viewability validation; the denial must win over BadMatch.
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
    state.install_access_policy(Arc::new(DrawingResourcePolicy {
        label: "GetImage bounds order",
        calls: Arc::clone(&calls),
        deny_id: Some(Arc::new(AtomicU32::new(DRAWABLE.0))),
        deny_all: false,
        decision: Decision::Deny(DenyError {
            code: x11::error::BAD_ACCESS,
            value: 0x55,
        }),
    }));
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

#[test]
fn set_dashes_checks_gc_before_zero_dash_validation() {
    // Mutation: move SetDashes' SET_ATTR check after zero-dash validation; the policy denial must win over BadValue.
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
        .install_access_policy(Arc::new(DrawingResourcePolicy {
            label: "SetDashes zero value order",
            calls: Arc::clone(&calls),
            deny_id: Some(Arc::new(AtomicU32::new(fixture.gc.0))),
            deny_all: false,
            decision: Decision::Deny(DenyError {
                code: x11::error::BAD_ACCESS,
                value: 0x55,
            }),
        }));
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
            target_owner: ctx.target_owner(target),
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

struct WindowResourcePolicy {
    calls: Arc<Mutex<Vec<RecordedExtensionCheck>>>,
    deny_id: Option<Arc<AtomicU32>>,
    deny_mode: Option<AccessMode>,
    deny_hides: bool,
}

impl AccessPolicy for WindowResourcePolicy {
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
            extension: "window resources".to_owned(),
            request: ctx.request(),
            peer: None,
            admission: None,
            owner: ctx.owner(target.id, target.kind),
            target_owner: ctx.target_owner(target),
            target: Some(*target),
            property: None,
            mode: Some(mode),
        });
        let deny_id = self
            .deny_id
            .as_ref()
            .is_some_and(|deny_id| deny_id.load(Ordering::SeqCst) == target.id.0);
        if self.deny_hides && mode == AccessMode::HIDE
            || deny_id && self.deny_mode.is_none_or(|deny_mode| deny_mode == mode)
        {
            Decision::Deny(DenyError {
                code: x11::error::BAD_VALUE,
                value: 0x55,
            })
        } else {
            Decision::Allow
        }
    }
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
    target: ResourceId,
    kind: ResourceKind,
    mode: AccessMode,
    creation: Option<Creation>,
}

#[test]
fn window_resource_request_denials_match_modes_and_stop_before_effects() {
    // Mutation: remove a listed check or use the wrong CWA mode; its row's error/mode assertion must fail.
    // Mutation: authorize an unresolved ChangeSaveSet window; it must keep its current no-check success behavior.
    // Mutation: apply the CWA event-mask selection before checking CW operands; the event-mask snapshot must fail.
    let mut fixture = window_access_fixture();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let denied_id = Arc::new(AtomicU32::new(0));
    fixture
        .state
        .install_access_policy(Arc::new(WindowResourcePolicy {
            calls: Arc::clone(&calls),
            deny_id: Some(Arc::clone(&denied_id)),
            deny_mode: None,
            deny_hides: false,
        }));
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
    let cases = vec![
        WindowDenyCase {
            label: "CreateWindow parent ADD",
            opcode: 1,
            data: 24,
            body: create_body.clone(),
            target: ROOT_WINDOW,
            kind: ResourceKind::Window,
            mode: AccessMode::ADD,
            creation: None,
        },
        WindowDenyCase {
            label: "CreateWindow background READ",
            opcode: 1,
            data: 24,
            body: create_body.clone(),
            target: fixture.background,
            kind: ResourceKind::Pixmap,
            mode: AccessMode::READ,
            creation: None,
        },
        WindowDenyCase {
            label: "CreateWindow colormap USE",
            opcode: 1,
            data: 24,
            body: create_body.clone(),
            target: fixture.colormap,
            kind: ResourceKind::Colormap,
            mode: AccessMode::USE,
            creation: None,
        },
        WindowDenyCase {
            label: "CreateWindow cursor USE",
            opcode: 1,
            data: 24,
            body: create_body.clone(),
            target: fixture.cursor,
            kind: ResourceKind::Cursor,
            mode: AccessMode::USE,
            creation: None,
        },
        WindowDenyCase {
            label: "CreateWindow border pixmap READ",
            opcode: 1,
            data: 24,
            body: create_body.clone(),
            target: fixture.border,
            kind: ResourceKind::Pixmap,
            mode: AccessMode::READ,
            creation: None,
        },
        WindowDenyCase {
            label: "CreateWindow creation",
            opcode: 1,
            data: 24,
            body: create_body,
            target: created,
            kind: ResourceKind::Window,
            mode: AccessMode::CREATE | AccessMode::SET_ATTR,
            creation: Some(Creation {
                parent: Some(ROOT_WINDOW),
                owner: CLIENT,
            }),
        },
        WindowDenyCase {
            label: "ChangeWindowAttributes window",
            opcode: 2,
            data: 0,
            body: attrs_body.clone(),
            target: fixture.window,
            kind: ResourceKind::Window,
            mode: AccessMode::RECEIVE | AccessMode::SET_ATTR,
            creation: None,
        },
        WindowDenyCase {
            label: "ChangeWindowAttributes event mask only",
            opcode: 2,
            data: 0,
            body: words(&[fixture.window.0, 0x0800, 1]),
            target: fixture.window,
            kind: ResourceKind::Window,
            mode: AccessMode::RECEIVE,
            creation: None,
        },
        WindowDenyCase {
            label: "ChangeWindowAttributes non-event attribute only",
            opcode: 2,
            data: 0,
            body: words(&[fixture.window.0, 0x0200, 1]),
            target: fixture.window,
            kind: ResourceKind::Window,
            mode: AccessMode::SET_ATTR,
            creation: None,
        },
        WindowDenyCase {
            label: "ChangeWindowAttributes background READ",
            opcode: 2,
            data: 0,
            body: attrs_body.clone(),
            target: fixture.background,
            kind: ResourceKind::Pixmap,
            mode: AccessMode::READ,
            creation: None,
        },
        WindowDenyCase {
            label: "ChangeWindowAttributes colormap USE",
            opcode: 2,
            data: 0,
            body: attrs_body.clone(),
            target: fixture.colormap,
            kind: ResourceKind::Colormap,
            mode: AccessMode::USE,
            creation: None,
        },
        WindowDenyCase {
            label: "ChangeWindowAttributes cursor USE",
            opcode: 2,
            data: 0,
            body: attrs_body.clone(),
            target: fixture.cursor,
            kind: ResourceKind::Cursor,
            mode: AccessMode::USE,
            creation: None,
        },
        WindowDenyCase {
            label: "ChangeWindowAttributes border pixmap READ",
            opcode: 2,
            data: 0,
            body: attrs_body,
            target: fixture.border,
            kind: ResourceKind::Pixmap,
            mode: AccessMode::READ,
            creation: None,
        },
        WindowDenyCase {
            label: "GetWindowAttributes",
            opcode: 3,
            data: 0,
            body: words(&[fixture.window.0]),
            target: fixture.window,
            kind: ResourceKind::Window,
            mode: AccessMode::GET_ATTR,
            creation: None,
        },
        WindowDenyCase {
            label: "DestroyWindow target",
            opcode: 4,
            data: 0,
            body: words(&[fixture.destroy_window.0]),
            target: fixture.destroy_window,
            kind: ResourceKind::Window,
            mode: AccessMode::DESTROY,
            creation: None,
        },
        WindowDenyCase {
            label: "DestroyWindow parent",
            opcode: 4,
            data: 0,
            body: words(&[fixture.destroy_window.0]),
            target: fixture.destroy_parent,
            kind: ResourceKind::Window,
            mode: AccessMode::REMOVE,
            creation: None,
        },
        WindowDenyCase {
            label: "DestroySubwindows parent",
            opcode: 5,
            data: 0,
            body: words(&[fixture.parent.0]),
            target: fixture.parent,
            kind: ResourceKind::Window,
            mode: AccessMode::REMOVE,
            creation: None,
        },
        WindowDenyCase {
            label: "ChangeSaveSet",
            opcode: 6,
            data: 0,
            body: words(&[fixture.window.0]),
            target: fixture.window,
            kind: ResourceKind::Window,
            mode: AccessMode::MANAGE,
            creation: None,
        },
    ];

    let mut sequence = 50;
    for case in cases {
        denied_id.store(case.target.0, Ordering::SeqCst);
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
        assert_eq!(
            call.target,
            Some(ResourceTarget {
                owner: None,
                id: case.target,
                kind: case.kind,
                creation: case.creation,
            }),
            "{} target",
            case.label
        );
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

const COLORMAP: ResourceId = ResourceId(0x0030_0001);
const NEW_COLORMAP: ResourceId = ResourceId(0x0030_0002);

struct ColormapAccessCase {
    label: &'static str,
    opcode: u8,
    data: u8,
    body: Vec<u8>,
    checks: Vec<(ResourceTarget, AccessMode)>,
    installed_colormaps: Option<Vec<ResourceId>>,
}

fn colormap_target(id: ResourceId, creation: Option<Creation>) -> ResourceTarget {
    ResourceTarget {
        owner: None,
        id,
        kind: ResourceKind::Colormap,
        creation,
    }
}

fn colormap_access_cases() -> Vec<ColormapAccessCase> {
    let window = ResourceTarget {
        owner: None,
        id: ROOT_WINDOW,
        kind: ResourceKind::Window,
        creation: None,
    };
    let colormap = colormap_target(COLORMAP, None);
    let created = colormap_target(
        NEW_COLORMAP,
        Some(Creation {
            parent: None,
            owner: CLIENT,
        }),
    );
    let child_colormap = colormap_target(
        NEW_COLORMAP,
        Some(Creation {
            parent: Some(ROOT_WINDOW),
            owner: CLIENT,
        }),
    );
    let mut alloc_color = COLORMAP.0.to_le_bytes().to_vec();
    alloc_color.extend_from_slice(&[0; 8]);
    let alloc_named_color = named_color_body(COLORMAP, b"red");
    let mut alloc_color_cells = COLORMAP.0.to_le_bytes().to_vec();
    alloc_color_cells.extend_from_slice(&[1, 0, 0, 0]);
    let mut alloc_color_planes = COLORMAP.0.to_le_bytes().to_vec();
    alloc_color_planes.extend_from_slice(&[1, 0, 1, 0, 1, 0, 1, 0]);
    let mut free_colors = COLORMAP.0.to_le_bytes().to_vec();
    free_colors.extend_from_slice(&[0; 4]);
    let mut store_named_color = COLORMAP.0.to_le_bytes().to_vec();
    store_named_color.extend_from_slice(&[0; 4]);
    store_named_color.extend_from_slice(&[3, 0, 0, 0]);
    store_named_color.extend_from_slice(b"red\0");
    let mut query_colors = COLORMAP.0.to_le_bytes().to_vec();
    query_colors.extend_from_slice(&[0; 4]);

    vec![
        ColormapAccessCase {
            label: "CreateColormap",
            opcode: 78,
            data: 0,
            body: words(&[
                NEW_COLORMAP.0,
                ROOT_WINDOW.0,
                crate::resources::ROOT_VISUAL.0,
            ]),
            checks: vec![
                (window, AccessMode::GET_ATTR),
                (child_colormap, AccessMode::CREATE),
            ],
            installed_colormaps: None,
        },
        ColormapAccessCase {
            label: "FreeColormap",
            opcode: 79,
            data: 0,
            body: words(&[COLORMAP.0]),
            checks: vec![(colormap, AccessMode::DESTROY)],
            installed_colormaps: None,
        },
        ColormapAccessCase {
            label: "CopyColormapAndFree",
            opcode: 80,
            data: 0,
            body: words(&[NEW_COLORMAP.0, COLORMAP.0]),
            checks: vec![
                (colormap, AccessMode::READ | AccessMode::REMOVE),
                (created, AccessMode::CREATE),
            ],
            installed_colormaps: None,
        },
        ColormapAccessCase {
            label: "InstallColormap",
            opcode: 81,
            data: 0,
            body: words(&[COLORMAP.0]),
            checks: vec![(colormap, AccessMode::INSTALL)],
            installed_colormaps: None,
        },
        ColormapAccessCase {
            label: "UninstallColormap",
            opcode: 82,
            data: 0,
            body: words(&[COLORMAP.0]),
            checks: vec![(colormap, AccessMode::UNINSTALL)],
            installed_colormaps: Some(vec![COLORMAP]),
        },
        ColormapAccessCase {
            label: "ListInstalledColormaps",
            opcode: 83,
            data: 0,
            body: words(&[ROOT_WINDOW.0]),
            checks: vec![(window, AccessMode::GET_ATTR)],
            installed_colormaps: None,
        },
        ColormapAccessCase {
            label: "AllocColor",
            opcode: 84,
            data: 0,
            body: alloc_color,
            checks: vec![(colormap, AccessMode::ADD)],
            installed_colormaps: None,
        },
        ColormapAccessCase {
            label: "AllocNamedColor",
            opcode: 85,
            data: 0,
            body: alloc_named_color,
            checks: vec![(colormap, AccessMode::ADD)],
            installed_colormaps: None,
        },
        ColormapAccessCase {
            label: "AllocColorCells",
            opcode: 86,
            data: 0,
            body: alloc_color_cells,
            checks: vec![(colormap, AccessMode::ADD)],
            installed_colormaps: None,
        },
        ColormapAccessCase {
            label: "AllocColorPlanes",
            opcode: 87,
            data: 0,
            body: alloc_color_planes,
            checks: vec![(colormap, AccessMode::ADD)],
            installed_colormaps: None,
        },
        ColormapAccessCase {
            label: "FreeColors",
            opcode: 88,
            data: 0,
            body: free_colors,
            checks: vec![(colormap, AccessMode::REMOVE)],
            installed_colormaps: None,
        },
        ColormapAccessCase {
            label: "StoreColors",
            opcode: 89,
            data: 0,
            body: words(&[COLORMAP.0]),
            checks: vec![(colormap, AccessMode::WRITE)],
            installed_colormaps: None,
        },
        ColormapAccessCase {
            label: "StoreNamedColor",
            opcode: 90,
            data: 0,
            body: store_named_color,
            checks: vec![(colormap, AccessMode::WRITE)],
            installed_colormaps: None,
        },
        ColormapAccessCase {
            label: "QueryColors",
            opcode: 91,
            data: 0,
            body: query_colors,
            checks: vec![(colormap, AccessMode::READ)],
            installed_colormaps: None,
        },
        ColormapAccessCase {
            label: "LookupColor",
            opcode: 92,
            data: 0,
            body: named_color_body(COLORMAP, b"red"),
            checks: vec![(colormap, AccessMode::READ)],
            installed_colormaps: None,
        },
    ]
}

fn named_color_body(colormap: ResourceId, name: &[u8]) -> Vec<u8> {
    let mut body = colormap.0.to_le_bytes().to_vec();
    body.extend_from_slice(&u16::try_from(name.len()).unwrap().to_le_bytes());
    body.extend_from_slice(&[0; 2]);
    body.extend_from_slice(name);
    body.resize(body.len().div_ceil(4) * 4, 0);
    body
}

struct ColormapAccessFixture {
    state: ServerState,
    backend: RecordingBackend,
    peer: UnixStream,
}

fn colormap_access_fixture() -> ColormapAccessFixture {
    let mut state = ServerState::new();
    let peer = install_client(&mut state, CLIENT.0);
    state
        .resources
        .create_colormap(CLIENT, COLORMAP, crate::resources::ROOT_VISUAL);
    ColormapAccessFixture {
        state,
        backend: RecordingBackend::new(),
        peer,
    }
}

fn colormap_request(
    fixture: &mut ColormapAccessFixture,
    sequence: u16,
    opcode: u8,
    data: u8,
    body: &[u8],
) -> Vec<u8> {
    process_request(
        &mut fixture.state,
        &mut fixture.backend,
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
    .expect("colormap request through process_request");
    read_all_available(&mut fixture.peer)
}

#[test]
fn colormap_resource_requests_deny_each_target_before_effects() {
    // Mutation: remove FreeColormap's resource check; its denial and retained-colormap assertions must fail.
    for case in colormap_access_cases() {
        for (deny_index, (denied_target, denied_mode)) in case.checks.iter().enumerate() {
            let mut fixture = colormap_access_fixture();
            if let Some(installed) = &case.installed_colormaps {
                fixture.state.installed_colormaps.clone_from(installed);
            }
            let calls = Arc::new(Mutex::new(Vec::new()));
            let denied_id = Arc::new(AtomicU32::new(denied_target.id.0));
            fixture
                .state
                .install_access_policy(Arc::new(WindowResourcePolicy {
                    calls: Arc::clone(&calls),
                    deny_id: Some(Arc::clone(&denied_id)),
                    deny_mode: Some(*denied_mode),
                    deny_hides: false,
                }));

            let colormap_before = fixture
                .state
                .resources
                .colormap(COLORMAP)
                .map(|colormap| format!("{colormap:?}"));
            let new_colormap_before = fixture
                .state
                .resources
                .colormap(NEW_COLORMAP)
                .map(|colormap| format!("{colormap:?}"));
            let installed_before = fixture.state.installed_colormaps.clone();
            let backend_before = fixture.backend.calls();
            let sequence = 200 + u16::try_from(deny_index).unwrap();
            let bytes =
                colormap_request(&mut fixture, sequence, case.opcode, case.data, &case.body);
            assert_x11_error(&bytes, x11::error::BAD_VALUE, 0x55, case.opcode, sequence);
            assert_eq!(
                fixture.backend.calls(),
                backend_before,
                "{} backend",
                case.label
            );
            assert_eq!(
                fixture
                    .state
                    .resources
                    .colormap(COLORMAP)
                    .map(|colormap| format!("{colormap:?}")),
                colormap_before,
                "{} source colormap",
                case.label
            );
            assert_eq!(
                fixture
                    .state
                    .resources
                    .colormap(NEW_COLORMAP)
                    .map(|colormap| format!("{colormap:?}")),
                new_colormap_before,
                "{} new colormap",
                case.label
            );
            assert_eq!(
                fixture.state.installed_colormaps, installed_before,
                "{} installed colormaps",
                case.label
            );

            let recorded = calls.lock().unwrap();
            let expected = &case.checks[..=deny_index];
            assert_eq!(
                recorded.len(),
                expected.len(),
                "{} checked prefix",
                case.label
            );
            for (call, (target, mode)) in recorded.iter().zip(expected) {
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
                assert_eq!(call.target, Some(*target), "{} target", case.label);
                assert_eq!(call.mode, Some(*mode), "{} mode", case.label);
            }
        }
    }
}

#[test]
fn colormap_resource_requests_ask_every_expected_target() {
    // Mutation: change QueryColors from READ to WRITE; its recorded-mode assertion must fail.
    for case in colormap_access_cases() {
        let mut fixture = colormap_access_fixture();
        if let Some(installed) = &case.installed_colormaps {
            fixture.state.installed_colormaps.clone_from(installed);
        }
        let calls = Arc::new(Mutex::new(Vec::new()));
        fixture
            .state
            .install_access_policy(Arc::new(WindowResourcePolicy {
                calls: Arc::clone(&calls),
                deny_id: None,
                deny_mode: None,
                deny_hides: false,
            }));
        let _ = colormap_request(&mut fixture, 240, case.opcode, case.data, &case.body);
        let recorded = calls.lock().unwrap();
        assert_eq!(
            recorded.len(),
            case.checks.len(),
            "{} check count",
            case.label
        );
        for (call, (target, mode)) in recorded.iter().zip(&case.checks) {
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
            assert_eq!(call.target, Some(*target), "{} target", case.label);
            assert_eq!(call.mode, Some(*mode), "{} mode", case.label);
        }
    }
}

#[test]
fn colormap_unconditional_errors_survive_allow_all_policy() {
    // Mutation: skip AllocColorCells' unconditional BadAlloc after Allow; byte equality with the no-policy reply must fail.
    for (opcode, data, body, expected_code) in [
        (
            86,
            0,
            words(&[COLORMAP.0, 1 | (1 << 16)]),
            x11::error::BAD_ALLOC,
        ),
        (
            87,
            0,
            words(&[COLORMAP.0, 1 | (1 << 16), 1 | (1 << 16)]),
            x11::error::BAD_ALLOC,
        ),
        (89, 0, words(&[COLORMAP.0]), x11::error::BAD_ACCESS),
        (
            90,
            0,
            {
                let mut body = COLORMAP.0.to_le_bytes().to_vec();
                body.extend_from_slice(&[0; 4]);
                body.extend_from_slice(&[0; 4]);
                body
            },
            x11::error::BAD_ACCESS,
        ),
    ] {
        let mut baseline = colormap_access_fixture();
        let baseline_bytes = colormap_request(&mut baseline, 250, opcode, data, &body);
        assert_x11_error(&baseline_bytes, expected_code, 0, opcode, 250);

        let mut allowed = colormap_access_fixture();
        let calls = Arc::new(Mutex::new(Vec::new()));
        allowed
            .state
            .install_access_policy(Arc::new(WindowResourcePolicy {
                calls: Arc::clone(&calls),
                deny_id: None,
                deny_mode: None,
                deny_hides: false,
            }));
        let allowed_bytes = colormap_request(&mut allowed, 250, opcode, data, &body);
        assert_eq!(
            allowed_bytes, baseline_bytes,
            "opcode {opcode} allow-all bytes"
        );
        assert_eq!(
            calls.lock().unwrap().len(),
            1,
            "opcode {opcode} access check"
        );
    }
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

    let expected = vec![
        (
            CLIENT,
            ROOT_WINDOW,
            ResourceKind::Window,
            AccessMode::ADD,
            Some(RequestOpcodes { major: 1, minor: 0 }),
        ),
        (
            CLIENT,
            fixture.background,
            ResourceKind::Pixmap,
            AccessMode::READ,
            Some(RequestOpcodes { major: 1, minor: 0 }),
        ),
        (
            CLIENT,
            fixture.colormap,
            ResourceKind::Colormap,
            AccessMode::USE,
            Some(RequestOpcodes { major: 1, minor: 0 }),
        ),
        (
            CLIENT,
            fixture.cursor,
            ResourceKind::Cursor,
            AccessMode::USE,
            Some(RequestOpcodes { major: 1, minor: 0 }),
        ),
        (
            CLIENT,
            fixture.border,
            ResourceKind::Pixmap,
            AccessMode::READ,
            Some(RequestOpcodes { major: 1, minor: 0 }),
        ),
        (
            CLIENT,
            created,
            ResourceKind::Window,
            AccessMode::CREATE | AccessMode::SET_ATTR,
            Some(RequestOpcodes { major: 1, minor: 0 }),
        ),
        (
            CLIENT,
            fixture.window,
            ResourceKind::Window,
            AccessMode::RECEIVE | AccessMode::SET_ATTR,
            Some(RequestOpcodes { major: 2, minor: 0 }),
        ),
        (
            CLIENT,
            fixture.background,
            ResourceKind::Pixmap,
            AccessMode::READ,
            Some(RequestOpcodes { major: 2, minor: 0 }),
        ),
        (
            CLIENT,
            fixture.colormap,
            ResourceKind::Colormap,
            AccessMode::USE,
            Some(RequestOpcodes { major: 2, minor: 0 }),
        ),
        (
            CLIENT,
            fixture.cursor,
            ResourceKind::Cursor,
            AccessMode::USE,
            Some(RequestOpcodes { major: 2, minor: 0 }),
        ),
        (
            CLIENT,
            fixture.border,
            ResourceKind::Pixmap,
            AccessMode::READ,
            Some(RequestOpcodes { major: 2, minor: 0 }),
        ),
        (
            CLIENT,
            fixture.window,
            ResourceKind::Window,
            AccessMode::RECEIVE,
            Some(RequestOpcodes { major: 2, minor: 0 }),
        ),
        (
            CLIENT,
            fixture.window,
            ResourceKind::Window,
            AccessMode::SET_ATTR,
            Some(RequestOpcodes { major: 2, minor: 0 }),
        ),
        (
            CLIENT,
            fixture.window,
            ResourceKind::Window,
            AccessMode::GET_ATTR,
            Some(RequestOpcodes { major: 3, minor: 0 }),
        ),
        (
            CLIENT,
            fixture.destroy_window,
            ResourceKind::Window,
            AccessMode::DESTROY,
            Some(RequestOpcodes { major: 4, minor: 0 }),
        ),
        (
            CLIENT,
            fixture.destroy_parent,
            ResourceKind::Window,
            AccessMode::REMOVE,
            Some(RequestOpcodes { major: 4, minor: 0 }),
        ),
        (
            CLIENT,
            fixture.subwindows_parent,
            ResourceKind::Window,
            AccessMode::REMOVE,
            Some(RequestOpcodes { major: 5, minor: 0 }),
        ),
        (
            CLIENT,
            fixture.subwindow_children[0],
            ResourceKind::Window,
            AccessMode::DESTROY,
            Some(RequestOpcodes { major: 5, minor: 0 }),
        ),
        (
            CLIENT,
            fixture.subwindow_children[1],
            ResourceKind::Window,
            AccessMode::DESTROY,
            Some(RequestOpcodes { major: 5, minor: 0 }),
        ),
        (
            CLIENT,
            fixture.window,
            ResourceKind::Window,
            AccessMode::MANAGE,
            Some(RequestOpcodes { major: 6, minor: 0 }),
        ),
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
    // Mutation: check before the bulk unmap, or continue after the first child denial; each changes these event/state assertions.
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
        .install_access_policy(Arc::new(WindowResourcePolicy {
            calls: Arc::clone(&calls),
            deny_id: Some(Arc::new(AtomicU32::new(c2.0))),
            deny_mode: Some(AccessMode::DESTROY),
            deny_hides: false,
        }));
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
    // Mutation: put a HIDE check in the shared unmap helper; denying HIDE must not keep a child mapped here.
    let mut fixture = mapped_destroy_subwindows_fixture();
    let [c1, c2, c3] = fixture.children.as_slice() else {
        panic!("three children");
    };
    let (c1, c2, c3) = (*c1, *c2, *c3);
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .state
        .install_access_policy(Arc::new(WindowResourcePolicy {
            calls: Arc::clone(&calls),
            deny_id: Some(Arc::new(AtomicU32::new(c2.0))),
            deny_mode: Some(AccessMode::DESTROY),
            deny_hides: true,
        }));
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
    // Mutation: report the MapWindow SHOW denial as an X error; this request must instead succeed without mapping or notifying.
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
        .install_access_policy(Arc::new(WindowResourcePolicy {
            calls: Arc::clone(&calls),
            deny_id: Some(Arc::new(AtomicU32::new(fixture.window.0))),
            deny_mode: Some(AccessMode::SHOW),
            deny_hides: false,
        }));
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
    // Mutation: omit or reorder any scoped whole-request or derived-target check; its recorded target/mode sequence must differ.
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
            .install_access_policy(Arc::new(WindowResourcePolicy {
                calls: Arc::clone(&calls),
                deny_id: None,
                deny_mode: None,
                deny_hides: false,
            }));

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
    // Mutation: remove or change a whole-request target check; the policy error, request opcode, backend log, or window snapshot must fail.
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
    let reparent_body = words(&[fixture.window.0, fixture.destroy_parent.0, 0]);
    let configure_body = words(&[fixture.window.0, 0x0001, 17]);
    let configure_sibling_body = words(&[fixture.window.0, 0x0060, sibling.0, 0]);
    let mut warp_body = words(&[fixture.parent.0, fixture.window.0]);
    warp_body.extend_from_slice(&[0; 12]);
    let cases = vec![
        WindowDenyCase {
            label: "ReparentWindow window",
            opcode: 7,
            data: 0,
            body: reparent_body.clone(),
            target: fixture.window,
            kind: ResourceKind::Window,
            mode: AccessMode::MANAGE,
            creation: None,
        },
        WindowDenyCase {
            label: "ReparentWindow new parent",
            opcode: 7,
            data: 0,
            body: reparent_body,
            target: fixture.destroy_parent,
            kind: ResourceKind::Window,
            mode: AccessMode::ADD,
            creation: None,
        },
        WindowDenyCase {
            label: "MapSubwindows parent",
            opcode: 9,
            data: 0,
            body: words(&[fixture.subwindows_parent.0]),
            target: fixture.subwindows_parent,
            kind: ResourceKind::Window,
            mode: AccessMode::LIST,
            creation: None,
        },
        WindowDenyCase {
            label: "UnmapWindow",
            opcode: 10,
            data: 0,
            body: words(&[fixture.window.0]),
            target: fixture.window,
            kind: ResourceKind::Window,
            mode: AccessMode::HIDE,
            creation: None,
        },
        WindowDenyCase {
            label: "UnmapSubwindows parent",
            opcode: 11,
            data: 0,
            body: words(&[fixture.subwindows_parent.0]),
            target: fixture.subwindows_parent,
            kind: ResourceKind::Window,
            mode: AccessMode::LIST,
            creation: None,
        },
        WindowDenyCase {
            label: "ConfigureWindow",
            opcode: 12,
            data: 0,
            body: configure_body,
            target: fixture.window,
            kind: ResourceKind::Window,
            mode: AccessMode::MANAGE | AccessMode::SET_ATTR,
            creation: None,
        },
        WindowDenyCase {
            label: "ConfigureWindow sibling",
            opcode: 12,
            data: 0,
            body: configure_sibling_body,
            target: sibling,
            kind: ResourceKind::Window,
            mode: AccessMode::GET_ATTR,
            creation: None,
        },
        WindowDenyCase {
            label: "CirculateWindow container",
            opcode: 13,
            data: 0,
            body: words(&[fixture.subwindows_parent.0]),
            target: fixture.subwindows_parent,
            kind: ResourceKind::Window,
            mode: AccessMode::MANAGE,
            creation: None,
        },
        WindowDenyCase {
            label: "GetGeometry window",
            opcode: 14,
            data: 0,
            body: words(&[fixture.window.0]),
            target: fixture.window,
            kind: ResourceKind::Window,
            mode: AccessMode::GET_ATTR,
            creation: None,
        },
        WindowDenyCase {
            label: "GetGeometry pixmap",
            opcode: 14,
            data: 0,
            body: words(&[fixture.border.0]),
            target: fixture.border,
            kind: ResourceKind::Pixmap,
            mode: AccessMode::GET_ATTR,
            creation: None,
        },
        WindowDenyCase {
            label: "QueryTree",
            opcode: 15,
            data: 0,
            body: words(&[fixture.window.0]),
            target: fixture.window,
            kind: ResourceKind::Window,
            mode: AccessMode::LIST,
            creation: None,
        },
        WindowDenyCase {
            label: "QueryPointer",
            opcode: 38,
            data: 0,
            body: words(&[fixture.window.0]),
            target: fixture.window,
            kind: ResourceKind::Window,
            mode: AccessMode::GET_ATTR,
            creation: None,
        },
        WindowDenyCase {
            label: "GetMotionEvents",
            opcode: 39,
            data: 0,
            body: words(&[fixture.window.0, 0, 0]),
            target: fixture.window,
            kind: ResourceKind::Window,
            mode: AccessMode::GET_ATTR,
            creation: None,
        },
        WindowDenyCase {
            label: "TranslateCoordinates source",
            opcode: 40,
            data: 0,
            body: words(&[fixture.window.0, fixture.parent.0, 0]),
            target: fixture.window,
            kind: ResourceKind::Window,
            mode: AccessMode::GET_ATTR,
            creation: None,
        },
        WindowDenyCase {
            label: "TranslateCoordinates destination",
            opcode: 40,
            data: 0,
            body: words(&[fixture.window.0, fixture.parent.0, 0]),
            target: fixture.parent,
            kind: ResourceKind::Window,
            mode: AccessMode::GET_ATTR,
            creation: None,
        },
        WindowDenyCase {
            label: "WarpPointer destination",
            opcode: 41,
            data: 0,
            body: warp_body.clone(),
            target: fixture.window,
            kind: ResourceKind::Window,
            mode: AccessMode::GET_ATTR,
            creation: None,
        },
        WindowDenyCase {
            label: "WarpPointer source",
            opcode: 41,
            data: 0,
            body: warp_body,
            target: fixture.parent,
            kind: ResourceKind::Window,
            mode: AccessMode::GET_ATTR,
            creation: None,
        },
    ];
    let calls = Arc::new(Mutex::new(Vec::new()));
    let denied_id = Arc::new(AtomicU32::new(0));
    fixture
        .state
        .install_access_policy(Arc::new(WindowResourcePolicy {
            calls: Arc::clone(&calls),
            deny_id: Some(Arc::clone(&denied_id)),
            deny_mode: None,
            deny_hides: false,
        }));

    for (index, case) in cases.into_iter().enumerate() {
        denied_id.store(case.target.0, Ordering::SeqCst);
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
            Some((case.target, case.kind)),
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
    // Mutation: map the denied middle child or stop the loop there; only the two allowed children may map and notify.
    let mut fixture = three_child_window_access_fixture(false);
    let [c1, c2, c3] = fixture.children.as_slice() else {
        panic!("three children");
    };
    let (c1, c2, c3) = (*c1, *c2, *c3);
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .state
        .install_access_policy(Arc::new(WindowResourcePolicy {
            calls: Arc::clone(&calls),
            deny_id: Some(Arc::new(AtomicU32::new(c2.0))),
            deny_mode: Some(AccessMode::SHOW),
            deny_hides: false,
        }));
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
    // Mutation: unmap the denied middle child or stop the loop there; the two allowed children must still unmap.
    let mut fixture = three_child_window_access_fixture(true);
    let [c1, c2, c3] = fixture.children.as_slice() else {
        panic!("three children");
    };
    let (c1, c2, c3) = (*c1, *c2, *c3);
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .state
        .install_access_policy(Arc::new(WindowResourcePolicy {
            calls: Arc::clone(&calls),
            deny_id: Some(Arc::new(AtomicU32::new(c2.0))),
            deny_mode: Some(AccessMode::HIDE),
            deny_hides: false,
        }));
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
    // Mutation: restack the chosen child before or despite its MANAGE+SET_ATTR denial; order, events, and backend calls must stay unchanged.
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
        .install_access_policy(Arc::new(WindowResourcePolicy {
            calls: Arc::clone(&calls),
            deny_id: Some(Arc::new(AtomicU32::new(child.0))),
            deny_mode: Some(AccessMode::MANAGE | AccessMode::SET_ATTR),
            deny_hides: false,
        }));

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
    // Mutation: set only W's map state instead of running UnmapWindow's subtree/focus/grab transition; C or focus/grab state must diverge.
    for with_pointer_grab in [false, true] {
        let (expected_backend, expected_focus, expected_revert) =
            unmap_then_reparent_baseline(with_pointer_grab);
        let mut fixture = reparent_access_fixture(with_pointer_grab);
        let start = fixture.backend.calls().len();
        let calls = Arc::new(Mutex::new(Vec::new()));
        fixture
            .state
            .install_access_policy(Arc::new(WindowResourcePolicy {
                calls: Arc::clone(&calls),
                deny_id: Some(Arc::new(AtomicU32::new(fixture.window.0))),
                deny_mode: Some(AccessMode::SHOW),
                deny_hides: false,
            }));

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
            .install_access_policy(Arc::new(WindowResourcePolicy {
                calls: Arc::new(Mutex::new(Vec::new())),
                deny_id: None,
                deny_mode: None,
                deny_hides: false,
            }));
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
    // Mutation: take a policy-installed remap branch or change its event/backend sequence; allow-all must match no-policy bytes.
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
    // Mutation: omit the parent REMOVE check; DestroyWindow must then destroy the target instead of returning this denial.
    let mut fixture = window_access_fixture();
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .state
        .install_access_policy(Arc::new(WindowResourcePolicy {
            calls: Arc::clone(&calls),
            deny_id: Some(Arc::new(AtomicU32::new(fixture.destroy_parent.0))),
            deny_mode: Some(AccessMode::REMOVE),
            deny_hides: false,
        }));
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
    // Mutation: defer ConfigureWindow's window or sibling check until after sibling validation; the earlier policy denial must win.
    let invalid_sibling = ResourceId(0x0054_00ff);
    let mut fixture = window_access_fixture();
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .state
        .install_access_policy(Arc::new(WindowResourcePolicy {
            calls: Arc::clone(&calls),
            deny_id: Some(Arc::new(AtomicU32::new(fixture.window.0))),
            deny_mode: Some(AccessMode::MANAGE | AccessMode::SET_ATTR),
            deny_hides: false,
        }));
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
        .install_access_policy(Arc::new(WindowResourcePolicy {
            calls: Arc::clone(&calls),
            deny_id: Some(Arc::new(AtomicU32::new(fixture.destroy_parent.0))),
            deny_mode: Some(AccessMode::GET_ATTR),
            deny_hides: false,
        }));
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
    // Mutation: validate the source before checking the already-resolved destination; the destination's policy error must win.
    let mut fixture = window_access_fixture();
    let invalid_source = ResourceId(0x0054_00fe);
    let mut body = words(&[invalid_source.0, fixture.window.0]);
    body.extend_from_slice(&[0; 12]);
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .state
        .install_access_policy(Arc::new(WindowResourcePolicy {
            calls: Arc::clone(&calls),
            deny_id: Some(Arc::new(AtomicU32::new(fixture.window.0))),
            deny_mode: Some(AccessMode::GET_ATTR),
            deny_hides: false,
        }));
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
    // Mutation: keep ReparentWindow checks behind check_reparent_window; MANAGE and ADD denials must beat later BadWindow/BadMatch.
    let mut fixture = window_access_fixture();
    let invalid_parent = ResourceId(0x0054_00fd);
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .state
        .install_access_policy(Arc::new(WindowResourcePolicy {
            calls: Arc::clone(&calls),
            deny_id: Some(Arc::new(AtomicU32::new(fixture.window.0))),
            deny_mode: Some(AccessMode::MANAGE),
            deny_hides: false,
        }));
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
        .install_access_policy(Arc::new(WindowResourcePolicy {
            calls: Arc::clone(&calls),
            deny_id: Some(Arc::new(AtomicU32::new(fixture.window.0))),
            deny_mode: Some(AccessMode::ADD),
            deny_hides: false,
        }));
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
    // Mutation: use header.data as the minor for a core request; direction 1 must still reach policy as minor 0 and the error packet must encode 0.
    let mut fixture = window_access_fixture();
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .state
        .install_access_policy(Arc::new(WindowResourcePolicy {
            calls: Arc::clone(&calls),
            deny_id: Some(Arc::new(AtomicU32::new(fixture.subwindows_parent.0))),
            deny_mode: Some(AccessMode::MANAGE),
            deny_hides: false,
        }));
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

fn gc_font_access_target(
    id: ResourceId,
    kind: ResourceKind,
    mode: AccessMode,
) -> (ResourceId, ResourceKind, AccessMode, Option<Creation>) {
    (id, kind, mode, None)
}

fn gc_font_access_requests(fx: &GcFontAccessFixture) -> Vec<GcFontAccessRequest> {
    let creation = |parent| {
        Some(Creation {
            parent,
            owner: CLIENT,
        })
    };
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
    let mut recolor_cursor = fx.cursor.0.to_le_bytes().to_vec();
    recolor_cursor.extend_from_slice(&[0; 12]);

    let mut open_font = 0x0055_0022u32.to_le_bytes().to_vec();
    open_font.extend_from_slice(&5u16.to_le_bytes());
    open_font.extend_from_slice(&[0, 0]);
    open_font.extend_from_slice(b"fixed");
    open_font.resize(open_font.len().next_multiple_of(4), 0);

    let create_pixmap_id = ResourceId(0x0055_0007);
    let create_cursor_id = ResourceId(0x0055_0033);
    let create_glyph_cursor_id = ResourceId(0x0055_0034);
    let cases = vec![
        GcFontAccessRequest {
            label: "OpenFont",
            opcode: 45,
            data: 0,
            body: open_font,
            expected: vec![(
                ResourceId(0x0055_0022),
                ResourceKind::Font,
                AccessMode::CREATE,
                creation(None),
            )],
            deny_index: 0,
        },
        GcFontAccessRequest {
            label: "QueryFont",
            opcode: 47,
            data: 0,
            body: words(&[fx.font.0]),
            expected: vec![gc_font_access_target(
                fx.font,
                ResourceKind::Font,
                AccessMode::GET_ATTR,
            )],
            deny_index: 0,
        },
        GcFontAccessRequest {
            label: "QueryFont(GC fontable)",
            opcode: 47,
            data: 0,
            body: words(&[fx.gc.0]),
            expected: vec![gc_font_access_target(
                fx.gc,
                ResourceKind::Gc,
                AccessMode::GET_ATTR,
            )],
            deny_index: 0,
        },
        GcFontAccessRequest {
            label: "QueryTextExtents",
            opcode: 48,
            data: 0,
            body: words(&[fx.font.0]),
            expected: vec![gc_font_access_target(
                fx.font,
                ResourceKind::Font,
                AccessMode::GET_ATTR,
            )],
            deny_index: 0,
        },
        GcFontAccessRequest {
            label: "QueryTextExtents(GC fontable)",
            opcode: 48,
            data: 0,
            body: words(&[fx.gc.0]),
            expected: vec![gc_font_access_target(
                fx.gc,
                ResourceKind::Gc,
                AccessMode::GET_ATTR,
            )],
            deny_index: 0,
        },
        GcFontAccessRequest {
            label: "CreatePixmap",
            opcode: 53,
            data: 24,
            body: words(&[create_pixmap_id.0, ROOT_WINDOW.0, 8 | (8 << 16)]),
            expected: vec![
                gc_font_access_target(ROOT_WINDOW, ResourceKind::Window, AccessMode::GET_ATTR),
                (
                    create_pixmap_id,
                    ResourceKind::Pixmap,
                    AccessMode::CREATE,
                    creation(Some(ROOT_WINDOW)),
                ),
            ],
            deny_index: 1,
        },
        GcFontAccessRequest {
            label: "CreateGC",
            opcode: 55,
            data: 0,
            body: create_gc_values,
            expected: vec![
                gc_font_access_target(ROOT_WINDOW, ResourceKind::Window, AccessMode::GET_ATTR),
                gc_font_access_target(fx.tile, ResourceKind::Pixmap, AccessMode::READ),
                gc_font_access_target(fx.stipple, ResourceKind::Pixmap, AccessMode::READ),
                gc_font_access_target(fx.font, ResourceKind::Font, AccessMode::USE),
                gc_font_access_target(fx.clip_mask, ResourceKind::Pixmap, AccessMode::READ),
                (
                    ResourceId(0x0055_0013),
                    ResourceKind::Gc,
                    AccessMode::CREATE | AccessMode::SET_ATTR,
                    creation(Some(ROOT_WINDOW)),
                ),
            ],
            deny_index: 5,
        },
        GcFontAccessRequest {
            label: "ChangeGC",
            opcode: 56,
            data: 0,
            body: change_gc_values,
            expected: vec![
                gc_font_access_target(fx.gc, ResourceKind::Gc, AccessMode::SET_ATTR),
                gc_font_access_target(fx.tile, ResourceKind::Pixmap, AccessMode::READ),
                gc_font_access_target(fx.stipple, ResourceKind::Pixmap, AccessMode::READ),
                gc_font_access_target(fx.font, ResourceKind::Font, AccessMode::USE),
                gc_font_access_target(fx.clip_mask, ResourceKind::Pixmap, AccessMode::READ),
            ],
            deny_index: 0,
        },
        GcFontAccessRequest {
            label: "CopyGC",
            opcode: 57,
            data: 0,
            body: words(&[fx.gc.0, fx.copy_gc.0, 1 << 2]),
            expected: vec![
                gc_font_access_target(fx.gc, ResourceKind::Gc, AccessMode::GET_ATTR),
                gc_font_access_target(fx.copy_gc, ResourceKind::Gc, AccessMode::SET_ATTR),
            ],
            deny_index: 0,
        },
        GcFontAccessRequest {
            label: "SetDashes",
            opcode: 58,
            data: 0,
            body: set_dashes,
            expected: vec![gc_font_access_target(
                fx.gc,
                ResourceKind::Gc,
                AccessMode::SET_ATTR,
            )],
            deny_index: 0,
        },
        GcFontAccessRequest {
            label: "SetClipRectangles",
            opcode: 59,
            data: 0,
            body: words(&[fx.gc.0, 0]),
            expected: vec![gc_font_access_target(
                fx.gc,
                ResourceKind::Gc,
                AccessMode::SET_ATTR,
            )],
            deny_index: 0,
        },
        GcFontAccessRequest {
            label: "CreateCursor",
            opcode: 93,
            data: 0,
            body: words(&[create_cursor_id.0, fx.source.0, fx.stipple.0, 0, 0, 0, 0]),
            expected: vec![
                gc_font_access_target(fx.source, ResourceKind::Pixmap, AccessMode::READ),
                gc_font_access_target(fx.stipple, ResourceKind::Pixmap, AccessMode::READ),
                (
                    create_cursor_id,
                    ResourceKind::Cursor,
                    AccessMode::CREATE,
                    creation(None),
                ),
            ],
            deny_index: 2,
        },
        GcFontAccessRequest {
            label: "CreateGlyphCursor",
            opcode: 94,
            data: 0,
            body: words(&[
                create_glyph_cursor_id.0,
                fx.font.0,
                fx.second_font.0,
                0,
                0,
                0,
                0,
            ]),
            expected: vec![
                gc_font_access_target(fx.font, ResourceKind::Font, AccessMode::USE),
                gc_font_access_target(fx.second_font, ResourceKind::Font, AccessMode::USE),
                (
                    create_glyph_cursor_id,
                    ResourceKind::Cursor,
                    AccessMode::CREATE,
                    creation(None),
                ),
            ],
            deny_index: 2,
        },
        GcFontAccessRequest {
            label: "RecolorCursor",
            opcode: 96,
            data: 0,
            body: recolor_cursor,
            expected: vec![gc_font_access_target(
                fx.cursor,
                ResourceKind::Cursor,
                AccessMode::WRITE,
            )],
            deny_index: 0,
        },
        GcFontAccessRequest {
            label: "CloseFont",
            opcode: 46,
            data: 0,
            body: words(&[fx.second_font.0]),
            expected: vec![gc_font_access_target(
                fx.second_font,
                ResourceKind::Font,
                AccessMode::DESTROY,
            )],
            deny_index: 0,
        },
        GcFontAccessRequest {
            label: "FreePixmap",
            opcode: 54,
            data: 0,
            body: words(&[fx.free_pixmap.0]),
            expected: vec![gc_font_access_target(
                fx.free_pixmap,
                ResourceKind::Pixmap,
                AccessMode::DESTROY,
            )],
            deny_index: 0,
        },
        GcFontAccessRequest {
            label: "FreeGC",
            opcode: 60,
            data: 0,
            body: words(&[fx.free_gc.0]),
            expected: vec![gc_font_access_target(
                fx.free_gc,
                ResourceKind::Gc,
                AccessMode::DESTROY,
            )],
            deny_index: 0,
        },
        GcFontAccessRequest {
            label: "FreeCursor",
            opcode: 95,
            data: 0,
            body: words(&[fx.free_cursor.0]),
            expected: vec![gc_font_access_target(
                fx.free_cursor,
                ResourceKind::Cursor,
                AccessMode::DESTROY,
            )],
            deny_index: 0,
        },
    ];
    cases
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
    // Mutation: remove or reorder a GC, pixmap, font, or cursor check; the recorded target, mode, creation, and core minor must differ.
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
    // Mutation: remove the selected target check from a request row; that row must stop returning the policy error before changing state or calling the backend.
    for case in gc_font_access_requests(&gc_font_access_fixture()) {
        let mut fixture = gc_font_access_fixture();
        let (target_id, target_kind, target_mode, creation) = case.expected[case.deny_index];
        let before_resource = access_target_snapshot(&fixture.state, target_id, target_kind);
        let before_backend = fixture.backend.calls();
        let calls = Arc::new(Mutex::new(Vec::new()));
        fixture
            .state
            .install_access_policy(Arc::new(WindowResourcePolicy {
                calls: Arc::clone(&calls),
                deny_id: Some(Arc::new(AtomicU32::new(target_id.0))),
                deny_mode: Some(target_mode),
                deny_hides: false,
            }));

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

struct DenyPixmapReads;

impl AccessPolicy for DenyPixmapReads {
    fn resource(
        &self,
        _ctx: &AccessContext<'_>,
        _client: ClientId,
        target: &ResourceTarget,
        mode: AccessMode,
    ) -> Decision {
        if target.kind == ResourceKind::Pixmap && mode == AccessMode::READ {
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
fn close_font_and_free_pixmap_denied_keep_the_object() {
    // Mutation: perform the CloseFont and FreePixmap removals before checking DESTROY; after denial the still-open font and pixmap must remain usable.
    let mut fixture = gc_font_access_fixture();
    let denied_id = Arc::new(AtomicU32::new(fixture.font.0));
    fixture
        .state
        .install_access_policy(Arc::new(WindowResourcePolicy {
            calls: Arc::new(Mutex::new(Vec::new())),
            deny_id: Some(Arc::clone(&denied_id)),
            deny_mode: Some(AccessMode::DESTROY),
            deny_hides: false,
        }));

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
    // Mutation: check the CreateGC tile XID without first resolving it as a pixmap; the deny-pixmap policy must then reject the request.
    let mut fixture = gc_font_access_fixture();
    fixture
        .state
        .install_access_policy(Arc::new(DenyPixmapReads));
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
    // Mutation: copy the selected foreground before checking either GC; each denied row must leave the destination snapshot unchanged.
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
            .install_access_policy(Arc::new(WindowResourcePolicy {
                calls: Arc::new(Mutex::new(Vec::new())),
                deny_id: Some(Arc::new(AtomicU32::new(denied_id.0))),
                deny_mode: Some(denied_mode),
                deny_hides: false,
            }));

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

    /// An allow-all policy that only counts its calls, so the timing is the
    /// layer's own cost rather than the recording policy's bookkeeping.
    struct CountingAllow(std::sync::atomic::AtomicUsize);

    impl crate::access::AccessPolicy for CountingAllow {
        fn resource(
            &self,
            _ctx: &crate::access::AccessContext<'_>,
            _client: ClientId,
            _target: &ResourceTarget,
            _mode: AccessMode,
        ) -> Decision {
            self.0.fetch_add(1, Ordering::Relaxed);
            Decision::Allow
        }
    }
    const GC_ID: ResourceId = ResourceId(0x0055_0090);

    let run = |with_policy: bool, counting: Option<Arc<CountingAllow>>| {
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
            state.install_access_policy(counting);
        } else if with_policy {
            state.install_access_policy(Arc::new(DrawingResourcePolicy {
                label: "PolyPoint benchmark allow-all",
                calls: Arc::clone(&calls),
                deny_id: None,
                deny_all: false,
                decision: Decision::Deny(DenyError {
                    code: x11::error::BAD_ACCESS,
                    value: 0x55,
                }),
            }));
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
    let counter = Arc::new(CountingAllow(std::sync::atomic::AtomicUsize::new(0)));
    let counting = run(true, Some(Arc::clone(&counter)));
    assert!(
        counting.1.is_empty(),
        "counting-policy PolyPoint requests wrote an error or reply"
    );
    assert_eq!(counting.2, REQUESTS as usize, "counting-policy point draws");
    assert_eq!(
        counter.0.load(Ordering::Relaxed),
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
    // Mutation: skip RecordingBackend::poly_point recording; each phase must record all request draws.
    assert_eq!(without_policy.2, REQUESTS as usize, "no-policy point draws");
    assert_eq!(with_policy.2, REQUESTS as usize, "policy point draws");
    assert_eq!(without_policy.3, (0, 0, 0), "no-policy resource checks");
    assert_eq!(
        with_policy.3,
        (REQUESTS as usize * 2, REQUESTS as usize, REQUESTS as usize,),
        "allow-all policy asks once for drawable and once for GC per request"
    );
    println!(
        "PolyPoint ({REQUESTS} requests): no policy = {:.1} ns/request; counting allow-all = {:.1} ns/request; recording allow-all = {:.1} ns/request",
        without_policy.0.as_nanos() as f64 / f64::from(REQUESTS),
        counting.0.as_nanos() as f64 / f64::from(REQUESTS),
        with_policy.0.as_nanos() as f64 / f64::from(REQUESTS),
    );
}
