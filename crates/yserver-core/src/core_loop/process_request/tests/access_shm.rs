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
    sync::{Arc, Mutex},
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
const RO_SEGMENT: ResourceId = ResourceId(0x0010_5104);
/// MIT-SHM's `BadShmSeg`, the extension's first error.
const BAD_SHM_SEG: u8 = crate::nested::MIT_SHM_FIRST_ERROR;
const DENY_ERROR: DenyError = DenyError {
    code: x11::error::BAD_ACCESS,
    value: 0xA55A_1234,
};

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
    test_shm_fd_with_size(4096)
}

fn test_shm_fd_with_size(size: libc::off_t) -> OwnedFd {
    let fd = crate::anon_shm::create_anon_shm(c"access-shm-test").expect("create test shm fd");
    assert_eq!(unsafe { libc::ftruncate(fd.as_raw_fd(), size) }, 0);
    fd
}

fn test_shm_socket_fd() -> (OwnedFd, UnixStream) {
    let (fd, peer) = UnixStream::pair().expect("create test socket pair");
    (OwnedFd::from(fd), peer)
}

fn install_segment(state: &mut ServerState, id: ResourceId, read_only: bool) {
    install_segment_with_size(state, id, read_only, 4096);
}

fn install_segment_with_size(
    state: &mut ServerState,
    id: ResourceId,
    read_only: bool,
    size: libc::off_t,
) {
    let fd = test_shm_fd_with_size(size);
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
    assert_x_error(bytes, DENY_ERROR.code, DENY_ERROR.value, minor);
}

fn assert_x_error(bytes: &[u8], code: u8, value: u32, minor: u8) {
    let mut expected = vec![0, code];
    expected.extend_from_slice(&1u16.to_le_bytes());
    expected.extend_from_slice(&value.to_le_bytes());
    expected.extend_from_slice(&u16::from(minor).to_le_bytes());
    expected.push(130);
    expected.extend_from_slice(&[0; 21]);
    assert_eq!(bytes, expected, "MIT-SHM error packet");
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
            existing(DRAWABLE, ResourceKind::Pixmap, AccessMode::READ),
            existing(SEGMENT, ResourceKind::ShmSegment, AccessMode::READ),
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
            existing(SEGMENT, ResourceKind::ShmSegment, AccessMode::READ),
            pixmap_creation(NEW_PIXMAP),
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
            expected: vec![
                existing(DRAWABLE, K::Pixmap, M::READ),
                existing(SEGMENT, K::ShmSegment, M::READ),
            ],
        },
        DenialCase {
            label: "GetImage drawable",
            site: DeniedSite::GetDrawable,
            minor: shm::GET_IMAGE,
            target: DRAWABLE,
            kind: K::Pixmap,
            mode: M::READ,
            expected: vec![existing(DRAWABLE, K::Pixmap, M::READ)],
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
                existing(SEGMENT, K::ShmSegment, M::READ),
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
                assert!(
                    fixture.backend.calls().is_empty(),
                    "segment denial must happen before backend allocation"
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
            existing(SEGMENT, ResourceKind::ShmSegment, AccessMode::READ),
            pixmap_creation(NEW_PIXMAP),
        ],
        shm::CREATE_PIXMAP,
    );
    assert!(fixture.state.resources.pixmap(NEW_PIXMAP).is_none());
    assert!(fixture.backend.calls().is_empty());
}

#[test]
fn shm_create_pixmap_denied_segment_allocates_nothing() {
    // Mutation: allocate the backend pixmap before the segment READ check; this assertion must find the unwanted allocation.
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
            existing(SEGMENT, ResourceKind::ShmSegment, AccessMode::READ),
        ],
        shm::CREATE_PIXMAP,
    );
    assert!(fixture.state.resources.pixmap(NEW_PIXMAP).is_none());
    assert!(fixture.backend.live_pixmaps.is_empty());
    assert!(fixture.backend.calls().is_empty());
}

#[test]
fn shm_create_pixmap_missing_segment_allocates_nothing() {
    // Mutation: allocate the backend pixmap before checking segment existence; the call log must catch it.
    let mut fixture = shm_fixture();
    let bytes = dispatch(
        &mut fixture,
        shm::CREATE_PIXMAP,
        &create_pixmap_body(NEW_PIXMAP, MISSING),
        None,
        1,
    );

    assert_x_error(&bytes, BAD_SHM_SEG, MISSING.0, shm::CREATE_PIXMAP);
    assert!(fixture.state.resources.pixmap(NEW_PIXMAP).is_none());
    assert!(fixture.backend.calls().is_empty());
    assert!(fixture.backend.live_pixmaps.is_empty());
}

#[test]
fn shm_create_pixmap_undersized_segment_allocates_nothing() {
    // Mutation: allocate the backend pixmap before checking the segment byte range; the call log must catch it.
    let mut fixture = shm_fixture();
    // The OS may round the object up to a page (16 KiB on macOS), so the
    // offset, not the 1-byte size, is what puts the pixmap past the end.
    const OFFSET_PAST_ANY_PAGE: u32 = 1 << 20;
    install_segment_with_size(&mut fixture.state, SEGMENT, false, 1);
    let mut body = create_pixmap_body(NEW_PIXMAP, SEGMENT);
    body[20..24].copy_from_slice(&OFFSET_PAST_ANY_PAGE.to_le_bytes());
    let bytes = dispatch(&mut fixture, shm::CREATE_PIXMAP, &body, None, 1);

    assert_x_error(
        &bytes,
        x11::error::BAD_VALUE,
        OFFSET_PAST_ANY_PAGE,
        shm::CREATE_PIXMAP,
    );
    assert!(fixture.state.resources.pixmap(NEW_PIXMAP).is_none());
    assert!(fixture.state.mit_shm_segments.contains_key(&SEGMENT.0));
    assert!(fixture.backend.calls().is_empty());
    assert!(fixture.backend.live_pixmaps.is_empty());
}

#[test]
fn shm_create_pixmap_valid_segment_allocates_exactly_once() {
    // Mutation: skip backend.create_pixmap or the upload; their exact call records must catch it.
    let mut fixture = shm_fixture();
    install_segment(&mut fixture.state, SEGMENT, false);
    let bytes = dispatch(
        &mut fixture,
        shm::CREATE_PIXMAP,
        &create_pixmap_body(NEW_PIXMAP, SEGMENT),
        None,
        1,
    );

    assert!(bytes.is_empty());
    let calls = fixture.backend.calls();
    assert_eq!(
        calls
            .iter()
            .filter(|call| matches!(
                call,
                crate::backend::recording::RecordedCall::CreatePixmap { .. }
            ))
            .count(),
        1,
        "valid segment creates exactly one backend pixmap"
    );
    let pixmap = fixture
        .state
        .resources
        .pixmap(NEW_PIXMAP)
        .expect("valid MIT-SHM CreatePixmap resource");
    let host = pixmap.host_xid.expect("backend pixmap handle").as_raw();
    assert_eq!(fixture.backend.live_pixmaps.len(), 1);
    assert!(fixture.backend.live_pixmaps.contains(&host));
    let uploads: Vec<_> = calls
        .iter()
        .filter_map(|call| match call {
            crate::backend::recording::RecordedCall::PutImage {
                host_xid,
                depth,
                width,
                height,
                ..
            } => Some((*host_xid, *depth, *width, *height)),
            _ => None,
        })
        .collect();
    assert_eq!(
        uploads,
        vec![(host, pixmap.depth, pixmap.width, pixmap.height)],
        "valid segment uploads once to the registered pixmap with its requested dimensions"
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
        &[existing(DRAWABLE, ResourceKind::Pixmap, AccessMode::READ)],
        shm::GET_IMAGE,
    );
    assert_eq!(fixture.backend.get_image_calls, 0);
}

#[test]
fn shm_detach_unresolved_segment_is_bad_shm_seg_and_not_asked() {
    // Mutation: drop Detach's existence check; it must answer BadShmSeg (Xorg's VERIFY_SHMSEG) without asking the policy.
    let mut fixture = shm_fixture();
    let calls = install_allow_policy(&mut fixture);
    let bytes = dispatch(&mut fixture, shm::DETACH, &words(&[MISSING.0]), None, 1);
    assert_x_error(&bytes, BAD_SHM_SEG, MISSING.0, shm::DETACH);
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

/// One MIT-SHM request with a bad segment, offset or extent, and what
/// Xorg 21.1 answers (measured with xcb against Xvfb, 4 KiB segments).
struct RangeCase {
    label: &'static str,
    minor: u8,
    body: Vec<u8>,
    /// `None` for Success; otherwise the error code and value.
    error: Option<(u8, u32)>,
}

fn with_offset(mut body: Vec<u8>, at: usize, offset: u32) -> Vec<u8> {
    body[at..at + 4].copy_from_slice(&offset.to_le_bytes());
    body
}

fn shm_create(segment: ResourceId, offset: u32, width: u16, height: u16) -> Vec<u8> {
    let mut body = with_offset(create_pixmap_body(NEW_PIXMAP, segment), 20, offset);
    body[8..10].copy_from_slice(&width.to_le_bytes());
    body[10..12].copy_from_slice(&height.to_le_bytes());
    body
}

fn shm_get(segment: ResourceId, offset: u32) -> Vec<u8> {
    with_offset(get_image_body(DRAWABLE, segment), 24, offset)
}

/// PutImage of a `total` image's `src` rectangle (x, y, width, height).
fn shm_put(
    segment: ResourceId,
    offset: u32,
    total: (u16, u16),
    src: (u16, u16, u16, u16),
) -> Vec<u8> {
    let mut body = with_offset(put_image_body(segment), 32, offset);
    for (at, value) in [
        (8, total.0),
        (10, total.1),
        (12, src.0),
        (14, src.1),
        (16, src.2),
        (18, src.3),
    ] {
        body[at..at + 2].copy_from_slice(&value.to_le_bytes());
    }
    body
}

fn range_cases(size: u32) -> Vec<RangeCase> {
    use x11::error::{BAD_ACCESS, BAD_ALLOC, BAD_VALUE};
    let (rw, ro) = (SEGMENT.0, RO_SEGMENT.0);
    // 4x4 at depth 24 is 64 bytes: in range at `size - 4`, but overrunning.
    let (overrun, past) = (size - 4, size * 2);
    let image = (4, 4, 4, 4);
    let full = (0, 0, 4, 4);
    let case = |label, minor, body, error| RangeCase {
        label,
        minor,
        body,
        error,
    };
    vec![
        case(
            "CreatePixmap unaligned",
            shm::CREATE_PIXMAP,
            shm_create(SEGMENT, 2, 4, 4),
            Some((BAD_VALUE, 2)),
        ),
        case(
            "CreatePixmap past the end",
            shm::CREATE_PIXMAP,
            shm_create(SEGMENT, past, 4, 4),
            Some((BAD_VALUE, past)),
        ),
        case(
            "CreatePixmap overrun",
            shm::CREATE_PIXMAP,
            shm_create(SEGMENT, overrun, 4, 4),
            Some((BAD_ACCESS, rw)),
        ),
        case(
            "CreatePixmap at the end",
            shm::CREATE_PIXMAP,
            shm_create(SEGMENT, size, 4, 4),
            Some((BAD_ACCESS, rw)),
        ),
        case(
            "CreatePixmap read-only",
            shm::CREATE_PIXMAP,
            shm_create(RO_SEGMENT, 0, 4, 4),
            Some((BAD_ACCESS, ro)),
        ),
        case(
            "CreatePixmap read-only unaligned",
            shm::CREATE_PIXMAP,
            shm_create(RO_SEGMENT, 2, 4, 4),
            Some((BAD_VALUE, 2)),
        ),
        case(
            "CreatePixmap missing segment",
            shm::CREATE_PIXMAP,
            shm_create(MISSING, 2, 4, 4),
            Some((BAD_SHM_SEG, MISSING.0)),
        ),
        case(
            "CreatePixmap zero width",
            shm::CREATE_PIXMAP,
            shm_create(SEGMENT, 0, 0, 4),
            Some((BAD_VALUE, 0)),
        ),
        case(
            "CreatePixmap over 32767 wide",
            shm::CREATE_PIXMAP,
            shm_create(SEGMENT, 0, 40000, 1),
            Some((BAD_ALLOC, rw)),
        ),
        case(
            "CreatePixmap wider than the segment",
            shm::CREATE_PIXMAP,
            shm_create(SEGMENT, 0, u16::try_from(size / 4 + 1).unwrap(), 1),
            Some((BAD_ACCESS, rw)),
        ),
        case(
            "GetImage unaligned",
            shm::GET_IMAGE,
            shm_get(SEGMENT, 2),
            Some((BAD_VALUE, 2)),
        ),
        case(
            "GetImage past the end",
            shm::GET_IMAGE,
            shm_get(SEGMENT, past),
            Some((BAD_VALUE, past)),
        ),
        case(
            "GetImage overrun",
            shm::GET_IMAGE,
            shm_get(SEGMENT, overrun),
            Some((BAD_ACCESS, rw)),
        ),
        case(
            "GetImage at the end",
            shm::GET_IMAGE,
            shm_get(SEGMENT, size),
            Some((BAD_ACCESS, rw)),
        ),
        case(
            "GetImage read-only",
            shm::GET_IMAGE,
            shm_get(RO_SEGMENT, 0),
            Some((BAD_ACCESS, ro)),
        ),
        case(
            "GetImage missing segment",
            shm::GET_IMAGE,
            shm_get(MISSING, 0),
            Some((BAD_SHM_SEG, MISSING.0)),
        ),
        case(
            "PutImage unaligned",
            shm::PUT_IMAGE,
            shm_put(SEGMENT, 2, (4, 4), full),
            Some((BAD_VALUE, 2)),
        ),
        case(
            "PutImage past the end",
            shm::PUT_IMAGE,
            shm_put(SEGMENT, past, (4, 4), full),
            Some((BAD_VALUE, past)),
        ),
        case(
            "PutImage overrun",
            shm::PUT_IMAGE,
            shm_put(SEGMENT, overrun, (4, 4), full),
            Some((BAD_VALUE, 4)),
        ),
        case(
            "PutImage at the end",
            shm::PUT_IMAGE,
            shm_put(SEGMENT, size, (4, 4), full),
            Some((BAD_VALUE, 4)),
        ),
        case(
            "PutImage read-only",
            shm::PUT_IMAGE,
            shm_put(RO_SEGMENT, 0, (4, 4), full),
            None,
        ),
        case(
            "PutImage missing segment",
            shm::PUT_IMAGE,
            shm_put(MISSING, 0, (4, 4), full),
            Some((BAD_SHM_SEG, MISSING.0)),
        ),
        case(
            "PutImage src x past the image",
            shm::PUT_IMAGE,
            shm_put(SEGMENT, 0, (4, 4), (5, 0, 1, 1)),
            Some((BAD_VALUE, 5)),
        ),
        case(
            "PutImage src y past the image",
            shm::PUT_IMAGE,
            shm_put(SEGMENT, 0, (4, 4), (0, 5, 1, 1)),
            Some((BAD_VALUE, 5)),
        ),
        case(
            "PutImage src width past the image",
            shm::PUT_IMAGE,
            shm_put(SEGMENT, 0, (4, 4), (2, 0, 3, 1)),
            Some((BAD_VALUE, 3)),
        ),
        case(
            "PutImage src height past the image",
            shm::PUT_IMAGE,
            shm_put(SEGMENT, 0, (4, 4), (0, 2, 1, 3)),
            Some((BAD_VALUE, 3)),
        ),
        case(
            "PutImage empty image",
            shm::PUT_IMAGE,
            shm_put(SEGMENT, 0, (4, 0), (0, 0, 0, 0)),
            None,
        ),
        // 64 pixels a row (256 bytes), one row more than the segment holds.
        case(
            "PutImage total image overrun",
            shm::PUT_IMAGE,
            shm_put(
                SEGMENT,
                0,
                (64, u16::try_from(size / 256 + 1).unwrap()),
                image,
            ),
            Some((BAD_VALUE, 64)),
        ),
    ]
}

#[test]
fn shm_range_errors_match_xorg() {
    // Mutation: report an overrun as BadValue, drop the alignment check, or check PutImage's source rectangle before the image's rows; a row fails.
    let probe = shm_fixture();
    let mut sizing = probe;
    install_segment(&mut sizing.state, SEGMENT, false);
    let size = u32::try_from(sizing.state.mit_shm_segments[&SEGMENT.0].size).unwrap();
    for case in range_cases(size) {
        let mut fixture = shm_fixture();
        install_segment(&mut fixture.state, SEGMENT, false);
        install_segment(&mut fixture.state, RO_SEGMENT, true);
        // A 4x4 depth-24 GetImage reply, so its size check is reached.
        let mut reply = vec![24];
        reply.resize(32 + 64, 0);
        fixture.backend.get_image_reply = Some(reply);
        let bytes = dispatch(&mut fixture, case.minor, &case.body, None, 1);
        match case.error {
            Some((code, value)) => {
                assert_eq!(bytes.len(), 32, "{}: one error packet", case.label);
                assert_eq!(bytes[0], 0, "{}: an error", case.label);
                assert_eq!(bytes[1], code, "{}: error code", case.label);
                assert_eq!(
                    u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
                    value,
                    "{}: error value",
                    case.label
                );
                assert_eq!(bytes[8], case.minor, "{}: minor", case.label);
                assert!(
                    fixture.state.resources.pixmap(NEW_PIXMAP).is_none(),
                    "{}: no pixmap",
                    case.label
                );
            }
            None => assert!(bytes.is_empty(), "{}: Success, got {bytes:?}", case.label),
        }
    }
}
