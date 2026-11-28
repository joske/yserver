# Scene children index cache (#388) — design

## Why

#388: on an "idle" i3 + fastcompmgr desktop with a GL wallpaper (3 outputs, RX 580),
yserver uses 8.75% CPU against Xorg's 6.07%. The desktop is not idle: the wallpaper
presents ~44/s and the compositor repaints ~49 times a second, so the scene is walked
at ~49 Hz on every output that has something to show.

Samueru's frame-pointer profile (`/data/Projects/claude-scratch/issue-388/samueru-3/x/`)
puts `build_scene_with` at 16.9% of yserver's user samples. 4.7% of that is
`children_index` (`scene/walk.rs:31`): every walk of every output rebuilds a
`HashMap<parent, Vec<child>>` over the whole `WindowsMap` and sorts each list by
`stack_rank`. Between wallpaper frames the window tree does not change, so the result
is the same every time.

## Goal

Build the children index only when the window tree may have changed, and share it
between every scene walk (all outputs, all ticks, and the non-tick callers of
`build_scene`) until then. The emitted scene must stay identical.

## Non-goals

- Caching per-node visibility, placement or damage collection in
  `visit_window_subtree` (the larger option B). That needs its own design and is
  decided after measuring this one.
- Changing the other whole-map child scans (`portable/inferiors.rs`,
  `portable/redirect.rs`, `portable/windows.rs`), hasher choice, or the `cow_diag`
  message.
- Rootless, macOS and Windows backends. They do not use the KMS scene walk.
- No new `YSERVER_*` environment variables.

## Approaches

- **A. Cache keyed on the existing window-map mutation point (chosen by the user).**
  `TrackedWindows` (`crates/yserver/src/kms/render/backend.rs:108`) already bumps a
  generation on every `DerefMut`, which is the only way to write the map. The index
  depends only on which windows exist and on their `parent` and `stack_rank`, and any
  change to those goes through `DerefMut`. Building the index lazily and dropping it in
  `DerefMut` is correct by construction. It is also conservative: a move or resize drops
  it too, which costs one rebuild and nothing else.
- **B. Also cache visibility per node, and re-walk only for damage.** It could save up
  to ~12–15%, but its inputs include store state (`scene_participating`,
  `redirected_target`, storage extents), shapes and `top_level_order`, not just the
  window map. Deferred (see Non-goals).
- **Rejected: a cache in `SceneCompositor` keyed on a generation number passed through
  `SceneInput`.** It works, but the cache would be separate from the thing it mirrors.
  A `TrackedWindows` that gets replaced would restart at generation 0 and could match a
  stale entry. Keeping the cache inside `TrackedWindows` removes that case.

## Design

1. **Cache.** `TrackedWindows` keeps the index in a `std::cell::OnceCell`.
   - It is built on first read by the existing `children_index` builder, so the
     ordering is the same: children bottom-to-top by `stack_rank`.
   - `DerefMut` clears it, in the same place that bumps `generation`.
   - The builder and `ChildrenIndex` become visible to the `yserver` crate (`pub`).
     Their behaviour does not change.
2. **Input.** `SceneInput` gains a field holding the index (for example
   `children: &'a ChildrenIndex`). `scene_input()` fills it from `TrackedWindows`.
   - `build_scene_with` uses the field instead of calling `children_index` itself.
   - The render crate's own test inputs (`scene/tests/mod.rs:1019`) build it with the
     same builder.
3. **Counter.** `TrackedWindows` counts the index builds, and the per-second telemetry
   line reports it next to `build_scene_calls/s`. With this counter Samueru can show the
   hit rate on his own setup. This is the measurement step before option B.

## Tests (rules A/B/C: real paths, through the KMS backend's production entry points)

- **Structural change is seen.** Drive a tick, then change the tree through production
  entry points (restack a sibling, reparent, map a new child, destroy one), then tick
  again. After each step the emitted draw order must match a fresh build. Mutation that
  must fail: `DerefMut` does not clear the cache.
- **Damage-only frames reuse it.** Several ticks that only add damage to a mapped
  window, across two outputs, produce exactly one index build. Mutation that must fail:
  build on every read.
- **The order is unchanged.** The existing scene ordering and stacking tests pass
  unmodified.

## Measurement

Report `build_scene` average ns before and after, on a local multi-output headless or
vkms scene with a damage-only client at ~50 Hz. Then ask Samueru for a perf rerun.
Expected: `children_index` disappears from the profile, and about 4.7% of yserver's
user samples are saved (~0.25 of the 2.5 pp user-time gap). This does not close the
gap by itself, and the result goes to the issue in those terms.

## Left to the builder

The exact field and method names, and where the counter is surfaced in telemetry.

## Provenance

yserver code only. No external sources.
