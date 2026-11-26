# Released-cursor reclamation by reference count (#387)

## Intent

Issue #387: Marvel Rivals runs at parity with Xorg in its menu. During a match it drops to 100–170 fps, against
~190 on Xorg, and it gets worse the longer the session lasts. During the match the GPU sits at 54–65%, against 86%
on Xorg, so the frame rate is limited on the server side.

The reporter's CPU profile of yserver during a match (`perf`, 21K samples) settles where the time goes:
- `XBackend::collect_released_cursors` accounts for **31.6% of all yserver CPU**;
- 10.9% of that is inside `cursor_referenced`, scanning animation frames;
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
- **Why it gets worse over time.** R and F only grow: browsers and themed or animated cursors free cursors that
  windows still reference. That matches the slowdown over session time, here and in #115.

Xorg keeps a reference count on each cursor (`dix/cursor.c:105-150`, `RefCursor`/`FreeCursor`):
- a window's cursor attribute takes a reference and drops it on change or destruction (`dix/window.c:968, 1536,
  1559`);
- an animated cursor takes a reference on each frame (`render/animcur.c:247, 360`).

A cursor is destroyed when its count reaches zero and its XID is gone. Each change costs O(1).

## Goals

- Every cursor-reference change costs O(1), apart from a cascade of cursors that really are freed by it.
- A cursor is destroyed at exactly the moment today's sweep would destroy it: same `cursor_records`,
  `cursor_pixmaps` and `anim_cursor_records` contents after every operation, and same sprite decrefs. Nothing a
  client sees changes.

## Non-goals

- Other costs in the profile: hashing, request handling and the per-`DamageNotify` flush (#388).
- Any change to how cursors are displayed or chosen.
- Any change to the core's cursor resource lifetime. The XID side (`FreeCursor` request, resource database) stays
  as it is.

## Approaches

1. **Full event-driven reference count (chosen).** Keep one count per cursor XID of the references held by window
   cursor slots and by animation frames.
   - The scalar slots (`effective_cursor_xid`, `default_cursor_xid`, `core.active_cursor`, `grab_cursor_override`)
     are few and are read directly, as today.
   - Whenever a reference to X is dropped, or X is released, X is reclaimed if it is released, its count is zero
     and no scalar slot holds it.
   - Reclaiming an animated cursor drops its frames' references, which can cascade.

   This is Xorg's model. Cost is O(1) per change.
2. **Index only, sweep kept.** Keep the counts so that `cursor_referenced` is O(1), but keep the
   loop-until-stable sweep over every released cursor. That is O(R) per call: smaller, but R still grows with the
   session. It is a halfway step with no advantage over 1, so it is rejected.
3. **Throttle the sweep** (once per dispatch iteration). Still O(R × (W + F)), only less often, and it changes when
   cursors are destroyed relative to requests. Rejected.

## Design

- **State.** In `XBackend`: `cursor_refs: HashMap<u32, u32>`, the number of window cursor slots plus animation
  frames that name each cursor XID. `released_cursors` stays, and still means "the client XID is gone".
- **Writes go through helpers.** Every write of a window's `cursor` slot and every insert or remove of an
  `anim_cursor_records` entry goes through `cursor_ref_add(xid)` / `cursor_ref_drop(xid)`.
  - Known write sites: `backend_cursor_set_window_cursor` (`cursor.rs:1027`), `backend_cursor_replace_cursor`
    (`cursor.rs:1133-1136`), window destruction (`windows.rs:1983-1990`), window creation with `cursor: None`
    (`windows.rs:1708`, `overlay.rs:65`), animated-cursor creation (`cursor.rs:977`) and reclamation
    (`cursor.rs:476`).
  - The test below enforces that every site is found.
- **Reclaim one candidate.** `reclaim_if_unreferenced(xid)` replaces the sweep body for one candidate. It runs
  when:
  - X is released (`backend_cursor_free_cursor`);
  - `cursor_ref_drop` takes X's count to zero;
  - a scalar slot changes away from X: the effective cursor (`refresh_effective_cursor`), the grab override and
    the active cursor.
- **The cascade.** Reclaiming an animated cursor drops each frame's reference through `cursor_ref_drop`. That
  re-enters `reclaim_if_unreferenced` for frames that became free, which gives the same cascade the old loop
  reached.
- **`collect_released_cursors` goes away.** Its call sites call the targeted reclaim for the XIDs they changed. Any
  site that changes several slots at once (`ReplaceCursor`) reclaims each old XID.
- **`cursor_referenced(xid)`** becomes the scalar-slot comparisons plus `cursor_refs.get(&xid) > 0`.

## Tests

- **Equivalence with the old sweep (the key test).** Keep today's sweep and today's `cursor_referenced` as a
  test-only oracle in the backend's test module, with no production `cfg(test)`. Drive randomized but seeded
  sequences through the backend's public entry points, and after every step assert that `cursor_records`,
  `cursor_pixmaps`, `anim_cursor_records`, `released_cursors` and the sprite store refcounts equal what the oracle
  sweep produces on a cloned pre-state. The sequences cover:
  - create static and animated cursors;
  - set and clear window cursors;
  - `FreeCursor`;
  - `ReplaceCursor`;
  - destroy windows;
  - grab cursor on and off;
  - pointer moves that change the effective cursor.

  Also assert after every step that `cursor_refs` equals a full recount over windows and frames.

  Mutations: miss one write site (for example replace-cursor), and drop the cascade.
- **Scaling guard.** With 2,000 released cursors still referenced by windows and 500 animation frames, 1,000
  window-cursor changes must not visit the released set. Assert this through a visit counter exposed by the oracle
  harness, not by timing.
- **Existing tests.** The cursor tests (`tests/cursor.rs`, the animated-cursor and sprite tests) pass unchanged.
- **Hardware acceptance.** A local client that creates animated cursors and window cursors it frees, then changes
  a window cursor 60 times a second. Compare the `ChangeWindowAttributes` time in the loop telemetry against
  master. A build of the branch also goes to the reporter, who compares the in-match fps against master.
- **Gate.** The workspace suite, the lavapipe `--ignored` suite, clippy for Linux and FreeBSD, and
  `tools/check-layout.sh`.

## Provenance

The behaviour reference is Xorg 21.1.24 (MIT): `dix/cursor.c`, `dix/window.c` and `render/animcur.c`, read for
behaviour only. The evidence is the reporter's profile, MangoHud logs and telemetry in
`/data/Projects/claude-scratch/issue-387/` (`dl3/`, `analysis/notes.md`).
