# RANDR Output Management (mode-set + layout) — Design

**Status:** Design / spec. Stacked on `feat/drm-hotplug` (issue #9). Not yet planned or implemented.

**Goal:** Make multi-monitor work end-to-end with a real desktop environment (MATE) on the KMS backend. After the hotplug branch, a runtime-connected monitor is detected and clients are notified, but the desktop can't drive it: yserver advertises only each connector's preferred mode, and `RRSetCrtcConfig` / `RRSetScreenSize` are no-ops. So the second monitor comes up at its preferred mode in a server-imposed position, the DE's layout (resolution, position, screen size) is silently ignored, and the result is a blank/mis-placed second screen the DE can't fix. This design implements client-driven RANDR output management so the DE owns layout, matching Xorg.

**Motivating evidence (fuji, MATE, HDMI hotplug, 2026-06-17):** yserver auto-lit HDMI-A-1 at its preferred 1360×768 extend-right; the monitor is capable of 1080p. MATE knew about both displays and re-tiled the laptop wallpaper, but the HDMI showed no wallpaper, stayed at 1360×768, and windows wouldn't drag onto it — because MATE applied its own layout via `RRSetCrtcConfig` (a no-op at `process_request.rs:2477` — *"no-op accept; yserver runs at the KMS-set mode and does not reconfigure outputs"*) and `state.randr.modes` only carried the picked mode, so 1080p wasn't even offered.

## Non-goals

- Rotation / reflection / RANDR transforms / projective scaling (`crtc->transforms`). Out of scope; the v2 compositor has no transform path.
- RANDR providers / output leases / PRIME offload / GPU screens.
- **Client-defined modes** (`RRCreateMode` / `RRAddOutputMode` / `RRDeleteOutputMode`) — only connector-advertised (EDID/kernel) modes are selectable. MATE's resolution picker only ever chooses from the advertised list.
- Panning / per-CRTC gamma / brightness properties.

## Xorg is the de-facto spec

All semantics below are taken from the Xorg RANDR core at `/home/jos/Projects/xserver/randr/` (rotation paths excluded). Cited inline so the implementation and its tests are grounded in real behavior, not assumption.

Key rules:
- **No auto-enable on runtime hotplug.** A newly-connected output flips connection state and fires `RROutputChangeNotify`; `output->crtc` stays `NULL` — the monitor stays **off** until a client enables it via `RRSetCrtcConfig` (`rrinfo.c:173`, `rroutput.c:293`). Startup is separate (`xf86InitialConfiguration` picks an initial layout for already-connected outputs).
- **Screen must encompass active CRTCs.** `RRSetCrtcConfig` returns `BadValue` when `x + mode.width > screen.width` or `y + mode.height > screen.height` (`rrcrtc.c:1436–1471`). `RRSetScreenSize` returns `BadMatch` when the new size would crop any active CRTC (`rrscreen.c:266–281`). ⇒ canonical sequence: grow screen first, then place CRTCs; shrink CRTCs first, then screen.
- **`RRSetCrtcConfig` is synchronous**: validate → DDX modeset → reply `status` (`RRSetConfigSuccess`/`RRSetConfigFailed`) → fire notifies (`rrcrtc.c:1291–1502`, notify via `RRTellChanged`).
- **"Off" = `mode=None, outputs=[]`** on a still-connected output (how MATE disables the laptop panel when docked) — distinct from disconnected.
- **Mode lists**: `GetOutputInfo` returns the output's full mode list, preferred modes first, with `nPreferred` (`rroutput.c:461–593`). `GetScreenResources` returns the deduped union of all outputs' modes (`rrscreen.c:482–638`, `rrmode.c`). CRTC/output arrays are the full set regardless of connection.
- **`GetScreenResources` forces a re-probe; `GetScreenResourcesCurrent` is cached.** `GetScreenResources` calls `RRGetInfo(force_query=TRUE)` to re-query the driver before replying and bumps `lastConfigTime`; `GetScreenResourcesCurrent` returns the cached view (`rrscreen.c:482`, `rrinfo.c:173`). They are NOT interchangeable.
- **`RRSetScreenSize` does more than resize.** `RRScreenSizeSet` emits a root `ConfigureNotify`, fires `RRScreenChangeNotify`, bumps the changed/config timestamps via `RRTellChanged`, and re-fixes pointer bounds (`RRPointerScreenConfigured` / `ScreenRestructured`) (`rrscreen.c:136,152,167`).

## Architecture & layering

Mirror the hotplug split:
- **`yserver-core`** owns RANDR protocol state + request handlers: validation, replies, notify fanout. No DRM/Vk.
- **KMS backend (`yserver`)** owns DRM apply behind new `Backend` trait methods: modeset, scanout alloc/free, scene rebuild, root/COW storage + logical-screen resize.

This keeps the layering that already exists (emit in core, GPU work in backend) and lets the core-side validation be unit-tested without hardware.

### Connector registry vs active outputs (the structural shift)

Xorg separates outputs (which may have `crtc=None`) from CRTCs. yserver runs strict 1 connector = 1 CRTC = 1 output, but still needs the "known but off" state. Introduce a **connector registry** (grown from the existing `RandrIdAllocator`) keyed by connector name, holding for every connector yserver has seen:
- stable `output_id` + `crtc_id` (allocated once, never renumbered — already implemented),
- full mode list with stable per-`(w,h,vrefresh)` mode ids, preferred-first + `num_preferred`,
- connection state (`Connected` / `Disconnected`),
- current config: `Off` or `Enabled { mode_id, x, y }`,
- **`client_configured: bool`** — the persistent bit that distinguishes server-default auto-layout from client-set geometry. Set `true` whenever a `RRSetCrtcConfig` (or `RRSetScreenSize`) touches the output; cleared on disconnect (a reconnected monitor reverts to default until the DE reconfigures it). This is the flag the rescan/resume preservation rule keys off (below): `recompact_horizontal_layout` and any auto-extend only ever touch `!client_configured` outputs; `client_configured` outputs keep their stored `(x,y,mode)` verbatim.

`platform.outputs` continues to hold **only enabled** connectors (one scanning-out `OutputLayout` each); the scene still ticks one entry per element — unchanged. `RandrState.outputs` is rebuilt from the registry: enabled connectors take geometry from `platform.outputs`; off/disconnected report `crtc=0, mode=0`. The disconnected-but-queryable behavior from the hotplug branch is subsumed by the registry.

## Data model changes

- `drm::modeset::Output`: add `modes: Vec<Mode>` — the connector's full local mode list (already computed as `local_modes` in `discover_outputs`, currently discarded), preferred-first. `picked` stays as the boot default.
- `RandrOutput` (core): add `mode_ids: Vec<u32>` (available, preferred-first) and `num_preferred: u16`; `mode_id` becomes the *current* mode (0 = off); keep `connected`, `crtc_id`, `x`, `y`. State derives: `connected=false` ⇒ disconnected; `connected=true, mode_id=0` ⇒ off; else enabled.
- `RandrState`: `screen_width`/`screen_height` become the **logical** screen size — set by `RRSetScreenSize`, defaulting to the auto bounding box at boot and after hotplug. Mode list (`modes`) becomes the deduped union of all registry connectors' mode lists.
- **Bounding-box math must be 2-D.** `RandrState::from_outputs` currently computes `screen_height = max(height)` (`randr.rs:67`) and `recompute_fb_extent_from` ignores `y` (`platform.rs`); `recompact_horizontal_layout` forces `y=0`. With client-driven layout a CRTC can sit at any `(x,y)` (vertical stacking — external monitor above the laptop is common), so both must become `screen_width = max(x+width)`, `screen_height = max(y+height)`. (The compose path is already 2-D correct — it offsets each output by `-layout.x/-layout.y` before sampling, `scene.rs:1903` + `composite.vert.glsl:35` — so only the extent/backing math needs the fix.)
- **The registry is authoritative for position/mode; the rescan path must stop overwriting it.** `recompact_horizontal_layout` is the **boot/default** extend-right helper only. Today `requery_outputs_and_modeset` calls it **unconditionally** after every rescan (`platform.rs:2351`) and both resume and display-rescan run through that path (`backend.rs:4784,4949`), so a client's `(x,y)` (and any non-default mode) would be flattened on the next hotplug or VT resume. This must change: an output whose registry entry has `client_configured = true` keeps its `(x,y,mode)` verbatim across rescan/resume; `recompact_horizontal_layout` runs only over `!client_configured` outputs (the boot default / a brand-new connector before any `SetCrtcConfig`). Concretely: drop the unconditional recompact from the rescan path (`platform.rs:2351`) and re-apply each enabled output's registry config instead. The VT-resume relight (`dpms_set_outputs_active(true)`, already on the branch) likewise re-commits the registry's stored mode, not a fresh auto-layout. (This is target behavior the implementation establishes — the current code still recompacts unconditionally.)
- Connector registry struct (in `yserver` backend; the core sees only the resulting `Vec<RandrOutput>` via `randr_outputs()`): name → { ids, modes, connection, current config }.

## Request handlers (`process_request`, core)

- **`GetScreenResources` / `GetScreenResourcesCurrent`**: emit the deduped union mode list; all crtcs/outputs from the registry.
- **`GetOutputInfo`**: full mode-id list preferred-first + `nPreferred`; `crtc = 0` when off; `connection` from registry; possible-crtcs = the single stable crtc id (1:1).
- **`GetCrtcInfo`**: enabled ⇒ `x,y,mode,width,height` + the one output; off ⇒ `x=y=w=h=0, mode=0`.
- **`GetScreenResources`** forces a connector re-probe via a new backend hook `reprobe_connectors()` (updates the registry + bumps `config_timestamp` on change) before building the reply; **`GetScreenResourcesCurrent`** uses the cached registry snapshot. (yserver's udev hotplug normally keeps the registry fresh, but matching the force-probe covers a missed uevent and the timestamp contract.)
- **`GetMonitors` (RANDR 1.5) / XINERAMA** (`active_monitors`, `process_request.rs:2085/2341`): yserver has no manual `RRSetMonitor`/`DeleteMonitor` (out of scope), so the list is purely the **automatic** monitors Xorg derives from outputs — and Xorg builds an automatic monitor only for an output that has an active CRTC (`rrmonitor.c:193,247,309,578`). So the list is **one monitor per enabled output**, at its client-set `(x,y,width,height)`; off and disconnected outputs are **absent** (not zero-geometry entries). With no manual monitors, the `get_active` flag's only effect (filtering zero-area manual monitors) is moot, so `true` and `false` return the same enabled set — accept the flag for protocol-compliance but build the same list either way. XINERAMA's `QueryScreens` maps to this set. It must track the client-set layout, not the raw boot order — this is what MATE/marco read for per-monitor placement, maximize, and panel geometry.
- **`RRSetScreenSize`**: validate width/height against `screen_size_range` (`BadValue` out of range); **reject `BadMatch` if it would crop any enabled output** (`x+w` / `y+h` beyond new size, `rrscreen.c:266`); `BadValue` for zero physical mm; call backend `set_logical_screen_size(w,h)`; on success, matching `RRScreenSizeSet`: update `RandrState` logical size + root/overlay records, **emit a root `ConfigureNotify`** to `StructureNotify` selectors, fire `ScreenChangeNotify`, bump changed/config timestamps. `handle_host_container_resize` already does the root `ConfigureNotify` + RANDR fanout (`run.rs:859,887,918`) and can be reused for *those*, but it does **not** clamp the pointer — there is no screen-resize pointer path today (the only clamp is confine-window, `process_request.rs:14875`). So add an **explicit pointer step**: clamp `cursor_x/y` into `[0,w)×[0,h)` and warp the cursor in if the screen shrank below its current position (Xorg `RRPointerScreenConfigured`/`ScreenRestructured`, `rrscreen.c:152,167`). The KMS motion clamp already reads `fb_w/fb_h`, but that only takes effect on the *next* motion event — the explicit warp is needed so a shrunk screen doesn't leave the cursor stranded off-screen.
- **`RRSetCrtcConfig`**: replace the no-op. Validation order matching `rrcrtc.c`:
  1. `mode == None` ⇒ `numOutputs == 0` else `BadMatch`; `mode != None` ⇒ `numOutputs >= 1` else `BadMatch`.
  2. output(s) resolve to known connectors; the addressed crtc is the output's crtc (1:1) else `BadMatch`.
  3. `mode` ∈ that output's mode list else `BadMatch`.
  4. if enabling: `x + mode.width <= logical_screen.width` and `y + mode.height <= logical_screen.height` else `BadValue`.
  5. call backend `apply_crtc_config(connector, mode_opt, x, y)`; DDX failure ⇒ reply `status = RRSetConfigFailed` (not a protocol error).
  6. on success: update registry current config; rebuild `RandrState`; fire per-changed `CrtcChangeNotify` + `OutputChangeNotify` (+ `ScreenChangeNotify` only if the screen actually changed); reply `RRSetConfigSuccess` + timestamp.

## Backend trait methods (`trait_def`, impl in KMS v2; default no-op for ynest/recording)

- `apply_crtc_config(&mut self, connector: &str, mode: Option<ModeSpec>, x: i32, y: i32) -> io::Result<()>`
  - `mode = None` (disable): `disable_output` on its CRTC; free/disarm its scanout pool; remove from `platform.outputs` (+ scene state) via the wait_idle+drain+rebuild path; registry → Off. Keeps the connector known.
  - `mode = Some` (enable/change): map `ModeSpec` → the connector's DRM mode; if not currently enabled or the resolution changed, (re)allocate the scanout pool at the new size; `commit_modeset` at the mode; set `OutputLayout` `{x,y,width,height}`; add to / update `platform.outputs`; scene rebuild. Registry → Enabled.
- `set_logical_screen_size(&mut self, w: u16, h: u16) -> io::Result<()>` — reallocate root + COW backing storage to `w×h`; update `platform.fb_w/fb_h` (pointer clamp + logical extent follow). (Root/COW storage currently allocated once at `fb_w×fb_h`; this is its resize path.)
- `reprobe_connectors(&mut self) -> io::Result<()>` — re-run `discover_outputs` and reconcile the registry (connection state + mode lists), without changing any enabled output's config. Called by the `GetScreenResources` (force-query) handler. Idempotent; reuses the hotplug rescan's discover/diff logic minus the auto-enable.

`ModeSpec` carries the resolved `(width, height, vrefresh)` so the backend can find the exact DRM mode without a core→DRM type leak.

## Lifecycle

- **Boot** — auto-enable connected outputs extend-right at preferred mode (matches `xf86InitialConfiguration`); registry + `platform.outputs` populated; logical size = bounding box.
- **Hotplug add** — register connector **off** (mode list discovered, `crtc=None`), fire `OutputChangeNotify`. No scanout/modeset. (Revises the eager add-path in the hotplug branch.)
- **Hotplug remove** — registry → disconnected; if it was enabled, `disable_output` + free scanout + remove from `platform.outputs`; fire notify.
- **`SetCrtcConfig` enable/disable** and **`SetScreenSize`** — as above; these are the only paths that change an output's mode/position/on-state at runtime.

The server never re-imposes a layout on an output the client has configured; it only re-touches an output on connector disconnect. The VT/seat-resume re-light (the `dpms_set_outputs_active(true)` fix already on the branch) re-commits the *current* registry config, not a fresh auto-layout.

## Error handling

| Request | Condition | Result |
|---|---|---|
| SetCrtcConfig | mode=None with outputs, or mode set with 0 outputs | `BadMatch` |
| SetCrtcConfig | output doesn't drive the crtc (1:1 mismatch) | `BadMatch` |
| SetCrtcConfig | mode ∉ output's advertised modes | `BadMatch` |
| SetCrtcConfig | enabling CRTC exceeds current logical screen | `BadValue` |
| SetCrtcConfig | DDX modeset / scanout alloc fails | reply `status = RRSetConfigFailed` |
| SetScreenSize | out of `screen_size_range` | `BadValue` |
| SetScreenSize | would crop an enabled CRTC | `BadMatch` |
| SetScreenSize | zero physical mm | `BadValue` |

A `RRSetConfigFailed` (or screen-resize alloc failure) must leave the server in its prior consistent state (no partial enable). On an enable that fails after scanout alloc but before commit, free the freshly-allocated pool and stay off.

## Testing strategy

**Core unit tests** (no GPU; expected values grounded in the cited Xorg line numbers, not invented):
- mode-list union is deduped; per-output list is preferred-first with correct `nPreferred`.
- `SetCrtcConfig` validation matrix → exact error codes for each failure row above.
- `SetScreenSize` crop-rejection (`BadMatch`) and range (`BadValue`).
- registry transitions: connected→enabled→off→disconnected, and that off/disconnected stay queryable (`crtc=0,mode=0`), primary stays on an enabled output, no phantom 0-modes.
- `RandrState` rebuild from a registry with mixed states yields correct `screen_resources` / `output_info` / `crtc_info`.
- 2-D bounding box: an output at `(0, 1080)` (stacked below) yields `screen_height = 1080 + its height`, not `max(height)`.
- XINERAMA/`GetMonitors`: off and disconnected outputs are absent from the active monitor list; enabled outputs appear at their client-set `(x,y,w,h)`.
- `GetScreenResources` bumps `config_timestamp` after a reprobe that changed connection state; `GetScreenResourcesCurrent` does not reprobe.
- Position preservation: after a client sets an output to `(x=0, y=1080)` (stacked below) or a non-default mode, a subsequent rescan (hotplug of a *third* connector) and a VT-resume both leave that output's `(x,y,mode)` unchanged — `recompact_horizontal_layout` does not flatten it.
- `GetMonitors`: the list is one monitor per enabled output at its client-set geometry; off and disconnected outputs are absent in both `get_active` branches (no manual monitors), and an output stays out of the list until a `SetCrtcConfig` enables it.
- `SetScreenSize` shrink: cursor positioned beyond the new bounds is warped inside on the resize, not just on the next motion.

**Backend (DRM/Vk)** — HW-only; covered by the acceptance gate.

**HW acceptance (fuji, MATE) — the real gate:**
1. `xrandr --output HDMI-A-1 --mode 1920x1080 --right-of eDP-1` → HDMI lights at 1080p, MATE paints wallpaper on it, windows drag across the seam.
2. Resolution change via MATE Display settings applies.
3. Disable laptop panel (`--output eDP-1 --off`) → eDP goes dark, HDMI keeps working; re-enable restores it.
4. Hotplug while idle → monitor stays dark until configured (Xorg-faithful), then `xrandr --auto` / MATE lights it.
5. Regression: single-monitor MATE unchanged; VT-switch away/back still re-lights (resume fix); rendercheck/XTS unaffected.

## Implementation phasing (for the plan; lands as one stacked set on `feat/drm-hotplug`)

1. `Output.modes` + connector registry + `randr_outputs()` rebuild from registry (full mode list, preferred-first); fix the 2-D bounding-box math (`max(y+height)`).
2. `GetScreenResources` (force re-probe via `reprobe_connectors`) vs `GetScreenResourcesCurrent` (cached) + `GetOutputInfo`/`GetCrtcInfo` full modes + correct off/disconnected reporting; `GetMonitors`/XINERAMA from enabled outputs only.
3. `set_logical_screen_size` backend method + `RRSetScreenSize` handler (crop check + root `ConfigureNotify` + pointer re-clamp + timestamps, via the `handle_host_container_resize` machinery).
4. `apply_crtc_config` backend method + `RRSetCrtcConfig` handler (validation matrix + enable/disable/mode-change).
5. Revise hotplug add-path to off-until-configured.
6. HW acceptance on fuji.

## Open questions / risks

- **COW vs root storage on resize**: with marco compositing on, the scanout source is the COW; with it off, root+windows. `set_logical_screen_size` must resize whichever backs the logical screen; confirm both paths during implementation.
- **Reconfigure bursts**: a DE applies SetScreenSize + N×SetCrtcConfig back-to-back. Each is applied + replied synchronously (Xorg-faithful); position-only changes skip scanout realloc, so only genuine resolution changes pay the drain+realloc cost. No coalescing (would desync the per-request status reply).
- **Mode count**: connectors can advertise many modes; the union list is bounded by hardware and fine to send verbatim (Xorg does).
