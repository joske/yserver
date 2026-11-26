# Released-cursor reclamation by reference count (#387) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `collect_released_cursors` cost proportional to the cursors that change state, instead of O(released × (windows + frames)) per call.

**Architecture:** `XBackend` keeps a per-XID reference count for window cursor slots and animation frames (`cursor_refs`), plus a list of candidates (`reclaim_pending`). Task 1 adds the bookkeeping, routes every write through helpers, and makes `cursor_referenced` O(1); the old sweep loop stays, so behaviour is unchanged. Task 2 replaces the sweep body with a drain of the pending candidates at the same call sites, behind one counted eligibility predicate.

**Tech Stack:** Rust, `crates/yserver` KMS backend (`kms/render/backend/portable/`), unit tests on `KmsBackend::for_tests()`.

**Spec:** `/data/Projects/yserver-387/docs/superpowers/specs/2026-11-26-cursor-release-refcount-design.md` (approved 2026-11-26).

## Global Constraints

- Reclamation happens only at today's `collect_released_cursors` call sites (`portable/cursor.rs:405, 996, 1033, 1049, 1145`, `portable/windows.rs:1990`); none is added, removed or moved.
- `released_cursors` keeps its meaning: the core called `backend.free_cursor`.
- Nothing a client sees changes: no change to cursor selection, display, `CursorNotify`, or the core's XID lifetime.
- The InputOnly stand-in (`input_only_pointer_hosts`, `pointer.rs:100-115`) is NOT counted in `cursor_refs`.
- `default_cursor_xid` / `effective_cursor_xid` writes in `init_cursor_sprite` push nothing.
- The two non-reclamation sprite operations stay where they are: `swap_anim_frame_into_maps` (`cursor.rs:630`) and the upload-failure decref (`cursor.rs:171`).
- No `cfg(test)` in production code; no new `YSERVER_*` environment variable.
- Reuse first: new code only where the goal needs it; no new crate dependency (the tests use a local xorshift, not `rand`).
- Cite Xorg 21.1.24 (`dix/cursor.c`, `dix/window.c`, `render/animcur.c`) in comments where a rule comes from it; never write "as Xorg" for a yserver-only rule.
- Tests drive public backend entry points (`define_cursor`, `free_cursor`, `replace_cursor`, `set_grab_cursor`, `create_anim_cursor`, `destroy_subwindow`, overlay calls); new lifecycle tests never write `windows`, `cursor_refs` or scalar slots directly, except through the Task 1 helpers.
- Before every commit: `cargo +nightly fmt`, then `cargo clippy --all-targets -- -D warnings` (AGENTS.md). Every `cargo test … | tail` command runs under `set -o pipefail`.
- Commits are signed (`-S`) and end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>` and `Implemented-By: codex (gpt-6-luna xhigh)`.

## Review Focus

1. An animated cursor that names the same frame source twice (`[c1, c1, c2]`): one reference per frame, two drops at reclamation. Pinned in Task 1 (`duplicate_frame_sources_count_per_frame`).
2. Freeing an animated cursor whose frame XIDs were all freed earlier (what libXcursor does): one reclamation point removes the animation and all its frames. Pinned by the existing `animated_cursor_keeps_its_freed_frames_alive` and by the Task 2 cascade mutation. Animations as frames of animations are not tested: the protocol handler rejects them (`yserver-core/.../process_request/render.rs:1122`), as Xorg does (`render/animcur.c:314`).
3. The overlay projection rebuilt on deferred reacquisition overwrites an entry whose `cursor` is `Some`: the overwritten reference must be dropped. Pinned in Task 1 (`cow_reacquisition_drops_the_overwritten_cursor_reference`).
4. `ReplaceCursor` whose old cursor is also a frame of a live animation: window slots move, the frame reference stays (Xorg `ReplaceCursor` does not touch animation frames). Pinned in Task 1 (generator op + recount).
5. One cursor on many windows, then `FreeCursor`, then the windows lose it one by one: reclaimed exactly at the point where the last one goes. Pinned in Task 2 (`shared_cursor_goes_with_the_last_window`).

---

### Task 1: Reference counts and write helpers (behaviour unchanged)

**Files:**
- Create: `crates/yserver/src/kms/render/backend/portable/cursor_refs.rs` (helpers below; `mod cursor_refs;` in `portable/mod.rs`)
- Modify: `crates/yserver/src/kms/render/backend.rs` (fields on `XBackend` next to `released_cursors`, ~line 545; initialised in the constructor ~line 622)
- Modify: `portable/cursor.rs` (`cursor_referenced` :447, `refresh_effective_cursor` :389, anim insert :977, `backend_cursor_free_cursor` :987-998, define :1026-1031, grab :1047, replace :1133-1143)
- Modify: `portable/windows.rs` (insert :1691, destroy :1983)
- Modify: `portable/overlay.rs` (:19, :114, :193)
- Test: create `crates/yserver/src/kms/render/backend/tests/cursor_refcount.rs` (`mod cursor_refcount;` in `tests/mod.rs`)

**Interfaces:**
- Produces (all `pub(in crate::kms::render::backend)` on `impl<P: Platform> XBackend<P>`):
  - fields `cursor_refs: HashMap<u32, u32>` (an XID with count 0 has no entry), `reclaim_pending: Vec<u32>`;
  - `fn window_geom_insert(&mut self, host_xid: u32, geom: WindowGeometry) -> Option<WindowGeometry>` — inserts or replaces; adds a ref for `geom.cursor`, drops one for the replaced entry's cursor;
  - `fn window_geom_remove(&mut self, host_xid: u32) -> Option<WindowGeometry>` — drops the removed entry's cursor ref;
  - `fn window_set_cursor(&mut self, host_xid: u32, cursor: Option<u32>) -> bool` — false when the window is absent (no change);
  - `fn cursor_ref_add(&mut self, xid: u32)`;
  - `fn cursor_ref_drop(&mut self, xid: u32)` — on reaching 0, removes the entry and pushes `xid` onto `reclaim_pending`. It never reclaims.
  - `fn cursor_release_candidate(&mut self, xid: u32)` — pushes `xid` onto `reclaim_pending`.

- [ ] **Step 1: Write the failing tests** in `tests/cursor_refcount.rs`

  Test-side helpers (in the test file):
  - `fn recount(b: &KmsBackend) -> HashMap<u32, u32>` — for each `b.windows` entry with `cursor: Some(x)` add 1 to `x`; for each `anim_cursor_records` frame add 1 to `frame.source`. It never reads `input_only_pointer_hosts`.
  - `struct Model { sprites: HashMap<u32, DrawableId>, frames: HashMap<u32, Vec<u32>> }` — the test's own record, written when it creates each cursor, of the sprite it registered and, for an animation, its frame sources. Invariants compare against this record, never only against the backend's current maps, so a lost registration or a lost frame list is seen.
  - `fn sprite_cursor(b: &mut KmsBackend, m: &mut Model) -> u32` — `test_cursor(b)`, then allocates a `Storage::for_tests_null` 4×4 `Pixmap` in `b.store` at a fresh xid, inserts it into `b.cursor_pixmaps` for that cursor and into `m.sprites`. Animation frames are made from these, so frames carry sprites too.
  - `fn assert_invariants(b: &KmsBackend, m: &Model, reached_reclamation_point: bool)`:
    - `b.cursor_refs == recount(b)`;
    - **referenced** = recount keys ∪ {`effective_cursor_xid`, `default_cursor_xid`, `core.active_cursor`, `grab_cursor_override`}. For each referenced XID in `m`: its `cursor_records` entry exists; if `m.sprites` has it, that `DrawableId` is still in `b.store`; if `m.frames` has it, `anim_cursor_records[xid]` exists with exactly those frame sources (no early free);
    - if `reached_reclamation_point`: no XID in `released_cursors` is unreferenced (no leak).
  - A seeded generator (`xorshift64`, seeds `1..=64`, 300 steps each). Every seed starts with a fixed prefix: create a window, `define_cursor` a fresh cursor on it, `free_cursor` that cursor, `destroy_subwindow` the window, so the window-removal mutation always has a witness. Then ops, drawn uniformly, each skipped when its preconditions fail:
    - create a static cursor (`sprite_cursor`); create an animated cursor of 1–4 frames from live, unfreed static cursors, where a frame source may repeat;
    - create a window (through `window_geom_insert`, parented to the root, mapped) / `destroy_subwindow`;
    - `define_cursor` on a live window or the root with a live cursor or 0;
    - `free_cursor` on a live, not yet freed cursor (each XID at most once; freed XIDs are never used again, as the core guarantees);
    - `replace_cursor(old, new)` with both unfreed;
    - `set_grab_cursor(Some(live))` / `set_grab_cursor(None)`;
    - pointer move to a random live window: set `core.prev_pointer_window` and call `refresh_effective_cursor`, as `tests/cursor.rs:450, 512, 578` do. Ruling: the public path, `on_host_input`, needs a core `ServerState` that mirrors every window (`tests/crossings.rs:147`), which the random generator does not keep. `refresh_effective_cursor` is the backend function that path calls, so the bookkeeping under test is the same.
    - overlay: `get_overlay_window`; `define_cursor` on the COW xid; immediate release (`prepare_overlay_release` + `retire_overlay_window`); deferred release followed later by either reacquisition (`get_overlay_window`) or teardown (`stop_direct_after_scanout_replaced(DirectScope::Root, …)`).
      - Deferred-release fixture, as `present_scanout.rs:1079-1090` does: a fresh target window per cycle (`seed_window` at a new xid, so `install_direct_frame_for_target_test`'s `target + 1` source xid is new as well), `install_direct_frame_for_target_test(b, target, cow_id, true)`, then `b.pstate.direct.root_mut().unflip_shadow_ready = true` before `prepare_overlay_release`.
      - After a reacquisition, call `stop_direct_after_scanout_replaced` once to retire that cycle's frame before any later cycle installs another.
    - Leak checks follow each op's real guard. `assert_invariants(b, m, true)` runs after `define_cursor`, `set_grab_cursor`, `free_cursor` of a cursor with a record, `replace_cursor` with `old != new`, and `destroy_subwindow` of a window whose `cursor` was `Some` before the call. Every other op (creation, pointer move, every overlay op, `destroy_subwindow` of a cursorless window) runs `assert_invariants(b, m, false)`.

  Tests:
  - `cursor_refs_match_recount_under_random_sequences` — the generator above.
  - `duplicate_frame_sources_count_per_frame` — animation `[c1, c1, c2]`: `cursor_refs[c1] == 2`, `cursor_refs[c2] == 1`; free c1, c2 and the animation: all three gone, `cursor_refs` empty.
  - `cow_reacquisition_drops_the_overwritten_cursor_reference` — COW with `define_cursor(cow, c)`, deferred release, `get_overlay_window` again: `cursor_refs.get(&c) == None` and `assert_invariants(b, true)` holds after the next `set_grab_cursor(None)`.
  - `input_only_stand_in_is_not_counted` — built on the `tests/crossings.rs:147` fixture (core InputOnly window with a cursor, `windows_restructured`): first assert `b.input_only_pointer_hosts` has an entry whose `cursor == Some(c)`, then `cursor_refs.get(&c) == None`.

- [ ] **Step 2: Run to verify they fail**

  Run: `set -o pipefail; cargo test -p yserver --lib cursor_refcount 2>&1 | tail -20`
  Expected: compile failure on `cursor_refs` / `window_geom_insert` not found.

- [ ] **Step 3: Implement the fields and `cursor_refs.rs` helpers** with the signatures in Interfaces.

- [ ] **Step 4: Route every write through the helpers**
  - `self.windows.insert` / `.remove` in `windows.rs:1691`, `windows.rs:1983`, `overlay.rs:19`, `overlay.rs:114`, `overlay.rs:193` → `window_geom_insert` / `window_geom_remove`.
  - `geom.cursor =` in `cursor.rs:1027` and `cursor.rs:1135` → `window_set_cursor`. For `ReplaceCursor`, collect the matching keys first, then call the helper per key (the O(W) traversal stays, per spec Goals).
  - Animation insert (`cursor.rs:977`): `cursor_ref_add(frame.source)` per frame. Nothing removes `anim_cursor_records` outside `collect_released_cursors`; there, a removed record's frames are dropped with `cursor_ref_drop`.
  - Scalar slots: where `effective_cursor_xid` (:389), `grab_cursor_override` (:1047, :1142) or `core.active_cursor` (:1030, :1139) changes away from `Some(x)`, call `cursor_release_candidate(x)`. `backend_cursor_free_cursor` calls `cursor_release_candidate(host_xid)` next to its `released_cursors.insert`. `init_cursor_sprite` is untouched.
  - `cursor_referenced(xid)` becomes the four scalar comparisons plus `self.cursor_refs.contains_key(&xid)`.
  - The old loop in `collect_released_cursors` stays in this task; it only changes to drop frame refs when it removes an animation record, and to `reclaim_pending.clear()` at the end (candidates are not consumed yet).

- [ ] **Step 5: Write audit** (multiline, whole crate, run from `/data/Projects/yserver-387`)

  Run: `rg -U -n --glob '!**/tests/**' --glob '!**/for_tests.rs' '\bwindows\s*\.\s*(insert|remove)\s*\(|\.cursor\s*=[^=]|anim_cursor_records\s*\.\s*(insert|remove)\s*\(|\b(effective_cursor_xid|grab_cursor_override|active_cursor)\s*=[^=]' crates/yserver/src`
  Expected: hits only in `portable/cursor_refs.rs` (the helpers), the animation insert (`cursor.rs`, with its frame `cursor_ref_add`), the animation remove inside `collect_released_cursors`, `init_cursor_sprite`'s `effective_cursor_xid = None`, and the scalar assignments that call `cursor_release_candidate` for the value they replace. Paste the hit list into the report and account for each line. Fixture writes under `tests/` and `for_tests.rs` are allowed only in tests that do not exercise cursor lifecycle (spec, Design); none is added by this plan.

- [ ] **Step 6: Run tests**

  Run: `set -o pipefail; cargo test -p yserver --lib cursor 2>&1 | tail -5`
  Expected: all pass, existing `tests/cursor.rs` and `tests/cursor_sprites/` included, unchanged. Then `cargo +nightly fmt` and `cargo clippy --all-targets -- -D warnings`: clean.

  Mutations, each run and reverted: skip the drop in `window_geom_remove` → `cursor_refs_match_recount_under_random_sequences` fails; skip `cursor_ref_add` for frames → `duplicate_frame_sources_count_per_frame` fails; drop the replaced-entry drop in `window_geom_insert` → `cow_reacquisition_drops_the_overwritten_cursor_reference` fails.

- [ ] **Step 7: Commit**

  `git commit -S -m "fix(cursor): count window and animation-frame cursor references (#387)"` with the trailers from Global Constraints.

### Task 2: Pending-candidate reclamation and the counted predicate

**Files:**
- Modify: `crates/yserver/src/kms/render/backend.rs` (field `cursor_reclaim_checks: u64`)
- Modify: `crates/yserver/src/kms/render/backend.rs` constructor (~line 622: `cursor_reclaim_checks: 0`)
- Modify: `portable/cursor.rs` only: `cursor_reclaimable` goes beside `cursor_referenced` and `collect_released_cursors` (both stay private to `cursor.rs`), and the `collect_released_cursors` body
- Test: `crates/yserver/src/kms/render/backend/tests/cursor_refcount.rs`

**Interfaces:**
- Consumes: Task 1's fields and helpers.
- Produces:
  - field `pub(crate) cursor_reclaim_checks: u64` — cumulative, never reset;
  - `fn cursor_reclaimable(&mut self, xid: u32) -> bool` (private, in `portable/cursor.rs`) — increments `cursor_reclaim_checks`, returns `released_cursors.contains(&xid) && !self.cursor_referenced(xid)`. Every eligibility decision in `collect_released_cursors` goes through it.

- [ ] **Step 1: Write the failing tests**

  - `overlay_orphaned_cursor_survives_teardown_until_next_reclamation_point` — COW materialised, `define_cursor(cow, c)` with a `sprite_cursor` c, direct frame installed, `prepare_overlay_release` + `retire_overlay_window` (deferred), `free_cursor(c)`: c still alive. `stop_direct_after_scanout_replaced(...)` (teardown): right after it, `cursor_records` still has c and its sprite is still in `b.store`. Then `set_grab_cursor(None)`: c's record and sprite are gone, `released_cursors` does not contain c.
  - `shared_cursor_goes_with_the_last_window` — c defined on three windows, freed; `define_cursor(w, 0)` on two of them leaves c alive; the third leaves it gone.
  - `reclaim_checks_stay_bounded_with_a_large_released_backlog`:
    - 100 animated cursors of 20 frames each, every frame a `test_cursor`; all 2,000 frame XIDs freed; the animations stay live. Assert `released_cursors.len() == 2000`.
    - 20 windows created through `window_geom_insert`, two live static cursors a, b.
    - `let before = b.cursor_reclaim_checks;` then 1,000 `define_cursor` calls alternating a/b on the window under the pointer, and 1,000 `set_grab_cursor` calls alternating `Some(a)`/`None`.
    - Assert `b.cursor_reclaim_checks - before <= 4 * 2000` and `released_cursors.len() == 2000`.

- [ ] **Step 2: Run to verify they fail**

  Run: `set -o pipefail; cargo test -p yserver --lib cursor_refcount 2>&1 | tail -20`
  Expected: compile failure on `cursor_reclaim_checks`. Then add the field and `cursor_reclaimable`, and route the Task 1 sweep's `find` through `cursor_reclaimable` (the old body, now counted): the scaling test fails, since every call visits the 2,000 released frames. The overlay test may already pass, since teardown runs no sweep; its proof is the Step 5 mutation.

- [ ] **Step 3: Implement** the field, `cursor_reclaimable`, and the new `collect_released_cursors` body:
  1. `while let Some(xid) = self.reclaim_pending.pop()`;
  2. if `cursor_reclaimable(xid)`, then remove it from `released_cursors`, `cursor_records`, `cursor_pixmaps`; if it was an animation, `cursor_ref_drop` each frame source (this pushes cascade candidates); otherwise decref its sprite with `store_decref_with_invalidate`, exactly as today;
  3. the loop ends when `reclaim_pending` is empty. Nothing scans `released_cursors`.

  Keep the doc comment: it cites `dix/cursor.c` `FreeCursor` and `render/animcur.c:247` for the cascade.

- [ ] **Step 4: Run tests**

  Run: `set -o pipefail; cargo test -p yserver --lib 2>&1 | tail -5`
  Expected: all pass, Task 1's random-sequence test included. Then `cargo +nightly fmt` and `cargo clippy --all-targets -- -D warnings`: clean.

- [ ] **Step 5: Mutations** (each run and reverted)
  - Restore a scan of `released_cursors` through `cursor_reclaimable` inside `collect_released_cursors` → `reclaim_checks_stay_bounded_with_a_large_released_backlog` fails.
  - Reclaim inside `cursor_ref_drop` instead of pushing → `overlay_orphaned_cursor_survives_teardown_until_next_reclamation_point` fails.
  - Skip the frame `cursor_ref_drop` cascade → the existing `animated_cursor_keeps_its_freed_frames_alive` and the random test's leak assertion fail.
  - Skip `window_geom_remove` in `finish_cow_release` (direct `windows.remove`) → the overlay test or the random test's leak assertion fails.

- [ ] **Step 6: Diff review of the ordering rule**

  Run: `git diff -U8 8247002b -- crates/yserver/src/kms/render/backend/portable` and read the context around each of the six call sites (`cursor.rs` refresh, free, define, grab, replace; `windows.rs` destroy).
  Expected: each call keeps its position relative to the effective-cursor assignment, `sync_cursor_animation`, `displayed_cursor_pending` and `refresh_effective_cursor`; none is added, removed or moved. Record the six sites and the verdict in the report.

- [ ] **Step 7: Commit**

  `git commit -S -m "fix(cursor): reclaim released cursors from pending candidates, not a full sweep (#387)"` with the trailers.

### After the tasks (controller, not codex)

- Gate: `/data/Projects/claude-scratch/access-control/pr4c-v2/gate/gate.sh` adapted to this worktree (fmt, clippy ×3, Windows check of `yserver-core`, FreeBSD clippy, workspace tests, lavapipe `--ignored`, `tools/check-layout.sh`, `just man`).
- Hardware acceptance (user's permission per session): a local client in `/data/Projects/claude-scratch/issue-387/` creates 100 animated cursors and frees their frame XIDs, then changes a window cursor and re-grabs the pointer 60 times a second; compare `ChangeWindowAttributes` and `GrabPointer` times in `YSERVER_LOOP_TELEMETRY=1` output, master `8247002b` against the branch. Then CS2 in a session aged by browser use, master against the branch, with MangoHud.
- A branch build for the reporter.
