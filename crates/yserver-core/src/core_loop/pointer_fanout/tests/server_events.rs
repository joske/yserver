//! Pointer events whose window or raw form depends on who made the motion:
//! a grab's pick (the grab window's child) and a warp (no raw motion).

use super::*;

/// xwininfo, xprop and xkill grab the pointer on the root and take the
/// window clicked from the ButtonPress's `child`, which Xorg's
/// `FixUpEventFromWindow` fills with the grab window's child toward the
/// pointer's window. With `child` 0 they picked the root.
#[test]
fn a_grabbed_press_names_the_grab_windows_child_under_the_pointer() {
    use crate::{backend::Backend, resources::ROOT_VISUAL, server::ActivePointerGrab};

    const PICKER: u32 = 1;
    const APP: u32 = 2;
    const APP_WIN: u32 = 0x0020_0091;
    const APP_CHILD: u32 = 0x0020_0092;
    const HOST_APP_XID: u32 = 0xCAFE_0091;

    let mut state = ServerState::new();
    let mut picker = install_client(&mut state, PICKER);
    let _app = install_client(&mut state, APP);
    let mut backend = RecordingBackend::new();
    for (window, parent, x, y) in [
        (APP_WIN, ROOT_WINDOW, 400, 300),
        (APP_CHILD, ResourceId(APP_WIN), 10, 10),
    ] {
        state.resources.create_window(
            ClientId(APP),
            yserver_protocol::x11::CreateWindowRequest {
                depth: 24,
                window: ResourceId(window),
                parent,
                x,
                y,
                width: 100,
                height: 100,
                class: 1,
                visual: ROOT_VISUAL,
                ..Default::default()
            },
        );
        let _ = state.resources.map_window(ResourceId(window));
    }
    Backend::register_top_level(&mut backend, None, ResourceId(APP_WIN), HOST_APP_XID)
        .expect("register app host xid");
    state.active_pointer_grab = Some(ActivePointerGrab {
        owner: ClientId(PICKER),
        grab_window: ROOT_WINDOW,
        event_mask: 0x0000_0004 | 0x0000_0008, // ButtonPress | ButtonRelease
        cursor: ResourceId(0),
        time: 0,
        owner_events: false,
        via_xi2: false,
        implicit: false,
        passive: false,
        xi2_mask: 0,
    });
    let xid_map = backend.xid_map().clone();
    let press = HostPointerEvent {
        kind: PointerEventKind::ButtonPress,
        host_xid: HOST_APP_XID,
        detail: 1,
        time: 0x1000,
        root_x: 420,
        root_y: 320,
        event_x: 20,
        event_y: 20,
        state: 0,
        crossing_mode: 0,
        child: 0,
        raw_dx: 0,
        raw_dy: 0,
        origin: crate::core_loop::message::InputOrigin::XTest(4),
        tree_change: false,
        device_axes: None,
    };
    let _ = pointer_event_fanout_to_state(&mut state, &mut backend, &xid_map, press, true, false);
    let bytes = read_all_available(&mut picker);
    assert!(bytes.len() >= 32, "the grab client gets the press");
    assert_eq!(bytes[0] & 0x7f, 4, "ButtonPress");
    let event = u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);
    let child = u32::from_le_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    assert_eq!(event, ROOT_WINDOW.0, "reported on the grab window");
    assert_eq!(
        child, APP_WIN,
        "the root's child under the pointer, not its child's child"
    );
}

/// The XI2 event types of a stream of GenericEvents.
fn xi2_evtypes(bytes: &[u8]) -> Vec<u16> {
    let mut evtypes = Vec::new();
    let mut off = 0usize;
    while off + 32 <= bytes.len() {
        if bytes[off] == 35 {
            evtypes.push(u16::from_le_bytes([bytes[off + 8], bytes[off + 9]]));
            let length = u32::from_le_bytes(bytes[off + 4..off + 8].try_into().unwrap()) as usize;
            off += 32 + length * 4;
        } else {
            off += 32;
        }
    }
    evtypes
}

#[test]
fn a_warp_moves_the_pointer_with_no_raw_motion() {
    // Xorg: WarpPointer -> miPointerMove -> GetPointerEvents with
    // POINTER_NORAW; raw events come only from the device.
    let mut state = ServerState::new();
    let mut backend = crate::backend::recording::RecordingBackend::new();
    let mut peer = install_client(&mut state, 1);
    state
        .clients
        .get_mut(&1)
        .expect("client")
        .xi2_masks
        .insert((ROOT_WINDOW, 0), (1 << 6) | (1 << 17));
    let dropped = warp_pointer_event_fanout_to_state(
        &mut state,
        &mut backend,
        &HostXidMap::new(),
        motion_event(),
    );
    assert!(dropped.is_empty());
    let evtypes = xi2_evtypes(&read_all_available(&mut peer));
    assert!(evtypes.contains(&6), "the warp's XI_Motion: {evtypes:?}");
    assert!(!evtypes.contains(&17), "no XI_RawMotion: {evtypes:?}");

    // A motion from the device has its raw event.
    let mut device = motion_event();
    device.root_x += 1;
    device.event_x += 1;
    device.raw_dx = 1;
    let dropped = pointer_event_fanout_to_state(
        &mut state,
        &mut backend,
        &HostXidMap::new(),
        device,
        true,
        false,
    );
    assert!(dropped.is_empty());
    let evtypes = xi2_evtypes(&read_all_available(&mut peer));
    assert!(
        evtypes.contains(&17),
        "the device's XI_RawMotion: {evtypes:?}"
    );
}
