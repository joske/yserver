use super::{
    extensions::{ExtensionHook, RecordedExtensionCheck, RecordingExtensionPolicy},
    *,
};
use crate::{
    access::{
        AccessMode, Creation, Decision, DenyError, RequestOpcodes, ResourceKind, ResourceTarget,
    },
    backend::{Dri3Caps, PixmapHandle},
};
use std::{
    io::Read,
    os::fd::OwnedFd,
    sync::{Arc, Mutex},
};
use yserver_protocol::x11::{CreatePixmapRequest, CreateWindowRequest, ResourceId, dri3};

const CLIENT: ClientId = ClientId(1);
const OTHER_CLIENT: ClientId = ClientId(2);
const SERVER_OWNER: ClientId = ClientId(0);
const WINDOW: ResourceId = ResourceId(0x0010_4101);
const PIXMAP: ResourceId = ResourceId(0x0010_4102);
const PROVIDER: ResourceId = ResourceId(0x0010_4103);
const FENCE: ResourceId = ResourceId(0x0010_4104);
const SYNCOBJ: ResourceId = ResourceId(0x0010_4105);
const NEW_PIXMAP: ResourceId = ResourceId(0x0010_4111);
const NEW_FENCE: ResourceId = ResourceId(0x0010_4112);
const NEW_SYNCOBJ: ResourceId = ResourceId(0x0010_4113);
const MISSING: ResourceId = ResourceId(0x0010_41F0);
const DENY_ERROR: DenyError = DenyError {
    code: x11::error::BAD_ACCESS,
    value: 0xA55A_1234,
};

#[derive(Debug, Clone, Copy)]
struct ExpectedCheck {
    target: ResourceTarget,
    mode: AccessMode,
    owner: Option<ClientId>,
    target_owner: Option<ClientId>,
}

struct Dri3Fixture {
    state: ServerState,
    peer: UnixStream,
    backend: RecordingBackend,
}

fn dri3_fixture() -> Dri3Fixture {
    let mut state = ServerState::new();
    let peer = install_client(&mut state, CLIENT.0);
    state.resources.create_window(
        CLIENT,
        CreateWindowRequest {
            depth: 24,
            window: WINDOW,
            parent: crate::resources::ROOT_WINDOW,
            width: 4,
            height: 4,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    state.resources.create_pixmap(
        CLIENT,
        CreatePixmapRequest {
            depth: 24,
            pixmap: PIXMAP,
            drawable: WINDOW,
            width: 4,
            height: 4,
        },
    );
    assert!(
        state
            .resources
            .set_pixmap_host_xid(PIXMAP, PixmapHandle::from_raw_for_test(0x4102))
    );
    state.randr.set_providers(vec![crate::randr::RandrProvider {
        provider_id: PROVIDER.0,
        name: "recording provider".to_owned(),
        capabilities: 0,
        is_gpu: true,
        crtcs: Vec::new(),
        outputs: Vec::new(),
        associations: Vec::new(),
    }]);
    state.sync_fences.insert(
        FENCE.0,
        crate::server::SyncFence {
            owner: CLIENT,
            triggered: false,
        },
    );
    assert!(state.resources.register_dri3_syncobj(SYNCOBJ, CLIENT));
    let mut backend = RecordingBackend::new();
    backend.dri3_caps = Dri3Caps {
        version: (1, 4),
        modifiers: true,
        fence_fd: true,
        syncobj: true,
    };
    Dri3Fixture {
        state,
        peer,
        backend,
    }
}

fn existing(
    id: ResourceId,
    kind: ResourceKind,
    mode: AccessMode,
    owner: ClientId,
) -> ExpectedCheck {
    ExpectedCheck {
        target: ResourceTarget {
            id,
            kind,
            creation: None,
            owner: None,
        },
        mode,
        owner: Some(owner),
        target_owner: Some(owner),
    }
}

fn creation(id: ResourceId, kind: ResourceKind, parent: Option<ResourceId>) -> ExpectedCheck {
    ExpectedCheck {
        target: ResourceTarget {
            id,
            kind,
            creation: Some(Creation {
                parent,
                owner: CLIENT,
            }),
            owner: None,
        },
        mode: AccessMode::CREATE,
        owner: None,
        target_owner: None,
    }
}

fn words(values: &[u32]) -> Vec<u8> {
    let mut body = Vec::with_capacity(values.len() * 4);
    for value in values {
        body.extend_from_slice(&value.to_le_bytes());
    }
    body
}

fn pixmap_from_buffer_body(pixmap: ResourceId, drawable: ResourceId) -> Vec<u8> {
    let mut body = words(&[pixmap.0, drawable.0, 64]);
    body.extend_from_slice(&4u16.to_le_bytes());
    body.extend_from_slice(&4u16.to_le_bytes());
    body.extend_from_slice(&16u16.to_le_bytes());
    body.extend_from_slice(&[24, 32]);
    body
}

fn pixmap_from_buffers_body(pixmap: ResourceId, window: ResourceId) -> Vec<u8> {
    let mut body = vec![0u8; 60];
    body[0..4].copy_from_slice(&pixmap.0.to_le_bytes());
    body[4..8].copy_from_slice(&window.0.to_le_bytes());
    body[8] = 1;
    body[12..14].copy_from_slice(&4u16.to_le_bytes());
    body[14..16].copy_from_slice(&4u16.to_le_bytes());
    body[16..20].copy_from_slice(&16u32.to_le_bytes());
    body[48] = 24;
    body[49] = 32;
    body
}

fn fence_from_fd_body(drawable: ResourceId, fence: ResourceId) -> Vec<u8> {
    let mut body = words(&[drawable.0, fence.0]);
    body.extend_from_slice(&[0, 0, 0, 0]);
    body
}

fn open_body(drawable: ResourceId, provider: ResourceId) -> Vec<u8> {
    words(&[drawable.0, provider.0])
}

fn dispatch(
    fixture: &mut Dri3Fixture,
    minor: u8,
    body: &[u8],
    fd: Option<OwnedFd>,
    sequence: u16,
) -> Vec<u8> {
    process_request(
        &mut fixture.state,
        &mut fixture.backend,
        CLIENT,
        SequenceNumber(sequence),
        RequestHeader {
            opcode: 147,
            data: minor,
            length_units: u32::try_from(1 + body.len().div_ceil(4)).unwrap(),
        },
        body,
        fd,
    )
    .expect("DRI3 request dispatch");
    read_all_or_buffered(&mut fixture.state, CLIENT.0, &mut fixture.peer)
}

fn test_fd() -> OwnedFd {
    test_fd_with_close_probe().0
}

fn test_fd_with_close_probe() -> (OwnedFd, UnixStream) {
    let (fd, peer) = UnixStream::pair().expect("create fd-passing test socket pair");
    (fd.into(), peer)
}

fn install_allow_policy(fixture: &mut Dri3Fixture) -> Arc<Mutex<Vec<RecordedExtensionCheck>>> {
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .state
        .install_access_policy(Arc::new(RecordingExtensionPolicy::new(
            Decision::Allow,
            Arc::clone(&calls),
        )));
    calls
}

fn resource_calls(calls: &[RecordedExtensionCheck]) -> Vec<RecordedExtensionCheck> {
    calls
        .iter()
        .filter(|call| call.hook == ExtensionHook::Resource)
        .cloned()
        .collect()
}

fn assert_resource_calls(actual: &[RecordedExtensionCheck], expected: &[ExpectedCheck], minor: u8) {
    assert_eq!(
        actual.len(),
        expected.len(),
        "DRI3 {minor} resource checks: actual={actual:?}, expected={expected:?}"
    );
    for (call, expected) in actual.iter().zip(expected) {
        assert_eq!(call.hook, ExtensionHook::Resource);
        assert_eq!(call.client, CLIENT);
        assert_eq!(call.target, Some(expected.target));
        assert_eq!(call.mode, Some(expected.mode));
        assert_eq!(call.owner, expected.owner);
        assert_eq!(call.target_owner, expected.target_owner);
        assert_eq!(call.request, Some(RequestOpcodes { major: 147, minor }));
    }
}

fn assert_policy_error(bytes: &[u8], minor: u8) {
    let mut expected = vec![0, DENY_ERROR.code];
    expected.extend_from_slice(&1u16.to_le_bytes());
    expected.extend_from_slice(&DENY_ERROR.value.to_le_bytes());
    expected.extend_from_slice(&u16::from(minor).to_le_bytes());
    expected.push(147);
    expected.extend_from_slice(&[0; 21]);
    assert_eq!(
        bytes, expected,
        "DRI3 policy error must be forwarded verbatim"
    );
}

fn deny_policy(
    fixture: &mut Dri3Fixture,
    target: ResourceId,
    mode: AccessMode,
) -> Arc<Mutex<Vec<RecordedExtensionCheck>>> {
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .state
        .install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
            Arc::clone(&calls),
            target,
            Some(mode),
            DENY_ERROR,
        )));
    calls
}

#[test]
fn dri3_access_ask_table_records_each_target_and_opcode() {
    // Mutation: omit Open's provider READ check; the ASK table must fail on the missing target.
    let mut expected_by_request: Vec<(u8, Vec<ExpectedCheck>)> = Vec::new();
    let mut sequence = 1;

    let mut fixture = dri3_fixture();
    let calls = install_allow_policy(&mut fixture);
    let bytes = dispatch(
        &mut fixture,
        dri3::OPEN,
        &open_body(WINDOW, PROVIDER),
        None,
        sequence,
    );
    assert_eq!(bytes[1], x11::error::BAD_ALLOC);
    expected_by_request.push((
        dri3::OPEN,
        vec![
            existing(WINDOW, ResourceKind::Window, AccessMode::GET_ATTR, CLIENT),
            existing(
                PROVIDER,
                ResourceKind::RandrProvider,
                AccessMode::READ,
                SERVER_OWNER,
            ),
        ],
    ));
    sequence += 1;

    let fd = test_fd();
    let bytes = dispatch(
        &mut fixture,
        dri3::PIXMAP_FROM_BUFFER,
        &pixmap_from_buffer_body(NEW_PIXMAP, WINDOW),
        Some(fd),
        sequence,
    );
    assert_eq!(bytes[1], x11::error::BAD_ALLOC);
    expected_by_request.push((
        dri3::PIXMAP_FROM_BUFFER,
        vec![
            existing(WINDOW, ResourceKind::Window, AccessMode::GET_ATTR, CLIENT),
            creation(NEW_PIXMAP, ResourceKind::Pixmap, Some(WINDOW)),
        ],
    ));
    sequence += 1;

    let bytes = dispatch(
        &mut fixture,
        dri3::BUFFER_FROM_PIXMAP,
        &words(&[PIXMAP.0]),
        None,
        sequence,
    );
    assert_eq!(bytes[1], x11::error::BAD_PIXMAP);
    expected_by_request.push((
        dri3::BUFFER_FROM_PIXMAP,
        vec![existing(
            PIXMAP,
            ResourceKind::Pixmap,
            AccessMode::WRITE,
            CLIENT,
        )],
    ));
    sequence += 1;

    let fd = test_fd();
    let bytes = dispatch(
        &mut fixture,
        dri3::FENCE_FROM_FD,
        &fence_from_fd_body(WINDOW, NEW_FENCE),
        Some(fd),
        sequence,
    );
    assert!(bytes.is_empty());
    expected_by_request.push((
        dri3::FENCE_FROM_FD,
        vec![
            existing(WINDOW, ResourceKind::Window, AccessMode::GET_ATTR, CLIENT),
            creation(NEW_FENCE, ResourceKind::SyncFence, Some(WINDOW)),
        ],
    ));
    sequence += 1;

    let bytes = dispatch(
        &mut fixture,
        dri3::FD_FROM_FENCE,
        &words(&[WINDOW.0, FENCE.0]),
        None,
        sequence,
    );
    assert_eq!(bytes[1], x11::error::BAD_ALLOC);
    expected_by_request.push((
        dri3::FD_FROM_FENCE,
        vec![
            existing(WINDOW, ResourceKind::Window, AccessMode::GET_ATTR, CLIENT),
            existing(FENCE, ResourceKind::SyncFence, AccessMode::WRITE, CLIENT),
        ],
    ));
    sequence += 1;

    let mut modifiers = words(&[WINDOW.0]);
    modifiers.extend_from_slice(&[24, 32, 0, 0]);
    let bytes = dispatch(
        &mut fixture,
        dri3::GET_SUPPORTED_MODIFIERS,
        &modifiers,
        None,
        sequence,
    );
    assert_eq!(bytes[0], 1);
    expected_by_request.push((
        dri3::GET_SUPPORTED_MODIFIERS,
        vec![existing(
            WINDOW,
            ResourceKind::Window,
            AccessMode::GET_ATTR,
            CLIENT,
        )],
    ));
    sequence += 1;

    let fd = test_fd();
    let bytes = dispatch(
        &mut fixture,
        dri3::PIXMAP_FROM_BUFFERS,
        &pixmap_from_buffers_body(NEW_PIXMAP, WINDOW),
        Some(fd),
        sequence,
    );
    assert_eq!(bytes[1], x11::error::BAD_ALLOC);
    expected_by_request.push((
        dri3::PIXMAP_FROM_BUFFERS,
        vec![
            existing(WINDOW, ResourceKind::Window, AccessMode::GET_ATTR, CLIENT),
            creation(NEW_PIXMAP, ResourceKind::Pixmap, Some(WINDOW)),
        ],
    ));
    sequence += 1;

    let bytes = dispatch(
        &mut fixture,
        dri3::BUFFERS_FROM_PIXMAP,
        &words(&[PIXMAP.0]),
        None,
        sequence,
    );
    assert_eq!(bytes[1], x11::error::BAD_PIXMAP);
    expected_by_request.push((
        dri3::BUFFERS_FROM_PIXMAP,
        vec![existing(
            PIXMAP,
            ResourceKind::Pixmap,
            AccessMode::WRITE,
            CLIENT,
        )],
    ));
    sequence += 1;

    let bytes = dispatch(
        &mut fixture,
        dri3::SET_DRM_DEVICE_IN_USE,
        &words(&[WINDOW.0, 1, 2]),
        None,
        sequence,
    );
    assert!(bytes.is_empty());
    expected_by_request.push((
        dri3::SET_DRM_DEVICE_IN_USE,
        vec![existing(
            WINDOW,
            ResourceKind::Window,
            AccessMode::GET_ATTR,
            CLIENT,
        )],
    ));
    sequence += 1;

    let fd = test_fd();
    let bytes = dispatch(
        &mut fixture,
        dri3::IMPORT_SYNCOBJ,
        &words(&[NEW_SYNCOBJ.0, WINDOW.0]),
        Some(fd),
        sequence,
    );
    assert!(bytes.is_empty());
    expected_by_request.push((
        dri3::IMPORT_SYNCOBJ,
        vec![
            existing(WINDOW, ResourceKind::Window, AccessMode::GET_ATTR, CLIENT),
            creation(NEW_SYNCOBJ, ResourceKind::Dri3Syncobj, Some(WINDOW)),
        ],
    ));
    sequence += 1;

    let bytes = dispatch(
        &mut fixture,
        dri3::FREE_SYNCOBJ,
        &words(&[SYNCOBJ.0]),
        None,
        sequence,
    );
    assert_eq!(bytes[1], x11::error::BAD_VALUE);
    expected_by_request.push((
        dri3::FREE_SYNCOBJ,
        vec![existing(
            SYNCOBJ,
            ResourceKind::Dri3Syncobj,
            AccessMode::WRITE,
            CLIENT,
        )],
    ));

    let recorded = calls.lock().unwrap();
    let actual = resource_calls(&recorded);
    let expected: Vec<_> = expected_by_request
        .iter()
        .flat_map(|(_, checks)| checks.iter().copied())
        .collect();
    assert_eq!(actual.len(), expected.len());
    let mut actual_offset = 0;
    for (minor, checks) in &expected_by_request {
        let end = actual_offset + checks.len();
        assert_resource_calls(&actual[actual_offset..end], checks, *minor);
        actual_offset = end;
    }
}

#[test]
fn dri3_resource_denial_table_stops_before_side_effects() {
    // Mutation: omit SetDRMDeviceInUse's Window check; that row must stop returning the policy error.
    struct DenialCase {
        label: &'static str,
        minor: u8,
        body: Vec<u8>,
        target: ResourceId,
        kind: ResourceKind,
        mode: AccessMode,
        fd: bool,
        expected_checks: Vec<ExpectedCheck>,
    }

    let cases = vec![
        DenialCase {
            label: "Open drawable",
            minor: dri3::OPEN,
            body: open_body(WINDOW, ResourceId(0)),
            target: WINDOW,
            kind: ResourceKind::Window,
            mode: AccessMode::GET_ATTR,
            fd: false,
            expected_checks: vec![existing(
                WINDOW,
                ResourceKind::Window,
                AccessMode::GET_ATTR,
                CLIENT,
            )],
        },
        DenialCase {
            label: "Open provider",
            minor: dri3::OPEN,
            body: open_body(WINDOW, PROVIDER),
            target: PROVIDER,
            kind: ResourceKind::RandrProvider,
            mode: AccessMode::READ,
            fd: false,
            expected_checks: vec![
                existing(WINDOW, ResourceKind::Window, AccessMode::GET_ATTR, CLIENT),
                existing(
                    PROVIDER,
                    ResourceKind::RandrProvider,
                    AccessMode::READ,
                    SERVER_OWNER,
                ),
            ],
        },
        DenialCase {
            label: "PixmapFromBuffer drawable",
            minor: dri3::PIXMAP_FROM_BUFFER,
            body: pixmap_from_buffer_body(NEW_PIXMAP, WINDOW),
            target: WINDOW,
            kind: ResourceKind::Window,
            mode: AccessMode::GET_ATTR,
            fd: true,
            expected_checks: vec![existing(
                WINDOW,
                ResourceKind::Window,
                AccessMode::GET_ATTR,
                CLIENT,
            )],
        },
        DenialCase {
            label: "PixmapFromBuffer creation",
            minor: dri3::PIXMAP_FROM_BUFFER,
            body: pixmap_from_buffer_body(NEW_PIXMAP, WINDOW),
            target: NEW_PIXMAP,
            kind: ResourceKind::Pixmap,
            mode: AccessMode::CREATE,
            fd: true,
            expected_checks: vec![
                existing(WINDOW, ResourceKind::Window, AccessMode::GET_ATTR, CLIENT),
                creation(NEW_PIXMAP, ResourceKind::Pixmap, Some(WINDOW)),
            ],
        },
        DenialCase {
            label: "BufferFromPixmap",
            minor: dri3::BUFFER_FROM_PIXMAP,
            body: words(&[PIXMAP.0]),
            target: PIXMAP,
            kind: ResourceKind::Pixmap,
            mode: AccessMode::WRITE,
            fd: false,
            expected_checks: vec![existing(
                PIXMAP,
                ResourceKind::Pixmap,
                AccessMode::WRITE,
                CLIENT,
            )],
        },
        DenialCase {
            label: "FenceFromFD drawable",
            minor: dri3::FENCE_FROM_FD,
            body: fence_from_fd_body(WINDOW, NEW_FENCE),
            target: WINDOW,
            kind: ResourceKind::Window,
            mode: AccessMode::GET_ATTR,
            fd: true,
            expected_checks: vec![existing(
                WINDOW,
                ResourceKind::Window,
                AccessMode::GET_ATTR,
                CLIENT,
            )],
        },
        DenialCase {
            label: "FenceFromFD creation",
            minor: dri3::FENCE_FROM_FD,
            body: fence_from_fd_body(WINDOW, NEW_FENCE),
            target: NEW_FENCE,
            kind: ResourceKind::SyncFence,
            mode: AccessMode::CREATE,
            fd: true,
            expected_checks: vec![
                existing(WINDOW, ResourceKind::Window, AccessMode::GET_ATTR, CLIENT),
                creation(NEW_FENCE, ResourceKind::SyncFence, Some(WINDOW)),
            ],
        },
        DenialCase {
            label: "FDFromFence drawable",
            minor: dri3::FD_FROM_FENCE,
            body: words(&[WINDOW.0, FENCE.0]),
            target: WINDOW,
            kind: ResourceKind::Window,
            mode: AccessMode::GET_ATTR,
            fd: false,
            expected_checks: vec![existing(
                WINDOW,
                ResourceKind::Window,
                AccessMode::GET_ATTR,
                CLIENT,
            )],
        },
        DenialCase {
            label: "FDFromFence fence",
            minor: dri3::FD_FROM_FENCE,
            body: words(&[WINDOW.0, FENCE.0]),
            target: FENCE,
            kind: ResourceKind::SyncFence,
            mode: AccessMode::WRITE,
            fd: false,
            expected_checks: vec![
                existing(WINDOW, ResourceKind::Window, AccessMode::GET_ATTR, CLIENT),
                existing(FENCE, ResourceKind::SyncFence, AccessMode::WRITE, CLIENT),
            ],
        },
        DenialCase {
            label: "GetSupportedModifiers window",
            minor: dri3::GET_SUPPORTED_MODIFIERS,
            body: [words(&[WINDOW.0]), vec![24, 32, 0, 0]].concat(),
            target: WINDOW,
            kind: ResourceKind::Window,
            mode: AccessMode::GET_ATTR,
            fd: false,
            expected_checks: vec![existing(
                WINDOW,
                ResourceKind::Window,
                AccessMode::GET_ATTR,
                CLIENT,
            )],
        },
        DenialCase {
            label: "PixmapFromBuffers window",
            minor: dri3::PIXMAP_FROM_BUFFERS,
            body: pixmap_from_buffers_body(NEW_PIXMAP, WINDOW),
            target: WINDOW,
            kind: ResourceKind::Window,
            mode: AccessMode::GET_ATTR,
            fd: true,
            expected_checks: vec![existing(
                WINDOW,
                ResourceKind::Window,
                AccessMode::GET_ATTR,
                CLIENT,
            )],
        },
        DenialCase {
            label: "PixmapFromBuffers creation",
            minor: dri3::PIXMAP_FROM_BUFFERS,
            body: pixmap_from_buffers_body(NEW_PIXMAP, WINDOW),
            target: NEW_PIXMAP,
            kind: ResourceKind::Pixmap,
            mode: AccessMode::CREATE,
            fd: true,
            expected_checks: vec![
                existing(WINDOW, ResourceKind::Window, AccessMode::GET_ATTR, CLIENT),
                creation(NEW_PIXMAP, ResourceKind::Pixmap, Some(WINDOW)),
            ],
        },
        DenialCase {
            label: "BuffersFromPixmap",
            minor: dri3::BUFFERS_FROM_PIXMAP,
            body: words(&[PIXMAP.0]),
            target: PIXMAP,
            kind: ResourceKind::Pixmap,
            mode: AccessMode::WRITE,
            fd: false,
            expected_checks: vec![existing(
                PIXMAP,
                ResourceKind::Pixmap,
                AccessMode::WRITE,
                CLIENT,
            )],
        },
        DenialCase {
            label: "SetDRMDeviceInUse window",
            minor: dri3::SET_DRM_DEVICE_IN_USE,
            body: words(&[WINDOW.0, 1, 2]),
            target: WINDOW,
            kind: ResourceKind::Window,
            mode: AccessMode::GET_ATTR,
            fd: false,
            expected_checks: vec![existing(
                WINDOW,
                ResourceKind::Window,
                AccessMode::GET_ATTR,
                CLIENT,
            )],
        },
        DenialCase {
            label: "ImportSyncobj drawable",
            minor: dri3::IMPORT_SYNCOBJ,
            body: words(&[NEW_SYNCOBJ.0, WINDOW.0]),
            target: WINDOW,
            kind: ResourceKind::Window,
            mode: AccessMode::GET_ATTR,
            fd: true,
            expected_checks: vec![existing(
                WINDOW,
                ResourceKind::Window,
                AccessMode::GET_ATTR,
                CLIENT,
            )],
        },
        DenialCase {
            label: "ImportSyncobj creation",
            minor: dri3::IMPORT_SYNCOBJ,
            body: words(&[NEW_SYNCOBJ.0, WINDOW.0]),
            target: NEW_SYNCOBJ,
            kind: ResourceKind::Dri3Syncobj,
            mode: AccessMode::CREATE,
            fd: true,
            expected_checks: vec![
                existing(WINDOW, ResourceKind::Window, AccessMode::GET_ATTR, CLIENT),
                creation(NEW_SYNCOBJ, ResourceKind::Dri3Syncobj, Some(WINDOW)),
            ],
        },
        DenialCase {
            label: "FreeSyncobj",
            minor: dri3::FREE_SYNCOBJ,
            body: words(&[SYNCOBJ.0]),
            target: SYNCOBJ,
            kind: ResourceKind::Dri3Syncobj,
            mode: AccessMode::WRITE,
            fd: false,
            expected_checks: vec![existing(
                SYNCOBJ,
                ResourceKind::Dri3Syncobj,
                AccessMode::WRITE,
                OTHER_CLIENT,
            )],
        },
    ];

    for case in cases {
        let mut fixture = dri3_fixture();
        if case.minor == dri3::FREE_SYNCOBJ {
            assert!(
                fixture
                    .state
                    .resources
                    .remove_dri3_syncobj(SYNCOBJ)
                    .is_some()
            );
            assert!(
                fixture
                    .state
                    .resources
                    .register_dri3_syncobj(SYNCOBJ, OTHER_CLIENT)
            );
        }
        let calls = deny_policy(&mut fixture, case.target, case.mode);
        let (fd, fd_probe) = if case.fd {
            let (fd, peer) = test_fd_with_close_probe();
            (Some(fd), Some(peer))
        } else {
            (None, None)
        };
        let bytes = dispatch(&mut fixture, case.minor, &case.body, fd, 1);
        assert_policy_error(&bytes, case.minor);
        let recorded = calls.lock().unwrap();
        let actual = resource_calls(&recorded);
        assert_resource_calls(&actual, &case.expected_checks, case.minor);
        assert_eq!(
            actual
                .last()
                .and_then(|call| call.target)
                .map(|target| target.kind),
            Some(case.kind),
            "{} denied the expected resource kind",
            case.label
        );
        assert!(
            fixture.backend.calls().is_empty(),
            "{} reached backend: {:?}",
            case.label,
            fixture.backend.calls()
        );
        if let Some(mut fd_probe) = fd_probe {
            // DRI3 owns and drops a received descriptor when access is denied before import.
            fd_probe
                .set_nonblocking(true)
                .expect("set close probe nonblocking");
            let mut byte = [0];
            let bytes_read = fd_probe
                .read(&mut byte)
                .expect("read received-fd close probe");
            assert_eq!(bytes_read, 0, "{} must close the received fd", case.label);
        }
        match case.minor {
            dri3::PIXMAP_FROM_BUFFER | dri3::PIXMAP_FROM_BUFFERS => assert!(
                fixture.state.resources.pixmap(NEW_PIXMAP).is_none(),
                "{} registered a denied pixmap",
                case.label
            ),
            dri3::FENCE_FROM_FD => assert!(
                !fixture.state.sync_fences.contains_key(&NEW_FENCE.0),
                "{} registered a denied fence",
                case.label
            ),
            dri3::IMPORT_SYNCOBJ => assert!(
                fixture
                    .state
                    .resources
                    .dri3_syncobj_owner(NEW_SYNCOBJ)
                    .is_none(),
                "{} registered a denied syncobj",
                case.label
            ),
            dri3::FREE_SYNCOBJ => assert_eq!(
                fixture.state.resources.dri3_syncobj_owner(SYNCOBJ),
                Some(OTHER_CLIENT),
                "denied FreeSyncobj changed ownership"
            ),
            _ => {}
        }
    }
}

#[test]
fn free_syncobj_denied_before_owner_bad_access() {
    // Mutation: move the resource check below the owner comparison; the foreign-owned denial must become BadAccess and fail.
    let mut fixture = dri3_fixture();
    assert!(
        fixture
            .state
            .resources
            .remove_dri3_syncobj(SYNCOBJ)
            .is_some()
    );
    assert!(
        fixture
            .state
            .resources
            .register_dri3_syncobj(SYNCOBJ, OTHER_CLIENT)
    );
    let body = words(&[SYNCOBJ.0]);

    let no_policy = dispatch(&mut fixture, dri3::FREE_SYNCOBJ, &body, None, 1);
    assert_eq!(no_policy[1], x11::error::BAD_ACCESS);
    assert_eq!(no_policy[4..8], SYNCOBJ.0.to_le_bytes());
    assert_eq!(
        fixture.state.resources.dri3_syncobj_owner(SYNCOBJ),
        Some(OTHER_CLIENT)
    );

    let mut denied = dri3_fixture();
    assert!(
        denied
            .state
            .resources
            .remove_dri3_syncobj(SYNCOBJ)
            .is_some()
    );
    assert!(
        denied
            .state
            .resources
            .register_dri3_syncobj(SYNCOBJ, OTHER_CLIENT)
    );
    let calls = deny_policy(&mut denied, SYNCOBJ, AccessMode::WRITE);
    let bytes = dispatch(&mut denied, dri3::FREE_SYNCOBJ, &body, None, 1);
    assert_policy_error(&bytes, dri3::FREE_SYNCOBJ);
    assert_resource_calls(
        &resource_calls(&calls.lock().unwrap()),
        &[existing(
            SYNCOBJ,
            ResourceKind::Dri3Syncobj,
            AccessMode::WRITE,
            OTHER_CLIENT,
        )],
        dri3::FREE_SYNCOBJ,
    );
    assert!(denied.backend.calls().is_empty());
    assert_eq!(
        denied.state.resources.dri3_syncobj_owner(SYNCOBJ),
        Some(OTHER_CLIENT)
    );

    let mut missing = dri3_fixture();
    let calls = deny_policy(&mut missing, MISSING, AccessMode::WRITE);
    let bytes = dispatch(
        &mut missing,
        dri3::FREE_SYNCOBJ,
        &words(&[MISSING.0]),
        None,
        1,
    );
    assert_eq!(bytes[1], x11::error::BAD_VALUE);
    assert!(resource_calls(&calls.lock().unwrap()).is_empty());
}

#[test]
fn buffer_from_pixmap_denied_parks_no_export() {
    // Mutation: move the Pixmap WRITE check after dri3_begin_export_pixmap_buffers; the backend call/pending token assertion must fail.
    let mut fixture = dri3_fixture();
    let token = crate::backend::Dri3ExportToken(0x4108);
    fixture.backend.pending_dri3_export = Some(token);
    let calls = deny_policy(&mut fixture, PIXMAP, AccessMode::WRITE);
    let bytes = dispatch(
        &mut fixture,
        dri3::BUFFERS_FROM_PIXMAP,
        &words(&[PIXMAP.0]),
        None,
        1,
    );
    assert_policy_error(&bytes, dri3::BUFFERS_FROM_PIXMAP);
    assert_resource_calls(
        &resource_calls(&calls.lock().unwrap()),
        &[existing(
            PIXMAP,
            ResourceKind::Pixmap,
            AccessMode::WRITE,
            CLIENT,
        )],
        dri3::BUFFERS_FROM_PIXMAP,
    );
    assert!(fixture.backend.calls().is_empty());
    assert!(
        fixture
            .state
            .clients
            .get(&CLIENT.0)
            .unwrap()
            .outbound
            .is_empty()
    );
    assert!(fixture.state.resources.pixmap(PIXMAP).is_some());
    assert_eq!(fixture.backend.pending_dri3_export, Some(token));
}

#[test]
fn dri3_unvalidated_wrong_kind_and_missing_operands_keep_policy_parity() {
    // Mutation: treat a Pixmap as GetSupportedModifiers' Window; its no-ASK parity row must fail.
    struct OptionalCase {
        minor: u8,
        no_policy_body: Vec<u8>,
        allow_policy_body: Vec<u8>,
    }

    let cases = [
        OptionalCase {
            minor: dri3::OPEN,
            no_policy_body: open_body(MISSING, MISSING),
            allow_policy_body: open_body(MISSING, MISSING),
        },
        OptionalCase {
            minor: dri3::OPEN,
            no_policy_body: open_body(FENCE, WINDOW),
            allow_policy_body: open_body(FENCE, WINDOW),
        },
        OptionalCase {
            minor: dri3::FD_FROM_FENCE,
            no_policy_body: words(&[MISSING.0, MISSING.0]),
            allow_policy_body: words(&[MISSING.0, MISSING.0]),
        },
        OptionalCase {
            minor: dri3::FD_FROM_FENCE,
            no_policy_body: words(&[FENCE.0, WINDOW.0]),
            allow_policy_body: words(&[FENCE.0, WINDOW.0]),
        },
        OptionalCase {
            minor: dri3::GET_SUPPORTED_MODIFIERS,
            no_policy_body: [words(&[MISSING.0]), vec![24, 32, 0, 0]].concat(),
            allow_policy_body: [words(&[MISSING.0]), vec![24, 32, 0, 0]].concat(),
        },
        OptionalCase {
            minor: dri3::GET_SUPPORTED_MODIFIERS,
            no_policy_body: [words(&[PIXMAP.0]), vec![24, 32, 0, 0]].concat(),
            allow_policy_body: [words(&[PIXMAP.0]), vec![24, 32, 0, 0]].concat(),
        },
        OptionalCase {
            minor: dri3::SET_DRM_DEVICE_IN_USE,
            no_policy_body: words(&[MISSING.0, 1, 2]),
            allow_policy_body: words(&[MISSING.0, 1, 2]),
        },
        OptionalCase {
            minor: dri3::SET_DRM_DEVICE_IN_USE,
            no_policy_body: words(&[PIXMAP.0, 1, 2]),
            allow_policy_body: words(&[PIXMAP.0, 1, 2]),
        },
    ];

    for (index, case) in cases.into_iter().enumerate() {
        let mut no_policy = dri3_fixture();
        let without_policy = dispatch(
            &mut no_policy,
            case.minor,
            &case.no_policy_body,
            None,
            index as u16 + 1,
        );

        let mut with_policy = dri3_fixture();
        let calls = install_allow_policy(&mut with_policy);
        let with_allow = dispatch(
            &mut with_policy,
            case.minor,
            &case.allow_policy_body,
            None,
            index as u16 + 1,
        );
        assert_eq!(
            with_allow, without_policy,
            "DRI3 minor {} changed bytes",
            case.minor
        );
        assert!(resource_calls(&calls.lock().unwrap()).is_empty());
    }
}

#[test]
fn pixmap_from_buffer_creation_with_missing_parent_records_none() {
    // Mutation: always use the request's drawable as Creation.parent; this unresolved-parent ASK record must fail.
    let mut fixture = dri3_fixture();
    let calls = install_allow_policy(&mut fixture);
    let fd = test_fd();
    let bytes = dispatch(
        &mut fixture,
        dri3::PIXMAP_FROM_BUFFER,
        &pixmap_from_buffer_body(NEW_PIXMAP, MISSING),
        Some(fd),
        1,
    );
    assert_eq!(bytes[1], x11::error::BAD_ALLOC);
    assert_resource_calls(
        &resource_calls(&calls.lock().unwrap()),
        &[creation(NEW_PIXMAP, ResourceKind::Pixmap, None)],
        dri3::PIXMAP_FROM_BUFFER,
    );
}
