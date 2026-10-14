//! The window system's two input sources: one keyboard and one pointer,
//! published as XI slaves of the core keyboard and pointer.

use yserver_core::{
    core_loop::{DeviceInfo, HostInputEvent, InputOrigin, message::LibinputConfigSnapshot},
    xinput::{InputCapabilities, InputSourceId},
};

use super::{
    keymap::{ev, x_keycode_of_evdev},
    translate::key_event,
};

const KEYBOARD: InputSourceId = InputSourceId(1);
pub(super) const POINTER: InputSourceId = InputSourceId(2);

pub(crate) fn keyboard_origin() -> InputOrigin {
    InputOrigin::Physical(KEYBOARD)
}

pub(crate) fn pointer_origin() -> InputOrigin {
    InputOrigin::Physical(POINTER)
}

#[cfg(test)]
pub(crate) fn keyboard_source() -> InputSourceId {
    KEYBOARD
}

#[cfg(test)]
pub(crate) fn pointer_source() -> InputSourceId {
    POINTER
}

fn device(source_id: InputSourceId, name: &str, keyboard: bool) -> DeviceInfo {
    DeviceInfo {
        source_id,
        enabled: true,
        resume_key: None,
        capabilities: InputCapabilities {
            keyboard,
            pointer: !keyboard,
            touch: false,
        },
        name: name.to_owned(),
        device_node: String::new(),
        sysname: String::new(),
        vendor_id: 0,
        product_id: 0,
        is_touchpad: false,
        config: LibinputConfigSnapshot::default(),
    }
}

/// What a shell sends before any input: both devices, then a Num Lock
/// press+release so the keypad types digits, as on the Mac.
pub(crate) fn announcements(time: u32) -> Vec<HostInputEvent> {
    let num_lock = x_keycode_of_evdev(ev::KEY_NUMLOCK);
    vec![
        HostInputEvent::DeviceAdded(device(KEYBOARD, "macOS keyboard", true)),
        HostInputEvent::DeviceAdded(device(POINTER, "macOS pointer", false)),
        key_event(num_lock, true, time),
        key_event(num_lock, false, time),
    ]
}

#[cfg(test)]
mod tests {
    use yserver_core::core_loop::HostInputEvent;

    use super::{KEYBOARD, POINTER, announcements};

    #[test]
    fn announcements_add_a_keyboard_and_a_pointer_then_lock_num_lock() {
        let events = announcements(7);
        let [
            HostInputEvent::DeviceAdded(keyboard),
            HostInputEvent::DeviceAdded(pointer),
            HostInputEvent::Key(press),
            HostInputEvent::Key(release),
        ] = events.as_slice()
        else {
            panic!("{events:?}");
        };
        assert_eq!(keyboard.source_id, KEYBOARD);
        assert!(keyboard.capabilities.keyboard && !keyboard.capabilities.pointer);
        assert_eq!(pointer.source_id, POINTER);
        assert!(pointer.capabilities.pointer && !pointer.capabilities.keyboard);
        assert!(keyboard.device_node.is_empty() && pointer.device_node.is_empty());
        assert_eq!(
            (press.keycode, press.pressed),
            (77, true),
            "KEY_NUMLOCK + 8"
        );
        assert_eq!((release.keycode, release.pressed), (77, false));
        assert_eq!(press.origin, super::keyboard_origin());
    }
}
