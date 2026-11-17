# RANDR user modes (#360)

**Goal:** `xrandr --newmode`, `--addmode`, `--output X --mode <user mode>`,
`--delmode` and `--rmmode` behave as on Xorg 21.1.24 with the modesetting
driver. The motivating case is a VGA monitor without EDID (#360): the kernel
offers only its fallback list up to 1024x768, and a user mode is the only way
to drive 1920x1080, as on Xorg.

**Out of scope:** `xorg.conf` Monitor `Modeline`s. On Xorg those are DDX probed
modes (`xf86Crtc.c:1717`, `:1818`), not RANDR user modes; yserver has no
`xorg.conf`. Leases (`CreateLease` keeps its `BadImplementation`).

Xorg references are `../xserver` at tag `xorg-server-21.1.24`. "Measured"
means Xvfb from the same tag, with a connection held so the server does not
reset between `xrandr` invocations.

## 1. Xorg behaviour to match

### Requests

| Request | Order of checks and errors | Effect | Events |
|---|---|---|---|
| CreateMode (16) | BadLength (fixed part) → window lookup, GetAttr access: BadWindow → name longer than the request: BadLength → **any existing mode with the same name** (user or probed, any screen): **BadName** (`rrmode.c:284-330`, `:106-120`, `:146-148`) | New mode, id allocated by the server (`FakeClientID(0)`, `:91`), the request's id field ignored. No timing validation. Same timings under a new name make a second mode (measured). | none |
| DestroyMode (17) | BadLength → unknown id: BadRRMode → not a user mode: BadMatch → referenced by any output's user list or any CRTC: **BadAccess** (`rrmode.c:332-347`) | Mode freed | none |
| AddOutputMode (18) | BadLength → BadRROutput → BadRRMode → leased output: BadAccess (`rrmode.c:349-364`) → already in the output's list (probed or user): **Success, no-op, no event** (`rroutput.c:208-214`) → DDX `mode_valid` (modesetting: always OK, `drmmode_display.c:2547-2550`) | Appended to the output's user list | **RRScreenChangeNotify + RROutputChangeNotify** for that output, config timestamp bumped (`rroutput.c:232-233`, `randr.c:481-570`, `:636-638`) |
| DeleteOutputMode (19) | BadLength → BadRROutput → BadRRMode → leased: BadAccess → **not in the output's user list** (a probed mode, or never added): **BadAccess** → the output's CRTC currently shows this mode: **BadMatch** (`rroutput.c:238-262`) | Removed from the user list | **none**, no timestamp change |

AddOutputMode does not check that the mode is a user mode
(`rroutput.c:199-235`); we follow that, it is harmless.

Measured on Xvfb, all consistent with the table: duplicate name BadName; same
timings under a new name OK; AddOutputMode twice OK; DestroyMode while on an
output's list (current or not) BadAccess; DeleteOutputMode while current
BadMatch; DeleteOutputMode of a probed mode or of a user mode not on that
output BadAccess.

### Replies

- **GetScreenResources(Current)** mode order (`rrmode.c:161-227`), deduped:
  each output's probed modes then its user modes; then each CRTC's current
  mode; then every user mode of the screen not yet listed, attached or not,
  in creation order. Names in the same order.
- **GetOutputInfo** (`rroutput.c:523-560`): probed modes, then user modes;
  `nPreferred` counts probed modes only.

### SetCrtcConfig with a user mode

- dix: the mode must be in each output's probed **or user** list, else
  BadMatch (`rrcrtc.c:1339-1350`). Other checks unchanged.
- DDX: the user mode is converted with `Clock = dotClock / 1000` (kHz,
  truncated) and `Flags = modeFlags & FLAG_BITS` (`xf86RandR12.c:1102-1128`,
  `FLAG_BITS` `:981`), then to a `drmModeModeInfo` with an empty name and
  `type = 0` (`drmmode_display.c:1413-1435`). No clock or `ValidMode` check
  before the kernel.
- A kernel rejection is **not an X error**: the reply carries
  `RRSetConfigFailed` (`drmmode_display.c:1585-1589`, `xf86RandR12.c:1206-1216`,
  `rrcrtc.c:1459-1483`).
- After a successful set, the CRTC reports the mode found **by timing**,
  searching the outputs' probed lists before their user lists
  (`xf86RandR12.c:997-1027`, `:1077-1088`). A user mode identical in timing to
  a probed mode is therefore reported as the probed one.

### Lifetime

- User modes are owned by the server, not the creating client
  (`rrmode.c:91-92`). They survive the client's disconnect (measured: they
  vanish only when the server resets after the last client leaves).
- Re-probe and hotplug replace only the probed list (`rroutput.c:161-195`);
  user lists are kept, also on a disconnected connector
  (`drmmode_display.c:4042-4044`).
- Server reset frees them.

## 2. yserver today

- CreateMode answers `BadImplementation`
  (`randr_ext.rs:409-433`); DestroyMode, AddOutputMode and DeleteOutputMode
  run their access checks and then succeed doing nothing (`:338-403`).
  Request bodies are already swapped to little-endian by the reader
  (`client_reader.rs:184`, `request_swap::swap_request_body`), so handlers
  decode little-endian.
- `state.randr` is **rebuilt from the backend** on every SetCrtcConfig,
  force-probe, hotplug and VT resume (KMS `kms/randr.rs:1078`, surface
  `surface/present.rs:93`). Anything stored in it is lost.
- Backend mode ids come from `RandrIdAllocator` (`kms/state.rs:1058`), keyed by
  exact timing (`ModeIdentity`, `drm/modeset.rs:60-104`) and never pruned, so
  a timing keeps its id across rescans. Outputs, CRTCs, modes and providers
  share one counter starting at 1.
- `ModeSpec` (`backend/trait_def.rs:32-36`) carries only width, height and
  refresh. Every backend resolves the mode by `(w, h, vrefresh)` in the
  connector's list and takes the kernel's own `drm` mode
  (`kms/render/platform/connectors.rs:323-373`, `drm/modeset.rs:1093-1099`).
- `ModeTiming` (`randr.rs:102-123`) and the KMS `mode_timing`/`ModeIdentity`
  (`kms/randr.rs:126`) have no `hskew` and keep only the low six flag bits;
  mode names are synthesized as `"WxH"` (`randr.rs:705`).
- RANDR notifications read mode ids straight from `state.randr.outputs`
  (`run.rs:4121`), and `access_resource_owner` recognizes a RandrMode only if
  it is in `state.randr` (`server.rs:1763-1769`).
- Server XIDs: client 0 owns `0..FIRST_CLIENT_BASE` (`0x0010_0000`,
  `server.rs:24`). The fixed server XIDs are `0x100..=0x10a`
  (`resources/mod.rs:31-61`). The KMS backend RANDR counter starts at 1 and
  only grows, across rescans too, so it **already** reaches `0x100` once 255
  outputs, CRTCs, modes and providers have been handed out, and aliases the
  root window. Surface (`3`, `4..`) and nested (`1..=3`) ids stay small.
- SetCrtcConfig already maps a backend `Err` to `RRSetConfigFailed`
  (`randr_ext/crtc.rs:930-934`) and already runs a `TEST_ONLY` commit on KMS
  (`drm/modeset.rs:1809-1821`).
- Client monitors (`SetMonitor`) already live outside `RandrState`, in
  `ServerState::randr_client_monitors`, for the same rebuild reason
  (`randr.rs:1077-1080`). A server reset seeds a fresh `ServerState`
  (`core_loop/reset.rs`), which clears them.

## 3. Design

### 3.1 Core state: `ServerState::randr_user_modes`

```rust
pub struct UserMode {
    pub id: ResourceId,
    pub name: Vec<u8>,
    pub info: ModeInfo,          // the full xRRModeInfo as sent, id field replaced
}
pub struct RandrUserModes {
    modes: Vec<UserMode>,                       // creation order
    on_output: HashMap<u32, Vec<ResourceId>>,   // output id -> user modes, add order
    next_id: u32,
}
```

- Stored in `ServerState` next to `randr_client_monitors`, so backend rebuilds
  never touch it, and a server reset clears it with the rest of the state.
- `ModeInfo` keeps every wire field (`dotClock` in Hz, `hSkew`, all flag bits)
  so GetScreenResources echoes exactly what the client sent, as Xorg does.
- Keyed by **output id**. KMS output ids are stable per connector across
  rescans (`RandrIdAllocator::connectors`), and Xorg keeps user lists on
  disconnected connectors, so entries are not pruned on hotplug. An output id
  the backend no longer reports is simply not listed.
- "Referenced" for DestroyMode = present in any `on_output` list, or equal to
  a CRTC's reported current mode (3.4).

### 3.2 XID ranges in the server's space

Client 0's space (`0..0x0010_0000`) gets explicit, bounded ranges, defined
once next to `FIRST_CLIENT_BASE`:

| Range | Use |
|---|---|
| `0x0000_0001..=0x0000_0FFF` | fixed server XIDs (root, colormaps, visuals, COW, ...) and the small fixed surface/nested RANDR ids |
| `0x0001_0000..=0x0007_FFFF` | backend RANDR ids (KMS `RandrIdAllocator`) |
| `0x0008_0000..=0x000F_FFFF` | RANDR user modes |

- Each allocator asserts it stays in its range; running out answers
  `BadAlloc` for CreateMode and logs and stops handing out new backend ids
  (it cannot wrap into another range).
- Moving the backend counter is a fix of its own (commit 0): the counter
  aliases the root window today on a machine that has handed out 255 RANDR
  ids, which rescans make reachable over time. Unit test: an allocator
  seeded near its old limit never returns a fixed server XID.

### 3.3 Requests

- **CreateMode:** checks in Xorg's order
  (table above). The name check runs against the user modes **and the names
  the replies advertise for backend modes** (today the synthesized `"WxH"`),
  so `--newmode 1920x1080 …` fails with BadName when a probed 1920x1080 mode
  is listed, as on Xorg. Window lookup goes through the existing access path
  with GetAttr. Then the policy is asked `CREATE` on the new mode id
  (`ResourceKind::RandrMode`, owner `SERVER_OWNER`) before insertion; a denial
  answers the policy's error and nothing is stored. The mode is recorded as
  owned by `SERVER_OWNER`. Reply
  `mode = id`. No events.
- **DestroyMode:** unknown id → BadRRMode; a backend mode → BadMatch;
  referenced → BadAccess; else remove. Existing `DESTROY` access check stays
  where it is.
- **AddOutputMode:** unknown output → BadRROutput; unknown mode (user or
  backend) → BadRRMode; already listed for the output → Success, no event;
  else append, bump the config timestamp and emit through
  `emit_randr_connector_change_notifications` with an **empty CRTC list** and
  this output in the output list: RRScreenChangeNotify + RROutputChangeNotify,
  no RRCrtcChangeNotify. (`emit_randr_change_notifications` passes the same
  list as both and would add a CrtcChangeNotify.) No backend validation at add time
  (modesetting accepts everything here; the kernel decides at set time).
- **DeleteOutputMode:** not in the output's user list → BadAccess; the
  output's CRTC currently reports this mode → BadMatch; else remove. No
  events.
- Byte order: no handler change. DestroyMode, AddOutputMode and
  DeleteOutputMode already have swap entries (`request_swap.rs:425`, `:452`).
  Add CreateMode: window, then the `ModeInfo` id, width, height, dotClock,
  h/v sync start/end/total, nameLength and modeFlags; the name is untouched
  (`SProcRRCreateMode`, `randr/rrsdispatch.c:221-246`). Xorg does **not** swap
  `hSkew` there, and we match it: a big-endian client's skew arrives
  byte-swapped, as on Xorg.
  Tests feed big-endian wire data for all four requests through
  `swap_request_body`.
- `access_resource_owner` recognizes user modes in the new store (owner
  `SERVER_OWNER`), so DestroyMode/AddOutputMode/SetCrtcConfig access checks
  see them.

### 3.4 One mode projection for replies, events and checks

The backend keeps building `state.randr`. The core applies one projection
over it, `RandrView` (state.randr + user modes), and every consumer goes
through it: GetScreenResources(Current), GetOutputInfo, GetCrtcInfo, the
RANDR notifications (`run.rs:4121`), SetCrtcConfig validation, and the
"referenced" checks of DestroyMode/DeleteOutputMode.

- **Output modes:** backend list, then the output's user modes not already
  listed; `num_preferred` unchanged.
- **Screen modes:** Xorg's order from section 1. Names: the user's name for
  user modes, the existing synthesis for backend modes.
- **CRTC current mode:** Xorg's `xf86RandRModeMatches`
  (`xf86RandR12.c:996-1038`) over the CRTC's outputs, backend lists before
  user lists: equal `dotClock / 1000` (kHz, truncated), width, hSyncStart,
  hSyncEnd, hTotal, **hSkew**, height, vSyncStart, vSyncEnd, vTotal, and
  `modeFlags & FLAG_BITS` (the fourteen bits `RR_HSyncPositive..=RR_ClockDivideBy2`).
  Names are not compared. No match: the backend id.
- A backend id that the projection replaced by a user mode is not advertised
  anywhere (replies or events), so clients only ever see one id per CRTC.

This needs the backend to report the **full** programmed timing: the
platform `drm::Mode`, `ModeIdentity` and `ModeTiming` all carry `hskew`, and
keep all `FLAG_BITS` (the KMS `mode_timing` stops masking to six bits). VidMode's `GetModeLine` forwards
`mode_flags` verbatim and the `V_*` namespace diverges above bit 10
(`randr.rs:111-118`), so VidMode masks to the six shared bits itself before
it encodes; that keeps its current bytes.

### 3.5 SetCrtcConfig to a user mode

- `validate_set_crtc_config` accepts a mode in the output's backend list or
  its user list (`randr.rs:838`), BadMatch otherwise, as Xorg.
- `ModeSpec` gains the full timing for user modes:

  ```rust
  pub struct ModeSpec {
      pub width: u16,
      pub height: u16,
      pub vrefresh: u32,
      pub user_timing: Option<UserTiming>,   // None for advertised modes
  }
  ```

  `UserTiming` is the widened `ModeTiming` from 3.4 plus width and height:
  clock in kHz (`dotClock / 1000`, truncated as Xorg), h/v sync start/end/total,
  hskew, and `modeFlags & FLAG_BITS`. The RANDR
  and DRM flag bits are identical for these fourteen bits
  (`randr.h:160-173`, `drm_mode.h:71-84`), so the mask is the whole mapping.
- **KMS:** when `user_timing` is set, build the `drm` mode from it instead of
  resolving it in the connector's list: empty name, `type = 0`, `vrefresh`
  left for the kernel, as Xorg. The connector-list re-checks in
  `kms/crtc_config.rs` (`:497`, `:682-693`) and
  `resolve_connector_enable` accept it. The existing `TEST_ONLY` commit runs
  first; a refusal becomes `Err`, which the core already answers with
  `RRSetConfigFailed`. Nothing else changes in the commit path.
- **Change detection compares full timing.** `apply_crtc_config` treats an
  unchanged `(w, h, vrefresh, x, y)` as a no-op (`kms/crtc_config.rs:996`),
  and `PerCrtcChange::classify` treats an unchanged `(w, h, vrefresh)` as
  position-only (`:1302`). A user mode with the same nominal size and refresh
  but different sync timing would then succeed without being programmed. Both
  compare the effective timing (the `ModeIdentity` of the programmed mode
  against the requested one) instead; `ConnectorConfig::Enabled` carries it.
- **The programmed mode must survive** rescans, VT resume and DPMS. Today
  `ConnectorConfig::Enabled` stores only w/h/refresh (`kms/state.rs:1103-1114`),
  while relighting uses the stored `layout.output.mode` (`kms/session.rs:679`,
  `:780`) and rescans keep `picked`/`mode` (`connectors.rs:1243-1248`). Store
  the full `drm` mode (or the `UserTiming`) wherever a re-resolution by
  `(w, h, vrefresh)` would otherwise fail to find it in the connector list.
  Each of those paths gets a test (5).
- **Surface hosts (macOS, Windows):** `apply_window_crtc_config` already
  accepts any size that fits and ignores timing (`surface/platform.rs:629-657`);
  a user mode resizes the window like any other. User modes still appear in
  the replies through 3.4.
- **Nested and KMS-rootless:** unchanged, `apply_crtc_config` stays a no-op
  success there (`trait_def.rs:825-834`).

### 3.6 Lifetime

- No per-client cleanup: user modes are server-owned and survive their
  creator, as on Xorg.
- Server reset clears them with `ServerState`. A CRTC still programmed with a
  user mode at reset keeps scanning out; the new generation's replies report
  the backend's id for that timing, as for any mode not in the connector
  list. (Xorg's modesetting re-sets the initial configuration on reset; we
  don't change our reset behaviour here.)
- Hotplug: user lists are kept; an unplugged output simply isn't listed. On
  replug, its user modes come back with it, as on Xorg.

## 4. Commits

0. XID ranges (3.2): backend RANDR ids move to their range, bounded
   allocators, the alias test. Independent of user modes; could land first
   on its own.
1. Core: user-mode store, requests 16-19 with Xorg's errors and events, swap
   entries, `access_resource_owner`, the `RandrView` projection for replies,
   events and checks (3.1, 3.3, 3.4 without the timing match). SetCrtcConfig
   with a user mode answers **`RRSetConfigFailed` explicitly** in this commit:
   the backend still resolves by `(w, h, vrefresh)` and could pick a different
   advertised timing.
2. Full timing: `ModeTiming` with hskew and all flag bits, VidMode's own mask,
   the current-mode match (3.4).
3. `ModeSpec::user_timing`, KMS mode construction and the paths that must keep
   the programmed mode; lift the refusal from commit 1 (3.5).
4. Surface hosts: confirm the size path, tests.
5. vng scenario `xrandr-usermode` and its golden.

## 5. Tests

**Unit (core), expected values from the table in 1 and the Xvfb run:**
- CreateMode: reply id, server-owned, BadName on a duplicate user name and on
  a backend mode's name, BadLength on a short name, BadWindow, same timings
  under a new name gives a new id, big-endian client.
- DestroyMode: BadRRMode, BadMatch for a backend mode, BadAccess while on an
  output list and while current, success after removal.
- AddOutputMode: errors, duplicate no-op without events, and with Screen,
  Crtc and Output subscriptions all selected: exactly one ScreenChangeNotify
  and one OutputChangeNotify, no CrtcChangeNotify; timestamp bump.
- Byte order: big-endian CreateMode/DestroyMode/AddOutputMode/DeleteOutputMode
  through `swap_request_body` give the same results as little-endian.
- Access: a recording policy sees the user mode on DestroyMode,
  AddOutputMode and SetCrtcConfig.
- Projection: notifications after a set to a user mode carry the user mode's
  id, never the backend id for that timing.
- Timing match: hskew difference, a flag above bit 5 (e.g. `RR_CSync`), and a
  dot clock differing below 1 kHz (matches, as Xorg's truncation).
- VidMode `GetModeLine` bytes unchanged for a mode with high DRM flag bits.
- DeleteOutputMode: BadAccess for a backend mode and for a user mode not on
  the output, BadMatch while current, no events.
- Replies: GetScreenResources order and names, GetOutputInfo order and
  `nPreferred`, CRTC current-mode mapping (backend timing match wins over a
  user mode).
- Survival: a backend rebuild (hotplug replay on the recording backend) keeps
  user modes and their output lists; client disconnect keeps them; server
  reset clears them.

**Unit (KMS):**
- `UserTiming` → `drm` mode field by field (clock kHz truncation, flags mask,
  empty name, type 0).
- SetCrtcConfig with a user mode not in the connector list reaches the
  commit; a `TEST_ONLY` refusal gives `RRSetConfigFailed`.
- A user mode with the same width, height and refresh as the current mode
  but different sync timing is programmed, both at the same position (not a
  no-op) and at a changed position (not position-only).
- `hskew` round-trips through `drm::Mode`, `ModeIdentity` and `ModeTiming`.
- The programmed user mode survives a rescan, VT suspend/resume and DPMS
  off/on (the `per_crtc_config.rs` and `tests/randr.rs` fixtures).

**vng:** `tools/vng-scenarios/xrandr-usermode.sh` runs the measured Xvfb
sequence (newmode, duplicate, addmode twice, set, rmmode/delmode while
current, switch back, delmode, rmmode) and sets a user mode with different
timings from every listed mode, checking the scanout size. Golden from the
same script on Xvfb; masks for ids.

**Hardware:**
- The #360 reporter: VGA without EDID, `--newmode/--addmode/--output --mode`
  for 1920x1080 from `.xinitrc`, then VT switch and DPMS.
- One of ours: a user mode with the timings of a listed mode but a new name
  (the current-mode mapping), and a refused mode (an absurd clock) giving
  `RRSetConfigFailed` without disturbing the output.

## 6. Open questions

- `-dpi`/physical size: a user mode changes the screen size like any set; no
  special handling planned.
