# XINERAMA extension — multi-head support (fixes muffin/mutter crash)

**Date:** 2026-06-12
**Branch:** `feat/lightdm-launch` (XINERAMA is a ship-blocker for usable multi-head DM sessions)
**Reference:** `/usr/include/X11/extensions/panoramiXproto.h` (wire format); Xorg
`randr/rrxinerama.c` (the **RANDR-backed** Xinerama implementation — registered
by `RRXineramaExtensionInit`, `randr/randr.c:449`). Mirror *that*, not the
legacy full-PanoramiX path (`Xext/panoramiX.c`), since yserver is RANDR-backed:
`ProcRRXineramaIsActive` reports active when monitor count > 0
(`rrxinerama.c:159`), `QueryScreens` returns RANDR monitor rects
(`rrxinerama.c:274`), and all requests use `REQUEST_SIZE_MATCH` + validate the
window via `dixLookupWindow`.

## Goal

Implement the **XINERAMA** X extension so multi-head window managers (muffin/
mutter, and others) see **one Xinerama screen per RANDR monitor**, eliminating
a NULL-deref crash and giving correct multi-monitor layout.

## Root cause (confirmed on hardware, silence, 2026-06-12)

yserver advertises **RANDR** (reports *N* monitors via `RR_GET_MONITORS`) but
implements **no XINERAMA**. mutter/muffin, when `XineramaIsActive()` is false,
builds its **logical monitor** list from RANDR (*N* entries) but collapses its
**xinerama** list to a single synthesized screen. Mapping logical monitor #1
(the second head) then does:

```
meta_display_logical_index_to_xinerama_index(1)
  → g_list_nth(xinerama_list /* len 1 */, 1) → NULL
  → mov (%rax),%rbx on NULL → SIGSEGV     (libmuffin.so, ~11s after start)
```

Proven by: **single-head works, dual-head crashes, same binary.** Real Xorg
derives Xinerama from its monitors, so its xinerama list always matches its
monitor count — no overrun. The fix is to give yserver the same property.

## The load-bearing invariant

**Xinerama screen count MUST equal the RANDR monitor count, always.** The two
must be sourced from the *same* data so they can never disagree. yserver builds
RANDR monitors from `state.randr.outputs` (1:1, primary = index 0, in
`RR_GET_MONITORS` at `process_request.rs:2290`). XINERAMA `QueryScreens` MUST
source from that **same list, same order**. This invariant — not any single
reply field — is what prevents the crash.

## Scope

Implement **all six** PanoramiX/XINERAMA requests (no stubs — yserver's standing
rule, and the legacy PanoramiX trio predates the `Xinerama*` calls so older WMs
may use it):

| # | Request | Reply |
|---|---------|-------|
| 0 | `PanoramiXQueryVersion` | server version (1.1) |
| 1 | `PanoramiXGetState` | active flag + echoed window |
| 2 | `PanoramiXGetScreenCount` | N + echoed window |
| 3 | `PanoramiXGetScreenSize` | root/virtual-screen width/height + echoed window/screen |
| 4 | `XineramaIsActive` | active flag (CARD32) |
| 5 | `XineramaQueryScreens` | N screen rects |

No events, no errors of its own (`first_event = event_count = first_error = 0`),
no per-client state, no state mutation — entirely read-only over
`state.randr.outputs`. This is among the simplest X extensions.

## Components

### 1. `crates/yserver-protocol/src/x11/xinerama.rs` (new)

Pure wire layer, mirroring `randr.rs`. Constants + encoders, unit-tested against
the `panoramiXproto.h` byte layouts. All replies are the **32-byte fixed reply
header**; only `QueryScreens` appends `N × 8` bytes.

```rust
pub const MAJOR_VERSION: u16 = 1;
pub const MINOR_VERSION: u16 = 1;

// minor opcodes
pub const QUERY_VERSION: u8 = 0;
pub const GET_STATE: u8 = 1;
pub const GET_SCREEN_COUNT: u8 = 2;
pub const GET_SCREEN_SIZE: u8 = 3;
pub const IS_ACTIVE: u8 = 4;
pub const QUERY_SCREENS: u8 = 5;

/// One Xinerama screen (wire: x_org i16, y_org i16, width u16, height u16).
pub struct ScreenInfo { pub x_org: i16, pub y_org: i16, pub width: u16, pub height: u16 }

pub fn encode_query_version_reply(bo, seq) -> Vec<u8>;        // major=1, minor=1
pub fn encode_get_state_reply(bo, seq, state: bool, window: u32) -> Vec<u8>;
pub fn encode_get_screen_count_reply(bo, seq, count: u8, window: u32) -> Vec<u8>;
pub fn encode_get_screen_size_reply(bo, seq, width: u32, height: u32, window: u32, screen: u32) -> Vec<u8>;
pub fn encode_is_active_reply(bo, seq, active: bool) -> Vec<u8>;
pub fn encode_query_screens_reply(bo, seq, screens: &[ScreenInfo]) -> Vec<u8>;
```

**Exact reply layouts** (all 32 bytes unless noted; `length` is in 4-byte words
of data *beyond* the 32-byte header):

- **QueryVersion**: `[1, pad, seq(2), len=0(4), major u16=1, minor u16=1, pad×20]`
- **GetState**: `[1, state u8, seq(2), len=0(4), window u32, pad×20]` — note `state` rides in **byte 1** (the pad1 slot), per `xPanoramiXGetStateReply`.
- **GetScreenCount**: `[1, ScreenCount u8, seq(2), len=0(4), window u32, pad×20]` — count in **byte 1**.
- **GetScreenSize**: `[1, pad, seq(2), len=0(4), width u32, height u32, window u32, screen u32, pad×8]`
- **IsActive**: `[1, pad, seq(2), len=0(4), state u32, pad×20]`
- **QueryScreens**: `[1, pad, seq(2), len = N*2 (4), number u32 = N, pad×20]` then `N × {x_org i16, y_org i16, width u16, height u16}`.

### 2. `nested::EXTENSIONS` entry

```rust
ExtensionMetadata {
    name: "XINERAMA",
    major_opcode: XINERAMA_MAJOR_OPCODE,   // = 151 (strictly above all
                                           // assigned opcodes; current max is
                                           // MIT-SCREEN-SAVER = 150). Arbitrary
                                           // but unique — clients learn it via
                                           // QueryExtension.
    first_event: 0, event_count: 0, first_error: 0,
    availability: ExtensionAvailability::Always,
    unsupported_minor_policy: UnsupportedMinorPolicy::HandledInline,
}
```

Makes `QueryExtension("XINERAMA")` report present with the major opcode, so
libXinerama (`XineramaQueryExtension`) succeeds and clients proceed to the
`Xinerama*` calls.

### 3. Dispatch arm — `process_request.rs:200` `match header.opcode`

```rust
XINERAMA_MAJOR_OPCODE => handle_xinerama_request(state, client_id, sequence, header, body),
```

### 4. `handle_xinerama_request`

Matches `header.data` (the minor opcode) over the six requests. Builds the
screen list from the **shared monitor-list helper** (see below), mapping each to
`ScreenInfo { x_org: o.x, y_org: o.y, width: o.width, height: o.height }`.

All six requests are fixed-size: validate length with `REQUEST_SIZE_MATCH`
semantics — **any** size mismatch (too short *or* too long) → `BadLength`
(`rrxinerama.c` uses `REQUEST_SIZE_MATCH` for all six). The three windowed
requests (`GetState`, `GetScreenCount`, `GetScreenSize`) carry a `window` and
must **validate it** (`dixLookupWindow` equivalent — yserver's window lookup);
an invalid window → `BadWindow` (`rrxinerama.c:124,172,202`).

- `QueryVersion` → `encode_query_version_reply` (major=1, minor=1).
- `GetState` → validate `window`; `encode_get_state_reply(active = true, window)`.
  Xorg's `PanoramiXGetState` returns true whenever the RANDR screen-private
  exists — i.e. **independent of monitor count** (`rrxinerama.c:129-134`) — and
  yserver always has RANDR, so this is unconditionally `true`. (Distinct from
  `IsActive` below, which *is* the monitor-count path.)
- `GetScreenCount` → validate `window`; `encode_get_screen_count_reply(count = screens.len(), window)`.
- `GetScreenSize` → validate `window`; reply with the **root/virtual-screen**
  width/height (the full bounding size, not the per-monitor size), echoing the
  requested `window` and `screen`. **No bounds-check on `screen`** — Xorg's
  RANDR-backed path returns the root drawable size regardless of the screen
  index and does *not* error (`rrxinerama.c:202,210`). (This is a deliberate
  match to `rrxinerama.c`, replacing an earlier draft that wrongly returned
  per-screen size + `BadValue` — that was legacy full-PanoramiX behavior, not
  the RANDR path yserver mirrors.)
- `IsActive` → `encode_is_active_reply(active = !screens.is_empty())`.
- `QueryScreens` → `encode_query_screens_reply(&screens)`.
- Unknown minor opcode → **BadRequest** (X convention for an extension's
  unknown minor).

**`IsActive` = true whenever ≥1 monitor** (`rrxinerama.c:238` reports active
when monitor count > 0) — so clients use `QueryScreens` (the matching-count
path) rather than the single-screen fallback that crashes. Single-head → 1
screen, IsActive=true, harmless (the one screen is the whole display).
(`GetState` is the separate "RANDR present?" flag above — always true here.)

### 5. Shared monitor-list helper (enforces the invariant mechanically)

To make the load-bearing invariant (XINERAMA screen count == RANDR monitor
count) **impossible to break by future drift**, extract the monitor-list
construction `RR_GET_MONITORS` currently does inline (`process_request.rs:2290`)
into one helper, and have **both** `RR_GET_MONITORS` and XINERAMA `QueryScreens`
/ `GetScreenCount` call it:

```rust
/// The active monitor list — the single source of truth for both
/// RANDR GetMonitors and XINERAMA. Order: primary first (index 0).
fn active_monitors(state: &ServerState) -> Vec<MonitorRect> { /* from state.randr.outputs */ }
```

Xorg does exactly this: `RRGetMonitors` and `RRXineramaQueryScreens` share
`RRMonitorMakeList` (`rrmonitor.c:600`, `rrxinerama.c:284`). If any
enabled/disconnected filtering is later added, it lands in the one helper and
both sides stay consistent — the crash can't silently return.

## Error handling

| Condition | Response |
|-----------|----------|
| Request size mismatch (short *or* long) — all 6 are fixed-size | `BadLength` (`REQUEST_SIZE_MATCH`) |
| Invalid `window` on `GetState`/`GetScreenCount`/`GetScreenSize` | `BadWindow` |
| Unknown minor opcode | `BadRequest` |

(`GetScreenSize` does **not** bounds-check the screen index — see component 4.)

## Testing

- **Unit (wire encoders):** one per reply, asserting exact bytes vs
  `panoramiXproto.h`. Critically a **2-screen `QueryScreens`**: `length == 4`
  (N*2), `number == 2`, and the two 8-byte `ScreenInfo` records for DP-1 @ (0,0)
  2560×1440 and HDMI-A-1 @ (2560,0) 2560×1440.
- **Invariant test:** XINERAMA `QueryScreens` and `RR_GET_MONITORS` both call
  the shared `active_monitors` helper, so their counts are equal *by
  construction*; a test asserts both reply with the same N for a multi-output
  `state.randr.outputs` (guards against any future one-sided filtering).
- **`GetScreenSize`:** returns the root/virtual-screen size and echoes the
  requested `window`/`screen` for any in-bounds-or-not screen index (no
  `BadValue`); an invalid `window` → `BadWindow`.
- **Length/window validation:** an over-long or truncated request → `BadLength`;
  `GetState`/`GetScreenCount`/`GetScreenSize` with a bogus window → `BadWindow`.
- **Dispatch/QueryExtension:** `QueryExtension("XINERAMA")` reports present with
  the major opcode; an unknown minor → `BadRequest`.
- **HW smoke (the real gate):** dual-head Cinnamon under lightdm on silence —
  no longer crashes; `xdpyinfo -ext XINERAMA` reports **2** screens with the
  correct rects; `xrandr` still shows 2 monitors. Per repo practice, the
  display-path change isn't done until observed on hardware.

## Risk / open validation

The crash mechanism (count mismatch) is proven, and matching the counts is the
established Xorg fix. The one thing only HW confirms: that mutter actually
consumes `IsActive=true` + `QueryScreens=N` to build *N* xinerama entries (vs.
still synthesizing one). If it still crashes after this, the next step is to
trace whether this mutter build reads the X **XINERAMA** extension vs. its own
RANDR-monitor list for the xinerama mapping — but the crash function name
(`..._to_xinerama_index`) and Xorg's own XINERAMA-from-RANDR glue both point
squarely here.

## Out of scope

- Runtime display hotplug (separate — GH #9).
- VT switching under the DM (separate, also landing on this branch).
- RANDR 1.5 "monitor" objects that span multiple outputs (yserver maps monitors
  1:1 to outputs today; XINERAMA follows that same mapping).
