use super::{
    extensions::{ExtensionHook, RecordedExtensionCheck, RecordingExtensionPolicy},
    *,
};
use crate::access::{
    AccessMode, Decision, DenyError, RequestOpcodes, ResourceKind, ResourceTarget,
};
use std::os::fd::IntoRawFd;

const CLIENT: ClientId = ClientId(1);
const WINDOW: ResourceId = ResourceId(0x0010_8001);
const OTHER_WINDOW: ResourceId = ResourceId(0x0010_8002);
const CURSOR: ResourceId = ResourceId(0x0090_8001);
const BARRIER_ONE: ResourceId = ResourceId(0x00a0_8001);
const BARRIER_TWO: ResourceId = ResourceId(0x00a0_8002);
const POINTER_SHM: ResourceId = ResourceId(0x0010_8008);
const POINTER_COLORMAP: ResourceId = ResourceId(0x0010_8009);
const XI_MAJOR: u8 = 137;
const XKB_MAJOR: u8 = 136;
const XTEST_MAJOR: u8 = 146;
type InputRequestRow = (&'static str, u8, u8);
const R_XI1_SELECT: InputRequestRow = ("XI1 SelectExtensionEvent", XI_MAJOR, 6);
const R_XI1_GET_SEL: InputRequestRow = ("XI1 GetSelectedExtensionEvents", XI_MAJOR, 7);
const R_XI1_CDP: InputRequestRow = ("XI1 ChangeDeviceDontPropagateList", XI_MAJOR, 8);
const R_XI1_DGP: InputRequestRow = ("XI1 GetDeviceDontPropagateList", XI_MAJOR, 9);
const R_XI1_GRAB: InputRequestRow = ("XI1 GrabDevice", XI_MAJOR, 13);
const R_XI1_GRAB_KEY: InputRequestRow = ("XI1 GrabDeviceKey", XI_MAJOR, 15);
const R_XI1_UNGRAB_KEY: InputRequestRow = ("XI1 UngrabDeviceKey", XI_MAJOR, 16);
const R_XI1_GRAB_BTN: InputRequestRow = ("XI1 GrabDeviceButton", XI_MAJOR, 17);
const R_XI1_UNGRAB_BTN: InputRequestRow = ("XI1 UngrabDeviceButton", XI_MAJOR, 18);
const R_XI1_FOCUS: InputRequestRow = ("XI1 SetDeviceFocus", XI_MAJOR, 21);
const R_XI2_SELECT: InputRequestRow = ("XI2 SelectEvents", XI_MAJOR, 46);
const R_XI2_GET_SEL: InputRequestRow = ("XI2 GetSelectedEvents", XI_MAJOR, 60);
const R_XI2_QUERY: InputRequestRow = ("XI2 QueryPointer", XI_MAJOR, 40);
const R_XI2_GRAB: InputRequestRow = ("XI2 GrabDevice window and cursor", XI_MAJOR, 51);
const R_XI2_PGRAB: InputRequestRow = ("XI2 PassiveGrabDevice window and cursor", XI_MAJOR, 54);
const R_XI2_PUNGRAB: InputRequestRow = ("XI2 PassiveUngrabDevice", XI_MAJOR, 55);
const R_XI2_WARP: InputRequestRow = ("XI2 WarpPointer destination then source", XI_MAJOR, 41);
const R_XI2_CHANGE_CUR: InputRequestRow = ("XI2 ChangeCursor window and cursor", XI_MAJOR, 42);
const R_XI2_FOCUS: InputRequestRow = ("XI2 SetFocus", XI_MAJOR, 49);
const R_XI2_SET_CLIENT: InputRequestRow = ("XI2 SetClientPointer", XI_MAJOR, 44);
const R_XI2_GET_CLIENT: InputRequestRow = ("XI2 GetClientPointer", XI_MAJOR, 45);
const R_XI2_BARRIER: InputRequestRow = ("XI2 BarrierReleasePointer", XI_MAJOR, 61);
const R_XKB_BELL: InputRequestRow = ("XKB Bell", XKB_MAJOR, 3);
const R_XTEST_CMP: InputRequestRow = ("XTEST CompareCursor", XTEST_MAJOR, 1);
const R_XTEST_FAKE: InputRequestRow = ("XTEST FakeInput root", XTEST_MAJOR, 2);

#[derive(Clone, Copy, Debug)]
struct ExpectedInputCheck {
    id: ResourceId,
    kind: ResourceKind,
    mode: AccessMode,
}

struct InputRequestCase {
    label: &'static str,
    major: u8,
    minor: u8,
    body: Vec<u8>,
    checks: Vec<ExpectedInputCheck>,
}

fn ic(
    (label, major, minor): InputRequestRow,
    body: Vec<u8>,
    checks: Vec<ExpectedInputCheck>,
) -> InputRequestCase {
    InputRequestCase {
        label,
        major,
        minor,
        body,
        checks,
    }
}

fn put_u16(body: &mut Vec<u8>, value: u16) {
    body.extend_from_slice(&value.to_le_bytes());
}

fn put_u32(body: &mut Vec<u8>, value: u32) {
    body.extend_from_slice(&value.to_le_bytes());
}

fn input_words(values: &[u32]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

fn input_fixture() -> (ServerState, UnixStream, RecordingBackend) {
    let mut state = ServerState::new();
    let peer = install_client(&mut state, CLIENT.0);
    for (id, host) in [(WINDOW, 0x0040_8001), (OTHER_WINDOW, 0x0040_8002)] {
        state.resources.create_window(
            CLIENT,
            CreateWindowRequest {
                depth: 24,
                window: id,
                parent: ROOT_WINDOW,
                width: 80,
                height: 60,
                class: 1,
                visual: crate::resources::ROOT_VISUAL,
                ..Default::default()
            },
        );
        let window = state.resources.window_mut(id).expect("test window");
        window.host_xid = Some(crate::backend::WindowHandle::from_raw_for_test(host));
        window.map_state = crate::resources::MapState::Viewable;
    }
    state.resources.create_cursor(CLIENT, CURSOR);
    for (id, event_id) in [(BARRIER_ONE, 7), (BARRIER_TWO, 9)] {
        state.pointer_barriers.insert(
            id.0,
            crate::server::PointerBarrier {
                owner: CLIENT,
                window: WINDOW,
                x1: 1,
                y1: 2,
                x2: 3,
                y2: 4,
                directions: 0,
                devices: vec![2],
                hit: true,
                seen: true,
                event_id,
                release_event_id: 0,
                last_timestamp: 1,
            },
        );
    }
    (state, peer, RecordingBackend::new())
}

fn input_request(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    peer: &mut UnixStream,
    major: u8,
    minor: u8,
    body: &[u8],
) -> Vec<u8> {
    super::access_resources::resource_request(state, backend, peer, 1, major, minor, body)
}

fn seed_client_pointer_resource(state: &mut ServerState, id: ResourceId, kind: ResourceKind) {
    match kind {
        ResourceKind::ShmSegment => {
            let path = std::env::temp_dir().join(format!(
                "yserver-access-input-shm-{}-{}",
                std::process::id(),
                id.0
            ));
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&path)
                .expect("create input access test SHM file");
            file.set_len(4096).expect("size input access test SHM file");
            let segment = crate::server::MitShmSegment::from_fd(CLIENT, file.into_raw_fd(), false)
                .expect("map input access test SHM file");
            assert!(state.mit_shm_segments.insert(id.0, segment).is_none());
            std::fs::remove_file(path).expect("remove input access test SHM file");
        }
        ResourceKind::Colormap => {
            state
                .resources
                .create_colormap(CLIENT, id, crate::resources::ROOT_VISUAL)
        }
        _ => unreachable!("not a client-pointer extension resource: {kind:?}"),
    }
}

fn client_pointer_resource_snapshot(
    state: &ServerState,
    id: ResourceId,
    kind: ResourceKind,
) -> String {
    match kind {
        ResourceKind::ShmSegment => format!(
            "{:?}",
            state.mit_shm_segments.get(&id.0).map(|segment| (
                segment.owner,
                segment.size,
                segment.read_only
            ))
        ),
        ResourceKind::Colormap => format!("{:?}", state.resources.colormap(id)),
        _ => unreachable!("not a client-pointer extension resource: {kind:?}"),
    }
}

#[test]
fn xi_client_pointer_extension_resource_is_asked() {
    // Mutation: keep the eight-kind resolver list; SHM and colormap rows must fail to record their
    // expected access checks.
    let rows = [
        (
            44,
            POINTER_SHM,
            ResourceKind::ShmSegment,
            AccessMode::MANAGE,
        ),
        (
            44,
            POINTER_COLORMAP,
            ResourceKind::Colormap,
            AccessMode::MANAGE,
        ),
        (
            45,
            POINTER_SHM,
            ResourceKind::ShmSegment,
            AccessMode::GET_ATTR,
        ),
        (
            45,
            POINTER_COLORMAP,
            ResourceKind::Colormap,
            AccessMode::GET_ATTR,
        ),
    ];

    for (minor, id, kind, mode) in rows {
        let body = if minor == 44 {
            input_words(&[id.0, 2])
        } else {
            input_words(&[id.0])
        };
        let (mut state, mut peer, mut backend) = input_fixture();
        seed_client_pointer_resource(&mut state, id, kind);
        let calls = Arc::new(Mutex::new(Vec::new()));
        state.install_access_policy(Arc::new(RecordingExtensionPolicy::new(
            Decision::Allow,
            Arc::clone(&calls),
        )));
        let actual = input_request(&mut state, &mut backend, &mut peer, XI_MAJOR, minor, &body);

        let (mut no_policy_state, mut no_policy_peer, mut no_policy_backend) = input_fixture();
        seed_client_pointer_resource(&mut no_policy_state, id, kind);
        let expected = input_request(
            &mut no_policy_state,
            &mut no_policy_backend,
            &mut no_policy_peer,
            XI_MAJOR,
            minor,
            &body,
        );
        assert_eq!(actual, expected, "XI minor {minor} changed its reply");

        let calls = calls.lock().unwrap();
        let resource_calls: Vec<_> = calls
            .iter()
            .filter(|call| call.hook == ExtensionHook::Resource)
            .collect();
        assert_eq!(resource_calls.len(), 1, "XI minor {minor}, {kind:?}");
        assert_input_check(
            resource_calls[0],
            check(id, kind, mode),
            XI_MAJOR,
            minor,
            "XI client pointer extension resource",
        );
    }

    let error = DenyError {
        code: x11::error::BAD_ACCESS,
        value: 0xA55A_1234,
    };
    for (minor, id, kind, mode) in rows {
        let body = if minor == 44 {
            input_words(&[id.0, 2])
        } else {
            input_words(&[id.0])
        };
        let (mut state, mut peer, mut backend) = input_fixture();
        seed_client_pointer_resource(&mut state, id, kind);
        let calls = Arc::new(Mutex::new(Vec::new()));
        state.install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
            Arc::clone(&calls),
            id,
            Some(mode),
            error,
        )));
        let input_before = input_state_snapshot(&state);
        let resource_before = client_pointer_resource_snapshot(&state, id, kind);
        let backend_before = backend.calls();

        let bytes = input_request(&mut state, &mut backend, &mut peer, XI_MAJOR, minor, &body);

        assert_policy_error(
            &bytes,
            XI_MAJOR,
            minor,
            error,
            "XI client pointer extension resource",
        );
        assert_eq!(
            input_state_snapshot(&state),
            input_before,
            "XI minor {minor} changed input state"
        );
        assert_eq!(
            client_pointer_resource_snapshot(&state, id, kind),
            resource_before,
            "XI minor {minor} changed {kind:?}"
        );
        assert_eq!(
            backend.calls(),
            backend_before,
            "XI minor {minor} reached backend"
        );
        let calls = calls.lock().unwrap();
        let resource_calls: Vec<_> = calls
            .iter()
            .filter(|call| call.hook == ExtensionHook::Resource)
            .collect();
        assert_eq!(resource_calls.len(), 1, "XI minor {minor}, {kind:?}");
        assert_input_check(
            resource_calls[0],
            check(id, kind, mode),
            XI_MAJOR,
            minor,
            "XI client pointer extension resource",
        );
    }
}

fn enable_xkb(state: &mut ServerState, backend: &mut RecordingBackend, peer: &mut UnixStream) {
    let reply = input_request(state, backend, peer, XKB_MAJOR, 0, &[1, 0, 0, 0]);
    assert_eq!(reply.len(), 32, "XkbUseExtension setup reply");
    assert!(crate::core_loop::xkb_select::xkb_initialized(state, CLIENT));
}

fn check(id: ResourceId, kind: ResourceKind, mode: AccessMode) -> ExpectedInputCheck {
    ExpectedInputCheck { id, kind, mode }
}

fn input_request_cases() -> Vec<InputRequestCase> {
    use AccessMode as M;
    use ResourceKind::{Cursor, PointerBarrier, Window};

    let class = ((crate::xinput::DEVICEID_XTEST_POINTER as u32) << 8) | 82;
    let mut xi1_select = input_words(&[WINDOW.0]);
    put_u16(&mut xi1_select, 1);
    put_u16(&mut xi1_select, 0);
    put_u32(&mut xi1_select, class);

    let mut dont_prop = input_words(&[WINDOW.0]);
    put_u16(&mut dont_prop, 1);
    dont_prop.extend_from_slice(&[0, 0]); // AddToList, pad
    put_u32(&mut dont_prop, class);

    let mut xi1_grab_device = input_words(&[WINDOW.0, 0]);
    put_u16(&mut xi1_grab_device, 0); // class count
    xi1_grab_device.extend_from_slice(&[1, 1, 1, 2, 0, 0]);

    let mut xi1_grab_key = input_words(&[WINDOW.0]);
    put_u16(&mut xi1_grab_key, 0); // class count
    put_u16(&mut xi1_grab_key, 0); // modifiers
    xi1_grab_key.extend_from_slice(&[255, 3, 8, 1, 1, 1, 0, 0]);

    let mut xi1_ungrab_key = input_words(&[WINDOW.0]);
    put_u16(&mut xi1_ungrab_key, 0); // modifiers
    xi1_ungrab_key.extend_from_slice(&[255, 8, 3, 0, 0, 0]);

    let mut xi1_grab_button = input_words(&[WINDOW.0]);
    xi1_grab_button.extend_from_slice(&[2, 255]); // devices
    put_u16(&mut xi1_grab_button, 0); // class count
    put_u16(&mut xi1_grab_button, 0); // modifiers
    xi1_grab_button.extend_from_slice(&[1, 1, 1, 1, 0, 0]);

    let mut xi1_ungrab_button = input_words(&[WINDOW.0]);
    put_u16(&mut xi1_ungrab_button, 0); // modifiers
    xi1_ungrab_button.extend_from_slice(&[255, 1, 2, 0, 0, 0]);

    let mut xi1_set_focus = input_words(&[WINDOW.0, 0]);
    xi1_set_focus.extend_from_slice(&[2, 3, 0, 0]); // RevertToParent, keyboard

    let mut xi2_set_focus = input_words(&[WINDOW.0, 0]);
    put_u16(&mut xi2_set_focus, 3); // master keyboard
    put_u16(&mut xi2_set_focus, 0);

    let mut xi2_select = input_words(&[WINDOW.0]);
    put_u16(&mut xi2_select, 1); // one event mask
    put_u16(&mut xi2_select, 0);
    put_u16(&mut xi2_select, 2); // master pointer
    put_u16(&mut xi2_select, 1); // one mask word
    put_u32(&mut xi2_select, 1);

    let mut xi2_query_pointer = input_words(&[WINDOW.0]);
    put_u16(&mut xi2_query_pointer, 2);
    put_u16(&mut xi2_query_pointer, 0);

    let mut xi2_grab = input_words(&[WINDOW.0, 0, CURSOR.0]);
    put_u16(&mut xi2_grab, 2); // master pointer
    xi2_grab.extend_from_slice(&[1, 1, 1, 0]);
    put_u16(&mut xi2_grab, 0); // mask length
    put_u16(&mut xi2_grab, 0);

    let mut xi2_passive_grab = input_words(&[0, WINDOW.0, CURSOR.0, 1]);
    put_u16(&mut xi2_passive_grab, 2); // master pointer
    put_u16(&mut xi2_passive_grab, 0); // modifiers
    put_u16(&mut xi2_passive_grab, 0); // mask length
    xi2_passive_grab.extend_from_slice(&[0, 1, 1, 1, 0, 0]);

    let mut xi2_passive_ungrab = input_words(&[WINDOW.0, 1]);
    put_u16(&mut xi2_passive_ungrab, 2); // master pointer
    put_u16(&mut xi2_passive_ungrab, 0); // modifiers
    xi2_passive_ungrab.extend_from_slice(&[0, 0, 0, 0]);

    let xi_warp = xi_warp_body(OTHER_WINDOW, WINDOW);

    let mut change_cur = input_words(&[WINDOW.0, CURSOR.0]);
    put_u16(&mut change_cur, 2);
    put_u16(&mut change_cur, 0);

    let mut set_client_ptr = input_words(&[WINDOW.0]);
    put_u16(&mut set_client_ptr, 2);
    put_u16(&mut set_client_ptr, 0);

    let mut xi_barrier = vec![0; 4];
    xi_barrier[..4].copy_from_slice(&1u32.to_le_bytes());
    put_u16(&mut xi_barrier, 2);
    put_u16(&mut xi_barrier, 0);
    put_u32(&mut xi_barrier, BARRIER_ONE.0);
    put_u32(&mut xi_barrier, 7);

    let mut xkb_bell = vec![0; 24];
    xkb_bell[..2].copy_from_slice(&0x0100u16.to_le_bytes()); // XkbUseCoreKbd
    xkb_bell[16..20].copy_from_slice(&0x1234_5678u32.to_le_bytes()); // name atom
    xkb_bell[20..24].copy_from_slice(&WINDOW.0.to_le_bytes());

    let mut xtest_fake_input = vec![0; 28];
    xtest_fake_input[0] = 6; // MotionNotify
    xtest_fake_input[8..12].copy_from_slice(&WINDOW.0.to_le_bytes());

    let window_body = input_words(&[WINDOW.0]);
    let get_win = vec![check(WINDOW, Window, M::GET_ATTR)];
    let manage_win = vec![check(WINDOW, Window, M::MANAGE)];
    let set_win = vec![check(WINDOW, Window, M::SET_ATTR)];
    let recv_win = vec![check(WINDOW, Window, M::RECEIVE)];
    let grab_checks = vec![
        check(WINDOW, Window, M::SET_ATTR),
        check(CURSOR, Cursor, M::USE),
    ];
    let warp_checks = vec![
        check(WINDOW, Window, M::GET_ATTR),
        check(OTHER_WINDOW, Window, M::GET_ATTR),
    ];
    let cchecks = vec![
        check(WINDOW, Window, M::SET_ATTR),
        check(CURSOR, Cursor, M::USE),
    ];
    let bar_read = vec![check(BARRIER_ONE, PointerBarrier, M::READ)];
    let cmp_checks = vec![
        check(WINDOW, Window, M::GET_ATTR),
        check(CURSOR, Cursor, M::READ),
    ];
    let cmp_body = input_words(&[WINDOW.0, CURSOR.0]);
    let cases = vec![
        ic(R_XI1_SELECT, xi1_select, recv_win.clone()),
        ic(R_XI1_GET_SEL, window_body.clone(), get_win.clone()),
        ic(R_XI1_CDP, dont_prop, set_win.clone()),
        ic(R_XI1_DGP, window_body.clone(), get_win.clone()),
        ic(R_XI1_GRAB, xi1_grab_device, set_win.clone()),
        ic(R_XI1_GRAB_KEY, xi1_grab_key, set_win.clone()),
        ic(R_XI1_UNGRAB_KEY, xi1_ungrab_key, set_win.clone()),
        ic(R_XI1_GRAB_BTN, xi1_grab_button, set_win.clone()),
        ic(R_XI1_UNGRAB_BTN, xi1_ungrab_button, set_win.clone()),
        ic(R_XI1_FOCUS, xi1_set_focus, set_win.clone()),
        ic(R_XI2_SELECT, xi2_select, recv_win.clone()),
        ic(R_XI2_GET_SEL, window_body.clone(), get_win.clone()),
        ic(R_XI2_QUERY, xi2_query_pointer, get_win.clone()),
        ic(R_XI2_GRAB, xi2_grab, grab_checks.clone()),
        ic(R_XI2_PGRAB, xi2_passive_grab, grab_checks.clone()),
        ic(R_XI2_PUNGRAB, xi2_passive_ungrab, set_win.clone()),
        ic(R_XI2_WARP, xi_warp, warp_checks.clone()),
        ic(R_XI2_CHANGE_CUR, change_cur, cchecks.clone()),
        ic(R_XI2_FOCUS, xi2_set_focus, set_win.clone()),
        ic(R_XI2_SET_CLIENT, set_client_ptr, manage_win),
        ic(R_XI2_GET_CLIENT, window_body.clone(), get_win.clone()),
        ic(R_XI2_BARRIER, xi_barrier, bar_read.clone()),
        ic(R_XKB_BELL, xkb_bell, get_win.clone()),
        ic(R_XTEST_CMP, cmp_body, cmp_checks.clone()),
        ic(R_XTEST_FAKE, xtest_fake_input, get_win.clone()),
    ];
    cases
}

fn assert_input_check(
    call: &RecordedExtensionCheck,
    expected: ExpectedInputCheck,
    major: u8,
    minor: u8,
    label: &str,
) {
    let target = ResourceTarget {
        id: expected.id,
        kind: expected.kind,
        creation: None,
        owner: None,
    };
    assert_eq!(call.hook, ExtensionHook::Resource, "{label}");
    assert_eq!(call.client, CLIENT, "{label}");
    assert_eq!(call.target, Some(target), "{label}");
    assert_eq!(call.mode, Some(expected.mode), "{label}");
    assert_eq!(call.owner, Some(CLIENT), "{label}");
    assert_eq!(call.target_owner, Some(CLIENT), "{label}");
    assert_eq!(
        call.request,
        Some(RequestOpcodes { major, minor }),
        "{label}"
    );
}

fn xi_warp_body(source: ResourceId, destination: ResourceId) -> Vec<u8> {
    let mut body = input_words(&[source.0, destination.0, 0, 0]);
    put_u16(&mut body, 0);
    put_u16(&mut body, 0);
    put_u32(&mut body, 0);
    put_u32(&mut body, 0);
    put_u16(&mut body, 2);
    put_u16(&mut body, 0);
    body
}

fn invalid_later_input_cases() -> Vec<InputRequestCase> {
    let invalid_cursor = ResourceId(0xDEAD_8101);
    let invalid_source = ResourceId(0xDEAD_8102);
    let window_check = vec![check(WINDOW, ResourceKind::Window, AccessMode::GET_ATTR)];
    let compare = (
        "CompareCursor denied window before BadCursor",
        XTEST_MAJOR,
        1,
    );
    let warp = (
        "XIWarpPointer denied destination before BadWindow source",
        XI_MAJOR,
        41,
    );
    let compare_body = input_words(&[WINDOW.0, invalid_cursor.0]);
    let warp_body = xi_warp_body(invalid_source, WINDOW);
    vec![
        ic(compare, compare_body, window_check.clone()),
        ic(warp, warp_body, window_check),
    ]
}

#[test]
fn earlier_operand_denial_precedes_later_invalid_operand() {
    // Mutations: move CompareCursor's window check after cursor lookup or XIWarpPointer's
    // destination check after source lookup; each policy row must then lose to the later error.
    let error = DenyError {
        code: x11::error::BAD_ACCESS,
        value: 0xA55A_1234,
    };
    for row in invalid_later_input_cases() {
        let denied = row.checks[0];
        let (no_policy_code, invalid_id) = if row.major == XTEST_MAJOR {
            (
                x11::error::BAD_CURSOR,
                u32::from_le_bytes(row.body[4..8].try_into().unwrap()),
            )
        } else {
            (
                x11::error::BAD_WINDOW,
                u32::from_le_bytes(row.body[0..4].try_into().unwrap()),
            )
        };
        let no_policy_error = DenyError {
            code: no_policy_code,
            value: invalid_id,
        };
        let (mut state, mut peer, mut backend) = input_fixture();
        let before = input_state_snapshot(&state);
        let backend_before = backend.calls();
        let bytes = input_request(
            &mut state,
            &mut backend,
            &mut peer,
            row.major,
            row.minor,
            &row.body,
        );
        assert_policy_error(&bytes, row.major, row.minor, no_policy_error, row.label);
        assert_eq!(
            input_state_snapshot(&state),
            before,
            "{} changed state without policy",
            row.label
        );
        assert_eq!(
            backend.calls(),
            backend_before,
            "{} reached backend without policy",
            row.label
        );

        let (mut state, mut peer, mut backend) = input_fixture();
        let calls = Arc::new(Mutex::new(Vec::new()));
        state.install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
            Arc::clone(&calls),
            denied.id,
            Some(denied.mode),
            error,
        )));
        let before = input_state_snapshot(&state);
        let backend_before = backend.calls();
        let bytes = input_request(
            &mut state,
            &mut backend,
            &mut peer,
            row.major,
            row.minor,
            &row.body,
        );
        assert_policy_error(&bytes, row.major, row.minor, error, row.label);
        assert_eq!(
            input_state_snapshot(&state),
            before,
            "{} changed state",
            row.label
        );
        assert_eq!(
            backend.calls(),
            backend_before,
            "{} reached backend",
            row.label
        );
        let calls = calls.lock().unwrap();
        let resource_calls: Vec<_> = calls
            .iter()
            .filter(|call| call.hook == ExtensionHook::Resource)
            .collect();
        assert_eq!(resource_calls.len(), 1, "{}: check count", row.label);
        assert_input_check(resource_calls[0], denied, row.major, row.minor, row.label);
    }
}

#[test]
fn input_operands_ask_every_site_in_table_order() {
    // Mutation: omit any listed check or read XKB Bell's name atom as its window; the exact target
    // table must fail.
    for case in input_request_cases() {
        let (mut state, mut peer, mut backend) = input_fixture();
        if case.major == XKB_MAJOR {
            enable_xkb(&mut state, &mut backend, &mut peer);
        }
        let calls = Arc::new(Mutex::new(Vec::new()));
        state.install_access_policy(Arc::new(RecordingExtensionPolicy::new(
            Decision::Allow,
            Arc::clone(&calls),
        )));
        let _reply = input_request(
            &mut state,
            &mut backend,
            &mut peer,
            case.major,
            case.minor,
            &case.body,
        );
        let calls = calls.lock().unwrap();
        let resource_calls: Vec<_> = calls
            .iter()
            .filter(|call| call.hook == ExtensionHook::Resource)
            .collect();
        assert_eq!(resource_calls.len(), case.checks.len(), "{}", case.label);
        for (call, expected) in resource_calls.into_iter().zip(case.checks) {
            assert_input_check(call, expected, case.major, case.minor, case.label);
        }
    }
}

fn assert_policy_error(bytes: &[u8], major: u8, minor: u8, error: DenyError, label: &str) {
    assert_eq!(
        bytes.len(),
        32,
        "{label}: policy denial must be one X error"
    );
    assert_eq!(bytes[0], 0, "{label}: X error response");
    assert_eq!(bytes[1], error.code, "{label}: policy error code");
    assert_eq!(
        u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
        error.value,
        "{label}: policy error value"
    );
    assert_eq!(
        u16::from_le_bytes(bytes[8..10].try_into().unwrap()),
        u16::from(minor),
        "{label}: minor opcode"
    );
    assert_eq!(bytes[10], major, "{label}: major opcode");
}

fn input_state_snapshot(state: &ServerState) -> String {
    let client = state.clients.get(&CLIENT.0).expect("test client");
    format!(
        "{:?}{:?}{:?}",
        (
            client.xi1_event_classes.clone(),
            client.xi1_window_event_classes.clone(),
            client.xi2_masks.clone(),
            state.xi1_window_dont_propagate.clone(),
            state.xi1_device_focus.clone(),
            state.xi1_active_grabs.clone(),
            state.xi1_passive_grabs.clone(),
            state.xi2_pointer_grabs.clone(),
            state.xi2_keyboard_grabs.clone(),
            state.active_pointer_grab,
            state.active_keyboard_grab,
            state.button_grabs.clone(),
        ),
        (
            state.key_grabs.clone(),
            state.pointer_barriers.clone(),
            state.core_focus,
            state.pointer_root,
        ),
        (
            state.buttons_down,
            state.keys_down,
            state.pointer_motion_history.len(),
            state
                .resources
                .window(WINDOW)
                .and_then(|window| window.cursor),
            state
                .resources
                .window(OTHER_WINDOW)
                .and_then(|window| window.cursor),
        )
    )
}

#[test]
fn input_operands_deny_each_site_without_state_or_backend_effects() {
    // Mutation: ignore a resource-policy denial, or call XKB Bell's backend proxy before its check;
    // these denial assertions must fail.
    let error = DenyError {
        code: x11::error::BAD_ACCESS,
        value: 0xA55A_1234,
    };
    for case in input_request_cases() {
        for (denied_index, denied) in case.checks.iter().copied().enumerate() {
            let (mut state, mut peer, mut backend) = input_fixture();
            if case.major == XKB_MAJOR {
                enable_xkb(&mut state, &mut backend, &mut peer);
            }
            let calls = Arc::new(Mutex::new(Vec::new()));
            state.install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
                Arc::clone(&calls),
                denied.id,
                Some(denied.mode),
                error,
            )));
            let before = input_state_snapshot(&state);
            let backend_before = backend.calls();
            let bytes = input_request(
                &mut state,
                &mut backend,
                &mut peer,
                case.major,
                case.minor,
                &case.body,
            );
            assert_policy_error(&bytes, case.major, case.minor, error, case.label);
            assert_eq!(input_state_snapshot(&state), before, "{}", case.label);
            assert_eq!(
                backend.calls(),
                backend_before,
                "{} reached backend",
                case.label
            );
            match (case.major, case.minor) {
                (XKB_MAJOR, 3) => assert!(
                    !backend.calls().iter().any(|call| matches!(
                        call,
                        crate::backend::recording::RecordedCall::XkbProxy { minor: 3, .. }
                    )),
                    "XKB Bell reached xkb_proxy before denial"
                ),
                (XI_MAJOR, 40) => assert!(
                    !backend
                        .calls()
                        .contains(&crate::backend::recording::RecordedCall::QueryPointer),
                    "XI2 QueryPointer reached the backend before denial"
                ),
                _ => {}
            }
            let calls = calls.lock().unwrap();
            let resource_calls: Vec<_> = calls
                .iter()
                .filter(|call| call.hook == ExtensionHook::Resource)
                .collect();
            assert_eq!(resource_calls.len(), denied_index + 1, "{}", case.label);
            for (call, expected) in resource_calls.into_iter().zip(case.checks.iter().copied()) {
                assert_input_check(call, expected, case.major, case.minor, case.label);
            }
        }
    }
}

fn barrier_release_body(entries: &[(ResourceId, u32)]) -> Vec<u8> {
    let mut body = Vec::with_capacity(4 + entries.len() * 12);
    put_u32(&mut body, u32::try_from(entries.len()).unwrap());
    for (barrier, event_id) in entries {
        put_u16(&mut body, 2);
        put_u16(&mut body, 0);
        put_u32(&mut body, barrier.0);
        put_u32(&mut body, *event_id);
    }
    body
}

#[test]
fn barrier_release_denied_second_entry_keeps_first_release() {
    // Mutation: check every barrier before releasing any; the first release must still be recorded.
    let (mut state, mut peer, mut backend) = input_fixture();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let error = DenyError {
        code: x11::error::BAD_ACCESS,
        value: BARRIER_TWO.0,
    };
    state.install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
        Arc::clone(&calls),
        BARRIER_TWO,
        Some(AccessMode::READ),
        error,
    )));
    let bytes = input_request(
        &mut state,
        &mut backend,
        &mut peer,
        XI_MAJOR,
        61,
        &barrier_release_body(&[(BARRIER_ONE, 7), (BARRIER_TWO, 9)]),
    );
    assert_policy_error(&bytes, XI_MAJOR, 61, error, "second barrier denial");
    assert_eq!(state.pointer_barriers[&BARRIER_ONE.0].release_event_id, 7);
    assert_eq!(state.pointer_barriers[&BARRIER_TWO.0].release_event_id, 0);
    assert_eq!(
        backend.calls(),
        Vec::<crate::backend::recording::RecordedCall>::new()
    );
    let calls = calls.lock().unwrap();
    let resource_calls: Vec<_> = calls
        .iter()
        .filter(|call| call.hook == ExtensionHook::Resource)
        .collect();
    assert_eq!(resource_calls.len(), 2);
    assert_input_check(
        resource_calls[0],
        check(BARRIER_ONE, ResourceKind::PointerBarrier, AccessMode::READ),
        XI_MAJOR,
        61,
        "first barrier",
    );
    assert_input_check(
        resource_calls[1],
        check(BARRIER_TWO, ResourceKind::PointerBarrier, AccessMode::READ),
        XI_MAJOR,
        61,
        "second barrier",
    );
}

#[test]
fn set_client_ptr_unresolved_id_is_not_asked() {
    // Mutation: ask the policy for an unresolved resource id; this must become an unexpected
    // resource check.
    let (mut state, mut peer, mut backend) = input_fixture();
    let calls = Arc::new(Mutex::new(Vec::new()));
    state.install_access_policy(Arc::new(RecordingExtensionPolicy::new(
        Decision::Allow,
        Arc::clone(&calls),
    )));
    let unknown = ResourceId(0xdead_8001);
    let body = input_words(&[unknown.0, 2]);
    let bytes = input_request(&mut state, &mut backend, &mut peer, XI_MAJOR, 44, &body);
    assert!(
        bytes.is_empty(),
        "XISetClientPointer remains a no-op: {bytes:02x?}"
    );
    assert!(
        calls
            .lock()
            .unwrap()
            .iter()
            .all(|call| call.hook != ExtensionHook::Resource),
        "unresolved id was asked for resource access"
    );
}

#[test]
fn set_client_ptr_no_policy_remains_a_noop() {
    // Mutation: emit an X error for a valid XISetClientPointer request; the empty-output assertion
    // must fail.
    let (mut state, mut peer, mut backend) = input_fixture();
    let body = input_words(&[WINDOW.0, 2]);
    let before = input_state_snapshot(&state);
    let backend_before = backend.calls();
    let bytes = input_request(&mut state, &mut backend, &mut peer, XI_MAJOR, 44, &body);
    assert!(
        bytes.is_empty(),
        "XISetClientPointer reply/error: {bytes:02x?}"
    );
    assert_eq!(input_state_snapshot(&state), before);
    assert_eq!(backend.calls(), backend_before);
}
