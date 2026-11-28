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
- **B (real paths):** windows live in a core `ServerState` and in the backend together,
  as in `stacking.rs:222` (`seed_state_window`, `state.resources.map_window` /
  `configure_window`, then `b.sync_top_level_order(&state)`, which is where the walk's
  `top_level_order` comes from). Mapping uses `map_window_for_tests`, which realizes
  storage; bare `map_subwindow` leaves a window without storage. Restack, reparent and
  destroy go through the core resource call and then the matching backend entry, and
  `sync_top_level_order` runs after each. Ticks run through the existing two-output recording fixture
  (`RecordingSceneFixture::new(2)`, `backend/tests/commit_identity/recording_scene.rs:116`),
  with accepted flips retired between ticks the way that fixture's existing tests do.
  No direct writes to the index, and no direct `windows.insert` in the new tests.
- **Live Vulkan:** the new tests follow the branch's convention for recording-fixture
  tests (`#[ignore]`, run with `-- --ignored` and `YSERVER_ALLOW_SOFTWARE_VULKAN=1`).
  - A fixture skip is not a pass. `skip_unless_vulkan_fed_scanout` prints
    `SKIP recording_scene`.
  - Every live-Vulkan command (the capability check, the targeted runs, the mutation
    runs and the loaded runs) first sources
    `/data/Projects/claude-scratch/issue-388/walk/lvp-env.sh`. That pins the lavapipe
    ICD; system Mesa has no lavapipe.
  - Check the capabilities with `vulkaninfo` under that env, the same way
    `ci.yml:153-165` does (`VK_KHR_external_semaphore_fd`,
    `VK_EXT_image_drm_format_modifier`).
  - The test commands add `--nocapture`, because libtest hides the skip line otherwise.
    Unmutated runs must show the named tests passing with no `SKIP recording_scene`
    line. If the fixture skips, report BLOCKED.
  - Mutation runs are the opposite: each one must FAIL at the assertion its test names
    for it, still with no `SKIP` line. Record that assertion message in the report.
- **F (load):** run the new tests plus `stacking` and `window_tree` 5× under `nproc`
  busy loops, with zero failures.

## Review Focus

1. A test or fixture that writes `b.windows` through `DerefMut` between two ticks, for
   example `insert`, must see a fresh index. That write path stays valid, and existing
   fixtures use it.
2. Destroying a parent whose children are still in the map: the destroy is a map write,
   so the next read rebuilds. The builder's semantics are unchanged: it still lists
   children still in the map under their recorded parent id. The scene drops the subtree
   because traversal stops at the missing parent, as it does today.
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
  - `#[derive(Debug, Default)] pub struct IndexedWindows`, which owns the `WindowsMap`,
    a `OnceCell<ChildrenIndex>` and a `Cell<u64>` counting builds;
  - `impl From<WindowsMap> for IndexedWindows`, used by the render-crate fixtures and by
    the reference walk;
  - `impl Deref<Target = WindowsMap>`;
  - `pub fn map_mut(&mut self) -> &mut WindowsMap`, which empties the cell first;
  - `pub fn children(&self) -> &ChildrenIndex`, which builds on a miss and adds 1 to the
    counter;
  - `pub fn children_index_builds(&self) -> u64`, cumulative;
- Produces: `SceneInput::windows: &'a IndexedWindows`.
- `TrackedWindows`:
  - Its inner field becomes `IndexedWindows`.
  - It keeps `Deref<Target = WindowsMap>` and `IntoIterator for &TrackedWindows`, so
    existing readers and loops are unchanged.
  - `deref_mut` returns `self.map.map_mut()` after bumping `generation`.
  - `generation()` is unchanged.
  - New `pub(crate) fn indexed(&self) -> &IndexedWindows`.
- `scene_input(core, windows: &IndexedWindows, …)`. Callers pass
  `self.windows.indexed()`, and the new tests read
  `b.windows.indexed().children_index_builds()`.
- Render-crate fixtures (`scene/tests/mod.rs:1014-1030`): the fixture that builds a
  `SceneInput` owns an `IndexedWindows` built with `From` before `TestCore::input`
  borrows it. Storing it in the fixture struct in place of the bare map is enough.

- [ ] **Step 1: Write the failing tests** in `scene_children_cache.rs`. Both use
  `RecordingSceneFixture::new(2)` and the rule-B entries. After every tick, before any
  other scene call, the test asserts how many walks the tick did (the lifetime
  `build_scene_calls` telemetry delta) and how many index builds it caused. Draw rects
  are read only after that.
  - `children_index_built_once_across_damage_only_ticks`:
    - Setup: on output 0, a top-level with two overlapping mapped child siblings. Also
      two top-levels that span both outputs.
    - Action: tick once, then five times add damage to the child only (a
      `poly_fill_rectangle` or `copy_area` into it), retire the accepted flips, and
      tick.
    - Assert on the first tick: both outputs walked (walk delta 2) and the index was
      built once.
    - Assert on each later tick: at least one output walked (walk delta ≥ 1) and there
      were 0 builds.
    - Assert also: `scene_draw_rects_for_tests()` (output 0) is identical to the list
      after the first tick. Read it after the counters.
    - Mutation that must fail: `children()` rebuilding on every call.
  - `children_index_follows_tree_changes`:
    - Action: after a first tick, apply in turn, each one visible on output 0:
      - restack the lower of the two overlapping sibling children above the other;
      - reparent one child to the other top-level on output 0;
      - create and map a new child there;
      - destroy one top-level that has a visible part on output 0.

      Retire flips and tick after each.
    - Assert, for each step, that output 0's draw list changed from the previous step,
      so the step is observable.
    - Assert right after each tick: walk delta ≥ 1 and exactly 1 build.
    - Then compare `scene_draw_rects_for_tests()` with a reference list from a new
      `#[doc(hidden)]` accessor. The accessor builds a fresh
      `IndexedWindows::from(map.clone())` of the current map and calls `scene_draw_rects`
      with it, so the cache under test is never consulted.
    - Mutation that must fail: `map_mut()` not emptying the cell.

- [ ] **Step 2: Run them to verify they fail to compile or fail**

  Run: `cargo test -p yserver --lib scene_children_cache -- --ignored`
  Expected: compile errors (`children_index_builds`, `IndexedWindows` not found).

- [ ] **Step 3: Implement `IndexedWindows` and wire it.**
  - `build_scene_with` replaces `let children = children_index(input.windows);` with
    `let children = input.windows.children();`. The call stays inside the existing
    timer in `tick_one_output`.
  - `visit_window_subtree` keeps taking `&ChildrenIndex` and `&WindowsMap` (deref).
  - Fix compile errors at every caller.

- [ ] **Step 4: Run the new tests and the scene suites**

  Run: `. /data/Projects/claude-scratch/issue-388/walk/lvp-env.sh && cargo test -p yserver --lib scene_children_cache -- --ignored --nocapture`
  → 2 passed, with no `SKIP recording_scene`. Then each by its own command:
  `cargo test -p yserver-render --lib scene`, `cargo test -p yserver --lib stacking`,
  `cargo test -p yserver --lib window_tree`, `cargo test -p yserver --lib crtc_transform`
  → all pass.
  - No existing test is modified except for type plumbing.
  - Render-crate migration: `TestCore` owns no map. Its callers pass local `WindowsMap`s
    and helper parameters (`scene/tests/mod.rs:989-1030`, `build_scene.rs`,
    `projection.rs`, and the others). Wrap each map with `IndexedWindows::from` before
    it is borrowed into `SceneInput`.
  - Change helper signatures where the map is threaded through.

- [ ] **Step 5: Run the mutations listed in Step 1** under the live-Vulkan rules.
  - Each must fail its named test at the named assertion, with no `SKIP`.
  - Record the output in the report, then revert.
  - Run the Gate below before the commit.

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
  `telemetry_counts_children_index_builds_on_tick_walks`, in the same file and with the
  same fixture and conventions as Task 1:
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

- [ ] **Step 4: Run it.** Expected: PASS, with no `SKIP`, under the live-Vulkan rules.
  - Then the Task 1 tests still pass.
  - Run the named mutation; it must fail at its assertion.
  - Run the Gate below before the commit.

- [ ] **Step 5: Commit**

```bash
git commit -S -am "perf(telemetry): report children index builds per second (#388)"
```

## Gate (implementer, in each task before its commit)

- `cargo +nightly fmt` first, then `cargo +nightly fmt -- --check`.
- `cargo clippy --all-targets -- -D warnings` (as CI and AGENTS.md require), and also
  `cargo clippy --workspace --all-targets --features xdmcp -- -D warnings`.
- `cargo test --workspace --features xdmcp`.
- Each integration test file in `crates/yserver/tests/` by its own command, except
  `render_acceptance`.
- The lavapipe suite:
  `. /data/Projects/claude-scratch/issue-388/walk/lvp-env.sh && cargo test --workspace --features xdmcp --no-fail-fast -- --ignored`,
  run after the capability check above.
- `just man`.
- Rule F: the new tests plus `stacking` and `window_tree`, 5× under `nproc` busy loops,
  with zero failures.
- FreeBSD compile check (`~/Projects/claude-scratch/freebsd-sysroot/env.sh`, clippy
  `--target x86_64-unknown-freebsd`).

## Measurement and handoff (coordinator, after Task 2)

- Use the existing harness `tools/vng-shot.sh` with `--gpu none --outputs 2 --env YSERVER_LOOP_TELEMETRY=1`,
  which gives software rendering on two outputs and the telemetry line. `--headless`
  creates no outputs and does not apply.
- Run the same scene at master `d45fdcff` and at the branch head: ~10 mapped windows,
  one client damaging a window at ~50 Hz, 30 s each.
- Report `avg_build_scene_ns` and `build_scene_calls/s`, plus `children_index_builds/s`
  for the branch only. At `d45fdcff` there is no such field: every walk built the index,
  so the baseline build rate equals `build_scene_calls/s`.
- Per spec, `avg_build_scene_ns` is walk latency. The CPU total comes from Samueru's
  perf rerun.
- No GPU hardware run without asking the user.
- Handoff: after the PR is open, draft a #388 comment for the user to post. It covers:
  - the local numbers;
  - the correction that only part of the walk is cacheable;
  - a request for Samueru's perf rerun, with the `children_index_builds/s` telemetry line.

  When he answers, report the result on #388 the same way.
