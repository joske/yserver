//! Key actions between xkbcommon's V1 text (`SetMods(modifiers=Shift)`) and
//! Xorg's 8-byte wire form (`xkbActionWireDesc`, XKBproto.h).
//!
//! The parser reads what xkbcommon's serializer writes, for every action
//! type xkeyboard-config's compat and symbols use. The writer emits the same
//! syntax for the cooking keymap, with modifiers already resolved to real
//! modifiers (the cooking keymap declares no virtual modifiers).

use super::{
    Action, SA_ACTION_MESSAGE, SA_DEVICE_BTN, SA_DEVICE_VALUATOR, SA_ISO_LOCK, SA_LATCH_GROUP,
    SA_LATCH_MODS, SA_LOCK_CONTROLS, SA_LOCK_DEVICE_BTN, SA_LOCK_GROUP, SA_LOCK_MODS,
    SA_LOCK_PTR_BTN, SA_MOVE_PTR, SA_NO_ACTION, SA_PTR_BTN, SA_REDIRECT_KEY, SA_SET_CONTROLS,
    SA_SET_GROUP, SA_SET_MODS, SA_SET_PTR_DFLT, SA_SWITCH_SCREEN, SA_TERMINATE,
    SA_USE_MOD_MAP_MODS,
};
use crate::kms::xkb::REAL_MOD_NAMES;

/// `XkbSA_ClearLocks` (Set/Latch mods and group).
const SA_CLEAR_LOCKS: u8 = 0x01;
/// `XkbSA_LatchToLock`.
const SA_LATCH_TO_LOCK: u8 = 0x02;
/// `XkbSA_LockNoLock` / `XkbSA_LockNoUnlock`.
const SA_LOCK_NO_LOCK: u8 = 0x01;
const SA_LOCK_NO_UNLOCK: u8 = 0x02;
/// `XkbSA_GroupAbsolute`, `XkbSA_DfltBtnAbsolute`, `XkbSA_SwitchAbsolute`.
const SA_ABSOLUTE: u8 = 0x04;
/// `XkbSA_NoAcceleration`, `XkbSA_MoveAbsoluteX`, `XkbSA_MoveAbsoluteY`.
const SA_NO_ACCELERATION: u8 = 0x01;
const SA_MOVE_ABSOLUTE_X: u8 = 0x02;
const SA_MOVE_ABSOLUTE_Y: u8 = 0x04;
/// `XkbSA_AffectDfltBtn`.
const SA_AFFECT_DFLT_BTN: u8 = 0x01;
/// `XkbSA_SwitchApplication` (SwitchScreen `!same`).
const SA_SWITCH_APPLICATION: u8 = 0x01;

/// The XKB boolean controls by the names xkbcommon writes, `XkbControls`
/// bit order.
pub(crate) const CONTROL_NAMES: [(&str, u32); 13] = [
    ("RepeatKeys", 1 << 0),
    ("SlowKeys", 1 << 1),
    ("BounceKeys", 1 << 2),
    ("StickyKeys", 1 << 3),
    ("MouseKeys", 1 << 4),
    ("MouseKeysAccel", 1 << 5),
    ("AccessXKeys", 1 << 6),
    ("AccessXTimeout", 1 << 7),
    ("AccessXFeedback", 1 << 8),
    ("AudibleBell", 1 << 9),
    ("Overlay1", 1 << 10),
    ("Overlay2", 1 << 11),
    ("IgnoreGroupLock", 1 << 12),
];

/// Aliases xkbcommon also accepts for controls.
fn control_bit(name: &str) -> Option<u32> {
    let n = name.trim();
    if n.eq_ignore_ascii_case("none") {
        return Some(0);
    }
    if n.eq_ignore_ascii_case("all") {
        return Some(0x1fff);
    }
    let alias = match n.to_ascii_lowercase().as_str() {
        "repeat" | "autorepeat" => "RepeatKeys",
        "accessx" => "AccessXKeys",
        "accessxfeedback" | "accessx_feedback" => "AccessXFeedback",
        _ => n,
    };
    CONTROL_NAMES
        .iter()
        .find(|(c, _)| c.eq_ignore_ascii_case(alias))
        .map(|(_, b)| *b)
}

/// Why an action text couldn't be encoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ActionError(pub String);

impl std::fmt::Display for ActionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "can't encode action {:?}", self.0)
    }
}

/// `Shift+Mod1+NumLock` → `(real mods, virtual mods)`; `all`, `none` and hex
/// values as xkbcommon writes them. `vmod_bit` maps a virtual modifier name
/// to its bit.
pub(crate) fn parse_mods(text: &str, vmod_bit: &dyn Fn(&str) -> Option<u16>) -> (u8, u16) {
    let mut real = 0u8;
    let mut virt = 0u16;
    for part in text.split('+') {
        let p = part.trim();
        if p.is_empty() || p.eq_ignore_ascii_case("none") {
            continue;
        }
        if p.eq_ignore_ascii_case("all") {
            real = 0xff;
            continue;
        }
        if let Some(hex) = p.strip_prefix("0x") {
            let v = u32::from_str_radix(hex, 16).unwrap_or(0);
            real |= u8::try_from(v & 0xff).unwrap_or(0);
            virt |= u16::try_from((v >> 8) & 0xffff).unwrap_or(0);
            continue;
        }
        if let Some(i) = REAL_MOD_NAMES
            .iter()
            .position(|n| n.eq_ignore_ascii_case(p))
        {
            real |= 1 << i;
        } else if let Some(bit) = vmod_bit(p) {
            virt |= bit;
        }
    }
    (real, virt)
}

/// Real modifiers as `Shift+Mod1`, `none` for none.
pub(crate) fn real_mods_text(mask: u8) -> String {
    if mask == 0 {
        return "none".to_owned();
    }
    REAL_MOD_NAMES
        .iter()
        .enumerate()
        .filter(|(i, _)| mask & (1 << i) != 0)
        .map(|(_, n)| *n)
        .collect::<Vec<_>>()
        .join("+")
}

/// `name(a=b,c)` → `("name", ["a=b", "c"])`.
fn split_call(text: &str) -> (&str, Vec<&str>) {
    let (name, args) = text.split_once('(').unwrap_or((text, ")"));
    let args = args.trim_end().strip_suffix(')').unwrap_or(args);
    (
        name.trim(),
        super::text::split_top_level(args, b',')
            .into_iter()
            .collect(),
    )
}

/// `+3` → (3, relative), `-1` → (-1, relative), `2` → (2, absolute).
fn signed_arg(v: &str) -> Option<(i32, bool)> {
    let v = v.trim();
    if let Some(r) = v.strip_prefix('+') {
        return Some((r.trim().parse().ok()?, false));
    }
    if v.starts_with('-') {
        return Some((v.parse().ok()?, false));
    }
    Some((v.parse().ok()?, true))
}

fn int_arg(v: &str) -> Option<i32> {
    let v = v.trim();
    if let Some(h) = v.strip_prefix("0x") {
        return i32::from_str_radix(h, 16).ok();
    }
    v.parse().ok()
}

/// `affect=lock|unlock|both|neither` → the lock flags.
fn affect_flags(v: &str) -> u8 {
    match v.trim().to_ascii_lowercase().as_str() {
        "lock" => SA_LOCK_NO_UNLOCK,
        "unlock" => SA_LOCK_NO_LOCK,
        "neither" => SA_LOCK_NO_LOCK | SA_LOCK_NO_UNLOCK,
        _ => 0,
    }
}

fn i8_byte(v: i32) -> u8 {
    (v & 0xff) as u8
}

/// Encode one xkbcommon action text to wire bytes. Modifier actions keep
/// `mask = realMods` (what xkbcomp stores; the server resolves the virtual
/// part when it applies the action to a key).
pub(crate) fn parse_action(
    text: &str,
    vmod_bit: &dyn Fn(&str) -> Option<u16>,
) -> Result<Action, ActionError> {
    let (name, args) = split_call(text);
    let err = || ActionError(text.to_owned());
    let arg = |key: &str| -> Option<&str> {
        args.iter().find_map(|a| {
            let (k, v) = a.split_once('=')?;
            k.trim().eq_ignore_ascii_case(key).then_some(v.trim())
        })
    };
    let flag = |f: &str| args.iter().any(|a| a.trim().eq_ignore_ascii_case(f));
    let lower = name.to_ascii_lowercase();
    let mut a = [0u8; 8];
    match lower.as_str() {
        "noaction" | "voidaction" => {}
        "setmods" | "latchmods" | "lockmods" => {
            a[0] = match lower.as_str() {
                "setmods" => SA_SET_MODS,
                "latchmods" => SA_LATCH_MODS,
                _ => SA_LOCK_MODS,
            };
            let mods = arg("modifiers").or_else(|| arg("mods")).unwrap_or("none");
            let mut named = String::new();
            for part in mods.split('+') {
                if part.trim().eq_ignore_ascii_case("modMapMods") {
                    a[1] |= SA_USE_MOD_MAP_MODS;
                } else {
                    named.push_str(part);
                    named.push('+');
                }
            }
            let (real, vmods) = parse_mods(&named, vmod_bit);
            if a[0] == SA_LOCK_MODS {
                if let Some(v) = arg("affect") {
                    a[1] |= affect_flags(v);
                }
            } else {
                if flag("clearLocks") {
                    a[1] |= SA_CLEAR_LOCKS;
                }
                if a[0] == SA_LATCH_MODS && flag("latchToLock") {
                    a[1] |= SA_LATCH_TO_LOCK;
                }
            }
            a[2] = real;
            a[3] = real;
            a[4..6].copy_from_slice(&vmods.to_be_bytes());
        }
        "setgroup" | "latchgroup" | "lockgroup" => {
            a[0] = match lower.as_str() {
                "setgroup" => SA_SET_GROUP,
                "latchgroup" => SA_LATCH_GROUP,
                _ => SA_LOCK_GROUP,
            };
            let (g, absolute) = signed_arg(arg("group").ok_or_else(err)?).ok_or_else(err)?;
            if absolute {
                a[1] |= SA_ABSOLUTE;
                a[2] = i8_byte(g - 1);
            } else {
                a[2] = i8_byte(g);
            }
            if a[0] != SA_LOCK_GROUP {
                if flag("clearLocks") {
                    a[1] |= SA_CLEAR_LOCKS;
                }
                if flag("latchToLock") {
                    a[1] |= SA_LATCH_TO_LOCK;
                }
            }
        }
        "moveptr" | "movepointer" => {
            a[0] = SA_MOVE_PTR;
            let (x, ax) = signed_arg(arg("x").unwrap_or("+0")).ok_or_else(err)?;
            let (y, ay) = signed_arg(arg("y").unwrap_or("+0")).ok_or_else(err)?;
            if ax {
                a[1] |= SA_MOVE_ABSOLUTE_X;
            }
            if ay {
                a[1] |= SA_MOVE_ABSOLUTE_Y;
            }
            if flag("!accel") || arg("accel").is_some_and(is_false) {
                a[1] |= SA_NO_ACCELERATION;
            }
            let xb = i16::try_from(x).map_err(|_| err())?.to_be_bytes();
            let yb = i16::try_from(y).map_err(|_| err())?.to_be_bytes();
            a[2..4].copy_from_slice(&xb);
            a[4..6].copy_from_slice(&yb);
        }
        "ptrbtn" | "pointerbutton" | "lockptrbtn" | "lockpointerbutton" | "lockptrbutton"
        | "lockpointerbtn" => {
            a[0] = if lower.starts_with("lock") {
                SA_LOCK_PTR_BTN
            } else {
                SA_PTR_BTN
            };
            let button = arg("button").unwrap_or("default");
            a[3] = if button.eq_ignore_ascii_case("default") {
                0
            } else {
                u8::try_from(int_arg(button).ok_or_else(err)?).map_err(|_| err())?
            };
            if let Some(c) = arg("count") {
                a[2] = u8::try_from(int_arg(c).ok_or_else(err)?).map_err(|_| err())?;
            }
            if a[0] == SA_LOCK_PTR_BTN
                && let Some(v) = arg("affect")
            {
                a[1] |= affect_flags(v);
            }
        }
        "setptrdflt" | "setpointerdefault" => {
            a[0] = SA_SET_PTR_DFLT;
            a[2] = SA_AFFECT_DFLT_BTN;
            let (v, absolute) = signed_arg(arg("button").ok_or_else(err)?).ok_or_else(err)?;
            if absolute {
                a[1] |= SA_ABSOLUTE;
            }
            a[3] = i8_byte(v);
        }
        "terminate" | "terminateserver" => a[0] = SA_TERMINATE,
        "switchscreen" => {
            a[0] = SA_SWITCH_SCREEN;
            let (s, absolute) = signed_arg(arg("screen").ok_or_else(err)?).ok_or_else(err)?;
            if absolute {
                a[1] |= SA_ABSOLUTE;
            }
            if flag("!same") || arg("same").is_some_and(is_false) {
                a[1] |= SA_SWITCH_APPLICATION;
            }
            a[2] = i8_byte(s);
        }
        "setcontrols" | "lockcontrols" => {
            a[0] = if lower == "setcontrols" {
                SA_SET_CONTROLS
            } else {
                SA_LOCK_CONTROLS
            };
            let mut ctrls = 0u32;
            for c in arg("controls")
                .or_else(|| arg("ctrls"))
                .unwrap_or("none")
                .split('+')
            {
                ctrls |= control_bit(c).ok_or_else(err)?;
            }
            if a[0] == SA_LOCK_CONTROLS
                && let Some(v) = arg("affect")
            {
                a[1] |= affect_flags(v);
            }
            a[2..6].copy_from_slice(&ctrls.to_be_bytes());
        }
        "private" => {
            a[0] = u8::try_from(int_arg(arg("type").ok_or_else(err)?).ok_or_else(err)?)
                .map_err(|_| err())?;
            for (i, byte) in a.iter_mut().enumerate().skip(1) {
                if let Some(v) = arg(&format!("data[{}]", i - 1)) {
                    *byte = u8::try_from(int_arg(v).ok_or_else(err)?).map_err(|_| err())?;
                }
            }
        }
        _ => return Err(err()),
    }
    Ok(a)
}

fn is_false(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "false" | "no" | "off"
    )
}

/// Whether xkbcommon can carry this action type into a keymap. The others
/// (§4.6: ISOLock, ActionMessage, RedirectKey, DeviceBtn, LockDeviceBtn,
/// DeviceValuator) are stored and read back exactly but cooked as
/// `NoAction`.
pub(crate) fn cookable(act: &Action) -> bool {
    !matches!(
        act[0],
        SA_ISO_LOCK
            | SA_ACTION_MESSAGE
            | SA_REDIRECT_KEY
            | SA_DEVICE_BTN
            | SA_LOCK_DEVICE_BTN
            | SA_DEVICE_VALUATOR
    )
}

/// The action's name for a log line.
pub(crate) fn type_name(t: u8) -> &'static str {
    match t {
        SA_NO_ACTION => "NoAction",
        SA_SET_MODS => "SetMods",
        SA_LATCH_MODS => "LatchMods",
        SA_LOCK_MODS => "LockMods",
        SA_SET_GROUP => "SetGroup",
        SA_LATCH_GROUP => "LatchGroup",
        SA_LOCK_GROUP => "LockGroup",
        SA_MOVE_PTR => "MovePtr",
        SA_PTR_BTN => "PtrBtn",
        SA_LOCK_PTR_BTN => "LockPtrBtn",
        SA_SET_PTR_DFLT => "SetPtrDflt",
        SA_ISO_LOCK => "ISOLock",
        SA_TERMINATE => "Terminate",
        SA_SWITCH_SCREEN => "SwitchScreen",
        SA_SET_CONTROLS => "SetControls",
        SA_LOCK_CONTROLS => "LockControls",
        SA_ACTION_MESSAGE => "ActionMessage",
        SA_REDIRECT_KEY => "RedirectKey",
        SA_DEVICE_BTN => "DeviceBtn",
        SA_LOCK_DEVICE_BTN => "LockDeviceBtn",
        SA_DEVICE_VALUATOR => "DeviceValuator",
        _ => "Private",
    }
}

fn signed_text(v: i32, absolute: bool) -> String {
    if absolute || v < 0 {
        format!("{v}")
    } else {
        format!("+{v}")
    }
}

fn lock_affect_text(flags: u8) -> &'static str {
    match flags & (SA_LOCK_NO_LOCK | SA_LOCK_NO_UNLOCK) {
        SA_LOCK_NO_UNLOCK => ",affect=lock",
        SA_LOCK_NO_LOCK => ",affect=unlock",
        3 => ",affect=neither",
        _ => "",
    }
}

/// The cooking keymap's text for a wire action: modifiers as the resolved
/// real mask, everything xkbcommon can't carry as `NoAction()`.
pub(crate) fn action_text(act: &Action) -> String {
    let flags = act[1];
    match act[0] {
        SA_SET_MODS | SA_LATCH_MODS => format!(
            "{}(modifiers={}{}{})",
            type_name(act[0]),
            real_mods_text(act[2]),
            if flags & SA_CLEAR_LOCKS != 0 {
                ",clearLocks"
            } else {
                ""
            },
            if act[0] == SA_LATCH_MODS && flags & SA_LATCH_TO_LOCK != 0 {
                ",latchToLock"
            } else {
                ""
            },
        ),
        SA_LOCK_MODS => format!(
            "LockMods(modifiers={}{})",
            real_mods_text(act[2]),
            lock_affect_text(flags)
        ),
        SA_SET_GROUP | SA_LATCH_GROUP | SA_LOCK_GROUP => {
            let absolute = flags & SA_ABSOLUTE != 0;
            let g = i32::from(act[2] as i8);
            let group = if absolute {
                format!("{}", g + 1)
            } else {
                signed_text(g, false)
            };
            let extra = if act[0] == SA_LOCK_GROUP {
                String::new()
            } else {
                format!(
                    "{}{}",
                    if flags & SA_CLEAR_LOCKS != 0 {
                        ",clearLocks"
                    } else {
                        ""
                    },
                    if flags & SA_LATCH_TO_LOCK != 0 {
                        ",latchToLock"
                    } else {
                        ""
                    }
                )
            };
            format!("{}(group={group}{extra})", type_name(act[0]))
        }
        SA_MOVE_PTR => {
            let x = i32::from(i16::from_be_bytes([act[2], act[3]]));
            let y = i32::from(i16::from_be_bytes([act[4], act[5]]));
            format!(
                "MovePtr(x={},y={}{})",
                signed_text(x, flags & SA_MOVE_ABSOLUTE_X != 0),
                signed_text(y, flags & SA_MOVE_ABSOLUTE_Y != 0),
                if flags & SA_NO_ACCELERATION != 0 {
                    ",!accel"
                } else {
                    ""
                }
            )
        }
        SA_PTR_BTN | SA_LOCK_PTR_BTN => {
            let button = if act[3] == 0 {
                "default".to_owned()
            } else {
                act[3].to_string()
            };
            let count = if act[2] != 0 {
                format!(",count={}", act[2])
            } else {
                String::new()
            };
            let affect = if act[0] == SA_LOCK_PTR_BTN {
                match flags & (SA_LOCK_NO_LOCK | SA_LOCK_NO_UNLOCK) {
                    SA_LOCK_NO_UNLOCK => ",affect=lock",
                    SA_LOCK_NO_LOCK => ",affect=unlock",
                    3 => ",affect=neither",
                    _ => ",affect=both",
                }
            } else {
                ""
            };
            format!("{}(button={button}{count}{affect})", type_name(act[0]))
        }
        SA_SET_PTR_DFLT => format!(
            "SetPtrDflt(affect=button,button={})",
            signed_text(i32::from(act[3] as i8), flags & SA_ABSOLUTE != 0)
        ),
        SA_TERMINATE => "Terminate()".to_owned(),
        SA_SWITCH_SCREEN => format!(
            "SwitchScreen(screen={},{}same)",
            signed_text(i32::from(act[2] as i8), flags & SA_ABSOLUTE != 0),
            if flags & SA_SWITCH_APPLICATION != 0 {
                "!"
            } else {
                ""
            }
        ),
        SA_SET_CONTROLS | SA_LOCK_CONTROLS => {
            let ctrls = u32::from_be_bytes([act[2], act[3], act[4], act[5]]);
            let names: Vec<&str> = CONTROL_NAMES
                .iter()
                .filter(|(_, b)| ctrls & b != 0)
                .map(|(n, _)| *n)
                .collect();
            let list = if names.is_empty() {
                "none".to_owned()
            } else {
                names.join("+")
            };
            let affect = if act[0] == SA_LOCK_CONTROLS {
                lock_affect_text(flags)
            } else {
                ""
            };
            format!("{}(controls={list}{affect})", type_name(act[0]))
        }
        t if t >= 0x15 => format!(
            "Private(type=0x{t:02x},data[0]=0x{:02x},data[1]=0x{:02x},data[2]=0x{:02x},\
             data[3]=0x{:02x},data[4]=0x{:02x},data[5]=0x{:02x},data[6]=0x{:02x})",
            act[1], act[2], act[3], act[4], act[5], act[6], act[7]
        ),
        _ => "NoAction()".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_vmods(_: &str) -> Option<u16> {
        None
    }

    /// Every action text xkbcommon writes in the frozen keymaps' compat
    /// map encodes to the bytes Xorg's GetCompatMap reports for that
    /// interpret (xorg-xkb-pristine.txt `si` rows, us layout).
    #[test]
    fn parse_matches_xorg_interpret_actions() {
        let vmod = |n: &str| {
            [
                "NumLock",
                "Alt",
                "LevelThree",
                "Super",
                "LevelFive",
                "Meta",
                "Hyper",
                "ScrollLock",
            ]
            .iter()
            .position(|v| *v == n)
            .map(|i| 1u16 << i)
        };
        let cases = [
            (
                "LatchMods(modifiers=Shift,clearLocks,latchToLock)",
                "0203010100000000",
            ),
            ("LockMods(modifiers=NumLock)", "0300000000010000"),
            (
                "SetMods(modifiers=LevelThree,clearLocks)",
                "0101000000040000",
            ),
            (
                "SetMods(modifiers=modMapMods,clearLocks)",
                "0105000000000000",
            ),
            ("LockMods(modifiers=modMapMods)", "0304000000000000"),
            ("SetGroup(group=+1)", "0400010000000000"),
            ("LatchGroup(group=2)", "0504010000000000"),
            ("LockGroup(group=+1)", "0600010000000000"),
            ("LockGroup(group=-1)", "0600ff0000000000"),
            ("LockGroup(group=1)", "0604000000000000"),
            ("MovePtr(x=-1,y=+1)", "0700ffff00010000"),
            ("MovePtr(x=+0,y=+1)", "0700000000010000"),
            ("PtrBtn(button=default)", "0800000000000000"),
            ("PtrBtn(button=default,count=2)", "0800020000000000"),
            ("PtrBtn(button=1)", "0800000100000000"),
            ("LockPtrBtn(button=default,affect=lock)", "0902000000000000"),
            (
                "LockPtrBtn(button=default,affect=unlock)",
                "0901000000000000",
            ),
            ("SetPtrDflt(affect=button,button=1)", "0a04010100000000"),
            ("SetPtrDflt(affect=button,button=+1)", "0a00010100000000"),
            ("SetPtrDflt(affect=button,button=-1)", "0a0001ff00000000"),
            ("LockControls(controls=MouseKeys)", "0f00000000100000"),
            ("LockControls(controls=AccessXFeedback)", "0f00000001000000"),
            ("Terminate()", "0c00000000000000"),
            ("SwitchScreen(screen=1,!same)", "0d05010000000000"),
            (
                "Private(type=0x86,data[0]=0x2d,data[1]=0x56,data[2]=0x4d,data[3]=0x6f,\
                 data[4]=0x64,data[5]=0x65,data[6]=0x00)",
                "862d564d6f646500",
            ),
            ("LockMods(modifiers=Lock)", "0300020200000000"),
        ];
        for (text, hex) in cases {
            let got = parse_action(text, &vmod).expect(text);
            let want: Vec<u8> = (0..8)
                .map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap())
                .collect();
            assert_eq!(got.to_vec(), want, "{text}");
        }
    }

    /// The writer's text encodes back to the same bytes, for every action
    /// type xkbcommon can carry (modifiers written as the resolved mask).
    #[test]
    fn written_text_parses_back() {
        let acts: [Action; 12] = [
            [1, 1, 0x41, 0x41, 0, 0, 0, 0],
            [2, 3, 0x01, 0x01, 0, 0, 0, 0],
            [3, 2, 0x10, 0x10, 0, 0, 0, 0],
            [4, 0, 0, 0, 0, 0, 0, 0],
            [5, 4, 1, 0, 0, 0, 0, 0],
            [6, 0, 0xff, 0, 0, 0, 0, 0],
            [7, 1, 0xff, 0xff, 0, 1, 0, 0],
            [8, 0, 2, 3, 0, 0, 0, 0],
            [9, 3, 0, 0, 0, 0, 0, 0],
            [0x0a, 4, 1, 2, 0, 0, 0, 0],
            [0x0d, 5, 3, 0, 0, 0, 0, 0],
            [0x0f, 1, 0, 0, 0x02, 0x10, 0, 0],
        ];
        for a in acts {
            let text = action_text(&a);
            assert_eq!(parse_action(&text, &no_vmods).expect(&text), a, "{text}");
        }
    }
}
