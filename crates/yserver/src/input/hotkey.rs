//! Server-internal hotkey detection on raw evdev keycodes, before XKB
//! translation. Shared by the Direct-mode input thread and the
//! Libseat-mode on-core libinput dispatch.

use crate::input::InputEvent;

// Linux evdev keycodes (raw, before the X11 +8 translation).
pub(crate) const LINUX_KEY_ENTER: u32 = 28;
pub(crate) const LINUX_KEY_BACKSPACE: u32 = 14;
pub(crate) const LINUX_KEY_LEFTCTRL: u32 = 29;
pub(crate) const LINUX_KEY_LEFTALT: u32 = 56;
pub(crate) const LINUX_KEY_RIGHTCTRL: u32 = 97;
pub(crate) const LINUX_KEY_RIGHTALT: u32 = 100;
pub(crate) const LINUX_KEY_D: u32 = 32;
// F1..F10 are contiguous 59..=68. F11=87, F12=88.
const LINUX_KEY_F1: u32 = 59;
const LINUX_KEY_F10: u32 = 68;
const LINUX_KEY_F11: u32 = 87;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hotkey {
    /// Ctrl+Alt+Backspace — emergency shutdown.
    Zap,
    /// Ctrl+Alt+Enter — diagnostic scanout dump (SIGUSR1 path).
    DumpScanout,
    /// Ctrl+Alt+D — diagnostic per-drawable storage dump (SIGUSR2).
    DumpDrawables,
    /// Ctrl+Alt+F<N> — VT switch to VT N (1-based). F12 is still a VT
    /// key, so SwitchVt covers F1..F11 → VT1..VT11.
    SwitchVt(u32),
}

/// Tracks Ctrl/Alt held state off the raw kernel scancodes and matches
/// the fixed hotkey combos. Off-X-side on purpose: a grabbing client or
/// remapped keymap must not be able to swallow zap or the VT switch.
#[derive(Debug, Clone, Copy, Default)]
pub struct HotkeyDetector {
    ctrl_pressed: bool,
    alt_pressed: bool,
}

impl HotkeyDetector {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Clear tracked modifier state. Called across a VT-switch
    /// suspend/resume: the modifier *release* events land on whatever
    /// owns the keyboard after the switch, not us, so without this the
    /// flags stay stuck-`true` and a bare `F<n>` would fire `SwitchVt`
    /// (observed: at the greeter, plain F6 → TTY6). Mirrors Xorg's
    /// VT-enter "forget held keys" resync.
    pub fn reset(&mut self) {
        self.ctrl_pressed = false;
        self.alt_pressed = false;
    }

    /// Update modifier state for `ev`; return the hotkey it fires, if any.
    /// Only key *presses* fire; releases just update modifier state.
    pub fn check(&mut self, ev: &InputEvent) -> Option<Hotkey> {
        match *ev {
            InputEvent::KeyPress { keycode } => match keycode {
                LINUX_KEY_LEFTCTRL | LINUX_KEY_RIGHTCTRL => {
                    self.ctrl_pressed = true;
                    None
                }
                LINUX_KEY_LEFTALT | LINUX_KEY_RIGHTALT => {
                    self.alt_pressed = true;
                    None
                }
                _ if !(self.ctrl_pressed && self.alt_pressed) => None,
                LINUX_KEY_BACKSPACE => Some(Hotkey::Zap),
                LINUX_KEY_D => Some(Hotkey::DumpDrawables),
                LINUX_KEY_ENTER => Some(Hotkey::DumpScanout),
                LINUX_KEY_F1..=LINUX_KEY_F10 => Some(Hotkey::SwitchVt(keycode - LINUX_KEY_F1 + 1)),
                LINUX_KEY_F11 => Some(Hotkey::SwitchVt(11)),
                _ => None,
            },
            InputEvent::KeyRelease { keycode } => {
                match keycode {
                    LINUX_KEY_LEFTCTRL | LINUX_KEY_RIGHTCTRL => self.ctrl_pressed = false,
                    LINUX_KEY_LEFTALT | LINUX_KEY_RIGHTALT => self.alt_pressed = false,
                    _ => {}
                }
                None
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(d: &mut HotkeyDetector, kc: u32) -> Option<Hotkey> {
        d.check(&InputEvent::KeyPress { keycode: kc })
    }
    fn release(d: &mut HotkeyDetector, kc: u32) {
        d.check(&InputEvent::KeyRelease { keycode: kc });
    }

    #[test]
    fn ctrl_alt_f2_switches_to_vt2() {
        let mut d = HotkeyDetector::new();
        assert_eq!(press(&mut d, LINUX_KEY_LEFTCTRL), None);
        assert_eq!(press(&mut d, LINUX_KEY_LEFTALT), None);
        assert_eq!(press(&mut d, 60 /* F2 */), Some(Hotkey::SwitchVt(2)));
    }

    #[test]
    fn ctrl_alt_f1_switches_to_vt1() {
        let mut d = HotkeyDetector::new();
        press(&mut d, LINUX_KEY_RIGHTCTRL);
        press(&mut d, LINUX_KEY_RIGHTALT);
        assert_eq!(press(&mut d, LINUX_KEY_F1), Some(Hotkey::SwitchVt(1)));
    }

    #[test]
    fn f_keys_without_modifiers_do_not_switch() {
        let mut d = HotkeyDetector::new();
        assert_eq!(press(&mut d, 60), None);
    }

    #[test]
    fn releasing_a_modifier_disarms() {
        let mut d = HotkeyDetector::new();
        press(&mut d, LINUX_KEY_LEFTCTRL);
        press(&mut d, LINUX_KEY_LEFTALT);
        release(&mut d, LINUX_KEY_LEFTALT);
        assert_eq!(press(&mut d, 60), None);
    }

    #[test]
    fn reset_clears_stuck_modifiers_after_vt_switch() {
        // Ctrl+Alt held when a VT switch fires; the releases land on the
        // VT we switched to, so the detector never sees them. Without
        // reset() on the suspend/resume boundary, a bare F6 would fire
        // SwitchVt(6) (observed: at the greeter, plain F6 → TTY6).
        let mut d = HotkeyDetector::new();
        press(&mut d, LINUX_KEY_LEFTCTRL);
        press(&mut d, LINUX_KEY_LEFTALT);
        d.reset();
        assert_eq!(press(&mut d, 64 /* F6 */), None);
    }

    #[test]
    fn zap_and_dumps_still_fire() {
        let mut d = HotkeyDetector::new();
        press(&mut d, LINUX_KEY_LEFTCTRL);
        press(&mut d, LINUX_KEY_LEFTALT);
        assert_eq!(press(&mut d, LINUX_KEY_BACKSPACE), Some(Hotkey::Zap));
        assert_eq!(press(&mut d, LINUX_KEY_D), Some(Hotkey::DumpDrawables));
        assert_eq!(press(&mut d, LINUX_KEY_ENTER), Some(Hotkey::DumpScanout));
    }

    #[test]
    fn ctrl_alt_d_is_dump_not_text() {
        let mut d = HotkeyDetector::new();
        press(&mut d, LINUX_KEY_LEFTCTRL);
        press(&mut d, LINUX_KEY_LEFTALT);
        assert_eq!(press(&mut d, LINUX_KEY_D), Some(Hotkey::DumpDrawables));
    }
}
