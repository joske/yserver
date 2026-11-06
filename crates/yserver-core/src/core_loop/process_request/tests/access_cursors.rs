use super::{
    extensions::{ExtensionHook, RecordedExtensionCheck, RecordingExtensionPolicy},
    *,
};
use crate::access::{AccessMode, Decision, RequestOpcodes, ResourceKind, ResourceTarget};
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
