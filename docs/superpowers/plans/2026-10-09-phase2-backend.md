# Phase 2.11a: split `kms/render/backend.rs` (production code)

Step 2.11a of `2026-10-08-source-layout-cleanup.md`. Manifest:
`tools/split/manifests/backend_2.toml` (+ `.paths`). Accepted by jos
2026-10-09 and executed (decisions at the end); sizes below are the final tree.

## Structure today (31,742 lines, tests already in `backend/tests/`)

- **Types (lines 1–2106):** `KmsBackend` (476-line struct), window geometry,
  scanout M0/M1/M2 state, RANDR connector registry/allocator, crtc-config
  probe jobs, gamma, export/clip caches. 13 small helper-type impls.
- **Inherent `impl KmsBackend`:** three blocks, 16,152 + 173 + 390 lines,
  ~390 methods (~100 of them `*_for_tests`).
- **`impl Backend for KmsBackend`:** one block, 9,928 lines, 229 methods.
- **Free fns:** ~95 (scanout read routes, dumps, glyph parsing, z-pixmap and
  clip helpers), ~3.5k lines.
- **Macros:** no `macro_rules!`. Uses `crate::vk_count!` (absolute, fine).
- **Logs:** 163 `warn!`, 110 `debug!`, 52 `info!`, 40 `error!`, 14 `trace!`
  (68 with `target:`). No `module_path!`/`line!`/`file!`/`#[track_caller]`
  defined here; 38 fns call scene's `#[track_caller]` damage-audit helpers
  (the phase-1a `[locations]` set; it applies unchanged).
- **Relative paths:** 32 `super::{engine,store,target,platform,
  probe_executor}::` and `super::super::vk::` sites (incl. 2 in the trait
  impl). One more in a doc comment (`super::super::backend::…`, left alone).

## Target tree (lines after `cargo +nightly fmt`)

All under `kms/render/backend/` (descendants: `kms::render::backend` log
filters still match). Module name `render` was refused by verify (it shadows
a name used in 6 fns) → `render_ops`.

```
backend.rs            1811  imports, all types (private fields stay visible to
                            every child), helper-type impls, mod tests,
                            crtc_transform_tests, shared_backing_move_pieces,
                            order_pieces_for_in_place_move (tests import them;
                            alone in paint_target the glob is unused in lib)
trait_impl.rs         9930  impl Backend for KmsBackend, whole (see below)
for_tests.rs          2038  *_for_tests / for_tests_* / test_* helpers
portable/mod.rs         21  globs: windows, render_ops, text, clip, draw, dump
portable/windows.rs   1719  leaf storage sync/relayout, background, border,
                            restack, register/allocate leaf, root bg tile
portable/draw.rs      1489  fills, tiled fill, CPU rop fallbacks, copy-area
                            scissors, z-pixmap/GetImage helpers
portable/render_ops.rs 1098 resolve picture, render_composite_inner, glyph
                            drop/premul, gradient/picture clip helpers
portable/clip.rs       915  current clip, clip-mask cache, subwindow mode
portable/pointer.rs    870  pointer hosts, crossings, motion/button paths
portable/paint_target.rs 819 resolve_*_paint_target, shared backing moves
portable/keyboard.rs   807  xkb cooking, floating keyboards, LEDs, keymaps
portable/inferiors.rs  774  IncludeInferiors fan-out, stroke inferiors, overlay
portable/text.rs       622  core text, glyph spans, parse_composite_glyph_items
portable/redirect.rs   620  backing seed/inferiors, store alloc/decref, lifetimes
portable/dump.rs       469  drawable/cursor PPM dumps
portable/devices.rs    399  host key, device/facet hold release (impl block #3)
portable/stats.rs      283  telemetry drains, traces, render gap log
kms/mod.rs              17  globs: scanout, readback, crtc_config, randr,
                            present, export, session (cursor: methods only)
kms/randr.rs          1943  connector registry/probes, providers, outputs,
                            rebuild, fire_randr_changes, virtual screen extent,
                            relight, display rescan, gamma, mode timing
kms/scanout.rs        1685  direct scanout/unflip/COW, M0/M1 eligibility+probe
kms/readback.rs        990  root scanout read routes, readback op, scanout dump
kms/session.rs         751  open, VT, input thread, suspend/resume, disable
kms/cursor.rs          697  cursor records, sprite, animation, display (mostly
                            cursor-plane, decision 4)
kms/present.rs         655  present batches/completion, vblank arming, crtc clock
kms/crtc_config.rs     489  topology signature/epoch, crtc-config probes
kms/export.rs          298  dma-buf export entries, GLX export, DRI3 version
```

Every file ≤ 5k except `trait_impl.rs` (accepted until 2.11b, decision 1).
Total 32,209 lines (31,742 before; the delta is `mod`/`use` lines and fmt).

## Delegation decision: (a) trait impl whole now, delegate in 2.11b

`split apply` can spread an *inherent* impl per member (it re-opens one
`impl KmsBackend {}` per target file). It does not refuse spreading a trait
impl, but that would create several `impl Backend for KmsBackend` blocks
(E0119), so the trait impl moves whole. `apply` does not write delegators;
`verify --delegate` only checks hand/script-written ones. Rule 1 also makes
move and delegate separate commit kinds. So: 2.11a = this move (1550/1550
leaves byte-identical, verified); 2.11b = per-subsystem delegate commits
(each trait body → `Self::<sub>_<name>(self, …)`), shrinking `trait_impl.rs`
to ~1.5–2k. Rough 2.11b destinations by trait-impl line count: windows/
redirect/COW 1.3k, core draw+copy+GetImage 1.75k, RENDER 1.55k, RANDR/crtc
0.85k, present 0.95k, DRI3 0.55k, input 0.5k, scanout/frame loop 0.5k,
cursor/font/pixmap 0.5k, keymap/shape/pointer/DPMS 0.8k, misc 0.6k.

## Results

Prep commit: the 32 relative paths → `crate::kms::render::…` /
`crate::kms::vk::…`, + fmt (backend tests green). Then `split apply`,
`cargo +nightly fmt`, `split verify` → **OK**: 1550/1550 leaves identical,
267 manifest visibility changes, 13 include paths same bytes, 0 audited
exceptions, 38 audited locations, 144 leaves' log targets moved to
descendants, no shadowed names. `verify --tests` against fresh pre-move
snapshots: 4008 / 4013 / 4121 tests (default / tcp-transport / xdmcp)
mapped. Rule-5 gate green (fmt, clippy ×3, tests ×3, lavapipe), except one
default `cargo test --all-targets` run that exited 101 with the failing test
name lost; four reruns of that config were green.

**Visibility delta (267, all to the same reach as before):**
- 202 methods and 64 free fns private → `pub(in crate::kms::render::backend)`
  (= the old private reach; children are two levels deep, so `pub(super)` is
  too narrow). 230 needed by the lib, 36 only by `backend/tests`.
- `parse_composite_glyph_items`: `pub(super)` → `pub(in crate::kms::render)`
  (`engine/tests/glyph_runs.rs` imports it via `render::backend::`).
- Re-exports: root `pub(super) use portable::*` (for that fn) and
  `use kms::*`; `portable/mod.rs` `pub(super) use` windows/render_ops/clip/
  draw/dump and `pub(in crate::kms::render) use text::*`; `kms/mod.rs`
  `pub(super) use` its 7 children. Method-only children get no glob (unused).

**Refusals and handling:**
1. Relative paths (sound): prep commit, 32 sites.
2. `#[track_caller]` locations (expected): the same 38 fns as phase 1a;
   reasons copied, text updated to "file:line moves". Not avoidable.
3. Name resolution (sound): `portable::render` shadowed `render` in 6 fns →
   renamed `render_ops`.
4. Log targets, macros, imports/traits in scope, delegate: none.
5. Unused glob (compiler, not the tool): the two paint_target free fns are
   used only by tests outside paint_target → kept in the root.

## Portability (portable/** grep, phase 3 territory, report only)

No `std::os::fd`/`OwnedFd`/`RawFd`, `libc::`, `nix::`, `rustix`, `drm`,
`gbm`, `udev`, `epoll`, `platform::` paths in portable code (hits only in
comments: libinput/ioctl mentions in keyboard/pointer/devices). But 76 code
lines touch `self.platform`: 42× `&mut self.platform` passed to store/engine
(GpuCore seam A), `vk`/`fb_w`/`fb_h`/`allocate_drawable_storage_as`
(GpuCore), and **14 KMS-only uses** — cursor-plane move/hide/hotspot and
`outputs`/`output_root_rect` in `pointer.rs` (11) and `cursor.rs` (3).
`cursor.rs` now lives in `kms/` (decision 4), so its uses left portable/.
`pointer.rs` stays in portable/ with its `cursor_plane_move` and
`outputs`/`output_root_rect` calls: a known phase-3 item (they go behind the
seam there). The CI grep as written (`platform::`) would pass; it should also
ban `cursor_plane_` / `.outputs` in portable/ once those go behind a seam.

## Decisions (jos, 2026-10-09)

1. Delegation later: the whole `impl Backend for KmsBackend` stays in
   `trait_impl.rs` (9.9k, accepted until 2.11b); per-subsystem delegators
   are a later, separate step (2.11b).
2. All types stay in `backend.rs`.
3. `for_tests.rs` stays one file.
4. `cursor.rs` moves from portable/ to `kms/` (almost all cursor-plane);
   `pointer.rs` stays in portable/ (its KMS uses are a phase-3 item, above).
5. The 36 test-only visibility changes (same reach) are accepted.

## Commit sequence

1. `refactor(kms): qualify backend super:: paths (prep)`: 32 sites, plus fmt.
2. `refactor(kms): split backend into portable/kms/trait_impl modules
   (move)`: `split apply` + fmt, `split verify --manifest …` and the rule-4
   test lists, then the full gate (rule 5).
3. `chore: blame-ignore` for 1 and 2.
4. This doc and the plan updated to the final tree.

## Phase 2.11b: delegators (executed 2026-10-09)

Each non-trivial trait body moved to an inherent
`KmsBackend::backend_<subsystem>_<method>` (`<subsystem>` = the file stem),
visibility `pub(in crate::kms::render::backend)`, appended as one
`impl KmsBackend {}` block per file; the trait method is now
`Self::backend_<subsystem>_<method>(self, args..)`. Generated by a
deterministic script (the trait method's lines, its leading comment block
moved to the helper); no new subsystem module. Left in `trait_impl.rs`: 54
trivial methods (one-line body or already a single call) and two whose
helper would leave a `kms/mod.rs` glob re-export unused (`-D warnings`):
`arm_present_absolute_vblank` (only lib user of `absolute_seq_user_data`,
whose other users are tests) and `on_vt_acquire` (only user of
`try_acquire_master_bounded`). For the same reason the crtc-config methods
went to `kms/randr.rs`, not `kms/crtc_config.rs`, and PolyText/ImageText to
`portable/draw.rs`, not `portable/text.rs`.

`trait_impl.rs`: 9,930 → 2,033 lines (229 trait methods, 173 delegators).
Helpers per file: windows 17, redirect 12, draw 22, clip 6, render_ops 19,
text 6, keyboard 8, devices 2, pointer 2, dump 1; kms/: cursor 10, present
18, export 20, randr 13, scanout 9, readback 1, session 7. Largest file now
`portable/draw.rs` (3,339). `portable/**` grep: clean.

**Verification.** `tools/split` fix first: a delegated helper may live in
another file of the tree (the "same file" rule held for every leaf, helpers
included). Manifests `backend_2b_<group>.toml` (one per commit) share the
identity table `backend_2b.paths`; each commit verifies OK with
`split verify --manifest … --delegate backend_ <sha>`. Audited per commit:
`[locations]` (calls to the `#[track_caller]` scene damage-audit helpers whose
recorded file:line moves with the body or shifts inside `trait_impl.rs`) and
`[log_targets]` (untargeted log macros now log under
`kms::render::backend::{portable,kms}::<file>` instead of `…::trait_impl`;
both under `kms::render::backend`). Location and log-target exceptions are
keyed by the helper's key, as `verify` looks them up by the new leaf. A
whole-series check (a throwaway commit with HEAD's tree on the pre-2.11b
parent) verifies OK: 173 delegations, 0 visibility changes, 26 location and
52 log-target audits; `verify --tests` against fresh pre-2.11b snapshots:
4008 / 4013 / 4121 tests mapped (default / tcp-transport / xdmcp). Rule-5
gate green: fmt, clippy ×3, tests ×3 (3693 / 3698 / 3806 passed), lavapipe
317 passed.

**Side effect.** Section header comments (`// ── RENDER ──` etc.) belonged to
the first method of each section, so `verify` moves them with that body into
the helper file; `trait_impl.rs` keeps only "A. Accessors" and "Misc".
