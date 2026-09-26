use xkbcommon::xkb::Keymap;

/// The XKB keycode range of `keymap` as X11 sees it: from 8, the X minimum
/// and the minimum the keymap text declares (`minimum = 8;`), to its highest
/// keycode clamped to CARD8. xkbcommon's own minimum is the lowest *named*
/// keycode (9 for evdev, which names no keycode 8), and it moves when a
/// mapping change names keycode 8; Xorg's stays at 8 (captured MapNotify
/// `min=8`), matching the core range, and so no mapping change ever needs a
/// NewKeyboardNotify for it.
pub(super) fn clamped_keycode_bounds(keymap: &Keymap) -> (u8, u8) {
    let max = u8::try_from(keymap.max_keycode().raw().min(255))
        .unwrap_or(255)
        .max(8);
    (8, max)
}

/// Core (`GetKeyboardMapping`) view of an XKB keymap, laid out exactly as
/// Xorg's `XkbGetCoreMap` (xkb/xkbUtils.c) does per XKB protocol §12.4.
pub(crate) struct CoreKeyMap {
    pub min_keycode: u8,
    /// `keysyms_per_keycode`: one width for the whole map, as in Xorg.
    pub width: u8,
    /// `(max - min + 1) * width` keysyms, row per keycode from `min_keycode`.
    pub syms: Vec<u32>,
}

impl CoreKeyMap {
    /// Keysym rows for `[first, first + count)`; keycodes outside the map are NoSymbol.
    pub fn rows(&self, first: u8, count: u8) -> Vec<u32> {
        let w = usize::from(self.width);
        let mut out = vec![0u32; usize::from(count) * w];
        for i in 0..usize::from(count) {
            let kc = usize::from(first) + i;
            let Some(row) = kc.checked_sub(usize::from(self.min_keycode)) else {
                continue;
            };
            if let Some(src) = self.syms.get(row * w..(row + 1) * w) {
                out[i * w..(i + 1) * w].copy_from_slice(src);
            }
        }
        out
    }
}

/// Bounds-checked store into one core row (Xorg writes unchecked into a row it sized).
fn put(core: &mut [u32], i: usize, v: u32) {
    if let Some(slot) = core.get_mut(i) {
        *slot = v;
    }
}

/// Port of Xorg's `XkbGetCoreMap` (xkb/xkbUtils.c) over each key's groups
/// (one Vec per group, each its type's level count wide) from `min_kc`.
pub(super) fn core_map_from_groups(min_kc: u8, groups: &[Vec<Vec<u32>>]) -> CoreKeyMap {
    // Size pass (XkbGetCoreMap "determine sizes").
    let (mut max_syms, mut max_g1_width, mut max_groups) = (0usize, 0usize, 0usize);
    for key in groups {
        let mut tmp = 0usize;
        if let Some(g1) = key.first() {
            let w = g1.len();
            tmp += if w <= 2 { 2 } else { w + 2 };
            max_g1_width = max_g1_width.max(w);
        }
        if let Some(g2) = key.get(1) {
            let w = g2.len();
            if tmp <= 2 {
                tmp += if w < 2 { 2 } else { w };
            } else if w > 2 {
                tmp += w - 2;
            }
        }
        tmp += key.iter().skip(2).map(Vec::len).sum::<usize>();
        max_syms = max_syms.max(tmp);
        max_groups = max_groups.max(key.len());
    }
    // §12.4: room to replicate the widest group 1 across every group.
    max_syms = max_syms
        .max(max_groups * max_g1_width)
        .min(usize::from(u8::MAX));
    let width = max_syms;

    let mut syms = vec![0u32; groups.len() * width];
    for (key, core) in groups.iter().zip(syms.chunks_mut(width.max(1))) {
        let mut n_out = 2usize;
        if let Some(g1) = key.first() {
            for (n, &s) in g1.iter().enumerate() {
                put(core, if n < 2 { n } else { 2 + n }, s);
            }
            if g1.len() > 2 {
                n_out = g1.len();
            }
        }
        if key.len() == 1 {
            // One-group key: ABCDE on a multi-group map becomes ABABCDECDE[ABCDE…].
            let g1 = &key[0];
            let gw = g1.len();
            let (a, b) = (
                core.first().copied().unwrap_or(0),
                core.get(1).copied().unwrap_or(0),
            );
            if gw > 0 && width >= 3 {
                put(core, 2, a);
            }
            if gw > 1 && width >= 4 {
                put(core, 3, b);
            }
            let mut idx = 2 + gw;
            while gw > 2 && idx < width && idx < gw * 2 {
                core[idx] = core[idx - gw + 2];
                idx += 1;
            }
            idx = (2 * gw).max(4);
            for _ in 3..=max_groups {
                for &s in g1 {
                    if idx >= max_syms {
                        break;
                    }
                    put(core, idx, s);
                    idx += 1;
                }
            }
        }
        n_out += 2;
        if let Some(g2) = key.get(1) {
            for (n, &s) in g2.iter().enumerate() {
                put(core, if n < 2 { 2 + n } else { n_out + n - 2 }, s);
            }
            if g2.len() > 2 {
                n_out += g2.len() - 2;
            }
        }
        for g in key.iter().skip(2) {
            for &s in g {
                put(core, n_out, s);
                n_out += 1;
            }
        }
    }
    CoreKeyMap {
        min_keycode: min_kc,
        width: u8::try_from(width).unwrap_or(u8::MAX),
        syms,
    }
}

/// The real-modifier mask (X11 KeyButMask bits 0..=7) a key actually
/// activates in THIS keymap, by pressing it in a scratch xkb state and
/// reading the effective mods. Correct across layouts/options (e.g. the
/// lv3 chooser binds ISO_Level3_Shift to Mod5, not the keysym-table guess).
#[cfg(test)]
fn real_mod_mask_for_keycode(keymap: &Keymap, kc: u32) -> u8 {
    let mut st = xkbcommon::xkb::State::new(keymap);
    st.update_key(
        xkbcommon::xkb::Keycode::new(kc),
        xkbcommon::xkb::KeyDirection::Down,
    );
    let mut mask = 0u8;
    for (name, bit) in [
        ("Shift", 0x01u8),
        ("Lock", 0x02),
        ("Control", 0x04),
        ("Mod1", 0x08),
        ("Mod2", 0x10),
        ("Mod3", 0x20),
        ("Mod4", 0x40),
        ("Mod5", 0x80),
    ] {
        if st.mod_name_is_active(name, xkbcommon::xkb::STATE_MODS_EFFECTIVE) {
            mask |= bit;
        }
    }
    mask
}

/// The eight real modifiers as xkbcommon and `modifier_map` name them, in
/// X11 bit order (bit 0 Shift … bit 7 Mod5).
pub(super) const REAL_MOD_NAMES: [&str; 8] = [
    "Shift", "Lock", "Control", "Mod1", "Mod2", "Mod3", "Mod4", "Mod5",
];

/// XKB UseExtension reply (minor=0). Fixed 32 bytes.
/// Reports success and server protocol version 1.0.
pub(super) fn reply_use_extension() -> Vec<u8> {
    let mut r = vec![0u8; 32];
    r[0] = 1; // reply type
    r[1] = 1; // success
    // [2..4] sequence: rewritten by caller
    // [4..8] extra length in 4-byte units = 0
    r[8] = 1; // server-major
    r[9] = 0; // server-minor
    r
}

/// The XKB controls GetControls reports enabled: RepeatKeys (bit 0), so
/// xkbcommon enables auto-repeat by default. Also the `enabledControls` of
/// our ControlsNotify.
pub(super) const XKB_ENABLED_CONTROLS: u32 = 0x0000_0001;

/// XKB GetDeviceInfo reply (minor=24). The wire-correct *empty*
/// reply is 36 bytes, not 32 — `sizeof(xcb_xkb_get_device_info_reply_t)`
/// (verified via gcc on `xcb/xkb.h`) is 36 because the C struct
/// places `nameLen: CARD16` at offset 32 with 2 bytes of trailing
/// pad. xcb-based clients (xkbcommon-x11 inside `vkgears`,
/// `wezterm`, …) cast the libxcb reply pointer straight to that
/// struct and access `reply->nameLen` plus
/// `xcb_xkb_get_device_info_name(reply) = (char*)(reply + 1)`
/// — both **read past a 32-byte allocation**, producing garbage
/// atoms that the client then fans out as GetAtomName requests
/// (we saw 0xAE4BAA70, 0xAE4B5808, 22057 in the log). xkbcommon-x11
/// then errors out and returns NULL, so `vkgears` segfaults on the
/// resulting `xkb_keymap_ref(NULL)`.
///
/// We publish an empty keyboard: no LED feedbacks, no buttons, no
/// name string, no actions. That's still a 36-byte body — fixed
/// header + `nameLen=0` (2B) + pad-to-4 (2B) — with `length = 1`.
pub(super) fn reply_get_device_info() -> Vec<u8> {
    let mut r = vec![0u8; 36];
    r[0] = 1; // reply
    r[1] = 1; // deviceID = 1
    // [4..8] extra length = (36 - 32) / 4 = 1
    r[4..8].copy_from_slice(&1u32.to_le_bytes());
    // [8..10] present, [10..12] supported, [12..14] unsupported = 0
    // [14..16] nDeviceLedFBs = 0
    // [16] firstBtnWanted, [17] nBtnsWanted
    // [18] firstBtnRtrn, [19] nBtnsRtrn
    // [20..22] totalBtns = 0
    // [22] hasOwnState
    // [23] (padding/alignment)
    // [24..26] dfltKbdFB, [26..28] dfltLedFB
    // [28..32] devType atom = 0
    // [32..34] nameLen = 0
    // [34..36] pad align(4)
    r
}

/// XKB PerClientFlags reply (minor=21). Fixed 32 bytes.
/// Mirrors Xorg's reply shape: advertise the standard per-client flag
/// mask and report the requested value for changed bits. This keeps
/// clients that enable detectable auto-repeat from seeing an all-zero
/// capability/value pair.
pub(super) fn reply_per_client_flags(body: &[u8]) -> Vec<u8> {
    const XKB_PCF_ALL_FLAGS_MASK: u32 = 0x1f;

    let mut r = vec![0u8; 32];
    r[0] = 1; // reply type
    r[1] = 1; // deviceID
    // [2..4] sequence: rewritten by caller
    // [4..8] extra length in 4-byte units = 0
    r[8..12].copy_from_slice(&XKB_PCF_ALL_FLAGS_MASK.to_le_bytes());

    if body.len() >= 12 {
        let change = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
        let value = u32::from_le_bytes([body[8], body[9], body[10], body[11]]);
        let effective = value & change & XKB_PCF_ALL_FLAGS_MASK;
        r[12..16].copy_from_slice(&effective.to_le_bytes());
    }

    r
}

/// Minimal all-zero 32-byte reply for XKB minors that clients tolerate silently.
/// Only use for minors with no required reply content (e.g. SetControls has none).
pub(super) fn reply_minimal(minor: u8) -> Vec<u8> {
    log::debug!("xkb: unimplemented minor {minor}, returning minimal reply");
    let mut r = vec![0u8; 32];
    r[0] = 1; // reply type
    r[1] = 1; // deviceID — must match the value returned by reply_get_map
    // and reply_get_controls etc.; xkbcommon-x11 cross-validates the
    // deviceID across replies and tears down the keymap when it
    // doesn't agree. GTK3's startup path probes minors 4 (GetState)
    // and 21 (PerClientFlags) through here.
    r
}

/// Per-group layout list extracted from an XKB `symbols` KcCGST
/// component string by [`parse_symbols_layouts`].
///
/// Both fields are comma-joined, one entry per keyboard group, in
/// group-slot order (group 1 first). They feed straight into
/// xkbcommon `new_from_names(layout=…, variant=…)` (Task 1b-2's
/// `recompile_keymap`), so the shapes match RMLVO's `layout`/`variant`
/// convention: `layouts = "us,de,us"`, `variants = ",,"`.
// Consumed by the XkbGetKbdByName layout-switch path
// (`KmsBackend::load_keymap_by_components`).
pub(super) struct SymbolsLayouts {
    /// Comma-joined layout codes in group order (e.g. `"us,de,us"`).
    pub layouts: String,
    /// Comma-joined variants, one slot per layout (e.g. `",polytonic"`);
    /// empty string for a group with no variant.
    pub variants: String,
    /// Comma-joined RMLVO option group:variant entries extracted from the
    /// behaviour partials in the symbols string (e.g.
    /// `"caps:none,lv3:ralt_switch"`). Empty string if none. These map
    /// straight onto xkbcommon `new_from_names(options=…)`. The
    /// `level3(ralt_switch)` chooser is the load-bearing one: it binds
    /// RAlt→ISO_Level3_Shift→Mod5, which is what makes AltGr (level 2/3)
    /// reachable. Dropping it leaves RAlt as Mod1 and breaks `€`/Belgian.
    pub options: String,
}

/// Maps a behaviour-partial bare name (the head of a `name(variant)`
/// symbols segment) to its RMLVO option *group* prefix, or `None` if the
/// name is not a recognised option partial. A recognised partial with a
/// variant becomes `group:variant` (e.g. `level3(ralt_switch)` →
/// `lv3:ralt_switch`, `capslock(none)` → `caps:none`).
///
/// NB: `pc*` models and the `inet*` family are NOT options — they carry
/// no chooser and are skipped entirely (see [`is_extra`]). This table is
/// the option subset of `SYMBOLS_EXTRAS`.
fn option_group_for(name: &str) -> Option<&'static str> {
    match name {
        "level3" | "lv3" => Some("lv3"),
        "level5" | "lv5" => Some("lv5"),
        "capslock" | "caps" => Some("caps"),
        "group" | "grp" => Some("grp"),
        "compose" => Some("compose"),
        "ctrl" => Some("ctrl"),
        "eurosign" => Some("eurosign"),
        "nbsp" => Some("nbsp"),
        "kpdl" => Some("kpdl"),
        "keypad" => Some("keypad"),
        _ => None,
    }
}

/// Non-layout tokens an XKB `symbols` string carries alongside the
/// real layouts — keyboard model (`pc`/`pc104`/…) and the various
/// behaviour partials (`inet(evdev)`, `group(…)`, `compose(…)`, …).
/// Matched against the bare token before any `(`/`:` suffix; the
/// `inet`/`group`/… entries are prefix-matched (see [`is_extra`]).
const SYMBOLS_EXTRAS: &[&str] = &[
    "pc",
    "pc104",
    "pc105",
    "pc101",
    "pc102",
    "inet",
    "group",
    "grp",
    "compose",
    // NB: no `"lv"` — `lv` is the real Latvian layout
    // (/usr/share/X11/xkb/symbols/lv, in evdev rules), not a level
    // partial. The level partials are `level2`/`level3`/`level5`, all
    // covered by the `"level"` prefix entry above; a `"lv"` prefix
    // would silently swallow Latvian and break fail-closed.
    "level",
    "terminate",
    "capslock",
    "ctrl",
    "keypad",
    "kpdl",
    "eurosign",
    "srvr_ctrl",
    "nbsp",
];

/// True if `token` is a recognised non-layout extra (model / behaviour
/// partial) that should be skipped, not treated as a layout. The
/// `inet`/`group`/`grp`/`compose`/`level` families are matched by
/// prefix because they appear as `inet(evdev)`, `group(alts_toggle)`,
/// … and the parenthesised part is already stripped before this call,
/// but the bare head (`inet`) is what we compare.
fn is_extra(token: &str) -> bool {
    SYMBOLS_EXTRAS.iter().any(|&e| {
        // Prefix-matched families vs. exact model strings. `inet`,
        // `group`, `grp`, `compose`, `level` are the partial
        // namespaces; the rest (`pc*`, `capslock`, …) match exactly.
        matches!(e, "inet" | "group" | "grp" | "compose" | "level")
            .then(|| token.starts_with(e))
            .unwrap_or(token == e)
    })
}

/// True if `token` looks like a layout code: lowercase ASCII letters,
/// length 2..=8, optionally followed by ASCII digits (e.g. `us`, `de`,
/// `gr`, `latam`, `dvorak`). Deliberately conservative — anything that
/// doesn't fit this shape is treated as ambiguous and rejected by the
/// caller (fail-closed).
fn looks_like_layout(token: &str) -> bool {
    let len = token.len();
    if !(2..=8).contains(&len) {
        return false;
    }
    let mut chars = token.chars();
    // First char must be a lowercase ASCII letter.
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        _ => return false,
    }
    // Remaining: lowercase letters, then optionally digits — but never
    // a letter after a digit (keeps it to `<alpha><digits>`).
    let mut seen_digit = false;
    for c in chars {
        if c.is_ascii_lowercase() {
            if seen_digit {
                return false;
            }
        } else if c.is_ascii_digit() {
            seen_digit = true;
        } else {
            return false;
        }
    }
    true
}

/// Parse an XKB `symbols` KcCGST component string into a per-group
/// layout list, or `None` if any segment is ambiguous.
///
/// When a client (Cinnamon) sends `XkbGetKbdByName`, the `symbols`
/// component encodes a multi-group layout as `+`-joined segments, e.g.
/// the captured `pc+us+de:2+us:3+inet(evdev)` (us=group 1, de=group 2,
/// us=group 3; the `:N` suffix is the 1-based group slot). This
/// extracts `layouts = "us,de,us"` / `variants = ",,"` so a later task
/// can recompile a multi-group keymap via xkbcommon
/// `new_from_names(layout=…, variant=…)`.
///
/// This is a NARROW heuristic for the desktop layout-switch path, NOT
/// general KcCGST→RMLVO inversion, and it MUST FAIL CLOSED: any segment
/// that is neither a recognised layout ([`looks_like_layout`]) nor a
/// known non-layout extra ([`is_extra`]) returns `None`, so the caller
/// keeps the current keymap rather than guessing wrong (a silently
/// wrong layout is worse than no switch).
///
/// Each segment is `<layout>[(<variant>)][:<N>]`. The `:N` places the
/// layout at group slot `N-1`; segments without `:N` fill sequentially
/// from slot 0. Sparse slots (gaps) are malformed → `None`. Zero
/// layouts found → `None`.
pub(super) fn parse_symbols_layouts(symbols: &str) -> Option<SymbolsLayouts> {
    // (slot_index, layout, variant) for each layout segment.
    let mut placed: Vec<(usize, String, String)> = Vec::new();
    // Next sequential slot for a segment without an explicit `:N`.
    let mut next_seq = 0usize;
    // RMLVO option `group:variant` entries from the behaviour partials.
    let mut options: Vec<String> = Vec::new();

    for segment in symbols.split('+') {
        if segment.is_empty() {
            continue;
        }
        // Strip a trailing `:N` group-slot suffix.
        let (head, explicit_slot) = match segment.rsplit_once(':') {
            Some((before, n)) => {
                let slot = n.parse::<usize>().ok()?;
                if slot == 0 {
                    return None; // 1-based; :0 is malformed
                }
                (before, Some(slot - 1))
            }
            None => (segment, None),
        };
        // Strip an optional `(variant)` suffix.
        let (token, variant) = match head.split_once('(') {
            Some((tok, rest)) => {
                let variant = rest.strip_suffix(')')?; // unbalanced paren → malformed
                (tok, variant)
            }
            None => (head, ""),
        };

        // A recognised behaviour partial with a `(variant)` is an RMLVO
        // option (`level3(ralt_switch)` → `lv3:ralt_switch`), not a
        // layout. Capture it and move on; a variant-less option partial
        // (e.g. bare `compose`) carries no chooser, so just skip it.
        if let Some(group) = option_group_for(token) {
            if !variant.is_empty() {
                options.push(format!("{group}:{variant}"));
            }
            continue;
        }
        if is_extra(token) {
            // Pure model / `inet` extra (no option) — skip. A `:N` on an
            // extra is unexpected but harmless; it doesn't claim a slot.
            continue;
        }
        if !looks_like_layout(token) {
            // Ambiguous (neither layout nor known extra) → fail closed.
            return None;
        }

        let slot = match explicit_slot {
            Some(s) => s,
            None => {
                let s = next_seq;
                next_seq += 1;
                s
            }
        };
        placed.push((slot, token.to_string(), variant.to_string()));
    }

    if placed.is_empty() {
        return None;
    }

    // Order by slot and require a dense 0..n range (no gaps, no dupes).
    placed.sort_by_key(|(slot, _, _)| *slot);
    for (expected, (slot, _, _)) in placed.iter().enumerate() {
        if *slot != expected {
            return None; // sparse / duplicate slot → malformed
        }
    }

    let layouts = placed
        .iter()
        .map(|(_, l, _)| l.as_str())
        .collect::<Vec<_>>()
        .join(",");
    let variants = placed
        .iter()
        .map(|(_, _, v)| v.as_str())
        .collect::<Vec<_>>()
        .join(",");
    Some(SymbolsLayouts {
        layouts,
        variants,
        options: options.join(","),
    })
}

/// XKB GetState reply (minor 4) — `xkbGetStateReply` (sz=32, length=0).
/// Layout (XKBproto.h `_xkbGetStateReply`):
///   `mods@8 baseMods@9 latchedMods@10 lockedMods@11 group@12
///    lockedGroup@13 baseGroup:INT16@14 latchedGroup:INT16@16
///    compatState@18 grabMods@19 compatGrabMods@20 lookupMods@21
///    compatLookupMods@22 pad1@23 ptrBtnState:CARD16@24 pad2@26 pad3@28`.
///
/// Derived from the live `xkb_state` plus the authoritative
/// `locked_group` yserver stamps into events. yserver has no base/latched
/// group, so effective group == locked group == `locked_group`. The
/// modifier masks come straight from xkbcommon (`serialize_mods`); only
/// the low 8 bits (real mods Shift/Lock/Control/Mod1..Mod5) are wire
/// CARD8s. compat/grab/lookup/ptrBtn state are all zero (yserver tracks
/// no passive grabs or compat-state). A steady group-0/no-lock state
/// therefore byte-matches the all-zero Xorg capture (trace 4271/10952).
pub(super) fn reply_get_state(state: &xkbcommon::xkb::State, locked_group: u8) -> Vec<u8> {
    // Real-mod masks are the low 8 bits of the serialized mask.
    let effective = state.serialize_mods(xkbcommon::xkb::STATE_MODS_EFFECTIVE) as u8;
    let base = state.serialize_mods(xkbcommon::xkb::STATE_MODS_DEPRESSED) as u8;
    let latched = state.serialize_mods(xkbcommon::xkb::STATE_MODS_LATCHED) as u8;
    let locked = state.serialize_mods(xkbcommon::xkb::STATE_MODS_LOCKED) as u8;

    let mut r = vec![0u8; 32];
    r[0] = 1; // reply type
    r[1] = 1; // deviceID = 1
    // [2..4] sequence: rewritten by the caller
    // [4..8] length = 0
    r[8] = effective; // mods (effective real-mod mask)
    r[9] = base; // baseMods
    r[10] = latched; // latchedMods
    r[11] = locked; // lockedMods
    r[12] = locked_group; // group (effective == locked; no base/latched group)
    r[13] = locked_group; // lockedGroup
    // [14..16] baseGroup:INT16 = 0, [16..18] latchedGroup:INT16 = 0
    // [18] compatState, [19] grabMods, [20] compatGrabMods,
    // [21] lookupMods, [22] compatLookupMods, [23] pad1,
    // [24..26] ptrBtnState:CARD16 = 0, [26..28] pad2, [28..32] pad3 — all 0.
    r
}

/// Minimal empty `xkbGetGeometryReply` (XKBproto.h:796-815,
/// `sz_xkbGetGeometryReply` = 32). `name`=0, all section counts 0 — a
/// structurally-valid empty geometry, the embedded block for
/// `XkbGBN_GeometryMask`.
///
/// `found`=FALSE: libxkbcommon dropped XKB geometry entirely (it never
/// compiles a `xkb_geometry` section), so we have no geometry to report.
/// Xorg only has geometry via xkbcomp, and no xkbcommon-x11/Wayland client
/// consumes XKB geometry — faithful geometry would need an xkbcomp-style
/// parser, which is deferred. Reporting found=FALSE (rather than TRUE over
/// an all-zero body) tells a client truthfully that no geometry is present.
fn reply_get_geometry() -> Vec<u8> {
    let mut r = vec![0u8; 32];
    r[0] = 1; // reply type
    r[1] = 1; // deviceID = 1
    // [4..8] length = 0 (no trailing geometry sections)
    // [8..12] name atom = 0
    r[12] = 0; // found = FALSE (no geometry; see doc-comment above)
    // [13] pad, [14..] widthMM/heightMM/nProperties/.../labelColorNdx = 0
    r
}

/// Convert the X-protocol XkbGBN_* component mask the client sends in
/// `want`/`need` into the set of components actually located/embeddable,
/// mirroring Xorg's `XkbConvertGetByNameComponents` round-trip
/// (xkbfmisc.c:397). Maps to XKM-space and back: `XkbGBN_SymbolsMask` (both
/// Client+Server symbol bits) collapses to one XKM symbols bit and expands
/// back to BOTH; any nonzero result implies `XkbGBN_OtherNamesMask`. The
/// `OtherNames` pseudo-component has no XKM bit, so it never survives the
/// round-trip unless re-added by the `orig != 0` clause.
fn convert_gbn_components(orig: u16) -> u16 {
    use crate::kms::xkb_desc::reply::{
        GBN_CLIENT_SYMBOLS, GBN_COMPAT_MAP, GBN_GEOMETRY, GBN_INDICATOR_MAP, GBN_KEY_NAMES,
        GBN_OTHER_NAMES, GBN_SERVER_SYMBOLS, GBN_TYPES,
    };
    // toXkm: GBN bits -> XKM bits.
    let mut xkm: u16 = 0;
    if orig & GBN_TYPES != 0 {
        xkm |= 1 << 0; // XkmTypesMask
    }
    if orig & GBN_COMPAT_MAP != 0 {
        xkm |= 1 << 1; // XkmCompatMapMask
    }
    if orig & (GBN_CLIENT_SYMBOLS | GBN_SERVER_SYMBOLS) != 0 {
        xkm |= 1 << 2; // XkmSymbolsMask
    }
    if orig & GBN_INDICATOR_MAP != 0 {
        xkm |= 1 << 3; // XkmIndicatorsMask
    }
    if orig & GBN_KEY_NAMES != 0 {
        xkm |= 1 << 4; // XkmKeyNamesMask
    }
    if orig & GBN_GEOMETRY != 0 {
        xkm |= 1 << 5; // XkmGeometryMask
    }
    // fromXkm: XKM bits -> GBN bits.
    let mut gbn: u16 = 0;
    if xkm & (1 << 0) != 0 {
        gbn |= GBN_TYPES;
    }
    if xkm & (1 << 1) != 0 {
        gbn |= GBN_COMPAT_MAP;
    }
    if xkm & (1 << 2) != 0 {
        gbn |= GBN_CLIENT_SYMBOLS | GBN_SERVER_SYMBOLS;
    }
    if xkm & (1 << 3) != 0 {
        gbn |= GBN_INDICATOR_MAP;
    }
    if xkm & (1 << 4) != 0 {
        gbn |= GBN_KEY_NAMES;
    }
    if xkm & (1 << 5) != 0 {
        gbn |= GBN_GEOMETRY;
    }
    if xkm != 0 {
        gbn |= GBN_OTHER_NAMES;
    }
    gbn
}

/// Build the `XkbGetKbdByName` (minor 23) reply, as `ProcXkbGetKbdByName`
/// does: a fixed 32-byte `xkbGetKbdByNameReply` header (XKBproto.h:904-920)
/// followed by full nested component replies, one per reported component,
/// in XkbGBN bit order, each with its own header: GetMap (the parts the
/// reported symbols and types carry), GetCompatMap (all interprets, all
/// groups), GetIndicatorMap (all), GetNames (every name for OtherNames, key
/// names and aliases for KeyNames), Geometry.
///
/// `reported` = `convert_gbn_components(want | need)`. `found` is the
/// component mask located: `reported & ~OtherNames` on success (the
/// captured Xorg reply's found=0x7f vs reported=0xff), 0 on failure.
/// Geometry is name-only (#171 open question 1): its block reports
/// found=False.
///
/// Ground truth: cinnamon-xorg.xtrace:6202 (header found=0x7f reported=0xff
/// loaded=1, first embedded block = 40-byte-header GetMap).
pub(super) fn reply_get_kbd_by_name(
    desc: &crate::kms::xkb_desc::XkbDesc,
    want: u16,
    need: u16,
    loaded: bool,
    intern_atom: &mut dyn FnMut(&str) -> u32,
) -> Vec<u8> {
    use crate::kms::xkb_desc::reply::{
        self, GBN_CLIENT_SYMBOLS, GBN_COMPAT_MAP, GBN_GEOMETRY, GBN_INDICATOR_MAP, GBN_KEY_NAMES,
        GBN_OTHER_NAMES, GBN_SERVER_SYMBOLS, GBN_TYPES,
    };
    let reported = convert_gbn_components(want | need);
    let found: u16 = if loaded {
        reported & !GBN_OTHER_NAMES
    } else {
        0
    };
    let mut body: Vec<u8> = Vec::new();
    if reported & (GBN_TYPES | GBN_CLIENT_SYMBOLS | GBN_SERVER_SYMBOLS) != 0 {
        body.extend_from_slice(&reply::kbd_by_name_map(desc, reported));
    }
    if reported & GBN_COMPAT_MAP != 0 {
        body.extend_from_slice(&reply::encode_compat_map(desc, 0x0f, 0, desc.compat.len()));
    }
    if reported & GBN_INDICATOR_MAP != 0 {
        body.extend_from_slice(&reply::encode_indicator_map(desc, u32::MAX));
    }
    if reported & (GBN_KEY_NAMES | GBN_OTHER_NAMES) != 0 {
        body.extend_from_slice(&reply::kbd_by_name_names(desc, reported, intern_atom));
    }
    if reported & GBN_GEOMETRY != 0 {
        body.extend_from_slice(&reply_get_geometry());
    }
    debug_assert_eq!(body.len() % 4, 0, "nested blocks must be 4-byte aligned");
    let length_words = u32::try_from(body.len() / 4).unwrap_or(u32::MAX);

    let mut r = vec![0u8; 32 + body.len()];
    r[0] = 1; // type = Reply
    r[1] = 1; // deviceID = 1 — must match the embedded blocks' deviceID
    r[4..8].copy_from_slice(&length_words.to_le_bytes());
    r[8] = desc.min_key_code;
    r[9] = desc.max_key_code;
    r[10] = u8::from(loaded);
    r[11] = 0; // newKeyboard (BOOL) — the NKN is a separate broadcast event
    r[12..14].copy_from_slice(&found.to_le_bytes());
    r[14..16].copy_from_slice(&reported.to_le_bytes());
    r[32..].copy_from_slice(&body);
    r
}

/// The evdev/pc105 keymap the `testdata/xorg-*` goldens were captured
/// against, frozen as text (`xkbcli compile-keymap --format 1` over
/// xkeyboard-config 2.48, the version in the goldens' headers). Compiling
/// from RMLVO instead reads the host's xkeyboard-config, so a distro with
/// another version compiles a different keymap and every golden diff fails
/// on the input, not on our conversion (Ubuntu CI).
#[cfg(test)]
pub(crate) fn golden_keymap(layout: &str, options: Option<&str>) -> xkbcommon::xkb::Keymap {
    let text = match (layout, options) {
        ("us", None) => include_str!("testdata/xkb-keymap-us.xkb"),
        ("gb", None) => include_str!("testdata/xkb-keymap-gb.xkb"),
        ("de", None) => include_str!("testdata/xkb-keymap-de.xkb"),
        ("us,ru", Some("grp:alt_shift_toggle")) => include_str!("testdata/xkb-keymap-usru.xkb"),
        other => panic!("no frozen golden keymap for {other:?}"),
    };
    let ctx = xkbcommon::xkb::Context::new(xkbcommon::xkb::CONTEXT_NO_FLAGS);
    xkbcommon::xkb::Keymap::new_from_string(
        &ctx,
        text.to_owned(),
        xkbcommon::xkb::KEYMAP_FORMAT_TEXT_V1,
        xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS,
    )
    .expect("frozen golden keymap parses")
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::kms::xkb_desc::{XkbDesc, probe, reply};

    /// Build the `us,de` keymap matching the capture's RMLVO
    /// (`pc+us+de:2+us:3+inet(evdev)+pc(pc105)` → rules=evdev, model=pc105,
    /// layout=us,de). Source of the IndicatorMap golden vector.
    fn us_de_keymap() -> xkbcommon::xkb::Keymap {
        let ctx = xkbcommon::xkb::Context::new(xkbcommon::xkb::CONTEXT_NO_FLAGS);
        xkbcommon::xkb::Keymap::new_from_names(
            &ctx,
            "evdev",
            "pc105",
            "us,de",
            "",
            None,
            xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .expect("us,de keymap")
    }

    fn desc_of(km: &xkbcommon::xkb::Keymap) -> XkbDesc {
        XkbDesc::from_keymap(km).expect("seed")
    }

    fn names_body(which: u32) -> [u8; 8] {
        let mut b = [0u8; 8];
        b[0..2].copy_from_slice(&0x0100_u16.to_le_bytes());
        b[4..8].copy_from_slice(&which.to_le_bytes());
        b
    }

    /// The name details xkbcommon-x11's `get_names` asks for.
    const XKBCOMMON_NAMES: u32 = 0x1ff5;

    /// External golden-vector test for the populated IndicatorMap block.
    ///
    /// Ground truth: cinnamon-xorg.xtrace:6202 (`us,de`), decoded in
    /// docs/superpowers/findings/2026-06-25-xkb-indicator-compat-golden-vector.md.
    /// The full 416-byte reply (32-byte header + 32 × 12-byte
    /// `xkbIndicatorMapWireDesc`) is asserted byte-for-byte.
    #[test]
    fn indicator_map_matches_us_de_golden_vector() {
        let got = reply::encode_indicator_map(&desc_of(&us_de_keymap()), u32::MAX);

        let mut want = vec![0u8; 32 + 32 * 12];
        want[0] = 1; // type
        want[1] = 1; // deviceID
        want[4..8].copy_from_slice(&96u32.to_le_bytes()); // length = (416-32)/4
        want[8..12].copy_from_slice(&0xffff_ffffu32.to_le_bytes()); // which
        want[12..16].copy_from_slice(&0x0000_07ffu32.to_le_bytes()); // realIndicators
        want[16] = 32; // nIndicators
        let slot = |idx: usize, bytes: [u8; 12]| (idx, bytes);
        for (idx, bytes) in [
            // flags wG g  wM mods real vmods(le) ctrls(le)
            slot(0, [0x80, 0, 0, 4, 0x02, 0x02, 0x00, 0x00, 0, 0, 0, 0]), // Caps Lock: Lock
            slot(1, [0x80, 0, 0, 4, 0x10, 0x00, 0x01, 0x00, 0, 0, 0, 0]), // Num Lock: NumLock→Mod2
            slot(2, [0x00, 0, 0, 4, 0x00, 0x00, 0x80, 0x00, 0, 0, 0, 0]), // Scroll Lock
            slot(11, [0x80, 0, 0, 4, 0x01, 0x01, 0x00, 0x00, 0, 0, 0, 0]), // Shift Lock: Shift
            slot(12, [0x80, 8, 0xfe, 0, 0x00, 0x00, 0x00, 0x00, 0, 0, 0, 0]), // Group 2
            slot(13, [0x20, 0, 0, 0, 0x00, 0x00, 0x00, 0x00, 0x10, 0, 0, 0]), // Mouse Keys
        ] {
            let off = 32 + idx * 12;
            want[off..off + 12].copy_from_slice(&bytes);
        }
        assert_eq!(got, want, "IndicatorMap diverged from golden vector");
    }

    #[test]
    fn de_e_key_reaches_the_euro_through_its_four_level_type() {
        // Golden vector (docs/superpowers/findings/2026-06-25-altgr-4level-
        // golden-vector.md): the de `e` key (keycode 26) has a four-level
        // type whose LevelThree (Mod5 0x80) selects level 2 = € (0x20ac).
        let mut core = crate::kms::core::KmsCore::for_tests();
        core.recompile_keymap(&crate::kms::core::XkbRmlvo {
            layout: "de".into(),
            ..crate::kms::core::XkbRmlvo::default()
        });
        let desc = &core.xkb_desc;
        let t = usize::from(desc.keys[26].kt_index[0]);
        assert_eq!(desc.types[t].num_levels, 4);
        for (mods, level) in [(0x01u8, 1u8), (0x80, 2), (0x81, 3)] {
            assert_eq!(desc.type_level(t, mods).0, level, "mods {mods:#04x}");
        }
        let w = usize::from(desc.keys[26].width);
        assert_eq!(desc.keys[26].syms[2], 0x20ac);
        assert!(w >= 4);
    }

    #[test]
    fn parse_symbols_basic_multigroup() {
        // The exact strings Cinnamon sent in the capture.
        let r = parse_symbols_layouts("pc+us+de:2+us:3+inet(evdev)").expect("parses");
        assert_eq!(r.layouts, "us,de,us");
        assert_eq!(r.variants, ",,"); // no variants -> empty per group

        let r2 = parse_symbols_layouts("pc+us+us:2+inet(evdev)").expect("parses");
        assert_eq!(r2.layouts, "us,us");
    }

    #[test]
    fn parse_symbols_with_variant() {
        let r = parse_symbols_layouts("pc+us+gr(polytonic):2+inet(evdev)").expect("parses");
        assert_eq!(r.layouts, "us,gr");
        assert_eq!(r.variants, ",polytonic");
    }

    #[test]
    fn parse_symbols_single_layout() {
        let r = parse_symbols_layouts("pc+de+inet(evdev)").expect("parses");
        assert_eq!(r.layouts, "de");
    }

    #[test]
    fn parse_symbols_latvian_not_treated_as_extra() {
        // `lv` is the Latvian layout, not a level partial.
        let r = parse_symbols_layouts("pc+us+lv:2+inet(evdev)").expect("parses");
        assert_eq!(r.layouts, "us,lv");
        let r2 = parse_symbols_layouts("pc+lv+inet(evdev)").expect("parses");
        assert_eq!(r2.layouts, "lv");
    }

    #[test]
    fn parse_symbols_extracts_level3_chooser_option() {
        let r =
            parse_symbols_layouts("pc+us+be:2+us:3+inet(evdev)+capslock(none)+level3(ralt_switch)")
                .expect("parses");
        assert_eq!(r.layouts, "us,be,us");
        assert!(
            r.options.split(',').any(|o| o == "lv3:ralt_switch"),
            "level3(ralt_switch) -> lv3:ralt_switch, got {:?}",
            r.options
        );
        assert!(r.options.split(',').any(|o| o == "caps:none"));
    }

    #[test]
    fn parse_symbols_fail_closed_on_unknown() {
        assert!(parse_symbols_layouts("pc+us+wat_is_this_xyz:2+inet(evdev)").is_none());
    }

    #[test]
    fn parse_symbols_fail_closed_on_sparse_or_duplicate_slots() {
        assert!(parse_symbols_layouts("pc+us+de:3+inet(evdev)").is_none());
        assert!(parse_symbols_layouts("pc+us:1+de:1+inet(evdev)").is_none());
        assert!(parse_symbols_layouts("pc+us:1+de+inet(evdev)").is_none());
    }

    fn test_keymap() -> xkbcommon::xkb::Keymap {
        let ctx = xkbcommon::xkb::Context::new(xkbcommon::xkb::CONTEXT_NO_FLAGS);
        xkbcommon::xkb::Keymap::new_from_names(
            &ctx,
            "evdev",
            "pc105",
            "us",
            "",
            None,
            xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .expect("test xkb keymap")
    }

    fn get_map_request_body(full: u16, partial: u16) -> [u8; 24] {
        let mut body = [0u8; 24];
        body[0..2].copy_from_slice(&0x0100_u16.to_le_bytes()); // UseCoreKbd
        body[2..4].copy_from_slice(&full.to_le_bytes());
        body[4..6].copy_from_slice(&partial.to_le_bytes());
        body
    }

    fn full_map(desc: &XkbDesc) -> Vec<u8> {
        reply::encode_map(desc, reply::MapRequest::full(desc))
    }

    fn key_line(map: &[u8], kc: u8) -> String {
        probe::map_lines(map)
            .2
            .into_iter()
            .find(|(k, _)| *k == kc)
            .map(|(_, l)| l)
            .expect("key")
    }

    #[test]
    fn get_map_multigroup_serializes_all_groups() {
        // us,de: keycode 29 (AD06) is `y` in group 1 (us), `z` in group 2
        // (de). Ground truth: cinnamon-xorg.xtrace shows the de `z` keysym
        // once per group in the loaded multi-group map.
        let map = full_map(&desc_of(&us_de_keymap()));
        let line = key_line(&map, 29);
        assert!(line.contains(" gi=0x02 "), "two groups: {line}");
        // de's group is FOUR_LEVEL_SEMIALPHABETIC, so the width is 4.
        assert!(
            line.contains(" w=4 syms=79,59,0,0,7a,5a,"),
            "y Y z Z: {line}"
        );
    }

    #[test]
    fn use_extension_reply_length() {
        assert_eq!(reply_use_extension().len(), 32);
    }

    #[test]
    fn use_extension_success_flag() {
        assert_eq!(reply_use_extension()[1], 1, "success must be 1");
    }

    #[test]
    fn modifier_mapping_places_super_on_mod4() {
        // evdev/pc105/us: Super_L lives on Mod4, not Mod5; Alt on Mod1;
        // Control_L on Control.
        let (kpm, data) = desc_of(&test_keymap()).modifier_mapping();
        let kpm = usize::from(kpm);
        let row = |idx: usize| &data[idx * kpm..(idx + 1) * kpm];
        assert!(row(0).contains(&50), "Shift_L (50) on Shift row");
        assert!(row(2).contains(&37), "Control_L (37) on Control row");
        assert!(row(3).contains(&64), "Alt_L (64) on Mod1 row");
        assert!(row(6).contains(&133), "Super_L (133) on Mod4");
        assert!(!row(7).contains(&133), "Super_L not on Mod5");
    }

    #[test]
    fn get_controls_field_offsets_match_xkbproto() {
        // Offsets per XKBproto.h's xkbGetControlsReply. xkbcommon's
        // get_controls requires `0 < numGroups <= 4`.
        let r = reply::reply_get_controls(&desc_of(&test_keymap()));
        assert_eq!(r.len(), 92);
        assert_eq!((r[0], r[1]), (1, 1));
        assert_eq!(u32::from_le_bytes([r[4], r[5], r[6], r[7]]), 15);
        assert_eq!(r[9], 1, "numGroups");
        assert_eq!(r[10], 0x01, "groupsWrap (golden: groupsWrap=0x01)");
        assert_eq!(u16::from_le_bytes([r[20], r[21]]), 500);
        assert_eq!(u16::from_le_bytes([r[22], r[23]]), 33);
        let enabled = u32::from_le_bytes([r[56], r[57], r[58], r[59]]);
        assert_eq!(enabled & 0x01, 0x01, "RepeatKeys bit set");
    }

    /// GH #150: libX11 allocates `xkb->server->behaviors` from the
    /// `present` KeyBehaviors bit alone. Xorg sets the bit for a full
    /// request and emits an EMPTY section for an all-default keymap
    /// (xkb.c:1538-1549, and the `totalKeyBehaviors > 0` guard in
    /// XkbSendMap, xkb.c:1428). Raw offsets per XKBproto.h (present=12,
    /// firstKeyBehavior=25, nKeyBehaviors=26, totalKeyBehaviors=27).
    #[test]
    fn get_map_advertises_empty_key_behaviors_section() {
        let desc = desc_of(&test_keymap());
        let without =
            reply::reply_get_map(&desc, &get_map_request_body(0xff & !(1 << 5), 0)).expect("ok");
        assert_eq!(u16::from_le_bytes([without[12], without[13]]) & (1 << 5), 0);
        assert_eq!((without[25], without[26], without[27]), (0, 0, 0));

        let r = reply::reply_get_map(&desc, &get_map_request_body(0xff, 0)).expect("ok");
        assert_eq!(u16::from_le_bytes([r[12], r[13]]) & 0x20, 0x20);
        assert_eq!((r[25], r[26], r[27]), (8, 248, 0));
        assert_eq!(r.len(), without.len(), "an empty section adds no bytes");

        let only = reply::reply_get_map(&desc, &get_map_request_body(1 << 5, 0)).expect("ok");
        assert_eq!(u16::from_le_bytes([only[12], only[13]]), 1 << 5);
        assert_eq!((only[25], only[26], only[27]), (8, 248, 0));
        assert_eq!(only.len(), 40, "empty sections only: bare 40-byte reply");
    }

    /// GetKbdByName embeds the whole GetMap reply as its first block when
    /// types and both symbol parts are reported.
    #[test]
    fn get_kbd_by_name_map_block_carries_key_behaviors() {
        use crate::kms::xkb_desc::reply::{GBN_CLIENT_SYMBOLS, GBN_SERVER_SYMBOLS, GBN_TYPES};
        let desc = desc_of(&test_keymap());
        let map = full_map(&desc);
        let mut next_atom = 1u32;
        let gbn = reply_get_kbd_by_name(
            &desc,
            GBN_TYPES | GBN_CLIENT_SYMBOLS | GBN_SERVER_SYMBOLS,
            0,
            true,
            &mut |_name| {
                next_atom += 1;
                next_atom
            },
        );
        assert_eq!(&gbn[32..32 + map.len()], map.as_slice());
        assert_eq!(map[27], 0, "totalKeyBehaviors = 0");
        assert_eq!(u16::from_le_bytes([map[12], map[13]]) & 0x20, 0x20);
    }

    #[test]
    fn get_map_request_only_advertises_requested_parts() {
        let desc = desc_of(&test_keymap());
        let r = reply::reply_get_map(&desc, &get_map_request_body(0x03, 0)).expect("ok");
        assert_eq!(u16::from_le_bytes([r[12], r[13]]), 0x03);
        let length_words = u32::from_le_bytes([r[4], r[5], r[6], r[7]]) as usize;
        assert_eq!(length_words * 4 + 32, r.len());
        assert_eq!(usize::from(r[15]), desc.types.len());
        assert_eq!((r[17], r[20]), (8, 248));
        assert_eq!(&r[21..40], &[0u8; 19][..], "no other part, no virtual mods");
    }

    #[test]
    fn get_map_partial_key_range_is_checked_as_xorg() {
        // ProcXkbGetMap's CHK_KEY_RANGE(0x05, …): past maxKeyCode →
        // BadValue _XkbErrCode4(0x05, first, num, max).
        let desc = desc_of(&test_keymap());
        let mut body = get_map_request_body(0, 0x02);
        body[8] = 250;
        body[9] = 10;
        let err = reply::reply_get_map(&desc, &body).expect_err("out of range");
        assert_eq!(err.code, 2);
        assert_eq!(err.value, 0x05fa_0aff);
        // full and partial overlapping → BadMatch _XkbErrCode2(0x01, overlap).
        let err =
            reply::reply_get_map(&desc, &get_map_request_body(0x02, 0x02)).expect_err("overlap");
        assert_eq!((err.code, err.value), (8, 0x0100_0002));
    }

    /// GH #59: GetMap must carry the key actions (SetMods on the modifier
    /// keys): the Super key has SetMods for Mod4.
    #[test]
    fn get_map_emits_setmods_action_for_super_mod4() {
        let map = full_map(&desc_of(&test_keymap()));
        assert!(u16::from_le_bytes([map[22], map[23]]) > 0, "totalActs > 0");
        let line = key_line(&map, 133);
        assert!(line.contains(" acts=0105404000000000 "), "Super_L: {line}");
    }

    #[test]
    fn get_names_carries_the_vmod_names_and_bindings() {
        // evdev/pc105/us: the Super virtual modifier binds Mod4, Alt Mod1,
        // and Super_L (133) carries the Super bit in its vmodmap.
        let desc = desc_of(&test_keymap());
        let super_idx = desc
            .names
            .vmods
            .iter()
            .position(|n| n.as_deref() == Some("Super"))
            .expect("Super vmod");
        assert_eq!(desc.vmods[super_idx], 0x40);
        let alt = desc
            .names
            .vmods
            .iter()
            .position(|n| n.as_deref() == Some("Alt"));
        assert_eq!(alt.map(|i| desc.vmods[i]), Some(0x08));
        assert_ne!(desc.vmodmap[133] & (1 << super_idx), 0);
        let mut seen = Vec::new();
        let _ = reply::reply_get_names(&desc, &names_body(XKBCOMMON_NAMES), &mut |n| {
            seen.push(n.to_owned());
            7
        });
        assert!(seen.iter().any(|n| n == "Super"));
    }

    /// xkbcommon-x11's `get_names` asks for 0x1ff5 and reads the four
    /// component names unconditionally: they come back first, in bit order
    /// (Keycodes, Symbols, Types, Compat), as real atoms of the names the
    /// rules resolved to.
    #[test]
    fn get_names_answers_what_xkbcommon_asks_for() {
        let core = crate::kms::core::KmsCore::for_tests();
        let desc = &core.xkb_desc;
        let mut atoms = probe::Atoms::default();
        let r =
            reply::reply_get_names(desc, &names_body(XKBCOMMON_NAMES), &mut |n| atoms.intern(n))
                .expect("ok");
        let which = u32::from_le_bytes([r[8], r[9], r[10], r[11]]);
        assert_eq!(which, XKBCOMMON_NAMES);
        let map = full_map(desc);
        assert_eq!(
            (r[12], r[13]),
            (map[10], map[11]),
            "keycode range as GetMap"
        );
        assert_eq!(r[14], map[15], "nTypes as GetMap");
        let lines = probe::names_lines(&r, &atoms);
        for want in [
            "name keycodes 'evdev+aliases(qwerty)'",
            "name symbols 'pc+us+inet(evdev)'",
            "name types 'complete'",
            "name compat 'complete'",
            "keyname 9 'ESC'",
        ] {
            assert!(lines.iter().any(|l| l == want), "{want} in {lines:?}");
        }
    }

    #[test]
    fn get_names_symbols_reflects_layout() {
        let mut core = crate::kms::core::KmsCore::for_tests();
        core.recompile_keymap(&crate::kms::core::XkbRmlvo {
            layout: "de".into(),
            ..crate::kms::core::XkbRmlvo::default()
        });
        assert_eq!(
            core.xkb_desc.names.symbols.as_deref(),
            Some("pc+de+inet(evdev)")
        );
    }

    /// GetCompatMap carries every interpret and the group compat maps. It
    /// must not be empty: libX11's `_XkbReadGetCompatMapReply` fails on a
    /// zero-length reply, and `setxkbmap` with it.
    #[test]
    fn get_compat_map_carries_the_interprets_and_group_compat() {
        let desc = desc_of(&test_keymap());
        let mut body = [0u8; 8];
        body[2] = 0x0f; // groups
        body[3] = 1; // getAllSI
        let r = reply::reply_get_compat_map(&desc, &body).expect("ok");
        assert!(u32::from_le_bytes([r[4], r[5], r[6], r[7]]) > 0);
        assert_eq!(r[8], 0x0f);
        assert_eq!(
            usize::from(u16::from_le_bytes([r[12], r[13]])),
            desc.compat.len()
        );
        assert!(desc.compat.len() > 100, "the whole compat map");
        assert_eq!(
            &r[r.len() - 16..],
            &[
                0, 0, 0, 0, 0x80, 0x80, 0, 0, 0x80, 0x80, 0, 0, 0x80, 0x80, 0, 0
            ],
            "group compat (golden: groupcompat rows)"
        );
    }

    #[test]
    fn convert_gbn_matches_captured_request() {
        // The captured Cinnamon request: need=0x00bf, want=0x00ff →
        // reported 0x00ff (cinnamon-xorg.xtrace:6202).
        use crate::kms::xkb_desc::reply::{
            GBN_CLIENT_SYMBOLS, GBN_OTHER_NAMES, GBN_SERVER_SYMBOLS,
        };
        assert_eq!(convert_gbn_components(0x00ff | 0x00bf), 0x00ff);
        assert_eq!(
            convert_gbn_components(GBN_CLIENT_SYMBOLS),
            GBN_CLIENT_SYMBOLS | GBN_SERVER_SYMBOLS | GBN_OTHER_NAMES
        );
        assert_eq!(convert_gbn_components(0), 0);
    }

    #[test]
    fn get_kbd_by_name_reply_header_grounded_in_capture() {
        // cinnamon-xorg.xtrace:6202: loaded=1, found=0x7f, reported=0xff,
        // Xorg's evdev keycode range 8..=255.
        let desc = desc_of(&test_keymap());
        let r = reply_get_kbd_by_name(&desc, 0x00ff, 0x00bf, true, &mut |_| 1u32);
        assert_eq!((r[0], r[1]), (1, 1));
        assert_eq!((r[8], r[9], r[10], r[11]), (8, 255, 1, 0));
        assert_eq!(u16::from_le_bytes([r[12], r[13]]), 0x007f, "found");
        assert_eq!(u16::from_le_bytes([r[14], r[15]]), 0x00ff, "reported");
        let length_words = u32::from_le_bytes([r[4], r[5], r[6], r[7]]);
        assert_eq!(length_words as usize * 4, r.len() - 32);
        // The first block is a full 40-byte-header GetMap reply.
        let map = full_map(&desc);
        assert_eq!(&r[32..32 + map.len()], map.as_slice());
    }

    #[test]
    fn get_kbd_by_name_load_failed_clears_found() {
        let desc = desc_of(&test_keymap());
        let r = reply_get_kbd_by_name(&desc, 0x00ff, 0x00bf, false, &mut |_| 1u32);
        assert_eq!(r[10], 0, "loaded = FALSE");
        assert_eq!(u16::from_le_bytes([r[12], r[13]]), 0, "found = 0");
    }

    #[test]
    fn get_device_info_reply_matches_xcb_struct_size() {
        // `sizeof(xcb_xkb_get_device_info_reply_t)` is 36.
        let r = reply_get_device_info();
        assert_eq!(r.len(), 36);
        assert_eq!((r[0], r[1]), (1, 1));
        assert_eq!(u32::from_le_bytes([r[4], r[5], r[6], r[7]]), 1);
        assert_eq!(u16::from_le_bytes([r[32], r[33]]), 0, "nameLen = 0");
    }

    #[test]
    fn per_client_flags_reports_supported_and_requested_flags() {
        let mut body = vec![0u8; 24];
        body[4..8].copy_from_slice(&1u32.to_le_bytes()); // change DetectableAutoRepeat
        body[8..12].copy_from_slice(&1u32.to_le_bytes()); // value DetectableAutoRepeat
        let r = reply_per_client_flags(&body);
        assert_eq!(r.len(), 32);
        assert_eq!((r[0], r[1]), (1, 1));
        assert_eq!(u32::from_le_bytes(r[8..12].try_into().unwrap()), 0x1f);
        assert_eq!(u32::from_le_bytes(r[12..16].try_into().unwrap()), 1);
    }

    fn us_keymap() -> xkbcommon::xkb::Keymap {
        test_keymap()
    }

    /// GetState golden (trace 4271/10952): a steady group-0/no-lock state
    /// is an all-zero reply.
    #[test]
    fn get_state_steady_group0_matches_all_zero_golden() {
        let km = us_keymap();
        let state = xkbcommon::xkb::State::new(&km);
        let r = reply_get_state(&state, 0);
        let mut want = vec![0u8; 32];
        want[0] = 1;
        want[1] = 1;
        assert_eq!(r, want);
    }

    #[test]
    fn get_state_group1_reports_group_and_locked_group() {
        let km = us_de_keymap();
        let mut state = xkbcommon::xkb::State::new(&km);
        state.update_mask(0, 0, 0, 0, 0, 1);
        let r = reply_get_state(&state, 1);
        assert_eq!((r[12], r[13]), (1, 1));
    }

    #[test]
    fn get_state_caps_locked_sets_locked_lock_bit() {
        let km = us_keymap();
        let mut state = xkbcommon::xkb::State::new(&km);
        state.update_mask(0, 0, 0x02, 0, 0, 0);
        assert_ne!(reply_get_state(&state, 0)[11] & 0x02, 0);
    }

    fn named_indicator_body(requested_atom: u32) -> Vec<u8> {
        let mut body = vec![0u8; 12];
        body[8..12].copy_from_slice(&requested_atom.to_le_bytes());
        body
    }

    /// GetNamedIndicator golden (trace 87810/87812): the map fields match
    /// the IndicatorMap golden's Num Lock (slot 1) / Caps Lock (slot 0).
    #[test]
    fn get_named_indicator_matches_golden() {
        let desc = desc_of(&us_de_keymap());
        let mut atoms = probe::Atoms::default();
        let num = atoms.intern("Num Lock");
        let caps = atoms.intern("Caps Lock");
        let r = reply::reply_get_named_indicator(&desc, 0, &named_indicator_body(num), &mut |n| {
            atoms.intern(n)
        });
        assert_eq!(r.len(), 32);
        assert_eq!(u32::from_le_bytes(r[8..12].try_into().unwrap()), num);
        assert_eq!(&r[12..22], &[1, 0, 1, 1, 0x80, 0, 0, 0x04, 0x10, 0x00]);
        assert_eq!(u16::from_le_bytes(r[22..24].try_into().unwrap()), 0x0001);
        assert_eq!(u32::from_le_bytes(r[24..28].try_into().unwrap()), 0);
        assert_eq!(r[28], 1, "supported");

        let r = reply::reply_get_named_indicator(&desc, 1, &named_indicator_body(caps), &mut |n| {
            atoms.intern(n)
        });
        assert_eq!(&r[12..22], &[1, 1, 1, 0, 0x80, 0, 0, 0x04, 0x02, 0x02]);
        assert_eq!(u16::from_le_bytes(r[22..24].try_into().unwrap()), 0);
    }

    /// An unknown atom: `found`=0 and `ndx`=XkbNoIndicator (0xff), as
    /// `ProcXkbGetNamedIndicator` fills it; everything else zero but
    /// `supported`.
    #[test]
    fn get_named_indicator_unknown_atom_not_found() {
        let desc = desc_of(&us_de_keymap());
        let bogus = 0xDEAD_BEEF;
        let r =
            reply::reply_get_named_indicator(&desc, 0, &named_indicator_body(bogus), &mut |_| 1);
        assert_eq!(u32::from_le_bytes(r[8..12].try_into().unwrap()), bogus);
        assert_eq!(r[12], 0, "found");
        assert_eq!(r[15], 0xff, "ndx = XkbNoIndicator");
        let mut rest = r[13..28].to_vec();
        rest[2] = 0;
        assert!(rest.iter().all(|&b| b == 0));
        assert_eq!(r[28], 1, "supported");
    }

    /// Parse a `testdata/xorg-core-map-*.txt` dump into `(min, width, rows)`.
    fn parse_core_golden(text: &str) -> (u8, u8, std::collections::BTreeMap<u8, Vec<u32>>) {
        let mut min = 0;
        let mut width = 0;
        let mut rows = std::collections::BTreeMap::new();
        for line in text.lines() {
            if let Some(hdr) = line.strip_prefix("# min=") {
                let f: Vec<&str> = hdr.split([' ', '=']).collect();
                min = f[0].parse().unwrap();
                width = f[4].parse().unwrap();
            } else if !line.starts_with('#') && !line.is_empty() {
                let mut it = line.split(' ');
                let kc: u8 = it.next().unwrap().parse().unwrap();
                let syms = it.map(|h| u32::from_str_radix(h, 16).unwrap()).collect();
                rows.insert(kc, syms);
            }
        }
        (min, width, rows)
    }

    /// Diff our core map (the model's `XkbGetCoreMap`) against an Xorg
    /// dump, key by key.
    fn core_map_diffs(layout: &str, options: Option<&str>, golden: &str) -> Vec<String> {
        let (min, width, rows) = parse_core_golden(golden);
        let ours = desc_of(&golden_keymap(layout, options)).core_map();
        let mut diffs = Vec::new();
        if (ours.min_keycode, ours.width) != (min, width) {
            diffs.push(format!(
                "min/keysyms_per_keycode: ours {}/{} xorg {min}/{width}",
                ours.min_keycode, ours.width
            ));
            return diffs;
        }
        let got = ours.rows(min, 255 - min + 1);
        for (i, row) in got.chunks(usize::from(width)).enumerate() {
            let kc = u8::try_from(usize::from(min) + i).unwrap();
            let want = rows
                .get(&kc)
                .cloned()
                .unwrap_or_else(|| vec![0; usize::from(width)]);
            if row != want.as_slice() {
                diffs.push(format!("keycode {kc}: ours {row:x?} xorg {want:x?}"));
            }
        }
        diffs
    }

    /// Golden: Xvfb (xorg-server 21.1.24) core map for evdev/pc105/`us`.
    #[test]
    fn core_map_matches_xorg_us() {
        let d = core_map_diffs("us", None, include_str!("testdata/xorg-core-map-us.txt"));
        assert!(d.is_empty(), "{}", d.join("\n"));
    }

    /// Golden: Xvfb core map for `gb` (GH #168 reporter's layout).
    #[test]
    fn core_map_matches_xorg_gb() {
        let d = core_map_diffs("gb", None, include_str!("testdata/xorg-core-map-gb.txt"));
        assert!(d.is_empty(), "{}", d.join("\n"));
    }

    /// Golden: Xvfb core map for `de` (level-3 AltGr keys, CDECDE replication).
    #[test]
    fn core_map_matches_xorg_de() {
        let d = core_map_diffs("de", None, include_str!("testdata/xorg-core-map-de.txt"));
        assert!(d.is_empty(), "{}", d.join("\n"));
    }

    /// Golden: Xvfb core map for two groups `us,ru` + `grp:alt_shift_toggle`.
    #[test]
    fn core_map_matches_xorg_us_ru() {
        let d = core_map_diffs(
            "us,ru",
            Some("grp:alt_shift_toggle"),
            include_str!("testdata/xorg-core-map-usru.txt"),
        );
        assert!(d.is_empty(), "{}", d.join("\n"));
    }

    /// `(keycodes_per_modifier, data)` for one `[layout]` of `testdata/xorg-modmap.txt`.
    fn modmap_golden(layout: &str) -> (u8, Vec<u8>) {
        let text = include_str!("testdata/xorg-modmap.txt");
        let mut lines = text
            .lines()
            .skip_while(|l| *l != format!("[{layout}]"))
            .skip(1);
        let kpm: u8 = lines
            .next()
            .and_then(|l| l.strip_prefix("# keycodes_per_modifier="))
            .expect("kpm header")
            .parse()
            .unwrap();
        let mut data = Vec::new();
        for (row, line) in lines.take(8).enumerate() {
            let mut it = line.split(' ').map(|v| v.parse::<u8>().unwrap());
            assert_eq!(it.next(), u8::try_from(row).ok());
            data.extend(it);
        }
        assert_eq!(data.len(), 8 * usize::from(kpm));
        (kpm, data)
    }

    fn assert_modmap_matches_xorg(layout: &str, options: Option<&str>, case: &str) {
        let rows = |(kpm, d): &(u8, Vec<u8>)| -> Vec<(usize, Vec<u8>)> {
            d.chunks(usize::from(*kpm).max(1))
                .map(<[u8]>::to_vec)
                .enumerate()
                .collect()
        };
        let got = desc_of(&golden_keymap(layout, options)).modifier_mapping();
        let want = modmap_golden(case);
        assert_eq!((got.0, rows(&got)), (want.0, rows(&want)), "{case}");
    }

    #[test]
    fn modifier_mapping_matches_xorg_us() {
        assert_modmap_matches_xorg("us", None, "us");
    }

    #[test]
    fn modifier_mapping_matches_xorg_gb() {
        assert_modmap_matches_xorg("gb", None, "gb");
    }

    #[test]
    fn modifier_mapping_matches_xorg_de() {
        assert_modmap_matches_xorg("de", None, "de");
    }

    #[test]
    fn modifier_mapping_matches_xorg_us_ru() {
        assert_modmap_matches_xorg("us,ru", Some("grp:alt_shift_toggle"), "usru");
    }

    /// Golden (Xvfb 21.1.24, `testdata/xorg-per-key-repeat.txt`): the
    /// per-key repeat Xorg derives for its startup keymap (evdev/pc105/us),
    /// which seeds the keyboard's (`XkbFinishInit`). Keys whose level 1 is
    /// empty but a later level matches an interpret (<ALT>, <META>, <SUPR>)
    /// repeat: `XkbApplyCompatMapToKey` sets the bit when level 1 matched
    /// nothing.
    #[test]
    fn per_key_repeat_matches_xorg_startup_keymap() {
        let text = include_str!("testdata/xorg-per-key-repeat.txt");
        let bits = text
            .lines()
            .find_map(|l| l.strip_prefix("repeat: "))
            .expect("repeat line");
        let want: Vec<u8> = (0..32)
            .map(|i| u8::from_str_radix(&bits[2 * i..2 * i + 2], 16).unwrap())
            .collect();
        assert!(text.contains("query: layout:     us"));
        let got = desc_of(&golden_keymap("us", None)).per_key_repeat;
        let diff: Vec<u8> = (8..=255u8)
            .filter(|&kc| {
                let (i, b) = (usize::from(kc >> 3), 1u8 << (kc & 7));
                got[i] & b != want[i] & b
            })
            .collect();
        assert!(
            diff.is_empty(),
            "per-key repeat differs from Xorg for {diff:?}"
        );
    }

    /// The ISO_Level3_Shift key of `us,be,us` + `lv3:ralt_switch` cooks
    /// Mod5, not Mod1.
    #[test]
    fn modmap_binds_iso_level3_to_mod5_not_mod1() {
        let mut core = crate::kms::core::KmsCore::for_tests();
        core.recompile_keymap(&crate::kms::core::XkbRmlvo {
            rules: "evdev".into(),
            model: "pc105".into(),
            layout: "us,be,us".into(),
            variant: ",,".into(),
            options: Some("lv3:ralt_switch".into()),
        });
        let km = &core.xkb_keymap.0;
        let mut found = false;
        for kc in 8u32..=255 {
            let k = xkbcommon::xkb::Keycode::new(kc);
            if km.key_get_syms_by_level(k, 0, 0).first().map(|s| s.raw()) == Some(0xfe03) {
                let m = real_mod_mask_for_keycode(km, kc);
                assert_eq!(
                    m & 0x80,
                    0x80,
                    "ISO_Level3_Shift kc{kc} must bind Mod5, got {m:#04x}"
                );
                assert_eq!(m & 0x08, 0x00, "...and NOT Mod1");
                found = true;
            }
        }
        assert!(found, "be keymap must have an ISO_Level3_Shift key");
    }
}
