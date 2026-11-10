use super::{
    extensions::{ExtensionHook, RecordedExtensionCheck, RecordingExtensionPolicy},
    *,
};
use crate::{
    access::{
        AccessMode, Creation, Decision, DenyError, RequestOpcodes, ResourceKind, ResourceTarget,
    },
    backend::PixmapHandle,
};
use std::{
    io::Read,
    os::{
        fd::{AsRawFd, IntoRawFd, OwnedFd},
        unix::net::UnixStream,
    },
    sync::{Arc, Mutex, OnceLock},
};
use yserver_protocol::x11::{
    CreateGcRequest, CreatePixmapRequest, ResourceId,
    mit_shm::{self as shm},
};

const CLIENT: ClientId = ClientId(1);
const DRAWABLE: ResourceId = ResourceId(0x0010_5101);
const GC: ResourceId = ResourceId(0x0010_5102);
const SEGMENT: ResourceId = ResourceId(0x0010_5103);
const ATTACH_SEGMENT: ResourceId = ResourceId(0x0010_5111);
const ATTACH_FD_SEGMENT: ResourceId = ResourceId(0x0010_5112);
const CREATE_SEGMENT: ResourceId = ResourceId(0x0010_5113);
const NEW_PIXMAP: ResourceId = ResourceId(0x0010_5121);
const MISSING: ResourceId = ResourceId(0x0010_51F0);
const DENY_ERROR: DenyError = DenyError {
    code: x11::error::BAD_ACCESS,
    value: 0xA55A_1234,
};

struct WarningCapture;

static WARNING_CAPTURE: WarningCapture = WarningCapture;
static WARNING_MESSAGES: Mutex<Vec<String>> = Mutex::new(Vec::new());
static WARNING_CAPTURE_INIT: OnceLock<()> = OnceLock::new();

impl log::Log for WarningCapture {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::Level::Warn
    }

    fn log(&self, record: &log::Record<'_>) {
        if self.enabled(record.metadata()) {
            WARNING_MESSAGES
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(record.args().to_string());
        }
    }

    fn flush(&self) {}
}

fn warning_messages() -> &'static Mutex<Vec<String>> {
    WARNING_CAPTURE_INIT.get_or_init(|| {
        log::set_logger(&WARNING_CAPTURE).expect("install MIT-SHM warning capture");
        log::set_max_level(log::LevelFilter::Warn);
    });
    &WARNING_MESSAGES
}

struct ShmFixture {
    state: ServerState,
    peer: UnixStream,
    backend: RecordingBackend,
}

fn shm_fixture() -> ShmFixture {
    let mut state = ServerState::new();
    let peer = install_client(&mut state, CLIENT.0);
    state.resources.create_pixmap(
        CLIENT,
        CreatePixmapRequest {
            depth: 24,
            pixmap: DRAWABLE,
            drawable: ROOT_WINDOW,
            width: 4,
            height: 4,
        },
    );
    assert!(
        state
            .resources
            .set_pixmap_host_xid(DRAWABLE, PixmapHandle::from_raw_for_test(0x5101))
    );
    state.resources.create_gc(
        CLIENT,
        CreateGcRequest {
            gc: GC,
            drawable: DRAWABLE,
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
    ShmFixture {
        state,
        peer,
        backend: RecordingBackend::new(),
    }
}

fn test_shm_fd() -> OwnedFd {
    let fd = crate::anon_shm::create_anon_shm(c"access-shm-test").expect("create test shm fd");
    assert_eq!(unsafe { libc::ftruncate(fd.as_raw_fd(), 4096) }, 0);
    fd
}

fn test_shm_socket_fd() -> (OwnedFd, UnixStream) {
    let (fd, peer) = UnixStream::pair().expect("create test socket pair");
    (OwnedFd::from(fd), peer)
}

fn install_segment(state: &mut ServerState, id: ResourceId, read_only: bool) {
    let fd = test_shm_fd();
    let mut segment = crate::server::MitShmSegment::from_fd(CLIENT, fd.into_raw_fd(), read_only)
        .expect("map test SHM segment");
    if !read_only {
        segment
            .as_mut_slice()
            .expect("writable test segment")
            .fill(0x5a);
    }
    state.mit_shm_segments.insert(id.0, segment);
}

fn words(values: &[u32]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

fn attach_body(id: ResourceId) -> Vec<u8> {
    words(&[id.0, u32::MAX, 0])
}

fn attach_fd_body(id: ResourceId) -> Vec<u8> {
    words(&[id.0, 0])
}

fn create_segment_body(id: ResourceId) -> Vec<u8> {
    let mut body = words(&[id.0, 4096]);
    body.extend_from_slice(&[0, 0, 0, 0]);
    body
}

fn create_pixmap_body(id: ResourceId, segment: ResourceId) -> Vec<u8> {
    let mut body = words(&[id.0, DRAWABLE.0]);
    body.extend_from_slice(&4u16.to_le_bytes());
    body.extend_from_slice(&4u16.to_le_bytes());
    body.extend_from_slice(&[24, 0, 0, 0]);
    body.extend_from_slice(&segment.0.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body
}

fn put_image_body(segment: ResourceId) -> Vec<u8> {
    let mut body = words(&[DRAWABLE.0, GC.0]);
    body.extend_from_slice(&4u16.to_le_bytes());
    body.extend_from_slice(&4u16.to_le_bytes());
    body.extend_from_slice(&0i16.to_le_bytes());
    body.extend_from_slice(&0i16.to_le_bytes());
    body.extend_from_slice(&4u16.to_le_bytes());
    body.extend_from_slice(&4u16.to_le_bytes());
    body.extend_from_slice(&0i16.to_le_bytes());
    body.extend_from_slice(&0i16.to_le_bytes());
    body.extend_from_slice(&[24, shm::PIXMAP_FORMAT_Z_PIXMAP, 0, 0]);
    body.extend_from_slice(&segment.0.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body
}

fn get_image_body(drawable: ResourceId, segment: ResourceId) -> Vec<u8> {
    let mut body = words(&[drawable.0]);
    body.extend_from_slice(&0i16.to_le_bytes());
    body.extend_from_slice(&0i16.to_le_bytes());
    body.extend_from_slice(&4u16.to_le_bytes());
    body.extend_from_slice(&4u16.to_le_bytes());
    body.extend_from_slice(&u32::MAX.to_le_bytes());
    body.extend_from_slice(&[shm::PIXMAP_FORMAT_Z_PIXMAP, 0, 0, 0]);
    body.extend_from_slice(&segment.0.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body
}

fn dispatch(
    fixture: &mut ShmFixture,
    minor: u8,
    body: &[u8],
    attached_fd: Option<OwnedFd>,
    sequence: u16,
) -> Vec<u8> {
    process_request(
        &mut fixture.state,
        &mut fixture.backend,
        CLIENT,
        SequenceNumber(sequence),
        RequestHeader {
            opcode: 130,
            data: minor,
            length_units: u32::try_from(1 + body.len().div_ceil(4)).unwrap(),
        },
        body,
        attached_fd,
    )
    .expect("MIT-SHM request dispatch");
    read_all_or_buffered(&mut fixture.state, CLIENT.0, &mut fixture.peer)
}

fn existing(id: ResourceId, kind: ResourceKind, mode: AccessMode) -> ExpectedCheck {
    ExpectedCheck {
        target: ResourceTarget {
            owner: None,
            id,
            kind,
            creation: None,
        },
        mode,
        owner: Some(CLIENT),
        target_owner: Some(CLIENT),
    }
}

fn creation(id: ResourceId, kind: ResourceKind) -> ExpectedCheck {
    ExpectedCheck {
        target: ResourceTarget {
            owner: None,
            id,
            kind,
            creation: Some(Creation {
                parent: None,
                owner: CLIENT,
            }),
        },
        mode: AccessMode::CREATE,
        owner: None,
        target_owner: None,
    }
}

fn pixmap_creation(id: ResourceId) -> ExpectedCheck {
    ExpectedCheck {
        target: ResourceTarget {
            owner: None,
            id,
            kind: ResourceKind::Pixmap,
            creation: Some(Creation {
                parent: Some(DRAWABLE),
                owner: CLIENT,
            }),
        },
        mode: AccessMode::CREATE,
        owner: None,
        target_owner: None,
    }
}

#[derive(Clone, Copy, Debug)]
struct ExpectedCheck {
    target: ResourceTarget,
    mode: AccessMode,
    owner: Option<ClientId>,
    target_owner: Option<ClientId>,
}

fn resource_calls(calls: &[RecordedExtensionCheck]) -> Vec<RecordedExtensionCheck> {
    calls
        .iter()
        .filter(|call| call.hook == ExtensionHook::Resource)
        .cloned()
        .collect()
}

fn install_allow_policy(fixture: &mut ShmFixture) -> Arc<Mutex<Vec<RecordedExtensionCheck>>> {
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .state
        .install_access_policy(Arc::new(RecordingExtensionPolicy::new(
            Decision::Allow,
            Arc::clone(&calls),
        )));
    calls
}

fn install_deny_policy(
    fixture: &mut ShmFixture,
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

fn assert_resource_calls(actual: &[RecordedExtensionCheck], expected: &[ExpectedCheck], minor: u8) {
    let actual = resource_calls(actual);
    assert_eq!(
        actual.len(),
        expected.len(),
        "MIT-SHM {minor} resource checks: actual={actual:?}, expected={expected:?}"
    );
    for (call, expected) in actual.iter().zip(expected) {
        assert_eq!(call.client, CLIENT);
        assert_eq!(call.target, Some(expected.target));
        assert_eq!(call.mode, Some(expected.mode));
        assert_eq!(call.owner, expected.owner);
        assert_eq!(call.target_owner, expected.target_owner);
        assert_eq!(call.request, Some(RequestOpcodes { major: 130, minor }));
    }
}

fn assert_policy_error(bytes: &[u8], minor: u8) {
    let mut expected = vec![0, DENY_ERROR.code];
    expected.extend_from_slice(&1u16.to_le_bytes());
    expected.extend_from_slice(&DENY_ERROR.value.to_le_bytes());
    expected.extend_from_slice(&u16::from(minor).to_le_bytes());
    expected.push(130);
    expected.extend_from_slice(&[0; 21]);
    assert_eq!(
        bytes, expected,
        "MIT-SHM policy error is forwarded verbatim"
    );
}

#[test]
fn mit_shm_access_ask_table_records_each_target_and_opcode() {
    // Mutation: omit Detach's segment READ check; the ASK table must report a missing resource target.
    let mut fixture = shm_fixture();
    let calls = install_allow_policy(&mut fixture);
    let bytes = dispatch(
        &mut fixture,
        shm::ATTACH,
        &attach_body(ATTACH_SEGMENT),
        None,
        1,
    );
    assert_eq!(
        bytes[1],
        x11::error::BAD_VALUE,
        "invalid SysV shmid remains invalid"
    );
    assert_resource_calls(
        &calls.lock().unwrap(),
        &[creation(ATTACH_SEGMENT, ResourceKind::ShmSegment)],
        shm::ATTACH,
    );

    let mut fixture = shm_fixture();
    let calls = install_allow_policy(&mut fixture);
    let bytes = dispatch(
        &mut fixture,
        shm::ATTACH_FD,
        &attach_fd_body(ATTACH_FD_SEGMENT),
        Some(test_shm_fd()),
        1,
    );
    assert!(bytes.is_empty());
    assert!(
        fixture
            .state
            .mit_shm_segments
            .contains_key(&ATTACH_FD_SEGMENT.0)
    );
    assert_resource_calls(
        &calls.lock().unwrap(),
        &[creation(ATTACH_FD_SEGMENT, ResourceKind::ShmSegment)],
        shm::ATTACH_FD,
    );

    let mut fixture = shm_fixture();
    install_segment(&mut fixture.state, SEGMENT, false);
    let calls = install_allow_policy(&mut fixture);
    assert!(dispatch(&mut fixture, shm::DETACH, &words(&[SEGMENT.0]), None, 1).is_empty());
    assert!(!fixture.state.mit_shm_segments.contains_key(&SEGMENT.0));
    assert_resource_calls(
        &calls.lock().unwrap(),
        &[existing(
            SEGMENT,
            ResourceKind::ShmSegment,
            AccessMode::READ,
        )],
        shm::DETACH,
    );

    let mut fixture = shm_fixture();
    install_segment(&mut fixture.state, SEGMENT, false);
    let calls = install_allow_policy(&mut fixture);
    assert!(
        dispatch(
            &mut fixture,
            shm::PUT_IMAGE,
            &put_image_body(SEGMENT),
            None,
            1,
        )
        .is_empty()
    );
    assert_resource_calls(
        &calls.lock().unwrap(),
        &[
            existing(DRAWABLE, ResourceKind::Pixmap, AccessMode::WRITE),
            existing(GC, ResourceKind::Gc, AccessMode::USE),
            existing(SEGMENT, ResourceKind::ShmSegment, AccessMode::READ),
        ],
        shm::PUT_IMAGE,
    );

    let mut fixture = shm_fixture();
    install_segment(&mut fixture.state, SEGMENT, false);
    let calls = install_allow_policy(&mut fixture);
    let bytes = dispatch(
        &mut fixture,
        shm::GET_IMAGE,
        &get_image_body(DRAWABLE, SEGMENT),
        None,
        1,
    );
    assert_eq!(
        bytes[1],
        x11::error::BAD_DRAWABLE,
        "recording backend has no image"
    );
    assert_resource_calls(
        &calls.lock().unwrap(),
        &[
            existing(SEGMENT, ResourceKind::ShmSegment, AccessMode::READ),
            existing(DRAWABLE, ResourceKind::Pixmap, AccessMode::READ),
        ],
        shm::GET_IMAGE,
    );

    let mut fixture = shm_fixture();
    install_segment(&mut fixture.state, SEGMENT, false);
    let calls = install_allow_policy(&mut fixture);
    assert!(
        dispatch(
            &mut fixture,
            shm::CREATE_PIXMAP,
            &create_pixmap_body(NEW_PIXMAP, SEGMENT),
            None,
            1,
        )
        .is_empty()
    );
    assert!(fixture.state.resources.pixmap(NEW_PIXMAP).is_some());
    assert_resource_calls(
        &calls.lock().unwrap(),
        &[
            existing(DRAWABLE, ResourceKind::Pixmap, AccessMode::GET_ATTR),
            pixmap_creation(NEW_PIXMAP),
            existing(SEGMENT, ResourceKind::ShmSegment, AccessMode::READ),
        ],
        shm::CREATE_PIXMAP,
    );

    let mut fixture = shm_fixture();
    let calls = install_allow_policy(&mut fixture);
    let bytes = dispatch(
        &mut fixture,
        shm::CREATE_SEGMENT,
        &create_segment_body(CREATE_SEGMENT),
        None,
        1,
    );
    assert_eq!(bytes.len(), 32, "CreateSegment success reply");
    assert!(
        fixture
            .state
            .mit_shm_segments
            .contains_key(&CREATE_SEGMENT.0)
    );
    assert_resource_calls(
        &calls.lock().unwrap(),
        &[creation(CREATE_SEGMENT, ResourceKind::ShmSegment)],
        shm::CREATE_SEGMENT,
    );
}

#[derive(Clone, Copy)]
enum DeniedSite {
    AttachCreate,
    AttachFdCreate,
    DetachRead,
    PutDrawable,
    PutGc,
    PutSegment,
    GetSegment,
    GetDrawable,
    CreatePixmapDrawable,
    CreatePixmapCreate,
    CreatePixmapSegment,
    CreateSegmentCreate,
}

struct DenialCase {
    label: &'static str,
    site: DeniedSite,
    minor: u8,
    target: ResourceId,
    kind: ResourceKind,
    mode: AccessMode,
    expected: Vec<ExpectedCheck>,
}

fn denial_cases() -> Vec<DenialCase> {
    use AccessMode as M;
    use ResourceKind as K;
    vec![
        DenialCase {
            label: "Attach creation",
            site: DeniedSite::AttachCreate,
            minor: shm::ATTACH,
            target: ATTACH_SEGMENT,
            kind: K::ShmSegment,
            mode: M::CREATE,
            expected: vec![creation(ATTACH_SEGMENT, K::ShmSegment)],
        },
        DenialCase {
            label: "AttachFd creation",
            site: DeniedSite::AttachFdCreate,
            minor: shm::ATTACH_FD,
            target: ATTACH_FD_SEGMENT,
            kind: K::ShmSegment,
            mode: M::CREATE,
            expected: vec![creation(ATTACH_FD_SEGMENT, K::ShmSegment)],
        },
        DenialCase {
            label: "Detach segment",
            site: DeniedSite::DetachRead,
            minor: shm::DETACH,
            target: SEGMENT,
            kind: K::ShmSegment,
            mode: M::READ,
            expected: vec![existing(SEGMENT, K::ShmSegment, M::READ)],
        },
        DenialCase {
            label: "PutImage drawable",
            site: DeniedSite::PutDrawable,
            minor: shm::PUT_IMAGE,
            target: DRAWABLE,
            kind: K::Pixmap,
            mode: M::WRITE,
            expected: vec![existing(DRAWABLE, K::Pixmap, M::WRITE)],
        },
        DenialCase {
            label: "PutImage GC",
            site: DeniedSite::PutGc,
            minor: shm::PUT_IMAGE,
            target: GC,
            kind: K::Gc,
            mode: M::USE,
            expected: vec![
                existing(DRAWABLE, K::Pixmap, M::WRITE),
                existing(GC, K::Gc, M::USE),
            ],
        },
        DenialCase {
            label: "PutImage segment",
            site: DeniedSite::PutSegment,
            minor: shm::PUT_IMAGE,
            target: SEGMENT,
            kind: K::ShmSegment,
            mode: M::READ,
            expected: vec![
                existing(DRAWABLE, K::Pixmap, M::WRITE),
                existing(GC, K::Gc, M::USE),
                existing(SEGMENT, K::ShmSegment, M::READ),
            ],
        },
        DenialCase {
            label: "GetImage segment",
            site: DeniedSite::GetSegment,
            minor: shm::GET_IMAGE,
            target: SEGMENT,
            kind: K::ShmSegment,
            mode: M::READ,
            expected: vec![existing(SEGMENT, K::ShmSegment, M::READ)],
        },
        DenialCase {
            label: "GetImage drawable",
            site: DeniedSite::GetDrawable,
            minor: shm::GET_IMAGE,
            target: DRAWABLE,
            kind: K::Pixmap,
            mode: M::READ,
            expected: vec![
                existing(SEGMENT, K::ShmSegment, M::READ),
                existing(DRAWABLE, K::Pixmap, M::READ),
            ],
        },
        DenialCase {
            label: "CreatePixmap drawable",
            site: DeniedSite::CreatePixmapDrawable,
            minor: shm::CREATE_PIXMAP,
            target: DRAWABLE,
            kind: K::Pixmap,
            mode: M::GET_ATTR,
            expected: vec![existing(DRAWABLE, K::Pixmap, M::GET_ATTR)],
        },
        DenialCase {
            label: "CreatePixmap creation",
            site: DeniedSite::CreatePixmapCreate,
            minor: shm::CREATE_PIXMAP,
            target: NEW_PIXMAP,
            kind: K::Pixmap,
            mode: M::CREATE,
            expected: vec![
                existing(DRAWABLE, K::Pixmap, M::GET_ATTR),
                pixmap_creation(NEW_PIXMAP),
            ],
        },
        DenialCase {
            label: "CreatePixmap segment",
            site: DeniedSite::CreatePixmapSegment,
            minor: shm::CREATE_PIXMAP,
            target: SEGMENT,
            kind: K::ShmSegment,
            mode: M::READ,
            expected: vec![
                existing(DRAWABLE, K::Pixmap, M::GET_ATTR),
                pixmap_creation(NEW_PIXMAP),
                existing(SEGMENT, K::ShmSegment, M::READ),
            ],
        },
        DenialCase {
            label: "CreateSegment creation",
            site: DeniedSite::CreateSegmentCreate,
            minor: shm::CREATE_SEGMENT,
            target: CREATE_SEGMENT,
            kind: K::ShmSegment,
            mode: M::CREATE,
            expected: vec![creation(CREATE_SEGMENT, K::ShmSegment)],
        },
    ]
}

fn denial_request(site: DeniedSite) -> (Vec<u8>, Option<OwnedFd>) {
    match site {
        DeniedSite::AttachCreate => (attach_body(ATTACH_SEGMENT), None),
        DeniedSite::AttachFdCreate => (attach_fd_body(ATTACH_FD_SEGMENT), Some(test_shm_fd())),
        DeniedSite::DetachRead => (words(&[SEGMENT.0]), None),
        DeniedSite::PutDrawable | DeniedSite::PutGc | DeniedSite::PutSegment => {
            (put_image_body(SEGMENT), None)
        }
        DeniedSite::GetSegment | DeniedSite::GetDrawable => {
            (get_image_body(DRAWABLE, SEGMENT), None)
        }
        DeniedSite::CreatePixmapDrawable
        | DeniedSite::CreatePixmapCreate
        | DeniedSite::CreatePixmapSegment => (create_pixmap_body(NEW_PIXMAP, SEGMENT), None),
        DeniedSite::CreateSegmentCreate => (create_segment_body(CREATE_SEGMENT), None),
    }
}

#[test]
fn mit_shm_resource_denial_table_matches_each_site_and_stops_effects() {
    // Mutation: move GetImage's drawable READ check after the backend readback; its denial row must observe the forbidden call.
    for case in denial_cases() {
        assert_eq!(
            case.expected
                .last()
                .expect("denial site has a check")
                .target
                .kind,
            case.kind,
            "{} expected target kind",
            case.label
        );
        let mut fixture = shm_fixture();
        let segment_read_only = matches!(case.site, DeniedSite::GetSegment);
        if !matches!(
            case.site,
            DeniedSite::AttachCreate | DeniedSite::AttachFdCreate | DeniedSite::CreateSegmentCreate
        ) {
            install_segment(&mut fixture.state, SEGMENT, segment_read_only);
        }
        let calls = install_deny_policy(&mut fixture, case.target, case.mode);
        let (body, fd) = denial_request(case.site);
        let bytes = dispatch(&mut fixture, case.minor, &body, fd, 1);
        assert_policy_error(&bytes, case.minor);
        assert_resource_calls(&calls.lock().unwrap(), &case.expected, case.minor);

        match case.site {
            DeniedSite::AttachCreate
            | DeniedSite::AttachFdCreate
            | DeniedSite::CreateSegmentCreate => {
                assert!(
                    !fixture.state.mit_shm_segments.contains_key(&case.target.0),
                    "{} denial created a mapping",
                    case.label
                );
            }
            DeniedSite::DetachRead => {
                assert!(fixture.state.mit_shm_segments.contains_key(&SEGMENT.0))
            }
            DeniedSite::PutDrawable | DeniedSite::PutGc | DeniedSite::PutSegment => {
                assert!(
                    fixture.backend.calls().is_empty(),
                    "{} ran backend work",
                    case.label
                );
            }
            DeniedSite::GetSegment | DeniedSite::GetDrawable => {
                assert_eq!(
                    fixture.backend.get_image_calls, 0,
                    "{} read the drawable",
                    case.label
                );
                assert!(fixture.state.mit_shm_segments.contains_key(&SEGMENT.0));
            }
            DeniedSite::CreatePixmapDrawable | DeniedSite::CreatePixmapCreate => {
                assert!(fixture.state.resources.pixmap(NEW_PIXMAP).is_none());
                assert!(
                    fixture.backend.calls().is_empty(),
                    "{} allocated",
                    case.label
                );
            }
            DeniedSite::CreatePixmapSegment => {
                assert!(fixture.state.resources.pixmap(NEW_PIXMAP).is_none());
                assert!(fixture.backend.live_pixmaps.is_empty());
                assert_eq!(
                    fixture.backend.calls(),
                    vec![
                        crate::backend::recording::RecordedCall::CreatePixmap {
                            depth: 24,
                            width: 4,
                            height: 4,
                        },
                        crate::backend::recording::RecordedCall::FreePixmap(0x0001_0000),
                    ],
                    "late segment denial frees its one allocated host pixmap"
                );
            }
        }
    }
}

#[test]
fn shm_create_pixmap_denied_creation_allocates_nothing() {
    // Mutation: ask for CREATE after backend.create_pixmap; this assertion must catch the allocation.
    let mut fixture = shm_fixture();
    install_segment(&mut fixture.state, SEGMENT, false);
    let calls = install_deny_policy(&mut fixture, NEW_PIXMAP, AccessMode::CREATE);
    let bytes = dispatch(
        &mut fixture,
        shm::CREATE_PIXMAP,
        &create_pixmap_body(NEW_PIXMAP, SEGMENT),
        None,
        1,
    );
    assert_policy_error(&bytes, shm::CREATE_PIXMAP);
    assert_resource_calls(
        &calls.lock().unwrap(),
        &[
            existing(DRAWABLE, ResourceKind::Pixmap, AccessMode::GET_ATTR),
            pixmap_creation(NEW_PIXMAP),
        ],
        shm::CREATE_PIXMAP,
    );
    assert!(fixture.state.resources.pixmap(NEW_PIXMAP).is_none());
    assert!(fixture.backend.calls().is_empty());
}

#[test]
fn shm_create_pixmap_denied_segment_frees_allocation() {
    // Mutation: return the segment denial without backend.free_pixmap; this assertion must find the leaked host allocation.
    let mut fixture = shm_fixture();
    install_segment(&mut fixture.state, SEGMENT, false);
    let calls = install_deny_policy(&mut fixture, SEGMENT, AccessMode::READ);
    let bytes = dispatch(
        &mut fixture,
        shm::CREATE_PIXMAP,
        &create_pixmap_body(NEW_PIXMAP, SEGMENT),
        None,
        1,
    );
    assert_policy_error(&bytes, shm::CREATE_PIXMAP);
    assert_resource_calls(
        &calls.lock().unwrap(),
        &[
            existing(DRAWABLE, ResourceKind::Pixmap, AccessMode::GET_ATTR),
            pixmap_creation(NEW_PIXMAP),
            existing(SEGMENT, ResourceKind::ShmSegment, AccessMode::READ),
        ],
        shm::CREATE_PIXMAP,
    );
    assert!(fixture.state.resources.pixmap(NEW_PIXMAP).is_none());
    assert!(fixture.backend.live_pixmaps.is_empty());
    assert!(matches!(
        fixture.backend.calls().as_slice(),
        [
            crate::backend::recording::RecordedCall::CreatePixmap { .. },
            crate::backend::recording::RecordedCall::FreePixmap(0x0001_0000)
        ]
    ));
}

#[test]
fn shm_create_pixmap_keeps_policy_error_when_rollback_fails() {
    // Mutation: omit the cleanup-error warn!; this must fail at the captured-warning assertion.
    let warnings = warning_messages();
    warnings
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
    let mut fixture = shm_fixture();
    install_segment(&mut fixture.state, SEGMENT, false);
    fixture.backend.free_pixmap_fails = true;
    let calls = install_deny_policy(&mut fixture, SEGMENT, AccessMode::READ);
    let bytes = dispatch(
        &mut fixture,
        shm::CREATE_PIXMAP,
        &create_pixmap_body(NEW_PIXMAP, SEGMENT),
        None,
        1,
    );

    assert_policy_error(&bytes, shm::CREATE_PIXMAP);
    assert_resource_calls(
        &calls.lock().unwrap(),
        &[
            existing(DRAWABLE, ResourceKind::Pixmap, AccessMode::GET_ATTR),
            pixmap_creation(NEW_PIXMAP),
            existing(SEGMENT, ResourceKind::ShmSegment, AccessMode::READ),
        ],
        shm::CREATE_PIXMAP,
    );
    assert!(fixture.state.resources.pixmap(NEW_PIXMAP).is_none());
    assert_eq!(
        fixture.backend.calls(),
        vec![
            crate::backend::recording::RecordedCall::CreatePixmap {
                depth: 24,
                width: 4,
                height: 4,
            },
            crate::backend::recording::RecordedCall::FreePixmap(0x0001_0000),
        ]
    );
    assert!(fixture.backend.live_pixmaps.contains(&0x0001_0000));
    assert!(
        warnings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|message| message.contains(
                "MIT-SHM::CreatePixmap could not free host pixmap 0x10000 after segment access denial: free_pixmap failure injected"
            )),
        "rollback failure should be logged"
    );
}

#[test]
fn shm_get_image_denied_drawable_reads_back_nothing() {
    // Mutation: move the drawable READ check after backend.get_image; this test must observe its readback counter increase.
    let mut fixture = shm_fixture();
    install_segment(&mut fixture.state, SEGMENT, false);
    let calls = install_deny_policy(&mut fixture, DRAWABLE, AccessMode::READ);
    let bytes = dispatch(
        &mut fixture,
        shm::GET_IMAGE,
        &get_image_body(DRAWABLE, SEGMENT),
        None,
        1,
    );
    assert_policy_error(&bytes, shm::GET_IMAGE);
    assert_resource_calls(
        &calls.lock().unwrap(),
        &[
            existing(SEGMENT, ResourceKind::ShmSegment, AccessMode::READ),
            existing(DRAWABLE, ResourceKind::Pixmap, AccessMode::READ),
        ],
        shm::GET_IMAGE,
    );
    assert_eq!(fixture.backend.get_image_calls, 0);
}

#[test]
fn shm_detach_unresolved_segment_is_not_asked() {
    // Mutation: replace the if-present Detach lookup with an unconditional check; the policy must record no target.
    let mut fixture = shm_fixture();
    let calls = install_allow_policy(&mut fixture);
    let bytes = dispatch(&mut fixture, shm::DETACH, &words(&[MISSING.0]), None, 1);
    assert!(
        bytes.is_empty(),
        "unresolved Detach keeps its silent success"
    );
    assert_resource_calls(&calls.lock().unwrap(), &[], shm::DETACH);
}

#[test]
fn denied_shm_segment_creations_leave_no_mapping_or_fd() {
    // Mutation: omit AttachFd's CREATE check; the valid descriptor would become a mapped segment.
    let mut attach = shm_fixture();
    let attach_calls = install_deny_policy(&mut attach, ATTACH_SEGMENT, AccessMode::CREATE);
    let attach_error = dispatch(
        &mut attach,
        shm::ATTACH,
        &attach_body(ATTACH_SEGMENT),
        None,
        1,
    );
    assert_policy_error(&attach_error, shm::ATTACH);
    assert!(
        !attach
            .state
            .mit_shm_segments
            .contains_key(&ATTACH_SEGMENT.0)
    );
    assert_resource_calls(
        &attach_calls.lock().unwrap(),
        &[creation(ATTACH_SEGMENT, ResourceKind::ShmSegment)],
        shm::ATTACH,
    );

    let mut attach_fd = shm_fixture();
    let calls = install_deny_policy(&mut attach_fd, ATTACH_FD_SEGMENT, AccessMode::CREATE);
    // Mutation: leak the denied AttachFd descriptor; the retained peer must no longer see EOF.
    let (fd, mut peer) = test_shm_socket_fd();
    peer.set_nonblocking(true).expect("set peer nonblocking");
    let attach_fd_error = dispatch(
        &mut attach_fd,
        shm::ATTACH_FD,
        &attach_fd_body(ATTACH_FD_SEGMENT),
        Some(fd),
        1,
    );
    assert_policy_error(&attach_fd_error, shm::ATTACH_FD);
    assert!(
        !attach_fd
            .state
            .mit_shm_segments
            .contains_key(&ATTACH_FD_SEGMENT.0)
    );
    let mut byte = [0u8; 1];
    assert_eq!(
        peer.read(&mut byte).expect("read denied AttachFd peer"),
        0,
        "denied AttachFd closes the unconsumed descriptor"
    );
    assert_resource_calls(
        &calls.lock().unwrap(),
        &[creation(ATTACH_FD_SEGMENT, ResourceKind::ShmSegment)],
        shm::ATTACH_FD,
    );

    let mut create_segment = shm_fixture();
    let calls = install_deny_policy(&mut create_segment, CREATE_SEGMENT, AccessMode::CREATE);
    let create_error = dispatch(
        &mut create_segment,
        shm::CREATE_SEGMENT,
        &create_segment_body(CREATE_SEGMENT),
        None,
        1,
    );
    assert_policy_error(&create_error, shm::CREATE_SEGMENT);
    assert!(
        !create_segment
            .state
            .mit_shm_segments
            .contains_key(&CREATE_SEGMENT.0)
    );
    assert_resource_calls(
        &calls.lock().unwrap(),
        &[creation(CREATE_SEGMENT, ResourceKind::ShmSegment)],
        shm::CREATE_SEGMENT,
    );
}
