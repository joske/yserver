use super::{
    extensions::{ExtensionHook, RecordedExtensionCheck, RecordingExtensionPolicy},
    *,
};
use crate::access::{
    AccessMode, Decision, DenyError, RequestOpcodes, ResourceKind, ResourceTarget,
};
use std::sync::{Arc, Mutex};
use yserver_protocol::x11::xfixes as x11xfixes;

const A: ClientId = ClientId(1);
const B: ClientId = ClientId(2);
const D: ClientId = ClientId(3);
const A_BASE: u32 = 0x0010_0000;
const B_BASE: u32 = 0x0020_0000;
const XID_MASK: u32 = 0x000f_ffff;

fn words(values: &[u32]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

fn request(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    client: ClientId,
    peer: &mut UnixStream,
    sequence: u16,
    opcode: u8,
    data: u8,
    body: &[u8],
) -> Vec<u8> {
    process_request(
        state,
        backend,
        client,
        SequenceNumber(sequence),
        RequestHeader {
            opcode,
            data,
            length_units: u32::try_from(1 + body.len().div_ceil(4)).unwrap(),
        },
        body,
        None,
    )
    .expect("cursor lifecycle request through process_request");
    read_all_available(peer)
}

fn set_base(state: &mut ServerState, client: ClientId, base: u32) {
    let client_state = state.clients.get_mut(&client.0).unwrap();
    client_state.resource_id_base = base;
    client_state.resource_id_mask = XID_MASK;
}

fn create_cursor(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    client: ClientId,
    peer: &mut UnixStream,
    sequence: &mut u16,
    base: u32,
) -> (ResourceId, u32) {
    let pixmap = ResourceId(base + 1);
    let cursor = ResourceId(base + 2);
    let bytes = request(
        state,
        backend,
        client,
        peer,
        *sequence,
        53,
        24,
        &words(&[pixmap.0, ROOT_WINDOW.0, 8 | (8 << 16)]),
    );
    assert_no_error(&bytes, "cursor fixture CreatePixmap");
    *sequence += 1;

    let bytes = request(
        state,
        backend,
        client,
        peer,
        *sequence,
        93,
        0,
        &words(&[cursor.0, pixmap.0, 0, 0, 0, 0, 0]),
    );
    assert_no_error(&bytes, "cursor fixture CreateCursor");
    *sequence += 1;
    let host = state.resources.cursor_host_xid(cursor).unwrap();
    (cursor, host)
}

fn create_window(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    peer: &mut UnixStream,
    sequence: &mut u16,
    window: ResourceId,
) {
    let bytes = request(
        state,
        backend,
        B,
        peer,
        *sequence,
        1,
        24,
        &words(&[
            window.0,
            ROOT_WINDOW.0,
            0,
            8 | (8 << 16),
            0,
            crate::resources::ROOT_VISUAL.0,
            0,
        ]),
    );
    assert_no_error(&bytes, "cursor fixture CreateWindow");
    *sequence += 1;
}

fn set_window_cursor(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    peer: &mut UnixStream,
    sequence: &mut u16,
    window: ResourceId,
    cursor: ResourceId,
) -> Vec<u8> {
    let bytes = request(
        state,
        backend,
        B,
        peer,
        *sequence,
        2,
        0,
        &words(&[window.0, 0x4000, cursor.0]),
    );
    *sequence += 1;
    bytes
}

fn cursor_name_body(cursor: ResourceId, name: &[u8]) -> Vec<u8> {
    let mut body = cursor.0.to_le_bytes().to_vec();
    body.extend_from_slice(&u16::try_from(name.len()).unwrap().to_le_bytes());
    body.extend_from_slice(&[0, 0]);
    body.extend_from_slice(name);
    body.resize(body.len().div_ceil(4) * 4, 0);
    body
}

fn pointer_barrier_body(barrier: ResourceId, window: ResourceId) -> Vec<u8> {
    let mut body = words(&[barrier.0, window.0]);
    for value in [0i16, 0, 1, 0] {
        body.extend_from_slice(&value.to_le_bytes());
    }
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&[0; 4]);
    body
}

fn active_cursor_image(host_xid: u32) -> crate::backend::ActiveCursorImage {
    crate::backend::ActiveCursorImage {
        host_xid,
        width: 1,
        height: 1,
        hot_x: 0,
        hot_y: 0,
        x: 3,
        y: 4,
        serial: 1,
        bgra_bytes: Arc::new(vec![0, 0, 0xff, 0xff]),
    }
}

fn assert_error(bytes: &[u8], code: u8, value: u32, major: u8, minor: u8, sequence: u16) {
    assert_eq!(
        bytes.len(),
        32,
        "access denial is an error, without reply bytes"
    );
    assert_eq!(bytes[0], 0, "X11 error packet");
    assert_eq!(bytes[1], code, "X11 error code");
    assert_eq!(u16::from_le_bytes([bytes[2], bytes[3]]), sequence);
    assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), value);
    assert_eq!(u16::from_le_bytes([bytes[8], bytes[9]]), u16::from(minor));
    assert_eq!(bytes[10], major);
}

fn xfixes_request_checks(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    client: ClientId,
    peer: &mut UnixStream,
    sequence: u16,
    minor: u8,
    body: &[u8],
    calls: &Arc<Mutex<Vec<RecordedExtensionCheck>>>,
    expected: &[(ResourceTarget, AccessMode)],
) {
    calls.lock().unwrap().clear();
    let expected = expected
        .iter()
        .map(|(target, mode)| {
            (
                client,
                Some(RequestOpcodes { major: 140, minor }),
                *target,
                *mode,
                target
                    .owner
                    .or_else(|| state.access_resource_owner(target.id, target.kind)),
            )
        })
        .collect::<Vec<_>>();
    let bytes = request(state, backend, client, peer, sequence, 140, minor, body);
    assert_no_error(&bytes, "allow-policy XFIXES request");
    let actual = calls
        .lock()
        .unwrap()
        .iter()
        .filter(|call| call.hook == ExtensionHook::Resource)
        .map(|call| {
            (
                call.client,
                call.request,
                call.target.unwrap(),
                call.mode.unwrap(),
                call.target_owner,
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(actual, expected, "XFIXES minor {minor} resource checks");
}

#[test]
fn freed_displayed_cursor_record_lives_until_release() {
    // Mutation: drop the record in free_cursor; keep it after release.
    let mut state = ServerState::new();
    let mut a_peer = install_client(&mut state, A.0);
    let mut b_peer = install_client(&mut state, B.0);
    set_base(&mut state, A, A_BASE);
    set_base(&mut state, B, B_BASE);
    let mut backend = RecordingBackend::new();
    let mut a_sequence = 1;
    let mut b_sequence = 1;
    let (cursor, host) = create_cursor(
        &mut state,
        &mut backend,
        A,
        &mut a_peer,
        &mut a_sequence,
        A_BASE,
    );
    let window = ResourceId(B_BASE + 1);
    create_window(
        &mut state,
        &mut backend,
        &mut b_peer,
        &mut b_sequence,
        window,
    );
    assert_no_error(
        &set_window_cursor(
            &mut state,
            &mut backend,
            &mut b_peer,
            &mut b_sequence,
            window,
            cursor,
        ),
        "B sets A's cursor",
    );

    assert_no_error(
        &request(
            &mut state,
            &mut backend,
            A,
            &mut a_peer,
            a_sequence,
            95,
            0,
            &cursor.0.to_le_bytes(),
        ),
        "A frees the displayed cursor XID",
    );
    assert_eq!(state.resources.cursor_host_origin(host), Some((cursor, A)));

    assert_no_error(
        &set_window_cursor(
            &mut state,
            &mut backend,
            &mut b_peer,
            &mut b_sequence,
            window,
            ResourceId(0),
        ),
        "B clears the window cursor",
    );
    assert_eq!(state.resources.cursor_host_origin(host), None);
    assert!(backend.calls().contains(&RecordedCall::FreeCursor(host)));
}

fn grab_pointer_with_cursor(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    client: ClientId,
    peer: &mut UnixStream,
    sequence: u16,
    cursor: ResourceId,
) -> Vec<u8> {
    let mut body = words(&[ROOT_WINDOW.0]);
    body.extend_from_slice(&0u16.to_le_bytes());
    body.extend_from_slice(&[0, 0]);
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&cursor.0.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    request(state, backend, client, peer, sequence, 26, 0, &body)
}

fn retained_cursor_image_access_checks(root_cursor_reset: bool) {
    let mut state = ServerState::new();
    let mut a_peer = install_client(&mut state, A.0);
    let mut b_peer = install_client(&mut state, B.0);
    set_base(&mut state, A, A_BASE);
    set_base(&mut state, B, B_BASE);
    state.xfixes_client_major.insert(B.0, 5);
    let mut backend = RecordingBackend::new();
    let mut a_sequence = 1;
    let mut b_sequence = 10;
    let (cursor, host) = create_cursor(
        &mut state,
        &mut backend,
        A,
        &mut a_peer,
        &mut a_sequence,
        A_BASE,
    );
    backend.alive_cursors.insert(host);

    if root_cursor_reset {
        assert_no_error(
            &request(
                &mut state,
                &mut backend,
                A,
                &mut a_peer,
                a_sequence,
                2,
                0,
                &words(&[ROOT_WINDOW.0, 0x4000, cursor.0]),
            ),
            "A defines the root cursor",
        );
        a_sequence += 1;
        assert_no_error(
            &request(
                &mut state,
                &mut backend,
                A,
                &mut a_peer,
                a_sequence,
                95,
                0,
                &cursor.0.to_le_bytes(),
            ),
            "A frees the root cursor XID",
        );
        a_sequence += 1;
        assert_no_error(
            &request(
                &mut state,
                &mut backend,
                A,
                &mut a_peer,
                a_sequence,
                2,
                0,
                &words(&[ROOT_WINDOW.0, 0x4000, 0]),
            ),
            "A resets the root cursor",
        );
    } else {
        let reply =
            grab_pointer_with_cursor(&mut state, &mut backend, A, &mut a_peer, a_sequence, cursor);
        assert_no_error(&reply, "A grabs the pointer with C");
        assert_eq!(reply[1], 0, "GrabPointer succeeds");
        a_sequence += 1;
        assert_no_error(
            &request(
                &mut state,
                &mut backend,
                A,
                &mut a_peer,
                a_sequence,
                95,
                0,
                &cursor.0.to_le_bytes(),
            ),
            "A frees the grabbed cursor XID",
        );
    }
    backend.active_cursor_image = Some(active_cursor_image(host));
    let image_and_name_mode = AccessMode::READ | AccessMode::GET_ATTR;

    for (minor, _) in [
        (x11xfixes::GET_CURSOR_IMAGE, AccessMode::READ),
        (x11xfixes::GET_CURSOR_IMAGE_AND_NAME, image_and_name_mode),
    ] {
        let bytes = request(
            &mut state,
            &mut backend,
            B,
            &mut b_peer,
            b_sequence,
            140,
            minor,
            &[],
        );
        assert_no_error(&bytes, "no-policy retained cursor image");
        assert_eq!(bytes.first().copied(), Some(1), "cursor image reply");
        b_sequence += 1;
    }

    let denied_error = DenyError {
        code: yserver_protocol::x11::error::BAD_ACCESS,
        value: 0x6b,
    };
    let calls = Arc::new(Mutex::new(Vec::new()));
    let resource_checks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let policy_checks = Arc::clone(&resource_checks);
    state.install_access_policy(Arc::new(RecordingExtensionPolicy::custom(
        Arc::clone(&calls),
        "",
        move |_, check| {
            if check.hook == ExtensionHook::Resource
                && policy_checks
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    .is_multiple_of(2)
            {
                Decision::Deny(denied_error)
            } else {
                Decision::Allow
            }
        },
    )));

    for (minor, _) in [
        (x11xfixes::GET_CURSOR_IMAGE, AccessMode::READ),
        (x11xfixes::GET_CURSOR_IMAGE_AND_NAME, image_and_name_mode),
    ] {
        let origin_before = state.resources.cursor_host_origin(host);
        let names_before = state.resources.cursor_name_for_host(host);
        let backend_before = backend.calls();
        let denied_sequence = b_sequence;
        let denied = request(
            &mut state,
            &mut backend,
            B,
            &mut b_peer,
            denied_sequence,
            140,
            minor,
            &[],
        );
        assert_error(
            &denied,
            denied_error.code,
            denied_error.value,
            140,
            minor,
            denied_sequence,
        );
        assert_eq!(state.resources.cursor_host_origin(host), origin_before);
        assert_eq!(state.resources.cursor_name_for_host(host), names_before);
        assert_eq!(backend.calls(), backend_before, "denial reached backend");
        b_sequence += 1;

        let allowed_sequence = b_sequence;
        let allowed = request(
            &mut state,
            &mut backend,
            B,
            &mut b_peer,
            allowed_sequence,
            140,
            minor,
            &[],
        );
        assert_no_error(&allowed, "ASK row allows the retained cursor image");
        assert_eq!(allowed.first().copied(), Some(1), "cursor image reply");
        b_sequence += 1;
        assert_eq!(state.resources.cursor_host_origin(host), Some((cursor, A)));
    }

    let resource_checks = calls
        .lock()
        .unwrap()
        .iter()
        .filter(|check| check.hook == ExtensionHook::Resource)
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        resource_checks.len(),
        4,
        "one DENY and one ASK row per site"
    );
    for (check, (minor, mode)) in resource_checks.iter().zip([
        (x11xfixes::GET_CURSOR_IMAGE, AccessMode::READ),
        (x11xfixes::GET_CURSOR_IMAGE, AccessMode::READ),
        (x11xfixes::GET_CURSOR_IMAGE_AND_NAME, image_and_name_mode),
        (x11xfixes::GET_CURSOR_IMAGE_AND_NAME, image_and_name_mode),
    ]) {
        assert_eq!(
            (
                check.client,
                check.target,
                check.mode,
                check.owner,
                check.target_owner,
                check.request,
            ),
            (
                B,
                Some(ResourceTarget {
                    owner: Some(A),
                    id: cursor,
                    kind: ResourceKind::Cursor,
                    creation: None,
                }),
                Some(mode),
                None,
                Some(A),
                Some(RequestOpcodes { major: 140, minor }),
            )
        );
    }
}

#[test]
// Mutation: drop the retained origin at free time even while the backend keeps the cursor alive.
fn xfixes_get_cursor_image_checks_grab_cursor_after_free() {
    retained_cursor_image_access_checks(false);
}

#[test]
// Mutation: drop the retained origin at reset time even while the backend keeps the cursor alive.
fn xfixes_get_cursor_image_checks_root_cursor_after_reset() {
    retained_cursor_image_access_checks(true);
}

#[test]
fn cursor_record_dropped_once_backend_destroys_cursor() {
    // Mutation: never sweep origins kept alive at an earlier release point.
    let mut state = ServerState::new();
    let mut a_peer = install_client(&mut state, A.0);
    let mut b_peer = install_client(&mut state, B.0);
    set_base(&mut state, A, A_BASE);
    set_base(&mut state, B, B_BASE);
    state.xfixes_client_major.insert(B.0, 5);
    let mut backend = RecordingBackend::new();
    let mut a_sequence = 1;
    let mut b_sequence = 10;
    let (cursor, host) = create_cursor(
        &mut state,
        &mut backend,
        A,
        &mut a_peer,
        &mut a_sequence,
        A_BASE,
    );
    backend.alive_cursors.insert(host);
    backend.active_cursor_image = Some(active_cursor_image(host));
    assert_no_error(
        &grab_pointer_with_cursor(&mut state, &mut backend, A, &mut a_peer, a_sequence, cursor),
        "A grabs the pointer with C",
    );
    a_sequence += 1;
    assert_no_error(
        &request(
            &mut state,
            &mut backend,
            A,
            &mut a_peer,
            a_sequence,
            95,
            0,
            &cursor.0.to_le_bytes(),
        ),
        "A frees C while the backend retains it",
    );
    assert_eq!(state.resources.cursor_host_origin(host), Some((cursor, A)));

    assert_no_error(
        &request(
            &mut state,
            &mut backend,
            A,
            &mut a_peer,
            a_sequence + 1,
            27,
            0,
            &0u32.to_le_bytes(),
        ),
        "A ends the grab",
    );
    backend.alive_cursors.remove(&host);
    let (next_cursor, _) = create_cursor(
        &mut state,
        &mut backend,
        B,
        &mut b_peer,
        &mut b_sequence,
        B_BASE,
    );
    assert_no_error(
        &request(
            &mut state,
            &mut backend,
            B,
            &mut b_peer,
            b_sequence,
            95,
            0,
            &next_cursor.0.to_le_bytes(),
        ),
        "B frees a cursor at the next release point",
    );
    assert_eq!(state.resources.cursor_host_origin(host), None);

    backend.active_cursor_image = Some(active_cursor_image(host));
    let calls = Arc::new(Mutex::new(Vec::new()));
    state.install_access_policy(Arc::new(RecordingExtensionPolicy::new(
        Decision::Allow,
        Arc::clone(&calls),
    )));
    for minor in [
        x11xfixes::GET_CURSOR_IMAGE,
        x11xfixes::GET_CURSOR_IMAGE_AND_NAME,
    ] {
        calls.lock().unwrap().clear();
        let bytes = request(
            &mut state,
            &mut backend,
            B,
            &mut b_peer,
            b_sequence + 1,
            140,
            minor,
            &[],
        );
        assert_no_error(&bytes, "server cursor image has no retained owner");
        assert!(
            calls
                .lock()
                .unwrap()
                .iter()
                .all(|check| check.hook != ExtensionHook::Resource),
            "a host cursor without an origin is not a policy target"
        );
        b_sequence += 1;
    }
}

fn check_cursor_owner(
    state: &ServerState,
    requester: ClientId,
    id: ResourceId,
    owner: Option<ClientId>,
) {
    assert_eq!(
        state.check_resource(
            requester,
            &ResourceTarget {
                id,
                kind: ResourceKind::Cursor,
                creation: None,
                owner,
            },
            AccessMode::READ,
            Some(RequestOpcodes {
                major: 140,
                minor: 4
            }),
        ),
        Decision::Allow
    );
}

#[test]
fn recycled_base_cursor_keeps_its_owner() {
    // Mutation: resolve the retained instance's owner through ctx.owner.
    let mut state = ServerState::new();
    let mut a_peer = install_client(&mut state, A.0);
    let mut b_peer = install_client(&mut state, B.0);
    let mut d_peer = install_client(&mut state, D.0);
    set_base(&mut state, A, A_BASE);
    set_base(&mut state, B, B_BASE);
    let mut backend = RecordingBackend::new();
    let mut a_sequence = 1;
    let mut b_sequence = 1;
    let mut d_sequence = 1;
    let (cursor, host_a) = create_cursor(
        &mut state,
        &mut backend,
        A,
        &mut a_peer,
        &mut a_sequence,
        A_BASE,
    );
    let window_a = ResourceId(B_BASE + 1);
    let window_d = ResourceId(B_BASE + 2);
    create_window(
        &mut state,
        &mut backend,
        &mut b_peer,
        &mut b_sequence,
        window_a,
    );
    create_window(
        &mut state,
        &mut backend,
        &mut b_peer,
        &mut b_sequence,
        window_d,
    );
    assert_no_error(
        &set_window_cursor(
            &mut state,
            &mut backend,
            &mut b_peer,
            &mut b_sequence,
            window_a,
            cursor,
        ),
        "B displays A's cursor",
    );

    crate::core_loop::process_disconnect::process_disconnect(&mut state, &mut backend, A);
    assert_eq!(
        state.resources.cursor_host_origin(host_a),
        Some((cursor, A))
    );

    // D receives A's recycled base and creates a live cursor with the same XID.
    set_base(&mut state, D, A_BASE);
    let (live_cursor, host_d) = create_cursor(
        &mut state,
        &mut backend,
        D,
        &mut d_peer,
        &mut d_sequence,
        A_BASE,
    );
    assert_eq!(live_cursor, cursor);
    assert_ne!(host_a, host_d);
    assert_no_error(
        &set_window_cursor(
            &mut state,
            &mut backend,
            &mut b_peer,
            &mut b_sequence,
            window_d,
            live_cursor,
        ),
        "B displays D's cursor",
    );

    let observations = Arc::new(Mutex::new(Vec::<RecordedExtensionCheck>::new()));
    state.install_access_policy(Arc::new(RecordingExtensionPolicy::new(
        Decision::Allow,
        Arc::clone(&observations),
    )));
    let origin_a = state.resources.cursor_host_origin(host_a).unwrap();
    assert_eq!(origin_a, (cursor, A));
    check_cursor_owner(&state, B, cursor, Some(origin_a.1));
    check_cursor_owner(&state, B, live_cursor, None);
    assert_eq!(
        observations
            .lock()
            .unwrap()
            .iter()
            .map(|call| {
                assert_eq!(call.hook, ExtensionHook::Resource);
                let target = call.target.expect("resource check target");
                (target.owner, call.target_owner, call.owner)
            })
            .collect::<Vec<_>>(),
        [(Some(A), Some(A), Some(D)), (None, Some(D), Some(D))]
    );

    assert_no_error(
        &request(
            &mut state,
            &mut backend,
            D,
            &mut d_peer,
            d_sequence,
            95,
            0,
            &live_cursor.0.to_le_bytes(),
        ),
        "D frees its displayed cursor XID",
    );
    assert_eq!(
        state.resources.cursor_host_origin(host_a),
        Some((cursor, A))
    );
    let origin_d = state.resources.cursor_host_origin(host_d).unwrap();
    assert_eq!(origin_d, (cursor, D));
    observations.lock().unwrap().clear();
    check_cursor_owner(&state, B, cursor, Some(origin_a.1));
    check_cursor_owner(&state, B, live_cursor, Some(origin_d.1));
    assert_eq!(
        observations
            .lock()
            .unwrap()
            .iter()
            .map(|call| {
                assert_eq!(call.hook, ExtensionHook::Resource);
                let target = call.target.expect("resource check target");
                (target.owner, call.target_owner, call.owner)
            })
            .collect::<Vec<_>>(),
        [(Some(A), Some(A), None), (Some(D), Some(D), None)]
    );
}

#[test]
fn change_cursor_alias_keeps_creator_record() {
    // Mutation: make retarget_cursor_host create or overwrite the new host's record.
    let mut state = ServerState::new();
    let mut a_peer = install_client(&mut state, A.0);
    let mut b_peer = install_client(&mut state, B.0);
    set_base(&mut state, A, A_BASE);
    set_base(&mut state, B, B_BASE);
    state.xfixes_client_major.insert(B.0, 5);
    let mut backend = RecordingBackend::new();
    let mut a_sequence = 1;
    let mut b_sequence = 1;
    let (source, host_a) = create_cursor(
        &mut state,
        &mut backend,
        A,
        &mut a_peer,
        &mut a_sequence,
        A_BASE,
    );
    let (destination, host_b) = create_cursor(
        &mut state,
        &mut backend,
        B,
        &mut b_peer,
        &mut b_sequence,
        B_BASE,
    );

    assert_no_error(
        &request(
            &mut state,
            &mut backend,
            B,
            &mut b_peer,
            b_sequence,
            140,
            x11xfixes::CHANGE_CURSOR,
            &words(&[source.0, destination.0]),
        ),
        "XFIXES ChangeCursor",
    );
    assert_eq!(state.resources.cursor_host_xid(destination), Some(host_a));
    assert_eq!(
        state.resources.cursor_host_origin(host_a),
        Some((source, A))
    );
    assert_eq!(state.resources.cursor_host_origin(host_b), None);
}

#[test]
fn change_cursor_by_name_skips_denied_cursor() {
    // Mutation: turn a derived cursor denial into a whole-request denial or apply it anyway.
    let mut state = ServerState::new();
    let mut a_peer = install_client(&mut state, A.0);
    let mut b_peer = install_client(&mut state, B.0);
    let mut d_peer = install_client(&mut state, D.0);
    set_base(&mut state, A, A_BASE);
    set_base(&mut state, B, B_BASE);
    set_base(&mut state, D, 0x0030_0000);
    for client in [A, B, D] {
        state.xfixes_client_major.insert(client.0, 5);
    }
    let mut backend = RecordingBackend::new();
    let mut a_sequence = 1;
    let mut b_sequence = 1;
    let mut d_sequence = 1;
    let (source, source_host) = create_cursor(
        &mut state,
        &mut backend,
        A,
        &mut a_peer,
        &mut a_sequence,
        A_BASE,
    );
    let (denied, denied_host) = create_cursor(
        &mut state,
        &mut backend,
        B,
        &mut b_peer,
        &mut b_sequence,
        B_BASE,
    );
    let (allowed, allowed_host) = create_cursor(
        &mut state,
        &mut backend,
        D,
        &mut d_peer,
        &mut d_sequence,
        0x0030_0000,
    );
    for (client, peer, sequence, cursor) in [
        (B, &mut b_peer, &mut b_sequence, denied),
        (D, &mut d_peer, &mut d_sequence, allowed),
    ] {
        assert_no_error(
            &request(
                &mut state,
                &mut backend,
                client,
                peer,
                *sequence,
                140,
                x11xfixes::SET_CURSOR_NAME,
                &cursor_name_body(cursor, b"x"),
            ),
            "name destination cursor",
        );
        *sequence += 1;
    }

    let calls = Arc::new(Mutex::new(Vec::new()));
    state.install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
        Arc::clone(&calls),
        denied,
        Some(AccessMode::WRITE | AccessMode::SET_ATTR),
        crate::access::DenyError {
            code: yserver_protocol::x11::error::BAD_ACCESS,
            value: 0x66,
        },
    )));
    let mut name = source.0.to_le_bytes().to_vec();
    name.extend_from_slice(&1u16.to_le_bytes());
    name.extend_from_slice(&[0, 0, b'x', 0, 0, 0]);
    let bytes = request(
        &mut state,
        &mut backend,
        A,
        &mut a_peer,
        a_sequence,
        140,
        x11xfixes::CHANGE_CURSOR_BY_NAME,
        &name,
    );
    assert_no_error(&bytes, "ChangeCursorByName skips its denied derived cursor");
    assert_eq!(state.resources.cursor_host_xid(denied), Some(denied_host));
    assert_eq!(state.resources.cursor_host_xid(allowed), Some(source_host));
    assert_eq!(
        backend
            .calls()
            .iter()
            .filter_map(|call| match call {
                RecordedCall::ReplaceCursor {
                    old_host_xid,
                    new_host_xid,
                } => Some((*old_host_xid, *new_host_xid)),
                _ => None,
            })
            .collect::<Vec<_>>(),
        [(allowed_host, source_host)]
    );
    assert_eq!(
        calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.hook == ExtensionHook::Resource)
            .map(|call| {
                (
                    call.client,
                    call.request,
                    call.target.unwrap(),
                    call.mode.unwrap(),
                    call.target_owner,
                )
            })
            .collect::<Vec<_>>(),
        [
            (
                A,
                Some(RequestOpcodes {
                    major: 140,
                    minor: 27
                }),
                ResourceTarget {
                    owner: None,
                    id: source,
                    kind: ResourceKind::Cursor,
                    creation: None,
                },
                AccessMode::READ | AccessMode::GET_ATTR,
                Some(A),
            ),
            (
                A,
                Some(RequestOpcodes {
                    major: 140,
                    minor: 27
                }),
                ResourceTarget {
                    owner: None,
                    id: denied,
                    kind: ResourceKind::Cursor,
                    creation: None,
                },
                AccessMode::WRITE | AccessMode::SET_ATTR,
                Some(B),
            ),
            (
                A,
                Some(RequestOpcodes {
                    major: 140,
                    minor: 27
                }),
                ResourceTarget {
                    owner: None,
                    id: allowed,
                    kind: ResourceKind::Cursor,
                    creation: None,
                },
                AccessMode::WRITE | AccessMode::SET_ATTR,
                Some(D),
            ),
        ]
    );
}

#[test]
fn get_cursor_image_server_cursor_is_not_asked() {
    // Mutation: treat a displayed cursor without a client origin as a checked resource.
    let mut state = ServerState::new();
    let mut b_peer = install_client(&mut state, B.0);
    state.xfixes_client_major.insert(B.0, 5);
    let mut backend = RecordingBackend::new();
    backend.active_cursor_image = Some(active_cursor_image(0xdead_beef));
    let calls = Arc::new(Mutex::new(Vec::new()));
    state.install_access_policy(Arc::new(RecordingExtensionPolicy::new(
        Decision::Allow,
        Arc::clone(&calls),
    )));

    let bytes = request(
        &mut state,
        &mut backend,
        B,
        &mut b_peer,
        1,
        140,
        x11xfixes::GET_CURSOR_IMAGE,
        &[],
    );
    assert_no_error(&bytes, "server cursor image remains available");
    assert_eq!(
        calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.hook == ExtensionHook::Resource)
            .count(),
        0
    );
}

#[test]
fn get_cursor_image_of_freed_displayed_cursor_denied() {
    // Mutation: skip the retained-instance check because its XID is not live.
    let mut state = ServerState::new();
    let mut a_peer = install_client(&mut state, A.0);
    let mut b_peer = install_client(&mut state, B.0);
    set_base(&mut state, A, A_BASE);
    set_base(&mut state, B, B_BASE);
    state.xfixes_client_major.insert(A.0, 5);
    state.xfixes_client_major.insert(B.0, 5);
    let mut backend = RecordingBackend::new();
    let mut a_sequence = 1;
    let mut b_sequence = 1;
    let (cursor, host) = create_cursor(
        &mut state,
        &mut backend,
        A,
        &mut a_peer,
        &mut a_sequence,
        A_BASE,
    );
    assert_no_error(
        &request(
            &mut state,
            &mut backend,
            A,
            &mut a_peer,
            a_sequence,
            140,
            x11xfixes::SET_CURSOR_NAME,
            &cursor_name_body(cursor, b"retained-image"),
        ),
        "name the cursor before displaying and freeing it",
    );
    a_sequence += 1;
    let window = ResourceId(B_BASE + 1);
    create_window(
        &mut state,
        &mut backend,
        &mut b_peer,
        &mut b_sequence,
        window,
    );
    assert_no_error(
        &set_window_cursor(
            &mut state,
            &mut backend,
            &mut b_peer,
            &mut b_sequence,
            window,
            cursor,
        ),
        "B displays A's cursor",
    );
    assert_no_error(
        &request(
            &mut state,
            &mut backend,
            A,
            &mut a_peer,
            a_sequence,
            95,
            0,
            &cursor.0.to_le_bytes(),
        ),
        "A frees the displayed cursor XID",
    );
    backend.active_cursor_image = Some(active_cursor_image(host));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let denied_error = crate::access::DenyError {
        code: yserver_protocol::x11::error::BAD_ACCESS,
        value: 0x67,
    };
    state.install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
        Arc::clone(&calls),
        cursor,
        None,
        denied_error,
    )));

    for (minor, sequence, mode) in [
        (x11xfixes::GET_CURSOR_IMAGE, b_sequence, AccessMode::READ),
        (
            x11xfixes::GET_CURSOR_IMAGE_AND_NAME,
            b_sequence + 1,
            AccessMode::READ | AccessMode::GET_ATTR,
        ),
    ] {
        let bytes = request(
            &mut state,
            &mut backend,
            B,
            &mut b_peer,
            sequence,
            140,
            minor,
            &[],
        );
        assert_error(
            &bytes,
            denied_error.code,
            denied_error.value,
            140,
            minor,
            sequence,
        );
        assert_eq!(calls.lock().unwrap().last().unwrap().mode, Some(mode));
        let target = calls.lock().unwrap().last().unwrap().target.unwrap();
        assert_eq!(target.id, cursor);
        assert_eq!(target.owner, Some(A));
        assert_eq!(calls.lock().unwrap().last().unwrap().target_owner, Some(A));
    }
}

#[test]
fn change_cursor_by_name_freed_displayed_cursor_denied_keeps_image() {
    // Mutation: skip the retained-instance check because its XID is not live.
    let mut state = ServerState::new();
    let mut a_peer = install_client(&mut state, A.0);
    let mut b_peer = install_client(&mut state, B.0);
    set_base(&mut state, A, A_BASE);
    set_base(&mut state, B, B_BASE);
    state.xfixes_client_major.insert(A.0, 5);
    state.xfixes_client_major.insert(B.0, 5);
    let mut backend = RecordingBackend::new();
    let mut a_sequence = 1;
    let mut b_sequence = 1;
    let (destination, old_host) = create_cursor(
        &mut state,
        &mut backend,
        A,
        &mut a_peer,
        &mut a_sequence,
        A_BASE,
    );
    let (source, source_host) = create_cursor(
        &mut state,
        &mut backend,
        B,
        &mut b_peer,
        &mut b_sequence,
        B_BASE,
    );
    assert_no_error(
        &request(
            &mut state,
            &mut backend,
            A,
            &mut a_peer,
            a_sequence,
            140,
            x11xfixes::SET_CURSOR_NAME,
            &cursor_name_body(destination, b"retained"),
        ),
        "name destination cursor",
    );
    a_sequence += 1;
    let window = ResourceId(B_BASE + 3);
    create_window(
        &mut state,
        &mut backend,
        &mut b_peer,
        &mut b_sequence,
        window,
    );
    assert_no_error(
        &set_window_cursor(
            &mut state,
            &mut backend,
            &mut b_peer,
            &mut b_sequence,
            window,
            destination,
        ),
        "B displays A's cursor",
    );
    assert_no_error(
        &request(
            &mut state,
            &mut backend,
            A,
            &mut a_peer,
            a_sequence,
            95,
            0,
            &destination.0.to_le_bytes(),
        ),
        "A frees its displayed cursor XID",
    );
    let calls = Arc::new(Mutex::new(Vec::new()));
    state.install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
        Arc::clone(&calls),
        destination,
        Some(AccessMode::WRITE | AccessMode::SET_ATTR),
        crate::access::DenyError {
            code: yserver_protocol::x11::error::BAD_ACCESS,
            value: 0x68,
        },
    )));
    let mut body = source.0.to_le_bytes().to_vec();
    body.extend_from_slice(&8u16.to_le_bytes());
    body.extend_from_slice(&[0, 0]);
    body.extend_from_slice(b"retained");
    let bytes = request(
        &mut state,
        &mut backend,
        B,
        &mut b_peer,
        b_sequence,
        140,
        x11xfixes::CHANGE_CURSOR_BY_NAME,
        &body,
    );
    assert_no_error(&bytes, "denied retained cursor is skipped");
    assert_eq!(state.resources.cursor_host_xid(source), Some(source_host));
    assert_eq!(
        state.resources.cursor_host_origin(old_host),
        Some((destination, A))
    );
    assert!(backend.calls().iter().all(|call| !matches!(
        call,
        RecordedCall::ReplaceCursor { old_host_xid, .. } if *old_host_xid == old_host
    )));
    let call = calls.lock().unwrap().last().unwrap().clone();
    assert_eq!(call.target.unwrap().owner, Some(A));
    assert_eq!(call.target_owner, Some(A));
}

#[test]
fn change_cursor_by_name_freed_displayed_cursor_no_policy_matches_today() {
    // Mutation: require the named cursor's XID to remain live before replacing its host cursor.
    let mut state = ServerState::new();
    let mut a_peer = install_client(&mut state, A.0);
    let mut b_peer = install_client(&mut state, B.0);
    set_base(&mut state, A, A_BASE);
    set_base(&mut state, B, B_BASE);
    state.xfixes_client_major.insert(A.0, 5);
    state.xfixes_client_major.insert(B.0, 5);
    let mut backend = RecordingBackend::new();
    let mut a_sequence = 1;
    let mut b_sequence = 1;
    let (destination, old_host) = create_cursor(
        &mut state,
        &mut backend,
        A,
        &mut a_peer,
        &mut a_sequence,
        A_BASE,
    );
    let (source, source_host) = create_cursor(
        &mut state,
        &mut backend,
        B,
        &mut b_peer,
        &mut b_sequence,
        B_BASE,
    );
    assert_no_error(
        &request(
            &mut state,
            &mut backend,
            A,
            &mut a_peer,
            a_sequence,
            140,
            x11xfixes::SET_CURSOR_NAME,
            &cursor_name_body(destination, b"retained"),
        ),
        "name destination cursor",
    );
    a_sequence += 1;
    let window = ResourceId(B_BASE + 3);
    create_window(
        &mut state,
        &mut backend,
        &mut b_peer,
        &mut b_sequence,
        window,
    );
    assert_no_error(
        &set_window_cursor(
            &mut state,
            &mut backend,
            &mut b_peer,
            &mut b_sequence,
            window,
            destination,
        ),
        "B displays A's cursor",
    );
    assert_no_error(
        &request(
            &mut state,
            &mut backend,
            A,
            &mut a_peer,
            a_sequence,
            95,
            0,
            &destination.0.to_le_bytes(),
        ),
        "A frees its displayed cursor XID",
    );
    let mut body = source.0.to_le_bytes().to_vec();
    body.extend_from_slice(&8u16.to_le_bytes());
    body.extend_from_slice(&[0, 0]);
    body.extend_from_slice(b"retained");
    let bytes = request(
        &mut state,
        &mut backend,
        B,
        &mut b_peer,
        b_sequence,
        140,
        x11xfixes::CHANGE_CURSOR_BY_NAME,
        &body,
    );
    assert_no_error(&bytes, "unpolicied retained cursor replacement");
    assert_eq!(state.resources.cursor_host_xid(source), Some(source_host));
    assert_eq!(state.resources.cursor_host_origin(old_host), None);
    assert!(backend.calls().iter().any(|call| matches!(
        call,
        RecordedCall::ReplaceCursor { old_host_xid, new_host_xid }
            if *old_host_xid == old_host && *new_host_xid == source_host
    )));
}

#[test]
fn xfixes_change_save_set_denied_leaves_save_set_unchanged() {
    // Mutation: borrow and update ClientState before checking ChangeSaveSet.
    let mut state = ServerState::new();
    let mut a_peer = install_client(&mut state, A.0);
    let mut b_peer = install_client(&mut state, B.0);
    set_base(&mut state, A, A_BASE);
    set_base(&mut state, B, B_BASE);
    state.xfixes_client_major.insert(A.0, 5);
    let mut backend = RecordingBackend::new();
    let mut b_sequence = 1;
    let window = ResourceId(B_BASE + 1);
    create_window(
        &mut state,
        &mut backend,
        &mut b_peer,
        &mut b_sequence,
        window,
    );
    assert!(!state.clients[&A.0].save_set.contains(&window));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let denied_error = DenyError {
        code: yserver_protocol::x11::error::BAD_ACCESS,
        value: 0x69,
    };
    state.install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
        Arc::clone(&calls),
        window,
        Some(AccessMode::MANAGE),
        denied_error,
    )));

    let mut body = vec![0, 0, 0, 0];
    body.extend_from_slice(&window.0.to_le_bytes());
    let bytes = request(
        &mut state,
        &mut backend,
        A,
        &mut a_peer,
        1,
        140,
        x11xfixes::CHANGE_SAVE_SET,
        &body,
    );
    assert_error(
        &bytes,
        denied_error.code,
        denied_error.value,
        140,
        x11xfixes::CHANGE_SAVE_SET,
        1,
    );
    assert!(!state.clients[&A.0].save_set.contains(&window));
    assert_eq!(
        calls.lock().unwrap().last().unwrap().mode,
        Some(AccessMode::MANAGE)
    );
}

#[test]
fn recycled_base_get_cursor_image_uses_retained_owner() {
    // Mutation: resolve the retained cursor's owner from the recycled live XID.
    let mut state = ServerState::new();
    let mut a_peer = install_client(&mut state, A.0);
    let mut b_peer = install_client(&mut state, B.0);
    let mut d_peer = install_client(&mut state, D.0);
    set_base(&mut state, A, A_BASE);
    set_base(&mut state, B, B_BASE);
    state.xfixes_client_major.insert(D.0, 5);
    let mut backend = RecordingBackend::new();
    let mut a_sequence = 1;
    let mut b_sequence = 1;
    let mut d_sequence = 1;
    let (cursor, host_a) = create_cursor(
        &mut state,
        &mut backend,
        A,
        &mut a_peer,
        &mut a_sequence,
        A_BASE,
    );
    let window = ResourceId(B_BASE + 1);
    create_window(
        &mut state,
        &mut backend,
        &mut b_peer,
        &mut b_sequence,
        window,
    );
    assert_no_error(
        &set_window_cursor(
            &mut state,
            &mut backend,
            &mut b_peer,
            &mut b_sequence,
            window,
            cursor,
        ),
        "B displays A's cursor",
    );
    crate::core_loop::process_disconnect::process_disconnect(&mut state, &mut backend, A);
    set_base(&mut state, D, A_BASE);
    let (live_cursor, host_d) = create_cursor(
        &mut state,
        &mut backend,
        D,
        &mut d_peer,
        &mut d_sequence,
        A_BASE,
    );
    assert_eq!(live_cursor, cursor);
    assert_ne!(host_a, host_d);
    backend.active_cursor_image = Some(active_cursor_image(host_a));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let denied_error = DenyError {
        code: yserver_protocol::x11::error::BAD_ACCESS,
        value: 0x6a,
    };
    state.install_access_policy(Arc::new(
        RecordingExtensionPolicy::denying_resources_not_owned_by_requester(
            Arc::clone(&calls),
            denied_error,
        ),
    ));

    let bytes = request(
        &mut state,
        &mut backend,
        D,
        &mut d_peer,
        d_sequence,
        140,
        x11xfixes::GET_CURSOR_IMAGE,
        &[],
    );
    assert_error(
        &bytes,
        denied_error.code,
        denied_error.value,
        140,
        x11xfixes::GET_CURSOR_IMAGE,
        d_sequence,
    );
    let calls = calls.lock().unwrap();
    let resource_calls = calls
        .iter()
        .filter(|call| call.hook == ExtensionHook::Resource)
        .collect::<Vec<_>>();
    assert_eq!(resource_calls.len(), 1);
    let call = resource_calls[0];
    assert_eq!(call.client, D);
    assert_eq!(
        call.request,
        Some(RequestOpcodes {
            major: 140,
            minor: 4
        })
    );
    assert_eq!(
        call.target,
        Some(ResourceTarget {
            owner: Some(A),
            id: cursor,
            kind: ResourceKind::Cursor,
            creation: None,
        })
    );
    assert_eq!(call.owner, Some(D));
    assert_eq!(call.target_owner, Some(A));
    assert_eq!(call.mode, Some(AccessMode::READ));
}

#[test]
fn xfixes_resource_access_ask_table() {
    // Mutation: remove, reorder, mistype, or omit a listed XFIXES resource check.
    let mut state = ServerState::new();
    let mut a_peer = install_client(&mut state, A.0);
    let mut b_peer = install_client(&mut state, B.0);
    set_base(&mut state, A, A_BASE);
    set_base(&mut state, B, B_BASE);
    state.xfixes_client_major.insert(A.0, 5);
    state.xfixes_client_major.insert(B.0, 5);
    let mut backend = RecordingBackend::new();
    let mut a_sequence = 1;
    let mut b_sequence = 1;
    let (source, source_host) = create_cursor(
        &mut state,
        &mut backend,
        A,
        &mut a_peer,
        &mut a_sequence,
        A_BASE,
    );
    let (destination, _) = create_cursor(
        &mut state,
        &mut backend,
        B,
        &mut b_peer,
        &mut b_sequence,
        B_BASE,
    );
    let window = ResourceId(B_BASE + 3);
    create_window(
        &mut state,
        &mut backend,
        &mut b_peer,
        &mut b_sequence,
        window,
    );
    backend.active_cursor_image = Some(active_cursor_image(source_host));
    let calls = Arc::new(Mutex::new(Vec::new()));
    state.install_access_policy(Arc::new(RecordingExtensionPolicy::new(
        Decision::Allow,
        Arc::clone(&calls),
    )));
    let target = |id, kind| ResourceTarget {
        owner: None,
        id,
        kind,
        creation: None,
    };

    let mut save_set = vec![0, 0, 0, 0];
    save_set.extend_from_slice(&window.0.to_le_bytes());
    xfixes_request_checks(
        &mut state,
        &mut backend,
        A,
        &mut a_peer,
        20,
        x11xfixes::CHANGE_SAVE_SET,
        &save_set,
        &calls,
        &[(target(window, ResourceKind::Window), AccessMode::MANAGE)],
    );
    xfixes_request_checks(
        &mut state,
        &mut backend,
        B,
        &mut b_peer,
        21,
        x11xfixes::SELECT_SELECTION_INPUT,
        &words(&[window.0, 17, 1]),
        &calls,
        &[(target(window, ResourceKind::Window), AccessMode::GET_ATTR)],
    );
    xfixes_request_checks(
        &mut state,
        &mut backend,
        B,
        &mut b_peer,
        22,
        x11xfixes::SELECT_CURSOR_INPUT,
        &words(&[window.0, 1]),
        &calls,
        &[(target(window, ResourceKind::Window), AccessMode::GET_ATTR)],
    );
    xfixes_request_checks(
        &mut state,
        &mut backend,
        B,
        &mut b_peer,
        23,
        x11xfixes::GET_CURSOR_IMAGE,
        &[],
        &calls,
        &[(target(source, ResourceKind::Cursor), AccessMode::READ)],
    );
    xfixes_request_checks(
        &mut state,
        &mut backend,
        B,
        &mut b_peer,
        24,
        x11xfixes::SET_CURSOR_NAME,
        &cursor_name_body(destination, b"named"),
        &calls,
        &[(
            target(destination, ResourceKind::Cursor),
            AccessMode::SET_ATTR,
        )],
    );
    xfixes_request_checks(
        &mut state,
        &mut backend,
        B,
        &mut b_peer,
        25,
        x11xfixes::GET_CURSOR_NAME,
        &destination.0.to_le_bytes(),
        &calls,
        &[(
            target(destination, ResourceKind::Cursor),
            AccessMode::GET_ATTR,
        )],
    );
    xfixes_request_checks(
        &mut state,
        &mut backend,
        B,
        &mut b_peer,
        26,
        x11xfixes::GET_CURSOR_IMAGE_AND_NAME,
        &[],
        &calls,
        &[(
            target(source, ResourceKind::Cursor),
            AccessMode::READ | AccessMode::GET_ATTR,
        )],
    );
    xfixes_request_checks(
        &mut state,
        &mut backend,
        B,
        &mut b_peer,
        27,
        x11xfixes::CHANGE_CURSOR_BY_NAME,
        &cursor_name_body(source, b"named"),
        &calls,
        &[
            (
                target(source, ResourceKind::Cursor),
                AccessMode::READ | AccessMode::GET_ATTR,
            ),
            (
                target(destination, ResourceKind::Cursor),
                AccessMode::WRITE | AccessMode::SET_ATTR,
            ),
        ],
    );
    xfixes_request_checks(
        &mut state,
        &mut backend,
        B,
        &mut b_peer,
        28,
        x11xfixes::CHANGE_CURSOR,
        &words(&[source.0, destination.0]),
        &calls,
        &[
            (
                target(source, ResourceKind::Cursor),
                AccessMode::READ | AccessMode::GET_ATTR,
            ),
            (
                target(destination, ResourceKind::Cursor),
                AccessMode::WRITE | AccessMode::SET_ATTR,
            ),
        ],
    );
    xfixes_request_checks(
        &mut state,
        &mut backend,
        B,
        &mut b_peer,
        29,
        x11xfixes::HIDE_CURSOR,
        &window.0.to_le_bytes(),
        &calls,
        &[(target(window, ResourceKind::Window), AccessMode::GET_ATTR)],
    );
    xfixes_request_checks(
        &mut state,
        &mut backend,
        B,
        &mut b_peer,
        30,
        x11xfixes::SHOW_CURSOR,
        &window.0.to_le_bytes(),
        &calls,
        &[(target(window, ResourceKind::Window), AccessMode::GET_ATTR)],
    );

    let barrier = ResourceId(B_BASE + 4);
    let mut create_barrier = words(&[barrier.0, window.0]);
    for value in [0i16, 0, 1, 0] {
        create_barrier.extend_from_slice(&value.to_le_bytes());
    }
    create_barrier.extend_from_slice(&0u32.to_le_bytes());
    create_barrier.extend_from_slice(&[0; 4]);
    xfixes_request_checks(
        &mut state,
        &mut backend,
        B,
        &mut b_peer,
        31,
        x11xfixes::CREATE_POINTER_BARRIER,
        &create_barrier,
        &calls,
        &[
            (target(window, ResourceKind::Window), AccessMode::READ),
            (
                ResourceTarget {
                    owner: None,
                    id: barrier,
                    kind: ResourceKind::PointerBarrier,
                    creation: Some(crate::access::Creation {
                        parent: Some(window),
                        owner: B,
                    }),
                },
                AccessMode::CREATE,
            ),
        ],
    );
    xfixes_request_checks(
        &mut state,
        &mut backend,
        B,
        &mut b_peer,
        32,
        x11xfixes::DELETE_POINTER_BARRIER,
        &barrier.0.to_le_bytes(),
        &calls,
        &[(
            target(barrier, ResourceKind::PointerBarrier),
            AccessMode::DESTROY,
        )],
    );
}

#[test]
fn xfixes_selection_and_cursor_checks_precede_bad_event_masks() {
    // Mutations: move either window check after its event-mask validation; the policy row must
    // return the policy error, while its no-policy pair keeps the existing BadValue.
    let rows = xfixes_invalid_mask_rows(0x8000_0000);
    for (label, minor, bad_mask) in rows {
        let mut state = ServerState::new();
        let mut peer = install_client(&mut state, B.0);
        let window = ResourceId(B_BASE + 9);
        state.resources.create_window(
            B,
            CreateWindowRequest {
                depth: 24,
                window,
                parent: ROOT_WINDOW,
                width: 40,
                height: 30,
                class: 1,
                visual: crate::resources::ROOT_VISUAL,
                ..Default::default()
            },
        );
        state.xfixes_client_major.insert(B.0, 5);
        let mut backend = RecordingBackend::new();
        let body = if minor == x11xfixes::SELECT_SELECTION_INPUT {
            words(&[window.0, 17, bad_mask])
        } else {
            words(&[window.0, bad_mask])
        };
        let before = (
            state.xfixes_selection_masks.clone(),
            state.xfixes_cursor_masks.clone(),
        );
        let backend_before = backend.calls();
        let bytes = request(
            &mut state,
            &mut backend,
            B,
            &mut peer,
            41,
            140,
            minor,
            &body,
        );
        let no_policy_minor = if minor == x11xfixes::SELECT_SELECTION_INPUT {
            0
        } else {
            minor
        };
        assert_error(
            &bytes,
            x11::error::BAD_VALUE,
            bad_mask,
            140,
            no_policy_minor,
            41,
        );
        assert_eq!(
            (
                state.xfixes_selection_masks.clone(),
                state.xfixes_cursor_masks.clone()
            ),
            before,
            "{label}: no-policy request changed subscriptions"
        );
        assert_eq!(
            backend.calls(),
            backend_before,
            "{label}: no-policy backend call"
        );

        let calls = Arc::new(Mutex::new(Vec::new()));
        let error = DenyError {
            code: x11::error::BAD_ACCESS,
            value: 0x61,
        };
        state.install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
            Arc::clone(&calls),
            window,
            Some(AccessMode::GET_ATTR),
            error,
        )));
        let bytes = request(
            &mut state,
            &mut backend,
            B,
            &mut peer,
            42,
            140,
            minor,
            &body,
        );
        assert_error(&bytes, error.code, error.value, 140, minor, 42);
        assert_eq!(
            (
                state.xfixes_selection_masks.clone(),
                state.xfixes_cursor_masks.clone()
            ),
            before,
            "{label}: denied request changed subscriptions"
        );
        assert_eq!(
            backend.calls(),
            backend_before,
            "{label}: denied request reached backend"
        );
        let calls = calls.lock().unwrap();
        let check = calls
            .iter()
            .find(|call| call.hook == ExtensionHook::Resource)
            .expect("window check recorded before invalid mask");
        let target = ResourceTarget {
            owner: None,
            id: window,
            kind: ResourceKind::Window,
            creation: None,
        };
        assert_eq!(check.client, B);
        assert_eq!(check.target, Some(target));
        assert_eq!(check.mode, Some(AccessMode::GET_ATTR));
        assert_eq!(check.owner, Some(B));
        assert_eq!(check.target_owner, Some(B));
        assert_eq!(check.request, Some(RequestOpcodes { major: 140, minor }));
    }
}

fn xfixes_invalid_mask_rows(mask: u32) -> [(&'static str, u8, u32); 2] {
    const SELECT_SELECTION: u8 = x11xfixes::SELECT_SELECTION_INPUT;
    [
        ("SelectSelectionInput", SELECT_SELECTION, mask),
        ("SelectCursorInput", x11xfixes::SELECT_CURSOR_INPUT, mask),
    ]
}

#[derive(Clone, Copy, Debug)]
enum XfixesDeniedSite {
    ChangeSaveSet,
    SelectSelectionInput,
    SelectCursorInput,
    GetCursorImage,
    GetCursorImageAndName,
    SetCursorName,
    GetCursorName,
    ChangeCursorSource,
    ChangeCursorDestination,
    ChangeCursorByNameSource,
    HideCursor,
    ShowCursor,
    CreatePointerBarrierWindow,
    CreatePointerBarrierResource,
    DeletePointerBarrier,
}

#[test]
fn xfixes_resource_access_deny_table() {
    // Mutation: ignore a resource denial at any listed XFIXES site; its exact error or unchanged
    // state assertion fails.
    let sites = [
        XfixesDeniedSite::ChangeSaveSet,
        XfixesDeniedSite::SelectSelectionInput,
        XfixesDeniedSite::SelectCursorInput,
        XfixesDeniedSite::GetCursorImage,
        XfixesDeniedSite::GetCursorImageAndName,
        XfixesDeniedSite::SetCursorName,
        XfixesDeniedSite::GetCursorName,
        XfixesDeniedSite::ChangeCursorSource,
        XfixesDeniedSite::ChangeCursorDestination,
        XfixesDeniedSite::ChangeCursorByNameSource,
        XfixesDeniedSite::HideCursor,
        XfixesDeniedSite::ShowCursor,
        XfixesDeniedSite::CreatePointerBarrierWindow,
        XfixesDeniedSite::CreatePointerBarrierResource,
        XfixesDeniedSite::DeletePointerBarrier,
    ];
    let denied_error = DenyError {
        code: yserver_protocol::x11::error::BAD_ACCESS,
        value: 0x6a,
    };

    for site in sites {
        let mut state = ServerState::new();
        let mut a_peer = install_client(&mut state, A.0);
        let mut b_peer = install_client(&mut state, B.0);
        set_base(&mut state, A, A_BASE);
        set_base(&mut state, B, B_BASE);
        state.xfixes_client_major.insert(B.0, 5);
        let mut backend = RecordingBackend::new();
        let mut a_sequence = 1;
        let mut b_sequence = 1;
        let (source, source_host) = create_cursor(
            &mut state,
            &mut backend,
            A,
            &mut a_peer,
            &mut a_sequence,
            A_BASE,
        );
        let (destination, destination_host) = create_cursor(
            &mut state,
            &mut backend,
            B,
            &mut b_peer,
            &mut b_sequence,
            B_BASE,
        );
        let window = ResourceId(B_BASE + 3);
        create_window(
            &mut state,
            &mut backend,
            &mut b_peer,
            &mut b_sequence,
            window,
        );
        assert_no_error(
            &set_window_cursor(
                &mut state,
                &mut backend,
                &mut b_peer,
                &mut b_sequence,
                window,
                destination,
            ),
            "B displays destination cursor",
        );

        for (cursor, name) in [(source, b"shared".as_slice()), (destination, b"shared")] {
            let bytes = request(
                &mut state,
                &mut backend,
                B,
                &mut b_peer,
                b_sequence,
                140,
                x11xfixes::SET_CURSOR_NAME,
                &cursor_name_body(cursor, name),
            );
            assert_no_error(&bytes, "seed cursor name");
            b_sequence += 1;
        }
        assert_no_error(
            &request(
                &mut state,
                &mut backend,
                B,
                &mut b_peer,
                b_sequence,
                140,
                x11xfixes::SELECT_SELECTION_INPUT,
                &words(&[window.0, 17, 1]),
            ),
            "seed selection subscription",
        );
        b_sequence += 1;
        assert_no_error(
            &request(
                &mut state,
                &mut backend,
                B,
                &mut b_peer,
                b_sequence,
                140,
                x11xfixes::SELECT_CURSOR_INPUT,
                &words(&[window.0, 1]),
            ),
            "seed cursor subscription",
        );
        b_sequence += 1;
        let mut save_set_body = vec![0, 0, 0, 0];
        save_set_body.extend_from_slice(&window.0.to_le_bytes());
        assert_no_error(
            &request(
                &mut state,
                &mut backend,
                B,
                &mut b_peer,
                b_sequence,
                140,
                x11xfixes::CHANGE_SAVE_SET,
                &save_set_body,
            ),
            "seed save set",
        );
        b_sequence += 1;
        assert_no_error(
            &request(
                &mut state,
                &mut backend,
                B,
                &mut b_peer,
                b_sequence,
                140,
                x11xfixes::HIDE_CURSOR,
                &window.0.to_le_bytes(),
            ),
            "seed hidden cursor",
        );
        b_sequence += 1;
        let existing_barrier = ResourceId(B_BASE + 5);
        assert_no_error(
            &request(
                &mut state,
                &mut backend,
                B,
                &mut b_peer,
                b_sequence,
                140,
                x11xfixes::CREATE_POINTER_BARRIER,
                &pointer_barrier_body(existing_barrier, window),
            ),
            "seed pointer barrier",
        );
        b_sequence += 1;
        backend.active_cursor_image = Some(active_cursor_image(source_host));

        let new_barrier = ResourceId(B_BASE + 4);
        let window_target = ResourceTarget {
            owner: None,
            id: window,
            kind: ResourceKind::Window,
            creation: None,
        };
        let cursor_target = |id| ResourceTarget {
            owner: None,
            id,
            kind: ResourceKind::Cursor,
            creation: None,
        };
        let (minor, body, denied_target, denied_mode, asked) = match site {
            XfixesDeniedSite::ChangeSaveSet => {
                let mut body = vec![1, 0, 0, 0];
                body.extend_from_slice(&window.0.to_le_bytes());
                (
                    x11xfixes::CHANGE_SAVE_SET,
                    body,
                    window_target,
                    AccessMode::MANAGE,
                    vec![(window_target, AccessMode::MANAGE)],
                )
            }
            XfixesDeniedSite::SelectSelectionInput => (
                x11xfixes::SELECT_SELECTION_INPUT,
                words(&[window.0, 17, 0]),
                window_target,
                AccessMode::GET_ATTR,
                vec![(window_target, AccessMode::GET_ATTR)],
            ),
            XfixesDeniedSite::SelectCursorInput => (
                x11xfixes::SELECT_CURSOR_INPUT,
                words(&[window.0, 0]),
                window_target,
                AccessMode::GET_ATTR,
                vec![(window_target, AccessMode::GET_ATTR)],
            ),
            XfixesDeniedSite::GetCursorImage => (
                x11xfixes::GET_CURSOR_IMAGE,
                Vec::new(),
                cursor_target(source),
                AccessMode::READ,
                vec![(cursor_target(source), AccessMode::READ)],
            ),
            XfixesDeniedSite::GetCursorImageAndName => (
                x11xfixes::GET_CURSOR_IMAGE_AND_NAME,
                Vec::new(),
                cursor_target(source),
                AccessMode::READ | AccessMode::GET_ATTR,
                vec![(
                    cursor_target(source),
                    AccessMode::READ | AccessMode::GET_ATTR,
                )],
            ),
            XfixesDeniedSite::SetCursorName => (
                x11xfixes::SET_CURSOR_NAME,
                cursor_name_body(destination, b"changed"),
                cursor_target(destination),
                AccessMode::SET_ATTR,
                vec![(cursor_target(destination), AccessMode::SET_ATTR)],
            ),
            XfixesDeniedSite::GetCursorName => (
                x11xfixes::GET_CURSOR_NAME,
                destination.0.to_le_bytes().to_vec(),
                cursor_target(destination),
                AccessMode::GET_ATTR,
                vec![(cursor_target(destination), AccessMode::GET_ATTR)],
            ),
            XfixesDeniedSite::ChangeCursorSource => (
                x11xfixes::CHANGE_CURSOR,
                words(&[source.0, destination.0]),
                cursor_target(source),
                AccessMode::READ | AccessMode::GET_ATTR,
                vec![(
                    cursor_target(source),
                    AccessMode::READ | AccessMode::GET_ATTR,
                )],
            ),
            XfixesDeniedSite::ChangeCursorDestination => (
                x11xfixes::CHANGE_CURSOR,
                words(&[source.0, destination.0]),
                cursor_target(destination),
                AccessMode::WRITE | AccessMode::SET_ATTR,
                vec![
                    (
                        cursor_target(source),
                        AccessMode::READ | AccessMode::GET_ATTR,
                    ),
                    (
                        cursor_target(destination),
                        AccessMode::WRITE | AccessMode::SET_ATTR,
                    ),
                ],
            ),
            XfixesDeniedSite::ChangeCursorByNameSource => (
                x11xfixes::CHANGE_CURSOR_BY_NAME,
                cursor_name_body(source, b"shared"),
                cursor_target(source),
                AccessMode::READ | AccessMode::GET_ATTR,
                vec![(
                    cursor_target(source),
                    AccessMode::READ | AccessMode::GET_ATTR,
                )],
            ),
            XfixesDeniedSite::HideCursor => (
                x11xfixes::HIDE_CURSOR,
                window.0.to_le_bytes().to_vec(),
                window_target,
                AccessMode::GET_ATTR,
                vec![(window_target, AccessMode::GET_ATTR)],
            ),
            XfixesDeniedSite::ShowCursor => (
                x11xfixes::SHOW_CURSOR,
                window.0.to_le_bytes().to_vec(),
                window_target,
                AccessMode::GET_ATTR,
                vec![(window_target, AccessMode::GET_ATTR)],
            ),
            XfixesDeniedSite::CreatePointerBarrierWindow => (
                x11xfixes::CREATE_POINTER_BARRIER,
                pointer_barrier_body(new_barrier, window),
                window_target,
                AccessMode::READ,
                vec![(window_target, AccessMode::READ)],
            ),
            XfixesDeniedSite::CreatePointerBarrierResource => (
                x11xfixes::CREATE_POINTER_BARRIER,
                pointer_barrier_body(new_barrier, window),
                ResourceTarget {
                    owner: None,
                    id: new_barrier,
                    kind: ResourceKind::PointerBarrier,
                    creation: Some(crate::access::Creation {
                        parent: Some(window),
                        owner: B,
                    }),
                },
                AccessMode::CREATE,
                vec![
                    (window_target, AccessMode::READ),
                    (
                        ResourceTarget {
                            owner: None,
                            id: new_barrier,
                            kind: ResourceKind::PointerBarrier,
                            creation: Some(crate::access::Creation {
                                parent: Some(window),
                                owner: B,
                            }),
                        },
                        AccessMode::CREATE,
                    ),
                ],
            ),
            XfixesDeniedSite::DeletePointerBarrier => (
                x11xfixes::DELETE_POINTER_BARRIER,
                existing_barrier.0.to_le_bytes().to_vec(),
                ResourceTarget {
                    owner: None,
                    id: existing_barrier,
                    kind: ResourceKind::PointerBarrier,
                    creation: None,
                },
                AccessMode::DESTROY,
                vec![(
                    ResourceTarget {
                        owner: None,
                        id: existing_barrier,
                        kind: ResourceKind::PointerBarrier,
                        creation: None,
                    },
                    AccessMode::DESTROY,
                )],
            ),
        };
        let calls = Arc::new(Mutex::new(Vec::new()));
        state.install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
            Arc::clone(&calls),
            denied_target.id,
            Some(denied_mode),
            denied_error,
        )));

        let selection_masks_before = state.xfixes_selection_masks.clone();
        let cursor_masks_before = state.xfixes_cursor_masks.clone();
        let save_set_before = state.clients[&B.0].save_set.clone();
        let hide_counts_before = state.xfixes_cursor_hide_counts.clone();
        let barriers_before = state.pointer_barriers.clone();
        let cursor_state_before = [source, destination].map(|id| {
            (
                id,
                state.resources.cursor_host_xid(id),
                state.resources.cursor_name_atom(id),
            )
        });
        let host_state_before = [source_host, destination_host].map(|host| {
            (
                host,
                state.resources.cursor_host_origin(host),
                state.resources.cursor_name_for_host(host),
            )
        });
        let window_cursor_before = state
            .resources
            .window(window)
            .and_then(|window| window.cursor_host);
        let backend_before = backend.calls();
        let expected = asked
            .iter()
            .map(|(target, mode)| {
                (
                    B,
                    Some(RequestOpcodes { major: 140, minor }),
                    *target,
                    *mode,
                    target
                        .owner
                        .or_else(|| state.access_resource_owner(target.id, target.kind)),
                )
            })
            .collect::<Vec<_>>();

        let bytes = request(
            &mut state,
            &mut backend,
            B,
            &mut b_peer,
            b_sequence,
            140,
            minor,
            &body,
        );
        assert_error(
            &bytes,
            denied_error.code,
            denied_error.value,
            140,
            minor,
            b_sequence,
        );
        let actual = calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.hook == ExtensionHook::Resource)
            .map(|call| {
                (
                    call.client,
                    call.request,
                    call.target.unwrap(),
                    call.mode.unwrap(),
                    call.target_owner,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(actual, expected, "denied XFIXES minor {minor} at {site:?}");

        assert_eq!(
            state.xfixes_selection_masks, selection_masks_before,
            "{site:?}"
        );
        assert_eq!(state.xfixes_cursor_masks, cursor_masks_before, "{site:?}");
        assert_eq!(state.clients[&B.0].save_set, save_set_before, "{site:?}");
        assert_eq!(
            state.xfixes_cursor_hide_counts, hide_counts_before,
            "{site:?}"
        );
        assert_eq!(state.pointer_barriers, barriers_before, "{site:?}");
        assert_eq!(
            [source, destination].map(|id| {
                (
                    id,
                    state.resources.cursor_host_xid(id),
                    state.resources.cursor_name_atom(id),
                )
            }),
            cursor_state_before,
            "{site:?} cursor XIDs and names"
        );
        assert_eq!(
            [source_host, destination_host].map(|host| {
                (
                    host,
                    state.resources.cursor_host_origin(host),
                    state.resources.cursor_name_for_host(host),
                )
            }),
            host_state_before,
            "{site:?} host cursor mappings and names"
        );
        assert_eq!(
            state
                .resources
                .window(window)
                .and_then(|window| window.cursor_host),
            window_cursor_before,
            "{site:?} displayed window cursor"
        );
        assert_eq!(backend.calls(), backend_before, "{site:?} backend calls");
    }
}
