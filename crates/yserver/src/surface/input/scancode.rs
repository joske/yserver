//! PC set-1 scan codes (Windows `WM_KEYDOWN`'s, an `E0` prefix as `0xE0xx`)
//! to evdev codes (`KEY_*`); X keycode = evdev + 8, as for the Mac.
//!
//! - Pairs: keycodemapdb `data/keymaps.csv` (the table QEMU and
//!   GTK-VNC build their keymaps from), columns "AT set1 keycode" and
//!   "Linux Keycode". Its Mac column agrees with every `kVK_*` pair of
//!   [`super::keymap`] but the deliberate Help = Insert.
//! - `KEY_*` values: Linux `include/uapi/linux/input-event-codes.h`; each
//!   entry was checked against the header by name.
//! - Only the keys of PC keyboards (104/105-key, Japanese 106/109, Brazilian
//!   ABNT2) and the common media/browser keys are listed. Pause, Num Lock
//!   and Print Screen come by virtual key instead (`crate::windows::input`):
//!   Pause is the sequence `E1 1D 45`, ending in Num Lock's code, and Print
//!   Screen `E0 2A E0 37`; neither fits one entry, so the three do not depend
//!   on how Windows folds those sequences into one scan code.

/// `(set-1 scan code, evdev code)`; the name is the evdev one.
const TABLE: &[(u16, u16)] = &[
    (0x01, 1),     // KEY_ESC
    (0x02, 2),     // KEY_1
    (0x03, 3),     // KEY_2
    (0x04, 4),     // KEY_3
    (0x05, 5),     // KEY_4
    (0x06, 6),     // KEY_5
    (0x07, 7),     // KEY_6
    (0x08, 8),     // KEY_7
    (0x09, 9),     // KEY_8
    (0x0A, 10),    // KEY_9
    (0x0B, 11),    // KEY_0
    (0x0C, 12),    // KEY_MINUS
    (0x0D, 13),    // KEY_EQUAL
    (0x0E, 14),    // KEY_BACKSPACE
    (0x0F, 15),    // KEY_TAB
    (0x10, 16),    // KEY_Q
    (0x11, 17),    // KEY_W
    (0x12, 18),    // KEY_E
    (0x13, 19),    // KEY_R
    (0x14, 20),    // KEY_T
    (0x15, 21),    // KEY_Y
    (0x16, 22),    // KEY_U
    (0x17, 23),    // KEY_I
    (0x18, 24),    // KEY_O
    (0x19, 25),    // KEY_P
    (0x1A, 26),    // KEY_LEFTBRACE
    (0x1B, 27),    // KEY_RIGHTBRACE
    (0x1C, 28),    // KEY_ENTER
    (0x1D, 29),    // KEY_LEFTCTRL
    (0x1E, 30),    // KEY_A
    (0x1F, 31),    // KEY_S
    (0x20, 32),    // KEY_D
    (0x21, 33),    // KEY_F
    (0x22, 34),    // KEY_G
    (0x23, 35),    // KEY_H
    (0x24, 36),    // KEY_J
    (0x25, 37),    // KEY_K
    (0x26, 38),    // KEY_L
    (0x27, 39),    // KEY_SEMICOLON
    (0x28, 40),    // KEY_APOSTROPHE
    (0x29, 41),    // KEY_GRAVE
    (0x2A, 42),    // KEY_LEFTSHIFT
    (0x2B, 43),    // KEY_BACKSLASH
    (0x2C, 44),    // KEY_Z
    (0x2D, 45),    // KEY_X
    (0x2E, 46),    // KEY_C
    (0x2F, 47),    // KEY_V
    (0x30, 48),    // KEY_B
    (0x31, 49),    // KEY_N
    (0x32, 50),    // KEY_M
    (0x33, 51),    // KEY_COMMA
    (0x34, 52),    // KEY_DOT
    (0x35, 53),    // KEY_SLASH
    (0x36, 54),    // KEY_RIGHTSHIFT
    (0x37, 55),    // KEY_KPASTERISK
    (0x38, 56),    // KEY_LEFTALT
    (0x39, 57),    // KEY_SPACE
    (0x3A, 58),    // KEY_CAPSLOCK
    (0x3B, 59),    // KEY_F1
    (0x3C, 60),    // KEY_F2
    (0x3D, 61),    // KEY_F3
    (0x3E, 62),    // KEY_F4
    (0x3F, 63),    // KEY_F5
    (0x40, 64),    // KEY_F6
    (0x41, 65),    // KEY_F7
    (0x42, 66),    // KEY_F8
    (0x43, 67),    // KEY_F9
    (0x44, 68),    // KEY_F10
    (0x45, 69),    // KEY_NUMLOCK
    (0x46, 70),    // KEY_SCROLLLOCK
    (0x47, 71),    // KEY_KP7
    (0x48, 72),    // KEY_KP8
    (0x49, 73),    // KEY_KP9
    (0x4A, 74),    // KEY_KPMINUS
    (0x4B, 75),    // KEY_KP4
    (0x4C, 76),    // KEY_KP5
    (0x4D, 77),    // KEY_KP6
    (0x4E, 78),    // KEY_KPPLUS
    (0x4F, 79),    // KEY_KP1
    (0x50, 80),    // KEY_KP2
    (0x51, 81),    // KEY_KP3
    (0x52, 82),    // KEY_KP0
    (0x53, 83),    // KEY_KPDOT
    (0x56, 86),    // KEY_102ND
    (0x57, 87),    // KEY_F11
    (0x58, 88),    // KEY_F12
    (0x59, 117),   // KEY_KPEQUAL
    (0x70, 93),    // KEY_KATAKANAHIRAGANA
    (0x73, 89),    // KEY_RO
    (0x79, 92),    // KEY_HENKAN
    (0x7B, 94),    // KEY_MUHENKAN
    (0x7D, 124),   // KEY_YEN
    (0x7E, 121),   // KEY_KPCOMMA
    (0xE010, 165), // KEY_PREVIOUSSONG
    (0xE019, 163), // KEY_NEXTSONG
    (0xE01C, 96),  // KEY_KPENTER
    (0xE01D, 97),  // KEY_RIGHTCTRL
    (0xE020, 113), // KEY_MUTE
    (0xE021, 140), // KEY_CALC
    (0xE022, 164), // KEY_PLAYPAUSE
    (0xE024, 166), // KEY_STOPCD
    (0xE02E, 114), // KEY_VOLUMEDOWN
    (0xE030, 115), // KEY_VOLUMEUP
    (0xE032, 172), // KEY_HOMEPAGE
    (0xE035, 98),  // KEY_KPSLASH
    (0xE038, 100), // KEY_RIGHTALT
    (0xE046, 119), // KEY_PAUSE
    (0xE047, 102), // KEY_HOME
    (0xE048, 103), // KEY_UP
    (0xE049, 104), // KEY_PAGEUP
    (0xE04B, 105), // KEY_LEFT
    (0xE04D, 106), // KEY_RIGHT
    (0xE04F, 107), // KEY_END
    (0xE050, 108), // KEY_DOWN
    (0xE051, 109), // KEY_PAGEDOWN
    (0xE052, 110), // KEY_INSERT
    (0xE053, 111), // KEY_DELETE
    (0xE05B, 125), // KEY_LEFTMETA
    (0xE05C, 126), // KEY_RIGHTMETA
    (0xE05D, 127), // KEY_COMPOSE
    (0xE05E, 116), // KEY_POWER
    (0xE05F, 142), // KEY_SLEEP
    (0xE063, 143), // KEY_WAKEUP
    (0xE065, 217), // KEY_SEARCH
    (0xE066, 156), // KEY_BOOKMARKS
    (0xE067, 173), // KEY_REFRESH
    (0xE068, 128), // KEY_STOP
    (0xE069, 159), // KEY_FORWARD
    (0xE06A, 158), // KEY_BACK
    (0xE06B, 157), // KEY_COMPUTER
    (0xE06C, 155), // KEY_MAIL
    (0xE06D, 226), // KEY_MEDIA
];

/// The `E0` prefix of an extended scan code.
pub(crate) const EXTENDED: u16 = 0xE000;

/// The evdev code of set-1 `scancode` (`EXTENDED | code` for `E0 code`).
pub(crate) fn evdev_code(scancode: u16) -> Option<u16> {
    TABLE
        .iter()
        .find(|&&(code, _)| code == scancode)
        .map(|&(_, evdev)| evdev)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::{EXTENDED, TABLE, evdev_code};

    /// Spot checks against `input-event-codes.h`.
    #[test]
    fn pinned_entries_match_the_header() {
        for (scancode, evdev) in [
            (0x01, 1),              // Escape → KEY_ESC
            (0x1E, 30),             // A → KEY_A
            (0x1C, 28),             // Enter → KEY_ENTER
            (EXTENDED | 0x1C, 96),  // keypad Enter → KEY_KPENTER
            (0x1D, 29),             // left Ctrl → KEY_LEFTCTRL
            (EXTENDED | 0x1D, 97),  // right Ctrl → KEY_RIGHTCTRL
            (0x38, 56),             // left Alt → KEY_LEFTALT
            (EXTENDED | 0x38, 100), // right Alt → KEY_RIGHTALT
            (0x2A, 42),             // left Shift → KEY_LEFTSHIFT
            (0x36, 54),             // right Shift → KEY_RIGHTSHIFT
            (EXTENDED | 0x5B, 125), // left Windows → KEY_LEFTMETA
            (EXTENDED | 0x48, 103), // Up → KEY_UP
            (0x48, 72),             // keypad 8 → KEY_KP8
            (EXTENDED | 0x53, 111), // Delete → KEY_DELETE
            (0x53, 83),             // keypad . → KEY_KPDOT
            (0x56, 86),             // ISO < > → KEY_102ND
            (0x57, 87),             // F11 → KEY_F11
            (0x73, 89),             // ABNT2 / → KEY_RO
        ] {
            assert_eq!(evdev_code(scancode), Some(evdev), "{scancode:#06x}");
        }
    }

    #[test]
    fn table_is_injective_and_in_x_range() {
        let codes: HashSet<u16> = TABLE.iter().map(|&(code, _)| code).collect();
        let evdevs: HashSet<u16> = TABLE.iter().map(|&(_, evdev)| evdev).collect();
        assert_eq!(codes.len(), TABLE.len(), "one entry per scan code");
        assert_eq!(evdevs.len(), TABLE.len(), "one scan code per evdev code");
        for &(_, evdev) in TABLE {
            assert!((8..=255).contains(&(evdev + 8)), "evdev {evdev}");
        }
    }

    #[test]
    fn fake_shifts_and_unknown_codes_have_no_key() {
        // `E0 2A` / `E0 36`: the shifts a PS/2 keyboard wraps around
        // navigation keys, not keys.
        assert_eq!(evdev_code(EXTENDED | 0x2A), None);
        assert_eq!(evdev_code(EXTENDED | 0x36), None);
        assert_eq!(evdev_code(0), None);
    }
}
