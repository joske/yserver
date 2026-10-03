//! Server-internal hotkey detection on raw evdev keycodes, before XKB
//! translation. Used by the direct-mode input thread.

use crate::input::InputEvent;
use std::collections::{HashMap, HashSet};
use yserver_core::xinput::InputSourceId;

// Linux evdev keycodes (raw, before the X11 +8 translation).
pub(crate) const LINUX_KEY_ENTER: u32 = 28;
pub(crate) const LINUX_KEY_BACKSPACE: u32 = 14;
pub(crate) const LINUX_KEY_LEFTCTRL: u32 = 29;
pub(crate) const LINUX_KEY_LEFTALT: u32 = 56;
pub(crate) const LINUX_KEY_RIGHTCTRL: u32 = 97;
pub(crate) const LINUX_KEY_RIGHTALT: u32 = 100;
// F1..F10 are contiguous 59..=68. F11=87, F12=88.
const LINUX_KEY_F1: u32 = 59;
const LINUX_KEY_F10: u32 = 68;
const LINUX_KEY_F11: u32 = 87;
pub(crate) const LINUX_KEY_F12: u32 = 88;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hotkey {
    /// Ctrl+Alt+Backspace — emergency shutdown.
    Zap,
    /// Ctrl+Alt+Enter — diagnostic scanout dump.
    DumpScanout,
    /// Ctrl+Alt+F12 — diagnostic per-drawable storage dump.
    DumpDrawables,
    /// Ctrl+Alt+F<N> — VT switch to VT N (1-based). SwitchVt covers
    /// F1..F11 → VT1..VT11; F12 is reserved for the drawable dump
    /// (VT12 typically doesn't exist).
    SwitchVt(u32),
}

/// Tracks Ctrl/Alt held state off the raw kernel scancodes and matches
/// the fixed hotkey combos. Off-X-side on purpose: a grabbing client or
/// remapped keymap must not be able to swallow zap or the VT switch.
#[derive(Debug, Clone, Default)]
pub struct HotkeyDetector {
    modifier_keys_by_source: HashMap<InputSourceId, HashSet<u32>>,
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
        self.modifier_keys_by_source.clear();
    }

    /// Update modifier state for `ev`; return the hotkey it fires, if any.
    /// Only key *presses* fire; releases just update modifier state.
    pub fn check(&mut self, ev: &InputEvent) -> Option<Hotkey> {
        match ev {
            InputEvent::KeyPress { source_id, keycode } => {
                match *keycode {
                    LINUX_KEY_LEFTCTRL | LINUX_KEY_RIGHTCTRL | LINUX_KEY_LEFTALT
                    | LINUX_KEY_RIGHTALT => {
                        self.modifier_keys_by_source
                            .entry(*source_id)
                            .or_default()
                            .insert(*keycode);
                        None
                    }
                    _ if !(self.ctrl_pressed() && self.alt_pressed()) => None,
                    LINUX_KEY_BACKSPACE => Some(Hotkey::Zap),
                    LINUX_KEY_ENTER => Some(Hotkey::DumpScanout),
                    LINUX_KEY_F1..=LINUX_KEY_F10 => {
                        Some(Hotkey::SwitchVt(*keycode - LINUX_KEY_F1 + 1))
                    }
                    LINUX_KEY_F11 => Some(Hotkey::SwitchVt(11)),
                    // F12 is not a VT key (VT12 typically doesn't exist), so
                    // it's a free slot for the per-drawable storage dump.
                    LINUX_KEY_F12 => Some(Hotkey::DumpDrawables),
                    _ => None,
                }
            }
            InputEvent::KeyRelease { source_id, keycode } => {
                if let Some(keys) = self.modifier_keys_by_source.get_mut(source_id) {
                    keys.remove(keycode);
                    if keys.is_empty() {
                        self.modifier_keys_by_source.remove(source_id);
                    }
                }
                None
            }
            InputEvent::DeviceSuspended { source_id } | InputEvent::DeviceRemoved { source_id } => {
                self.modifier_keys_by_source.remove(source_id);
                None
            }
            _ => None,
        }
    }

    fn ctrl_pressed(&self) -> bool {
        self.modifier_keys_by_source
            .values()
            .any(|keys| keys.contains(&LINUX_KEY_LEFTCTRL) || keys.contains(&LINUX_KEY_RIGHTCTRL))
    }

    fn alt_pressed(&self) -> bool {
        self.modifier_keys_by_source
            .values()
            .any(|keys| keys.contains(&LINUX_KEY_LEFTALT) || keys.contains(&LINUX_KEY_RIGHTALT))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use yserver_core::xinput::InputSourceId;

    const TEST_SOURCE_ID: InputSourceId = InputSourceId(1);

    fn press(d: &mut HotkeyDetector, kc: u32) -> Option<Hotkey> {
        d.check(&InputEvent::KeyPress {
            source_id: TEST_SOURCE_ID,
            keycode: kc,
        })
    }
    fn release(d: &mut HotkeyDetector, kc: u32) {
        d.check(&InputEvent::KeyRelease {
            source_id: TEST_SOURCE_ID,
            keycode: kc,
        });
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
        assert!(d.modifier_keys_by_source.is_empty());
        assert_eq!(press(&mut d, 64 /* F6 */), None);
    }

    #[test]
    fn zap_and_dumps_still_fire() {
        let mut d = HotkeyDetector::new();
        press(&mut d, LINUX_KEY_LEFTCTRL);
        press(&mut d, LINUX_KEY_LEFTALT);
        assert_eq!(press(&mut d, LINUX_KEY_BACKSPACE), Some(Hotkey::Zap));
        assert_eq!(press(&mut d, LINUX_KEY_F12), Some(Hotkey::DumpDrawables));
        assert_eq!(press(&mut d, LINUX_KEY_ENTER), Some(Hotkey::DumpScanout));
    }

    #[test]
    fn ctrl_alt_f12_is_dump_not_vt_switch() {
        let mut d = HotkeyDetector::new();
        press(&mut d, LINUX_KEY_LEFTCTRL);
        press(&mut d, LINUX_KEY_LEFTALT);
        assert_eq!(press(&mut d, LINUX_KEY_F12), Some(Hotkey::DumpDrawables));
    }

    #[test]
    fn two_physical_sources_keep_independent_ctrl_holders() {
        let mut d = HotkeyDetector::new();
        let source_a = InputSourceId(1);
        let source_b = InputSourceId(2);
        d.check(&InputEvent::KeyPress {
            source_id: source_a,
            keycode: LINUX_KEY_LEFTCTRL,
        });
        d.check(&InputEvent::KeyPress {
            source_id: source_b,
            keycode: LINUX_KEY_LEFTCTRL,
        });
        d.check(&InputEvent::KeyRelease {
            source_id: source_a,
            keycode: LINUX_KEY_LEFTCTRL,
        });
        d.check(&InputEvent::KeyPress {
            source_id: source_b,
            keycode: LINUX_KEY_LEFTALT,
        });
        assert_eq!(
            d.check(&InputEvent::KeyPress {
                source_id: source_b,
                keycode: 60, // F2
            }),
            Some(Hotkey::SwitchVt(2)),
            "releasing A's Ctrl must leave B's held Ctrl active",
        );
        assert_eq!(d.modifier_keys_by_source.len(), 1);
        assert_eq!(
            d.modifier_keys_by_source.get(&source_b),
            Some(&HashSet::from([LINUX_KEY_LEFTCTRL, LINUX_KEY_LEFTALT])),
        );
    }

    #[test]
    fn suspending_one_source_clears_only_its_hotkey_holders() {
        let mut d = HotkeyDetector::new();
        let source_a = InputSourceId(1);
        let source_b = InputSourceId(2);
        d.check(&InputEvent::KeyPress {
            source_id: source_a,
            keycode: LINUX_KEY_LEFTCTRL,
        });
        d.check(&InputEvent::DeviceSuspended {
            source_id: source_a,
        });
        assert_eq!(
            d.check(&InputEvent::KeyPress {
                source_id: source_b,
                keycode: 60,
            }),
            None,
            "a suspended source's Ctrl must not remain stuck",
        );
        d.check(&InputEvent::KeyPress {
            source_id: source_b,
            keycode: LINUX_KEY_LEFTCTRL,
        });
        d.check(&InputEvent::KeyPress {
            source_id: source_a,
            keycode: LINUX_KEY_LEFTALT,
        });
        d.check(&InputEvent::DeviceRemoved {
            source_id: source_a,
        });
        assert_eq!(
            d.check(&InputEvent::KeyPress {
                source_id: source_b,
                keycode: 60,
            }),
            None,
            "removal clears that source's Alt without clearing B's Ctrl",
        );
        assert_eq!(d.modifier_keys_by_source.len(), 1);
        assert_eq!(
            d.modifier_keys_by_source.get(&source_b),
            Some(&HashSet::from([LINUX_KEY_LEFTCTRL])),
        );
    }
}
