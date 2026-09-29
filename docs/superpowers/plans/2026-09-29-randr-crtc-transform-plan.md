# RANDR CRTC transforms — implementation plan

Spec: [`../specs/2026-09-29-randr-crtc-transform-design.md`](../specs/2026-09-29-randr-crtc-transform-design.md)
(D1–D6, Q1–Q6). Issue #185. Branch `feat/185-crtc-transform`.

Gates for every task: `cargo +nightly fmt`, `cargo clippy --all-targets -- -D warnings`,
`cargo test --workspace` (debug; the 3 `should_panic` tests fail only under
`--release`). Render tasks also: the lavapipe `--ignored` render tests
(`VK_DRIVER_FILES=/usr/share/vulkan/icd.d/lvp_icd.json`). Goldens only from the
measured tables in the spec or new Xorg vng captures, never invented.

## Transform support modes

A server-wide `TransformSupport` decides SetCrtcTransform admission:

- **`Legacy`** — production until task 13, `hasTransforms = 0`. Keeps today's
  answers exactly (`process_request.rs:3302-3345`): only today's structural
  parse (BadLength) and CRTC lookup (BadCrtc), then an **identity** request
  is a `Success` **no-op regardless of its filter payload** (an unknown filter
  or a parameter-only tail still succeeds, as today), and any non-identity
  request is `BadMatch`. No pending/current storage; GetCrtcTransform keeps
  returning identity with no filter. No Xorg filter or invertibility
  validation runs in this mode. muffin sends identity transforms with
  `hasTransforms = 0` (captured), and those must keep succeeding.
- **`Unsupported`** — test-only: Xorg's no-transform-support state. Runs
  Xorg's order up to and including the transform-support check
  (`rrcrtc.c:1755-1785`): BadCrtc, (leases n/a), non-invertible BadMatch,
  negative-param BadLength, **then** `BadValue` (`rrcrtc.c:1101-1102`), for
  identity requests too. Exists only to cover that branch.
- **`Supported`** — task 13 onwards: D1/D2 in full, `hasTransforms = 1`.

Task 13 removes `Legacy` and switches production to `Supported` in one
commit, so no intermediate commit changes what clients see.

## Coordinate spaces (used by tasks 8, 11, 12)

One equation, split between two types and reused everywhere:

- `d` = a **mode-local scanout pixel** of the CRTC, `(0,0)` at its top-left.
- `M` = `current` as received (no translation, D2).
- **Intermediate-local** sample for `d`: `i = M · d`. The intermediate's
  `(0,0)` is the CRTC's root origin, so this is also the footprint-local
  position.
- **Root** position: `r = i + (crtc.x, crtc.y)`. The CRTC origin is added only
  when going to root space (scene, input, GetImage) — never inside the scale
  shader, which works purely intermediate-local.
- Inverse (cursor placement, hit-testing, absolute devices):
  `d = M⁻¹ · (r − crtc_origin)`.
- Ownership: `CrtcTransform` knows no origin and provides only
  `scanout_to_intermediate(d) = M · d` and `intermediate_to_scanout(i) =
  M⁻¹ · i`. `RandrOutput`, which owns (x, y), provides `scanout_to_root` /
  `root_to_scanout` by adding / subtracting its origin around those.
- GetImage source rect in the intermediate:
  `(requested_root ∩ footprint_root) − crtc_origin`.

## Phase 1 — protocol, state, geometry (D1–D3)

### 1. Transform type and footprint
- New `crates/yserver-core/src/randr/transform.rs` (or a module in
  `randr.rs`): `CrtcTransform { matrix: [i32; 9] /* 16.16 as received */,
  forward: [f64; 9], inverse: [f64; 9], filter: Option<Filter>, params:
  Vec<i32> }`, `Filter::{Nearest, Bilinear}` from the names `nearest`/`fast`
  and `bilinear`/`good`/`best`, canonical name for readback.
- `is_pure_scale()` per D2; `invertible()`.
- `footprint(mode_w, mode_h) -> (u16, u16)`: a port of
  `pixman_transform_bounds` over the fixed matrix (Q1).
- Tests: the Xorg goldens 1280×800 × {1.599991 → 2048×1280, 0.625 → 800×500,
  0.799988 → 1024×640, 2 → 2560×1600, 0.5 → 640×400, 1.333328 → 1707×1067}
  (the 16.16 values as xrandr sent them — record the raw words from a vng
  xtrace of `xrandr --scale` if not already known); muffin's 2560×1440 ×
  {2.0, 1.599991, 1.337494, 0.5, 0.799988} → footprints consistent with the
  captured SetScreenSize sizes (7680×2880 etc.).

### 2. Per-CRTC pending/current state
- `RandrOutput` gains `pending: CrtcTransform`, `current: CrtcTransform`
  (default identity, `filter: None`, per `RRTransformInit`).
- `RandrOutput::footprint()` = mode through `current`; identity = mode size.
- Tests: default readback empty filter; footprint of identity = mode.

### 3. Wire: SetCrtcTransform parse, GetCrtcTransform encode
- `crates/yserver-protocol/src/x11/randr.rs`: parse the full request
  (matrix, filter name, params) in **both byte orders** (today LE-only,
  `randr.rs:486-505`); encode the 96-byte reply + pending/current name and
  params.
- Tests: byte-level both orders; the reply for default state (zero filter
  bytes) and for a pending ≠ current state.

### 4. SetCrtcTransform validation and storage
- In `Supported` (and `Unsupported`, up to its BadValue) mode: Xorg's order
  (spec, "Validation order"), storing `pending`. Filter validation (BadName,
  parameter checks, params without filter) runs only in these modes, never in
  `Legacy`. The BadMatch
  for non-pure-scale (D2) and, until task 13, for every non-identity matrix,
  come **after** Xorg's own checks so error codes match Xorg where both
  reject.
- Tests, one per branch of the spec's validation order, in order:
  BadCrtc; BadAccess for a leased CRTC — **not applicable**, yserver has no
  RANDR leases (assert and document); non-invertible matrix BadMatch;
  a request whose length leaves a negative parameter count → BadLength;
  no transform support → BadValue (the test-only `Unsupported` mode); unknown
  filter BadName; filter parameter validation (`convolution`, which D2 then
  rejects with BadMatch; `nearest`/`bilinear` have no validator, so Xorg
  **accepts** parameters for them, stores and echoes them in
  GetCrtcTransform (`rrtransform.c:66-87`) — do the same, rendering ignores
  them); params without a filter BadMatch; then D2's own BadMatch for
  non-pure-scale.

### 5. SetCrtcConfig applies pending → current
- In the SetCrtcConfig path (`process_request.rs`, `begin_crtc_config`): a
  differing `pending` makes an otherwise identical config a real change;
  on success copy pending → current.
- Skip `screen_encompasses` only in `Supported` mode (task 13), the same
  switch that sets `hasTransforms`.
- Tests: identical mode/x/y + new pending → reconfigures and notifies;
  CrtcChangeNotify carries the mode size.

### 6. Geometry consumers → footprint; root extent untouched
- Switch to `footprint()`: `crtc_info` (`randr.rs:798`), `active_monitors`
  (GetMonitors, XINERAMA), Present's CRTC selection
  (`process_request.rs` ~10995), `enabled_output_bbox` (`run.rs:2841`, derived
  extent + caught-up check only).
- SetScreenSize crop check (`screen_size_would_crop`, `randr.rs:580`): keep
  the untransformed `x + mode.w`, `y + mode.h` rule (Q3), and add the exact
  boundary goldens from `xrandr-scale-crop.sh` (3840 ok / 3839 BadMatch,
  1440 ok / 1439 BadMatch, scaled and identity).
- Client logical-size override survives SetCrtcConfig (existing test stays
  green; add one with a transformed bbox larger than a cropped root).
- Replay tests: muffin's four captured sequences (spec, "What muffin sends")
  as unit tests driving the request handlers with transforms force-enabled in
  the test: screen size, GetCrtcInfo, GetMonitors after every step,
  including the scale-up sequence (both CRTCs off → 4608×1152 → re-enable).

## Phase 2 — rendering, cursor, input, readback (D4–D6)

### 7. Intermediate image per transformed output
- In the KMS backend (`kms/render/scene.rs` output layout, `backend.rs`
  output bookkeeping): when an output's `current` is non-identity, allocate
  a footprint-sized intermediate (`SAMPLED | COLOR_ATTACHMENT`, the scene's
  render format), own `vram by use` bucket; free on identity/disable.
- The scene walk for that output targets the intermediate with the layout
  rect = footprint at (crtc.x, crtc.y); composite only footprint ∩ root;
  clear the rest to transparent black when root or footprint changes.
- Tests (lavapipe): allocation/free lifecycle; a root smaller than the
  footprint leaves the cropped part black.

### 8. Scale pass into the scanout image
- One full-screen draw per repaint of a transformed output: scanout image as
  colour attachment (it has `COLOR_ATTACHMENT`, Q4); each mode-local pixel `d`
  samples the intermediate at `i = M · d` (see "Coordinate spaces") — **no
  origin participates in the shader**; nearest or linear sampler,
  clamp-to-edge. New pipeline next to the existing composite pipelines.
- Both scanout routes: shared pool and the copied (PRIME) route.
- Tests (lavapipe): a known pattern at 2.0 / 0.5 nearest is exact; 1.6 / 0.8
  bilinear within tolerance; the cropped edge blends toward black; **a
  transformed right-hand output at non-zero x** (Cinnamon's layout: identity
  at 0,0, scaled at 2560,0) shows its own content, not black or shifted by
  the origin.

### 9. Direct scanout off while transformed
- `m1_gate_open` / `direct_scanout_topology_eligible` (`backend.rs` ~3528,
  ~2711): closed while any CRTC has a non-identity current transform;
  entering a transform while flipped unflips first.
- Tests: gate closed with a transform on another output too.

### 10. Software cursor on every output
- Force `OutputCursorMode::Sw` on **all** outputs while any is transformed;
  back to HW when none is. On a transformed output the cursor is drawn into
  the intermediate at root coordinates.
- Tests: mode switches with the transform set; cursor pixels present on both
  outputs of a mixed layout (lavapipe).

### 11. Input: confinement and mapping (D5b)
- Backend pointer clamp (`backend.rs` ~13910): nearest-footprint confinement
  before cursor update and event delivery; `push_position` resync after a
  hole clamp and after RANDR relocates the pointer. Input thread keeps its
  rectangular root clamp (`input_thread.rs:91`), fed from the root extent.
- `RandrOutput::scanout_to_root` / `root_to_scanout` (built on
  `CrtcTransform`'s origin-free helpers) for absolute devices and cursor
  placement.
- Warps (WarpPointer, XIWarpPointer, XTEST) through the same confinement.
- Tests: pointer in the scale-up layout's hole moves to the nearest CRTC;
  warp into the hole likewise; relative motion unscaled (Q6); a **round trip
  on a non-zero-origin output** (scaled CRTC at 2560,0): `root_to_scanout`
  then `scanout_to_root` returns the root position, and a known root position
  maps to the expected mode-local pixel.

### 12. Root GetImage in framebuffer space
- `read_root_scanout_assembled` (`backend.rs` ~7516): per output overlap, read
  the intermediate for transformed outputs, the scanout image otherwise; a
  request crossing both is assembled from both; holes zero-filled as today.
- Tests: GetImage across a transformed + identity pair returns root-space
  pixels, with the transformed output on the **right at non-zero x**; vng A/B
  against Xorg after `xrandr --scale` (root content only).

## Phase 3 — advertise

### 13. Enable
- One commit: remove `Legacy`, production `Supported` — accept D2's forms,
  `hasTransforms = 1`, the `screen_encompasses` skip (task 5).
- `docs/status.md`: partial RANDR transform support (pure scale,
  nearest/bilinear; rotation/translation/convolution BadMatch on purpose).
- vng: `xrandr --scale` 1.6 / 0.8 / 2 against the Xorg goldens
  (`xrandr-scale.sh`, run on yserver), `pointer-scale-host.sh yserver` now
  including the scale2 phase.

### 14. Hardware smoke (jos)
- silence, two 2560×1440 outputs, Cinnamon: scale-down 100/125/150%,
  scale-up 125%, back to fractional off; cursor across both outputs; a video
  or game (direct scanout) before, during and after a transform.

## Risks to watch

- Task 8 on the copied (PRIME) route and on scanout formats/modifiers that
  the render path hasn't used as attachments before.
- Task 9: transform toggles while flipped — known unflip / stale-frame bugs
  live there.
- Task 10: SW↔HW cursor switch glitches on the untransformed output.
- VRAM (Q5): 56.25 MiB for a 5120×2880 intermediate; allocate on first
  repaint if it matters.
