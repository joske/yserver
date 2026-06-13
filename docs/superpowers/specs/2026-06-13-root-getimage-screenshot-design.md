# Root-window GetImage (screenshot support) — design

**Date:** 2026-06-13
**Status:** draft (rev 3 — codex review folded in)
**Branch:** `feat/root-getimage-screenshot` (worktree `yserver-master`)
**Issue:** [#21 — Are screendumps supported?](https://github.com/joske/yserver/issues/21)

## Summary

Generic X11 screenshot tools (`xwd -root`, `scrot`, ImageMagick `import`,
`xfwm4-screenshooter`) **hang** on yserver. Live diagnosis on a running
lightdm/Cinnamon `yserver :0` (single 2560×1440 output) showed the cause
is **not** a blank image and **not** the root having no storage (both
were earlier wrong guesses). The real failure is a per-client output cap
that is smaller than a single legitimate full-screen `GetImage` reply, so
the reply is never delivered. A second, content-level gap means that even
once the reply is delivered it would show the background without windows.

This revises the original (rev 1) premise, which incorrectly assumed root
`GetImage` returned a zeroed reply.

## Background: how this differs from the compositor's own screenshots

Cinnamon's PrintScreen works because Muffin **is** the compositor: it
redirects every window (Composite), textures them via GLX
texture-from-pixmap onto its own Clutter/Cogl GL stage, and reads that
stage back (`cogl read_pixels`) for screenshots — entirely inside the
compositor, never issuing X11 `GetImage`. Generic tools have no
compositor to ask, so they do the textbook thing: `XGetImage` on the root
window over the wire. That wire path is what's broken.

## Root cause (confirmed by log evidence)

Diagnosis sequence:
- `xwd -root` / `import -window root` against `:0` → hang (15 s timeout,
  0-byte output). yserver process state `Sl` (idle), **not** blocked in a
  fence wait — so the request was processed and the loop returned to
  idle; the client blocks waiting for reply bytes that never arrive.
- This signature = an X11 reply-framing/delivery failure: the client
  reads a reply header promising N words of pixel data, then blocks in
  `read()` for bytes the server never sends.
- Both reply writers were ruled out as mis-framing:
  - `write_get_image_reply` (`yserver-protocol/.../x11/mod.rs:2712`,
    the fallback path) appends `data_bytes` of zeros matching its declared
    `length` — self-consistent (would yield a black image, not a hang).
  - `wrap_get_image_reply` (`kms/v2/backend.rs:8158`) sets
    `reply_length_units = pixel_len/4` and appends exactly `pixel_bytes`
    — self-consistent.
- The common factor is **size**. A near-full-screen reply at 2560×1440
  ZPixmap 32bpp is ~14 MiB.

**Smoking gun** — `target/x-0.log`, three lines matching three capture
attempts:

```
WARN yserver_core::core_loop::client_io: outbound cap exceeded —
     outbound=0 bytes pending, +14526368 new ⇒ Disconnect
```

The write path (`core_loop/client_io.rs`):
1. `write_or_buffer` tries a direct non-blocking `write()` of the ~14 MiB
   reply; the socket returns `EAGAIN` with **0** bytes on the wire.
2. `buffer_or_disconnect` is handed the entire 14,526,368-byte reply,
   sees `0 (pending) + 14.5 MiB > OUTBOUND_CAP (4 MiB)`, and returns
   `WriteOutcome::Disconnect` **without buffering anything**.
3. `write_to_client` maps that to `RequestOutcome::Disconnect(client_id)`.

`OUTBOUND_CAP = 4 * 1024 * 1024` (`client_io.rs:30`) was sized for a
~786 KB QueryFont reply and never reconsidered for full-screen
`GetImage`. **A single legitimate reply exceeds the cap, so it can never
be delivered.**

### Secondary root cause (confirmed by codex review): Disconnect leaves a half-open socket

The handler returns `Disconnect`, yet `xwd` *hung* rather than receiving
an immediate EOF. Root-caused: the disconnect path sends
`ReaderControl::Shutdown` and drops `ClientState`
(`core_loop/process_disconnect.rs:96,173`), but the per-client **reader
thread** is blocked in `read_request` holding a *cloned* `UnixStream`
(`core_loop/client_reader.rs:19`). Dropping `ClientState` drops the
*writer* `Arc<Mutex<UnixStream>>`, but the reader's clone keeps the fd
open, so the kernel never tears the connection down — the client blocks
forever on `read()`. This is the actual reason the observed symptom was a
hang and not an EOF.

This is a **real, independent bug**: with Part 1 in place, legitimate
replies no longer trigger a cap-disconnect, but a *genuine* slow-client
disconnect would still leave a half-open socket. So Part 1 must fix the
teardown, not merely "verify" it.

## Goal

`GetImage` on the root window returns the composited on-screen pixels,
without hanging, so standard X11 screenshot tools work — full-screen,
sub-region, and window grabs — matching Xorg `GetImage` semantics.

## Non-goals

- **Multi-head / XINERAMA rect stitching** — a single root `GetImage`
  rect spanning more than one output. Deferred to a follow-up. The test
  machine is single-output; single-output rects are fully supported now.
  **Interim behaviour must be explicit, not silent:** a root rect that
  spans more than one `scanout_pool` must `Err` (→ degrade to the existing
  reply), NOT silently capture one pool and present it as the whole root.
  A rect contained within a single output is served normally.
- **Hardware cursor in the capture** — separate DRM plane, not in the
  scanout BO; Xorg's `GetImage` also omits it. Correct to exclude.
- **Access-control / security model** — per the issue owner, match Xorg:
  any client may read the root.

## Design

The fix has two independent parts. Part 1 is the immediate blocker (the
hang); Part 2 makes the delivered image correct.

### Part 1 — deliver large replies (fixes the hang)

**Principle:** a single in-flight reply must never be refused by the
backpressure cap. The cap exists to disconnect a peer that is *not
draining* (accumulating backlog), not to reject one large legitimate
reply.

**Change** (`core_loop/client_io.rs`, `buffer_or_disconnect`):
- When `client.outbound.is_empty()` — i.e. we are starting to buffer a
  fresh reply — **always buffer it in full**, regardless of size, and
  return `WouldBlock`. The existing writable-interest machinery
  (`reconcile_client_writable_interest`, `drain_outbound`) then streams it
  out across subsequent `WRITABLE` events while the single-threaded loop
  keeps serving other clients. No blocking writes.
- The `OUTBOUND_CAP` check still applies to *additional* writes that pile
  onto an already-non-empty buffer — that is the genuine
  slow/non-draining-client case, and still ends in `Disconnect`.

This bounds steady-state memory to roughly one in-flight reply per client
(a new huge reply only begins buffering once the previous drained and
`outbound` is empty again), preserves the slow-client protection for
accumulation, and matches Xorg, which delivers large replies rather than
dropping them.

**Considered and rejected:** blunt-raise `OUTBOUND_CAP` to e.g. 64 MiB.
Arbitrary, still fails on 8K (~132 MiB), and weakens accumulation
protection without addressing the conflation of "one big reply" with
"slow client".

**Also in Part 1 (required, not "verify") — close the socket on
disconnect.** The disconnect teardown
(`core_loop/process_disconnect.rs`) must perform an explicit
`UnixStream::shutdown(Shutdown::Both)` on the live socket before/at
`ClientState` removal, so a reader thread blocked in `read_request`
unblocks and the fd is actually released. Without this, a genuine
cap-exceed disconnect (or any disconnect of a client whose reader is
mid-`read`) leaves a half-open socket and the client hangs — exactly the
observed `xwd` symptom. (The writer `Arc<Mutex<UnixStream>>` and the
reader's cloned stream share the fd; closing one handle is not enough —
`shutdown(Both)` acts on the kernel socket regardless of how many
descriptors reference it.)

**Also in Part 1 — byte-order-correct reply header.** The GetImage reply
header fields (sequence, reply length, visual) are currently written
little-endian unconditionally: `wrap_get_image_reply`
(`kms/v2/backend.rs:8158`) uses `to_le_bytes` for the length, and
`handle_get_image` patches sequence `[2..4]` and visual `[8..12]` with
`to_le_bytes` (`process_request.rs:18816`). This is wrong for big-endian
clients — and **XTS connects big-endian clients**, which is the
validation gate. Fix: write these header fields in the client's byte
order (`handle_get_image` already resolves `byte_order` at line 18791;
thread it into `wrap_get_image_reply` and the patch sites, mirroring the
already-correct `write_get_image_reply` fallback which takes
`ClientByteOrder`). This is a pre-existing bug in the **shared** GetImage
reply path, so the fix benefits window GetImage too. **Pixel data is
unchanged** — ZPixmap bytes are emitted in the server's advertised
image-byte-order, which the client already accounts for; only the
fixed header words need per-client ordering.

### Part 2 — return the composited image (content)

Once Part 1 lands, root `GetImage` will reply — but from root *storage*,
which is only the **bottom (background) layer** of the scene
(`scene.rs:1749-1796` samples `DrawableKind::Root` storage as the base
`CompositeDraw`; windows are composited on top into the scanout BO via
`emit_window_subtree`). With a compositor active, Muffin paints the full
desktop to the COW/scanout, and root storage still holds only the
background. So a delivered reply would show wallpaper without windows.

**Empirical gate before implementing Part 2.** Rather than assume, after
Part 1 is built we run `xwd -root` once:
- If it returns the **full desktop** → root storage already reflects the
  composite in this configuration; Part 2 is unnecessary and we close
  the issue.
- If it returns **wallpaper only** → implement scanout readback (below).

**Part 2 implementation (if the gate shows wallpaper-only):**

1. **`read_scanout_region(rect) -> io::Result<Vec<u8>>`** — refactor the
   GPU→staging→CPU core out of `do_dump_scanout_v2`
   (`kms/v2/backend.rs:7328`) into a reusable function taking a
   root-relative rect: sub-rect `BufferImageCopy` (offset/extent), staging
   sized to the rect, `COPY`-scoped barriers (not `ALL_COMMANDS`).
   `do_dump_scanout_v2` becomes one caller (full-BO rect + its PPM tail);
   root `GetImage` becomes the other.
   **BO selection differs from the dump.** The diagnostic dump falls back
   `OnScreen → Pending → Submitted → Recording` (`backend.rs:7341`); a
   user-visible screenshot must NOT — a `Pending`/`Recording` BO can be a
   not-yet-presented or half-composited (torn) frame. `read_scanout_region`
   for `GetImage` uses the **`OnScreen` BO only**; if there is no `OnScreen`
   BO (e.g. very early boot, before the first flip), it returns `Err`, and
   the handler degrades to the existing self-consistent reply rather than
   capturing an in-progress frame. The dump caller keeps its permissive
   fallback via a parameter.
2. **Hook in `KmsBackendV2::get_image`** (`kms/v2/backend.rs:11601`) —
   when the resolved target is the root/screen, source from
   `read_scanout_region(rect)` instead of `engine.get_image`, then run the
   bytes through the **existing shared tail** (`z_to_xy_planes`,
   `apply_z_plane_mask`, `wrap_get_image_reply`). Scanout is BGRX-order,
   matching window storage and the depth-24 root visual, so no new
   conversion.
3. **Handler reachability** (`process_request.rs:18711`) — preserve all
   existing validation (viewable / rect-within-window+border / fully
   on-screen → `BadMatch`; unknown → `BadDrawable`; bad format →
   `BadValue`); ensure the root target actually reaches the backend
   scanout path.

**Capture paths by target** (Part 2 perf shape):

| Target | Path | Cost |
|---|---|---|
| Specific window | existing `engine.get_image` (window storage) | cheap |
| Area/region on root | scanout readback of just the rect | cheap (copy sized to selection) |
| Full screen | scanout readback of full BO | one full-frame copy, on demand |

Sub-rect readback keeps interactive region grabs cheap; only true
full-screen pays the full copy, and `GetImage` is synchronous by nature
(`engine.get_image` already blocks on `ticket.wait()`).

## Data flow (root region grab, after both parts)

```
client GetImage(root, x,y,w,h, ZPixmap, plane_mask)
  └─ handle_get_image: validate (viewable / on-screen / format)
       └─ backend.get_image(root target, rect)
            ├─ root/screen?  → read_scanout_region(clamped rect)   [Part 2]
            ├─ z_to_xy_planes / apply_z_plane_mask  (shared tail)
            └─ wrap_get_image_reply(depth, bytes)
       └─ patch sequence[2..4] + visual[8..12]
       └─ write_to_client → write_or_buffer
            └─ buffers full reply when outbound empty; streams out        [Part 1]
               across WRITABLE events (no cap rejection of one reply)
```

## Error handling

- **Renderer failed / no Vulkan / no live BO** → `read_scanout_region`
  returns `Err`; backend returns `Ok(None)`; handler falls back to the
  existing self-consistent zeroed reply (degrade, don't crash).
- **Rect clamping** — clamp to BO bounds before copy (handler already
  rejects off-screen rects with `BadMatch`; mirror `engine::clamp_rect`).
- **Zero-size / fully-clamped rect** — note the handler currently forces
  `req.width.max(1)` / `req.height.max(1)` before the backend sees the
  request (`process_request.rs:18803`), so a 0×0 `GetImage` yields a 1×1
  pixel, not a zero-length reply. This is pre-existing and immaterial to
  screenshots (never 0-size). The scanout path inherits the same
  `max(1)` input; we do **not** special-case zero here (keep behaviour
  identical to the existing window path rather than diverge). If strict
  zero-length-reply conformance is later wanted, fix it once at the
  handler for all GetImage paths.
- **Genuine slow client** (accumulating backlog past the cap) → still
  `Disconnect`, now via a teardown verified to close the fd.

## Testing

- **Unit (Part 1 — cap)** — `write_or_buffer`/`buffer_or_disconnect`: a
  single reply larger than `OUTBOUND_CAP` with `outbound` empty buffers in
  full and returns `WouldBlock` (currently returns `Disconnect` — this is
  the failing test written first). Accumulation past the cap onto a
  non-empty buffer still returns `Disconnect`.
- **Unit/integration (Part 1 — socket close)** — disconnecting a client
  whose reader is blocked in `read_request` releases the fd: after
  teardown, a `read()` on the peer end returns EOF (0), not a block.
  Failing first: assert the peer sees EOF; pre-fix it would hang.
- **Unit (Part 1 — byte order)** — `wrap_get_image_reply` with a
  big-endian client writes the reply length big-endian; `handle_get_image`
  patches sequence/visual big-endian. Round-trip: a BE client's GetImage
  reply header decodes to the correct sequence/length/visual. (XTS BE
  GetImage is the on-HW gate.)
- **Unit (Part 2, if implemented)** — `read_scanout_region` rect-clamp /
  packed-stride math; sub-rect copy equals the matching window of a
  full-BO copy.
- **Round-trip (Part 2)** — fill the scanout BO with a known pattern,
  root `GetImage` a sub-rect, assert returned bytes (style of
  `fill_then_get_image_observes_clear_color`, `engine.rs:9597`).
- **Smoke (HW, the live `:0` or a `just startx` session)** —
  `xwd -root | xwdtopnm` returns the desktop (no hang); region grab; a
  window grab (existing path, regression check); Ctrl-Alt-Enter PPM dump
  still works (shared `read_scanout_region`).

## Risks

- **Per-client memory** — Part 1 lets one in-flight reply (up to a
  full-screen image, ~14 MiB at 2560×1440, ~33 MiB at 4K) buffer per
  client. Bounded to roughly one reply at a time; acceptable and matches
  Xorg's behaviour of delivering large replies.
- **Synchronous full-screen stall** — a full-screen Part 2 readback is a
  full-frame device→host copy plus fence wait, stalling the
  single-threaded core loop for a few ms during the grab. Acceptable for
  an on-demand, infrequent op; region grabs are tiny.
- **Pixel-format assumption (Part 2)** — assumes scanout is BGRX-order
  matching window storage / depth-24 root visual. Called out so a future
  scanout-format change doesn't silently corrupt screenshots.
- **Multi-head** — a rect spanning outputs is out of scope; single-output
  behaviour is correct. Follow-up adds stitching across `scanout_pools`.
