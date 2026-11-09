# Solid fills as one instanced draw — implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `RecordedFillRect` is emitted as one instanced draw (`vkCmdDraw(4, N)`) instead of `vkCmdClearAttachments` with N rectangles, with identical pixels.

**Architecture:** Fill recording uploads the clamped rectangles to the frame's upload arena as instance data (the core-text pattern). A new solid-fill pipeline (per destination format, lazily cached) reuses `logic_fill.frag` and the `LogicFillPushConsts` ABI, with a new instance-rate vertex shader. `emit_fill_draws` binds pipeline, scissor, push constants and the pinned instance slice, and draws once.

**Tech Stack:** Rust, ash/Vulkan, GLSL compiled by `crates/yserver-render/build.rs`; engine tests on lavapipe.

**Spec:** `docs/superpowers/specs/2026-11-08-instanced-solid-fill-design.md` (revision 2, approved by the user 2026-11-08).

## Global Constraints

- Scope: only `fill_rect` / `fill_rect_batch` → `RecordedFillRect` → `emit_fill_draws`. Unchanged: every `logic_fill` case (depth-1 GXcopy included), CPU fallbacks, `scene/compose.rs` background clear, `kms/vk/ops/fill.rs`.
- No per-driver path, no `YSERVER_*` variable, no option to restore the clear. No new destination formats (production: BGRA8, R8).
- Push ABI: `LogicFillPushConsts`, 48 bytes, viewport at byte 16, colour at byte 32, same VERTEX|FRAGMENT range; `logic_fill.frag` unchanged.
- Pipeline: colour attachment 0, 1 sample, triangle strip, no cull/depth/stencil, no logic op, no blending, RGBA write mask; must not need the `logicOp` feature.
- Pass state unchanged (barriers, LOAD/STORE dynamic rendering, close transition). The viewport is not reset per fill; pipeline, scissor, push constants and vertex buffer (buffer **and** offset) are set on every fill.
- The pin set comes from the frame being replayed, threaded through all three callers.
- `RecordedFillRect` carries pin + instance count, not the rectangle list; the trace at `engine/frame.rs` ~365 reports the instance count.
- Reuse first; new code only where the goal is impossible otherwise, each piece justified in the report. No copyleft code.
- Tests run on lavapipe through real record/emit paths; a fixture that returns early without a device is not a pass. Each test names the mutation that fails it; every mutation is run; report executed vs skipped. Test files stay under 6000 lines (`tools/check-layout.sh`): new tests go in a new `engine/tests/instanced_fill.rs` registered in `engine/tests/mod.rs`.

## Review Focus

1. A fill that continues a session after a composite or logic fill must not inherit that op's pipeline, scissor, push constants or vertex buffer.
2. Two fills in one frame must each read their own instance slice (nonzero arena offset).
3. Depth-32 alpha and R8 bytes must be written exactly as the clear wrote them (no blend, no premultiply, no RGB-only mask).
4. Pixels outside every rectangle, including one-pixel and edge-clipped rectangles, must be untouched.
5. A very large fill (dedicated arena block) must draw every instance.

---

### Task 1: Instanced solid fill (one functional commit)

**Files:**
- Create: `crates/yserver-render/src/kms/vk/shaders/solid_fill.vert.glsl`; the pipeline cache (a new `vk/solid_fill_pipeline.rs`, or a variant inside `vk/logic_fill_pipeline.rs` if that reuses more; justify the choice).
- Modify: `render/frame_builder.rs` (`RecordedFillRect` ~912: pin + instance count instead of `rects`), `render/engine/fill_copy.rs` (`fill_rect_batch` ~104–152: upload with `upload_to_frame`, `UPLOAD_VERTEX_ALIGN`), `render/engine.rs` (cache field next to `logic_fill_caches` ~741) and its initializer in `render/engine/lifecycle.rs` (~174–195), following the existing cache's ownership/Drop pattern; `vk/mod.rs` (~20) if a new pipeline module is added; lazy-cache pattern at `engine/fill_copy.rs` ~174, instance-input pattern at `vk/text_pipeline.rs` ~380–418; `render/engine/emit.rs` (`emit_fill_draws` ~1114 and its callers ~1056/1088, `emit_session_open_and_draws` ~1354/1379, `emit_session_continue_draws` ~1458/1469: pass the replay `FramePinSet`), `render/engine/frame.rs` (~365 trace; replay call sites ~729), plus any test that constructs `RecordedFillRect` directly (e.g. `frame_builder.rs` ~1562, ~1728).
- Test: new `crates/yserver-render/src/kms/render/engine/tests/instanced_fill.rs`, registered in `engine/tests/mod.rs`.

**Interfaces:**
- Produces: `RecordedFillRect { …, instance_pin: PinnedUploadIdx, instance_count: u32 }`; a per-format solid-fill pipeline cache owned by the engine; a test-only, engine-scoped fill observation recording, per emitted `RecordedFillRect`, the attachment clears it issued and its draw's instance count (no process-global counters, no production telemetry).

- [ ] **Step 1: Failing tests** (spec *Testing*, names fixed here):
  - `instanced_fill_one_draw_per_fill`: N = 1 and N = 1000 post-clamp non-empty rects → 0 clears, 1 draw, instance count N. Mutations: one draw per rect; clear kept; instance count 1.
  - `instanced_fill_pixels_exact`: odd-sized destination; one-pixel rects and spans, adjacent, overlapping, edge-clipped; sentinels outside. Depth-32 colour with alpha ∉ {0, 255} over a different known colour, all four BGRA bytes compared; R8 with red ∉ {0, 255} and ≠ alpha. Mutations: quad size − 1; premultiplied source-over blending.
  - `instanced_fill_in_session`: fold-clean textured composite or logic-fill predecessor leaving a narrow scissor, then two fills with different colours and instance slices; both take `Continue` (observed at replay, `engine/frame.rs` ~714–737, through test-only engine state); pixels outside the predecessor's scissor correct. A and B are small uploads in the same arena block: assert the same buffer handle and B's nonzero offset, and give A and B distinguishable coverage that survives to readback. Mutations (each alone): skip pipeline bind; skip scissor; bind the vertex buffer at offset 0; skip the colour push.
  - `instanced_fill_large`: 200 000 varied rects (dedicated arena block) → 1 draw, instance count 200 000, early and late instances read back. Mutation: instance count 1.
- [ ] **Step 2:** Run them on lavapipe (`--ignored`, software-device opt-in, ICD pinned, `--nocapture` so early-return diagnostics are visible; the new fixtures fail, not return, without a device) and record each result. The draw-count rows fail on the observer (clears present); pixel rows may already pass and must not be weakened.
- [ ] **Step 3:** Implement per the spec *Design*.
- [ ] **Step 4:** All `yserver-render` engine tests, the existing fill/span/core-text pixel tests and `render_acceptance` pass unchanged; run every named mutation and record its failing assertion.
- [ ] **Step 5:** Task 1 gate (Verification). Commit.

### Task 2: Docs

**Files:** `docs/status.md` (one new entry at the top).

- [ ] **Step 1:** Entry: solid fills on the `RecordedFillRect` path are one instanced draw; why (#155: hasvk per-rect BLORP, gen7 parser cost, the 128 MiB batch-pool crash); scope and what is unchanged; performance numbers come from Task 3.
- [ ] **Step 2:** `git diff --check`. Commit.

### Task 3: Performance (coordinator; ask the user first)

x11perf `-rect1 -rect10 -rect100 -ftext -fitext` (`-repeat 2 -time 3`) on master `68fa4be2` and on this branch, AMD DP-1 and NVIDIA DP-3, logs under `~/Projects/claude-scratch/issue-155/instanced-fill/perf/`. Report measured values; any regression beyond run-to-run noise goes back to the user before the PR. Then a test branch for the #155 reporter.

## Verification

All `cargo test` runs source `/home/ariel_santangelo/Projects/claude-scratch/atomic-cursor-split/sdd-p1/lvp-env.sh`, use `--nocapture` (or `--show-output`) for the runs that feed the report, and report executed vs early-skipped tests (existing hardware-dependent skips stay visible, not fixed here).

- **Task 1 gate:** `cargo +nightly fmt --all`; `cargo build --bin yserver`; `cargo test --workspace --locked --features xdmcp`; the whole ignored suite `--workspace --locked --features xdmcp -- --ignored` (includes `render_acceptance`); the `yserver-render` engine tests and the new module with `--include-ignored` 5× under 16 busy loops, reaping every loop; `cargo clippy --all-targets -- -D warnings` and `--workspace --all-targets --locked --features xdmcp -- -D warnings`; FreeBSD clippy (`source ~/Projects/claude-scratch/freebsd-sysroot/env.sh`, `--target x86_64-unknown-freebsd --all-targets`); `tools/check-layout.sh`.
- **Task 2 gate:** `git diff --check`.
- **Branch gate (coordinator, before the PR):** the Task 1 gate on the final HEAD, plus `just man`.
