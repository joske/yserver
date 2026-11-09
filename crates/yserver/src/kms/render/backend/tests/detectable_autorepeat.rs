use super::*;

use std::os::unix::net::UnixStream;
use yserver_core::{
    backend::Backend,
    core_loop::{DeviceInfo, HostInputEvent, InputOrigin},
    host_x11::HostKeyEvent,
    resources::ROOT_WINDOW,
    server::ServerState,
    xinput::{InputCapabilities, InputSourceId, XiFacetKind},
};
use yserver_protocol::x11::{ClientId, RequestHeader, SequenceNumber};

const XKB_OPCODE: u8 = 136;
const XINPUT_OPCODE: u8 = 137;
const XKB_USE_CORE_KBD: u16 = 0x0100;
const XKB_USE_CORE_PTR: u16 = 0x0200;
const DETECTABLE_AUTO_REPEAT: u32 = 1;
const CORE_KEY_PRESS_MASK: u32 = 1;
const CORE_KEY_RELEASE_MASK: u32 = 2;
const KEYCODE_A: u8 = 38;
const XI_FIRST_EVENT: u8 = 66;

fn request(
    state: &mut ServerState,
    backend: &mut KmsBackend,
    client: u32,
    sequence: u16,
    opcode: u8,
    data: u8,
    body: &[u8],
) {
    yserver_core::core_loop::process_request::process_request(
        state,
        backend,
        ClientId(client),
        SequenceNumber(sequence),
        RequestHeader {
            opcode,
            data,
            length_units: u32::try_from(1 + body.len().div_ceil(4)).unwrap(),
        },
        body,
        None,
    )
    .expect("process request");
}

fn use_xkb(state: &mut ServerState, backend: &mut KmsBackend, client: u32, peer: &mut UnixStream) {
    request(state, backend, client, 1, XKB_OPCODE, 0, &[1, 0, 0, 0]);
    assert_eq!(kbd_map_drain(peer).len(), 32, "UseExtension reply");
}

fn set_detectable_repeat(
    state: &mut ServerState,
    backend: &mut KmsBackend,
    client: u32,
    peer: &mut UnixStream,
    enabled: bool,
) {
    let mut body = [0u8; 24];
    body[0..2].copy_from_slice(&XKB_USE_CORE_KBD.to_le_bytes());
    body[4..8].copy_from_slice(&DETECTABLE_AUTO_REPEAT.to_le_bytes());
    let value = if enabled { 1u32 } else { 0 };
    body[8..12].copy_from_slice(&value.to_le_bytes());
    request(state, backend, client, 2, XKB_OPCODE, 21, &body);
    let reply = kbd_map_drain(peer);
    assert_eq!(
        reply.len(),
        32,
        "PerClientFlags reply; xkb client state: {:?}",
        state.xkb_clients.get(&client)
    );
    assert_eq!(u32::from_le_bytes(reply[12..16].try_into().unwrap()), value);
}

/// Mutation: accept `XkbUseCorePtr` as a keyboard. KMS must return XKB's
/// `BadKeyboard` error and leave this client's flags unchanged.
#[test]
fn per_client_flags_rejects_non_keyboard_device() {
    let mut backend = KmsBackend::for_tests();
    let mut state = ServerState::new();
    let mut peer = client(&mut state, 61);
    use_xkb(&mut state, &mut backend, 61, &mut peer);

    let mut body = [0u8; 24];
    body[0..2].copy_from_slice(&XKB_USE_CORE_PTR.to_le_bytes());
    body[4..8].copy_from_slice(&DETECTABLE_AUTO_REPEAT.to_le_bytes());
    body[8..12].copy_from_slice(&DETECTABLE_AUTO_REPEAT.to_le_bytes());
    request(&mut state, &mut backend, 61, 2, XKB_OPCODE, 21, &body);
    let error = kbd_map_drain(&mut peer);
    assert_eq!(error.len(), 32);
    assert_eq!((error[0], error[1]), (0, 162)); // XKB BadKeyboard
    assert_eq!(&error[4..8], &0x0200_0200u32.to_le_bytes());

    let mut query = [0u8; 24];
    query[0..2].copy_from_slice(&XKB_USE_CORE_KBD.to_le_bytes());
    request(&mut state, &mut backend, 61, 3, XKB_OPCODE, 21, &query);
    let reply = kbd_map_drain(&mut peer);
    assert_eq!(reply.len(), 32);
    assert_eq!(&reply[12..16], &0u32.to_le_bytes());
}

fn select_core(state: &mut ServerState, backend: &mut KmsBackend, client: u32, mask: u32) {
    let mut body = Vec::with_capacity(12);
    body.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
    body.extend_from_slice(&(1u32 << 11).to_le_bytes()); // CWEventMask
    body.extend_from_slice(&mask.to_le_bytes());
    request(state, backend, client, 3, 2, 0, &body); // ChangeWindowAttributes
}

fn select_xi1_keys(
    state: &mut ServerState,
    backend: &mut KmsBackend,
    client: u32,
    peer: &mut UnixStream,
    deviceid: u16,
) {
    let mut body = Vec::with_capacity(16);
    body.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
    body.extend_from_slice(&2u16.to_le_bytes()); // two XEventClasses
    body.extend_from_slice(&0u16.to_le_bytes());
    for offset in [
        yserver_core::xinput::XI_DEVICE_KEY_PRESS_OFFSET,
        yserver_core::xinput::XI_DEVICE_KEY_RELEASE_OFFSET,
    ] {
        let class = (u32::from(deviceid) << 8) | u32::from(XI_FIRST_EVENT + offset);
        body.extend_from_slice(&class.to_le_bytes());
    }
    request(state, backend, client, 4, XINPUT_OPCODE, 6, &body);
    let response = kbd_map_drain(peer);
    assert!(response.is_empty(), "SelectExtensionEvent: {response:02x?}");
}

fn event_types(bytes: &[u8]) -> Vec<u8> {
    assert_eq!(bytes.len() % 32, 0, "core/XI1 events are 32 bytes");
    bytes.chunks_exact(32).map(|event| event[0]).collect()
}

fn key_event(pressed: bool) -> HostKeyEvent {
    HostKeyEvent {
        origin: InputOrigin::NestedHost,
        pressed,
        keycode: KEYCODE_A,
        time: 0,
        root_x: 0,
        root_y: 0,
        event_x: 0,
        event_y: 0,
        state: 0,
    }
}

fn key_input(backend: &mut KmsBackend, state: &mut ServerState, pressed: bool) {
    backend.on_host_input(state, HostInputEvent::Key(key_event(pressed)));
}

fn repeat_pair(backend: &mut KmsBackend, state: &mut ServerState) {
    backend.on_host_input(state, HostInputEvent::KeyRepeat(key_event(false)));
    backend.on_host_input(state, HostInputEvent::KeyRepeat(key_event(true)));
}

fn client(state: &mut ServerState, id: u32) -> UnixStream {
    kbd_map_client_id(state, id)
}

/// Mutation: drop every synthetic release. The release-only selector must
/// keep receiving today's release then the press-only selector keeps only its press.
#[test]
fn held_key_unflagged_core_gets_todays_pairs() {
    let mut backend = KmsBackend::for_tests();
    let mut state = ServerState::new();
    state.core_focus.raw = ROOT_WINDOW.0;
    let mut both = client(&mut state, 51);
    let mut press_only = client(&mut state, 52);
    let mut release_only = client(&mut state, 53);
    select_core(
        &mut state,
        &mut backend,
        51,
        CORE_KEY_PRESS_MASK | CORE_KEY_RELEASE_MASK,
    );
    select_core(&mut state, &mut backend, 52, CORE_KEY_PRESS_MASK);
    select_core(&mut state, &mut backend, 53, CORE_KEY_RELEASE_MASK);

    key_input(&mut backend, &mut state, true);
    let _ = kbd_map_drain(&mut both);
    let _ = kbd_map_drain(&mut press_only);
    let _ = kbd_map_drain(&mut release_only);
    repeat_pair(&mut backend, &mut state);

    let both_bytes = kbd_map_drain(&mut both);
    let press_bytes = kbd_map_drain(&mut press_only);
    let release_bytes = kbd_map_drain(&mut release_only);
    assert_eq!(event_types(&both_bytes), [3, 2]);
    assert_eq!(event_types(&press_bytes), [2]);
    assert_eq!(event_types(&release_bytes), [3]);
    let mut normalized_release = both_bytes[..32].to_vec();
    normalized_release[0] = 2;
    assert_eq!(&normalized_release[1..4], &both_bytes[33..36]);
    assert_eq!(&normalized_release[8..32], &both_bytes[40..64]);
}

/// Mutation: ignore the stored DetectableAutoRepeat flag. A flagged client
/// must see a press for each repeat and the real key-up release exactly once;
/// disabling the flag restores the old pair.
#[test]
fn held_key_flagged_core_gets_presses_only() {
    let mut backend = KmsBackend::for_tests();
    let mut state = ServerState::new();
    state.core_focus.raw = ROOT_WINDOW.0;
    let mut peer = client(&mut state, 54);
    use_xkb(&mut state, &mut backend, 54, &mut peer);
    set_detectable_repeat(&mut state, &mut backend, 54, &mut peer, true);
    select_core(
        &mut state,
        &mut backend,
        54,
        CORE_KEY_PRESS_MASK | CORE_KEY_RELEASE_MASK,
    );
    key_input(&mut backend, &mut state, true);
    let _ = kbd_map_drain(&mut peer);

    repeat_pair(&mut backend, &mut state);
    assert_eq!(event_types(&kbd_map_drain(&mut peer)), [2]);
    key_input(&mut backend, &mut state, false);
    assert_eq!(event_types(&kbd_map_drain(&mut peer)), [3]);

    key_input(&mut backend, &mut state, true);
    let _ = kbd_map_drain(&mut peer);
    set_detectable_repeat(&mut state, &mut backend, 54, &mut peer, false);
    repeat_pair(&mut backend, &mut state);
    assert_eq!(event_types(&kbd_map_drain(&mut peer)), [3, 2]);
}

/// Mutation: ignore the stored per-client flag in normal window fanout. On
/// one window, the flagged recipient gets only the press while the unflagged
/// recipient retains the release+press pair.
#[test]
fn mixed_core_clients_get_their_own_form() {
    let mut backend = KmsBackend::for_tests();
    let mut state = ServerState::new();
    state.core_focus.raw = ROOT_WINDOW.0;
    let mut flagged = client(&mut state, 55);
    let mut unflagged = client(&mut state, 56);
    use_xkb(&mut state, &mut backend, 55, &mut flagged);
    set_detectable_repeat(&mut state, &mut backend, 55, &mut flagged, true);
    select_core(&mut state, &mut backend, 55, 3);
    select_core(&mut state, &mut backend, 56, 3);
    key_input(&mut backend, &mut state, true);
    let _ = kbd_map_drain(&mut flagged);
    let _ = kbd_map_drain(&mut unflagged);

    repeat_pair(&mut backend, &mut state);
    assert_eq!(event_types(&kbd_map_drain(&mut flagged)), [2]);
    assert_eq!(event_types(&kbd_map_drain(&mut unflagged)), [3, 2]);
}

fn add_physical_keyboard(
    backend: &mut KmsBackend,
    state: &mut ServerState,
    source: InputSourceId,
) -> u16 {
    state.core_focus.raw = ROOT_WINDOW.0;
    backend.on_host_input(
        state,
        HostInputEvent::DeviceAdded(DeviceInfo {
            source_id: source,
            enabled: true,
            resume_key: None,
            capabilities: InputCapabilities {
                keyboard: true,
                pointer: false,
                touch: false,
            },
            name: "DetectableAutoRepeat XI1 test keyboard".to_owned(),
            device_node: "/dev/input/xi1-detectable-repeat".to_owned(),
            sysname: "xi1-detectable-repeat".to_owned(),
            vendor_id: 0,
            product_id: 0,
            is_touchpad: false,
            config: Default::default(),
        }),
    );
    state
        .xi_devices
        .facet(source, XiFacetKind::Keyboard)
        .expect("physical keyboard facet")
}

fn physical_key_event(origin: InputOrigin, pressed: bool) -> HostKeyEvent {
    HostKeyEvent {
        origin,
        pressed,
        keycode: KEYCODE_A,
        time: 0,
        root_x: 0,
        root_y: 0,
        event_x: 0,
        event_y: 0,
        state: 0,
    }
}

/// Mutation: ignore the stored flag in the XI1 writer. The unflagged XI1
/// client retains both event classes, while the flagged client skips only
/// the repeat release.
#[test]
fn xi1_repeat_honours_flag() {
    let mut backend = KmsBackend::for_tests();
    let mut state = ServerState::new();
    state.core_focus.raw = ROOT_WINDOW.0;
    let source = InputSourceId(0x3551);
    let mut flagged = client(&mut state, 57);
    let mut unflagged = client(&mut state, 58);
    use_xkb(&mut state, &mut backend, 57, &mut flagged);
    set_detectable_repeat(&mut state, &mut backend, 57, &mut flagged, true);
    let deviceid = add_physical_keyboard(&mut backend, &mut state, source);
    select_xi1_keys(&mut state, &mut backend, 57, &mut flagged, deviceid);
    select_xi1_keys(&mut state, &mut backend, 58, &mut unflagged, deviceid);
    assert_eq!(
        state.clients[&57].xi1_window_event_classes[&ROOT_WINDOW].len(),
        2
    );
    assert_eq!(
        state.clients[&58].xi1_window_event_classes[&ROOT_WINDOW].len(),
        2
    );
    let origin = InputOrigin::Physical(source);
    backend.on_host_input(
        &mut state,
        HostInputEvent::Key(physical_key_event(origin, true)),
    );
    let _ = kbd_map_drain(&mut flagged);
    let _ = kbd_map_drain(&mut unflagged);

    backend.on_host_input(
        &mut state,
        HostInputEvent::KeyRepeat(physical_key_event(origin, false)),
    );
    backend.on_host_input(
        &mut state,
        HostInputEvent::KeyRepeat(physical_key_event(origin, true)),
    );
    assert_eq!(
        event_types(&kbd_map_drain(&mut flagged)),
        [XI_FIRST_EVENT + yserver_core::xinput::XI_DEVICE_KEY_PRESS_OFFSET]
    );
    assert_eq!(
        event_types(&kbd_map_drain(&mut unflagged)),
        [
            XI_FIRST_EVENT + yserver_core::xinput::XI_DEVICE_KEY_RELEASE_OFFSET,
            XI_FIRST_EVENT + yserver_core::xinput::XI_DEVICE_KEY_PRESS_OFFSET
        ]
    );

    backend.on_host_input(
        &mut state,
        HostInputEvent::Key(physical_key_event(origin, false)),
    );
    let release = XI_FIRST_EVENT + yserver_core::xinput::XI_DEVICE_KEY_RELEASE_OFFSET;
    assert_eq!(event_types(&kbd_map_drain(&mut flagged)), [release]);
    assert_eq!(event_types(&kbd_map_drain(&mut unflagged)), [release]);
}

fn grab_keyboard(
    state: &mut ServerState,
    backend: &mut KmsBackend,
    client: u32,
    peer: &mut UnixStream,
    keyboard_mode: u8,
) {
    let mut body = Vec::with_capacity(12);
    body.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes()); // CurrentTime
    body.extend_from_slice(&[1, keyboard_mode, 0, 0]); // async pointer, requested keyboard mode
    request(state, backend, client, 5, 31, 0, &body); // XGrabKeyboard
    let reply = kbd_map_drain(peer);
    assert_eq!(reply.len(), 32, "GrabKeyboard reply");
    assert_eq!(reply[1], 0, "keyboard grab succeeded");
    let _ = kbd_map_drain(peer); // focus notifications
}

/// Mutation: omit the synthetic marker from the grab-owner path. The flagged
/// core grab owner skips the synthetic release but still gets the real key-up.
#[test]
fn flagged_grab_owner_gets_no_synthetic_release() {
    let mut backend = KmsBackend::for_tests();
    let mut state = ServerState::new();
    let mut peer = client(&mut state, 59);
    use_xkb(&mut state, &mut backend, 59, &mut peer);
    set_detectable_repeat(&mut state, &mut backend, 59, &mut peer, true);
    grab_keyboard(&mut state, &mut backend, 59, &mut peer, 1); // asynchronous keyboard grab

    key_input(&mut backend, &mut state, true);
    let _ = kbd_map_drain(&mut peer);
    set_detectable_repeat(&mut state, &mut backend, 59, &mut peer, false);
    repeat_pair(&mut backend, &mut state);
    assert_eq!(event_types(&kbd_map_drain(&mut peer)), [3, 2]);
    set_detectable_repeat(&mut state, &mut backend, 59, &mut peer, true);
    repeat_pair(&mut backend, &mut state);
    assert_eq!(event_types(&kbd_map_drain(&mut peer)), [2]);
    key_input(&mut backend, &mut state, false);
    assert_eq!(event_types(&kbd_map_drain(&mut peer)), [3]);
}

/// Mutation: drop the marker when storing HostKeyTransition in the frozen
/// keyboard queue. After thaw, the flagged grab owner must still see only the
/// repeat press, followed by the real key-up release.
#[test]
fn frozen_keyboard_replay_keeps_marker() {
    let mut backend = KmsBackend::for_tests();
    let mut state = ServerState::new();
    let mut peer = client(&mut state, 60);
    use_xkb(&mut state, &mut backend, 60, &mut peer);
    set_detectable_repeat(&mut state, &mut backend, 60, &mut peer, true);
    grab_keyboard(&mut state, &mut backend, 60, &mut peer, 0); // synchronous keyboard grab

    key_input(&mut backend, &mut state, true);
    repeat_pair(&mut backend, &mut state);
    assert!(
        kbd_map_drain(&mut peer).is_empty(),
        "frozen events are queued"
    );
    assert!(
        !state.sync_pending.is_empty(),
        "the keyboard queue retains input"
    );

    request(&mut state, &mut backend, 60, 6, 35, 3, &0u32.to_le_bytes()); // AsyncKeyboard
    let replayed = event_types(&kbd_map_drain(&mut peer));
    assert_eq!(replayed, [2, 2], "initial press + repeat press");

    key_input(&mut backend, &mut state, false);
    assert_eq!(event_types(&kbd_map_drain(&mut peer)), [3]);
}
