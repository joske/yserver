use super::{
    extensions::{ExtensionHook, RecordedExtensionCheck, RecordingExtensionPolicy},
    *,
};
use crate::access::{
    AccessMode, Decision, DenyError, RequestOpcodes, ResourceKind, ResourceTarget,
};

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
