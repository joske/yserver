# Plan: runtime keymap mutation reaches XKB (#171)

Status: DRAFT, for review. Not implemented.
Issue: #171 — `xmodmap` (ChangeKeyboardMapping / SetModifierMapping) and
`xkbcomp keymap.xkb $DISPLAY` (XKB SetMap & co) don't change the keymap that
XKB clients and yserver's own key cooking use. On Xorg they do.

## 1. Where we are (code map, 2026-09-25, master `ad71461c`)

**Core requests**
- `ChangeKeyboardMapping` (100) → `handle_change_keyboard_mapping`
  (`yserver-core/src/core_loop/process_request.rs:28180`) → `apply_keymap_change`
  → `Backend::change_keyboard_mapping` (KMS: `kms/render/backend.rs:27361`), which
  only writes the overlay `KmsCore::core_map_overrides` via
  `xkb::apply_core_mapping_change` (a port of `XkbUpdateKeyTypesFromCore`,
  `kms/xkb.rs:505`). Core `MappingNotify(Keyboard)` goes to all clients. No XKB
  event.
- XI1 `ChangeDeviceKeyMapping` shares that path (`process_request.rs:19424`).
- `SetModifierMapping` (118) → `handle_set_modifier_mapping`
  (`process_request.rs:24166`) only stores `ServerState::modifier_mapping_override`
  (core loop, no backend call), always replies Success (no MappingBusy), and sends
  `MappingNotify(Modifier)`.
- Readers: `GetKeyboardMapping` / `GetDeviceKeyMapping` see `core_map_overrides`
  (through `core_keyboard_map`). `GetModifierMapping` sees
  `modifier_mapping_override`. **XKB GetMap sees neither**, and neither does
  `cook_host_key`, which drives `core.xkb_state` over the untouched
  `core.xkb_keymap`. That is the whole bug: every XKB client (xkbcommon, GTK, Qt,
  xterm) and our own key cooking still run on the old map.

**XKB requests** (`handle_xkb_request`, `process_request.rs:20430`, then
`Backend::xkb_proxy`, `backend.rs:26771`): SetControls(7), SetMap(9),
SetCompatMap(11), SetIndicatorMap(14), SetNamedIndicator(16), SetNames(18),
SetGeometry(20) and SetDeviceInfo(25) all hit
`1 | 3 | 5 | 7 | 9 | 11 | 14 | 16 | 18 | 20 | 25 => None`: accepted, no effect.

**XKB events**: only NewKeyboardNotify, MapNotify and StateNotify encoders exist
(`yserver-protocol/src/x11/mod.rs:3553/3600/3672`). There are no
Names/CompatMap/Controls/Indicator notify encoders. Subscriber masks live in
`ServerState::xkb_select_event_masks`, queried via `xkb_layout::subscribers`.

**Keymap swap**: `KmsCore::recompile_keymap` (RMLVO; no-op when RMLVO is equal)
→ `install_keymap` (`kms/core.rs:2161`: fresh state, replays `down_keys`, clears
`core_map_overrides`). `load_keymap_by_components` (GetKbdByName) resets
`locked_group`; `set_keymap_rmlvo` (the `_XKB_RULES_NAMES` hook) doesn't. LEDs
resync only on the next key event.

**Keymap text**: production already serializes the live keymap
(`get_as_string`, V1) and parses it ad hoc in three places (`xkb.rs:262`, `:762`,
`:2197`). Nothing builds a keymap from text at runtime yet; `new_from_string` is
test-only (`golden_keymap`).

## 2. Xorg behaviour to match

- `ProcChangeKeyboardMapping` / `ProcSetModifierMapping` →
  `XkbApplyMappingChange` (`xkb/xkbUtils.c:549`): `XkbUpdateKeyTypesFromCore`
  rewrites the changed keys' types and syms in the live `XkbDesc`,
  `XkbUpdateActions` → `XkbUpdateDescActions` recomputes their actions and vmodmap
  from the compat interprets (skipping keys with explicit actions), a modmap change
  recomputes actions for the whole range, and `XkbSendNotification` sends
  `XkbMapNotify` (plus secondary effects such as ControlsNotify for per-key
  repeat). The core `MappingNotify` comes from the dix side.
- `SetModifierMapping` returns `MappingBusy` when a key whose modifier bits change
  is currently down (dix `change_modmap` / `check_modmap_change`), and the map is
  then not applied.
- `_XkbSetMap` (`xkb/xkb.c:2658`) applies each present part (types, syms,
  actions, behaviors, vmods, explicit, modmap, vmodmap) and sends MapNotify. A
  keycode-range change sends NewKeyboardNotify first. `SetCompatMap`
  (`xkb.c:3165`) re-derives actions when asked (`recomputeActions`) and sends
  CompatMapNotify. `SetNames` (`xkb.c:4522`) sends NamesNotify.
  `SetIndicatorMap` sends IndicatorMapNotify.
- `xkbcomp file $DISPLAY` uploads through libxkbfile `XkbWriteToServer`. The
  order is believed to be SetMap(all) → SetIndicatorMap → SetControls →
  SetCompatMap → SetNames → SetGeometry. **Verify with x11trace against Xorg
  before designing phase 4.**

## 3. Approach

**One source of truth: the xkbcommon keymap.** Every mutation produces a new V1
keymap text: current `get_as_string` output, edited. That text is compiled with
`Keymap::new_from_string` and installed. Cooking, GetMap, GetKeyboardMapping and
GetModifierMapping then all read one keymap, so the overlays
(`core_map_overrides`, and in KMS also `modifier_mapping_override`) become
unnecessary and are removed for the KMS backend. `ServerState::keymap_overrides`
stays as the fallback for backends that return `false` from
`change_keyboard_mapping`.

Why text and not an in-memory XkbDesc model: xkbcommon keymaps are immutable and
it exposes no structured access to types, actions, explicit or behaviors, so any
model has to be parsed from the V1 dump anyway. Editing the dump keeps
xkbcommon's own compiler as the thing that re-derives actions and vmodmap from
interprets, which is what `XkbUpdateDescActions` does in Xorg. Phase 4 may still
want a small structured writer (see its open question).

### Phase 1 — install-from-text infrastructure

- `KmsCore::install_keymap_text(text) -> Result<(min, max), _>`:
  `new_from_string(V1)`, then `install_keymap`. On a compile failure keep the old
  map and log a warning with the compiler message (fail-closed).
- Keymap provenance: replace the "RMLVO equality ⇒ no-op" test with a provenance
  field (`KeymapSource::Rmlvo(XkbRmlvo)` / `Edited { base: XkbRmlvo }`). Then
  `setxkbmap` with the same RMLVO after an `xmodmap` edit does reload (Xorg always
  reloads on setxkbmap), and `_XKB_RULES_NAMES` keeps reporting the base RMLVO.
- Locked state across an in-place edit: Xorg keeps the locked group and locked
  mods through a mapping change. Unlike the full RMLVO swap (which resets on
  purpose), an edit must carry `locked_group` (clamped to the new group count) and
  the locked mods across. Do it the same way held keys are re-asserted (replay),
  not with `update_mask` (see the `recompile_keymap` doc comment).
- `xkb_edit` module (`kms/xkb_edit.rs`) over the V1 dump:
  - find, replace or insert a `key <NAME> { … };` entry in `xkb_symbols`;
  - rewrite the `modifier_map` statements from a per-keycode modmap;
  - add a keycode name for a keycode that has none (`<Innn> = nnn;` in
    `xkb_keycodes`), since ChangeKeyboardMapping may target one.
  Unit-test it on the frozen `testdata/xkb-keymap-*.xkb` fixtures: every edit must
  recompile, and untouched keys must be unchanged key by key.
- MapNotify helper in the core loop: `XkbMapNotify` to `subscribers(0x0002)`,
  with the fields taken from what the backend reports changed.

### Phase 2 — ChangeKeyboardMapping edits the real keymap

- `change_keyboard_mapping` keeps computing each changed key's groups (type name
  + syms) with the existing `apply_core_mapping_change` logic, but emits
  `key <N> { type[GroupK]="T", symbols[GroupK]=[…] … };` for each changed key into
  the text, recompiles and installs, instead of writing `core_map_overrides`.
  Actions come from interprets at compile time. A key that previously had
  explicit actions keeps them (Xorg skips keys with XkbExplicitInterpretMask).
- **Spike first**: confirm that xkbcommon's V1 dump writes `actions[…]` only for
  explicit-action keys (otherwise re-emitting a key must drop the stale actions),
  and that `vmods=` / `repeat=` survive re-emission.
- Remove `core_map_overrides` from `KmsCore`. `core_keyboard_map` then derives
  GetKeyboardMapping from the rebuilt keymap.
- **Acceptance**: the 26-case `xorg-change-keyboard-mapping.txt` golden (#168)
  must still pass. Now it runs through the real keymap, which proves the edit is
  faithful. Add: after the change, `cook_host_key` on the changed keycode yields
  the new keysym, and XKB GetMap of the changed keys equals an Xorg capture
  (new golden, see §4).
- Events, in Xorg's order: core `MappingNotify(Keyboard)` (existing), then
  `XkbMapNotify`. The changed mask and first/num ranges come from an Xorg capture;
  don't guess.
- Also routed here: XI1 `ChangeDeviceKeyMapping` (same path).

### Phase 3 — SetModifierMapping edits the real keymap

- Move it to a backend call. Rewrite the `modifier_map` statements, recompile,
  install. vmodmap and actions are re-derived by the compiler (Xorg:
  `XkbUpdateActions` over the whole range).
- `MappingBusy`: if a keycode whose modifier bits change is in `down_keys`, reply
  Busy and apply nothing (Xorg `check_modmap_change`).
- Remove `modifier_mapping_override` for KMS. GetModifierMapping reads the
  keymap (`modifier_mapping_from_keymap`), which also fixes XKB GetMap's modmap
  part.
- Events: `MappingNotify(Modifier)` + `XkbMapNotify(ModifierMap…)`, per Xorg
  capture.
- A combined `xmodmap` script (`remove Lock = Caps_Lock`,
  `keycode 66 = Control_L`, `add Control = Control_L`) is the headline end-to-end
  case: the vng A/B must show Caps acting as Control for an XKB client on both
  servers.

### Phase 4 — XKB SetMap & co (`xkbcomp` upload)

- First capture the real request sequence and payloads of `xkbcomp file :N`
  against Xorg (x11trace).
- Decode `SetMap` parts: types, syms, actions, behaviors, vmods, explicit, modmap,
  vmodmap. This is the inverse of the GetMap encoder already in `xkb.rs`, so the
  GetMap reply layout can be reused and round-trip tested (GetMap(A) → SetMap →
  GetMap must equal A).
- Generate keymap text from the delivered parts plus the current keymap for the
  parts not delivered. Handle `SetCompatMap` (interprets + group compat;
  `recomputeActions`), `SetNames` (key, type, level, indicator and group names:
  the text needs key names) and `SetIndicatorMap`. Controls and Geometry: accept,
  store what GetControls needs, no geometry.
- **Open question for review**: xkbcomp sends several requests in a row, and
  after SetMap alone the names or compat can be inconsistent with the new
  symbols. Two choices: (a) apply every request immediately, as Xorg does, each
  producing a compilable text (needs a writer that can emit a full keymap from
  parts, a real structured model); or (b) apply SetMap immediately, and have
  SetCompatMap/SetNames re-generate over the result. (a) is Xorg-faithful;
  recommend (a) if the phase-1/2 editing grows into a writer anyway.
- Events: NewKeyboardNotify on a keycode-range change, MapNotify,
  CompatMapNotify, NamesNotify, IndicatorMapNotify (new encoders), each to its
  subscribers, per Xorg capture.

Phases 1–3 fix `xmodmap` and `~/.Xmodmap` (the common case) and can ship on their
own. Phase 4 is the xkbcomp path.

## 4. Ground truth and verification

- No guessed vectors. Capture every golden from Xorg with a probe (the Xvfb
  21.1.24 + xkeyboard-config 2.48 setup of the #168 goldens), over the frozen
  `testdata/xkb-keymap-*.xkb` inputs.
  - After each ChangeKeyboardMapping / SetModifierMapping case: XKB GetMap
    (syms, types, actions, modmap, vmodmap parts) and the MapNotify fields.
  - For phase 4: the xkbcomp request trace, then GetMap / GetCompatMap / GetNames
    afterwards.
- Heads-up: in the vng `--server xorg` guest, `setxkbmap -layout gb` reported
  success but Xorg kept `us` (found 2026-09-25 while dumping keymaps). Captures
  that need a layout switch should use Xvfb on the host (as #168 did) until that's
  understood.
- vng A/B scenario (`tools/vng-scenarios/`): apply the xmodmap script, then
  inject keys through XTEST and read them with an XKB-aware probe
  (`XkbLookupKeySym` / an xkbcommon-x11 client). Compare the keysym and state
  stream on Xorg vs yserver. Same for `xkbcomp` of an edited dump.
- Unit: the `xkb_edit` tests; the #168 goldens through the real keymap; cooking
  after an edit; locked group and held keys surviving an edit; MappingBusy.
- HW (release gate): `~/.Xmodmap` loaded at session start takes effect in GTK,
  Qt and xterm.

## 5. Risks

- xkbcommon V1 dump / reparse round-trip fidelity (the phase-2 spike). Any key
  whose re-emission isn't lossless shows up as a whole-keymap diff in the
  fixture tests.
- Compile cost: a full keymap compile per request (a few ms). An xmodmap script
  issues one request per line, so tens of recompiles is fine. Log the duration at
  debug level.
- Clients that cache the keymap and only listen for core MappingNotify (older
  Xlib) refetch through GetKeyboardMapping, which now reads the same keymap.
- Removing the KMS overlays changes the #168 code path. The 26 goldens are the
  regression net and must pass unchanged.
