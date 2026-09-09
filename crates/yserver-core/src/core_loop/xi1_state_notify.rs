//! XI 1.x DeviceStateNotify delivery — the port of Xorg's
//! `DeliverStateNotifyEvent` (dix/enterleave.c:697-765): after every
//! DeviceFocusIn delivered to a window, clients that selected the
//! `DeviceStateNotify` event class on that window for the device get a
//! snapshot of the device's key / button / valuator state.
//!
//! The snapshot is a chain of 32-byte wire events reassembled by libXi
//! into one `XDeviceStateNotifyEvent` (XExtInt.c): a leading
//! `deviceStateNotify` (first 32 key bits, 32 button bits, 3 valuator
//! values) followed — when the device has more — by a
//! `deviceKeyStateNotify` (keys 32..=255) and one `deviceValuator` per
//! 6 remaining axes, every event but the last carrying
//! `XI1_MORE_EVENTS` in its deviceid byte.

use yserver_protocol::x11::{ClientId, ResourceId};

use crate::{
    core_loop::fanout::fanout_event_to_clients,
    server::{ServerState, Xi1DeviceInputState},
};

/// Class bits for `classes_reported` (XI.h KeyClass=0 / ButtonClass=1 /
/// ValuatorClass=2; XIproto.h `ModeBitsShift` = 6).
const KEY_CLASS_BIT: u8 = 1 << 0;
const BUTTON_CLASS_BIT: u8 = 1 << 1;
const VALUATOR_CLASS_BIT: u8 = 1 << 2;
const MODE_BITS_SHIFT: u8 = 6;

/// Device shape reported by DeviceStateNotify. MUST stay in sync with
/// the XI1 ListInputDevices reply (`encode_list_input_devices_reply`:
/// keyboards report keycodes 8..=255, pointers 7 buttons + 4 relative
/// axes) and the XIQueryDevice class list — clients cross-check them.
const KEY_MIN: u8 = 8;
const KEY_MAX: u8 = 255;
const NUM_BUTTONS: u8 = 7;
const NUM_AXES: u8 = 4;

/// Deliver the device-state snapshot for `deviceid` to every client
/// that selected its `DeviceStateNotify` class on `window` via
/// SelectExtensionEvent. Called for each window a DeviceFocusIn lands
/// on (Xorg `DeviceFocusEvent` tail, dix/enterleave.c:835-836) —
/// independent of whether anyone subscribed to the focus event itself.
pub fn deliver_state_notify(state: &mut ServerState, deviceid: u16, window: ResourceId) {
    let event_type = crate::server::XI_FIRST_EVENT + crate::xinput::XI_DEVICE_STATE_NOTIFY_OFFSET;
    let class = (u32::from(deviceid) << 8) | u32::from(event_type);
    let targets: Vec<ClientId> = state
        .clients
        .iter()
        .filter(|(_, c)| {
            c.xi1_window_event_classes
                .get(&window)
                .is_some_and(|set| set.contains(&class))
        })
        .map(|(id, _)| ClientId(*id))
        .collect();
    if targets.is_empty() {
        return;
    }

    let dev_state = state
        .xi1_device_input_state
        .get(&deviceid)
        .copied()
        .unwrap_or_default();
    let has_keys = crate::core_loop::process_request::xi1_device_has_keys(deviceid);
    let has_buttons = crate::core_loop::process_request::xi1_device_has_buttons(deviceid);
    let has_valuators = crate::core_loop::process_request::xi1_device_has_valuators(deviceid);
    // Stored axis values (Xorg `axisVal`): real motion keeps axes 0/1
    // at the sprite position, device-motion fakes write their payload.
    let axes = dev_state.valuators;
    let time = state.timestamp_now();
    log::debug!(
        "xi1_state_notify: device {deviceid} window=0x{:x} -> {} client(s)",
        window.0,
        targets.len(),
    );
    let _dropped = fanout_event_to_clients(state, &targets, |buf, seq, order| {
        encode_state_notify_chain(
            buf,
            order,
            seq,
            deviceid,
            time,
            &dev_state,
            has_keys,
            has_buttons,
            has_valuators,
            &axes,
        );
    });
}

/// Encode the full wire chain for one client (Xorg
/// `DeliverStateNotifyEvent`'s `sev[]` assembly). Every event carries
/// the same sequence number; every event but the last sets
/// `XI1_MORE_EVENTS` on its deviceid byte.
#[allow(clippy::too_many_arguments, clippy::fn_params_excessive_bools)]
fn encode_state_notify_chain(
    buf: &mut Vec<u8>,
    order: yserver_protocol::x11::ClientByteOrder,
    seq: yserver_protocol::x11::SequenceNumber,
    deviceid: u16,
    time: u32,
    dev_state: &Xi1DeviceInputState,
    has_keys: bool,
    has_buttons: bool,
    has_valuators: bool,
    axes: &[i32; 4],
) {
    let first = crate::server::XI_FIRST_EVENT;
    #[allow(clippy::cast_possible_truncation)]
    let dev_byte = deviceid as u8;

    // Continuations needed (FixDeviceStateNotify counting,
    // dix/enterleave.c:712-730): keys beyond the first 32 keycode bits
    // ride in a deviceKeyStateNotify; axes beyond the first 3 in
    // deviceValuator events. 7 buttons fit the leading event.
    let num_keys: u8 = if has_keys { KEY_MAX - KEY_MIN } else { 0 };
    let more_keys = has_keys && num_keys > 32;
    let more_valuators = has_valuators && NUM_AXES > 3;

    let mut classes_reported = 0u8;
    if has_keys {
        classes_reported |= KEY_CLASS_BIT;
    }
    if has_buttons {
        classes_reported |= BUTTON_CLASS_BIT;
    }
    if has_valuators {
        classes_reported |= VALUATOR_CLASS_BIT;
        classes_reported |= dev_state.valuator_mode << MODE_BITS_SHIFT;
    }

    let lead_dev_byte = if more_keys || more_valuators {
        dev_byte | crate::xinput::XI1_MORE_EVENTS
    } else {
        dev_byte
    };
    crate::xinput::encode_xi1_device_state_notify(
        buf,
        order,
        first + crate::xinput::XI_DEVICE_STATE_NOTIFY_OFFSET,
        lead_dev_byte,
        seq,
        time,
        num_keys,
        if has_buttons { NUM_BUTTONS } else { 0 },
        if has_valuators { NUM_AXES.min(3) } else { 0 },
        classes_reported,
        dev_state.buttons_down[..4].try_into().expect("4 bytes"),
        dev_state.keys_down[..4].try_into().expect("4 bytes"),
        [axes[0], axes[1], axes[2]],
    );

    if more_keys {
        let key_dev_byte = if more_valuators {
            dev_byte | crate::xinput::XI1_MORE_EVENTS
        } else {
            dev_byte
        };
        crate::xinput::encode_xi1_device_key_state_notify(
            buf,
            order,
            first + crate::xinput::XI_DEVICE_KEY_STATE_NOTIFY_OFFSET,
            key_dev_byte,
            seq,
            dev_state.keys_down[4..32].try_into().expect("28 bytes"),
        );
    }

    if more_valuators {
        // 4 axes → one continuation with the single remaining axis.
        crate::xinput::encode_xi1_device_valuator(
            buf,
            order,
            first + crate::xinput::XI_DEVICE_VALUATOR_OFFSET,
            dev_byte,
            seq,
            0, // device_state: libXi ignores it on this path
            NUM_AXES - 3,
            3,
            [axes[3], 0, 0, 0, 0, 0],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{resources::ROOT_WINDOW, server::ClientState};
    use std::{
        collections::{HashMap, HashSet, VecDeque},
        io::Read,
        os::unix::net::UnixStream,
        sync::{Arc, Mutex, atomic::AtomicU16},
    };
    use yserver_protocol::x11::{ClientByteOrder, CreateWindowRequest};

    const POINTER: u16 = crate::xinput::DEVICEID_SLAVE_POINTER;
    const KEYBOARD: u16 = crate::xinput::DEVICEID_SLAVE_KEYBOARD;

    // Duplicated from xi1_focus.rs::tests (shared test_fixtures module
    // is a tracked follow-up).
    fn install_client(state: &mut ServerState, id: u32) -> UnixStream {
        let (a, b) = UnixStream::pair().unwrap();
        state.clients.insert(
            id,
            ClientState {
                writer: Arc::new(Mutex::new(crate::transport::Transport::Unix(a))),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0)),
                resource_id_base: 0,
                resource_id_mask: u32::MAX,
                event_masks: HashMap::new(),
                save_set: HashSet::new(),
                big_requests_enabled: false,
                xi2_masks: HashMap::new(),
                xi1_event_classes: HashSet::new(),
                xi1_window_event_classes: HashMap::new(),
                outbound: VecDeque::new(),
                watching_writable: false,
                focused_window: ROOT_WINDOW,
                reader_control: None,
            },
        );
        b
    }

    fn read_all_available(peer: &mut UnixStream) -> Vec<u8> {
        peer.set_nonblocking(true).expect("set_nonblocking");
        let mut out = Vec::new();
        let mut buf = [0u8; 512];
        loop {
            match peer.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(err) => panic!("read failed: {err}"),
            }
        }
        peer.set_nonblocking(false).expect("unset_nonblocking");
        out
    }

    fn make_window(state: &mut ServerState, id: u32) -> ResourceId {
        let rid = ResourceId(id);
        state.resources.create_window(
            ClientId(1),
            CreateWindowRequest {
                depth: 24,
                window: rid,
                parent: ROOT_WINDOW,
                x: 50,
                y: 50,
                width: 30,
                height: 30,
                border_width: 0,
                class: 1,
                visual: crate::resources::ROOT_VISUAL,
                ..Default::default()
            },
        );
        let _ = state.resources.map_window(rid);
        rid
    }

    /// Subscribe client 1 to DeviceStateNotify for `dev` on `window`.
    fn select_state_notify(state: &mut ServerState, window: ResourceId, dev: u16) {
        let class = (u32::from(dev) << 8)
            | u32::from(
                crate::server::XI_FIRST_EVENT + crate::xinput::XI_DEVICE_STATE_NOTIFY_OFFSET,
            );
        state
            .clients
            .get_mut(&1)
            .unwrap()
            .xi1_window_event_classes
            .entry(window)
            .or_default()
            .insert(class);
    }

    #[test]
    fn pointer_chain_is_state_notify_plus_valuator() {
        let mut state = ServerState::new();
        let mut peer = install_client(&mut state, 1);
        let w = make_window(&mut state, 0x0040_0001);
        select_state_notify(&mut state, w, POINTER);
        // Buttons 1 and 3 down; axes 0/1 as if real motion landed at
        // (55, 66).
        let entry = state.xi1_device_input_state.entry(POINTER).or_default();
        entry.buttons_down[0] = 0b0000_1010;
        entry.valuators = [55, 66, 0, 0];

        deliver_state_notify(&mut state, POINTER, w);
        let bytes = read_all_available(&mut peer);
        assert_eq!(bytes.len(), 64, "deviceStateNotify + deviceValuator");

        let first = crate::server::XI_FIRST_EVENT;
        let sn = &bytes[..32];
        assert_eq!(sn[0], first + crate::xinput::XI_DEVICE_STATE_NOTIFY_OFFSET);
        assert_eq!(sn[1], 4 | crate::xinput::XI1_MORE_EVENTS);
        assert_eq!(sn[8], 0, "num_keys");
        assert_eq!(sn[9], 7, "num_buttons");
        assert_eq!(sn[10], 3, "num_valuators (first 3 of 4)");
        assert_eq!(sn[11], 0b0000_0110, "Button+Valuator classes, Relative");
        assert_eq!(sn[12], 0b0000_1010, "buttons 1+3 down");
        assert_eq!(&sn[16..20], &[0u8; 4], "no key bits");
        assert_eq!(i32::from_le_bytes(sn[20..24].try_into().unwrap()), 55);
        assert_eq!(i32::from_le_bytes(sn[24..28].try_into().unwrap()), 66);

        let dv = &bytes[32..];
        assert_eq!(dv[0], first + crate::xinput::XI_DEVICE_VALUATOR_OFFSET);
        assert_eq!(dv[1], 4, "last event: no MORE_EVENTS");
        assert_eq!(dv[6], 1, "one remaining valuator");
        assert_eq!(dv[7], 3, "first_valuator");
        assert_eq!(i32::from_le_bytes(dv[8..12].try_into().unwrap()), 0);
    }

    #[test]
    fn keyboard_chain_is_state_notify_plus_key_state() {
        let mut state = ServerState::new();
        let mut peer = install_client(&mut state, 1);
        let w = make_window(&mut state, 0x0040_0011);
        select_state_notify(&mut state, w, KEYBOARD);
        // Keycodes 9 and 40 down: bit 9 lands in the leading event's
        // 4 key bytes, bit 40 in the continuation (40-32=8 → byte 1).
        let entry = state.xi1_device_input_state.entry(KEYBOARD).or_default();
        entry.keys_down[1] = 0b0000_0010; // keycode 9
        entry.keys_down[5] = 0b0000_0001; // keycode 40

        deliver_state_notify(&mut state, KEYBOARD, w);
        let bytes = read_all_available(&mut peer);
        assert_eq!(bytes.len(), 64, "deviceStateNotify + deviceKeyStateNotify");

        let first = crate::server::XI_FIRST_EVENT;
        let sn = &bytes[..32];
        assert_eq!(sn[0], first + crate::xinput::XI_DEVICE_STATE_NOTIFY_OFFSET);
        assert_eq!(sn[1], 5 | crate::xinput::XI1_MORE_EVENTS);
        assert_eq!(sn[8], 247, "num_keys = max-min (Xorg parity)");
        assert_eq!(sn[9], 0, "no buttons");
        assert_eq!(sn[10], 0, "no valuators");
        assert_eq!(sn[11], 0b0000_0001, "KeyClass only");
        assert_eq!(sn[17], 0b0000_0010, "keycode 9 bit");

        let ks = &bytes[32..];
        assert_eq!(
            ks[0],
            first + crate::xinput::XI_DEVICE_KEY_STATE_NOTIFY_OFFSET
        );
        assert_eq!(ks[1], 5, "last event: no MORE_EVENTS");
        assert_eq!(ks[5], 0b0000_0001, "keycode 40 bit (byte 4+1)");
    }

    #[test]
    fn no_selection_no_events() {
        let mut state = ServerState::new();
        let mut peer = install_client(&mut state, 1);
        let w = make_window(&mut state, 0x0040_0021);
        deliver_state_notify(&mut state, POINTER, w);
        assert!(read_all_available(&mut peer).is_empty());
    }

    #[test]
    fn absolute_mode_sets_mode_bits() {
        let mut state = ServerState::new();
        let mut peer = install_client(&mut state, 1);
        let w = make_window(&mut state, 0x0040_0031);
        select_state_notify(&mut state, w, POINTER);
        state
            .xi1_device_input_state
            .entry(POINTER)
            .or_default()
            .valuator_mode = 1; // Absolute (SetDeviceMode)

        deliver_state_notify(&mut state, POINTER, w);
        let bytes = read_all_available(&mut peer);
        assert_eq!(
            bytes[11], 0b0100_0110,
            "Button+Valuator classes, Absolute at ModeBitsShift"
        );
    }
}
