# Released-cursor reclamation by reference count (#387)

## Intent

Issue #387: Marvel Rivals runs at parity with Xorg in its menu. During a match it drops to 100–170 fps, against
~190 on Xorg, and it gets worse the longer the session lasts. During the match the GPU sits at 54–65%, against 86%
on Xorg, so the frame rate is limited on the server side.

The reporter's CPU profile of yserver during a match (`perf`, 21K samples) settles where the time goes:
- `XBackend::collect_released_cursors` accounts for **31.6% of sampled cycles**;
- one branch alone, `cursor_referenced` scanning animation frames, is 10.9%, and other animation-scan branches sit
  under the same function;
- the next entry is hashing, at 2–3%, and nothing else reaches 1.1%.

A second test disabled Proton's pointer grab (`GrabPointer=N`). The `GrabPointer` traffic disappeared, but the
match frame rate stayed low. The grab is one trigger among several; it is not the cause.

## The cause

`collect_released_cursors` (`portable/cursor.rs:463`) reclaims backend cursors that the core has *released*.
The core calls `backend.free_cursor` only once no cursor XID and no core window names the host cursor
(`cursor_host_referenced`, `resources/picture_font_cursor.rs:257`). What can still hold a released backend cursor:
- a frame of a live animated cursor. Xcursor and toolkits free each frame's XID right after
  `RenderCreateAnimCursor`;
- the grab override, the default, active or displayed cursor;
- a backend window projection that outlives its core window, for example the Composite overlay kept behind direct
  scanout.
- **Cost of one call.** It loops until nothing more is freed. In each pass it takes every released cursor and asks
  `cursor_referenced` about it. `cursor_referenced` scans every window and every frame of every animated cursor.
  One call costs O(R × (W + F)) per pass. R is released cursors still referenced, which in practice means the
  frames of live animated cursors. W is windows and F is animation frames.
- **How often it runs.** It runs after every `FreeCursor`, every effective-cursor refresh, every grab-cursor change,
  every window-cursor change, every `ReplaceCursor` and every window destruction with a cursor (`cursor.rs:405, 996,
  1033, 1049, 1145`, `windows.rs:1990`).
  - In a match the game sends about 60 `ChangeWindowAttributes` a second, and it is the longest request at
    1.6–2.5 ms. The opcode telemetry does not record the attribute mask, so how many of those change the cursor is
    not known. The refresh path also returns early when the effective cursor does not change.
  - Proton re-grabs the pointer about 60 times a second, and every grab calls the sweep.
- **Why it gets worse over time.** This is plausible but not measured: the logs track neither R nor F over time,
  and reclamation can shrink both. Both grow with the animated cursors alive in the session: themed progress and
  busy cursors from browsers and desktops, plus the game's own. That would match the slowdown over session time,
  here and in #115.

Xorg keeps a reference count on each cursor (`dix/cursor.c:105-150`, `RefCursor`/`FreeCursor`):
- a window's cursor attribute takes a reference and drops it on change or destruction (`dix/window.c:968, 1536,
  1559`);
- an animated cursor takes a reference on each frame (`render/animcur.c:247, 360`).

A cursor is destroyed when its count reaches zero and its XID is gone. The counter update costs O(1). The
zero-count path does screen cleanup and can release animation frames.

## Goals

- **Amortized O(1) backend accounting.** Every backend cursor-reference update costs O(1). Reclamation work is
  proportional to the cursors that actually change state.
  - This does not cover the request paths' other scans: `ReplaceCursor`'s window traversal and the core's
    `cursor_host_referenced` release check. They run per `FreeCursor` or `ReplaceCursor`, not per cursor change or
    grab, and stay as they are.
- **Same reclamation points.** Reclamation happens only at the points where the sweep runs today, and in the same
  order relative to the effective-cursor refresh, animation sync and `CursorNotify` queueing. A cursor whose last
  reference disappears somewhere the sweep does not run today (for example Composite overlay teardown) is reclaimed
  at the next of those points, as today.
- **No leaks, no early frees.** After every reclamation point:
  - no released cursor is left unreferenced;
  - no referenced cursor has lost its record, its sprite or its frames.
- **Nothing a client sees changes.** Backend cursor records are internal; the core already freed the XID.

## Non-goals

- Other costs in the profile: hashing, request handling and the per-`DamageNotify` flush (#388).
- Any change to how cursors are displayed or chosen.
- Any change to the core's cursor resource lifetime. The XID side (`FreeCursor` request, resource database) stays
  as it is.
- Matching the sweep's incidental timing beyond the points above. Today a cursor can also be reclaimed by a sweep
  that an unrelated request triggers. With pending candidates, that is the next reclamation point, which is the
  same point.

## Approaches

1. **Reference count with pending candidates (chosen).** Keep one count per cursor XID for the references held by
   window cursor slots and by animation frames. A drop to zero, or a release, adds the XID to a pending set. The
   existing reclamation points process only the pending set.
   - The scalar slots (`effective_cursor_xid`, `default_cursor_xid`, `core.active_cursor`, `grab_cursor_override`)
     are few and are read directly.
   - This is Xorg's model (`RefCursor`/`FreeCursor`), kept at yserver's existing reclamation points.
2. **Index only, sweep kept.** The counts make `cursor_referenced` O(1), but the loop over every released cursor
   stays: O(R) per call, and R grows with the session. Rejected.
3. **Throttle the sweep** (once per dispatch iteration). It is still O(R × (W + F)), only less often, and it moves
   reclamation points. Rejected.

## Design

- **State.** In `XBackend`:
  - `cursor_refs: HashMap<u32, u32>`: window cursor slots plus animation frames that name each cursor XID;
  - `reclaim_pending: Vec<u32>`: candidates to check at the next reclamation point.

  `released_cursors` stays, and means "the core called `backend.free_cursor`": no cursor XID and no core window
  names the host cursor any more.
- **Window slots go through two helpers.** Every insert, replace or remove of a `self.windows` entry goes through
  `window_geom_insert` / `window_geom_remove`, and every write of an entry's `cursor` field goes through
  `window_set_cursor`. They keep `cursor_refs` in step, and a drop to zero pushes the XID onto `reclaim_pending`.
  - The rule is grep-checkable: no direct `self.windows.insert/remove` and no direct `.cursor =` outside the
    helpers.
  - It covers the window-cursor request (`cursor.rs:1027`), `ReplaceCursor` (`cursor.rs:1133-1136`), window
    destruction (`windows.rs:1983-1990`), window creation (`windows.rs:1708`), and the Composite overlay paths:
    teardown in `finish_cow_release` (`overlay.rs:19`), the projection rebuilt on deferred reacquisition
    (`overlay.rs:94-110`), and the fresh overlay insertion (`overlay.rs:193`).
  - The InputOnly stand-in projection (`pointer.rs:100-115`) copies cursor state only to select a cursor, and it is
    not part of today's reference predicate. It stays excluded from `cursor_refs`, so a cached stand-in can never
    extend a cursor's lifetime.
- **Animation frames.** Inserting an `anim_cursor_records` entry (`cursor.rs:977`) adds one reference per frame
  source. Removing it, inside reclamation, drops them, and that can push more candidates.
- **Scalar slots.**
  - When the effective cursor, the grab override or the active cursor moves away from X, X is pushed onto
    `reclaim_pending`.
  - `FreeCursor` (`backend_cursor_free_cursor`) inserts into `released_cursors` and pushes X.
  - `init_cursor_sprite` (`cursor.rs:17-45`) sets `default_cursor_xid` and resets `effective_cursor_xid` once, at
    initialisation. Nothing can be released then, so it pushes nothing.
- **Reclamation points.** `collect_released_cursors` keeps its name and every call site (`cursor.rs:405, 996, 1033,
  1049, 1145`, `windows.rs:1990`), so its position relative to the refresh, animation sync and `CursorNotify` does
  not change. Its body becomes:
  1. drain `reclaim_pending`;
  2. for each candidate that is released, has a count of zero and is held by no scalar slot, remove its records and
     drop its sprite exactly as today;
  3. dropping an animated cursor's frame references pushes new candidates, so repeat until `reclaim_pending` is
     empty.
- **`cursor_referenced(xid)`** becomes the scalar comparisons plus `cursor_refs[xid] > 0`.
- **Check counter.** All reclamation eligibility goes through one predicate, `cursor_reclaimable(xid)`, which
  increments a cumulative `cursor_reclaim_checks: u64` on `XBackend`. It is cumulative, not a per-second bucket. Any
  scan over `released_cursors`, rejected visits included, has to pass through it. The scaling test reads the delta;
  production code has no `cfg(test)`. The existing cursor telemetry can report its rate.
- **Single decref site.** A cursor's records and sprite are removed only inside `collect_released_cursors`. This is
  grep-checkable: `cursor_records.remove`, `cursor_pixmaps.remove` and the sprite `store_decref_with_invalidate`
  for cursors appear nowhere else. That alone keeps reclamation at the existing points.

## Tests

- **Invariant after every reclamation point (the key test).** Run seeded random sequences through the backend's
  public entry points:
  - create static and animated cursors;
  - set and clear window cursors;
  - `FreeCursor` and `ReplaceCursor`;
  - destroy windows;
  - grab cursor on and off;
  - pointer moves that change the effective cursor;
  - Composite overlay get, release, deferred release and reacquisition.

  After every step, a test-side full recount over windows, frames and scalar slots computes:
  - **referenced** — every cursor that something still names;
  - **reclaimable** — released and not referenced.

  The test asserts:
  - `cursor_refs` equals the recount;
  - every referenced cursor still has its `cursor_records` entry, its sprite in the store, and its animation frames
    (no early free);
  - right after any call that reaches a reclamation point, no reclaimable cursor is left (no leak);
  - a cursor orphaned by overlay teardown is gone after the next reclamation point.

  The fixture registers real cursor sprites, so sprite decrefs are observed.

  Overlay: the test reaches the deferred-release state, so that a released cursor is held only by the overlay's
  backend projection, and runs teardown. Right after teardown, before any call that reaches a reclamation point,
  the cursor's record and sprite still exist. They are gone after the next reclamation point.

  Mutations:
  - skip `window_geom_remove` in overlay teardown (the leak assertion fails);
  - drop the frame cascade (the leak assertion fails);
  - reclaim inside `cursor_ref_drop` instead of at the reclamation point (the overlay survives-teardown assertion
    fails).
- **Scaling guard.** The backlog is built the way it happens in practice: 100 live animated cursors of 20 frames
  each, with every frame's XID freed, which gives 2,000 released frames held only by their animations. Then 1,000
  window-cursor changes and 1,000 grab-cursor toggles run. The cumulative `cursor_reclaim_checks` delta across that
  workload is at most 4 per operation. Mutation: restore a scan of `released_cursors` inside
  `collect_released_cursors`; its rejected visits go through `cursor_reclaimable`, so the delta explodes.
- **Existing tests.** The cursor tests (`tests/cursor.rs`, the animated-cursor and sprite tests) pass unchanged.
- **Hardware acceptance.** A local client builds the same backlog (animated cursors whose frame XIDs it frees),
  then changes a window cursor and re-grabs the pointer 60 times a second. Compare the `ChangeWindowAttributes` time in the loop telemetry against
  master. Also run CS2 in a session aged by browser use, master against the branch, with MangoHud. A build of the
  branch goes to the reporter for a before/after run of the same match.
- **Gate.** The workspace suite, the lavapipe `--ignored` suite, clippy for Linux and FreeBSD, and
  `tools/check-layout.sh`.

## Provenance

The behaviour reference is Xorg 21.1.24 (MIT): `dix/cursor.c`, `dix/window.c` and `render/animcur.c`, read for
behaviour only. The evidence is the reporter's profile, MangoHud logs and telemetry in
`/data/Projects/claude-scratch/issue-387/` (`dl3/`, `analysis/notes.md`).
