//! Window-system key and button events to host input: the keys and buttons
//! reported down, modifier state from device bits, Caps Lock toggles,
//! dropped autorepeat and the releases on focus loss.

use yserver_core::{core_loop::HostInputEvent, host_x11::HostKeyEvent};

use super::{
    devices::{keyboard_origin, pointer_origin},
    keymap::{ev, kvk, x_keycode, x_keycode_of_evdev},
};

/// Device-dependent `modifierFlags` bits, IOKit `hidsystem/IOLLEvent.h`
/// `NX_DEVICE*KEYMASK` (not cross-checked: await the SDK header).
mod nx {
    pub(super) const NX_DEVICELCTLKEYMASK: u64 = 0x0000_0001;
    pub(super) const NX_DEVICELSHIFTKEYMASK: u64 = 0x0000_0002;
    pub(super) const NX_DEVICERSHIFTKEYMASK: u64 = 0x0000_0004;
    pub(super) const NX_DEVICELCMDKEYMASK: u64 = 0x0000_0008;
    pub(super) const NX_DEVICERCMDKEYMASK: u64 = 0x0000_0010;
    pub(super) const NX_DEVICELALTKEYMASK: u64 = 0x0000_0020;
    pub(super) const NX_DEVICERALTKEYMASK: u64 = 0x0000_0040;
    pub(super) const NX_DEVICERCTLKEYMASK: u64 = 0x0000_2000;
}

/// Device-independent `NSEventModifierFlags` (`NSEvent.h`; the Rust
/// app-kit bindings 0.3.2, `generated/NSEvent.rs:389–397`).
const FLAG_CAPS_LOCK: u64 = 1 << 16;
const FLAG_SHIFT: u64 = 1 << 17;
const FLAG_CONTROL: u64 = 1 << 18;
const FLAG_OPTION: u64 = 1 << 19;
const FLAG_COMMAND: u64 = 1 << 20;

/// `(kVK, device bit, device-independent bit)` of each sided modifier.
const MODIFIERS: [(u16, u64, u64); 8] = [
    (kvk::kVK_Shift, nx::NX_DEVICELSHIFTKEYMASK, FLAG_SHIFT),
    (kvk::kVK_RightShift, nx::NX_DEVICERSHIFTKEYMASK, FLAG_SHIFT),
    (kvk::kVK_Control, nx::NX_DEVICELCTLKEYMASK, FLAG_CONTROL),
    (
        kvk::kVK_RightControl,
        nx::NX_DEVICERCTLKEYMASK,
        FLAG_CONTROL,
    ),
    (kvk::kVK_Option, nx::NX_DEVICELALTKEYMASK, FLAG_OPTION),
    (kvk::kVK_RightOption, nx::NX_DEVICERALTKEYMASK, FLAG_OPTION),
    (kvk::kVK_Command, nx::NX_DEVICELCMDKEYMASK, FLAG_COMMAND),
    (
        kvk::kVK_RightCommand,
        nx::NX_DEVICERCMDKEYMASK,
        FLAG_COMMAND,
    ),
];

/// Linux `BTN_*` (`input-event-codes.h:357–361`).
const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;
const BTN_SIDE: u16 = 0x113;
const BTN_EXTRA: u16 = 0x114;

/// What a key event reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeyKind {
    Down,
    Up,
    /// A modifier or Caps Lock changed; `flags` says to what.
    FlagsChanged,
}

/// A window-system key event.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RawKey {
    pub(crate) kvk: u16,
    pub(crate) kind: KeyKind,
    /// `modifierFlags` after the event.
    pub(crate) flags: u64,
    /// The window system's autorepeat.
    pub(crate) is_repeat: bool,
}

/// The keys and buttons reported down, in press order.
#[derive(Debug, Default)]
pub(crate) struct Translator {
    iso: bool,
    keys: Vec<u8>,
    buttons: Vec<u16>,
    /// The button an Option- or Command-click stands for, while down.
    fake: Option<u16>,
}

impl Translator {
    /// `iso`: an ISO keyboard (see [`super::keymap::evdev_code`]).
    pub(crate) fn new(iso: bool) -> Self {
        Self {
            iso,
            ..Self::default()
        }
    }

    /// Translate one key event.
    pub(crate) fn key(&mut self, raw: RawKey, time: u32, out: &mut Vec<HostInputEvent>) {
        match raw.kind {
            // The core repeats held keys itself, at the XKB rate.
            KeyKind::Down if raw.is_repeat => {}
            KeyKind::Down => {
                if let Some(keycode) = x_keycode(raw.kvk, self.iso) {
                    self.press(keycode, time, out);
                }
            }
            KeyKind::Up => {
                if let Some(keycode) = x_keycode(raw.kvk, self.iso) {
                    self.release(keycode, time, out);
                }
            }
            KeyKind::FlagsChanged => self.flags_changed(raw, time, out),
        }
    }

    fn flags_changed(&mut self, raw: RawKey, time: u32, out: &mut Vec<HostInputEvent>) {
        if raw.kvk == kvk::kVK_CapsLock {
            // One event per lock change, none on release: each toggles X once.
            let caps = x_keycode_of_evdev(ev::KEY_CAPSLOCK);
            out.push(key_event(caps, true, time));
            out.push(key_event(caps, false, time));
            return;
        }
        let Some(&(_, device_bit, group_bit)) =
            MODIFIERS.iter().find(|&&(mac, _, _)| mac == raw.kvk)
        else {
            return;
        };
        let Some(keycode) = x_keycode(raw.kvk, self.iso) else {
            return;
        };
        let down = raw.flags & group_bit != 0 && raw.flags & device_bit != 0;
        if down {
            self.press(keycode, time, out);
        } else {
            self.release(keycode, time, out);
        }
    }

    fn press(&mut self, keycode: u8, time: u32, out: &mut Vec<HostInputEvent>) {
        if !self.keys.contains(&keycode) {
            self.keys.push(keycode);
            out.push(key_event(keycode, true, time));
        }
    }

    /// A release of a key not reported down (Cmd after Cmd-Tab back) is dropped.
    fn release(&mut self, keycode: u8, time: u32, out: &mut Vec<HostInputEvent>) {
        if let Some(at) = self.keys.iter().position(|&held| held == keycode) {
            self.keys.remove(at);
            out.push(key_event(keycode, false, time));
        }
    }

    /// Translate a mouse button: `number` is the window system's 0-based
    /// button number (0 left, 1 right, 2 middle, 3 back, 4 forward);
    /// `flags` the `modifierFlags` at the event; `emulate`: Option- and
    /// Command-clicks are buttons 2 and 3 (the setting, read per press).
    pub(crate) fn button(
        &mut self,
        number: i64,
        pressed: bool,
        flags: u64,
        emulate: bool,
        time: u32,
        out: &mut Vec<HostInputEvent>,
    ) {
        if number == 0
            && let Some(button) = self.fake_button(pressed, flags, emulate, time, out)
        {
            self.change_button(button, pressed, time, out);
            return;
        }
        let button = match number {
            0 => BTN_LEFT,
            1 => BTN_RIGHT,
            2 => BTN_MIDDLE,
            3 => BTN_SIDE,
            4 => BTN_EXTRA,
            _ => return,
        };
        self.change_button(button, pressed, time, out);
    }

    fn change_button(
        &mut self,
        button: u16,
        pressed: bool,
        time: u32,
        out: &mut Vec<HostInputEvent>,
    ) {
        let held = self.buttons.iter().position(|&b| b == button);
        match (pressed, held) {
            (true, None) => self.buttons.push(button),
            (false, Some(at)) => {
                self.buttons.remove(at);
            }
            _ => return,
        }
        out.push(button_event(button, pressed, time));
    }

    /// XQuartz's fake buttons (`darwinEvents.c:503-541`, masks `darwin.c:137-138`):
    /// a primary click that starts with Option held is button 2, with
    /// Command button 3; the modifier is released in X first. The release
    /// matches the press, whatever the modifiers or `emulate` are by then.
    fn fake_button(
        &mut self,
        pressed: bool,
        flags: u64,
        emulate: bool,
        time: u32,
        out: &mut Vec<HostInputEvent>,
    ) -> Option<u16> {
        if !pressed {
            return self.fake.take();
        }
        if let Some(stale) = self.fake.take() {
            self.change_button(stale, false, time, out);
        }
        if !emulate {
            return None;
        }
        let (button, group) = if flags & FLAG_OPTION != 0 {
            (BTN_MIDDLE, FLAG_OPTION)
        } else if flags & FLAG_COMMAND != 0 {
            (BTN_RIGHT, FLAG_COMMAND)
        } else {
            return None;
        };
        for &(kvk, _, bit) in &MODIFIERS {
            if bit == group
                && let Some(keycode) = x_keycode(kvk, self.iso)
            {
                self.release(keycode, time, out);
            }
        }
        self.fake = Some(button);
        Some(button)
    }

    /// The window lost the keyboard: release every held key, last pressed
    /// first, then every held button.
    pub(crate) fn release_all(&mut self, time: u32, out: &mut Vec<HostInputEvent>) {
        while let Some(keycode) = self.keys.pop() {
            out.push(key_event(keycode, false, time));
        }
        while let Some(button) = self.buttons.pop() {
            out.push(button_event(button, false, time));
        }
        self.fake = None;
    }
}

/// Route a key event: with `key_equivalents`, a Command key-down is
/// offered once to `menu` (the app menu, which runs a matching item);
/// whether the event then goes to X: not when the menu took it.
pub(crate) fn key_goes_to_x(
    raw: &RawKey,
    key_equivalents: bool,
    menu: impl FnOnce() -> bool,
) -> bool {
    let offered = key_equivalents && raw.kind == KeyKind::Down && raw.flags & FLAG_COMMAND != 0;
    !(offered && menu())
}

/// Whether `modifierFlags` has Caps Lock on.
pub(crate) fn caps_locked(flags: u64) -> bool {
    flags & FLAG_CAPS_LOCK != 0
}

/// The Caps Lock press+release that brings the X lock (`x_locked`) to the
/// window system's (`host_locked`), or nothing when they agree.
pub(crate) fn caps_lock_sync(x_locked: bool, host_locked: bool, time: u32) -> Vec<HostInputEvent> {
    if x_locked == host_locked {
        return Vec::new();
    }
    let caps = x_keycode_of_evdev(ev::KEY_CAPSLOCK);
    vec![key_event(caps, true, time), key_event(caps, false, time)]
}

/// A key event from the keyboard source; the backend cooks the state.
pub(crate) fn key_event(keycode: u8, pressed: bool, time: u32) -> HostInputEvent {
    HostInputEvent::Key(HostKeyEvent {
        origin: keyboard_origin(),
        pressed,
        keycode,
        time,
        root_x: 0,
        root_y: 0,
        event_x: 0,
        event_y: 0,
        state: 0,
    })
}

fn button_event(button: u16, pressed: bool, time: u32) -> HostInputEvent {
    HostInputEvent::PointerButton {
        origin: pointer_origin(),
        button,
        pressed,
        time,
    }
}

#[cfg(test)]
mod tests {
    use yserver_core::core_loop::HostInputEvent;

    use super::{
        FLAG_CAPS_LOCK, FLAG_COMMAND, FLAG_OPTION, FLAG_SHIFT, KeyKind, RawKey, Translator,
        caps_lock_sync, caps_locked, key_goes_to_x, nx,
    };
    use crate::surface::input::keymap::kvk;

    /// `(keycode, pressed)` of key events, `(256 + button, pressed)` of buttons.
    fn summary(events: &[HostInputEvent]) -> Vec<(u32, bool)> {
        events
            .iter()
            .map(|ev| match ev {
                HostInputEvent::Key(key) => (u32::from(key.keycode), key.pressed),
                HostInputEvent::PointerButton {
                    button, pressed, ..
                } => (256 + u32::from(*button), *pressed),
                other => panic!("unexpected {other:?}"),
            })
            .collect()
    }

    fn run(t: &mut Translator, keys: &[(u16, KeyKind, u64, bool)]) -> Vec<(u32, bool)> {
        let mut out = Vec::new();
        for &(kvk, kind, flags, is_repeat) in keys {
            t.key(
                RawKey {
                    kvk,
                    kind,
                    flags,
                    is_repeat,
                },
                0,
                &mut out,
            );
        }
        summary(&out)
    }

    const SHIFT_L: u64 = FLAG_SHIFT | nx::NX_DEVICELSHIFTKEYMASK;
    const SHIFT_R: u64 = FLAG_SHIFT | nx::NX_DEVICERSHIFTKEYMASK;
    const CMD_L: u64 = FLAG_COMMAND | nx::NX_DEVICELCMDKEYMASK;

    #[test]
    fn shift_sides_press_and_release_from_their_device_bits() {
        use KeyKind::FlagsChanged as F;
        let mut t = Translator::new(false);
        let events = run(
            &mut t,
            &[
                (kvk::kVK_Shift, F, SHIFT_L, false),
                (kvk::kVK_RightShift, F, SHIFT_L | SHIFT_R, false),
                (kvk::kVK_Shift, F, SHIFT_R, false),
                (kvk::kVK_RightShift, F, 0, false),
            ],
        );
        assert_eq!(events, [(50, true), (62, true), (50, false), (62, false)]);
    }

    #[test]
    fn a_lost_modifier_event_cannot_invert_the_state() {
        use KeyKind::FlagsChanged as F;
        let mut t = Translator::new(false);
        let events = run(
            &mut t,
            &[
                (kvk::kVK_Shift, F, SHIFT_L, false),
                (kvk::kVK_Shift, F, SHIFT_L, false),
                (kvk::kVK_Shift, F, 0, false),
                (kvk::kVK_Shift, F, 0, false),
            ],
        );
        assert_eq!(events, [(50, true), (50, false)]);
    }

    #[test]
    fn each_caps_lock_change_is_one_press_and_release() {
        use KeyKind::FlagsChanged as F;
        let mut t = Translator::new(false);
        let events = run(
            &mut t,
            &[
                (kvk::kVK_CapsLock, F, FLAG_CAPS_LOCK, false),
                (kvk::kVK_CapsLock, F, 0, false),
            ],
        );
        assert_eq!(events, [(66, true), (66, false), (66, true), (66, false)]);
    }

    #[test]
    fn autorepeat_is_dropped() {
        use KeyKind::{Down, Up};
        let mut t = Translator::new(false);
        let events = run(
            &mut t,
            &[
                (kvk::kVK_ANSI_A, Down, 0, false),
                (kvk::kVK_ANSI_A, Down, 0, true),
                (kvk::kVK_ANSI_A, Down, 0, true),
                (kvk::kVK_ANSI_A, Up, 0, false),
            ],
        );
        assert_eq!(events, [(38, true), (38, false)]);
    }

    #[test]
    fn autorepeat_after_a_focus_loss_does_not_press_again() {
        use KeyKind::Down;
        let mut t = Translator::new(false);
        run(&mut t, &[(kvk::kVK_ANSI_A, Down, 0, false)]);
        let mut out = Vec::new();
        t.release_all(0, &mut out);
        let events = run(&mut t, &[(kvk::kVK_ANSI_A, Down, 0, true)]);
        assert!(events.is_empty(), "the held key stays released: {events:?}");
    }

    #[test]
    fn a_release_of_a_key_never_pressed_is_dropped() {
        let mut t = Translator::new(false);
        let events = run(&mut t, &[(kvk::kVK_ANSI_A, KeyKind::Up, 0, false)]);
        assert!(events.is_empty());
    }

    #[test]
    fn focus_loss_releases_keys_last_first_then_buttons() {
        use KeyKind::{Down, FlagsChanged as F};
        let mut t = Translator::new(false);
        run(
            &mut t,
            &[
                (kvk::kVK_Shift, F, SHIFT_L, false),
                (kvk::kVK_ANSI_A, Down, SHIFT_L, false),
                (kvk::kVK_ANSI_S, Down, SHIFT_L, false),
            ],
        );
        let mut out = Vec::new();
        t.button(0, true, 0, true, 0, &mut out);
        t.button(1, true, 0, true, 0, &mut out);
        out.clear();
        t.release_all(0, &mut out);
        assert_eq!(
            summary(&out),
            [
                (39, false),
                (38, false),
                (50, false),
                (256 + 0x111, false),
                (256 + 0x110, false)
            ]
        );
        out.clear();
        t.release_all(0, &mut out);
        assert!(out.is_empty(), "nothing left held");
    }

    #[test]
    fn cmd_tab_away_and_back_leaves_nothing_held() {
        use KeyKind::{Down, FlagsChanged as F};
        let mut t = Translator::new(false);
        // Cmd down, Tab down; the window resigns key and gets no Tab/Cmd up.
        let mut events = run(
            &mut t,
            &[
                (kvk::kVK_Command, F, CMD_L, false),
                (kvk::kVK_Tab, Down, CMD_L, false),
            ],
        );
        let mut out = Vec::new();
        t.release_all(0, &mut out);
        events.extend(summary(&out));
        // Back again: only Cmd's release arrives.
        events.extend(run(&mut t, &[(kvk::kVK_Command, F, 0, false)]));
        assert_eq!(events, [(133, true), (23, true), (23, false), (133, false)]);
    }

    #[test]
    fn buttons_map_to_linux_codes_and_extra_buttons_are_dropped() {
        let mut t = Translator::new(false);
        let mut out = Vec::new();
        for number in 0..=5 {
            t.button(number, true, 0, true, 0, &mut out);
        }
        assert_eq!(
            summary(&out),
            [
                (256 + 0x110, true),
                (256 + 0x111, true),
                (256 + 0x112, true),
                (256 + 0x113, true),
                (256 + 0x114, true)
            ]
        );
        out.clear();
        t.button(0, true, 0, true, 0, &mut out);
        t.button(5, false, 0, true, 0, &mut out);
        assert!(out.is_empty(), "a held button and button 5 send nothing");
    }

    const OPT_L: u64 = FLAG_OPTION | nx::NX_DEVICELALTKEYMASK;
    const ALT_L: u32 = 64;
    const SUPER_L: u32 = 133;
    const LEFT: u32 = 256 + 0x110;
    const RIGHT: u32 = 256 + 0x111;
    const MIDDLE: u32 = 256 + 0x112;

    fn click(t: &mut Translator, number: i64, pressed: bool, flags: u64) -> Vec<(u32, bool)> {
        click_with(t, number, pressed, flags, true)
    }

    fn click_with(
        t: &mut Translator,
        number: i64,
        pressed: bool,
        flags: u64,
        emulate: bool,
    ) -> Vec<(u32, bool)> {
        let mut out = Vec::new();
        t.button(number, pressed, flags, emulate, 0, &mut out);
        summary(&out)
    }

    /// XQuartz fake buttons (`darwinEvents.c:503-541`, `darwin.c:137-138`):
    /// Option+click is button 2 with Option released in X first.
    #[test]
    fn option_click_is_the_middle_button_without_alt() {
        use KeyKind::FlagsChanged as F;
        let mut t = Translator::new(false);
        assert_eq!(
            run(&mut t, &[(kvk::kVK_Option, F, OPT_L, false)]),
            [(ALT_L, true)]
        );
        assert_eq!(
            click(&mut t, 0, true, OPT_L),
            [(ALT_L, false), (MIDDLE, true)]
        );
        // Option let go before the button: nothing more for Alt.
        assert!(run(&mut t, &[(kvk::kVK_Option, F, 0, false)]).is_empty());
        assert_eq!(
            click(&mut t, 0, false, 0),
            [(MIDDLE, false)],
            "matches the press"
        );
        assert_eq!(
            click(&mut t, 0, true, 0),
            [(LEFT, true)],
            "a plain click again"
        );
    }

    #[test]
    fn command_click_is_the_right_button_without_super() {
        use KeyKind::FlagsChanged as F;
        let mut t = Translator::new(false);
        run(&mut t, &[(kvk::kVK_Command, F, CMD_L, false)]);
        assert_eq!(
            click(&mut t, 0, true, CMD_L),
            [(SUPER_L, false), (RIGHT, true)]
        );
        assert_eq!(click(&mut t, 0, false, CMD_L), [(RIGHT, false)]);
        assert!(
            run(&mut t, &[(kvk::kVK_Command, F, 0, false)]).is_empty(),
            "Super was already released"
        );
    }

    #[test]
    fn emulation_turned_off_while_held_releases_the_emulated_button() {
        let mut t = Translator::new(false);
        assert_eq!(click_with(&mut t, 0, true, CMD_L, true), [(RIGHT, true)]);
        assert_eq!(
            click_with(&mut t, 0, false, CMD_L, false),
            [(RIGHT, false)],
            "matches the press"
        );
        assert_eq!(
            click_with(&mut t, 0, true, CMD_L, false),
            [(LEFT, true)],
            "the next press is not emulated"
        );
        assert_eq!(click_with(&mut t, 0, false, CMD_L, true), [(LEFT, false)]);
    }

    #[test]
    fn only_the_primary_button_is_emulated() {
        let mut t = Translator::new(false);
        assert_eq!(click(&mut t, 1, true, OPT_L), [(RIGHT, true)]);
        assert_eq!(click(&mut t, 1, false, OPT_L), [(RIGHT, false)]);
        // A modifier pressed after the button went down changes nothing.
        assert_eq!(click(&mut t, 0, true, 0), [(LEFT, true)]);
        assert_eq!(click(&mut t, 0, false, OPT_L), [(LEFT, false)]);
    }

    #[test]
    fn focus_loss_releases_an_emulated_button() {
        let mut t = Translator::new(false);
        click(&mut t, 0, true, OPT_L);
        let mut out = Vec::new();
        t.release_all(0, &mut out);
        assert_eq!(summary(&out), [(MIDDLE, false)]);
        assert_eq!(click(&mut t, 0, false, 0), [], "the release was sent");
    }

    fn key(kvk: u16, kind: KeyKind, flags: u64) -> RawKey {
        RawKey {
            kvk,
            kind,
            flags,
            is_repeat: false,
        }
    }

    /// `(goes to X, times the menu was asked)`.
    fn route(raw: &RawKey, key_equivalents: bool, handled: bool) -> (bool, u32) {
        let mut asked = 0;
        let to_x = key_goes_to_x(raw, key_equivalents, || {
            asked += 1;
            handled
        });
        (to_x, asked)
    }

    #[test]
    fn a_handled_command_shortcut_is_dispatched_once_and_not_sent_to_x() {
        let cmd_q = key(kvk::kVK_ANSI_Q, KeyKind::Down, CMD_L);
        assert_eq!(route(&cmd_q, true, true), (false, 1));
    }

    #[test]
    fn unhandled_command_keys_and_other_keys_go_to_x() {
        let cmd_t = key(kvk::kVK_ANSI_T, KeyKind::Down, CMD_L);
        assert_eq!(route(&cmd_t, true, false), (true, 1), "not a menu key");
        let q = key(kvk::kVK_ANSI_Q, KeyKind::Down, 0);
        assert_eq!(route(&q, true, true), (true, 0), "no Command");
        let up = key(kvk::kVK_ANSI_Q, KeyKind::Up, CMD_L);
        assert_eq!(route(&up, true, true), (true, 0), "a release");
        let cmd = key(kvk::kVK_Command, KeyKind::FlagsChanged, CMD_L);
        assert_eq!(route(&cmd, true, true), (true, 0), "the modifier itself");
    }

    #[test]
    fn without_key_equivalents_everything_goes_to_x() {
        let cmd_q = key(kvk::kVK_ANSI_Q, KeyKind::Down, CMD_L);
        assert_eq!(route(&cmd_q, false, true), (true, 0));
    }

    #[test]
    fn caps_lock_is_read_from_the_device_independent_bit() {
        assert!(caps_locked(FLAG_CAPS_LOCK | SHIFT_L));
        assert!(!caps_locked(SHIFT_L));
    }

    #[test]
    fn caps_sync_toggles_only_when_the_locks_differ() {
        assert!(caps_lock_sync(true, true, 0).is_empty());
        assert!(caps_lock_sync(false, false, 0).is_empty());
        assert_eq!(
            summary(&caps_lock_sync(false, true, 0)),
            [(66, true), (66, false)]
        );
        assert_eq!(
            summary(&caps_lock_sync(true, false, 0)),
            [(66, true), (66, false)]
        );
    }
}
