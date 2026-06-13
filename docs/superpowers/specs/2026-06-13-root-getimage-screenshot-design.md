# Root-window GetImage (screenshot support) — design

**Date:** 2026-06-13
**Status:** draft
**Branch:** `feat/root-getimage-screenshot` (worktree `yserver-master`)
**Issue:** [#21 — Are screendumps supported?](https://github.com/joske/yserver/issues/21)

## Problem

Screenshot tools (`xwd -root`, `xfwm4-screenshooter`, `scrot`, `maim`,
ImageMagick `import`, …) come back blank on yserver. They capture by
issuing `GetImage` on the root window (optionally with a sub-rectangle),
but yserver composites each frame **directly into the scanout BO** — the
root window has no live backing `storage` image to read. So
`handle_get_image` (`core_loop/process_request.rs:18711`) dispatches to
`backend.get_image`, which resolves the target and reads
`store.get(target.id).storage` — and for the root that storage is
absent/blank. The handler then writes a zeroed `GetImage` reply.

Per the issue owner's comment, this is "not implemented", not a security
model: the only current way to capture the screen is the Ctrl-Alt-Enter
diagnostic dump (`do_dump_scanout_v2`, `kms/v2/backend.rs:7328`), which
writes a PPM of the scanout BO to the cwd.

## Goal

Make `GetImage` on the root window return the composited on-screen
pixels, so standard X11 screenshot tools work — for full-screen grabs,
sub-region (drag-select) grabs, and window grabs alike, matching Xorg
`GetImage` semantics.

## Non-goals

- **Multi-head / XINERAMA rect stitching** — a single root `GetImage`
  rect that spans more than one output. Deferred to a follow-up
  (see "Multi-head: deferred"). Single-output rects (the common case)
  are fully supported now.
- **Hardware cursor in the capture** — the HW cursor lives on a separate
  DRM plane (`cursor_plane.rs`, legacy ioctls), not in the scanout BO.
  Xorg's `GetImage` also omits the cursor, so excluding it is correct,
  not a gap.
- **Any access-control / security model** — per the issue owner, match
  Xorg: any client may read the root.

## Background: what already works

- **Window grabs already work.** Every `DrawableKind::Window` gets its
  own GPU `storage` in the `DrawableStore` (`kms/v2/store.rs`), and
  `engine.get_image` (`kms/v2/engine.rs:4070`) reads back any drawable's
  `storage` via a GPU→staging→CPU copy. A `GetImage` targeting a specific
  window XID therefore returns that window's own (unobscured) content.
  This path is unchanged.

- **The readback mechanism exists.** `do_dump_scanout_v2`
  (`kms/v2/backend.rs:7328`) already does the exact scanout-BO →
  `TRANSFER_SRC_OPTIMAL` → `cmd_copy_image_to_buffer` → mapped staging →
  CPU sequence (then writes PPM). It selects the live BO by phase
  (`BoPhase::OnScreen` preferred, then Pending/Submitted/Recording).

The gap is exclusively the **root window**, and the fix is to feed root
`GetImage` from the scanout BO.

## Capture paths by target

The fix splits by *what* is grabbed; only the full-screen case is an
inherently large copy:

| Tool target | Path | Cost |
|---|---|---|
| Specific window (xwd-click, "active window") | **Existing** — `engine.get_image` reads the window's own `storage` | Cheap |
| Area/region on root (drag-select) | **New** — scanout readback of just the requested sub-rect | Cheap; copy sized to the selection |
| Full screen (`xwd -root`, "whole desktop") | **New** — scanout readback of the full BO | One full-frame device→host copy, on demand only |

This addresses the perf concern: today's `do_dump_scanout_v2` always
copies the *entire* BO of *every* pool through a freshly-allocated
staging buffer with `ALL_COMMANDS` barriers. `GetImage` carries
`(x, y, width, height)`, so for the common region/window-area grab we
copy only that sub-rectangle. Full-screen is unavoidably a full copy, but
it is occasional and `GetImage` is already a synchronous CPU readback
(`engine.get_image` blocks on `ticket.wait()`), so the added latency is
acceptable.

## Architecture

### 1. `read_scanout_region(rect) -> io::Result<Vec<u8>>`

Refactor the GPU→staging→CPU core out of `do_dump_scanout_v2` into a
reusable function on the platform/backend that takes a **root-relative
rect** and returns its pixels as a tightly-packed byte buffer in scanout
pixel order.

Differences from the current full-BO dump:
- **Sub-rect copy** — `BufferImageCopy` with `image_offset` /
  `image_extent` set to the (clamped) rect, not the whole BO.
- **Staging sized to the rect**, not the full BO.
- **Tighter barriers** — `COPY`-scoped src/dst stages instead of
  `ALL_COMMANDS` (the dump's heavy barriers were conservative for a
  diagnostic path).
- **BO selection reused** — same `BoPhase::OnScreen`-preferred selection
  as the dump.

`do_dump_scanout_v2` is refactored to call `read_scanout_region` (with a
full-BO rect) and keep its PPM-writing tail, so there is one readback
implementation with two callers.

Row stride: the returned buffer is packed to `width * bpp` rows (no BO
pitch padding), matching what the `GetImage` reply tail and
`engine.get_image` already produce.

### 2. Root detection + hook in `KmsBackendV2::get_image`

In `KmsBackendV2::get_image` (`kms/v2/backend.rs:11601`), when the
resolved target is the root/screen (no live backing storage), route to
`read_scanout_region(rect)` instead of `engine.get_image`. The
**existing tail is shared unchanged**: `z_to_xy_planes` (XYPixmap),
`apply_z_plane_mask` (partial plane mask), and `wrap_get_image_reply`
(32-byte reply header). Scanout is already BGRX-order, the same as window
`storage`, so the bytes flow through the existing format handling with no
new conversion.

The root/screen is identified by the backend's known root drawable id /
screen geometry (the same identity the compositor already tracks for
scanout); precise predicate confirmed during implementation.

### 3. Handler reachability for the root

`handle_get_image` (`process_request.rs:18711`) only calls
`backend.get_image` when `host_drawable_target(drawable)` is `Some`. If
the root window yields no host target, add an explicit root-window branch
that still invokes the backend scanout path. All existing validation —
viewable, rect within window+border, fully on-screen → `BadMatch`;
unknown drawable → `BadDrawable`; bad format → `BadValue` — is preserved
and runs before the readback.

### 4. Multi-head: deferred

A root rect intersecting more than one output requires copying each
intersecting output's sub-region into its correct position in the result
buffer. Out of scope for this change. Behaviour for a spanning rect in
the interim: read from the single output the rect's origin falls in
(documented limitation), so the common single-monitor and
within-one-output cases are correct. The follow-up adds stitching across
`scanout_pools`.

## Data flow (root region grab)

```
client GetImage(root, x,y,w,h, ZPixmap, plane_mask)
  └─ handle_get_image: validate (viewable / on-screen / format)
       └─ backend.get_image(root target, rect)
            ├─ root/screen?  → read_scanout_region(clamped rect)
            │     └─ select OnScreen BO → barrier → copy sub-rect
            │          → staging → CPU bytes (BGRX, packed rows)
            ├─ z_to_xy_planes / apply_z_plane_mask  (shared tail)
            └─ wrap_get_image_reply(depth=24, bytes)
       └─ patch sequence[2..4] + visual[8..12]; write_to_client
```

## Error handling

- **Renderer failed / no Vulkan / no live BO** → `read_scanout_region`
  returns `Err`; backend returns `Ok(None)`; handler falls back to the
  existing zeroed reply (degrade, don't crash) — same contract as
  `engine.get_image` failure today.
- **Rect clamping** — clamp the requested rect to BO bounds before the
  copy (the handler already rejects off-screen rects with `BadMatch`, so
  clamping is a defensive backstop; mirror `engine.rs:clamp_rect`).
- **Empty rect** after clamp → return empty bytes (matches
  `engine.get_image`'s `copy_w == 0 || copy_h == 0` path).

## Testing

- **Unit** — `read_scanout_region` rect-clamp and packed-stride math;
  sub-rect copy equals the corresponding window of a full-BO copy.
- **Round-trip** — fill the scanout BO with a known pattern, root
  `GetImage` a sub-rect, assert the returned bytes (in the style of
  `fill_then_get_image_observes_clear_color`, `engine.rs:9597`, and
  `depth32_put_image_get_image_round_trip`, `engine.rs:9528`).
- **Smoke (HW, bee)** — with a desktop up:
  - `xwd -root -out /tmp/s.xwd && xwdtopnm /tmp/s.xwd > /tmp/s.pnm` shows
    the real desktop.
  - A region/drag grab (`scrot -s` or screenshooter region) yields the
    selected area.
  - A window grab (xwd-click / "active window") yields that window
    (the existing window-storage path — regression check).
  - The Ctrl-Alt-Enter PPM dump still works (shared `read_scanout_region`).

## Risks

- **BO phase races** — reusing the dump's `OnScreen`-preferred selection;
  if no committed frame exists yet (very early boot) the readback may hit
  a pre-clear BO. Acceptable: `Err` → zeroed reply.
- **Synchronous stall on full-screen 4K** — ~33 MB device→host copy plus
  fence wait stalls the single-threaded core loop for a few ms during the
  grab. Acceptable for an on-demand, infrequent operation; region grabs
  (the common case) are tiny.
- **Pixel format assumption** — assumes scanout is BGRX-order matching
  window `storage` / root visual depth-24. If a future scanout format
  diverges, the shared format tail would need a conversion step; called
  out so it isn't silently wrong.
