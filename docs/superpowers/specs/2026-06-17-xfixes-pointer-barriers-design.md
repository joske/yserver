# XFIXES / XInput pointer barriers — design

**Date:** 2026-06-17 · **Status:** design (pre-implementation) · **Scope:** full barriers (confinement + XI2 `BarrierHit`/`BarrierLeave` events + `XIBarrierReleasePointer`)

## Motivation

yserver already forces XFIXES ≥ 5.0 in `QueryVersion` because Mutter/muffin refuses to start a WM otherwise (`crates/yserver-protocol/src/x11/xfixes.rs:55`). But `CreatePointerBarrier`/`DeletePointerBarrier` (XFIXES minor 31/32) and `XIBarrierReleasePointer` (XI2 minor 61) currently fall through their dispatchers' catch-all arms as silent no-ops. This is the cardinal *"advertised but can't deliver"* sin from the 2026-06-17 tech-debt audit (Tier 2, **HIGH**).

The two facets of a barrier:

1. **Confinement** — the pointer physically stops at the barrier line (Xorg `barrier_clamp_to_barrier`).
2. **The XI2 feedback loop** — `BarrierHit`/`BarrierLeave` events let a client measure how hard the pointer is pushing, and `XIBarrierReleasePointer` lets it wave the pointer through one crossing.

**Why we must do both (not confinement-only):** Mutter places barriers at multi-monitor seams and relies on the Hit→Release round-trip to implement "push firmly to cross to the next monitor." Today's no-op lets the pointer slide freely between monitors. Confinement-only would turn those seam barriers into *un-crossable walls* — the pointer gets **trapped** at the monitor edge, strictly worse than the no-op for the exact client we advertise v5 for. So v1 is the complete feature.

This document follows the project rule that Xorg is the de-facto spec. All algorithm/wire references are to `/home/jos/Projects/xserver` (`Xi/xibarriers.c`, `mi/mipointer.c`) and `/usr/include/X11/extensions/{xfixesproto.h,xfixeswire.h,XI2proto.h,XI2.h}`.

## Non-goals

- Multiple master pointers. yserver models a single master pointer (XI device 2); barrier per-device state collapses to that one device. The wire still parses/validates the device-id list.
- Physically holding the host cursor under **ynest**. ynest does not own the sprite (the real X host does), so a barrier there is *advisory*: it clamps the coordinates yserver reports to its clients, but the host cursor moves freely. Acceptable for the dev/test path; KMS (bare metal) is the real target and the HW-smoke gate.
- Touch/tablet barriers, barrier coordinates spanning multiple CRTCs with per-CRTC transforms. Single root-coordinate space only (yserver's current model).

## Resource model

A `PointerBarrier` is a client-owned XID resource, modeled on the existing `XFixesRegion` pattern (a `HashMap` field on `ServerState`), **not** the core `ResourceTable`.

```rust
// crates/yserver-core/src/server.rs — new field on ServerState, beside xfixes_regions
pub pointer_barriers: HashMap<u32, PointerBarrier>,

pub struct PointerBarrier {
    pub owner: ClientId,
    pub window: ResourceId,   // CreatePointerBarrier `window` arg; selects the screen,
                              // echoed as the events' `event` window
    pub x1: i16, pub y1: i16, // normalized so x1<=x2, y1<=y2 EXCEPT when an endpoint
    pub x2: i16, pub y2: i16, // is negative (ray/line convention — see clamp algorithm)
    pub directions: u32,      // stuff->directions & 0x0f, with axis-irrelevant bits stripped
    pub devices: Vec<u16>,    // empty = all master pointers. Wildcards
                              // XIAllDevices(0)/XIAllMasterDevices(1) also
                              // mean "all" (Xorg barrier_blocks_device,
                              // xibarriers.c:301). yserver has one master
                              // pointer (2), so {empty, [0], [1], [2]} are
                              // all equivalent.

    // --- runtime hit-state for the single master pointer (XI device 2) ---
    pub hit: bool,            // pointer is currently resting on this barrier
    pub seen: bool,           // scratch flag for one constrain pass
    pub event_id: u32,        // monotonic per hit-sequence; starts at 1
    pub release_event_id: u32,// set by XIBarrierReleasePointer; starts at 0
    pub last_timestamp: u32,  // ms of last Hit, for the event `dtime` delta
}
```

Invariants from Xorg: `event_id` starts at 1 and `release_event_id` at 0, so the first sequence is releasable; a fresh `event_id` is minted each time the pointer leaves the hit box.

### Lifetime & XID namespace

- **Create:** XID supplied by the client (like Xorg). Reject if already in use (`state.xid_occupied(id)` → `BadAlloc`).
- **Free:** on `DeletePointerBarrier` and on client disconnect — `pointer_barriers.retain(|_, b| b.owner != client_id)` added to `crates/yserver-core/src/core_loop/process_disconnect.rs` (beside the `xfixes_regions` sweep). On free, if the barrier is currently `hit`, synthesize a `BarrierLeave` first (Xorg `BarrierFreeBarrier`, xibarriers.c:655). This non-motion leave has a specific payload: `flags = XIBarrierPointerReleased`, `sourceid = 0`, `dx = dy = 0` (FP3232 zero), and preserves the barrier's current `root`/`event` window and `event_id`/`dtime`. Don't emit a generic zeroed event — sourceid and the released flag are load-bearing for clients tracking the sequence.
- **XID namespace registration (MANDATORY — XC-MISC time-bomb guard, see `project_xcmisc_missing_wm_death`):** add `pointer_barriers` to `xid_occupied` (`server.rs:1429`), `used_xids_in` (`server.rs:1444`), and extend the `xid_occupied_covers_every_namespace` test (`server.rs:4832`).
- **Window destroy:** the barrier's `window` is a *reference* for screen selection + event routing, not an owner — the barrier is its own resource and is NOT freed when the window dies (matches Xorg; in practice the window is almost always the root, which never dies). **Confinement is independent of the window** and keeps working. Event delivery mirrors Xorg `ProcessBarrierEvent` (`Xi/exevents.c:1724`): it does `dixLookupWindow(be->window)` and **returns on failure with NO root fallback** — so while the barrier's window is absent, `BarrierHit`/`BarrierLeave` events are simply **dropped** (not rerouted to root). The barrier itself persists until `DeletePointerBarrier` or owner disconnect. (Do not invent a forced-leave or root-fallback on window destroy — that would diverge from upstream.)
- **Screen / RANDR geometry change:** barrier coordinates are absolute root coords and are not rescaled. After a geometry change a barrier may fall partly or wholly outside the new bounds — it simply stops being hit there (the segment-intersection test naturally returns no crossing). If the pointer was resting on a barrier that geometry-change moved out from under it, the next motion's hit-box test emits the `BarrierLeave`. No special rescale logic; document that barriers do not migrate with outputs.

## Wire protocol

Parsing helpers go in `crates/yserver-protocol/src/x11/xfixes.rs` (add minor consts `CREATE_POINTER_BARRIER = 31`, `DELETE_POINTER_BARRIER = 32`; existing readers `read_u16_le`/`read_i16_le`/`read_u32_le`). All parse helpers return `Option` and the caller ignores `None` (malformed), consistent with the file.

### CreatePointerBarrier (XFIXES minor 31) — reply-less

`xfixesproto.h:508` (`sz_… = 28`):

| off | field | type |
|----|-------|------|
| 0  | barrier | CARD32 (new XID) |
| 4  | window | CARD32 |
| 8  | x1 | INT16 |
| 10 | y1 | INT16 |
| 12 | x2 | INT16 |
| 14 | y2 | INT16 |
| 16 | directions | CARD32 |
| 20 | pad | CARD16 |
| 22 | num_devices | CARD16 |
| 24 | device_ids[num_devices] | CARD16 each, padded to 4 |

(Offsets above are within the request body **after** the 4-byte generic header that yserver's dispatch already strips; the on-wire struct adds the 4-byte `reqType/xfixesReqType/length` prefix, i.e. wire offset = body offset + 4.)

`directions` bitmask (`xfixeswire.h:137`) names the directions of motion the barrier **permits**:
- `BarrierPositiveX = 1<<0`, `BarrierPositiveY = 1<<1`, `BarrierNegativeX = 1<<2`, `BarrierNegativeY = 1<<3`.
- A direction is *blocked* unless its allow-bit is set; `directions == 0` ⇒ fully solid. Keep only low 4 bits.

A barrier must be axis-aligned: horizontal (`y1 == y2`) or vertical (`x1 == x2`).

### DeletePointerBarrier (XFIXES minor 32) — reply-less

`barrier: CARD32` (body offset 0).

### XIBarrierReleasePointer (XI2 minor 61) — reply-less

`XI2proto.h:839`. Body: `num_barriers: CARD32` (off 0), then `num_barriers ×` `{ deviceid: u16, pad: u16, barrier: CARD32, eventid: CARD32 }` (12 bytes each) at off 4. Dispatched from `handle_xi2_request` (`process_request.rs:9775`), currently catch-all at `:14132`.

### Events: XI_BarrierHit (25) / XI_BarrierLeave (26)

`xXIBarrierEvent` (`XI2proto.h:1068`), a GenericEvent (type 35): the 32-byte GenericEvent base + a **36-byte tail** = **68 bytes total** (the tail is 36, not 32, because `dx`/`dy` are `FP3232` = 8 bytes each):

| off | field | notes |
|----|-------|-------|
| 0 | type=35 | GenericEvent |
| 1 | extension | XI major opcode (137) |
| 2 | sequenceNumber | u16 |
| 4 | length | u32 = **9** — `(sizeof(xXIBarrierEvent) − 32)/4` = `(68 − 32)/4`. The event is 68 bytes because `dx`/`dy` are `FP3232` (8 bytes each), so 36 extra bytes past the 32-byte GenericEvent base = 9 words. (NOT 6, and NOT 8 — a count that treats `FP3232` as 4 bytes is wrong.) |
| 8 | evtype | u16: 25 Hit / 26 Leave |
| 10 | deviceid | u16: master pointer (2) |
| 12 | time | Time |
| 16 | eventid | u32 |
| 20 | root | Window |
| 24 | event | Window = barrier's `window` |
| 28 | barrier | CARD32 XID |
| 32 | dtime | u32 ms since previous event in sequence (0 on first) |
| 36 | flags | u32: `XIBarrierPointerReleased=1<<0`, `XIBarrierDeviceIsGrabbed=1<<1` |
| 40 | sourceid | u16 |
| 42 | pad | int16 |
| 44 | root_x | FP1616 (filled with the **post-clamp** final position) |
| 48 | root_y | FP1616 |
| 52 | dx | FP3232 (attempted delta that hit the barrier) |
| 60 | dy | FP3232 |

Delivery: a client receives the event if its `xi2_masks[(barrier.window, master_deviceid)]` (existing per-client storage, `server.rs:1740`) has the corresponding bit set (`XI_BarrierHit` bit 25 / `XI_BarrierLeave` bit 26; both fit in the u32 mask). Reuse the existing XI2 fanout machinery (`xi2_mask_for_client`).

### Errors (match Xorg)

| Condition | Error | Notes |
|----------|-------|-------|
| barrier neither horizontal nor vertical (diagonal) | `BadValue` | xibarriers.c:809 |
| zero-length (both H and V) | `BadValue` | xibarriers.c:813 |
| horizontal with `y1<0\|\|y2<0`, or vertical with `x1<0\|\|x2<0` | `BadValue` | negative reserved for ray convention, xibarriers.c:817 |
| bad `window` | `BadWindow` (errorValue=window) | dixLookupWindow |
| new barrier XID already in use | `BadAlloc` | |
| device id in list is neither a wildcard (`XIAllDevices`=0 / `XIAllMasterDevices`=1) nor the master pointer (2) — e.g. a keyboard or slave id | `BadDevice` (errorValue=device_id) | wildcards + master are accepted (xibarriers.c:301); only genuinely non-applicable ids error |
| DeletePointerBarrier: unknown XID | `BadValue` (errorValue=barrier) | |
| Delete / Release by non-creating client | `BadAccess` | only the owner may destroy/release |
| Release: bad device | `BadDevice` | |
| Release: `num_barriers` overflows request length | `BadLength` | |

Emit via `emit_x11_error` / `emit_x11_error_with_minor` (`process_request.rs:15543`/`:15848`), `major_opcode = XFIXES_MAJOR_OPCODE` (140) for 31/32, `XI2_MAJOR_OPCODE` (137, minor 61) for release.

## Clamp algorithm (port of Xi/xibarriers.c)

Pure functions in a new `crates/yserver-core/src/core_loop/barriers.rs` module, unit-testable without `ServerState`.

```
barrier_get_direction(x1,y1,x2,y2) -> u32:
    dir = 0
    if x2 > x1: dir |= PositiveX ; elif x2 < x1: dir |= NegativeX
    if y2 > y1: dir |= PositiveY ; elif y2 < y1: dir |= NegativeY

inside_segment(v, v1, v2) -> bool:   # negative-endpoint ray convention
    if v1 < 0 and v2 < 0: true              # infinite line
    elif v1 < 0:          v <= v2           # ray
    elif v2 < 0:          v >= v1           # ray
    else:                 v1 <= v <= v2     # finite segment

# T(v,a,b) = (v-a)/(b-a) ;  F(t,a,b) = t*(a-b)+a   (NB sign: a-b)
barrier_is_blocking(b, x1,y1,x2,y2) -> Option<f32 distance>:
    # vertical barrier at x=b.x1, spanning y in [b.y1,b.y2]:
    t = T(b.x1, x1, x2)
    if t < 0 or t > 1: None
    if x2 > x1 and t == 0: None             # sitting on barrier, moving +X away
    y = F(t, y1, y2)
    if not inside_segment(y, b.y1, b.y2): None
    Some( sqrt((y-y1)^2 + (b.x1-x1)^2) )
    # horizontal: mirror image (swap X/Y roles; edge case y2>y1 && t==0)

barrier_find_nearest(barriers, dir, x1,y1,x2,y2) -> nearest blocking barrier:
    among barriers that (a) are NOT already marked `seen` this pass
    (else the same barrier is re-selected every iteration and the loop spins),
    (b) block one of `dir`'s bits
    (barrier_is_blocking_direction: (b.directions & d) != d), (c) apply to the
    device, (d) geometrically block — pick min distance.

barrier_clamp_to_barrier(b, dir, &mut x, &mut y):
    # vertical:
    if (dir & NegativeX) & ~b.directions: x = b.x1        # approached from +X side
    if (dir & PositiveX) & ~b.directions: x = b.x1 - 1    # approached from -X side
    # horizontal:
    if (dir & NegativeY) & ~b.directions: y = b.y1
    if (dir & PositiveY) & ~b.directions: y = b.y1 - 1
    # only the blocking axis is modified; the cursor slides ALONG the barrier
```

**Iterative constrain** (Xorg `input_constrain_cursor`): loop while `dir != 0`: find nearest *unseen* blocking barrier; mark it `seen` and `hit`; clamp; remove the now-resolved axis from `dir` (vertical clears X bits, horizontal clears Y bits) and advance the current coord on that axis; emit a `BarrierHit`. A diagonal move that crosses both a vertical and a horizontal barrier resolves in two passes. After the loop, in a second sweep clear every barrier's `seen`; for any barrier still flagged `hit` whose final position left its hit box (`barrier_inside_hit_box`, `HIT_EDGE_EXTENTS = 2`), **clear `hit`**, emit `BarrierLeave`, and increment `event_id`. Clearing `hit` is load-bearing: `new_sequence = !hit`, so a barrier that stays `hit` forever never re-arms a fresh Hit sequence (Xorg xibarriers.c:509).

The `release_event_id` short-circuit (Xorg xibarriers.c:460): in the loop, `if barrier.event_id == barrier.release_event_id { continue; }` — skip clamping this barrier for the released crossing. Once the pointer leaves the hit box, `event_id` increments past `release_event_id`, so it re-arms (one-shot release).

## Integration into the motion path

Hook: `crates/yserver-core/src/core_loop/pointer_fanout.rs`, in `pointer_event_fanout_to_state`, **immediately after** the existing confinement block (~line 119) and **before** committing `state.pointer_root = (event.root_x, event.root_y)` (~line 185).

- Old position = `state.pointer_root`; proposed = `(event.root_x, event.root_y)`.
- Run the constrain against `state.pointer_barriers` for the screen of the barrier's `window`. Mutate `event.root_x/root_y` to the clamped result in place (same idiom the confine block uses).
- **Relative-motion only:** barriers constrain genuine device motion, never `WarpPointer` (Xorg only constrains `Relative`). Gate with a bypass flag analogous to the existing `state.confine_warp_active`: set `state.barrier_bypass` while servicing the `WarpPointer` request (`handle_warp_pointer`, `process_request.rs:21953`) and during the warp-back re-entry, so explicit warps and our own corrective warps skip the clamp.

### The cross-thread catch on KMS (the key risk)

On bare metal the cursor position has **two authorities**:
1. the libinput **input thread** (`crates/yserver/src/input_thread.rs:127`) — its own `cursor_x/y` accumulator, clamped to FB bounds, **no `ServerState` access**;
2. the **core thread** (`crates/yserver/src/kms/v2/backend.rs:5597` `process_pointer_absolute` → fanout).

Clamping only in core lets the input-thread accumulator march past the barrier, so the wall won't physically hold (the next relative delta is measured from the un-clamped accumulator). The existing **confinement feature has this same latent drift** (flagged during research). Fix, shared by both features:

- Extend the input-thread control channel with a `SetPosition { x, y }` command. The current channel is a **one-byte Pause/Resume latch** — it cannot carry coordinates, so `SetPosition` needs its own synchronized slot (e.g. an `AtomicU64` packing `x:i32|y:i32` plus a "position-dirty" flag the thread checks each iteration, or a small `mpsc`/`ArrayQueue`). After a barrier/confine clamp on KMS, the core publishes the corrected absolute position; the input thread overwrites its `cursor_x/y` accumulator with it on the next iteration.
- **Invalidate coalesced motion.** The input thread coalesces motion into a `pending_motion` before emitting. A `SetPosition` MUST drop/replace any in-flight `pending_motion`, otherwise a stale pre-clamp delta is replayed *after* the correction and drives the cursor straight back through the barrier. This is the subtlest race in the feature — call it out explicitly in the impl and test it (rapid motion into a barrier must not jitter across).
- Continue to pull the HW cursor plane via `backend.warp_pointer_root` (`backend.rs:15987`), guarded against re-entrancy by the existing `confine_warp_active`-style flag (warp_pointer_root re-enters the fanout).
- **Ordering:** because the input thread runs concurrently and keeps accumulating, treat the core's clamped position as authoritative and the input thread's accumulator as a cache that the core overwrites — never the reverse. A `SetPosition` that races a just-emitted motion is harmless as long as `pending_motion` is cleared, since the next real delta is applied to the corrected base.

ynest: `warp_pointer_root` is a no-op (`trait_def.rs:1911`); the clamp only adjusts reported coordinates. Documented advisory behavior.

## XI2 event delivery & release

- **Hit:** in the constrain loop, build `xXIBarrierEvent` (evtype 25). `new_sequence = !barrier.hit`; `dtime = new_sequence ? 0 : now - last_timestamp`; `dx/dy = proposed - old` in FP3232; `root_x/root_y` patched with the final clamped position (FP1616). Deliver to clients selecting `XI_BarrierHit` on `barrier.window` for the master pointer.
- **Leave:** when a hit barrier's final position leaves the hit box, evtype 26, then `event_id += 1`.
- **XIBarrierReleasePointer:** for each `{deviceid, barrier, eventid}`: owner check (`BadAccess` otherwise); if `barrier.event_id == eventid` set `barrier.release_event_id = eventid`. Effect realized by the loop's short-circuit above.
- **Grab semantics (Xorg `ProcessBarrierEvent`, `Xi/exevents.c:1724-1744`):** two *separate* rules — do not conflate them:
  1. **Flag:** whenever the master pointer is actively grabbed, set `XIBarrierDeviceIsGrabbed` (1<<1) in the emitted event — unconditionally, regardless of who holds the grab.
  2. **Grabbed-path delivery (narrowly gated):** route via the grab (`DeliverGrabbedEvent`) **only when BOTH** `CLIENT_ID(barrier) == CLIENT_ID(grab)` (the barrier's creating client owns the grab) **AND** `grab.window == barrier.window`. Otherwise — including for *unrelated* grabs held by other clients — fall through to **normal** window-mask delivery (`xi2_masks[(barrier.window, device)]`), with the flag still set. A blanket "any grab → grabbed-path" rule is wrong: it would suppress/misroute barrier events whenever any client holds an unrelated pointer grab.

## Module / file plan

| File | Change |
|------|--------|
| `crates/yserver-protocol/src/x11/xfixes.rs` | minor consts 31/32; `parse_create_pointer_barrier`, `parse_delete_pointer_barrier`; barrier-event encoders |
| `crates/yserver-protocol/src/x11/xi2.rs` (or sibling) | `parse_xi_barrier_release`; `xXIBarrierEvent` encoder |
| `crates/yserver-core/src/core_loop/barriers.rs` (new) | pure clamp/direction/segment/hit-box functions + unit tests |
| `crates/yserver-core/src/server.rs` | `pointer_barriers` field + `PointerBarrier` struct; `xid_occupied`/`used_xids_in` + test |
| `crates/yserver-core/src/core_loop/process_request.rs` | XFIXES 31/32 arms; XI2 61 arm; barrier-bypass on WarpPointer |
| `crates/yserver-core/src/core_loop/pointer_fanout.rs` | constrain hook + event emission + warp-back resync |
| `crates/yserver-core/src/core_loop/process_disconnect.rs` | owner sweep |
| `crates/yserver/src/input_thread.rs` + control channel | `SetPosition` resync — **not** a trivial enum add: the channel is a one-byte Pause/Resume latch, so this needs a coordinate-carrying slot/queue + `pending_motion` invalidation (see KMS section) |

## Testing

Per `feedback_vng_pass_not_hw_pass`: vng is the iteration signal; **bee multi-monitor GNOME is the release gate.** Per `feedback_test_vectors_must_be_external`: clamp expected-values come from Xorg's documented math, and the wire layouts are asserted against the C structs in xfixesproto.h / XI2proto.h — not from my own arithmetic.

1. **Clamp math (unit, `barriers.rs`):** fixed vectors for: vertical/horizontal block, directional pass-through (allowed direction not clamped), the `x1` vs `x1-1` asymmetric snap, diagonal two-axis resolution, ray (`v<0`) vs finite-segment `inside_segment`, the `t==0` moving-away edge case. Expected positions hand-derived from `barrier_clamp_to_barrier`.
2. **Parse (unit):** request bodies decoded to the right fields, offsets matching xfixesproto.h; `directions & 0x0f`; device-list parse.
3. **Encode (unit):** `xXIBarrierEvent` byte layout asserted field-by-field against `XI2proto.h:1068`.
4. **Validation (integration):** every error row above.
5. **Release state machine (integration):** Hit → ReleasePointer(eventid) → cross succeeds → leave hit box → re-arms; stale eventid release is a no-op.
6. **Lifetime:** disconnect frees barriers; `DeletePointerBarrier`-while-hit synthesizes `BarrierLeave`+released flag; **window-destroy** — confinement keeps holding but barrier events are *dropped* (no root fallback, matching `ProcessBarrierEvent`), and the barrier survives; geometry-shrink makes an out-of-bounds barrier un-hittable; XID namespace test extended.
7. **Grab routing:** (a) any active grab sets `XIBarrierDeviceIsGrabbed`; (b) grabbed-path delivery only when the barrier's owner holds the grab AND grab.window == barrier.window — an *unrelated* client's grab still gets normal window-mask delivery (with the flag set).
8. **`pending_motion` invalidation (KMS, the race):** simulate rapid relative motion straight into a barrier; assert the post-clamp position holds and no stale coalesced delta replays the cursor across (regression guard for the subtlest race).
9. **HW smoke (gate):** bee dual-head GNOME — pointer holds at monitor seam, firm push crosses (release path), no trap; `xinput` barrier test app sanity. Capture an xtrace from a real Xorg barrier session to validate wire/events against (`feedback_xorg_is_the_de_facto_spec`).

## Risks

- **Input-thread `SetPosition` resync (KMS)** — the part most likely to need HW iteration; touches the existing confinement path. Mitigation: land it as its own commit and verify confinement still holds before adding barriers on top.
- **Relative vs absolute gating** — Xorg constrains `mode == Relative` motion only. yserver has a live absolute-pointer path (libinput `MotionAbsolute` for touch/tablet → `process_pointer_absolute` → fanout), and the relative/absolute distinction is lost where `input_thread.rs` collapses both into `HostInputEvent::PointerMotion`. Resolution: add a `relative: bool` to `HostInputEvent::PointerMotion`, set `true` only for libinput relative motion (`false` for `MotionAbsolute` and for warp-injected motion); `process_pointer_absolute` converts `!relative` into `barrier_bypass` around the fanout dispatch. The barrier hook clamps only when `!barrier_bypass && !confine_warp_active`. This gates absolute touch/tablet AND every KMS warp (warp re-injects `relative:false`) with one mechanism — no per-warp-site helper. ynest motion doesn't traverse `process_pointer_absolute`, so it still clamps reported coords (the documented advisory-on-ynest behavior).
- **Per-screen barrier scoping** — yserver's single root-coordinate space simplifies this vs Xorg's per-`ScreenPtr` lists, but the `window`→screen mapping must be correct for multi-CRTC.
```
