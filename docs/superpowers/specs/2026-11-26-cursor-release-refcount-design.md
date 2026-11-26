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

`collect_released_cursors` (`portable/cursor.rs:463`) reclaims cursors that clients freed but that something may
still use: a window's cursor attribute, a frame of an animated cursor, the grab override, the default cursor, the
active cursor or the displayed cursor.
- **Cost of one call.** It loops until nothing more is freed. In each pass it takes every released cursor and asks
  `cursor_referenced` about it. `cursor_referenced` scans every window and every frame of every animated cursor.
  One call costs O(R × (W + F)) per pass, where R is released cursors still referenced, W is windows and F is
  animation frames.
- **How often it runs.** It runs after every `FreeCursor`, every effective-cursor refresh, every grab-cursor change,
  every window-cursor change, every `ReplaceCursor` and every window destruction with a cursor (`cursor.rs:405, 996,
  1033, 1049, 1145`, `windows.rs:1990`). In a match the game changes its cursor about 60 times a second
  (`ChangeWindowAttributes`, the longest request at 1.6–2.5 ms), and Proton re-grabs the pointer about 60 times a
  second.
- **Why it gets worse over time.** This is plausible but not measured: the logs do not track R or F over time,
  and reclamation can shrink both. R and F grow whenever cursors stay referenced: browsers and themed or animated cursors free cursors that
  windows still reference. That matches the slowdown over session time, here and in #115.

Xorg keeps a reference count on each cursor (`dix/cursor.c:105-150`, `RefCursor`/`FreeCursor`):
- a window's cursor attribute takes a reference and drops it on change or destruction (`dix/window.c:968, 1536,
  1559`);
- an animated cursor takes a reference on each frame (`render/animcur.c:247, 360`).

A cursor is destroyed when its count reaches zero and its XID is gone. Each change costs O(1).

## Goals

- **O(1) per change.** Every cursor-reference change costs O(1). Reclamation work is proportional to the cursors
  that actually change state.
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

  `released_cursors` stays, and still means "the client XID is gone".
- **Window slots go through two helpers.** Every insert, replace or remove of a `self.windows` entry goes through
  `window_geom_insert` / `window_geom_remove`, and every write of an entry's `cursor` field goes through
  `window_set_cursor`. They keep `cursor_refs` in step, and a drop to zero pushes the XID onto `reclaim_pending`.
  - The rule is grep-checkable: no direct `self.windows.insert/remove` and no direct `.cursor =` outside the
    helpers.
  - It covers the window-cursor request (`cursor.rs:1027`), `ReplaceCursor` (`cursor.rs:1133-1136`), window
    destruction (`windows.rs:1983-1990`), window creation (`windows.rs:1708`), and the Composite overlay paths:
    teardown in `finish_cow_release` (`overlay.rs:19`) and the projection rebuilt on deferred reacquisition
    (`overlay.rs:94-110`).
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
- **Telemetry.** A counter of reclamation candidates examined per second, `cursor_reclaim_checks`, joins the
  existing cursor telemetry. The scaling test reads it; production code has no `cfg(test)`.

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

  Mutations: skip `window_geom_remove` in overlay teardown; drop the frame cascade; reclaim inside
  `cursor_ref_drop` instead of at the reclamation point (this must fail an ordering assertion, namely that
  `CursorNotify` is queued before the old sprite is decref'd).
- **Scaling guard.** With 2,000 released cursors still referenced by windows and 500 animation frames, 1,000
  window-cursor changes raise `cursor_reclaim_checks` by at most a small constant per change, for example ≤ 4.
  Mutation: restore a scan of `released_cursors` inside `collect_released_cursors`.
- **Existing tests.** The cursor tests (`tests/cursor.rs`, the animated-cursor and sprite tests) pass unchanged.
- **Hardware acceptance.** A local client creates animated cursors and window cursors that it then frees, and changes
  a window cursor 60 times a second. Compare the `ChangeWindowAttributes` time in the loop telemetry against
  master. Also run CS2 in a session aged by browser use, master against the branch, with MangoHud. A build of the
  branch goes to the reporter for a before/after run of the same match.
- **Gate.** The workspace suite, the lavapipe `--ignored` suite, clippy for Linux and FreeBSD, and
  `tools/check-layout.sh`.

## Provenance

The behaviour reference is Xorg 21.1.24 (MIT): `dix/cursor.c`, `dix/window.c` and `render/animcur.c`, read for
behaviour only. The evidence is the reporter's profile, MangoHud logs and telemetry in
`/data/Projects/claude-scratch/issue-387/` (`dl3/`, `analysis/notes.md`).
