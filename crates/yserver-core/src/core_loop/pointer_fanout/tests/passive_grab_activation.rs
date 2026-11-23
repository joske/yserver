//! Where a passive button grab's activating press is reported (GH #277).

use super::{
    implicit_grabs::{
        Fixture, assert_end_state, assert_end_state_with_xi1_entries, core, create_window,
        dispatch, map_window, move_to_out, press_over_d, read_packets, release, select_core,
        select_xi2, send, xi,
    },
    *,
};
use crate::{
    backend::recording::RecordingBackend, core_loop::InputOrigin, host_x11::HostXidMap,
    server::ServerState,
};
use yserver_protocol::x11::{ClientId, ResourceId};

const CLIENT_A: u32 = 0xA753;
const CLIENT_B: u32 = 0xA754;
const CLIENT_C: u32 = 0xA756;
const W: u32 = 0x0010_A753;
const D: u32 = 0x0020_A753;
const OUT: u32 = 0x0010_A754;
const TP14_WIN: u32 = 0x0010_A757;
const HOST_W: u32 = 0xCAFE_A753;
const HOST_OUT: u32 = 0xCAFE_A754;
const XI2_OPCODE: u8 = 137;
const XI_XI_ALLOW_EVENTS: u8 = 53;
const XI_PASSIVE_GRAB_DEVICE: u8 = 54;
#[allow(dead_code)] // shared request helper for the following passive-grab task
const XI_PASSIVE_UNGRAB_DEVICE: u8 = 55;
const RECORD_CREATE_CONTEXT: u8 = yserver_protocol::x11::record::CREATE_CONTEXT;
const RECORD_ENABLE_CONTEXT: u8 = yserver_protocol::x11::record::ENABLE_CONTEXT;
const RECORD_CLIENT: u32 = 0xA755;
const RECORD_CONTEXT: u32 = 0x0300_0001;
const XI_MASTER: u16 = 2;
const XI_MASTER_KEYBOARD: u16 = 3;
const XI_XTEST: u16 = 4;
const XI_ALL_DEVICES: u16 = 0;
const XI_MOTION: u32 = 1 << 6;
const XI_BUTTON_PRESS: u32 = 1 << 4;
const XI_BUTTON_RELEASE: u32 = 1 << 5;
const XI_BUTTONS: u32 = XI_BUTTON_PRESS | XI_BUTTON_RELEASE;
const XI_GRAB_MASK: u32 = XI_MOTION | XI_BUTTONS;
const ANY_BUTTON: u8 = 0;
const ANY_MODIFIER: u16 = 0x8000;
const BUTTON_1: u16 = 1;
const BUTTON_3: u16 = 1 << 2;

/// Uses the shared request-dispatched windows and capture clients, with D
/// owned by B as in the passive-grab measurements.
fn p_fixture() -> Fixture {
    let mut state = ServerState::new();
    let client_a = super::install_capture_client(&mut state, CLIENT_A);
    let client_b = super::install_capture_client(&mut state, CLIENT_B);
    let mut f = Fixture {
        state,
        backend: RecordingBackend::new(),
        xid_map: HostXidMap::new(),
        client_a,
        client_b,
        pointer: (145, 145),
        time: 1000,
    };
    create_window(&mut f, CLIENT_A, W, 0, 100, 100, 300, 300);
    map_window(&mut f, CLIENT_A, W);
    create_window(&mut f, CLIENT_B, D, W, 20, 20, 100, 100);
    map_window(&mut f, CLIENT_B, D);
    create_window(&mut f, CLIENT_B, OUT, 0, 550, 100, 300, 300);
    map_window(&mut f, CLIENT_B, OUT);
    f.xid_map.insert(HOST_W, ResourceId(W));
    f.xid_map.insert(HOST_OUT, ResourceId(OUT));
    f
}

fn grab_button_core(
    f: &mut Fixture,
    client: u32,
    window: u32,
    button: u8,
    modifiers: u16,
    pointer_mode: u8,
    owner_events: bool,
) {
    let mut body = Vec::with_capacity(20);
    body.extend_from_slice(&window.to_le_bytes());
    body.extend_from_slice(&0x000cu16.to_le_bytes()); // ButtonPress|ButtonRelease
    body.extend_from_slice(&[pointer_mode, 1]); // pointer mode, async keyboard mode
    body.extend_from_slice(&0u32.to_le_bytes()); // no confinement
    body.extend_from_slice(&0u32.to_le_bytes()); // no cursor
    body.extend_from_slice(&[button, 0]);
    body.extend_from_slice(&modifiers.to_le_bytes());
    dispatch(f, client, 28, u8::from(owner_events), &body);
    assert!(f.state.button_grabs.iter().any(|grab| {
        grab.owner == ClientId(client)
            && grab.grab_window == ResourceId(window)
            && grab.button == button
            && grab.modifiers == modifiers
            && !grab.via_xi2
    }));
}

#[allow(dead_code)] // shared request helper for the following passive-grab task
pub(super) fn ungrab_button_core(
    f: &mut Fixture,
    client: u32,
    window: u32,
    button: u8,
    modifiers: u16,
) {
    let mut body = Vec::with_capacity(8);
    body.extend_from_slice(&window.to_le_bytes());
    body.extend_from_slice(&modifiers.to_le_bytes());
    body.extend_from_slice(&[0; 2]); // request padding
    dispatch(f, client, 29, button, &body);
}

fn xi_grab_button(
    f: &mut Fixture,
    client: u32,
    device: u16,
    window: u32,
    button: u8,
    mode: u8,
    owner_events: bool,
    mask: u32,
) {
    let mut body = Vec::with_capacity(36);
    body.extend_from_slice(&0u32.to_le_bytes()); // CurrentTime
    body.extend_from_slice(&window.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes()); // no cursor
    body.extend_from_slice(&u32::from(button).to_le_bytes());
    body.extend_from_slice(&device.to_le_bytes());
    body.extend_from_slice(&1u16.to_le_bytes()); // one modifier
    body.extend_from_slice(&1u16.to_le_bytes()); // one mask word
    body.extend_from_slice(&[0, mode, 1, u8::from(owner_events)]); // Button, mode, async paired, owner_events
    body.extend_from_slice(&0u16.to_le_bytes()); // pad
    body.extend_from_slice(&mask.to_le_bytes());
    body.extend_from_slice(&0x8000_0000u32.to_le_bytes()); // XIAnyModifier
    dispatch(f, client, XI2_OPCODE, XI_PASSIVE_GRAB_DEVICE, &body);
    assert!(f.state.button_grabs.iter().any(|grab| {
        grab.owner == ClientId(client)
            && grab.grab_window == ResourceId(window)
            && grab.device_id == device
            && grab.button == button
            && grab.modifiers == ANY_MODIFIER
            && grab.via_xi2
    }));
}

#[allow(dead_code)] // shared request helper for the following passive-grab task
pub(super) fn xi_ungrab_button(f: &mut Fixture, client: u32, device: u16, window: u32, button: u8) {
    let mut body = Vec::with_capacity(20);
    body.extend_from_slice(&window.to_le_bytes());
    body.extend_from_slice(&u32::from(button).to_le_bytes());
    body.extend_from_slice(&device.to_le_bytes());
    body.extend_from_slice(&1u16.to_le_bytes()); // one modifier
    body.push(0); // Button grab type
    body.extend_from_slice(&[0; 3]); // pad
    body.extend_from_slice(&0x8000_0000u32.to_le_bytes()); // XIAnyModifier
    dispatch(f, client, XI2_OPCODE, XI_PASSIVE_UNGRAB_DEVICE, &body);
}

fn allow_events_core(f: &mut Fixture, client: u32, mode: u8) {
    dispatch(f, client, 35, mode, &0u32.to_le_bytes()); // CurrentTime
}

fn xi_allow_events(f: &mut Fixture, client: u32, device: u16, mode: u8) {
    let mut body = Vec::with_capacity(8);
    body.extend_from_slice(&0u32.to_le_bytes()); // CurrentTime
    body.extend_from_slice(&device.to_le_bytes());
    body.extend_from_slice(&[mode, 0]);
    dispatch(f, client, XI2_OPCODE, XI_XI_ALLOW_EVENTS, &body);
}

fn enable_record_button_events(f: &mut Fixture, peer: &mut crate::transport::CapturedPeer) {
    use yserver_protocol::x11::{ClientByteOrder, record};

    let mut body = Vec::with_capacity(record::REGISTER_FIXED_BODY + 4 + record::RANGE_SIZE);
    body.extend_from_slice(&RECORD_CONTEXT.to_le_bytes());
    body.extend_from_slice(&[0; 4]); // no element header
    body.extend_from_slice(&1u32.to_le_bytes()); // one client spec
    body.extend_from_slice(&1u32.to_le_bytes()); // one range
    body.extend_from_slice(&record::ALL_CLIENTS.to_le_bytes());
    record::Range {
        device_events: (4, 5), // ButtonPress through ButtonRelease
        ..record::Range::default()
    }
    .encode(ClientByteOrder::LittleEndian, &mut body);
    dispatch(
        f,
        RECORD_CLIENT,
        crate::nested::RECORD_MAJOR_OPCODE,
        RECORD_CREATE_CONTEXT,
        &body,
    );
    dispatch(
        f,
        RECORD_CLIENT,
        crate::nested::RECORD_MAJOR_OPCODE,
        RECORD_ENABLE_CONTEXT,
        &RECORD_CONTEXT.to_le_bytes(),
    );
    let start_marker = read_all_capture_available(peer);
    assert_eq!(
        start_marker.len(),
        32,
        "EnableContext sends its start marker"
    );
    assert_eq!(start_marker[0], 1, "start marker is an X reply");
}

/// Adds Task 1's held-button invariant to the shared grab/freeze end check.
fn assert_task1_end_state(f: &Fixture, xi1_entries: &[u16]) {
    if xi1_entries.is_empty() {
        assert_end_state(f);
    } else {
        assert_end_state_with_xi1_entries(f, xi1_entries);
    }
    assert_eq!(f.state.buttons_down, 0, "master has no held buttons");
    for device in f.state.xi_devices.devices().iter().filter(|device| {
        device.id == XI_XTEST || device.facet == Some(crate::xinput::XiFacetKind::PointerTouch)
    }) {
        assert_eq!(
            device.buttons_down, 0,
            "pointer device {} has no held buttons",
            device.id
        );
    }
    assert!(
        f.state.unpublished_pointer_buttons_down.is_empty(),
        "unpublished pointer sources have no held buttons"
    );
}

/// Reproduces PR1: a client's ReplayPointer must not consume another
/// client's frozen pointer event merely because its keyboard grab synced it.
#[test]
fn replay_pointer_from_synced_only_client_is_a_noop() {
    use crate::server::Xi1SyncState;

    let mut f = p_fixture();
    assert_eq!(f.state.buttons_down, 0);
    select_xi2(&mut f, CLIENT_B, D, XI_MASTER, XI_GRAB_MASK);
    grab_button_core(&mut f, CLIENT_A, W, 1, ANY_MODIFIER, 0, false);

    press_over_d(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    assert_eq!(read_packets(&mut f.client_a), vec![core(4, W)]);
    assert!(read_packets(&mut f.client_b).is_empty());
    assert_eq!(
        f.state.xi1_frozen[&XI_MASTER].state,
        Xi1SyncState::FrozenWithEvent
    );
    assert!(f.state.xi1_frozen[&XI_MASTER].stored.is_some());

    // A's explicit sync GrabPointer replaces the passive grab and leaves the
    // event non-replayable, as CheckGrabForSyncs does for a new sync grab.
    grab_core_pointer_sync(&mut f, CLIENT_A, W);
    assert_eq!(
        f.state.xi1_frozen[&XI_MASTER].state,
        Xi1SyncState::FrozenNoEvent
    );
    assert!(f.state.xi1_frozen[&XI_MASTER].stored.is_some());
    grab_keyboard_sync_pointer(&mut f, CLIENT_B, W);
    assert_eq!(
        f.state.xi1_frozen[&XI_MASTER].other,
        Some(ClientId(CLIENT_B))
    );

    // Drain setup replies/crossings so the rejected request is checked as
    // raw client output, including errors and crossing events.
    let _ = read_all_capture_available(&mut f.client_a);
    let _ = read_all_capture_available(&mut f.client_b);
    allow_events_core(&mut f, CLIENT_B, 2); // ReplayPointer
    assert!(
        read_all_capture_available(&mut f.client_a).is_empty(),
        "B's rejected replay produces no packet for A"
    );
    assert!(
        read_all_capture_available(&mut f.client_b).is_empty(),
        "B's rejected replay produces no packet for B"
    );
    let grab = f
        .state
        .active_pointer_grab
        .expect("A's pointer grab remains active");
    assert_eq!(grab.owner, ClientId(CLIENT_A));
    assert!(!grab.passive && !grab.implicit);
    assert_eq!(
        f.state.xi1_frozen[&XI_MASTER].state,
        Xi1SyncState::FrozenNoEvent
    );
    assert_eq!(
        f.state.xi1_frozen[&XI_MASTER].other,
        Some(ClientId(CLIENT_B))
    );
    assert!(f.state.xi1_frozen[&XI_MASTER].stored.is_some());

    let _ = read_all_capture_available(&mut f.client_a);
    let _ = read_all_capture_available(&mut f.client_b);
    allow_events_core(&mut f, CLIENT_A, 2); // ReplayPointer while FrozenNoEvent
    assert!(
        read_all_capture_available(&mut f.client_a).is_empty(),
        "A's rejected replay produces no packet for A"
    );
    assert!(
        read_all_capture_available(&mut f.client_b).is_empty(),
        "A's rejected replay produces no packet for B"
    );
    let still_grabbed = f
        .state
        .active_pointer_grab
        .expect("A's pointer grab remains active");
    assert_eq!(still_grabbed.owner, grab.owner);
    assert_eq!(still_grabbed.grab_window, grab.grab_window);
    assert_eq!(still_grabbed.passive, grab.passive);
    assert_eq!(still_grabbed.implicit, grab.implicit);
    assert_eq!(
        f.state.xi1_frozen[&XI_MASTER].state,
        Xi1SyncState::FrozenNoEvent
    );
    assert_eq!(
        f.state.xi1_frozen[&XI_MASTER].other,
        Some(ClientId(CLIENT_B))
    );
    assert!(f.state.xi1_frozen[&XI_MASTER].stored.is_some());

    ungrab_core_pointer_request(&mut f, CLIENT_A);
    ungrab_keyboard(&mut f, CLIENT_B);
    release(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    assert!(read_packets(&mut f.client_a).is_empty());
    assert_eq!(
        read_packets(&mut f.client_b),
        vec![xi(5, XI_MASTER, XI_XTEST, D)],
        "PR1: after both ungrabs, only the natural XI2 release is delivered"
    );
    assert_task1_end_state(&f, &[XI_MASTER, XI_MASTER_KEYBOARD]);
}

/// Reproduces PR2: a synced-only ReplayPointer is inert, then the grab owner
/// can still replay the original FrozenWithEvent press below W.
#[test]
fn owner_replay_after_synced_client_noop_replays() {
    use crate::server::Xi1SyncState;

    let mut f = p_fixture();
    assert_eq!(f.state.buttons_down, 0);
    select_xi2(&mut f, CLIENT_B, D, XI_MASTER, XI_GRAB_MASK);
    grab_button_core(&mut f, CLIENT_A, W, 1, ANY_MODIFIER, 0, false);

    press_over_d(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    assert_eq!(read_packets(&mut f.client_a), vec![core(4, W)]);
    assert!(read_packets(&mut f.client_b).is_empty());
    let passive = f
        .state
        .active_pointer_grab
        .expect("A's passive grab is active");
    assert!(passive.passive && !passive.implicit);
    assert_eq!(
        f.state.xi1_frozen[&XI_MASTER].state,
        Xi1SyncState::FrozenWithEvent
    );

    grab_keyboard_sync_pointer(&mut f, CLIENT_B, W);
    assert_eq!(
        f.state.xi1_frozen[&XI_MASTER].other,
        Some(ClientId(CLIENT_B))
    );
    let _ = read_all_capture_available(&mut f.client_a);
    let _ = read_all_capture_available(&mut f.client_b);
    allow_events_core(&mut f, CLIENT_B, 2); // ReplayPointer from synced-only B
    assert!(
        read_all_capture_available(&mut f.client_a).is_empty(),
        "B's rejected replay produces no packet for A"
    );
    assert!(
        read_all_capture_available(&mut f.client_b).is_empty(),
        "B's rejected replay produces no packet for B"
    );
    let still_passive = f
        .state
        .active_pointer_grab
        .expect("A's passive pointer grab remains active");
    assert_eq!(still_passive.owner, passive.owner);
    assert_eq!(still_passive.grab_window, passive.grab_window);
    assert_eq!(still_passive.passive, passive.passive);
    assert_eq!(still_passive.implicit, passive.implicit);
    assert_eq!(
        f.state.xi1_frozen[&XI_MASTER].state,
        Xi1SyncState::FrozenWithEvent
    );
    assert_eq!(
        f.state.xi1_frozen[&XI_MASTER].other,
        Some(ClientId(CLIENT_B))
    );
    assert!(f.state.xi1_frozen[&XI_MASTER].stored.is_some());

    allow_events_core(&mut f, CLIENT_B, 0); // AsyncPointer clears only B's sync hold
    assert!(read_packets(&mut f.client_a).is_empty());
    assert!(read_packets(&mut f.client_b).is_empty());
    assert_eq!(f.state.xi1_frozen[&XI_MASTER].other, None);
    assert_eq!(
        f.state.xi1_frozen[&XI_MASTER].state,
        Xi1SyncState::FrozenWithEvent
    );
    assert!(f.state.xi1_frozen[&XI_MASTER].stored.is_some());

    allow_events_core(&mut f, CLIENT_A, 2); // ReplayPointer
    let implicit = f
        .state
        .active_pointer_grab
        .expect("the replayed master press creates B's implicit grab on D");
    assert_eq!(implicit.owner, ClientId(CLIENT_B));
    assert_eq!(implicit.grab_window, ResourceId(D));
    assert!(implicit.implicit && !implicit.passive && implicit.via_xi2);
    assert_eq!(f.state.buttons_down, BUTTON_1);
    assert_eq!(
        read_packets(&mut f.client_b),
        vec![xi(4, XI_MASTER, XI_XTEST, D)],
        "PR2: A's replay delivers the press as a master XI2 event below W"
    );
    assert!(read_packets(&mut f.client_a).is_empty());

    ungrab_keyboard(&mut f, CLIENT_B);
    release(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    assert_eq!(
        read_packets(&mut f.client_b),
        vec![xi(5, XI_MASTER, XI_XTEST, D)]
    );
    assert!(read_packets(&mut f.client_a).is_empty());
    assert_task1_end_state(&f, &[XI_MASTER, XI_MASTER_KEYBOARD]);
}

/// Exercises the shared ReplayKeyboard admission path with a core key grab:
/// a pointer grab's paired sync must not replay another client's key.
#[test]
fn replay_keyboard_from_synced_only_client_is_a_noop() {
    use crate::{
        host_x11::HostKeyEvent,
        server::{ActiveKeyboardGrabSource, Xi1SyncState},
    };

    const KEYCODE: u8 = 38;
    const KEY_PRESS_RELEASE_MASK: u32 = 0x0000_0003;
    const ROOT: u32 = crate::resources::ROOT_WINDOW.0;
    let mut f = p_fixture();
    assert_eq!(f.state.buttons_down, 0);
    select_core(&mut f, CLIENT_B, D, KEY_PRESS_RELEASE_MASK);
    let mut set_focus = Vec::with_capacity(8);
    set_focus.extend_from_slice(&D.to_le_bytes());
    set_focus.extend_from_slice(&0u32.to_le_bytes()); // CurrentTime
    dispatch(&mut f, CLIENT_B, 42, 0, &set_focus); // SetInputFocus

    let mut grab_key = Vec::with_capacity(12);
    grab_key.extend_from_slice(&ROOT.to_le_bytes());
    grab_key.extend_from_slice(&0u16.to_le_bytes()); // modifiers
    grab_key.extend_from_slice(&[KEYCODE, 1, 0, 0, 0, 0]); // async pointer, sync keyboard
    dispatch(&mut f, CLIENT_A, 33, 0, &grab_key); // GrabKey
    assert!(f.state.key_grabs.iter().any(|grab| {
        grab.owner == ClientId(CLIENT_A)
            && grab.grab_window.0 == ROOT
            && grab.keycode == KEYCODE
            && grab.keyboard_mode == 0
    }));

    crate::core_loop::key_fanout::key_event_fanout_to_state(
        &mut f.state,
        &mut f.backend,
        HostKeyEvent {
            origin: InputOrigin::NestedHost,
            pressed: true,
            keycode: KEYCODE,
            time: f.time,
            root_x: 145,
            root_y: 145,
            event_x: 145,
            event_y: 145,
            state: 0,
        },
    );
    assert!(f.state.active_keyboard_grab.is_some_and(|grab| {
        grab.owner == ClientId(CLIENT_A)
            && matches!(
                grab.source,
                ActiveKeyboardGrabSource::PassiveKey { keycode: KEYCODE }
            )
    }));
    // Xorg ActivatePassiveGrab promotes the activating sync key to
    // FrozenWithEvent while preserving it for ReplayKeyboard.
    assert_eq!(
        f.state.xi1_frozen[&XI_MASTER_KEYBOARD].state,
        Xi1SyncState::FrozenWithEvent
    );
    assert!(f.state.xi1_frozen[&XI_MASTER_KEYBOARD].stored.is_some());
    let _ = read_all_capture_available(&mut f.client_a);
    let _ = read_all_capture_available(&mut f.client_b);

    // Re-grabbing the keyboard synchronously puts its unified state in
    // FrozenNoEvent, matching the pointer sequence's explicit re-grab.
    let mut grab_keyboard = Vec::with_capacity(12);
    grab_keyboard.extend_from_slice(&W.to_le_bytes());
    grab_keyboard.extend_from_slice(&0u32.to_le_bytes()); // CurrentTime
    grab_keyboard.extend_from_slice(&[1, 0, 0, 0]); // async pointer, sync keyboard
    dispatch(&mut f, CLIENT_A, 31, 0, &grab_keyboard); // GrabKeyboard
    assert!(f.state.active_keyboard_grab.is_some_and(|grab| {
        grab.owner == ClientId(CLIENT_A)
            && matches!(grab.source, ActiveKeyboardGrabSource::Explicit)
    }));
    assert_eq!(
        f.state.xi1_frozen[&XI_MASTER_KEYBOARD].state,
        Xi1SyncState::FrozenNoEvent
    );
    assert!(f.state.xi1_frozen[&XI_MASTER_KEYBOARD].stored.is_some());
    let _ = read_all_capture_available(&mut f.client_a);
    let _ = read_all_capture_available(&mut f.client_b);

    // B's GrabPointer syncs only the paired keyboard, so it is the
    // synced-only requester for ReplayKeyboard.
    let mut grab_pointer = Vec::with_capacity(20);
    grab_pointer.extend_from_slice(&W.to_le_bytes());
    grab_pointer.extend_from_slice(&0u16.to_le_bytes()); // no pointer events
    grab_pointer.extend_from_slice(&[1, 0]); // async pointer, sync keyboard
    grab_pointer.extend_from_slice(&0u32.to_le_bytes()); // no confinement
    grab_pointer.extend_from_slice(&0u32.to_le_bytes()); // no cursor
    grab_pointer.extend_from_slice(&0u32.to_le_bytes()); // CurrentTime
    dispatch(&mut f, CLIENT_B, 26, 0, &grab_pointer); // GrabPointer
    assert_eq!(
        f.state.xi1_frozen[&XI_MASTER_KEYBOARD].other,
        Some(ClientId(CLIENT_B))
    );
    let _ = read_all_capture_available(&mut f.client_a);
    let _ = read_all_capture_available(&mut f.client_b);
    allow_events_core(&mut f, CLIENT_B, 5); // ReplayKeyboard
    assert!(
        read_all_capture_available(&mut f.client_a).is_empty(),
        "B's rejected replay produces no packet for A"
    );
    assert!(
        read_all_capture_available(&mut f.client_b).is_empty(),
        "B's rejected replay produces no packet for B"
    );
    assert!(f.state.active_keyboard_grab.is_some_and(|grab| {
        grab.owner == ClientId(CLIENT_A)
            && matches!(grab.source, ActiveKeyboardGrabSource::Explicit)
    }));
    assert_eq!(
        f.state.xi1_frozen[&XI_MASTER_KEYBOARD].state,
        Xi1SyncState::FrozenNoEvent
    );
    assert_eq!(
        f.state.xi1_frozen[&XI_MASTER_KEYBOARD].other,
        Some(ClientId(CLIENT_B))
    );
    assert!(f.state.xi1_frozen[&XI_MASTER_KEYBOARD].stored.is_some());

    let _ = read_all_capture_available(&mut f.client_a);
    let _ = read_all_capture_available(&mut f.client_b);
    allow_events_core(&mut f, CLIENT_A, 5); // ReplayKeyboard
    assert!(f.state.active_keyboard_grab.is_some_and(|grab| {
        grab.owner == ClientId(CLIENT_A)
            && matches!(grab.source, ActiveKeyboardGrabSource::Explicit)
    }));
    assert_eq!(
        f.state.xi1_frozen[&XI_MASTER_KEYBOARD].state,
        Xi1SyncState::FrozenNoEvent
    );
    assert_eq!(
        f.state.xi1_frozen[&XI_MASTER_KEYBOARD].other,
        Some(ClientId(CLIENT_B))
    );
    assert!(f.state.xi1_frozen[&XI_MASTER_KEYBOARD].stored.is_some());
    assert!(
        read_all_capture_available(&mut f.client_a).is_empty(),
        "A's rejected replay produces no packet for A"
    );
    assert!(
        read_all_capture_available(&mut f.client_b).is_empty(),
        "A's rejected replay produces no packet for B"
    );

    ungrab_keyboard(&mut f, CLIENT_A);
    dispatch(&mut f, CLIENT_B, 27, 0, &0u32.to_le_bytes()); // UngrabPointer
    crate::core_loop::key_fanout::key_event_fanout_to_state(
        &mut f.state,
        &mut f.backend,
        HostKeyEvent {
            origin: InputOrigin::NestedHost,
            pressed: false,
            keycode: KEYCODE,
            time: f.time + 1,
            root_x: 145,
            root_y: 145,
            event_x: 145,
            event_y: 145,
            state: 0,
        },
    );
    let release = read_all_capture_available(&mut f.client_b);
    assert_eq!(release.len(), 32);
    assert_eq!(release[0] & 0x7f, 3, "the terminating packet is KeyRelease");
    assert!(f.state.active_keyboard_grab.is_none());
    assert_task1_end_state(&f, &[XI_MASTER, XI_MASTER_KEYBOARD]);
}

/// Exercises a request-created synchronous passive key grab through owner
/// replay, using the existing fixture and production key fanout entry.
#[test]
fn owner_replay_keyboard_after_passive_key_grab_replays() {
    use crate::{
        host_x11::HostKeyEvent,
        server::{ActiveKeyboardGrabSource, Xi1SyncState},
    };

    const KEYCODE: u8 = 38;
    const KEY_PRESS_RELEASE_MASK: u32 = 0x0000_0003;
    const ROOT: u32 = crate::resources::ROOT_WINDOW.0;

    let mut f = p_fixture();
    select_core(&mut f, CLIENT_B, D, KEY_PRESS_RELEASE_MASK);
    let mut set_focus = Vec::with_capacity(8);
    set_focus.extend_from_slice(&D.to_le_bytes());
    set_focus.extend_from_slice(&0u32.to_le_bytes()); // CurrentTime
    dispatch(&mut f, CLIENT_B, 42, 0, &set_focus); // SetInputFocus

    let mut grab_key = Vec::with_capacity(12);
    grab_key.extend_from_slice(&ROOT.to_le_bytes());
    grab_key.extend_from_slice(&0u16.to_le_bytes()); // modifiers
    grab_key.extend_from_slice(&[KEYCODE, 1, 0, 0, 0, 0]); // async pointer, sync keyboard
    dispatch(&mut f, CLIENT_A, 33, 0, &grab_key); // GrabKey
    assert!(f.state.key_grabs.iter().any(|grab| {
        grab.owner == ClientId(CLIENT_A)
            && grab.grab_window.0 == ROOT
            && grab.keycode == KEYCODE
            && grab.keyboard_mode == 0
    }));

    let _ = read_all_capture_available(&mut f.client_a);
    let _ = read_all_capture_available(&mut f.client_b);
    crate::core_loop::key_fanout::key_event_fanout_to_state(
        &mut f.state,
        &mut f.backend,
        HostKeyEvent {
            origin: InputOrigin::NestedHost,
            pressed: true,
            keycode: KEYCODE,
            time: f.time,
            root_x: 145,
            root_y: 145,
            event_x: 145,
            event_y: 145,
            state: 0,
        },
    );

    let grab = f
        .state
        .active_keyboard_grab
        .expect("the request-created passive key grab activates");
    assert_eq!(grab.owner, ClientId(CLIENT_A));
    assert_eq!(grab.grab_window, ResourceId(ROOT));
    assert!(matches!(
        grab.source,
        ActiveKeyboardGrabSource::PassiveKey { keycode: KEYCODE }
    ));
    let freeze = &f.state.xi1_frozen[&XI_MASTER_KEYBOARD];
    assert_eq!(freeze.state, Xi1SyncState::FrozenWithEvent);
    assert!(matches!(
        freeze.stored.as_ref(),
        Some(crate::server::QueuedInputEvent::HostKeyTransition(event, _, _))
            if event.pressed && event.keycode == KEYCODE
    ));
    let activating = read_all_capture_available(&mut f.client_a);
    assert_eq!(activating.len(), 32);
    assert_eq!(activating[0] & 0x7f, 2, "activation sends KeyPress to A");
    assert_eq!(activating[1], KEYCODE);
    assert_eq!(
        u32::from_le_bytes(activating[12..16].try_into().unwrap()),
        ROOT
    );
    assert!(read_all_capture_available(&mut f.client_b).is_empty());

    allow_events_core(&mut f, CLIENT_A, 5); // ReplayKeyboard
    assert!(f.state.active_keyboard_grab.is_none());
    assert_eq!(
        f.state.xi1_frozen[&XI_MASTER_KEYBOARD].state,
        Xi1SyncState::Thawed
    );
    let replayed = read_all_capture_available(&mut f.client_b);
    assert_eq!(replayed.len(), 32);
    assert_eq!(replayed[0] & 0x7f, 2, "ReplayKeyboard delivers KeyPress");
    assert_eq!(replayed[1], KEYCODE);
    assert_eq!(
        u32::from_le_bytes(replayed[12..16].try_into().unwrap()),
        D,
        "the replay reaches B's natural focus window"
    );
    assert!(read_all_capture_available(&mut f.client_a).is_empty());

    crate::core_loop::key_fanout::key_event_fanout_to_state(
        &mut f.state,
        &mut f.backend,
        HostKeyEvent {
            origin: InputOrigin::NestedHost,
            pressed: false,
            keycode: KEYCODE,
            time: f.time + 1,
            root_x: 145,
            root_y: 145,
            event_x: 145,
            event_y: 145,
            state: 0,
        },
    );
    let released = read_all_capture_available(&mut f.client_b);
    assert_eq!(released.len(), 32);
    assert_eq!(
        released[0] & 0x7f,
        3,
        "KeyRelease follows the replayed press"
    );
    assert_eq!(released[1], KEYCODE);
    assert_eq!(u32::from_le_bytes(released[12..16].try_into().unwrap()), D);
    assert!(read_all_capture_available(&mut f.client_a).is_empty());
    assert_task1_end_state(&f, &[XI_MASTER, XI_MASTER_KEYBOARD]);
}

/// GH #277 (probe V9, measured on Xorg in
/// `tools/vng-scenarios/key-grab-activation`): Xorg `ActivatePassiveGrab`
/// (dix/events.c) delivers the activating press with
/// `FixUpEventFromWindow(grab->window)` + `TryClientEvents` — no
/// `owner_events` walk. The grab client selects ButtonPress on the root
/// and on `bar` but not on the grab window `top`; the press is still
/// reported on `top`, child `bar`. The release, under the active grab,
/// takes the owner_events walk to `bar`.
#[test]
fn passive_grab_activating_press_skips_owner_events_walk() {
    use yserver_protocol::x11::ResourceId;

    let mut state = ServerState::new();
    let top = ResourceId(0x0020_0001);
    let bar = ResourceId(0x0020_0002);
    let mut peer = install_client(&mut state, 1);
    for (win, parent, x, y, w, h) in [
        (top, crate::resources::ROOT_WINDOW, 100, 100, 400, 300),
        (bar, top, 0, 250, 400, 50),
    ] {
        state.resources.create_window(
            ClientId(1),
            yserver_protocol::x11::CreateWindowRequest {
                depth: 24,
                window: win,
                parent,
                x,
                y,
                width: w,
                height: h,
                class: 1,
                visual: crate::resources::ROOT_VISUAL,
                ..Default::default()
            },
        );
        let _ = state.resources.map_window(win);
    }
    let masks = &mut state.clients.get_mut(&1).unwrap().event_masks;
    masks.insert(crate::resources::ROOT_WINDOW, 0x0000_000C);
    masks.insert(bar, 0x0000_000C);
    state.button_grabs.push(crate::server::PassiveButtonGrab {
        device_id: 0,
        owner: ClientId(1),
        grab_window: top,
        button: 1,
        modifiers: 0x8000,
        owner_events: true,
        event_mask: 0x0000_000C,
        pointer_mode: 1,
        keyboard_mode: 1,
        confine_to: ResourceId(0),
        via_xi2: false,
    });
    let mut xid_map = HostXidMap::new();
    xid_map.insert(0xCAFE_u32, crate::resources::ROOT_WINDOW);
    let mut backend = crate::backend::recording::RecordingBackend::default();
    let mut press = HostPointerEvent {
        origin: crate::core_loop::message::InputOrigin::XTest(4),
        kind: PointerEventKind::ButtonPress,
        host_xid: 0xCAFE,
        detail: 1,
        time: 0,
        root_x: 300,
        root_y: 375,
        event_x: 300,
        event_y: 375,
        state: 0,
        crossing_mode: 0,
        child: 0,
        raw_dx: 0,
        raw_dy: 0,
        tree_change: false,
        device_axes: None,
    };
    let fields = |bytes: &[u8], code: u8| -> Vec<(u32, u32, i16, i16)> {
        bytes
            .chunks(32)
            .filter(|e| e[0] & 0x7f == code)
            .map(|e| {
                let u32_at = |at: usize| u32::from_le_bytes(e[at..at + 4].try_into().unwrap());
                let i16_at = |at: usize| i16::from_le_bytes([e[at], e[at + 1]]);
                (u32_at(12), u32_at(16), i16_at(24), i16_at(26))
            })
            .collect()
    };
    let _ = pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, press, true, false);
    assert_eq!(
        fields(&read_all_available(&mut peer), 4),
        [(top.0, bar.0, 200, 275)],
        "activating press: event=top child=bar ev=200,275 (Xorg probe V9)",
    );
    press.kind = PointerEventKind::ButtonRelease;
    press.state = 0x100;
    let _ = pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, press, true, false);
    assert_eq!(
        fields(&read_all_available(&mut peer), 5),
        [(bar.0, 0, 200, 25)],
        "release under the grab walks to bar (Xorg probe V9)",
    );
}

#[test]
fn p1_async_slave_grab_owner_events_no_master_press() {
    let mut f = p_fixture();
    assert_eq!(f.state.buttons_down, 0);
    select_xi2(&mut f, CLIENT_B, D, XI_XTEST, XI_GRAB_MASK);
    select_xi2(&mut f, CLIENT_B, D, XI_MASTER, XI_GRAB_MASK);
    xi_grab_button(&mut f, CLIENT_A, XI_XTEST, W, 1, 1, true, XI_GRAB_MASK);

    press_over_d(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    let grab = f.state.xi2_pointer_grabs.get(&XI_XTEST).copied();
    assert!(grab.is_some_and(|grab| {
        grab.owner == ClientId(CLIENT_A)
            && grab.grab_window == ResourceId(W)
            && grab.via_xi2
            && grab.passive
            && !grab.implicit
    }));
    assert_eq!(
        f.state.xi2_detached_masters.get(&XI_XTEST),
        Some(&XI_MASTER)
    );
    let press_a = read_packets(&mut f.client_a);
    let press_b = read_packets(&mut f.client_b);
    assert_eq!(press_a, vec![xi(4, XI_XTEST, XI_XTEST, W)]);
    assert!(
        press_b.is_empty() && f.state.buttons_down == 0,
        "slave activation has no master press or transition: packets={press_b:?}, buttons_down=0x{:x}",
        f.state.buttons_down
    );

    send(
        &mut f,
        InputOrigin::XTest(XI_XTEST),
        PointerEventKind::MotionNotify,
        0,
        155,
        155,
    );
    assert_eq!(
        read_packets(&mut f.client_a),
        vec![xi(6, XI_XTEST, XI_XTEST, W)]
    );
    assert!(read_packets(&mut f.client_b).is_empty());
    release(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    assert_eq!(
        read_packets(&mut f.client_a),
        vec![xi(5, XI_XTEST, XI_XTEST, W)]
    );
    assert!(read_packets(&mut f.client_b).is_empty());
    assert_task1_end_state(&f, &[XI_XTEST]);
}

#[test]
fn p1_slave_activation_records_no_master_press() {
    let mut f = p_fixture();
    assert_eq!(f.state.buttons_down, 0);
    let mut record_peer = super::install_capture_client(&mut f.state, RECORD_CLIENT);
    {
        let client = f.state.clients.get_mut(&RECORD_CLIENT).unwrap();
        client.resource_id_base = 0x0300_0000;
        client.resource_id_mask = 0x00ff_ffff;
    }
    enable_record_button_events(&mut f, &mut record_peer);

    select_xi2(&mut f, CLIENT_B, D, XI_XTEST, XI_BUTTONS);
    select_xi2(&mut f, CLIENT_B, D, XI_MASTER, XI_BUTTONS);
    xi_grab_button(&mut f, CLIENT_A, XI_XTEST, W, 1, 1, true, XI_GRAB_MASK);

    press_over_d(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    assert!(
        f.state
            .xi2_pointer_grabs
            .get(&XI_XTEST)
            .is_some_and(|grab| {
                grab.owner == ClientId(CLIENT_A)
                    && grab.grab_window == ResourceId(W)
                    && grab.via_xi2
                    && grab.passive
                    && !grab.implicit
            })
    );
    assert_eq!(
        f.state.xi2_detached_masters.get(&XI_XTEST),
        Some(&XI_MASTER)
    );
    assert_eq!(f.state.buttons_down, 0);
    release(&mut f, InputOrigin::XTest(XI_XTEST), 1);

    let recorded = read_all_capture_available(&mut record_peer);
    assert!(
        recorded.is_empty(),
        "RECORD must receive no core ButtonPress or ButtonRelease for a slave activation; got {recorded:02x?}"
    );
    assert_task1_end_state(&f, &[XI_XTEST]);
}

#[test]
fn p1m_master_xi2_grab_delivers_only_xi2() {
    let mut f = p_fixture();
    assert_eq!(f.state.buttons_down, 0);
    select_xi2(&mut f, CLIENT_B, D, XI_XTEST, XI_GRAB_MASK);
    select_xi2(&mut f, CLIENT_B, D, XI_MASTER, XI_GRAB_MASK);
    xi_grab_button(&mut f, CLIENT_A, XI_MASTER, W, 1, 1, true, XI_GRAB_MASK);

    press_over_d(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    let grab = f.state.active_pointer_grab;
    assert!(grab.is_some_and(|grab| {
        grab.owner == ClientId(CLIENT_A)
            && grab.grab_window == ResourceId(W)
            && grab.via_xi2
            && grab.passive
            && !grab.implicit
    }));
    let slave_grab = f.state.xi2_pointer_grabs.get(&XI_XTEST);
    assert!(slave_grab.is_some_and(|grab| {
        grab.owner == ClientId(CLIENT_B)
            && grab.grab_window == ResourceId(D)
            && grab.via_xi2
            && grab.implicit
            && !grab.passive
    }));
    assert_eq!(f.state.buttons_down, BUTTON_1);
    let press_a = read_packets(&mut f.client_a);
    assert_eq!(press_a, vec![xi(4, XI_MASTER, XI_XTEST, W)]);
    assert!(press_a[0].xi2, "the activation has no core ButtonPress");
    assert_eq!(press_a[0].child, D);
    assert_eq!((press_a[0].event_x, press_a[0].event_y), (50, 50));
    assert_eq!(
        read_packets(&mut f.client_b),
        vec![xi(4, XI_XTEST, XI_XTEST, D)]
    );

    send(
        &mut f,
        InputOrigin::XTest(XI_XTEST),
        PointerEventKind::MotionNotify,
        0,
        155,
        155,
    );
    assert_eq!(
        read_packets(&mut f.client_a),
        vec![xi(6, XI_MASTER, XI_XTEST, W)]
    );
    assert_eq!(
        read_packets(&mut f.client_b),
        vec![xi(6, XI_XTEST, XI_XTEST, D)]
    );
    release(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    assert_eq!(
        read_packets(&mut f.client_a),
        vec![xi(5, XI_MASTER, XI_XTEST, W)]
    );
    assert_eq!(
        read_packets(&mut f.client_b),
        vec![xi(5, XI_XTEST, XI_XTEST, D)]
    );
    assert_task1_end_state(&f, &[XI_MASTER, XI_MASTER_KEYBOARD]);
}

#[test]
fn p2_sync_slave_grab_release_only_mask_gets_press() {
    let mut f = p_fixture();
    assert_eq!(f.state.buttons_down, 0);
    select_xi2(&mut f, CLIENT_B, D, XI_XTEST, XI_BUTTONS);
    select_xi2(&mut f, CLIENT_B, D, XI_MASTER, XI_BUTTONS);
    xi_grab_button(
        &mut f,
        CLIENT_A,
        XI_XTEST,
        W,
        1,
        0,
        false,
        XI_BUTTON_RELEASE,
    );

    press_over_d(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    let grab = f.state.xi2_pointer_grabs.get(&XI_XTEST).copied();
    assert!(grab.is_some_and(|grab| {
        grab.owner == ClientId(CLIENT_A)
            && grab.grab_window == ResourceId(W)
            && grab.via_xi2
            && grab.passive
            && !grab.implicit
    }));
    assert_eq!(
        f.state.xi2_detached_masters.get(&XI_XTEST),
        Some(&XI_MASTER)
    );
    assert_eq!(f.state.buttons_down, 0);
    assert_eq!(
        read_packets(&mut f.client_a),
        vec![xi(4, XI_XTEST, XI_XTEST, W)]
    );
    assert!(read_packets(&mut f.client_b).is_empty());

    xi_allow_events(&mut f, CLIENT_A, XI_XTEST, 0); // AsyncDevice
    release(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    assert_eq!(
        read_packets(&mut f.client_a),
        vec![xi(5, XI_XTEST, XI_XTEST, W)]
    );
    assert!(read_packets(&mut f.client_b).is_empty());
    assert_task1_end_state(&f, &[XI_XTEST]);
}

#[test]
fn p2m_master_sync_release_only_mask_single_press() {
    let mut f = p_fixture();
    assert_eq!(f.state.buttons_down, 0);
    select_xi2(&mut f, CLIENT_B, D, XI_XTEST, XI_BUTTONS);
    select_xi2(&mut f, CLIENT_B, D, XI_MASTER, XI_BUTTONS);
    xi_grab_button(
        &mut f,
        CLIENT_A,
        XI_MASTER,
        W,
        1,
        0,
        false,
        XI_BUTTON_RELEASE,
    );

    press_over_d(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    let grab = f.state.active_pointer_grab;
    assert!(grab.is_some_and(|grab| {
        grab.owner == ClientId(CLIENT_A)
            && grab.grab_window == ResourceId(W)
            && grab.via_xi2
            && grab.passive
            && !grab.implicit
    }));
    let slave_grab = f.state.xi2_pointer_grabs.get(&XI_XTEST);
    assert!(slave_grab.is_some_and(|grab| {
        grab.owner == ClientId(CLIENT_B)
            && grab.grab_window == ResourceId(D)
            && grab.via_xi2
            && grab.implicit
            && !grab.passive
    }));
    assert_eq!(f.state.buttons_down, BUTTON_1);
    assert_eq!(
        read_packets(&mut f.client_a),
        vec![xi(4, XI_MASTER, XI_XTEST, W)]
    );
    assert_eq!(
        read_packets(&mut f.client_b),
        vec![xi(4, XI_XTEST, XI_XTEST, D)]
    );

    xi_allow_events(&mut f, CLIENT_A, XI_MASTER, 0); // AsyncDevice
    release(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    assert_eq!(
        read_packets(&mut f.client_a),
        vec![xi(5, XI_MASTER, XI_XTEST, W)]
    );
    assert_eq!(
        read_packets(&mut f.client_b),
        vec![xi(5, XI_XTEST, XI_XTEST, D)]
    );
    assert_task1_end_state(&f, &[XI_MASTER, XI_MASTER_KEYBOARD]);
}

#[test]
fn p4_xfwm4_core_then_xi2_grab_single_xi2_press() {
    let mut f = p_fixture();
    assert_eq!(f.state.buttons_down, 0);
    select_xi2(&mut f, CLIENT_B, D, XI_MASTER, XI_BUTTONS);
    grab_button_core(&mut f, CLIENT_A, D, ANY_BUTTON, ANY_MODIFIER, 0, false);
    xi_grab_button(
        &mut f, CLIENT_A, XI_MASTER, D, ANY_BUTTON, 0, false, XI_BUTTONS,
    );

    press_over_d(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    let grab = f.state.active_pointer_grab;
    assert!(grab.is_some_and(|grab| {
        grab.owner == ClientId(CLIENT_A)
            && grab.grab_window == ResourceId(D)
            && grab.via_xi2
            && grab.passive
            && !grab.implicit
    }));
    assert!(f.state.xi2_pointer_grabs.is_empty());
    assert_eq!(f.state.buttons_down, BUTTON_1);
    assert_eq!(
        read_packets(&mut f.client_a),
        vec![xi(4, XI_MASTER, XI_XTEST, D)]
    );
    assert!(read_packets(&mut f.client_b).is_empty());

    allow_events_core(&mut f, CLIENT_A, 2); // ReplayPointer
    assert_eq!(
        read_packets(&mut f.client_b),
        vec![xi(4, XI_MASTER, XI_XTEST, D)]
    );
    assert!(read_packets(&mut f.client_a).is_empty());
    release(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    assert_eq!(
        read_packets(&mut f.client_b),
        vec![xi(5, XI_MASTER, XI_XTEST, D)]
    );
    assert!(read_packets(&mut f.client_a).is_empty());
    assert_task1_end_state(&f, &[XI_MASTER, XI_MASTER_KEYBOARD]);
}

#[test]
fn held_button_blocks_core_grab_on_master_pass() {
    let mut f = p_fixture();
    assert_eq!(f.state.buttons_down, 0);
    grab_button_core(&mut f, CLIENT_A, W, 1, ANY_MODIFIER, 1, false);
    let source = crate::xinput::InputSourceId(0xABCD);
    let _physical_device = super::pointer_source(&mut f.state, source.0, true);

    send(
        &mut f,
        InputOrigin::Physical(source),
        PointerEventKind::ButtonPress,
        3,
        150,
        150,
    );
    assert_eq!(f.state.buttons_down, BUTTON_3);
    press_over_d(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    assert!(f.state.active_pointer_grab.is_none());
    assert_eq!(f.state.buttons_down, BUTTON_1 | BUTTON_3);
    assert!(read_packets(&mut f.client_a).is_empty());
    assert!(read_packets(&mut f.client_b).is_empty());

    release(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    assert_eq!(f.state.buttons_down, BUTTON_3);
    release(&mut f, InputOrigin::Physical(source), 3);
    assert_task1_end_state(&f, &[]);
}

#[test]
fn held_master_button_survives_slave_grab() {
    let mut f = p_fixture();
    assert_eq!(f.state.buttons_down, 0);
    let source = crate::xinput::InputSourceId(0xABCD);
    let physical_device = super::pointer_source(&mut f.state, source.0, true);
    assert_ne!(physical_device, XI_XTEST);
    xi_grab_button(&mut f, CLIENT_A, XI_XTEST, W, 1, 1, false, XI_BUTTONS);

    send(
        &mut f,
        InputOrigin::Physical(source),
        PointerEventKind::ButtonPress,
        3,
        150,
        150,
    );
    assert_eq!(f.state.buttons_down, BUTTON_3);
    assert!(read_packets(&mut f.client_b).is_empty());

    press_over_d(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    let grab = f.state.xi2_pointer_grabs.get(&XI_XTEST).copied();
    assert!(
        grab.is_some_and(|grab| {
            grab.owner == ClientId(CLIENT_A)
                && grab.grab_window == ResourceId(W)
                && grab.via_xi2
                && grab.passive
                && !grab.implicit
        }),
        "expected slave passive activation, got grab={grab:?}, buttons_down=0x{:x}",
        f.state.buttons_down
    );
    assert_eq!(
        f.state.xi2_detached_masters.get(&XI_XTEST),
        Some(&XI_MASTER)
    );
    assert_eq!(
        f.state.buttons_down, BUTTON_3,
        "slave activation preserves the other source's held master button"
    );
    assert!(read_packets(&mut f.client_b).is_empty());

    release(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    assert_eq!(f.state.buttons_down, BUTTON_3);
    assert!(read_packets(&mut f.client_b).is_empty());
    release(&mut f, InputOrigin::Physical(source), 3);
    assert!(read_packets(&mut f.client_b).is_empty());
    assert_task1_end_state(&f, &[XI_XTEST]);
}

#[test]
fn p4c_core_requests_keep_xi2_grab() {
    let mut f = p_fixture();
    select_xi2(&mut f, CLIENT_B, D, XI_MASTER, XI_BUTTONS);
    xi_grab_button(
        &mut f, CLIENT_A, XI_MASTER, D, ANY_BUTTON, 0, false, XI_BUTTONS,
    );
    grab_button_core(&mut f, CLIENT_A, D, ANY_BUTTON, ANY_MODIFIER, 0, false);

    let same_window_grabs: Vec<_> = f
        .state
        .button_grabs
        .iter()
        .filter(|grab| grab.owner == ClientId(CLIENT_A) && grab.grab_window == ResourceId(D))
        .collect();
    assert_eq!(same_window_grabs.len(), 2, "core and XI2 grabs coexist");

    ungrab_button_core(&mut f, CLIENT_A, D, ANY_BUTTON, ANY_MODIFIER);
    let remaining: Vec<_> = f
        .state
        .button_grabs
        .iter()
        .filter(|grab| grab.owner == ClientId(CLIENT_A) && grab.grab_window == ResourceId(D))
        .collect();
    assert_eq!(
        remaining.len(),
        1,
        "UngrabButton removes only the core grab"
    );
    assert!(remaining[0].via_xi2 && remaining[0].device_id == XI_MASTER);

    press_over_d(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    assert!(f.state.active_pointer_grab.is_some_and(|grab| {
        grab.owner == ClientId(CLIENT_A)
            && grab.grab_window == ResourceId(D)
            && grab.via_xi2
            && grab.passive
            && !grab.implicit
    }));
    assert!(f.state.xi2_pointer_grabs.is_empty());
    assert_eq!(f.state.buttons_down, BUTTON_1);
    assert_eq!(
        read_packets(&mut f.client_a),
        vec![xi(4, XI_MASTER, XI_XTEST, D)]
    );
    assert!(read_packets(&mut f.client_b).is_empty());

    xi_allow_events(&mut f, CLIENT_A, XI_MASTER, 2); // ReplayDevice
    assert_eq!(
        read_packets(&mut f.client_b),
        vec![xi(4, XI_MASTER, XI_XTEST, D)]
    );
    assert!(read_packets(&mut f.client_a).is_empty());
    release(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    assert_eq!(
        read_packets(&mut f.client_b),
        vec![xi(5, XI_MASTER, XI_XTEST, D)]
    );
    assert!(read_packets(&mut f.client_a).is_empty());
    assert_task1_end_state(&f, &[XI_MASTER, XI_MASTER_KEYBOARD]);
}

#[test]
fn p4b_older_xi2_newer_core_core_wins() {
    let mut f = p_fixture();
    select_xi2(&mut f, CLIENT_B, D, XI_MASTER, XI_BUTTONS);
    xi_grab_button(
        &mut f, CLIENT_A, XI_MASTER, D, ANY_BUTTON, 0, false, XI_BUTTONS,
    );
    grab_button_core(&mut f, CLIENT_A, D, ANY_BUTTON, ANY_MODIFIER, 0, false);
    assert_eq!(
        f.state
            .button_grabs
            .iter()
            .filter(|grab| grab.owner == ClientId(CLIENT_A) && grab.grab_window == ResourceId(D))
            .count(),
        2,
        "both registrations exist before the press"
    );

    press_over_d(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    assert!(f.state.active_pointer_grab.is_some_and(|grab| {
        grab.owner == ClientId(CLIENT_A)
            && grab.grab_window == ResourceId(D)
            && !grab.via_xi2
            && grab.passive
            && !grab.implicit
    }));
    assert!(f.state.xi2_pointer_grabs.is_empty());
    assert_eq!(f.state.buttons_down, BUTTON_1);
    assert_eq!(read_packets(&mut f.client_a), vec![core(4, D)]);
    assert!(read_packets(&mut f.client_b).is_empty());

    allow_events_core(&mut f, CLIENT_A, 2); // ReplayPointer
    assert_eq!(
        read_packets(&mut f.client_b),
        vec![xi(4, XI_MASTER, XI_XTEST, D)]
    );
    assert!(read_packets(&mut f.client_a).is_empty());
    release(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    assert_eq!(
        read_packets(&mut f.client_b),
        vec![xi(5, XI_MASTER, XI_XTEST, D)]
    );
    assert!(read_packets(&mut f.client_a).is_empty());
    assert_task1_end_state(&f, &[XI_MASTER, XI_MASTER_KEYBOARD]);
}

#[test]
fn p5_slave_grab_on_descendant_beats_core_grab_on_ancestor() {
    let mut f = p_fixture();
    select_xi2(&mut f, CLIENT_B, D, XI_MASTER, XI_BUTTONS);
    grab_button_core(&mut f, CLIENT_A, W, ANY_BUTTON, ANY_MODIFIER, 1, false);
    xi_grab_button(&mut f, CLIENT_B, XI_XTEST, D, 1, 1, false, XI_BUTTONS);

    press_over_d(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    assert!(
        f.state
            .xi2_pointer_grabs
            .get(&XI_XTEST)
            .is_some_and(|grab| {
                grab.owner == ClientId(CLIENT_B)
                    && grab.grab_window == ResourceId(D)
                    && grab.via_xi2
                    && grab.passive
                    && !grab.implicit
            })
    );
    assert!(f.state.active_pointer_grab.is_none());
    assert_eq!(f.state.buttons_down, 0);
    assert_eq!(
        read_packets(&mut f.client_b),
        vec![xi(4, XI_XTEST, XI_XTEST, D)]
    );
    assert!(read_packets(&mut f.client_a).is_empty());

    release(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    assert_eq!(
        read_packets(&mut f.client_b),
        vec![xi(5, XI_XTEST, XI_XTEST, D)]
    );
    assert!(read_packets(&mut f.client_a).is_empty());
    assert_task1_end_state(&f, &[XI_XTEST]);
}

#[test]
fn xiall_devices_grab_activates_on_slave_pass() {
    let mut f = p_fixture();
    xi_grab_button(&mut f, CLIENT_A, XI_ALL_DEVICES, W, 1, 1, false, XI_BUTTONS);

    press_over_d(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    assert!(
        f.state
            .xi2_pointer_grabs
            .get(&XI_XTEST)
            .is_some_and(|grab| {
                grab.owner == ClientId(CLIENT_A)
                    && grab.grab_window == ResourceId(W)
                    && grab.via_xi2
                    && grab.passive
                    && !grab.implicit
            })
    );
    assert_eq!(
        f.state.xi2_detached_masters.get(&XI_XTEST),
        Some(&XI_MASTER)
    );
    assert!(f.state.active_pointer_grab.is_none());
    assert_eq!(f.state.buttons_down, 0);
    assert_eq!(
        read_packets(&mut f.client_a),
        vec![xi(4, XI_XTEST, XI_XTEST, W)]
    );
    assert!(read_packets(&mut f.client_b).is_empty());

    release(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    assert_eq!(
        read_packets(&mut f.client_a),
        vec![xi(5, XI_XTEST, XI_XTEST, W)]
    );
    assert!(read_packets(&mut f.client_b).is_empty());
    assert_task1_end_state(&f, &[XI_XTEST]);
}

#[test]
fn each_ungrab_removes_only_its_protocol() {
    let mut f = p_fixture();
    grab_button_core(&mut f, CLIENT_A, W, 1, ANY_MODIFIER, 1, false);
    xi_grab_button(&mut f, CLIENT_A, XI_MASTER, W, 1, 1, false, XI_BUTTONS);
    assert_eq!(f.state.button_grabs.len(), 2);

    xi_ungrab_button(&mut f, CLIENT_A, XI_MASTER, W, 1);
    assert_eq!(f.state.button_grabs.len(), 1);
    assert!(!f.state.button_grabs[0].via_xi2);

    xi_grab_button(&mut f, CLIENT_A, XI_MASTER, W, 1, 1, false, XI_BUTTONS);
    assert_eq!(f.state.button_grabs.len(), 2);
    ungrab_button_core(&mut f, CLIENT_A, W, 1, ANY_MODIFIER);
    assert_eq!(
        f.state.button_grabs.len(),
        1,
        "ungrab state: {:?}",
        f.state.button_grabs
    );
    assert!(f.state.button_grabs[0].via_xi2);
    assert_eq!(f.state.button_grabs[0].device_id, XI_MASTER);
    assert_task1_end_state(&f, &[]);
}

#[test]
fn p2r_slave_replay_gives_only_slave_forms() {
    let mut f = p_fixture();
    select_xi2(&mut f, CLIENT_B, D, XI_XTEST, XI_BUTTONS);
    select_xi2(&mut f, CLIENT_B, D, XI_MASTER, XI_BUTTONS);
    xi_grab_button(
        &mut f,
        CLIENT_A,
        XI_XTEST,
        W,
        1,
        0,
        false,
        XI_BUTTON_RELEASE,
    );

    press_over_d(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    let grab = f
        .state
        .xi2_pointer_grabs
        .get(&XI_XTEST)
        .expect("the synchronous slave passive grab is active after press");
    assert_eq!(grab.owner, ClientId(CLIENT_A));
    assert_eq!(grab.grab_window, ResourceId(W));
    assert!(grab.via_xi2 && grab.passive && !grab.implicit);
    assert!(f.state.active_pointer_grab.is_none());
    assert_eq!(f.state.buttons_down, 0);
    assert_eq!(
        read_packets(&mut f.client_a),
        vec![xi(4, XI_XTEST, XI_XTEST, W)]
    );
    assert!(read_packets(&mut f.client_b).is_empty());

    xi_allow_events(&mut f, CLIENT_A, XI_XTEST, 2); // ReplayDevice
    let implicit = f
        .state
        .xi2_pointer_grabs
        .get(&XI_XTEST)
        .expect("ReplayDevice creates the implicit grab on the replayed slave");
    assert_eq!(implicit.owner, ClientId(CLIENT_B));
    assert_eq!(implicit.grab_window, ResourceId(D));
    assert!(implicit.via_xi2 && implicit.implicit && !implicit.passive);
    assert!(f.state.active_pointer_grab.is_none());
    assert_eq!(f.state.buttons_down, 0);
    assert!(!f.state.xi2_detached_masters.contains_key(&XI_XTEST));
    assert_eq!(
        read_packets(&mut f.client_b),
        vec![xi(4, XI_XTEST, XI_XTEST, D)],
        "P2r: replay delivers one slave press and no master-form press"
    );
    assert!(read_packets(&mut f.client_a).is_empty());

    release(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    assert_eq!(
        read_packets(&mut f.client_b),
        vec![xi(5, XI_XTEST, XI_XTEST, D)]
    );
    assert!(read_packets(&mut f.client_a).is_empty());
    assert_task1_end_state(&f, &[XI_XTEST]);
}

#[test]
fn p3_core_replay_does_not_repeat_slave_press() {
    let mut f = p_fixture();
    select_xi2(&mut f, CLIENT_B, D, XI_XTEST, XI_GRAB_MASK);
    grab_button_core(&mut f, CLIENT_A, W, ANY_BUTTON, ANY_MODIFIER, 0, false);

    press_over_d(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    let grab = f
        .state
        .active_pointer_grab
        .expect("the core synchronous passive grab is active after press");
    assert_eq!(grab.owner, ClientId(CLIENT_A));
    assert_eq!(grab.grab_window, ResourceId(W));
    assert!(!grab.via_xi2 && grab.passive && !grab.implicit);
    let slave = f
        .state
        .xi2_pointer_grabs
        .get(&XI_XTEST)
        .expect("the first slave-form press already created B's implicit grab");
    assert_eq!(slave.owner, ClientId(CLIENT_B));
    assert_eq!(slave.grab_window, ResourceId(D));
    assert!(slave.via_xi2 && slave.implicit && !slave.passive);
    assert_eq!(f.state.buttons_down, BUTTON_1);
    assert_eq!(read_packets(&mut f.client_a), vec![core(4, W)]);
    assert_eq!(
        read_packets(&mut f.client_b),
        vec![xi(4, XI_XTEST, XI_XTEST, D)]
    );

    allow_events_core(&mut f, CLIENT_A, 2); // ReplayPointer
    assert!(f.state.active_pointer_grab.is_none());
    assert!(
        f.state
            .xi2_pointer_grabs
            .get(&XI_XTEST)
            .is_some_and(|grab| {
                grab.owner == ClientId(CLIENT_B)
                    && grab.grab_window == ResourceId(D)
                    && grab.via_xi2
                    && grab.implicit
                    && !grab.passive
            }),
        "the previously delivered slave form keeps its implicit grab"
    );
    assert_eq!(f.state.buttons_down, BUTTON_1);
    assert!(
        read_packets(&mut f.client_b).is_empty(),
        "P3: ReplayPointer must not repeat B's already-delivered slave press"
    );
    assert!(read_packets(&mut f.client_a).is_empty());

    move_to_out(&mut f, InputOrigin::XTest(XI_XTEST));
    assert_eq!(
        read_packets(&mut f.client_b),
        vec![xi(6, XI_XTEST, XI_XTEST, D)]
    );
    release(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    assert_eq!(
        read_packets(&mut f.client_b),
        vec![xi(5, XI_XTEST, XI_XTEST, D)]
    );
    assert!(read_packets(&mut f.client_a).is_empty());
    assert_task1_end_state(&f, &[XI_MASTER, XI_MASTER_KEYBOARD]);
}

#[test]
fn core_replay_activates_grab_below_grab_window() {
    let mut f = p_fixture();
    grab_button_core(&mut f, CLIENT_A, W, 1, ANY_MODIFIER, 0, false);
    grab_button_core(&mut f, CLIENT_B, D, 1, ANY_MODIFIER, 1, false);

    press_over_d(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    let grab = f
        .state
        .active_pointer_grab
        .expect("the ancestor core synchronous passive grab wins initially");
    assert_eq!(grab.owner, ClientId(CLIENT_A));
    assert_eq!(grab.grab_window, ResourceId(W));
    assert!(!grab.via_xi2 && grab.passive && !grab.implicit);
    assert!(f.state.xi2_pointer_grabs.is_empty());
    assert_eq!(f.state.buttons_down, BUTTON_1);
    assert_eq!(read_packets(&mut f.client_a), vec![core(4, W)]);
    assert!(read_packets(&mut f.client_b).is_empty());

    allow_events_core(&mut f, CLIENT_A, 2); // ReplayPointer
    let replay_grab = f
        .state
        .active_pointer_grab
        .expect("ReplayPointer rechecks passive grabs below W (Xorg events.c:1331-1363)");
    assert_eq!(replay_grab.owner, ClientId(CLIENT_B));
    assert_eq!(replay_grab.grab_window, ResourceId(D));
    assert!(!replay_grab.via_xi2 && replay_grab.passive && !replay_grab.implicit);
    assert_eq!(f.state.buttons_down, BUTTON_1);
    assert_eq!(read_packets(&mut f.client_b), vec![core(4, D)]);
    assert!(read_packets(&mut f.client_a).is_empty());

    release(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    assert_eq!(read_packets(&mut f.client_b), vec![core(5, D)]);
    assert!(read_packets(&mut f.client_a).is_empty());
    assert_task1_end_state(&f, &[XI_MASTER, XI_MASTER_KEYBOARD]);
}

#[test]
fn replay_does_not_reactivate_grab_on_grab_window() {
    let mut f = p_fixture();
    select_xi2(&mut f, CLIENT_B, D, XI_MASTER, XI_BUTTONS);
    grab_button_core(&mut f, CLIENT_A, D, ANY_BUTTON, ANY_MODIFIER, 0, false);
    xi_grab_button(
        &mut f, CLIENT_A, XI_MASTER, D, ANY_BUTTON, 0, false, XI_BUTTONS,
    );

    press_over_d(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    let grab = f
        .state
        .active_pointer_grab
        .expect("P4's newer XI2 passive grab activates first");
    assert_eq!(grab.owner, ClientId(CLIENT_A));
    assert_eq!(grab.grab_window, ResourceId(D));
    assert!(grab.via_xi2 && grab.passive && !grab.implicit);
    assert!(f.state.xi2_pointer_grabs.is_empty());
    assert_eq!(f.state.buttons_down, BUTTON_1);
    assert_eq!(
        read_packets(&mut f.client_a),
        vec![xi(4, XI_MASTER, XI_XTEST, D)]
    );
    assert!(read_packets(&mut f.client_b).is_empty());

    allow_events_core(&mut f, CLIENT_A, 2); // ReplayPointer
    let replay_grab = f
        .state
        .active_pointer_grab
        .expect("natural replay delivery creates B's master implicit grab");
    assert_eq!(replay_grab.owner, ClientId(CLIENT_B));
    assert_eq!(replay_grab.grab_window, ResourceId(D));
    assert!(replay_grab.via_xi2 && replay_grab.implicit && !replay_grab.passive);
    assert_eq!(f.state.buttons_down, BUTTON_1);
    assert_eq!(
        read_packets(&mut f.client_b),
        vec![xi(4, XI_MASTER, XI_XTEST, D)]
    );
    assert!(read_packets(&mut f.client_a).is_empty());

    release(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    assert_eq!(
        read_packets(&mut f.client_b),
        vec![xi(5, XI_MASTER, XI_XTEST, D)]
    );
    assert!(read_packets(&mut f.client_a).is_empty());
    assert_task1_end_state(&f, &[XI_MASTER, XI_MASTER_KEYBOARD]);
}

#[test]
fn core_replay_activates_grab_below_despite_slave_implicit_grab() {
    let mut f = p_fixture();
    // P3's slave selection already gave B the device-4 press and implicit grab
    // before A's synchronous core grab replays on the master.
    select_xi2(&mut f, CLIENT_B, D, XI_XTEST, XI_GRAB_MASK);
    grab_button_core(&mut f, CLIENT_A, W, 1, ANY_MODIFIER, 0, false);
    grab_button_core(&mut f, CLIENT_B, D, 1, ANY_MODIFIER, 1, false);

    press_over_d(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    let grab = f
        .state
        .active_pointer_grab
        .expect("A's synchronous core passive grab is active after press");
    assert_eq!(grab.owner, ClientId(CLIENT_A));
    assert_eq!(grab.grab_window, ResourceId(W));
    assert!(!grab.via_xi2 && grab.passive && !grab.implicit);
    let slave = f
        .state
        .xi2_pointer_grabs
        .get(&XI_XTEST)
        .expect("P3's earlier slave form created B's implicit grab");
    assert_eq!(slave.owner, ClientId(CLIENT_B));
    assert_eq!(slave.grab_window, ResourceId(D));
    assert!(slave.via_xi2 && slave.implicit && !slave.passive);
    assert_eq!(f.state.buttons_down, BUTTON_1);
    assert_eq!(read_packets(&mut f.client_a), vec![core(4, W)]);
    assert_eq!(
        read_packets(&mut f.client_b),
        vec![xi(4, XI_XTEST, XI_XTEST, D)]
    );

    allow_events_core(&mut f, CLIENT_A, 2); // ReplayPointer
    let replay_grab = f
        .state
        .active_pointer_grab
        .expect("ReplayPointer checks B's core grab below W despite the slave grab");
    assert_eq!(replay_grab.owner, ClientId(CLIENT_B));
    assert_eq!(replay_grab.grab_window, ResourceId(D));
    assert!(!replay_grab.via_xi2 && replay_grab.passive && !replay_grab.implicit);
    assert_eq!(f.state.buttons_down, BUTTON_1);
    assert_eq!(read_packets(&mut f.client_b), vec![core(4, D)]);
    assert!(read_packets(&mut f.client_a).is_empty());

    release(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    let released = read_packets(&mut f.client_b);
    assert_eq!(released.len(), 2);
    assert!(released.contains(&core(5, D)));
    assert!(released.contains(&xi(5, XI_XTEST, XI_XTEST, D)));
    assert!(read_packets(&mut f.client_a).is_empty());
    assert_task1_end_state(&f, &[XI_MASTER, XI_MASTER_KEYBOARD]);
}

#[test]
fn master_replay_survives_source_slave_detach() {
    const XI_GRAB_DEVICE: u8 = 51;
    const XI_UNGRAB_DEVICE: u8 = 52;

    let mut f = p_fixture();
    let mut client_c = super::install_capture_client(&mut f.state, CLIENT_C);
    select_core(&mut f, CLIENT_B, D, 0x000cu32);
    select_xi2(&mut f, CLIENT_B, D, XI_MASTER, XI_BUTTONS);
    grab_button_core(&mut f, CLIENT_A, W, 1, ANY_MODIFIER, 0, false);

    press_over_d(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    let grab = f
        .state
        .active_pointer_grab
        .expect("A's synchronous core passive grab is active after press");
    assert_eq!(grab.owner, ClientId(CLIENT_A));
    assert_eq!(grab.grab_window, ResourceId(W));
    assert!(!grab.via_xi2 && grab.passive && !grab.implicit);
    assert_eq!(f.state.buttons_down, BUTTON_1);
    assert_eq!(read_packets(&mut f.client_a), vec![core(4, W)]);
    assert!(read_packets(&mut f.client_b).is_empty());

    let mut grab_body = Vec::with_capacity(20);
    grab_body.extend_from_slice(&crate::resources::ROOT_WINDOW.0.to_le_bytes());
    grab_body.extend_from_slice(&0u32.to_le_bytes()); // CurrentTime
    grab_body.extend_from_slice(&0u32.to_le_bytes()); // no cursor
    grab_body.extend_from_slice(&XI_XTEST.to_le_bytes());
    grab_body.extend_from_slice(&[1, 1, 0, 0]); // async pointer and paired device
    grab_body.extend_from_slice(&0u16.to_le_bytes()); // no event mask
    dispatch(&mut f, CLIENT_C, XI2_OPCODE, XI_GRAB_DEVICE, &grab_body);
    let grab_reply = read_all_capture_available(&mut client_c);
    assert_eq!(grab_reply.len(), 32, "XIGrabDevice returns its reply");
    assert_eq!(grab_reply[0], 1, "XIGrabDevice returns an X reply");
    assert_eq!(grab_reply[8], 0, "XIGrabDevice succeeds");
    let slave_grab = f
        .state
        .xi2_pointer_grabs
        .get(&XI_XTEST)
        .expect("C's explicit device grab occupies slave 4");
    assert_eq!(slave_grab.owner, ClientId(CLIENT_C));
    assert_eq!(slave_grab.grab_window, crate::resources::ROOT_WINDOW);
    assert!(slave_grab.via_xi2 && !slave_grab.passive && !slave_grab.implicit);
    assert_eq!(
        f.state.xi_devices.device(XI_XTEST).unwrap().attached_master,
        None,
        "XIGrabDevice detaches slave 4 while A's master grab remains active"
    );
    assert!(f.state.active_pointer_grab.is_some_and(|active| {
        active.owner == ClientId(CLIENT_A)
            && active.grab_window == ResourceId(W)
            && active.passive
            && !active.implicit
    }));
    assert_eq!(f.state.buttons_down, BUTTON_1);

    allow_events_core(&mut f, CLIENT_A, 2); // ReplayPointer
    let replayed = read_packets(&mut f.client_b);
    assert_eq!(replayed, vec![xi(4, XI_MASTER, XI_XTEST, D)]);
    // B selected both core and XI2 master events. Xorg's same-client
    // duplicate suppression leaves the master XI2 form in the packet stream.
    assert!(read_packets(&mut f.client_a).is_empty());
    let replay_grab = f
        .state
        .active_pointer_grab
        .expect("the replayed master press creates B's master implicit grab");
    assert_eq!(replay_grab.owner, ClientId(CLIENT_B));
    assert_eq!(replay_grab.grab_window, ResourceId(D));
    assert!(replay_grab.via_xi2 && replay_grab.implicit && !replay_grab.passive);
    assert_eq!(f.state.buttons_down, BUTTON_1);
    assert_eq!(
        f.state.xi_devices.device(XI_XTEST).unwrap().attached_master,
        None,
        "replay retains the captured master route while slave 4 remains detached"
    );

    let mut ungrab_body = Vec::with_capacity(8);
    ungrab_body.extend_from_slice(&0u32.to_le_bytes()); // CurrentTime
    ungrab_body.extend_from_slice(&XI_XTEST.to_le_bytes());
    ungrab_body.extend_from_slice(&0u16.to_le_bytes()); // padding
    dispatch(&mut f, CLIENT_C, XI2_OPCODE, XI_UNGRAB_DEVICE, &ungrab_body);
    assert!(!f.state.xi2_pointer_grabs.contains_key(&XI_XTEST));
    assert_eq!(
        f.state.xi_devices.device(XI_XTEST).unwrap().attached_master,
        Some(XI_MASTER),
        "XIUngrabDevice reattaches slave 4 before release"
    );

    move_to_out(&mut f, InputOrigin::XTest(XI_XTEST));
    release(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    assert_eq!(
        read_packets(&mut f.client_b),
        vec![xi(5, XI_MASTER, XI_XTEST, D)]
    );
    assert!(read_packets(&mut f.client_a).is_empty());
    assert!(read_all_capture_available(&mut client_c).is_empty());
    assert_task1_end_state(&f, &[XI_MASTER, XI_MASTER_KEYBOARD, XI_XTEST]);
}

#[test]
fn master_xi2_passive_grab_activates_on_replay_after_detach() {
    const XI_GRAB_DEVICE: u8 = 51;
    const XI_UNGRAB_DEVICE: u8 = 52;

    let mut f = p_fixture();
    let mut client_c = super::install_capture_client(&mut f.state, CLIENT_C);
    // No XISelectEvents selection: activation must route through B's grab.
    xi_grab_button(&mut f, CLIENT_B, XI_MASTER, D, 1, 1, false, XI_BUTTON_PRESS);
    grab_button_core(&mut f, CLIENT_A, W, 1, ANY_MODIFIER, 0, false);

    press_over_d(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    let grab = f
        .state
        .active_pointer_grab
        .expect("A's synchronous core passive grab is active after press");
    assert_eq!(grab.owner, ClientId(CLIENT_A));
    assert_eq!(grab.grab_window, ResourceId(W));
    assert!(!grab.via_xi2 && grab.passive && !grab.implicit);
    assert_eq!(f.state.buttons_down, BUTTON_1);
    assert_eq!(read_packets(&mut f.client_a), vec![core(4, W)]);
    assert!(read_packets(&mut f.client_b).is_empty());

    let mut grab_body = Vec::with_capacity(20);
    grab_body.extend_from_slice(&crate::resources::ROOT_WINDOW.0.to_le_bytes());
    grab_body.extend_from_slice(&0u32.to_le_bytes()); // CurrentTime
    grab_body.extend_from_slice(&0u32.to_le_bytes()); // no cursor
    grab_body.extend_from_slice(&XI_XTEST.to_le_bytes());
    grab_body.extend_from_slice(&[1, 1, 0, 0]); // async pointer and paired device
    grab_body.extend_from_slice(&0u16.to_le_bytes()); // no event mask
    dispatch(&mut f, CLIENT_C, XI2_OPCODE, XI_GRAB_DEVICE, &grab_body);
    let grab_reply = read_all_capture_available(&mut client_c);
    assert_eq!(grab_reply.len(), 32, "XIGrabDevice returns its reply");
    assert_eq!(grab_reply[0], 1, "XIGrabDevice returns an X reply");
    assert_eq!(grab_reply[8], 0, "XIGrabDevice succeeds");
    assert!(
        f.state
            .xi2_pointer_grabs
            .get(&XI_XTEST)
            .is_some_and(|grab| {
                grab.owner == ClientId(CLIENT_C)
                    && grab.grab_window == crate::resources::ROOT_WINDOW
                    && grab.via_xi2
                    && !grab.passive
                    && !grab.implicit
            })
    );
    assert_eq!(
        f.state.xi_devices.device(XI_XTEST).unwrap().attached_master,
        None,
        "XIGrabDevice detaches slave 4 while A's master grab remains active"
    );
    assert_eq!(f.state.buttons_down, BUTTON_1);

    allow_events_core(&mut f, CLIENT_A, 2); // ReplayPointer
    let replay_grab = f
        .state
        .active_pointer_grab
        .expect("replay activates B's master XI2 passive grab");
    assert_eq!(replay_grab.owner, ClientId(CLIENT_B));
    assert_eq!(replay_grab.grab_window, ResourceId(D));
    assert!(replay_grab.via_xi2 && replay_grab.passive && !replay_grab.implicit);
    assert_eq!(f.state.buttons_down, BUTTON_1);
    assert_eq!(
        read_packets(&mut f.client_b),
        vec![xi(4, XI_MASTER, XI_XTEST, D)],
        "the master passive grab receives exactly one activating press"
    );
    assert!(read_packets(&mut f.client_a).is_empty());
    assert_eq!(
        f.state.xi_devices.device(XI_XTEST).unwrap().attached_master,
        None,
        "replay uses the captured master while the source slave stays detached"
    );

    let mut ungrab_body = Vec::with_capacity(8);
    ungrab_body.extend_from_slice(&0u32.to_le_bytes()); // CurrentTime
    ungrab_body.extend_from_slice(&XI_XTEST.to_le_bytes());
    ungrab_body.extend_from_slice(&0u16.to_le_bytes()); // padding
    dispatch(&mut f, CLIENT_C, XI2_OPCODE, XI_UNGRAB_DEVICE, &ungrab_body);
    assert!(!f.state.xi2_pointer_grabs.contains_key(&XI_XTEST));
    assert_eq!(
        f.state.xi_devices.device(XI_XTEST).unwrap().attached_master,
        Some(XI_MASTER),
        "XIUngrabDevice reattaches slave 4 before release"
    );
    release(&mut f, InputOrigin::XTest(XI_XTEST), 1);
    assert_eq!(
        read_packets(&mut f.client_b),
        vec![xi(5, XI_MASTER, XI_XTEST, D)],
        "the active master grab routes the release to the same window"
    );
    assert!(read_packets(&mut f.client_a).is_empty());
    assert!(read_all_capture_available(&mut client_c).is_empty());
    assert_task1_end_state(&f, &[XI_MASTER, XI_MASTER_KEYBOARD, XI_XTEST]);
}

/// Core GrabPointer through `process_request`, synchronous pointer mode,
/// asynchronous keyboard mode, core PointerMotion in the grab mask.
fn grab_core_pointer_sync(f: &mut Fixture, client: u32, window: u32) {
    let mut body = Vec::with_capacity(20);
    body.extend_from_slice(&window.to_le_bytes());
    body.extend_from_slice(&(1u16 << 6).to_le_bytes()); // PointerMotion
    body.extend_from_slice(&[0, 1]); // sync pointer, async keyboard
    body.extend_from_slice(&0u32.to_le_bytes()); // no confinement
    body.extend_from_slice(&0u32.to_le_bytes()); // no cursor
    body.extend_from_slice(&0u32.to_le_bytes()); // CurrentTime
    dispatch(f, client, 26, 0, &body); // GrabPointer
    assert!(
        f.state.active_pointer_grab.is_some_and(|grab| {
            grab.owner == ClientId(client)
                && grab.grab_window == ResourceId(window)
                && !grab.implicit
                && !grab.passive
                && !grab.via_xi2
        }),
        "GrabPointer must install A's explicit core grab"
    );
}

fn ungrab_core_pointer_request(f: &mut Fixture, client: u32) {
    dispatch(f, client, 27, 0, &0u32.to_le_bytes()); // UngrabPointer
}

fn grab_xi2_slave_sync(f: &mut Fixture, client: u32, window: u32, device: u16, mask: u32) {
    let mut body = Vec::with_capacity(24);
    body.extend_from_slice(&window.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes()); // CurrentTime
    body.extend_from_slice(&0u32.to_le_bytes()); // no cursor
    body.extend_from_slice(&device.to_le_bytes());
    body.extend_from_slice(&[0, 1, 0, 0]); // sync device, async paired, owner_events=false
    body.extend_from_slice(&1u16.to_le_bytes());
    body.extend_from_slice(&mask.to_le_bytes());
    dispatch(f, client, XI2_OPCODE, 51, &body); // XIGrabDevice
    assert!(
        f.state
            .xi2_pointer_grabs
            .get(&device)
            .is_some_and(|grab| grab.owner == ClientId(client)),
        "XIGrabDevice must install the slave grab"
    );
}

/// Spec invariant 8 (`../xserver/mi/mieq.c:502-508`, `dix/events.c`
/// `ComputeFreezes`): a frozen master withholds only the master's forms.
/// The slave's XI2 form is delivered at once; the master's core form is
/// queued and delivered once, at thaw, without repeating the slave form.
#[test]
fn master_frozen_slave_xi2_form_delivered_master_form_queued() {
    let mut f = p_fixture();
    let source = crate::xinput::InputSourceId(0xABCD);
    let device = super::pointer_source(&mut f.state, source.0, true);
    let origin = InputOrigin::Physical(source);
    select_xi2(&mut f, CLIENT_B, W, device, XI_MOTION);
    send(&mut f, origin, PointerEventKind::MotionNotify, 0, 150, 150);
    let _ = read_packets(&mut f.client_a);
    let _ = read_packets(&mut f.client_b);

    grab_core_pointer_sync(&mut f, CLIENT_A, W);
    let _ = read_packets(&mut f.client_a);
    let _ = read_packets(&mut f.client_b);
    assert!(
        f.state
            .xi1_frozen
            .get(&XI_MASTER)
            .is_some_and(crate::server::Xi1Freeze::frozen),
        "the synchronous core grab freezes the master pointer"
    );

    send(&mut f, origin, PointerEventKind::MotionNotify, 0, 155, 155);
    assert_eq!(
        read_packets(&mut f.client_b),
        vec![xi(6, device, device, W)],
        "the slave's XI2 form is delivered while only the master is frozen"
    );
    assert!(
        read_packets(&mut f.client_a).is_empty(),
        "the master's core form stays queued"
    );

    allow_events_core(&mut f, CLIENT_A, 0); // AsyncPointer
    assert_eq!(
        read_packets(&mut f.client_a),
        vec![core(6, W)],
        "the master's core form is delivered at thaw"
    );
    assert!(
        read_packets(&mut f.client_b).is_empty(),
        "thaw does not repeat the slave form"
    );
    assert!(f.state.sync_pending.is_empty(), "the queue is drained");
    ungrab_core_pointer_request(&mut f, CLIENT_A);
    assert!(read_packets(&mut f.client_a).is_empty());
    assert!(read_packets(&mut f.client_b).is_empty());
    assert_task1_end_state(&f, &[XI_MASTER, XI_MASTER_KEYBOARD]);
}

/// GUARD: when the slave itself is frozen, the whole event stays queued
/// (spec invariant 8, first rule; the master pass runs when the slave's
/// event is processed).
#[test]
fn slave_frozen_queues_whole_event() {
    let mut f = p_fixture();
    select_xi2(&mut f, CLIENT_B, W, XI_XTEST, XI_MOTION);
    send(
        &mut f,
        InputOrigin::XTest(XI_XTEST),
        PointerEventKind::MotionNotify,
        0,
        150,
        150,
    );
    let _ = read_packets(&mut f.client_a);
    let _ = read_packets(&mut f.client_b);

    grab_xi2_slave_sync(&mut f, CLIENT_A, W, XI_XTEST, XI_MOTION);
    let _ = read_packets(&mut f.client_a);
    let _ = read_packets(&mut f.client_b);
    send(
        &mut f,
        InputOrigin::XTest(XI_XTEST),
        PointerEventKind::MotionNotify,
        0,
        155,
        155,
    );
    assert!(read_packets(&mut f.client_a).is_empty(), "queued entirely");
    assert!(read_packets(&mut f.client_b).is_empty(), "queued entirely");
    assert_eq!(f.state.sync_pending.len(), 1);

    xi_allow_events(&mut f, CLIENT_A, XI_XTEST, 0); // AsyncDevice
    assert_eq!(
        read_packets(&mut f.client_a),
        vec![xi(6, XI_XTEST, XI_XTEST, W)],
        "the grabber receives the queued motion at thaw"
    );
    assert!(read_packets(&mut f.client_b).is_empty());
    assert!(f.state.sync_pending.is_empty());

    let mut ungrab_body = Vec::with_capacity(8);
    ungrab_body.extend_from_slice(&0u32.to_le_bytes());
    ungrab_body.extend_from_slice(&XI_XTEST.to_le_bytes());
    ungrab_body.extend_from_slice(&0u16.to_le_bytes());
    dispatch(&mut f, CLIENT_A, XI2_OPCODE, 52, &ungrab_body); // XIUngrabDevice
    assert_task1_end_state(&f, &[XI_XTEST]);
}

/// Spec invariant 8 for a press: with only the master frozen (a synchronous
/// keyboard grab holds the pointer, no pointer grab exists), the slave's XI2
/// press is delivered now, the master's bitmap is untouched, and the
/// master's passive-grab search and core press happen when the master thaws
/// (`../xserver/mi/mieq.c:502-508`, `dix/events.c` `ComputeFreezes`).
#[test]
fn master_frozen_press_slave_form_now_master_passive_check_at_thaw() {
    let mut f = p_fixture();
    let source = crate::xinput::InputSourceId(0xABCD);
    let device = super::pointer_source(&mut f.state, source.0, true);
    let origin = InputOrigin::Physical(source);
    select_xi2(&mut f, CLIENT_B, W, device, XI_BUTTONS);
    grab_button_core(&mut f, CLIENT_B, D, 1, ANY_MODIFIER, 1, false);
    send(&mut f, origin, PointerEventKind::MotionNotify, 0, 150, 150);
    let _ = read_packets(&mut f.client_a);
    let _ = read_packets(&mut f.client_b);

    // A's GrabKeyboard(pointer sync, keyboard async) freezes the master
    // pointer on A's behalf without any pointer grab.
    let mut body = Vec::with_capacity(8);
    body.extend_from_slice(&W.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes()); // CurrentTime
    body.extend_from_slice(&[0, 1, 0, 0]); // sync pointer, async keyboard
    dispatch(&mut f, CLIENT_A, 31, 0, &body);
    assert!(f.state.active_pointer_grab.is_none());
    assert!(
        f.state
            .xi1_frozen
            .get(&XI_MASTER)
            .is_some_and(crate::server::Xi1Freeze::frozen),
        "A's keyboard grab freezes the master pointer"
    );
    let _ = read_packets(&mut f.client_a);
    let _ = read_packets(&mut f.client_b);

    send(&mut f, origin, PointerEventKind::ButtonPress, 1, 150, 150);
    assert_eq!(
        read_packets(&mut f.client_b),
        vec![xi(4, device, device, W)],
        "the slave's XI2 press is delivered now, no core press yet"
    );
    assert!(read_packets(&mut f.client_a).is_empty());
    assert_eq!(f.state.buttons_down, 0, "the master's bitmap waits");
    assert!(f.state.active_pointer_grab.is_none());
    let slave_grab = f
        .state
        .xi2_pointer_grabs
        .get(&device)
        .expect("the delivered slave press creates its implicit grab before thaw");
    assert_eq!(slave_grab.owner, ClientId(CLIENT_B));
    assert_eq!(slave_grab.grab_window, ResourceId(W));
    assert!(slave_grab.via_xi2 && slave_grab.implicit && !slave_grab.passive);
    assert_eq!(f.state.sync_pending.len(), 1, "the master part is queued");

    dispatch(&mut f, CLIENT_A, 32, 0, &0u32.to_le_bytes()); // UngrabKeyboard
    assert_eq!(f.state.buttons_down, BUTTON_1, "the master press ran");
    assert!(
        f.state.active_pointer_grab.is_some_and(|grab| {
            grab.owner == ClientId(CLIENT_B)
                && grab.grab_window == ResourceId(D)
                && grab.passive
                && !grab.via_xi2
        }),
        "B's passive core grab on D is found by the master pass at thaw"
    );
    assert_eq!(
        read_packets(&mut f.client_b),
        vec![core(4, D)],
        "the master's core press reaches the passive grab owner once"
    );
    assert!(read_packets(&mut f.client_a).is_empty());
    assert!(f.state.sync_pending.is_empty());

    release(&mut f, origin, 1);
    assert_eq!(
        read_packets(&mut f.client_b),
        vec![xi(5, device, device, W), core(5, D)]
    );
    assert!(read_packets(&mut f.client_a).is_empty());
    assert_task1_end_state(&f, &[XI_MASTER, XI_MASTER_KEYBOARD]);
}

/// Reproduce XTS XAllowDeviceEvents-12's unresolved early return followed by
/// TP14's no-grab SyncAll/button/motion probe. The TP12 button press is queued
/// on the frozen master; its XTest slave motion still reaches the XI1 client,
/// so the XTS helper reports that the device was not frozen and exits through
/// grab-window cleanup. That cleanup must not let the delayed button press
/// install a synchronous core grab on a window that is being destroyed.
#[test]
fn xts_ade12_cleanup_does_not_leave_sync_core_grab_for_ade14() {
    let mut f = p_fixture();
    select_xi1_pointer_events(&mut f, W);

    // XTS grabstartup warps to (1,1) in grabwin before TP12's synchronous
    // keyboard grab and synchronous core Button1 passive grab.
    send(
        &mut f,
        InputOrigin::XTest(XI_XTEST),
        PointerEventKind::MotionNotify,
        0,
        101,
        101,
    );
    let _ = read_packets(&mut f.client_a);
    let mut keyboard_grab = Vec::with_capacity(12);
    keyboard_grab.extend_from_slice(&W.to_le_bytes());
    keyboard_grab.extend_from_slice(&0u32.to_le_bytes()); // CurrentTime
    keyboard_grab.extend_from_slice(&[0, 1, 1, 0]); // sync pointer + keyboard
    dispatch(&mut f, CLIENT_A, 31, 0, &keyboard_grab); // GrabKeyboard
    assert!(f.state.active_keyboard_grab.is_some());
    assert!(f.state.xi1_frozen[&XI_MASTER].frozen());

    let mut button_grab = Vec::with_capacity(20);
    button_grab.extend_from_slice(&W.to_le_bytes());
    button_grab.extend_from_slice(&0x0040u16.to_le_bytes()); // PointerMotionMask
    button_grab.extend_from_slice(&[1, 1]); // sync pointer + keyboard
    button_grab.extend_from_slice(&0u32.to_le_bytes()); // no confine window
    button_grab.extend_from_slice(&0u32.to_le_bytes()); // no cursor
    button_grab.extend_from_slice(&[1, 0]); // Button1, pad
    button_grab.extend_from_slice(&0u16.to_le_bytes()); // no modifiers
    dispatch(&mut f, CLIENT_A, 28, 0, &button_grab); // GrabButton

    send(
        &mut f,
        InputOrigin::XTest(XI_XTEST),
        PointerEventKind::ButtonPress,
        1,
        101,
        101,
    );
    assert!(f.state.active_pointer_grab.is_none());
    assert_eq!(
        f.state.sync_pending.len(),
        1,
        "the master press is deferred"
    );
    assert_eq!(
        read_packets(&mut f.client_a),
        vec![xi1_packet(4, XI_XTEST, W)],
        "TP12's button's slave form is reported before its master pass"
    );

    // XTS ispfrozen(): absolute device motion to x=0, retaining y=1. This is
    // visible on the slave, so TP12 returns early without its SyncAll path.
    send(
        &mut f,
        InputOrigin::XTest(XI_XTEST),
        PointerEventKind::MotionNotify,
        0,
        0,
        101,
    );
    assert_eq!(
        read_packets(&mut f.client_a),
        vec![xi1_packet(6, XI_XTEST, W)],
        "the TP12 probe makes the XTS purpose unresolved"
    );

    // TET's per-purpose grabcleanup destroys grabwin after TP12's early
    // return. TP13 then grabs the XI1 device and keyboard, allows SyncAll,
    // probes motion, and ungrabs both. This intervening purpose is needed to
    // reproduce the suite's exact TP12→TP14 state transition.
    dispatch(&mut f, CLIENT_A, 4, 0, &W.to_le_bytes()); // DestroyWindow
    let tp12_cleanup = (
        f.state.active_pointer_grab,
        f.state.active_keyboard_grab,
        f.state.xi1_frozen.values().all(|freeze| !freeze.frozen()),
        f.state.sync_pending.len(),
        f.state.master_sprite_root,
    );
    select_xi1_pointer_events(&mut f, OUT);

    let device_classes: Vec<u32> = [
        crate::xinput::XI_DEVICE_BUTTON_PRESS_OFFSET,
        crate::xinput::XI_DEVICE_BUTTON_RELEASE_OFFSET,
        crate::xinput::XI_DEVICE_MOTION_NOTIFY_OFFSET,
    ]
    .into_iter()
    .map(|offset| (u32::from(XI_XTEST) << 8) | u32::from(crate::server::XI_FIRST_EVENT + offset))
    .collect();
    let mut device_grab = Vec::with_capacity(16 + device_classes.len() * 4);
    device_grab.extend_from_slice(&OUT.to_le_bytes());
    device_grab.extend_from_slice(&0u32.to_le_bytes()); // CurrentTime
    device_grab.extend_from_slice(&u16::try_from(device_classes.len()).unwrap().to_le_bytes());
    device_grab.extend_from_slice(&[1, 0, 1, u8::try_from(XI_XTEST).unwrap()]); // sync, async, owner
    device_grab.extend_from_slice(&[0; 2]);
    for class in device_classes {
        device_grab.extend_from_slice(&class.to_le_bytes());
    }
    dispatch(&mut f, CLIENT_A, XI2_OPCODE, 13, &device_grab); // TP13 GrabDevice
    let mut keyboard_grab = Vec::with_capacity(12);
    keyboard_grab.extend_from_slice(&OUT.to_le_bytes());
    keyboard_grab.extend_from_slice(&0u32.to_le_bytes()); // CurrentTime
    keyboard_grab.extend_from_slice(&[0, 1, 1, 0]); // sync pointer + keyboard
    dispatch(&mut f, CLIENT_A, 31, 0, &keyboard_grab); // TP13 GrabKeyboard

    let mut allow_body = Vec::with_capacity(8);
    allow_body.extend_from_slice(&0u32.to_le_bytes()); // CurrentTime
    allow_body.extend_from_slice(&XI_XTEST.to_le_bytes());
    allow_body.extend_from_slice(&[5, 0]); // SyncAll
    dispatch(
        &mut f,
        CLIENT_A,
        XI2_OPCODE,
        XI_XI_ALLOW_EVENTS,
        &allow_body,
    );
    send(
        &mut f,
        InputOrigin::XTest(XI_XTEST),
        PointerEventKind::MotionNotify,
        0,
        0,
        101,
    ); // TP13 ispfrozen() probe while GrabDevice is active
    assert!(
        read_packets(&mut f.client_a)
            .iter()
            .any(|packet| packet.kind == 6 && packet.device == XI_XTEST && packet.window == OUT),
        "TP13 SyncAll releases its device before ispfrozen()"
    );

    dispatch(&mut f, CLIENT_A, 32, 0, &0u32.to_le_bytes()); // TP13 UngrabKeyboard
    let mut ungrab_body = Vec::with_capacity(8);
    ungrab_body.extend_from_slice(&0u32.to_le_bytes());
    ungrab_body.extend_from_slice(&[u8::try_from(XI_XTEST).unwrap(), 0, 0, 0]);
    dispatch(&mut f, CLIENT_A, XI2_OPCODE, 14, &ungrab_body); // TP13 UngrabDevice
    let _ = read_packets(&mut f.client_a);
    dispatch(&mut f, CLIENT_A, 4, 0, &OUT.to_le_bytes()); // TP13 grabcleanup

    // TP14's grabstartup creates a fresh window and selects DeviceMotionNotify
    // there. The absolute x=0 probe can route to root, so the test also selects
    // root as an observer; this does not install a grab or change thaw state.
    create_window(&mut f, CLIENT_A, TP14_WIN, 0, 0, 0, 200, 200);
    map_window(&mut f, CLIENT_A, TP14_WIN);
    f.xid_map.insert(HOST_W, ResourceId(TP14_WIN));
    select_xi1_pointer_events(&mut f, TP14_WIN);
    // Observe the x=0 absolute probe even when it is outside the test window;
    // this is only a test observer, while XTS observes its grab window.
    select_xi1_pointer_events(&mut f, crate::resources::ROOT_WINDOW.0);
    send(
        &mut f,
        InputOrigin::XTest(XI_XTEST),
        PointerEventKind::MotionNotify,
        0,
        1,
        1,
    ); // grabstartup warp
    let _ = read_packets(&mut f.client_a);

    // TP14 runs XAllowDeviceEvents(SyncAll) with no grabs, presses the
    // XTest button at (1,1), then checks whether its absolute x=0 probe is
    // delivered on TP14_WIN.
    let mut allow_body = Vec::with_capacity(8);
    allow_body.extend_from_slice(&0u32.to_le_bytes()); // CurrentTime
    allow_body.extend_from_slice(&XI_XTEST.to_le_bytes());
    allow_body.extend_from_slice(&[5, 0]); // SyncAll
    dispatch(
        &mut f,
        CLIENT_A,
        XI2_OPCODE,
        XI_XI_ALLOW_EVENTS,
        &allow_body,
    );
    send(
        &mut f,
        InputOrigin::XTest(XI_XTEST),
        PointerEventKind::ButtonPress,
        1,
        1,
        1,
    );
    send(
        &mut f,
        InputOrigin::XTest(XI_XTEST),
        PointerEventKind::MotionNotify,
        0,
        0,
        1,
    );
    let packets = read_packets(&mut f.client_a);
    assert!(
        packets.iter().any(|packet| {
            !packet.xi2
                && packet.kind == 6
                && packet.device == XI_XTEST
                && (packet.window == TP14_WIN || packet.window == crate::resources::ROOT_WINDOW.0)
        }),
        "TP14's button must not freeze the device after SyncAll; packets={packets:?}; active={:?}; freezes={:?}; queue={:?}",
        f.state.active_pointer_grab,
        f.state.xi1_frozen,
        f.state.sync_pending
    );
    assert!(
        tp12_cleanup.0.is_none(),
        "TP12 cleanup state: pointer={:?}, keyboard={:?}, xi1_thawed={}, queued={}, master_sprite_root={:?}",
        tp12_cleanup.0,
        tp12_cleanup.1,
        tp12_cleanup.2,
        tp12_cleanup.3,
        tp12_cleanup.4
    );
    assert!(
        tp12_cleanup.1.is_none(),
        "TP12 cleanup left a keyboard grab"
    );
    assert!(tp12_cleanup.2, "TP12 cleanup left an XI1 device frozen");
    assert_eq!(tp12_cleanup.3, 0, "TP12 cleanup left queued master events");
    assert_eq!(tp12_cleanup.4, (0, 101), "TP12 probe sprite root");
    assert!(
        f.state.active_pointer_grab.is_none(),
        "TP12 cleanup left active pointer grab {:?}; keyboard {:?}; XI1 freezes {:?}; queue {:?}",
        f.state.active_pointer_grab,
        f.state.active_keyboard_grab,
        f.state.xi1_frozen,
        f.state.sync_pending
    );
    assert!(f.state.sync_pending.is_empty());
    assert!(f.state.xi1_frozen.values().all(|freeze| !freeze.frozen()));
    send(
        &mut f,
        InputOrigin::XTest(XI_XTEST),
        PointerEventKind::ButtonRelease,
        1,
        0,
        1,
    ); // XTS devicerelbuttons() cleanup
    assert_eq!(f.state.buttons_down, 0);
    assert_eq!(f.state.xi_devices.device(XI_XTEST).unwrap().buttons_down, 0);
    assert!(f.state.sync_pending.is_empty());
}

fn select_xi1_pointer_events(f: &mut Fixture, window: u32) {
    let classes: Vec<u32> = [
        crate::xinput::XI_DEVICE_BUTTON_PRESS_OFFSET,
        crate::xinput::XI_DEVICE_BUTTON_RELEASE_OFFSET,
        crate::xinput::XI_DEVICE_MOTION_NOTIFY_OFFSET,
    ]
    .into_iter()
    .map(|offset| (u32::from(XI_XTEST) << 8) | u32::from(crate::server::XI_FIRST_EVENT + offset))
    .collect();
    let mut body = Vec::with_capacity(8 + classes.len() * 4);
    body.extend_from_slice(&window.to_le_bytes());
    body.extend_from_slice(&u16::try_from(classes.len()).unwrap().to_le_bytes());
    body.extend_from_slice(&[0; 2]);
    for class in classes {
        body.extend_from_slice(&class.to_le_bytes());
    }
    dispatch(f, CLIENT_A, XI2_OPCODE, 6, &body); // XI1 SelectExtensionEvent
}

fn xi1_packet(kind: u16, device: u16, window: u32) -> super::implicit_grabs::Packet {
    super::implicit_grabs::Packet {
        xi2: false,
        kind,
        device,
        source: device,
        window,
        scroll_value: None,
        child: 0,
        event_x: 0,
        event_y: 0,
    }
}

fn grab_keyboard_sync_pointer(f: &mut Fixture, client: u32, window: u32) {
    let mut body = Vec::with_capacity(8);
    body.extend_from_slice(&window.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes()); // CurrentTime
    body.extend_from_slice(&[0, 1, 0, 0]); // sync pointer, async keyboard
    dispatch(f, client, 31, 0, &body);
}

fn ungrab_keyboard(f: &mut Fixture, client: u32) {
    dispatch(f, client, 32, 0, &0u32.to_le_bytes());
}

/// Xorg's deferred master pass drops a transition already held by another
/// source before passive-grab matching or delivery
/// (`../xserver/Xi/exevents.c:944-982,1848-1850`). Replaying a processed
/// activating event remains Task 3's separate path.
#[test]
fn queued_duplicate_master_press_is_dropped_before_passive_activation() {
    let mut f = p_fixture();
    let source_a = crate::xinput::InputSourceId(0xABCD);
    let source_b = crate::xinput::InputSourceId(0xABCE);
    let _device_a = super::pointer_source(&mut f.state, source_a.0, true);
    let device_b = super::pointer_source(&mut f.state, source_b.0, true);
    let origin_a = InputOrigin::Physical(source_a);
    let origin_b = InputOrigin::Physical(source_b);
    select_xi2(&mut f, CLIENT_B, W, device_b, XI_BUTTONS);
    grab_button_core(&mut f, CLIENT_B, D, 1, ANY_MODIFIER, 1, false);

    // Source A holds button 1 outside D, so it cannot activate B's passive grab.
    send(&mut f, origin_a, PointerEventKind::ButtonPress, 1, 650, 150);
    assert_eq!(f.state.buttons_down, BUTTON_1);
    let _ = read_packets(&mut f.client_a);
    let _ = read_packets(&mut f.client_b);
    grab_keyboard_sync_pointer(&mut f, CLIENT_A, W);
    assert!(f.state.xi1_frozen[&XI_MASTER].frozen());
    let _ = read_packets(&mut f.client_a);
    let _ = read_packets(&mut f.client_b);

    send(&mut f, origin_b, PointerEventKind::ButtonPress, 1, 150, 150);
    assert_eq!(
        read_packets(&mut f.client_b),
        vec![xi(4, device_b, device_b, W)],
        "the live slave pass delivers before the queued master transition"
    );
    assert_eq!(f.state.sync_pending.len(), 1);

    ungrab_keyboard(&mut f, CLIENT_A);
    assert_eq!(f.state.buttons_down, BUTTON_1);
    assert!(
        f.state.active_pointer_grab.is_none(),
        "a duplicate master press must not activate B's passive core grab"
    );
    assert!(
        read_packets(&mut f.client_b).is_empty(),
        "the rejected master transition has no second form"
    );

    send(
        &mut f,
        origin_b,
        PointerEventKind::ButtonRelease,
        1,
        150,
        150,
    );
    send(
        &mut f,
        origin_a,
        PointerEventKind::ButtonRelease,
        1,
        650,
        150,
    );
    assert_eq!(f.state.buttons_down, 0);
    assert_eq!(f.state.xi_devices.device(device_b).unwrap().buttons_down, 0);
    assert_task1_end_state(&f, &[XI_MASTER, XI_MASTER_KEYBOARD]);
}

/// Xorg also drops a queued release when another attached source still holds
/// the aggregate master button (`../xserver/Xi/exevents.c:944-982`).
#[test]
fn queued_aggregate_release_is_not_delivered_until_last_source_releases() {
    let mut f = p_fixture();
    let source_a = crate::xinput::InputSourceId(0xABCF);
    let source_b = crate::xinput::InputSourceId(0xABD0);
    let device_a = super::pointer_source(&mut f.state, source_a.0, true);
    let device_b = super::pointer_source(&mut f.state, source_b.0, true);
    let origin_a = InputOrigin::Physical(source_a);
    let origin_b = InputOrigin::Physical(source_b);
    select_xi2(&mut f, CLIENT_B, W, XI_MASTER, XI_BUTTON_RELEASE);

    send(&mut f, origin_a, PointerEventKind::ButtonPress, 1, 150, 150);
    send(&mut f, origin_b, PointerEventKind::ButtonPress, 1, 150, 150);
    assert_eq!(f.state.buttons_down, BUTTON_1);
    assert_eq!(
        f.state.xi_devices.device(device_a).unwrap().buttons_down,
        BUTTON_1
    );
    assert_eq!(
        f.state.xi_devices.device(device_b).unwrap().buttons_down,
        BUTTON_1
    );
    let _ = read_packets(&mut f.client_a);
    let _ = read_packets(&mut f.client_b);

    grab_keyboard_sync_pointer(&mut f, CLIENT_A, W);
    assert!(f.state.xi1_frozen[&XI_MASTER].frozen());
    let _ = read_packets(&mut f.client_a);
    let _ = read_packets(&mut f.client_b);
    send(
        &mut f,
        origin_b,
        PointerEventKind::ButtonRelease,
        1,
        150,
        150,
    );
    assert_eq!(f.state.xi_devices.device(device_b).unwrap().buttons_down, 0);
    assert!(read_packets(&mut f.client_b).is_empty());
    assert_eq!(f.state.sync_pending.len(), 1);

    ungrab_keyboard(&mut f, CLIENT_A);
    assert_eq!(f.state.buttons_down, BUTTON_1);
    assert!(
        read_packets(&mut f.client_b).is_empty(),
        "the queued aggregate release is rejected while source A holds button 1"
    );

    send(
        &mut f,
        origin_a,
        PointerEventKind::ButtonRelease,
        1,
        150,
        150,
    );
    assert_eq!(
        read_packets(&mut f.client_b),
        vec![xi(5, XI_MASTER, device_a, W)],
        "the last source's release changes and delivers the master transition"
    );
    assert_eq!(f.state.buttons_down, 0);
    assert_task1_end_state(&f, &[XI_MASTER, XI_MASTER_KEYBOARD]);
}

/// The physical scroll conversion belongs to the live slave pass. Its cooked
/// master Motion waits with the master part and carries the already-generated
/// valuator through thaw, so the source counter advances once.
#[test]
fn master_frozen_scroll_waits_for_thaw_without_advancing_slave_twice() {
    let mut f = p_fixture();
    let source = crate::xinput::InputSourceId(0xABD1);
    let device = super::pointer_source(&mut f.state, source.0, true);
    let origin = InputOrigin::Physical(source);
    select_xi2(&mut f, CLIENT_B, W, device, XI_MOTION);
    select_xi2(&mut f, CLIENT_A, W, XI_MASTER, XI_MOTION);
    grab_keyboard_sync_pointer(&mut f, CLIENT_A, W);
    let _ = read_packets(&mut f.client_a);
    let _ = read_packets(&mut f.client_b);

    send(&mut f, origin, PointerEventKind::ButtonPress, 5, 150, 150);
    let mut slave_scroll = xi(6, device, device, W);
    slave_scroll.scroll_value = Some(1);
    assert_eq!(
        read_packets(&mut f.client_b),
        vec![slave_scroll],
        "slave cooked scroll Motion is processed during the slave pass"
    );
    assert!(
        read_packets(&mut f.client_a).is_empty(),
        "the master's cooked scroll Motion waits behind the pointer freeze"
    );
    assert_eq!(
        f.state
            .xi_devices
            .device(device)
            .unwrap()
            .scroll_axis_values,
        [1, 0],
        "the source scroll counter advances in the slave pass"
    );

    ungrab_keyboard(&mut f, CLIENT_A);
    let mut master_scroll = xi(6, XI_MASTER, device, W);
    master_scroll.scroll_value = Some(1);
    assert_eq!(read_packets(&mut f.client_a), vec![master_scroll]);
    assert!(
        read_packets(&mut f.client_b).is_empty(),
        "thaw emits no second slave Motion"
    );
    assert_eq!(
        f.state
            .xi_devices
            .device(device)
            .unwrap()
            .scroll_axis_values,
        [1, 0],
        "processing the queued master part does not mutate slave scroll state"
    );

    send(&mut f, origin, PointerEventKind::ButtonRelease, 5, 150, 150);
    assert_eq!(f.state.buttons_down, 0);
    assert_eq!(f.state.xi_devices.device(device).unwrap().buttons_down, 0);
    assert_task1_end_state(&f, &[XI_MASTER, XI_MASTER_KEYBOARD]);
}

/// Xorg builds smooth scroll Motion before the emulated legacy button events
/// (`../xserver/dix/getevents.c:1646-1718`). Rejecting a duplicate master
/// button transition at thaw must not discard its already-saved Motion.
#[test]
fn queued_duplicate_wheel_press_does_not_suppress_master_scroll_motion() {
    let mut f = p_fixture();
    let source_a = crate::xinput::InputSourceId(0xABD7);
    let source_b = crate::xinput::InputSourceId(0xABD8);
    let _device_a = super::pointer_source(&mut f.state, source_a.0, true);
    let device_b = super::pointer_source(&mut f.state, source_b.0, true);
    let origin_a = InputOrigin::Physical(source_a);
    let origin_b = InputOrigin::Physical(source_b);
    select_xi2(&mut f, CLIENT_B, W, device_b, XI_MOTION | XI_BUTTONS);
    select_xi2(&mut f, CLIENT_A, W, XI_MASTER, XI_MOTION);
    grab_button_core(&mut f, CLIENT_B, D, 5, ANY_MODIFIER, 1, false);

    // Source A already holds button 5. Its location is outside W, so it
    // cannot activate B's passive grab on D.
    send(&mut f, origin_a, PointerEventKind::ButtonPress, 5, 650, 150);
    assert_eq!(f.state.buttons_down, 1 << 4);
    let _ = read_packets(&mut f.client_a);
    let _ = read_packets(&mut f.client_b);

    grab_keyboard_sync_pointer(&mut f, CLIENT_A, W);
    assert!(f.state.xi1_frozen[&XI_MASTER].frozen());
    let _ = read_packets(&mut f.client_a);
    let _ = read_packets(&mut f.client_b);

    send(&mut f, origin_b, PointerEventKind::ButtonPress, 5, 150, 150);
    let mut slave_scroll = xi(6, device_b, device_b, W);
    slave_scroll.scroll_value = Some(1);
    assert_eq!(
        read_packets(&mut f.client_b),
        vec![slave_scroll, xi(4, device_b, device_b, W)],
        "the live slave pass reports its cooked scroll and legacy button"
    );
    assert!(read_packets(&mut f.client_a).is_empty());
    assert_eq!(
        f.state
            .xi_devices
            .device(device_b)
            .unwrap()
            .scroll_axis_values,
        [1, 0]
    );
    assert_eq!(f.state.sync_pending.len(), 1);

    ungrab_keyboard(&mut f, CLIENT_A);
    let mut master_scroll = xi(6, XI_MASTER, device_b, W);
    master_scroll.scroll_value = Some(1);
    assert_eq!(
        read_packets(&mut f.client_a),
        vec![master_scroll],
        "the saved master scroll Motion is independent of duplicate button rejection"
    );
    assert!(
        read_packets(&mut f.client_b).is_empty(),
        "the rejected master button has no second delivery"
    );
    assert!(
        f.state.active_pointer_grab.is_none(),
        "the duplicate master press does not activate B's passive core grab"
    );
    assert_eq!(f.state.buttons_down, 1 << 4);
    assert_eq!(
        f.state
            .xi_devices
            .device(device_b)
            .unwrap()
            .scroll_axis_values,
        [1, 0],
        "master thaw does not advance the slave scroll counter again"
    );

    send(
        &mut f,
        origin_b,
        PointerEventKind::ButtonRelease,
        5,
        150,
        150,
    );
    let _ = read_packets(&mut f.client_a);
    let _ = read_packets(&mut f.client_b);
    send(
        &mut f,
        origin_a,
        PointerEventKind::ButtonRelease,
        5,
        650,
        150,
    );
    assert_task1_end_state(&f, &[XI_MASTER, XI_MASTER_KEYBOARD]);
}

/// Slave recipients are resolved from the pre-move window, while an explicit
/// master grab independently receives the master form even when that client
/// only has a natural slave selection on the old window.
#[test]
fn earlier_slave_selection_does_not_veto_master_grab_delivery() {
    let mut f = p_fixture();
    let source = crate::xinput::InputSourceId(0xABD2);
    let device = super::pointer_source(&mut f.state, source.0, true);
    let origin = InputOrigin::Physical(source);
    send(&mut f, origin, PointerEventKind::MotionNotify, 0, 150, 150);
    let _ = read_packets(&mut f.client_a);
    let _ = read_packets(&mut f.client_b);
    select_xi2(&mut f, CLIENT_A, W, device, XI_MOTION);

    let mut grab_body = Vec::with_capacity(24);
    grab_body.extend_from_slice(&W.to_le_bytes());
    grab_body.extend_from_slice(&0u32.to_le_bytes()); // CurrentTime
    grab_body.extend_from_slice(&0u32.to_le_bytes()); // no cursor
    grab_body.extend_from_slice(&XI_MASTER.to_le_bytes());
    grab_body.extend_from_slice(&[1, 1, 0, 0]); // async pointer and paired device
    grab_body.extend_from_slice(&1u16.to_le_bytes());
    grab_body.extend_from_slice(&XI_MOTION.to_le_bytes());
    dispatch(&mut f, CLIENT_A, XI2_OPCODE, 51, &grab_body); // XIGrabDevice
    assert!(f.state.active_pointer_grab.is_some_and(|grab| {
        grab.owner == ClientId(CLIENT_A)
            && grab.grab_window == ResourceId(W)
            && grab.via_xi2
            && !grab.passive
            && !grab.implicit
    }));
    let _ = read_packets(&mut f.client_a); // successful XIGrabDevice reply

    send(&mut f, origin, PointerEventKind::MotionNotify, 0, 650, 150);
    assert_eq!(
        read_packets(&mut f.client_a),
        vec![xi(6, device, device, W), xi(6, XI_MASTER, device, W),],
        "the slave form is natural on W and the master form routes through A's grab on W"
    );
    assert!(read_packets(&mut f.client_b).is_empty());

    let mut ungrab_body = Vec::with_capacity(8);
    ungrab_body.extend_from_slice(&0u32.to_le_bytes()); // CurrentTime
    ungrab_body.extend_from_slice(&XI_MASTER.to_le_bytes());
    ungrab_body.extend_from_slice(&0u16.to_le_bytes());
    dispatch(&mut f, CLIENT_A, XI2_OPCODE, 52, &ungrab_body); // XIUngrabDevice
    assert_task1_end_state(&f, &[XI_MASTER, XI_MASTER_KEYBOARD]);
}

/// The queued master event is owned by the master captured in
/// `PendingSyncEvent`, even if an explicit XI2 slave grab detaches its source
/// before thaw (`../xserver/mi/mieq.c:484-508`).
#[test]
fn queued_master_press_uses_captured_master_after_source_detach() {
    let mut f = p_fixture();
    let source = crate::xinput::InputSourceId(0xABD3);
    let device = super::pointer_source(&mut f.state, source.0, true);
    let origin = InputOrigin::Physical(source);
    grab_button_core(&mut f, CLIENT_B, D, 1, ANY_MODIFIER, 1, false);
    grab_keyboard_sync_pointer(&mut f, CLIENT_A, W);
    assert!(f.state.xi1_frozen[&XI_MASTER].frozen());
    let _ = read_packets(&mut f.client_a);
    let _ = read_packets(&mut f.client_b);

    send(&mut f, origin, PointerEventKind::ButtonPress, 1, 150, 150);
    assert_eq!(f.state.buttons_down, 0, "the master transition is deferred");
    assert_eq!(
        f.state.xi_devices.device(device).unwrap().buttons_down,
        BUTTON_1
    );
    assert_eq!(f.state.sync_pending.len(), 1);

    let mut grab_body = Vec::with_capacity(20);
    grab_body.extend_from_slice(&crate::resources::ROOT_WINDOW.0.to_le_bytes());
    grab_body.extend_from_slice(&0u32.to_le_bytes()); // CurrentTime
    grab_body.extend_from_slice(&0u32.to_le_bytes()); // no cursor
    grab_body.extend_from_slice(&device.to_le_bytes());
    grab_body.extend_from_slice(&[1, 1, 0, 0]); // async pointer and paired device
    grab_body.extend_from_slice(&0u16.to_le_bytes()); // no XI2 event mask
    dispatch(&mut f, CLIENT_A, XI2_OPCODE, 51, &grab_body); // XIGrabDevice
    let _ = read_packets(&mut f.client_a); // successful XIGrabDevice reply
    assert!(f.state.xi2_pointer_grabs.get(&device).is_some_and(|grab| {
        grab.owner == ClientId(CLIENT_A)
            && grab.grab_window == crate::resources::ROOT_WINDOW
            && grab.via_xi2
            && !grab.passive
            && !grab.implicit
    }));
    assert_eq!(
        f.state.xi_devices.device(device).unwrap().attached_master,
        None,
        "the explicit slave grab detaches the pending source"
    );

    ungrab_keyboard(&mut f, CLIENT_A);
    assert_eq!(f.state.buttons_down, BUTTON_1);
    let active = f
        .state
        .active_pointer_grab
        .expect("the captured master pass activates B's passive core grab");
    assert_eq!(active.owner, ClientId(CLIENT_B));
    assert_eq!(active.grab_window, ResourceId(D));
    assert!(active.passive && !active.implicit && !active.via_xi2);
    assert_eq!(read_packets(&mut f.client_b), vec![core(4, D)]);
    assert!(read_packets(&mut f.client_a).is_empty());
    assert!(f.state.sync_pending.is_empty());

    let mut ungrab_body = Vec::with_capacity(8);
    ungrab_body.extend_from_slice(&0u32.to_le_bytes()); // CurrentTime
    ungrab_body.extend_from_slice(&device.to_le_bytes());
    ungrab_body.extend_from_slice(&0u16.to_le_bytes());
    dispatch(&mut f, CLIENT_A, XI2_OPCODE, 52, &ungrab_body); // XIUngrabDevice
    assert_eq!(
        f.state.xi_devices.device(device).unwrap().attached_master,
        Some(XI_MASTER)
    );
    send(&mut f, origin, PointerEventKind::ButtonRelease, 1, 150, 150);
    assert_eq!(read_packets(&mut f.client_b), vec![core(5, D)]);
    assert!(f.state.active_pointer_grab.is_none());
    assert_task1_end_state(&f, &[XI_MASTER, XI_MASTER_KEYBOARD, device]);
}

/// A floating pointer updates its private position, not the master sprite
/// used to resolve the next attached slave's pre-move window.
#[test]
fn floating_pointer_motion_does_not_advance_master_sprite_snapshot() {
    let mut f = p_fixture();
    let attached_source = crate::xinput::InputSourceId(0xABD4);
    let floating_source = crate::xinput::InputSourceId(0xABD5);
    let attached_device = super::pointer_source(&mut f.state, attached_source.0, true);
    let floating_device = super::pointer_source(&mut f.state, floating_source.0, true);
    let attached_origin = InputOrigin::Physical(attached_source);
    let floating_origin = InputOrigin::Physical(floating_source);
    select_xi2(&mut f, CLIENT_A, W, attached_device, XI_MOTION);
    send(
        &mut f,
        attached_origin,
        PointerEventKind::MotionNotify,
        0,
        150,
        150,
    );
    let _ = read_packets(&mut f.client_a);
    let _ = read_packets(&mut f.client_b);

    let mut grab_body = Vec::with_capacity(20);
    grab_body.extend_from_slice(&crate::resources::ROOT_WINDOW.0.to_le_bytes());
    grab_body.extend_from_slice(&0u32.to_le_bytes()); // CurrentTime
    grab_body.extend_from_slice(&0u32.to_le_bytes()); // no cursor
    grab_body.extend_from_slice(&floating_device.to_le_bytes());
    grab_body.extend_from_slice(&[1, 1, 0, 0]); // async pointer and paired device
    grab_body.extend_from_slice(&0u16.to_le_bytes()); // no XI2 event mask
    dispatch(&mut f, CLIENT_A, XI2_OPCODE, 51, &grab_body); // XIGrabDevice
    let _ = read_packets(&mut f.client_a);
    assert_eq!(
        f.state
            .xi_devices
            .device(floating_device)
            .unwrap()
            .attached_master,
        None,
        "the second source is floating while explicitly grabbed"
    );

    send(
        &mut f,
        floating_origin,
        PointerEventKind::MotionNotify,
        0,
        650,
        150,
    );
    let _ = read_packets(&mut f.client_a);
    assert_eq!(f.state.pointer_root, (150, 150));
    assert_eq!(
        f.state.floating_pointer_positions.get(&floating_device),
        Some(&(650.0, 150.0))
    );

    send(
        &mut f,
        attached_origin,
        PointerEventKind::MotionNotify,
        0,
        650,
        150,
    );
    assert_eq!(
        read_packets(&mut f.client_a),
        vec![xi(6, attached_device, attached_device, W)],
        "the attached slave routes from the master's still-current sprite in W"
    );

    let mut ungrab_body = Vec::with_capacity(8);
    ungrab_body.extend_from_slice(&0u32.to_le_bytes()); // CurrentTime
    ungrab_body.extend_from_slice(&floating_device.to_le_bytes());
    ungrab_body.extend_from_slice(&0u16.to_le_bytes());
    dispatch(&mut f, CLIENT_A, XI2_OPCODE, 52, &ungrab_body); // XIUngrabDevice
    assert_task1_end_state(&f, &[floating_device]);
}

/// Both slave passes see W while their master copies wait in a frozen queue:
/// only processing those master copies moves the sprite window, matching
/// Xorg `ProcessOtherEvent`/`CheckMotion` (`../xserver/Xi/exevents.c:1854-1855,1950`).
#[test]
fn successive_motions_behind_frozen_master_keep_the_pre_move_window() {
    let mut f = p_fixture();
    let source = crate::xinput::InputSourceId(0xABD6);
    let device = super::pointer_source(&mut f.state, source.0, true);
    let origin = InputOrigin::Physical(source);
    select_xi2(&mut f, CLIENT_B, W, device, XI_MOTION);
    select_xi2(&mut f, CLIENT_B, OUT, XI_MASTER, XI_MOTION);
    send(&mut f, origin, PointerEventKind::MotionNotify, 0, 150, 150);
    assert_eq!(f.state.master_sprite_root, (150, 150));
    let _ = read_packets(&mut f.client_a);
    let _ = read_packets(&mut f.client_b);
    grab_keyboard_sync_pointer(&mut f, CLIENT_A, W);
    assert!(f.state.xi1_frozen[&XI_MASTER].frozen());
    let _ = read_packets(&mut f.client_a);
    let _ = read_packets(&mut f.client_b);

    for x in [650, 700] {
        send(&mut f, origin, PointerEventKind::MotionNotify, 0, x, 150);
        let packets = read_packets(&mut f.client_b);
        assert_eq!(
            packets,
            vec![xi(6, device, device, W)],
            "the slave pass at x={x} still uses W while the master is frozen"
        );
    }
    assert_eq!(f.state.master_sprite_root, (150, 150));
    assert_eq!(f.state.sync_pending.len(), 2);

    ungrab_keyboard(&mut f, CLIENT_A);
    assert_eq!(
        read_packets(&mut f.client_b),
        vec![xi(6, XI_MASTER, device, OUT), xi(6, XI_MASTER, device, OUT)],
        "thaw processes both master forms without repeating either slave form"
    );
    assert_eq!(f.state.master_sprite_root, (700, 150));
    assert!(f.state.sync_pending.is_empty());
    assert_task1_end_state(&f, &[XI_MASTER, XI_MASTER_KEYBOARD]);
}

/// Xvfb 21.1.24 measurement (cases A, B, C, E; spec invariant 8): one client
/// selects XI2 Motion for slave 4 and master 2 on W. The slave's form routes
/// from the window under the sprite BEFORE the move, the master's from AFTER.
#[test]
fn xi2_slave_motion_routes_from_pre_move_window_master_from_post_move() {
    let mut f = p_fixture();
    let origin = InputOrigin::XTest(XI_XTEST);
    select_xi2(&mut f, CLIENT_A, W, XI_XTEST, XI_MOTION);
    select_xi2(&mut f, CLIENT_A, W, XI_MASTER, XI_MOTION);
    send(&mut f, origin, PointerEventKind::MotionNotify, 0, 145, 145);
    let _ = read_packets(&mut f.client_a);
    let _ = read_packets(&mut f.client_b);

    // C: inside -> inside, both forms (slave first).
    send(&mut f, origin, PointerEventKind::MotionNotify, 0, 150, 150);
    assert_eq!(
        read_packets(&mut f.client_a),
        vec![xi(6, XI_XTEST, XI_XTEST, W), xi(6, XI_MASTER, XI_XTEST, W)]
    );
    // A: inside -> outside, slave form only.
    send(&mut f, origin, PointerEventKind::MotionNotify, 0, 650, 150);
    assert_eq!(
        read_packets(&mut f.client_a),
        vec![xi(6, XI_XTEST, XI_XTEST, W)]
    );
    // E: outside -> outside, nothing.
    send(&mut f, origin, PointerEventKind::MotionNotify, 0, 700, 150);
    assert!(read_packets(&mut f.client_a).is_empty());
    // B: outside -> inside, master form only.
    send(&mut f, origin, PointerEventKind::MotionNotify, 0, 150, 150);
    assert_eq!(
        read_packets(&mut f.client_a),
        vec![xi(6, XI_MASTER, XI_XTEST, W)]
    );
    assert!(read_packets(&mut f.client_b).is_empty());
    assert_task1_end_state(&f, &[]);
}
