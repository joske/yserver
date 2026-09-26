# Plan: runtime keymap mutation reaches XKB (#171)

Status: phases 1–3 implemented (branch `feat/171-xkb-keymap-mutation`); phase 4 open.
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
- `xkbcomp file $DISPLAY` uploads (captured, `testdata/xorg-xkbcomp-upload-trace.txt`):
  SetMap(present=0xff) → SetIndicatorMap → SetCompatMap(recomputeActions=1) →
  SetNames → SetGeometry. No SetControls; no read-back first. SetGeometry is
  what triggers the NewKeyboardNotify.
- Captured Xorg facts (`testdata/xorg-xkb-*.txt`, `tools/xkb-mutation-probe.c`):
  ChangeKeyboardMapping → MapNotify changed=0x0012 (keysyms over the requested
  keys, even for no-op changes; the action range is XkbUpdateDescActions', which
  stops at the last key with actions, and a vmodmap change adds 0x80/0x40), one
  per device; SetModifierMapping →
  MapNotify changed=0x0014 (0x0094 when vmodmap changes), modmap range 8+248,
  with Xorg's buggy action/vmodmap ranges (copy them). MappingBusy if ANY old or
  new modifier key is held (not 255: Xorg off-by-one). A keycode under two
  modifiers is BadValue, so xkbcommon's one-modifier-per-key limit is not a gap.
  keycodes_per_modifier is never stored: readback = max keys on any modifier.
  A keymap load keeps the per-key repeat (GetKbdByName `XkbCopyControls`).
  A vmod no key's vmodmap names keeps its real mapping; a key no interpret
  matches keeps its vmodmap.

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
- Events, in Xorg's (captured) order: `XkbMapNotify` (changed=0x0012 over the
  requested keys), then core `MappingNotify(Keyboard)`, then
  `XkbControlsNotify(PerKeyRepeat)` when a changed key's auto-repeat changed.
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

### Phase 4 — XKB SetMap & co (`xkbcomp` upload): design

Scope: `xkbcomp keymap.xkb $DISPLAY` (and any other client's XKB `SetMap`,
`SetCompatMap`, `SetIndicatorMap`, `SetNames`, `SetGeometry`) changes the server
keymap as on Xorg: each request takes effect immediately, cooking follows it,
every XKB readback between the requests is what Xorg reports, and the events go
out as Xorg sends them. This section supersedes §3's "one source of truth: the
xkbcommon keymap" from phase 4b on (see 4.2).

#### 4.0 Decisions in one place

1. **State model: a structured Xorg `XkbDesc` in Rust** (`kms/xkb_desc.rs`), not
   dump-text patching. The model is authoritative for every XKB/core readback.
   The xkbcommon keymap becomes a *derived artifact*: after every mutation the
   model is written out as complete V1 text and compiled, only so `xkb_state` can
   cook keys. Phases 2–3 (ChangeKeyboardMapping, SetModifierMapping) move onto the
   same path. One mutation path: `mutate model → write text → compile → install`.
2. The model is **seeded** from the xkbcommon keymap when a keymap is loaded by
   name (startup, `setxkbmap`/GetKbdByName, `_XKB_RULES_NAMES`). Nothing else
   re-seeds it.
3. Each Set* request is a **literal port** of Xorg's handler over the model,
   including its range arithmetic and bugs (captured below). Nothing is
   re-derived by xkbcommon: the writer emits every key's resolved actions,
   `virtualMods= none` plus explicit virtual-modifier mappings, and no
   interprets.
4. Things xkbcommon can't cook (behaviors, RedirectKey, ISOLock, …) are stored
   in the model and read back exactly, and are not cooked (4.6).
5. yserver sends **one** of each event (device 1), where Xorg sends one per
   device (3, 5, 7), as phases 2–3 already do.
6. New encoders: `XkbCompatMapNotify`, `XkbNamesNotify`,
   `XkbExtensionDeviceNotify`. New per-client state: "XKB initialised" and the
   XkbSelectEvents detail masks (Xorg filters on them, and the legacy core
   MappingNotify depends on them).
7. SetGeometry is accepted and validated, updates the geometry *name* and sends
   Xorg's events, but the geometry itself isn't stored (open question 1).
8. Ground truth: new Xvfb captures (4.10). `xkbcomp`'s own request bytes are
   recorded, then replayed one request at a time with a full-state delta after
   each one. Nine cases, 45 recorded requests.

#### 4.1 What Xorg does (captured: `testdata/xorg-xkbcomp-steps.txt`)

`xkbcomp FILE :N` connects and sends `UseExtension`, 105 `InternAtom`s, then
exactly these five requests, never reading anything back first. Every case is
the same shape (sizes vary):

| # | request (identity case) | Xorg handler |
|---|---|---|
| 1 | `SetMap` 5736 B: present=0xff flags=3 (ResizeTypes\|RecomputeActions), min=8 max=255, types 0+27, syms 8+248, acts 8+248 **total 0**, behaviors 8+248 total 0, explicit 8+248 total 70, modmap 8+248 total 14, vmodmap 8+248 total 0, virtualMods=0xffff | `_XkbSetMap` xkb.c:2658 |
| 2 | `SetIndicatorMap` 396 B, which=0xffffffff | `_XkbSetIndicatorMap` xkb.c:3381 |
| 3 | `SetCompatMap` 2016 B, recomputeActions=1 truncateSI=1 groups=0x0f, SI 0+124 | `_XkbSetCompatMap` xkb.c:3012 |
| 4 | `SetNames` 2288 B, which=0x1fff, types 4+23, ktLevels 0+27, indicators 0x3fff, groupNames 0x01, keys 8+248, 73 aliases, virtualMods 0xff | `_XkbSetNames` xkb.c:4363 |
| 5 | `SetGeometry` 2020 B | `_XkbSetGeometry` xkb.c:5687 |

No `SetControls`: an uploaded `repeat= No` is **not** applied to the per-key
repeat control. In case `explicit`, <RCTL> gets explicit=0xa0 but its repeat bit
stays on. So yserver not having SetControls costs no parity for xkbcomp.

Events per request, in order (listener = XkbSelectEvents(all) on the core
keyboard; "core" = a client that never called UseExtension):

| request | XKB events (one per device on Xorg; one on yserver) | core events |
|---|---|---|
| SetMap, same range | `MapNotify` changed=**0x00f3** types 0+27 syms 8+248 acts **8+196** beh 8+248 expl 8+248 modmap 8+248 vmodmap **64+9** vmods **0x003f** | `MappingNotify(Keyboard, 8, 248)` to core clients **and** to XKB clients whose map detail mask ∩ changed ≠ 0 (the listener got it); **no** `MappingNotify(Modifier)`, even when the modmap changed (case `capsctrl`) |
| SetMap, min/max ≠ server's (case `range`, max=247) | `NewKeyboardNotify` changed=Keycodes(1), min/max **and** old min/max all 8/255 (the range only ever grows), req=XKB/9; **no MapNotify** | `MappingNotify(Keyboard, 8, 248)` and `MappingNotify(Modifier)`, to non-XKB clients only |
| SetIndicatorMap | `IndicatorMapNotify` state=lit changed=which; `ExtensionDeviceNotify` reason=0x0008 (IndicatorMaps) ledClass=0 ledID=0 ledsDefined=0x3fff ledState=lit supported=0x1f | — |
| SetCompatMap | `CompatMapNotify` changedGroups=0x0f firstSI=0 nSI=124 nTotalSI=124; then `MapNotify` changed=0x0010 acts **8+75** (XkbUpdateActions over the whole range) | — |
| SetNames | `NamesNotify` changed=0x1fff types 4+23 levelNames **0+23** (= nTypes, Xorg bug) nRadioGroups=0 nKeyAliases=73 changedGroupNames=**0** changedVMods=**0x0001** (= groupNames: Xorg overwrites it) keys 8+248 changedIndicators=0x3fff; `ExtensionDeviceNotify` reason=0x0004 (IndicatorNames) | — |
| SetGeometry | (`NamesNotify` changed=GeometryName only if the name changed; after SetNames it never does) `NewKeyboardNotify` changed=Geometry(2), min/max = old = 8/255, req=XKB/20 | — (Geometry changes don't produce legacy events) |

Side effects visible after the identity upload, all of which the model must
reproduce because they come straight from the request bytes:

- **explicit 0x01 → 0x0f on the 70 explicitly typed keys.** Source: SetMap's
  explicit part. The server's own compile of the symbols files
  (`type[Group1]=`) gave `XkbExplicitKeyType1`. `xkbcomp -xkb` dumps
  `type= "X"` with no group, and xkbcomp compiles that as explicit for all four
  groups. `SetKeyExplicit` (xkb.c:2353) stores the wire bytes as they are. This
  changes later behaviour too: an `xmodmap` after an upload
  (`XkbUpdateKeyTypesFromCore`) now leaves groups 2–4 of those keys alone.
- **Type map entries reordered** in types 6, 8, 9, 14, 15, 17 (and preserve with
  them). The dump writes them in another order and SetKeyTypes stores the wire
  order.
- Names: keycodes `evdev+aliases(qwerty)`→`evdev_aliases(qwerty)`, geometry
  None→`pc(pc105)`, symbols `pc+gb+inet(evdev)`→`pc_gb_inet(evdev)`,
  phys_symbols→None. Type/key/indicator/vmod/group names are unchanged.
- MapNotify `changed` lacks ExplicitComponents (0x08) and ModifierMap (0x04)
  although their ranges are filled. `SetKeyExplicit`/`SetModifierMap`/
  `SetVirtualModMap` set first/num but never the bit (xkb.c:2353-2455). That's
  why no core `MappingNotify(Modifier)` goes out.
- `vmods=0x003f` in the MapNotify although GetMap's vmods end up unchanged:
  `SetVirtualMods` overwrites them with the wire values and marks them, then
  `XkbUpdateDescActions` recomputes the vmods some vmodmap names, and
  `changes->map.vmods` keeps the bits.
- `num_types` never shrinks (case `droptype`: 26 types sent, GetMap still has
  27, and type 26 keeps the old `SHIFT+ALT` with its name). SetKeyTypes only
  grows `num_types` (xkb.c:2092). MapNotify then reports types 0+27 because
  `XkbApplyVirtualModChanges` adds type 26 (it names the changed vmod Alt).
- A group-count change (case `usru`) updates GetControls numGroups 1→2 but sends
  no ControlsNotify. `XkbComputeControlsNotify` returns true, but
  `XkbSendControlsNotify` filters on `ctrlsNotifyMask & changedControls`, and
  changedControls=0.

#### 4.2 State model (Q1)

**Recommendation: (a), a structured in-memory Xorg `XkbDesc`.** Reasons:

- Xorg's semantics *are* in-place mutation of an `XkbDesc`: SetKeyTypes keeps
  names by index, `num_types` never shrinks, level-name slots stay stale,
  explicit bits are stored raw, actions are stored raw and re-derived only
  when asked. None of this can be expressed as "edit the dump and let xkbcommon
  recompile": xkbcommon names types by name (unique), re-derives actions and
  vmodmap from interprets on every compile, keeps one modifier per key, and has
  no explicit-bit, behavior, group-compat or stale-vmodmap state. Phases 2–3
  already needed three side tables to cover that (`xkb_explicit_types`,
  `xkb_stale_vmodmap`, the vmod "pins" re-install in `finish_mapping_change`).
  Phase 4 would need a dozen more.
- SetMap delivers a binary `XkbDesc` fragment. Decoding it into a model and
  applying it is the direct port. Translating it into text edits needs the same
  decode plus a lossy mapping.
- Readbacks (GetMap/GetNames/GetCompatMap/GetIndicatorMap/GetKbdByName) become
  plain encoders over the model. Today they are approximations: a derived and
  deduplicated type table, no explicit/behaviors, only modifier actions, zero
  SIs, canonical type names. With the model they become exact, which also fixes
  the dump side of the round trip: `xkbcomp -xkb` against yserver today yields
  a keymap with no interprets, and re-uploading that breaks every modifier.

Model (`XkbDesc`, one per keyboard; lives in `KmsCore` beside
`xkb_keymap`/`xkb_state`):

```text
min_key_code, max_key_code : u8        // 8/255 on yserver (clamped), only grows
types      : Vec<KeyType>              // Xorg index order; len only grows via SetMap
  KeyType  { mods: Mods, num_levels, map: Vec<KtEntry{active, mods: Mods, level}>,
             preserve: Option<Vec<Mods>>, name: Option<String>,
             level_names: Vec<Option<String>> }   // None = atom 0 / uninitialised
  Mods     { mask, real_mods, vmods }  // mask = real | XkbMaskForVMask(vmods), kept current
keys[256]  : { kt_index: [u8;4], group_info: u8, width: u8, syms: Vec<u32> }
acts[256]  : Option<Vec<[u8;8]>>       // server->key_acts: None or one wire action per sym slot
behaviors[256] : (type u8, data u8)
explicit[256]  : u8
modmap[256]    : u8                    // full byte (xkbcommon keeps one bit)
vmodmap[256]   : u16                   // STORED, as Xorg (no stale-vmodmap side table)
vmods[16]      : u8                    // server->vmods
compat     : { si: Vec<SymInterpret{sym, mods, match_, virtual_mod, flags, act:[u8;8]}>,
               groups: [Mods; 4] }
indicators : [IndicatorMap{flags, which_groups, groups, which_mods, mods: Mods, ctrls}; 32]
names      : { keycodes, geometry, symbols, phys_symbols, types, compat: Option<String>,
               vmods: [Option<String>;16], indicators: [Option<String>;32],
               groups: [Option<String>;4], keys: [[u8;4];256],
               key_aliases: Vec<([u8;4] real, [u8;4] alias)>, radio_groups: Vec<Option<String>> }
num_groups : u8                        // ctrls->num_groups (GetControls numGroups)
n_radio_groups : u8                    // xkbi->nRadioGroups
```

Names are strings, not atoms. Atoms live in the core loop. SetNames resolves
its atoms through a lookup the core loop passes in (BadAtom on an unknown atom,
`_XkbCheckAtoms`), and the Get* encoders intern as today. Per-key repeat stays in
`ServerState::keyboard_control` as in phase 2. The model's derivations return
the re-derived repeat bits (`KeyboardMappingChange::repeats`). Core
`ChangeKeyboardControl` per-key settings must also set explicit 0x20 in the
model. Phase 2 already models them as explicit (`auto_repeats_explicit`); 4b
moves that bit into `explicit[]` so GetMap reports it.

**Seeding** (`XkbDesc::from_keymap`, replacing `xkb_derive::KeymapModel::new`
and the side tables). From the xkbcommon keymap plus its V1 dump:

- types in Xorg order: the four required first, then dump order (the order
  `xkb_derive` already proves against the goldens), with modifiers, entries,
  preserve and level names from the dump;
- syms, width, groups and kt_index from the xkbcommon API;
- explicit type bits from `explicit_type_masks`. `XkbExplicitInterpretMask`
  where the dump has `actions[]`, AutoRepeat where it has `repeat=`, VModMap
  where it has `virtualMods=`;
- actions and vmodmap from the Rust `XkbApplyCompatMapToKey` over all keys
  (what xkbcomp stored);
- compat SIs from the dump's interprets, encoded to wire (`sym`, `mods`,
  `match` | LevelOneOnly, `virtual_mod` 255=none, flags AutoRepeat/LockingKey,
  8-byte action). Order = dump order, which is Xorg's (checked: 124 SIs in gb
  and us,ru, `Any` at 122/123 in both). The group compat is xkeyboard-config's
  constant (group 1 none, 2–4 Mod5; xkbcommon ignores `group N=`). That is what
  `reply_get_compat_map` sends today;
- indicator maps and names from the dump (the existing parsers);
- key names, aliases and vmod names from xkbcommon;
- component names as today (Xorg after setxkbmap: keycodes
  `evdev+aliases(qwerty)`, symbols = phys_symbols `pc+gb+inet(evdev)`,
  types/compat `complete`, geometry None; see `xorg-xkb-pristine.txt`).

Seeding needs a V1 action-text → wire-action encoder for every action type in
xkeyboard-config's compat and symbols. That is a superset of
`xkb_derive::parse_action`: Set/Latch/LockMods, Set/Latch/LockGroup, MovePtr,
PtrBtn, LockPtrBtn, SetPtrDflt, Set/LockControls, Terminate, SwitchScreen,
Private (e.g. `XF86_Next_VMode`, the dump's `Private(type=0x86,data…)` =
Xorg's `862d564d6f646500`).

**Known seed deviations** (xkbcommon's compile vs xkbcomp's; measured against
`xorg-xkb-pristine.txt` for us/gb/de): types `PC_SHIFT_SUPER_LEVEL2` and
`PC_CONTROL_SUPER_LEVEL2` carry `preserve[Super]=Super` in xkbcommon while Xorg
has none. `CTRL+ALT` and `FOUR_LEVEL_X` list their Level1 entries in another
order, and `FOUR_LEVEL_X` also differs in preserve. us,ru matches. The 4a spike
tries to reproduce xkbcomp's rule. Failing that, these stay a listed tolerance
in the pristine test. After an xkbcomp upload they disappear, because the types
then come from the request.

**One mutation path** (4b):

```text
KmsBackend::mutate_keymap(f: FnOnce(&mut XkbDesc, &mut XkbChanges) -> Result<(), XError>)
  1. clone the model; run f on the clone (a literal Xorg handler port)
  2. text = clone.to_v1_text(); if text == last_text: skip compile
     else install_keymap_text(text)  (locks/held keys carried, LEDs resynced)
  3. compile error -> keep the old model and keymap, reply BadImplementation, log
     the text size + xkbcommon's message (a writer bug; never expected)
  4. commit clone; return XkbChanges (for the events) + re-derived repeats
```

ChangeKeyboardMapping (a port of `XkbUpdateKeyTypesFromCore` over the model:
`core_mapping_change` rebased from "xkbcommon keymap + explicit masks" onto the
model's types by index and `explicit[]`), SetModifierMapping, and every XKB Set*
request go through it. `xkb_edit`'s text surgery (`set_key`,
`set_modifier_map`, `ensure_keycode_name`, `set_virtual_modifier_mappings`) and
the side tables are deleted. The dump *readers* move into the seeder.
`KeymapSource::Edited` covers uploads too, so `setxkbmap` with the base RMLVO
still reloads, and `_XKB_RULES_NAMES` keeps the base RMLVO (Xorg doesn't touch
the property on an upload).

#### 4.3 The V1 writer

`XkbDesc::to_v1_text()` emits a complete keymap whose **cooking** equals the
model. Its names are synthetic, so names can never make it fail and SetNames
never forces a recompile unless an indicator name changes:

- `xkb_keycodes`: `minimum/maximum` = model; `<K008> = 8; … <K255> = 255;` for
  every keycode, because model key names may be empty, duplicated or stale.
  `indicator N = "<name>"` for every indicator with a map or a name (model name,
  else `"yserver-led<N>"`). LED state is read back by name
  (`led_name_is_active`).
- `xkb_types`: `virtual_modifiers <name>=<real mask>, …` for all 16 vmods with
  explicit mappings (model name, else `VMod<i>`). Types named `"T<index>"`,
  `modifiers=` real+vmod names, entries in model order (when two entries have
  the same mods only the first is written, which is what Xorg matches),
  `preserve[]`, level names.
- `xkb_compat`: **no interprets**; only `indicator "<name>" { whichModState,
  modifiers, whichGroupState, groups, controls }` from the model maps.
- `xkb_symbols`: per key with groups, `type[GroupN]= "T<kt_index>"`,
  `symbols[GroupN]` (the type's level count), `actions[GroupN]` (the model's
  actions as text, when the key has any), `virtualMods= none`, `repeat=` from
  the per-key repeat control, `groupsRedirect=/groupsClamp` from `group_info`.
  `modifier_map`: each key's lowest modmap bit (xkbcommon keeps one; nothing we
  cook reads it, since actions carry resolved modifiers).

Why this reproduces the model exactly: with no interprets and `virtualMods=
none` everywhere, xkbcommon derives nothing. Actions, vmod mappings and types
are exactly what we wrote. Spike (done while designing, xkbcli 1.13):
`virtual_modifiers NumLock=Mod3` with the only NumLock key under Mod2 and
`virtualMods= none` keeps NumLock=Mod3. Private actions survive. A second
`modifier_map` for the same key replaces the first. RedirectKey compiles to
NoAction. xkbcommon's `entry_is_active` rule ("mods ≠ 0 but mask = 0 ⇒
inactive") is Xorg's `active` rule.

Action text: modifier actions get their `real_mods`/`vmods` after
`_XkbSetActionKeyMods` (no `modMapMods`), with `clearLocks`, `latchToLock` and
`affect=` for the lock flags. Group actions get their group and flags. Pointer,
controls, screen and terminate actions map one to one. Types xkbcommon can't cook
are written as `NoAction()` (4.6).

#### 4.4 Intermediate consistency (Q2)

There is nothing to reconcile. The model *is* Xorg's desc after each request,
the writer turns whatever it holds into an equivalent xkbcommon keymap, and
readbacks encode the model. What Xorg holds after SetMap and before the rest
(step `1-SetMap` of every case):

- types = the request's, at their indices. **Names stay by index**: the old
  name at an old index, None at a new index (case `newtype`: type 4 is
  `YS_SHIFT_CTRL`'s definition still named `PC_ALT_LEVEL2` with levels
  `['Base' 'Alt' ?]`, and type 27 is None). A type that gained levels has
  uninitialised level-name slots (`XkbResizeKeyType` reallocarray,
  XKBMAlloc.c:326-335; a new type index is zeroed by `XkbAllocClientMap`, so
  its name is None and all its level names are garbage). **yserver reports None (atom 0) for those
  slots.** Xorg's value is undefined memory: the capture prints `?`, and tests
  skip `?`;
- syms, kt_index, group_info and width from the request. num_groups is updated;
- actions: the request's (xkbcomp: none), then, because flags has
  RecomputeActions, `XkbUpdateActions` over first..last of the union of the
  syms and modmap ranges, **with the old compat**. Case `compat`: <CAPS> keeps
  `LockMods(Lock)` until step 3. Case `capsctrl`: <CAPS> is Control_L and gets
  `SetMods(Control)` (0105040400000000) at step 1, from the old compat's
  Control_L interpret;
- explicit, modmap, vmodmap and vmods from the request, then vmodmap and vmods
  recomputed by the same XkbUpdateActions (behaviors cleared, then LockingKey
  re-derived);
- key names, aliases, indicator maps and names, compat, group names:
  unchanged. Case `range`: keys 248–255 keep their names and syms, because the
  request's ranges stop at 247 and the range doesn't shrink.

Between SetMap and SetNames our GetNames therefore reports stale type names by
index, as Xorg does. xkbcommon-x11 clients that rebuild on the SetMap
MapNotify rebuild from that. Xorg clients see the same.

#### 4.5 Actions, explicit bits, recompute (Q3)

- **SetMap actions** (`SetKeyActions` xkb.c:2237) are stored raw per key: a
  count of 0 clears the key, otherwise one 8-byte action per sym slot
  (`CheckKeyActions`: count must equal the key's syms). xkbcomp sends actions
  only for keys with their own `actions[]` (case `explicit`: acts total=1,
  <COMP> `LockGroup(+1)` = 0600010000000000, explicit=0x10).
- **Recompute**: SetMap with flag 2 → `XkbUpdateActions(first..last)` where
  first/last is the union of the syms and modmap change ranges (xkb.c:2733-2766,
  including its "last > 0" tests). SetCompatMap with recomputeActions →
  `XkbUpdateActions(min, XkbNumKeys)` (xkb.c:3155). Both run
  `XkbUpdateDescActions` → `XkbApplyCompatMapToKey` per key (XKBMisc.c). A key
  with `XkbExplicitInterpretMask` is skipped entirely. Otherwise the matching
  interpret's action is copied with `_XkbSetActionKeyMods`, or key_acts is
  cleared when nothing matches. vmodmap is set unless `ExplicitVModMap`.
  `LockingKey` sets `KB_Lock` unless `ExplicitBehavior`. Per-key repeat is set
  from `interps[0]` unless `ExplicitAutoRepeat`. Then the vmod recompute, then
  `XkbApplyVirtualModChanges` (type masks, actions naming the vmods, indicator
  maps). `xkb_derive` already ports most of this (`find_interp`, `derive_key`,
  `update_desc_actions` with `_XkbAddKeyChange` and the closing-merge quirk).
  4b generalises it to raw actions of every type, behaviors and stored vmodmap.
- **Where 0x01→0x0f comes from**: SetMap's explicit part (4.1). It is stored,
  not derived.
- **xkbcommon's view of explicitness is irrelevant** in this design: explicit
  bits live only in `explicit[]` and are read only by the Rust
  `XkbApplyCompatMapToKey` / `XkbUpdateKeyTypesFromCore`. The writer emits
  resolved actions, `virtualMods= none` and `repeat=` for every key, so the
  compiled keymap reproduces the result, not the rule.

#### 4.6 What xkbcommon can't represent (Q4)

| thing | stored in model / read back | cooked | why this is acceptable |
|---|---|---|---|
| type names by index, None/stale names, level names | yes (exact; uninitialised slots → None) | n/a (synthetic names in text) | names don't affect cooking |
| explicit bits (all 8) | yes | n/a | used by our own derivation only |
| behaviors: KB_Lock, radio groups, overlays, permanent | yes (`SetKeyBehaviors` incl. KB_Permanent refusal, nRadioGroups) | **no** | xkbcommon has no behaviors; not in xkeyboard-config defaults; log once per key at info when a non-default behavior is installed |
| modmap with >1 bit per key (legal via XKB SetMap) | yes (full byte) | lowest bit written | cooking uses resolved action mods, not modmap |
| vmod mapping that isn't the OR of vmodmap×modmap (SetVirtualMods without recompute; stale vmods) | yes | yes: explicit `virtual_modifiers` + `virtualMods= none` | spike above |
| RedirectKey, ISOLock, DeviceBtn/LockDeviceBtn, DeviceValuator, ActionMessage | yes (raw 8 bytes) | **no** (`NoAction()`) | unsupported by xkbcommon 1.13 (RedirectKey spike → NoAction). ActionMessage would also need XkbActionMessage events. Rare in practice |
| group compat maps (`compat.groups`) | yes | no | affects only compat state; xkbcommon ignores it |
| preserve entries | yes | yes (`preserve[]`) | — |
| indicator flags `!allowExplicit`, `LEDDrivesKB`, `noAutomatic` | yes | no (xkbcommon drops them) | SetLedState-driven LEDs not modelled today |
| per-key `repeat=` from an upload | model gets the explicit bit, **control unchanged** | — | Xorg doesn't apply it either (no SetControls) |
| geometry | name only (open question 1) | n/a | — |

#### 4.7 Keycode range, SetGeometry, SetControls (Q5)

- **Keycode range**: yserver's range is always 8..255 (`clamped_keycode_bounds`),
  the legal maximum, and Xorg's `XkbChangeKeycodeRange` (XKBMAlloc.c:552) only
  lowers min or raises max. So on yserver any SetMap whose min/max differ from
  8/255 takes the NewKeyboardNotify path with an unchanged range: NKN
  changed=Keycodes, min=oldMin=8, max=oldMax=255, req=(XKB major, 9), **no
  MapNotify**, legacy `MappingNotify(Keyboard, 8, 248)` + `MappingNotify(Modifier)`
  to non-XKB clients only (captured, case `range`). The parts are still applied,
  over the request's own key ranges. Checks first: min < 8 → BadValue, min > max
  → BadMatch (`_XkbSetMapChecks`). Everything else is checked against the
  request's min/max (`CHK_REQ_KEY_RANGE`).
- **SetGeometry**: validate (`CHK_ATOM_OR_NONE(name)` → BadAtom, length), set
  `names.geometry`, send `NamesNotify(GeometryName)` if the name changed, then
  NKN changed=Geometry(2) with min/max = old = 8/255 and req=(XKB major, 20).
  Xorg sends three (devices 3/5/7); yserver sends one, device 1. It goes to
  clients whose NKN detail mask includes Geometry. No legacy core events. The
  geometry body is not stored, and GetGeometry keeps answering found=False
  (open question 1).
- **SetControls**: stays a no-op (xkbcomp never sends it). Out of scope.

#### 4.8 Events (Q6)

| event | encoder | fields (from `XkbChanges`/request, Xorg's quirks included) | recipients |
|---|---|---|---|
| `XkbMapNotify` | exists | changed + ranges exactly as the port computes them (4.1). min/max = current. ptrBtnActions 0 | clients whose **map detail** ∩ changed ≠ 0 (Xorg `clients[i]->mapNotifyMask`, all devices) |
| core `MappingNotify` (legacy) | exists | Keyboard(firstKeySym, nKeySyms) if changed has KeySyms; Modifier if changed has ModifierMap (never, from SetMap); from NKN(Keycodes): Keyboard(min, max-min+1) + Modifier | non-XKB clients, plus XKB clients whose map detail ∩ changed ≠ 0 (MapNotify only; XKB clients never get NKN's) — `XkbSendLegacyMapNotify` xkbEvents.c:55. The XI `DeviceMappingNotify` companion is sent as in phase 2 |
| `XkbNewKeyboardNotify` | exists | dev=oldDev=1, min/max/oldMin/oldMax, requestMajor = **our XKB major opcode** (`xkb_info()`), requestMinor 9/20, changed. Bytes 18–31 zero (Xorg leaks stack there; tests ignore) | clients whose **NKN detail** ∩ changed ≠ 0 |
| `XkbCompatMapNotify` | **new** | changedGroups=req.groups, firstSI, nSI (request), nTotalSI=num_si after | clients with any compat detail (Xorg tests `compatNotifyMask` ≠ 0) |
| `XkbIndicatorMapNotify` | exists | state = lit after, changed = which | indicator-map detail ∩ changed |
| `XkbIndicatorStateNotify` | exists | if the new maps change the lit state (`XkbUpdateLedAutoState`) | state detail ∩ changed |
| `XkbNamesNotify` | **new** | changed=which; firstType/nTypes (if KeyTypeNames); firstLevelName=0, **nLevelNames=nTypes**; nRadioGroups/nAliases after; changedGroupNames **0**; changedVirtualMods = virtualMods, **overwritten by groupNames when GroupNames is set**; firstKey/nKeys; changedIndicators | names detail ∩ changed |
| `XkbExtensionDeviceNotify` | **new** (xkbType 11) | reason (0x08 IndicatorMaps after SetIndicatorMap/XkbApplyLedMapChanges, 0x04 IndicatorNames after SetNames with indicator names), ledClass=0 (KbdFeedbackClass), ledID=0, ledsDefined = names∪maps present, ledState, firstBtn=nBtns=0, supported=0x1f, unsupported=0 | ext-dev detail ∩ reason |
| `XkbControlsNotify` | exists | only with changedControls ≠ 0: per-key repeat changes from a recompute, cause = (XKB major, 9 or 11). **Not** for a numGroups-only change | ctrls detail ∩ changed |

Per-request order = Xorg's (4.1). The backend returns an ordered event list
(`XkbSetOutcome { error: Option<(code, value)>, events: Vec<XkbEvent> }` from a
new `Backend::xkb_set(minor, body, atoms)`). `xkb_proxy`'s
`9 | 11 | 14 | 18 | 20 => None` go away, and the core loop fans out with the
filters above.

**New per-client XKB state** (core loop). A client is "XKB initialised" after a
successful UseExtension; Xorg answers BadAccess to Set* requests without it,
and the legacy filters depend on it. The detail masks come from XkbSelectEvents
(`ProcXkbSelectEvents`: per-event affect/value pairs, the map detail from
affectMap/map, `selectAll` = every detail). `ServerState::xkb_select_event_masks`
becomes a struct per (client, device) holding the top mask plus the map, NKN,
names, compat, indicator-map/state, ctrls, ext-dev and state details: the
"D2b" change `subscribers()` already anticipates. With one keyboard,
Xorg's per-device interest lists and per-client masks reduce to the same thing.

#### 4.9 Validation and errors

Port Xorg's checks exactly, since they decide *whether* anything is applied:
`_XkbSetMapCheckLength` (BadLength), `_XkbSetMapChecks` → `CheckKeyTypes`
(required types: 1 level for index 0, 2 levels for 1–3; entry mods ⊆ type
mods; level < numLevels; preserve ⊆ entry), `CheckKeySyms` (ktIndex < nTypes,
width = max type width, nSyms = width×groups), `CheckKeyActions`,
`CheckKeyBehaviors` (KB_Permanent, radio-group bound, overlay key range),
`CheckVirtualMods`, `CheckKeyExplicit`, `CheckModifierMap`, `CheckVirtualModMap`
(each BadValue with Xorg's `_XkbErrCodeN` errorValue). SetCompatMap: firstSI >
num_si → BadValue, length. The "broken Any+AnyOfOrNone(all)→Private" SI is
skipped (xkb.c:3082). SetIndicatorMap: which=0 → Success/no-op,
`CHK_MASK_LEGAL` whichGroups/whichMods. SetNames: `_XkbSetNamesCheck`
(KeyTypeNames with firstType ≤ 3 → **BadAccess**, level widths ≠ num_levels →
BadMatch, bad atoms → BadAtom). No UseExtension → BadAccess. Errors apply
nothing and send nothing.

#### 4.10 Ground truth and tests (Q7)

**Capture method: record xkbcomp, replay one request at a time.**
`tools/xkb-mutation-goldens.sh steps` does, per case:

1. record: fresh `Xvfb :92 -noreset` + `setxkbmap -rules evdev -model pc105
   -layout gb`, run `xkbcomp CASE.xkb` under `x11trace -n -m 1000000`, and save
   every XKB request after UseExtension **byte-exact** as
   `testdata/xkbcomp-requests/CASE/N-Request.bin`, plus xkbcomp's InternAtom
   replies as `atoms.txt` (`0xATOM NAME` in order);
2. replay: a fresh server with the same setup, then `xkb-mutation-probe -x
   atoms:atoms.txt xreq:1-SetMap.bin xreq:2-… total`. The actor interns the
   names in order and **dies unless every atom has the recorded value**, so the
   recorded atoms mean the same. It then sends each request raw after its own
   XkbUseExtension, and after each one prints the events plus the delta of
   GetMap/GetControls (as before) and, new with `-x`, of GetCompatMap,
   GetIndicatorMap, GetNames (atoms by name), GetControls numGroups and the
   GetGeometry header;
3. check: a third fresh server where the probe runs `xkbcomp CASE.xkb` itself.
   **Its total delta must equal the replay's**, or the script fails. This
   proves the one-at-a-time replay is equivalent to the real upload.

x11trace + raw replay was chosen over a pausing proxy (which would have to
rewrite sequence numbers) and over re-implementing xkbcomp's upload with
libxkbfile (whose payloads could differ from xkbcomp's). The atom check makes
the replay safe, and step 3 proves the result.

Cases (all onto gb; CASE.xkb = `xkbcomp -xkb` dump of the fresh gb server,
edited; the diff is in the golden):

| case | edit | exercises |
|---|---|---|
| `identity` | none | the side effects every upload has (4.1) |
| `swap` | <AC01>/<AC02> symbols swapped | the original trace's case, now per request |
| `newtype` | new type `YS_SHIFT_CTRL` first among the non-required types (Xorg index 4, the rest shift up), used by <AC01> with 3 levels | stale type names by index, None for new index 27, uninitialised level names, NamesNotify types 4+24 |
| `droptype` | unused `SHIFT+ALT` removed | num_types doesn't shrink, MapNotify types 0+27 |
| `compat` | Caps_Lock interpret → `SetMods(Control)`; Caps Lock LED on Shift | SetIndicatorMap map change; SetCompatMap SI 121 + <CAPS> action change |
| `capsctrl` | <CAPS> = Control_L under Control | modmap via SetMap, no core Modifier MappingNotify, coremodmap readback |
| `explicit` | <COMP> own action `LockGroup(+1)`; <RCTL> `repeat= No, virtualMods= Alt` | SetMap actions part, explicit 0x10/0xa0, explicit vmodmap → vmods Alt 0x08→0x0c, repeat control untouched |
| `range` | maximum 255→247, keycodes 248–255 dropped | NKN(Keycodes) path with unchanged range, no MapNotify, core Keyboard+Modifier to non-XKB clients |
| `usru` | the unedited us,ru(+grp:alt_shift_toggle) dump | 2 groups, numGroups 1→2 without ControlsNotify, group names, NamesNotify changedVMods=0x0003 |

Files (Xvfb 21.1.24, xkeyboard-config 2.48, generated by the script, never
hand-edited):

- `testdata/xorg-xkbcomp-steps.txt`: per case, the edit, the decoded request
  headers, and per request the result, events and state delta.
- `testdata/xkbcomp-requests/<case>/N-*.bin` + `atoms.txt`: the 45 request
  payloads, the decoder's and the harness's input.
- `testdata/xorg-xkb-pristine.txt`: `probe -x full` (the whole description: every
  key row, every type, vmods, repeat, SIs, group compat, indicator maps, all
  names, geometry header, core modmap) after setxkbmap for us, gb, de and
  us,ru. This is the state every case starts from and the oracle for seeding.
- Unchanged: `xorg-xkbcomp-upload-trace.txt` (whole-upload trace) and the
  phase 1–3 goldens. The probe's output without `-x` is byte-identical, which
  was verified by regenerating them: only raw seq/time changed.

Known non-determinism in the goldens, which tests must ignore: raw event seq
(bytes 2–3) and time (4–7); NKN bytes 18–31 (Xorg stack garbage); level names
printed `?` (uninitialised Xorg memory; the probe tracks which slots from the
requests it sends).

**Test harness** (Rust, `kms` tests), per case: a `KmsBackend` on the frozen
gb fixture, `ServerState` with an XKB listener (UseExtension + SelectEvents all)
and a plain client, the atom table pre-seeded with `atoms.txt`'s (value, name)
pairs (test-only insert, so the payloads replay byte-exact), then each
`include_bytes!` request through `handle_xkb_request` (major opcode patched).
Compared after every request:

- the result (ok or error);
- events on both clients by field (Xorg's dev-3 events ↔ our device 1;
  Xorg's dev-5/7 duplicates dropped);
- the **absolute** state: Xorg's = `xorg-xkb-pristine.txt` gb + the deltas so
  far; ours = our Get* replies decoded into the same line grammar (a Rust port
  of the probe's printer, which also checks our encoders' wire layout
  byte-for-byte: parse must end at the reply length). The seed deviations
  (4.2) are the only tolerated differences, and only for rows no request
  has replaced yet. `?` slots are skipped.

Plus: decoder round trip (decode → encode = the recorded bytes, for all 45
payloads, and the decoded headers equal the golden's decoded request lines).
Cooking: after `compat` step 3, keycode 66 sets Control. After `capsctrl`
step 1, keycode 66 cooks Control_L with Control. After `swap` step 1, keycode 38
cooks `s`. Writer invariant (4b): for each fixture, seed → text → compile gives
the same keysym and the same modifier/group state change for every keycode × group ×
modifier combination as the RMLVO-compiled keymap.

Error vectors aren't captured yet. 4c records them with the same tooling:
`xreq:` with recorded payloads mutated by the script (bad ktIndex, bad width,
level ≥ numLevels, firstType ≤ 3 in SetNames, bad atom, short length), with
Xvfb's error code and value as the golden.

End-to-end (vng A/B, `tools/vng-scenarios/`): `xkbcomp capsctrl.xkb $DISPLAY`,
then XTEST Caps and an XKB-aware probe (`XkbLookupKeySym` plus an
xkbcommon-x11 client) on Xorg and on yserver; `xkbcli dump-keymap-x11` equal on
both, modulo the seed deviations; on yserver, `xkbcomp -xkb` → upload →
`xkbcomp -xkb` gives two identical dumps (the round trip that is broken today).
HW (release gate): an xkbcomp keymap loaded in a real session takes effect in
GTK, Qt and xterm, and Caps Lock/Num Lock LEDs still track.

#### 4.11 Phasing (Q8)

Every step ships on its own and keeps the phase 1–3 goldens green.

- **4a: model as a read view, exact readbacks.** `XkbDesc` + seeding +
  V1-action encoder; GetMap / GetCompatMap (real SIs) / GetNames / GetIndicatorMap
  and GetKbdByName's blocks encoded from it. The model is re-seeded after each
  phase 2–3 text edit, absorbing `xkb_explicit_types` and `xkb_stale_vmodmap`.
  *Accept*: `xorg-xkb-pristine.txt` for us/gb/de/us,ru equals our state except
  the listed seed deviations (spike: reproduce xkbcomp's type entry order and
  vmod preserve); CKM (26 cases) and SMM goldens pass with **tighter**
  comparison (types by index, explicit and behaviors compared, all action
  types); xkbcommon-x11 and libX11 clients still load the keymap (the existing
  GetMap/GetNames invariant tests, vng smoke).
- **4b: model authoritative + writer + one mutation path.** `to_v1_text`,
  `mutate_keymap`, CKM (`XkbUpdateKeyTypesFromCore` on the model) and SMM
  ported, `xkb_edit` text surgery and side tables deleted.
  *Accept*: CKM/SMM goldens unchanged; the writer invariant for 4 fixtures;
  existing cooking, locks-across-edit and MappingBusy tests; vng xmodmap A/B
  unchanged.
- **4c: SetMap.** Decoder + checks + the literal `_XkbSetMap` port
  (SetKeyTypes with `XkbResizeKeyType` incl. its key-width resizing, SetKeySyms
  with num_groups, SetKeyActions, SetKeyBehaviors, SetVirtualMods,
  SetKeyExplicit/SetModifierMap/SetVirtualModMap quirks, recompute,
  `XkbChangeKeycodeRange`); MapNotify / NKN(Keycodes) / legacy MappingNotify;
  the per-client XKB-initialised flag and detail masks; BadAccess.
  *Accept*: step 1 of all 9 cases (state + events), decoder round trip,
  captured error vectors, the cooking checks for `swap`/`capsctrl`.
- **4d: SetCompatMap + SetIndicatorMap.** SI apply/truncate/skip, group compat,
  whole-range recompute, `XkbApplyLedMapChanges`; CompatMapNotify and
  ExtensionDeviceNotify encoders.
  *Accept*: steps 2–3 of all cases; `compat` cooking (Caps → Control) and the
  Caps Lock LED following Shift after step 2.
- **4e: SetNames + SetGeometry, end to end.** Names apply (incl. aliases, radio
  groups, `if (type->level_names)` guard), NamesNotify encoder with Xorg's field
  bugs, SetGeometry name + NKN(Geometry).
  *Accept*: steps 4–5 of all cases (so the whole upload matches per request);
  vng A/B + dump round trip; HW gate.

**Review outcome (codex, 2026-09-26).**
- 4a and 4b ship together, as one decision gate: the model must keep every
  phase 1–3 `xmodmap` behaviour and generate a cooking keymap equivalent to the
  model. If that gate fails, stop there and keep phases 1–3; don't build
  4c–4e on it.
- Cooking gate, tighter than exact readbacks: after every one of the 45
  captured mutations (and for the phase 2–3 CKM/SMM cases), check that the
  compiled xkbcommon keymap cooks every key/level the way the model's action
  says, for every supported action type (SetMods/LatchMods/LockMods,
  SetGroup/LatchGroup/LockGroup, NoAction, plus the per-key repeat and
  modmap). Press/release each key on a fresh `xkb_state` and compare the
  resulting mods/group with what the action prescribes. The model equals Xorg
  (goldens) and cooking equals the model, so cooking equals Xorg. Exact GetMap
  replies alone can pass while key events differ.
- Actions and behaviors xkbcommon can't cook (§4.6) stay explicit
  limitations: stored and read back exactly, not cooked, and logged once
  (warn) per upload that uses one, naming the key and action.
- Open questions: all answered as recommended. (1) geometry name-only for
  #171, which leaves GetGeometry and geometry-preserving dumps incomplete, a
  known limit; (2) None; (3) BadAccess on every XKB request, as Xorg;
  (4) listed seed tolerance acceptable; (5) one event on device 1;
  (6) 4a+4b together.

#### 4.12 Open questions for review

1. **Geometry storage**: store SetGeometry's body and serve GetGeometry from it
   (xkbcomp dumps and xkbprint read it; needs `_CheckSetGeom` +
   `XkbSendGeometry` ports), or accept name-only as designed? Today yserver
   answers found=False and nobody has complained.
2. **Uninitialised level names**: None is our choice where Xorg returns
   garbage. Alternatively repeat the old names or synthesize "LevelN". None is
   the only value a client can't mistake for a real name.
3. **BadAccess without UseExtension**: enforce it for the Set* requests only
   (they're new) or for all XKB requests as Xorg does? Enforcing it on Get*
   could break a client that skips UseExtension and works on yserver today.
   Xlib and xcb clients always call it.
4. **Seed deviations**: if the 4a spike can't reproduce xkbcomp's type
   normalisation, is a listed tolerance in the pristine test acceptable? It
   only affects GetMap readback before any upload; cooking is identical.
5. **One event vs Xorg's three**: phases 2–3 settled on one (device 1). Confirm
   the same for NKN, whose dev-5/7 copies some clients might count.
6. **4a scope**: switching GetMap from the derived type table to the model's
   real types (with vmods and preserve) is visible to every xkbcommon-x11 client
   on day one. Ship 4a behind nothing, or land 4a+4b together?

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
