# Scene children index cache (#388) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** build the scene's parent→children index only when the window map changes,
instead of on every walk of every output.

**Architecture:** a render-crate type owns the `WindowsMap` together with a lazily built
index. Its only write accessor empties the index. `TrackedWindows` stores that type, and
`SceneInput::windows` points at it, so the walk takes the map and the index from one
owner. A telemetry counter reports how many builds happen.

**Tech Stack:** Rust, `std::cell::OnceCell`, existing `children_index` builder.

**Spec:** `docs/superpowers/specs/2026-11-27-scene-children-index-cache-design.md`

## Global Constraints

- The emitted scene must be identical: the same index contents and ordering (children
  bottom-to-top by `stack_rank`, the existing builder's stable sort).
- No new `YSERVER_*` environment variables (maintainer rule).
- Reuse first: the existing `children_index` builder and the existing `DerefMut` hook in
  `TrackedWindows` (`crates/yserver/src/kms/render/backend.rs:108-142`). Add new code
  only where the goal is impossible otherwise.
- No platform-specific code. macOS and Windows share `XBackend<P>` and get the cache
  through the same path.
- Out of scope: per-node visibility caching, the other whole-map child scans
  (`portable/inferiors.rs`, `portable/redirect.rs`, `portable/windows.rs`), the hasher,
  `cow_diag`.

## Test rules (restate in every implementer prompt)

- **A (end state):** each scenario test asserts the final state (the emitted draw order
  and the build count), not only intermediate calls.
- **B (real paths):** windows are created, mapped, restacked, reparented and destroyed
  through the `KmsBackend` production entries that core calls (`create_subwindow`,
  `map_subwindow`/`map_window_for_tests`, the configure/restack and reparent entries,
  `destroy_subwindow`), and ticks run through `maybe_composite` /
  `tick_maybe_composite_for_tests`. No direct writes to the index, and no direct
  `windows.insert` in the new tests.
- **F (load):** run the touched lavapipe suites 5× under `nproc` busy loops.

## Review Focus

1. A test or fixture that writes `b.windows` through `DerefMut` between two ticks, for
   example `insert`, must see a fresh index. That write path stays valid, and existing
   fixtures use it.
2. Destroying a parent whose children are still in the map: the index must drop both
   with the next read.
3. A geometry-only change (move, resize) invalidates the index. That is correct, and
   the cost is one rebuild.
4. Root readback (`GetImage`/`CopyArea` of the root) between ticks shares the cache and
   must not change what the tick emits.
5. Two outputs in one tick: one build serves both.

---

### Task 1: Map-plus-index owner, wired into the scene walk

**Files:**
- Modify: `crates/yserver-render/src/kms/render/window_geometry.rs` (new owner type
  beside `WindowsMap`)
- Modify: `crates/yserver-render/src/kms/render/scene.rs:126` (`SceneInput::windows`
  type; `ChildrenIndex` at `:1082` becomes `pub`)
- Modify: `crates/yserver-render/src/kms/render/scene/walk.rs:31` (`children_index`
  stays the builder; visibility as needed by the owner type)
- Modify: `crates/yserver-render/src/kms/render/scene/build.rs:393` (read the index from
  the input)
- Modify: `crates/yserver-render/src/kms/render/scene/tests/mod.rs:1019` (test inputs
  wrap their map)
- Modify: `crates/yserver/src/kms/render/backend.rs:108-160` (`TrackedWindows` stores
  the owner; `scene_input()` takes it)
- Modify: every `scene_input(` caller (`kms/session.rs:427`, `portable/compose.rs:112`,
  `portable/readback.rs:161,211`, `for_tests.rs:148,180`, `crtc_transform_tests.rs:87,405,473`)
  and any other reader that needs `&WindowsMap`, where deref does not cover it
- Test: `crates/yserver/src/kms/render/backend/tests/` (a new file
  `scene_children_cache.rs`, registered in the tests module)

**Interfaces:**
- Produces, in `yserver_render::kms::render::window_geometry`:
  - `pub struct IndexedWindows`, which owns the `WindowsMap`, a
    `OnceCell<ChildrenIndex>` and a `Cell<u64>` counting builds;
  - `impl Default`;
  - `impl Deref<Target = WindowsMap>`;
  - `pub fn map_mut(&mut self) -> &mut WindowsMap`, which empties the cell first;
  - `pub fn children(&self) -> &ChildrenIndex`, which builds on a miss and adds 1 to the
    counter;
  - `pub fn children_index_builds(&self) -> u64`, cumulative;
  - `impl<'a> IntoIterator for &'a IndexedWindows`, matching the existing
    `TrackedWindows` impl.
- Produces: `SceneInput::windows: &'a IndexedWindows`.
- `TrackedWindows::deref_mut` returns `self.map.map_mut()` after bumping `generation`.
  `generation()` is unchanged.

- [ ] **Step 1: Write the failing tests** in `scene_children_cache.rs`, all through the
  rule-B entries.
  - `children_index_built_once_across_damage_only_ticks`:
    - Setup: two outputs, three mapped top-levels, one of them with a mapped child.
    - Action: tick once, then five times add damage to the child only (a
      `poly_fill_rectangle` or `copy_area` into it) and tick.
    - Assert: `b.windows.children_index_builds()` grew by exactly 1 over all six ticks,
      for both outputs together.
    - Mutation that must fail: `children()` rebuilding on every call.
  - `children_index_follows_tree_changes`:
    - Action: after a first tick, apply in turn: restack a sibling above another,
      reparent the child to another top-level, create and map a new child, destroy one
      top-level. Tick after each.
    - Assert after each tick: `scene_draw_rects_for_tests()` equals the draw rects from
      a reference walk over a freshly built index of the same map. Add a `#[doc(hidden)]`
      test accessor that walks with `children_index(&map)` built fresh, reusing
      `scene_draw_rects`.
    - Assert also: the build counter grew by exactly 1 per change.
    - Mutation that must fail: `map_mut()` not emptying the cell.
  - `fixture_write_through_deref_mut_invalidates`:
    - Action: tick, then `b.windows.insert(...)` of a mapped sibling (the path existing
      fixtures use), then tick.
    - Assert: the new window's rect is in the draw list.
    - Mutation that must fail: `TrackedWindows::deref_mut` returning the inner map
      without `map_mut()`.

- [ ] **Step 2: Run them to verify they fail to compile or fail**

  Run: `cargo test -p yserver --lib scene_children_cache`
  Expected: compile errors (`children_index_builds`, `IndexedWindows` not found).

- [ ] **Step 3: Implement `IndexedWindows` and wire it.**
  - `build_scene_with` replaces `let children = children_index(input.windows);` with
    `let children = input.windows.children();`. The call stays inside the existing
    timer in `tick_one_output`.
  - `visit_window_subtree` keeps taking `&ChildrenIndex` and `&WindowsMap` (deref).
  - Fix compile errors at every caller.

- [ ] **Step 4: Run the new tests and the scene suites**

  Run: `cargo test -p yserver --lib scene_children_cache` → 3 passed. Then
  `cargo test -p yserver-render --lib scene` and
  `cargo test -p yserver --lib stacking window_tree crtc_transform` → all pass, with no
  existing test modified except for type plumbing.

- [ ] **Step 5: Run the mutations listed in Step 1.** Each must fail its named test.
  Record the output in the report. Then revert.

- [ ] **Step 6: Commit**

```bash
git add -A crates
git commit -S -m "perf(scene): build the children index once per window-map change (#388)"
```

### Task 2: Telemetry counter

**Files:**
- Modify: `crates/yserver-render/src/kms/render/telemetry.rs:165-175, 540-620, 1140`
- Modify: `crates/yserver-render/src/kms/render/scene/tick.rs:766-779`

**Interfaces:**
- Consumes: `IndexedWindows::children_index_builds()` from Task 1.
- Produces: `Telemetry::record_children_index_builds(&mut self, n: u64)`, bucket and
  lifetime, the same shape as `record_build_scene_ns`. The per-second line prints
  `children_index_builds/s=<n>` right after `build_scene_calls/s`.

- [ ] **Step 1: Write the failing test**
  `telemetry_counts_children_index_builds_on_tick_walks`, through the backend:
  - Action: tick (1 build), three damage-only ticks (0 builds), a restack plus a tick
    (1 build).
  - Assert: the lifetime telemetry counter is 2, while `build_scene_calls` ≥ 5.
  - Mutation that must fail: recording the delta from before the first tick, or
    recording 1 per call.

- [ ] **Step 2: Run it to verify it fails.** Expected: the method is not found.

- [ ] **Step 3: Implement.** In `tick_one_output`:
  - read `input.windows.children_index_builds()` just before `build_scene_with` and
    again just after;
  - record the difference next to `record_build_scene_ns`.

  Only tick walks count, so root readbacks and the damage audit's second walk stay out
  of the numerator (spec, Counter).

- [ ] **Step 4: Run it.** Expected: PASS. Then the Task 1 tests still pass.

- [ ] **Step 5: Commit**

```bash
git commit -S -am "perf(telemetry): report children index builds per second (#388)"
```

## Gate (implementer, after each task)

- `cargo fmt --all` first, then `cargo fmt --all -- --check`.
- `cargo clippy --workspace --all-targets --features xdmcp -- -D warnings`.
- `cargo test --workspace --features xdmcp`.
- Each integration test file in `crates/yserver/tests/` by its own command, except
  `render_acceptance`.
- The lavapipe suite: `cargo test --workspace --features xdmcp -- --ignored`, with the
  lavapipe ICD env.
- `just man`.
- Rule F: the new tests plus `stacking` and `window_tree`, 5× under `nproc` busy loops,
  with zero failures.
- FreeBSD compile check (`~/Projects/claude-scratch/freebsd-sysroot/env.sh`, clippy
  `--target x86_64-unknown-freebsd`).

## Measurement (coordinator, after Task 2)

- Run the same headless (lavapipe) scene at master `d45fdcff` and at the branch head:
  2 outputs, ~10 mapped windows, one client damaging a window at ~50 Hz, 30 s each.
- Report `avg_build_scene_ns`, `build_scene_calls/s` and `children_index_builds/s` from
  the telemetry line.
- Per spec, `avg_build_scene_ns` is walk latency. The CPU total comes from Samueru's
  perf rerun.
- No GPU hardware run without asking the user.
