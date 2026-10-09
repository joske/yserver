# Phase 2.6 proposal: split `process_request.rs` (production code)

Step 2.6 of `2026-10-08-source-layout-cleanup.md`. Manifest:
`tools/split/manifests/process_request_2.toml` (+ `.paths`). Accepted by jos
2026-10-09 and executed (decisions at the end); sizes below are the final tree.

## Structure today (35,796 lines, tests already out)

- **Dispatch:** `process_request` (365 lines) does the common checks (local-only
  extensions, core and exact lengths, value masks and ranges), then one opcode
  `match`. Core opcodes go to ~140 `handle_*` fns, each with its own argument
  list (`state`, `backend`, `origin`, `client_id`, `sequence`, `header`,
  `body`). Extension majors go to one fn per extension (`handle_randr_request`,
  `handle_xi2_request`, …), and each of those has its own `minor` match.
- **Big fns:** `handle_xi2_request` 5,138 lines (XI1 and XI2 minors),
  `handle_randr_request` 2,273, `handle_xfixes_request` 1,123,
  `handle_glx_request` 1,001, `handle_render_request` 945, `handle_present_request`
  840, `handle_dri3_request` 794.
- **Shared helpers:** `emit_x11_error*`, `write_to_client`, `send_reply_with_fd`,
  `drawable_lookup`/`validate_*`, `xid_out_of_client_range`, `drawable_exists`.
- **State:** only `&mut ServerState` and `&mut dyn Backend`.
- **Macros:** two fn-local `macro_rules!`: `verify_pictures!`
  (`handle_render_request`, 13 uses) and `require_len!`
  (`handle_xinerama_request`, 6 uses). Their transcribers name only fn locals,
  absolute paths and root helpers. There are no other macro definitions.
- **Logs:** 376 `debug!`, 64 `warn!`, 17 `trace!`, 7 `info!`, 1 `error!`
  (41 with `target:`). No `module_path!`/`line!`/`file!`/`#[track_caller]`.
- **Outside users:** 7 `core_loop` siblings call ~45 `pub(crate)`/`pub(super)`
  items; `yserver` calls two `pub` present fns.

## Target tree (lines after `cargo +nightly fmt`)

All files sit under `core_loop/process_request/`, so they are descendants and
`env_logger` filters on `core_loop::process_request` still match. Module names
avoid any name that this file or its tests use as a path (`damage`, `present`,
`randr`, `xfixes`, `sync`, `shm`, `xinput`, `error`, `properties`).

```
process_request.rs   979  imports, opcode consts, RequestOutcome, process_request + match,
                          reject_non_local…, shared helpers (above), 3 types with private
                          fields (CopyAreaSubRect, CurrentVidModeOutput, PresentDomainSelection)
windows.rs          2938  create/configure/reparent/destroy(+subtree)/map/unmap/circulate,
                          attributes, query_tree, geometry, translate, exposures, save-set
redirect.rs          913  composite redirect backing lifecycle
drawing.rs          1893  poly*, fill, clear, copy_area/plane, put/get_image, image/poly text,
                          get_image reply header patch
gc_pixmap_cursor.rs  641  GC, pixmap, cursor create/free/change, query_best_size
fonts.rs             491  open/close/query font, list fonts, font path
colormaps.rs         488  colormaps and colour allocation
props.rs            1265  core property requests, atoms, dispatch_change_property, XI
                          property validation/commit (run.rs calls it)
selection.rs         578  selections, SendEvent
grabs.rs            1583  core pointer/keyboard/button/key grabs, AllowEvents (core + XI2 slave)
focus_pointer.rs     620  input focus, QueryPointer, WarpPointer (core + XI)
input_ctl.rs         925  keyboard/pointer control and mapping, keymap, bell
misc.rs              819  kill client, close-down mode, hosts, extensions, BIG-REQ, GE,
                          XC-MISC (+ largest free XID gap), X-Resource, grab server, motion events
render.rs           1038  RENDER
randr_ext.rs        2900  RANDR + Xinerama, crtc-config completion types
present_ext.rs      2976  PRESENT pacing, supersede, completion
dri3.rs              796  DRI3
glx.rs              1411  GLX
xshm.rs              853  MIT-SHM
shape_xfixes.rs     1829  SHAPE + XFIXES
composite_damage.rs  684  Composite + DAMAGE requests
sync_ext.rs          978  SYNC counters/fences/alarms
saver_dpms.rs       1092  DPMS, MIT-SCREEN-SAVER, core screen-saver requests, idletime
vidmode.rs           505  XF86VidMode
xkb.rs               390  XKB
xtest.rs             613  XTEST fake input
xi/mod.rs            575  XI1 helpers/consts, XI hierarchy/focus, XI2 version/bootstrap
xi/dispatch.rs      5140  handle_xi2_request alone (over the 5k ceiling: one fn; phase 2c)
```

## Results

Prep commit (15 paths qualified), then `split apply`, `cargo +nightly fmt`,
`split verify` → **OK**: 1630/1630 leaves identical, 232 visibility changes,
2 audited exceptions, 164 leaves with log targets moved to descendants, no
shadowed names. `verify --tests` against fresh pre-move snapshots: 4008 / 4013
/ 4121 tests (default / tcp-transport / xdmcp) mapped. Full rule-5 gate green.

**Visibility delta (232, all from the manifest; resolved reach never wider):**
- 224 items go from private to `pub(super)`:
  - 140 `handle_*` dispatch targets called by the root match.
  - ~39 helpers shared between siblings.
  - ~45 helpers that only tests reach (the tests call private helpers
    directly).
- 1 item becomes `pub(in crate::core_loop::process_request)`:
  `handle_xi2_request`, which is a grandchild of the root.
- 7 XI property items go from `pub(super)` (root, = `core_loop`) to
  `pub(in crate::core_loop)` in `props`: same reach.
- **Re-exports** (manifest lines): root `pub use` for `present_ext`/`randr_ext`,
  `pub(crate) use` for 10 children with `pub(crate)` items, plain `use` for the
  rest; `xi/mod.rs` has `pub(super) use dispatch::*`. Each level matches the
  widest item in that child; a broader glob warns and fails `-D warnings`.

**Refusals and how they were handled:**
1. **Relative paths (sound refusal).** There are 12 `super::run::` and 3
   `super::process_disconnect::` paths, in RANDR and KillClient. From a child,
   `super` names a different module. Fix: a **prep commit** that rewrites them
   as `crate::core_loop::…`, as in the phase 1b prep commits.
2. **Local macros (sound refusal).** `handle_render_request` and
   `handle_xinerama_request` define and invoke a fn-local `macro_rules!`. Both
   are listed under `exceptions`, with the transcriber's free names written out.
3. **Traits in scope (false positive, fixed in the tool).** The test mods
   `get_image_reply_tests` and `largest_free_xid_gap_tests` do
   `use super::<fn>;`. Once the fn moves, that import reaches it through the
   root's glob, and the model took it for a possible trait. `split` now
   follows in-tree globs for such an import (an opaque glob or a named
   re-export still counts as a possible trait), so both fns moved
   (`drawing`, `misc`).
4. **Things the tool cannot do yet:**
   - Private fields: tests and siblings read the private fields of
     `CopyAreaSubRect`, `PresentDomainSelection` and `CurrentVidModeOutput`.
     Fields are module-private, so these types stay in the root
     (`WarpRequest` moved with its only user, `handle_xi_warp_pointer`, to
     `focus_pointer`).
   - Already-`pub(super)` items: fixed in the tool. `apply` now replaces an
     item's own visibility with the manifest's; `verify` accepts the change
     only when listed and the resolved reach is not wider. The XI property
     validation moved to `props.rs` as `pub(in crate::core_loop)`.
5. **Log targets, locations, includes:** no refusals (every target is a
   descendant).

## Decisions (jos, 2026-10-09)

1. `xi/dispatch.rs` at 5,140 lines is accepted until phase 2c splits
   `handle_xi2_request` by minor.
2. The ~45 test-only `pub(super)` items are accepted.
3. `apply` was extended to replace an existing visibility (red→green
   fixtures), and the root extras it blocked moved out (root 1473 → 979).
4. The `_ext` suffixes stay.
5. The plan now says `core_loop/process_request/` (descendant rule).

## Commit sequence

1. `fix(tools/split): …`: visibility replacement + the glob-import trait fix.
2. `refactor(core): qualify process_request super:: paths (prep)`: 15
   sites, plus fmt.
3. `refactor(core): split process_request into request-family modules (move)`:
   `split apply` + fmt, `split verify --manifest …` and the rule-4 test lists,
   then the full gate (rule 5).
4. `chore: blame-ignore` for 2 and 3.
5. This doc and the plan updated to the final tree.
