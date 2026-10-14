//! Mac virtual keycodes (`kVK_*`) to evdev codes (`KEY_*`); X keycode =
//! evdev + 8, so the startup evdev keymap applies unchanged.
//!
//! - `kVK_*`: HIToolbox `Events.h` (Carbon.framework, macOS SDK). That
//!   header is not on the Linux build host; each value was cross-checked
//!   against pynput 1.8.2 (`pynput/_util/darwin_vks.py` `SYMBOLS`,
//!   `pynput/keyboard/_darwin.py` `Key`) and/or XQuartz
//!   (`hw/xquartz/quartzKeyboard.c` `known_keys`, `known_numeric_keys`,
//!   xorg-server 21.1.24). Values marked `*` have no second source and
//!   await a check against `Events.h`.
//! - `KEY_*`: Linux `include/uapi/linux/input-event-codes.h`, copied with
//!   the header line of each define.
//! - Pairing is by key identity; where macOS names a key differently the
//!   HID usage decides (Help = Insert, Keypad Clear = Num Lock, JIS Kana /
//!   Eisu = LANG1 / LANG2 = Hangeul / Hanja; the Apple JIS keypad comma is
//!   `<JPCM>` in xkeyboard-config `symbols/macintosh_vndr/jp`).

/// `kVK_*` from HIToolbox `Events.h`; `*` = no second source yet.
#[allow(non_upper_case_globals)]
pub(crate) mod kvk {
    pub(crate) const kVK_ANSI_A: u16 = 0x00;
    pub(crate) const kVK_ANSI_S: u16 = 0x01;
    pub(crate) const kVK_ANSI_D: u16 = 0x02;
    pub(crate) const kVK_ANSI_F: u16 = 0x03;
    pub(crate) const kVK_ANSI_H: u16 = 0x04;
    pub(crate) const kVK_ANSI_G: u16 = 0x05;
    pub(crate) const kVK_ANSI_Z: u16 = 0x06;
    pub(crate) const kVK_ANSI_X: u16 = 0x07;
    pub(crate) const kVK_ANSI_C: u16 = 0x08;
    pub(crate) const kVK_ANSI_V: u16 = 0x09;
    pub(crate) const kVK_ISO_Section: u16 = 0x0A; // *
    pub(crate) const kVK_ANSI_B: u16 = 0x0B;
    pub(crate) const kVK_ANSI_Q: u16 = 0x0C;
    pub(crate) const kVK_ANSI_W: u16 = 0x0D;
    pub(crate) const kVK_ANSI_E: u16 = 0x0E;
    pub(crate) const kVK_ANSI_R: u16 = 0x0F;
    pub(crate) const kVK_ANSI_Y: u16 = 0x10;
    pub(crate) const kVK_ANSI_T: u16 = 0x11;
    pub(crate) const kVK_ANSI_1: u16 = 0x12;
    pub(crate) const kVK_ANSI_2: u16 = 0x13;
    pub(crate) const kVK_ANSI_3: u16 = 0x14;
    pub(crate) const kVK_ANSI_4: u16 = 0x15;
    pub(crate) const kVK_ANSI_6: u16 = 0x16;
    pub(crate) const kVK_ANSI_5: u16 = 0x17;
    pub(crate) const kVK_ANSI_Equal: u16 = 0x18;
    pub(crate) const kVK_ANSI_9: u16 = 0x19;
    pub(crate) const kVK_ANSI_7: u16 = 0x1A;
    pub(crate) const kVK_ANSI_Minus: u16 = 0x1B;
    pub(crate) const kVK_ANSI_8: u16 = 0x1C;
    pub(crate) const kVK_ANSI_0: u16 = 0x1D;
    pub(crate) const kVK_ANSI_RightBracket: u16 = 0x1E;
    pub(crate) const kVK_ANSI_O: u16 = 0x1F;
    pub(crate) const kVK_ANSI_U: u16 = 0x20;
    pub(crate) const kVK_ANSI_LeftBracket: u16 = 0x21;
    pub(crate) const kVK_ANSI_I: u16 = 0x22;
    pub(crate) const kVK_ANSI_P: u16 = 0x23;
    pub(crate) const kVK_Return: u16 = 0x24;
    pub(crate) const kVK_ANSI_L: u16 = 0x25;
    pub(crate) const kVK_ANSI_J: u16 = 0x26;
    pub(crate) const kVK_ANSI_Quote: u16 = 0x27;
    pub(crate) const kVK_ANSI_K: u16 = 0x28;
    pub(crate) const kVK_ANSI_Semicolon: u16 = 0x29;
    pub(crate) const kVK_ANSI_Backslash: u16 = 0x2A;
    pub(crate) const kVK_ANSI_Comma: u16 = 0x2B;
    pub(crate) const kVK_ANSI_Slash: u16 = 0x2C;
    pub(crate) const kVK_ANSI_N: u16 = 0x2D;
    pub(crate) const kVK_ANSI_M: u16 = 0x2E;
    pub(crate) const kVK_ANSI_Period: u16 = 0x2F;
    pub(crate) const kVK_Tab: u16 = 0x30;
    pub(crate) const kVK_Space: u16 = 0x31;
    pub(crate) const kVK_ANSI_Grave: u16 = 0x32;
    pub(crate) const kVK_Delete: u16 = 0x33;
    pub(crate) const kVK_Escape: u16 = 0x35;
    pub(crate) const kVK_RightCommand: u16 = 0x36;
    pub(crate) const kVK_Command: u16 = 0x37;
    pub(crate) const kVK_Shift: u16 = 0x38;
    pub(crate) const kVK_CapsLock: u16 = 0x39;
    pub(crate) const kVK_Option: u16 = 0x3A;
    pub(crate) const kVK_Control: u16 = 0x3B;
    pub(crate) const kVK_RightShift: u16 = 0x3C;
    pub(crate) const kVK_RightOption: u16 = 0x3D;
    pub(crate) const kVK_RightControl: u16 = 0x3E;
    pub(crate) const kVK_F17: u16 = 0x40;
    pub(crate) const kVK_ANSI_KeypadDecimal: u16 = 0x41;
    pub(crate) const kVK_ANSI_KeypadMultiply: u16 = 0x43;
    pub(crate) const kVK_ANSI_KeypadPlus: u16 = 0x45;
    pub(crate) const kVK_ANSI_KeypadClear: u16 = 0x47; // *
    pub(crate) const kVK_ANSI_KeypadDivide: u16 = 0x4B;
    pub(crate) const kVK_ANSI_KeypadEnter: u16 = 0x4C;
    pub(crate) const kVK_ANSI_KeypadMinus: u16 = 0x4E;
    pub(crate) const kVK_F18: u16 = 0x4F;
    pub(crate) const kVK_F19: u16 = 0x50;
    pub(crate) const kVK_ANSI_KeypadEquals: u16 = 0x51;
    pub(crate) const kVK_ANSI_Keypad0: u16 = 0x52;
    pub(crate) const kVK_ANSI_Keypad1: u16 = 0x53;
    pub(crate) const kVK_ANSI_Keypad2: u16 = 0x54;
    pub(crate) const kVK_ANSI_Keypad3: u16 = 0x55;
    pub(crate) const kVK_ANSI_Keypad4: u16 = 0x56;
    pub(crate) const kVK_ANSI_Keypad5: u16 = 0x57;
    pub(crate) const kVK_ANSI_Keypad6: u16 = 0x58;
    pub(crate) const kVK_ANSI_Keypad7: u16 = 0x59;
    pub(crate) const kVK_F20: u16 = 0x5A;
    pub(crate) const kVK_ANSI_Keypad8: u16 = 0x5B;
    pub(crate) const kVK_ANSI_Keypad9: u16 = 0x5C;
    pub(crate) const kVK_JIS_Yen: u16 = 0x5D; // *
    pub(crate) const kVK_JIS_Underscore: u16 = 0x5E; // *
    pub(crate) const kVK_JIS_KeypadComma: u16 = 0x5F; // *
    pub(crate) const kVK_F5: u16 = 0x60;
    pub(crate) const kVK_F6: u16 = 0x61;
    pub(crate) const kVK_F7: u16 = 0x62;
    pub(crate) const kVK_F3: u16 = 0x63;
    pub(crate) const kVK_F8: u16 = 0x64;
    pub(crate) const kVK_F9: u16 = 0x65;
    pub(crate) const kVK_JIS_Eisu: u16 = 0x66; // *
    pub(crate) const kVK_F11: u16 = 0x67;
    pub(crate) const kVK_JIS_Kana: u16 = 0x68; // *
    pub(crate) const kVK_F13: u16 = 0x69;
    pub(crate) const kVK_F16: u16 = 0x6A;
    pub(crate) const kVK_F14: u16 = 0x6B;
    pub(crate) const kVK_F10: u16 = 0x6D;
    pub(crate) const kVK_ContextualMenu: u16 = 0x6E;
    pub(crate) const kVK_F12: u16 = 0x6F;
    pub(crate) const kVK_F15: u16 = 0x71;
    pub(crate) const kVK_Help: u16 = 0x72; // *
    pub(crate) const kVK_Home: u16 = 0x73;
    pub(crate) const kVK_PageUp: u16 = 0x74;
    pub(crate) const kVK_ForwardDelete: u16 = 0x75;
    pub(crate) const kVK_F4: u16 = 0x76;
    pub(crate) const kVK_End: u16 = 0x77;
    pub(crate) const kVK_F2: u16 = 0x78;
    pub(crate) const kVK_PageDown: u16 = 0x79;
    pub(crate) const kVK_F1: u16 = 0x7A;
    pub(crate) const kVK_LeftArrow: u16 = 0x7B;
    pub(crate) const kVK_RightArrow: u16 = 0x7C;
    pub(crate) const kVK_DownArrow: u16 = 0x7D;
    pub(crate) const kVK_UpArrow: u16 = 0x7E;
    /// Fn: no evdev peer, so no table entry. *
    #[cfg(test)]
    pub(crate) const kVK_Function: u16 = 0x3F;
}

/// `KEY_*` from `input-event-codes.h` (trailing comment: header line).
pub(crate) mod ev {
    pub(crate) const KEY_A: u16 = 30; // :106
    pub(crate) const KEY_S: u16 = 31; // :107
    pub(crate) const KEY_D: u16 = 32; // :108
    pub(crate) const KEY_F: u16 = 33; // :109
    pub(crate) const KEY_H: u16 = 35; // :111
    pub(crate) const KEY_G: u16 = 34; // :110
    pub(crate) const KEY_Z: u16 = 44; // :120
    pub(crate) const KEY_X: u16 = 45; // :121
    pub(crate) const KEY_C: u16 = 46; // :122
    pub(crate) const KEY_V: u16 = 47; // :123
    pub(crate) const KEY_102ND: u16 = 86; // :162
    pub(crate) const KEY_B: u16 = 48; // :124
    pub(crate) const KEY_Q: u16 = 16; // :92
    pub(crate) const KEY_W: u16 = 17; // :93
    pub(crate) const KEY_E: u16 = 18; // :94
    pub(crate) const KEY_R: u16 = 19; // :95
    pub(crate) const KEY_Y: u16 = 21; // :97
    pub(crate) const KEY_T: u16 = 20; // :96
    pub(crate) const KEY_1: u16 = 2; // :78
    pub(crate) const KEY_2: u16 = 3; // :79
    pub(crate) const KEY_3: u16 = 4; // :80
    pub(crate) const KEY_4: u16 = 5; // :81
    pub(crate) const KEY_6: u16 = 7; // :83
    pub(crate) const KEY_5: u16 = 6; // :82
    pub(crate) const KEY_EQUAL: u16 = 13; // :89
    pub(crate) const KEY_9: u16 = 10; // :86
    pub(crate) const KEY_7: u16 = 8; // :84
    pub(crate) const KEY_MINUS: u16 = 12; // :88
    pub(crate) const KEY_8: u16 = 9; // :85
    pub(crate) const KEY_0: u16 = 11; // :87
    pub(crate) const KEY_RIGHTBRACE: u16 = 27; // :103
    pub(crate) const KEY_O: u16 = 24; // :100
    pub(crate) const KEY_U: u16 = 22; // :98
    pub(crate) const KEY_LEFTBRACE: u16 = 26; // :102
    pub(crate) const KEY_I: u16 = 23; // :99
    pub(crate) const KEY_P: u16 = 25; // :101
    pub(crate) const KEY_ENTER: u16 = 28; // :104
    pub(crate) const KEY_L: u16 = 38; // :114
    pub(crate) const KEY_J: u16 = 36; // :112
    pub(crate) const KEY_APOSTROPHE: u16 = 40; // :116
    pub(crate) const KEY_K: u16 = 37; // :113
    pub(crate) const KEY_SEMICOLON: u16 = 39; // :115
    pub(crate) const KEY_BACKSLASH: u16 = 43; // :119
    pub(crate) const KEY_COMMA: u16 = 51; // :127
    pub(crate) const KEY_SLASH: u16 = 53; // :129
    pub(crate) const KEY_N: u16 = 49; // :125
    pub(crate) const KEY_M: u16 = 50; // :126
    pub(crate) const KEY_DOT: u16 = 52; // :128
    pub(crate) const KEY_TAB: u16 = 15; // :91
    pub(crate) const KEY_SPACE: u16 = 57; // :133
    pub(crate) const KEY_GRAVE: u16 = 41; // :117
    pub(crate) const KEY_BACKSPACE: u16 = 14; // :90
    pub(crate) const KEY_ESC: u16 = 1; // :77
    pub(crate) const KEY_RIGHTMETA: u16 = 126; // :204
    pub(crate) const KEY_LEFTMETA: u16 = 125; // :203
    pub(crate) const KEY_LEFTSHIFT: u16 = 42; // :118
    pub(crate) const KEY_CAPSLOCK: u16 = 58; // :134
    pub(crate) const KEY_LEFTALT: u16 = 56; // :132
    pub(crate) const KEY_LEFTCTRL: u16 = 29; // :105
    pub(crate) const KEY_RIGHTSHIFT: u16 = 54; // :130
    pub(crate) const KEY_RIGHTALT: u16 = 100; // :176
    pub(crate) const KEY_RIGHTCTRL: u16 = 97; // :173
    pub(crate) const KEY_F17: u16 = 187; // :269
    pub(crate) const KEY_KPDOT: u16 = 83; // :159
    pub(crate) const KEY_KPASTERISK: u16 = 55; // :131
    pub(crate) const KEY_KPPLUS: u16 = 78; // :154
    pub(crate) const KEY_NUMLOCK: u16 = 69; // :145
    pub(crate) const KEY_KPSLASH: u16 = 98; // :174
    pub(crate) const KEY_KPENTER: u16 = 96; // :172
    pub(crate) const KEY_KPMINUS: u16 = 74; // :150
    pub(crate) const KEY_F18: u16 = 188; // :270
    pub(crate) const KEY_F19: u16 = 189; // :271
    pub(crate) const KEY_KPEQUAL: u16 = 117; // :193
    pub(crate) const KEY_KP0: u16 = 82; // :158
    pub(crate) const KEY_KP1: u16 = 79; // :155
    pub(crate) const KEY_KP2: u16 = 80; // :156
    pub(crate) const KEY_KP3: u16 = 81; // :157
    pub(crate) const KEY_KP4: u16 = 75; // :151
    pub(crate) const KEY_KP5: u16 = 76; // :152
    pub(crate) const KEY_KP6: u16 = 77; // :153
    pub(crate) const KEY_KP7: u16 = 71; // :147
    pub(crate) const KEY_F20: u16 = 190; // :272
    pub(crate) const KEY_KP8: u16 = 72; // :148
    pub(crate) const KEY_KP9: u16 = 73; // :149
    pub(crate) const KEY_YEN: u16 = 124; // :202
    pub(crate) const KEY_RO: u16 = 89; // :165
    pub(crate) const KEY_KPJPCOMMA: u16 = 95; // :171
    pub(crate) const KEY_F5: u16 = 63; // :139
    pub(crate) const KEY_F6: u16 = 64; // :140
    pub(crate) const KEY_F7: u16 = 65; // :141
    pub(crate) const KEY_F3: u16 = 61; // :137
    pub(crate) const KEY_F8: u16 = 66; // :142
    pub(crate) const KEY_F9: u16 = 67; // :143
    pub(crate) const KEY_HANJA: u16 = 123; // :201
    pub(crate) const KEY_F11: u16 = 87; // :163
    pub(crate) const KEY_HANGEUL: u16 = 122; // :199
    pub(crate) const KEY_F13: u16 = 183; // :265
    pub(crate) const KEY_F16: u16 = 186; // :268
    pub(crate) const KEY_F14: u16 = 184; // :266
    pub(crate) const KEY_F10: u16 = 68; // :144
    pub(crate) const KEY_COMPOSE: u16 = 127; // :205
    pub(crate) const KEY_F12: u16 = 88; // :164
    pub(crate) const KEY_F15: u16 = 185; // :267
    pub(crate) const KEY_INSERT: u16 = 110; // :186
    pub(crate) const KEY_HOME: u16 = 102; // :178
    pub(crate) const KEY_PAGEUP: u16 = 104; // :180
    pub(crate) const KEY_DELETE: u16 = 111; // :187
    pub(crate) const KEY_F4: u16 = 62; // :138
    pub(crate) const KEY_END: u16 = 107; // :183
    pub(crate) const KEY_F2: u16 = 60; // :136
    pub(crate) const KEY_PAGEDOWN: u16 = 109; // :185
    pub(crate) const KEY_F1: u16 = 59; // :135
    pub(crate) const KEY_LEFT: u16 = 105; // :181
    pub(crate) const KEY_RIGHT: u16 = 106; // :182
    pub(crate) const KEY_DOWN: u16 = 108; // :184
    pub(crate) const KEY_UP: u16 = 103; // :179
}

/// `(kVK, evdev)` by key identity.
const TABLE: &[(u16, u16)] = &[
    (kvk::kVK_ANSI_A, ev::KEY_A),
    (kvk::kVK_ANSI_S, ev::KEY_S),
    (kvk::kVK_ANSI_D, ev::KEY_D),
    (kvk::kVK_ANSI_F, ev::KEY_F),
    (kvk::kVK_ANSI_H, ev::KEY_H),
    (kvk::kVK_ANSI_G, ev::KEY_G),
    (kvk::kVK_ANSI_Z, ev::KEY_Z),
    (kvk::kVK_ANSI_X, ev::KEY_X),
    (kvk::kVK_ANSI_C, ev::KEY_C),
    (kvk::kVK_ANSI_V, ev::KEY_V),
    (kvk::kVK_ISO_Section, ev::KEY_102ND),
    (kvk::kVK_ANSI_B, ev::KEY_B),
    (kvk::kVK_ANSI_Q, ev::KEY_Q),
    (kvk::kVK_ANSI_W, ev::KEY_W),
    (kvk::kVK_ANSI_E, ev::KEY_E),
    (kvk::kVK_ANSI_R, ev::KEY_R),
    (kvk::kVK_ANSI_Y, ev::KEY_Y),
    (kvk::kVK_ANSI_T, ev::KEY_T),
    (kvk::kVK_ANSI_1, ev::KEY_1),
    (kvk::kVK_ANSI_2, ev::KEY_2),
    (kvk::kVK_ANSI_3, ev::KEY_3),
    (kvk::kVK_ANSI_4, ev::KEY_4),
    (kvk::kVK_ANSI_6, ev::KEY_6),
    (kvk::kVK_ANSI_5, ev::KEY_5),
    (kvk::kVK_ANSI_Equal, ev::KEY_EQUAL),
    (kvk::kVK_ANSI_9, ev::KEY_9),
    (kvk::kVK_ANSI_7, ev::KEY_7),
    (kvk::kVK_ANSI_Minus, ev::KEY_MINUS),
    (kvk::kVK_ANSI_8, ev::KEY_8),
    (kvk::kVK_ANSI_0, ev::KEY_0),
    (kvk::kVK_ANSI_RightBracket, ev::KEY_RIGHTBRACE),
    (kvk::kVK_ANSI_O, ev::KEY_O),
    (kvk::kVK_ANSI_U, ev::KEY_U),
    (kvk::kVK_ANSI_LeftBracket, ev::KEY_LEFTBRACE),
    (kvk::kVK_ANSI_I, ev::KEY_I),
    (kvk::kVK_ANSI_P, ev::KEY_P),
    (kvk::kVK_Return, ev::KEY_ENTER),
    (kvk::kVK_ANSI_L, ev::KEY_L),
    (kvk::kVK_ANSI_J, ev::KEY_J),
    (kvk::kVK_ANSI_Quote, ev::KEY_APOSTROPHE),
    (kvk::kVK_ANSI_K, ev::KEY_K),
    (kvk::kVK_ANSI_Semicolon, ev::KEY_SEMICOLON),
    (kvk::kVK_ANSI_Backslash, ev::KEY_BACKSLASH),
    (kvk::kVK_ANSI_Comma, ev::KEY_COMMA),
    (kvk::kVK_ANSI_Slash, ev::KEY_SLASH),
    (kvk::kVK_ANSI_N, ev::KEY_N),
    (kvk::kVK_ANSI_M, ev::KEY_M),
    (kvk::kVK_ANSI_Period, ev::KEY_DOT),
    (kvk::kVK_Tab, ev::KEY_TAB),
    (kvk::kVK_Space, ev::KEY_SPACE),
    (kvk::kVK_ANSI_Grave, ev::KEY_GRAVE),
    (kvk::kVK_Delete, ev::KEY_BACKSPACE),
    (kvk::kVK_Escape, ev::KEY_ESC),
    (kvk::kVK_RightCommand, ev::KEY_RIGHTMETA),
    (kvk::kVK_Command, ev::KEY_LEFTMETA),
    (kvk::kVK_Shift, ev::KEY_LEFTSHIFT),
    (kvk::kVK_CapsLock, ev::KEY_CAPSLOCK),
    (kvk::kVK_Option, ev::KEY_LEFTALT),
    (kvk::kVK_Control, ev::KEY_LEFTCTRL),
    (kvk::kVK_RightShift, ev::KEY_RIGHTSHIFT),
    (kvk::kVK_RightOption, ev::KEY_RIGHTALT),
    (kvk::kVK_RightControl, ev::KEY_RIGHTCTRL),
    (kvk::kVK_F17, ev::KEY_F17),
    (kvk::kVK_ANSI_KeypadDecimal, ev::KEY_KPDOT),
    (kvk::kVK_ANSI_KeypadMultiply, ev::KEY_KPASTERISK),
    (kvk::kVK_ANSI_KeypadPlus, ev::KEY_KPPLUS),
    (kvk::kVK_ANSI_KeypadClear, ev::KEY_NUMLOCK),
    (kvk::kVK_ANSI_KeypadDivide, ev::KEY_KPSLASH),
    (kvk::kVK_ANSI_KeypadEnter, ev::KEY_KPENTER),
    (kvk::kVK_ANSI_KeypadMinus, ev::KEY_KPMINUS),
    (kvk::kVK_F18, ev::KEY_F18),
    (kvk::kVK_F19, ev::KEY_F19),
    (kvk::kVK_ANSI_KeypadEquals, ev::KEY_KPEQUAL),
    (kvk::kVK_ANSI_Keypad0, ev::KEY_KP0),
    (kvk::kVK_ANSI_Keypad1, ev::KEY_KP1),
    (kvk::kVK_ANSI_Keypad2, ev::KEY_KP2),
    (kvk::kVK_ANSI_Keypad3, ev::KEY_KP3),
    (kvk::kVK_ANSI_Keypad4, ev::KEY_KP4),
    (kvk::kVK_ANSI_Keypad5, ev::KEY_KP5),
    (kvk::kVK_ANSI_Keypad6, ev::KEY_KP6),
    (kvk::kVK_ANSI_Keypad7, ev::KEY_KP7),
    (kvk::kVK_F20, ev::KEY_F20),
    (kvk::kVK_ANSI_Keypad8, ev::KEY_KP8),
    (kvk::kVK_ANSI_Keypad9, ev::KEY_KP9),
    (kvk::kVK_JIS_Yen, ev::KEY_YEN),
    (kvk::kVK_JIS_Underscore, ev::KEY_RO),
    (kvk::kVK_JIS_KeypadComma, ev::KEY_KPJPCOMMA),
    (kvk::kVK_F5, ev::KEY_F5),
    (kvk::kVK_F6, ev::KEY_F6),
    (kvk::kVK_F7, ev::KEY_F7),
    (kvk::kVK_F3, ev::KEY_F3),
    (kvk::kVK_F8, ev::KEY_F8),
    (kvk::kVK_F9, ev::KEY_F9),
    (kvk::kVK_JIS_Eisu, ev::KEY_HANJA),
    (kvk::kVK_F11, ev::KEY_F11),
    (kvk::kVK_JIS_Kana, ev::KEY_HANGEUL),
    (kvk::kVK_F13, ev::KEY_F13),
    (kvk::kVK_F16, ev::KEY_F16),
    (kvk::kVK_F14, ev::KEY_F14),
    (kvk::kVK_F10, ev::KEY_F10),
    (kvk::kVK_ContextualMenu, ev::KEY_COMPOSE),
    (kvk::kVK_F12, ev::KEY_F12),
    (kvk::kVK_F15, ev::KEY_F15),
    (kvk::kVK_Help, ev::KEY_INSERT),
    (kvk::kVK_Home, ev::KEY_HOME),
    (kvk::kVK_PageUp, ev::KEY_PAGEUP),
    (kvk::kVK_ForwardDelete, ev::KEY_DELETE),
    (kvk::kVK_F4, ev::KEY_F4),
    (kvk::kVK_End, ev::KEY_END),
    (kvk::kVK_F2, ev::KEY_F2),
    (kvk::kVK_PageDown, ev::KEY_PAGEDOWN),
    (kvk::kVK_F1, ev::KEY_F1),
    (kvk::kVK_LeftArrow, ev::KEY_LEFT),
    (kvk::kVK_RightArrow, ev::KEY_RIGHT),
    (kvk::kVK_DownArrow, ev::KEY_DOWN),
    (kvk::kVK_UpArrow, ev::KEY_UP),
];

/// The evdev code of `kvk`. `iso`: an Apple ISO keyboard, which reports the
/// key left of `1` as `ISO_Section` and the key right of left Shift as
/// `ANSI_Grave`; they are swapped so pc105 finds `<TLDE>` and `<LSGT>`.
pub(crate) fn evdev_code(kvk: u16, iso: bool) -> Option<u16> {
    let kvk = match kvk {
        kvk::kVK_ISO_Section if iso => kvk::kVK_ANSI_Grave,
        kvk::kVK_ANSI_Grave if iso => kvk::kVK_ISO_Section,
        other => other,
    };
    TABLE
        .iter()
        .find(|&&(mac, _)| mac == kvk)
        .map(|&(_, evdev)| evdev)
}

/// The X keycode of `kvk` (evdev + 8).
pub(crate) fn x_keycode(kvk: u16, iso: bool) -> Option<u8> {
    evdev_code(kvk, iso).and_then(|evdev| u8::try_from(evdev + 8).ok())
}

/// The X keycode of an evdev code.
pub(crate) fn x_keycode_of_evdev(evdev: u16) -> u8 {
    u8::try_from(evdev + 8).expect("an evdev key below 248")
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::{TABLE, evdev_code, kvk, x_keycode};

    /// Cross-checked kVK values (see the module doc) and `input-event-codes.h`.
    #[test]
    fn pinned_entries_match_the_headers() {
        for (mac, evdev, x) in [
            (0x00, 30, 38),   // kVK_ANSI_A → KEY_A
            (0x24, 28, 36),   // kVK_Return → KEY_ENTER
            (0x35, 1, 9),     // kVK_Escape → KEY_ESC
            (0x37, 125, 133), // kVK_Command → KEY_LEFTMETA
            (0x36, 126, 134), // kVK_RightCommand → KEY_RIGHTMETA
            (0x39, 58, 66),   // kVK_CapsLock → KEY_CAPSLOCK
            (0x7B, 105, 113), // kVK_LeftArrow → KEY_LEFT
            (0x4C, 96, 104),  // kVK_ANSI_KeypadEnter → KEY_KPENTER
            (0x33, 14, 22),   // kVK_Delete → KEY_BACKSPACE
            (0x75, 111, 119), // kVK_ForwardDelete → KEY_DELETE
            (0x7A, 59, 67),   // kVK_F1 → KEY_F1
            (0x38, 42, 50),   // kVK_Shift → KEY_LEFTSHIFT
            (0x3B, 29, 37),   // kVK_Control → KEY_LEFTCTRL
            (0x3A, 56, 64),   // kVK_Option → KEY_LEFTALT
            (0x3D, 100, 108), // kVK_RightOption → KEY_RIGHTALT
        ] {
            assert_eq!(evdev_code(mac, false), Some(evdev), "kVK {mac:#04x}");
            assert_eq!(x_keycode(mac, false), Some(x), "kVK {mac:#04x}");
        }
    }

    #[test]
    fn table_is_injective_in_x_range_and_has_no_fn() {
        let macs: HashSet<u16> = TABLE.iter().map(|&(mac, _)| mac).collect();
        let evdevs: HashSet<u16> = TABLE.iter().map(|&(_, evdev)| evdev).collect();
        assert_eq!(macs.len(), TABLE.len(), "one entry per kVK");
        assert_eq!(evdevs.len(), TABLE.len(), "one kVK per evdev code");
        for &(_, evdev) in TABLE {
            assert!((8..=255).contains(&(evdev + 8)), "evdev {evdev}");
        }
        assert_eq!(
            evdev_code(kvk::kVK_Function, false),
            None,
            "Fn has no X key"
        );
        assert_eq!(evdev_code(kvk::kVK_Function, true), None);
    }

    #[test]
    fn iso_keyboards_swap_section_and_grave() {
        assert_eq!(evdev_code(kvk::kVK_ISO_Section, false), Some(86));
        assert_eq!(evdev_code(kvk::kVK_ANSI_Grave, false), Some(41));
        assert_eq!(evdev_code(kvk::kVK_ISO_Section, true), Some(41), "→ <TLDE>");
        assert_eq!(evdev_code(kvk::kVK_ANSI_Grave, true), Some(86), "→ <LSGT>");
        assert_eq!(
            evdev_code(kvk::kVK_ANSI_A, true),
            Some(30),
            "others unchanged"
        );
    }
}
