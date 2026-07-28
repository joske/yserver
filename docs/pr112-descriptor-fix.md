# PR #112: exact SHAPE clipping and composite descriptor capacity

This branch is rebased on upstream `master` at
`64d4b6e400f99ac51eb94f5705df2a3b449ec7bc`. It fixes the demonstrated capacity
and partial-frame failure paths. The RX580 crash reported on the original
July PR has not been reproduced; this change is not hardware qualification.

## Implementation

- Preserve exact SHAPE Bounding regions for descendants. The current upstream
  visibility walker uses capped regions for conservative occlusion/damage;
  those can become bounding boxes. A separate exact ancestor constraint clips
  each node's placement before emission, in both optimized and reference
  visibility modes. It derives from the parent's inner region (`winSize`),
  retaining upstream's border and Clip-shape semantics. No rectangle-count
  fallback paints mask holes.
- Normalize shapes once at the backend boundary. Nested intersections use the
  original PR's canonical YX-band sweep; inner Bounding/Clip intersections now
  preserve the same representation.
- Treat 1024 descriptor sets as initial capacity. A free pool slot grows to
  the next power of two covering the frame's draw count. Held slots are never
  resized/reset. Replacement is created before destroying the retired pool;
  growth failure preserves its old handle/capacity and releases the reservation.
  A failed reset marks the slot for replacement instead of reusing occupied
  capacity. Per-frame ownership and retirement remain unchanged.
- Allocate every descriptor for a frame in a single Vulkan batch. Any error
  propagates before recording or queue submission. The recorder also rejects
  mismatched draw/descriptor lengths instead of rendering a prefix. Existing
  submit-error handling retains damage and releases unsubmitted resources.
- Remove an already-unfulfilled `dead_code` expectation in `scene_diff.rs` so
  the rebased code passes the repository's exact Clippy gate. No lint is disabled.

## Software evidence

The focused tests explicitly select CPU Vulkan (Lavapipe) and fail rather than
silently skip if initialization fails or the selected device is not a CPU.

- Full-frame 5120x1440 COW mask: same draw list with/without the single rectangle
  on two simulated output positions, under both visibility modes.
- Nested 65x65 stripes: 4225 child pieces, checked pixel by pixel, exceeding both
  the 32-rectangle occlusion cap and the old 1024-descriptor capacity. The test
  fails when exact ancestor clipping is temporarily removed (negative control).
- Production Vulkan descriptor preparation and command recording: offscreen
  readback of two outputs over three pool-reuse rounds checks every pixel,
  including the final pieces after descriptor 1024. Holes retain background;
  covered pixels receive exactly one premultiplied SrcOver blend.
- Allocation fault injection replaces only `vkAllocateDescriptorSets` in a
  test device's Ash dispatch table. The actual `record_and_submit_render`
  path returns `ERROR_OUT_OF_POOL_MEMORY` with `gpu_submitted == false` at
  draw counts 1, 1023, 1024, 1025, 4225 and 4290. There are no production
  fault-injection switches. Pure tests also cover host/device-memory errors,
  invalid partial-success vectors, and the zero-draw case.
- Pool tests check two independent rings, all three held slots, failed growth,
  successful retry, retained grown capacity, and count-conversion overflow.
- Khronos validation layers were loaded from a package extracted under `/tmp`;
  no system installation was required. Offscreen recorder and allocation-error
  tests produced no Vulkan validation errors. Lavapipe reports missing external
  semaphore FD/DRM modifier support, which those headless tests do not use.

## Completed checks (2026-09-09)

- `cargo +nightly fmt` and the format check passed.
- `cargo clippy --all-targets -- -D warnings` passed with Rust/Clippy 1.98.0,
  selected using `RUSTUP_TOOLCHAIN=1.98.0`. The default toolchain was unchanged.
- `cargo test --all-targets --locked`: 2871 passed, zero failures (Rust 1.98.0).
  The earlier `cargo test --workspace` run also passed on Rust 1.96.1.
- Seven focused `software` tests passed with Rust 1.98.0, Lavapipe and the
  Khronos validation layer. The logged pool-reset failures are deliberate
  fault injection; no Vulkan validation errors were reported.
- Pool-reset failure is also injected through the test device dispatch table;
  the following acquisition replaces the pool before any reuse.
- Default tests requiring Unix sockets ran outside the filesystem/network
  sandbox after its `bind` restriction was identified. Vulkan stayed on CPU.

Local evidence logs are `/tmp/yserver-pr112-tests-1.98.log`,
`/tmp/yserver-pr112-clippy-1.98.log`, `/tmp/yserver-pr112-validation-1.98.log`,
and `/tmp/yserver-pr112-negative-control.log`.

## Reproduction

With a CPU ICD and, optionally, the Khronos validation layer installed:

```sh
export VK_DRIVER_FILES=/usr/share/vulkan/icd.d/lvp_icd.json
cargo test -p yserver --lib software -- --include-ignored --nocapture
cargo test -p yserver --lib pr112_ -- --include-ignored --nocapture
cargo +nightly fmt
cargo clippy --all-targets -- -D warnings
cargo test --workspace
```

Hardware-dependent ignored tests remain excluded. These runs do not exercise
DRI3 external-memory import, kernel pageflip/fence handoff, or the reporter's
restored Plasma session on RADV/Polaris.
