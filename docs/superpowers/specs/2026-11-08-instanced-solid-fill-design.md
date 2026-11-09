# Solid fills as one instanced draw — design

**Date:** 2026-11-08 · **Status:** revision 2 (sol review: test observer, session test, pixel fixtures, scope wording)
· **Issue:** #155 · **Scope:** the `RecordedFillRect` emit path of the render engine.

## Why

The engine's solid fill (`fill_rect` / `fill_rect_batch` → `RecordedFillRect` → `emit_fill_draws`) is drawn with
`vkCmdClearAttachments`, with one `VkClearRect` per rectangle. That path carries normal full-plane-mask GXcopy
fills such as `PolyFillRectangle`, the spans core text is drawn with, and server background clears routed
through it. On Ivy Bridge (hasvk, gen7) its cost grows with the rectangle count:

- hasvk runs one BLORP operation per clear rectangle (`clear_color_attachment` in `anv_blorp.c`). Each one emits
  vertex state, pipeline state and a primitive into the batch.
- On this generation, hasvk cannot use batch chaining because of i915's command-parser restriction. The kernel
  parses every batch, so the CPU cost of each submit grows with the batch size.
- Without chaining, hasvk grows one batch buffer by doubling it. Its batch-buffer pool has 16 size classes, up to
  128 MiB. The class index is checked only by an `assert`. When the batch has to grow to 256 MiB, a release build
  indexes past the end of that table and crashes. That is the segfault in #155, under `x11perf -rect10`, `-ftext`
  and `-fitext` (Mesa 26.2.6, symbolized with the matching debug build).

The per-rectangle cost is therefore the core-font stutter the reporter saw, and Xft fonts avoided it. The same
cost at its extreme is the crash. With the Vulkan validation layer, the same three tests on AMD reported no
validation errors.

## Decision

The fill on this path becomes **one instanced draw**: a 4-vertex quad with one instance per rectangle, and the
rectangles in a per-frame instance buffer. hasvk emits a single `3DPRIMITIVE` with the instance count, so the
commands emitted for one fill do not depend on its rectangle count. The same applies on every driver; there is
no per-driver path.

Not chosen:

- **One draw per rectangle** (the existing `logic_fill` emit pattern): it removes the BLORP cost, but the batch
  still grows with the rectangle count.
- **Capping rectangles per command buffer:** it avoids the crash and keeps the per-rectangle BLORP cost.

## Design

Reuse first. Core text already draws instanced quads from a buffer pinned in the frame, and `logic_fill`
already has a solid-colour fragment shader, its push-constant layout and a per-format pipeline cache.

- **Recording.** `fill_rect_batch` uploads the clamped, non-empty rectangles to the frame's upload arena with
  `upload_to_frame`, as core text does for its glyph instances. Each instance holds the rectangle's origin and
  size in pixels. `RecordedFillRect` carries the pin and the instance count instead of the rectangle list. The
  local clamped list is used for upload and damage and then dropped. Frame tracing reports the instance count.
- **Emit.** `emit_fill_draws` replaces `vkCmdClearAttachments` with these steps:
  - bind the solid-fill pipeline;
  - set the full-area scissor;
  - push the colour;
  - bind the instance buffer at the pin's buffer **and offset**;
  - issue `vkCmdDraw(4, N, 0, 0)`.

  The pin is resolved from the pin set of the frame being replayed. That set is passed through all three
  callers: the standalone fill and both session branches. The viewport is not set again. Every caller already
  sets the destination's viewport when it opens the pass, and an op that continues a session draws to the same
  destination. Pipeline, scissor, push constants and vertex buffer are set on every fill, because the op
  before it in a session may have changed them.
- **Shaders.** A new vertex shader reads each instance's origin and size from instance-rate attributes, as
  `text.vert` does. It builds the quad from `gl_VertexIndex` and maps pixels to NDC as `logic_fill.vert` does.
  The fragment stage is `logic_fill.frag`, unchanged. Both stages use the existing `LogicFillPushConsts` layout:
  48 bytes, viewport at byte 16, colour at byte 32, the same vertex and fragment push range. The fill pushes
  viewport and colour in that layout. `build.rs` already compiles every shader under `shaders/`.
- **Pipeline.** The pipeline is built lazily and cached per destination format, as `logic_fill_caches` is. Its
  state is:
  - colour attachment 0, one sample, a filled triangle strip;
  - no culling, depth or stencil;
  - no logic op and no blending, with the RGBA write mask.

  It writes exactly what the clear wrote: the R8 byte in red, and alpha as recorded. It must not use the
  `logicOp` feature, the RGB-only mask of the non-copy logic fills, or the text and composite alpha handling.
  The production formats on this path are BGRA8 and R8. The format is kept as the key, and no new format
  support is added.
- **Pass state is unchanged.** The barriers, the dynamic-rendering begin with LOAD/STORE, and the closing
  transition stay as they are. Pixels outside the rectangles keep their contents.
- **Exactness.** The rectangles are pixel-aligned and clamped, and the viewport covers the destination, so the
  quad covers exactly the pixels the clear covered. Overlapping instances write the same colour.

Unchanged:

- every `logic_fill` case, including depth-1 GXcopy and the non-copy functions;
- the CPU fallbacks for partial plane masks;
- the scene compositor's background clear (`scene/compose.rs`);
- `kms/vk/ops/fill.rs`, which has no production caller.

## Non-goals

- Instancing `logic_fill` or any other per-rectangle path.
- A per-driver switch, or an option to restore the old path. No new `YSERVER_*` variable.
- Changing how frames open and close, or bounding command-buffer size in general. Separate fill ops still each
  add their own fixed set of commands.
- Supporting new destination formats.
- A Mesa change. hasvk's unchecked bucket index is noted as a finding only.

## Testing

Backend tests on lavapipe, through the real record and emit paths. The fixture must initialize and run: pin a
working software ICD and use the software-device opt-in and `--ignored`, as the CI lavapipe job does. A test that
returns early without a device is not a pass. Mutations are run after the implementation exists.

- **Observer.** A test-only observation, scoped to the engine under test (not the process-global call counters),
  records for each emitted `RecordedFillRect`:
  - the number of attachment clears it issued;
  - the instance count of its draw.

  It excludes setup and readback commands, and adds no production telemetry.
- **One draw per fill:** fills of N = 1 and N = 1000 post-clamp, non-empty rectangles each record zero clears and
  one draw with instance count N. Mutations:
  - one draw per rectangle;
  - the clear kept;
  - instance count 1.
- **Pixels:** the destination is odd-sized. The rectangles include:
  - one-pixel rectangles and spans;
  - adjacent and overlapping rectangles;
  - rectangles clipped at the edge.

  Sentinel pixels outside every rectangle are checked as well. The colour cases are:
  - a depth-32 colour with non-trivial alpha over a different known destination colour, compared on all four raw
    BGRA bytes;
  - an R8 destination with a red value that is neither 0 nor 255 and differs from the pushed alpha.

  The existing depth-24 alpha tests, the fill, span and core-text pixel tests, and `render_acceptance` all pass
  unchanged. Mutations:
  - a quad one pixel short (size − 1);
  - premultiplied source-over blending enabled.
- **Inside a session:** the session opens with a fold-clean textured composite or a logic fill that leaves a
  narrow scissor. Then two fills follow, with different colours and different instance slices. The fills are
  asserted to take `Continue`, and their pixels outside the predecessor's scissor are correct. Mutations: skip
  binding the pipeline, setting the scissor, binding the vertex buffer at the pin's offset, or pushing the
  colour.
- **Large fill:** one fill of 200 000 varied rectangles goes through the arena's dedicated-block path. It records
  one draw with instance count 200 000, and early and late instances read back correctly. Mutation: instance
  count 1.

**Performance (ask the user first):** x11perf `-rect1`, `-rect10`, `-rect100`, `-ftext` and `-fitext`, before and
after, on AMD and NVIDIA, reported as measured. Then a branch for the #155 reporter on Ivy Bridge, to check
both the crash and the core-font stutter.

## Provenance

- yserver master `68fa4be2`:
  - `engine/fill_copy.rs` (`fill_rect_batch`);
  - `engine/emit.rs` (`emit_fill_draws`, its three callers, pass open and close);
  - `engine/frame.rs` (session eligibility, replay pins);
  - `engine/text.rs`, `vk/text_pipeline.rs` and `vk/ops/text.rs` (instanced glyph quads);
  - `vk/logic_fill_pipeline.rs` and `shaders/logic_fill.{vert,frag}.glsl`;
  - `engine/staging.rs` (`upload_to_frame`);
  - `backend/portable/draw.rs` (fill routing).
- Mesa `mesa-26.2.6` (MIT), read for diagnosis only:
  - `src/intel/vulkan_hasvk/anv_blorp.c`;
  - `anv_batch_chain.c`;
  - `anv_allocator.c`;
  - `anv_private.h`;
  - `anv_device.c`;
  - `genX_cmd_buffer.c`.
- Issue #155: the reporter's gdb backtrace; analysis in `~/Projects/claude-scratch/issue-155/crash-analysis.md`.

This document describes behaviour in original prose; no third-party code was used.
