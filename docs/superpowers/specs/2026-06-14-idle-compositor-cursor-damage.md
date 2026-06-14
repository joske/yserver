# Idle compositor — stop the stationary-cursor redraw loop + idle on master-drop

**Date:** 2026-06-14
**Status:** draft (rev 4 — folded in codex review round 3: explicit gating algorithm, footprint lifecycle/teardown reset, failed-submit re-poke, vacuous-clear)
**Branch:** `fix/idle-compositor` (new, off `master`)
**Reference:** `crates/yserver/src/kms/v2/scene.rs` (`tick_one_output`, `build_scene`, `derive_cursor_transition`), `crates/yserver/src/kms/v2/backend.rs` (`next_wakeup`, `maybe_composite`). Related: [[project_cc_shake_render_storm]] (the long-unsolved buffer-age render-storm — this is very likely a piece of it).

## Problem

An **idle** desktop never stops compositing. Live evidence (idle e16, debug build, no animated client, no input, 2560×1440@60): from the first second and continuously, **~43 full composes/s, ~8000 draws/s, ~78 atomic page-flips/s**. An idle desktop must compose **0/s**.

Root cause (confirmed independently by codex, gpt-5.4-mini, 2026-06-14): `build_scene` adds the cursor rect to `projected_damage` **on every frame the cursor is visible, whether or not it moved**:

- the *previous* SW rect, unconditionally when `cursor_prev_pos` is `Some` (`scene.rs:1886`);
- the *current* rect, unconditionally in **both** the HW branch (`scene.rs:1905`) and the SW branch (`scene.rs:1925`).

`tick_one_output` unions `projected_damage` into `output_damage` (`scene.rs:1376`) *before* the empty-damage fast-path skip (`scene.rs:1385`), so the "nothing changed → skip compose" path is **unreachable whenever a cursor is on-screen**. `projected_damage` is recomputed fresh every `build_scene` (never subtracted on retire), so it re-seeds the same damage every frame → compose+flip every vblank, forever.

Two consequences:
1. **Overnight freeze / `DEVICE_LOST`** (the reported blocker): continuous full-screen compositing all night is a credible GPU-hammer that, on RADV/amdgpu, eventually tripped a GPU reset (`VK_ERROR_DEVICE_LOST`, "context is innocent"); the steadily-climbing client PutImage latency fits a saturated core loop. Not *proven* to cause the reset, but a strong trigger/amplifier — and after a reset it stays hot instead of settling.
2. **No true idle** even with the screen on. Turning a monitor to standby sends yserver *no* event (DRM master not dropped, no client DPMS), so it keeps hammering — only a true idle makes screen-off benign.

Separately, when **DRM master is dropped** (VT switched away) or **outputs are DPMS-off**, `maybe_composite` already short-circuits the KMS submit — but `next_wakeup()` still returns `Some(now)` whenever `scene_structure_dirty` (`backend.rs:8881`), so the core loop **hot-spins** without doing useful work instead of sleeping until the seat/VT is regained.

## Goal

An idle desktop (no client repaint, no cursor movement, no sprite change, no input) composes **0 frames/s**. Cursor motion, sprite changes, and mode transitions still repaint correctly with no visible artifacts (no trails, no missing/stale cursor) on both the HW-plane and SW-in-BO paths. When DRM master is dropped or outputs are DPMS-off, the core loop sleeps (no busy-wake) until a real event (seat regain, input, client) arrives, with no loss of protocol/scene state.

## Non-goals

- **Re-enabling the buffer-age `Clipped` path.** The `pick_repaint_region` always-`Repaint::Full` stopgap stays. This spec is correct *under* always-Full and adds an explicit follow-up note for whoever re-enables Clipped (see Risks).
- **Fixing the post-power-cycle KMS/scene desync (codex RCA #3).** The DPMS-wake path does a shallower reinit than VT suspend/resume (it re-arms the cursor + marks damage but does not drain pending acks / reset scanout BO state / re-query outputs). That desync is a separate, secondary issue; this spec does not touch it. (Fixing the idle loop removes the GPU-hammer that is the actual overnight blocker; the desync only manifested because the VT switch was the recovery path.)
- **udev/hotplug monitoring.** Out of scope; the monitor-standby case is handled by "true idle", not by an event.
- **Cursor animation.** Animated cursors legitimately advance frames via `cursor_anim_deadline`; unaffected.

## Design

### Invariant

`output_damage` must represent the **actual pixel delta** between the last-committed scanout BO and the next frame. The cursor contributes to it **only** when the on-screen cursor pixels will actually change. `cursor_prev_pos` is a transactional *trail-clear anchor* (advanced only on successful retire), **not** a perpetual dirty source.

### Part A — cursor damage becomes change-gated (not presence-gated)

Today `build_scene` adds the cursor rect to `projected_damage` on every visible frame: the prev-rect (`scene.rs:1886`) whenever `cursor_prev_pos` is `Some`, and the current-rect unconditionally in **both** the HW branch (`scene.rs:1905`) and SW branch (`scene.rs:1925`). The fix makes the cursor's damage contribution the **delta of its scanout footprint**, so a cursor that has not changed contributes **zero** damage.

**The footprint model (resolves the HW-hide / HW-anchor problem).** Track a new per-output `last_present_cursor_rect: Option<Rect2D>` — the clipped cursor footprint of the last *presented* frame, advanced **transactionally on retire** (same rule as `cursor_prev_pos`, queued on the `PendingAck` and applied in `handle_page_flip_complete`), but for **all** modes including HW. Each frame compute `new_cursor_rect` = the clipped current footprint (`∅` if Hidden/off-output). The cursor's damage contribution is:

```
cursor_damage = last_present_cursor_rect ∪ new_cursor_rect      (either may be ∅)
```

**Exact gating algorithm (do not paraphrase as "always add the union" — that reintroduces the loop):**

```
cursor_damage_candidate = last_present_cursor_rect ∪ new_cursor_rect   // either may be ∅
cursor_changed =
       (new_cursor_rect != last_present_cursor_rect)     // footprint moved / appeared / disappeared
    || cursor_transition_to_queue.is_some()              // Hidden↔Sw↔Hw this frame
    || (record_version != last_present_cursor_version)   // sprite bitmap swapped
if cursor_changed { output_damage ∪= cursor_damage_candidate } else { /* add nothing */ }
```

**Hard rule:** when the cursor is stationary, same mode, and same sprite (`cursor_changed == false`), the cursor contributes **`∅`**, never its unchanged rect. The candidate union is the *damage to emit when something changed*, not an unconditional per-frame contribution. The current code's unconditional adds at `scene.rs:1886/1905/1925` are exactly what this replaces — all three move behind `cursor_changed`.

This yields the right behavior in every case, with no special-casing:
- **stationary, same mode, same sprite** → `cursor_changed == false` → `∅` → empty-damage fast-path fires → **idle**;
- **moved (SW)** → `old ∪ new` (old trail cleared + new painted);
- **Hidden→Hw / Hidden→Sw** (show) → `∅ ∪ new`, non-empty, carries `ShowOnRetire`;
- **Hw→Hidden / Sw→Hidden** (hide) → `old ∪ ∅`, non-empty poke that carries `HideOnRetire`. This is the case rev 2 missed: the HW current-rect lives only in the *visible* branch, so a `Hw→Hidden` frame had no rect and stranded the queued `HideOnRetire` (`derive_cursor_transition` `(Hw, Hidden)`, `scene.rs:1053`); the retained `last_present_cursor_rect` supplies the poke.

**Transactional advance / failed submit.** `last_present_cursor_rect` (and `last_present_cursor_version`) advance **only on successful retire**, queued on the `PendingAck` and applied in `handle_page_flip_complete` (`scene.rs:964`) — identical to `cursor_prev_pos` today. A **failed submit means no retire, so the footprint is unchanged and the next tick re-pokes** the same transition rather than losing it.

**Lifecycle / teardown.** `last_present_cursor_rect` and `last_present_cursor_version` are per-output state and must be reset everywhere `cursor_prev_pos` / `last_frame_cursor_mode` are reset: `drain_all()` (`scene.rs:793`, suspend/VT-release/device-loss teardown) clears them to `None`, and any output add/remove/reindex path resets the per-output entry. Otherwise a stale footprint can survive suspend/resume or output churn and mis-poke (or fail to poke) the first frame back. After a reset, `first_frame` already forces a full compose, so a `None` footprint is correct (show path = `∅ ∪ new`).

**`moved` is SW-only by construction.** HW pointer motion is serviced directly by `cursor_plane_move` in the pointer fast-path (`backend.rs:5185`) and never enters `scene.tick`, so a pure-HW move neither needs nor triggers a compose — correct, the plane moves independently and the BO never contains the cursor. `last_present_cursor_rect` for a pure-HW cursor therefore only updates on composes (it may lag the plane), which is harmless: it is used only to produce a *non-empty poke* for the hide transition, not to position any BO pixels. SW motion does enter `scene.tick`, updates the footprint each presented frame, and gets the `old ∪ new` trail-clear.

**Placement.** Compute the footprint delta + `cursor_changed` in `tick_one_output` next to the existing `derive_cursor_transition` call (`scene.rs:1373`), which already has `prev_mode`, `cursor_prev_pos_before`, and the queued transition in scope. Have `build_scene` return `new_cursor_rect` (it already computes the clipped rect) and let `tick_one_output` decide the union and whether to fold it into `output_damage`, then queue `last_present_cursor_rect`-advance on the `PendingAck`. Keeps the gating decision co-located with the transactional state, per codex RCA #4. (`cursor_prev_pos` may be subsumed by `last_present_cursor_rect`; decide during implementation — but `derive_cursor_transition`'s existing use of `cursor_prev_pos` for the SW trail must keep working.)

**Sprite-version term.** `register_cursor` already sets `scene_structure_dirty` on a sprite change (`scene.rs:573`) and `display_cursor_by_handle` (`backend.rs:1361`) feeds the scene/plane path, so `record_version` supplies the *damage*, not the *wake*. Load-bearing for **SW** sprite swaps (new bitmap must re-compose into the BO); defensive for HW (plane-upload handles the bitmap; a one-frame BO compose is harmless). Confirm during implementation it isn't double-counting an already-marked damage rect.

### Part B — clear `scene_structure_dirty` on an all-empty tick

`tick()` clears `scene_structure_dirty` only when `composed > 0` (`scene.rs:887`). So any caller that sets the dirty **bool** without producing a damage **rect** — e.g. `register_cursor` (`scene.rs:573`), or a bare `wake_for_damage` — leaves the loop hot: tick runs, finds empty damage, skips, dirty stays set, `next_wakeup` returns `Some(now)`, repeat. After Part A removes the cursor as a perpetual damage source, this becomes the dominant idle leak.

`scene_structure_dirty` is a coarse "something might have changed, go look" flag. Once a tick has *looked* (run `build_scene`) and found nothing to draw, the flag must clear. **Classify each output's tick outcome:**
- **Composed** → drew this output (already clears today).
- **SkippedEmpty** (`TickSkipReason::EmptyDamage`) → "checked, nothing to do" → safe to clear.
- **SkippedDefer** (`NoBO` / `PendingAcks` / `RetryDeadline`) → "deferred, retry later" → keep dirty; the existing retry-deadline / page-flip-retire path re-drives it.
- **Errored** (`tick_one_output` returned `Err`, which `tick()` logs and continues, `scene.rs:879`) → real damage may be un-rendered → keep dirty.

**Change:** clear `scene_structure_dirty` **only when every output was Composed or SkippedEmpty** — i.e. keep it set if *any* output was SkippedDefer **or** Errored. (A zero-output / no-outputs tick clears vacuously — there is nothing to defer.) (Today `tick()` clears on `composed > 0`, which both over-clears — a defer/error on another output is lost — and under-clears — an all-SkippedEmpty tick never clears. The new rule fixes both.) This is the general true-idle guarantee: it fixes the cursor case *and* any other bare-dirty source, without dropping damage on a transient submit/platform error.

**Why late-arriving damage is safe.** The compositor is single-threaded, so no damage can land *during* a tick. Damage that arrives between a submit and its retire is explicitly preserved by `handle_page_flip_complete` (it subtracts only the *submitted* snapshot, `scene.rs:920`), and that retire path re-sets the dirty flag, so clearing it here on an `EmptyDamage`/`Composed` tick cannot strand damage that has not yet been looked at.

### Part C — `next_wakeup` suppresses only KMS-output timers when scanout is disallowed

In `next_wakeup` (`backend.rs:8879`), when `!self.scanout_allowed()` (DRM master dropped) **or** `!self.kms_outputs_active` (DPMS-off), suppress the **`scene_deadline`** and **`cursor_anim` deadline** — these are KMS-output-bound and can't render. **Keep the `present_deadline`.** `enqueue_present_completion` can register `PresentBatchWait::Poll` batches (`backend.rs:14791`) that have **no real fd wake**; dropping their poll deadline would strand a pending client present-completion until unrelated activity happens. (If those polled batches are ever given a real wake source, the poll deadline can also be parked — out of scope here.)

With the scene/cursor-anim timers suppressed, the loop sleeps on its remaining timers + **fd** events (signalfd VT-acquire, libinput, client sockets), which are unaffected:
- VT acquire → signalfd wakes the loop → `run_resume` → `wake_for_damage` + `scanout_allowed()` true → next `next_wakeup` returns `Some(now)` → composes. No deadlock.
- Clients keep being serviced (separate epoll sources); they render to pixmaps, just nothing scans out.

This is the scheduling-layer half of codex RCA #5; the existing `maybe_composite` gates are the submit-layer half. Both are needed — the gates alone leave the hot wakeup loop.

> Note: Part C does **not** help the physical-monitor-standby case (there `scanout_allowed` and `kms_outputs_active` are both still true). That case is covered entirely by Parts A+B's true-idle.

## Data flow (idle, screen on, HW cursor, after fix)

```
no input, no client paint, cursor stationary
  → core loop wakes only on its existing cadence
  → maybe_composite: scene_structure_dirty == false  → can_submit_scene false → no tick
     (and even if a stale dirty bit were set, tick_one_output: output_damage empty → EmptyDamage skip)
  → next_wakeup: scene not dirty, present_deadline kept (None if no Poll batch), no cursor anim → sleeps
  → 0 composes/s, 0 flips/s   ✅
```

## Testing

- **Failing test first** ([[feedback_write_failing_test_first]]): a `tick_one_output`/`build_scene`-level unit test — steady-state HW cursor (prev_mode `Hw`, assignment `Hw`, same pos) → `output_damage` is **empty** → tick returns `Ok(false)` (skip). Must be **red** against current code (today it returns a non-empty cursor rect and composes), green after Part A.
- SW analogue: stationary SW cursor (same pos, same version) → empty damage / skip; moved SW cursor → damage covers **both** old and new rects (no trail).
- **Transition coverage (the cases that break if Part A over-deletes), at `tick_one_output`/`build_scene` level — not just the `derive_cursor_transition` matrix:** `Hidden→Hw` and **pure `Hw→Hidden` with no SW trail and no other damage** must each produce non-empty `output_damage` on the transition frame (so the `ShowOnRetire`/`HideOnRetire` rides a compose and retires via the footprint poke) and zero on the following stationary frame. Likewise `Hidden→Sw`, `Sw→Hw`, `Hw→Sw`.
- **Sprite swap:** stationary SW cursor whose `record_version` changes → produces damage that frame (new bitmap re-composed), then idles.
- **Part B (dirty-but-empty no-spin):** set `scene_structure_dirty` via a bare wake / `register_cursor` with no damage rect; one tick → `EmptyDamage` skip → `scene_structure_dirty` is now **false** and `next_wakeup()` returns `None` (no spin). A `NoBO`/`PendingAcks` defer skip must **leave** the flag set. **And** an errored tick (`tick_one_output` → `Err` on an output) must **leave** the flag set (no damage dropped).
- **Part C unit tests:** `next_wakeup` suppresses the scene/cursor-anim deadlines when `scanout_allowed()` is false (master dropped) even with `scene_structure_dirty == true`; restores them once allowed. **And** a `PresentBatchWait::Poll` batch enqueued while `!kms_outputs_active` still yields a non-`None` present-poll deadline (not stranded).
- **HW smoke (the real gate):** `just yserver-…-hw` with `YSERVER_LOOP_TELEMETRY=1` — idle desktop must show **0 composes/s** in the per-second log (today: ~43/s). Move the mouse → composes track motion then return to 0 at rest. Cursor sprite changes (resize-handle, text-caret) show correctly. No cursor trail, no stale/missing cursor. VT switch away → telemetry quiet (0 wake-spin), switch back → restores and idles. ([[feedback_vng_pass_not_hw_pass]] — HW-gated.) **If idle is still > 0/s after the fix, there is another per-frame damage source — hunt it, don't declare victory.**
- Existing scene/cursor unit tests + rendercheck must stay green ([[reference_rendercheck_v1_baseline]]).

## Risks

- **Re-enabling the buffer-age `Clipped` path later — hard requirement, not just a warning.** Under always-Full, a compose triggered by *other* damage repaints the whole BO including the SW cursor draw, so gating cursor damage out of `output_damage` cannot drop the cursor. Under `Clipped`, the repaint *region* might not cover a stationary SW cursor on a freshly-cycled (older-age) BO → missing/stale cursor. The cursor rect plays two roles — (1) gate whether to compose, (2) be inside the repaint region when we *do* compose. This spec removes role (1) for stationary cursors; role (2) stays implicit-via-always-Full. **Requirement on whoever re-enables `Clipped`:** the *current* SW cursor rect must be folded into the repaint region even when it did not itself trigger the frame. Encode this as (a) an explicit comment at the gating site in `tick_one_output`, and (b) a test that fails if a `Clipped` repaint region omits a stationary SW cursor — added now (ignored/marked while Clipped is disabled) so it can't be silently lost.
- **Sprite/visibility change missed → stale cursor.** Covered by the sprite-swap + transition tests; during implementation confirm which adjacent paths (`register_cursor` `scene.rs:573`, `display_cursor_by_handle` `backend.rs:1361`, cross-output) already dirty/damage so `cursor_changed` adds only load-bearing terms and doesn't double-count.
- **Part C starving a needed wake.** Only scene + cursor-anim timers are suppressed; present-poll and all fd event sources remain (the `PresentBatchWait::Poll`-stranding hazard codex flagged). Confirm via the VT-switch smoke that acquire still wakes and composes (no black-screen-until-input).
