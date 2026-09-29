# RANDR CRTC transforms (fractional scaling)

> **Status: implementation-ready — reviewed by codex rounds 1–4 (2026-09-29),
> changes applied.**
> Issue #185.

## Problem

Cinnamon's fractional scaling (muffin `x11-randr-fractional-scaling`, both
`fractional-scale-mode`s) and `xrandr --scale` / `--transform` need
`RRSetCrtcTransform`. yserver answers `BadMatch` for any non-identity
transform (`process_request.rs`, `RR_SET_CRTC_TRANSFORM`) and reports
`hasTransforms = 0`, so:

- scale-down 125% shows the 2× UI unscaled (200%) and cannot return to 100%
  while fractional scaling is on;
- scale-up 125% disables both CRTCs, sets a screen smaller than the modes,
  and the re-enable fails: the display stays dark (jos had to zap).

## Measured

### What muffin sends (jos, silence, two 2560×1440 outputs, 2026-09-29)

Captured with a diag build logging every mutating RANDR request
(`diag/185-randr-log`, local). CRTC 4 at 0,0 and CRTC 6 at 2560,0, both
mode `0x13` (2560×1440). Order per change: `SetScreenSize`, then per CRTC
`SetCrtcTransform` + `SetCrtcConfig`.

| setting | SetScreenSize | CRTC 4 | CRTC 6 |
|---|---|---|---|
| scale-down 100% | 7680×2880, 1355×508 mm | identity, `fast` | 2.0, `good` |
| scale-down 125% | 6656×2304, 1084×375 mm | identity, `fast` | 1.599991, `good` |
| scale-down 150% | 5984×1926, 906×292 mm | identity, `fast` | 1.337494, `good` |
| scale-up 125% | 4608×1152, 750×188 mm | 0.5, `nearest` | 0.799988, `good` |

- The wire matrix is **the reciprocal of muffin's log text** ("Scaling CRTC 6
  at 0.625" is its own per-CRTC scale; the wire carries 1.6). All matrices
  are pure diagonal scale, no translation, `m33 = 1`.
- Filters are named `fast`, `good` or `nearest`, never `bilinear`: the Render
  aliases matter.
- Scale-up keeps CRTC 6 at x = 2560 although CRTC 4's footprint is only 1280
  wide: outputs are not packed, the screen can have holes.
- Scale-up disables both CRTCs (mode 0) before re-enabling them with
  transforms; each CRTC is configured while the other may be off.

### What Xorg does (`tools/vng-scenarios/xrandr-scale.sh`)

Xorg 21.1.24, modesetting, vng guest, one 1280×800 output, a client holding
the connection (Xorg resets and drops the transform when its last client
leaves):

| `--scale` | screen = CRTC = monitor | transform read back |
|---|---|---|
| 1.6 | 2048×1280 | 1.599991 |
| 0.625 | 800×500 | 0.625000 |
| 0.8 | 1024×640 | 0.799988 |
| 2 | 2560×1600 | 2.000000 |
| 0.5 | 640×400 | 0.500000 |
| 1.333333 | 1707×1067 | 1.333328 |

The filter reads back `bilinear`; monitor mm are unchanged by the transform.

## Xorg reference behaviour

All from `../xserver`, 21.1 branch.

- **Direction.** The matrix maps CRTC (scanout) pixels to framebuffer (root)
  pixels. The framebuffer footprint of a CRTC is its mode rectangle pushed
  through the matrix (`RRModeGetScanoutSize`, `rrcrtc.c:1030-1058`). `f_transform`
  additionally carries the CRTC's x/y translation (`rrtransform.c:280-286`).
- **Pending vs current.** `SetCrtcTransform` only stores the client's
  *pending* transform (`RRCrtcTransformSet`, `rrcrtc.c:1091-1128`). The next
  `SetCrtcConfig` applies it and counts a changed pending transform as a
  change even with identical mode/x/y (`rrcrtc.c:765`); `RRCrtcNotify`
  copies pending to current (`rrcrtc.c:219-230`).
- **Validation order** (`ProcRRSetCrtcTransform`, `rrcrtc.c:1755-1785`):
  BadCrtc → BadAccess (leased) → non-invertible matrix BadMatch → negative
  param count BadLength → CRTC without transform support BadValue → unknown
  filter BadName → filter parameter check BadMatch → params without a filter
  BadMatch.
- **Filters** are Render's (`PictureSetDefaultFilters`, `render/filter.c:245-265`):
  `nearest`, `bilinear`, `convolution`, aliases `fast` → nearest,
  `good`/`best` → bilinear. GetCrtcTransform returns the canonical
  name.
- **Geometry.** GetCrtcInfo width/height and GetMonitors use the transformed
  footprint (`rrcrtc.c:1212`, `rrmonitor.c:71`). CrtcChangeNotify keeps the
  **mode** size (`rrcrtc.c:249-250`). SetCrtcConfig skips its screen-bounds
  check when the CRTC supports transforms (`rrcrtc.c:1436`); SetScreenSize
  checks each CRTC's transformed box (`rrscreen.c:271-279`).
- **Scanout.** The modesetting driver renders a transformed CRTC through a
  shadow: the framebuffer is composited into a per-CRTC shadow pixmap
  through the transform, damage-limited and widened by the filter size
  (`xf86Rotate.c:90-212`). Page flipping is off while any CRTC has a shadow
  (`modesetting/present.c:266`).
- **Cursor.** Hardware cursor is refused while any enabled CRTC has a
  transform (`xf86Cursors.c:569`): the software cursor is drawn in
  framebuffer space and scaled with the content.
- **Pointer** coordinates stay in framebuffer space, confined to the CRTCs'
  transformed bounds (`RRConstrainCursorHarder`, `rrcrtc.c:275-296`).
- **Root GetImage** reads the framebuffer, not the scanout: its content is
  untransformed.

## Design

### D1 — Protocol and state

- Per CRTC: `pending` and `current` transforms, each = 16.16 matrix as
  received, derived `f64` forward and inverse, canonical filter name
  (optional), filter params. Default identity with **no** filter, as Xorg's
  `RRTransformInit` (`rrtransform.c:27-36`); GetCrtcTransform then returns
  zero filter bytes (`rrcrtc.c:1809`). `nearest` is only the renderer's
  sampler fallback, never a protocol value the client did not send.
- `SetCrtcTransform`: Xorg's validation order above, storing `pending`.
  Big- and little-endian bodies.
- `SetCrtcConfig`: applies `pending` → `current`; a differing `pending` makes
  an otherwise identical config a real change (reconfigure + notifies).
- `GetCrtcTransform`: the full reply (pending and current, names, params).
- `hasTransforms = 1`. The branch merges only once D4–D6 render correctly.

### D2 — Accepted forms (phase 1 contract)

- **Matrices:** pure scale only: `m11 > 0`, `m22 > 0`, `m33 = 1`, every
  other element 0. Translation, rotation, shear, reflection and projective
  matrices → `BadMatch` on purpose (translation would need its own source
  origin, clipping, damage, cursor and readback rules; nothing measured sends
  it). Xorg accepts these; the divergence is documented in `docs/status.md`.
  CRTC rotation and reflection (SetCrtcConfig's `rotation`) are accepted and
  composed with this matrix: see the addendum.
- **Filters:** `nearest`, `bilinear`, `fast`, `good`, `best` render as named.
  `convolution` → `BadMatch` on purpose rather than
  being silently drawn as bilinear.
- Everything muffin and `xrandr --scale` send is inside this contract.

### D3 — Geometry: one footprint

- `RandrOutput::footprint()` = `pixman_transform_bounds` of the mode box
  `(0, 0, w, h)` through the **fixed-point** `current` matrix, as Xorg's
  `RRModeGetScanoutSize` (`rrcrtc.c:1028-1048`); goldens: 1280 × 1.333328 →
  1707, 1280 × 1.599991 → 2048. Never a rounded float scale.
- **Two extents, kept separate.** A client may set the root smaller than a
  footprint (Q3; Cinnamon does so transiently while changing scale):
  - the **footprint** serves CRTC and monitor geometry: `crtc_info`,
    `active_monitors` (GetMonitors, XINERAMA), Present's CRTC selection,
    output composition (D4) and nearest-CRTC confinement (D5b);
  - the **root extent** stays exactly what `RRSetScreenSize` set: root
    storage and protocol geometry, and the input thread's rectangular safety
    clamp. The footprint-based output bbox derives the root extent only where
    it does today — before any client logical size (startup, hotplug
    recompute) — and feeds the "outputs caught up" notification check
    (`run.rs:2819-2839`). Once a client has set a logical size, that override
    survives later SetCrtcConfig calls, as it already does
    (`randr.rs:1209`, `an_explicit_client_logical_size_overrides_a_reserved_slot`);
    a transformed bbox larger than a cropped root never resizes root storage.
  The KMS scanout images stay mode-sized. CrtcChangeNotify keeps the mode
  size.
- SetCrtcConfig skips `screen_encompasses` when transforms are supported, as
  Xorg does. This is what un-darkens the scale-up sequence.
- SetScreenSize's crop check keeps using the **untransformed** box: BadMatch
  iff `width < crtc.x + mode.width` or `height < crtc.y + mode.height`, for
  every enabled CRTC, transformed or not (measured at the exact boundaries,
  Q3). A screen may therefore crop a scaled CRTC's footprint.
- Pointer confinement follows what Xorg 21.1 actually runs (`RRPointerMoved`
  exists but has no caller): `RRConstrainCursorHarder` on every move and warp
  (`randr.c:356`, `rrcrtc.c:1942`) keeps a move that would leave every CRTC on
  the CRTC it came from, only when the CRTCs touch — in a non-touching layout
  (Cinnamon scale-up) the pointer can enter the hole; after a layout change,
  `RRPointerScreenConfigured` (`randr.c:665`) moves a pointer outside every
  CRTC to the nearest one. This also applies to identity layouts with holes.
- The scene composites a transformed output into an **intermediate image**
  allocated at the full footprint size, in root space, origin = the CRTC's
  (x, y): the existing walk, damage and buffer-age work unchanged, with the
  output's layout rect = the footprint.
- A **scale pass** then draws the whole mode-sized scanout image, sampling the
  intermediate through `current` (scanout pixel → root pixel, i.e. the wire
  matrix, minus the CRTC offset), with a nearest or linear sampler. A full
  pass every frame the output repaints; no damage transform in phase 1.
- **Outside the root extent.** Only footprint ∩ root is composited; the rest
  of the intermediate is explicitly cleared to transparent black whenever the
  root or the footprint changes. The pass samples the whole intermediate with
  clamp-to-edge (in bounds by construction), so the UV origin is fixed and the
  bilinear edge at the root boundary blends toward that black, matching
  Xorg's shadow pass, where every sample outside the root reads transparent
  black and is written as black: `PictOpSrc` from a source picture on the screen drawable with the
  default `repeat = None` (`xf86Rotate.c:59-89`). Bilinear filtering at the
  root edge blends toward black, as pixman does.
- Identity outputs keep today's path: no intermediate, no pass, no cost.
- Intermediate lifetime: allocated when a transform becomes current, freed
  when it goes back to identity or the output is disabled; accounted under
  its own `vram by use` bucket (#169).

### D5 — Direct scanout, cursor

- Direct scanout (client buffer flipped to the CRTC) is off while **any**
  CRTC has a non-identity current transform, as Xorg. Entering a transform
  while flipped goes through the normal unflip.
- The cursor is forced to the software path while any CRTC is transformed,
  and then on **every** output, identity ones included, so it does not vanish
  when crossing from a transformed output to an untransformed one. On a
  transformed output it is drawn into the intermediate at root coordinates and
  scales with the content.

### D5b — Input and cursor mapping

- The pointer lives in root (framebuffer) space, as on Xorg. Relative motion
  is applied in root space, unscaled: on a 2.0 output it covers half the
  physical distance per device unit, as Xorg (measured, Q6).
- Absolute devices (tablets, touch) map over the **whole root extent**, as
  Xorg's default does without a Coordinate Transformation Matrix
  (`input_thread.rs`, fed the root extent). Per-output mapping, and with it
  any scanout ↔ root mapping on the input side, is out of scope. The scale
  pass is the only consumer of `M` outside geometry, and a software cursor
  drawn in root space needs no inverse.
- **Where confinement runs.** The input thread only knows a rectangular
  root extent (`input_thread.rs:91`) and keeps clamping to it as a safety
  bound. Nearest-footprint confinement runs in the backend/core, where the
  existing pointer clamp lives (`backend.rs:13910`), **before** the cursor
  moves or any motion/crossing event is delivered, so no event carries an
  unreachable root position. After a hole clamp, and after a RANDR
  reconfiguration relocates the pointer, the backend resyncs the input thread
  with `push_position`.
- Warps (WarpPointer, XIWarpPointer, XTEST) set a root position and go
  through the same confinement.

### D6 — Root GetImage and screenshots

- Root GetImage / ShmGetImage returns **framebuffer-space** content for the
  whole requested rect, as Xorg: for each output the request overlaps, the
  overlapping part comes from the intermediate if the output is transformed,
  else from its scanout image; the rest as today (zero-filled holes). A
  request crossing a transformed and an untransformed output is assembled
  from both.
- The scanout dump keeps dumping scanout images (what the monitor shows).

## Phases

One feature branch, squashed on merge; merged only when all phases work.

1. **Protocol, state, geometry** (D1–D3), unit tests against the goldens.
2. **Rendering, cursor, input, readback** (D4–D6).
3. `docs/status.md`, vng runs, hardware smoke in both Cinnamon modes on
   silence's two outputs.

## Invariants

- An identity transform renders exactly as today, on the same path.
- All protocol-visible sizes of a CRTC come from `footprint()`, except
  CrtcChangeNotify (mode size).
- A root-space pixel read back by GetImage is the pixel the scene composited,
  never a scaled scanout pixel.
- No non-identity transform is ever flipped directly to a CRTC.

## Test plan

- Protocol: request/reply encoding both byte orders; validation order; the
  pending/current split and SetCrtcConfig re-apply; GetCrtcTransform
  canonical filter names. Goldens from Xorg modesetting in vng (extend
  `xrandr-scale.sh` with `xrandr --verbose` readback and error cases; Xvfb
  cannot serve, it has no transform support and answers BadValue).
- Geometry: footprint rounding for 1.6, 1.333, 0.8, 2.0, 0.5 against the
  table above; muffin's four captured sequences replayed as unit tests end to
  end (screen size, CRTC info, monitors after each step), including the
  scale-up sequence that currently goes dark.
- Pixels: a lavapipe test scaling a known pattern through the pass (nearest
  exact, bilinear within tolerance), including a root smaller than the
  footprint (the cropped part black, the edge blended toward black). Xorg's
  scanout cannot be dumped in vng, so that crop golden comes from the source
  rule above plus yserver's own scanout dump, and root GetImage (which only
  covers the root) is A/B'd against Xorg; vng: root GetImage A/B against Xorg
  after `xrandr --scale` (framebuffer space, should be identical); yserver's
  scanout dump vs root GetImage scaled on the CPU.
- Hardware: Cinnamon scale-down 100/125/150% and scale-up 125% on silence,
  cursor, direct-scanout apps (video, a game) under a transform, then back to
  identity.

## Open questions

- **Q1** *Settled:* `pixman_transform_bounds` over the fixed-point
  `crtc->transform` (`rrcrtc.c:1028-1048`); D3 names it.
- **Q2** *Settled (corrected in phase 2):* `RRConstrainCursorHarder` per
  move/warp plus `RRPointerScreenConfigured` after a layout change, not
  `RRPointerMoved` (no caller in 21.1); see D3.
- **Q3** *Settled by measurement* (`tools/vng-scenarios/xrandr-scale-crop.sh`,
  two outputs, B at x = 1920, 1920×1440, `--scale 2x2`, footprint to x = 5760
  and y = 2880): Xorg applies 5759, 3841 and **3840** wide, rejects **3839**;
  applies 1441 and **1440** high, rejects **1439** (width and height
  independently); the identity control has the same 3840 / 1440 thresholds. The check is the untransformed
  `crtc.x + mode.width` box, not the footprint and not the doubled offset the
  source reading (`rrscreen.c:266-281` through the translating `f_transform`)
  predicts. Why the source reads differently is untraced; the measured rule is
  what D3 adopts.
- **Q4** *Settled:* scanout images are `COLOR_ATTACHMENT | TRANSFER_SRC |
  TRANSFER_DST` (`scanout.rs:5801-5811`, pinned by
  `scanout_usage_matches_render_and_readback_paths`), so the scale pass renders
  into them as a colour attachment; they are deliberately not `SAMPLED`, which
  the pass does not need. The intermediate is `SAMPLED | COLOR_ATTACHMENT`.
- **Q5** VRAM: the transformed CRTC of the scale-down 100% capture needs a
  5120×2880 intermediate, 56.25 MiB at 32 bpp, on top of its scanout images;
  identity CRTCs need none. Acceptable, or allocate lazily on first repaint?

- **Q6** *Settled by measurement* (`tools/vng-scenarios/pointer-scale-host.sh`,
  QEMU PS/2 mouse, 4 × `mouse_move 25 10`): Xorg moves the root pointer
  +53,+21 at identity and +56,+22 under `--scale 2x2` — the same within
  acceleration noise, so relative motion is not scaled by the transform.
  yserver's identity phase: +53,+21. `xdotool mousemove_relative` remains a
  warp/confinement test only.

## Do not

- Do not merge before D4–D6 render correctly.
- Do not render accepted-but-unsupported matrices or filters approximately:
  reject them.
- Do not derive the footprint from a rounded float scale.
- Do not transform output damage in phase 1; repaint the pass fully.

## Addendum — rotation and reflection

`xrandr --rotate left|right|inverted|normal` and `--reflect x|y|xy` reuse
D3–D6 unchanged; only the matrix they are fed changes.

- **Accepted.** Every CRTC advertises modesetting's `rotations = 0x3f`
  (`RR_Rotate_0/90/180/270 | RR_Reflect_X/Y`, `xf86Crtc.c:832`, measured).
  SetCrtcConfig checks `(~rotations) & rotation` → BadMatch
  (`rrcrtc.c:1403`); a changed rotation is a change with identical
  mode/x/y (`rrcrtc.c:749`); a disable keeps the rotation, as
  `xf86RandR12CrtcSet` does. D2 is unchanged for the *client* matrix (pure
  scale only): rotation arrives through SetCrtcConfig, not SetCrtcTransform.
- **One matrix.** `crtc_matrix` (`randr/transform.rs`) is
  `RRTransformCompute` (`rrtransform.c:137-270`) in pixman fixed point, less
  the CRTC x/y: `M = C · T_refl · S_refl · T_rot · R`, with the translations
  that keep the image in the positive quadrant. `RandrOutput::crtc_transform`
  feeds it to the footprint (GetCrtcInfo, GetMonitors, XINERAMA, Present,
  confinement), the intermediate size, the scale pass, the SW cursor and root
  GetImage. GetCrtcTransform still returns the client matrix only (measured:
  identity under `--rotate left`, 2.0 under `--rotate left --scale 2x2`).
  A combination whose fixed-point multiply overflows (Xorg's rescaled
  projective fallback) is BadMatch.
- **Scale pass.** Push constants carry the two affine rows; nearest is
  pixman's exact centre sample `(a + b + 1) >> 1` rounding, split into
  whole/fraction words so no product needs 64 bits. Every rotation and
  reflection, and left × 2.0, is pixel-exact against pixman (lavapipe,
  `crtc_transform_tests.rs`). A pure rotation is nearest unless the client
  set a filter (Xorg's default picture filter).
- **Geometry, measured** (`tools/vng-scenarios/xrandr-rotate.sh`, Xorg
  21.1.24, 1280×800): left/right → CRTC = monitor = screen 800×1280;
  inverted and reflections keep 1280×800; left + scale 2 → 1600×2560.
  Monitor mm stay unrotated (325/203). With two outputs, the right one
  rotated left at x = 1920 is 1440×1920 there, screen 3360×1920.
  CrtcChangeNotify carries the rotation with the **mode** size; an
  OutputChangeNotify without a CRTC carries `RR_Rotate_0`; ScreenChangeNotify
  carries `crtcs[0]`'s rotation with pixels and mm swapped for 90/270
  (`rrscreen.c:95-121`, measured).
- **RANDR 1.0** (`tools/vng-scenarios/xrandr-orientation.sh`): GetScreenInfo
  is `RR10GetData` over `RRFirstOutput` (mode sizes unswapped while rotated,
  0x3f, rates for ≥ 1.1 clients). SetScreenConfig (`xrandr -o`) is
  `ProcRRSetScreenConfig` applied through the SetCrtcConfig path at 0,0;
  measured on Xorg: `-o left` gives screen and CRTC 800×1280, mm unchanged;
  statuses 1 (stale config time) and 2 (old time), BadValue for size, rate
  and rotation 3, BadMatch for 0x41, BadLength for a 1.0-sized request from
  a 1.5 client, and every success moves the timestamp.
- **SetScreenSize crop** uses the mode box **swapped** for 90/270, still
  never scaled (`rrscreen.c:271-279`, measured: rotated 1280×800 accepts
  800×1280, rejects 799×1280, 800×1279 and 1280×800; left × 2.0 accepts
  1599×2560 and 2560×1600 against a 1600×2560 footprint).
- **Pointer**: relative motion stays in root space, unrotated (measured,
  `pointer-rotate-host.sh`: +53..56, +21..22 for 4 × `mouse_move 25 10` in
  every rotation).
- **Cursor divergence.** Xorg keeps the hardware cursor for a rotation
  without a client transform (`transformPresent` is only set by one,
  `xf86Crtc.c:311`, `xf86Cursors.c:569`) and rotates its image; yserver uses
  the software cursor, as for any transform. Same picture, no protocol
  difference.
