# XINERAMA extension — multi-head support (fixes muffin/mutter crash)

**Date:** 2026-06-12
**Branch:** `feat/lightdm-launch` (XINERAMA is a ship-blocker for usable multi-head DM sessions)
**Reference:** `/usr/include/X11/extensions/panoramiXproto.h`; Xorg `Xext/panoramiX.c` / `Xext/xvmc`… (PanoramiX/Xinerama dispatch)

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
| 3 | `PanoramiXGetScreenSize` | width/height of screen *i* + echoed window/screen |
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
screen list **from the same `state.randr.outputs` the RANDR monitor path uses**
(same order, primary first), mapping each to `ScreenInfo { x_org: o.x, y_org:
o.y, width: o.width, height: o.height }`.

- `QueryVersion` → `encode_query_version_reply`.
- `GetState` → `encode_get_state_reply(active = !screens.is_empty(), window = req.window)`.
- `GetScreenCount` → `encode_get_screen_count_reply(count = screens.len(), window = req.window)`.
- `GetScreenSize` → if `req.screen < screens.len()`: reply with that screen's
  width/height (+ echoed window/screen); else **BadValue** (Xorg behavior).
- `IsActive` → `encode_is_active_reply(active = !screens.is_empty())`.
- `QueryScreens` → `encode_query_screens_reply(&screens)`.
- Unknown minor opcode → **BadRequest** (X convention for an extension's
  unknown minor).

**IsActive / GetState `active` = true whenever ≥1 output** — so clients use
`QueryScreens` (the matching-count path) rather than the single-screen fallback
that crashes. Single-head → 1 screen, active=true, harmless (the one screen is
the whole display).

## Error handling

| Condition | Response |
|-----------|----------|
| `GetScreenSize` screen index ≥ N | `BadValue` |
| Unknown minor opcode | `BadRequest` |
| Truncated request body | `BadLength` (consistent with other handlers) |

## Testing

- **Unit (wire encoders):** one per reply, asserting exact bytes vs
  `panoramiXproto.h`. Critically a **2-screen `QueryScreens`**: `length == 4`
  (N*2), `number == 2`, and the two 8-byte `ScreenInfo` records for DP-1 @ (0,0)
  2560×1440 and HDMI-A-1 @ (2560,0) 2560×1440.
- **Invariant test:** for a given `state.randr.outputs`, the XINERAMA screen
  count equals the `RR_GET_MONITORS` monitor count (guards the load-bearing
  property against future drift).
- **`GetScreenSize` bounds:** in-range returns the right size; out-of-range →
  `BadValue`.
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
